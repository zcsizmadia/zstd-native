//! Blocks and frames (libzstd's `zstd_compress.c`: `ZSTD_entropyCompressSeqStore`,
//! `ZSTD_compressBlock_internal`, `ZSTD_compress_frameChunk`,
//! `ZSTD_writeFrameHeader`, `ZSTD_writeEpilogue`), one-shot.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::fast::{MatchState, build_seq_store};
use super::hist::hist_count_fast;
use super::huf_enc::HufRepeat;
use super::literals::{HufCTables, compress_literals, min_gain};
use super::params::{
    CONTENTSIZE_UNKNOWN, CParams, Strategy, block_size_max, block_splitter_enabled,
    one_shot_cparams,
};
use super::presplit::{FpStats, optimal_block_size};
use super::seq_store::{
    LongLengthType, MAX_LL, MAX_ML, REP_START_VALUE, STREAM_ACCUMULATOR_MIN, SeqDef, SeqStore,
    highbit32, ll_code, max_nb_seq, ml_code,
};
use super::sequences::{
    FseCTables, FseRepeat, SET_COMPRESSED, SeqFields, build_ctable, encode_sequences,
    select_encoding_type,
};
use crate::error::{Error, Result};
use crate::fse::{
    LL_DEFAULT_NORM, MAX_LL as FSE_MAX_LL, MAX_ML as FSE_MAX_ML, MAX_OFF as FSE_MAX_OFF,
    ML_DEFAULT_NORM, OF_DEFAULT_NORM,
};

pub(super) const BLOCK_HEADER_SIZE: usize = 3;
const MIN_CBLOCK_SIZE: usize = 2;
pub(super) const LONGNBSEQ: usize = 0x7F00;
pub(super) const DEFAULT_MAX_OFF: u32 = 28;
pub(super) const RLE_MAX_LENGTH: usize = 25;
const SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO: usize = 20;

impl SeqFields for SeqDef {
    #[inline(always)]
    fn off_base(&self) -> u32 {
        self.off_base
    }
    #[inline(always)]
    fn lit_length(&self) -> u32 {
        u32::from(self.lit_length)
    }
    #[inline(always)]
    fn ml_base(&self) -> u32 {
        u32::from(self.ml_base)
    }
}

/// libzstd's `ZSTD_entropyCTables_t`.
#[derive(Clone)]
pub(crate) struct EntropyTables {
    pub huf: HufCTables,
    pub fse: FseCTables,
}

impl EntropyTables {
    fn new() -> Self {
        EntropyTables {
            huf: HufCTables::new(),
            fse: FseCTables::new(),
        }
    }
}

/// libzstd's `ZSTD_COMPRESSBOUND`: the largest compressed size of `n` bytes.
pub const fn compress_bound(n: usize) -> usize {
    n + (n >> 8)
        + if n < (128 << 10) {
            ((128 << 10) - n) >> 11
        } else {
            0
        }
}

/// libzstd's `ZSTD_literalsCompressionIsDisabled` (default mode: only the
/// fast strategy with acceleration, i.e. negative levels).
fn literals_compression_disabled(cp: &CParams) -> bool {
    cp.strategy == Strategy::Fast && cp.target_length > 0
}

/// A run of sequences of the seq store and their literals: the whole
/// block, or a partition of it for the block splitter (libzstd derives a
/// `SeqStore_t` with moved pointers, `ZSTD_deriveSeqStoreChunk`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeqChunk {
    /// The sequences `seq_start..seq_end` (and their codes).
    pub(crate) seq_start: usize,
    pub(crate) seq_end: usize,
    /// The literals `lit_start..lit_end` of the literal buffer.
    pub(crate) lit_start: usize,
    pub(crate) lit_end: usize,
    /// The long length, relative to `seq_start` (it may be `seq_end -
    /// seq_start`, just past the chunk, as in libzstd).
    pub(crate) long_length_type: LongLengthType,
    pub(crate) long_length_pos: u32,
}

impl SeqChunk {
    /// The whole store.
    pub(crate) fn full(ss: &SeqStore) -> Self {
        SeqChunk {
            seq_start: 0,
            seq_end: ss.nb_seq,
            lit_start: 0,
            lit_end: ss.lit_len,
            long_length_type: ss.long_length_type,
            long_length_pos: ss.long_length_pos,
        }
    }

    /// The number of sequences.
    #[inline]
    pub(crate) fn nb_seq(&self) -> usize {
        self.seq_end - self.seq_start
    }
}

/// `ZSTD_seqToCodes()` on a chunk: fills the code tables for its
/// sequences. Returns libzstd's `longOffsets` (only possible on 32-bit
/// targets).
pub(crate) fn chunk_to_codes(ss: &mut SeqStore, ch: &SeqChunk) -> bool {
    let (s, e) = (ch.seq_start, ch.seq_end);
    let mut long_offsets = false;
    let seqs = &ss.sequences[s..e];
    let ll = &mut ss.ll_code[s..e];
    let of = &mut ss.of_code[s..e];
    let ml = &mut ss.ml_code[s..e];
    for (u, q) in seqs.iter().enumerate() {
        let of_code = highbit32(q.off_base);
        ll[u] = ll_code(u32::from(q.lit_length)) as u8;
        of[u] = of_code as u8;
        ml[u] = ml_code(u32::from(q.ml_base)) as u8;
        debug_assert!(!(usize::BITS == 64 && of_code >= STREAM_ACCUMULATOR_MIN));
        if usize::BITS == 32 && of_code >= STREAM_ACCUMULATOR_MIN {
            long_offsets = true;
        }
    }
    // The long length may be just past a partition (libzstd writes its code
    // there too; it is the right code for that sequence).
    let p = s + ch.long_length_pos as usize;
    match ch.long_length_type {
        LongLengthType::LiteralLength if p < ss.ll_code.len() => ss.ll_code[p] = MAX_LL,
        LongLengthType::MatchLength if p < ss.ml_code.len() => ss.ml_code[p] = MAX_ML,
        _ => {}
    }
    long_offsets
}

/// libzstd's `ZSTD_symbolEncodingTypeStats_t`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SeqStats {
    pub(crate) ll_type: u32,
    pub(crate) of_type: u32,
    pub(crate) ml_type: u32,
    /// Size of the table descriptions written.
    pub(crate) size: usize,
    /// Size of the last compressed table description (0 if none).
    pub(crate) last_count_size: usize,
}

/// libzstd's `ZSTD_buildSequencesStatistics`: computes the codes of the
/// chunk's sequences (at least one), chooses each code's table encoding,
/// builds `next`'s tables and writes their descriptions to `dst`.
pub(crate) fn build_sequences_statistics(
    dst: &mut [u8],
    ss: &mut SeqStore,
    ch: &SeqChunk,
    prev: &FseCTables,
    next: &mut FseCTables,
    strategy: u32,
) -> Result<SeqStats> {
    let nb_seq = ch.nb_seq();
    debug_assert!(nb_seq != 0);
    chunk_to_codes(ss, ch);
    let mut count = [0u32; 256];
    let mut stats = SeqStats {
        ll_type: 0,
        of_type: 0,
        ml_type: 0,
        size: 0,
        last_count_size: 0,
    };
    let mut op = 0usize;
    let (ll_codes, of_codes, ml_codes) = (
        &ss.ll_code[ch.seq_start..ch.seq_end],
        &ss.of_code[ch.seq_start..ch.seq_end],
        &ss.ml_code[ch.seq_start..ch.seq_end],
    );

    // build CTable for Literal Lengths
    let mut max = FSE_MAX_LL as u32;
    let most = hist_count_fast(&mut count, &mut max, ll_codes) as usize;
    next.ll_repeat = prev.ll_repeat;
    stats.ll_type = select_encoding_type(
        &mut next.ll_repeat,
        &count,
        max,
        most,
        nb_seq,
        9,
        &prev.ll,
        &LL_DEFAULT_NORM,
        6,
        true,
        strategy,
    );
    let n = build_ctable(
        &mut dst[op..],
        &mut next.ll,
        9,
        stats.ll_type,
        &mut count,
        max,
        ll_codes,
        &LL_DEFAULT_NORM,
        6,
        FSE_MAX_LL as u32,
        &prev.ll,
    )?;
    if stats.ll_type == SET_COMPRESSED {
        stats.last_count_size = n;
    }
    op += n;

    // build CTable for Offsets
    let mut max = FSE_MAX_OFF as u32;
    let most = hist_count_fast(&mut count, &mut max, of_codes) as usize;
    // the basic table can only be used if max <= DefaultMaxOff, otherwise
    // the offsets are too large
    let default_allowed = max <= DEFAULT_MAX_OFF;
    next.of_repeat = prev.of_repeat;
    stats.of_type = select_encoding_type(
        &mut next.of_repeat,
        &count,
        max,
        most,
        nb_seq,
        8,
        &prev.of,
        &OF_DEFAULT_NORM,
        5,
        default_allowed,
        strategy,
    );
    let n = build_ctable(
        &mut dst[op..],
        &mut next.of,
        8,
        stats.of_type,
        &mut count,
        max,
        of_codes,
        &OF_DEFAULT_NORM,
        5,
        DEFAULT_MAX_OFF,
        &prev.of,
    )?;
    if stats.of_type == SET_COMPRESSED {
        stats.last_count_size = n;
    }
    op += n;

    // build CTable for MatchLengths
    let mut max = FSE_MAX_ML as u32;
    let most = hist_count_fast(&mut count, &mut max, ml_codes) as usize;
    next.ml_repeat = prev.ml_repeat;
    stats.ml_type = select_encoding_type(
        &mut next.ml_repeat,
        &count,
        max,
        most,
        nb_seq,
        9,
        &prev.ml,
        &ML_DEFAULT_NORM,
        6,
        true,
        strategy,
    );
    let n = build_ctable(
        &mut dst[op..],
        &mut next.ml,
        9,
        stats.ml_type,
        &mut count,
        max,
        ml_codes,
        &ML_DEFAULT_NORM,
        6,
        FSE_MAX_ML as u32,
        &prev.ml,
    )?;
    if stats.ml_type == SET_COMPRESSED {
        stats.last_count_size = n;
    }
    op += n;
    stats.size = op;
    Ok(stats)
}

/// libzstd's `ZSTD_entropyCompressSeqStore` (with `_internal` and
/// `_wExtLitBuffer`) on the chunk `ch` of `ss`, from a block (or
/// partition) of `block_size` bytes. Returns 0 when it should be stored
/// raw.
pub(crate) fn entropy_compress_seq_store(
    dst: &mut [u8],
    ss: &mut SeqStore,
    ch: &SeqChunk,
    prev: &EntropyTables,
    next: &mut EntropyTables,
    cp: &CParams,
    block_size: usize,
) -> Result<usize> {
    let r = entropy_compress_internal(dst, ss, ch, prev, next, cp);
    let c = match r {
        Ok(0) => return Ok(0),
        Err(Error::OutputTooSmall) if block_size <= dst.len() => return Ok(0),
        Err(e) => return Err(e),
        Ok(c) => c,
    };
    let max_c = block_size - min_gain(block_size, cp.strategy as u32);
    if c >= max_c {
        return Ok(0);
    }
    Ok(c)
}

fn entropy_compress_internal(
    dst: &mut [u8],
    ss: &mut SeqStore,
    ch: &SeqChunk,
    prev: &EntropyTables,
    next: &mut EntropyTables,
    cp: &CParams,
) -> Result<usize> {
    let strategy = cp.strategy as u32;
    let nb_seq = ch.nb_seq();
    let lit_size = ch.lit_end - ch.lit_start;
    let mut op = 0usize;
    {
        let suspect = nb_seq == 0 || lit_size / nb_seq >= SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO;
        op += compress_literals(
            dst,
            &ss.lit_buf[ch.lit_start..ch.lit_end],
            &prev.huf,
            &mut next.huf,
            strategy,
            literals_compression_disabled(cp),
            suspect,
        )?;
    }
    if dst.len() - op < 3 + 1 {
        return Err(Error::OutputTooSmall);
    }
    if nb_seq < 128 {
        dst[op] = nb_seq as u8;
        op += 1;
    } else if nb_seq < LONGNBSEQ {
        dst[op] = ((nb_seq >> 8) + 0x80) as u8;
        dst[op + 1] = nb_seq as u8;
        op += 2;
    } else {
        dst[op] = 0xFF;
        dst[op + 1..op + 3].copy_from_slice(&((nb_seq - LONGNBSEQ) as u16).to_le_bytes());
        op += 3;
    }
    if nb_seq == 0 {
        next.fse.clone_from(&prev.fse);
        return Ok(op);
    }

    let seq_head = op;
    op += 1;
    let stats =
        build_sequences_statistics(&mut dst[op..], ss, ch, &prev.fse, &mut next.fse, strategy)?;
    op += stats.size;
    dst[seq_head] = ((stats.ll_type << 6) + (stats.of_type << 4) + (stats.ml_type << 2)) as u8;

    let (s, e) = (ch.seq_start, ch.seq_end);
    let bitstream = encode_sequences(
        &mut dst[op..],
        &next.fse.ml,
        &ss.ml_code[s..e],
        &next.fse.of,
        &ss.of_code[s..e],
        &next.fse.ll,
        &ss.ll_code[s..e],
        &ss.sequences[s..e],
    )?;
    op += bitstream;
    // zstd <= 1.3.4 decoders reject a last table description under 4 bytes
    // together with the bitstream: libzstd stores such blocks raw.
    if stats.last_count_size != 0 && stats.last_count_size + bitstream < 4 {
        return Ok(0);
    }
    Ok(op)
}

/// libzstd's `ZSTD_isRLE`.
pub(super) fn is_rle(src: &[u8]) -> bool {
    src.iter().all(|&b| b == src[0])
}

/// libzstd's `ZSTD_compressionStage_e` (`cctx->stage`) for a started frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameStage {
    /// `ZSTDcs_init`: the frame header is not written yet.
    Init,
    /// `ZSTDcs_ongoing`: blocks are being written.
    Ongoing,
    /// `ZSTDcs_ending`: the last block was written.
    Ending,
}

/// Compression state for one frame at a time (libzstd's `ZSTD_CCtx`
/// without the streaming buffers, see [`StreamEncoder`](super::stream::StreamEncoder)).
pub(crate) struct FrameCompressor {
    pub(super) cp: CParams,
    pub(super) ms: MatchState,
    pub(super) ss: SeqStore,
    /// `blockState.prevCBlock->entropy`
    pub(super) prev: Box<EntropyTables>,
    /// `blockState.nextCBlock->entropy`
    pub(super) next: Box<EntropyTables>,
    /// `blockState.prevCBlock->rep`
    pub(super) rep: [u32; 3],
    fp: Box<FpStats>,
    /// `isFirstBlock`
    pub(super) first_block: bool,
    /// The post-block splitter is enabled (`ZSTD_blockSplitterEnabled`).
    split: bool,
    /// `blockSizeMax`
    pub(super) block_size_max: usize,
    /// `stage`
    pub(super) stage: FrameStage,
    /// `pledgedSrcSizePlusOne - 1`: the frame's content size, if known.
    pledged: Option<u64>,
    /// `consumedSrcSize` and `producedCSize` for the frame.
    consumed: u64,
    produced: u64,
}

impl FrameCompressor {
    pub(crate) fn new(level: i32, src_size: usize) -> Self {
        let cp = one_shot_cparams(level, src_size as u64);
        Self::with_params(cp, Some(src_size as u64))
    }

    /// A compressor for a frame of `pledged` bytes (`None`: unknown) with
    /// the resolved parameters `cp`.
    pub(crate) fn with_params(cp: CParams, pledged: Option<u64>) -> Self {
        let bsm = block_size_max(&cp, pledged.unwrap_or(CONTENTSIZE_UNKNOWN));
        FrameCompressor {
            cp,
            ms: MatchState::new(cp),
            ss: SeqStore::new(bsm, cp.min_match),
            prev: Box::new(EntropyTables::new()),
            next: Box::new(EntropyTables::new()),
            rep: REP_START_VALUE,
            fp: Box::new(FpStats::new()),
            first_block: true,
            split: block_splitter_enabled(&cp),
            block_size_max: bsm,
            stage: FrameStage::Init,
            pledged,
            consumed: 0,
            produced: 0,
        }
    }

    /// Prepares for a new frame of `src_size` bytes at `level`, reusing the
    /// buffers (libzstd's `ZSTD_resetCCtx_internal` with a clean match state
    /// and `ZSTD_reset_compressedBlockState`).
    pub(crate) fn reset(&mut self, level: i32, src_size: usize) {
        let cp = one_shot_cparams(level, src_size as u64);
        self.reset_with_params(cp, Some(src_size as u64));
    }

    /// As [`reset`](Self::reset), with resolved parameters and a frame of
    /// `pledged` bytes (`None`: unknown).
    pub(crate) fn reset_with_params(&mut self, cp: CParams, pledged: Option<u64>) {
        let bsm = block_size_max(&cp, pledged.unwrap_or(CONTENTSIZE_UNKNOWN));
        self.cp = cp;
        self.ms.reset(cp);
        if self.ss.max_nb_lit < bsm || self.ss.max_nb_seq < max_nb_seq(bsm, cp.min_match) {
            self.ss = SeqStore::new(bsm, cp.min_match);
        } else {
            self.ss.reset();
        }
        // Table contents don't matter while the repeat modes say none.
        for e in [&mut self.prev, &mut self.next] {
            e.huf.repeat = HufRepeat::None;
            e.fse.ll_repeat = FseRepeat::None;
            e.fse.of_repeat = FseRepeat::None;
            e.fse.ml_repeat = FseRepeat::None;
        }
        self.rep = REP_START_VALUE;
        self.first_block = true;
        self.split = block_splitter_enabled(&cp);
        self.block_size_max = bsm;
        self.stage = FrameStage::Init;
        self.pledged = pledged;
        self.consumed = 0;
        self.produced = 0;
    }

    /// libzstd's `ZSTD_compressBlock_internal`: 0 = store raw, 1 = RLE
    /// (`dst[0]` holds the byte), else a compressed block's size.
    fn compress_block(
        &mut self,
        dst: &mut [u8],
        src: &[u8],
        start: usize,
        end: usize,
    ) -> Result<usize> {
        let mut next_rep = self.rep;
        let c_size = if !build_seq_store(&mut self.ms, &mut self.ss, &mut next_rep, src, start, end)
        {
            0
        } else {
            let ch = SeqChunk::full(&self.ss);
            let mut c = entropy_compress_seq_store(
                dst,
                &mut self.ss,
                &ch,
                &self.prev,
                &mut self.next,
                &self.cp,
                end - start,
            )?;
            if !self.first_block && c < RLE_MAX_LENGTH && is_rle(&src[start..end]) {
                c = 1;
                dst[0] = src[start];
            }
            c
        };
        if c_size > 1 {
            core::mem::swap(&mut self.prev, &mut self.next);
            self.rep = next_rep;
        }
        if self.prev.fse.of_repeat == FseRepeat::Valid {
            self.prev.fse.of_repeat = FseRepeat::Check;
        }
        Ok(c_size)
    }

    /// libzstd's `ZSTD_compress_frameChunk`: compresses `buf[start..end]`
    /// (already in the window) as blocks of at most `blockSizeMax` bytes,
    /// the pre-splitter deciding their sizes; with `last_chunk`, the last
    /// one ends the frame. Returns the size written.
    fn frame_chunk(
        &mut self,
        dst: &mut [u8],
        buf: &[u8],
        start: usize,
        end: usize,
        last_chunk: bool,
    ) -> Result<usize> {
        let bsm = self.block_size_max;
        let mut savings = self.consumed as i64 - self.produced as i64;
        let mut op = 0usize;
        let mut pos = start;
        while pos < end {
            let bs = optimal_block_size(
                &buf[pos..end],
                bsm,
                0,
                self.cp.strategy,
                savings,
                &mut self.fp,
            );
            let last = u32::from(last_chunk && bs == end - pos);
            if dst.len() - op < BLOCK_HEADER_SIZE + MIN_CBLOCK_SIZE + 1 {
                return Err(Error::OutputTooSmall);
            }
            self.ms.prepare_block(pos, bs);
            let c_size = if self.split {
                self.compress_block_split(&mut dst[op..], buf, pos, pos + bs, last)?
            } else {
                let c =
                    self.compress_block(&mut dst[op + BLOCK_HEADER_SIZE..], buf, pos, pos + bs)?;
                if c == 0 {
                    // ZSTD_noCompressBlock
                    if bs + BLOCK_HEADER_SIZE > dst.len() - op {
                        return Err(Error::OutputTooSmall);
                    }
                    let h = last + ((bs as u32) << 3);
                    dst[op..op + 3].copy_from_slice(&h.to_le_bytes()[..3]);
                    dst[op + 3..op + 3 + bs].copy_from_slice(&buf[pos..pos + bs]);
                    bs + BLOCK_HEADER_SIZE
                } else {
                    let h = if c == 1 {
                        last + (1 << 1) + ((bs as u32) << 3)
                    } else {
                        last + (2 << 1) + ((c as u32) << 3)
                    };
                    dst[op..op + 3].copy_from_slice(&h.to_le_bytes()[..3]);
                    c + BLOCK_HEADER_SIZE
                }
            };
            savings += bs as i64 - c_size as i64;
            pos += bs;
            op += c_size;
            self.first_block = false;
        }
        if last_chunk && op > 0 {
            self.stage = FrameStage::Ending;
        }
        Ok(op)
    }

    /// libzstd's `ZSTD_compressContinue_internal` (frame mode): writes the
    /// frame header on the first call, appends `buf[start..end]` to the
    /// window and compresses it. Returns the size written.
    ///
    /// # Errors
    ///
    /// [`Error::OutputTooSmall`], or [`Error::SrcSizeWrong`] when more than
    /// the pledged size was given.
    pub(super) fn compress_continue(
        &mut self,
        dst: &mut [u8],
        buf: &[u8],
        start: usize,
        end: usize,
        last_chunk: bool,
    ) -> Result<usize> {
        let mut fh_size = 0;
        if self.stage == FrameStage::Init {
            fh_size = write_frame_header(dst, &self.cp, self.pledged)?;
            self.stage = FrameStage::Ongoing;
        }
        if start == end {
            return Ok(fh_size);
        }
        self.ms.update_window(start, end - start);
        let c_size = self.frame_chunk(&mut dst[fh_size..], buf, start, end, last_chunk)?;
        self.consumed += (end - start) as u64;
        self.produced += (c_size + fh_size) as u64;
        if self.pledged.is_some_and(|p| self.consumed > p) {
            return Err(Error::SrcSizeWrong);
        }
        Ok(c_size + fh_size)
    }

    /// libzstd's `ZSTD_compressEnd_public`: [`compress_continue`](Self::compress_continue)
    /// with the last chunk, then `ZSTD_writeEpilogue` (an empty last block
    /// if the last block isn't written yet).
    ///
    /// # Errors
    ///
    /// As `compress_continue`, and [`Error::SrcSizeWrong`] when the frame
    /// holds less than the pledged size.
    pub(super) fn compress_end(
        &mut self,
        dst: &mut [u8],
        buf: &[u8],
        start: usize,
        end: usize,
    ) -> Result<usize> {
        let mut op = self.compress_continue(dst, buf, start, end, true)?;
        debug_assert!(self.stage != FrameStage::Init);
        if self.stage != FrameStage::Ending {
            // one last empty raw block, marked last
            if dst.len() - op < BLOCK_HEADER_SIZE {
                return Err(Error::OutputTooSmall);
            }
            dst[op..op + 3].copy_from_slice(&1u32.to_le_bytes()[..3]);
            op += BLOCK_HEADER_SIZE;
        }
        self.stage = FrameStage::Init;
        if self.pledged.is_some_and(|p| self.consumed != p) {
            return Err(Error::SrcSizeWrong);
        }
        Ok(op)
    }

    /// Compresses `src` as one frame into `dst`; returns the frame size.
    pub(crate) fn compress(&mut self, dst: &mut [u8], src: &[u8]) -> Result<usize> {
        debug_assert_eq!(self.pledged, Some(src.len() as u64));
        self.compress_end(dst, src, 0, src.len())
    }
}

/// libzstd's `ZSTD_writeFrameHeader` (no checksum, no dictionary): with the
/// content size when `pledged` holds it.
fn write_frame_header(dst: &mut [u8], cp: &CParams, pledged: Option<u64>) -> Result<usize> {
    if dst.len() < 18 {
        return Err(Error::OutputTooSmall);
    }
    let window_size = 1u64 << cp.window_log;
    let src_size = pledged.unwrap_or(0);
    let content_size_flag = pledged.is_some();
    let single = content_size_flag && window_size >= src_size;
    let wlog_byte = ((cp.window_log - 10) << 3) as u8;
    let fcs_code = if content_size_flag {
        u8::from(src_size >= 256)
            + u8::from(src_size >= 65536 + 256)
            + u8::from(src_size >= 0xFFFF_FFFF)
    } else {
        0
    };
    dst[..4].copy_from_slice(&crate::frame::MAGIC.to_le_bytes());
    dst[4] = (u8::from(single) << 5) + (fcs_code << 6);
    let mut pos = 5;
    if !single {
        dst[pos] = wlog_byte;
        pos += 1;
    }
    match fcs_code {
        0 => {
            if single {
                dst[pos] = src_size as u8;
                pos += 1;
            }
        }
        1 => {
            dst[pos..pos + 2].copy_from_slice(&((src_size - 256) as u16).to_le_bytes());
            pos += 2;
        }
        2 => {
            dst[pos..pos + 4].copy_from_slice(&(src_size as u32).to_le_bytes());
            pos += 4;
        }
        _ => {
            dst[pos..pos + 8].copy_from_slice(&src_size.to_le_bytes());
            pos += 8;
        }
    }
    Ok(pos)
}

/// Compresses `src` at `level` into `dst` with `fc` (or a new compressor).
/// With `fresh`, a reused `fc` behaves like a new one, as libzstd's
/// one-shot `ZSTD_compress` (a new context per call); otherwise like a
/// reused libzstd context, whose row match finder hash salt changes from
/// frame to frame.
pub(crate) fn compress_to_slice_with(
    fc: &mut Option<Box<FrameCompressor>>,
    src: &[u8],
    dst: &mut [u8],
    level: i32,
    fresh: bool,
) -> Result<usize> {
    let fc = match fc {
        Some(f) => {
            if fresh {
                f.ms.clear_hash_salt();
            }
            f.reset(level, src.len());
            f
        }
        None => fc.insert(Box::new(FrameCompressor::new(level, src.len()))),
    };
    fc.compress(dst, src)
}

/// As [`compress_to_slice_with`], into a new vector.
pub(crate) fn compress_with(
    fc: &mut Option<Box<FrameCompressor>>,
    src: &[u8],
    level: i32,
    fresh: bool,
) -> Result<Vec<u8>> {
    let mut out = alloc::vec![0u8; compress_bound(src.len())];
    let n = compress_to_slice_with(fc, src, &mut out, level, fresh)?;
    out.truncate(n);
    Ok(out)
}
