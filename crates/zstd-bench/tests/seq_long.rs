//! The prefetching sequence loop (libzstd's `ZSTD_decompressSequencesLong`):
//! selected after a dictionary was installed (libzstd's `ddictIsCold`),
//! not for long offsets (libzstd's other trigger, slower here: see
//! `use_prefetch_loop` in `block.rs`); and frames forced through it
//! (large windows with matches 9 MiB back, dictionaries, blocks of a few
//! sequences), against libzstd.
//!
//! The loop selection and the block counter are process-wide, so the tests
//! of this file run one at a time.

use std::sync::Mutex;

use zstd_bench::{c_compress, log_corpus, numeric_corpus, source_corpus};
use zstd_native::__private::{long_blocks, set_seq_loop};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(target_pointer_width = "64")]
fn c_decompress(src: &[u8], cap: usize) -> Vec<u8> {
    zstd::bulk::Decompressor::new()
        .unwrap()
        .decompress(src, cap)
        .unwrap()
}

/// Blocks the prefetching loop decoded while `f` ran (`None` without
/// debug assertions, which keep the count).
fn count_long<T>(f: impl FnOnce() -> T) -> (T, Option<usize>) {
    let before = long_blocks();
    let r = f();
    (r, long_blocks().zip(before).map(|(a, b)| a - b))
}

/// `a`, then `a` again in pieces of 200-600 bytes with 1-8 random bytes
/// after each: every piece is a new match about `a.len()` back.
#[cfg(target_pointer_width = "64")]
fn far_copies(a_len: usize, seed: u64) -> Vec<u8> {
    let mut r = zstd_bench::Rng::new(seed);
    let a: Vec<u8> = (0..a_len).map(|_| r.next_u64() as u8).collect();
    let mut v = a.clone();
    let mut p = 0;
    while p < a.len() {
        let n = (200 + r.below(400) as usize).min(a.len() - p);
        v.extend_from_slice(&a[p..p + n]);
        p += n;
        for _ in 0..1 + r.below(8) {
            v.push(r.next_u64() as u8);
        }
    }
    v
}

#[cfg(target_pointer_width = "64")]
fn compress_far(data: &[u8], level: i32, wlog: u32) -> Vec<u8> {
    let mut enc = zstd::bulk::Compressor::new(level).unwrap();
    enc.long_distance_matching(true).unwrap();
    enc.window_log(wlog).unwrap();
    enc.compress(data).unwrap()
}

/// Streams `c` through a `StreamDecoder` with `out_size`-byte outputs.
fn stream(c: &[u8], out_size: usize) -> Vec<u8> {
    let mut d = zstd_native::StreamDecoder::new();
    let mut input = c;
    let mut out = vec![0u8; out_size];
    let mut v = Vec::new();
    loop {
        let p = d.decode(input, &mut out).unwrap();
        input = &input[p.read..];
        v.extend_from_slice(&out[..p.written]);
        if input.is_empty() && p.at_frame_boundary {
            return v;
        }
    }
}

/// History over 16 MiB with matches 9 MiB back (where libzstd takes its
/// prefetching loop): one-shot and streaming (with a window buffer that
/// wraps), through either loop, against libzstd. Left to itself the
/// decoder keeps the fused loop.
#[test]
#[cfg(target_pointer_width = "64")]
#[cfg_attr(miri, ignore = "decodes 18 MiB")]
fn long_offsets() {
    let _g = serial();
    let data = far_copies(9 << 20, 11);
    for (level, wlog) in [(3, 24), (19, 25)] {
        let c = compress_far(&data, level, wlog);
        assert!(
            c.len() < data.len() * 3 / 5,
            "L{level}: LDM should find the copy"
        );
        assert!(c_decompress(&c, data.len()) == data);
        let mut dec = zstd_native::Decompressor::new();
        for mode in [None, Some(true), Some(false)] {
            set_seq_loop(mode);
            let (got, n) = count_long(|| dec.decompress(&c).unwrap());
            assert!(got == data, "L{level} {mode:?} one-shot");
            let (got, m) = count_long(|| stream(&c, 64 << 10));
            assert!(got == data, "L{level} {mode:?} streaming");
            if let (Some(n), Some(m)) = (n, m) {
                assert_eq!(n > 0, mode == Some(true), "L{level} {mode:?}");
                assert_eq!(m > 0, mode == Some(true), "L{level} {mode:?}");
            }
        }
        set_seq_loop(None);
    }
}

/// A dictionary just installed is cold: the first compressed block of the
/// first frame takes the prefetching loop, later frames don't.
#[test]
fn selected_for_cold_dictionary() {
    let _g = serial();
    set_seq_loop(None);
    let corpus = log_corpus(1 << 20);
    let samples: Vec<&[u8]> = corpus.chunks(4000).collect();
    let dict = zstd::dict::from_samples(&samples, 32 << 10).unwrap();
    let mut c = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
    let frames: Vec<Vec<u8>> = samples[..4]
        .iter()
        .map(|s| c.compress(s).unwrap())
        .collect();
    for streaming in [false, true] {
        let d = zstd_native::Dictionary::new(&dict).unwrap();
        let mut one = zstd_native::Decompressor::with_dictionary(d.clone());
        let mut push = zstd_native::StreamDecoder::with_dictionary(d);
        let mut decode = |f: &[u8]| {
            if !streaming {
                return one.decompress(f).unwrap();
            }
            let mut out = vec![0u8; 1000];
            let mut v = Vec::new();
            let mut input = f;
            loop {
                let p = push.decode(input, &mut out).unwrap();
                input = &input[p.read..];
                v.extend_from_slice(&out[..p.written]);
                if input.is_empty() && p.at_frame_boundary {
                    return v;
                }
            }
        };
        for (i, f) in frames.iter().enumerate() {
            let (got, n) = count_long(|| decode(f));
            assert!(got == samples[i], "frame {i} streaming={streaming}");
            if let Some(n) = n {
                assert_eq!(
                    n,
                    usize::from(i == 0 && cfg!(target_arch = "x86_64")),
                    "frame {i} streaming={streaming}"
                );
            }
        }
    }
}

/// Every block through the prefetching loop: levels, sizes (blocks of a
/// few sequences end with the ring partly full), dictionaries (matches
/// reaching into them), streaming; against libzstd.
#[test]
fn forced_matches_libzstd() {
    let _g = serial();
    set_seq_loop(Some(true));
    let src = source_corpus();
    let datasets = [
        src[..src.len().min(1 << 20)].to_vec(),
        log_corpus(1 << 20),
        numeric_corpus(1 << 20),
    ];
    let mut dec = zstd_native::Decompressor::new();
    for (i, data) in datasets.iter().enumerate() {
        for level in [-1, 1, 3, 9, 19] {
            for n in [1, 20, 50, 100, 300, 1000, 5000, 200_000, data.len()] {
                let d = &data[..n];
                let c = c_compress(d, level);
                let got = dec.decompress(&c).unwrap();
                assert!(got == d, "dataset {i} L{level} {n} bytes");
                let mut buf = vec![0u8; n];
                assert_eq!(dec.decompress_to_slice(&c, &mut buf), Ok(n));
                if n > 1000 {
                    assert!(
                        stream(&c, 4096) == d,
                        "dataset {i} L{level} {n} bytes stream"
                    );
                }
            }
        }
    }
    // Dictionaries: trained (entropy tables and content) and raw content.
    let corpus = log_corpus(2 << 20);
    let samples: Vec<&[u8]> = corpus.chunks(3000).collect();
    let trained = zstd::dict::from_samples(&samples, 64 << 10).unwrap();
    let raw = corpus[..100_000].to_vec();
    for (name, dict, d) in [
        (
            "trained",
            &trained,
            zstd_native::Dictionary::new(&trained).unwrap(),
        ),
        ("raw", &raw, zstd_native::Dictionary::raw(&raw)),
    ] {
        let mut dec = zstd_native::Decompressor::with_dictionary(d);
        for level in [1, 3, 9, 19] {
            let mut c = zstd::bulk::Compressor::new(level).unwrap();
            c.set_dictionary(level, dict).unwrap();
            for (j, s) in samples.iter().skip(400).take(40).enumerate() {
                let s = &s[..[s.len(), 30, 200, 1000][j % 4]];
                let comp = c.compress(s).unwrap();
                let got = dec.decompress(&comp).unwrap();
                assert!(got == s, "{name} L{level} sample {j}");
            }
        }
    }
    set_seq_loop(None);
}
