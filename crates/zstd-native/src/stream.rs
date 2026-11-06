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
//!
//! # Direct output
//!
//! When a frame's first block arrives and the caller's output has room for
//! it, the frame is decoded straight into the caller's output instead
//! ("direct"), like libzstd's single-pass shortcut in
//! `ZSTD_decompressStream`, but block by block, so the whole frame needn't
//! be in the input. The frame's earlier output then lies right before the
//! write position in the caller's buffer and serves as history. The window
//! buffer's layout (`cur`, `old_end`) is still tracked, as integers only,
//! and each block sees exactly the history it would see in the window
//! buffer: the same bytes come out and the same input is rejected. When a
//! block doesn't fit, or the call returns mid-frame, the history the next
//! blocks can reach is copied into the window buffer at the positions the
//! layout gives it ([`StreamDecoder::leave_direct`]), and decoding goes on
//! buffered. That copy is at most the output this call decoded directly,
//! which the buffered path would have copied out anyway. A frame decoded
//! wholly in one call never touches (or allocates) the window buffer.
//!
//! [`Decoder`](crate::Decoder)'s `read_to_end` decodes into the `Vec`
//! (growing it) for the whole call, so frames there are always direct.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::block::{BlockDecoder, Output, WILDCOPY_OVERLENGTH};
use crate::decoder::reserve;
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
/// Output is decoded straight into the buffer passed to
/// [`decode`](Self::decode), without going through the decoder's window
/// buffer, from the start of a frame for as long as the buffer has room
/// for the next block (like libzstd's single-pass shortcut in
/// `ZSTD_decompressStream`, but needing neither the whole frame in the
/// input nor its size in the header). An output buffer that holds a whole
/// frame, given at its start, gets it with no copy and no window buffer.
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
    /// The dictionary was installed and no frame has used it yet.
    dict_fresh: bool,
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
    /// The current frame is decoded into the caller's output: its
    /// `produced` bytes so far end at the output's write position, and the
    /// window buffer holds nothing (`cur`, `old_end` give its layout).
    direct: bool,
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
            dict_fresh: false,
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
            direct: false,
            produced: 0,
            xxh: None,
        }
    }

    /// A decoder that uses `dict` for every frame.
    pub fn with_dictionary(dict: Dictionary) -> Self {
        let mut d = Self::new();
        d.dict = Some(dict);
        d.dict_fresh = true;
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
        self.direct = false;
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
        let mut out = Output {
            ptr: output.as_mut_ptr(),
            cap: output.len(),
            pos: 0,
        };
        let read = self
            .run(input, &mut out, None)
            .inspect_err(|_| self.drop_direct())?;
        // The caller's buffer is gone after this call.
        self.leave_direct(&output[..out.pos])?;
        Ok(Progress {
            read,
            written: out.pos,
            at_frame_boundary: self.at_frame_boundary(),
        })
    }

    /// Like [`decode`](Self::decode) but appends to `v`, growing it, until
    /// the input is used up; returns the input bytes consumed. A frame
    /// that starts here stays direct across calls: its history is `v`'s
    /// tail, so `v` must hold everything earlier calls appended, and
    /// [`leave_direct`](Self::leave_direct) must be called with it before
    /// any other kind of call.
    #[cfg(feature = "std")]
    pub(crate) fn decode_vec(&mut self, input: &[u8], v: &mut Vec<u8>) -> Result<usize> {
        let mut out = Output {
            ptr: v.as_mut_ptr(),
            cap: v.capacity(),
            pos: v.len(),
        };
        let r = self.run(input, &mut out, Some(&mut *v));
        // SAFETY: `out.pos` only advances over bytes the decoder wrote
        // (`run` keeps `out` in step with `v`'s allocation).
        #[allow(unsafe_code)]
        unsafe {
            v.set_len(out.pos)
        };
        r.inspect_err(|_| self.drop_direct())
    }

    /// After an error in a direct frame: the window buffer doesn't hold
    /// the history its layout claims, so forget the frame (the caller
    /// must [`reset`](Self::reset) anyway, but a call without one must
    /// not read the window buffer where nothing was written).
    fn drop_direct(&mut self) {
        if self.direct {
            self.reset();
        }
    }

    /// The decoding loop: into `out`, grown from `vec` when given (`out`
    /// then describes `vec`'s allocation). Returns the input bytes used.
    fn run(
        &mut self,
        input: &[u8],
        out: &mut Output,
        mut vec: Option<&mut Vec<u8>>,
    ) -> Result<usize> {
        let mut ip = 0;
        loop {
            // Pending output first.
            if self.flush < self.cur {
                debug_assert!(!self.direct);
                if let Some(v) = vec.as_deref_mut() {
                    reserve(v, out, self.cur - self.flush)?;
                }
                let n = (self.cur - self.flush).min(out.cap - out.pos);
                // SAFETY: `flush..cur` of the window was written and the
                // output has room for `n` bytes.
                #[allow(unsafe_code)]
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        self.win.as_ptr().add(self.flush),
                        out.ptr.add(out.pos),
                        n,
                    )
                };
                self.flush += n;
                out.pos += n;
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
                        let src = &input[ip - need..ip];
                        self.decode_block(btype, size, src, out, vec.as_deref_mut())?;
                    } else {
                        let take = (need - self.in_buf.len()).min(rest.len());
                        self.in_buf.extend_from_slice(&rest[..take]);
                        ip += take;
                        if self.in_buf.len() < need {
                            break;
                        }
                        let buf = core::mem::take(&mut self.in_buf);
                        let r = self.decode_block(btype, size, &buf, out, vec.as_deref_mut());
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
        Ok(ip)
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
        // libzstd's `ddictIsCold`: the first frame after the dictionary was
        // installed, or after a frame that wrapped the window buffer (libzstd
        // then compares the history end it moved with the dictionary's end).
        let fresh = core::mem::take(&mut self.dict_fresh);
        self.block.dict_cold = self.dict.is_some() && (fresh || self.old_end.is_some());
        // The window never needs to exceed the frame's size.
        let window = match fh.content_size {
            Some(cs) => fh.window_size.min(cs),
            None => fh.window_size,
        } as usize;
        self.window = window;
        // Allocated by the first buffered block (`ensure_window`): a frame
        // decoded directly never needs it.
        self.win_size = window + 2 * self.block_max + 2 * WILDCOPY_OVERLENGTH;
        self.cur = 0;
        self.prefix = 0;
        self.flush = 0;
        self.old_end = None;
        self.direct = false;
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

    /// Decodes one block (everything before was flushed): into the
    /// caller's output when the frame is direct (or can become direct: at
    /// its start, with room for the block), else into the window at `cur`.
    /// `src` is the block's content (1 byte for RLE). `out` and `vec` are
    /// as in [`run`](Self::run).
    fn decode_block(
        &mut self,
        btype: u8,
        size: usize,
        src: &[u8],
        out: &mut Output,
        vec: Option<&mut Vec<u8>>,
    ) -> Result<()> {
        debug_assert_eq!(self.flush, self.cur);
        if self.direct || self.produced == 0 {
            // The most this block can write: its size for a raw or RLE
            // block; `block_max` for a compressed one, or what's left of the
            // content size (more fails the frame anyway).
            let cs = self.fh.as_ref().and_then(|f| f.content_size);
            let need = if btype < 2 {
                size
            } else {
                let left = cs.map_or(u64::MAX, |cs| cs.saturating_sub(self.produced));
                left.min(self.block_max as u64) as usize
            };
            if let Some(v) = vec {
                if !self.direct
                    && let Some(cs) = cs
                {
                    // Frame start: room for the whole frame, within the
                    // window limit (no more than the window buffer that
                    // isn't needed now).
                    let want = cs.min(self.window_max) as usize;
                    reserve(v, out, want + WILDCOPY_OVERLENGTH)?;
                }
                reserve(v, out, need + WILDCOPY_OVERLENGTH)?;
            }
            if out.cap - out.pos >= need {
                self.direct = true;
            } else if self.direct {
                // SAFETY: `out.ptr[..out.pos]` is the caller's output so far
                // (initialized: written by the caller or this decoder).
                #[allow(unsafe_code)]
                let done = unsafe { core::slice::from_raw_parts(out.ptr, out.pos) };
                self.leave_direct(done)?;
            }
        }
        // Wrap when the block (plus wildcopy overrun) wouldn't fit. Tracked
        // in direct mode too: the layout decides the history a block sees.
        if self.cur + self.block_max + WILDCOPY_OVERLENGTH > self.win_size {
            self.old_end = Some(self.cur);
            self.cur = 0;
            self.prefix = 0;
            self.flush = 0;
        }
        let start = self.cur;
        // History before the segment: the dictionary until the first wrap,
        // then the old segment's last `hist` bytes (still within the window).
        let old = self
            .old_end
            .map(|old_end| (old_end, self.window.saturating_sub(start).min(old_end)));
        let written = if self.direct {
            // The segment is the output's last `start` bytes, the old
            // segment's history right before it.
            debug_assert!(start as u64 <= self.produced && out.pos as u64 >= self.produced);
            let (prefix, dict): (usize, &[u8]) = match old {
                None => (
                    out.pos - start,
                    self.dict.as_ref().map_or(&[][..], |d| &d.content[..]),
                ),
                Some((_, hist)) => (out.pos - start - hist, &[]),
            };
            let pos = out.pos;
            write_block(
                &mut self.block,
                btype,
                size,
                src,
                out,
                prefix,
                dict,
                self.block_max,
            )
            // With room for `need`, a block that doesn't fit is corrupt (as
            // the window buffer's bound or the content size check finds).
            .map_err(|e| match e {
                Error::OutputTooSmall => Error::Corrupted,
                e => e,
            })?;
            let n = out.pos - pos;
            if let Some(x) = &mut self.xxh {
                // SAFETY: just written.
                #[allow(unsafe_code)]
                x.update(unsafe { core::slice::from_raw_parts(out.ptr.add(pos), n) });
            }
            self.cur += n;
            self.flush = self.cur;
            n
        } else {
            self.ensure_window()?;
            let base = self.win.as_mut_ptr();
            let (dict, cap): (&[u8], usize) = match old {
                None => (
                    self.dict.as_ref().map_or(&[][..], |d| &d.content[..]),
                    self.win_size,
                ),
                Some((old_end, hist)) => {
                    let from = old_end - hist;
                    // SAFETY: `from..old_end` was written by earlier blocks
                    // (or `leave_direct`). The buffer sizing (window + 2
                    // blocks + 64) keeps it after `start + block_max +
                    // WILDCOPY_OVERLENGTH`, so the writes of this block
                    // (bounded by `cap = from`) never touch it.
                    debug_assert!(
                        from == old_end || start + self.block_max + WILDCOPY_OVERLENGTH <= from
                    );
                    #[allow(unsafe_code)]
                    let d = unsafe { core::slice::from_raw_parts(base.add(from), hist) };
                    (d, if from == old_end { self.win_size } else { from })
                }
            };
            let mut wout = Output {
                ptr: base,
                cap,
                pos: start,
            };
            write_block(
                &mut self.block,
                btype,
                size,
                src,
                &mut wout,
                self.prefix,
                dict,
                self.block_max,
            )?;
            self.cur = wout.pos;
            if let Some(x) = &mut self.xxh {
                // SAFETY: just written.
                #[allow(unsafe_code)]
                x.update(unsafe { core::slice::from_raw_parts(base.add(start), self.cur - start) });
            }
            self.cur - start
        };
        self.produced += written as u64;
        Ok(())
    }

    /// Allocates the window buffer for the current frame.
    fn ensure_window(&mut self) -> Result<()> {
        if self.win.capacity() < self.win_size {
            self.win = Vec::new();
            self.win
                .try_reserve_exact(self.win_size)
                .map_err(|_| Error::OutputTooLarge)?;
        }
        Ok(())
    }

    /// Ends direct output for the current frame, if on: copies the history
    /// later blocks can reach from `out` (the caller's output, ending at
    /// the write position) into the window buffer, where the layout (`cur`,
    /// `old_end`) puts it. Then decoding goes on buffered.
    pub(crate) fn leave_direct(&mut self, out: &[u8]) -> Result<()> {
        if !self.direct {
            return Ok(());
        }
        if let Err(e) = self.ensure_window() {
            self.reset(); // as `drop_direct`
            return Err(e);
        }
        self.direct = false;
        // The current segment: the output's last `cur` bytes.
        let seg = out.len() - self.cur;
        self.fill_window(0, &out[seg..]);
        if let Some(old_end) = self.old_end {
            // The old segment's tail that the blocks from `cur` on can
            // reach (less as `cur` grows), right before the segment.
            let hist = self.window.saturating_sub(self.cur).min(old_end);
            debug_assert!(
                hist == 0 || self.cur + self.block_max + WILDCOPY_OVERLENGTH <= old_end - hist
            );
            self.fill_window(old_end - hist, &out[seg - hist..seg]);
        }
        self.flush = self.cur;
        Ok(())
    }

    /// Copies `src` into the window buffer at `at`, in pieces: one large
    /// `memcpy` may use non-temporal stores (glibc does from about 1 MiB
    /// on), which leave the history out of the cache that the next blocks'
    /// matches read (measured: 11% more cycles for 1 MiB outputs).
    fn fill_window(&mut self, at: usize, src: &[u8]) {
        const PIECE: usize = 64 << 10;
        assert!(at <= self.win.capacity() && src.len() <= self.win.capacity() - at);
        let dst = self.win.as_mut_ptr();
        for (i, piece) in src.chunks(PIECE).enumerate() {
            // SAFETY: `at + src.len() <= capacity` (checked above).
            #[allow(unsafe_code)]
            unsafe {
                core::ptr::copy_nonoverlapping(piece.as_ptr(), dst.add(at + i * PIECE), piece.len())
            };
        }
    }

    fn end_of_blocks(&mut self) -> Result<Stage> {
        let fh = self.fh.take().ok_or(Error::Corrupted)?;
        if let Some(cs) = fh.content_size
            && cs != self.produced
        {
            return Err(Error::Corrupted);
        }
        // The frame's history isn't needed any more.
        self.direct = false;
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

/// Writes one block at `out.pos`: copies a raw block, repeats an RLE
/// block's byte, or decodes a compressed block (history: `out` from
/// `prefix` on, after `dict`). The caller checked the block's size against
/// `block_max`, and that a raw or RLE block fits in `out`.
#[allow(clippy::too_many_arguments)]
fn write_block(
    block: &mut BlockDecoder,
    btype: u8,
    size: usize,
    src: &[u8],
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
    block_max: usize,
) -> Result<()> {
    match btype {
        0 => {
            debug_assert!(size <= out.cap - out.pos && size <= src.len());
            // SAFETY: `src` holds the block's `size` bytes and the output
            // has room for them (the caller's checks, asserted).
            #[allow(unsafe_code)]
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr(), out.ptr.add(out.pos), size)
            };
            out.pos += size;
        }
        1 => {
            debug_assert!(size <= out.cap - out.pos);
            // SAFETY: as above.
            #[allow(unsafe_code)]
            unsafe {
                core::ptr::write_bytes(out.ptr.add(out.pos), src[0], size)
            };
            out.pos += size;
        }
        _ => {
            let start = out.pos;
            block.decode_block(src, out, prefix, dict, block_max)?;
            // `decode_block` bounds a block by `block_max`.
            debug_assert!(out.pos - start <= block_max);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enc::{EndOp, StreamEncoder};
    use alloc::vec;

    /// Text from a small vocabulary: matches near and far.
    fn text(len: usize) -> Vec<u8> {
        const WORDS: [&[u8]; 8] = [
            b"zstd ",
            b"window ",
            b"block ",
            b"direct ",
            b"history ",
            b"wrap\n",
            b"a ",
            b"the ",
        ];
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut v = Vec::new();
        while v.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.extend_from_slice(WORDS[(x % 8) as usize]);
            if x.is_multiple_of(11) {
                v.push((x >> 32) as u8);
            }
        }
        v.truncate(len);
        v
    }

    /// A frame of `data` with window log `wlog` (10: 1 KiB blocks), with
    /// or without the content size, with a checksum.
    fn frame(data: &[u8], wlog: u32, fcs: bool) -> Vec<u8> {
        let mut e = StreamEncoder::new(1);
        e.set_window_log(wlog);
        e.set_checksum(true).unwrap();
        e.set_content_size_flag(fcs).unwrap();
        let mut out = vec![0u8; crate::compress_bound(data.len()) + 64];
        let p = e.compress(data, &mut out, EndOp::End).unwrap();
        assert_eq!((p.read, p.remaining), (data.len(), 0));
        out.truncate(p.written);
        out
    }

    /// Streams `src` in `in_step` pieces into outputs of `out_size`.
    fn stream(src: &[u8], in_step: usize, out_size: usize) -> Result<Vec<u8>> {
        let mut d = StreamDecoder::new();
        let mut got = Vec::new();
        let mut buf = vec![0u8; out_size];
        let mut ip = 0;
        loop {
            let n = in_step.min(src.len() - ip);
            let p = d.decode(&src[ip..ip + n], &mut buf)?;
            ip += p.read;
            got.extend_from_slice(&buf[..p.written]);
            if ip == src.len() && p.read == 0 && p.written == 0 {
                return if p.at_frame_boundary {
                    Ok(got)
                } else {
                    Err(Error::Truncated)
                };
            }
        }
    }

    /// Direct output (outputs that hold a block or the frame), switching
    /// to the window buffer mid-frame (input or output running out), and
    /// frames many windows long, against the window buffer alone
    /// (1-byte outputs).
    #[test]
    fn direct_and_buffered_agree() {
        let len = if cfg!(miri) { 5000 } else { 300_000 };
        let data = text(len);
        let mut two = data.clone();
        two.extend_from_slice(&data);
        for (wlog, fcs) in [(10, true), (10, false), (17, true)] {
            let f = frame(&data, wlog, fcs);
            let mut f2 = f.clone();
            f2.extend_from_slice(&f);
            assert!(stream(&f, 1 << 20, 1).unwrap() == data);
            let steps: &[usize] = if cfg!(miri) {
                &[usize::MAX, 700]
            } else {
                &[usize::MAX, 1, 700, 4096]
            };
            for &in_step in steps {
                let outs: &[usize] = if cfg!(miri) {
                    &[1024, len, 2 * len + 7]
                } else {
                    &[1000, 1024, 3000, len - 1, len, len + 1, 2 * len + 7]
                };
                for &out in outs {
                    let got = stream(&f, in_step, out).unwrap();
                    assert!(got == data, "wlog {wlog} fcs {fcs} in {in_step} out {out}");
                    let got = stream(&f2, in_step, out).unwrap();
                    assert!(
                        got == two,
                        "2 frames: wlog {wlog} fcs {fcs} in {in_step} out {out}"
                    );
                }
            }
        }
    }

    /// Corrupt frames: direct output rejects what the window buffer
    /// rejects and accepts what it accepts, with the same output.
    #[test]
    fn direct_rejects_as_buffered() {
        let data = text(if cfg!(miri) { 3000 } else { 100_000 });
        let iters = if cfg!(miri) { 2 } else { 2000 };
        let mut x = 12345u64;
        for fcs in [true, false] {
            let f = frame(&data, 10, fcs);
            for _ in 0..iters {
                let mut m = f.clone();
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let p = (x >> 33) as usize % m.len();
                m[p] ^= 1 << ((x >> 20) % 8);
                let buffered = stream(&m, usize::MAX, 1);
                let direct = stream(&m, usize::MAX, 2 * data.len());
                assert_eq!(buffered.is_ok(), direct.is_ok(), "byte {p}");
                if let (Ok(a), Ok(b)) = (&buffered, &direct) {
                    assert!(a == b, "byte {p}: output differs");
                }
                // Going on after an error without `reset` (misuse) stays
                // memory safe: no history is read that was not written.
                let mut d = StreamDecoder::new();
                let mut buf = vec![0u8; 2 * data.len()];
                if d.decode(&m, &mut buf).is_err() {
                    let _ = d.decode(&f[f.len() / 2..], &mut buf[..1]);
                    let _ = d.decode(&f[f.len() / 2..], &mut buf);
                }
            }
        }
    }

    /// `Decoder::read_to_end` (`decode_vec`), and leaving direct output
    /// for later `decode` calls.
    #[cfg(feature = "std")]
    #[test]
    fn vec_output() {
        let data = text(if cfg!(miri) { 4000 } else { 200_000 });
        for fcs in [true, false] {
            let f = frame(&data, 10, fcs);
            let mut d = StreamDecoder::new();
            let mut v = b"prefix".to_vec();
            // In pieces: the frame stays direct in `v` across calls.
            let mut ip = 0;
            while ip < f.len() {
                let n = 999.min(f.len() - ip);
                ip += d.decode_vec(&f[ip..ip + n], &mut v).unwrap();
            }
            assert!(d.at_frame_boundary() && v[6..] == data[..]);

            // Half the input into the `Vec`, the rest through `decode`.
            let mut d = StreamDecoder::new();
            let mut v = Vec::new();
            let half = f.len() / 2;
            assert_eq!(d.decode_vec(&f[..half], &mut v), Ok(half));
            d.leave_direct(&v).unwrap();
            let mut buf = vec![0u8; 777];
            let mut ip = half;
            loop {
                let p = d.decode(&f[ip..], &mut buf).unwrap();
                ip += p.read;
                v.extend_from_slice(&buf[..p.written]);
                if p.written == 0 && p.read == 0 {
                    break;
                }
            }
            assert!(d.at_frame_boundary() && v == data);
        }
    }
}
