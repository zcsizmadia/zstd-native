//! Decode one dataset in a loop, for `perf record`.
//!
//! `cargo run --release -p zstd-bench --example profile_decode -- <dataset> <level> <lib> <seconds>`
//! with lib `native` (per-CPU default), `native-asm` / `native-noasm`
//! (force the asm or Rust Huffman loops) or `c`.

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ds = args.get(1).map_or("source", |s| s);
    let level: i32 = args.get(2).map_or(3, |s| s.parse().unwrap());
    let lib = args.get(3).map_or("native", |s| s);
    let secs: f64 = args.get(4).map_or(5.0, |s| s.parse().unwrap());
    let data = zstd_bench::datasets()
        .into_iter()
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
    while t.elapsed().as_secs_f64() < secs {
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
