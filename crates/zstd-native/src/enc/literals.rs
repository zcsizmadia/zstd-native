//! Literals section (libzstd's `compress/zstd_compress_literals.c`).

use super::huf_enc::{
    HUF_FLAGS_OPTIMAL_DEPTH, HUF_FLAGS_PREFER_REPEAT, HUF_FLAGS_SUSPECT_UNCOMPRESSIBLE, HufCTable,
    HufRepeat, huf_compress1x_repeat, huf_compress4x_repeat,
};
use crate::error::{Error, Result};

/// Strategy numbers (libzstd's `ZSTD_strategy`) used by the heuristics here.
pub(crate) const STRAT_LAZY: u32 = 4;
pub(crate) const STRAT_BTULTRA: u32 = 8;

const LIT_HUF_LOG: u32 = 11;

/// libzstd's `ZSTD_hufCTables_t`: the Huffman table and its repeat state.
#[derive(Clone)]
pub(crate) struct HufCTables {
    pub table: HufCTable,
    pub repeat: HufRepeat,
}

impl HufCTables {
    pub(crate) fn new() -> Self {
        HufCTables {
            table: HufCTable::new(),
            repeat: HufRepeat::None,
        }
    }
}

/// libzstd's `ZSTD_minGain`.
pub(crate) fn min_gain(src_size: usize, strategy: u32) -> usize {
    let minlog = if strategy >= STRAT_BTULTRA {
        strategy - 1
    } else {
        6
    };
    (src_size >> minlog) + 2
}

/// libzstd's `ZSTD_noCompressLiterals`: a raw literals section.
pub(crate) fn no_compress_literals(dst: &mut [u8], src: &[u8]) -> Result<usize> {
    let n = src.len();
    let fl = 1 + usize::from(n > 31) + usize::from(n > 4095);
    if n + fl > dst.len() {
        return Err(Error::OutputTooSmall);
    }
    write_raw_rle_header(dst, fl, 0, n);
    dst[fl..fl + n].copy_from_slice(src);
    Ok(n + fl)
}

/// libzstd's `ZSTD_compressRleLiteralsBlock`.
fn rle_literals(dst: &mut [u8], src: &[u8]) -> Result<usize> {
    let n = src.len();
    let fl = 1 + usize::from(n > 31) + usize::from(n > 4095);
    if fl + 1 > dst.len() {
        return Err(Error::OutputTooSmall);
    }
    write_raw_rle_header(dst, fl, 1, n);
    dst[fl] = src[0];
    Ok(fl + 1)
}

/// Raw/RLE literals header: `kind` 0 (raw) or 1 (RLE), 1-3 bytes.
fn write_raw_rle_header(dst: &mut [u8], fl: usize, kind: u32, n: usize) {
    let n = n as u32;
    match fl {
        1 => dst[0] = (kind + (n << 3)) as u8,
        2 => dst[..2].copy_from_slice(&((kind + (1 << 2) + (n << 4)) as u16).to_le_bytes()),
        // A 4-byte write in libzstd; only 3 bytes belong to the header and
        // the 4th is overwritten by the literals right after.
        _ => dst[..3].copy_from_slice(&(kind + (3 << 2) + (n << 4)).to_le_bytes()[..3]),
    }
}

/// libzstd's `ZSTD_minLiteralsToCompress`.
fn min_literals_to_compress(strategy: u32, repeat: HufRepeat) -> usize {
    let shift = (9 - strategy as i32).min(3);
    if repeat == HufRepeat::Valid {
        6
    } else {
        8 << shift
    }
}

/// libzstd's `ZSTD_compressLiterals`: writes the literals section and
/// returns its size. `next` starts as a copy of `prev` and holds the
/// Huffman table the block uses.
pub(crate) fn compress_literals(
    dst: &mut [u8],
    src: &[u8],
    prev: &HufCTables,
    next: &mut HufCTables,
    strategy: u32,
    disable_compression: bool,
    suspect_uncompressible: bool,
) -> Result<usize> {
    let src_size = src.len();
    let lh_size = 3 + usize::from(src_size >= 1024) + usize::from(src_size >= 16 * 1024);
    let mut single_stream = src_size < 256;
    let mut h_type = 2u32; // set_compressed
    next.clone_from(prev);
    if disable_compression || src_size < min_literals_to_compress(strategy, prev.repeat) {
        return no_compress_literals(dst, src);
    }
    if dst.len() < lh_size + 1 {
        return Err(Error::OutputTooSmall);
    }
    let mut repeat = prev.repeat;
    let flags = (if strategy < STRAT_LAZY && src_size <= 1024 {
        HUF_FLAGS_PREFER_REPEAT
    } else {
        0
    }) | (if strategy >= STRAT_BTULTRA {
        HUF_FLAGS_OPTIMAL_DEPTH
    } else {
        0
    }) | (if suspect_uncompressible {
        HUF_FLAGS_SUSPECT_UNCOMPRESSIBLE
    } else {
        0
    });
    if repeat == HufRepeat::Valid && lh_size == 3 {
        single_stream = true;
    }
    let out = &mut dst[lh_size..];
    let r = if single_stream {
        huf_compress1x_repeat(
            out,
            src,
            255,
            LIT_HUF_LOG,
            &mut next.table,
            &mut repeat,
            flags,
        )
    } else {
        huf_compress4x_repeat(
            out,
            src,
            255,
            LIT_HUF_LOG,
            &mut next.table,
            &mut repeat,
            flags,
        )
    };
    if repeat != HufRepeat::None {
        h_type = 3; // set_repeat
    }
    let c_lit_size = match r {
        Ok(n) if n != 0 && n < src_size - min_gain(src_size, strategy) => n,
        // Too small a gain, not compressible, or an error: raw literals.
        _ => {
            next.clone_from(prev);
            return no_compress_literals(dst, src);
        }
    };
    if c_lit_size == 1 && (src_size >= 8 || src.iter().all(|&b| b == src[0])) {
        next.clone_from(prev);
        return rle_literals(dst, src);
    }
    if h_type == 2 {
        next.repeat = HufRepeat::Check;
    }
    let (n, c) = (src_size as u32, c_lit_size as u32);
    match lh_size {
        3 => {
            let lhc = h_type + (u32::from(!single_stream) << 2) + (n << 4) + (c << 14);
            dst[..3].copy_from_slice(&lhc.to_le_bytes()[..3]);
        }
        4 => {
            let lhc = h_type + (2 << 2) + (n << 4) + (c << 18);
            dst[..4].copy_from_slice(&lhc.to_le_bytes());
        }
        _ => {
            let lhc = h_type + (3 << 2) + (n << 4) + (c << 22);
            dst[..4].copy_from_slice(&lhc.to_le_bytes());
            dst[4] = (c >> 10) as u8;
        }
    }
    Ok(lh_size + c_lit_size)
}
