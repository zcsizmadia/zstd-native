//! Blocks and frames (libzstd's `zstd_compress.c`: `ZSTD_entropyCompressSeqStore`,
//! `ZSTD_compressBlock_internal`, `ZSTD_compress_frameChunk`,
//! `ZSTD_writeFrameHeader`, `ZSTD_writeEpilogue`), one-shot.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::fast::{MatchState, build_seq_store};
use super::hist::hist_count_fast;
use super::literals::{HufCTables, compress_literals, min_gain};
use super::params::{CParams, Strategy, block_size_max, one_shot_cparams};
use super::presplit::{FpStats, optimal_block_size};
use super::seq_store::{REP_START_VALUE, SeqDef, SeqStore, seq_to_codes};
use super::sequences::{
    FseCTables, FseRepeat, SET_COMPRESSED, SeqFields, build_ctable, encode_sequences,
    select_encoding_type,
};
use crate::error::{Error, Result};
use crate::fse::{LL_DEFAULT_NORM, MAX_LL, MAX_ML, MAX_OFF, ML_DEFAULT_NORM, OF_DEFAULT_NORM};

const BLOCK_HEADER_SIZE: usize = 3;
const MIN_CBLOCK_SIZE: usize = 2;
const LONGNBSEQ: usize = 0x7F00;
const DEFAULT_MAX_OFF: u32 = 28;
const RLE_MAX_LENGTH: usize = 25;
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

/// libzstd's `ZSTD_entropyCompressSeqStore` (with `_internal` and
/// `_wExtLitBuffer`). Returns 0 when the block should be stored raw.
fn entropy_compress_seq_store(
    dst: &mut [u8],
    ss: &mut SeqStore,
    prev: &EntropyTables,
    next: &mut EntropyTables,
    cp: &CParams,
    block_size: usize,
) -> Result<usize> {
    let r = entropy_compress_internal(dst, ss, prev, next, cp);
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
    prev: &EntropyTables,
    next: &mut EntropyTables,
    cp: &CParams,
) -> Result<usize> {
    let strategy = cp.strategy as u32;
    let nb_seq = ss.nb_seq;
    let lit_size = ss.lit_len;
    let mut op = 0usize;
    {
        let suspect = nb_seq == 0 || lit_size / nb_seq >= SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO;
        op += compress_literals(
            dst,
            &ss.lit_buf[..lit_size],
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

    // ZSTD_buildSequencesStatistics
    let seq_head = op;
    op += 1;
    seq_to_codes(ss);
    let mut count = [0u32; 256];
    let mut last_count_size = 0usize;
    let (ll_codes, of_codes, ml_codes) = (
        &ss.ll_code[..nb_seq],
        &ss.of_code[..nb_seq],
        &ss.ml_code[..nb_seq],
    );

    let mut max = MAX_LL as u32;
    let most = hist_count_fast(&mut count, &mut max, ll_codes) as usize;
    next.fse.ll_repeat = prev.fse.ll_repeat;
    let ll_type = select_encoding_type(
        &mut next.fse.ll_repeat,
        &count,
        max,
        most,
        nb_seq,
        9,
        &prev.fse.ll,
        &LL_DEFAULT_NORM,
        6,
        true,
        strategy,
    );
    let n = build_ctable(
        &mut dst[op..],
        &mut next.fse.ll,
        9,
        ll_type,
        &mut count,
        max,
        ll_codes,
        &LL_DEFAULT_NORM,
        6,
        MAX_LL as u32,
        &prev.fse.ll,
    )?;
    if ll_type == SET_COMPRESSED {
        last_count_size = n;
    }
    op += n;

    let mut max = MAX_OFF as u32;
    let most = hist_count_fast(&mut count, &mut max, of_codes) as usize;
    let default_allowed = max <= DEFAULT_MAX_OFF;
    next.fse.of_repeat = prev.fse.of_repeat;
    let of_type = select_encoding_type(
        &mut next.fse.of_repeat,
        &count,
        max,
        most,
        nb_seq,
        8,
        &prev.fse.of,
        &OF_DEFAULT_NORM,
        5,
        default_allowed,
        strategy,
    );
    let n = build_ctable(
        &mut dst[op..],
        &mut next.fse.of,
        8,
        of_type,
        &mut count,
        max,
        of_codes,
        &OF_DEFAULT_NORM,
        5,
        DEFAULT_MAX_OFF,
        &prev.fse.of,
    )?;
    if of_type == SET_COMPRESSED {
        last_count_size = n;
    }
    op += n;

    let mut max = MAX_ML as u32;
    let most = hist_count_fast(&mut count, &mut max, ml_codes) as usize;
    next.fse.ml_repeat = prev.fse.ml_repeat;
    let ml_type = select_encoding_type(
        &mut next.fse.ml_repeat,
        &count,
        max,
        most,
        nb_seq,
        9,
        &prev.fse.ml,
        &ML_DEFAULT_NORM,
        6,
        true,
        strategy,
    );
    let n = build_ctable(
        &mut dst[op..],
        &mut next.fse.ml,
        9,
        ml_type,
        &mut count,
        max,
        ml_codes,
        &ML_DEFAULT_NORM,
        6,
        MAX_ML as u32,
        &prev.fse.ml,
    )?;
    if ml_type == SET_COMPRESSED {
        last_count_size = n;
    }
    op += n;
    dst[seq_head] = ((ll_type << 6) + (of_type << 4) + (ml_type << 2)) as u8;

    let bitstream = encode_sequences(
        &mut dst[op..],
        &next.fse.ml,
        ml_codes,
        &next.fse.of,
        of_codes,
        &next.fse.ll,
        ll_codes,
        &ss.sequences[..nb_seq],
    )?;
    op += bitstream;
    // zstd <= 1.3.4 decoders reject a last table description under 4 bytes
    // together with the bitstream: libzstd stores such blocks raw.
    if last_count_size != 0 && last_count_size + bitstream < 4 {
        return Ok(0);
    }
    Ok(op)
}

/// libzstd's `ZSTD_isRLE`.
fn is_rle(src: &[u8]) -> bool {
    src.iter().all(|&b| b == src[0])
}

/// Compression state for one frame at a time (libzstd's `ZSTD_CCtx` for
/// the one-shot path).
pub(crate) struct FrameCompressor {
    cp: CParams,
    ms: MatchState,
    ss: SeqStore,
    prev: Box<EntropyTables>,
    next: Box<EntropyTables>,
    rep: [u32; 3],
    fp: Box<FpStats>,
    first_block: bool,
}

impl FrameCompressor {
    pub(crate) fn new(level: i32, src_size: usize) -> Self {
        let cp = one_shot_cparams(level, src_size as u64);
        let bsm = block_size_max(&cp, src_size as u64);
        FrameCompressor {
            cp,
            ms: MatchState::new(cp),
            ss: SeqStore::new(bsm, cp.min_match),
            prev: Box::new(EntropyTables::new()),
            next: Box::new(EntropyTables::new()),
            rep: REP_START_VALUE,
            fp: Box::new(FpStats::new()),
            first_block: true,
        }
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
            let mut c = entropy_compress_seq_store(
                dst,
                &mut self.ss,
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

    /// Compresses `src` as one frame into `dst`; returns the frame size.
    pub(crate) fn compress(&mut self, dst: &mut [u8], src: &[u8]) -> Result<usize> {
        let n = src.len();
        let mut op = write_frame_header(dst, &self.cp, n as u64)?;
        if n == 0 {
            // ZSTD_writeEpilogue: one empty last raw block.
            if dst.len() - op < 3 {
                return Err(Error::OutputTooSmall);
            }
            dst[op..op + 3].copy_from_slice(&1u32.to_le_bytes()[..3]);
            return Ok(op + 3);
        }
        self.ms.update_window(0, n);
        let bsm = block_size_max(&self.cp, n as u64);
        let mut savings: i64 = 0;
        let mut pos = 0usize;
        while pos < n {
            let bs =
                optimal_block_size(&src[pos..], bsm, 0, self.cp.strategy, savings, &mut self.fp);
            let last = u32::from(bs == n - pos);
            if dst.len() - op < BLOCK_HEADER_SIZE + MIN_CBLOCK_SIZE + 1 {
                return Err(Error::OutputTooSmall);
            }
            self.ms.prepare_block(pos, bs);
            let c = self.compress_block(&mut dst[op + BLOCK_HEADER_SIZE..], src, pos, pos + bs)?;
            let c_size = if c == 0 {
                // ZSTD_noCompressBlock
                if bs + BLOCK_HEADER_SIZE > dst.len() - op {
                    return Err(Error::OutputTooSmall);
                }
                let h = last + ((bs as u32) << 3);
                dst[op..op + 3].copy_from_slice(&h.to_le_bytes()[..3]);
                dst[op + 3..op + 3 + bs].copy_from_slice(&src[pos..pos + bs]);
                bs + BLOCK_HEADER_SIZE
            } else {
                let h = if c == 1 {
                    last + (1 << 1) + ((bs as u32) << 3)
                } else {
                    last + (2 << 1) + ((c as u32) << 3)
                };
                dst[op..op + 3].copy_from_slice(&h.to_le_bytes()[..3]);
                c + BLOCK_HEADER_SIZE
            };
            savings += bs as i64 - c_size as i64;
            pos += bs;
            op += c_size;
            self.first_block = false;
        }
        Ok(op)
    }
}

/// libzstd's `ZSTD_writeFrameHeader` (content size known, no checksum, no
/// dictionary).
fn write_frame_header(dst: &mut [u8], cp: &CParams, src_size: u64) -> Result<usize> {
    if dst.len() < 18 {
        return Err(Error::OutputTooSmall);
    }
    let window_size = 1u64 << cp.window_log;
    let single = window_size >= src_size;
    let wlog_byte = ((cp.window_log - 10) << 3) as u8;
    let fcs_code = u8::from(src_size >= 256)
        + u8::from(src_size >= 65536 + 256)
        + u8::from(src_size >= 0xFFFF_FFFF);
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

/// Compresses `src` at `level` (like `ZSTD_compress`) into `dst`.
pub(crate) fn compress_to_slice_impl(src: &[u8], dst: &mut [u8], level: i32) -> Result<usize> {
    FrameCompressor::new(level, src.len()).compress(dst, src)
}

/// Compresses `src` at `level` into a new vector.
pub(crate) fn compress_impl(src: &[u8], level: i32) -> Result<Vec<u8>> {
    let mut out = alloc::vec![0u8; compress_bound(src.len()) + 18];
    let n = compress_to_slice_impl(src, &mut out, level)?;
    out.truncate(n);
    Ok(out)
}
