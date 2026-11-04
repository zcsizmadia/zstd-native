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
//! - `fast`: compress/zstd_fast.c (noDict, extDict and dictMatchState
//!   variants; the window)
//! - `dfast`: compress/zstd_double_fast.c (noDict, extDict, dictMatchState)
//! - `lazy`: compress/zstd_lazy.c (greedy, lazy, lazy2, btlazy2: parser,
//!   hash chain and binary tree; noDict, extDict and dictMatchState)
//! - `row`: compress/zstd_lazy.c (the row-based match finder)
//! - `opt`: compress/zstd_opt.c (btopt, btultra, btultra2: price model,
//!   binary tree match finder, optimal parser, dictionary statistics;
//!   noDict, extDict and dictMatchState)
//! - `presplit`: compress/zstd_preSplit.c
//! - `split`: the post-block splitter from compress/zstd_compress.c
//!   (`ZSTD_compressBlock_splitBlock`)
//! - `stream`: streaming compression from compress/zstd_compress.c
//!   (`ZSTD_compressStream2`, `ZSTD_compressStream_generic` in buffered
//!   mode, `ZSTD_CCtx_init_compressStream2`); the input ring buffer makes
//!   the window non-contiguous, which the match finders' extDict variants
//!   handle
//! - `cdict`: dictionaries (`ZSTD_CDict`, `ZSTD_createCDict`,
//!   `ZSTD_compress_insertDictionary`, `ZSTD_loadCEntropy`,
//!   `ZSTD_loadDictionaryContent`, `ZSTD_resetCCtx_usingCDict`): loading,
//!   CDicts with tagged tables, attaching or copying them, the parameters
//!   with a dictionary
//! - `ldm`: long distance matching, compress/zstd_ldm.c (gear rolling
//!   hash, long match generation, `ZSTD_ldm_blockCompress`), which libzstd
//!   enables by itself for `btopt` and above with a 128 MiB window; the
//!   optimal parser takes its matches as candidates (`ZSTD_optLdm_*` in
//!   `opt`)
//! - `mt` (with `std`): multithreaded compression,
//!   compress/zstdmt_compress.c (jobs compressed by worker threads, the
//!   overlap between jobs, the serial LDM step) and how
//!   `ZSTD_compressStream2` drives it with `nbWorkers >= 1`
//! - `table`: the match finder tables, 64-byte aligned like libzstd's
//!   workspace tables (`ZSTD_cwksp`)
#![allow(dead_code)] // filled in step by step (issue #6)

pub(crate) mod bitwriter;
pub(crate) mod cdict;
pub(crate) mod compress;
pub(crate) mod dfast;
pub(crate) mod fast;
pub(crate) mod fse_enc;
pub(crate) mod hist;
pub(crate) mod huf_enc;
pub(crate) mod lazy;
pub(crate) mod ldm;
pub(crate) mod literals;
#[cfg(feature = "std")]
pub(crate) mod mt;
pub(crate) mod opt;
pub(crate) mod params;
pub(crate) mod presplit;
pub(crate) mod row;
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
pub(crate) mod search_asm;
pub(crate) mod seq_store;
pub(crate) mod sequences;
pub(crate) mod split;
pub(crate) mod stream;
pub(crate) mod table;

pub use cdict::EncoderDictionary;
pub use compress::compress_bound;
pub use stream::{CompressProgress, EndOp, StreamEncoder};

/// Compresses `src` into one zstd frame at `level` (1 to 22, negative for
/// faster; 0 means the default, 3), like libzstd's `ZSTD_compress`: the
/// output is byte-identical to libzstd 1.5.7's.
///
/// Every level and strategy is implemented, `fast` to `btultra2`, with
/// libzstd's block splitters and its long distance matcher, which it
/// enables by itself at level 22 for inputs over 64 MiB (a 128 MiB window;
/// memory use is then libzstd's too, about 700 MiB of tables).
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
/// None in practice: the output buffer is [`compress_bound`] bytes, always
/// enough.
pub fn compress(src: &[u8], level: i32) -> crate::error::Result<alloc::vec::Vec<u8>> {
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
    with_cached(|fc| compress::compress_to_slice_with(fc, src, dst, level, true))
}

/// Compresses `src` into one zstd frame at `level` with the dictionary
/// `dict` (zstd-format or raw content, see [`EncoderDictionary`]; an empty
/// one is no dictionary). The output is byte-identical to libzstd 1.5.7's
/// `ZSTD_CCtx_loadDictionary` then `ZSTD_compress2` on a new context (the
/// `zstd` crate's `bulk::Compressor::with_dictionary(level, dict)`). It
/// indexes the dictionary on every call: for many inputs, use a
/// [`Compressor`] (which indexes it once) or an [`EncoderDictionary`].
///
/// ```
/// let dict = b"the quick brown fox jumps over the lazy dog";
/// let compressed = zstd_native::compress_with_dictionary(b"the lazy dog jumps", dict, 3)?;
/// let mut d = zstd_native::Decompressor::with_dictionary(zstd_native::Dictionary::new(dict)?);
/// assert_eq!(d.decompress(&compressed)?, b"the lazy dog jumps");
/// # Ok::<(), zstd_native::Error>(())
/// ```
///
/// # Errors
///
/// [`Error::InvalidDictionary`](crate::Error::InvalidDictionary) if a
/// zstd-format dictionary is corrupt, or as [`compress`].
pub fn compress_with_dictionary(
    src: &[u8],
    dict: &[u8],
    level: i32,
) -> crate::error::Result<alloc::vec::Vec<u8>> {
    let mut slot = cdict::DictSlot::local(dict)?;
    with_cached(|fc| compress::compress_dict_with(fc, &mut slot, src, level, true))
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
/// buffers between frames, which matters for many small inputs, and
/// optionally a dictionary.
///
/// ```
/// let mut c = zstd_native::Compressor::new(1);
/// let a = c.compress(b"first message, first message")?;
/// let b = c.compress(b"second message")?;
/// assert_eq!(zstd_native::decompress(&a)?, b"first message, first message");
/// assert_eq!(zstd_native::decompress(&b)?, b"second message");
/// # Ok::<(), zstd_native::Error>(())
/// ```
///
/// With a dictionary, its frames are byte-identical to those of libzstd's
/// context with the dictionary loaded (`ZSTD_CCtx_loadDictionary`, the
/// `zstd` crate's `bulk::Compressor::with_dictionary`) or referenced
/// (`ZSTD_CCtx_refCDict`, `with_prepared_dictionary`):
///
/// ```
/// let dict = b"{\"id\": 0, \"name\": \"\", \"tags\": []}";
/// let mut c = zstd_native::Compressor::with_dictionary(3, dict)?;
/// let a = c.compress(b"{\"id\": 1, \"name\": \"one\", \"tags\": [\"a\"]}")?;
/// let mut d = zstd_native::Decompressor::with_dictionary(zstd_native::Dictionary::new(dict)?);
/// assert_eq!(d.decompress(&a)?, b"{\"id\": 1, \"name\": \"one\", \"tags\": [\"a\"]}");
/// # Ok::<(), zstd_native::Error>(())
/// ```
///
/// With `std`, it can compress with worker threads
/// ([`set_workers`](Self::set_workers)), as libzstd's `ZSTD_compress2` with
/// `ZSTD_c_nbWorkers`.
pub struct Compressor {
    level: i32,
    inner: Cached,
    dict: cdict::DictSlot,
    #[cfg(feature = "std")]
    mt_params: mt::MtParams,
    #[cfg(feature = "std")]
    mt: Option<alloc::boxed::Box<mt::MtCtx>>,
}

impl core::fmt::Debug for Compressor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_struct("Compressor");
        d.field("level", &self.level).field("dict", &self.dict);
        #[cfg(feature = "std")]
        d.field("workers", &self.mt_params.workers);
        d.finish_non_exhaustive()
    }
}

impl Compressor {
    /// A compressor for `level` (see [`compress`]).
    pub fn new(level: i32) -> Self {
        Compressor {
            level,
            inner: None,
            dict: cdict::DictSlot::None,
            #[cfg(feature = "std")]
            mt_params: mt::MtParams::default(),
            #[cfg(feature = "std")]
            mt: None,
        }
    }

    /// Compresses with `n` worker threads (libzstd's `ZSTD_c_nbWorkers`; at
    /// most 256, 64 on 32-bit targets); 0, the default, compresses in the
    /// calling thread. The output is byte-identical to libzstd's
    /// `ZSTD_compress2` with the same worker count (a different count
    /// changes it, see below).
    ///
    /// As in libzstd, inputs of at most 512 KiB are still compressed in the
    /// calling thread; larger ones are cut into jobs (by default `4 <<
    /// windowLog` bytes, at least 1 MiB: 8 MiB at level 3, 32 MiB at level
    /// 19), each compressed by a worker starting from the end of the
    /// previous one, so a frame needs several jobs to use several workers.
    /// Multithreaded output differs from single-threaded output, and
    /// compresses slightly less well.
    ///
    /// The threads are started at the first frame that uses them and kept
    /// until the compressor is dropped (or the count changes). Memory: per
    /// worker, a compressor's tables for the level; per job in flight (at
    /// most the smallest power of 2 above the worker count plus 2), its
    /// input and output.
    #[cfg(feature = "std")]
    pub fn set_workers(&mut self, n: u32) {
        self.mt_params.set_workers(n);
    }

    /// The worker thread count (see [`set_workers`](Self::set_workers)).
    #[cfg(feature = "std")]
    pub fn workers(&self) -> u32 {
        self.mt_params.workers
    }

    /// Sets the job size of multithreaded compression (libzstd's
    /// `ZSTD_c_jobSize`: 0 is automatic, other sizes are raised to 512 KiB
    /// and lowered to 1 GiB, 512 MiB on 32-bit targets). For tests.
    #[cfg(feature = "std")]
    #[doc(hidden)]
    pub fn set_job_size(&mut self, size: usize) {
        self.mt_params.set_job_size(size);
    }

    /// Sets the overlap between jobs of multithreaded compression
    /// (libzstd's `ZSTD_c_overlapLog`: 0 is automatic, else 1 (none) to 9
    /// (the whole window)). For tests.
    ///
    /// # Panics
    ///
    /// If `log` is over 9.
    #[cfg(feature = "std")]
    #[doc(hidden)]
    pub fn set_overlap_log(&mut self, log: u32) {
        self.mt_params.set_overlap_log(log);
    }

    /// `ZSTD_compress2` with workers: the multithreaded path for `src`, if
    /// it is taken; the output written into `dst` (or a new vector if
    /// `None`).
    #[cfg(feature = "std")]
    fn compress_mt(
        &mut self,
        src: &[u8],
        dst: Option<&mut [u8]>,
    ) -> Option<crate::error::Result<(alloc::vec::Vec<u8>, usize)>> {
        let pledged = Some(src.len() as u64);
        if !self.mt_params.applies(pledged) {
            return None;
        }
        Some((|| {
            let (cd, level) = self.dict.for_frame(self.level)?;
            let cp = cdict::frame_cparams(level, cd.as_deref(), pledged, 0, false);
            let ldm = params::ldm_enabled(&cp);
            let m = self
                .mt
                .get_or_insert_with(|| alloc::boxed::Box::new(mt::MtCtx::new()));
            m.init_frame(&self.mt_params, cp, ldm, cd, pledged);
            match dst {
                Some(d) => {
                    let p = m.compress(src, d, EndOp::End)?;
                    if p.remaining != 0 {
                        m.abandon();
                        return Err(crate::Error::OutputTooSmall);
                    }
                    Ok((alloc::vec::Vec::new(), p.written))
                }
                None => {
                    let mut v = alloc::vec::Vec::with_capacity(compress_bound(src.len()));
                    m.compress_end_to_vec(src, &mut v)?;
                    let n = v.len();
                    Ok((v, n))
                }
            }
        })())
    }

    /// A compressor for `level` with the dictionary `dict` (see
    /// [`set_dictionary`](Self::set_dictionary)).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidDictionary`](crate::Error::InvalidDictionary) if a
    /// zstd-format dictionary is corrupt.
    pub fn with_dictionary(level: i32, dict: &[u8]) -> crate::error::Result<Self> {
        let mut c = Self::new(level);
        c.set_dictionary(dict)?;
        Ok(c)
    }

    /// A compressor with a prepared dictionary, at its level (see
    /// [`set_prepared_dictionary`](Self::set_prepared_dictionary)).
    pub fn with_prepared_dictionary(dict: &EncoderDictionary) -> Self {
        let mut c = Self::new(dict.level());
        c.set_prepared_dictionary(dict);
        c
    }

    /// Changes the level for the next frames. With a prepared dictionary,
    /// frames take the dictionary's level instead; a dictionary given to
    /// [`set_dictionary`](Self::set_dictionary) is indexed at the level of
    /// the first frame that uses it (as in libzstd).
    pub fn set_level(&mut self, level: i32) {
        self.level = level;
    }

    /// Uses `dict` for the next frames (libzstd's
    /// `ZSTD_CCtx_loadDictionary`), replacing any dictionary; an empty
    /// `dict` removes it. Zstd-format dictionaries (starting with the
    /// magic number `0xEC30A437`) and raw content are both accepted, see
    /// [`EncoderDictionary`]. The dictionary is indexed for the level of the
    /// next frame, then kept.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidDictionary`](crate::Error::InvalidDictionary) if a
    /// zstd-format dictionary is corrupt; the compressor then has no
    /// dictionary.
    pub fn set_dictionary(&mut self, dict: &[u8]) -> crate::error::Result<()> {
        self.dict = cdict::DictSlot::None;
        self.dict = cdict::DictSlot::local(dict)?;
        Ok(())
    }

    /// Uses the prepared `dict` for the next frames, at its level (libzstd's
    /// `ZSTD_CCtx_refCDict`), replacing any dictionary.
    pub fn set_prepared_dictionary(&mut self, dict: &EncoderDictionary) {
        self.dict = cdict::DictSlot::Prepared(dict.inner.clone());
    }

    /// Removes the dictionary.
    pub fn clear_dictionary(&mut self) {
        self.dict = cdict::DictSlot::None;
    }

    /// Compresses `src` as one frame (see [`compress`]).
    ///
    /// # Errors
    ///
    /// As [`compress`].
    pub fn compress(&mut self, src: &[u8]) -> crate::error::Result<alloc::vec::Vec<u8>> {
        #[cfg(feature = "std")]
        if let Some(r) = self.compress_mt(src, None) {
            return r.map(|(v, _)| v);
        }
        if !matches!(self.dict, cdict::DictSlot::None) {
            return compress::compress_dict_with(
                &mut self.inner,
                &mut self.dict,
                src,
                self.level,
                false,
            );
        }
        compress::compress_with(&mut self.inner, src, self.level, false)
    }

    /// Compresses `src` as one frame into `dst` (see [`compress_to_slice`]).
    ///
    /// # Errors
    ///
    /// As [`compress_to_slice`].
    pub fn compress_to_slice(&mut self, src: &[u8], dst: &mut [u8]) -> crate::error::Result<usize> {
        #[cfg(feature = "std")]
        if let Some(r) = self.compress_mt(src, Some(&mut *dst)) {
            return r.map(|(_, n)| n);
        }
        if !matches!(self.dict, cdict::DictSlot::None) {
            return compress::compress_dict_to_slice_with(
                &mut self.inner,
                &mut self.dict,
                src,
                dst,
                self.level,
                false,
            );
        }
        compress::compress_to_slice_with(&mut self.inner, src, dst, self.level, false)
    }
}
