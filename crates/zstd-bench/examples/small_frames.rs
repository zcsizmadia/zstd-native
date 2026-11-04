//! Per-frame cost on small inputs: decompress many independent frames of
//! one size with a reused context, zstd-native vs libzstd.
//!
//! `cargo run --profile bench -p zstd-bench --example small_frames -- [level] [seconds]`

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let level: i32 = args.get(1).map_or(3, |s| s.parse().unwrap());
    let secs: f64 = args.get(2).map_or(1.0, |s| s.parse().unwrap());
    let src = zstd_bench::source_corpus();
    let logs = zstd_bench::log_corpus(8 << 20);
    println!(
        "{:>8} {:>8} {:>12} {:>12} {:>7} {:>10} {:>10} {:>7}",
        "data", "size", "native ns", "libzstd ns", "ratio", "fn native", "fn C", "ratio"
    );
    for (name, corpus) in [("source", &src), ("log", &logs)] {
        let sizes: Vec<usize> = match std::env::var("SIZES") {
            Ok(s) => s.split(',').map(|x| x.parse().unwrap()).collect(),
            Err(_) => vec![100, 1 << 10, 4 << 10, 16 << 10, 64 << 10],
        };
        for &size in &sizes {
            let frames: Vec<(Vec<u8>, usize)> = corpus
                .chunks_exact(size)
                .take(256)
                .map(|c| (zstd_bench::c_compress(c, level), c.len()))
                .collect();
            let mut out = vec![0u8; size + 64];
            let mut native = zstd_native::Decompressor::new();
            let mut c = zstd::bulk::Decompressor::new().unwrap();
            let mut time = |f: &mut dyn FnMut(&[u8], &mut [u8]) -> usize| {
                let mut n = 0u64;
                let t = Instant::now();
                while t.elapsed().as_secs_f64() < secs {
                    for (fr, len) in &frames {
                        assert_eq!(f(fr, &mut out), *len);
                    }
                    n += frames.len() as u64;
                }
                t.elapsed().as_secs_f64() * 1e9 / n as f64
            };
            let only = std::env::var("ONLY").unwrap_or_default();
            let tn = if only == "c" {
                1.0
            } else {
                time(&mut |fr, o| native.decompress_to_slice(fr, o).unwrap())
            };
            let tf = if only.is_empty() {
                time(&mut |fr, o| zstd_native::decompress_to_slice(fr, o).unwrap())
            } else {
                1.0
            };
            let tcf = if only.is_empty() {
                time(&mut |fr, o| zstd::bulk::decompress_to_buffer(fr, o).unwrap())
            } else {
                1.0
            };
            let tc = if only == "native" {
                1.0
            } else {
                time(&mut |fr, o| c.decompress_to_buffer(fr, o).unwrap())
            };
            println!(
                "{name:>8} {size:>8} {tn:>12.0} {tc:>12.0} {:>6.2}x {tf:>10.0} {tcf:>10.0} {:>6.2}x",
                tc / tn,
                tcf / tf
            );
        }
    }
}
