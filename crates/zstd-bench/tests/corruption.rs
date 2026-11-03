//! Corrupt input must never panic or misbehave, and anything zstd-native
//! accepts, C libzstd must accept with the same output (zstd-native is at
//! least as strict; libzstd's fast Huffman path doesn't check that streams
//! are fully consumed, so it accepts some inputs zstd-native rejects).
//!
//! Run with debug assertions (`--profile ci`) so the `unsafe` hot paths'
//! assertions are checked. `ZSTD_NATIVE_CORRUPTION_ITERS` scales it up.

use zstd_bench::{Rng, c_compress, log_corpus, numeric_corpus, source_corpus};

fn c_decompress(src: &[u8], cap: usize) -> Option<Vec<u8>> {
    zstd::bulk::Decompressor::new().unwrap().decompress(src, cap).ok()
}

#[test]
fn mutated_frames() {
    let iters: usize = std::env::var("ZSTD_NATIVE_CORRUPTION_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    let src = source_corpus();
    let inputs: Vec<Vec<u8>> = vec![
        src[..20_000].to_vec(),
        src[50_000..51_000].to_vec(),
        log_corpus(30_000),
        numeric_corpus(30_000),
        b"tiny".to_vec(),
    ];
    let mut frames = Vec::new();
    for d in &inputs {
        for level in [1, 3, 9, 19] {
            frames.push((d.len(), c_compress(d, level)));
        }
    }
    let mut r = Rng::new(2024);
    let mut accepted = 0;
    let mut dec = zstd_native::Decompressor::new();
    for i in 0..iters {
        let (orig_len, f) = &frames[i % frames.len()];
        let mut m = f.clone();
        match r.below(4) {
            0 => {
                // Flip a few bits.
                for _ in 0..1 + r.below(4) {
                    let p = r.below(m.len() as u64) as usize;
                    m[p] ^= 1 << r.below(8);
                }
            }
            1 => {
                // Overwrite a byte (skip the magic number half the time).
                let p = r.below(m.len() as u64) as usize;
                m[p] = r.next_u64() as u8;
            }
            2 => {
                // Truncate.
                let n = r.below(m.len() as u64) as usize;
                m.truncate(n);
            }
            _ => {
                // Duplicate a slice inside the frame.
                let a = r.below(m.len() as u64) as usize;
                let b = (a + r.below(64) as usize).min(m.len());
                let piece = m[a..b].to_vec();
                let p = r.below(m.len() as u64) as usize;
                m.splice(p..p, piece);
            }
        }
        let native = dec.decompress(&m);
        let mut buf = vec![0u8; orig_len + 100];
        let native_slice = dec.decompress_to_slice(&m, &mut buf);
        if let Ok(v) = &native {
            accepted += 1;
            let c = c_decompress(&m, v.len().max(1) + 1000);
            assert_eq!(c.as_ref(), Some(v), "iteration {i}: zstd-native accepted what libzstd rejects or decodes differently");
            if let Ok(n) = native_slice {
                assert_eq!(&buf[..n], &v[..], "iteration {i}: slice and vec decoders differ");
            }
        }
    }
    eprintln!("{accepted} of {iters} mutated inputs decoded");
}
