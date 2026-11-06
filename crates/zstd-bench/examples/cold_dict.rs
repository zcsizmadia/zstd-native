//! Decoding with a cold dictionary: one 4 KiB frame per decompressor, each
//! decompressor given its own copy of a 110 KiB trained dictionary made
//! long before (2048 copies, 220 MiB: out of cache when used). libzstd:
//! one `DDict` per frame (`ddictIsCold`). Prints the best of 7 rounds.
//!
//! `cargo run --profile bench -p zstd-bench --example cold_dict -- <lib> <level>`
//! with lib `native` or `c`.

use std::time::Instant;

fn main() {
    let lib = std::env::args().nth(1).unwrap_or_else(|| "native".into());
    let level: i32 = std::env::args().nth(2).map_or(3, |s| s.parse().unwrap());
    let corpus = zstd_bench::log_corpus(8 << 20);
    let samples: Vec<&[u8]> = corpus.chunks(4096).collect();
    let dict = zstd::dict::from_samples(&samples[..1000], 110 << 10).unwrap();
    let mut c = zstd::bulk::Compressor::with_dictionary(level, &dict).unwrap();
    let frames: Vec<Vec<u8>> = samples[1000..1064]
        .iter()
        .map(|s| c.compress(s).unwrap())
        .collect();
    let parsed = zstd_native::Dictionary::new(&dict).unwrap();
    let copies = 2048;
    let mut out = vec![0u8; 8192];
    let mut best = f64::MAX;
    for _ in 0..7 {
        let t;
        if lib == "c" {
            let ddicts: Vec<_> = (0..copies)
                .map(|_| zstd::zstd_safe::DDict::create(&dict))
                .collect();
            let mut dctx = zstd::zstd_safe::DCtx::create();
            t = Instant::now();
            for (i, d) in ddicts.iter().enumerate() {
                let n = dctx
                    .decompress_using_ddict(&mut out[..], &frames[i % frames.len()], d)
                    .unwrap();
                assert_eq!(n, 4096);
            }
        } else {
            let mut decs: Vec<_> = (0..copies)
                .map(|_| zstd_native::Decompressor::with_dictionary(parsed.clone()))
                .collect();
            t = Instant::now();
            for (i, d) in decs.iter_mut().enumerate() {
                let n = d
                    .decompress_to_slice(&frames[i % frames.len()], &mut out)
                    .unwrap();
                assert_eq!(n, 4096);
            }
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!(
        "{lib} cold dictionary L{level}: {:.0} ns/frame",
        best * 1e9 / copies as f64
    );
}
