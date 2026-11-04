//! Compress one dataset in a loop (MiB/s of input), zstd-native vs libzstd.
//!
//! `cargo run --profile bench -p zstd-bench --example profile_encode -- <dataset> <level> <native|c> <seconds>`

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ds = args.get(1).map_or("source", |s| s);
    let level: i32 = args.get(2).map_or(1, |s| s.parse().unwrap());
    let lib = args.get(3).map_or("native", |s| s);
    let secs: f64 = args.get(4).map_or(3.0, |s| s.parse().unwrap());
    let mut data = zstd_bench::datasets()
        .into_iter()
        .find(|(n, _)| n == ds)
        .expect("dataset")
        .1;
    // Optional chunk size: compress many independent frames of that size.
    let chunk: usize = args.get(5).map_or(0, |s| s.parse().unwrap());
    if chunk > 0 {
        data.truncate(chunk * 256.min(data.len() / chunk));
    }
    let mut out = vec![0u8; zstd_native::compress_bound(data.len()) + 64];
    let mut c = zstd::bulk::Compressor::new(level).unwrap();
    let mut n = 0u64;
    let mut size = 0;
    let t = Instant::now();
    while t.elapsed().as_secs_f64() < secs {
        size = 0;
        let pieces: Vec<&[u8]> = if chunk > 0 {
            data.chunks(chunk).collect()
        } else {
            vec![&data[..]]
        };
        for p in pieces {
            size += match lib {
                "native" => zstd_native::compress_to_slice(p, &mut out, level).unwrap(),
                _ => c.compress_to_buffer(p, &mut out[..]).unwrap(),
            };
        }
        n += 1;
    }
    let el = t.elapsed().as_secs_f64();
    println!(
        "{lib} {ds} L{level}: ratio {:.3}, {:.0} MiB/s",
        data.len() as f64 / size as f64,
        (n as f64 * data.len() as f64) / el / (1 << 20) as f64
    );
}
