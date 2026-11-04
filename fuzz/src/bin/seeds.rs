//! Writes seed corpora for the `decode` and `stream_decode` targets: small
//! libzstd frames of every kind (levels, checksums, dictionaries, several
//! frames, skippable frames, every block type), so the fuzzer starts from
//! valid input instead of discovering the format.
//!
//! `cargo +nightly run --bin seeds -- corpus` (in `fuzz/`) writes
//! `corpus/decode/*` and `corpus/stream_decode/*`.

use std::fs;
use std::path::Path;

use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd::zstd_safe::{CCtx, CParameter, InBuffer, OutBuffer};
use zstd_native_fuzz::{Rng, samples, trained_dict};

fn frame(
    data: &[u8],
    level: i32,
    checksum: bool,
    dict: Option<&[u8]>,
    flush_every: usize,
) -> Vec<u8> {
    let mut c = CCtx::create();
    c.set_parameter(CParameter::CompressionLevel(level))
        .unwrap();
    c.set_parameter(CParameter::ChecksumFlag(checksum)).unwrap();
    if let Some(d) = dict {
        c.load_dictionary(d).unwrap();
    }
    c.set_pledged_src_size(Some(data.len() as u64)).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0;
    loop {
        let end = if flush_every == 0 {
            data.len()
        } else {
            (pos + flush_every).min(data.len())
        };
        let op = if end == data.len() {
            ZSTD_EndDirective::ZSTD_e_end
        } else {
            ZSTD_EndDirective::ZSTD_e_flush
        };
        let mut input = InBuffer::around(&data[pos..end]);
        loop {
            let mut output = OutBuffer::around(&mut buf[..]);
            let rem = c.compress_stream2(&mut output, &mut input, op).unwrap();
            let n = output.pos();
            out.extend_from_slice(&buf[..n]);
            if input.pos() == end - pos && rem == 0 {
                break;
            }
        }
        pos = end;
        if pos == data.len() {
            break;
        }
    }
    out
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "corpus".into());
    let dir = Path::new(&dir);
    let text: Vec<u8> = samples(3, 2000).concat();
    let mut r = Rng::new(9);
    let numeric: Vec<u8> = (0..20_000u32)
        .flat_map(|i| (i / 7 + (r.below(8) as u32)).to_le_bytes())
        .collect();
    let random: Vec<u8> = (0..70_000).map(|_| r.next_u64() as u8).collect();
    let rle = vec![b'a'; 70_000];
    let inputs: [(&str, &[u8]); 4] = [
        ("text", &text),
        ("numeric", &numeric),
        ("random", &random),
        ("rle", &rle),
    ];
    let mut seeds: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, data) in inputs {
        for len in [0usize, 1, 200, 5000, 70_000] {
            let data = &data[..len.min(data.len())];
            for level in [-5, 1, 3, 7, 12, 17, 22] {
                let checksum = level % 2 == 1;
                seeds.push((
                    format!("{name}_{len}_l{level}"),
                    frame(data, level, checksum, None, 0),
                ));
            }
            seeds.push((
                format!("{name}_{len}_dict"),
                frame(data, 3, true, Some(trained_dict()), 0),
            ));
            seeds.push((
                format!("{name}_{len}_flush"),
                frame(data, 5, false, None, 777),
            ));
        }
    }
    // Several frames and skippable frames.
    let mut multi = frame(&text[..3000], 1, true, None, 0);
    multi.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
    multi.extend_from_slice(&5u32.to_le_bytes());
    multi.extend_from_slice(b"skip!");
    multi.extend_from_slice(&frame(&numeric[..3000], 9, false, None, 100));
    multi.extend_from_slice(&frame(&[], 3, true, None, 0));
    seeds.push(("multi".into(), multi));

    for target in ["decode", "stream_decode"] {
        let d = dir.join(target);
        fs::create_dir_all(&d).unwrap();
        for (name, bytes) in &seeds {
            fs::write(d.join(format!("seed_{name}")), bytes).unwrap();
        }
    }
    println!("{} seeds in {}", seeds.len(), dir.display());
}
