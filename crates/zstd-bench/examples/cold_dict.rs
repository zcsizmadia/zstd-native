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
//!   native `Dictionary::new` + `set_dictionary(Some(dict))` + decode +
//!   `set_dictionary(None)`, libzstd `ZSTD_createDDict` +
//!   `ZSTD_decompress_usingDDict` + `ZSTD_freeDDict`. Both sides free the
//!   dictionary before parsing the next one, so the allocator may hand the
//!   next one the same address, which both libraries then treat as warm
//!   (`ddictIsCold` compares addresses).
//! - `create-keep`: `create` with the previous dictionary freed only after
//!   the next one is parsed (native: replaced by `set_dictionary`; libzstd:
//!   `ZSTD_freeDDict` of the previous `DDict` after `ZSTD_createDDict`),
//!   so every dictionary has a new address and is cold.
//! - `parse`: only the dictionary parse (and drop) of `create`.
//! - `parse-warm`: `parse` from one copy of the bytes, in cache: the table
//!   builds without the cold content copy.
//! - `warm`: one decompressor and one dictionary for all the frames (the
//!   dictionary installed once, before the timing).
//! - `reinstall`: `warm`, installing the same dictionary again before each
//!   frame (`set_dictionary(Some(dict.clone()))`; libzstd: `warm`, the
//!   same `DDict` passed each time).
//!
//! `size` sets the frame size (default 4096): smaller cuts the 4 KiB
//! frames, larger (up to 1 MiB, for literals sections up to 128 KiB) takes
//! 64 frames of `size` bytes from the same kind of data after the
//! training samples. `$COLD_DICT_DATA=<file>` takes the samples and frames
//! from that file (repeated as needed) instead of the generated logs.

use std::time::Instant;

fn main() {
    let lib = std::env::args().nth(1).unwrap_or_else(|| "native".into());
    let level: i32 = std::env::args().nth(2).map_or(3, |s| s.parse().unwrap());
    let mode = std::env::args().nth(3).unwrap_or_else(|| "ctx".into());
    let size: usize = std::env::args().nth(4).map_or(4096, |s| s.parse().unwrap());
    assert!(size <= 1 << 20, "size over 1 MiB");
    let len = (8 << 20).max(1000 * 4096 + 64 * size);
    // `$COLD_DICT_DATA=<file>`: that file (repeated as needed) instead of
    // the generated logs, e.g. a Silesia file for literals-heavy blocks.
    let corpus = match std::env::var("COLD_DICT_DATA") {
        Ok(path) => {
            let f = std::fs::read(path).unwrap();
            f.iter().copied().cycle().take(len).collect()
        }
        Err(_) => zstd_bench::log_corpus(len),
    };
    let samples: Vec<&[u8]> = corpus.chunks(4096).collect();
    let dict = zstd::dict::from_samples(&samples[..1000], 110 << 10).unwrap();
    let mut c = zstd::bulk::Compressor::with_dictionary(level, &dict).unwrap();
    let frames: Vec<Vec<u8>> = if size <= 4096 {
        samples[1000..1064]
            .iter()
            .map(|s| c.compress(&s[..size]).unwrap())
            .collect()
    } else {
        corpus[1000 * 4096..]
            .chunks_exact(size)
            .take(64)
            .map(|s| c.compress(s).unwrap())
            .collect()
    };
    zstd_native::__private::set_seq_loop(match lib.as_str() {
        "native-long" => Some(true),
        "native-short" => Some(false),
        _ => None,
    });
    let native = lib != "c";
    let copies = 2048;
    let mut out = vec![0u8; size.max(8192)];
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
            ("create" | "create-keep" | "parse", _) => {
                let raw: Vec<Vec<u8>> = (0..copies).map(|_| dict.clone()).collect();
                let parse_only = mode == "parse";
                let keep = mode == "create-keep";
                if native {
                    let mut d = zstd_native::Decompressor::new();
                    t = Instant::now();
                    for (i, r) in raw.iter().enumerate() {
                        let dict = zstd_native::Dictionary::new(r).unwrap();
                        if parse_only {
                            std::hint::black_box(&dict);
                            continue;
                        }
                        // Replacing drops the previous dictionary.
                        d.set_dictionary(Some(dict));
                        let n = d.decompress_to_slice(frame(i), &mut out).unwrap();
                        assert_eq!(n, size);
                        if !keep {
                            // `ZSTD_freeDDict` right after the frame.
                            d.set_dictionary(None);
                        }
                    }
                } else {
                    let mut dctx = zstd::zstd_safe::DCtx::create();
                    let mut prev = None;
                    t = Instant::now();
                    for (i, r) in raw.iter().enumerate() {
                        let dd = zstd::zstd_safe::DDict::create(r);
                        if parse_only {
                            std::hint::black_box(&dd);
                            continue;
                        }
                        // Frees the previous `DDict` (create-keep).
                        prev = None;
                        let n = dctx
                            .decompress_using_ddict(&mut out[..], frame(i), &dd)
                            .unwrap();
                        assert_eq!(n, size);
                        if keep {
                            prev = Some(dd);
                        }
                    }
                    drop(prev);
                }
            }
            ("parse-warm", _) => {
                t = Instant::now();
                for _ in 0..copies {
                    if native {
                        std::hint::black_box(zstd_native::Dictionary::new(&dict).unwrap());
                    } else {
                        std::hint::black_box(zstd::zstd_safe::DDict::create(&dict));
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
