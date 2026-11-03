//! Datasets shared by the benchmarks and the differential tests.
//!
//! - `source`: the zstd C sources from `third_party/zstd` (text, ~2-3 MB)
//! - `log`: synthetic server log lines (very compressible text)
//! - `numeric`: little-endian integer series (structured binary)
//! - `random`: incompressible bytes
//! - every file in `$ZSTD_BENCH_CORPUS` (for example the Silesia corpus)

use std::path::{Path, PathBuf};

/// Deterministic xorshift generator (no dependency, stable across runs).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The zstd C library and CLI sources, concatenated in a stable order.
pub fn source_corpus() -> Vec<u8> {
    let root = repo_root().join("third_party/zstd");
    let mut files = Vec::new();
    for dir in [
        "lib/common",
        "lib/compress",
        "lib/decompress",
        "lib/dictBuilder",
        "programs",
    ] {
        if let Ok(rd) = std::fs::read_dir(root.join(dir)) {
            for e in rd.flatten() {
                let p = e.path();
                if matches!(p.extension().and_then(|x| x.to_str()), Some("c" | "h")) {
                    files.push(p);
                }
            }
        }
    }
    files.sort();
    let mut out = Vec::new();
    for f in files {
        out.extend(std::fs::read(f).unwrap_or_default());
    }
    assert!(
        !out.is_empty(),
        "third_party/zstd is missing: git submodule update --init"
    );
    out
}

pub fn log_corpus(len: usize) -> Vec<u8> {
    let mut r = Rng::new(7);
    let methods = ["GET", "POST", "PUT", "DELETE"];
    let paths = [
        "/api/v1/users",
        "/api/v1/orders",
        "/static/app.js",
        "/health",
        "/api/v2/search?q=",
    ];
    let agents = [
        "Mozilla/5.0 (X11; Linux x86_64)",
        "curl/8.5.0",
        "Go-http-client/2.0",
    ];
    let mut out = Vec::with_capacity(len + 256);
    let mut t = 1_700_000_000u64;
    while out.len() < len {
        t += r.below(3);
        let line = format!(
            "{t} 10.{}.{}.{} {} {}{} {} {}ms \"{}\"\n",
            r.below(256),
            r.below(256),
            r.below(256),
            methods[r.below(4) as usize],
            paths[r.below(5) as usize],
            r.below(10000),
            [200, 200, 200, 304, 404, 500][r.below(6) as usize],
            r.below(900),
            agents[r.below(3) as usize]
        );
        out.extend_from_slice(line.as_bytes());
    }
    out.truncate(len);
    out
}

pub fn numeric_corpus(len: usize) -> Vec<u8> {
    let mut r = Rng::new(11);
    let mut out = Vec::with_capacity(len);
    let mut v: i64 = 1 << 20;
    while out.len() < len {
        v += r.below(64) as i64 - 31;
        out.extend_from_slice(&(v as u32).to_le_bytes());
        out.extend_from_slice(&((r.below(16) as u32) << 8).to_le_bytes());
    }
    out.truncate(len);
    out
}

pub fn random_corpus(len: usize) -> Vec<u8> {
    let mut r = Rng::new(3);
    (0..len).map(|_| r.next_u64() as u8).collect()
}

/// `(name, data)` for every dataset.
pub fn datasets() -> Vec<(String, Vec<u8>)> {
    let mut v = vec![
        ("source".to_string(), source_corpus()),
        ("log".to_string(), log_corpus(4 << 20)),
        ("numeric".to_string(), numeric_corpus(4 << 20)),
        ("random".to_string(), random_corpus(1 << 20)),
    ];
    if let Ok(dir) = std::env::var("ZSTD_BENCH_CORPUS") {
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("ZSTD_BENCH_CORPUS={dir}: {e}"))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            v.push((name, std::fs::read(&f).unwrap()));
        }
    }
    v
}

/// Compresses with C libzstd at `level` (the reference encoder).
pub fn c_compress(data: &[u8], level: i32) -> Vec<u8> {
    zstd::bulk::compress(data, level).expect("libzstd compress")
}
