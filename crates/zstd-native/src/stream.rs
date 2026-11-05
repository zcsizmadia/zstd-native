//! Streaming decompression (libzstd's `ZSTD_decompressStream`): a push
//! decoder that takes input and fills output in pieces of any size.
//!
//! Blocks are decoded into a window buffer of `window + 2 * block + 64`
//! bytes (libzstd's sizing) and copied out. When the next block wouldn't
//! fit, decoding wraps to the start of the buffer and the old segment's
//! last `window - position` bytes serve as history (the block decoder's
//! dictionary slot). The sizing keeps that history clear of everything the
//! next blocks write, wildcopy overrun included. Each frame starts at the
//! beginning of the buffer, after all earlier output was taken.
//!
//! Matches must stay within the window (libzstd's streaming decoder also
//! allows a little more after a wrap; valid encoders never produce that).

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::block::{BlockDecoder, Output, WILDCOPY_OVERLENGTH};
use crate::dict::Dictionary;
use crate::error::{Error, Result};
use crate::frame::{self, FrameHeader};
use crate::xxhash::Xxh64;

/// libzstd's default window limit for streaming (`ZSTD_WINDOWLOG_LIMIT_DEFAULT`).
const DEFAULT_WINDOW_LOG_MAX: u32 = 27;

/// What one [`StreamDecoder::decode`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Input bytes consumed.
    pub read: usize,
    /// Output bytes written.
    pub written: usize,
    /// The decoder is between frames with no output pending: the input so
    /// far is complete (more frames may follow).
    pub at_frame_boundary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Collecting the first 5 bytes of a frame (magic + descriptor).
    Magic,
    /// Collecting the rest of the frame header (`hdr_need` bytes in all).
    Header,
    /// Collecting the 4-byte size of a skippable frame.
    SkippableSize,
    /// Skipping skippable frame content.
    Skip(u64),
    BlockHeader,
    /// Collecting/decoding a block.
    Block {
        btype: u8,
        size: usize,
        last: bool,
    },
    Checksum,
}

/// A streaming decoder: feed it input in any pieces, get output in any
/// pieces. Handles concatenated and skippable frames. No `std` needed;
/// [`Decoder`](crate::Decoder) wraps it as a `std::io::Read`.
///
/// ```
/// # let compressed: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x05, 0x29, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f];
/// let mut d = zstd_native::StreamDecoder::new();
/// let mut out = [0u8; 2];
/// let mut got = Vec::new();
/// let mut input = compressed;
/// loop {
///     let p = d.decode(input, &mut out)?;
///     input = &input[p.read..];
///     got.extend_from_slice(&out[..p.written]);
///     if input.is_empty() && p.at_frame_boundary {
///         break;
///     }
/// }
/// assert_eq!(got, b"hello");
/// # Ok::<(), zstd_native::Error>(())
/// ```
pub struct StreamDecoder {
    block: Box<BlockDecoder>,
    dict: Option<Dictionary>,
    window_max: u64,
    stage: Stage,
    /// Frame header / small field staging.
    hdr: [u8; 18],
    hdr_len: usize,
    hdr_need: usize,
    /// Block staging when a block isn't wholly in the caller's input.
    in_buf: Vec<u8>,
    /// Window buffer (`cap` bytes; only written parts are ever read).
    win: Vec<u8>,
    win_size: usize,
    fh: Option<FrameHeader>,
    block_max: usize,
    window: usize,
    /// Write position, start of the current segment, and output not yet
    /// copied out (`flush..cur`).
    cur: usize,
    prefix: usize,
    flush: usize,
    /// End of the previous segment after a wrap (its history).
    old_end: Option<usize>,
    produced: u64,
    xxh: Option<Xxh64>,
}

impl core::fmt::Debug for StreamDecoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamDecoder")
            .field("stage", &self.stage)
            .field("window_max", &self.window_max)
            .finish_non_exhaustive()
    }
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    /// A decoder without a dictionary and a 128 MiB (2^27) window limit.
    pub fn new() -> Self {
        StreamDecoder {
            block: Box::new(BlockDecoder::new()),
            dict: None,
            window_max: 1 << DEFAULT_WINDOW_LOG_MAX,
            stage: Stage::Magic,
            hdr: [0; 18],
            hdr_len: 0,
            hdr_need: frame::STARTING_INPUT_LENGTH,
            in_buf: Vec::new(),
            win: Vec::new(),
            win_size: 0,
            fh: None,
            block_max: 0,
            window: 0,
            cur: 0,
            prefix: 0,
            flush: 0,
            old_end: None,
            produced: 0,
            xxh: None,
        }
    }

    /// A decoder that uses `dict` for every frame.
    pub fn with_dictionary(dict: Dictionary) -> Self {
        let mut d = Self::new();
        d.dict = Some(dict);
        d
    }

    /// Sets the largest window accepted (frames needing more fail with
    /// [`Error::WindowTooLarge`]), as `1 << log`, `log` 10 to 31 (30 on
    /// 32-bit targets, as libzstd); values outside are clamped. Frames
    /// with a window log over 31 (30 on 32-bit) are rejected in every
    /// mode, one-shot decoding included.
    /// The default is 27 (128 MiB), libzstd's.
    pub fn set_window_log_max(&mut self, log: u32) {
        self.window_max = 1u64 << log.clamp(10, frame::WINDOWLOG_MAX);
    }

    /// Forgets any partial frame; the next input must start a frame.
    pub fn reset(&mut self) {
        self.stage = Stage::Magic;
        self.hdr_len = 0;
        self.hdr_need = frame::STARTING_INPUT_LENGTH;
        self.in_buf.clear();
        self.cur = 0;
        self.flush = 0;
        self.fh = None;
    }

    /// True between frames with no output pending.
    pub fn at_frame_boundary(&self) -> bool {
        self.stage == Stage::Magic && self.hdr_len == 0 && self.flush == self.cur
    }

    /// Consumes input and writes output until the input is used up or the
    /// output is full.
    ///
    /// # Errors
    ///
    /// Any [`Error`] for corrupt or unsupported input; the decoder must
    /// then be [`reset`](Self::reset) before reuse.
    pub fn decode(&mut self, input: &[u8], output: &mut [u8]) -> Result<Progress> {
        let mut ip = 0;
        let mut op = 0;
        loop {
            // Pending output first.
            if self.flush < self.cur {
                let n = (self.cur - self.flush).min(output.len() - op);
                // SAFETY: `flush..cur` of the window was written.
                #[allow(unsafe_code)]
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        self.win.as_ptr().add(self.flush),
                        output.as_mut_ptr().add(op),
                        n,
                    )
                };
                self.flush += n;
                op += n;
                if self.flush < self.cur {
                    break; // output full
                }
            }
            let rest = &input[ip..];
            match self.stage {
                Stage::Magic | Stage::Header | Stage::SkippableSize | Stage::Checksum => {
                    let take = (self.hdr_need - self.hdr_len).min(rest.len());
                    self.hdr[self.hdr_len..self.hdr_len + take].copy_from_slice(&rest[..take]);
                    self.hdr_len += take;
                    ip += take;
                    if self.hdr_len < self.hdr_need {
                        break; // need input
                    }
                    self.small_field_done()?;
                }
                Stage::Skip(left) => {
                    let n = (left.min(rest.len() as u64)) as usize;
                    ip += n;
                    let left = left - n as u64;
                    if left > 0 {
                        self.stage = Stage::Skip(left);
                        break;
                    }
                    self.start_magic();
                }
                Stage::BlockHeader => {
                    if self.in_buf.is_empty() && rest.len() >= 3 {
                        ip += 3;
                        self.block_header([rest[0], rest[1], rest[2]])?;
                    } else {
                        let take = (3 - self.in_buf.len()).min(rest.len());
                        self.in_buf.extend_from_slice(&rest[..take]);
                        ip += take;
                        if self.in_buf.len() < 3 {
                            break;
                        }
                        let h = [self.in_buf[0], self.in_buf[1], self.in_buf[2]];
                        self.in_buf.clear();
                        self.block_header(h)?;
                    }
                }
                Stage::Block { btype, size, last } => {
                    let need = if btype == 1 { 1 } else { size };
                    if self.in_buf.is_empty() && rest.len() >= need {
                        // The whole block is in the input: decode in place.
                        ip += need;
                        self.decode_block(btype, size, &input[ip - need..ip])?;
                    } else {
                        let take = (need - self.in_buf.len()).min(rest.len());
                        self.in_buf.extend_from_slice(&rest[..take]);
                        ip += take;
                        if self.in_buf.len() < need {
                            break;
                        }
                        let buf = core::mem::take(&mut self.in_buf);
                        let r = self.decode_block(btype, size, &buf);
                        self.in_buf = buf;
                        self.in_buf.clear();
                        r?;
                    }
                    self.stage = if !last {
                        Stage::BlockHeader
                    } else {
                        self.end_of_blocks()?
                    };
                }
            }
        }
        Ok(Progress {
            read: ip,
            written: op,
            at_frame_boundary: self.at_frame_boundary(),
        })
    }

    fn start_magic(&mut self) {
        self.stage = Stage::Magic;
        self.hdr_len = 0;
        self.hdr_need = frame::STARTING_INPUT_LENGTH;
    }

    /// A staged header field is complete.
    fn small_field_done(&mut self) -> Result<()> {
        match self.stage {
            Stage::Magic => {
                let magic = frame::read_le32(&self.hdr, 0);
                if frame::is_skippable(magic) {
                    self.stage = Stage::SkippableSize;
                    self.hdr_need = 8;
                } else if magic == frame::MAGIC {
                    self.stage = Stage::Header;
                    self.hdr_need = frame::header_size(self.hdr[4]);
                } else {
                    return Err(Error::UnknownFrame);
                }
                // The 5th byte may already be complete for a skippable frame.
                if self.hdr_len >= self.hdr_need {
                    return self.small_field_done();
                }
                Ok(())
            }
            Stage::SkippableSize => {
                let size = frame::read_le32(&self.hdr, 4);
                self.stage = Stage::Skip(u64::from(size));
                if size == 0 {
                    self.start_magic();
                }
                Ok(())
            }
            Stage::Header => self.begin_frame(),
            Stage::Checksum => {
                let want = frame::read_le32(&self.hdr, 0);
                let got = self.xxh.take().map_or(0, |x| x.digest() as u32);
                if want != got {
                    return Err(Error::ChecksumMismatch);
                }
                self.start_magic();
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    fn begin_frame(&mut self) -> Result<()> {
        let fh = FrameHeader::parse(&self.hdr[..self.hdr_need])?;
        if fh.window_size > self.window_max {
            return Err(Error::WindowTooLarge);
        }
        let dict_id = self.dict.as_ref().map_or(0, |d| d.id);
        if fh.dict_id != 0 && fh.dict_id != dict_id {
            return Err(Error::DictionaryMismatch);
        }
        match self.dict.as_ref().and_then(|d| d.entropy.as_deref()) {
            Some(e) => self.block.ent.clone_from(e),
            None => self.block.ent.reset(),
        }
        self.block_max = fh.block_size_max();
        // The window never needs to exceed the frame's size.
        let window = match fh.content_size {
            Some(cs) => fh.window_size.min(cs),
            None => fh.window_size,
        } as usize;
        self.window = window;
        let size = window + 2 * self.block_max + 2 * WILDCOPY_OVERLENGTH;
        if self.win.capacity() < size {
            self.win = Vec::new();
            self.win
                .try_reserve_exact(size)
                .map_err(|_| Error::OutputTooLarge)?;
        }
        self.win_size = size;
        self.cur = 0;
        self.prefix = 0;
        self.flush = 0;
        self.old_end = None;
        self.produced = 0;
        self.xxh = fh.has_checksum.then(Xxh64::new);
        self.fh = Some(fh);
        self.hdr_len = 0;
        self.stage = Stage::BlockHeader;
        Ok(())
    }

    fn block_header(&mut self, h: [u8; 3]) -> Result<()> {
        let bh = u32::from_le_bytes([h[0], h[1], h[2], 0]);
        let last = bh & 1 != 0;
        let btype = ((bh >> 1) & 3) as u8;
        let size = (bh >> 3) as usize;
        // libzstd's streaming stages check both the compressed size (1 for
        // an RLE block) and the regenerated size against `blockSizeMax`
        // (`ZSTD_decompressContinue`). They differ only for an RLE block in
        // a window of 0 (a single-segment frame of content size 0), which
        // libzstd rejects even when it regenerates 0 bytes.
        let csize = if btype == 1 { 1 } else { size };
        if btype == 3 || size.max(csize) > self.block_max {
            return Err(Error::Corrupted);
        }
        self.stage = Stage::Block { btype, size, last };
        Ok(())
    }

    /// Decodes one block into the window at `cur` (everything before was
    /// flushed). `src` is the block's content (1 byte for RLE).
    fn decode_block(&mut self, btype: u8, size: usize, src: &[u8]) -> Result<()> {
        debug_assert_eq!(self.flush, self.cur);
        // Wrap when the block (plus wildcopy overrun) wouldn't fit.
        if self.cur + self.block_max + WILDCOPY_OVERLENGTH > self.win_size {
            self.old_end = Some(self.cur);
            self.cur = 0;
            self.prefix = 0;
            self.flush = 0;
        }
        let start = self.cur;
        let base = self.win.as_mut_ptr();
        // History before the segment: the dictionary until the first wrap,
        // then the part of the old segment still within the window.
        let (dict, cap): (&[u8], usize) = match self.old_end {
            None => (
                self.dict.as_ref().map_or(&[][..], |d| &d.content[..]),
                self.win_size,
            ),
            Some(old_end) => {
                let need = self.window.saturating_sub(start);
                let from = old_end - need.min(old_end);
                // SAFETY: `from..old_end` was written by earlier blocks. The
                // buffer sizing (window + 2 blocks + 64) keeps it after
                // `start + block_max + WILDCOPY_OVERLENGTH`, so the writes of
                // this block (bounded by `cap = from`) never touch it.
                debug_assert!(
                    from == old_end || start + self.block_max + WILDCOPY_OVERLENGTH <= from
                );
                #[allow(unsafe_code)]
                let d = unsafe { core::slice::from_raw_parts(base.add(from), old_end - from) };
                (d, if from == old_end { self.win_size } else { from })
            }
        };
        let mut out = Output {
            ptr: base,
            cap,
            pos: start,
        };
        match btype {
            0 => {
                // SAFETY: `size <= block_max` and the block fits before `cap`.
                #[allow(unsafe_code)]
                unsafe {
                    core::ptr::copy_nonoverlapping(src.as_ptr(), base.add(start), size)
                };
                out.pos += size;
            }
            1 => {
                // SAFETY: as above.
                #[allow(unsafe_code)]
                unsafe {
                    core::ptr::write_bytes(base.add(start), src[0], size)
                };
                out.pos += size;
            }
            _ => {
                self.block
                    .decode_block(src, &mut out, self.prefix, dict, self.block_max)?;
                // `decode_block` bounds a block by `block_max`.
                debug_assert!(out.pos - start <= self.block_max);
            }
        }
        self.cur = out.pos;
        self.produced += (self.cur - start) as u64;
        if let Some(x) = &mut self.xxh {
            // SAFETY: just written.
            #[allow(unsafe_code)]
            x.update(unsafe { core::slice::from_raw_parts(base.add(start), self.cur - start) });
        }
        Ok(())
    }

    fn end_of_blocks(&mut self) -> Result<Stage> {
        let fh = self.fh.take().ok_or(Error::Corrupted)?;
        if let Some(cs) = fh.content_size
            && cs != self.produced
        {
            return Err(Error::Corrupted);
        }
        self.hdr_len = 0;
        if fh.has_checksum {
            self.hdr_need = 4;
            Ok(Stage::Checksum)
        } else {
            self.hdr_need = frame::STARTING_INPUT_LENGTH;
            Ok(Stage::Magic)
        }
    }
}
