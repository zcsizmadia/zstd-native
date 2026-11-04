//! Block pre-splitting: a port of libzstd 1.5.7's `compress/zstd_preSplit.c`
//! (`ZSTD_splitBlock` and its fingerprinting) and of `ZSTD_optimalBlockSize`
//! (`compress/zstd_compress.c`), which decides when to call it.
//!
//! libzstd hashes 2 bytes with `MEM_read16` (native byte order); this port
//! reads them little-endian, i.e. it matches libzstd on little-endian
//! machines (the output of libzstd itself differs on big-endian ones).

use super::params::Strategy;

/// `BLOCKSIZE_MIN` (unused by libzstd 1.5.7's code, kept for reference).
#[allow(dead_code)]
const BLOCKSIZE_MIN: usize = 3500;
const THRESHOLD_PENALTY_RATE: u64 = 16;
const THRESHOLD_BASE: u64 = THRESHOLD_PENALTY_RATE - 2;
const THRESHOLD_PENALTY: i32 = 3;

const HASHLENGTH: usize = 2;
const HASHLOG_MAX: u32 = 10;
const HASHTABLESIZE: usize = 1 << HASHLOG_MAX;
const KNUTH: u32 = 0x9e37_79b9;

const KB: usize = 1 << 10;
/// `CHUNKSIZE`.
const CHUNKSIZE: usize = 8 << 10;
/// `SEGMENT_SIZE` (`ZSTD_splitBlock_fromBorders`).
const SEGMENT_SIZE: usize = 512;

/// `hash2()`: for `hash_log > 8`, hashes 2 bytes; for 8, takes the byte.
#[inline(always)]
fn hash2(p: &[u8], hash_log: u32) -> usize {
    debug_assert!(hash_log >= 8);
    if hash_log == 8 {
        return p[0] as usize;
    }
    debug_assert!(hash_log <= HASHLOG_MAX);
    let v = u32::from(u16::from_le_bytes([p[0], p[1]]));
    (v.wrapping_mul(KNUTH) >> (32 - hash_log)) as usize
}

/// `Fingerprint`.
#[derive(Clone, Debug)]
pub(crate) struct Fingerprint {
    events: [u32; HASHTABLESIZE],
    nb_events: usize,
}

impl Fingerprint {
    const fn new() -> Self {
        Self {
            events: [0; HASHTABLESIZE],
            nb_events: 0,
        }
    }
}

/// `FPStats`: the splitter's workspace (libzstd's
/// `ZSTD_SLIPBLOCK_WORKSPACESIZE`). Keep one per compressor to avoid
/// re-allocating it per block.
#[derive(Clone, Debug)]
pub(crate) struct FpStats {
    past_events: Fingerprint,
    new_events: Fingerprint,
}

impl FpStats {
    /// A zeroed workspace.
    pub(crate) const fn new() -> Self {
        Self {
            past_events: Fingerprint::new(),
            new_events: Fingerprint::new(),
        }
    }

    /// `initStats()`.
    fn init(&mut self) {
        *self = Self::new();
    }
}

impl Default for FpStats {
    fn default() -> Self {
        Self::new()
    }
}

/// `addEvents_generic()`.
#[inline(always)]
fn add_events(fp: &mut Fingerprint, src: &[u8], sampling_rate: usize, hash_log: u32) {
    debug_assert!(src.len() >= HASHLENGTH);
    let limit = src.len() - HASHLENGTH + 1;
    let mut n = 0;
    while n < limit {
        fp.events[hash2(&src[n..], hash_log)] += 1;
        n += sampling_rate;
    }
    fp.nb_events += limit / sampling_rate;
}

/// `recordFingerprint_generic()`.
#[inline(always)]
fn record_fingerprint(fp: &mut Fingerprint, src: &[u8], sampling_rate: usize, hash_log: u32) {
    fp.events[..1 << hash_log].fill(0);
    fp.nb_events = 0;
    add_events(fp, src, sampling_rate, hash_log);
}

/// `abs64()`.
#[inline(always)]
fn abs64(s: i64) -> u64 {
    s.unsigned_abs()
}

/// `fpDistance()`.
fn fp_distance(fp1: &Fingerprint, fp2: &Fingerprint, hash_log: u32) -> u64 {
    debug_assert!(hash_log <= HASHLOG_MAX);
    let mut distance = 0u64;
    for n in 0..1usize << hash_log {
        distance = distance.wrapping_add(abs64(
            (i64::from(fp1.events[n]) * fp2.nb_events as i64)
                .wrapping_sub(i64::from(fp2.events[n]) * fp1.nb_events as i64),
        ));
    }
    distance
}

/// `compareFingerprints()`: whether `newfp` is "too different" from `reference`.
fn compare_fingerprints(
    reference: &Fingerprint,
    newfp: &Fingerprint,
    penalty: i32,
    hash_log: u32,
) -> bool {
    debug_assert!(reference.nb_events > 0);
    debug_assert!(newfp.nb_events > 0);
    let p50 = reference.nb_events as u64 * newfp.nb_events as u64;
    let deviation = fp_distance(reference, newfp, hash_log);
    let threshold =
        p50 * (THRESHOLD_BASE as i64 + i64::from(penalty)) as u64 / THRESHOLD_PENALTY_RATE;
    deviation >= threshold
}

/// `mergeEvents()`.
fn merge_events(acc: &mut Fingerprint, newfp: &Fingerprint) {
    for (a, n) in acc.events.iter_mut().zip(newfp.events.iter()) {
        *a += *n;
    }
    acc.nb_events += newfp.nb_events;
}

/// `ZSTD_splitBlock_byChunks()`: levels 0..=3 sample every 43rd, 11th,
/// 5th or every position, with 8, 9, 10 and 10 hash bits.
fn split_block_by_chunks(block: &[u8], level: usize, fpstats: &mut FpStats) -> usize {
    const SAMPLING: [usize; 4] = [43, 11, 5, 1];
    const HASH_PARAMS: [u32; 4] = [8, 9, 10, 10];
    debug_assert!(level <= 3);
    let rate = SAMPLING[level];
    let hash_log = HASH_PARAMS[level];
    let block_size = block.len();
    debug_assert!(block_size == 128 << 10);
    let mut penalty = THRESHOLD_PENALTY;

    fpstats.init();
    record_fingerprint(
        &mut fpstats.past_events,
        &block[..CHUNKSIZE],
        rate,
        hash_log,
    );
    let mut pos = CHUNKSIZE;
    while pos <= block_size - CHUNKSIZE {
        record_fingerprint(
            &mut fpstats.new_events,
            &block[pos..pos + CHUNKSIZE],
            rate,
            hash_log,
        );
        if compare_fingerprints(&fpstats.past_events, &fpstats.new_events, penalty, hash_log) {
            return pos;
        }
        merge_events(&mut fpstats.past_events, &fpstats.new_events);
        if penalty > 0 {
            penalty -= 1;
        }
        pos += CHUNKSIZE;
    }
    debug_assert!(pos == block_size);
    block_size
}

/// `HIST_add()`.
fn hist_add(count: &mut [u32; HASHTABLESIZE], src: &[u8]) {
    for &b in src {
        count[b as usize] += 1;
    }
}

/// `ZSTD_splitBlock_fromBorders()`: compares byte histograms of the first
/// and last 512 bytes, and of the middle 512 bytes, to split at 32, 64 or
/// 96 KB (or not at all).
///
/// libzstd places the middle fingerprint inside the workspace, overlapping
/// the upper half of `pastEvents.events` and part of `newEvents.events`;
/// the entries it uses (0..256) start out zero and the entries it
/// overwrites are never read again, so a separate zeroed fingerprint is
/// equivalent.
fn split_block_from_borders(block: &[u8], fpstats: &mut FpStats) -> usize {
    let block_size = block.len();
    debug_assert!(block_size == 128 << 10);
    fpstats.init();
    hist_add(&mut fpstats.past_events.events, &block[..SEGMENT_SIZE]);
    hist_add(
        &mut fpstats.new_events.events,
        &block[block_size - SEGMENT_SIZE..],
    );
    fpstats.past_events.nb_events = SEGMENT_SIZE;
    fpstats.new_events.nb_events = SEGMENT_SIZE;
    if !compare_fingerprints(&fpstats.past_events, &fpstats.new_events, 0, 8) {
        return block_size;
    }

    let mut middle = Fingerprint::new();
    let mid = block_size / 2 - SEGMENT_SIZE / 2;
    hist_add(&mut middle.events, &block[mid..mid + SEGMENT_SIZE]);
    middle.nb_events = SEGMENT_SIZE;
    let dist_from_begin = fp_distance(&fpstats.past_events, &middle, 8);
    let dist_from_end = fp_distance(&fpstats.new_events, &middle, 8);
    const MIN_DISTANCE: u64 = (SEGMENT_SIZE * SEGMENT_SIZE / 3) as u64;
    if abs64((dist_from_begin as i64).wrapping_sub(dist_from_end as i64)) < MIN_DISTANCE {
        return 64 * KB;
    }
    if dist_from_begin > dist_from_end {
        32 * KB
    } else {
        96 * KB
    }
}

/// `ZSTD_splitBlock()`: where to end the block starting at `block`, which
/// must be a full 128 KB block. `level` 0..=4: 0 compares the borders
/// ([`split_block_from_borders`]), 1..=4 compare 8 KB chunks with
/// increasing sampling density. Returns a size in `8 KB..=128 KB`.
pub(crate) fn split_block(block: &[u8], level: u32, fpstats: &mut FpStats) -> usize {
    assert!(level <= 4);
    assert!(
        block.len() == 128 << 10,
        "ZSTD_splitBlock only accepts full 128 KB blocks"
    );
    if level == 0 {
        return split_block_from_borders(block, fpstats);
    }
    split_block_by_chunks(block, level as usize - 1, fpstats)
}

/// `ZSTD_optimalBlockSize()`: the size of the next block, `remaining` being
/// the rest of the frame's input from the block start. `split_level` is
/// `ZSTD_c_blockSplitterLevel` (0 = by strategy, 1 = never split, 2..=6 =
/// splitter level 0..=4); `savings` is libzstd's running
/// `consumedSrcSize - producedCSize` for the frame, updated after each
/// block with `blockSize - cSize`.
pub(crate) fn optimal_block_size(
    remaining: &[u8],
    block_size_max: usize,
    split_level: i32,
    strategy: Strategy,
    savings: i64,
    fpstats: &mut FpStats,
) -> usize {
    // split level based on compression strategy, from `fast` to `btultra2`
    const SPLIT_LEVELS: [i32; 10] = [0, 0, 1, 2, 2, 3, 3, 4, 4, 4];
    let src_size = remaining.len();
    // Only full blocks (128 KB) are split.
    if src_size < 128 * KB || block_size_max < 128 * KB {
        return src_size.min(block_size_max);
    }
    // Do not split incompressible data: require verified savings. As a
    // consequence, the first full block is not split.
    if savings < 3 {
        return 128 * KB;
    }
    let level = match split_level {
        1 => return 128 * KB,
        0 => SPLIT_LEVELS[strategy as usize],
        l => {
            debug_assert!((2..=6).contains(&l));
            l - 2
        }
    };
    split_block(&remaining[..block_size_max], level as u32, fpstats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn random(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n).map(|_| xorshift(&mut s) as u8).collect()
    }

    fn text(n: usize) -> Vec<u8> {
        b"The quick brown fox jumps over the lazy dog. "
            .iter()
            .copied()
            .cycle()
            .take(n)
            .collect()
    }

    const BLOCK: usize = 128 << 10;

    /// Mixed text with noise: words, with a random byte now and then.
    fn noisy_text(n: usize, seed: u64) -> Vec<u8> {
        const WORDS: [&[u8]; 8] = [
            b"alpha ", b"beta ", b"gamma ", b"delta ", b"split ", b"block ", b"zstd ", b"\n",
        ];
        let mut s = seed | 1;
        let mut v = Vec::with_capacity(n + 8);
        while v.len() < n {
            let r = xorshift(&mut s);
            if r.is_multiple_of(13) {
                v.push((r >> 8) as u8);
            } else {
                v.extend_from_slice(WORDS[(r >> 16) as usize % WORDS.len()]);
            }
        }
        v.truncate(n);
        v
    }

    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    /// The test blocks; the expected split points (per level 0..=4) below
    /// were produced by libzstd 1.5.7's `ZSTD_splitBlock` on the same data.
    pub(super) fn datasets() -> Vec<Vec<u8>> {
        alloc::vec![
            random(BLOCK, 1),
            text(BLOCK),
            alloc::vec![0u8; BLOCK],
            noisy_text(BLOCK, 2),
            cat(&[random(BLOCK / 2, 3), text(BLOCK / 2)]),
            cat(&[text(BLOCK * 3 / 4), random(BLOCK / 4, 4)]),
            cat(&[random(BLOCK / 4, 5), text(BLOCK * 3 / 4)]),
            cat(&[noisy_text(BLOCK / 2, 6), alloc::vec![7u8; BLOCK / 2]]),
            cat(&[
                noisy_text(40_000, 7),
                random(30_000, 8),
                noisy_text(BLOCK - 70_000, 9)
            ]),
        ]
    }

    #[test]
    fn split_points_match_libzstd() {
        const EXPECTED: [[usize; 5]; 9] = EXPECTED_FROM_LIBZSTD;
        let mut ws = FpStats::new();
        for (i, data) in datasets().iter().enumerate() {
            for level in 0..=4 {
                assert_eq!(
                    split_block(data, level, &mut ws),
                    EXPECTED[i][level as usize],
                    "dataset {i} level {level}"
                );
            }
        }
    }

    #[test]
    fn split_points_are_valid() {
        let mut ws = FpStats::new();
        let mut s = 99u64;
        for _ in 0..20 {
            let mut data = Vec::with_capacity(BLOCK);
            while data.len() < BLOCK {
                let r = xorshift(&mut s);
                let len = 1000 + (r % 20000) as usize;
                if r & 1 == 0 {
                    data.extend_from_slice(&random(len, r));
                } else {
                    data.extend_from_slice(&noisy_text(len, r));
                }
            }
            data.truncate(BLOCK);
            for level in 0..=4 {
                let p = split_block(&data, level, &mut ws);
                assert!(
                    p.is_multiple_of(8 << 10) && (8 << 10..=BLOCK).contains(&p),
                    "{p}"
                );
            }
        }
    }

    #[test]
    fn fingerprint_arithmetic() {
        let mut a = Fingerprint::new();
        let mut b = Fingerprint::new();
        // 10 positions sampled every 3rd -> events at 0,3,6,9 (4 events)
        // but nbEvents = limit / rate = 10 / 3 = 3, as in libzstd.
        record_fingerprint(&mut a, &[0u8; 11], 3, 8);
        assert_eq!(a.nb_events, 3);
        assert_eq!(a.events[0], 4);
        record_fingerprint(&mut b, &[1u8; 11], 3, 8);
        // distance = |4*3 - 0*3| + |0*3 - 4*3| = 24
        assert_eq!(fp_distance(&a, &b, 8), 24);
        // threshold = 3*3*(14+0)/16 = 7 -> 24 >= 7
        assert!(compare_fingerprints(&a, &b, 0, 8));
        assert!(!compare_fingerprints(&a, &a, 0, 8));
        // hash2 with 10 bits: (0x0201 * KNUTH) >> 22
        assert_eq!(
            hash2(&[1, 2], 10),
            (0x0201u32.wrapping_mul(KNUTH) >> 22) as usize
        );
    }

    #[test]
    fn optimal_size() {
        let mut ws = FpStats::new();
        let data = text(300_000);
        let fast = Strategy::Fast;
        assert_eq!(
            optimal_block_size(&data[..1000], 1 << 17, 0, fast, 100, &mut ws),
            1000
        );
        assert_eq!(
            optimal_block_size(&data, 1 << 16, 0, fast, 100, &mut ws),
            1 << 16
        );
        assert_eq!(
            optimal_block_size(&data, 1 << 17, 0, fast, 2, &mut ws),
            1 << 17
        );
        assert_eq!(
            optimal_block_size(&data, 1 << 17, 1, fast, 100, &mut ws),
            1 << 17
        );
        // Level by strategy (fast -> 0, dfast -> 1, ...) or explicit 2..=6.
        let mixed = &datasets()[4];
        let e = EXPECTED_FROM_LIBZSTD[4];
        assert_eq!(
            optimal_block_size(mixed, 1 << 17, 0, fast, 100, &mut ws),
            e[0]
        );
        assert_eq!(
            optimal_block_size(mixed, 1 << 17, 0, Strategy::DFast, 100, &mut ws),
            e[1]
        );
        assert_eq!(
            optimal_block_size(mixed, 1 << 17, 0, Strategy::BtUltra2, 100, &mut ws),
            e[4]
        );
        for sl in 2..=6 {
            let got = optimal_block_size(mixed, 1 << 17, sl, fast, 100, &mut ws);
            assert_eq!(got, e[sl as usize - 2]);
        }
    }

    const EXPECTED_FROM_LIBZSTD: [[usize; 5]; 9] = [
        [131072, 8192, 131072, 131072, 131072],
        [131072, 131072, 131072, 8192, 131072],
        [131072, 131072, 131072, 131072, 131072],
        [131072, 131072, 131072, 131072, 131072],
        [65536, 8192, 65536, 65536, 65536],
        [98304, 98304, 98304, 8192, 98304],
        [32768, 8192, 32768, 32768, 32768],
        [65536, 65536, 65536, 65536, 65536],
        [131072, 40960, 40960, 40960, 40960],
    ];
}
