//! The sequence store and the match finders' shared helpers, ported from
//! libzstd 1.5.7's `compress/zstd_compress_internal.h` and
//! `compress/zstd_compress.c`: `SeqStore_t`, `ZSTD_storeSeq`,
//! `ZSTD_seqToCodes`, `ZSTD_LLcode` / `ZSTD_MLcode`, `ZSTD_count`,
//! `ZSTD_count_2segments` and the `ZSTD_hashPtr` hashes.
//!
//! Offsets are stored in libzstd's "offBase" sum type: repcodes 1..=3 are
//! stored as themselves ([`repcode_to_offbase`], [`REPCODE1_TO_OFFBASE`]),
//! real offsets as `offset + 3` ([`offset_to_offbase`]). Match lengths are
//! stored as `mlBase = matchLength - MINMATCH`. A single literal or match
//! length per block may exceed `u16`; it is stored truncated and flagged by
//! `long_length_type` / `long_length_pos` (libzstd's `longLengthType` /
//! `longLengthPos`).

use alloc::vec;
use alloc::vec::Vec;
use core::ptr;

/// `ZSTD_REP_NUM`.
pub(crate) const REP_NUM: u32 = 3;
/// `MINMATCH`.
pub(crate) const MINMATCH: u32 = 3;
/// `WILDCOPY_OVERLENGTH`: slack after the literal buffer, and the
/// over-read distance `ZSTD_storeSeq` may use before `litLimit`.
pub(crate) const WILDCOPY_OVERLENGTH: usize = 32;
/// `MaxLL`.
pub(crate) const MAX_LL: u8 = 35;
/// `MaxML`.
pub(crate) const MAX_ML: u8 = 52;
/// `MaxOff`.
pub(crate) const MAX_OFF: u8 = 31;
/// `STREAM_ACCUMULATOR_MIN` (bitstream.h): 25 on 32-bit, 57 on 64-bit.
pub(crate) const STREAM_ACCUMULATOR_MIN: u32 = if usize::BITS == 32 { 25 } else { 57 };
/// `repStartValue`: the initial repeat offsets of a frame.
pub(crate) const REP_START_VALUE: [u32; 3] = [1, 4, 8];

/// `REPCODE1_TO_OFFBASE`.
pub(crate) const REPCODE1_TO_OFFBASE: u32 = 1;

/// `REPCODE_TO_OFFBASE(r)`: repcode IDs 1..=3 are stored as-is.
#[inline(always)]
pub(crate) const fn repcode_to_offbase(r: u32) -> u32 {
    debug_assert!(r >= 1 && r <= REP_NUM);
    r
}

/// `OFFSET_TO_OFFBASE(o)`: `o + ZSTD_REP_NUM`.
#[inline(always)]
pub(crate) const fn offset_to_offbase(o: u32) -> u32 {
    debug_assert!(o > 0);
    o + REP_NUM
}

/// `OFFBASE_IS_OFFSET(o)`.
#[inline(always)]
pub(crate) const fn offbase_is_offset(o: u32) -> bool {
    o > REP_NUM
}

/// `OFFBASE_IS_REPCODE(o)`.
#[inline(always)]
pub(crate) const fn offbase_is_repcode(o: u32) -> bool {
    1 <= o && o <= REP_NUM
}

/// `OFFBASE_TO_OFFSET(o)`.
#[inline(always)]
pub(crate) const fn offbase_to_offset(o: u32) -> u32 {
    debug_assert!(offbase_is_offset(o));
    o - REP_NUM
}

/// `ZSTD_updateRep()`: updates the repeat offsets after a sequence with
/// `off_base` and `ll0 = (litLength == 0)`.
#[inline]
pub(crate) fn update_rep(rep: &mut [u32; 3], off_base: u32, ll0: bool) {
    if offbase_is_offset(off_base) {
        rep[2] = rep[1];
        rep[1] = rep[0];
        rep[0] = offbase_to_offset(off_base);
    } else {
        let rep_code = off_base - 1 + ll0 as u32;
        if rep_code > 0 {
            let current_offset = if rep_code == REP_NUM {
                rep[0].wrapping_sub(1)
            } else {
                rep[rep_code as usize]
            };
            rep[2] = if rep_code >= 2 { rep[1] } else { rep[2] };
            rep[1] = rep[0];
            rep[0] = current_offset;
        }
    }
}

/// `SeqDef`: one stored sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SeqDef {
    /// `offBase`: offset + 3, or repcode 1..=3.
    pub(crate) off_base: u32,
    /// `litLength` (low 16 bits if this is the long length).
    pub(crate) lit_length: u16,
    /// `mlBase = matchLength - MINMATCH` (low 16 bits if long).
    pub(crate) ml_base: u16,
}

/// `ZSTD_longLengthType_e`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LongLengthType {
    /// `ZSTD_llt_none`
    None,
    /// `ZSTD_llt_literalLength`
    LiteralLength,
    /// `ZSTD_llt_matchLength`
    MatchLength,
}

/// `ZSTD_SequenceLength`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SequenceLength {
    /// Full literal length.
    pub(crate) lit_length: u32,
    /// Full match length (MINMATCH added back).
    pub(crate) match_length: u32,
}

/// `SeqStore_t`. The pointer pairs of libzstd become a buffer plus a count:
/// `sequences[..nb_seq]` (libzstd's `sequencesStart..sequences`) and
/// `lit_buf[..lit_len]` (`litStart..lit`). The code tables hold
/// `max_nb_seq` entries, filled for `[..nb_seq]` by [`seq_to_codes`].
#[derive(Debug)]
pub(crate) struct SeqStore {
    pub(crate) sequences: Vec<SeqDef>,
    pub(crate) nb_seq: usize,
    /// `max_nb_lit + WILDCOPY_OVERLENGTH` bytes.
    pub(crate) lit_buf: Vec<u8>,
    pub(crate) lit_len: usize,
    pub(crate) ll_code: Vec<u8>,
    pub(crate) ml_code: Vec<u8>,
    pub(crate) of_code: Vec<u8>,
    pub(crate) max_nb_seq: usize,
    pub(crate) max_nb_lit: usize,
    pub(crate) long_length_type: LongLengthType,
    /// Index of the sequence the long length applies to.
    pub(crate) long_length_pos: u32,
}

/// `ZSTD_maxNbSeq()` (zstd_compress.c), without an external sequence
/// producer.
pub(crate) const fn max_nb_seq(block_size: usize, min_match: u32) -> usize {
    let divider = if min_match == 3 { 3 } else { 4 };
    block_size / divider
}

impl SeqStore {
    /// Allocates a store as `ZSTD_resetCCtx_internal` does: `max_nb_seq`
    /// sequences ([`max_nb_seq`]`(block_size, min_match)`) and `block_size`
    /// literals plus `WILDCOPY_OVERLENGTH`, where `block_size` is
    /// `cctx->blockSizeMax` (`params::block_size_max`).
    pub(crate) fn new(block_size: usize, min_match: u32) -> Self {
        let max_nb_seq = max_nb_seq(block_size, min_match);
        Self {
            sequences: vec![SeqDef::default(); max_nb_seq],
            nb_seq: 0,
            lit_buf: vec![0; block_size + WILDCOPY_OVERLENGTH],
            lit_len: 0,
            ll_code: vec![0; max_nb_seq],
            ml_code: vec![0; max_nb_seq],
            of_code: vec![0; max_nb_seq],
            max_nb_seq,
            max_nb_lit: block_size,
            long_length_type: LongLengthType::None,
            long_length_pos: 0,
        }
    }

    /// `ZSTD_resetSeqStore()`.
    #[inline]
    pub(crate) fn reset(&mut self) {
        self.lit_len = 0;
        self.nb_seq = 0;
        self.long_length_type = LongLengthType::None;
    }

    /// The stored sequences.
    #[inline]
    pub(crate) fn sequences(&self) -> &[SeqDef] {
        &self.sequences[..self.nb_seq]
    }

    /// The stored literals.
    #[inline]
    pub(crate) fn literals(&self) -> &[u8] {
        &self.lit_buf[..self.lit_len]
    }

    /// `ZSTD_getSequenceLength()`: the full lengths of sequence `idx`.
    pub(crate) fn sequence_length(&self, idx: usize) -> SequenceLength {
        let seq = self.sequences[idx];
        let mut len = SequenceLength {
            lit_length: u32::from(seq.lit_length),
            match_length: u32::from(seq.ml_base) + MINMATCH,
        };
        if self.long_length_pos as usize == idx {
            match self.long_length_type {
                LongLengthType::LiteralLength => len.lit_length += 0x10000,
                LongLengthType::MatchLength => len.match_length += 0x10000,
                LongLengthType::None => {}
            }
        }
        len
    }

    /// `ZSTD_storeSeqOnly()`: stores a sequence without copying literals.
    /// `off_base` uses [`offset_to_offbase`] / [`repcode_to_offbase`];
    /// `match_length >= MINMATCH`.
    #[inline(always)]
    pub(crate) fn store_seq_only(&mut self, lit_length: usize, off_base: u32, match_length: usize) {
        let n = self.nb_seq;
        assert!(n < self.max_nb_seq);
        debug_assert!(lit_length <= crate::enc::params::BLOCKSIZE_MAX);
        if lit_length > 0xFFFF {
            debug_assert!(self.long_length_type == LongLengthType::None);
            self.long_length_type = LongLengthType::LiteralLength;
            self.long_length_pos = n as u32;
        }
        debug_assert!(match_length <= crate::enc::params::BLOCKSIZE_MAX);
        debug_assert!(match_length >= MINMATCH as usize);
        let ml_base = match_length - MINMATCH as usize;
        if ml_base > 0xFFFF {
            debug_assert!(self.long_length_type == LongLengthType::None);
            self.long_length_type = LongLengthType::MatchLength;
            self.long_length_pos = n as u32;
        }
        // The `assert!` above lets the compiler drop the bounds check here.
        self.sequences[n] = SeqDef {
            off_base,
            lit_length: lit_length as u16,
            ml_base: ml_base as u16,
        };
        self.nb_seq = n + 1;
    }

    /// `ZSTD_storeSeq()`: copies `src[lit_start..lit_start + lit_length]`
    /// to the literal buffer and stores the sequence. Like libzstd it may
    /// read up to `WILDCOPY_OVERLENGTH` bytes past the literals as long as
    /// that stays before `lit_limit` (libzstd's `litLimit`, the block end),
    /// and write up to that far past them in the literal buffer's slack.
    #[inline(always)]
    pub(crate) fn store_seq(
        &mut self,
        src: &[u8],
        lit_start: usize,
        lit_length: usize,
        lit_limit: usize,
        off_base: u32,
        match_length: usize,
    ) {
        assert!(lit_start + lit_length <= lit_limit && lit_limit <= src.len());
        assert!(self.lit_len + lit_length <= self.max_nb_lit);
        // SAFETY: the source range [lit_start, lit_start + lit_length) is in
        // `src` (asserted). The wild copy runs only when the literals end at
        // least WILDCOPY_OVERLENGTH bytes before `lit_limit <= src.len()`,
        // and reads/writes at most 15 bytes past the literal end (16-byte
        // chunks); the destination has `max_nb_lit + WILDCOPY_OVERLENGTH`
        // bytes and `lit_len + lit_length <= max_nb_lit` (asserted).
        #[allow(unsafe_code)]
        unsafe {
            let ip = src.as_ptr().add(lit_start);
            let op = self.lit_buf.as_mut_ptr().add(self.lit_len);
            if lit_start + lit_length + WILDCOPY_OVERLENGTH <= lit_limit {
                ptr::copy_nonoverlapping(ip, op, 16);
                let mut i = 16;
                while i < lit_length {
                    ptr::copy_nonoverlapping(ip.add(i), op.add(i), 16);
                    i += 16;
                }
            } else {
                ptr::copy_nonoverlapping(ip, op, lit_length);
            }
        }
        self.lit_len += lit_length;
        self.store_seq_only(lit_length, off_base, match_length);
    }

    /// `ZSTD_storeLastLiterals()`.
    #[inline]
    pub(crate) fn store_last_literals(&mut self, literals: &[u8]) {
        let end = self.lit_len + literals.len();
        self.lit_buf[self.lit_len..end].copy_from_slice(literals);
        self.lit_len = end;
    }
}

/// `ZSTD_highbit32`.
#[inline(always)]
pub(crate) const fn highbit32(v: u32) -> u32 {
    debug_assert!(v != 0);
    31 - v.leading_zeros()
}

/// `ZSTD_LLcode()`: the literal length code of `lit_length`.
#[inline(always)]
pub(crate) fn ll_code(lit_length: u32) -> u32 {
    #[rustfmt::skip]
    const LL_CODE: [u8; 64] = [
         0,  1,  2,  3,  4,  5,  6,  7,
         8,  9, 10, 11, 12, 13, 14, 15,
        16, 16, 17, 17, 18, 18, 19, 19,
        20, 20, 20, 20, 21, 21, 21, 21,
        22, 22, 22, 22, 22, 22, 22, 22,
        23, 23, 23, 23, 23, 23, 23, 23,
        24, 24, 24, 24, 24, 24, 24, 24,
        24, 24, 24, 24, 24, 24, 24, 24,
    ];
    const LL_DELTA_CODE: u32 = 19;
    if lit_length > 63 {
        highbit32(lit_length) + LL_DELTA_CODE
    } else {
        u32::from(LL_CODE[lit_length as usize])
    }
}

/// `ZSTD_MLcode()`: the match length code of `ml_base`
/// (`matchLength - MINMATCH`).
#[inline(always)]
pub(crate) fn ml_code(ml_base: u32) -> u32 {
    #[rustfmt::skip]
    const ML_CODE: [u8; 128] = [
         0,  1,  2,  3,  4,  5,  6,  7,  8,  9, 10, 11, 12, 13, 14, 15,
        16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
        32, 32, 33, 33, 34, 34, 35, 35, 36, 36, 36, 36, 37, 37, 37, 37,
        38, 38, 38, 38, 38, 38, 38, 38, 39, 39, 39, 39, 39, 39, 39, 39,
        40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40,
        41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41,
        42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42,
        42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42,
    ];
    const ML_DELTA_CODE: u32 = 36;
    if ml_base > 127 {
        highbit32(ml_base) + ML_DELTA_CODE
    } else {
        u32::from(ML_CODE[ml_base as usize])
    }
}

/// `ZSTD_seqToCodes()`: fills `ll_code`, `of_code` and `ml_code` for the
/// stored sequences (the long length, if any, gets `MaxLL` / `MaxML`).
/// Returns libzstd's `longOffsets`: an offset code needs more bits than the
/// bit stream accumulator guarantees (only possible on 32-bit targets).
pub(crate) fn seq_to_codes(ss: &mut SeqStore) -> bool {
    let nb_seq = ss.nb_seq;
    debug_assert!(nb_seq <= ss.max_nb_seq);
    let mut long_offsets = false;
    let seqs = &ss.sequences[..nb_seq];
    let ll = &mut ss.ll_code[..nb_seq];
    let of = &mut ss.of_code[..nb_seq];
    let ml = &mut ss.ml_code[..nb_seq];
    for (u, s) in seqs.iter().enumerate() {
        let of_code = highbit32(s.off_base);
        ll[u] = ll_code(u32::from(s.lit_length)) as u8;
        of[u] = of_code as u8;
        ml[u] = ml_code(u32::from(s.ml_base)) as u8;
        debug_assert!(!(usize::BITS == 64 && of_code >= STREAM_ACCUMULATOR_MIN));
        if usize::BITS == 32 && of_code >= STREAM_ACCUMULATOR_MIN {
            long_offsets = true;
        }
    }
    match ss.long_length_type {
        LongLengthType::LiteralLength => ss.ll_code[ss.long_length_pos as usize] = MAX_LL,
        LongLengthType::MatchLength => ss.ml_code[ss.long_length_pos as usize] = MAX_ML,
        LongLengthType::None => {}
    }
    long_offsets
}

// ---------------------------------------------------------------------------
// Match length counting and hashing over raw pointers (the match finders'
// hot loops work on pointers, like libzstd).

/// Reads 4 bytes, native order (`MEM_read32`).
///
/// # Safety
/// `p..p + 4` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn read32(p: *const u8) -> u32 {
    // SAFETY: per the caller.
    unsafe { ptr::read_unaligned(p.cast::<u32>()) }
}

/// Reads 4 bytes little-endian (`MEM_readLE32`).
///
/// # Safety
/// `p..p + 4` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn read_le32(p: *const u8) -> u32 {
    // SAFETY: per the caller.
    u32::from_le(unsafe { read32(p) })
}

/// Reads 8 bytes little-endian (`MEM_readLE64`).
///
/// # Safety
/// `p..p + 8` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn read_le64(p: *const u8) -> u64 {
    // SAFETY: per the caller.
    u64::from_le(unsafe { ptr::read_unaligned(p.cast::<u64>()) })
}

/// Reads a machine word little-endian.
///
/// # Safety
/// `p..p + size_of::<usize>()` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn read_le_word(p: *const u8) -> usize {
    // SAFETY: per the caller.
    usize::from_le(unsafe { ptr::read_unaligned(p.cast::<usize>()) })
}

/// `ZSTD_count()`: the length of the common prefix of `p_in` and `p_match`,
/// stopping at `p_in_limit`.
///
/// # Safety
/// `p_in..p_in_limit` must be readable, and so must
/// `p_match..p_match + (p_in_limit - p_in)`. `p_in <= p_in_limit` is not
/// required (the result is then 0).
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn count(p_in: *const u8, p_match: *const u8, p_in_limit: *const u8) -> usize {
    const W: usize = size_of::<usize>();
    // Same comparisons as libzstd (word steps, then 4/2/1 bytes), on one
    // offset `i` from both starts.
    let len = (p_in_limit as usize).saturating_sub(p_in as usize);
    let mut i = 0;
    // SAFETY: every read is of `k` bytes at offset `i` with `i + k <= len`,
    // from `p_in` and from `p_match`; both ranges are readable per the
    // caller.
    unsafe {
        while i + W <= len {
            let diff = read_le_word(p_match.add(i)) ^ read_le_word(p_in.add(i));
            if diff != 0 {
                return i + (diff.trailing_zeros() >> 3) as usize;
            }
            i += W;
        }
        if W == 8 && i + 4 <= len && read32(p_match.add(i)) == read32(p_in.add(i)) {
            i += 4;
        }
        if i + 2 <= len
            && ptr::read_unaligned(p_match.add(i).cast::<u16>())
                == ptr::read_unaligned(p_in.add(i).cast::<u16>())
        {
            i += 2;
        }
        if i < len && *p_match.add(i) == *p_in.add(i) {
            i += 1;
        }
    }
    i
}

/// `ZSTD_count_2segments()`: counts a match whose source may run from
/// `m_end` (the end of the extDict segment) into `i_start` (the prefix).
///
/// # Safety
/// As [`count`] for both segments: `ip..i_end` readable, `mtch..m_end`
/// readable, and `i_start..` readable for the remaining length.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn count_2segments(
    ip: *const u8,
    mtch: *const u8,
    i_end: *const u8,
    m_end: *const u8,
    i_start: *const u8,
) -> usize {
    let v_end = ip.wrapping_add(m_end as usize - mtch as usize);
    let v_end = if (v_end as usize) < (i_end as usize) {
        v_end
    } else {
        i_end
    };
    // SAFETY: per the caller.
    unsafe {
        let match_length = count(ip, mtch, v_end);
        if mtch.wrapping_add(match_length) != m_end {
            return match_length;
        }
        match_length + count(ip.add(match_length), i_start, i_end)
    }
}

/// `prime4bytes`.
pub(crate) const PRIME4BYTES: u32 = 2654435761;
/// `prime5bytes`.
pub(crate) const PRIME5BYTES: u64 = 889523592379;
/// `prime6bytes`.
pub(crate) const PRIME6BYTES: u64 = 227718039650203;
/// `prime7bytes`.
pub(crate) const PRIME7BYTES: u64 = 58295818150454627;
/// `prime8bytes`.
pub(crate) const PRIME8BYTES: u64 = 0xCF1BBCDCB7A56463;

/// `ZSTD_hash4()` (salt 0).
#[inline(always)]
pub(crate) const fn hash4(u: u32, h: u32) -> usize {
    (u.wrapping_mul(PRIME4BYTES) >> (32 - h)) as usize
}

/// `ZSTD_hash5()` (salt 0).
#[inline(always)]
pub(crate) const fn hash5(u: u64, h: u32) -> usize {
    ((u << (64 - 40)).wrapping_mul(PRIME5BYTES) >> (64 - h)) as usize
}

/// `ZSTD_hash6()` (salt 0).
#[inline(always)]
pub(crate) const fn hash6(u: u64, h: u32) -> usize {
    ((u << (64 - 48)).wrapping_mul(PRIME6BYTES) >> (64 - h)) as usize
}

/// `ZSTD_hash7()` (salt 0).
#[inline(always)]
pub(crate) const fn hash7(u: u64, h: u32) -> usize {
    ((u << (64 - 56)).wrapping_mul(PRIME7BYTES) >> (64 - h)) as usize
}

/// `ZSTD_hash8()` (salt 0).
#[inline(always)]
pub(crate) const fn hash8(u: u64, h: u32) -> usize {
    (u.wrapping_mul(PRIME8BYTES) >> (64 - h)) as usize
}

/// `ZSTD_hash4Ptr()`.
///
/// # Safety
/// `p..p + 4` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn hash4_ptr(p: *const u8, h: u32) -> usize {
    // SAFETY: per the caller.
    hash4(unsafe { read_le32(p) }, h)
}

/// `ZSTD_hash5Ptr()`.
///
/// # Safety
/// `p..p + 8` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn hash5_ptr(p: *const u8, h: u32) -> usize {
    // SAFETY: per the caller.
    hash5(unsafe { read_le64(p) }, h)
}

/// `ZSTD_hash6Ptr()`.
///
/// # Safety
/// `p..p + 8` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn hash6_ptr(p: *const u8, h: u32) -> usize {
    // SAFETY: per the caller.
    hash6(unsafe { read_le64(p) }, h)
}

/// `ZSTD_hash7Ptr()`.
///
/// # Safety
/// `p..p + 8` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn hash7_ptr(p: *const u8, h: u32) -> usize {
    // SAFETY: per the caller.
    hash7(unsafe { read_le64(p) }, h)
}

/// `ZSTD_hash8Ptr()`.
///
/// # Safety
/// `p..p + 8` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn hash8_ptr(p: *const u8, h: u32) -> usize {
    // SAFETY: per the caller.
    hash8(unsafe { read_le64(p) }, h)
}

/// `ZSTD_hashPtr()`: hashes the first `mls` bytes at `p` to `h_bits` bits
/// (`mls` outside 5..=8 hashes 4 bytes, like libzstd's `default:`).
///
/// # Safety
/// `p..p + 8` must be readable (`HASH_READ_SIZE`; 4 for `mls == 4`).
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn hash_ptr(p: *const u8, h_bits: u32, mls: u32) -> usize {
    debug_assert!(h_bits <= 32);
    // SAFETY: per the caller.
    unsafe {
        match mls {
            5 => hash5_ptr(p, h_bits),
            6 => hash6_ptr(p, h_bits),
            7 => hash7_ptr(p, h_bits),
            8 => hash8_ptr(p, h_bits),
            _ => hash4_ptr(p, h_bits),
        }
    }
}

/// Safe [`hash_ptr`] on a slice position (`pos + 8 <= src.len()`).
pub(crate) fn hash_at(src: &[u8], pos: usize, h_bits: u32, mls: u32) -> usize {
    assert!(pos + 8 <= src.len());
    // SAFETY: 8 bytes at `pos` are in `src` (asserted).
    #[allow(unsafe_code)]
    unsafe {
        hash_ptr(src.as_ptr().add(pos), h_bits, mls)
    }
}

/// Safe [`count`] on slice positions: the common prefix length of
/// `src[ip..limit]` and `src[mtch..]` (`mtch < ip`).
pub(crate) fn count_at(src: &[u8], ip: usize, mtch: usize, limit: usize) -> usize {
    assert!(mtch <= ip && limit <= src.len());
    // SAFETY: `ip..limit` is in `src`, and `mtch..mtch + (limit - ip)` too
    // since `mtch <= ip`.
    #[allow(unsafe_code)]
    unsafe {
        let p = src.as_ptr();
        count(p.add(ip.min(limit)), p.add(mtch.min(limit)), p.add(limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(ll_code(0), 0);
        assert_eq!(ll_code(15), 15);
        assert_eq!(ll_code(16), 16);
        assert_eq!(ll_code(17), 16);
        assert_eq!(ll_code(63), 24);
        assert_eq!(ll_code(64), 25); // highbit(64)=6 + 19
        assert_eq!(ll_code(65535), 34);
        assert_eq!(ll_code(65536), 35);
        assert_eq!(ml_code(0), 0);
        assert_eq!(ml_code(31), 31);
        assert_eq!(ml_code(32), 32);
        assert_eq!(ml_code(127), 42);
        assert_eq!(ml_code(128), 43); // highbit(128)=7 + 36
        assert_eq!(ml_code(65535), 51);
        assert_eq!(ml_code(65536), 52);
    }

    #[test]
    fn offbase() {
        assert_eq!(offset_to_offbase(1), 4);
        assert_eq!(repcode_to_offbase(2), 2);
        assert!(offbase_is_repcode(REPCODE1_TO_OFFBASE));
        assert!(offbase_is_offset(4));
        assert_eq!(offbase_to_offset(10), 7);
        let mut rep = REP_START_VALUE;
        update_rep(&mut rep, offset_to_offbase(100), false);
        assert_eq!(rep, [100, 1, 4]);
        update_rep(&mut rep, 1, false); // rep0: unchanged
        assert_eq!(rep, [100, 1, 4]);
        update_rep(&mut rep, 1, true); // ll0: rep1
        assert_eq!(rep, [1, 100, 4]);
        update_rep(&mut rep, 3, true); // ll0 + rep3 -> rep0 - 1
        assert_eq!(rep, [0, 1, 100]);
        update_rep(&mut rep, 2, false); // rep2 -> rotate rep1 to front
        assert_eq!(rep, [1, 0, 100]);
    }

    #[test]
    fn store_and_codes() {
        let src: Vec<u8> = (0..200u8).collect();
        let mut ss = SeqStore::new(1 << 17, 4);
        ss.store_seq(&src, 0, 10, 200, offset_to_offbase(1), 4);
        ss.store_seq(&src, 10, 0, 200, REPCODE1_TO_OFFBASE, 3 + 130);
        // literals near the end: no wild copy
        ss.store_seq(&src, 180, 15, 200, offset_to_offbase(1000), 5);
        ss.store_last_literals(&src[195..]);
        assert_eq!(ss.nb_seq, 3);
        let mut lits = src[..10].to_vec();
        lits.extend_from_slice(&src[180..]);
        assert_eq!(ss.literals(), &lits[..]);
        assert!(!seq_to_codes(&mut ss));
        assert_eq!(&ss.ll_code[..3], &[10, 0, 15]);
        assert_eq!(&ss.ml_code[..3], &[1, 43, 2]);
        // highbit(4)=2, highbit(1)=0, highbit(1003)=9
        assert_eq!(&ss.of_code[..3], &[2, 0, 9]);
        assert_eq!(ss.sequence_length(1).match_length, 133);
        ss.reset();
        assert_eq!(ss.nb_seq, 0);
        assert_eq!(ss.lit_len, 0);
    }

    #[test]
    fn long_lengths() {
        let src = vec![7u8; 140_000];
        let mut ss = SeqStore::new(1 << 17, 4);
        // Only one long length per block (a block is at most 128 KB).
        ss.store_seq(&src, 0, 5, src.len(), offset_to_offbase(8), 600);
        ss.store_seq(&src, 0, 0x10005, src.len(), offset_to_offbase(8), 4);
        assert_eq!(ss.long_length_type, LongLengthType::LiteralLength);
        assert_eq!(ss.long_length_pos, 1);
        assert_eq!(ss.sequence_length(1).lit_length, 0x10005);
        assert_eq!(ss.sequences[1].lit_length, 5);
        assert_eq!(&ss.literals()[5..5 + 0x10005], &src[..0x10005]);
        seq_to_codes(&mut ss);
        assert_eq!(ss.ll_code[1], MAX_LL);
        assert_eq!(ss.ll_code[0], 5);
        assert_eq!(ss.ml_code[0], ml_code(600 - 3) as u8);

        let mut ss = SeqStore::new(1 << 17, 4);
        ss.store_seq(&src, 0, 1, src.len(), offset_to_offbase(8), 70_000);
        assert_eq!(ss.long_length_type, LongLengthType::MatchLength);
        assert_eq!(ss.sequence_length(0).match_length, 70_000);
        seq_to_codes(&mut ss);
        assert_eq!(ss.ml_code[0], MAX_ML);
    }

    #[test]
    fn counting() {
        let mut a = vec![0u8; 64];
        for (i, b) in a.iter_mut().enumerate() {
            *b = (i % 7) as u8;
        }
        // period 7: src[i] == src[i - 7] everywhere
        for ip in 7..64 {
            for limit in ip..=64 {
                assert_eq!(count_at(&a, ip, ip - 7, limit), limit - ip, "{ip} {limit}");
            }
        }
        a[40] = 99;
        assert_eq!(count_at(&a, 20, 13, 64), 20);
        assert_eq!(count_at(&a, 20, 19, 64), 0);
        assert_eq!(count_at(&a, 64, 10, 64), 0);
    }

    #[test]
    fn count_two_segments() {
        let dict = b"xxxxabcdefgh";
        let prefix = b"ijklmnopabcdefghijklmnopq";
        // match at dict[4..] continues into prefix[0..].
        #[allow(unsafe_code)]
        // SAFETY: all pointers are within the two arrays.
        let n = unsafe {
            count_2segments(
                prefix.as_ptr().add(8),
                dict.as_ptr().add(4),
                prefix.as_ptr().add(prefix.len()),
                dict.as_ptr().add(dict.len()),
                prefix.as_ptr(),
            )
        };
        // "abcdefgh" (8) then "ijklmnop" vs prefix "ijklmnop" (8), then
        // "q" vs "a": stop.
        assert_eq!(n, 16);
    }

    #[test]
    fn hashes() {
        let data = *b"\x01\x02\x03\x04\x05\x06\x07\x08";
        let u32v = u32::from_le_bytes([1, 2, 3, 4]);
        let u64v = u64::from_le_bytes(data);
        assert_eq!(
            hash_at(&data, 0, 17, 4),
            (u32v.wrapping_mul(2654435761) >> 15) as usize
        );
        assert_eq!(hash_at(&data, 0, 17, 3), hash_at(&data, 0, 17, 4));
        let h5 = ((u64v << 24).wrapping_mul(889523592379) >> 47) as usize;
        assert_eq!(hash_at(&data, 0, 17, 5), h5);
        let h8 = (u64v.wrapping_mul(0xCF1BBCDCB7A56463) >> 47) as usize;
        assert_eq!(hash_at(&data, 0, 17, 8), h8);
        // mls bytes only: changing byte 6 does not change the 5- and
        // 6-byte hashes, but changes the 7-byte one.
        let mut d2 = data;
        d2[6] = 0xFF;
        assert_eq!(hash_at(&data, 0, 20, 5), hash_at(&d2, 0, 20, 5));
        assert_eq!(hash_at(&data, 0, 20, 6), hash_at(&d2, 0, 20, 6));
        assert_ne!(hash_at(&data, 0, 20, 7), hash_at(&d2, 0, 20, 7));
        for mls in 4..=8 {
            assert!(hash_at(&data, 0, 10, mls) < 1024);
        }
    }
}
