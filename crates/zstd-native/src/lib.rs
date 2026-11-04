//! Pure Rust Zstandard (zstd, RFC 8878) decompression and compression,
//! built to match or beat C libzstd 1.5.7 in speed. No C code, `no_std` +
//! `alloc` capable.
//!
//! # One-shot
//!
//! ```
//! // A frame holding "hello" as a raw block.
//! let compressed = [0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x05, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o'];
//! assert_eq!(zstd_native::decompress(&compressed)?, b"hello");
//! # Ok::<(), zstd_native::Error>(())
//! ```
//!
//! [`Decompressor`] keeps its tables and buffers between calls (and can
//! hold a [`Dictionary`]); [`decompress_to_slice`] writes into a buffer
//! you provide.
//!
//! # Streaming
//!
//! [`Decoder`] reads a zstd stream from any `std::io::Read` (frame after
//! frame); [`StreamDecoder`] is the push API underneath (input and output
//! in pieces of any size, no `std` needed).
//!
//! ```
//! use std::io::Read;
//! # let compressed: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x05, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o'];
//! let mut text = String::new();
//! zstd_native::Decoder::new(compressed).read_to_string(&mut text)?;
//! # assert_eq!(text, "hello");
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # Compression
//!
//! [`compress`] (and [`Compressor`], [`compress_to_slice`]) produce the
//! same bytes as libzstd 1.5.7 at every level. [`Encoder`] compresses into
//! any `std::io::Write`; [`StreamEncoder`] is the push API underneath
//! (libzstd's `ZSTD_compressStream2`, byte-identical for the same calls).
//! All of them take dictionaries too ([`compress_with_dictionary`],
//! [`Compressor::with_dictionary`], or an [`EncoderDictionary`] prepared
//! once), with libzstd's output. With `std`, [`Compressor`],
//! [`StreamEncoder`] and [`Encoder`] also compress with worker threads
//! (`set_workers`, libzstd's `ZSTD_c_nbWorkers`), with libzstd's
//! multithreaded output.
//!
//! ```
//! use std::io::Write;
//! let mut enc = zstd_native::Encoder::new(Vec::new(), 3);
//! enc.write_all(b"hello, hello, hello")?;
//! let compressed = enc.finish()?;
//! assert_eq!(zstd_native::decompress(&compressed).unwrap(), b"hello, hello, hello");
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # Speed
//!
//! The hot loops (4-stream Huffman decoding, sequence decoding and
//! execution) are compiled twice, generic and with BMI2, and the CPU picks
//! one at run time (with `std`; without it the compile-time target
//! features decide). The `portable` feature forces the generic code.
#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

/// Aligns the code that follows to a 64-byte boundary plus `$pad` bytes
/// of `nop`, for the top of a hot loop (libzstd does the same before its
/// sequence loop). The hot loops' speed swings by a few percent with where
/// the linker puts them; this pins their placement to the function instead
/// of the whole binary. `$pad` is tuned per loop so that the loop's first
/// instruction (after the compiler's own preheader tail) lands where it
/// measured fastest. The padding runs once per loop entry.
macro_rules! align_hot_loop {
    ($pad:literal) => {
        #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
        // SAFETY: an alignment directive and `nop`s; they touch no state.
        #[allow(unsafe_code, unused_unsafe)]
        unsafe {
            core::arch::asm!(
                ".p2align 6",
                concat!(".rept ", $pad),
                "nop",
                ".endr",
                options(nomem, nostack, preserves_flags)
            );
        }
    };
}

mod bits;
mod block;
mod cpu;
mod decoder;
mod dict;
mod enc;
mod error;
mod frame;
mod fse;
pub(crate) mod huf;
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
mod huf_asm;
#[cfg(feature = "std")]
mod io;
mod stream;
mod xxhash;

pub use decoder::{Decompressor, content_size, decompress, decompress_to_slice};
pub use dict::Dictionary;
pub use enc::{
    CompressProgress, Compressor, EncoderDictionary, EndOp, StreamEncoder, compress,
    compress_bound, compress_to_slice, compress_with_dictionary,
};
pub use error::Error;
pub use frame::FrameHeader;
#[cfg(feature = "std")]
pub use io::{Decoder, Encoder};
pub use stream::{Progress, StreamDecoder};
pub use xxhash::xxh64;

#[doc(hidden)]
pub mod __private {
    pub use crate::cpu::{selected_paths, set_asm};
    pub use crate::huf::{
        bench_read as huf_bench_read, first_tree_description as huf_first_tree_description,
    };
}
