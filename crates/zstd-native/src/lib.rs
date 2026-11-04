//! Pure Rust Zstandard (zstd, RFC 8878) decompression, built to match or
//! beat C libzstd 1.5.7 in speed. No C code, `no_std` + `alloc` capable.
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

mod bits;
mod block;
mod cpu;
mod decoder;
mod dict;
mod error;
mod frame;
mod fse;
pub(crate) mod huf;
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
mod huf_asm;
mod xxhash;

pub use decoder::{Decompressor, content_size, decompress, decompress_to_slice};
pub use dict::Dictionary;
pub use error::Error;
pub use frame::FrameHeader;
pub use xxhash::xxh64;

#[doc(hidden)]
pub mod __private {
    pub use crate::cpu::{selected_paths, set_asm};
    pub use crate::huf::{
        bench_read as huf_bench_read, first_tree_description as huf_first_tree_description,
    };
}
