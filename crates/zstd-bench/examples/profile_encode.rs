//! Compress one dataset in a loop (MiB/s of input), zstd-native vs libzstd.
//!
//! `cargo run --profile bench -p zstd-bench --example profile_encode -- <dataset> <level> <native|c|structured|native-stream|c-stream> <seconds> [chunk]`
//!
//! `ZSTD_BENCH_WORKERS=n` compresses with n worker threads (one-shot and
//! streaming modes), and `ZSTD_BENCH_REPEAT=k` repeats the dataset k times
//! (multithreading needs inputs of several jobs). The dataset `silesia` is
//! every file of `ZSTD_BENCH_CORPUS` concatenated (in name order).

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ds = args.get(1).map_or("source", |s| s);
    let level: i32 = args.get(2).map_or(1, |s| s.parse().unwrap());
    let lib = args.get(3).map_or("native", |s| s);
    let secs: f64 = args.get(4).map_or(3.0, |s| s.parse().unwrap());
    let all = zstd_bench::datasets();
    let mut data = if ds == "silesia" {
        // the corpus files follow the five built-in datasets
        all.into_iter()
            .skip(5)
            .map(|(_, d)| d)
            .collect::<Vec<_>>()
            .concat()
    } else {
        all.into_iter().find(|(n, _)| n == ds).expect("dataset").1
    };
    let repeat: usize = std::env::var("ZSTD_BENCH_REPEAT").map_or(1, |s| s.parse().unwrap());
    data = data.repeat(repeat);
    let workers: u32 = std::env::var("ZSTD_BENCH_WORKERS").map_or(0, |s| s.parse().unwrap());
    // Optional chunk size: compress many independent frames of that size.
    let chunk: usize = args.get(5).map_or(0, |s| s.parse().unwrap());
    if chunk > 0 {
        data.truncate(chunk * 256.min(data.len() / chunk));
    }
    let mut out = vec![0u8; zstd_native::compress_bound(data.len()) + 64];
    let mut c = zstd::bulk::Compressor::new(level).unwrap();
    let mut nc = zstd_native::Compressor::new(level);
    if workers > 0 {
        c.set_parameter(zstd::zstd_safe::CParameter::NbWorkers(workers))
            .unwrap();
        nc.set_workers(workers);
    }
    // Optional start offset into the data (alignment experiments).
    let off: usize = std::env::var("ZSTD_BENCH_OFFSET").map_or(0, |s| s.parse().unwrap());
    let mut n = 0u64;
    let mut size = 0;
    let t = Instant::now();
    while t.elapsed().as_secs_f64() < secs {
        size = 0;
        let pieces: Vec<&[u8]> = if chunk > 0 {
            data.chunks(chunk).collect()
        } else {
            vec![&data[off..]]
        };
        for p in pieces {
            size += match lib {
                "native" => nc.compress_to_slice(p, &mut out).unwrap(),
                // Streaming: 128 KB writes into an `io::Write` encoder.
                "native-stream" => {
                    use std::io::Write;
                    let mut e = zstd_native::Encoder::new(Vec::new(), level);
                    if workers > 0 {
                        e.set_workers(workers).unwrap();
                    }
                    for w in p.chunks(128 << 10) {
                        e.write_all(w).unwrap();
                    }
                    e.finish().unwrap().len()
                }
                "c-stream" => {
                    use std::io::Write;
                    let mut e = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
                    if workers > 0 {
                        e.multithread(workers).unwrap();
                    }
                    for w in p.chunks(128 << 10) {
                        e.write_all(w).unwrap();
                    }
                    e.finish().unwrap().len()
                }
                "structured" => structured_zstd::encoding::compress_slice_to_vec(
                    p,
                    structured_zstd::encoding::CompressionLevel::Level(level),
                )
                .len(),
                _ => c.compress_to_buffer(p, &mut out[..]).unwrap(),
            };
        }
        n += 1;
    }
    let el = t.elapsed().as_secs_f64();
    println!(
        "{lib} {ds} L{level}: ratio {:.3}, {:.3} MiB/s",
        data.len() as f64 / size as f64,
        (n as f64 * data.len() as f64) / el / (1 << 20) as f64
    );
}
