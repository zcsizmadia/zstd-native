//! zstd-native must decode everything C libzstd 1.5.7 produces, exactly.

use std::io::Write;

use zstd_bench::{
    Rng, c_compress, datasets, log_corpus, numeric_corpus, random_corpus, source_corpus,
};

fn check(name: &str, data: &[u8], compressed: &[u8]) {
    let got = zstd_native::decompress(compressed).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert!(
        got == data,
        "{name}: output differs (len {} vs {})",
        got.len(),
        data.len()
    );
    let mut buf = vec![0u8; data.len()];
    let n = zstd_native::decompress_to_slice(compressed, &mut buf)
        .unwrap_or_else(|e| panic!("{name} slice: {e}"));
    assert_eq!(n, data.len(), "{name} slice");
    assert!(buf == data, "{name}: slice output differs");
}

const LEVELS: &[i32] = &[-5, -1, 1, 2, 3, 4, 5, 6, 7, 9, 12, 15, 16, 19, 22];

#[test]
fn levels_and_datasets() {
    for (name, data) in datasets() {
        let data = &data[..data.len().min(3 << 20)];
        for &level in LEVELS {
            if level >= 16 && data.len() > (1 << 20) {
                continue; // slow C levels on big inputs
            }
            check(&format!("{name} L{level}"), data, &c_compress(data, level));
        }
    }
}

#[test]
fn sizes() {
    let src = source_corpus();
    let mut r = Rng::new(99);
    for &len in &[
        0usize, 1, 2, 5, 15, 16, 17, 31, 32, 33, 63, 64, 100, 255, 256, 257, 1000, 4096, 65535,
        65536, 131071, 131072, 131073, 300_000,
    ] {
        let start = r.below((src.len() - len.min(src.len())) as u64 + 1) as usize;
        let data = &src[start..start + len.min(src.len())];
        for level in [1, 3, 9, 19] {
            check(
                &format!("len {len} L{level}"),
                data,
                &c_compress(data, level),
            );
        }
    }
}

fn compress_with(
    data: &[u8],
    level: i32,
    f: impl FnOnce(&mut zstd::stream::write::Encoder<'static, Vec<u8>>),
) -> Vec<u8> {
    let mut enc = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
    f(&mut enc);
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

#[test]
fn checksum_and_streaming_frames() {
    let data = log_corpus(2 << 20);
    for level in [1, 3, 7, 19] {
        // Streaming encoder: unknown content size, checksum, many blocks.
        let c = compress_with(&data, level, |e| {
            e.include_checksum(true).unwrap();
            e.include_contentsize(false).unwrap();
        });
        check(&format!("streamed checksum L{level}"), &data, &c);
    }
    // A corrupted checksum must be reported.
    let mut c = compress_with(&data[..1000], 3, |e| e.include_checksum(true).unwrap());
    let n = c.len();
    c[n - 1] ^= 1;
    assert_eq!(
        zstd_native::decompress(&c),
        Err(zstd_native::Error::ChecksumMismatch)
    );
}

#[test]
fn long_windows() {
    // Window larger than the block, matches far back (long distance matching).
    let mut data = random_corpus(3 << 20);
    let copy = data[..1 << 20].to_vec();
    data.extend_from_slice(&copy);
    for level in [3, 19] {
        let mut enc = zstd::bulk::Compressor::new(level).unwrap();
        enc.long_distance_matching(true).unwrap();
        enc.window_log(23).unwrap();
        let c = enc.compress(&data).unwrap();
        assert!(c.len() < data.len() - (1 << 19), "LDM should find the copy");
        check(&format!("ldm L{level}"), &data, &c);
    }
}

#[test]
fn concatenated_and_skippable_frames() {
    let a = numeric_corpus(300_000);
    let b = log_corpus(200_000);
    let mut c = c_compress(&a, 3);
    // Skippable frame between them.
    c.extend_from_slice(&0x184D_2A53u32.to_le_bytes());
    c.extend_from_slice(&5u32.to_le_bytes());
    c.extend_from_slice(b"skip!");
    c.extend_from_slice(&c_compress(&b, 5));
    let mut want = a.clone();
    want.extend_from_slice(&b);
    check("concat", &want, &c);
    // Trailing garbage shorter than a frame header is an error, as in libzstd.
    c.extend_from_slice(&[1, 2, 3]);
    assert_eq!(
        zstd_native::decompress(&c),
        Err(zstd_native::Error::Truncated)
    );
}

#[test]
fn dictionaries() {
    // Train a dictionary on samples of the log corpus.
    let corpus = log_corpus(2 << 20);
    let samples: Vec<&[u8]> = corpus.chunks(2000).collect();
    let dict = zstd::dict::from_samples(&samples, 64 << 10).unwrap();
    let d = zstd_native::Dictionary::new(&dict).unwrap();
    assert_ne!(d.id(), 0);
    let mut dec = zstd_native::Decompressor::with_dictionary(d);
    for (i, s) in samples.iter().take(200).enumerate() {
        for level in [1, 3, 9, 19] {
            let mut c = zstd::bulk::Compressor::with_dictionary(level, &dict).unwrap();
            let comp = c.compress(s).unwrap();
            let got = dec
                .decompress(&comp)
                .unwrap_or_else(|e| panic!("sample {i} L{level}: {e}"));
            assert!(got == *s, "sample {i} L{level}");
        }
    }
    // Without the dictionary: an error, not garbage.
    let mut c = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
    let comp = c.compress(samples[0]).unwrap();
    assert_eq!(
        zstd_native::decompress(&comp),
        Err(zstd_native::Error::DictionaryMismatch)
    );

    // Raw content dictionary.
    let raw = &corpus[..100_000];
    let mut c = zstd::bulk::Compressor::new(3).unwrap();
    c.set_dictionary(3, raw).unwrap();
    let mut dec = zstd_native::Decompressor::with_dictionary(zstd_native::Dictionary::raw(raw));
    for s in samples.iter().skip(100).take(50) {
        let comp = c.compress(s).unwrap();
        assert!(dec.decompress(&comp).unwrap() == *s);
    }
}

#[test]
fn decompressor_reuse() {
    let mut dec = zstd_native::Decompressor::new();
    for (name, data) in datasets() {
        let data = &data[..data.len().min(500_000)];
        for level in [1, 6] {
            let c = c_compress(data, level);
            assert!(dec.decompress(&c).unwrap() == data, "{name} L{level}");
        }
    }
}

#[test]
fn output_too_small() {
    let data = log_corpus(100_000);
    let c = c_compress(&data, 3);
    let mut buf = vec![0u8; data.len() - 1];
    assert_eq!(
        zstd_native::decompress_to_slice(&c, &mut buf),
        Err(zstd_native::Error::OutputTooSmall)
    );
}

#[test]
fn asm_and_rust_huffman_loops_agree() {
    // Same results with the asm Huffman loops on and off (where they exist).
    eprintln!(
        "selected paths: {:?}",
        zstd_native::__private::selected_paths()
    );
    let datasets = [
        source_corpus(),
        log_corpus(1 << 20),
        numeric_corpus(1 << 20),
    ];
    for asm in [false, true] {
        zstd_native::__private::set_asm(Some(asm));
        eprintln!("asm={asm}: {:?}", zstd_native::__private::selected_paths());
        for (i, data) in datasets.iter().enumerate() {
            for level in [1, 3, 19] {
                check(
                    &format!("asm={asm} dataset {i} L{level}"),
                    data,
                    &c_compress(data, level),
                );
            }
        }
    }
    zstd_native::__private::set_asm(None);
}
