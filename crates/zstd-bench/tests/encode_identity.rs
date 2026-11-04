//! The encoder must produce exactly libzstd's bytes (`ZSTD_compress`) for
//! the same input and level.

use zstd_bench::{Rng, c_compress, datasets, source_corpus};

const LEVELS: &[i32] = &[
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 19, -1, -3, -7,
];

/// The highest level whose strategy is ported (`btlazy2` or below) for an
/// input of `n` bytes (libzstd's clevels.h: the table depends on the size).
fn max_supported_level(n: usize) -> i32 {
    if n <= 16 << 10 {
        10
    } else if n <= 256 << 10 {
        12
    } else {
        15
    }
}

fn check(name: &str, data: &[u8], level: i32) {
    let got = match zstd_native::compress(data, level) {
        Ok(v) => v,
        // btopt and above are not ported yet (issue #9).
        Err(zstd_native::Error::UnsupportedFrameParameter)
            if level > max_supported_level(data.len()) =>
        {
            return;
        }
        Err(e) => panic!("{name} L{level}: {e}"),
    };
    assert!(
        level <= max_supported_level(data.len()),
        "{name} L{level}: expected UnsupportedFrameParameter"
    );
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

/// One reused `zstd_native::Compressor` against one reused libzstd context
/// (`ZSTD_compress2` on the same `ZSTD_CCtx`): the row match finder's hash
/// salt changes from frame to frame (`ZSTD_advanceHashSalt`, mixing in the
/// hashes searched so far), and its tag table is not cleared.
#[test]
fn reused_context_identical() {
    let src = source_corpus();
    let mut r = Rng::new(12);
    // Sizes over 16 KB use the row match finder at greedy..lazy2 levels,
    // smaller ones the hash chain (and leave the salt alone).
    let sizes = [
        100_000usize,
        1 << 20,
        20_000,
        300_000,
        5_000,
        200_000,
        100_000,
        16_385,
        700,
        2_000_000,
    ];
    let inputs: Vec<&[u8]> = sizes
        .iter()
        .map(|&n| {
            let start = r.below((src.len() - n) as u64 + 1) as usize;
            &src[start..start + n]
        })
        .collect();
    let compare = |what: &str, i: usize, len: usize, got: Vec<u8>, want: Vec<u8>| {
        if got != want {
            let first = got.iter().zip(&want).position(|(a, b)| a != b);
            panic!(
                "{what}, frame {i} ({} bytes): output differs from libzstd at byte {first:?} (sizes {} vs {})",
                len,
                got.len(),
                want.len()
            );
        }
    };
    for level in [4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15] {
        let mut native = zstd_native::Compressor::new(level);
        let mut c = zstd::bulk::Compressor::new(level).unwrap();
        for (i, data) in inputs.iter().enumerate() {
            if level > max_supported_level(data.len()) {
                continue; // btopt: the libzstd context still compresses it
            }
            let got = native.compress(data).unwrap();
            let want = c.compress(data).unwrap();
            compare(&format!("L{level}"), i, data.len(), got, want);
        }
    }
    // Switching levels on the same contexts, including frames that don't
    // use the row match finder (and frames libzstd compresses with btopt,
    // which leave the salt alone too).
    let mut native = zstd_native::Compressor::new(1);
    let mut c = zstd::bulk::Compressor::new(1).unwrap();
    let mut k = 0;
    for round in 0..3 {
        for &level in &[6, 3, 12, 5, 16, 7, 1, 9, 13, 8] {
            let data = inputs[(k * 7 + round) % inputs.len()];
            k += 1;
            native.set_level(level);
            c.set_compression_level(level).unwrap();
            let want = c.compress(data).unwrap();
            if level > max_supported_level(data.len()) {
                continue;
            }
            let got = native.compress(data).unwrap();
            compare(&format!("switching, L{level}"), k, data.len(), got, want);
        }
    }
}
