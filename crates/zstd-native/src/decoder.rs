//! One-shot decompression: whole input in memory.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::block::{BLOCK_SIZE_MAX, BlockDecoder, Output, WILDCOPY_OVERLENGTH};
use crate::dict::Dictionary;
use crate::error::{Error, Result};
use crate::frame::{self, FrameHeader};
use crate::xxhash::Xxh64;

/// A reusable decompressor (libzstd's `ZSTD_DCtx`).
///
/// Keeps its tables and buffers between calls, so decompressing many
/// small inputs doesn't allocate each time, and optionally holds a
/// [`Dictionary`].
///
/// ```
/// # let compressed: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x05, 0x29, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f];
/// let mut d = zstd_native::Decompressor::new();
/// assert_eq!(d.decompress(compressed)?, b"hello");
/// # Ok::<(), zstd_native::Error>(())
/// ```
pub struct Decompressor {
    block: Box<BlockDecoder>,
    dict: Option<Dictionary>,
    /// Where the content of the dictionary of the last frame ends (an
    /// address; 0 for none). A frame whose dictionary ends elsewhere finds
    /// it cold (libzstd's `ddictIsCold`: `dctx->dictEnd != dictEnd`), see
    /// `BlockDecoder::dict_cold`.
    dict_end: usize,
}

impl core::fmt::Debug for Decompressor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decompressor")
            .field("dict", &self.dict)
            .finish_non_exhaustive()
    }
}

impl Default for Decompressor {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a frame's output goes: a growable `Vec` or a fixed slice.
enum Sink<'a> {
    Vec(&'a mut Vec<u8>),
    Slice,
}

impl Decompressor {
    /// A decompressor without a dictionary.
    pub fn new() -> Self {
        Decompressor {
            block: Box::new(BlockDecoder::new()),
            dict: None,
            dict_end: 0,
        }
    }

    /// A decompressor that uses `dict` for every frame.
    pub fn with_dictionary(dict: Dictionary) -> Self {
        Decompressor {
            block: Box::new(BlockDecoder::new()),
            dict: Some(dict),
            dict_end: 0,
        }
    }

    /// Replaces (or removes) the dictionary.
    pub fn set_dictionary(&mut self, dict: Option<Dictionary>) {
        // The same dictionary installed again keeps what was built for it.
        let same = matches!((&self.dict, &dict), (Some(a), Some(b)) if a.same(b));
        if !same {
            self.block.dict_changed();
        }
        self.dict = dict;
    }

    /// Decompresses every frame in `src` (concatenated frames and skippable
    /// frames are allowed) into a new vector.
    ///
    /// # Errors
    ///
    /// Any [`Error`] for corrupt, truncated or unsupported input.
    pub fn decompress(&mut self, src: &[u8]) -> Result<Vec<u8>> {
        let mut v = Vec::new();
        self.decompress_append(src, &mut v)?;
        Ok(v)
    }

    /// Like [`decompress`](Self::decompress) but appends to `dst`, reusing
    /// its capacity.
    ///
    /// # Errors
    ///
    /// Any [`Error`] for corrupt, truncated or unsupported input; `dst`
    /// then holds the data decoded before the error.
    pub fn decompress_append(&mut self, src: &[u8], dst: &mut Vec<u8>) -> Result<()> {
        let mut out = Output {
            ptr: dst.as_mut_ptr(),
            cap: dst.capacity(),
            pos: dst.len(),
        };
        let res = self.decode_frames(src, &mut out, &mut Sink::Vec(&mut *dst));
        // SAFETY: `out.pos` only advances over bytes the decoder wrote.
        #[allow(unsafe_code)]
        unsafe {
            dst.set_len(out.pos)
        };
        res
    }

    /// Decompresses every frame in `src` into `dst`, returning the
    /// decompressed size.
    ///
    /// # Errors
    ///
    /// [`Error::OutputTooSmall`] if `dst` can't hold the result, or any
    /// other [`Error`] for corrupt, truncated or unsupported input.
    pub fn decompress_to_slice(&mut self, src: &[u8], dst: &mut [u8]) -> Result<usize> {
        let mut out = Output {
            ptr: dst.as_mut_ptr(),
            cap: dst.len(),
            pos: 0,
        };
        self.decode_frames(src, &mut out, &mut Sink::Slice)?;
        Ok(out.pos)
    }

    fn decode_frames(&mut self, src: &[u8], out: &mut Output, sink: &mut Sink<'_>) -> Result<()> {
        let mut ip = 0;
        while src.len() - ip >= frame::STARTING_INPUT_LENGTH {
            let magic = frame::read_le32(src, ip);
            if frame::is_skippable(magic) {
                if src.len() - ip < 8 {
                    return Err(Error::Truncated);
                }
                let size = frame::read_le32(src, ip + 4) as usize;
                if size > src.len() - ip - 8 {
                    return Err(Error::Truncated);
                }
                ip += 8 + size;
                continue;
            }
            ip += self.decode_frame(&src[ip..], out, sink)?;
        }
        if ip != src.len() {
            return Err(Error::Truncated);
        }
        Ok(())
    }

    /// Decodes one frame; returns the input bytes it used.
    fn decode_frame(&mut self, src: &[u8], out: &mut Output, sink: &mut Sink<'_>) -> Result<usize> {
        if src.len() < frame::FRAME_HEADER_SIZE_MIN + frame::BLOCK_HEADER_SIZE {
            return Err(Error::Truncated);
        }
        let fh = FrameHeader::parse(src)?;
        let mut ip = fh.header_size;
        if src.len() < ip + frame::BLOCK_HEADER_SIZE {
            return Err(Error::Truncated);
        }
        let dict: &[u8] = match &self.dict {
            Some(d) => {
                if fh.dict_id != 0 && fh.dict_id != d.id() {
                    return Err(Error::DictionaryMismatch);
                }
                self.block.start_frame(d.entropy());
                d.content()
            }
            None => {
                if fh.dict_id != 0 {
                    return Err(Error::DictionaryMismatch);
                }
                self.block.start_frame(None);
                &[]
            }
        };
        let dict_ent = self.dict.as_ref().and_then(Dictionary::entropy);
        // libzstd's `ddictIsCold`: another dictionary than the last frame's
        // (one installed again stays warm, and so does a new one where the
        // last one was, just written; a frame without compressed blocks
        // uses it up too).
        let end = self
            .dict
            .as_ref()
            .map_or(0, |d| d.content().as_ptr_range().end as usize);
        self.block.dict_cold = end != 0 && end != core::mem::replace(&mut self.dict_end, end);
        let block_max = fh.block_size_max();
        let prefix = out.pos;

        if let Sink::Vec(v) = sink {
            // Room for the whole frame when its size is known (bounded by
            // what the input could possibly expand to), else per block.
            if let Some(cs) = fh.content_size {
                let bound = ((src.len() as u64 / 3) + 1).saturating_mul(BLOCK_SIZE_MAX as u64);
                let want = cs.min(bound).saturating_add(WILDCOPY_OVERLENGTH as u64);
                let want = usize::try_from(want).map_err(|_| Error::OutputTooLarge)?;
                reserve(v, out, want)?;
            }
        }

        let mut xxh = fh.has_checksum.then(Xxh64::new);
        loop {
            if src.len() - ip < frame::BLOCK_HEADER_SIZE {
                return Err(Error::Truncated);
            }
            let bh = u32::from_le_bytes([src[ip], src[ip + 1], src[ip + 2], 0]);
            ip += frame::BLOCK_HEADER_SIZE;
            let last = bh & 1 != 0;
            let btype = (bh >> 1) & 3;
            let bsize = (bh >> 3) as usize;
            let start = out.pos;
            match btype {
                0 => {
                    // Raw
                    if bsize > src.len() - ip {
                        return Err(Error::Truncated);
                    }
                    if let Sink::Vec(v) = sink {
                        reserve(v, out, bsize + WILDCOPY_OVERLENGTH)?;
                    }
                    if bsize > out.cap - out.pos {
                        return Err(Error::OutputTooSmall);
                    }
                    // SAFETY: `bsize` input bytes exist and the output has room.
                    #[allow(unsafe_code)]
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            src.as_ptr().add(ip),
                            out.ptr.add(out.pos),
                            bsize,
                        )
                    };
                    out.pos += bsize;
                    ip += bsize;
                }
                1 => {
                    // RLE: `bsize` copies of one byte.
                    if ip >= src.len() {
                        return Err(Error::Truncated);
                    }
                    if let Sink::Vec(v) = sink {
                        reserve(v, out, bsize + WILDCOPY_OVERLENGTH)?;
                    }
                    if bsize > out.cap - out.pos {
                        return Err(Error::OutputTooSmall);
                    }
                    // SAFETY: the output has room for `bsize` bytes.
                    #[allow(unsafe_code)]
                    unsafe {
                        core::ptr::write_bytes(out.ptr.add(out.pos), src[ip], bsize)
                    };
                    out.pos += bsize;
                    ip += 1;
                }
                2 => {
                    if bsize > src.len() - ip {
                        return Err(Error::Truncated);
                    }
                    if let Sink::Vec(v) = sink {
                        reserve(v, out, block_max + WILDCOPY_OVERLENGTH)?;
                    }
                    self.block.decode_block(
                        &src[ip..ip + bsize],
                        out,
                        prefix,
                        dict,
                        dict_ent,
                        block_max,
                    )?;
                    ip += bsize;
                }
                _ => return Err(Error::Corrupted),
            }
            if let Some(x) = &mut xxh {
                // SAFETY: `start..out.pos` was just written.
                #[allow(unsafe_code)]
                let produced =
                    unsafe { core::slice::from_raw_parts(out.ptr.add(start), out.pos - start) };
                x.update(produced);
            }
            if last {
                break;
            }
        }
        if let Some(cs) = fh.content_size
            && (out.pos - prefix) as u64 != cs
        {
            return Err(Error::Corrupted);
        }
        if let Some(x) = xxh {
            if src.len() - ip < 4 {
                return Err(Error::Truncated);
            }
            if frame::read_le32(src, ip) != x.digest() as u32 {
                return Err(Error::ChecksumMismatch);
            }
            ip += 4;
        }
        Ok(ip)
    }
}

/// Makes room for `extra` more bytes after `out.pos` in `v`.
pub(crate) fn reserve(v: &mut Vec<u8>, out: &mut Output, extra: usize) -> Result<()> {
    if out.cap - out.pos >= extra {
        return Ok(());
    }
    // SAFETY: `out.pos` bytes were written; record them so `reserve` keeps them.
    #[allow(unsafe_code)]
    unsafe {
        v.set_len(out.pos)
    };
    v.try_reserve(extra).map_err(|_| Error::OutputTooLarge)?;
    out.ptr = v.as_mut_ptr();
    out.cap = v.capacity();
    Ok(())
}

/// Decompresses every frame in `src` into a new vector.
///
/// ```
/// // A frame holding "hello" as a raw block.
/// let compressed = [0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x05, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o'];
/// assert_eq!(zstd_native::decompress(&compressed)?, b"hello");
/// # Ok::<(), zstd_native::Error>(())
/// ```
///
/// # Errors
///
/// Any [`Error`] for corrupt, truncated or unsupported input.
pub fn decompress(src: &[u8]) -> Result<Vec<u8>> {
    with_cached(|d| d.decompress(src))
}

/// Decompresses every frame in `src` into `dst`, returning the size.
///
/// # Errors
///
/// [`Error::OutputTooSmall`] if `dst` is too small, or any other [`Error`]
/// for corrupt, truncated or unsupported input.
pub fn decompress_to_slice(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    with_cached(|d| d.decompress_to_slice(src, dst))
}

/// Runs `f` with a per-thread cached [`Decompressor`] (with `std`), so the
/// one-call functions don't allocate and zero the decoder's ~40 KB of
/// tables on every call: that dominated small inputs.
#[cfg(feature = "std")]
fn with_cached<T>(f: impl FnOnce(&mut Decompressor) -> T) -> T {
    use core::cell::Cell;
    std::thread_local! {
        static CACHED: Cell<Option<Box<Decompressor>>> = const { Cell::new(None) };
    }
    // Take it out while in use (re-entrancy and panics just make a new one).
    let mut d = CACHED
        .with(Cell::take)
        .unwrap_or_else(|| Box::new(Decompressor::new()));
    let r = f(&mut d);
    CACHED.with(|c| c.set(Some(d)));
    r
}

#[cfg(not(feature = "std"))]
fn with_cached<T>(f: impl FnOnce(&mut Decompressor) -> T) -> T {
    f(&mut Decompressor::new())
}

/// The decompressed size stored in the header of the first frame, or
/// `None` when the header doesn't store it.
///
/// # Errors
///
/// As [`FrameHeader::parse`].
pub fn content_size(src: &[u8]) -> Result<Option<u64>> {
    Ok(FrameHeader::parse(src)?.content_size)
}
