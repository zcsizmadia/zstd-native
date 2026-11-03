//! Huffman literal decoding.
//!
//! The table is libzstd's single-symbol (`X1`) layout: one `u16` per
//! `dt_log`-bit prefix, `symbol << 8 | code_length`, with `dt_log` at least
//! 11 (shorter tables are scaled up, as libzstd does, so the fast loop can
//! use a constant shift).
//!
//! Four-stream literals go through a fast loop that decodes 5 symbols from
//! each stream per iteration with no bounds checks (the iteration count is
//! computed up front from the input and output room), then each stream is
//! finished with the generic decoder. Every stream must be consumed exactly.

use crate::bits::{BitReader, Status, read_u64_le};
use crate::cpu::Features;
use crate::error::{Error, Result};
use crate::fse;

pub(crate) const HUF_TABLELOG_MAX: u32 = 12;
const FAST_TABLELOG: u32 = 11;

#[derive(Clone)]
pub(crate) struct HufTable {
    pub dt: [u16; 1 << HUF_TABLELOG_MAX],
    pub log: u32,
    /// Double-symbol table (libzstd's `X2`), built on demand from `dt`
    /// when `log` is 11: `s1 | s2 << 8 | bits << 16 | count << 24`.
    dt2: [u32; 1 << FAST_TABLELOG],
    x2_ready: bool,
    /// Scratch for `build_x2`.
    x2_tmpl: [u32; 1 << FAST_TABLELOG],
}

impl core::fmt::Debug for HufTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HufTable")
            .field("log", &self.log)
            .finish_non_exhaustive()
    }
}

impl HufTable {
    pub(crate) const fn new() -> Self {
        HufTable {
            dt: [0; 1 << HUF_TABLELOG_MAX],
            log: 0,
            dt2: [0; 1 << FAST_TABLELOG],
            x2_ready: false,
            x2_tmpl: [0; 1 << FAST_TABLELOG],
        }
    }

    /// Reads a Huffman tree description and builds the table
    /// (libzstd's `HUF_readStats` + `HUF_readDTableX1`). Returns the bytes used.
    pub(crate) fn read(&mut self, src: &[u8]) -> Result<usize> {
        let mut weights = [0u8; 256];
        let &header = src.first().ok_or(Error::Corrupted)?;
        let mut isize = header as usize;
        let osize;
        if isize >= 128 {
            osize = isize - 127;
            isize = osize.div_ceil(2);
            if isize + 1 > src.len() || osize >= 256 {
                return Err(Error::Corrupted);
            }
            for n in (0..osize).step_by(2) {
                let b = src[1 + n / 2];
                weights[n] = b >> 4;
                weights[n + 1] = b & 15;
            }
        } else {
            if isize + 1 > src.len() {
                return Err(Error::Corrupted);
            }
            osize = fse::decompress_weights(&src[1..1 + isize], &mut weights)?;
        }

        let mut rank = [0u32; HUF_TABLELOG_MAX as usize + 1];
        let mut total = 0u32;
        for &w in &weights[..osize] {
            if u32::from(w) > HUF_TABLELOG_MAX {
                return Err(Error::Corrupted);
            }
            rank[w as usize] += 1;
            total += (1 << w) >> 1;
        }
        if total == 0 {
            return Err(Error::Corrupted);
        }
        let table_log = 32 - total.leading_zeros(); // highbit + 1
        if table_log > HUF_TABLELOG_MAX {
            return Err(Error::Corrupted);
        }
        let rest = (1u32 << table_log) - total;
        if !rest.is_power_of_two() {
            return Err(Error::Corrupted);
        }
        let last = rest.trailing_zeros() + 1;
        weights[osize] = last as u8;
        rank[last as usize] += 1;
        if rank[1] < 2 || rank[1] & 1 != 0 {
            return Err(Error::Corrupted);
        }
        let nb_symbols = osize + 1;

        // Scale up to the fast table log (HUF_rescaleStats).
        let mut log = table_log;
        if log < FAST_TABLELOG {
            let scale = FAST_TABLELOG - log;
            for w in &mut weights[..nb_symbols] {
                if *w != 0 {
                    *w += scale as u8;
                }
            }
            for r in (1..=FAST_TABLELOG as usize).rev() {
                rank[r] = if r > scale as usize {
                    rank[r - scale as usize]
                } else {
                    0
                };
            }
            log = FAST_TABLELOG;
        }

        // Symbols sorted by weight, then by value; fill in canonical order.
        let mut start = [0u32; HUF_TABLELOG_MAX as usize + 2];
        let mut next = 0u32;
        for w in 0..=log as usize {
            start[w] = next;
            next += rank[w];
        }
        let mut sorted = [0u8; 256];
        for (s, &w) in weights[..nb_symbols].iter().enumerate() {
            sorted[start[w as usize] as usize] = s as u8;
            start[w as usize] += 1;
        }
        let mut pos = 0usize;
        let mut idx = rank[0] as usize;
        for w in 1..=log {
            let count = rank[w as usize] as usize;
            let len = 1usize << (w - 1);
            let nb_bits = (log + 1 - w) as u16;
            for &s in &sorted[idx..idx + count] {
                let e = (u16::from(s) << 8) | nb_bits;
                self.dt[pos..pos + len].fill(e);
                pos += len;
            }
            idx += count;
        }
        debug_assert_eq!(pos, 1 << log);
        self.log = log;
        self.x2_ready = false;
        Ok(isize + 1)
    }
    /// Builds the double-symbol table: each entry decodes the first symbol
    /// and, when its code fits in the remaining bits of the 11-bit window,
    /// the next one too. Same symbols and bit counts as two X1 lookups, so
    /// X1 and X2 decode identically.
    fn build_x2(&mut self) {
        debug_assert_eq!(self.log, FAST_TABLELOG);
        const SIZE: usize = 1 << FAST_TABLELOG;
        // An X1 entry with code length n1 fills a run of 2^(11 - n1) cells
        // starting at a multiple of the run length, so cell k of every such
        // run sees the same next bits, `k << n1`. The second-symbol part is
        // therefore one template per code length, ORed with the first symbol.
        // (Templates of distinct lengths, 1 to 11 bits, sum to under SIZE cells.)
        let tmpl = &mut self.x2_tmpl;
        let mut tmpl_at = [usize::MAX; FAST_TABLELOG as usize + 1];
        let mut used = 0;
        let mut i = 0;
        while i < SIZE {
            let e1 = u32::from(self.dt[i]);
            let n1 = e1 & 0xFF;
            let s1 = e1 >> 8;
            let r = FAST_TABLELOG - n1;
            let run = 1usize << r;
            if tmpl_at[n1 as usize] == usize::MAX {
                tmpl_at[n1 as usize] = used;
                for (k, t) in tmpl[used..used + run].iter_mut().enumerate() {
                    let e2 = u32::from(self.dt[k << n1]);
                    let n2 = e2 & 0xFF;
                    *t = if n2 <= r {
                        ((e2 >> 8) << 8) | ((n1 + n2) << 16) | (2 << 24)
                    } else {
                        (n1 << 16) | (1 << 24)
                    };
                }
                used += run;
            }
            let t = &tmpl[tmpl_at[n1 as usize]..][..run];
            for (o, &t) in self.dt2[i..i + run].iter_mut().zip(t) {
                *o = t | s1;
            }
            i += run;
        }
        self.x2_ready = true;
    }
}

/// libzstd's `HUF_selectDecoder`: whether the double-symbol decoder is
/// expected to be faster, from the literals' size and compression ratio
/// (libzstd's measured table-build and decode times).
pub(crate) fn prefer_x2(dst_size: usize, csrc_size: usize) -> bool {
    const ALGO_TIME: [[(u32, u32); 2]; 16] = [
        [(0, 0), (1, 1)],
        [(0, 0), (1, 1)],
        [(150, 216), (381, 119)],
        [(170, 205), (514, 112)],
        [(177, 199), (539, 110)],
        [(197, 194), (644, 107)],
        [(221, 192), (735, 107)],
        [(256, 189), (881, 106)],
        [(359, 188), (1167, 109)],
        [(582, 187), (1570, 114)],
        [(688, 187), (1712, 122)],
        [(825, 186), (1965, 136)],
        [(976, 185), (2131, 150)],
        [(1180, 186), (2070, 175)],
        [(1377, 185), (1731, 202)],
        [(1412, 185), (1695, 202)],
    ];
    if dst_size == 0 {
        return false;
    }
    let q = if csrc_size >= dst_size {
        15
    } else {
        csrc_size * 16 / dst_size
    };
    let d256 = (dst_size >> 8) as u32;
    let [(t0, d0), (t1, d1)] = ALGO_TIME[q];
    let time0 = t0 + d0 * d256;
    let mut time1 = t1 + d1 * d256;
    time1 += time1 >> 5;
    time1 < time0
}

/// Decodes one symbol (libzstd's `HUF_decodeSymbolX1`).
#[inline(always)]
fn decode_symbol(br: &mut BitReader<'_>, dt: &[u16; 1 << HUF_TABLELOG_MAX], log: u32) -> u8 {
    let e = dt[br.peek_fast(log) as usize];
    br.skip(u32::from(e & 0xFF));
    (e >> 8) as u8
}

/// Decodes `out.len()` symbols from `br` (libzstd's `HUF_decodeStreamX1`).
#[inline(always)]
fn decode_stream(out: &mut [u8], br: &mut BitReader<'_>, t: &HufTable) {
    let (dt, log) = (&t.dt, t.log);
    let mut p = 0;
    let end = out.len();
    if end > 3 {
        while (br.reload() == Status::Unfinished) & (p < end - 3) {
            out[p] = decode_symbol(br, dt, log);
            out[p + 1] = decode_symbol(br, dt, log);
            out[p + 2] = decode_symbol(br, dt, log);
            out[p + 3] = decode_symbol(br, dt, log);
            p += 4;
        }
    } else {
        br.reload();
    }
    while p < end {
        out[p] = decode_symbol(br, dt, log);
        p += 1;
    }
}

/// Single-stream literals.
pub(crate) fn decode_1x(out: &mut [u8], src: &[u8], t: &HufTable) -> Result<()> {
    let mut br = BitReader::new(src)?;
    decode_stream(out, &mut br, t);
    if !br.is_end() {
        return Err(Error::Corrupted);
    }
    Ok(())
}

/// Four-stream literals; `x2` selects the double-symbol fast loop (when the
/// table allows it). Stream tails always use the single-symbol table.
pub(crate) fn decode_4x(
    out: &mut [u8],
    src: &[u8],
    t: &mut HufTable,
    x2: bool,
    cpu: Features,
) -> Result<()> {
    if src.len() < 10 || out.len() < 6 {
        return Err(Error::Corrupted);
    }
    let l1 = u16::from_le_bytes([src[0], src[1]]) as usize;
    let l2 = u16::from_le_bytes([src[2], src[3]]) as usize;
    let l3 = u16::from_le_bytes([src[4], src[5]]) as usize;
    let Some(l4) = src.len().checked_sub(6 + l1 + l2 + l3) else {
        return Err(Error::Corrupted);
    };
    let starts = [6, 6 + l1, 6 + l1 + l2, 6 + l1 + l2 + l3];
    let lens = [l1, l2, l3, l4];
    let n = out.len();
    let seg = n.div_ceil(4);
    if 3 * seg > n {
        return Err(Error::Corrupted);
    }
    let seg_starts = [0, seg, 2 * seg, 3 * seg];
    let seg_ends = [seg, 2 * seg, 3 * seg, n];

    // (ip, bits-consumed) per stream after the fast loop.
    let mut resume: [Option<(usize, u32)>; 4] = [None; 4];
    let mut op = seg_starts;
    if t.log == FAST_TABLELOG && lens.iter().all(|&l| l >= 8) && 3 * seg < n {
        let mut ip = [0usize; 4];
        let mut bits = [0u64; 4];
        for s in 0..4 {
            ip[s] = starts[s] + lens[s] - 8;
            let last = src[starts[s] + lens[s] - 1];
            if last == 0 {
                return Err(Error::Corrupted);
            }
            bits[s] = (read_u64_le(src, ip[s]) | 1) << (last.leading_zeros() + 1);
        }
        let mut st = FastState { ip, op, bits };
        if x2 {
            if !t.x2_ready {
                t.build_x2();
            }
            fast_loop_x2_dispatch(&mut st, out, src, &starts, &seg_ends, &t.dt2, cpu);
        } else {
            fast_loop_dispatch(&mut st, out, src, &starts, &t.dt, cpu);
        }
        op = st.op;
        for s in 0..4 {
            resume[s] = Some((st.ip[s] - starts[s], st.bits[s].trailing_zeros()));
        }
    }
    for s in 0..4 {
        let stream = &src[starts[s]..starts[s] + lens[s]];
        let mut br = match resume[s] {
            Some((pos, consumed)) => BitReader::from_parts(stream, pos, consumed),
            None => BitReader::new(stream)?,
        };
        decode_stream(&mut out[op[s]..seg_ends[s]], &mut br, &*t);
        if !br.is_end() {
            return Err(Error::Corrupted);
        }
    }
    Ok(())
}

struct FastState {
    ip: [usize; 4],
    op: [usize; 4],
    bits: [u64; 4],
}

#[inline]
fn fast_loop_dispatch(
    st: &mut FastState,
    out: &mut [u8],
    src: &[u8],
    starts: &[usize; 4],
    dt: &[u16; 1 << HUF_TABLELOG_MAX],
    cpu: Features,
) {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if cpu.huf_asm {
        fast_loop_asm(st, out, src, starts, None, dt.as_ptr().cast());
        return;
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if cpu.bmi2 {
        #[allow(unsafe_code)]
        // SAFETY: the caller detected BMI2 (and BMI1/LZCNT) on this CPU.
        unsafe {
            fast_loop_bmi2(st, out, src, starts, dt)
        };
        return;
    }
    let _ = cpu;
    fast_loop(st, out, src, starts, dt);
}

#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[target_feature(enable = "bmi1,bmi2,lzcnt")]
fn fast_loop_bmi2(
    st: &mut FastState,
    out: &mut [u8],
    src: &[u8],
    starts: &[usize; 4],
    dt: &[u16; 1 << HUF_TABLELOG_MAX],
) {
    fast_loop_body(st, out, src, starts, dt);
}

fn fast_loop(
    st: &mut FastState,
    out: &mut [u8],
    src: &[u8],
    starts: &[usize; 4],
    dt: &[u16; 1 << HUF_TABLELOG_MAX],
) {
    fast_loop_body(st, out, src, starts, dt);
}

/// libzstd's `HUF_decompress4X1_usingDTable_internal_fast_c_loop`.
#[inline(always)]
#[allow(unsafe_code)]
fn fast_loop_body(
    st: &mut FastState,
    out: &mut [u8],
    src: &[u8],
    starts: &[usize; 4],
    dt: &[u16; 1 << HUF_TABLELOG_MAX],
) {
    let oend = out.len();
    let [mut ip0, mut ip1, mut ip2, mut ip3] = st.ip;
    let [mut op0, mut op1, mut op2, mut op3] = st.op;
    let [mut b0, mut b1, mut b2, mut b3] = st.bits;
    let srcp = src.as_ptr();
    let outp = out.as_mut_ptr();
    let dtp = dt.as_ptr();
    loop {
        // 5 symbols per stream per iteration, up to 55 bits < 7 bytes.
        let oiters = (oend - op3) / 5;
        let iiters = ((ip0 - starts[0]) / 7)
            .min((ip1 - starts[1]) / 7)
            .min((ip2 - starts[2]) / 7)
            .min((ip3 - starts[3]) / 7);
        let iters = oiters.min(iiters);
        if iters == 0 {
            break;
        }
        let olimit = op3 + iters * 5;
        // SAFETY: `iters` bounds every access: each stream's output stays
        // within its segment (stream 3's segment is the shortest and all
        // advance in lock step), each `ip` stays at or above its stream's
        // start (at most 7 bytes back per iteration), and `ip + 8` never
        // passes the stream's end since `ip` only decreases. Indexes into
        // `dt` are `bits >> 53` < 2048.
        unsafe {
            macro_rules! sym {
                ($b:ident, $op:ident, $k:expr) => {{
                    let e = *dtp.add(($b >> 53) as usize);
                    $b <<= e & 0x3F;
                    *outp.add($op + $k) = (e >> 8) as u8;
                }};
            }
            macro_rules! reload {
                ($b:ident, $ip:ident, $op:ident) => {{
                    let ctz = $b.trailing_zeros();
                    $op += 5;
                    $ip -= (ctz >> 3) as usize;
                    $b = (u64::from_le(srcp.add($ip).cast::<u64>().read_unaligned()) | 1)
                        << (ctz & 7);
                }};
            }
            loop {
                sym!(b0, op0, 0);
                sym!(b1, op1, 0);
                sym!(b2, op2, 0);
                sym!(b3, op3, 0);
                sym!(b0, op0, 1);
                sym!(b1, op1, 1);
                sym!(b2, op2, 1);
                sym!(b3, op3, 1);
                sym!(b0, op0, 2);
                sym!(b1, op1, 2);
                sym!(b2, op2, 2);
                sym!(b3, op3, 2);
                sym!(b0, op0, 3);
                sym!(b1, op1, 3);
                sym!(b2, op2, 3);
                sym!(b3, op3, 3);
                sym!(b0, op0, 4);
                sym!(b1, op1, 4);
                sym!(b2, op2, 4);
                sym!(b3, op3, 4);
                reload!(b0, ip0, op0);
                reload!(b1, ip1, op1);
                reload!(b2, ip2, op2);
                reload!(b3, ip3, op3);
                if op3 >= olimit {
                    break;
                }
            }
        }
    }
    st.ip = [ip0, ip1, ip2, ip3];
    st.op = [op0, op1, op2, op3];
    st.bits = [b0, b1, b2, b3];
}

#[inline]
fn fast_loop_x2_dispatch(
    st: &mut FastState,
    out: &mut [u8],
    src: &[u8],
    starts: &[usize; 4],
    seg_ends: &[usize; 4],
    dt2: &[u32; 1 << FAST_TABLELOG],
    cpu: Features,
) {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if cpu.huf_asm {
        fast_loop_asm(st, out, src, starts, Some(seg_ends), dt2.as_ptr().cast());
        return;
    }
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if cpu.bmi2 {
        #[allow(unsafe_code)]
        // SAFETY: the caller detected BMI2 (and BMI1/LZCNT) on this CPU.
        unsafe {
            fast_loop_x2_bmi2(st, out, src, starts, seg_ends, dt2)
        };
        return;
    }
    let _ = cpu;
    fast_loop_x2(st, out, src, starts, seg_ends, dt2);
}

#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[target_feature(enable = "bmi1,bmi2,lzcnt")]
fn fast_loop_x2_bmi2(
    st: &mut FastState,
    out: &mut [u8],
    src: &[u8],
    starts: &[usize; 4],
    seg_ends: &[usize; 4],
    dt2: &[u32; 1 << FAST_TABLELOG],
) {
    fast_loop_x2_body(st, out, src, starts, seg_ends, dt2);
}

fn fast_loop_x2(
    st: &mut FastState,
    out: &mut [u8],
    src: &[u8],
    starts: &[usize; 4],
    seg_ends: &[usize; 4],
    dt2: &[u32; 1 << FAST_TABLELOG],
) {
    fast_loop_x2_body(st, out, src, starts, seg_ends, dt2);
}

/// libzstd's `HUF_decompress4X2_usingDTable_internal_fast_c_loop`: 5
/// lookups per stream per iteration, each producing 1 or 2 symbols.
#[inline(always)]
#[allow(unsafe_code)]
fn fast_loop_x2_body(
    st: &mut FastState,
    out: &mut [u8],
    src: &[u8],
    starts: &[usize; 4],
    seg_ends: &[usize; 4],
    dt2: &[u32; 1 << FAST_TABLELOG],
) {
    let [mut ip0, mut ip1, mut ip2, mut ip3] = st.ip;
    let [mut op0, mut op1, mut op2, mut op3] = st.op;
    let [mut b0, mut b1, mut b2, mut b3] = st.bits;
    let srcp = src.as_ptr();
    let outp = out.as_mut_ptr();
    let dtp = dt2.as_ptr();
    debug_assert!(seg_ends[3] <= out.len());
    loop {
        // Up to 55 bits (< 7 bytes) and 10 output bytes per stream per iteration.
        let iiters = ((ip0 - starts[0]) / 7)
            .min((ip1 - starts[1]) / 7)
            .min((ip2 - starts[2]) / 7)
            .min((ip3 - starts[3]) / 7);
        let oiters = ((seg_ends[0] - op0) / 10)
            .min((seg_ends[1] - op1) / 10)
            .min((seg_ends[2] - op2) / 10)
            .min((seg_ends[3] - op3) / 10);
        let iters = iiters.min(oiters);
        if iters == 0 {
            break;
        }
        // SAFETY: `iters` bounds every access: each stream writes at most
        // 10 bytes per iteration (the 2-byte store of the last lookup ends
        // at most 10 bytes in) inside its own segment, each `ip` stays at or
        // above its stream's start (at most 7 bytes back per iteration) and
        // only decreases, and `dt2` indexes are `bits >> 53` < 2048.
        unsafe {
            macro_rules! sym {
                ($b:ident, $op:ident) => {{
                    let e = *dtp.add(($b >> 53) as usize);
                    outp.add($op)
                        .cast::<u16>()
                        .write_unaligned((e as u16).to_le());
                    $b <<= (e >> 16) & 0x3F;
                    $op += (e >> 24) as usize;
                }};
            }
            macro_rules! reload {
                ($b:ident, $ip:ident) => {{
                    let ctz = $b.trailing_zeros();
                    $ip -= (ctz >> 3) as usize;
                    $b = (u64::from_le(srcp.add($ip).cast::<u64>().read_unaligned()) | 1)
                        << (ctz & 7);
                }};
            }
            for _ in 0..iters {
                sym!(b0, op0);
                sym!(b1, op1);
                sym!(b2, op2);
                sym!(b3, op3);
                sym!(b0, op0);
                sym!(b1, op1);
                sym!(b2, op2);
                sym!(b3, op3);
                sym!(b0, op0);
                sym!(b1, op1);
                sym!(b2, op2);
                sym!(b3, op3);
                sym!(b0, op0);
                sym!(b1, op1);
                sym!(b2, op2);
                sym!(b3, op3);
                sym!(b0, op0);
                sym!(b1, op1);
                sym!(b2, op2);
                sym!(b3, op3);
                reload!(b0, ip0);
                reload!(b1, ip1);
                reload!(b2, ip2);
                reload!(b3, ip3);
            }
        }
    }
    st.ip = [ip0, ip1, ip2, ip3];
    st.op = [op0, op1, op2, op3];
    st.bits = [b0, b1, b2, b3];
}

/// Drives the `asm!` loops (libzstd's `huf_decompress_amd64.S`): computes
/// how many iterations are safe, exactly as the Rust loops do, and lets
/// the asm run them. `seg_ends` is `Some` for the double-symbol loop.
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[allow(unsafe_code)]
fn fast_loop_asm(
    st: &mut FastState,
    out: &mut [u8],
    src: &[u8],
    starts: &[usize; 4],
    seg_ends: Option<&[usize; 4]>,
    dt: *const u8,
) {
    use crate::huf_asm::{self, AsmArgs};
    let srcp = src.as_ptr();
    let outp = out.as_mut_ptr();
    loop {
        let iiters = (0..4).map(|s| (st.ip[s] - starts[s]) / 7).min().unwrap();
        let oiters = match seg_ends {
            // Double-symbol: up to 10 bytes per stream per iteration.
            Some(ends) => (0..4).map(|s| (ends[s] - st.op[s]) / 10).min().unwrap(),
            // Single-symbol: 5 per stream, stream 3's segment is the shortest.
            None => (out.len() - st.op[3]) / 5,
        };
        let iters = iiters.min(oiters);
        if iters == 0 {
            break;
        }
        // SAFETY: indexes are in bounds (`ip + 8 <= src.len()`,
        // `op <= out.len()`, `op[3] + 5 * iters <= out.len()`), and the
        // iteration bound above is what the asm's safety contract needs:
        // it stops once `op[3]` reaches `olimit`, which takes at most
        // `iters` iterations as `op[3]` advances by at least 5 each.
        unsafe {
            let mut a = AsmArgs {
                ip: st.ip.map(|i| srcp.add(i)),
                op: st.op.map(|o| outp.add(o)),
                bits: st.bits,
                dt,
                olimit: outp.add(st.op[3] + 5 * iters),
            };
            if seg_ends.is_some() {
                huf_asm::x2(&mut a);
            } else {
                huf_asm::x1(&mut a);
            }
            st.ip = a.ip.map(|p| p as usize - srcp as usize);
            st.op = a.op.map(|p| p as usize - outp as usize);
            st.bits = a.bits;
        }
    }
}
