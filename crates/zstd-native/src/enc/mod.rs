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
//! - `dfast`: compress/zstd_double_fast.c
//! - `lazy`: compress/zstd_lazy.c (greedy, lazy, lazy2, btlazy2: parser,
//!   hash chain and binary tree)
//! - `row`: compress/zstd_lazy.c (the row-based match finder)
//! - `presplit`: compress/zstd_preSplit.c
#![allow(dead_code)] // filled in step by step (issue #6)

pub(crate) mod bitwriter;
pub(crate) mod compress;
pub(crate) mod dfast;
pub(crate) mod fast;
pub(crate) mod fse_enc;
pub(crate) mod hist;
pub(crate) mod huf_enc;
pub(crate) mod lazy;
pub(crate) mod literals;
pub(crate) mod params;
pub(crate) mod presplit;
pub(crate) mod row;
pub(crate) mod seq_store;
pub(crate) mod sequences;

pub use compress::compress_bound;

/// Compresses `src` into one zstd frame at `level` (1 to 22, negative for
/// faster; 0 means the default, 3), like libzstd's `ZSTD_compress`: the
/// output is byte-identical to libzstd 1.5.7's.
///
/// Work in progress (issue #6): the strategies up to `btlazy2` are
/// implemented (levels up to 15 for inputs over 256 KB, 12 for 16-256 KB,
/// 10 up to 16 KB); higher levels (`btopt` and above) return
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
    with_cached(|fc| compress::compress_with(fc, src, level, true))
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
    with_cached(|fc| compress::compress_to_slice_with(fc, src, dst, level, true))
}

type Cached = Option<alloc::boxed::Box<compress::FrameCompressor>>;

/// Runs `f` with a per-thread cached compressor (with `std`): reusing its
/// tables and buffers matters for small inputs, as in libzstd's CCtx.
#[cfg(feature = "std")]
fn with_cached<T>(f: impl FnOnce(&mut Cached) -> T) -> T {
    use core::cell::Cell;
    std::thread_local! {
        static CACHED: Cell<Cached> = const { Cell::new(None) };
    }
    let mut c = CACHED.with(Cell::take);
    let r = f(&mut c);
    CACHED.with(|cell| cell.set(c));
    r
}

#[cfg(not(feature = "std"))]
fn with_cached<T>(f: impl FnOnce(&mut Cached) -> T) -> T {
    f(&mut None)
}

/// A reusable compressor (libzstd's `ZSTD_CCtx`): keeps its tables and
/// buffers between frames, which matters for many small inputs.
///
/// ```
/// let mut c = zstd_native::Compressor::new(1);
/// let a = c.compress(b"first message, first message")?;
/// let b = c.compress(b"second message")?;
/// assert_eq!(zstd_native::decompress(&a)?, b"first message, first message");
/// assert_eq!(zstd_native::decompress(&b)?, b"second message");
/// # Ok::<(), zstd_native::Error>(())
/// ```
pub struct Compressor {
    level: i32,
    inner: Cached,
}

impl core::fmt::Debug for Compressor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Compressor")
            .field("level", &self.level)
            .finish_non_exhaustive()
    }
}

impl Compressor {
    /// A compressor for `level` (see [`compress`]).
    pub fn new(level: i32) -> Self {
        Compressor { level, inner: None }
    }

    /// Changes the level for the next frames.
    pub fn set_level(&mut self, level: i32) {
        self.level = level;
    }

    /// Compresses `src` as one frame (see [`compress`]).
    ///
    /// # Errors
    ///
    /// As [`compress`].
    pub fn compress(&mut self, src: &[u8]) -> crate::error::Result<alloc::vec::Vec<u8>> {
        check_level(self.level, src.len())?;
        compress::compress_with(&mut self.inner, src, self.level, false)
    }

    /// Compresses `src` as one frame into `dst` (see [`compress_to_slice`]).
    ///
    /// # Errors
    ///
    /// As [`compress_to_slice`].
    pub fn compress_to_slice(&mut self, src: &[u8], dst: &mut [u8]) -> crate::error::Result<usize> {
        check_level(self.level, src.len())?;
        compress::compress_to_slice_with(&mut self.inner, src, dst, self.level, false)
    }
}

fn check_level(level: i32, src_size: usize) -> crate::error::Result<()> {
    let cp = params::one_shot_cparams(level, src_size as u64);
    if cp.strategy > params::Strategy::BtLazy2 {
        return Err(crate::error::Error::UnsupportedFrameParameter);
    }
    Ok(())
}
