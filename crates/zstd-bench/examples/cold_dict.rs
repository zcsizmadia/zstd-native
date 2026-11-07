//! Decoding with a cold dictionary: 4 KiB frames compressed with a 110 KiB
//! trained dictionary, each frame decoded right after its dictionary is
//! installed, with 2048 copies of the dictionary made long before (220 MiB
//! and more: out of cache when used). Prints the best of 7 rounds, in ns
//! per frame.
//!
//! `cargo run --profile bench -p zstd-bench --example cold_dict -- <lib> <level> [mode] [size]`
//! with lib `native`, `native-long` / `native-short` (force the prefetching
//! or the fused sequence loop) or `c`, and mode:
//!
//! - `ctx` (default): one decompressor per frame, each holding its own
//!   parsed copy of the dictionary (made before the timing). libzstd: one
//!   reused (warm) `DCtx`, one `DDict` per frame (`ddictIsCold`): the
//!   same as its `switch`.
//! - `fresh`: `ctx` with one libzstd `DCtx` per frame too (made before the
//!   timing, like the decompressors), so both sides start cold.
//! - `switch`: one reused decompressor; each frame installs the next of
//!   the prepared (parsed) dictionaries, then decodes: native
//!   `set_dictionary(Some(dict.clone()))`, libzstd
//!   `ZSTD_decompress_usingDDict` with the next `DDict`.
//! - `create`: one reused decompressor; each frame parses its own (cold)
//!   copy of the dictionary bytes, installs it, decodes and drops it:
//!   native `Dictionary::new`, libzstd `ZSTD_createDDict` +
//!   `ZSTD_decompress_usingDDict` + `ZSTD_freeDDict`.
//! - `parse`: only the dictionary parse (and drop) of `create`.
//! - `warm`: one decompressor and one dictionary for all the frames (the
//!   dictionary installed once, before the timing).
//! - `reinstall`: `warm`, installing the same dictionary again before each
//!   frame (`set_dictionary(Some(dict.clone()))`; libzstd: `warm`, the
//!   same `DDict` passed each time).
//!
//! `size` cuts the frames to fewer bytes (default and maximum 4096).

use std::time::Instant;

fn main() {
    let lib = std::env::args().nth(1).unwrap_or_else(|| "native".into());
    let level: i32 = std::env::args().nth(2).map_or(3, |s| s.parse().unwrap());
    let mode = std::env::args().nth(3).unwrap_or_else(|| "ctx".into());
    let size: usize = std::env::args().nth(4).map_or(4096, |s| s.parse().unwrap());
    let corpus = zstd_bench::log_corpus(8 << 20);
    let samples: Vec<&[u8]> = corpus.chunks(4096).collect();
    let dict = zstd::dict::from_samples(&samples[..1000], 110 << 10).unwrap();
    let mut c = zstd::bulk::Compressor::with_dictionary(level, &dict).unwrap();
    let frames: Vec<Vec<u8>> = samples[1000..1064]
        .iter()
        .map(|s| c.compress(&s[..size]).unwrap())
        .collect();
    zstd_native::__private::set_seq_loop(match lib.as_str() {
        "native-long" => Some(true),
        "native-short" => Some(false),
        _ => None,
    });
    let native = lib != "c";
    let copies = 2048;
    let mut out = vec![0u8; 8192];
    let frame = |i: usize| &frames[i % frames.len()][..];
    let mut best = f64::MAX;
    for _ in 0..7 {
        let t;
        match (mode.as_str(), native) {
            ("ctx" | "switch", false) => {
                let ddicts: Vec<_> = (0..copies)
                    .map(|_| zstd::zstd_safe::DDict::create(&dict))
                    .collect();
                let mut dctx = zstd::zstd_safe::DCtx::create();
                t = Instant::now();
                for (i, d) in ddicts.iter().enumerate() {
                    let n = dctx
                        .decompress_using_ddict(&mut out[..], frame(i), d)
                        .unwrap();
                    assert_eq!(n, size);
                }
            }
            ("fresh", false) => {
                let mut ctxs: Vec<_> = (0..copies)
                    .map(|_| {
                        let d = zstd::zstd_safe::DDict::create(&dict);
                        (zstd::zstd_safe::DCtx::create(), d)
                    })
                    .collect();
                t = Instant::now();
                for (i, (dctx, d)) in ctxs.iter_mut().enumerate() {
                    let n = dctx
                        .decompress_using_ddict(&mut out[..], frame(i), d)
                        .unwrap();
                    assert_eq!(n, size);
                }
            }
            ("ctx" | "fresh", true) => {
                let mut decs: Vec<_> = (0..copies)
                    .map(|_| {
                        let d = zstd_native::Dictionary::new(&dict).unwrap();
                        zstd_native::Decompressor::with_dictionary(d)
                    })
                    .collect();
                t = Instant::now();
                for (i, d) in decs.iter_mut().enumerate() {
                    let n = d.decompress_to_slice(frame(i), &mut out).unwrap();
                    assert_eq!(n, size);
                }
            }
            ("switch", true) => {
                let dicts: Vec<_> = (0..copies)
                    .map(|_| zstd_native::Dictionary::new(&dict).unwrap())
                    .collect();
                let mut d = zstd_native::Decompressor::new();
                t = Instant::now();
                for (i, dict) in dicts.iter().enumerate() {
                    d.set_dictionary(Some(dict.clone()));
                    let n = d.decompress_to_slice(frame(i), &mut out).unwrap();
                    assert_eq!(n, size);
                }
            }
            ("create" | "parse", _) => {
                let raw: Vec<Vec<u8>> = (0..copies).map(|_| dict.clone()).collect();
                let parse_only = mode == "parse";
                if native {
                    let mut d = zstd_native::Decompressor::new();
                    t = Instant::now();
                    for (i, r) in raw.iter().enumerate() {
                        let dict = zstd_native::Dictionary::new(r).unwrap();
                        if parse_only {
                            std::hint::black_box(&dict);
                            continue;
                        }
                        d.set_dictionary(Some(dict));
                        let n = d.decompress_to_slice(frame(i), &mut out).unwrap();
                        assert_eq!(n, size);
                    }
                } else {
                    let mut dctx = zstd::zstd_safe::DCtx::create();
                    t = Instant::now();
                    for (i, r) in raw.iter().enumerate() {
                        let dd = zstd::zstd_safe::DDict::create(r);
                        if parse_only {
                            std::hint::black_box(&dd);
                            continue;
                        }
                        let n = dctx
                            .decompress_using_ddict(&mut out[..], frame(i), &dd)
                            .unwrap();
                        assert_eq!(n, size);
                    }
                }
            }
            ("warm" | "reinstall", false) => {
                let dd = zstd::zstd_safe::DDict::create(&dict);
                let mut dctx = zstd::zstd_safe::DCtx::create();
                t = Instant::now();
                for i in 0..copies {
                    let n = dctx
                        .decompress_using_ddict(&mut out[..], frame(i), &dd)
                        .unwrap();
                    assert_eq!(n, size);
                }
            }
            ("warm", true) => {
                let mut d = zstd_native::Decompressor::with_dictionary(
                    zstd_native::Dictionary::new(&dict).unwrap(),
                );
                t = Instant::now();
                for i in 0..copies {
                    let n = d.decompress_to_slice(frame(i), &mut out).unwrap();
                    assert_eq!(n, size);
                }
            }
            ("reinstall", true) => {
                let dict = zstd_native::Dictionary::new(&dict).unwrap();
                let mut d = zstd_native::Decompressor::new();
                t = Instant::now();
                for i in 0..copies {
                    d.set_dictionary(Some(dict.clone()));
                    let n = d.decompress_to_slice(frame(i), &mut out).unwrap();
                    assert_eq!(n, size);
                }
            }
            _ => panic!("unknown mode {mode}"),
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!(
        "{lib} {mode} dictionary L{level} {size} B: {:.0} ns/frame",
        best * 1e9 / copies as f64
    );
}
