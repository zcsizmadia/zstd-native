//! Compress one dataset in a loop (MiB/s of input), zstd-native vs libzstd.
//!
//! `cargo run --profile bench -p zstd-bench --example profile_encode -- <dataset> <level> <native|c|structured|native-stream|c-stream|native-flush|c-flush|native-dict|c-dict> <seconds> [chunk]`
//!
//! - `native-flush` / `c-flush`: one streaming frame (`StreamEncoder` /
//!   `ZSTD_compressStream2`) with a flush (`ZSTD_e_flush`) after every
//!   `chunk` bytes (default 4096). Once the input buffer wraps, blocks are
//!   compressed in extDict mode. `ZSTD_BENCH_WLOG=n` sets the window log
//!   (default: the level's, for an unknown size).
//! - `native-dict` / `c-dict`: frames of `chunk` bytes (default 4096), each
//!   compressed with a prepared dictionary (`EncoderDictionary` /
//!   `ZSTD_createCDict` + `ZSTD_CCtx_refCDict` + `ZSTD_compress2`), from up
//!   to 4 MiB of the dataset (after the dictionary's bytes for `raw:`).
//!   Small inputs attach the dictionary (dictMatchState: up to 8 KiB for
//!   `fast`, 16 KiB for `dfast`), larger ones copy its tables (extDict).
//!   `ZSTD_BENCH_DICT` picks it: `raw:n` (default `raw:16384`, the
//!   dataset's first n bytes), `trained:n` (ZDICT-trained on 4 KiB pieces
//!   of the dataset's first 8 MiB, at most n bytes) or a file.
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
    // A file of `ZSTD_BENCH_CORPUS` is read alone (quicker start, less
    // noise in `perf stat` counts).
    let file = std::env::var("ZSTD_BENCH_CORPUS")
        .ok()
        .map(|d| std::path::Path::new(&d).join(ds))
        .filter(|p| p.is_file());
    let all = if file.is_some() {
        Vec::new()
    } else {
        zstd_bench::datasets()
    };
    let mut data = if let Some(f) = file {
        std::fs::read(f).expect("corpus file")
    } else if ds == "silesia" {
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
    if lib.ends_with("-flush") || lib.ends_with("-dict") {
        return special(ds, level, lib, secs, args.get(5), &data);
    }
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
    // `ZSTD_BENCH_ITERS=n` runs exactly n iterations instead of `secs`
    // (fixed work for `perf stat` instruction counts).
    let iters: Option<u64> = std::env::var("ZSTD_BENCH_ITERS")
        .ok()
        .map(|s| s.parse().unwrap());
    while iters.map_or_else(|| t.elapsed().as_secs_f64() < secs, |i| n < i) {
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
        "{lib} {ds} L{level}: ratio {:.3}, {:.3} MiB/s, {size} bytes",
        data.len() as f64 / size as f64,
        (n as f64 * data.len() as f64) / el / (1 << 20) as f64
    );
}

/// Runs `ZSTD_BENCH_ITERS` (or `secs` worth of) iterations of `f`, which
/// returns the input and output sizes of one iteration, and prints the
/// speed.
fn run(name: &str, secs: f64, mut f: impl FnMut() -> (usize, usize)) {
    let iters: Option<u64> = std::env::var("ZSTD_BENCH_ITERS")
        .ok()
        .map(|s| s.parse().unwrap());
    let (mut n, mut ins, mut outs) = (0u64, 0, 0);
    let t = Instant::now();
    while iters.map_or_else(|| t.elapsed().as_secs_f64() < secs, |i| n < i) {
        (ins, outs) = f();
        n += 1;
    }
    let el = t.elapsed().as_secs_f64();
    println!(
        "{name}: ratio {:.3}, {:.3} MiB/s, {outs} bytes",
        ins as f64 / outs as f64,
        (n as f64 * ins as f64) / el / (1 << 20) as f64
    );
}

/// The streaming-with-flushes and dictionary modes.
fn special(ds: &str, level: i32, lib: &str, secs: f64, chunk: Option<&String>, data: &[u8]) {
    use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective;
    use zstd::zstd_safe::{CCtx, CParameter, InBuffer, OutBuffer};
    use zstd_native::{EndOp, StreamEncoder};
    let chunk: usize = chunk.map_or(4096, |s| s.parse().unwrap());
    // The output buffer of every call: large enough for a whole block, so
    // that both libraries compress straight into it.
    let mut out = vec![0u8; zstd_native::compress_bound(1 << 17) + 1024];
    if lib.ends_with("-flush") {
        let wlog: u32 = std::env::var("ZSTD_BENCH_WLOG").map_or(0, |s| s.parse().unwrap());
        let name = format!("{lib} {ds} L{level} w{wlog} flush {chunk}");
        let pieces = data.len().div_ceil(chunk);
        if lib == "native-flush" {
            let mut e = StreamEncoder::new(level);
            e.set_window_log(wlog);
            run(&name, secs, || {
                let mut written = 0;
                for (i, piece) in data.chunks(chunk).enumerate() {
                    let op = if i + 1 == pieces {
                        EndOp::End
                    } else {
                        EndOp::Flush
                    };
                    let mut p = piece;
                    loop {
                        let r = e.compress(p, &mut out, op).unwrap();
                        p = &p[r.read..];
                        written += r.written;
                        if p.is_empty() && r.remaining == 0 {
                            break;
                        }
                    }
                }
                (data.len(), written)
            });
        } else {
            let mut c = CCtx::create();
            c.set_parameter(CParameter::CompressionLevel(level))
                .unwrap();
            if wlog != 0 {
                c.set_parameter(CParameter::WindowLog(wlog)).unwrap();
            }
            run(&name, secs, || {
                let mut written = 0;
                for (i, piece) in data.chunks(chunk).enumerate() {
                    let op = if i + 1 == pieces {
                        ZSTD_EndDirective::ZSTD_e_end
                    } else {
                        ZSTD_EndDirective::ZSTD_e_flush
                    };
                    let mut input = InBuffer::around(piece);
                    loop {
                        let mut o = OutBuffer::around(&mut out[..]);
                        let rem = c.compress_stream2(&mut o, &mut input, op).unwrap();
                        written += o.pos();
                        if input.pos() == piece.len() && rem == 0 {
                            break;
                        }
                    }
                }
                (data.len(), written)
            });
        }
        return;
    }
    // The dictionary modes.
    let spec = std::env::var("ZSTD_BENCH_DICT").unwrap_or_else(|_| "raw:16384".into());
    let (dict, skip) = if let Some(n) = spec.strip_prefix("raw:") {
        let n: usize = n.parse().unwrap();
        (data[..n].to_vec(), n)
    } else if let Some(n) = spec.strip_prefix("trained:") {
        let n: usize = n.parse().unwrap();
        let sample = &data[..data.len().min(8 << 20)];
        let sizes: Vec<usize> = sample.chunks(4096).map(<[u8]>::len).collect();
        let d = zstd::dict::from_continuous(sample, &sizes, n).expect("ZDICT");
        (d, 0)
    } else {
        (std::fs::read(&spec).expect("ZSTD_BENCH_DICT file"), 0)
    };
    let input = &data[skip..data.len().min(skip + (4 << 20))];
    let name = format!("{lib} {ds} L{level} dict {spec} chunk {chunk}");
    if lib == "native-dict" {
        let d = zstd_native::EncoderDictionary::new(&dict, level).unwrap();
        let mut c = zstd_native::Compressor::with_prepared_dictionary(&d);
        run(&name, secs, || {
            let w: usize = input
                .chunks(chunk)
                .map(|p| c.compress_to_slice(p, &mut out).unwrap())
                .sum();
            (input.len(), w)
        });
    } else {
        let d = zstd::dict::EncoderDictionary::copy(&dict, level);
        let mut c = zstd::bulk::Compressor::with_prepared_dictionary(&d).unwrap();
        run(&name, secs, || {
            let w: usize = input
                .chunks(chunk)
                .map(|p| c.compress_to_buffer(p, &mut out[..]).unwrap())
                .sum();
            (input.len(), w)
        });
    }
}
