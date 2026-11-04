//! Compression: a port of libzstd 1.5.7's compressor whose output is
//! byte-identical to libzstd's for the same input and parameters (issue #6).
//!
//! Layout (each module ports the named libzstd file; function names in
//! comments refer to libzstd):
//! - `bitwriter`: `BIT_CStream_t` (common/bitstream.h)
//! - `hist`: compress/hist.c
//! - `fse_enc`: compress/fse_compress.c (+ FSE encoder parts of common/fse.h)
//! - `huf_enc`: compress/huf_compress.c
//! - `literals`: compress/zstd_compress_literals.c
//! - `sequences`: compress/zstd_compress_sequences.c
//! - `compress`: blocks and frames from compress/zstd_compress.c
//! - `seq_store`: `SeqStore_t`, `ZSTD_storeSeq`, `ZSTD_count`, hashing
//!   (compress/zstd_compress_internal.h)
//! - `params`: compression levels and parameter adjustment (clevels.h,
//!   `ZSTD_getCParams_internal`, `ZSTD_adjustCParams_internal`)
//! - `fast`: compress/zstd_fast.c
//! - `presplit`: compress/zstd_preSplit.c
#![allow(dead_code)] // filled in step by step (issue #6)

pub(crate) mod bitwriter;
pub(crate) mod compress;
pub(crate) mod fast;
pub(crate) mod fse_enc;
pub(crate) mod hist;
pub(crate) mod huf_enc;
pub(crate) mod literals;
pub(crate) mod params;
pub(crate) mod presplit;
pub(crate) mod seq_store;
pub(crate) mod sequences;

pub use compress::compress_bound;

/// Compresses `src` into one zstd frame at `level` (1 to 22, negative for
/// faster; 0 means the default, 3), like libzstd's `ZSTD_compress`: the
/// output is byte-identical to libzstd 1.5.7's.
///
/// Work in progress (issue #6): only the fast strategy (levels 1, 2 and
/// negative) is implemented so far; other levels return
/// [`Error::UnsupportedFrameParameter`](crate::Error::UnsupportedFrameParameter).
///
/// ```
/// let data = b"hello hello hello hello hello hello";
/// let compressed = zstd_native::compress(data, 1)?;
/// assert_eq!(zstd_native::decompress(&compressed)?, data);
/// # Ok::<(), zstd_native::Error>(())
/// ```
///
/// # Errors
///
/// [`Error::UnsupportedFrameParameter`](crate::Error::UnsupportedFrameParameter)
/// for levels not implemented yet.
pub fn compress(src: &[u8], level: i32) -> crate::error::Result<alloc::vec::Vec<u8>> {
    check_level(level, src.len())?;
    compress::compress_impl(src, level)
}

/// Compresses into `dst`, returning the compressed size; `dst` of
/// [`compress_bound`]`(src.len())` bytes is always enough.
///
/// # Errors
///
/// [`Error::OutputTooSmall`](crate::Error::OutputTooSmall) if `dst` is too
/// small, or as [`compress`].
pub fn compress_to_slice(src: &[u8], dst: &mut [u8], level: i32) -> crate::error::Result<usize> {
    check_level(level, src.len())?;
    compress::compress_to_slice_impl(src, dst, level)
}

fn check_level(level: i32, src_size: usize) -> crate::error::Result<()> {
    let cp = params::one_shot_cparams(level, src_size as u64);
    if cp.strategy != params::Strategy::Fast {
        return Err(crate::error::Error::UnsupportedFrameParameter);
    }
    Ok(())
}
