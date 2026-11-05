//! x86-64 assembly for the whole noDict block loops of the `fast` and
//! `dfast` match finders (libzstd's `ZSTD_compressBlock_fast_noDict_generic`
//! and `ZSTD_compressBlock_doubleFast_noDict_generic`): the position search
//! *and* the match handling (match length, backward extension, sequence
//! store, table fill, immediate repcode loop).
//!
//! An asm search loop that returns to Rust for every match (PR #32) left
//! text 4-6% behind C: a match every 10-20 bytes, and the Rust match path
//! cost 10-40 instructions per match more than C, as LLVM reloaded the
//! block invariants (base, the sequence store pointers, the limits) from
//! the stack around the asm block and spilled the cursor. With the whole
//! loop here, the per-position values stay in registers, the per-match
//! values are memory operands of one context struct ([`FastCtx`],
//! [`DfastCtx`], like C's spills), and nothing moves between the two. The
//! `dfast` loop also pipelines the short-table lookup one position ahead
//! (libzstd only pipelines the long one): the loops are table-miss bound.
//!
//! The asm runs from libzstd's `_start` label (the top of its outer loop)
//! to the end of the block (`_cleanup`); the Rust loops
//! (`fast::no_dict_generic`, `dfast::no_dict_generic`) run the block until
//! the first repcode is valid (`rep_offset1 != 0`, so the repcode test
//! needs no mask) and remain the fallback (no BMI2, the `portable`
//! feature, other targets). Uses BMI1/BMI2 (`shrx`, `tzcnt`): call only
//! when the CPU has them.
#![allow(unsafe_code)]

use super::seq_store::SeqDef;
use core::arch::asm;
use core::mem::offset_of;

/// Read instead of an invalid candidate (index below the window's low
/// limit), so the test stays branch-free; a match on it is rejected by a
/// second index check.
static DUMMY: [u8; 8] = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];

/// The per-match state of the `fast` noDict block loop, in memory (the
/// asm reads it by field offset through one register).
#[repr(C)]
pub(super) struct FastCtx {
    /// Hash: `(read64(p) * prime) >> shift` (see `fast::hash_prime`).
    pub(super) prime: u64,
    /// `stepSize` (from `targetLength`).
    pub(super) step_size: usize,
    /// Current `step` (the gap between position pairs).
    pub(super) step: usize,
    /// Search until the new `ip2` is at or past `lim = min(nextStep,
    /// ilimit - 1)`, the two end-of-iteration tests folded into one.
    pub(super) lim: usize,
    pub(super) next_step: usize,
    pub(super) anchor: *const u8,
    pub(super) iend: *const u8,
    /// `iend - HASH_READ_SIZE`.
    pub(super) ilimit: *const u8,
    /// `base + prefixStartIndex`.
    pub(super) prefix_start: *const u8,
    /// `iend - WILDCOPY_OVERLENGTH` (saturating): literals ending at or
    /// before it are copied 16/32 bytes at a time.
    pub(super) lit_limit_w: usize,
    /// The sequence store's literal write pointer (`seqStore->lit`).
    pub(super) lit: *mut u8,
    /// The sequence store's sequence write pointer (`seqStore->sequences`).
    pub(super) seq: *mut SeqDef,
    pub(super) rep1: u64,
    pub(super) rep2: u64,
    /// Out: the long length type (0 none, 1 literal, 2 match) and the
    /// sequence it applies to, if a length exceeded `u16`.
    pub(super) llt: u64,
    pub(super) llpos: *mut SeqDef,
}

/// `ZSTD_count(pIn, pMatch, iend)` for `$pin = pIn`, `$pm = pMatch`
/// (64-bit registers): leaves `$pin` at the end of the match (`pIn +
/// count`); clobbers `$pm`, `$lim` and the temporary `$t` (given with its
/// 32-, 16- and 8-bit names). `$ctx` holds the context with the `iend`
/// field. Word steps, then 4/2/1 bytes, like libzstd.
macro_rules! count_asm {
    ($ctx:literal, $pin:literal, $pm:literal, $t:literal, $t32:literal, $t16:literal,
     $t8:literal, $lim:literal) => {
        concat!(
            "mov ",
            $lim,
            ", qword ptr [",
            $ctx,
            " + {iend}]\n",
            "sub ",
            $lim,
            ", 7\n", // pInLoopLimit
            "200:\n",
            "cmp ",
            $pin,
            ", ",
            $lim,
            "\n",
            "jae 202f\n",
            "mov ",
            $t,
            ", qword ptr [",
            $pm,
            "]\n",
            "xor ",
            $t,
            ", qword ptr [",
            $pin,
            "]\n",
            "jnz 201f\n",
            "add ",
            $pin,
            ", 8\n",
            "add ",
            $pm,
            ", 8\n",
            "jmp 200b\n",
            "201:\n",
            "tzcnt ",
            $t,
            ", ",
            $t,
            "\n",
            "shr ",
            $t32,
            ", 3\n",
            "add ",
            $pin,
            ", ",
            $t,
            "\n",
            "jmp 205f\n",
            "202:\n",
            "add ",
            $lim,
            ", 4\n", // iend - 3
            "cmp ",
            $pin,
            ", ",
            $lim,
            "\n",
            "jae 203f\n",
            "mov ",
            $t32,
            ", dword ptr [",
            $pm,
            "]\n",
            "cmp ",
            $t32,
            ", dword ptr [",
            $pin,
            "]\n",
            "jne 203f\n",
            "add ",
            $pin,
            ", 4\n",
            "add ",
            $pm,
            ", 4\n",
            "203:\n",
            "add ",
            $lim,
            ", 2\n", // iend - 1
            "cmp ",
            $pin,
            ", ",
            $lim,
            "\n",
            "jae 204f\n",
            "movzx ",
            $t32,
            ", word ptr [",
            $pm,
            "]\n",
            "cmp ",
            $t16,
            ", word ptr [",
            $pin,
            "]\n",
            "jne 204f\n",
            "add ",
            $pin,
            ", 2\n",
            "add ",
            $pm,
            ", 2\n",
            "204:\n",
            "inc ",
            $lim,
            "\n", // iend
            "cmp ",
            $pin,
            ", ",
            $lim,
            "\n",
            "jae 205f\n",
            "movzx ",
            $t32,
            ", byte ptr [",
            $pm,
            "]\n",
            "cmp ",
            $t8,
            ", byte ptr [",
            $pin,
            "]\n",
            "jne 205f\n",
            "inc ",
            $pin,
            "\n",
            "205:\n",
        )
    };
}

/// `ZSTD_storeSeq`'s literal copy and sequence write, for `$ip` = the
/// match start (literals are `[anchor, ip)`), `$off` = the offBase
/// (32-bit register), `$ml` = the match length (64-bit register, kept);
/// then `ip += mLength; anchor = ip` (`$ip` advanced). Clobbers `rax`,
/// `rdx`, `rsi`, `$lp` (the literal/sequence pointer), `xmm0`, `xmm1`.
/// Uses the labels 210-218 (`210`: the literal copy for literals ending
/// after `lit_limit_w`, byte by byte).
macro_rules! store_seq_asm {
    ($ctx:literal, $ip:literal, $off:literal, $ml:literal, $lp:literal) => {
        concat!(
            "mov rsi, qword ptr [",
            $ctx,
            " + {anchor}]\n",
            "mov rdx, ",
            $ip,
            "\n",
            "sub rdx, rsi\n", // litLength
            "mov ",
            $lp,
            ", qword ptr [",
            $ctx,
            " + {lit}]\n",
            // literals ending at or before litLimit_w: 16 bytes, then 32
            // at a time (reads up to 31 bytes past them)
            "cmp ",
            $ip,
            ", qword ptr [",
            $ctx,
            " + {lit_limit_w}]\n",
            "ja 210f\n",
            "movups xmm0, xmmword ptr [rsi]\n",
            "movups xmmword ptr [",
            $lp,
            "], xmm0\n",
            "cmp rdx, 16\n",
            "jbe 212f\n",
            "lea rax, [",
            $lp,
            " + rdx]\n",
            "add ",
            $lp,
            ", 16\n",
            "add rsi, 16\n",
            "211:\n",
            "movups xmm0, xmmword ptr [rsi]\n",
            "movups xmm1, xmmword ptr [rsi + 16]\n",
            "movups xmmword ptr [",
            $lp,
            "], xmm0\n",
            "movups xmmword ptr [",
            $lp,
            " + 16], xmm1\n",
            "add rsi, 32\n",
            "add ",
            $lp,
            ", 32\n",
            "cmp ",
            $lp,
            ", rax\n",
            "jb 211b\n",
            "212:\n",
            "add qword ptr [",
            $ctx,
            " + {lit}], rdx\n",
            "mov ",
            $lp,
            ", qword ptr [",
            $ctx,
            " + {seq}]\n",
            "cmp rdx, 0xFFFF\n",
            "ja 215f\n", // long literal length
            "213:\n",
            "lea rax, [",
            $ml,
            " - 3]\n", // mlBase
            "cmp rax, 0xFFFF\n",
            "ja 216f\n", // long match length
            "214:\n",
            "mov dword ptr [",
            $lp,
            "], ",
            $off,
            "\n",
            "mov word ptr [",
            $lp,
            " + 4], dx\n",
            "mov word ptr [",
            $lp,
            " + 6], ax\n",
            "add ",
            $lp,
            ", 8\n",
            "mov qword ptr [",
            $ctx,
            " + {seq}], ",
            $lp,
            "\n",
            "add ",
            $ip,
            ", ",
            $ml,
            "\n",
            "mov qword ptr [",
            $ctx,
            " + {anchor}], ",
            $ip,
            "\n",
            "jmp 218f\n",
            // literals ending after litLimit_w: byte by byte
            "210:\n",
            "test rdx, rdx\n",
            "jz 212b\n",
            "217:\n",
            "movzx eax, byte ptr [rsi]\n",
            "mov byte ptr [",
            $lp,
            "], al\n",
            "inc rsi\n",
            "inc ",
            $lp,
            "\n",
            "dec rdx\n",
            "jnz 217b\n",
            "mov rdx, ",
            $ip,
            "\n",
            "sub rdx, qword ptr [",
            $ctx,
            " + {anchor}]\n",
            "jmp 212b\n",
            // long literal / match length: flag the sequence
            "215:\n",
            "mov qword ptr [",
            $ctx,
            " + {llt}], 1\n",
            "mov qword ptr [",
            $ctx,
            " + {llpos}], ",
            $lp,
            "\n",
            "jmp 213b\n",
            "216:\n",
            "mov qword ptr [",
            $ctx,
            " + {llt}], 2\n",
            "mov qword ptr [",
            $ctx,
            " + {llpos}], ",
            $lp,
            "\n",
            "jmp 214b\n",
            "218:\n",
        )
    };
}

/// The sequence write of `ZSTD_storeSeq(seqStore, 0, anchor, iend,
/// REPCODE1_TO_OFFBASE, rLength)` (the immediate repcode loops: no
/// literals), `$ml` = rLength; then `ip += rLength; anchor = ip`. Clobbers
/// `rax` and `$sp` (the sequence pointer). Labels 220-222.
macro_rules! store_rep_asm {
    ($ctx:literal, $ip:literal, $ml:literal, $sp:literal) => {
        concat!(
            "mov ",
            $sp,
            ", qword ptr [",
            $ctx,
            " + {seq}]\n",
            "lea rax, [",
            $ml,
            " - 3]\n",
            "cmp rax, 0xFFFF\n",
            "ja 221f\n",
            "220:\n",
            "shl rax, 48\n", // SeqDef { off_base: 1, lit_length: 0, ml_base }
            "or rax, 1\n",
            "mov qword ptr [",
            $sp,
            "], rax\n",
            "add ",
            $sp,
            ", 8\n",
            "mov qword ptr [",
            $ctx,
            " + {seq}], ",
            $sp,
            "\n",
            "add ",
            $ip,
            ", ",
            $ml,
            "\n",
            "mov qword ptr [",
            $ctx,
            " + {anchor}], ",
            $ip,
            "\n",
            "jmp 222f\n",
            "221:\n",
            "mov qword ptr [",
            $ctx,
            " + {llt}], 2\n",
            "mov qword ptr [",
            $ctx,
            " + {llpos}], ",
            $sp,
            "\n",
            "jmp 220b\n",
            "222:\n",
        )
    };
}

/// `ZSTD_compressBlock_fast_noDict_generic` from `_start` with `ip0` to
/// `_cleanup`, for `rep_offset1 != 0` (the cmov candidate test, which
/// finds the same matches as the branch variant). Writes the sequences at
/// `ctx.lit` / `ctx.seq` and leaves the state for the cleanup in `ctx`
/// (`anchor`, `rep1`, `rep2`, `lit`, `seq`, `llt`, `llpos`).
///
/// # Safety
/// The contract of `fast::no_dict_generic` at its `_start` label: `ip0`
/// and `ctx.anchor` are positions in `[base + low, ctx.iend)` with
/// `anchor <= ip0`; `ctx.ilimit = iend - 8`; `ctx.rep1 != 0` and
/// `rep_disp = -rep1`, with `ip0 - rep1` in the window (and `rep2` likewise
/// if nonzero); `ht` has `1 << (64 - shift)` entries; the sequence store
/// has room for the block's sequences and literals at `ctx.lit` / `ctx.seq`
/// (`SeqStore::reserve`), with `WILDCOPY_OVERLENGTH` bytes of slack after
/// the literals; the CPU has BMI1 and BMI2.
#[inline(always)]
pub(super) unsafe fn fast_block(
    ctx: &mut FastCtx,
    ip0: *const u8,
    ht: *mut u32,
    base: *const u8,
    low: u64,
    rep_disp: usize,
    shift: u64,
) {
    debug_assert!(ctx.rep1 != 0 && rep_disp == 0usize.wrapping_sub(ctx.rep1 as usize));
    // With LDM the range may be empty or tiny: `ip0` can be at `iend` (or
    // one past it, `ip0 == prefixStart`); the asm stops at `ip3 >= ilimit`.
    debug_assert!(ctx.anchor <= ip0 && ip0 <= ctx.iend.wrapping_add(1));
    debug_assert!(ctx.ilimit == ctx.iend.wrapping_sub(8));
    // SAFETY: per the caller: the reads, table writes and sequence store
    // writes are those of the Rust loop. `rbx` and `rbp` are saved and
    // restored in the template (they can't be `asm!` operands).
    unsafe {
        asm!(
            "push rbx",
            "push rbp",
            "lea rbx, [rip + {dummy}]",
            "jmp 20f",
            // rdi: ip0, rsi: ip2 (ip1 = ip0 + 1, ip3 = ip2 + 1), r8: hash0,
            // r9: hash1, r10: matchIdx (of ip0), r11: hashTable, r12: base,
            // r13: prefixStartIndex, r14: -rep_offset1, r15: 64 - hashLog,
            // rbx: DUMMY, rcx: ctx. Two positions per iteration (libzstd).
            // The loop is 127 bytes: on a 64-byte boundary it spans two
            // lines; 32 bytes off (`.p2align 5`, as the linker happened to
            // place it) three, and level 1 was 1.5-2% slower (#34).
            ".p2align 6",
            "2:",
            // load repcode match for ip[2]
            "mov eax, dword ptr [rsi + r14]",
            // write back hash table entry
            "mov edx, edi",
            "sub edx, r12d",
            "mov dword ptr [r11 + 4*r8], edx",
            // check repcode at ip[2]
            "cmp eax, dword ptr [rsi]",
            "je 30f",
            // table candidate of ip0: select, compare 4 bytes
            "lea rdx, [r12 + r10]",
            "cmp r10d, r13d",
            "cmovb rdx, rbx",
            "mov eax, dword ptr [rdi]",
            "cmp eax, dword ptr [rdx]",
            "je 40f",
            "5:",
            // lookup ip[1]; hash ip[2]
            "mov r10d, dword ptr [r11 + 4*r9]",
            "mov r8, r9",
            "mov r9, qword ptr [rsi]",
            "imul r9, qword ptr [rcx + {prime}]",
            "shrx r9, r9, r15",
            // advance (ip0 = ip1)
            "inc rdi",
            // write back hash table entry
            "mov edx, edi",
            "sub edx, r12d",
            "mov dword ptr [r11 + 4*r8], edx",
            // table candidate of ip0
            "lea rdx, [r12 + r10]",
            "cmp r10d, r13d",
            "cmovb rdx, rbx",
            "mov eax, dword ptr [rdi]",
            "cmp eax, dword ptr [rdx]",
            "je 60f",
            "7:",
            // lookup ip[1]; hash ip[2] (= ip2 + 1)
            "mov r10d, dword ptr [r11 + 4*r9]",
            "mov r8, r9",
            "mov r9, qword ptr [rsi + 1]",
            "imul r9, qword ptr [rcx + {prime}]",
            "shrx r9, r9, r15",
            // advance: ip0 = ip2, ip2 = ip0 + step
            "mov rdi, rsi",
            "add rsi, qword ptr [rcx + {step}]",
            "cmp rsi, qword ptr [rcx + {lim}]",
            "jb 2b",
            // calculate step
            "cmp rsi, qword ptr [rcx + {next_step}]",
            "jb 8f",
            "inc qword ptr [rcx + {step}]",
            "prefetcht0 byte ptr [rdi + 65]",
            "prefetcht0 byte ptr [rdi + 129]",
            "add qword ptr [rcx + {next_step}], 128",
            "8:",
            // while (ip3 < ilimit)
            "lea rax, [rsi + 1]",
            "cmp rax, qword ptr [rcx + {ilimit}]",
            "jae 90f",
            // lim = min(nextStep, ilimit - 1)
            "mov rax, qword ptr [rcx + {ilimit}]",
            "dec rax",
            "mov rdx, qword ptr [rcx + {next_step}]",
            "cmp rdx, rax",
            "cmova rdx, rax",
            "mov qword ptr [rcx + {lim}], rdx",
            "jmp 2b",
            //
            // _start: step = stepSize, nextStep = ip0 + kStepIncr, ip2 =
            // ip0 + step; stop if ip3 >= ilimit; hash ip0, ip1; look up ip0.
            "20:",
            "mov rax, qword ptr [rcx + {step_size}]",
            "mov qword ptr [rcx + {step}], rax",
            "lea rdx, [rdi + 128]",
            "mov qword ptr [rcx + {next_step}], rdx",
            "lea rsi, [rdi + rax]",
            "lea rax, [rsi + 1]",
            "cmp rax, qword ptr [rcx + {ilimit}]",
            "jae 90f",
            "mov rax, qword ptr [rcx + {ilimit}]",
            "dec rax",
            "cmp rdx, rax",
            "cmova rdx, rax",
            "mov qword ptr [rcx + {lim}], rdx",
            "mov r8, qword ptr [rdi]",
            "imul r8, qword ptr [rcx + {prime}]",
            "shrx r8, r8, r15",
            "mov r9, qword ptr [rdi + 1]",
            "imul r9, qword ptr [rcx + {prime}]",
            "shrx r9, r9, r15",
            "mov r10d, dword ptr [r11 + 4*r8]",
            "jmp 2b",
            //
            // repcode match at ip2: hashTable[hash1] = ip1 - base; ip0 =
            // ip2, match0 = ip0 - rep1, extended back by one byte if it
            // matches; mLength = 4 + that, offcode = REPCODE1_TO_OFFBASE.
            "30:",
            "lea eax, [rdi + 1]",
            "sub eax, r12d",
            "mov dword ptr [r11 + 4*r9], eax",
            "mov rbp, rdi", // current0's position, for the table fill
            "mov rdi, rsi",
            "lea rdx, [rdi + r14]",
            "movzx eax, byte ptr [rdi - 1]",
            "cmp al, byte ptr [rdx - 1]",
            "sete al",
            "movzx eax, al",
            "sub rdi, rax",
            "sub rdx, rax",
            "lea r8, [rax + 4]",
            "mov r9d, 1",
            "jmp 50f",
            //
            // table match at ip0 (first position; the candidate must be
            // valid, the dummy may have matched): hashTable[hash1] =
            // ip1 - base.
            "40:",
            "cmp r10d, r13d",
            "jb 5b",
            "lea eax, [rdi + 1]",
            "sub eax, r12d",
            "mov dword ptr [r11 + 4*r9], eax",
            "jmp 45f",
            // table match at the second position: hashTable[hash1] =
            // ip1 - base (ip1 = ip2 here) unless search resumes at or
            // before it (step > 4).
            "60:",
            "cmp r10d, r13d",
            "jb 7b",
            "cmp qword ptr [rcx + {step}], 4",
            "ja 45f",
            "mov eax, esi",
            "sub eax, r12d",
            "mov dword ptr [r11 + 4*r9], eax",
            //
            // _offset: match0 = base + matchIdx; rep2 = rep1; rep1 = ip0 -
            // match0; offcode = OFFSET_TO_OFFBASE(rep1); mLength = 4;
            // count backwards while ip0 > anchor and match0 > prefixStart.
            "45:",
            "mov rbp, rdi",
            "lea rdx, [r12 + r10]",
            "mov rax, qword ptr [rcx + {rep1}]",
            "mov qword ptr [rcx + {rep2}], rax",
            "mov eax, edi",
            "sub eax, edx",
            "mov qword ptr [rcx + {rep1}], rax",
            "lea r9d, [rax + 3]",
            "neg rax",
            "mov r14, rax",
            "mov r8d, 4",
            "mov rax, qword ptr [rcx + {anchor}]",
            "mov rsi, qword ptr [rcx + {prefix_start}]",
            "46:",
            "cmp rdi, rax",
            "jbe 50f",
            "cmp rdx, rsi",
            "jbe 50f",
            "movzx r10d, byte ptr [rdi - 1]",
            "cmp r10b, byte ptr [rdx - 1]",
            "jne 50f",
            "dec rdi",
            "dec rdx",
            "inc r8",
            "jmp 46b",
            //
            // _match (rdi: ip0, rdx: match0, r8: mLength, r9: offcode, rbp:
            // current0's position): mLength += count(ip0 + mLength, match0
            // + mLength, iend).
            "50:",
            "lea rax, [rdi + r8]",
            "add rdx, r8",
            count_asm!("rcx", "rax", "rdx", "rsi", "esi", "si", "sil", "r10"),
            "mov r8, rax",
            "sub r8, rdi",
            // ZSTD_storeSeq(ip0 - anchor, anchor, iend, offcode, mLength);
            // ip0 += mLength; anchor = ip0
            store_seq_asm!("rcx", "rdi", "r9d", "r8", "r10"),
            "cmp rdi, qword ptr [rcx + {ilimit}]",
            "ja 20b",
            // fill table: hashTable[hash(base + current0 + 2)] = current0 +
            // 2; hashTable[hash(ip0 - 2)] = ip0 - 2 - base
            "mov rax, qword ptr [rbp + 2]",
            "imul rax, qword ptr [rcx + {prime}]",
            "shrx rax, rax, r15",
            "lea edx, [rbp + 2]",
            "sub edx, r12d",
            "mov dword ptr [r11 + 4*rax], edx",
            "mov rax, qword ptr [rdi - 2]",
            "imul rax, qword ptr [rcx + {prime}]",
            "shrx rax, rax, r15",
            "lea edx, [rdi - 2]",
            "sub edx, r12d",
            "mov dword ptr [r11 + 4*rax], edx",
            // check for an immediate repcode (rep2 != 0)
            "mov rdx, qword ptr [rcx + {rep2}]",
            "test edx, edx",
            "jz 20b",
            "55:",
            "mov eax, dword ptr [rdi]",
            "mov rsi, rdi",
            "sub rsi, rdx",
            "cmp eax, dword ptr [rsi]",
            "jne 20b",
            // rLength = count(ip0 + 4, ip0 + 4 - rep2, iend) + 4
            "lea rax, [rdi + 4]",
            "lea rdx, [rsi + 4]",
            count_asm!("rcx", "rax", "rdx", "rsi", "esi", "si", "sil", "r10"),
            "mov r8, rax",
            "sub r8, rdi",
            // swap rep1 <=> rep2
            "mov rax, qword ptr [rcx + {rep1}]",
            "mov rdx, qword ptr [rcx + {rep2}]",
            "mov qword ptr [rcx + {rep1}], rdx",
            "mov qword ptr [rcx + {rep2}], rax",
            "neg rdx",
            "mov r14, rdx",
            // hashTable[hash(ip0)] = ip0 - base
            "mov rax, qword ptr [rdi]",
            "imul rax, qword ptr [rcx + {prime}]",
            "shrx rax, rax, r15",
            "mov edx, edi",
            "sub edx, r12d",
            "mov dword ptr [r11 + 4*rax], edx",
            // storeSeq(0, anchor, iend, REPCODE1, rLength); ip0 += rLength
            store_rep_asm!("rcx", "rdi", "r8", "r10"),
            "cmp rdi, qword ptr [rcx + {ilimit}]",
            "ja 20b",
            "mov rdx, qword ptr [rcx + {rep2}]",
            "jmp 55b",
            //
            // _cleanup
            "90:",
            "pop rbp",
            "pop rbx",
            prime = const offset_of!(FastCtx, prime),
            step_size = const offset_of!(FastCtx, step_size),
            step = const offset_of!(FastCtx, step),
            lim = const offset_of!(FastCtx, lim),
            next_step = const offset_of!(FastCtx, next_step),
            anchor = const offset_of!(FastCtx, anchor),
            iend = const offset_of!(FastCtx, iend),
            ilimit = const offset_of!(FastCtx, ilimit),
            prefix_start = const offset_of!(FastCtx, prefix_start),
            lit_limit_w = const offset_of!(FastCtx, lit_limit_w),
            lit = const offset_of!(FastCtx, lit),
            seq = const offset_of!(FastCtx, seq),
            rep1 = const offset_of!(FastCtx, rep1),
            rep2 = const offset_of!(FastCtx, rep2),
            llt = const offset_of!(FastCtx, llt),
            llpos = const offset_of!(FastCtx, llpos),
            dummy = sym DUMMY,
            in("rcx") ctx as *mut FastCtx,
            inout("rdi") ip0 => _,
            in("r11") ht,
            in("r12") base,
            in("r13") low,
            inout("r14") rep_disp => _,
            in("r15") shift,
            out("rax") _,
            out("rdx") _,
            out("rsi") _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("xmm0") _,
            out("xmm1") _,
        );
    }
}

/// The per-match state of the `dfast` noDict block loop, in memory (the
/// asm reads it by field offset through one register).
#[repr(C)]
pub(super) struct DfastCtx {
    /// Short hash: `(read64(p) * prime_s) >> shift_s` (see
    /// `fast::hash_prime`).
    pub(super) prime_s: u64,
    /// Long hash: `(read64(p) * prime_l) >> shift_l` (`ZSTD_hash8Ptr`).
    pub(super) prime_l: u64,
    /// `64 - chainLog`.
    pub(super) shift_s: u64,
    /// `64 - hashLog`.
    pub(super) shift_l: u64,
    /// `prefixLowestIndex`.
    pub(super) low: u64,
    /// `1 - offset_1` (wrapping): the repcode candidate of `ip + 1` is at
    /// `ip + rep_disp`.
    pub(super) rep_disp: usize,
    /// Current `step`.
    pub(super) step: usize,
    /// Search while `ip < lim = min(nextStep, ilimit + 1 - step)`, the two
    /// end-of-position tests folded into one.
    pub(super) lim: usize,
    pub(super) next_step: usize,
    pub(super) anchor: *const u8,
    pub(super) iend: *const u8,
    /// `iend - HASH_READ_SIZE`.
    pub(super) ilimit: *const u8,
    /// `base + prefixLowestIndex`.
    pub(super) prefix_lowest: *const u8,
    /// `iend - WILDCOPY_OVERLENGTH` (saturating), see [`FastCtx`].
    pub(super) lit_limit_w: usize,
    pub(super) lit: *mut u8,
    pub(super) seq: *mut SeqDef,
    pub(super) off1: u64,
    pub(super) off2: u64,
    pub(super) llt: u64,
    pub(super) llpos: *mut SeqDef,
    /// The search position of the current match (`curr`), for the
    /// complementary insertion.
    pub(super) curr_pos: *const u8,
}

/// `ZSTD_compressBlock_doubleFast_noDict_generic` from the top of its
/// outer loop with `ip` to `_cleanup`, for `offset_1 != 0`. Writes the
/// sequences at `ctx.lit` / `ctx.seq` and leaves the state for the cleanup
/// in `ctx` (`anchor`, `off1`, `off2`, `lit`, `seq`, `llt`, `llpos`).
///
/// # Safety
/// The contract of `dfast::no_dict_generic` at the top of its outer loop:
/// `ip` and `ctx.anchor` are positions in `[base + low, ctx.iend)` with
/// `anchor <= ip`; `ctx.ilimit = iend - 8`; `ctx.off1 != 0` and
/// `ctx.rep_disp = 1 - off1`, with `ip + 1 - off1` in the window (and
/// `off2` likewise if nonzero); `hash_long` / `hash_small` have `1 << (64 -
/// shift_l)` / `1 << (64 - shift_s)` entries; the sequence store has room
/// for the block's sequences and literals at `ctx.lit` / `ctx.seq`
/// (`SeqStore::reserve`), with `WILDCOPY_OVERLENGTH` bytes of slack after
/// the literals; the CPU has BMI1 and BMI2.
#[inline(always)]
pub(super) unsafe fn dfast_block(
    ctx: &mut DfastCtx,
    ip: *const u8,
    hash_long: *mut u32,
    hash_small: *mut u32,
    base: *const u8,
) {
    debug_assert!(ctx.off1 != 0 && ctx.rep_disp == 1usize.wrapping_sub(ctx.off1 as usize));
    // With LDM the range may be empty or tiny (see `fast_block`).
    debug_assert!(ctx.anchor <= ip && ip <= ctx.iend.wrapping_add(1));
    debug_assert!(ctx.ilimit == ctx.iend.wrapping_sub(8));
    // SAFETY: per the caller: the reads, table writes and sequence store
    // writes are those of the Rust loop. `rbx` and `rbp` are saved and
    // restored in the template (they can't be `asm!` operands).
    unsafe {
        asm!(
            "push rbx",
            "push rbp",
            "mov rbp, rdx",
            "jmp 20f",
            // rdi: ip, rsi: ip1, r14: hl0, rbx: hs0, r15: idxl0, r9: idxs0,
            // r11: base, rcx: hashLong, r8: hashSmall, r13: 1 - offset_1,
            // r10/r12: short/long hash shifts, rbp: ctx. Both table lookups
            // are pipelined one position ahead (libzstd only pipelines the
            // long one): `hashSmall[hs1]` is read after this position's
            // write, the only one in between, so the value is the one
            // libzstd reads at `ip1`. An invalid candidate (index below
            // the window) reads `ctx` instead of the window (any readable
            // address does, like libzstd's dummy array); the index check
            // rejects a match on it.
            ".p2align 5",
            "2:",
            "mov rax, qword ptr [rdi]",
            // update hash tables: hashLong[hl0] = hashSmall[hs0] = curr
            "mov edx, edi",
            "sub edx, r11d",
            "mov dword ptr [rcx + 4*r14], edx",
            "mov dword ptr [r8 + 4*rbx], edx",
            // check repcode at ip + 1
            "mov edx, dword ptr [rdi + 1]",
            "cmp edx, dword ptr [rdi + r13]",
            "je 30f",
            // hl1 = hash_l(ip1), hs1 = hash_s(ip1)
            "mov r14, qword ptr [rsi]",
            "mov rbx, r14",
            "imul r14, qword ptr [rbp + {prime_l}]",
            "shrx r14, r14, r12",
            "imul rbx, qword ptr [rbp + {prime_s}]",
            "shrx rbx, rbx, r10",
            // long candidate: select, compare 8 bytes
            "lea rdx, [r11 + r15]",
            "cmp r15d, dword ptr [rbp + {low}]",
            "cmovb rdx, rbp",
            "cmp rax, qword ptr [rdx]",
            "je 40f",
            "5:",
            // idxl1 = hashLong[hl1]
            "mov r15d, dword ptr [rcx + 4*r14]",
            // short candidate: select, compare 4 bytes
            "lea rdx, [r11 + r9]",
            "cmp r9d, dword ptr [rbp + {low}]",
            "cmovb rdx, rbp",
            "cmp eax, dword ptr [rdx]",
            "je 60f",
            "7:",
            // idxs1 = hashSmall[hs1]
            "mov r9d, dword ptr [r8 + 4*rbx]",
            // advance: ip = ip1, ip1 += step, while ip < lim
            "mov rdi, rsi",
            "add rsi, qword ptr [rbp + {step}]",
            "cmp rdi, qword ptr [rbp + {lim}]",
            "jb 2b",
            // step increase due (ip1 >= nextStep, on the new ip)?
            "cmp rdi, qword ptr [rbp + {next_step}]",
            "jb 8f",
            "prefetcht0 byte ptr [rdi + 64]",
            "prefetcht0 byte ptr [rdi + 128]",
            "inc qword ptr [rbp + {step}]",
            "add qword ptr [rbp + {next_step}], 256",
            "8:",
            // ip1 = ip + step; while (ip1 <= ilimit)
            "mov rax, qword ptr [rbp + {step}]",
            "lea rsi, [rdi + rax]",
            "cmp rsi, qword ptr [rbp + {ilimit}]",
            "ja 90f",
            // lim = min(nextStep, ilimit + 1 - step)
            "mov rdx, qword ptr [rbp + {ilimit}]",
            "inc rdx",
            "sub rdx, rax",
            "mov rax, qword ptr [rbp + {next_step}]",
            "cmp rax, rdx",
            "cmova rax, rdx",
            "mov qword ptr [rbp + {lim}], rax",
            "jmp 2b",
            //
            // outer loop top: step = 1, nextStep = ip + kStepIncr, ip1 = ip
            // + 1; stop if ip1 > ilimit; reload the shifts and the repcode
            // displacement (the match path uses their registers); hash ip.
            "20:",
            "mov qword ptr [rbp + {step}], 1",
            "lea rax, [rdi + 256]",
            "mov qword ptr [rbp + {next_step}], rax",
            "lea rsi, [rdi + 1]",
            "cmp rsi, qword ptr [rbp + {ilimit}]",
            "ja 90f",
            "mov rdx, qword ptr [rbp + {ilimit}]",
            "cmp rax, rdx",
            "cmova rax, rdx",
            "mov qword ptr [rbp + {lim}], rax",
            "mov r10, qword ptr [rbp + {shift_s}]",
            "mov r12, qword ptr [rbp + {shift_l}]",
            "mov r13, qword ptr [rbp + {rep_disp}]",
            "mov r14, qword ptr [rdi]",
            "mov rbx, r14",
            "imul r14, qword ptr [rbp + {prime_l}]",
            "shrx r14, r14, r12",
            "imul rbx, qword ptr [rbp + {prime_s}]",
            "shrx rbx, rbx, r10",
            "mov r15d, dword ptr [rcx + 4*r14]",
            "mov r9d, dword ptr [r8 + 4*rbx]",
            "jmp 2b",
            //
            // repcode match at ip + 1: mLength = count(ip + 5, ip + 5 -
            // offset_1, iend) + 4; ip++; storeSeq(ip - anchor, anchor, iend,
            // REPCODE1_TO_OFFBASE, mLength); _match_stored.
            "30:",
            "mov qword ptr [rbp + {curr_pos}], rdi",
            "lea rax, [rdi + 5]",
            "lea rdx, [rdi + r13 + 4]",
            count_asm!("rbp", "rax", "rdx", "rsi", "esi", "si", "sil", "r10"),
            "inc rdi",
            "mov r12, rax",
            "sub r12, rdi",
            "mov r9d, 1",
            "jmp 50f",
            //
            // long match at ip (the candidate must be valid, the dummy may
            // have matched): mLength = count(ip + 8, matchl0 + 8, iend) +
            // 8; offset = ip - matchl0; catch up.
            "40:",
            "cmp r15d, dword ptr [rbp + {low}]",
            "jb 5b",
            "mov qword ptr [rbp + {curr_pos}], rdi",
            "lea rdx, [r11 + r15]",
            "mov r9d, edi",
            "sub r9d, edx",
            "mov rbx, rdx",
            "lea rax, [rdi + 8]",
            "add rdx, 8",
            count_asm!("rbp", "rax", "rdx", "r10", "r10d", "r10w", "r10b", "r12"),
            "mov r12, rax",
            "sub r12, rdi",
            "jmp 47f",
            //
            // short match at ip (_search_next_long): mLength = count(ip +
            // 4, matchs0 + 4, iend) + 4; offset = ip - matchs0; then the
            // long candidate of ip1 (idxl1 > prefixLowestIndex, 8 bytes
            // equal) replaces it if longer.
            "60:",
            "cmp r9d, dword ptr [rbp + {low}]",
            "jb 7b",
            "mov qword ptr [rbp + {curr_pos}], rdi",
            "lea rdx, [r11 + r9]",
            "mov rbx, rdx",
            "mov r9d, edi",
            "sub r9d, edx",
            "lea rax, [rdi + 4]",
            "add rdx, 4",
            count_asm!("rbp", "rax", "rdx", "r10", "r10d", "r10w", "r10b", "r12"),
            "mov r12, rax",
            "sub r12, rdi",
            "cmp r15d, dword ptr [rbp + {low}]",
            "jbe 47f",
            "lea rdx, [r11 + r15]",
            "mov rax, qword ptr [rdx]",
            "cmp rax, qword ptr [rsi]",
            "jne 47f",
            "mov r15, rdx",
            "lea rax, [rsi + 8]",
            "add rdx, 8",
            count_asm!("rbp", "rax", "rdx", "r10", "r10d", "r10w", "r10b", "r13"),
            "sub rax, rsi",
            "cmp rax, r12",
            "jbe 47f",
            // use the long match instead
            "mov rdi, rsi",
            "mov r12, rax",
            "mov r9d, edi",
            "sub r9d, r15d",
            "mov rbx, r15",
            //
            // complete backward (rdi: ip, rbx: match, r12: mLength): while
            // ip > anchor and match > prefixLowest and ip[-1] == match[-1]
            "47:",
            "mov rax, qword ptr [rbp + {anchor}]",
            "mov rdx, qword ptr [rbp + {prefix_lowest}]",
            "48:",
            "cmp rdi, rax",
            "jbe 49f",
            "cmp rbx, rdx",
            "jbe 49f",
            "movzx r10d, byte ptr [rdi - 1]",
            "cmp r10b, byte ptr [rbx - 1]",
            "jne 49f",
            "dec rdi",
            "dec rbx",
            "inc r12",
            "jmp 48b",
            //
            // _match_found (rdi: ip, r9d: offset, r12: mLength, rsi: ip1,
            // r14: hl1): offset_2 = offset_1; offset_1 = offset; if step <
            // 4: hashLong[hl1] = ip1 - base; offcode = OFFSET_TO_OFFBASE.
            "49:",
            "mov rax, qword ptr [rbp + {off1}]",
            "mov qword ptr [rbp + {off2}], rax",
            "mov qword ptr [rbp + {off1}], r9",
            "lea rax, [r9 - 1]",
            "neg rax",
            "mov qword ptr [rbp + {rep_disp}], rax",
            "cmp qword ptr [rbp + {step}], 4",
            "jae 51f",
            "mov eax, esi",
            "sub eax, r11d",
            "mov dword ptr [rcx + 4*r14], eax",
            "51:",
            "add r9d, 3",
            //
            // storeSeq(ip - anchor, anchor, iend, r9d, mLength); ip +=
            // mLength; anchor = ip (_match_stored)
            "50:",
            store_seq_asm!("rbp", "rdi", "r9d", "r12", "r10"),
            "cmp rdi, qword ptr [rbp + {ilimit}]",
            "ja 20b",
            // complementary insertion: indexToInsert = curr + 2 (its
            // position p = curr_pos + 2): hashLong[hash_l(p)] =
            // hashSmall[hash_s(p)] = indexToInsert; hashLong[hash_l(ip -
            // 2)] = ip - 2 - base; hashSmall[hash_s(ip - 1)] = ip - 1 - base
            "mov r10, qword ptr [rbp + {shift_s}]",
            "mov r12, qword ptr [rbp + {shift_l}]",
            "mov rsi, qword ptr [rbp + {curr_pos}]",
            "add rsi, 2",
            "mov r9d, esi",
            "sub r9d, r11d",
            "mov rax, qword ptr [rsi]",
            "mov rdx, rax",
            "imul rax, qword ptr [rbp + {prime_l}]",
            "shrx rax, rax, r12",
            "mov dword ptr [rcx + 4*rax], r9d",
            "imul rdx, qword ptr [rbp + {prime_s}]",
            "shrx rdx, rdx, r10",
            "mov dword ptr [r8 + 4*rdx], r9d",
            "mov rax, qword ptr [rdi - 2]",
            "imul rax, qword ptr [rbp + {prime_l}]",
            "shrx rax, rax, r12",
            "lea edx, [rdi - 2]",
            "sub edx, r11d",
            "mov dword ptr [rcx + 4*rax], edx",
            "mov rax, qword ptr [rdi - 1]",
            "imul rax, qword ptr [rbp + {prime_s}]",
            "shrx rax, rax, r10",
            "lea edx, [rdi - 1]",
            "sub edx, r11d",
            "mov dword ptr [r8 + 4*rax], edx",
            // check immediate repcode: while ip <= ilimit and offset_2 > 0
            // and read32(ip) == read32(ip - offset_2)
            "mov rdx, qword ptr [rbp + {off2}]",
            "test edx, edx",
            "jz 20b",
            "61:",
            "mov eax, dword ptr [rdi]",
            "mov rsi, rdi",
            "sub rsi, rdx",
            "cmp eax, dword ptr [rsi]",
            "jne 20b",
            // rLength = count(ip + 4, ip + 4 - offset_2, iend) + 4
            "lea rax, [rdi + 4]",
            "lea rdx, [rsi + 4]",
            count_asm!("rbp", "rax", "rdx", "rsi", "esi", "si", "sil", "r9"),
            "mov rbx, rax",
            "sub rbx, rdi",
            // swap offset_2 <=> offset_1 (rep_disp reloaded at the top)
            "mov rax, qword ptr [rbp + {off1}]",
            "mov rdx, qword ptr [rbp + {off2}]",
            "mov qword ptr [rbp + {off1}], rdx",
            "mov qword ptr [rbp + {off2}], rax",
            "lea rax, [rdx - 1]",
            "neg rax",
            "mov qword ptr [rbp + {rep_disp}], rax",
            // hashSmall[hash_s(ip)] = hashLong[hash_l(ip)] = ip - base
            "mov rax, qword ptr [rdi]",
            "mov rdx, rax",
            "mov r9d, edi",
            "sub r9d, r11d",
            "imul rax, qword ptr [rbp + {prime_s}]",
            "shrx rax, rax, r10",
            "mov dword ptr [r8 + 4*rax], r9d",
            "imul rdx, qword ptr [rbp + {prime_l}]",
            "shrx rdx, rdx, r12",
            "mov dword ptr [rcx + 4*rdx], r9d",
            // storeSeq(0, anchor, iend, REPCODE1, rLength); ip += rLength
            store_rep_asm!("rbp", "rdi", "rbx", "rsi"),
            "cmp rdi, qword ptr [rbp + {ilimit}]",
            "ja 20b",
            "mov rdx, qword ptr [rbp + {off2}]",
            "jmp 61b",
            //
            // _cleanup
            "90:",
            "pop rbp",
            "pop rbx",
            prime_s = const offset_of!(DfastCtx, prime_s),
            prime_l = const offset_of!(DfastCtx, prime_l),
            shift_s = const offset_of!(DfastCtx, shift_s),
            shift_l = const offset_of!(DfastCtx, shift_l),
            low = const offset_of!(DfastCtx, low),
            rep_disp = const offset_of!(DfastCtx, rep_disp),
            step = const offset_of!(DfastCtx, step),
            lim = const offset_of!(DfastCtx, lim),
            next_step = const offset_of!(DfastCtx, next_step),
            anchor = const offset_of!(DfastCtx, anchor),
            iend = const offset_of!(DfastCtx, iend),
            ilimit = const offset_of!(DfastCtx, ilimit),
            prefix_lowest = const offset_of!(DfastCtx, prefix_lowest),
            lit_limit_w = const offset_of!(DfastCtx, lit_limit_w),
            lit = const offset_of!(DfastCtx, lit),
            seq = const offset_of!(DfastCtx, seq),
            llt = const offset_of!(DfastCtx, llt),
            llpos = const offset_of!(DfastCtx, llpos),
            off1 = const offset_of!(DfastCtx, off1),
            off2 = const offset_of!(DfastCtx, off2),
            curr_pos = const offset_of!(DfastCtx, curr_pos),
            inout("rdx") ctx as *mut DfastCtx => _,
            inout("rdi") ip => _,
            in("rcx") hash_long,
            in("r8") hash_small,
            in("r11") base,
            out("rax") _,
            out("rsi") _,
            out("r9") _,
            out("r10") _,
            out("r12") _,
            out("r13") _,
            out("r14") _,
            out("r15") _,
            out("xmm0") _,
            out("xmm1") _,
        );
    }
}
