//! x86-64 assembly for the position-search loops of the `fast` and `dfast`
//! match finders (the inner loops of libzstd's
//! `ZSTD_compressBlock_fast_noDict_generic` and
//! `ZSTD_compressBlock_doubleFast_noDict_generic`).
//!
//! These loops need more values per position than x86-64 has registers
//! (positions, step, limits, tables, base, low index, repcode, hash primes
//! and shifts, the pipelined next hash and candidate), and LLVM keeps the
//! wrong ones on the stack: it reloads base, limits, shifts and primes
//! every position and turns the `cmov` candidate selection back into a
//! branch. Here the values used every position stay in registers, the rest
//! are memory operands, and each candidate test is a `cmov` plus one
//! compare and branch (libzstd's `ZSTD_selectAddr` scheme).
//!
//! The asm only searches: it writes the searched positions to the tables
//! and returns at the first candidate (repcode or table match) or when the
//! position reaches the caller's limit, with the same state and exit code
//! as the Rust loops (`fast::fast_search_rust`, `dfast::search_rust`),
//! which remain the fallback (no BMI2, the `portable` feature, other
//! targets, and a zero repcode). The caller counts and stores the matches.
//! Uses BMI2 (`shrx`): call only when the CPU has it.
//!
//! The hash is `(read64(p) * prime) >> (64 - hBits)` for every `mls`
//! (libzstd's `ZSTD_hashPtr`, the prime pre-shifted: `fast::hash_prime`).
#![allow(unsafe_code)]

use super::dfast::{Cursor, Search};
use super::fast::{FastCursor, FastSearch};
use core::arch::asm;
use core::mem::offset_of;

/// Read instead of an invalid candidate (index below the window's low
/// limit), so the test stays branch-free; a match on it is rejected by a
/// second index check.
static DUMMY: [u8; 8] = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];

/// `dfast::search_rust` in asm: the same table writes, candidate tests and
/// exits (`dfast::EXIT_*`, returned).
///
/// `rbx` and `rbp` can't be `asm!` operands, so the template saves and
/// restores them (and keeps `lim`, `step` and the primes on its stack, so
/// no `nostack`).
///
/// # Safety
/// The contract of `dfast::search_rust`, with `offset_1 != 0` and
/// `s.rep_disp = 1 - offset_1`; the CPU must have BMI2.
#[inline(always)]
pub(super) unsafe fn dfast_search(s: &Search, c: &mut Cursor) -> u32 {
    let code: u64;
    // SAFETY: per the caller: the reads and table writes are those of the
    // Rust loop. The stack is restored before the end.
    unsafe {
        asm!(
            "push rbx",
            "push rbp",
            "push qword ptr [rax + {pl}]",
            "push qword ptr [rax + {ps}]",
            "push qword ptr [rax + {step}]",
            "push qword ptr [rax + {lim}]",
            // [rsp]: lim, [rsp + 8]: step, [rsp + 16]: prime_s,
            // [rsp + 24]: prime_l.
            // rdi: ip, rsi: ip1, r14: hl0, r15: idxl0, r11: base,
            // rcx: hashLong, r8: hashSmall, r9: prefixLowestIndex,
            // r13: 1 - offset_1, r10/r12: short/long hash shifts.
            ".p2align 5",
            "2:",
            // hs0 = hash_s(ip); idxs0 = hashSmall[hs0] (ebx)
            "mov rax, qword ptr [rdi]",
            "mov rdx, rax",
            "imul rdx, qword ptr [rsp + 16]",
            "shrx rdx, rdx, r10",
            // curr = ip - base
            "mov ebp, edi",
            "sub ebp, r11d",
            "mov ebx, dword ptr [r8 + 4*rdx]",
            // update hash tables
            "mov dword ptr [rcx + 4*r14], ebp",
            "mov dword ptr [r8 + 4*rdx], ebp",
            // check repcode at ip + 1
            "mov edx, dword ptr [rdi + 1]",
            "cmp edx, dword ptr [rdi + r13]",
            "je 3f",
            // hl1 = hash_l(ip1) (r14)
            "mov r14, qword ptr [rsi]",
            "imul r14, qword ptr [rsp + 24]",
            "shrx r14, r14, r12",
            // long candidate: select, compare 8 bytes
            "mov edx, r15d",
            "add rdx, r11",
            "lea rbp, [rip + {dummy}]",
            "cmp r15d, r9d",
            "cmovae rbp, rdx",
            "cmp rax, qword ptr [rbp]",
            "je 4f",
            "5:",
            // idxl1 = hashLong[hl1] (r15)
            "mov r15d, dword ptr [rcx + 4*r14]",
            // short candidate: select, compare 4 bytes
            "mov edx, ebx",
            "add rdx, r11",
            "lea rbp, [rip + {dummy}]",
            "cmp ebx, r9d",
            "cmovae rbp, rdx",
            "cmp eax, dword ptr [rbp]",
            "je 6f",
            "7:",
            // advance: ip = ip1, ip1 += step, until ip >= lim
            "mov rdi, rsi",
            "add rsi, qword ptr [rsp + 8]",
            "cmp rdi, qword ptr [rsp]",
            "jb 2b",
            "mov eax, 3",
            "jmp 9f",
            // repcode match
            "3:",
            "xor eax, eax",
            "jmp 9f",
            // long: bytes equal; a match only if the candidate was valid
            "4:",
            "cmp r15d, r9d",
            "jb 5b",
            "mov eax, 1",
            "jmp 9f",
            // short: likewise
            "6:",
            "cmp ebx, r9d",
            "jb 7b",
            "mov eax, 2",
            "9:",
            "mov edx, ebx",
            "add rsp, 32",
            "pop rbp",
            "pop rbx",
            pl = const offset_of!(Search, prime_l),
            ps = const offset_of!(Search, prime_s),
            step = const offset_of!(Search, step),
            lim = const offset_of!(Search, lim),
            dummy = sym DUMMY,
            inout("rax") s as *const Search => code,
            in("rcx") s.hash_long,
            out("rdx") c.idxs,
            inout("rdi") c.ip,
            inout("rsi") c.ip1,
            in("r8") s.hash_small,
            in("r9") s.low,
            in("r10") s.shift_s,
            in("r11") s.base,
            in("r12") s.shift_l,
            in("r13") s.rep_disp,
            inout("r14") c.hl,
            inout("r15") c.idxl,
        );
    }
    code as u32
}

/// `fast::fast_search_rust` in asm: the same table writes, candidate tests
/// and exits (`fast::FAST_*`, returned). Two positions per iteration like
/// libzstd; all state in registers (`rcx` points to the invariants).
///
/// # Safety
/// The contract of `fast::fast_search_rust`, with `rep_offset1 != 0` and
/// `s.rep_disp = -rep_offset1`; the CPU must have BMI2.
#[inline(always)]
pub(super) unsafe fn fast_search(s: &FastSearch, c: &mut FastCursor) -> u32 {
    let code: u64;
    let dummy = DUMMY.as_ptr();
    // SAFETY: per the caller: the reads and table writes are those of the
    // Rust loop.
    unsafe {
        asm!(
            // rdi: ip0, rsi: ip2 (ip1 = ip0 + 1, ip3 = ip2 + 1),
            // r8: hash0, r9: hash1, r10: matchIdx (of ip0),
            // r11: hashTable, r12: base, r13: prefixStartIndex,
            // r14: -rep_offset1, r15: 64 - hashLog.
            ".p2align 5",
            "2:",
            // load repcode match for ip[2]
            "mov eax, dword ptr [rsi + r14]",
            // write back hash table entry
            "mov edx, edi",
            "sub edx, r12d",
            "mov dword ptr [r11 + 4*r8], edx",
            // check repcode at ip[2]
            "cmp eax, dword ptr [rsi]",
            "je 3f",
            // table candidate of ip0: select, compare 4 bytes
            "mov edx, r10d",
            "add rdx, r12",
            "cmp r10d, r13d",
            "cmovb rdx, {dummy}",
            "mov eax, dword ptr [rdi]",
            "cmp eax, dword ptr [rdx]",
            "je 4f",
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
            "mov edx, r10d",
            "add rdx, r12",
            "cmp r10d, r13d",
            "cmovb rdx, {dummy}",
            "mov eax, dword ptr [rdi]",
            "cmp eax, dword ptr [rdx]",
            "je 6f",
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
            "mov eax, 3",
            "jmp 9f",
            "3:",
            "xor eax, eax",
            "jmp 9f",
            // bytes equal: a match only if the candidate was valid
            "4:",
            "cmp r10d, r13d",
            "jb 5b",
            "mov eax, 1",
            "jmp 9f",
            "6:",
            "cmp r10d, r13d",
            "jb 7b",
            "mov eax, 2",
            "9:",
            prime = const offset_of!(FastSearch, prime),
            step = const offset_of!(FastSearch, step),
            lim = const offset_of!(FastSearch, lim),
            dummy = in(reg) dummy,
            in("rcx") s as *const FastSearch,
            out("rax") code,
            out("rdx") _,
            inout("rdi") c.ip0,
            inout("rsi") c.ip2,
            inout("r8") c.hash0,
            inout("r9") c.hash1,
            inout("r10") c.match_idx,
            in("r11") s.ht,
            in("r12") s.base,
            in("r13") s.low,
            in("r14") s.rep_disp,
            in("r15") s.shift,
            options(nostack),
        );
    }
    code as u32
}
