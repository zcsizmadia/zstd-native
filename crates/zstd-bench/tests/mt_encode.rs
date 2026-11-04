//! Multithreaded compression must produce exactly libzstd's bytes for the
//! same worker count, level, job size and overlap log
//! (`ZSTD_c_nbWorkers`, `ZSTD_c_jobSize`, `ZSTD_c_overlapLog`):
//! - one-shot (`Compressor` with workers vs `ZSTD_compress2`), every
//!   strategy, inputs from empty to tens of MiB over many jobs, every
//!   overlap log;
//! - streaming (`StreamEncoder` / `Encoder` with workers vs
//!   `ZSTD_compressStream2`): chunked input, flushes (including ones that
//!   leave a job's prefix under 8 bytes), unknown and pledged sizes, `End`
//!   with input, small output buffers;
//! - dictionaries (raw content and trained, loaded and prepared);
//! - long distance matching, forced on with small windows so that the round
//!   buffer wraps;
//! - contexts reused across frames, with the worker count changing.
//!
//! Small job sizes (libzstd's minimum, 512 KiB) give many jobs cheaply.

use std::io::Write;

use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd::zstd_safe::{CCtx, CDict, CParameter, InBuffer, OutBuffer};
use zstd_bench::{Rng, log_corpus, random_corpus, source_corpus};
use zstd_native::{Compressor, Dictionary, EncoderDictionary, EndOp, StreamEncoder};

const KB: usize = 1 << 10;
const MB: usize = 1 << 20;

/// An input with long repeats: chunks of the corpora and noise, each a new
/// one or (often) a copy of an earlier chunk, slightly changed, up to
/// `reach` bytes back.
fn input(n: usize, reach: usize, seed: u64) -> Vec<u8> {
    let src = source_corpus();
    let log = log_corpus(1 << 20);
    let mut r = Rng::new(seed);
    let mut v: Vec<u8> = Vec::with_capacity(n + (1 << 16));
    while v.len() < n {
        let len = 1000 + r.below(60_000) as usize;
        let k = r.below(10);
        if k < 4 && v.len() > len + 1000 {
            let back = (r.below(reach.min(v.len() - len) as u64) as usize).max(1) + len;
            let at = v.len() - back.min(v.len());
            v.extend_from_within(at..at + len);
            for _ in 0..r.below(4) {
                let p = v.len() - 1 - r.below(len as u64) as usize;
                v[p] = v[p].wrapping_add(1 + r.below(255) as u8);
            }
        } else if k < 8 {
            let at = r.below((src.len() - len) as u64) as usize;
            v.extend_from_slice(&src[at..at + len]);
        } else if k < 9 {
            let at = r.below((log.len() - len) as u64) as usize;
            v.extend_from_slice(&log[at..at + len]);
        } else {
            v.extend_from_slice(&random_corpus(len.min(5000)));
        }
    }
    v.truncate(n);
    v
}

/// A dictionary for both encoders.
#[derive(Clone, Copy)]
enum Dict<'a> {
    None,
    /// `ZSTD_CCtx_loadDictionary`
    Loaded(&'a [u8]),
    /// `ZSTD_createCDict(dict, level)` + `ZSTD_CCtx_refCDict`
    Prepared(&'a [u8], i32),
}

/// The parameters of a frame.
#[derive(Clone, Copy, Debug)]
struct Params {
    level: i32,
    workers: u32,
    job_size: usize,
    overlap_log: u32,
    window_log: u32,
    ldm: bool,
}

impl Params {
    fn new(level: i32, workers: u32, job_size: usize, overlap_log: u32) -> Self {
        Params {
            level,
            workers,
            job_size,
            overlap_log,
            window_log: 0,
            ldm: false,
        }
    }
}

fn set_c(c: &mut CCtx<'_>, p: &Params) {
    c.set_parameter(CParameter::CompressionLevel(p.level))
        .unwrap();
    c.set_parameter(CParameter::NbWorkers(p.workers)).unwrap();
    c.set_parameter(CParameter::JobSize(p.job_size as u32))
        .unwrap();
    c.set_parameter(CParameter::OverlapSizeLog(p.overlap_log))
        .unwrap();
    c.set_parameter(CParameter::WindowLog(p.window_log))
        .unwrap();
    c.set_parameter(CParameter::EnableLongDistanceMatching(p.ldm))
        .unwrap();
}

fn set_native(e: &mut StreamEncoder, p: &Params) {
    e.set_level(p.level);
    e.set_workers(p.workers).unwrap();
    e.set_job_size(p.job_size);
    e.set_overlap_log(p.overlap_log);
    e.set_window_log(p.window_log);
    e.set_long_distance_matching(p.ldm);
}

fn c_context(dict: Dict<'_>) -> CCtx<'static> {
    let mut c = CCtx::create();
    match dict {
        Dict::None => {}
        Dict::Loaded(d) => {
            c.load_dictionary(d).unwrap();
        }
        Dict::Prepared(d, level) => {
            // leaked: the context references it ('static)
            let cd: &'static CDict<'static> = Box::leak(Box::new(CDict::create(d, level)));
            c.ref_cdict(cd).unwrap();
        }
    }
    c
}

fn native_encoder(dict: Dict<'_>) -> StreamEncoder {
    match dict {
        Dict::None => StreamEncoder::new(3),
        Dict::Loaded(d) => StreamEncoder::with_dictionary(3, d).unwrap(),
        Dict::Prepared(d, level) => {
            StreamEncoder::with_prepared_dictionary(&EncoderDictionary::new(d, level).unwrap())
        }
    }
}

fn directive(op: EndOp) -> ZSTD_EndDirective {
    match op {
        EndOp::Continue => ZSTD_EndDirective::ZSTD_e_continue,
        EndOp::Flush => ZSTD_EndDirective::ZSTD_e_flush,
        EndOp::End => ZSTD_EndDirective::ZSTD_e_end,
    }
}

/// One frame: parameters, pledged size, and the calls (input length,
/// operation), each repeated until its input is consumed and, for flush and
/// end, nothing remains; with output buffers of `out_size` bytes.
#[derive(Clone, Debug)]
struct Frame {
    p: Params,
    pledged: Option<u64>,
    ops: Vec<(usize, EndOp)>,
    out_size: usize,
}

impl Frame {
    /// `n` bytes in `chunk`-byte `Continue` calls, a `Flush` after every
    /// `flush_every` of them (0: never), then an `End` without input.
    fn streamed(p: Params, n: usize, chunk: usize, flush_every: usize) -> Self {
        let mut ops = Vec::new();
        let mut left = n;
        let mut i = 0;
        while left > 0 {
            let k = chunk.min(left);
            left -= k;
            i += 1;
            ops.push((k, EndOp::Continue));
            if flush_every != 0 && i % flush_every == 0 {
                ops.push((0, EndOp::Flush));
            }
        }
        ops.push((0, EndOp::End));
        Frame {
            p,
            pledged: None,
            ops,
            out_size: 1 << 17,
        }
    }

    fn total(&self) -> usize {
        self.ops.iter().map(|o| o.0).sum()
    }
}

/// Runs `frames` one after the other on `data` with libzstd (one context).
fn run_c(frames: &[Frame], data: &[u8], dict: Dict<'_>) -> Vec<Vec<u8>> {
    let mut c = c_context(dict);
    let mut outs = Vec::new();
    let mut pos = 0;
    for f in frames {
        set_c(&mut c, &f.p);
        if f.pledged.is_some() {
            c.set_pledged_src_size(f.pledged).unwrap();
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; f.out_size];
        for &(len, op) in &f.ops {
            let piece = &data[pos..pos + len];
            pos += len;
            let mut input = InBuffer::around(piece);
            loop {
                let mut output = OutBuffer::around(&mut buf[..]);
                let remaining = c
                    .compress_stream2(&mut output, &mut input, directive(op))
                    .unwrap_or_else(|e| panic!("libzstd: {}", zstd::zstd_safe::get_error_name(e)));
                let n = output.pos();
                out.extend_from_slice(&buf[..n]);
                if input.pos() == piece.len() && (op == EndOp::Continue || remaining == 0) {
                    break;
                }
            }
        }
        outs.push(out);
    }
    outs
}

/// As [`run_c`], with zstd-native's [`StreamEncoder`] (`enc` if given).
fn run_native(
    frames: &[Frame],
    data: &[u8],
    dict: Dict<'_>,
    enc: Option<&mut StreamEncoder>,
) -> Vec<Vec<u8>> {
    let mut own;
    let enc = match enc {
        Some(e) => e,
        None => {
            own = native_encoder(dict);
            &mut own
        }
    };
    let mut outs = Vec::new();
    let mut pos = 0;
    for f in frames {
        set_native(enc, &f.p);
        if f.pledged.is_some() {
            enc.set_pledged_src_size(f.pledged).unwrap();
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; f.out_size];
        for &(len, op) in &f.ops {
            let mut piece = &data[pos..pos + len];
            pos += len;
            loop {
                let p = enc
                    .compress(piece, &mut buf, op)
                    .unwrap_or_else(|e| panic!("zstd-native: {e}"));
                piece = &piece[p.read..];
                out.extend_from_slice(&buf[..p.written]);
                if piece.is_empty() && (op == EndOp::Continue || p.remaining == 0) {
                    break;
                }
            }
        }
        assert!(enc.at_frame_boundary());
        outs.push(out);
    }
    outs
}

fn decoder(dict: Dict<'_>) -> zstd_native::Decompressor {
    match dict {
        Dict::None => zstd_native::Decompressor::new(),
        Dict::Loaded(d) | Dict::Prepared(d, _) => {
            zstd_native::Decompressor::with_dictionary(Dictionary::new(d).unwrap())
        }
    }
}

#[track_caller]
fn compare(what: &str, got: &[u8], want: &[u8]) {
    if got != want {
        let first = got
            .iter()
            .zip(want)
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(want.len()));
        panic!(
            "{what}: output differs from libzstd at byte {first} (sizes {} vs {})",
            got.len(),
            want.len()
        );
    }
}

/// Compresses `data` frame by frame with both, compares, and round trips
/// every frame through the native decoder.
#[track_caller]
fn check(name: &str, frames: &[Frame], data: &[u8], dict: Dict<'_>) {
    let want = run_c(frames, data, dict);
    let got = run_native(frames, data, dict, None);
    let mut d = decoder(dict);
    let mut pos = 0;
    for (k, f) in frames.iter().enumerate() {
        compare(&format!("{name} frame {k} {:?}", f.p), &got[k], &want[k]);
        let n = f.total();
        assert!(
            d.decompress(&got[k]).unwrap() == data[pos..pos + n],
            "{name} frame {k}: round trip"
        );
        pos += n;
    }
}

/// libzstd's `ZSTD_compress2` with the parameters.
fn c_one_shot(p: &Params, data: &[u8], dict: Dict<'_>) -> Vec<u8> {
    let mut c = c_context(dict);
    set_c(&mut c, p);
    let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
    c.compress2(&mut out, data)
        .unwrap_or_else(|e| panic!("libzstd: {}", zstd::zstd_safe::get_error_name(e)));
    out
}

fn native_compressor(p: &Params, dict: Dict<'_>) -> Compressor {
    let mut c = match dict {
        Dict::None => Compressor::new(p.level),
        Dict::Loaded(d) => Compressor::with_dictionary(p.level, d).unwrap(),
        Dict::Prepared(d, level) => {
            Compressor::with_prepared_dictionary(&EncoderDictionary::new(d, level).unwrap())
        }
    };
    c.set_level(p.level);
    c.set_workers(p.workers);
    c.set_job_size(p.job_size);
    c.set_overlap_log(p.overlap_log);
    assert!(p.window_log == 0 && !p.ldm, "not settable on a Compressor");
    c
}

/// One-shot with both (and `compress_to_slice` too, with `to_slice`).
#[track_caller]
fn check_one_shot(name: &str, p: &Params, data: &[u8], dict: Dict<'_>, to_slice: bool) {
    let want = c_one_shot(p, data, dict);
    let mut c = native_compressor(p, dict);
    let got = c.compress(data).unwrap();
    compare(&format!("{name} {p:?} ({} bytes)", data.len()), &got, &want);
    assert!(
        decoder(dict).decompress(&got).unwrap() == data,
        "{name}: round trip"
    );
    if to_slice {
        let mut dst = vec![0u8; zstd_native::compress_bound(data.len())];
        let n = c.compress_to_slice(data, &mut dst).unwrap();
        compare(&format!("{name} {p:?} to slice"), &dst[..n], &want);
        // too small
        if n > 0 {
            let mut small = vec![0u8; n - 1];
            assert_eq!(
                c.compress_to_slice(data, &mut small),
                Err(zstd_native::Error::OutputTooSmall)
            );
        }
        // and the compressor still works after the error
        compare(
            &format!("{name} {p:?} after error"),
            &c.compress(data).unwrap(),
            &want,
        );
    }
}

#[test]
fn one_shot_every_strategy() {
    let data = input(3 * MB + 12_345, 1 << 20, 1);
    let levels = [-5, -1, 1, 2, 3, 4, 5, 6, 7, 9, 12, 13, 16, 17, 19, 22];
    for (i, &level) in levels.iter().enumerate() {
        let workers = [1, 2, 4][i % 3];
        let overlap_log = (i % 10) as u32;
        // the slow levels on less input
        let n = if level >= 16 { 1_200_000 } else { data.len() };
        let p = Params::new(level, workers, 512 * KB, overlap_log);
        check_one_shot("one shot", &p, &data[..n], Dict::None, level == 3);
        // the automatic job size and overlap: one job, or a few at low levels
        let p = Params::new(level, workers, 0, 0);
        check_one_shot("one shot auto", &p, &data[..n], Dict::None, false);
    }
}

#[test]
fn one_shot_sizes() {
    let data = input(26 * MB, 8 * MB, 2);
    let job = 512 * KB;
    let sizes = [
        0,
        1,
        1000,
        job - 1,
        job, // still single-threaded
        job + 1,
        2 * job - 1,
        2 * job,
        2 * job + 1,
        3 * job,
        3 * job + 7,
        5 * MB + 3,
    ];
    for (i, &n) in sizes.iter().enumerate() {
        let p = Params::new([1, 3, 6][i % 3], [1, 2, 3][i % 3], job, 0);
        check_one_shot("sizes", &p, &data[..n], Dict::None, i % 4 == 0);
    }
    // tens of MiB, with the automatic job size (2 MiB at level 1: 13 jobs)
    check_one_shot("large", &Params::new(1, 4, 0, 0), &data, Dict::None, false);
    // and with the minimum job size
    check_one_shot(
        "large",
        &Params::new(-3, 3, job, 2),
        &data,
        Dict::None,
        false,
    );
}

#[test]
fn overlap_logs() {
    let data = input(2 * MB + 99, 1 << 20, 3);
    for level in [3, 7, 13] {
        for ov in 0..=9 {
            let p = Params::new(level, 2, 512 * KB, ov);
            check_one_shot("overlap", &p, &data, Dict::None, false);
        }
    }
    for ov in [0, 1, 5, 9] {
        let p = Params::new(17, 3, 512 * KB, ov);
        check_one_shot("overlap", &p, &data[..1_100_000], Dict::None, false);
    }
}

#[test]
fn output_does_not_depend_on_worker_count() {
    // without LDM, only the jobs matter, not who compresses them
    let data = input(4 * MB, 1 << 20, 4);
    let p = Params::new(5, 1, 512 * KB, 4);
    let want = c_one_shot(&p, &data, Dict::None);
    for workers in [1, 2, 3, 8, 17] {
        let mut c = native_compressor(&Params { workers, ..p }, Dict::None);
        compare(
            &format!("{workers} workers"),
            &c.compress(&data).unwrap(),
            &want,
        );
    }
}

#[test]
fn streaming() {
    let data = input(5 * MB, 2 * MB, 5);
    let job = 512 * KB;
    let s = Frame::streamed;
    let p = Params::new;
    // small output buffers
    let mut small_out = s(p(5, 2, job, 0), 2 * MB, 65_536, 5);
    small_out.out_size = 1000;
    let frames = [
        // unknown size, chunks of every size, with and without flushes
        s(p(3, 2, job, 0), 3 * MB, 100_000, 0),
        s(p(1, 3, job, 3), 2 * MB, 7_777, 40),
        s(p(6, 2, job, 7), 2 * MB + 5, 300_001, 2),
        s(p(12, 4, job, 0), 1_500_000, 1 << 20, 1),
        s(p(-2, 1, job, 9), 3 * MB, 3 * MB, 0),
        // the automatic job size (8 MiB at level 3: one job), all at once
        s(p(3, 2, 0, 0), 5 * MB, 5 * MB, 0),
        // input ending exactly at job boundaries
        s(p(4, 2, job, 0), 3 * job, job, 0),
        s(p(4, 2, job, 0), 2 * job, 2 * job, 0),
        small_out,
        // empty frames, and tiny ones (multithreaded: the size is unknown)
        s(p(3, 2, job, 0), 0, 1, 0),
        s(p(3, 2, job, 0), 5, 1, 0),
    ];
    for f in &frames {
        let n = f.total();
        check("streaming", std::slice::from_ref(f), &data[..n], Dict::None);
    }
}

#[test]
fn streaming_flush_edge_cases() {
    let data = input(4 * MB, MB, 6);
    let job = 512 * KB;
    let p = Params::new(3, 2, job, 0);
    let mut frames = Vec::new();
    // flushes after a few bytes: the next job's prefix is under 8 bytes
    // (not loaded) or just 8
    for tiny in [1, 5, 7, 8, 9, 100] {
        frames.push(Frame {
            p,
            pledged: None,
            ops: vec![
                (700_000, EndOp::Continue),
                (tiny, EndOp::Flush),
                (900_000, EndOp::Flush),
                (tiny, EndOp::Flush),
                (300_000, EndOp::Continue),
                (0, EndOp::End),
            ],
            out_size: 1 << 17,
        });
    }
    // End with input (more than one job of it), flush with no input,
    // repeated flushes
    frames.push(Frame {
        p: Params::new(7, 3, job, 0),
        pledged: None,
        ops: vec![
            (0, EndOp::Flush),
            (100_000, EndOp::Continue),
            (0, EndOp::Flush),
            (0, EndOp::Flush),
            (1_700_000, EndOp::End),
        ],
        out_size: 1 << 17,
    });
    // End with input only: the size becomes known (multithreaded above
    // 512 KiB)
    frames.push(Frame {
        p,
        pledged: None,
        ops: vec![(job, EndOp::End)],
        out_size: 1 << 17,
    });
    frames.push(Frame {
        p,
        pledged: None,
        ops: vec![(job + 1, EndOp::End)],
        out_size: 1 << 17,
    });
    for f in &frames {
        let n = f.total();
        check("flushes", std::slice::from_ref(f), &data[..n], Dict::None);
    }
}

#[test]
fn pledged_sizes() {
    let data = input(3 * MB, MB, 7);
    let job = 512 * KB;
    for (n, chunk) in [
        (job, 100_000), // single-threaded
        (job + 1, 100_000),
        (2 * MB + 17, 250_000),
        (3 * MB, 3 * MB),
    ] {
        let mut f = Frame::streamed(Params::new(3, 2, job, 0), n, chunk, 0);
        f.pledged = Some(n as u64);
        check("pledged", std::slice::from_ref(&f), &data[..n], Dict::None);
    }
    // pledged and flushed
    let mut f = Frame::streamed(Params::new(9, 3, job, 3), 2 * MB, 200_000, 3);
    f.pledged = Some(2 * MB as u64);
    check(
        "pledged flushed",
        std::slice::from_ref(&f),
        &data,
        Dict::None,
    );
}

#[test]
fn wrong_pledged_size() {
    let data = input(2 * MB, MB, 8);
    let mut buf = vec![0u8; 1 << 17];
    for (pledged, n) in [(MB, MB + 1), (MB, MB - 1), (MB, 2 * MB)] {
        let mut e = StreamEncoder::new(3);
        e.set_workers(2).unwrap();
        e.set_job_size(512 * KB);
        e.set_pledged_src_size(Some(pledged as u64)).unwrap();
        // the input in pieces, then the end
        let mut err = None;
        for (piece, op) in data[..n]
            .chunks(100_000)
            .map(|c| (c, EndOp::Continue))
            .chain([(&[][..], EndOp::End)])
        {
            let mut piece = piece;
            loop {
                match e.compress(piece, &mut buf, op) {
                    Ok(p) => {
                        piece = &piece[p.read..];
                        if piece.is_empty() && (op == EndOp::Continue || p.remaining == 0) {
                            break;
                        }
                    }
                    Err(x) => {
                        err = Some(x);
                        break;
                    }
                }
            }
            if err.is_some() {
                break;
            }
        }
        assert_eq!(err, Some(zstd_native::Error::SrcSizeWrong), "{pledged} {n}");
        assert!(e.at_frame_boundary());
        // the encoder still works
        let mut out = Vec::new();
        let mut piece = &data[..MB];
        loop {
            let p = e.compress(piece, &mut buf, EndOp::End).unwrap();
            piece = &piece[p.read..];
            out.extend_from_slice(&buf[..p.written]);
            if piece.is_empty() && p.remaining == 0 {
                break;
            }
        }
        assert!(zstd_native::decompress(&out).unwrap() == data[..MB]);
    }
}

#[test]
fn io_encoder() {
    let data = input(3 * MB, MB, 9);
    for (level, workers, chunk) in [(3, 2, 10_000), (6, 4, 1 << 20), (1, 1, 333)] {
        let mut want = zstd::stream::Encoder::new(Vec::new(), level).unwrap();
        want.multithread(workers).unwrap();
        let mut got = zstd_native::Encoder::new(Vec::new(), level);
        got.set_workers(workers).unwrap();
        for (k, piece) in data.chunks(chunk).enumerate() {
            want.write_all(piece).unwrap();
            got.write_all(piece).unwrap();
            if k % 97 == 50 {
                want.flush().unwrap();
                got.flush().unwrap();
            }
        }
        let want = want.finish().unwrap();
        let got = got.finish().unwrap();
        compare(&format!("Encoder L{level} {workers} workers"), &got, &want);
        assert!(zstd_native::decompress(&got).unwrap() == data);
    }
    // not after the first write
    let mut e = zstd_native::Encoder::new(Vec::new(), 3);
    e.write_all(b"x").unwrap();
    assert_eq!(
        e.set_workers(2).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

/// Samples to train a dictionary on.
fn trained(size: usize) -> Vec<u8> {
    let src = source_corpus();
    let log = log_corpus(2 << 20);
    let mut r = Rng::new(17);
    let mut samples = Vec::new();
    for i in 0..2000 {
        let data = if i % 2 == 0 { &src } else { &log };
        let len = 200 + r.below(2000) as usize;
        let at = r.below((data.len() - len) as u64) as usize;
        samples.push(data[at..at + len].to_vec());
    }
    zstd::dict::from_samples(&samples, size).expect("ZDICT")
}

#[test]
fn dictionaries() {
    let data = input(3 * MB, MB, 10);
    let src = source_corpus();
    let dicts: Vec<(&str, Vec<u8>)> = vec![
        ("trained", trained(32 * KB)),
        ("raw5", src[1000..1005].to_vec()),
        ("raw3000", src[50_000..53_000].to_vec()),
        ("raw300k", src[100_000..400_000].to_vec()),
    ];
    let job = 512 * KB;
    for (k, (name, d)) in dicts.iter().enumerate() {
        for (i, level) in [1, 4, 9, 16].into_iter().enumerate() {
            let n = if level >= 16 { 1_100_000 } else { 2 * MB };
            let dict = if (k + i) % 2 == 0 {
                Dict::Loaded(d)
            } else {
                Dict::Prepared(d, level)
            };
            let p = Params::new(level, 2, job, 0);
            check_one_shot(name, &p, &data[..n], dict, false);
            let f = Frame::streamed(p, n, 150_000, 4);
            check(name, std::slice::from_ref(&f), &data[..n], dict);
        }
    }
}

#[test]
fn long_distance_matching() {
    // long repeats over the whole input, a small window and job size: the
    // round buffer (window + 3 jobs) wraps several times
    let data = input(7 * MB, 6 * MB, 11);
    let job = 512 * KB;
    let cases: &[(i32, u32, u32, u32, usize)] = &[
        // level, workers, window log, overlap log, size
        (1, 2, 20, 0, 7 * MB),
        (3, 1, 20, 3, 6 * MB),
        (5, 4, 21, 0, 7 * MB),
        (7, 3, 20, 9, 5 * MB),
        (16, 2, 20, 0, 3 * MB),
        (19, 3, 20, 5, 2 * MB + 1),
    ];
    for &(level, workers, window_log, overlap_log, n) in cases {
        let mut p = Params::new(level, workers, job, overlap_log);
        p.window_log = window_log;
        p.ldm = true;
        let f = Frame::streamed(p, n, 400_000, 0);
        check("ldm", std::slice::from_ref(&f), &data[..n], Dict::None);
    }
    // pledged, flushed, with a dictionary (not loaded into the LDM table in
    // multithreaded mode)
    let mut p = Params::new(4, 2, job, 0);
    p.window_log = 20;
    p.ldm = true;
    let mut f = Frame::streamed(p, 3 * MB, 500_000, 2);
    f.pledged = Some(3 * MB as u64);
    let dict = &data[6 * MB..6 * MB + 100_000];
    check(
        "ldm dict",
        std::slice::from_ref(&f),
        &data,
        Dict::Loaded(dict),
    );
}

#[test]
fn reused_across_frames() {
    let data = input(9 * MB, 2 * MB, 12);
    let job = 512 * KB;
    let mut ldm = Params::new(3, 3, job, 0);
    ldm.window_log = 20;
    ldm.ldm = true;
    let frames = vec![
        Frame::streamed(Params::new(3, 2, job, 0), 2 * MB, 200_000, 0),
        // single-threaded in between
        Frame::streamed(Params::new(3, 0, job, 0), MB, 200_000, 0),
        Frame::streamed(Params::new(5, 4, job, 6), 2 * MB, 1 << 20, 0),
        Frame::streamed(ldm, 2 * MB, 300_000, 0),
        // small frames with workers: single-threaded once the size is known
        Frame {
            p: Params::new(7, 2, job, 0),
            pledged: None,
            ops: vec![(400_000, EndOp::End)],
            out_size: 1 << 17,
        },
        Frame::streamed(Params::new(1, 1, job, 0), 1_500_000, 70_000, 0),
    ];
    let total: usize = frames.iter().map(Frame::total).sum();
    check("reused", &frames, &data[..total], Dict::None);

    // one-shot: a compressor reused, the worker count changing
    let mut c = Compressor::new(3);
    for (i, workers) in [2, 0, 3, 3, 1].into_iter().enumerate() {
        let p = Params::new(3 + i as i32, workers, job, 0);
        c.set_level(p.level);
        c.set_workers(workers);
        c.set_job_size(job);
        let n = MB + i * 300_000;
        let want = c_one_shot(&p, &data[..n], Dict::None);
        compare(
            &format!("reused one shot {p:?}"),
            &c.compress(&data[..n]).unwrap(),
            &want,
        );
    }
}

#[test]
fn abandoned_frames() {
    // a frame abandoned in the middle (reset, or an error) does not affect
    // the next one
    let data = input(4 * MB, MB, 13);
    let p = Params::new(3, 3, 512 * KB, 0);
    let mut e = StreamEncoder::new(3);
    set_native(&mut e, &p);
    let mut buf = vec![0u8; 1000];
    let pr = e
        .compress(&data[..3 * MB], &mut buf, EndOp::Continue)
        .unwrap();
    assert!(pr.read > 0);
    e.reset();
    let f = Frame::streamed(p, 2 * MB, 300_000, 0);
    let got = run_native(std::slice::from_ref(&f), &data, Dict::None, Some(&mut e));
    let want = run_c(std::slice::from_ref(&f), &data, Dict::None);
    compare("after reset", &got[0], &want[0]);
    // workers can't change in the middle of a frame
    let _ = e.compress(&data[..MB], &mut buf, EndOp::Continue).unwrap();
    assert_eq!(e.set_workers(1), Err(zstd_native::Error::StageWrong));
    // dropping it in the middle of a frame
    drop(e);
}

#[test]
fn level22_automatic_ldm() {
    // level 22 with an unknown size: 128 MiB window, LDM on by itself, the
    // default job size (512 MiB) and overlap (the whole 128 MiB window)
    let data = input(1_200_000, MB, 14);
    let f = Frame::streamed(Params::new(22, 2, 0, 0), data.len(), 300_000, 0);
    check("level 22", std::slice::from_ref(&f), &data, Dict::None);
}
