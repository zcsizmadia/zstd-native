//! Compression: a port of libzstd 1.5.7's compressor whose output is
//! byte-identical to libzstd's for the same input and parameters (issue #6).
//!
//! Layout (each module ports the named libzstd file; function names in
//! comments refer to libzstd):
//! - `bitwriter`: `BIT_CStream_t` (common/bitstream.h)
//! - `hist`: compress/hist.c
//! - `fse_enc`: compress/fse_compress.c (+ FSE encoder parts of common/fse.h)
//! - `huf_enc`: compress/huf_compress.c
//! - `seq_store`: `SeqStore_t`, `ZSTD_storeSeq`, `ZSTD_count`, hashing
//!   (compress/zstd_compress_internal.h)
//! - `params`: compression levels and parameter adjustment (clevels.h,
//!   `ZSTD_getCParams_internal`, `ZSTD_adjustCParams_internal`)
//! - `fast`: compress/zstd_fast.c
//! - `presplit`: compress/zstd_preSplit.c
#![allow(dead_code)] // filled in step by step (issue #6)

pub(crate) mod bitwriter;
pub(crate) mod fast;
pub(crate) mod fse_enc;
pub(crate) mod hist;
pub(crate) mod huf_enc;
pub(crate) mod params;
pub(crate) mod presplit;
pub(crate) mod seq_store;
