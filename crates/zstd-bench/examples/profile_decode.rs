//! Decode one dataset in a loop, for `perf record`.
//!
//! `cargo run --release -p zstd-bench --example profile_decode -- <dataset> <level> <lib> <seconds>`
//! with lib `native` (per-CPU default), `native-asm` / `native-noasm`
//! (force the asm or Rust Huffman loops), `c`, or `structured`
//! (structured-zstd), or a streaming mode (`native-<mode>`, `c-<mode>`, see
//! [`STREAM_MODES`]).

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ds = args.get(1).map_or("source", |s| s);
    let level: i32 = args.get(2).map_or(3, |s| s.parse().unwrap());
    let lib = args.get(3).map_or("native", |s| s);
    let secs: f64 = args.get(4).map_or(5.0, |s| s.parse().unwrap());
    // `datasets()` (with `$ZSTD_BENCH_CORPUS`) or `short_offset_datasets()`.
    let data = zstd_bench::datasets()
        .into_iter()
        .chain(zstd_bench::short_offset_datasets())
        .find(|(n, _)| n == ds)
        .expect("dataset")
        .1;
    // `ZSTD_BENCH_COMP_CACHE=dir` keeps the compressed input on disk, so
    // repeated A/B runs at high levels don't recompress every time.
    let comp = match std::env::var("ZSTD_BENCH_COMP_CACHE") {
        Ok(dir) => {
            let path = std::path::Path::new(&dir).join(format!("{ds}.L{level}.zst"));
            match std::fs::read(&path) {
                Ok(c) => c,
                Err(_) => {
                    let c = zstd_bench::c_compress(&data, level);
                    std::fs::write(&path, &c).expect("write cache");
                    c
                }
            }
        }
        Err(_) => zstd_bench::c_compress(&data, level),
    };
    let mut out = vec![0u8; data.len() + 64];
    let mut n = 0u64;
    let t = Instant::now();
    zstd_native::__private::set_asm(match lib {
        "native-noasm" => Some(false),
        "native-asm" => Some(true),
        _ => None,
    });
    let mut native = zstd_native::Decompressor::new();
    let mut c = zstd::bulk::Decompressor::new().unwrap();
    let mut sd = structured_zstd::decoding::FrameDecoder::new();
    // `ZSTD_BENCH_ITERS=n` runs exactly n iterations instead of `secs`
    // (fixed work for `perf stat` instruction counts).
    let iters: Option<u64> = std::env::var("ZSTD_BENCH_ITERS")
        .ok()
        .map(|s| s.parse().unwrap());
    let mut native_push = zstd_native::StreamDecoder::new();
    let mut c_push = zstd::zstd_safe::DCtx::create();
    // Streaming modes: `native-<mode>` and `c-<mode>` (see `STREAM_MODES`).
    let stream = lib
        .strip_prefix("native-")
        .map(|m| (true, m))
        .or_else(|| lib.strip_prefix("c-").map(|m| (false, m)))
        .filter(|(_, m)| STREAM_MODES.contains(m));
    while iters.map_or_else(|| t.elapsed().as_secs_f64() < secs, |i| n < i) {
        if let Some((is_native, mode)) = stream {
            let len = if is_native {
                native_stream_mode(mode, &comp, &mut out, &mut native_push)
            } else {
                c_stream_mode(mode, &comp, &mut out, &mut c_push)
            };
            assert_eq!(len, data.len());
            n += 1;
            continue;
        }
        let len = match lib {
            "native-stream" => {
                use std::io::Read;
                let mut d = zstd_native::Decoder::with_buffer(&comp[..]);
                let mut total = 0;
                loop {
                    let n = d.read(&mut out[..128 << 10]).unwrap();
                    if n == 0 {
                        break total;
                    }
                    total += n;
                }
            }
            "c-stream" => {
                use std::io::Read;
                let mut d = zstd::stream::read::Decoder::with_buffer(&comp[..]).unwrap();
                let mut total = 0;
                loop {
                    let n = d.read(&mut out[..128 << 10]).unwrap();
                    if n == 0 {
                        break total;
                    }
                    total += n;
                }
            }
            "native" | "native-noasm" | "native-asm" => {
                native.decompress_to_slice(&comp, &mut out).unwrap()
            }
            "structured" => sd.decode_all(&comp, &mut out).unwrap(),
            _ => c.decompress_to_buffer(&comp, &mut out[..]).unwrap(),
        };
        assert_eq!(len, data.len());
        n += 1;
    }
    let el = t.elapsed().as_secs_f64();
    println!(
        "{lib} {ds} L{level}: ratio {:.2}, {:.0} MiB/s",
        data.len() as f64 / comp.len() as f64,
        (n as f64 * data.len() as f64) / el / (1 << 20) as f64
    );
}

/// Streaming modes, for `native-<mode>` and `c-<mode>` (libzstd through
/// the `zstd` crate):
/// - `push-128k`, `push-1m`, `push-all`: the push API
///   (`StreamDecoder::decode`, `ZSTD_decompressStream`), all the input at
///   once, output buffers of 128 KiB, 1 MiB or the whole output;
/// - `read-128k`, `read-1m`: `io::Read::read` into buffers of that size,
///   from a 128 KiB `BufReader` (reading a file);
/// - `rte`: `read_to_end` into a new `Vec`, the whole input in memory;
/// - `rte-buf`: `read_to_end` into a new `Vec` through a 128 KiB
///   `BufReader`.
///
/// The older `native-stream` / `c-stream` are `io::Read::read` into 128 KiB
/// buffers with the whole input in memory.
const STREAM_MODES: [&str; 7] = [
    "push-128k",
    "push-1m",
    "push-all",
    "read-128k",
    "read-1m",
    "rte",
    "rte-buf",
];

fn out_size(mode: &str, all: usize) -> usize {
    if mode.ends_with("128k") {
        128 << 10
    } else if mode.ends_with("1m") {
        1 << 20
    } else {
        all
    }
}

/// Reads `r` to its end in `buf`-sized pieces; returns the bytes read.
fn read_all(mut r: impl std::io::Read, buf: &mut [u8]) -> usize {
    let mut total = 0;
    loop {
        let n = r.read(buf).unwrap();
        if n == 0 {
            return total;
        }
        total += n;
    }
}

fn native_stream_mode(
    mode: &str,
    comp: &[u8],
    out: &mut [u8],
    d: &mut zstd_native::StreamDecoder,
) -> usize {
    use std::io::Read;
    let size = out_size(mode, out.len());
    match mode {
        "rte" => {
            let mut v = Vec::new();
            zstd_native::Decoder::with_buffer(comp)
                .read_to_end(&mut v)
                .unwrap()
        }
        "rte-buf" => {
            let mut v = Vec::new();
            zstd_native::Decoder::new(comp).read_to_end(&mut v).unwrap()
        }
        m if m.starts_with("read") => read_all(zstd_native::Decoder::new(comp), &mut out[..size]),
        _ => {
            let mut input = comp;
            let mut total = 0;
            loop {
                let p = d.decode(input, &mut out[..size]).unwrap();
                input = &input[p.read..];
                total += p.written;
                if input.is_empty() && p.at_frame_boundary {
                    return total;
                }
            }
        }
    }
}

fn c_stream_mode(
    mode: &str,
    comp: &[u8],
    out: &mut [u8],
    d: &mut zstd::zstd_safe::DCtx<'static>,
) -> usize {
    use std::io::Read;
    use zstd::zstd_safe::{InBuffer, OutBuffer, ResetDirective};
    let size = out_size(mode, out.len());
    match mode {
        "rte" => {
            let mut v = Vec::new();
            zstd::stream::read::Decoder::with_buffer(comp)
                .unwrap()
                .read_to_end(&mut v)
                .unwrap()
        }
        "rte-buf" => {
            let mut v = Vec::new();
            zstd::stream::read::Decoder::new(comp)
                .unwrap()
                .read_to_end(&mut v)
                .unwrap()
        }
        m if m.starts_with("read") => read_all(
            zstd::stream::read::Decoder::new(comp).unwrap(),
            &mut out[..size],
        ),
        _ => {
            d.reset(ResetDirective::SessionOnly).unwrap();
            let mut input = InBuffer::around(comp);
            let mut total = 0;
            loop {
                let mut o = OutBuffer::around(&mut out[..size]);
                let hint = d.decompress_stream(&mut o, &mut input).unwrap();
                total += o.pos();
                if hint == 0 && input.pos() == comp.len() {
                    return total;
                }
            }
        }
    }
}
