//! Multithreaded compression (with `std`): a port of libzstd 1.5.7's
//! `compress/zstdmt_compress.c` and of how `ZSTD_compressStream2` drives it
//! (`nbWorkers >= 1`), whose output is byte-identical to libzstd's for the
//! same parameters (level, worker count, job size, overlap log).
//!
//! # Jobs
//!
//! The input of a frame is cut into jobs of `targetSectionSize` bytes
//! ([`MtParams::job_size`], or by default `4 << windowLog` bytes, at least
//! 1 MiB), and a shorter one wherever a flush or the end of the frame
//! comes. Each job is compressed by one worker on its own, as a frame of
//! its own whose header is dropped (`ZSTDMT_compressionJob`): the first
//! job writes the frame header (and uses the dictionary, if any), each
//! later one starts from the last `overlap` bytes of the previous job
//! (loaded as a raw-content prefix, `ZSTD_c_forceMaxWindow`) with the
//! repcodes invalidated, and the last one ends the frame. A job compresses
//! its input in chunks of 512 KiB (`ZSTD_compressContinue`), which is where
//! its block splitting restarts. The jobs' outputs are written out in order
//! (`ZSTDMT_flushProduced`), so the output depends only on the parameters,
//! the input and where the flushes are, never on thread timing.
//!
//! # Long distance matching
//!
//! The long matches are found serially, job after job
//! (`ZSTDMT_serialState_genSequences`), in an LDM window over libzstd's
//! round input buffer: its positions (where a job's input lands, and when
//! the buffer wraps) are reproduced exactly (`ZSTDMT_tryGetInputRange`),
//! since they decide when the window becomes non-contiguous. The serial
//! step keeps its own copy of that buffer. The job then takes the long
//! matches as external sequences (`ZSTD_referenceExternalSequences`).
//!
//! # Differences in timing only
//!
//! libzstd posts a job when a worker is free and its table of jobs has
//! room; here every job is cut and queued as soon as libzstd would cut it
//! (a full job, a flush, the end), with at most libzstd's table size of
//! jobs not yet written out. Output arrives per job, not per chunk. One
//! case where libzstd's own output depends on timing: input that ends
//! exactly at a job boundary under `ZSTD_e_continue`, then `ZSTD_e_end`,
//! gives a last empty block here (libzstd's usual output), but no empty
//! block in libzstd if its job table happened to be full at that moment.

use std::any::Any;
use std::boxed::Box;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::vec;
use std::vec::Vec;

use super::cdict::{CDict, load_prefix};
use super::compress::{FrameCompressor, FrameParams, compress_bound};
use super::ldm::{LdmState, RawSeqStore};
use super::params::{BLOCKSIZE_MAX, CParams, Strategy, cycle_log};
use super::stream::{CompressProgress, EndOp};
use crate::error::{Error, Result};
use crate::xxhash::Xxh64;

/// `ZSTDMT_JOBSIZE_MIN`: the smallest job size, and the largest frame
/// compressed single-threaded even with workers.
pub(crate) const JOBSIZE_MIN: usize = 512 << 10;
/// `ZSTDMT_JOBLOG_MAX`
const JOBLOG_MAX: u32 = if usize::BITS == 32 { 29 } else { 30 };
/// `ZSTDMT_JOBSIZE_MAX`
pub(crate) const JOBSIZE_MAX: usize = if usize::BITS == 32 {
    512 << 20
} else {
    1 << 30
};
/// `ZSTDMT_NBWORKERS_MAX`
pub(crate) const NBWORKERS_MAX: u32 = if usize::BITS == 32 { 64 } else { 256 };
/// A job is compressed in chunks of this size (`4 * ZSTD_BLOCKSIZE_MAX`).
const CHUNK_SIZE: usize = 4 * BLOCKSIZE_MAX;
/// The job input the larger job table may hold (see [`job_table_size`]).
const RUN_AHEAD_BYTES: usize = 256 << 20;
/// Bytes after the LDM copy of the round buffer, never in its window.
const RING_SLACK: usize = 64;
/// The room before a job's data in its input buffer, to place it at an
/// offset of [`COPY_SKEW`] (mod 4 KiB) from the caller's input.
const PAGE: usize = 4096;
/// Where the input buffer starts relative to the caller's input, mod
/// [`PAGE`]: glibc's `memcpy` takes a much slower (non-temporal) path when
/// the destination is within a few hundred bytes of the source mod 4 KiB
/// (4K aliasing; 1.8x slower overall at level 1 with 16 workers on Zen 3).
const COPY_SKEW: usize = PAGE / 2;

/// The multithreading parameters (libzstd's `ZSTD_c_nbWorkers`,
/// `ZSTD_c_jobSize`, `ZSTD_c_overlapLog`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MtParams {
    /// `nbWorkers` (0: single-threaded).
    pub(crate) workers: u32,
    /// `jobSize` (0: automatic; else at least [`JOBSIZE_MIN`]).
    pub(crate) job_size: usize,
    /// `overlapLog` (0: automatic per strategy, else 1 to 9).
    pub(crate) overlap_log: u32,
}

impl MtParams {
    /// Sets the worker count (clamped to `ZSTDMT_NBWORKERS_MAX`).
    pub(crate) fn set_workers(&mut self, n: u32) {
        self.workers = n.min(NBWORKERS_MAX);
    }

    /// Sets the job size as `ZSTD_c_jobSize` does (0: automatic; else
    /// raised to [`JOBSIZE_MIN`], lowered to `ZSTDMT_JOBSIZE_MAX`).
    pub(crate) fn set_job_size(&mut self, size: usize) {
        self.job_size = if size == 0 {
            0
        } else {
            size.clamp(JOBSIZE_MIN, JOBSIZE_MAX)
        };
    }

    /// Sets the overlap log.
    ///
    /// # Panics
    ///
    /// If `log` is over 9.
    pub(crate) fn set_overlap_log(&mut self, log: u32) {
        assert!(log <= 9, "overlap log out of range");
        self.overlap_log = log;
    }

    /// `ZSTD_CCtx_init_compressStream2`'s choice: multithreaded with
    /// workers, unless the frame's size is known and at most
    /// [`JOBSIZE_MIN`].
    pub(crate) fn applies(&self, pledged: Option<u64>) -> bool {
        self.workers > 0 && pledged.is_none_or(|p| p > JOBSIZE_MIN as u64)
    }
}

/// `ZSTDMT_computeTargetJobLog()`.
fn target_job_log(cp: &CParams, ldm: bool) -> u32 {
    let job_log = if ldm {
        // In Long Range Mode, the windowLog is typically oversized: the job
        // size derives from the cycle log instead.
        21.max(cycle_log(cp.chain_log, cp.strategy) + 3)
    } else {
        20.max(cp.window_log + 2)
    };
    job_log.min(JOBLOG_MAX)
}

/// How many jobs may be posted and not yet written out: libzstd's job
/// table (`ZSTDMT_createJobsTable(nbWorkers + 2)`, the power of 2 above
/// `nbWorkers + 2`), or, with 8 workers or more, the power of 2 at or
/// above `2 * nbWorkers + 2` if that holds at most [`RUN_AHEAD_BYTES`] of
/// job input. Jobs are written out in order, so while the oldest job runs
/// the later ones finish and wait in the table; with only `nbWorkers + 2`
/// entries the queue then runs dry and workers idle (jobs of 2 to 8 MiB,
/// levels 1 to 4, vary in duration with the data). With 4 workers the
/// larger table measured 2-3% slower. The table size changes only the
/// timing, never the output.
fn job_table_size(workers: usize, target: usize) -> usize {
    let table = 1 << ((workers + 2).ilog2() + 1);
    if workers < 8 {
        return table;
    }
    let deep = (2 * workers + 2).next_power_of_two();
    table.max(deep.min(RUN_AHEAD_BYTES / target.max(1)))
}

/// `ZSTDMT_overlapLog_default()`.
fn overlap_log_default(strategy: Strategy) -> u32 {
    match strategy {
        Strategy::BtUltra2 => 9,
        Strategy::BtUltra | Strategy::BtOpt => 8,
        Strategy::BtLazy2 | Strategy::Lazy2 => 7,
        _ => 6,
    }
}

/// `ZSTDMT_computeOverlapSize()`: how much of the previous job a job
/// starts from.
fn overlap_size(overlap_log: u32, cp: &CParams, ldm: bool) -> usize {
    let log = if overlap_log == 0 {
        overlap_log_default(cp.strategy)
    } else {
        overlap_log
    };
    let overlap_r_log = 9 - log as i32;
    let mut ov_log = if overlap_r_log >= 8 {
        0
    } else {
        cp.window_log as i32 - overlap_r_log
    };
    if ldm {
        // the overlap becomes a fraction of the job size
        ov_log = cp.window_log.min(target_job_log(cp, ldm) - 2) as i32 - overlap_r_log;
    }
    debug_assert!((0..=31).contains(&ov_log));
    if ov_log == 0 { 0 } else { 1usize << ov_log }
}

/// The buffer sizes of a multithreaded frame, from
/// `ZSTDMT_initCStream_internal()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameSizes {
    /// `targetSectionSize`: the job size.
    pub(crate) target: usize,
    /// `targetPrefixSize`: the overlap.
    pub(crate) prefix_size: usize,
    /// The output buffer of a job (`ZSTD_compressBound(targetSectionSize)`).
    pub(crate) out_cap: usize,
    /// `roundBuff.capacity`: `max(windowSize, targetSectionSize *
    /// nbWorkers)` plus 2 or 3 jobs of slack.
    pub(crate) ring_cap: usize,
}

impl FrameSizes {
    /// The sizes for `p`, `cp` and `ldm`.
    ///
    /// # Errors
    ///
    /// [`Error::MemoryAllocation`] if a buffer would be larger than
    /// `isize::MAX` bytes (`PTRDIFF_MAX`), which no allocation can be.
    /// Only possible on 32-bit targets, where jobs are up to 512 MiB
    /// (`ZSTDMT_JOBSIZE_MAX`) and workers up to 64: level 22's 512 MiB
    /// jobs or `set_job_size(512 << 20)` need a round buffer of
    /// `(nbWorkers + 3) * 512 MiB`, 2 GiB with one worker. libzstd computes
    /// these sizes in `size_t` without overflow checks and `malloc`s the
    /// round buffer up front: the allocation fails (`memory_allocation`),
    /// or with more workers the size wraps around (to a smaller
    /// buffer than its jobs use). zstd-native returns the error in both
    /// cases.
    pub(crate) fn new(p: &MtParams, cp: &CParams, ldm: bool) -> Result<FrameSizes> {
        const MAX: usize = isize::MAX as usize;
        let prefix_size = overlap_size(p.overlap_log, cp, ldm);
        let target = if p.job_size == 0 {
            1usize << target_job_log(cp, ldm)
        } else {
            p.job_size
        };
        // job size must be >= overlap size
        let target = target.max(prefix_size);
        // compress_bound(n) <= n + n / 256 + 64
        let out_cap = target
            .checked_add((target >> 8) + 64)
            .filter(|&n| n <= MAX)
            .map(|_| compress_bound(target));
        // If ldm is enabled we need windowSize space; two buffers of slack,
        // plus one for the overlap.
        let window_size = if ldm { 1usize << cp.window_log } else { 0 };
        let nb_slack = 2 + usize::from(prefix_size > 0);
        let workers = (p.workers as usize).max(1);
        let ring_cap = target.checked_mul(workers).and_then(|sections| {
            window_size
                .max(sections)
                .checked_add(target.checked_mul(nb_slack)?)
        });
        match (out_cap, ring_cap) {
            // the LDM copy of the round buffer has RING_SLACK bytes more
            (Some(out_cap), Some(ring_cap)) if ring_cap <= MAX - RING_SLACK => Ok(FrameSizes {
                target,
                prefix_size,
                out_cap,
                ring_cap,
            }),
            _ => Err(Error::MemoryAllocation),
        }
    }
}

/// Locks `m`, ignoring poisoning (a panicking job is reported through its
/// [`Slot`]).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a job produced.
enum Outcome {
    /// The output buffer and the size written.
    Done(Vec<u8>, usize),
    Failed(Error),
    Panicked(Box<dyn Any + Send>),
}

/// Where a job's outcome appears.
#[derive(Default)]
struct Slot {
    outcome: Mutex<Option<Outcome>>,
    cv: Condvar,
}

impl Slot {
    fn set(&self, o: Outcome) {
        *lock(&self.outcome) = Some(o);
        self.cv.notify_all();
    }

    /// The outcome, waiting for it with `block`.
    fn take(&self, block: bool) -> Option<Outcome> {
        let mut g = lock(&self.outcome);
        while block && g.is_none() {
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
        g.take()
    }
}

/// Input and output buffers of finished jobs, for the next ones
/// (`ZSTDMT_bufferPool`): reusing them saves allocating and touching fresh
/// memory for every job.
#[derive(Default)]
struct Buffers {
    inputs: Mutex<Vec<Vec<u8>>>,
    outputs: Mutex<Vec<Vec<u8>>>,
    /// Buffers kept at most, of each kind: enough for a full job table
    /// (set per frame), so that steady-state compression allocates nothing.
    max_spare: AtomicUsize,
}

impl Buffers {
    /// An empty input buffer of at least `capacity` bytes.
    fn input(&self, capacity: usize) -> Vec<u8> {
        let mut b = lock(&self.inputs)
            .pop()
            .filter(|b| b.capacity() >= capacity)
            .unwrap_or_else(|| Vec::with_capacity(capacity));
        b.clear();
        b
    }

    /// An output buffer of at least `size` bytes.
    fn output(&self, size: usize) -> Vec<u8> {
        lock(&self.outputs)
            .pop()
            .filter(|b| b.len() >= size)
            .unwrap_or_else(|| vec![0u8; size])
    }

    fn put(&self, list: &Mutex<Vec<Vec<u8>>>, b: Vec<u8>) {
        let mut l = lock(list);
        if l.len() < self.max_spare.load(Ordering::Relaxed) {
            l.push(b);
        }
    }

    /// Takes back a job's input buffer when `data` is its last reference:
    /// the job's and the next job's prefix share it, and whichever of the
    /// worker and the calling thread lets go of it last recycles it
    /// (`Arc::into_inner` hands it to exactly one of them).
    fn put_shared_input(&self, data: Arc<Vec<u8>>) {
        if let Some(b) = Arc::into_inner(data) {
            self.put(&self.inputs, b);
        }
    }
}

/// The serial state (`SerialState`): the LDM state, shared by the jobs of
/// the frames of one [`MtCtx`], which reach it one after the other.
struct Serial {
    state: Mutex<SerialState>,
    cv: Condvar,
}

struct SerialState {
    /// The frame it is set up for (jobs of an abandoned frame skip it).
    generation: u64,
    /// `nextJobID`: the job whose turn it is.
    next_job: u64,
    ldm: Option<Box<LdmState>>,
    /// The round buffer's content as the LDM window sees it (grown as
    /// needed, up to the round buffer's size).
    ring: Vec<u8>,
}

/// What the jobs of a frame share.
struct Frame {
    cp: CParams,
    /// The frame parameters (the first job writes the header; the last
    /// one, if it is also the first, the checksum).
    fparams: FrameParams,
    /// The dictionary (the first job's only).
    cdict: Option<Arc<CDict>>,
    /// `frameContentSize`
    pledged: Option<u64>,
    /// The LDM serial state, with the frame's generation in it.
    serial: Option<Arc<Serial>>,
    generation: u64,
    /// `roundBuff.capacity`
    ring_cap: usize,
    /// Set when the frame is abandoned: its jobs stop early.
    cancelled: AtomicBool,
    bufs: Arc<Buffers>,
}

/// One job (`ZSTDMT_jobDescription`).
struct Job {
    frame: Arc<Frame>,
    /// `jobID`
    id: u64,
    /// Padding (`base` bytes), the prefix (the overlap with the previous
    /// job), then the input (from `prefix`).
    data: Arc<Vec<u8>>,
    base: usize,
    prefix: usize,
    /// Where the input is in libzstd's round buffer (for the LDM window).
    ring_pos: usize,
    /// `lastJob`
    last: bool,
    /// The output buffer size (`ZSTD_compressBound(targetSectionSize)`).
    out_cap: usize,
    slot: Arc<Slot>,
}

/// `ZSTDMT_serialState_genSequences()`: waits for the job's turn, then
/// finds its long matches (with LDM).
fn serial_step(job: &Job) -> Result<RawSeqStore> {
    let f = &*job.frame;
    let Some(s) = &f.serial else {
        return Ok(RawSeqStore::default());
    };
    let mut g = lock(&s.state);
    while g.generation == f.generation
        && g.next_job < job.id
        && !f.cancelled.load(Ordering::Relaxed)
    {
        g = s.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
    }
    if g.generation != f.generation {
        return Err(Error::StageWrong); // abandoned frame
    }
    let mut r = Ok(RawSeqStore::default());
    if g.next_job == job.id && !f.cancelled.load(Ordering::Relaxed) {
        let st = &mut *g;
        let src = &job.data[job.prefix..];
        let (start, end) = (job.ring_pos, job.ring_pos + src.len());
        debug_assert!(end <= f.ring_cap);
        if st.ring.len() < end + RING_SLACK {
            let n = (st.ring.len() * 2).clamp(end, f.ring_cap) + RING_SLACK;
            st.ring.resize(n, 0);
        }
        st.ring[start..end].copy_from_slice(src);
        if let Some(ldm) = st.ldm.as_deref_mut() {
            r = ldm.job_sequences(&st.ring, start, end);
        }
    }
    // Now it is the next job's turn
    g.next_job = g.next_job.max(job.id + 1);
    s.cv.notify_all();
    r
}

/// `ZSTDMT_serialState_ensureFinished()`: lets the next jobs through after
/// a job that failed before its serial step.
fn ensure_serial_finished(f: &Frame, id: u64) {
    if let Some(s) = &f.serial {
        let mut g = lock(&s.state);
        if g.generation == f.generation && g.next_job <= id {
            g.next_job = id + 1;
            s.cv.notify_all();
        }
    }
}

/// `ZSTDMT_compressionJob()` with the compressor `fc` (kept between jobs,
/// like libzstd's pool of contexts). Returns the output buffer and the size
/// written.
fn run_job(fc: &mut Option<Box<FrameCompressor>>, job: &Job) -> Result<(Vec<u8>, usize)> {
    let f = &*job.frame;
    let cancelled = || f.cancelled.load(Ordering::Relaxed);
    // Perform serial step as early as possible
    let seqs = serial_step(job)?;
    if cancelled() {
        return Err(Error::StageWrong);
    }
    let buf = &job.data[job.base..];
    let src_start = job.prefix - job.base;
    let size = buf.len() - src_start;
    let fc = if job.id == 0 {
        // the frame's parameters and size, the dictionary
        let fc = FrameCompressor::start_frame(fc, f.cp, f.cdict.as_ref(), f.pledged, false);
        fc.set_frame_params(f.fparams);
        fc
    } else {
        // the job's size, the previous job's end as a prefix
        let fc = FrameCompressor::start_frame_for_prefix(fc, f.cp, Some(size as u64), false);
        load_prefix(&mut fc.ms, buf, 0, src_start);
        fc
    };
    // External Sequences can only be applied after CCtx initialization
    fc.ext = seqs;
    let mut out = f.bufs.output(job.out_cap);
    let dst = &mut out[..job.out_cap];
    if job.id != 0 {
        // flush and overwrite frame header when it's not first job
        fc.compress_continue(dst, buf, src_start, src_start, false)?;
        fc.rep = [0; 3]; // ZSTD_invalidateRepCodes
    }
    // compress the entire job by smaller chunks, for better granularity
    let nb_chunks = size.div_ceil(CHUNK_SIZE);
    let mut ip = src_start;
    let mut op = 0;
    for _ in 1..nb_chunks {
        if cancelled() {
            return Err(Error::StageWrong);
        }
        op += fc.compress_continue(&mut dst[op..], buf, ip, ip + CHUNK_SIZE, false)?;
        ip += CHUNK_SIZE;
    }
    // last block
    if nb_chunks > 0 || job.last {
        let last1 = size & (CHUNK_SIZE - 1);
        let last_size = if last1 == 0 && size >= CHUNK_SIZE {
            CHUNK_SIZE
        } else {
            last1
        };
        op += if job.last {
            fc.compress_end(&mut dst[op..], buf, ip, ip + last_size)?
        } else {
            fc.compress_continue(&mut dst[op..], buf, ip, ip + last_size, false)?
        };
    }
    Ok((out, op))
}

/// Runs `job` and reports its outcome.
fn execute(fc: &mut Option<Box<FrameCompressor>>, job: Job) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(fc, &job)));
    let outcome = match r {
        Ok(Ok((v, n))) => Outcome::Done(v, n),
        Ok(Err(e)) => Outcome::Failed(e),
        Err(p) => {
            *fc = None;
            Outcome::Panicked(p)
        }
    };
    let Job {
        frame,
        id,
        data,
        slot,
        ..
    } = job;
    ensure_serial_finished(&frame, id);
    // the input buffer, unless the next job's prefix is still to be taken
    // from it (then the calling thread recycles it)
    frame.bufs.put_shared_input(data);
    slot.set(outcome);
}

/// The worker threads (libzstd's `POOL_ctx`), each with its compressor.
struct Pool {
    tx: Option<mpsc::Sender<Job>>,
    threads: Vec<JoinHandle<()>>,
    /// The worker count asked for (threads that could not be started are
    /// replaced by running their jobs in the calling thread).
    size: u32,
}

impl Pool {
    fn new(size: u32) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let mut threads = Vec::new();
        for i in 0..size {
            let rx = Arc::clone(&rx);
            let spawned = std::thread::Builder::new()
                .name(std::format!("zstd-native-{i}"))
                .spawn(move || {
                    let mut fc = None;
                    loop {
                        let job = lock(&rx).recv();
                        match job {
                            Ok(job) => execute(&mut fc, job),
                            Err(_) => return,
                        }
                    }
                });
            match spawned {
                Ok(h) => threads.push(h),
                Err(_) => break,
            }
        }
        Pool {
            tx: (!threads.is_empty()).then_some(tx),
            threads,
            size,
        }
    }
}

/// Where the output goes: a buffer of fixed size, or a vector it is
/// appended to.
enum Out<'a> {
    Slice(&'a mut [u8]),
    Vec(&'a mut Vec<u8>),
}

impl Out<'_> {
    /// The room left after `pos` bytes were written.
    fn room(&self, pos: usize) -> usize {
        match self {
            Out::Slice(s) => s.len() - pos,
            Out::Vec(_) => usize::MAX,
        }
    }

    /// Writes `data` at `*pos` (at most [`room`](Self::room) bytes).
    fn write(&mut self, pos: &mut usize, data: &[u8]) {
        match self {
            Out::Slice(s) => s[*pos..*pos + data.len()].copy_from_slice(data),
            Out::Vec(v) => v.extend_from_slice(data),
        }
        *pos += data.len();
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.tx = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// A job ready to be posted (`jobReady`).
enum Ready {
    Work(Job),
    /// `ZSTDMT_writeLastEmptyBlock`: no worker needed.
    Done(Vec<u8>),
}

/// The multithreaded compression context (`ZSTDMT_CCtx`), reused across
/// frames: its workers, and the state of the current frame.
pub(crate) struct MtCtx {
    pool: Pool,
    /// The compressor of jobs run in the calling thread (no worker could
    /// be started).
    inline_fc: Option<Box<FrameCompressor>>,
    serial: Option<Arc<Serial>>,
    generation: u64,
    frame: Option<Arc<Frame>>,
    /// `targetSectionSize`
    target: usize,
    /// `targetPrefixSize`
    prefix_size: usize,
    /// `roundBuff.capacity` and `roundBuff.pos`.
    ring_cap: usize,
    ring_pos: usize,
    /// `inBuff`: padding (`in_base` bytes), the prefix, then the input
    /// loaded so far (from `in_prefix`); `None` when no input range is held.
    in_buf: Option<Vec<u8>>,
    in_base: usize,
    in_prefix: usize,
    in_ring_pos: usize,
    /// The next job's prefix: the end of the previous job's data.
    prev: Option<(Arc<Vec<u8>>, usize, usize)>,
    /// `nextJobID`
    next_job: u64,
    /// The jobs posted and not yet written out, in order.
    jobs: VecDeque<Arc<Slot>>,
    ready: Option<Ready>,
    /// The output of the oldest job (`out[..out_len]`), being written out.
    out: Vec<u8>,
    out_len: usize,
    out_pos: usize,
    flushing: bool,
    bufs: Arc<Buffers>,
    /// `frameEnded`
    frame_ended: bool,
    /// Input taken in the frame, and its pledged size.
    consumed: u64,
    pledged: Option<u64>,
    /// `serial.xxhState`: the XXH64 of the input taken in the frame, with
    /// the checksum flag.
    xxh: Option<Xxh64>,
    /// The checksum to append to the last job's output, when the frame has
    /// several jobs (`frameChecksumNeeded`; a single job writes its own).
    trailer: Option<u32>,
    /// The job table's size (the smallest power of 2 above `nbWorkers +
    /// 2`): at most this many jobs posted and not written out.
    max_jobs: usize,
    out_cap: usize,
}

impl core::fmt::Debug for MtCtx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MtCtx")
            .field("workers", &self.pool.size)
            .field("target", &self.target)
            .field("prefix_size", &self.prefix_size)
            .finish_non_exhaustive()
    }
}

impl Drop for MtCtx {
    fn drop(&mut self) {
        self.abandon();
    }
}

impl MtCtx {
    pub(crate) fn new() -> Self {
        MtCtx {
            pool: Pool {
                tx: None,
                threads: Vec::new(),
                size: 0,
            },
            inline_fc: None,
            serial: None,
            generation: 0,
            frame: None,
            target: 0,
            prefix_size: 0,
            ring_cap: 0,
            ring_pos: 0,
            in_buf: None,
            in_base: 0,
            in_prefix: 0,
            in_ring_pos: 0,
            prev: None,
            next_job: 0,
            jobs: VecDeque::new(),
            ready: None,
            out: Vec::new(),
            out_len: 0,
            out_pos: 0,
            flushing: false,
            bufs: Arc::default(),
            frame_ended: false,
            consumed: 0,
            pledged: None,
            xxh: None,
            trailer: None,
            max_jobs: 0,
            out_cap: 0,
        }
    }

    /// `ZSTDMT_initCStream_internal()`: starts a frame of `pledged` bytes
    /// (`None`: unknown) with the frame parameters `cp`, long distance
    /// matching if `ldm`, and the dictionary `cdict`, abandoning any frame
    /// in progress.
    ///
    /// # Errors
    ///
    /// [`Error::MemoryAllocation`] if the buffers of the frame don't fit
    /// in memory (see [`FrameSizes::new`]); no frame is started.
    pub(crate) fn init_frame(
        &mut self,
        p: &MtParams,
        cp: CParams,
        ldm: bool,
        cdict: Option<Arc<CDict>>,
        pledged: Option<u64>,
        fparams: FrameParams,
    ) -> Result<()> {
        debug_assert!(p.workers > 0);
        self.abandon();
        let sizes = FrameSizes::new(p, &cp, ldm)?;
        if self.pool.size != p.workers {
            self.pool = Pool::new(0); // join the old workers first
            self.pool = Pool::new(p.workers);
        }
        let workers = p.workers as usize;
        let target = sizes.target;
        self.prefix_size = sizes.prefix_size;
        self.target = target;
        self.out_cap = sizes.out_cap;
        self.ring_cap = sizes.ring_cap;
        self.ring_pos = 0;
        self.in_buf = None;
        self.in_base = 0;
        self.in_prefix = 0;
        self.in_ring_pos = 0;
        self.prev = None;
        self.next_job = 0;
        self.frame_ended = false;
        self.consumed = 0;
        self.pledged = pledged;
        self.xxh = fparams.checksum.then(Xxh64::new);
        self.trailer = None;
        self.max_jobs = job_table_size(workers, target);
        // a buffer per job of the table, plus the one being filled
        self.bufs
            .max_spare
            .store(self.max_jobs + 1, Ordering::Relaxed);
        self.generation += 1;
        let serial = if ldm {
            let s = self.serial.get_or_insert_with(|| {
                Arc::new(Serial {
                    state: Mutex::new(SerialState {
                        generation: 0,
                        next_job: 0,
                        ldm: None,
                        ring: Vec::new(),
                    }),
                    cv: Condvar::new(),
                })
            });
            {
                // ZSTDMT_serialState_reset
                let mut g = lock(&s.state);
                g.generation = self.generation;
                g.next_job = 0;
                match g.ldm.as_deref_mut() {
                    Some(l) => l.reset(&cp, target),
                    None => g.ldm = Some(Box::new(LdmState::new(&cp, target))),
                }
                // (the copy of the round buffer grows as jobs use it)
                s.cv.notify_all();
            }
            Some(Arc::clone(s))
        } else {
            None
        };
        self.frame = Some(Arc::new(Frame {
            cp,
            fparams,
            cdict,
            pledged,
            serial,
            generation: self.generation,
            ring_cap: self.ring_cap,
            cancelled: AtomicBool::new(false),
            bufs: Arc::clone(&self.bufs),
        }));
        Ok(())
    }

    /// Abandons the frame in progress, if any: its jobs stop as soon as
    /// they can, their output is dropped.
    pub(crate) fn abandon(&mut self) {
        if let Some(f) = self.frame.take() {
            f.cancelled.store(true, Ordering::Relaxed);
            if let Some(s) = &f.serial {
                let _g = lock(&s.state);
                s.cv.notify_all();
            }
        }
        self.jobs.clear();
        self.ready = None;
        self.in_buf = None;
        self.prev = None;
        self.flushing = false;
    }

    /// The input bytes loaded into the input buffer (`inBuff.filled`).
    fn filled(&self) -> usize {
        self.in_buf.as_ref().map_or(0, |b| b.len() - self.in_prefix)
    }

    /// `ZSTD_compressStream2` on the multithreaded context: takes input,
    /// cuts and posts jobs, writes out their output. With
    /// [`EndOp::Continue`] it returns after any progress (waiting for the
    /// oldest job when no input could be taken); with [`EndOp::Flush`] and
    /// [`EndOp::End`] when done or the output is full.
    ///
    /// # Errors
    ///
    /// [`Error::StageWrong`] for input or [`EndOp::Continue`] after the
    /// frame's end was asked, [`Error::SrcSizeWrong`] for more or less input
    /// than the pledged size, or a job's error. The frame is then
    /// abandoned.
    pub(crate) fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        op: EndOp,
    ) -> Result<CompressProgress> {
        self.run(input, Out::Slice(output), op)
    }

    /// [`compress`](Self::compress) with [`EndOp::End`], appending the
    /// output to `out` (as much as it takes: the frame is complete).
    pub(crate) fn compress_end_to_vec(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let p = self.run(input, Out::Vec(out), EndOp::End)?;
        debug_assert!(p.read == input.len() && p.remaining == 0);
        Ok(())
    }

    fn run(&mut self, input: &[u8], mut output: Out<'_>, op: EndOp) -> Result<CompressProgress> {
        let mut ip = 0;
        let mut opos = 0;
        let remaining = loop {
            let (ip0, opos0) = (ip, opos);
            let r = match self.generic(input, &mut ip, &mut output, &mut opos, op) {
                Ok(r) => r,
                Err(e) => {
                    self.abandon();
                    return Err(e);
                }
            };
            if op == EndOp::Continue {
                // only some progress is required
                if ip != ip0 || opos != opos0 || ip == input.len() || output.room(opos) == 0 {
                    break r;
                }
            } else if r == 0 || output.room(opos) == 0 {
                // maximal progress
                break r;
            }
        };
        if op == EndOp::End && remaining == 0 {
            // the frame is complete
            self.frame = None;
        }
        Ok(CompressProgress {
            read: ip,
            written: opos,
            remaining,
        })
    }

    /// `ZSTDMT_compressStream_generic()`.
    fn generic(
        &mut self,
        input: &[u8],
        ip: &mut usize,
        output: &mut Out<'_>,
        opos: &mut usize,
        mut op: EndOp,
    ) -> Result<usize> {
        if self.frame_ended && (op == EndOp::Continue || *ip < input.len()) {
            // current frame being ended: only flush/end are allowed
            return Err(Error::StageWrong);
        }
        let mut forward_progress = false;
        // fill input buffer
        if self.ready.is_none() && *ip < input.len() {
            if self.in_buf.is_none() {
                self.get_input_range(input[*ip..].as_ptr() as usize);
            }
            let filled = self.filled();
            let to_load = (input.len() - *ip).min(self.target - filled);
            self.consumed += to_load as u64;
            if self.pledged.is_some_and(|p| self.consumed > p) {
                return Err(Error::SrcSizeWrong);
            }
            let loaded = &input[*ip..*ip + to_load];
            if let Some(h) = &mut self.xxh {
                h.update(loaded);
            }
            let b = self.in_buf.as_mut().expect("input range");
            b.extend_from_slice(loaded);
            *ip += to_load;
            forward_progress = to_load > 0;
        }
        if *ip < input.len() && op == EndOp::End {
            // can't end yet because the input is not fully consumed
            op = EndOp::Flush;
        }
        let filled = self.filled();
        if self.ready.is_some()
            || filled >= self.target
            || (op != EndOp::Continue && filled > 0)
            || (op == EndOp::End && !self.frame_ended)
        {
            self.create_job(op)?;
        }
        // check for potential compressed data ready to be flushed
        let r = self.flush_produced(output, opos, !forward_progress, op)?;
        Ok(if *ip < input.len() { r.max(1) } else { r })
    }

    /// `ZSTDMT_tryGetInputRange()`, which here always succeeds: the next
    /// input buffer, starting with the prefix, at the round buffer's
    /// position (back at the start, after the prefix, when the rest of the
    /// round buffer is too small for a job). The input is placed at
    /// [`COPY_SKEW`] from `src` (the address it is loaded from) mod 4 KiB.
    fn get_input_range(&mut self, src: usize) {
        let prefix = self.prev.as_ref().map_or(0, |p| p.2);
        if self.ring_cap - self.ring_pos < self.target {
            self.ring_pos = prefix;
        }
        self.in_ring_pos = self.ring_pos;
        // room for a job, or for the rest of the pledged input
        let rest = self
            .pledged
            .map_or(u64::MAX, |p| p.saturating_sub(self.consumed));
        let size = (self.target as u64).min(rest.max(1)) as usize;
        let mut b = self.bufs.input(PAGE + prefix + size);
        let base = src
            .wrapping_add(COPY_SKEW)
            .wrapping_sub(b.as_ptr() as usize + prefix)
            % PAGE;
        b.resize(base, 0);
        // the prefix is copied: the previous job's buffer is no longer
        // needed here
        if let Some((data, start, len)) = self.prev.take() {
            b.extend_from_slice(&data[start..start + len]);
            self.bufs.put_shared_input(data);
        }
        self.in_buf = Some(b);
        self.in_base = base;
        self.in_prefix = base + prefix;
    }

    /// `ZSTDMT_createCompressionJob()`: makes the loaded input a job (the
    /// last of the frame for [`EndOp::End`]), then posts the ready job if
    /// the table has room.
    fn create_job(&mut self, op: EndOp) -> Result<()> {
        if self.ready.is_none() {
            let end = op == EndOp::End;
            if end && self.pledged.is_some_and(|p| self.consumed != p) {
                return Err(Error::SrcSizeWrong);
            }
            let src_size = self.filled();
            if end && self.next_job > 0 {
                // several jobs: the checksum follows the last one's output
                self.trailer = self.xxh.as_ref().map(|h| h.digest() as u32);
            }
            if src_size == 0 && self.next_job > 0 {
                // a last empty block ends the frame
                debug_assert!(end);
                let mut v = vec![1, 0, 0];
                if let Some(c) = self.trailer.take() {
                    v.extend_from_slice(&c.to_le_bytes());
                }
                self.ready = Some(Ready::Done(v));
                self.in_buf = None;
                self.prev = None;
                self.frame_ended = true;
                self.next_job += 1;
            } else {
                let frame = Arc::clone(self.frame.as_ref().expect("frame started"));
                let data = Arc::new(self.in_buf.take().unwrap_or_default());
                let (base, prefix) = (self.in_base, self.in_prefix);
                // Update the round buffer pos and clear the input buffer
                self.ring_pos += src_size;
                if end {
                    self.prev = None;
                    self.frame_ended = true;
                } else {
                    // the prefix of the next job
                    let n = src_size.min(self.prefix_size);
                    self.prev = Some((Arc::clone(&data), prefix + src_size - n, n));
                }
                self.ready = Some(Ready::Work(Job {
                    frame,
                    id: self.next_job,
                    data,
                    base,
                    prefix,
                    ring_pos: self.in_ring_pos,
                    last: end,
                    out_cap: self.out_cap,
                    slot: Arc::new(Slot::default()),
                }));
                self.in_base = 0;
                self.in_prefix = 0;
                self.next_job += 1;
            }
        }
        self.post_ready();
        Ok(())
    }

    /// Posts the ready job, if the job table has room.
    fn post_ready(&mut self) {
        if self.jobs.len() >= self.max_jobs {
            return;
        }
        match self.ready.take() {
            None => {}
            Some(Ready::Done(v)) => {
                let slot = Slot::default();
                let n = v.len();
                slot.set(Outcome::Done(v, n));
                self.jobs.push_back(Arc::new(slot));
            }
            Some(Ready::Work(job)) => {
                self.jobs.push_back(Arc::clone(&job.slot));
                match &self.pool.tx {
                    Some(tx) => {
                        if let Err(mpsc::SendError(job)) = tx.send(job) {
                            execute(&mut self.inline_fc, job);
                        }
                    }
                    None => execute(&mut self.inline_fc, job),
                }
            }
        }
    }

    /// `ZSTDMT_flushProduced()`: writes out what the oldest job produced
    /// (waiting for it with `block`), moving to the next job once it is
    /// written out. Returns the bytes left to write (1 when unknown).
    fn flush_produced(
        &mut self,
        output: &mut Out<'_>,
        opos: &mut usize,
        block: bool,
        op: EndOp,
    ) -> Result<usize> {
        if !self.flushing
            && let Some(slot) = self.jobs.front()
        {
            match slot.take(block) {
                None => return Ok(1), // current job not completely compressed
                Some(Outcome::Done(mut v, mut n)) => {
                    if self.frame_ended
                        && self.jobs.len() == 1
                        && self.ready.is_none()
                        && let Some(c) = self.trailer.take()
                    {
                        // the last job: add the frame checksum
                        if v.len() < n + 4 {
                            v.resize(n + 4, 0);
                        }
                        v[n..n + 4].copy_from_slice(&c.to_le_bytes());
                        n += 4;
                    }
                    self.out = v;
                    self.out_len = n;
                    self.out_pos = 0;
                    self.flushing = true;
                }
                Some(Outcome::Failed(e)) => return Err(e),
                Some(Outcome::Panicked(p)) => {
                    self.abandon();
                    std::panic::resume_unwind(p);
                }
            }
        }
        if self.flushing {
            let n = (self.out_len - self.out_pos).min(output.room(*opos));
            output.write(opos, &self.out[self.out_pos..self.out_pos + n]);
            self.out_pos += n;
            if self.out_pos < self.out_len {
                return Ok(self.out_len - self.out_pos);
            }
            // job fully written out: free its position (and buffer)
            self.flushing = false;
            self.bufs
                .put(&self.bufs.outputs, core::mem::take(&mut self.out));
            self.jobs.pop_front();
            self.post_ready();
        }
        if !self.jobs.is_empty() || self.ready.is_some() || self.filled() > 0 {
            return Ok(1);
        }
        if op == EndOp::End {
            // is the frame complete?
            return Ok(usize::from(!self.frame_ended));
        }
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enc::params::get_cparams_internal;

    #[test]
    fn job_and_overlap_sizes() {
        let unknown = u64::MAX;
        let cp = get_cparams_internal(
            3,
            unknown,
            0,
            super::super::params::CParamMode::NoAttachDict,
        );
        // level 3: windowLog 21, default overlap log 6
        assert_eq!(1usize << target_job_log(&cp, false), 8 << 20);
        assert_eq!(overlap_size(0, &cp, false), 1 << (21 - 3));
        assert_eq!(overlap_size(9, &cp, false), 1 << 21);
        assert_eq!(overlap_size(1, &cp, false), 0);
        let mut p = MtParams::default();
        p.set_job_size(1);
        assert_eq!(p.job_size, JOBSIZE_MIN);
        p.set_job_size(0);
        assert_eq!(p.job_size, 0);
        assert!(!p.applies(None));
        p.set_workers(1000);
        assert_eq!(p.workers, NBWORKERS_MAX);
        assert!(p.applies(None));
        assert!(!p.applies(Some(JOBSIZE_MIN as u64)));
        assert!(p.applies(Some(JOBSIZE_MIN as u64 + 1)));
        // libzstd's job table, twice as large for small jobs
        const MB: usize = 1 << 20;
        assert_eq!(job_table_size(1, 2 * MB), 4);
        assert_eq!(job_table_size(4, 2 * MB), 8);
        assert_eq!(job_table_size(8, 2 * MB), 32);
        assert_eq!(job_table_size(12, 2 * MB), 32);
        assert_eq!(job_table_size(16, 2 * MB), 64);
        assert_eq!(job_table_size(12, 8 * MB), 32);
        assert_eq!(job_table_size(16, 8 * MB), 32);
        assert_eq!(job_table_size(12, 32 * MB), 16);
        assert_eq!(job_table_size(256, 2 * MB), 512);
    }

    #[test]
    fn send_sync() {
        fn check<T: Send + Sync>() {}
        check::<MtCtx>();
        check::<crate::Compressor>();
        check::<crate::StreamEncoder>();
        check::<crate::Encoder<Vec<u8>>>();
    }

    #[test]
    fn jobs_run_in_the_calling_thread_without_workers() {
        // a pool whose threads could not be started: jobs run inline, with
        // the same output
        let data: Vec<u8> = (0..3_000_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 27) as u8 ^ (i / 1000) as u8)
            .collect();
        let mut p = MtParams::default();
        p.set_workers(2);
        p.set_job_size(JOBSIZE_MIN);
        let cp = get_cparams_internal(
            3,
            data.len() as u64,
            0,
            super::super::params::CParamMode::NoAttachDict,
        );
        let mut outs = Vec::new();
        for inline in [false, true] {
            let mut m = MtCtx::new();
            let fp = FrameParams {
                checksum: true,
                ..FrameParams::default()
            };
            m.init_frame(&p, cp, false, None, Some(data.len() as u64), fp)
                .unwrap();
            if inline {
                m.pool = Pool {
                    tx: None,
                    threads: Vec::new(),
                    size: 2,
                };
            }
            let mut out = Vec::new();
            m.compress_end_to_vec(&data, &mut out).unwrap();
            assert_eq!(crate::decompress(&out).unwrap(), data);
            outs.push(out);
        }
        assert_eq!(outs[0], outs[1]);
    }

    #[test]
    fn frame_sizes_without_overflow() {
        use super::super::params::{CParamMode, ldm_enabled};
        const MB: usize = 1 << 20;
        let unknown = u64::MAX;
        // level 22, size unknown: windowLog 27, LDM on, 512 MiB jobs (on
        // 32-bit targets too: ZSTDMT_JOBLOG_MAX is 29 there), the whole
        // 128 MiB window as overlap
        let cp22 = get_cparams_internal(22, unknown, 0, CParamMode::NoAttachDict);
        assert!(ldm_enabled(&cp22));
        let cp3 = get_cparams_internal(3, unknown, 0, CParamMode::NoAttachDict);
        let mut p = MtParams::default();
        p.set_workers(8);
        let mut big = p;
        big.set_job_size(usize::MAX); // clamped to ZSTDMT_JOBSIZE_MAX
        assert_eq!(big.job_size, JOBSIZE_MAX);
        big.set_job_size(512 * MB);
        assert_eq!(big.job_size, 512 * MB);
        let small = |workers: u32| {
            let mut s = MtParams::default();
            s.set_workers(workers);
            s
        };
        // ordinary sizes, the same everywhere
        assert_eq!(
            FrameSizes::new(&small(4), &cp3, false),
            Ok(FrameSizes {
                target: 8 * MB,
                prefix_size: 256 << 10,
                out_cap: compress_bound(8 * MB),
                ring_cap: 4 * 8 * MB + 3 * 8 * MB,
            })
        );
        #[cfg(target_pointer_width = "64")]
        {
            // roundBuff.capacity = max(window, 8 jobs) + 3 jobs
            let s = FrameSizes::new(&p, &cp22, true).unwrap();
            assert_eq!((s.target, s.prefix_size), (512 * MB, 128 * MB));
            assert_eq!(s.ring_cap, 11 * 512 * MB);
            let s = FrameSizes::new(&big, &cp3, false).unwrap();
            assert_eq!(s.ring_cap, 11 * 512 * MB);
            assert_eq!(s.out_cap, compress_bound(512 * MB));
            let s = FrameSizes::new(&small(NBWORKERS_MAX), &cp22, true).unwrap();
            assert_eq!(s.ring_cap, (256 + 3) * 512 * MB);
        }
        #[cfg(target_pointer_width = "32")]
        {
            // 8 * 512 MiB overflows usize; 1 worker needs a 2 GiB round
            // buffer, over isize::MAX
            for workers in [1, 4, 5, 8, NBWORKERS_MAX] {
                assert_eq!(
                    FrameSizes::new(&small(workers), &cp22, true),
                    Err(Error::MemoryAllocation)
                );
                let mut b = big;
                b.set_workers(workers);
                assert_eq!(
                    FrameSizes::new(&b, &cp3, false),
                    Err(Error::MemoryAllocation)
                );
            }
            // the largest that fits: 256 MiB jobs, 4 workers: 7 * 256 MiB
            let mut b = small(4);
            b.set_job_size(256 * MB);
            let s = FrameSizes::new(&b, &cp3, false).unwrap();
            assert_eq!(s.ring_cap, 7 * 256 * MB);
            b.set_workers(5);
            assert_eq!(
                FrameSizes::new(&b, &cp3, false),
                Err(Error::MemoryAllocation)
            );
        }
    }

    // The whole path on 32-bit targets: an error before any allocation or
    // thread, from every API, and the context still works afterwards.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn large_jobs_fail_on_32_bit() {
        let data = vec![7u8; JOBSIZE_MIN + 1];
        let mut c = crate::Compressor::new(3);
        c.set_workers(8);
        c.set_job_size(512 << 20);
        assert_eq!(c.compress(&data), Err(Error::MemoryAllocation));
        let mut dst = vec![0u8; compress_bound(data.len())];
        assert_eq!(
            c.compress_to_slice(&data, &mut dst),
            Err(Error::MemoryAllocation)
        );
        c.set_job_size(0);
        assert_eq!(
            crate::decompress(&c.compress(&data).unwrap()).unwrap(),
            data
        );

        let mut e = crate::StreamEncoder::new(22);
        e.set_workers(8).unwrap();
        let mut out = vec![0u8; 1 << 16];
        assert_eq!(
            e.compress(&data, &mut out, EndOp::Continue),
            Err(Error::MemoryAllocation)
        );
        let mut w = crate::Encoder::new(Vec::new(), 22);
        w.set_workers(8).unwrap();
        let err = std::io::Write::write_all(&mut w, &data).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::OutOfMemory);
    }
}
