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
    let mut c = [[0u32; 256]; 4];
    // One table per byte lane of each 32-bit word, as libzstd (which
    // unrolls 4 words per iteration; the sums are the same).
    let (chunks, rest) = src.as_chunks::<4>();
    for w in chunks {
        let v = u32::from_le_bytes(*w);
        c[0][(v & 0xFF) as usize] += 1;
        c[1][((v >> 8) & 0xFF) as usize] += 1;
        c[2][((v >> 16) & 0xFF) as usize] += 1;
        c[3][(v >> 24) as usize] += 1;
    }
    for &b in rest {
        c[0][b as usize] += 1;
    }
    let mut max = 0;
    for s in 0..256 {
        let v = c[0][s] + c[1][s] + c[2][s] + c[3][s];
        c[0][s] = v;
        if v > max {
            max = v;
        }
    }
    let mut max_sv = 255;
    while c[0][max_sv] == 0 {
        max_sv -= 1;
    }
    if check && max_sv > *max_symbol_value as usize {
        return Err(Error::Corrupted);
    }
    *max_symbol_value = max_sv as u32;
    count[..count_len].copy_from_slice(&c[0][..count_len]);
    Ok(max)
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
            0usize, 1, 3, 15, 16, 17, 100, 1499, 1500, 1501, 5000, 131072,
        ] {
            for &alpha in &[1u32, 2, 40, 200, 256] {
                let src = data(n, n as u64 * 31 + u64::from(alpha), alpha);
                let (rc, rmax, rl) = reference(&src);
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
}
