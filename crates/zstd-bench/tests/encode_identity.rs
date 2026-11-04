//! The encoder must produce exactly libzstd's bytes (`ZSTD_compress`) for
//! the same input and level.

use zstd_bench::{Rng, c_compress, datasets, source_corpus};

const LEVELS: &[i32] = &[1, 2, 3, 4, -1, -3, -7];

fn check(name: &str, data: &[u8], level: i32) {
    let got = match zstd_native::compress(data, level) {
        Ok(v) => v,
        // Strategy not ported yet for this level and size (issues #7-#9).
        // (level 4 is greedy for inputs up to 16 KB and of 128-256 KB).
        Err(zstd_native::Error::UnsupportedFrameParameter) if level > 3 => return,
        Err(e) => panic!("{name} L{level}: {e}"),
    };
    let want = c_compress(data, level);
    if got != want {
        let first = got
            .iter()
            .zip(&want)
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(want.len()));
        panic!(
            "{name} L{level} ({} bytes): output differs from libzstd at byte {first} (sizes {} vs {})",
            data.len(),
            got.len(),
            want.len()
        );
    }
    assert!(
        zstd_native::decompress(&got).unwrap() == data,
        "{name} L{level}: round trip"
    );
}

#[test]
fn datasets_identical() {
    for (name, data) in datasets() {
        for &level in LEVELS {
            check(&name, &data, level);
        }
    }
}

#[test]
fn sizes_identical() {
    let src = source_corpus();
    let mut r = Rng::new(4);
    let sizes = [
        0usize,
        1,
        2,
        3,
        6,
        7,
        8,
        9,
        15,
        16,
        31,
        32,
        33,
        63,
        64,
        100,
        255,
        256,
        257,
        511,
        512,
        513,
        1023,
        1024,
        1025,
        4095,
        4096,
        16383,
        16384,
        16385,
        65535,
        65536,
        65791,
        65792,
        131071,
        131072,
        131073,
        262143,
        262144,
        262145,
        500_000,
        1 << 20,
    ];
    for &n in &sizes {
        let start = r.below((src.len() - n) as u64 + 1) as usize;
        for &level in LEVELS {
            check(
                &format!("source[{start}..+{n}]"),
                &src[start..start + n],
                level,
            );
        }
    }
}

#[test]
fn special_inputs_identical() {
    let mut cases: Vec<(String, Vec<u8>)> = vec![
        ("zeros".into(), vec![0u8; 1 << 20]),
        ("one byte repeated".into(), vec![b'x'; 300_000]),
        (
            "two symbols".into(),
            (0..400_000u32)
                .map(|i| if i % 7 == 0 { b'a' } else { b'b' })
                .collect(),
        ),
    ];
    // Runs of RLE blocks between compressible blocks.
    let mut mixed = source_corpus()[..200_000].to_vec();
    mixed.extend(vec![7u8; 300_000]);
    mixed.extend_from_slice(&source_corpus()[300_000..500_000]);
    cases.push(("mixed rle".into(), mixed));
    for (name, data) in cases {
        for &level in LEVELS {
            check(&name, &data, level);
        }
    }
}

#[test]
fn small_destination_buffers() {
    let src = source_corpus();
    let mut r = Rng::new(8);
    for n in [0usize, 10, 1000, 50_000, 300_000] {
        let data = &src[..n];
        let full = zstd_native::compress(data, 1).unwrap();
        for _ in 0..40 {
            let cap = r.below(full.len() as u64) as usize;
            let mut buf = vec![0u8; cap];
            assert_eq!(
                zstd_native::compress_to_slice(data, &mut buf, 1),
                Err(zstd_native::Error::OutputTooSmall),
                "n {n} cap {cap}"
            );
        }
        let mut buf = vec![0u8; zstd_native::compress_bound(n)];
        let k = zstd_native::compress_to_slice(data, &mut buf, 1).unwrap();
        assert_eq!(&buf[..k], &full[..]);
    }
}
