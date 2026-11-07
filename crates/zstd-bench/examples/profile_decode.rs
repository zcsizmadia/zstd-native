//! Decode one dataset in a loop, for `perf record`.
//!
//! `cargo run --release -p zstd-bench --example profile_decode -- <dataset> <level> <lib> <seconds>`
//! with lib `native` (per-CPU default), `native-asm` / `native-noasm`
//! (force the asm or Rust Huffman loops), `c`, or `structured`
//! (structured-zstd), or a streaming mode (`native-<mode>`, `c-<mode>`, see
//! [`STREAM_MODES`]). `native-long` / `native-short` force the prefetching
//! or the fused sequence loop (`native` takes the prefetching loop only for
//! the first block after a dictionary is installed).
//!
//! Dataset `cat` is every file of `$ZSTD_BENCH_CORPUS` catenated in name
//! order (the Silesia corpus as one stream), cut to `$ZSTD_BENCH_CAT_MB`
//! MiB if set. libzstd compresses at `<level>` with `$ZSTD_BENCH_WLOG`
//! (window log), `$ZSTD_BENCH_LONG=1` (long distance matching) and
//! `$ZSTD_BENCH_WORKERS` (threads, for big inputs at high levels) when set.
//! Dataset `cat-dup` is `cat` followed by a copy with a few bytes changed
//! every 1-8 KiB (two versions of a big file: matches a whole `cat` back,
//! for long distance matching).
//!
//! `$ZSTD_BENCH_DICT=<bytes>` decodes with a dictionary instead: the dataset
//! cut into chunks of that size, each compressed on its own with a 110 KiB
//! dictionary trained on them, all decoded per iteration by one
//! decompressor holding the dictionary (lib `native*` or `c`).

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ds = args.get(1).map_or("source", |s| s);
    let level: i32 = args.get(2).map_or(3, |s| s.parse().unwrap());
    let lib = args.get(3).map_or("native", |s| s);
    let secs: f64 = args.get(4).map_or(5.0, |s| s.parse().unwrap());
    // `datasets()` (with `$ZSTD_BENCH_CORPUS`) or `short_offset_datasets()`.
    let data = if ds == "cat" {
        catenated_corpus()
    } else if ds == "cat-dup" {
        edited_copy(catenated_corpus())
    } else {
        zstd_bench::datasets()
            .into_iter()
            .chain(zstd_bench::short_offset_datasets())
            .find(|(n, _)| n == ds)
            .expect("dataset")
            .1
    };
    let params = CompressParams::from_env();
    // `$ZSTD_BENCH_DICT`: the dictionary and the chunks' frames.
    let dict_mode = std::env::var("ZSTD_BENCH_DICT").ok().map(|s| {
        let chunk: usize = s.parse().unwrap();
        let chunks: Vec<&[u8]> = data.chunks(chunk).collect();
        let dict = zstd::dict::from_samples(&chunks, 110 << 10).unwrap();
        let mut c = zstd::bulk::Compressor::with_dictionary(level, &dict).unwrap();
        let frames: Vec<Vec<u8>> = chunks.iter().map(|ch| c.compress(ch).unwrap()).collect();
        (dict, frames)
    });
    // `ZSTD_BENCH_COMP_CACHE=dir` keeps the compressed input on disk, so
    // repeated A/B runs at high levels don't recompress every time.
    let comp = match std::env::var("ZSTD_BENCH_COMP_CACHE") {
        _ if dict_mode.is_some() => Vec::new(),
        Ok(dir) => {
            let cut = match std::env::var("ZSTD_BENCH_CAT_MB") {
                Ok(mb) if ds.starts_with("cat") => format!(".{mb}m"),
                _ => String::new(),
            };
            let name = format!("{ds}{cut}.L{level}{}.zst", params.suffix());
            let path = std::path::Path::new(&dir).join(name);
            match std::fs::read(&path) {
                Ok(c) => c,
                Err(_) => {
                    let c = params.compress(&data, level);
                    std::fs::write(&path, &c).expect("write cache");
                    c
                }
            }
        }
        Err(_) => params.compress(&data, level),
    };
    zstd_native::__private::set_seq_loop(match lib {
        "native-long" => Some(true),
        "native-short" => Some(false),
        _ => None,
    });
    let mut out = vec![0u8; data.len() + 64];
    let mut n = 0u64;
    let t = Instant::now();
    zstd_native::__private::set_asm(match lib {
        "native-noasm" => Some(false),
        "native-asm" => Some(true),
        _ => None,
    });
    let (mut native, mut c) = match &dict_mode {
        Some((dict, _)) => (
            zstd_native::Decompressor::with_dictionary(zstd_native::Dictionary::new(dict).unwrap()),
            zstd::bulk::Decompressor::with_dictionary(dict).unwrap(),
        ),
        None => (
            zstd_native::Decompressor::new(),
            zstd::bulk::Decompressor::new().unwrap(),
        ),
    };
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
        if let Some((_, frames)) = &dict_mode {
            let mut o = 0;
            for f in frames {
                o += if lib.starts_with("native") {
                    native.decompress_to_slice(f, &mut out[o..]).unwrap()
                } else {
                    c.decompress_to_buffer(f, &mut out[o..]).unwrap()
                };
            }
            assert_eq!(o, data.len());
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
            "native" | "native-noasm" | "native-asm" | "native-long" | "native-short" => {
                native.decompress_to_slice(&comp, &mut out).unwrap()
            }
            "structured" => sd.decode_all(&comp, &mut out).unwrap(),
            _ => c.decompress_to_buffer(&comp, &mut out[..]).unwrap(),
        };
        assert_eq!(len, data.len());
        n += 1;
    }
    // Builds with debug assertions count the prefetching loop's blocks.
    if let Some(b) = zstd_native::__private::long_blocks() {
        eprintln!("prefetching loop: {b} blocks in {n} runs");
    }
    let el = t.elapsed().as_secs_f64();
    let comp_len = dict_mode
        .as_ref()
        .map_or(comp.len(), |(_, f)| f.iter().map(Vec::len).sum());
    println!(
        "{lib} {ds} L{level}: ratio {:.2}, {:.0} MiB/s",
        data.len() as f64 / comp_len as f64,
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

/// Dataset `cat`: the corpus files catenated in name order.
fn catenated_corpus() -> Vec<u8> {
    let dir = std::env::var("ZSTD_BENCH_CORPUS").expect("dataset cat needs ZSTD_BENCH_CORPUS");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let mut v = Vec::new();
    for f in files {
        v.extend_from_slice(&std::fs::read(f).unwrap());
    }
    if let Ok(mb) = std::env::var("ZSTD_BENCH_CAT_MB") {
        v.truncate(mb.parse::<usize>().unwrap() << 20);
    }
    v
}

/// `data`, then `data` again with 1-4 bytes replaced or inserted every 1-8
/// KiB.
fn edited_copy(mut data: Vec<u8>) -> Vec<u8> {
    let mut r = zstd_bench::Rng::new(7);
    let n = data.len();
    let mut p = 0;
    while p < n {
        let run = (1024 + r.below(7 << 10) as usize).min(n - p);
        data.extend_from_within(p..p + run);
        p += run;
        for _ in 0..1 + r.below(4) {
            if r.below(2) == 0 {
                data.push(r.next_u64() as u8);
            } else {
                p += 1;
                data.push(r.next_u64() as u8);
            }
        }
    }
    data
}

/// libzstd parameters beyond the level, from the environment.
struct CompressParams {
    wlog: Option<u32>,
    long: bool,
    workers: Option<u32>,
}

impl CompressParams {
    fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().map(|s| s.parse::<u32>().unwrap());
        CompressParams {
            wlog: var("ZSTD_BENCH_WLOG"),
            long: var("ZSTD_BENCH_LONG").is_some_and(|v| v != 0),
            workers: var("ZSTD_BENCH_WORKERS"),
        }
    }

    /// Cache file name suffix (the worker count changes the frame too).
    fn suffix(&self) -> String {
        let mut s = String::new();
        if let Some(w) = self.wlog {
            s += &format!(".w{w}");
        }
        if self.long {
            s += ".long";
        }
        if let Some(n) = self.workers {
            s += &format!(".t{n}");
        }
        s
    }

    fn compress(&self, data: &[u8], level: i32) -> Vec<u8> {
        use zstd::zstd_safe::{CCtx, CParameter};
        if self.wlog.is_none() && !self.long && self.workers.is_none() {
            return zstd_bench::c_compress(data, level);
        }
        let mut c = CCtx::create();
        c.set_parameter(CParameter::CompressionLevel(level))
            .unwrap();
        if let Some(w) = self.wlog {
            c.set_parameter(CParameter::WindowLog(w)).unwrap();
        }
        if self.long {
            c.set_parameter(CParameter::EnableLongDistanceMatching(true))
                .unwrap();
        }
        if let Some(n) = self.workers {
            c.set_parameter(CParameter::NbWorkers(n)).unwrap();
        }
        let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
        c.compress2(&mut out, data).unwrap();
        out
    }
}
