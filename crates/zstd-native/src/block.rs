//! Compressed blocks: literals section, sequences section, and the fused
//! sequence decode + execute loop.
//!
//! Output is written through raw pointers so the same code serves a
//! `Vec`'s spare capacity, a caller's slice and the streaming window. The
//! fast path copies in 16-byte chunks that may write up to
//! [`WILDCOPY_OVERLENGTH`] bytes past the sequence end and read that far
//! past the literals, so it only runs while the output has that much room
//! and every literal buffer carries that much slack; the last sequences
//! of a buffer take the exact (slow) path, as in libzstd.

use alloc::vec::Vec;
use core::ptr;

use crate::bits::BitReader;
use crate::cpu::Features;
use crate::error::{Error, Result};
use crate::fse::{self, LL_KIND, ML_KIND, OF_KIND, SeqSymbol, SeqTables};
use crate::huf::{self, HufTable};

pub(crate) const WILDCOPY_OVERLENGTH: usize = 32;
pub(crate) const BLOCK_SIZE_MAX: usize = 128 << 10;
const MIN_LITERALS_FOR_4_STREAMS: usize = 6;
const LONGNBSEQ: usize = 0x7F00;

/// Where decoded bytes go. `ptr[..cap]` must be writable (it may be
/// uninitialized); `ptr[..pos]` holds the output so far.
#[derive(Debug)]
pub(crate) struct Output {
    pub ptr: *mut u8,
    pub cap: usize,
    pub pos: usize,
}

/// Entropy state carried between the blocks of a frame.
#[derive(Clone, Debug)]
pub(crate) struct Entropy {
    pub huf: HufTable,
    pub huf_ok: bool,
    pub seq: SeqTables,
    pub fse_ok: bool,
    pub rep: [u32; 3],
}

impl Entropy {
    pub(crate) const fn new() -> Self {
        Entropy {
            huf: HufTable::new(),
            huf_ok: false,
            seq: SeqTables::new(),
            fse_ok: false,
            rep: [1, 4, 8],
        }
    }

    /// State at the start of a frame without a dictionary.
    pub(crate) fn reset(&mut self) {
        self.huf_ok = false;
        self.fse_ok = false;
        self.rep = [1, 4, 8];
    }
}

/// Decoder state for blocks: entropy tables plus the literals buffer.
#[derive(Debug)]
pub(crate) struct BlockDecoder {
    pub ent: Entropy,
    /// Decoded literals, always with `WILDCOPY_OVERLENGTH` readable bytes
    /// after the last literal.
    lit_buf: Vec<u8>,
    pub cpu: Features,
}

/// The literals of one block.
struct Literals {
    ptr: *const u8,
    len: usize,
}

impl BlockDecoder {
    pub(crate) fn new() -> Self {
        BlockDecoder {
            ent: Entropy::new(),
            lit_buf: Vec::new(),
            cpu: Features::detect(),
        }
    }

    fn lit_space(&mut self, n: usize) -> &mut [u8] {
        let need = n + WILDCOPY_OVERLENGTH;
        if self.lit_buf.len() < need {
            self.lit_buf.resize(need.max(4096), 0);
        }
        &mut self.lit_buf[..n]
    }

    /// Decodes one compressed block of a frame. `out.pos - prefix` bytes of
    /// the frame are already in the output, and `dict` precedes them.
    pub(crate) fn decode_block(
        &mut self,
        src: &[u8],
        out: &mut Output,
        prefix: usize,
        dict: &[u8],
        block_max: usize,
    ) -> Result<()> {
        if src.len() > block_max {
            return Err(Error::Corrupted);
        }
        let room = (out.cap - out.pos).min(block_max);
        let (lits, used) = self.decode_literals(src, block_max, room)?;
        let src = &src[used..];
        let (nb_seq, used) = self.decode_seq_headers(src)?;
        let seq_src = &src[used..];
        if nb_seq == 0 {
            // Literals only.
            if lits.len > out.cap - out.pos {
                return Err(Error::OutputTooSmall);
            }
            // SAFETY: `lits` points at `lits.len` initialized bytes (in
            // `lit_buf` or `src`), and the output has room for them.
            #[allow(unsafe_code)]
            unsafe {
                ptr::copy_nonoverlapping(lits.ptr, out.ptr.add(out.pos), lits.len)
            };
            out.pos += lits.len;
            return Ok(());
        }
        self.ent.fse_ok = true;
        let tables = &self.ent.seq;
        let mut rep = self.ent.rep.map(|r| r as usize);
        let res = run_sequences(
            self.cpu, tables, &mut rep, seq_src, nb_seq, &lits, out, prefix, dict,
        );
        self.ent.rep = rep.map(|r| r as u32);
        res
    }

    /// Literals section (libzstd's `ZSTD_decodeLiteralsBlock`). Returns the
    /// literals and the bytes of `src` used.
    fn decode_literals(
        &mut self,
        src: &[u8],
        block_max: usize,
        room: usize,
    ) -> Result<(Literals, usize)> {
        if src.len() < 2 {
            return Err(Error::Corrupted);
        }
        let b0 = src[0];
        let kind = b0 & 3;
        let lhl = (b0 >> 2) & 3;
        match kind {
            // Raw or RLE
            0 | 1 => {
                let (lh, lit_size) = match lhl {
                    1 => (2, (u16::from_le_bytes([src[0], src[1]]) >> 4) as usize),
                    3 => {
                        if src.len() < 3 {
                            return Err(Error::Corrupted);
                        }
                        (
                            3,
                            (u32::from_le_bytes([src[0], src[1], src[2], 0]) >> 4) as usize,
                        )
                    }
                    _ => (1, (b0 >> 3) as usize),
                };
                if lit_size > block_max {
                    return Err(Error::Corrupted);
                }
                if lit_size > room {
                    return Err(Error::OutputTooSmall);
                }
                if kind == 0 {
                    if lh + lit_size + WILDCOPY_OVERLENGTH <= src.len() {
                        // Enough slack after them: use the literals in place.
                        return Ok((
                            Literals {
                                ptr: src[lh..].as_ptr(),
                                len: lit_size,
                            },
                            lh + lit_size,
                        ));
                    }
                    if lh + lit_size > src.len() {
                        return Err(Error::Corrupted);
                    }
                    self.lit_space(lit_size)
                        .copy_from_slice(&src[lh..lh + lit_size]);
                    Ok((
                        Literals {
                            ptr: self.lit_buf.as_ptr(),
                            len: lit_size,
                        },
                        lh + lit_size,
                    ))
                } else {
                    if lh >= src.len() {
                        return Err(Error::Corrupted);
                    }
                    let b = src[lh];
                    self.lit_space(lit_size).fill(b);
                    Ok((
                        Literals {
                            ptr: self.lit_buf.as_ptr(),
                            len: lit_size,
                        },
                        lh + 1,
                    ))
                }
            }
            // Compressed or treeless
            _ => {
                if src.len() < 5 {
                    return Err(Error::Corrupted);
                }
                let lhc = u32::from_le_bytes([src[0], src[1], src[2], src[3]]) as usize;
                let (lh, lit_size, lit_csize, single) = match lhl {
                    0 | 1 => (3, (lhc >> 4) & 0x3FF, (lhc >> 14) & 0x3FF, lhl == 0),
                    2 => (4, (lhc >> 4) & 0x3FFF, lhc >> 18, false),
                    _ => (
                        5,
                        (lhc >> 4) & 0x3FFFF,
                        (lhc >> 22) + ((src[4] as usize) << 10),
                        false,
                    ),
                };
                if lit_size > block_max {
                    return Err(Error::Corrupted);
                }
                if !single && lit_size < MIN_LITERALS_FOR_4_STREAMS {
                    return Err(Error::Corrupted);
                }
                if lit_csize + lh > src.len() {
                    return Err(Error::Corrupted);
                }
                if lit_size > room {
                    return Err(Error::OutputTooSmall);
                }
                let mut csrc = &src[lh..lh + lit_csize];
                if kind == 3 {
                    if !self.ent.huf_ok {
                        return Err(Error::Corrupted);
                    }
                } else {
                    if lit_csize == 0 {
                        return Err(Error::Corrupted);
                    }
                    let h = self.ent.huf.read(csrc)?;
                    if h >= csrc.len() {
                        return Err(Error::Corrupted);
                    }
                    csrc = &csrc[h..];
                    self.ent.huf_ok = true;
                }
                let cpu = self.cpu;
                let need = lit_size + WILDCOPY_OVERLENGTH;
                if self.lit_buf.len() < need {
                    self.lit_buf.resize(need.max(4096), 0);
                }
                let out = &mut self.lit_buf[..lit_size];
                if single {
                    huf::decode_1x(out, csrc, &self.ent.huf)?;
                } else {
                    let x2 = huf::prefer_x2(lit_size, lit_csize);
                    huf::decode_4x(out, csrc, &mut self.ent.huf, x2, cpu)?;
                }
                Ok((
                    Literals {
                        ptr: self.lit_buf.as_ptr(),
                        len: lit_size,
                    },
                    lh + lit_csize,
                ))
            }
        }
    }

    /// Sequences section header (libzstd's `ZSTD_decodeSeqHeaders`).
    fn decode_seq_headers(&mut self, src: &[u8]) -> Result<(usize, usize)> {
        let Some(&b0) = src.first() else {
            return Err(Error::Corrupted);
        };
        let mut ip = 1;
        let mut nb_seq = b0 as usize;
        if nb_seq > 0x7F {
            if nb_seq == 0xFF {
                if ip + 2 > src.len() {
                    return Err(Error::Corrupted);
                }
                nb_seq = u16::from_le_bytes([src[1], src[2]]) as usize + LONGNBSEQ;
                ip += 2;
            } else {
                if ip >= src.len() {
                    return Err(Error::Corrupted);
                }
                nb_seq = ((nb_seq - 0x80) << 8) + src[1] as usize;
                ip += 1;
            }
        }
        if nb_seq == 0 {
            if ip != src.len() {
                return Err(Error::Corrupted);
            }
            return Ok((0, ip));
        }
        let Some(&modes) = src.get(ip) else {
            return Err(Error::Corrupted);
        };
        if modes & 3 != 0 {
            return Err(Error::Corrupted);
        }
        ip += 1;
        let fse_ok = self.ent.fse_ok;
        let t = &mut self.ent.seq;
        let cpu = self.cpu;
        ip += t.read(modes >> 6, LL_KIND, &src[ip..], fse_ok, cpu)?;
        ip += t.read((modes >> 4) & 3, OF_KIND, &src[ip..], fse_ok, cpu)?;
        ip += t.read((modes >> 2) & 3, ML_KIND, &src[ip..], fse_ok, cpu)?;
        Ok((nb_seq, ip))
    }
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn run_sequences(
    cpu: Features,
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if cpu.bmi2 {
        #[allow(unsafe_code)]
        // SAFETY: the caller detected BMI2 (and BMI1/LZCNT) on this CPU.
        return unsafe { sequences_bmi2(t, rep, src, nb_seq, lits, out, prefix, dict) };
    }
    let _ = cpu;
    sequences_generic(t, rep, src, nb_seq, lits, out, prefix, dict)
}

#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[target_feature(enable = "bmi1,bmi2,lzcnt")]
#[allow(clippy::too_many_arguments)]
fn sequences_bmi2(
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    sequences_body(t, rep, src, nb_seq, lits, out, prefix, dict)
}

#[allow(clippy::too_many_arguments)]
fn sequences_generic(
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    sequences_body(t, rep, src, nb_seq, lits, out, prefix, dict)
}

/// The cell for `state` in the combined table at `tp`.
#[inline(always)]
fn cell(tp: *const SeqSymbol, state: usize) -> SeqSymbol {
    debug_assert!(state < fse::SEQ_TABLE_SIZE);
    // SAFETY: FSE construction keeps every state inside its region:
    // initial states are `log`-bit reads plus the region start, and
    // `next_state + bits` stays below region start + `1 << log` for every
    // cell (see `fse::build_seq_table`). The table outlives the loop.
    #[allow(unsafe_code)]
    unsafe {
        *tp.add(state)
    }
}

#[derive(Clone, Copy)]
struct Seq {
    lit_len: usize,
    match_len: usize,
    offset: usize,
}

/// One sequence (libzstd's `ZSTD_decodeSequence`, 64-bit flavor).
#[inline(always)]
fn decode_sequence(
    br: &mut BitReader<'_>,
    tp: *const SeqSymbol,
    ll: &mut usize,
    ml: &mut usize,
    of: &mut usize,
    rep: &mut [usize; 3],
    last: bool,
) -> Seq {
    let llc = cell(tp, *ll);
    let mlc = cell(tp, *ml);
    let ofc = cell(tp, *of);
    let ll_bits = u32::from(llc.nb_add_bits);
    let ml_bits = u32::from(mlc.nb_add_bits);
    let of_bits = u32::from(ofc.nb_add_bits);

    // Extra bits in libzstd's order: offset, match length, (reload),
    // literal length. Reading them together measured slower on text (an
    // extra data-dependent branch, or worse register allocation).
    let mut lit_len = llc.base as usize;
    let mut match_len = mlc.base as usize;
    let offset;
    if of_bits > 1 {
        offset = ofc.base as usize + br.read_fast(of_bits) as usize;
        rep[2] = rep[1];
        rep[1] = rep[0];
        rep[0] = offset;
    } else {
        // Constant indexes only, so the three offsets stay in registers.
        let ll0 = llc.base == 0;
        if of_bits == 0 {
            // Repeat offset 1, or 2 when the literal length is 0.
            if ll0 {
                rep.swap(0, 1);
            }
            offset = rep[0];
        } else {
            // Code 1: repeat offset 2 or 3 (3 or "1 - 1" when ll is 0).
            let o = ll0 as usize + br.read_fast(1) as usize;
            let mut temp = match o {
                0 => rep[1],
                1 => rep[2],
                _ => rep[0].wrapping_sub(1),
            };
            // 0 is invalid: make it huge so execution reports corruption.
            temp = temp.wrapping_sub((temp == 0) as usize);
            if o != 0 {
                rep[2] = rep[1];
            }
            rep[1] = rep[0];
            rep[0] = temp;
            offset = temp;
        }
    }
    if ml_bits > 0 {
        match_len += br.read_fast(ml_bits) as usize;
    }
    if ll_bits + ml_bits + of_bits
        >= 64 - 7 - (fse::LL_FSE_LOG + fse::ML_FSE_LOG + fse::OFF_FSE_LOG)
    {
        br.reload();
    }
    if ll_bits > 0 {
        lit_len += br.read_fast(ll_bits) as usize;
    }
    if !last {
        // One read for the three state updates (LL bits first, then ML, then
        // OF; at most 9 + 9 + 8 bits): one dependency on the consumed count
        // instead of three.
        let (lb, mb, ob) = (
            u32::from(llc.nb_bits),
            u32::from(mlc.nb_bits),
            u32::from(ofc.nb_bits),
        );
        let v = br.read(lb + mb + ob);
        *ll = llc.next_state as usize + (v >> (mb + ob)) as usize;
        *ml = mlc.next_state as usize + ((v >> ob) & ((1 << mb) - 1)) as usize;
        *of = ofc.next_state as usize + (v & ((1 << ob) - 1)) as usize;
        br.reload();
    }
    Seq {
        lit_len,
        match_len,
        offset,
    }
}

#[allow(unsafe_code)]
mod copy {
    //! Copy helpers; every caller guarantees the ranges are in bounds.
    use core::ptr;

    /// Copies 16 bytes; the ranges must not overlap.
    #[inline(always)]
    pub(super) unsafe fn copy16(dst: *mut u8, src: *const u8) {
        // SAFETY: per the caller.
        unsafe { ptr::copy_nonoverlapping(src, dst, 16) }
    }

    #[inline(always)]
    pub(super) unsafe fn copy8(dst: *mut u8, src: *const u8) {
        // SAFETY: per the caller.
        unsafe { ptr::copy_nonoverlapping(src, dst, 8) }
    }

    /// Copies `len` bytes in 16-byte chunks, writing and reading up to 31
    /// bytes past the end. `dst - src >= 16` or the buffers are disjoint.
    #[inline(always)]
    pub(super) unsafe fn wildcopy(dst: *mut u8, src: *const u8, len: usize) {
        // SAFETY: per the caller.
        unsafe {
            copy16(dst, src);
            if len <= 16 {
                return;
            }
            let mut d = dst.add(16);
            let mut s = src.add(16);
            let end = dst.add(len);
            loop {
                copy16(d, s);
                copy16(d.add(16), s.add(16));
                d = d.add(32);
                s = s.add(32);
                if d >= end {
                    break;
                }
            }
        }
    }

    /// Wildcopy for `8 <= dst - src < 16`: 8 bytes at a time.
    #[inline(always)]
    pub(super) unsafe fn wildcopy8(dst: *mut u8, src: *const u8, len: usize) {
        // SAFETY: per the caller.
        unsafe {
            let mut d = dst;
            let mut s = src;
            let end = dst.add(len);
            loop {
                copy8(d, s);
                d = d.add(8);
                s = s.add(8);
                if d >= end {
                    break;
                }
            }
        }
    }

    /// libzstd's `ZSTD_overlapCopy8`: copies 8 bytes from `*m` to `*o` for
    /// an offset under 16, and leaves `*o - *m >= 8`.
    #[inline(always)]
    pub(super) unsafe fn overlap_copy8(o: &mut *mut u8, m: &mut *const u8, offset: usize) {
        const DEC32: [usize; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
        const DEC64: [usize; 8] = [8, 8, 8, 7, 8, 9, 10, 11];
        // SAFETY: per the caller: `*m = *o - offset` with 8 + 8 bytes in bounds.
        unsafe {
            if offset < 8 {
                let sub2 = DEC64[offset];
                *(*o) = **m;
                *(*o).add(1) = *(*m).add(1);
                *(*o).add(2) = *(*m).add(2);
                *(*o).add(3) = *(*m).add(3);
                *m = (*m).add(DEC32[offset]);
                ptr::copy_nonoverlapping(*m, (*o).add(4), 4);
                *m = (*m).sub(sub2);
            } else {
                copy8(*o, *m);
            }
            *m = (*m).add(8);
            *o = (*o).add(8);
        }
    }
}

/// The fused decode + execute loop (libzstd's `ZSTD_decompressSequences_body`).
/// (Decoding batches of sequences first and executing them after was
/// measured 15% slower.)
#[inline(always)]
#[allow(clippy::too_many_arguments, unsafe_code)]
fn sequences_body(
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    let mut br = BitReader::new(src)?;
    let tp = t.cells.as_ptr();
    let mut ll = LL_KIND.region + br.read(t.log[0]) as usize;
    br.reload();
    let mut of = OF_KIND.region + br.read(t.log[1]) as usize;
    br.reload();
    let mut ml = ML_KIND.region + br.read(t.log[2]) as usize;
    br.reload();
    let mut reps = *rep;

    // Pointers rather than base + index: fewer live registers. Addresses
    // are compared as integers so a corrupt length never forms an
    // out-of-bounds pointer.
    let base = out.ptr;
    let cap = out.cap;
    // SAFETY: `out.pos <= cap` and `prefix <= out.pos`.
    let (mut op, prefix_start) = unsafe { (base.add(out.pos), base.add(prefix)) };
    let oend_w = base as usize + cap.saturating_sub(WILDCOPY_OVERLENGTH);
    let lit_start = lits.ptr;
    let lit_end = lits.ptr as usize + lits.len;
    let mut lp = lits.ptr;

    let mut n = nb_seq;
    while n > 0 {
        n -= 1;
        {
            let seq = decode_sequence(&mut br, tp, &mut ll, &mut ml, &mut of, &mut reps, n == 0);
            let seq_len = seq.lit_len + seq.match_len;
            // In u64 so the sums can't wrap on 32-bit targets (free on 64-bit).
            if lp as u64 + seq.lit_len as u64 > lit_end as u64
                || op as u64 + seq_len as u64 > oend_w as u64
            {
                let o = op as usize - base as usize;
                let mut lit = lp as usize - lit_start as usize;
                let o = exec_sequence_end(
                    base, cap, o, seq, lit_start, lits.len, &mut lit, prefix, dict,
                )?;
                // SAFETY: the new positions are within the output and literals.
                unsafe {
                    op = base.add(o);
                    lp = lit_start.add(lit);
                }
                continue;
            }
            // SAFETY: the sequence plus WILDCOPY_OVERLENGTH fits in the
            // output, so every wildcopy write is in bounds; the literals have
            // WILDCOPY_OVERLENGTH readable bytes past their end (see
            // `decode_literals`); match sources are checked to lie in the
            // dictionary (`dict_match`) or in already written output.
            unsafe {
                copy::copy16(op, lp);
                if seq.lit_len > 16 {
                    copy::wildcopy(op.add(16), lp.add(16), seq.lit_len - 16);
                }
                lp = lp.add(seq.lit_len);
                let mut mop = op.add(seq.lit_len);
                let op_end = op.add(seq_len);
                let match_len = seq.match_len;
                let offset = seq.offset;
                let in_prefix = mop as usize - prefix_start as usize;
                if offset > in_prefix {
                    dict_match(mop, offset, in_prefix, match_len, prefix_start, dict)?;
                    op = op_end;
                    continue;
                }
                let mut mp: *const u8 = mop.sub(offset);
                if offset >= 16 {
                    copy::wildcopy(mop, mp, match_len);
                } else {
                    copy::overlap_copy8(&mut mop, &mut mp, offset);
                    if match_len > 8 {
                        copy::wildcopy8(mop, mp, match_len - 8);
                    }
                }
                op = op_end;
            }
        }
    }
    *rep = reps;
    if !br.is_end() {
        return Err(Error::Corrupted);
    }
    // Last literals.
    let last = lit_end - lp as usize;
    let o = op as usize - base as usize;
    if last > cap - o {
        return Err(Error::OutputTooSmall);
    }
    // SAFETY: `last` literals remain and the output has room for them.
    unsafe { ptr::copy_nonoverlapping(lp, op, last) };
    out.pos = o + last;
    Ok(())
}

/// Exact-size execution near the end of the buffer (libzstd's
/// `ZSTD_execSequenceEnd`). Returns the new output position.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments, unsafe_code)]
fn exec_sequence_end(
    base: *mut u8,
    cap: usize,
    o: usize,
    seq: Seq,
    lit_base: *const u8,
    lit_total: usize,
    lit: &mut usize,
    prefix: usize,
    dict: &[u8],
) -> Result<usize> {
    let total = seq
        .lit_len
        .checked_add(seq.match_len)
        .ok_or(Error::Corrupted)?;
    if total > cap - o {
        return Err(Error::OutputTooSmall);
    }
    if seq.lit_len > lit_total - *lit {
        return Err(Error::Corrupted);
    }
    let o_lit_end = o + seq.lit_len;
    let in_prefix = o_lit_end - prefix;
    if seq.offset > in_prefix + dict.len() {
        return Err(Error::Corrupted);
    }
    // SAFETY: the literal and output ranges were checked above; the match
    // source is in the dictionary or before `op` in the output.
    unsafe {
        ptr::copy_nonoverlapping(lit_base.add(*lit), base.add(o), seq.lit_len);
        *lit += seq.lit_len;
        let mut op = base.add(o_lit_end);
        let mut match_len = seq.match_len;
        if seq.offset > in_prefix {
            let back = seq.offset - in_prefix;
            let dp = dict.as_ptr().add(dict.len() - back);
            let n = back.min(match_len);
            ptr::copy_nonoverlapping(dp, op, n);
            op = op.add(n);
            match_len -= n;
            if match_len == 0 {
                return Ok(o + total);
            }
            // The rest starts at the beginning of the prefix.
            let mp = base.add(prefix);
            overlapping_copy(op, mp, match_len);
        } else {
            let mp = op.sub(seq.offset);
            overlapping_copy(op, mp, match_len);
        }
    }
    Ok(o + total)
}

/// Forward copy where `src` may be less than `len` bytes before `dst`
/// (the copy repeats the pattern, like LZ77).
#[inline]
#[allow(unsafe_code)]
unsafe fn overlapping_copy(dst: *mut u8, src: *const u8, len: usize) {
    let dist = dst as usize - src as usize;
    // SAFETY: per the caller, `src < dst` and both ranges are in bounds.
    unsafe {
        if dist >= len {
            ptr::copy_nonoverlapping(src, dst, len);
        } else {
            for i in 0..len {
                *dst.add(i) = *src.add(i);
            }
        }
    }
}

/// A match that starts in the dictionary (rare: kept out of the hot loop
/// so the dictionary's pointer and length don't take registers there).
#[cold]
#[inline(never)]
#[allow(unsafe_code)]
fn dict_match(
    mop: *mut u8,
    offset: usize,
    in_prefix: usize,
    match_len: usize,
    prefix_start: *mut u8,
    dict: &[u8],
) -> Result<()> {
    if offset > in_prefix + dict.len() {
        return Err(Error::Corrupted);
    }
    let back = offset - in_prefix;
    let n = back.min(match_len);
    // SAFETY: `back <= dict.len()`; the caller checked the output has room
    // for the match; the rest of the match starts at `prefix_start`, which
    // is `offset` bytes before its destination.
    unsafe {
        ptr::copy_nonoverlapping(dict.as_ptr().add(dict.len() - back), mop, n);
        if match_len > n {
            overlapping_copy(mop.add(n), prefix_start, match_len - n);
        }
    }
    Ok(())
}
