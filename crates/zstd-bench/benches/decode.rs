//! Decompression speed: zstd-native vs C libzstd 1.5.7 and the pure-Rust
//! crates. Every library reuses its context and decodes into a
//! preallocated buffer; each one's output is verified before timing.
//!
//! `ZSTD_BENCH_LEVELS=1,3,19` picks the levels (default 1,3,9,19),
//! `ZSTD_BENCH_ONLY=zstd-native,libzstd` the libraries, and
//! `ZSTD_BENCH_CORPUS=dir` adds files (see zstd-bench's lib.rs).

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use zstd_bench::{c_compress, datasets};

fn env_list(name: &str) -> Option<Vec<String>> {
    std::env::var(name)
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
}

fn decode(c: &mut Criterion) {
    let levels: Vec<i32> = env_list("ZSTD_BENCH_LEVELS")
        .map(|v| v.iter().map(|s| s.parse().unwrap()).collect())
        .unwrap_or_else(|| vec![1, 3, 9, 19]);
    let only = env_list("ZSTD_BENCH_ONLY");
    let enabled = |lib: &str| only.as_ref().is_none_or(|o| o.iter().any(|x| x == lib));

    for (name, data) in datasets() {
        for &level in &levels {
            let comp = c_compress(&data, level);
            let mut group = c.benchmark_group(format!("decode/{name}"));
            group.throughput(Throughput::Bytes(data.len() as u64));
            group.measurement_time(Duration::from_secs(3));
            group.warm_up_time(Duration::from_millis(500));
            group.sample_size(20);
            let id = |lib: &str| BenchmarkId::new(lib, format!("L{level}"));
            let mut out = vec![0u8; data.len() + 64];

            if enabled("zstd-native") {
                let mut d = zstd_native::Decompressor::new();
                let n = d.decompress_to_slice(&comp, &mut out).unwrap();
                assert!(out[..n] == data[..], "zstd-native output");
                group.bench_function(id("zstd-native"), |b| {
                    b.iter(|| d.decompress_to_slice(&comp, &mut out).unwrap())
                });
            }
            if enabled("libzstd") {
                let mut d = zstd::bulk::Decompressor::new().unwrap();
                let n = d.decompress_to_buffer(&comp, &mut out[..]).unwrap();
                assert!(out[..n] == data[..], "libzstd output");
                group.bench_function(id("libzstd"), |b| {
                    b.iter(|| d.decompress_to_buffer(&comp, &mut out[..]).unwrap())
                });
            }
            if enabled("ruzstd") {
                let mut d = ruzstd::decoding::FrameDecoder::new();
                match d.decode_all(&comp, &mut out) {
                    Ok(n) if out[..n] == data[..] => {
                        group.bench_function(id("ruzstd"), |b| {
                            b.iter(|| d.decode_all(&comp, &mut out).unwrap())
                        });
                    }
                    r => eprintln!(
                        "ruzstd skipped for {name} L{level}: {:?}",
                        r.map(|_| "wrong output")
                    ),
                }
            }
            if enabled("structured-zstd") {
                let mut d = structured_zstd::decoding::FrameDecoder::new();
                match d.decode_all(&comp, &mut out) {
                    Ok(n) if out[..n] == data[..] => {
                        group.bench_function(id("structured-zstd"), |b| {
                            b.iter(|| d.decode_all(&comp, &mut out).unwrap())
                        });
                    }
                    r => eprintln!(
                        "structured-zstd skipped for {name} L{level}: {:?}",
                        r.map(|_| "wrong output")
                    ),
                }
            }
            group.finish();
        }
    }
}

criterion_group!(benches, decode);
criterion_main!(benches);
