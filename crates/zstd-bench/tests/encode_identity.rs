//! The encoder must produce exactly libzstd's bytes (`ZSTD_compress`) for
//! the same input and level.

use zstd::zstd_safe::{CParameter, ParamSwitch};
use zstd_bench::{Rng, c_compress, datasets, source_corpus};

const LEVELS: &[i32] = &[
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, -1, -3, -7,
];

/// From this level up (btultra and btultra2 with deep searches), the large
/// inputs are cut to [`SLOW_INPUT_MAX`] bytes to keep the test time
/// reasonable: still several blocks, over 256 KB (the largest parameter
/// table).
const SLOW_LEVEL: i32 = 17;
const SLOW_INPUT_MAX: usize = 600_000;

/// `data`, cut to [`SLOW_INPUT_MAX`] bytes for the slow levels.
fn for_level(data: &[u8], level: i32) -> &[u8] {
    if level >= SLOW_LEVEL {
        &data[..data.len().min(SLOW_INPUT_MAX)]
    } else {
        data
    }
}

fn check(name: &str, data: &[u8], level: i32) {
    let got = zstd_native::compress(data, level).unwrap_or_else(|e| panic!("{name} L{level}: {e}"));
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
            check(&name, for_level(&data, level), level);
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
    cases.push(("mixed rle".into(), mixed.clone()));
    // The same, short enough for the slow levels to see all of it.
    let mut short_mixed = source_corpus()[..150_000].to_vec();
    short_mixed.extend(vec![7u8; 200_000]);
    short_mixed.extend_from_slice(&source_corpus()[300_000..450_000]);
    cases.push(("short mixed rle".into(), short_mixed));
    // Incompressible stretches between text (raw partitions of split
    // blocks, repcodes across them).
    let mut r = Rng::new(21);
    let mut patchy = Vec::new();
    for i in 0..8 {
        let at = 100_000 * i;
        patchy.extend_from_slice(&source_corpus()[at..at + 40_000]);
        patchy.extend((0..10_000 + 3_000 * i).map(|_| r.below(256) as u8));
    }
    cases.push(("patchy".into(), patchy));
    for (name, data) in cases {
        for &level in LEVELS {
            check(&name, for_level(&data, level), level);
        }
    }
}

/// libzstd with its post-block splitter (`ZSTD_c_splitAfterSequences`)
/// disabled.
fn c_compress_unsplit(data: &[u8], level: i32) -> Vec<u8> {
    let mut c = zstd::bulk::Compressor::new(level).unwrap();
    c.set_parameter(CParameter::UseBlockSplitter(ParamSwitch::Disable))
        .unwrap();
    c.compress(data).unwrap()
}

/// The post-block splitter (btopt and above, `windowLog >= 17`): inputs
/// where libzstd's output depends on it, i.e. where blocks really get
/// split.
#[test]
fn block_splitter_identical() {
    let src = source_corpus();
    let numeric = datasets().swap_remove(2).1;
    let inputs: [(&str, &[u8]); 6] = [
        ("source 100K", &src[..100_000]),
        ("source 131072", &src[50_000..181_072]),
        ("source 200K", &src[400_000..600_000]),
        ("source 700K", &src[900_000..1_600_000]),
        ("numeric 300K", &numeric[..300_000]),
        // windowLog 16: no splitter
        ("source 60K", &src[..60_000]),
    ];
    let mut splits = 0;
    for (name, data) in inputs {
        for level in [13, 15, 16, 17, 18, 19, 20, 21, 22] {
            check(name, data, level);
            if c_compress(data, level) != c_compress_unsplit(data, level) {
                splits += 1;
            }
        }
    }
    // Most of these frames have blocks split in partitions.
    assert!(splits >= 30, "only {splits} frames with split blocks");
}

/// Level 22 on more than 64 MiB: libzstd enables long distance matching
/// by itself (see `ldm_encode.rs` for more). All zeros and a short period:
/// repetitions the long distance matcher skips over.
#[test]
fn ldm_level22() {
    let mut big = vec![0u8; (64 << 20) + 1];
    for (i, b) in big[40 << 20..].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let want = c_compress(&big, 22);
    assert!(zstd_native::compress(&big, 22).unwrap() == want);
    assert!(zstd_native::Compressor::new(22).compress(&big).unwrap() == want);
    let mut dst = vec![0u8; zstd_native::compress_bound(big.len())];
    let n = zstd_native::compress_to_slice(&big, &mut dst, 22).unwrap();
    assert!(dst[..n] == want[..]);
    assert_eq!(
        zstd_native::compress_to_slice(&big, &mut dst[..100], 22),
        Err(zstd_native::Error::OutputTooSmall)
    );
    assert!(zstd_native::decompress(&want).unwrap() == big);
}

#[test]
fn small_destination_buffers() {
    let src = source_corpus();
    let mut r = Rng::new(8);
    for n in [0usize, 10, 1000, 50_000, 300_000] {
        let data = &src[..n];
        for level in [1, 19] {
            let full = zstd_native::compress(data, level).unwrap();
            for _ in 0..20 {
                let cap = r.below(full.len() as u64) as usize;
                let mut buf = vec![0u8; cap];
                assert_eq!(
                    zstd_native::compress_to_slice(data, &mut buf, level),
                    Err(zstd_native::Error::OutputTooSmall),
                    "n {n} cap {cap} L{level}"
                );
            }
            let mut buf = vec![0u8; zstd_native::compress_bound(n)];
            let k = zstd_native::compress_to_slice(data, &mut buf, level).unwrap();
            assert_eq!(&buf[..k], &full[..]);
        }
    }
}

/// One reused `zstd_native::Compressor` against one reused libzstd context
/// (`ZSTD_compress2` on the same `ZSTD_CCtx`): the row match finder's hash
/// salt changes from frame to frame (`ZSTD_advanceHashSalt`, mixing in the
/// hashes searched so far), and its tag table is not cleared; the optimal
/// parser's statistics restart with each frame.
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
    for level in 4..=22 {
        let mut native = zstd_native::Compressor::new(level);
        let mut c = zstd::bulk::Compressor::new(level).unwrap();
        for (i, data) in inputs.iter().enumerate() {
            let data = for_level(data, level);
            let got = native.compress(data).unwrap();
            let want = c.compress(data).unwrap();
            compare(&format!("L{level}"), i, data.len(), got, want);
        }
    }
    // Switching levels on the same contexts, including frames that don't
    // use the row match finder (which leave the salt alone).
    let mut native = zstd_native::Compressor::new(1);
    let mut c = zstd::bulk::Compressor::new(1).unwrap();
    let mut k = 0;
    for round in 0..3 {
        for &level in &[6, 3, 12, 5, 16, 7, 19, 1, 9, 22, 13, 8, 17] {
            let data = for_level(inputs[(k * 7 + round) % inputs.len()], level);
            k += 1;
            native.set_level(level);
            c.set_compression_level(level).unwrap();
            let want = c.compress(data).unwrap();
            let got = native.compress(data).unwrap();
            compare(&format!("switching, L{level}"), k, data.len(), got, want);
        }
    }
}
