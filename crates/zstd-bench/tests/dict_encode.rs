//! Compression with dictionaries must produce exactly libzstd's bytes:
//! - a loaded dictionary (`ZSTD_CCtx_loadDictionary`, the `zstd` crate's
//!   `bulk::Compressor::with_dictionary`, `Encoder::with_dictionary`);
//! - a prepared one (`ZSTD_createCDict` + `ZSTD_CCtx_refCDict`,
//!   `with_prepared_dictionary`),
//!
//! for zstd-format dictionaries (trained with ZDICT) and raw content, every
//! level, input sizes on both sides of the attach / copy / reload cutoffs,
//! reused contexts and streaming.

use std::io::Write;

use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd::zstd_safe::{CCtx, CParameter, InBuffer, OutBuffer};
use zstd_bench::{Rng, log_corpus, numeric_corpus, random_corpus, source_corpus};
use zstd_native::{Compressor, Dictionary, EncoderDictionary, EndOp, StreamEncoder};

/// From this level up, inputs are cut to [`SLOW_INPUT_MAX`] bytes.
const SLOW_LEVEL: i32 = 17;
const SLOW_INPUT_MAX: usize = 300_000;

/// Samples to train dictionaries on: pieces of the source and log corpora.
fn samples(seed: u64) -> Vec<Vec<u8>> {
    let src = source_corpus();
    let log = log_corpus(2 << 20);
    let mut r = Rng::new(seed);
    let mut v = Vec::new();
    for i in 0..2000 {
        let data = if i % 2 == 0 { &src } else { &log };
        let len = 200 + r.below(2000) as usize;
        let at = r.below((data.len() - len) as u64) as usize;
        v.push(data[at..at + len].to_vec());
    }
    v
}

/// A ZDICT-trained zstd-format dictionary of at most `size` bytes.
fn trained(size: usize, seed: u64) -> Vec<u8> {
    let d = zstd::dict::from_samples(&samples(seed), size).expect("ZDICT");
    assert_eq!(u32::from_le_bytes(d[..4].try_into().unwrap()), 0xEC30_A437);
    d
}

/// The dictionaries tested: trained ones of a few sizes, raw content
/// (tiny, under 8 bytes, small, larger than small windows).
fn dictionaries() -> Vec<(String, Vec<u8>)> {
    let src = source_corpus();
    vec![
        ("trained4k".into(), trained(4 << 10, 1)),
        ("trained32k".into(), trained(32 << 10, 2)),
        ("trained110k".into(), trained(110 << 10, 3)),
        ("raw5".into(), src[1000..1005].to_vec()),
        ("raw8".into(), src[2000..2008].to_vec()),
        ("raw3000".into(), src[50_000..53_000].to_vec()),
        ("raw300k".into(), src[100_000..400_000].to_vec()),
    ]
}

/// Inputs related to the dictionaries' content: the corpora at another
/// place, mixed with some noise.
fn input(n: usize, seed: u64) -> Vec<u8> {
    let src = source_corpus();
    let log = log_corpus(4 << 20);
    let mut r = Rng::new(seed);
    let mut v = Vec::with_capacity(n + 4096);
    while v.len() < n {
        let k = r.below(10);
        let len = 50 + r.below(3000) as usize;
        match k {
            0..=4 => {
                let at = r.below((src.len() - len) as u64) as usize;
                v.extend_from_slice(&src[at..at + len]);
            }
            5..=7 => {
                let at = r.below((log.len() - len) as u64) as usize;
                v.extend_from_slice(&log[at..at + len]);
            }
            8 => v.extend_from_slice(&numeric_corpus(len)),
            _ => v.extend_from_slice(&random_corpus(len.min(200))),
        }
    }
    v.truncate(n);
    v
}

fn first_diff(got: &[u8], want: &[u8]) -> usize {
    got.iter()
        .zip(want)
        .position(|(a, b)| a != b)
        .unwrap_or(got.len().min(want.len()))
}

#[track_caller]
fn compare(what: &str, got: &[u8], want: &[u8]) {
    if got != want {
        panic!(
            "{what}: output differs from libzstd at byte {} (sizes {} vs {})",
            first_diff(got, want),
            got.len(),
            want.len()
        );
    }
}

#[track_caller]
fn round_trip(what: &str, compressed: &[u8], dict: &[u8], data: &[u8]) {
    let mut d = zstd_native::Decompressor::with_dictionary(Dictionary::new(dict).unwrap());
    assert!(
        d.decompress(compressed).unwrap() == data,
        "{what}: round trip"
    );
}

/// libzstd with a loaded dictionary on a new context.
fn c_loaded(level: i32, dict: &[u8], data: &[u8]) -> Vec<u8> {
    zstd::bulk::Compressor::with_dictionary(level, dict)
        .unwrap()
        .compress(data)
        .unwrap()
}

const SIZES: &[usize] = &[
    0, 1, 7, 8, 9, 100, 1000, 5000, 8192, 8193, 12_000, 16_384, 16_385, 30_000, 32_768, 32_769,
    60_000, 131_071, 131_072, 131_073, 200_000, 700_000, 1_200_000,
];

fn levels() -> impl Iterator<Item = i32> {
    -5..=22
}

/// The sizes tested at `level` (the slow levels get fewer and shorter).
fn sizes_for(level: i32) -> impl Iterator<Item = usize> {
    SIZES.iter().copied().filter(move |&n| {
        level < SLOW_LEVEL || n <= SLOW_INPUT_MAX && (n < 100_000 || n == 200_000)
    })
}

#[test]
fn loaded_one_shot() {
    let data = input(1_200_000, 7);
    for (name, dict) in dictionaries() {
        for level in levels() {
            for n in sizes_for(level) {
                let src = &data[..n];
                let what = format!("{name} L{level} n={n}");
                let want = c_loaded(level, &dict, src);
                let got = zstd_native::compress_with_dictionary(src, &dict, level)
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
                compare(&what, &got, &want);
                round_trip(&what, &got, &dict, src);
            }
        }
    }
}

#[test]
fn prepared_reused() {
    let data = input(1_200_000, 8);
    for (name, dict) in dictionaries() {
        for level in levels() {
            let cdict = zstd::dict::EncoderDictionary::copy(&dict, level);
            let mut c = zstd::bulk::Compressor::with_prepared_dictionary(&cdict).unwrap();
            let edict = EncoderDictionary::new(&dict, level).unwrap();
            let mut n_c = Compressor::with_prepared_dictionary(&edict);
            // one context for every size (sizes up and down)
            let sizes: Vec<usize> = sizes_for(level).collect();
            for &n in sizes.iter().chain(sizes.iter().rev()) {
                let src = &data[n / 3..n / 3 + n.min(data.len() - n / 3)];
                let what = format!("{name} prepared L{level} n={}", src.len());
                let want = c.compress(src).unwrap();
                let got = n_c.compress(src).unwrap_or_else(|e| panic!("{what}: {e}"));
                compare(&what, &got, &want);
                round_trip(&what, &got, &dict, src);
            }
        }
    }
}

#[test]
fn reused_context_switching() {
    let dicts = dictionaries();
    let data = input(400_000, 9);
    let mut r = Rng::new(10);
    let mut cdicts = Vec::new();
    for (i, (_, d)) in dicts.iter().enumerate() {
        let level = [1, 3, 5, 9, 13, 16, 19][i % 7];
        cdicts.push((
            zstd::dict::EncoderDictionary::copy(d, level),
            EncoderDictionary::new(d, level).unwrap(),
        ));
    }
    let mut c = zstd::bulk::Compressor::new(3).unwrap();
    let mut n_c = Compressor::new(3);
    let mut current: Option<usize> = None;
    for step in 0..400 {
        // change dictionary or level now and then
        match r.below(8) {
            0 => {
                let level = r.below(25) as i32 - 3;
                let i = r.below(dicts.len() as u64) as usize;
                c.set_dictionary(level, &dicts[i].1).unwrap();
                n_c.set_level(level);
                n_c.set_dictionary(&dicts[i].1).unwrap();
                current = Some(i);
            }
            1 => {
                let i = r.below(dicts.len() as u64) as usize;
                c.set_prepared_dictionary(&cdicts[i].0).unwrap();
                n_c.set_prepared_dictionary(&cdicts[i].1);
                current = Some(i);
            }
            2 => {
                let level = r.below(20) as i32;
                c.set_compression_level(level).unwrap(); // clears the dictionary
                n_c.set_level(level);
                n_c.clear_dictionary();
                current = None;
            }
            _ => {}
        }
        let n = match r.below(4) {
            0 => r.below(100) as usize,
            1 => r.below(10_000) as usize,
            2 => r.below(70_000) as usize,
            _ => r.below(300_000) as usize,
        };
        let at = r.below((data.len() - n) as u64) as usize;
        let src = &data[at..at + n];
        let what = format!("step {step} dict {current:?} n={n}");
        let want = c.compress(src).unwrap();
        let got = n_c.compress(src).unwrap_or_else(|e| panic!("{what}: {e}"));
        compare(&what, &got, &want);
        match current {
            Some(i) => round_trip(&what, &got, &dicts[i].1, src),
            None => assert!(zstd_native::decompress(&got).unwrap() == src),
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

/// Feeds `data` to both streaming compressors in the same pieces with the
/// same operations and output buffer sizes (`ops`: input length,
/// operation, output size).
#[allow(clippy::type_complexity)]
fn stream_both(
    c: &mut CCtx<'_>,
    n: &mut StreamEncoder,
    data: &[u8],
    ops: &[(usize, EndOp, usize)],
) -> (Vec<u8>, Vec<u8>) {
    let (mut want, mut got) = (Vec::new(), Vec::new());
    let mut pos = 0;
    let mut buf = Vec::new();
    for &(len, op, out_size) in ops {
        let piece = &data[pos..pos + len];
        pos += len;
        buf.resize(out_size, 0);
        let mut input = InBuffer::around(piece);
        loop {
            let mut output = OutBuffer::around(&mut buf[..]);
            let remaining = c
                .compress_stream2(&mut output, &mut input, directive(op))
                .unwrap_or_else(|e| panic!("libzstd: {}", zstd::zstd_safe::get_error_name(e)));
            let k = output.pos();
            want.extend_from_slice(&buf[..k]);
            if input.pos() == piece.len() && (op == EndOp::Continue || remaining == 0) {
                break;
            }
        }
        let mut piece = piece;
        loop {
            let p = n
                .compress(piece, &mut buf, op)
                .unwrap_or_else(|e| panic!("zstd-native: {e}"));
            piece = &piece[p.read..];
            got.extend_from_slice(&buf[..p.written]);
            if piece.is_empty() && (op == EndOp::Continue || p.remaining == 0) {
                break;
            }
        }
    }
    (got, want)
}

/// `n` bytes in pieces of `chunk`, with a flush every `flush_every`
/// pieces (0: never), ended by an `End` without input.
fn script(
    n: usize,
    chunk: usize,
    flush_every: usize,
    out_size: usize,
) -> Vec<(usize, EndOp, usize)> {
    let mut ops = Vec::new();
    let mut left = n;
    let mut i = 0;
    while left > 0 {
        let k = chunk.min(left);
        left -= k;
        i += 1;
        let op = if flush_every != 0 && i % flush_every == 0 {
            EndOp::Flush
        } else {
            EndOp::Continue
        };
        ops.push((k, op, out_size));
    }
    ops.push((0, EndOp::End, out_size));
    ops
}

#[test]
fn streaming_loaded_and_prepared() {
    let data = input(2_500_000, 11);
    let dicts = dictionaries();
    let cases: &[(i32, u32, usize, usize, usize, usize)] = &[
        // level, windowLog, size, chunk, flush every, out size
        (1, 0, 300_000, 10_000, 0, 1 << 17),
        (3, 0, 300_000, 4096, 7, 1000),
        (3, 17, 1_000_000, 70_000, 0, 1 << 17),
        (5, 0, 50_000, 999, 3, 1 << 17),
        (6, 16, 600_000, 33_333, 0, 4000),
        (9, 0, 200_000, 50_000, 2, 1 << 17),
        (12, 18, 900_000, 100_000, 0, 1 << 17),
        (13, 0, 100_000, 7000, 0, 1 << 17),
        (16, 17, 500_000, 40_000, 4, 1 << 17),
        (19, 0, 150_000, 30_000, 0, 1 << 17),
        (19, 17, 400_000, 64_000, 0, 1 << 17),
        (22, 18, 60_000, 10_000, 0, 1 << 17), // (no LDM: window under 128 MiB)
        (-3, 15, 400_000, 20_000, 5, 1 << 17),
    ];
    for (name, dict) in &dicts {
        for &(level, wlog, size, chunk, flush_every, out_size) in cases {
            let src = &data[..size];
            let mut ops = script(size, chunk, flush_every, out_size);
            if flush_every == 0 && chunk > 30_000 {
                // the last piece with `End` (libzstd's `ZSTD_compressEnd` shortcut
                // when the output is large enough)
                ops.pop();
                let last = ops.pop().unwrap();
                ops.push((last.0, EndOp::End, 1 << 20));
            }
            // loaded
            let mut c = CCtx::create();
            c.set_parameter(CParameter::CompressionLevel(level))
                .unwrap();
            if wlog != 0 {
                c.set_parameter(CParameter::WindowLog(wlog)).unwrap();
            }
            c.load_dictionary(dict).unwrap();
            let mut n = StreamEncoder::with_dictionary(level, dict).unwrap();
            n.set_window_log(wlog);
            let what = format!("{name} stream L{level} w{wlog} n={size} chunk {chunk}");
            for frame in 0..2 {
                let (got, want) = stream_both(&mut c, &mut n, src, &ops);
                compare(&format!("{what} frame {frame}"), &got, &want);
                round_trip(&what, &got, dict, src);
            }
            // prepared, with the size pledged
            let cdict = zstd::dict::EncoderDictionary::copy(dict, level);
            let mut c = CCtx::create();
            if wlog != 0 {
                c.set_parameter(CParameter::WindowLog(wlog)).unwrap();
            }
            c.ref_cdict(cdict.as_cdict()).unwrap();
            let edict = EncoderDictionary::new(dict, level).unwrap();
            let mut n = StreamEncoder::with_prepared_dictionary(&edict);
            n.set_window_log(wlog);
            for frame in 0..2 {
                let pledged = if frame == 1 { Some(size as u64) } else { None };
                c.set_pledged_src_size(pledged).unwrap();
                n.set_pledged_src_size(pledged).unwrap();
                let (got, want) = stream_both(&mut c, &mut n, src, &ops);
                compare(&format!("{what} prepared frame {frame}"), &got, &want);
                round_trip(&what, &got, dict, src);
            }
        }
    }
}

#[test]
fn io_encoders() {
    let data = input(700_000, 12);
    for (name, dict) in dictionaries() {
        for level in [1, 3, 7, 11, 17, 19] {
            for n in [0, 100, 20_000, 700_000] {
                let src = &data[..n];
                let what = format!("{name} io L{level} n={n}");
                let mut c = zstd::stream::write::Encoder::with_dictionary(Vec::new(), level, &dict)
                    .unwrap();
                let mut e =
                    zstd_native::Encoder::with_dictionary(Vec::new(), level, &dict).unwrap();
                for piece in src.chunks(30_000) {
                    c.write_all(piece).unwrap();
                    e.write_all(piece).unwrap();
                }
                let want = c.finish().unwrap();
                let got = e.finish().unwrap();
                compare(&what, &got, &want);
                round_trip(&what, &got, &dict, src);

                let cdict = zstd::dict::EncoderDictionary::copy(&dict, level);
                let edict = EncoderDictionary::new(&dict, level).unwrap();
                let mut c =
                    zstd::stream::write::Encoder::with_prepared_dictionary(Vec::new(), &cdict)
                        .unwrap();
                let mut e = zstd_native::Encoder::with_prepared_dictionary(Vec::new(), &edict);
                for piece in src.chunks(30_000) {
                    c.write_all(piece).unwrap();
                    e.write_all(piece).unwrap();
                }
                let want = c.finish().unwrap();
                let got = e.finish().unwrap();
                compare(&format!("{what} prepared"), &got, &want);
            }
        }
    }
}

#[test]
fn dictionary_ids_and_errors() {
    let d = trained(4 << 10, 21);
    let id = u32::from_le_bytes(d[4..8].try_into().unwrap());
    let e = EncoderDictionary::new(&d, 5).unwrap();
    assert_eq!(e.id(), id);
    assert_eq!(e.level(), 5);
    assert_eq!(EncoderDictionary::new(&d, 0).unwrap().level(), 3);
    assert_eq!(EncoderDictionary::new(b"raw content", 1).unwrap().id(), 0);
    let f = zstd_native::compress_with_dictionary(b"some data, some data", &d, 3).unwrap();
    assert_eq!(zstd_native::FrameHeader::parse(&f).unwrap().dict_id, id);
    // corrupt zstd-format dictionaries are rejected, like libzstd does
    let mut bad = d.clone();
    bad[8..40].fill(0xFF);
    assert!(zstd::dict::EncoderDictionary::try_copy(&bad, 3).is_err());
    assert!(EncoderDictionary::new(&bad, 3).is_err());
    assert!(Compressor::with_dictionary(3, &bad).is_err());
    assert!(zstd_native::compress_with_dictionary(b"x", &bad, 3).is_err());
}
