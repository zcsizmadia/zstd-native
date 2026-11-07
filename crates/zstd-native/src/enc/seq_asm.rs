//! x86-64 assembly for the sequence bitstream loop (libzstd's
//! `ZSTD_encodeSequences_body`): the three FSE state transitions and the
//! six bit fields of each sequence, written backward from the last.
//!
//! The loop is three independent state chains (table lookup, shift, add,
//! table load: about 10 cycles each) and the bit container. On the EPYC
//! host libzstd's loop and the Rust one took 23-25 cycles a sequence
//! (reymont, level 1): the states, the container and the table and array
//! pointers don't fit in the registers, and the spills sit on the chains
//! (the Rust loop, with the fields packed as below, had 1.6 store-to-load
//! interlocks a sequence). Here the states, the container, its bit count,
//! the output pointer and the index stay in registers, with the three
//! tables at constant offsets from one base, and the arrays, the bit
//! count tables and the end index are memory operands of [`SeqCtx`]: about
//! 19 cycles a sequence, 75 instructions at 4 a cycle. The fields are
//! packed into one value with shifts local to the sequence and added to
//! the container once (three times for a sequence of more than 56 bits),
//! as in the Rust loop (`sequences::encode_sequences_body`), which stays
//! the fallback. Uses BMI2 (`bzhi`, `shlx`, `shrx`): call only when the
//! CPU has it.
#![allow(unsafe_code)]

use super::fse_enc::{FseCTable, SymbolTransform};
use super::seq_store::SeqDef;
use super::sequences::{FseCTables, LlCTable, MlCTable, OfCTable};
use core::arch::asm;
use core::mem::offset_of;

/// The arrays of [`encode_seqs`], read by field offset.
#[repr(C)]
pub(super) struct SeqCtx {
    pub(super) ll_codes: *const u8,
    pub(super) of_codes: *const u8,
    pub(super) ml_codes: *const u8,
    pub(super) seqs: *const SeqDef,
    /// Stop when the index reaches it.
    pub(super) stop: usize,
    /// `LL_bits` and `ML_bits`, padded to 64 entries (the codes are
    /// masked to 6 bits).
    pub(super) ll_bits: [u8; 64],
    pub(super) ml_bits: [u8; 64],
}

/// Offsets in [`FseCTables`] of a table's next states and transforms.
macro_rules! tbl_offsets {
    ($field:ident, $t:ty) => {
        (
            offset_of!(FseCTables, $field) + offset_of!($t, state_table),
            offset_of!(FseCTables, $field) + offset_of!($t, symbol_tt),
        )
    };
}
const LL_OFF: (usize, usize) = tbl_offsets!(ll, LlCTable);
const OF_OFF: (usize, usize) = tbl_offsets!(of, OfCTable);
const ML_OFF: (usize, usize) = tbl_offsets!(ml, MlCTable);
const DFS: usize = offset_of!(SymbolTransform, delta_find_state);
const DNB: usize = offset_of!(SymbolTransform, delta_nb_bits);
const _: () = assert!(size_of::<SymbolTransform>() == 8);

/// The index masks of the next-state tables and of the transforms (the
/// Rust `next_state` / `tt` masks), for these table types:
const LL_MASK: (usize, usize) = (512 - 1, 64 - 1);
const OF_MASK: (usize, usize) = (256 - 1, 32 - 1);
const ML_MASK: (usize, usize) = (512 - 1, 64 - 1);
// (fails to compile if the table types change)
#[allow(dead_code)]
fn table_types(
    l: LlCTable,
    o: OfCTable,
    m: MlCTable,
) -> (FseCTable<512, 64>, FseCTable<256, 32>, FseCTable<512, 64>) {
    (l, o, m)
}

/// One state transition (`FSE_encodeSymbol` without the write): state
/// `$st`, code `$c` (masked here to the transforms); leaves the output bit
/// count in `rcx`, the old state's low bits in `$bits` and the new state
/// in `$st`. The index `(state >> nb) + deltaFindState` is added in 32
/// bits: only its low bits (the table mask) are used.
#[rustfmt::skip]
macro_rules! fse_step_asm {
    ($st:literal, $st32:literal, $c:literal, $c32:literal, $bits:literal,
     $tt:literal, $states:literal, $smask:literal, $cmask:literal) => {
        concat!(
            "and ", $c32, ", ", $cmask, "\n",
            // nb = (state + deltaNbBits) >> 16
            "mov ecx, dword ptr [rbp + 8*", $c, " + ", $tt, " + {dnb}]\n",
            "add rcx, ", $st, "\n",
            "shr rcx, 16\n",
            "bzhi ", $bits, ", ", $st, ", rcx\n",
            // state = stateTable[(state >> nb) + deltaFindState]
            "shrx ", $st, ", ", $st, ", rcx\n",
            "add ", $st32, ", dword ptr [rbp + 8*", $c, " + ", $tt, " + {dfs}]\n",
            "and ", $st32, ", ", $smask, "\n",
            "movzx ", $st32, ", word ptr [rbp + 2*", $st, " + ", $states, "]\n",
        )
    };
}

/// Writes the container (`r8`, `r9d` bits) at `r10` and keeps the
/// incomplete byte (`BIT_flushBits`). Clobbers `rdi`.
#[rustfmt::skip]
macro_rules! flush_asm {
    () => {
        concat!(
            "mov qword ptr [r10], r8\n",
            "mov edi, r9d\n",
            "and edi, 56\n",
            "shrx r8, r8, rdi\n",
            "shr edi, 3\n",
            "add r10, rdi\n",
            "and r9d, 7\n",
        )
    };
}

/// Encodes the sequences `i - 1` down to `ctx.stop` (libzstd's loop of
/// `ZSTD_encodeSequences_body`, after the last sequence): `states` are the
/// match length, offset and literal length states, `container` /
/// `bit_pos` / `out` the bit writer's.
///
/// # Safety
/// `ctx.stop < i`, and the code arrays and `ctx.seqs` have at least `i`
/// entries; `*bit_pos <= 7` with no bits of `*container` at or above it;
/// `out` has `(i - ctx.stop) * 12 + 8` writable bytes (a sequence
/// advances it by at most 12); the CPU has BMI1 and BMI2.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn encode_seqs(
    ctx: &SeqCtx,
    cts: &FseCTables,
    states: &mut [usize; 3],
    container: &mut u64,
    bit_pos: &mut u32,
    out: &mut *mut u8,
    i: usize,
) {
    debug_assert!(ctx.stop < i && *bit_pos <= 7);
    let [mut ml, mut of, mut ll] = *states;
    let (mut c, mut bp, mut o) = (*container, u64::from(*bit_pos), *out);
    // SAFETY: per the contract. The codes index the transforms and the bit
    // tables masked to their sizes, and the next-state indices are masked
    // to their tables (as `FseCTable::tt` / `next_state` do), so the table
    // reads stay in `cts` and `ctx` whatever the codes and states; the
    // arrays are read at indices `stop..i`; each sequence writes 8 bytes
    // at `out` and moves it by at most 12 (7 pending bits and 89 bits of
    // fields). `rbx` and `rbp` are saved and restored in the template
    // (they can't be `asm!` operands).
    unsafe {
        asm!(
            "push rbx",
            "push rbp",
            "mov rbp, rdi",
            ".p2align 5",
            "2:",
            "dec r14",
            // offset state (its code stays in rdx): the fields in rsi,
            // their bit count in rbx
            "mov rax, qword ptr [r15 + {ofc}]",
            "movzx edx, byte ptr [rax + r14]",
            fse_step_asm!("r12", "r12d", "rdx", "edx", "rsi", "{of_tt}", "{of_st}", "{of_sm}",
                "{of_cm}"),
            "mov ebx, ecx",
            // match length state
            "mov rax, qword ptr [r15 + {mlc}]",
            "movzx eax, byte ptr [rax + r14]",
            fse_step_asm!("r11", "r11d", "rax", "eax", "rdi", "{ml_tt}", "{ml_st}", "{ml_sm}",
                "{ml_cm}"),
            "shlx rdi, rdi, rbx",
            "or rsi, rdi",
            "add ebx, ecx",
            // literal length state
            "mov rax, qword ptr [r15 + {llc}]",
            "movzx eax, byte ptr [rax + r14]",
            fse_step_asm!("r13", "r13d", "rax", "eax", "rdi", "{ll_tt}", "{ll_st}", "{ll_sm}",
                "{ll_cm}"),
            "shlx rdi, rdi, rbx",
            "or rsi, rdi",
            "add ebx, ecx",
            // literal length extra bits (the code is still in rax)
            "movzx ecx, byte ptr [r15 + rax + {llb}]",
            "mov rax, qword ptr [r15 + {seqs}]",
            "movzx edi, word ptr [rax + 8*r14 + {lit}]",
            "bzhi rdi, rdi, rcx",
            "shlx rdi, rdi, rbx",
            "or rsi, rdi",
            "add ebx, ecx",
            // match length extra bits
            "mov rcx, qword ptr [r15 + {mlc}]",
            "movzx ecx, byte ptr [rcx + r14]",
            "and ecx, 63",
            "movzx ecx, byte ptr [r15 + rcx + {mlb}]",
            "movzx edi, word ptr [rax + 8*r14 + {mlbase}]",
            "bzhi rdi, rdi, rcx",
            "shlx rdi, rdi, rbx",
            "or rsi, rdi",
            "add ebx, ecx",
            // offset extra bits in rax (rsi: the other fields, rbx bits,
            // at most 58)
            "mov eax, dword ptr [rax + 8*r14 + {off}]",
            "bzhi rax, rax, rdx",
            "lea ecx, [rbx + rdx]",
            "cmp ecx, 56",
            "ja 5f",
            // all at once: container |= fields << bit_pos
            "shlx rax, rax, rbx",
            "or rsi, rax",
            "shlx rsi, rsi, r9",
            "or r8, rsi",
            "add r9d, ecx",
            flush_asm!(),
            "3:",
            "cmp r14, qword ptr [r15 + {stop}]",
            "ja 2b",
            "jmp 9f",
            // over 56 bits: the other fields in two parts (at most 29
            // bits each), then the offset (at most 31), each flushed
            "5:",
            "mov ecx, 29",
            "cmp ebx, ecx",
            "cmovb ecx, ebx",
            "bzhi rdi, rsi, rcx",
            "shlx rdi, rdi, r9",
            "or r8, rdi",
            "add r9d, ecx",
            flush_asm!(),
            "shrx rsi, rsi, rcx",
            "sub ebx, ecx",
            "shlx rsi, rsi, r9",
            "or r8, rsi",
            "add r9d, ebx",
            flush_asm!(),
            "shlx rax, rax, r9",
            "or r8, rax",
            "add r9d, edx",
            flush_asm!(),
            "jmp 3b",
            "9:",
            "pop rbp",
            "pop rbx",
            ofc = const offset_of!(SeqCtx, of_codes),
            mlc = const offset_of!(SeqCtx, ml_codes),
            llc = const offset_of!(SeqCtx, ll_codes),
            seqs = const offset_of!(SeqCtx, seqs),
            stop = const offset_of!(SeqCtx, stop),
            llb = const offset_of!(SeqCtx, ll_bits),
            mlb = const offset_of!(SeqCtx, ml_bits),
            lit = const offset_of!(SeqDef, lit_length),
            mlbase = const offset_of!(SeqDef, ml_base),
            off = const offset_of!(SeqDef, off_base),
            of_st = const OF_OFF.0,
            of_tt = const OF_OFF.1,
            ml_st = const ML_OFF.0,
            ml_tt = const ML_OFF.1,
            ll_st = const LL_OFF.0,
            ll_tt = const LL_OFF.1,
            of_sm = const OF_MASK.0,
            of_cm = const OF_MASK.1,
            ml_sm = const ML_MASK.0,
            ml_cm = const ML_MASK.1,
            ll_sm = const LL_MASK.0,
            ll_cm = const LL_MASK.1,
            dfs = const DFS,
            dnb = const DNB,
            inout("rdi") core::ptr::from_ref(cts) => _,
            in("r15") core::ptr::from_ref(ctx),
            inout("r8") c,
            inout("r9") bp,
            inout("r10") o,
            inout("r11") ml,
            inout("r12") of,
            inout("r13") ll,
            inout("r14") i => _,
            out("rax") _,
            out("rcx") _,
            out("rdx") _,
            out("rsi") _,
        );
    }
    *states = [ml, of, ll];
    (*container, *bit_pos, *out) = (c, bp as u32, o);
}
