//! Long distance matching must produce exactly libzstd's bytes:
//! - where libzstd enables it by itself (`btopt` and above with a window
//!   of at least 128 MiB: level 22 one-shot on inputs over 64 MiB, level 22
//!   streaming with an unknown size, levels 16-21 with `windowLog` 27);
//! - forced on (the hidden `StreamEncoder::set_long_distance_matching`,
//!   libzstd's `ZSTD_c_enableLongDistanceMatching`) at every level, with
//!   small windows to keep the inputs small, one-shot and streaming (with
//!   a wrapping input buffer), with dictionaries (loaded into the LDM
//!   table or not).

use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd::zstd_safe::{CCtx, CDict, CParameter, InBuffer, OutBuffer};
use zstd_bench::{Rng, log_corpus, random_corpus, source_corpus};
use zstd_native::{EncoderDictionary, EndOp, StreamEncoder};

/// An input with long repeats: chunks of the corpora and noise, each a new
/// one or (often) a copy of an earlier chunk, shifted and slightly changed,
/// up to `reach` bytes back.
fn input(n: usize, reach: usize, seed: u64) -> Vec<u8> {
    let src = source_corpus();
    let log = log_corpus(1 << 20);
    let mut r = Rng::new(seed);
    let mut v: Vec<u8> = Vec::with_capacity(n + (1 << 16));
    while v.len() < n {
        let len = 1000 + r.below(60_000) as usize;
        let k = r.below(10);
        if k < 5 && v.len() > len + 1000 {
            // a copy from up to `reach` bytes back
            let back = (r.below(reach.min(v.len() - len) as u64) as usize).max(1) + len;
            let at = v.len() - back.min(v.len());
            v.extend_from_within(at..at + len);
            // a few changed bytes
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

/// A dictionary for [`C`] and the native encoder.
#[derive(Clone, Copy)]
enum Dict<'a> {
    None,
    /// `ZSTD_CCtx_loadDictionary`
    Loaded(&'a [u8]),
    /// `ZSTD_createCDict(dict, level)` + `ZSTD_CCtx_refCDict`
    Prepared(&'a [u8], i32),
}

/// One frame: level, window log (0: the level's), LDM forced, pledged
/// size, the input pieces (with their operation) and the output buffer
/// size.
#[derive(Clone, Debug)]
struct Frame {
    level: i32,
    window_log: u32,
    ldm: bool,
    pledged: Option<u64>,
    /// `ZSTD_c_checksumFlag`
    checksum: bool,
    ops: Vec<(usize, EndOp)>,
    out_size: usize,
}

impl Frame {
    /// The whole input in one `End` call with room for all of it: libzstd's
    /// `ZSTD_compress2`.
    fn one_shot(level: i32, window_log: u32, ldm: bool, n: usize) -> Self {
        Frame {
            level,
            window_log,
            ldm,
            pledged: None,
            checksum: false,
            ops: vec![(n, EndOp::End)],
            out_size: zstd_native::compress_bound(n),
        }
    }

    /// The input in `chunk`-byte `Continue` calls (with a flush after every
    /// `flush_every` of them, if not 0), then an empty `End`: an unknown
    /// size.
    fn streamed(level: i32, window_log: u32, ldm: bool, n: usize, chunk: usize) -> Self {
        Self::streamed_flushing(level, window_log, ldm, n, chunk, 0)
    }

    fn streamed_flushing(
        level: i32,
        window_log: u32,
        ldm: bool,
        n: usize,
        chunk: usize,
        flush_every: usize,
    ) -> Self {
        let mut ops = Vec::new();
        let mut left = n;
        while left > 0 {
            let c = chunk.min(left);
            ops.push((c, EndOp::Continue));
            if flush_every != 0 && ops.len() % (flush_every + 1) == flush_every {
                ops.push((0, EndOp::Flush));
            }
            left -= c;
        }
        ops.push((0, EndOp::End));
        Frame {
            level,
            window_log,
            ldm,
            pledged: None,
            checksum: false,
            ops,
            out_size: 1 << 16,
        }
    }

    fn total(&self) -> usize {
        self.ops.iter().map(|o| o.0).sum()
    }
}

fn directive(op: EndOp) -> ZSTD_EndDirective {
    match op {
        EndOp::Continue => ZSTD_EndDirective::ZSTD_e_continue,
        EndOp::Flush => ZSTD_EndDirective::ZSTD_e_flush,
        EndOp::End => ZSTD_EndDirective::ZSTD_e_end,
    }
}

/// A libzstd context for `f` with `dict`.
fn c_context(f: &Frame, dict: Dict<'_>) -> CCtx<'static> {
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(f.level))
        .unwrap();
    if f.ldm {
        cctx.set_parameter(CParameter::EnableLongDistanceMatching(true))
            .unwrap();
    }
    if f.window_log != 0 {
        cctx.set_parameter(CParameter::WindowLog(f.window_log))
            .unwrap();
    }
    match dict {
        Dict::None => {}
        Dict::Loaded(d) => {
            cctx.load_dictionary(d).unwrap();
        }
        Dict::Prepared(d, level) => {
            // leaked: the context references it ('static)
            let cd: &'static CDict<'static> = Box::leak(Box::new(CDict::create(d, level)));
            cctx.ref_cdict(cd).unwrap();
        }
    }
    cctx
}

/// Runs the frames `frames` on `data` (one after the other) with libzstd,
/// on one context.
fn run_c(frames: &[Frame], data: &[u8], dict: Dict<'_>) -> Vec<Vec<u8>> {
    let mut cctx = c_context(&frames[0], dict);
    let mut outs = Vec::new();
    let mut pos = 0;
    for f in frames {
        cctx.set_parameter(CParameter::CompressionLevel(f.level))
            .unwrap();
        cctx.set_parameter(CParameter::EnableLongDistanceMatching(f.ldm))
            .unwrap();
        cctx.set_parameter(CParameter::WindowLog(f.window_log))
            .unwrap();
        cctx.set_parameter(CParameter::ChecksumFlag(f.checksum))
            .unwrap();
        if f.pledged.is_some() {
            cctx.set_pledged_src_size(f.pledged).unwrap();
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; f.out_size];
        for &(len, op) in &f.ops {
            let piece = &data[pos..pos + len];
            pos += len;
            let mut input = InBuffer::around(piece);
            loop {
                let mut output = OutBuffer::around(&mut buf[..]);
                let remaining = cctx
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

/// As [`run_c`], with zstd-native's [`StreamEncoder`].
fn run_native(frames: &[Frame], data: &[u8], dict: Dict<'_>) -> Vec<Vec<u8>> {
    let mut enc = match dict {
        Dict::None => StreamEncoder::new(frames[0].level),
        Dict::Loaded(d) => StreamEncoder::with_dictionary(frames[0].level, d).unwrap(),
        Dict::Prepared(d, level) => {
            StreamEncoder::with_prepared_dictionary(&EncoderDictionary::new(d, level).unwrap())
        }
    };
    let mut outs = Vec::new();
    let mut pos = 0;
    for f in frames {
        enc.set_level(f.level);
        enc.set_long_distance_matching(f.ldm);
        enc.set_window_log(f.window_log);
        enc.set_checksum(f.checksum).unwrap();
        if f.pledged.is_some() {
            enc.set_pledged_src_size(f.pledged).unwrap();
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; f.out_size];
        for &(len, op) in &f.ops {
            let mut piece = &data[pos..pos + len];
            pos += len;
            loop {
                let p = enc.compress(piece, &mut buf, op).unwrap();
                piece = &piece[p.read..];
                out.extend_from_slice(&buf[..p.written]);
                if piece.is_empty() && (op == EndOp::Continue || p.remaining == 0) {
                    break;
                }
            }
        }
        outs.push(out);
    }
    outs
}

/// Compresses `data` frame by frame with both, compares, and round trips
/// each frame through the native decoder (with `dict`'s content as the
/// decoder's dictionary).
fn check(name: &str, frames: &[Frame], data: &[u8], dict: Dict<'_>) {
    let want = run_c(frames, data, dict);
    let got = run_native(frames, data, dict);
    let mut pos = 0;
    for (k, f) in frames.iter().enumerate() {
        let (g, w) = (&got[k], &want[k]);
        if g != w {
            let first = g
                .iter()
                .zip(w)
                .position(|(a, b)| a != b)
                .unwrap_or(g.len().min(w.len()));
            panic!(
                "{name} frame {k} L{} wlog {} ldm {} ({} bytes): output differs from libzstd \
                 at byte {first} (sizes {} vs {})",
                f.level,
                f.window_log,
                f.ldm,
                f.total(),
                g.len(),
                w.len()
            );
        }
        let n = f.total();
        let back = match dict {
            Dict::None => zstd_native::decompress(g).unwrap(),
            Dict::Loaded(d) | Dict::Prepared(d, _) => {
                let mut dec = zstd_native::Decompressor::with_dictionary(
                    zstd_native::Dictionary::new(d).unwrap(),
                );
                dec.decompress(g).unwrap()
            }
        };
        assert!(back == data[pos..pos + n], "{name} frame {k}: round trip");
        pos += n;
    }
}

/// libzstd without LDM, for comparison.
fn c_plain(data: &[u8], level: i32, window_log: u32) -> usize {
    let f = Frame::one_shot(level, window_log, false, data.len());
    run_c(&[f], data, Dict::None)[0].len()
}

/// Every level with LDM forced on and a 1 MiB window (or 4 MiB): long
/// matches the regular match finders miss, the fast strategies cutting
/// blocks at the long matches.
#[test]
fn forced_every_level() {
    let data = input(3 << 20, 1 << 20, 1);
    let levels = [
        -5, -1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
    ];
    for level in levels {
        // the slow levels on less
        let n = if level >= 17 { 1 << 20 } else { data.len() };
        for wlog in [20, 22] {
            let f = Frame::one_shot(level, wlog, true, n);
            check("forced", &[f], &data, Dict::None);
        }
    }
    // LDM finds long matches the default parameters miss
    let n = 1 << 20;
    let with = run_c(&[Frame::one_shot(1, 20, true, n)], &data, Dict::None)[0].len();
    assert!(with < c_plain(&data[..n], 1, 20), "LDM should help");
}

/// Small windows (the LDM table and buckets at their minimum sizes) and
/// small inputs: a few blocks, block sizes under the window, inputs under
/// the minimum match length.
#[test]
fn forced_small() {
    let data = input(400_000, 100_000, 2);
    for level in [1, 3, 5, 7, 13, 16, 18, 19, 22] {
        for wlog in [10, 11, 14, 17] {
            for n in [0usize, 5, 31, 64, 100, 3000, 70_000, 400_000] {
                let f = Frame::one_shot(level, wlog, true, n);
                check("small", &[f], &data, Dict::None);
            }
        }
    }
}

/// Repetitive data: long overlapping matches (all zeros, short periods),
/// where the matcher skips over repetitions.
#[test]
fn forced_repetitive() {
    let mut data = vec![0u8; 500_000];
    data.extend((0..500_000u32).map(|i| (i % 7) as u8));
    data.extend(input(500_000, 200_000, 3));
    data.extend(vec![b'x'; 300_000]);
    for level in [1, 2, 3, 4, 6, 8, 15, 16, 17, 19, 22] {
        let f = Frame::one_shot(level, 20, true, data.len());
        check("repetitive", &[f], &data, Dict::None);
    }
}

/// Streaming with LDM forced and a window of 256 KB to 1 MiB: the input
/// buffer wraps (the LDM window then has an extDict), with flushes.
#[test]
fn forced_streaming_wraps() {
    let data = input(4 << 20, 1 << 20, 4);
    for level in [1, 3, 5, 6, 12, 15, 16, 18, 19, 22] {
        let n = if level >= 18 { 2 << 20 } else { data.len() };
        for (wlog, chunk) in [(18, 100_000), (20, 333_333)] {
            let f = Frame::streamed(level, wlog, true, n, chunk);
            check("stream", &[f], &data, Dict::None);
        }
        let f = Frame::streamed_flushing(level, 19, true, n.min(1 << 20), 50_000, 3);
        check("stream flush", &[f], &data, Dict::None);
    }
}

/// One context for several frames, LDM on and off, of different sizes
/// (its LDM state is reset for each).
#[test]
fn forced_reused_context() {
    let data = input(3 << 20, 1 << 20, 5);
    let frames = [
        Frame::one_shot(19, 20, true, 700_000),
        Frame::one_shot(3, 0, false, 200_000),
        Frame::streamed(16, 19, true, 900_000, 64_000),
        Frame::one_shot(5, 21, true, 1 << 20),
        Frame::one_shot(22, 18, true, 150_000),
    ];
    check("reused", &frames, &data, Dict::None);
}

/// With dictionaries: a raw-content one loaded into the context's tables
/// (prepared, and an input over 128 KB and 6 times the dictionary) goes
/// into the LDM table too; a zstd-format one, or one attached or copied
/// from its CDict, does not.
#[test]
fn forced_with_dictionaries() {
    let data = input(1 << 20, 400_000, 6);
    let src = source_corpus();
    // the input repeats parts of the dictionary
    let raw = data[100_000..150_000].to_vec();
    let mut samples = Vec::new();
    let mut r = Rng::new(6);
    for _ in 0..1000 {
        let len = 100 + r.below(1000) as usize;
        let at = r.below((src.len() - len) as u64) as usize;
        samples.push(src[at..at + len].to_vec());
    }
    let trained = zstd::dict::from_samples(&samples, 16 << 10).unwrap();
    for level in [1, 3, 6, 13, 16, 19, 22] {
        for (dname, d) in [("raw", &raw[..]), ("trained", &trained[..])] {
            for (n, wlog) in [
                (400_000usize, 19),
                (1 << 20, 20),
                (20_000, 17),
                (200_000, 0),
            ] {
                let f = Frame::one_shot(level, wlog, true, n);
                let name = format!("{dname} prepared");
                check(
                    &name,
                    std::slice::from_ref(&f),
                    &data,
                    Dict::Prepared(d, level),
                );
                let name = format!("{dname} loaded");
                check(&name, &[f], &data, Dict::Loaded(d));
            }
            let f = Frame::streamed(level, 19, true, 600_000, 100_000);
            check(
                &format!("{dname} streamed"),
                &[f],
                &data,
                Dict::Prepared(d, level),
            );
        }
    }
}

/// Levels 16 to 21 with a 128 MiB window and an unknown size: LDM is on
/// by itself, as at level 22.
#[test]
fn auto_window_27() {
    let data = input(3 << 20, 2 << 20, 7);
    for level in 16..=21 {
        let n = if level >= 19 { 1 << 20 } else { data.len() };
        let f = Frame::streamed(level, 27, false, n, 200_000);
        check("wlog27", &[f], &data, Dict::None);
    }
}

/// Level 22 streaming with an unknown size (LDM on by itself, a 128 MiB
/// window), also with a dictionary.
#[test]
fn auto_level22_streaming() {
    let data = input(2 << 20, 1 << 20, 8);
    let f = Frame::streamed(22, 0, false, data.len(), 300_000);
    check("L22 stream", &[f], &data, Dict::None);
    let dict = data[..60_000].to_vec();
    let f = Frame::streamed(22, 0, false, 1 << 20, 100_000);
    check(
        "L22 stream dict",
        &[f],
        &data[60_000..],
        Dict::Loaded(&dict),
    );
}

/// Level 22 one-shot on more than 64 MiB: LDM on by itself, finding the
/// copies far back (about 1.5 GiB of memory for both encoders).
#[test]
fn auto_level22_one_shot_large() {
    let n = (64 << 20) + 100_000;
    // repeats of up to 48 MiB back: beyond the optimal parser's reach
    let data = input(n, 48 << 20, 9);
    let got = zstd_native::compress(&data, 22).unwrap();
    let want = zstd::bulk::compress(&data, 22).unwrap();
    if got != want {
        let first = got.iter().zip(&want).position(|(a, b)| a != b);
        panic!(
            "L22 {} bytes: output differs from libzstd at byte {first:?} (sizes {} vs {})",
            n,
            got.len(),
            want.len()
        );
    }
    assert!(zstd_native::decompress(&got).unwrap() == data, "round trip");
    // and with the reused compressor
    let mut c = zstd_native::Compressor::new(22);
    assert_eq!(
        c.compress(&data[..1000]).unwrap(),
        zstd::bulk::compress(&data[..1000], 22).unwrap()
    );
    assert!(c.compress(&data).unwrap() == want);
}

/// With the content checksum (`ZSTD_c_checksumFlag`): one-shot, streaming
/// with flushes and wraps, a reused context, a dictionary.
#[test]
fn forced_with_checksum() {
    let data = input(3 << 20, 1 << 20, 10);
    let with = |mut f: Frame| {
        f.checksum = true;
        f
    };
    for level in [1, 3, 5, 7, 12, 16, 19, 22] {
        let n = if level >= 16 { 1 << 20 } else { 3 << 20 };
        let f = with(Frame::one_shot(level, 20, true, n));
        check("checksum", &[f], &data, Dict::None);
        let f = with(Frame::streamed_flushing(level, 19, true, n, 70_000, 4));
        check("checksum stream", &[f], &data, Dict::None);
    }
    let mut pledged = with(Frame::streamed(6, 20, true, 1 << 20, 100_000));
    pledged.pledged = Some(1 << 20);
    let frames = [
        with(Frame::one_shot(19, 20, true, 700_000)),
        Frame::one_shot(3, 0, false, 200_000),
        pledged,
        with(Frame::one_shot(5, 21, true, 0)),
    ];
    check("checksum reused", &frames, &data, Dict::None);
    let dict = data[100_000..150_000].to_vec();
    let f = with(Frame::streamed(16, 19, true, 600_000, 100_000));
    check("checksum dict", &[f], &data, Dict::Prepared(&dict, 16));
    // level 22 with an unknown size (LDM on by itself, a 128 MiB window):
    // 64-bit only, since on 32-bit runners libzstd's allocation can fail
    // while other tests run in the same address space
    #[cfg(target_pointer_width = "64")]
    {
        let f = with(Frame::streamed(22, 0, false, 1 << 20, 300_000));
        check("checksum L22", &[f], &data, Dict::None);
    }
}
