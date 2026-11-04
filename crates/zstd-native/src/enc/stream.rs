//! Streaming compression: a port of libzstd 1.5.7's `ZSTD_compressStream2`
//! (`ZSTD_compressStream_generic`) in its default buffered mode
//! (`ZSTD_bm_buffered` for input and output), whose output is
//! byte-identical to libzstd's for the same input, level and sequence of
//! operations.
//!
//! # The input buffer
//!
//! Input is copied into a ring buffer of `windowSize + blockSize` bytes
//! (`inBuffSize`). A block is compressed (`ZSTD_compressContinue`) when the
//! buffer holds a full block past `inToCompress` (or on a flush or the
//! end); the next block then goes after it, or back at the start of the
//! buffer when it would not fit. Wrapping makes the window non-contiguous:
//! the older segment, at the end of the buffer, becomes the extDict and the
//! match finders' `_extDict` variants run until the window has moved past
//! it. Both segments are in the same buffer, so the window's positions (see
//! the `fast` module docs) map `base + i` (prefix) and `dictBase + i`
//! (extDict) into it.
//!
//! # Dictionaries
//!
//! A dictionary ([`StreamEncoder::with_dictionary`],
//! [`StreamEncoder::with_prepared_dictionary`]) applies to every frame; the
//! frame starts from it like a one-shot frame (see the `cdict` module), its
//! content staying in its own buffer.
//!
//! # Parameters
//!
//! They are chosen when the first call of a frame starts it
//! (`ZSTD_CCtx_init_compressStream2`): for the pledged source size if one
//! was given, else for the size of the input if that first call ends the
//! frame ([`EndOp::End`]), else for an unknown size (the level's largest
//! window). The frame header holds the content size when it is known.
//!
//! # Multithreading
//!
//! With `std` and workers (`StreamEncoder::set_workers`), a frame of
//! unknown size or over 512 KiB is compressed by the `mt` module instead
//! (`ZSTDMT_compressStream_generic`): its input goes into job buffers, not
//! the ring, the shortcut below does not apply, and the output does not
//! depend on the output buffer sizes.
//!
//! # libzstd's shortcut
//!
//! An [`EndOp::End`] call that finds the input buffer empty at its start
//! (nothing loaded yet in the frame, or just wrapped) and an output buffer
//! of at least [`compress_bound`] of the remaining input compresses that
//! input directly as the frame's last chunk (`ZSTD_compressEnd`), not block
//! by block through the input buffer. That changes the block boundaries,
//! hence the output, which is therefore not a function of the input and the
//! operations alone: it also depends on the output buffer sizes, exactly as
//! with libzstd.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::cdict::{DictSlot, EncoderDictionary, frame_cparams};
use super::compress::{FrameCompressor, FrameParams, FrameStage, compress_bound};
use super::params::{CONTENTSIZE_UNKNOWN, WINDOWLOG_ABSOLUTEMIN, WINDOWLOG_MAX, ldm_enabled};
use crate::error::{Error, Result};

/// Bytes after the input buffer's ring, never part of the window: reads
/// near the end of a segment stay inside the allocation.
const IN_BUFF_SLACK: usize = 32;
/// Gap between the ring and input appended after it (see
/// [`StreamEncoder::compress`]'s shortcut): keeps that input non-contiguous
/// with the window, as a separate buffer is in libzstd.
const APPEND_GAP: usize = 64;

/// What [`StreamEncoder::compress`] does after taking the input (libzstd's
/// `ZSTD_EndDirective`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndOp {
    /// `ZSTD_e_continue`: compress whole blocks as they fill up; keep the
    /// rest buffered.
    Continue,
    /// `ZSTD_e_flush`: also compress what is buffered (as a block that
    /// does not end the frame) and write it out. Flushing often costs
    /// compression ratio.
    Flush,
    /// `ZSTD_e_end`: compress everything and end the frame. The next call
    /// starts a new frame.
    End,
}

/// What one [`StreamEncoder::compress`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressProgress {
    /// Input bytes consumed.
    pub read: usize,
    /// Output bytes written.
    pub written: usize,
    /// Compressed bytes still waiting for room in the output (libzstd's
    /// return value of `ZSTD_compressStream2`). After [`EndOp::Flush`],
    /// 0 means the flush is complete; after [`EndOp::End`], that the frame
    /// is complete.
    pub remaining: usize,
}

/// `zcss_*`: the streaming stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// No frame started: the next call starts one.
    Init,
    /// Loading input into the input buffer.
    Load,
    /// Writing out the output buffer.
    Flush,
}

/// A streaming compressor (libzstd's `ZSTD_CStream` with
/// `ZSTD_compressStream2`): feed it input in any pieces, get output in any
/// pieces, frame after frame. The output is byte-identical to libzstd
/// 1.5.7's for the same level, input, operations and output buffer sizes
/// (see [`compress`](StreamEncoder::compress)). No `std` needed;
/// [`Encoder`](crate::Encoder) wraps it as a `std::io::Write`.
///
/// ```
/// use zstd_native::{EndOp, StreamEncoder};
///
/// let data = b"hello hello hello hello hello hello".repeat(100);
/// let mut enc = StreamEncoder::new(3);
/// let mut out = Vec::new();
/// let mut buf = [0u8; 64];
/// let mut input = &data[..];
/// // Feed the input in pieces of 100 bytes, then end the frame.
/// while !input.is_empty() {
///     let n = input.len().min(100);
///     let mut piece = &input[..n];
///     while !piece.is_empty() {
///         let p = enc.compress(piece, &mut buf, EndOp::Continue)?;
///         piece = &piece[p.read..];
///         out.extend_from_slice(&buf[..p.written]);
///     }
///     input = &input[n..];
/// }
/// loop {
///     let p = enc.compress(&[], &mut buf, EndOp::End)?;
///     out.extend_from_slice(&buf[..p.written]);
///     if p.remaining == 0 {
///         break;
///     }
/// }
/// assert_eq!(zstd_native::decompress(&out)?, data);
/// # Ok::<(), zstd_native::Error>(())
/// ```
///
/// At level 22 with a window of 128 MiB, i.e. for unknown sizes and sizes
/// over 64 MiB, libzstd enables long distance matching by itself, and so
/// does this encoder, with the same output (and libzstd's memory use: over
/// 800 MiB for the tables and the input buffer).
pub struct StreamEncoder {
    level: i32,
    /// `ZSTD_c_windowLog` (0: the level's).
    window_log: u32,
    /// `ZSTD_c_enableLongDistanceMatching` set to enable (else auto).
    force_ldm: bool,
    /// `pledgedSrcSizePlusOne - 1` for the next or current frame.
    pledged: Option<u64>,
    /// The frame parameters of the next frames.
    fparams: FrameParams,
    stage: Stage,
    fc: Option<Box<FrameCompressor>>,
    /// `inBuff`: `in_buff_size` bytes of ring and [`IN_BUFF_SLACK`].
    in_buff: Vec<u8>,
    in_buff_size: usize,
    in_to_compress: usize,
    in_buff_pos: usize,
    in_buff_target: usize,
    /// `outBuff`: `compress_bound(blockSize) + 1` bytes once used.
    out_buff: Vec<u8>,
    out_content: usize,
    out_flushed: usize,
    frame_ended: bool,
    /// The dictionary of the next frames.
    dict: DictSlot,
    /// Multithreading parameters of the next frames.
    #[cfg(feature = "std")]
    mt_params: super::mt::MtParams,
    /// The multithreaded context, once used.
    #[cfg(feature = "std")]
    mt: Option<Box<super::mt::MtCtx>>,
    /// The current frame is compressed by [`mt`](Self::mt).
    #[cfg(feature = "std")]
    mt_on: bool,
}

impl core::fmt::Debug for StreamEncoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamEncoder")
            .field("level", &self.level)
            .field("stage", &self.stage)
            .field("pledged", &self.pledged)
            .field("workers", &self.workers_or_0())
            .finish_non_exhaustive()
    }
}

impl StreamEncoder {
    /// A streaming compressor for `level` (see [`compress`](crate::compress)).
    pub fn new(level: i32) -> Self {
        StreamEncoder {
            level,
            window_log: 0,
            force_ldm: false,
            pledged: None,
            fparams: FrameParams::default(),
            stage: Stage::Init,
            fc: None,
            in_buff: Vec::new(),
            in_buff_size: 0,
            in_to_compress: 0,
            in_buff_pos: 0,
            in_buff_target: 0,
            out_buff: Vec::new(),
            out_content: 0,
            out_flushed: 0,
            frame_ended: false,
            dict: DictSlot::None,
            #[cfg(feature = "std")]
            mt_params: super::mt::MtParams::default(),
            #[cfg(feature = "std")]
            mt: None,
            #[cfg(feature = "std")]
            mt_on: false,
        }
    }

    /// The worker count (0 without `std`).
    fn workers_or_0(&self) -> u32 {
        #[cfg(feature = "std")]
        return self.mt_params.workers;
        #[cfg(not(feature = "std"))]
        0
    }

    /// Compresses the next frames with `n` worker threads (libzstd's
    /// `ZSTD_c_nbWorkers`; at most 256, 64 on 32-bit targets); 0, the
    /// default, compresses in the calling thread. The output is
    /// byte-identical to libzstd's with the same worker count, see
    /// [`Compressor::set_workers`](crate::Compressor::set_workers) for how
    /// frames are cut into jobs, threads and memory.
    ///
    /// A frame whose pledged size (or, when the first call ends it, input
    /// size) is at most 512 KiB is still compressed in the calling thread.
    /// Otherwise, input is buffered until a job is full (or a flush or the
    /// end comes), then compressed by a worker while more input is taken:
    /// [`compress`](Self::compress) with [`EndOp::Continue`] returns once
    /// it has taken some input or written some output (waiting for the
    /// oldest job when it could do neither), and the output does not
    /// depend on the output buffer sizes.
    ///
    /// # Errors
    ///
    /// [`Error::StageWrong`] in the middle of a frame.
    #[cfg(feature = "std")]
    pub fn set_workers(&mut self, n: u32) -> Result<()> {
        if self.stage != Stage::Init {
            return Err(Error::StageWrong);
        }
        self.mt_params.set_workers(n);
        Ok(())
    }

    /// The worker thread count (see [`set_workers`](Self::set_workers)).
    #[cfg(feature = "std")]
    pub fn workers(&self) -> u32 {
        self.mt_params.workers
    }

    /// Sets the job size of multithreaded compression, from the next frame
    /// on (see [`Compressor::set_job_size`](crate::Compressor::set_job_size)).
    /// For tests.
    #[cfg(feature = "std")]
    #[doc(hidden)]
    pub fn set_job_size(&mut self, size: usize) {
        self.mt_params.set_job_size(size);
    }

    /// Sets the overlap log of multithreaded compression, from the next
    /// frame on (see
    /// [`Compressor::set_overlap_log`](crate::Compressor::set_overlap_log)).
    /// For tests.
    ///
    /// # Panics
    ///
    /// If `log` is over 9.
    #[cfg(feature = "std")]
    #[doc(hidden)]
    pub fn set_overlap_log(&mut self, log: u32) {
        self.mt_params.set_overlap_log(log);
    }

    /// A streaming compressor for `level` with the dictionary `dict` (see
    /// [`set_dictionary`](Self::set_dictionary)).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidDictionary`] if a zstd-format dictionary is corrupt.
    pub fn with_dictionary(level: i32, dict: &[u8]) -> Result<Self> {
        let mut e = Self::new(level);
        e.set_dictionary(dict)?;
        Ok(e)
    }

    /// A streaming compressor with a prepared dictionary, at its level (see
    /// [`set_prepared_dictionary`](Self::set_prepared_dictionary)).
    pub fn with_prepared_dictionary(dict: &EncoderDictionary) -> Self {
        let mut e = Self::new(dict.level());
        e.dict = DictSlot::Prepared(dict.inner.clone());
        e
    }

    /// Changes the level, from the next frame on. With a prepared
    /// dictionary, frames take the dictionary's level instead (see
    /// [`Compressor::set_level`](crate::Compressor::set_level)).
    pub fn set_level(&mut self, level: i32) {
        self.level = level;
    }

    /// Uses `dict` for the next frames (libzstd's
    /// `ZSTD_CCtx_loadDictionary`; see
    /// [`Compressor::set_dictionary`](crate::Compressor::set_dictionary)),
    /// replacing any dictionary; an empty `dict` removes it.
    ///
    /// # Errors
    ///
    /// [`Error::StageWrong`] in the middle of a frame,
    /// [`Error::InvalidDictionary`] if a zstd-format dictionary is corrupt
    /// (the encoder then has no dictionary).
    pub fn set_dictionary(&mut self, dict: &[u8]) -> Result<()> {
        if self.stage != Stage::Init {
            return Err(Error::StageWrong);
        }
        self.dict = DictSlot::None;
        self.dict = DictSlot::local(dict)?;
        Ok(())
    }

    /// Uses the prepared `dict` for the next frames, at its level (libzstd's
    /// `ZSTD_CCtx_refCDict`), replacing any dictionary.
    ///
    /// # Errors
    ///
    /// [`Error::StageWrong`] in the middle of a frame.
    pub fn set_prepared_dictionary(&mut self, dict: &EncoderDictionary) -> Result<()> {
        if self.stage != Stage::Init {
            return Err(Error::StageWrong);
        }
        self.dict = DictSlot::Prepared(dict.inner.clone());
        Ok(())
    }

    /// Overrides the window log (libzstd's `ZSTD_c_windowLog`; 0 restores
    /// the level's), from the next frame on. For tests.
    ///
    /// # Panics
    ///
    /// If `log` is neither 0 nor in `10..=31` (`10..=30` on 32-bit targets).
    #[doc(hidden)]
    pub fn set_window_log(&mut self, log: u32) {
        assert!(
            log == 0 || (WINDOWLOG_ABSOLUTEMIN..=WINDOWLOG_MAX).contains(&log),
            "window log out of range"
        );
        self.window_log = log;
    }

    /// Forces long distance matching on (libzstd's
    /// `ZSTD_c_enableLongDistanceMatching` set to `ZSTD_ps_enable`; `false`
    /// restores the default, where it is enabled for `btopt` and above with
    /// a window of at least 128 MiB), from the next frame on. The window log
    /// then defaults to 27 (128 MiB) unless set with
    /// [`set_window_log`](Self::set_window_log). For tests.
    #[doc(hidden)]
    pub fn set_long_distance_matching(&mut self, on: bool) {
        self.force_ldm = on;
    }

    /// Declares the size of the next frame's content (libzstd's
    /// `ZSTD_CCtx_setPledgedSrcSize`; `None`: unknown, the default). The
    /// frame header then holds it, and the parameters are chosen for it
    /// (a smaller window for small sizes). The frame must then get exactly
    /// that many bytes. It applies to one frame only.
    ///
    /// # Errors
    ///
    /// [`Error::StageWrong`] in the middle of a frame.
    pub fn set_pledged_src_size(&mut self, size: Option<u64>) -> Result<()> {
        if self.stage != Stage::Init {
            return Err(Error::StageWrong);
        }
        self.pledged = size;
        Ok(())
    }

    /// Ends the next frames with a content checksum (libzstd's
    /// `ZSTD_c_checksumFlag`; off by default; see
    /// [`Compressor::set_checksum`](crate::Compressor::set_checksum)).
    ///
    /// # Errors
    ///
    /// [`Error::StageWrong`] in the middle of a frame.
    pub fn set_checksum(&mut self, on: bool) -> Result<()> {
        self.set_fparams(|fp| fp.checksum = on)
    }

    /// Writes the content size into the next frames' headers when it is
    /// known (libzstd's `ZSTD_c_contentSizeFlag`; on by default; see
    /// [`Compressor::set_content_size_flag`](crate::Compressor::set_content_size_flag)).
    /// The pledged size still chooses the parameters and is still checked.
    ///
    /// # Errors
    ///
    /// [`Error::StageWrong`] in the middle of a frame.
    pub fn set_content_size_flag(&mut self, on: bool) -> Result<()> {
        self.set_fparams(|fp| fp.content_size = on)
    }

    /// Writes the dictionary's ID into the next frames' headers (libzstd's
    /// `ZSTD_c_dictIDFlag`; on by default; see
    /// [`Compressor::set_dict_id_flag`](crate::Compressor::set_dict_id_flag)).
    ///
    /// # Errors
    ///
    /// [`Error::StageWrong`] in the middle of a frame.
    pub fn set_dict_id_flag(&mut self, on: bool) -> Result<()> {
        self.set_fparams(|fp| fp.dict_id = on)
    }

    fn set_fparams(&mut self, f: impl FnOnce(&mut FrameParams)) -> Result<()> {
        if self.stage != Stage::Init {
            return Err(Error::StageWrong);
        }
        f(&mut self.fparams);
        Ok(())
    }

    /// Abandons the current frame, if any (libzstd's `ZSTD_CCtx_reset`
    /// with `ZSTD_reset_session_only`): the next call starts a new one. The
    /// pledged source size is forgotten; the level is kept.
    pub fn reset(&mut self) {
        self.stage = Stage::Init;
        self.pledged = None;
        self.out_content = 0;
        self.out_flushed = 0;
        self.frame_ended = false;
        #[cfg(feature = "std")]
        if core::mem::take(&mut self.mt_on)
            && let Some(m) = self.mt.as_deref_mut()
        {
            m.abandon();
        }
    }

    /// Whether no frame is in progress: the last frame was ended and all its
    /// output was written (or nothing was compressed yet).
    pub fn at_frame_boundary(&self) -> bool {
        self.stage == Stage::Init
    }

    /// Compresses as much of `input` as possible into `output`, then does
    /// what `op` asks (libzstd's `ZSTD_compressStream2`). Starts a frame if
    /// none is in progress.
    ///
    /// With [`EndOp::Continue`], it consumes input until the output is full
    /// (and may consume all of it while writing nothing). With
    /// [`EndOp::Flush`] and [`EndOp::End`], call again (with the rest of
    /// the input, if not all was read) until
    /// [`remaining`](CompressProgress::remaining) is 0.
    ///
    /// The output matches libzstd's for the same calls with the same
    /// input pieces and the same output buffer sizes. Without flushes, it
    /// doesn't depend on how the input is split across [`EndOp::Continue`]
    /// calls, nor on the output sizes as long as the frame ends with an
    /// [`EndOp::End`] call that has no input (see the module docs for why
    /// an `End` call with input may differ).
    ///
    /// # Errors
    ///
    /// [`Error::SrcSizeWrong`] if a frame gets more or less input than its
    /// pledged size. After an error the frame is abandoned (as with
    /// [`reset`](Self::reset)).
    pub fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        op: EndOp,
    ) -> Result<CompressProgress> {
        let r = self.compress_generic(input, output, op);
        if r.is_err() {
            self.reset();
        }
        r
    }

    /// `ZSTD_CCtx_init_compressStream2`: starts a frame (with the
    /// dictionary, if any: `ZSTD_compressBegin_internal`).
    fn init(&mut self, op: EndOp, in_size: usize) -> Result<()> {
        if op == EndOp::End {
            self.pledged = Some(in_size as u64); // auto-determine pledgedSrcSize
        }
        let (cd, level) = self.dict.for_frame(self.level)?;
        // ZSTD_getCParamsFromCCtxParams
        let cp = frame_cparams(
            level,
            cd.as_deref(),
            self.pledged,
            self.window_log,
            self.force_ldm,
        );
        // ZSTD_resolveEnableLdm
        let ldm = self.force_ldm || ldm_enabled(&cp);
        #[cfg(feature = "std")]
        {
            // do not invoke multi-threading when src size is too small
            self.mt_on = self.mt_params.applies(self.pledged);
            if self.mt_on {
                let m = self
                    .mt
                    .get_or_insert_with(|| Box::new(super::mt::MtCtx::new()));
                m.init_frame(&self.mt_params, cp, ldm, cd, self.pledged, self.fparams);
                self.out_content = 0;
                self.out_flushed = 0;
                self.stage = Stage::Load;
                self.frame_ended = false;
                return Ok(());
            }
        }
        let fc = FrameCompressor::start_frame(&mut self.fc, cp, cd.as_ref(), self.pledged, ldm);
        fc.set_frame_params(self.fparams);
        let bsm = fc.block_size_max;
        let pledged = self.pledged.unwrap_or(CONTENTSIZE_UNKNOWN);
        let window_size = (1u64 << cp.window_log).min(pledged).max(1) as usize;
        self.in_buff_size = window_size + bsm;
        self.in_to_compress = 0;
        self.in_buff_pos = 0;
        // for small input: avoid automatic flush on reaching end of block,
        // since it would require to add a 3-bytes null block to end frame
        self.in_buff_target = bsm + usize::from(bsm as u64 == pledged);
        self.out_content = 0;
        self.out_flushed = 0;
        self.stage = Stage::Load;
        self.frame_ended = false;
        Ok(())
    }

    /// The frame is complete and written out: back to the init stage
    /// (`ZSTD_CCtx_reset(ZSTD_reset_session_only)`).
    fn end_session(&mut self) {
        self.stage = Stage::Init;
        self.pledged = None;
    }

    /// `ZSTD_compressStream_generic`.
    fn compress_generic(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        op: EndOp,
    ) -> Result<CompressProgress> {
        if self.stage == Stage::Init {
            self.init(op, input.len())?;
        }
        #[cfg(feature = "std")]
        if self.mt_on {
            let m = self.mt.as_deref_mut().expect("multithreaded context");
            let p = m.compress(input, output, op)?;
            if op == EndOp::End && p.remaining == 0 {
                // compression completed
                self.mt_on = false;
                self.end_session();
            }
            return Ok(p);
        }
        let mut ip = 0usize;
        let mut opos = 0usize;
        loop {
            if self.stage == Stage::Load {
                if op == EndOp::End
                    && output.len() - opos >= compress_bound(input.len() - ip)
                    && self.in_buff_pos == 0
                {
                    // shortcut to compression pass directly into output buffer
                    opos += self.compress_end_direct(&mut output[opos..], &input[ip..])?;
                    ip = input.len();
                    self.frame_ended = true;
                    self.end_session();
                    break;
                }
                // complete loading into the input buffer
                let need = self.in_buff_size + IN_BUFF_SLACK;
                if self.in_buff.len() < need {
                    self.in_buff.resize(need, 0);
                }
                let loaded = (self.in_buff_target - self.in_buff_pos).min(input.len() - ip);
                self.in_buff[self.in_buff_pos..self.in_buff_pos + loaded]
                    .copy_from_slice(&input[ip..ip + loaded]);
                self.in_buff_pos += loaded;
                ip += loaded;
                if op == EndOp::Continue && self.in_buff_pos < self.in_buff_target {
                    // not enough input to fill a full block: stop here
                    break;
                }
                if op == EndOp::Flush && self.in_buff_pos == self.in_to_compress {
                    // empty
                    break;
                }

                // compress the current block (this stage cannot be stopped
                // in the middle)
                let fc = self.fc.as_deref_mut().expect("frame started");
                let bsm = fc.block_size_max;
                let i_size = self.in_buff_pos - self.in_to_compress;
                let into_output = output.len() - opos >= compress_bound(i_size);
                let dst: &mut [u8] = if into_output {
                    &mut output[opos..]
                } else {
                    let n = compress_bound(bsm) + 1;
                    if self.out_buff.len() < n {
                        self.out_buff.resize(n, 0);
                    }
                    &mut self.out_buff[..]
                };
                let last = op == EndOp::End && ip == input.len();
                let (s, e) = (self.in_to_compress, self.in_buff_pos);
                let c_size = if last {
                    fc.compress_end(dst, &self.in_buff, s, e)?
                } else {
                    fc.compress_continue(dst, &self.in_buff, s, e, false)?
                };
                self.frame_ended = last;
                // prepare next block
                self.in_buff_target = self.in_buff_pos + bsm;
                if self.in_buff_target > self.in_buff_size {
                    self.in_buff_pos = 0;
                    self.in_buff_target = bsm;
                }
                debug_assert!(last || self.in_buff_target <= self.in_buff_size);
                self.in_to_compress = self.in_buff_pos;
                if into_output {
                    // no need to flush
                    opos += c_size;
                    if self.frame_ended {
                        self.end_session();
                        break;
                    }
                    continue;
                }
                self.out_content = c_size;
                self.out_flushed = 0;
                self.stage = Stage::Flush;
            }

            // flush stage
            debug_assert!(self.stage == Stage::Flush);
            let to_flush = self.out_content - self.out_flushed;
            let flushed = to_flush.min(output.len() - opos);
            output[opos..opos + flushed]
                .copy_from_slice(&self.out_buff[self.out_flushed..self.out_flushed + flushed]);
            opos += flushed;
            self.out_flushed += flushed;
            if to_flush != flushed {
                // flush not fully completed, presumably because dst is too small
                break;
            }
            self.out_content = 0;
            self.out_flushed = 0;
            if self.frame_ended {
                self.end_session();
                break;
            }
            self.stage = Stage::Load;
        }
        Ok(CompressProgress {
            read: ip,
            written: opos,
            remaining: self.out_content - self.out_flushed,
        })
    }

    /// libzstd's `ZSTD_compressEnd` on the caller's input, the shortcut of
    /// `ZSTD_compressStream_generic`. At the start of a frame the window is
    /// empty and `src` is compressed in place; after a wrap of the input
    /// buffer, `src` is a new segment of the window, non-contiguous with
    /// the buffered history: it is appended after the ring (behind a gap)
    /// so that both are in one buffer.
    fn compress_end_direct(&mut self, dst: &mut [u8], src: &[u8]) -> Result<usize> {
        let fc = self.fc.as_deref_mut().expect("frame started");
        if fc.stage == FrameStage::Init {
            // nothing compressed in this frame yet: the window is empty
            return fc.compress_end(dst, src, 0, src.len());
        }
        let start = self.in_buff.len().max(self.in_buff_size) + APPEND_GAP;
        self.in_buff.resize(start, 0);
        self.in_buff.extend_from_slice(src);
        let r = fc.compress_end(dst, &self.in_buff, start, start + src.len());
        self.in_buff.truncate(self.in_buff_size + IN_BUFF_SLACK);
        r
    }
}

impl Default for StreamEncoder {
    /// A streaming compressor at the default level (3).
    fn default() -> Self {
        Self::new(super::params::CLEVEL_DEFAULT)
    }
}
