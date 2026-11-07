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

/// A single-symbol (X1) table: all a dictionary keeps (8 KiB). The
/// decoder's own [`HufTable`] adds the double-symbol table and its scratch.
#[derive(Clone)]
pub(crate) struct HufDt {
    pub dt: [u16; 1 << HUF_TABLELOG_MAX],
    pub log: u32,
}

/// The decoder's own Huffman table: the X1 table, plus its double-symbol
/// table built on demand.
#[derive(Clone)]
pub(crate) struct HufTable {
    x1: HufDt,
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
            .field("log", &self.x1.log)
            .finish_non_exhaustive()
    }
}

impl core::fmt::Debug for HufDt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HufDt")
            .field("log", &self.log)
            .finish_non_exhaustive()
    }
}

impl core::ops::Deref for HufTable {
    type Target = HufDt;
    #[inline(always)]
    fn deref(&self) -> &HufDt {
        &self.x1
    }
}

impl HufTable {
    pub(crate) const fn new() -> Self {
        HufTable {
            x1: HufDt::new(),
            dt2: [0; 1 << FAST_TABLELOG],
            x2_ready: false,
            x2_tmpl: [0; 1 << FAST_TABLELOG],
        }
    }

    /// [`HufDt::read`], invalidating the double-symbol table.
    pub(crate) fn read(&mut self, src: &[u8]) -> Result<usize> {
        let n = self.x1.read(src)?;
        self.x2_ready = false;
        Ok(n)
    }

    fn build_x2(&mut self) {
        debug_assert_eq!(self.x1.log, FAST_TABLELOG);
        build_x2_table(&self.x1.dt, &mut self.dt2, &mut self.x2_tmpl);
        self.x2_ready = true;
    }

    /// Builds the double-symbol table of `shared` (a dictionary's, used
    /// read-only) into `dt2`, with this table's scratch space.
    pub(crate) fn build_x2_of(&mut self, shared: &HufDt, dt2: &mut X2Table) {
        debug_assert!(shared.x2_capable());
        build_x2_table(&shared.dt, dt2, &mut self.x2_tmpl);
    }
}

impl HufDt {
    pub(crate) const fn new() -> Self {
        HufDt {
            dt: [0; 1 << HUF_TABLELOG_MAX],
            log: 0,
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

        #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
        let log = self.build_from_masks(&mut weights, osize)?;
        #[cfg(not(all(target_arch = "x86_64", not(feature = "portable"))))]
        let log = self.build_sorted(&mut weights, osize)?;
        self.log = log;
        Ok(isize + 1)
    }

    /// Validates the weights, completes them with the implied last one and
    /// fills the X1 table, from a counting sort (portable version). Returns
    /// the table log.
    #[cfg_attr(
        all(target_arch = "x86_64", not(feature = "portable")),
        allow(dead_code)
    )]
    fn build_sorted(&mut self, weights: &mut [u8; 256], osize: usize) -> Result<u32> {
        // Validate and count (libzstd's HUF_readStats). The symbols are split
        // in 4 contiguous quarters with their own counters, visited
        // interleaved: runs of equal weights (common: all the zero-weight
        // control characters) would otherwise serialize on one counter
        // through store-to-load forwarding, here and in the sort below.
        let q = (osize + 1).div_ceil(4);
        let mut rq = [[0u32; 16]; 4];
        let mut total = 0u32;
        let mut bad = 0u8;
        for i in 0..q {
            for (k, r) in rq.iter_mut().enumerate() {
                let s = k * q + i;
                if s < osize {
                    let w = weights[s];
                    bad |= w;
                    r[(w & 15) as usize] += 1;
                    total += (1 << (w & 15)) >> 1;
                }
            }
        }
        let mut rank = [0u32; 16];
        for (w, r) in rank.iter_mut().enumerate() {
            *r = rq[0][w] + rq[1][w] + rq[2][w] + rq[3][w];
        }
        // Weights over 15 show in `bad`, 13 to 15 in the top counters.
        if bad > 15 || rank[13] + rank[14] + rank[15] != 0 || total == 0 {
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
        rq[osize / q][last as usize] += 1;
        if rank[1] < 2 || rank[1] & 1 != 0 {
            return Err(Error::Corrupted);
        }
        let nb_symbols = osize + 1;

        // The table has at least FAST_TABLELOG bits (libzstd's
        // HUF_rescaleStats): a weight-w code is still `table_log + 1 - w`
        // bits long and fills `1 << (w - 1 + scale)` cells; the weights
        // themselves don't change.
        let log = table_log.max(FAST_TABLELOG);
        let scale = log - table_log;

        // Symbols sorted by weight, then by value (counting sort).
        let mut start = [0u32; 16];
        let mut next = 0u32;
        for w in 0..=table_log as usize {
            start[w] = next;
            next += rank[w];
        }
        // Quarter k starts after quarters 0..k within each weight, so the
        // order stays by weight, then symbol.
        let mut st = [[0u32; 16]; 4];
        for w in 0..=table_log as usize {
            let mut p = start[w];
            for k in 0..4 {
                st[k][w] = p;
                p += rq[k][w];
            }
        }
        let mut sorted = [0u8; 256];
        for i in 0..q {
            for (k, sk) in st.iter_mut().enumerate() {
                let s = k * q + i;
                if s < nb_symbols {
                    let p = &mut sk[(weights[s] & 15) as usize];
                    sorted[(*p & 255) as usize] = s as u8;
                    *p += 1;
                }
            }
        }

        // Fill weight by weight, so the run length is constant in each inner
        // loop and short runs don't go through a variable-length fill
        // (libzstd's switch on the length).
        let mut pos = 0usize;
        let mut idx = rank[0] as usize;
        let dt = &mut self.dt;
        for w in 1..=table_log {
            let count = rank[w as usize] as usize;
            let len = 1usize << (w - 1 + scale);
            let nb_bits = (table_log + 1 - w) as u16;
            let syms = &sorted[idx..idx + count];
            match len {
                1 => {
                    for (d, &s) in dt[pos..pos + count].iter_mut().zip(syms) {
                        *d = (u16::from(s) << 8) | nb_bits;
                    }
                }
                2 => {
                    for (d, &s) in dt[pos..pos + 2 * count]
                        .as_chunks_mut::<2>()
                        .0
                        .iter_mut()
                        .zip(syms)
                    {
                        let e = (u16::from(s) << 8) | nb_bits;
                        d.copy_from_slice(&[e; 2]);
                    }
                }
                _ => {
                    for (d, &s) in dt[pos..pos + len * count].chunks_exact_mut(len).zip(syms) {
                        let e4 = [(u16::from(s) << 8) | nb_bits; 4];
                        for q in d.as_chunks_mut::<4>().0 {
                            q.copy_from_slice(&e4);
                        }
                    }
                }
            }
            pos += len * count;
            idx += count;
        }
        debug_assert_eq!(pos, 1 << log);
        Ok(log)
    }

    /// [`build_sorted`](Self::build_sorted) with SSE2 (baseline on x86-64):
    /// a 256-bit mask of the symbols of each weight, from 16-byte compares,
    /// gives the counts (popcounts) and the sorted order (set bits in
    /// order) without the memory read-modify-write chains of a histogram
    /// and a counting sort.
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    fn build_from_masks(&mut self, weights: &mut [u8; 256], osize: usize) -> Result<u32> {
        use core::arch::x86_64::{
            __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
        };
        // masks[w][j]: bit i set when weights[64 j + i] == w, for w <= 12.
        let mut masks = [[0u64; 4]; HUF_TABLELOG_MAX as usize + 1];
        let chunks = osize.div_ceil(16);
        for c in 0..chunks {
            // SAFETY: SSE2 is part of x86-64; `weights` has 256 bytes and
            // c < 16, so the 16-byte load is in bounds.
            #[allow(unsafe_code)]
            let v = unsafe { _mm_loadu_si128(weights.as_ptr().add(16 * c).cast::<__m128i>()) };
            for (w, m) in masks.iter_mut().enumerate() {
                // SAFETY: SSE2 is part of x86-64.
                #[allow(unsafe_code)]
                let bits =
                    unsafe { _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(w as i8))) } as u16;
                m[c / 4] |= u64::from(bits) << (16 * (c % 4));
            }
        }
        // Only symbols before `osize` count (the rest of `weights` is 0).
        let valid = |j: usize| -> u64 {
            let lo = 64 * j;
            if osize >= lo + 64 {
                !0
            } else if osize <= lo {
                0
            } else {
                (1u64 << (osize - lo)) - 1
            }
        };
        let mut rank = [0u32; 16];
        let mut counted = 0u32;
        let mut total = 0u32;
        for (w, m) in masks.iter_mut().enumerate() {
            for (j, word) in m.iter_mut().enumerate() {
                *word &= valid(j);
            }
            let n: u32 = m.iter().map(|x| x.count_ones()).sum();
            rank[w] = n;
            counted += n;
            // A weight-w symbol adds 2^(w-1); weight 0 adds nothing.
            total += if w == 0 { 0 } else { n << (w - 1) };
        }
        // A weight over 12 isn't in any mask.
        if counted != osize as u32 || total == 0 {
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
        masks[last as usize][osize / 64] |= 1 << (osize % 64);
        if rank[1] < 2 || rank[1] & 1 != 0 {
            return Err(Error::Corrupted);
        }
        let log = table_log.max(FAST_TABLELOG);
        let scale = log - table_log;

        let dt = &mut self.dt;
        let mut pos = 0usize;
        for w in 1..=table_log {
            let len = 1usize << (w - 1 + scale);
            let nb_bits = (table_log + 1 - w) as u16;
            let count = rank[w as usize] as usize;
            let run = &mut dt[pos..pos + len * count];
            let mut k = 0;
            for (j, &word) in masks[w as usize].iter().enumerate() {
                let mut m = word;
                while m != 0 {
                    let s = 64 * j + m.trailing_zeros() as usize;
                    m &= m - 1;
                    let e = ((s as u16) << 8) | nb_bits;
                    let d = &mut run[k..k + len];
                    match len {
                        1 => d[0] = e,
                        2 => d.copy_from_slice(&[e; 2]),
                        _ => {
                            for q in d.as_chunks_mut::<4>().0 {
                                q.copy_from_slice(&[e; 4]);
                            }
                        }
                    }
                    k += len;
                }
            }
            pos += len * count;
        }
        debug_assert_eq!(pos, 1 << log);
        Ok(log)
    }

    /// Loads the table into the cache: `block::touch_area`.
    pub(crate) fn touch(&self) {
        crate::block::touch_area(&self.dt[..1 << self.log]);
    }

    /// Whether the double-symbol decoders can use this table.
    pub(crate) fn x2_capable(&self) -> bool {
        self.log == FAST_TABLELOG
    }
}

/// A double-symbol table (libzstd's `X2`): `s1 | s2 << 8 | bits << 16 |
/// count << 24` for each 11-bit window.
pub(crate) type X2Table = [u32; 1 << FAST_TABLELOG];

/// Builds the double-symbol table of the single-symbol table `dt` (of log
/// 11) into `dt2`, with `tmpl` as scratch: each entry decodes the first
/// symbol and, when its code fits in the remaining bits of the 11-bit
/// window, the next one too. Same symbols and bit counts as two X1
/// lookups, so X1 and X2 decode identically.
fn build_x2_table(dt: &[u16; 1 << HUF_TABLELOG_MAX], dt2: &mut X2Table, tmpl: &mut X2Table) {
    const SIZE: usize = 1 << FAST_TABLELOG;
    // An X1 entry with code length n1 fills a run of 2^(11 - n1) cells
    // starting at a multiple of the run length, so cell k of every such
    // run sees the same next bits, `k << n1`. The second-symbol part is
    // therefore one template per code length, ORed with the first symbol.
    // (Templates of distinct lengths, 1 to 11 bits, sum to under SIZE cells.)
    let mut tmpl_at = [usize::MAX; FAST_TABLELOG as usize + 1];
    let mut used = 0;
    let mut i = 0;
    while i < SIZE {
        let e1 = u32::from(dt[i]);
        let n1 = e1 & 0xFF;
        let s1 = e1 >> 8;
        let r = FAST_TABLELOG - n1;
        let run = 1usize << r;
        if tmpl_at[n1 as usize] == usize::MAX {
            tmpl_at[n1 as usize] = used;
            for (k, t) in tmpl[used..used + run].iter_mut().enumerate() {
                let e2 = u32::from(dt[k << n1]);
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
        for (o, &t) in dt2[i..i + run].iter_mut().zip(t) {
            *o = t | s1;
        }
        i += run;
    }
}

/// libzstd's `HUF_selectDecoder` times (table build, decode of 256 bytes) of
/// the single- and the double-symbol decoders by compression ratio class.
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

/// The compressed/regenerated size ratio in 16ths (15 for incompressible).
fn ratio_class(dst_size: usize, csrc_size: usize) -> usize {
    if csrc_size >= dst_size {
        15
    } else {
        csrc_size * 16 / dst_size
    }
}

/// libzstd's `HUF_selectDecoder`: whether the double-symbol decoder is
/// expected to be faster, from the literals' size and compression ratio
/// (libzstd's measured table-build and decode times).
pub(crate) fn prefer_x2(dst_size: usize, csrc_size: usize) -> bool {
    if dst_size == 0 {
        return false;
    }
    let d256 = (dst_size >> 8) as u32;
    let [(t0, d0), (t1, d1)] = ALGO_TIME[ratio_class(dst_size, csrc_size)];
    let time0 = t0 + d0 * d256;
    let mut time1 = t1 + d1 * d256;
    time1 += time1 >> 5;
    time1 < time0
}

/// [`prefer_x2`] for a table whose double-symbol table is built already (a
/// dictionary's): the decode times only.
pub(crate) fn prefer_x2_built(dst_size: usize, csrc_size: usize) -> bool {
    let [(_, d0), (_, d1)] = ALGO_TIME[ratio_class(dst_size, csrc_size)];
    d1 + (d1 >> 5) < d0
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
fn decode_stream(out: &mut [u8], br: &mut BitReader<'_>, t: &HufDt) {
    let (dt, log) = (&t.dt, t.log);
    let mut p = 0;
    let end = out.len();
    if end > 3 {
        while (br.reload_short() == Status::Unfinished) & (p < end - 3) {
            out[p] = decode_symbol(br, dt, log);
            out[p + 1] = decode_symbol(br, dt, log);
            out[p + 2] = decode_symbol(br, dt, log);
            out[p + 3] = decode_symbol(br, dt, log);
            p += 4;
        }
    } else {
        br.reload_short();
    }
    while p < end {
        out[p] = decode_symbol(br, dt, log);
        p += 1;
    }
}

/// Decodes `out.len()` symbols from `br` with the double-symbol table, up
/// to 10 per reload (libzstd's `HUF_decodeStreamX2`), the last ones with
/// the single-symbol table (the two decode identically).
fn decode_stream_x2(out: &mut [u8], br: &mut BitReader<'_>, t: &HufDt, dt2: &X2Table) {
    debug_assert!(t.x2_capable());
    let mut p = 0;
    let end = out.len();
    if end >= 10 {
        // 5 lookups of at most 11 bits each (55 of the 57 available), at
        // most 10 symbols: each lookup writes 2 bytes and advances 1 or 2.
        while (br.reload_short() == Status::Unfinished) & (p < end - 9) {
            for _ in 0..5 {
                let e = dt2[br.peek_fast(FAST_TABLELOG) as usize];
                out[p] = e as u8;
                out[p + 1] = (e >> 8) as u8;
                br.skip((e >> 16) & 0xFF);
                p += (e >> 24) as usize;
            }
        }
    }
    decode_stream(&mut out[p..], br, t);
}

/// Single-stream literals, with `dt2` (`t`'s double-symbol table) if given.
pub(crate) fn decode_1x(
    out: &mut [u8],
    src: &[u8],
    t: &HufDt,
    dt2: Option<&X2Table>,
) -> Result<()> {
    let mut br = BitReader::new(src)?;
    if let Some(dt2) = dt2 {
        decode_stream_x2(out, &mut br, t, dt2);
    } else {
        decode_stream(out, &mut br, t);
    }
    if !br.is_end() {
        return Err(Error::Corrupted);
    }
    Ok(())
}

/// The Huffman table a block decodes with: the decoder's own (its
/// double-symbol table built on first use) or a dictionary's, shared and
/// read-only, as libzstd's `HUFptr` points into the `ZSTD_DDict`, with its
/// double-symbol table if the caller has one.
pub(crate) enum HufRef<'a> {
    Own(&'a mut HufTable),
    Shared(&'a HufDt, Option<&'a X2Table>),
}

impl HufRef<'_> {
    #[inline(always)]
    pub(crate) fn get(&self) -> &HufDt {
        match self {
            HufRef::Own(t) => t,
            HufRef::Shared(t, _) => t,
        }
    }

    /// A shared table's double-symbol table, if given.
    pub(crate) fn shared_x2(&self) -> Option<&X2Table> {
        match self {
            HufRef::Own(_) => None,
            HufRef::Shared(_, dt2) => *dt2,
        }
    }
}

/// Four-stream literals; `x2` selects the double-symbol fast loop (when the
/// table allows it). Stream tails always use the single-symbol table.
pub(crate) fn decode_4x(
    out: &mut [u8],
    src: &[u8],
    mut t: HufRef<'_>,
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
    let log = t.get().log;
    if log == FAST_TABLELOG && lens.iter().all(|&l| l >= 8) && 3 * seg < n {
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
        if x2
            && let HufRef::Own(t) = &mut t
            && !t.x2_ready
        {
            t.build_x2();
        }
        let dt2 = match &t {
            HufRef::Own(t) => (x2 && t.x2_ready).then_some(&t.dt2),
            HufRef::Shared(_, dt2) => dt2.filter(|_| x2),
        };
        match dt2 {
            Some(dt2) => fast_loop_x2_dispatch(&mut st, out, src, &starts, &seg_ends, dt2, cpu),
            None => fast_loop_dispatch(&mut st, out, src, &starts, &t.get().dt, cpu),
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
        decode_stream(&mut out[op[s]..seg_ends[s]], &mut br, t.get());
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
            align_hot_loop!(0);
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
            align_hot_loop!(0);
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

/// Benchmark hook: the Huffman tree description of the first compressed
/// literals section in `frame` (a single-frame zstd input), if any.
#[doc(hidden)]
pub fn first_tree_description(frame: &[u8]) -> Option<&[u8]> {
    let fh = crate::frame::FrameHeader::parse(frame).ok()?;
    let mut ip = fh.header_size;
    loop {
        let b = frame.get(ip..ip + 3)?;
        let bh = u32::from_le_bytes([b[0], b[1], b[2], 0]);
        ip += 3;
        let (last, btype, bsize) = (bh & 1 != 0, (bh >> 1) & 3, (bh >> 3) as usize);
        if btype == 2 {
            let blk = frame.get(ip..ip + bsize)?;
            if blk[0] & 3 == 2 {
                let lh = [3, 3, 4, 5][((blk[0] >> 2) & 3) as usize];
                return blk.get(lh..);
            }
        }
        ip += if btype == 1 { 1 } else { bsize };
        if last {
            return None;
        }
    }
}

/// Benchmark hook: reads the tree description `n` times, returns a checksum.
#[doc(hidden)]
pub fn bench_read(desc: &[u8], n: usize) -> usize {
    let mut t = HufTable::new();
    let mut acc = 0;
    for _ in 0..n {
        acc += t.read(core::hint::black_box(desc)).unwrap_or(0);
        acc += t.dt[acc & 2047] as usize;
    }
    acc
}

#[cfg(all(test, target_arch = "x86_64", not(feature = "portable")))]
mod tests {
    use super::*;

    /// The SSE2 mask build and the portable sort build agree exactly, on
    /// valid and invalid weights.
    #[test]
    fn mask_build_matches_sorted_build() {
        let mut r = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            r
        };
        let mut valid = 0;
        for case in 0..20_000 {
            let osize = 1 + (next() % 255) as usize;
            let mut w = [0u8; 256];
            // Mostly small weights; sometimes a bad one.
            let maxw = 1 + (next() % 12) as u8;
            for x in w[..osize].iter_mut() {
                *x = if next() % 3 == 0 {
                    0
                } else {
                    (next() % u64::from(maxw)) as u8 + 1
                };
            }
            if case % 50 == 0 {
                w[(next() as usize) % osize] = 13 + (next() % 200) as u8;
            }
            // Make about half the cases valid: fix the total up to a power
            // of two minus a power of two by construction is hard, so just
            // compare whatever comes out.
            let (mut a, mut b) = (HufDt::new(), HufDt::new());
            let (mut wa, mut wb) = (w, w);
            let ra = a.build_sorted(&mut wa, osize);
            let rb = b.build_from_masks(&mut wb, osize);
            assert_eq!(ra, rb, "case {case}");
            if let Ok(log) = ra {
                valid += 1;
                assert_eq!(&a.dt[..1 << log], &b.dt[..1 << log], "case {case}");
            }
        }
        assert!(valid > 100, "only {valid} valid cases");
    }
}
