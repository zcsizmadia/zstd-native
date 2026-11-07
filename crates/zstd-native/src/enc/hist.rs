//! Byte histograms (libzstd's compress/hist.c).
//!
//! All functions fill `count[0..=max_symbol_value]` (the value passed in),
//! set `*max_symbol_value` to the largest byte present (0 for empty input)
//! and return the largest count. Entries of `count` above the value passed
//! in are left untouched, as in libzstd.

// Index loops mirror libzstd's.
#![allow(clippy::needless_range_loop)]

use crate::error::{Error, Result};

/// libzstd's `HIST_add`: adds `src`'s bytes to `count` (no reset).
pub(crate) fn hist_add(count: &mut [u32; 256], src: &[u8]) {
    for &b in src {
        count[b as usize] += 1;
    }
}

/// libzstd's `HIST_count_simple`: one table, for small inputs.
///
/// Every byte of `src` must be `<= *max_symbol_value` (libzstd asserts
/// it). Returns the largest count.
pub(crate) fn hist_count_simple(
    count: &mut [u32; 256],
    max_symbol_value: &mut u32,
    src: &[u8],
) -> u32 {
    let mut max_sv = *max_symbol_value as usize;
    count[..=max_sv].fill(0);
    if src.is_empty() {
        *max_symbol_value = 0;
        return 0;
    }
    for &b in src {
        debug_assert!(b as usize <= max_sv);
        count[b as usize] += 1;
    }
    while count[max_sv] == 0 {
        max_sv -= 1;
    }
    *max_symbol_value = max_sv as u32;
    let mut largest = 0;
    for &c in &count[..=max_sv] {
        if c > largest {
            largest = c;
        }
    }
    largest
}

/// libzstd's `HIST_count_parallel_wksp`: four interleaved tables, summed at
/// the end. With `check`, a byte above `*max_symbol_value` is an error
/// (libzstd's `maxSymbolValue_tooSmall`, here [`Error::Corrupted`]).
fn hist_count_parallel(
    count: &mut [u32; 256],
    max_symbol_value: &mut u32,
    src: &[u8],
    check: bool,
) -> Result<u32> {
    debug_assert!(*max_symbol_value <= 255);
    let count_len = *max_symbol_value as usize + 1;
    if src.is_empty() {
        count[..count_len].fill(0);
        *max_symbol_value = 0;
        return Ok(0);
    }
    // One table per byte lane of each word: libzstd's four lanes of a
    // 32-bit word, or eight lanes of a 64-bit word for large inputs. A
    // byte that repeats with the word's period (x-ray's 16-bit samples,
    // whose high byte barely changes) hits the same counter every word,
    // and each increment waits for the previous one's store; twice the
    // lanes halve the length of those chains. The sums are the same.
    let (c, max) = if src.len() < LANES8_MIN {
        count_lanes::<4, 256>(src)
    } else {
        count_lanes::<8, 256>(src)
    };
    let mut max_sv = 255;
    while c[max_sv] == 0 {
        max_sv -= 1;
    }
    if check && max_sv > *max_symbol_value as usize {
        return Err(Error::Corrupted);
    }
    *max_symbol_value = max_sv as u32;
    count[..count_len].copy_from_slice(&c[..count_len]);
    Ok(max)
}

/// Inputs this long use eight lanes (the tables cost 8 KB to zero and sum).
const LANES8_MIN: usize = 8 * 1024;

/// The largest count of a byte in `src` (what `HIST_count_simple` returns),
/// counted in lanes.
pub(crate) fn hist_largest_count(src: &[u8]) -> u32 {
    if src.len() < LANES8_MIN {
        count_lanes::<4, 256>(src).1
    } else {
        count_lanes::<8, 256>(src).1
    }
}

/// The histogram of `src` counted in `L` lane tables of `N` entries (a
/// power of two; every byte must be below it), summed; and the largest
/// count.
#[inline(always)]
fn count_lanes<const L: usize, const N: usize>(src: &[u8]) -> ([u32; N], u32) {
    const { assert!(N.is_power_of_two() && N <= 256) };
    debug_assert!(N == 256 || src.iter().all(|&b| (b as usize) < N));
    let mut c = [[0u32; N]; L];
    let (chunks, rest) = src.as_chunks::<L>();
    for w in chunks {
        for (lane, &b) in c.iter_mut().zip(w) {
            lane[b as usize % N] += 1;
        }
    }
    for &b in rest {
        c[0][b as usize % N] += 1;
    }
    let mut max = 0;
    for s in 0..N {
        let mut v = 0;
        for lane in &c {
            v += lane[s];
        }
        c[0][s] = v;
        if v > max {
            max = v;
        }
    }
    (c[0], max)
}

/// libzstd's `HIST_countFast_wksp`: trusts the input (bytes above
/// `*max_symbol_value` are counted but not reported). Simple counting
/// under 1500 bytes, four tables above.
pub(crate) fn hist_count_fast(
    count: &mut [u32; 256],
    max_symbol_value: &mut u32,
    src: &[u8],
) -> u32 {
    if src.len() < 1500 {
        return hist_count_simple(count, max_symbol_value, src);
    }
    // Can't fail without `check`.
    hist_count_parallel(count, max_symbol_value, src, false).unwrap_or(0)
}

/// [`hist_count_fast`] for the sequence codes (every byte of `src` and
/// `*max_symbol_value` below [`CODES`]), with the same results. Lane
/// tables of [`CODES`] entries instead of 256 cost a quarter to clear and
/// sum, so they pay off from [`CODES_LANES_MIN`] codes on, where libzstd
/// counts in one table up to 1500: on the small blocks of dictionary
/// frames and flushes, runs of one code (repcodes, short lengths) make
/// each increment wait for the one before.
pub(crate) fn hist_count_codes(
    count: &mut [u32; 256],
    max_symbol_value: &mut u32,
    src: &[u8],
) -> u32 {
    debug_assert!((*max_symbol_value as usize) < CODES);
    if src.len() < CODES_LANES_MIN {
        return hist_count_simple(count, max_symbol_value, src);
    }
    let (c, max) = if src.len() < LANES8_MIN {
        count_lanes::<4, CODES>(src)
    } else {
        count_lanes::<8, CODES>(src)
    };
    let mut max_sv = CODES - 1;
    while c[max_sv] == 0 {
        max_sv -= 1;
    }
    let count_len = *max_symbol_value as usize + 1;
    *max_symbol_value = max_sv as u32;
    count[..count_len].copy_from_slice(&c[..count_len]);
    max
}

/// The sequence codes are below this ([`hist_count_codes`]).
pub(crate) const CODES: usize = 64;

/// [`hist_count_codes`] counts this many codes or more in lanes.
const CODES_LANES_MIN: usize = 64;

/// libzstd's `HIST_count_wksp`: like [`hist_count_fast`] but returns
/// [`Error::Corrupted`] (libzstd's `maxSymbolValue_tooSmall`) if a byte is
/// above `*max_symbol_value`.
pub(crate) fn hist_count(
    count: &mut [u32; 256],
    max_symbol_value: &mut u32,
    src: &[u8],
) -> Result<u32> {
    if *max_symbol_value < 255 {
        return hist_count_parallel(count, max_symbol_value, src, true);
    }
    *max_symbol_value = 255;
    Ok(hist_count_fast(count, max_symbol_value, src))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn reference(src: &[u8]) -> ([u32; 256], u32, u32) {
        let mut c = [0u32; 256];
        for &b in src {
            c[b as usize] += 1;
        }
        let max_sv = (0..256).rev().find(|&s| c[s] != 0).unwrap_or(0) as u32;
        let largest = *c.iter().max().unwrap();
        (c, max_sv, largest)
    }

    fn data(n: usize, seed: u64, alphabet: u32) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                // Skewed: small values more likely.
                let r = (s >> 32) as u32 % alphabet;
                (r * r / alphabet) as u8
            })
            .collect()
    }

    #[test]
    fn matches_reference() {
        for &n in &[
            0usize, 1, 3, 15, 16, 17, 100, 1499, 1500, 1501, 5000, 8191, 8192, 8200, 131072,
        ] {
            for &alpha in &[1u32, 2, 40, 200, 256] {
                let src = data(n, n as u64 * 31 + u64::from(alpha), alpha);
                let (rc, rmax, rl) = reference(&src);
                assert_eq!(hist_largest_count(&src), rl, "n={n} alpha={alpha}");
                for f in 0..4 {
                    let mut count = [7u32; 256];
                    let mut max = 255;
                    let largest = match f {
                        0 => hist_count_simple(&mut count, &mut max, &src),
                        1 => hist_count_fast(&mut count, &mut max, &src),
                        2 => hist_count(&mut count, &mut max, &src).unwrap(),
                        _ => {
                            max = 254;
                            match hist_count(&mut count, &mut max, &src) {
                                Ok(l) => l,
                                Err(_) => {
                                    assert_eq!(rmax, 255);
                                    continue;
                                }
                            }
                        }
                    };
                    assert_eq!(largest, rl, "f={f} n={n}");
                    assert_eq!(max, rmax, "f={f} n={n}");
                    let lim = if f == 3 { 255 } else { 256 };
                    assert_eq!(&count[..lim], &rc[..lim], "f={f} n={n}");
                }
            }
        }
    }

    #[test]
    fn small_max_symbol() {
        let src = [0u8, 1, 2, 2, 5, 5, 5];
        let mut count = [9u32; 256];
        let mut max = 7;
        assert_eq!(hist_count_simple(&mut count, &mut max, &src), 3);
        assert_eq!(max, 5);
        assert_eq!(&count[..9], &[1, 1, 2, 0, 0, 3, 0, 0, 9]);
        let mut max = 4;
        assert!(hist_count(&mut count, &mut max, &src).is_err());
    }

    /// [`hist_count_codes`] gives [`hist_count_fast`]'s results for codes
    /// (below 64), around its thresholds, with the table entries above
    /// the given maximum untouched.
    #[test]
    fn codes_match_fast() {
        let (ns, alphas): (&[usize], &[u32]) = if cfg!(miri) {
            (&[63, 64, 700], &[2, 53])
        } else {
            (
                &[1, 2, 63, 64, 65, 600, 1499, 1500, 8191, 8192, 30000],
                &[1, 2, 32, 36, 53, 64],
            )
        };
        for &n in ns {
            for &alpha in alphas {
                let src = data(n, n as u64 * 7 + u64::from(alpha), alpha);
                for &max_in in &[31u32, 35, 52, 63] {
                    if src.iter().any(|&b| u32::from(b) > max_in) {
                        continue;
                    }
                    let (mut a, mut b) = ([7u32; 256], [7u32; 256]);
                    let (mut ma, mut mb) = (max_in, max_in);
                    let la = hist_count_codes(&mut a, &mut ma, &src);
                    let lb = hist_count_fast(&mut b, &mut mb, &src);
                    assert_eq!((la, ma), (lb, mb), "n={n} alpha={alpha} max={max_in}");
                    assert_eq!(a, b, "n={n} alpha={alpha} max={max_in}");
                }
            }
        }
    }
}
