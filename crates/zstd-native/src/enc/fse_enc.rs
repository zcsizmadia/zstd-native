//! FSE encoding (libzstd's compress/fse_compress.c and the encoder half of
//! common/fse.h): table log selection, count normalization, table
//! description (NCount) writing, encoding tables, and the symbol encoder.
//!
//! Faithful ports: the same integer arithmetic, rounding and tie-breaking
//! as libzstd 1.5.7, so normalized counts, headers and bitstreams are
//! byte-identical. Error codes are mapped to [`Error::Corrupted`]
//! (libzstd's `GENERIC` / `tableLog_tooLarge` / `maxSymbolValue_tooSmall`)
//! and [`Error::OutputTooSmall`] (`dstSize_tooSmall`).

// Index loops mirror libzstd's.
#![allow(clippy::needless_range_loop)]

use super::bitwriter::BitWriter;
use crate::error::{Error, Result};
use crate::fse::inv_table_step;

/// libzstd's `FSE_MAX_TABLELOG` (`FSE_MAX_MEMORY_USAGE - 2`, default 14 - 2).
pub(crate) const FSE_MAX_TABLELOG: u32 = 12;
/// libzstd's `FSE_DEFAULT_TABLELOG` (`FSE_DEFAULT_MEMORY_USAGE - 2`).
pub(crate) const FSE_DEFAULT_TABLELOG: u32 = 11;
/// libzstd's `FSE_MIN_TABLELOG`.
pub(crate) const FSE_MIN_TABLELOG: u32 = 5;
/// libzstd's `FSE_NCOUNTBOUND`: largest table description.
pub(crate) const FSE_NCOUNTBOUND: usize = 512;

/// libzstd's `FSE_BLOCKBOUND`.
#[inline]
pub(crate) const fn fse_block_bound(size: usize) -> usize {
    size + (size >> 7) + 4 + 8
}

/// libzstd's `ZSTD_highbit32` (position of the highest set bit). `v == 0`
/// is undefined in libzstd; here it returns 0 instead of panicking.
#[inline(always)]
pub(crate) const fn highbit32(v: u32) -> u32 {
    31 - (v | 1).leading_zeros()
}

/// libzstd's `FSE_TABLESTEP`.
#[inline(always)]
const fn table_step(table_size: u32) -> u32 {
    (table_size >> 1) + (table_size >> 3) + 3
}

// ---------------------------------------------------------------------------
// Table log
// ---------------------------------------------------------------------------

/// libzstd's `FSE_minTableLog`: smallest table log that can represent the
/// distribution.
#[inline]
fn fse_min_table_log(src_size: usize, max_symbol_value: u32) -> u32 {
    let min_bits_src = highbit32(src_size as u32) + 1;
    let min_bits_symbols = highbit32(max_symbol_value) + 2;
    debug_assert!(src_size > 1);
    min_bits_src.min(min_bits_symbols)
}

/// libzstd's `FSE_optimalTableLog_internal`. `max_table_log == 0` means
/// [`FSE_DEFAULT_TABLELOG`]; `src_size > 1`.
pub(crate) fn fse_optimal_table_log_internal(
    max_table_log: u32,
    src_size: usize,
    max_symbol_value: u32,
    minus: u32,
) -> u32 {
    // Unsigned in libzstd: wraps (and so never lowers the log) when the
    // high bit is below `minus`.
    let max_bits_src = highbit32((src_size as u32).wrapping_sub(1)).wrapping_sub(minus);
    let mut table_log = max_table_log;
    let min_bits = fse_min_table_log(src_size, max_symbol_value);
    debug_assert!(src_size > 1);
    if table_log == 0 {
        table_log = FSE_DEFAULT_TABLELOG;
    }
    if max_bits_src < table_log {
        table_log = max_bits_src;
    }
    if min_bits > table_log {
        table_log = min_bits;
    }
    table_log.clamp(FSE_MIN_TABLELOG, FSE_MAX_TABLELOG)
}

/// libzstd's `FSE_optimalTableLog`.
#[inline]
pub(crate) fn fse_optimal_table_log(
    max_table_log: u32,
    src_size: usize,
    max_symbol_value: u32,
) -> u32 {
    fse_optimal_table_log_internal(max_table_log, src_size, max_symbol_value, 2)
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

/// libzstd's `FSE_normalizeM2`: the secondary normalization, used when the
/// primary method fails.
fn fse_normalize_m2(
    norm: &mut [i16],
    table_log: u32,
    count: &[u32],
    mut total: usize,
    max_symbol_value: u32,
    low_prob_count: i16,
) -> Result<()> {
    const NOT_YET_ASSIGNED: i16 = -2;
    let msv = max_symbol_value as usize;
    let mut distributed: u32 = 0;

    let low_threshold = (total >> table_log) as u32;
    let mut low_one = ((total * 3) >> (table_log + 1)) as u32;

    for s in 0..=msv {
        let c = count[s];
        if c == 0 {
            norm[s] = 0;
            continue;
        }
        if c <= low_threshold {
            norm[s] = low_prob_count;
            distributed += 1;
            total -= c as usize;
            continue;
        }
        if c <= low_one {
            norm[s] = 1;
            distributed += 1;
            total -= c as usize;
            continue;
        }
        norm[s] = NOT_YET_ASSIGNED;
    }
    let mut to_distribute = (1u32 << table_log) - distributed;

    if to_distribute == 0 {
        return Ok(());
    }

    if (total / to_distribute as usize) > low_one as usize {
        // Risk of rounding to zero.
        low_one = ((total * 3) / (to_distribute.wrapping_mul(2) as usize)) as u32;
        for s in 0..=msv {
            if norm[s] == NOT_YET_ASSIGNED && count[s] <= low_one {
                norm[s] = 1;
                distributed += 1;
                total -= count[s] as usize;
            }
        }
        to_distribute = (1u32 << table_log) - distributed;
    }

    if distributed == max_symbol_value + 1 {
        // All values are pretty poor (probably incompressible data):
        // give all remaining points to the max.
        let mut max_v = 0;
        let mut max_c = 0;
        for s in 0..=msv {
            if count[s] > max_c {
                max_v = s;
                max_c = count[s];
            }
        }
        norm[max_v] = norm[max_v].wrapping_add(to_distribute as i16);
        return Ok(());
    }

    if total == 0 {
        // Every symbol was low enough for lowOne or lowThreshold.
        let mut s = 0;
        while to_distribute > 0 {
            if norm[s] > 0 {
                to_distribute -= 1;
                norm[s] += 1;
            }
            s = (s + 1) % (msv + 1);
        }
        return Ok(());
    }

    let v_step_log = 62 - u64::from(table_log);
    let mid = (1u64 << (v_step_log - 1)) - 1;
    let total32 = total as u32;
    if total32 == 0 {
        return Err(Error::Corrupted);
    }
    // Scale on the remaining total.
    let r_step = ((1u64 << v_step_log) * u64::from(to_distribute) + mid) / u64::from(total32);
    let mut tmp_total = mid;
    for s in 0..=msv {
        if norm[s] == NOT_YET_ASSIGNED {
            let end = tmp_total + u64::from(count[s]) * r_step;
            let s_start = (tmp_total >> v_step_log) as u32;
            let s_end = (end >> v_step_log) as u32;
            let weight = s_end.wrapping_sub(s_start);
            if weight < 1 {
                return Err(Error::Corrupted);
            }
            norm[s] = weight as i16;
            tmp_total = end;
        }
    }
    Ok(())
}

/// libzstd's `FSE_normalizeCount`: scales `count[0..=max_symbol_value]`
/// (summing to `total`) to `norm`, summing to `1 << table_log` (in
/// absolute value: low-probability symbols get -1 when
/// `use_low_prob_count`, else 1).
///
/// Returns the table log used (`table_log`, or [`FSE_DEFAULT_TABLELOG`]
/// for 0), or 0 when one symbol has every count (libzstd's "rle special
/// case": `norm` is then left partly unset).
pub(crate) fn fse_normalize_count(
    norm: &mut [i16],
    mut table_log: u32,
    count: &[u32],
    total: usize,
    max_symbol_value: u32,
    use_low_prob_count: bool,
) -> Result<u32> {
    if table_log == 0 {
        table_log = FSE_DEFAULT_TABLELOG;
    }
    if !(FSE_MIN_TABLELOG..=FSE_MAX_TABLELOG).contains(&table_log) {
        return Err(Error::Corrupted);
    }
    if table_log < fse_min_table_log(total, max_symbol_value) {
        return Err(Error::Corrupted);
    }
    let msv = max_symbol_value as usize;
    if norm.len() <= msv || count.len() <= msv {
        return Err(Error::Corrupted);
    }

    const RTB_TABLE: [u64; 8] = [0, 473195, 504333, 520860, 550000, 700000, 750000, 830000];
    let low_prob_count: i16 = if use_low_prob_count { -1 } else { 1 };
    let scale = 62 - u64::from(table_log);
    let total32 = total as u32;
    if total32 == 0 {
        return Err(Error::Corrupted);
    }
    let step = (1u64 << 62) / u64::from(total32); // the one division
    let v_step = 1u64 << (scale - 20);
    let mut still_to_distribute: i32 = 1 << table_log;
    let mut largest = 0usize;
    let mut largest_p: i16 = 0;
    let low_threshold = (total >> table_log) as u32;

    for s in 0..=msv {
        let c = count[s];
        if c as usize == total {
            return Ok(0); // rle special case
        }
        if c == 0 {
            norm[s] = 0;
            continue;
        }
        if c <= low_threshold {
            norm[s] = low_prob_count;
            still_to_distribute -= 1;
        } else {
            let cs = u64::from(c).wrapping_mul(step);
            let mut proba = (cs >> scale) as i16;
            if proba < 8 {
                let rest_to_beat = v_step * RTB_TABLE[proba as usize];
                proba += i16::from(cs - ((proba as u64) << scale) > rest_to_beat);
            }
            if proba > largest_p {
                largest_p = proba;
                largest = s;
            }
            norm[s] = proba;
            still_to_distribute -= i32::from(proba);
        }
    }
    if -still_to_distribute >= i32::from(norm[largest] >> 1) {
        // Corner case: needs the other normalization method.
        fse_normalize_m2(
            norm,
            table_log,
            count,
            total,
            max_symbol_value,
            low_prob_count,
        )?;
    } else {
        norm[largest] = norm[largest].wrapping_add(still_to_distribute as i16);
    }
    Ok(table_log)
}

// ---------------------------------------------------------------------------
// Table description (NCount)
// ---------------------------------------------------------------------------

/// libzstd's `FSE_NCountWriteBound`.
pub(crate) fn fse_ncount_write_bound(max_symbol_value: u32, table_log: u32) -> usize {
    let max_header_size =
        (((max_symbol_value as usize + 1) * table_log as usize + 4 + 2) / 8) + 1 + 2;
    if max_symbol_value != 0 {
        max_header_size
    } else {
        FSE_NCOUNTBOUND
    }
}

/// libzstd's `FSE_writeNCount` (`FSE_writeNCount_generic`): writes the
/// table description of `norm[0..=max_symbol_value]` and returns its size.
///
/// [`Error::OutputTooSmall`] when `dst` is too small (checked exactly where
/// libzstd's unsafe-buffer variant checks), [`Error::Corrupted`] for an
/// invalid table log or distribution.
pub(crate) fn fse_write_ncount(
    dst: &mut [u8],
    norm: &[i16],
    max_symbol_value: u32,
    table_log: u32,
) -> Result<usize> {
    if !(FSE_MIN_TABLELOG..=FSE_MAX_TABLELOG).contains(&table_log) {
        return Err(Error::Corrupted);
    }
    // libzstd skips the bounds checks when `dst` has the bound; checking
    // always is equivalent (they can't fail then).
    let oend = dst.len();
    let mut out = 0usize;
    let table_size: i32 = 1 << table_log;
    let alphabet_size = max_symbol_value as usize + 1;
    let mut bit_stream: u32 = 0;
    let mut bit_count: i32 = 0;
    let mut symbol = 0usize;
    let mut previous_is0 = false;

    macro_rules! put16 {
        () => {{
            if out + 2 > oend {
                return Err(Error::OutputTooSmall);
            }
            dst[out] = bit_stream as u8;
            dst[out + 1] = (bit_stream >> 8) as u8;
            out += 2;
        }};
    }

    // Table size
    bit_stream = bit_stream.wrapping_add((table_log - FSE_MIN_TABLELOG) << bit_count);
    bit_count += 4;

    let mut remaining: i32 = table_size + 1; // +1 for extra accuracy
    let mut threshold: i32 = table_size;
    let mut nb_bits: i32 = table_log as i32 + 1;

    while symbol < alphabet_size && remaining > 1 {
        if previous_is0 {
            let mut start = symbol;
            while symbol < alphabet_size && norm[symbol] == 0 {
                symbol += 1;
            }
            if symbol == alphabet_size {
                break; // incorrect distribution
            }
            while symbol >= start + 24 {
                start += 24;
                bit_stream = bit_stream.wrapping_add(0xFFFF << bit_count);
                put16!();
                bit_stream >>= 16;
            }
            while symbol >= start + 3 {
                start += 3;
                bit_stream = bit_stream.wrapping_add(3 << bit_count);
                bit_count += 2;
            }
            bit_stream = bit_stream.wrapping_add(((symbol - start) as u32) << bit_count);
            bit_count += 2;
            if bit_count > 16 {
                put16!();
                bit_stream >>= 16;
                bit_count -= 16;
            }
        }
        {
            let mut count = i32::from(norm[symbol]);
            symbol += 1;
            let max = (2 * threshold - 1) - remaining;
            remaining -= count.abs();
            count += 1; // +1 for extra accuracy
            if count >= threshold {
                count += max; // [0..max[ [max..threshold[ (...) [threshold+max 2*threshold[
            }
            bit_stream = bit_stream.wrapping_add((count as u32) << bit_count);
            bit_count += nb_bits;
            bit_count -= i32::from(count < max);
            previous_is0 = count == 1;
            if remaining < 1 {
                return Err(Error::Corrupted);
            }
            while remaining < threshold {
                nb_bits -= 1;
                threshold >>= 1;
            }
        }
        if bit_count > 16 {
            put16!();
            bit_stream >>= 16;
            bit_count -= 16;
        }
    }

    if remaining != 1 {
        return Err(Error::Corrupted); // incorrect normalized distribution
    }
    debug_assert!(symbol <= alphabet_size);

    // Flush the remaining bit stream.
    if out + 2 > oend {
        return Err(Error::OutputTooSmall);
    }
    dst[out] = bit_stream as u8;
    dst[out + 1] = (bit_stream >> 8) as u8;
    out += ((bit_count + 7) / 8) as usize;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Encoding table
// ---------------------------------------------------------------------------

/// libzstd's `FSE_symbolCompressionTransform`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct SymbolTransform {
    pub delta_find_state: i32,
    pub delta_nb_bits: u32,
}

/// libzstd's `FSE_CTable`, unpacked: `STATES` is the largest table size
/// (`1 << maxTableLog`, at least 2) and `SYMBOLS` the largest alphabet
/// (`maxSymbolValue + 1`) the table can hold. Copy it like libzstd copies
/// its tables (repeat mode); entries past the current table are unused.
#[derive(Clone, Debug)]
pub(crate) struct FseCTable<const STATES: usize, const SYMBOLS: usize> {
    /// Header field `tableLog` (0 for an RLE table).
    pub table_log: u32,
    /// Header field `maxSymbolValue` (the symbol, for an RLE table; read
    /// by `ZSTD_getFSEMaxSymbolValue`).
    pub max_symbol_value: u32,
    /// Next-state values, sorted by symbol (libzstd's `tableU16`).
    pub state_table: [u16; STATES],
    /// Per-symbol transforms (libzstd's `symbolTT`).
    pub symbol_tt: [SymbolTransform; SYMBOLS],
}

impl<const STATES: usize, const SYMBOLS: usize> Default for FseCTable<STATES, SYMBOLS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const STATES: usize, const SYMBOLS: usize> FseCTable<STATES, SYMBOLS> {
    pub(crate) const fn new() -> Self {
        FseCTable {
            table_log: 0,
            max_symbol_value: 0,
            state_table: [0; STATES],
            symbol_tt: [SymbolTransform {
                delta_find_state: 0,
                delta_nb_bits: 0,
            }; SYMBOLS],
        }
    }

    /// libzstd's `FSE_buildCTable_wksp`: builds the table from
    /// `norm[0..=max_symbol_value]` (as produced by [`fse_normalize_count`]).
    ///
    /// [`Error::Corrupted`] if the table doesn't fit this type (libzstd's
    /// workspace check) or `norm` doesn't sum to `1 << table_log` (libzstd
    /// doesn't check; it never gets such input).
    pub(crate) fn build(
        &mut self,
        norm: &[i16],
        max_symbol_value: u32,
        table_log: u32,
    ) -> Result<()> {
        let table_size = 1u32 << table_log;
        let ts = table_size as usize;
        let msv = max_symbol_value as usize;
        let max_sv1 = msv + 1;
        if table_log > FSE_MAX_TABLELOG || ts > STATES || max_sv1 > SYMBOLS || norm.len() < max_sv1
        {
            return Err(Error::Corrupted);
        }
        let table_mask = table_size - 1;
        let step = table_step(table_size);
        let mut cumul = [0u32; 257];
        // (`ts <= STATES`: smaller than libzstd's `1 << FSE_MAX_TABLELOG`
        // workspace, which matters for the zeroing.)
        let mut table_symbol = [0u8; STATES];
        let mut high_threshold = table_size - 1;

        self.table_log = table_log;
        self.max_symbol_value = max_symbol_value;

        // Symbol start positions.
        let mut sum = 0u32;
        for u in 1..=max_sv1 {
            let n = norm[u - 1];
            if n == -1 {
                // Low probability symbol.
                cumul[u] = cumul[u - 1] + 1;
                table_symbol[high_threshold as usize] = (u - 1) as u8;
                high_threshold = high_threshold.wrapping_sub(1);
                sum += 1;
            } else {
                if n < 0 {
                    return Err(Error::Corrupted);
                }
                cumul[u] = cumul[u - 1] + n as u32;
                sum += n as u32;
            }
            if sum > table_size {
                return Err(Error::Corrupted);
            }
        }
        if sum != table_size {
            return Err(Error::Corrupted);
        }
        cumul[max_sv1] = table_size + 1;

        // Spread symbols and build the table: next state values, sorted by
        // symbol.
        if high_threshold == table_size - 1 {
            // No low-probability symbols: lay the symbols out in order
            // (libzstd's fast path), then build the table straight from
            // that layout (meta/zstd#4836): cell `u` holds the sorted
            // symbol at `(u * inv) & mask` (see `fse::inv_table_step`), so
            // the cells are walked in order instead of scattering the
            // symbols and reading them back. Same table as the loop below.
            let mut spread = [0u8; STATES];
            let mut pos = 0usize;
            // 8 bytes per store, as libzstd: a run may spill up to 7 bytes
            // past its end, which the next symbols' runs (or nothing, past
            // the table) overwrite. `fill` would be a `memset` call per
            // symbol, much of a small table's cost.
            let mut sv = 0u64;
            for &n in &norm[..max_sv1] {
                let end = pos + n as usize;
                let mut p = pos;
                while p < end && p + 8 <= STATES {
                    spread[p..p + 8].copy_from_slice(&sv.to_le_bytes());
                    p += 8;
                }
                while p < end {
                    spread[p] = sv as u8; // the last bytes of a full-size table
                    p += 1;
                }
                pos = end;
                sv = sv.wrapping_add(0x0101_0101_0101_0101);
            }
            let inv = inv_table_step(table_size) as usize;
            let mask = table_mask as usize;
            let mut si = 0usize;
            for u in 0..table_size {
                let c = &mut cumul[usize::from(spread[si])];
                si = (si + inv) & mask;
                self.state_table[*c as usize] = (table_size + u) as u16;
                *c += 1;
            }
            debug_assert_eq!(si, 0, "visited every cell");
        } else {
            let mut position = 0u32;
            for (symbol, &freq) in norm[..max_sv1].iter().enumerate() {
                for _ in 0..freq.max(0) {
                    table_symbol[position as usize] = symbol as u8;
                    position = (position + step) & table_mask;
                    while position > high_threshold {
                        position = (position + step) & table_mask; // low proba area
                    }
                }
            }
            debug_assert_eq!(position, 0);
            for (u, &s) in table_symbol[..ts].iter().enumerate() {
                let c = &mut cumul[s as usize];
                self.state_table[*c as usize] = (table_size + u as u32) as u16;
                *c += 1;
            }
        }

        // Symbol transformation table.
        let mut total: u32 = 0;
        for s in 0..=msv {
            let n = norm[s];
            let tt = &mut self.symbol_tt[s];
            match n {
                0 => {
                    // Filled anyway, for FSE_getMaxNbBits.
                    tt.delta_nb_bits = ((table_log + 1) << 16).wrapping_sub(1 << table_log);
                }
                -1 | 1 => {
                    tt.delta_nb_bits = (table_log << 16).wrapping_sub(1 << table_log);
                    tt.delta_find_state = total as i32 - 1;
                    total += 1;
                }
                _ => {
                    let n = n as u32;
                    let max_bits_out = table_log - highbit32(n - 1);
                    let min_state_plus = n << max_bits_out;
                    tt.delta_nb_bits = (max_bits_out << 16).wrapping_sub(min_state_plus);
                    tt.delta_find_state = total as i32 - n as i32;
                    total += n;
                }
            }
        }
        Ok(())
    }

    /// libzstd's `FSE_buildCTable_rle`: a table that encodes only `symbol`,
    /// with 0 bits.
    pub(crate) fn build_rle(&mut self, symbol: u8) {
        debug_assert!(STATES >= 2 && SYMBOLS > symbol as usize);
        self.table_log = 0;
        self.max_symbol_value = u32::from(symbol);
        self.state_table[0] = 0;
        self.state_table[1] = 0;
        self.symbol_tt[symbol as usize] = SymbolTransform::default();
    }

    /// libzstd's `FSE_getMaxNbBits`: the cost of `symbol` in whole bits,
    /// rounded up (`table_log + 1` for a symbol with no probability).
    #[inline]
    pub(crate) fn max_nb_bits(&self, symbol: u32) -> u32 {
        self.symbol_tt[symbol as usize]
            .delta_nb_bits
            .wrapping_add((1 << 16) - 1)
            >> 16
    }

    /// libzstd's `FSE_bitCost`: the cost of `symbol` in fixed point with
    /// `accuracy_log` fractional bits. Wrapping arithmetic, so stale or
    /// zero-probability entries give garbage instead of panicking (libzstd
    /// computes it for them too and ignores the result).
    #[inline]
    pub(crate) fn bit_cost(&self, symbol: u32, accuracy_log: u32) -> u32 {
        let table_log = self.table_log;
        let d = self.symbol_tt[symbol as usize].delta_nb_bits;
        let min_nb_bits = d >> 16;
        let threshold = (min_nb_bits.wrapping_add(1)) << 16;
        debug_assert!(table_log < 16);
        let table_size = 1u32 << table_log;
        let delta_from_threshold = threshold.wrapping_sub(d.wrapping_add(table_size));
        let normalized = delta_from_threshold
            .wrapping_shl(accuracy_log)
            .wrapping_shr(table_log);
        let bit_multiplier = 1u32 << accuracy_log;
        (min_nb_bits.wrapping_add(1))
            .wrapping_mul(bit_multiplier)
            .wrapping_sub(normalized)
    }
}

/// libzstd's `FSE_CState_t`; the table is passed to each call.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FseCState {
    pub value: usize,
    pub state_log: u32,
}

impl<const STATES: usize, const SYMBOLS: usize> FseCTable<STATES, SYMBOLS> {
    /// The transform of `symbol`. When `SYMBOLS` is a power of two the
    /// index is masked instead of bounds-checked: every code the encoders
    /// produce is below `SYMBOLS`, so the mask changes nothing, and it
    /// keeps a compare and branch out of the sequence loop.
    #[inline(always)]
    fn tt(&self, symbol: u32) -> SymbolTransform {
        debug_assert!((symbol as usize) < SYMBOLS);
        if SYMBOLS.is_power_of_two() {
            self.symbol_tt[symbol as usize & (SYMBOLS - 1)]
        } else {
            self.symbol_tt[symbol as usize]
        }
    }

    /// The next state at `idx` (same masking as [`tt`](Self::tt): a valid
    /// table never indexes past its `1 << table_log <= STATES` states).
    #[inline(always)]
    fn next_state(&self, idx: usize) -> usize {
        debug_assert!(idx < STATES);
        if STATES.is_power_of_two() {
            usize::from(self.state_table[idx & (STATES - 1)])
        } else {
            usize::from(self.state_table[idx])
        }
    }
}

impl FseCState {
    /// libzstd's `FSE_initCState`.
    #[inline(always)]
    pub(crate) fn new<const N: usize, const S: usize>(ct: &FseCTable<N, S>) -> Self {
        FseCState {
            value: 1usize << ct.table_log,
            state_log: ct.table_log,
        }
    }

    /// libzstd's `FSE_initCState2`: the first symbol encoded (the last
    /// decoded) starts from the smallest state possible, saving its bits.
    #[inline(always)]
    pub(crate) fn new2<const N: usize, const S: usize>(ct: &FseCTable<N, S>, symbol: u32) -> Self {
        let mut st = Self::new(ct);
        let tt = ct.tt(symbol);
        let nb_bits_out = (tt.delta_nb_bits.wrapping_add(1 << 15)) >> 16;
        let v = (nb_bits_out << 16).wrapping_sub(tt.delta_nb_bits);
        let idx = ((v >> nb_bits_out) as usize).wrapping_add(tt.delta_find_state as isize as usize);
        st.value = ct.next_state(idx);
        st
    }

    /// libzstd's `FSE_encodeSymbol`.
    #[inline(always)]
    pub(crate) fn encode<const N: usize, const S: usize>(
        &mut self,
        bw: &mut BitWriter<'_>,
        ct: &FseCTable<N, S>,
        symbol: u32,
    ) {
        let tt = ct.tt(symbol);
        let nb_bits_out = (self.value.wrapping_add(tt.delta_nb_bits as usize) >> 16) as u32;
        bw.add_bits(self.value as u64, nb_bits_out);
        let idx = (self.value >> nb_bits_out).wrapping_add(tt.delta_find_state as isize as usize);
        self.value = ct.next_state(idx);
    }

    /// libzstd's `FSE_flushCState`: writes the final state.
    #[inline(always)]
    pub(crate) fn flush(&self, bw: &mut BitWriter<'_>) {
        bw.add_bits(self.value as u64, self.state_log);
        bw.flush();
    }
}

/// libzstd's `FSE_compress_usingCTable`: encodes `src` with two
/// interleaved states (the 64-bit container variant). Returns the size, or
/// 0 if `src.len() <= 2` or the output didn't fit.
pub(crate) fn fse_compress_using_ctable<const N: usize, const S: usize>(
    dst: &mut [u8],
    src: &[u8],
    ct: &FseCTable<N, S>,
) -> usize {
    if src.len() <= 2 {
        return 0;
    }
    let Some(mut bw) = BitWriter::new(dst) else {
        return 0; // not enough space for a bitstream
    };
    // libzstd picks BIT_flushBitsFast when dst >= FSE_BLOCKBOUND; this
    // writer's fast flush is the same as its safe one.
    let mut ip = src.len();
    let (mut c1, mut c2);
    if src.len() & 1 != 0 {
        c1 = FseCState::new2(ct, u32::from(src[ip - 1]));
        c2 = FseCState::new2(ct, u32::from(src[ip - 2]));
        c1.encode(&mut bw, ct, u32::from(src[ip - 3]));
        bw.flush();
        ip -= 3;
    } else {
        c2 = FseCState::new2(ct, u32::from(src[ip - 1]));
        c1 = FseCState::new2(ct, u32::from(src[ip - 2]));
        ip -= 2;
    }

    // Join to a multiple of 4.
    let src_size = src.len() - 2;
    if src_size & 2 != 0 {
        c2.encode(&mut bw, ct, u32::from(src[ip - 1]));
        c1.encode(&mut bw, ct, u32::from(src[ip - 2]));
        bw.flush();
        ip -= 2;
    }

    // 4 symbols per loop (64-bit container).
    debug_assert_eq!(ip % 4, 0);
    for q in src[..ip].rchunks_exact(4) {
        c2.encode(&mut bw, ct, u32::from(q[3]));
        c1.encode(&mut bw, ct, u32::from(q[2]));
        c2.encode(&mut bw, ct, u32::from(q[1]));
        c1.encode(&mut bw, ct, u32::from(q[0]));
        bw.flush();
    }

    c2.flush(&mut bw);
    c1.flush(&mut bw);
    bw.close()
}

/// libzstd's `FSE_compressBound`.
#[inline]
pub(crate) const fn fse_compress_bound(size: usize) -> usize {
    FSE_NCOUNTBOUND + fse_block_bound(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enc::hist::hist_count_simple;
    use crate::fse::read_ncount;
    use alloc::vec::Vec;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            (self.next() >> 16) % n
        }
    }

    /// A random histogram: `nsym` symbols (some zero), skewed.
    fn random_counts(r: &mut Rng, nsym: usize) -> (Vec<u32>, usize) {
        let mut count = alloc::vec![0u32; nsym];
        let shape = r.below(4);
        for (s, c) in count.iter_mut().enumerate() {
            *c = match shape {
                0 => r.below(1000) as u32,
                1 => (r.below(100_000) as u32) >> (s % 16),
                2 => {
                    if r.below(3) == 0 {
                        0
                    } else {
                        1 + r.below(5) as u32
                    }
                }
                _ => {
                    if s == 0 {
                        50_000
                    } else {
                        r.below(4) as u32
                    }
                }
            };
        }
        // At least two non-zero symbols.
        count[0] += 1;
        count[nsym - 1] += 1;
        let total = count.iter().map(|&c| c as usize).sum();
        (count, total)
    }

    /// A random normalized distribution of `max_sv1` symbols summing to
    /// `1 << log`, with some -1 (low-probability) symbols when `low`
    /// (never symbol 0), as meta/zstd#4836's `FUZ_genNormalizedCount`.
    fn random_norm(r: &mut Rng, max_sv1: usize, log: u32, low: bool) -> Vec<i16> {
        let mut norm = alloc::vec![0i16; max_sv1];
        let mut remaining = 1u32 << log;
        if low {
            for n in norm.iter_mut().skip(1) {
                if remaining > 1 && r.below(4) == 0 {
                    *n = -1;
                    remaining -= 1;
                }
            }
        }
        // Skewed or flat: add to a few symbols or to any.
        let hot = 1 + r.below(max_sv1 as u64) as usize;
        while remaining > 0 {
            let s = if r.below(2) == 0 {
                r.below(hot as u64) as usize
            } else {
                r.below(max_sv1 as u64) as usize
            };
            if norm[s] >= 0 {
                norm[s] += 1;
                remaining -= 1;
            }
        }
        norm
    }

    /// The plain construction (libzstd's spread, then the read-back in
    /// cell order): the state table every build must reproduce.
    fn reference_state_table(norm: &[i16], log: u32) -> Vec<u16> {
        let size = 1u32 << log;
        let mask = size - 1;
        let step = table_step(size);
        let mut cells = alloc::vec![0u8; size as usize];
        let mut high = size - 1;
        for (s, &n) in norm.iter().enumerate() {
            if n == -1 {
                cells[high as usize] = s as u8;
                high = high.wrapping_sub(1);
            }
        }
        let mut pos = 0u32;
        for (s, &n) in norm.iter().enumerate() {
            for _ in 0..n.max(0) {
                cells[pos as usize] = s as u8;
                pos = (pos + step) & mask;
                while pos > high {
                    pos = (pos + step) & mask;
                }
            }
        }
        assert_eq!(pos, 0);
        let mut cumul = alloc::vec![0u32; norm.len()];
        let mut c = 0;
        for (s, &n) in norm.iter().enumerate() {
            cumul[s] = c;
            c += if n == -1 { 1 } else { n as u32 };
        }
        let mut table = alloc::vec![0u16; size as usize];
        for (u, &s) in cells.iter().enumerate() {
            table[cumul[s as usize] as usize] = (size + u as u32) as u16;
            cumul[s as usize] += 1;
        }
        table
    }

    /// Both spread paths (the fast one without -1 symbols, and the general one)
    /// build exactly the reference table, for every table log and many
    /// distributions, in the table types the encoder uses.
    #[test]
    fn build_matches_reference_spread() {
        fn check<const N: usize, const S: usize>(
            r: &mut Rng,
            logs: core::ops::RangeInclusive<u32>,
        ) {
            let mut ct = FseCTable::<N, S>::new();
            for log in logs {
                for low in [false, true] {
                    for _ in 0..if cfg!(miri) { 2 } else { 300 } {
                        let max_sv1 = 2 + r.below(S as u64 - 1) as usize;
                        let norm = random_norm(r, max_sv1, log, low);
                        ct.build(&norm, max_sv1 as u32 - 1, log).unwrap();
                        let size = 1usize << log;
                        assert_eq!(
                            &ct.state_table[..size],
                            &reference_state_table(&norm, log)[..],
                            "log {log} norm {norm:?}"
                        );
                    }
                }
            }
        }
        let mut r = Rng(0x2545_F491_4F6C_DD1D);
        // Sequence codes (literal lengths, match lengths, offsets).
        check::<512, 64>(&mut r, 5..=9);
        // Huffman weights.
        check::<64, 13>(&mut r, 5..=6);
        // Every table log the encoder supports.
        check::<4096, 256>(&mut r, 5..=FSE_MAX_TABLELOG);
    }

    #[test]
    fn optimal_table_log() {
        assert_eq!(fse_optimal_table_log(9, 100, 35), 7);
        assert_eq!(fse_optimal_table_log(9, 100_000, 35), 9);
        assert_eq!(fse_optimal_table_log(0, 100_000, 255), 11);
        assert_eq!(fse_optimal_table_log(6, 3, 12), 6);
        // highbit(1) - 2 wraps: no lowering from the source size.
        assert_eq!(fse_optimal_table_log(6, 2, 1), 6);
        assert_eq!(fse_optimal_table_log(12, 40, 3), 5);
    }

    #[test]
    fn normalize_write_read_round_trip() {
        let mut r = Rng(0x9E37_79B9_7F4A_7C15);
        for iter in 0..3000 {
            let nsym = 2 + r.below(255) as usize;
            let (count, total) = random_counts(&mut r, nsym);
            let max_sv = (nsym - 1) as u32;
            let max_log = 5 + r.below(8) as u32;
            let table_log = fse_optimal_table_log(max_log, total, max_sv);
            let low = iter % 2 == 0;
            let mut norm = alloc::vec![0i16; nsym];
            let tl = match fse_normalize_count(&mut norm, table_log, &count, total, max_sv, low) {
                Ok(tl) => tl,
                Err(_) => continue,
            };
            assert_eq!(tl, table_log);
            let sum: i32 = norm.iter().map(|&n| i32::from(n).abs()).sum();
            assert_eq!(sum, 1 << tl, "iter {iter}");
            for s in 0..nsym {
                assert_eq!(count[s] == 0, norm[s] == 0, "iter {iter} s {s}");
                if !low {
                    assert!(norm[s] >= 0);
                }
            }
            let bound = fse_ncount_write_bound(max_sv, tl);
            let mut buf = alloc::vec![0u8; bound];
            let n = fse_write_ncount(&mut buf, &norm, max_sv, tl).unwrap();
            assert!(n <= bound);
            // Too small a buffer: error, never a panic.
            if n > 2 {
                let mut small = alloc::vec![0u8; n - 2];
                assert!(fse_write_ncount(&mut small, &norm, max_sv, tl).is_err());
            }
            let mut back = [0i16; 256];
            let (msv_read, tl_read, used) = read_ncount(&buf[..n], 255, &mut back).unwrap();
            assert_eq!(tl_read, tl);
            assert_eq!(used, n, "iter {iter}");
            let last = (0..nsym).rev().find(|&s| norm[s] != 0).unwrap();
            assert_eq!(msv_read, last);
            assert_eq!(&back[..nsym], &norm[..], "iter {iter}");

            // Build an encoding table and check its transforms.
            let mut ct = FseCTable::<4096, 256>::new();
            ct.build(&norm, max_sv, tl).unwrap();
            for &st in &ct.state_table[..1 << tl] {
                assert!((1u32 << tl..2u32 << tl).contains(&u32::from(st)));
            }
            for s in 0..nsym {
                if norm[s] != 0 {
                    let nb = ct.max_nb_bits(s as u32);
                    assert!(nb <= tl);
                    let cost = ct.bit_cost(s as u32, 8);
                    assert!(cost <= (tl + 1) << 8);
                } else {
                    assert_eq!(ct.max_nb_bits(s as u32), tl + 1);
                }
            }
        }
    }

    #[test]
    fn normalize_rle_and_errors() {
        let count = [0u32, 10, 0];
        let mut norm = [0i16; 3];
        assert_eq!(
            fse_normalize_count(&mut norm, 6, &count, 10, 2, false).unwrap(),
            0
        );
        let count = [5u32, 5];
        assert!(fse_normalize_count(&mut norm, 4, &count, 10, 1, false).is_err());
        assert!(fse_normalize_count(&mut norm, 13, &count, 10, 1, false).is_err());
    }

    #[test]
    fn rle_table_encodes_nothing() {
        let mut ct = FseCTable::<512, 53>::new();
        ct.build_rle(7);
        let mut buf = [0u8; 32];
        let mut bw = BitWriter::new(&mut buf).unwrap();
        let mut st = FseCState::new2(&ct, 7);
        for _ in 0..100 {
            st.encode(&mut bw, &ct, 7);
            bw.flush();
        }
        st.flush(&mut bw);
        assert_eq!(bw.close(), 1);
        assert_eq!(buf[0], 1);
    }

    /// Two interleaved states through the decoder's bit reader, decoding
    /// with a table built from `read_ncount` like the decoder would.
    #[test]
    fn compress_using_ctable_decodes() {
        let mut r = Rng(777);
        for iter in 0..300 {
            let nsym = 2 + r.below(60) as usize;
            let len = 3 + r.below(3000) as usize;
            let src: Vec<u8> = (0..len)
                .map(|_| {
                    let a = r.below(nsym as u64);
                    let b = r.below(nsym as u64);
                    (a * b / nsym as u64) as u8
                })
                .collect();
            let mut count = [0u32; 256];
            let mut max_sv = 255;
            let largest = hist_count_simple(&mut count, &mut max_sv, &src);
            if largest as usize == len {
                continue;
            }
            let tl = fse_optimal_table_log(FSE_MAX_TABLELOG, len, max_sv);
            let mut norm = [0i16; 256];
            assert_eq!(
                fse_normalize_count(&mut norm, tl, &count, len, max_sv, iter % 2 == 0).unwrap(),
                tl
            );
            let mut ct = FseCTable::<4096, 256>::new();
            ct.build(&norm, max_sv, tl).unwrap();
            let mut buf = alloc::vec![0u8; fse_block_bound(len)];
            let size = fse_compress_using_ctable(&mut buf, &src, &ct);
            assert!(size > 0);
            let decoded = test_decode(&buf[..size], &norm[..=max_sv as usize], tl, len);
            assert_eq!(decoded, src, "iter {iter}");
            // Too small: 0, no panic.
            if size > 9 {
                let mut small = alloc::vec![0u8; size - 1];
                let s2 = fse_compress_using_ctable(&mut small, &src, &ct);
                assert!(s2 == 0 || s2 < size);
            }
        }
    }

    /// Reference FSE decoder (libzstd's `FSE_buildDTable` +
    /// `FSE_decompress_usingDTable`), for the tests.
    fn test_decode(src: &[u8], norm: &[i16], tl: u32, len: usize) -> Vec<u8> {
        use crate::bits::BitReader;
        let ts = 1usize << tl;
        let mut cells = alloc::vec![0u8; ts];
        let mut next: Vec<u32> = alloc::vec![0; norm.len()];
        let mut high = ts - 1;
        for (s, &n) in norm.iter().enumerate() {
            if n == -1 {
                cells[high] = s as u8;
                high = high.wrapping_sub(1);
                next[s] = 1;
            } else {
                next[s] = n as u32;
            }
        }
        let step = table_step(ts as u32) as usize;
        let mut pos = 0;
        for (s, &n) in norm.iter().enumerate() {
            for _ in 0..n.max(0) {
                cells[pos] = s as u8;
                pos = (pos + step) & (ts - 1);
                while pos > high {
                    pos = (pos + step) & (ts - 1);
                }
            }
        }
        let mut dt = alloc::vec![(0u8, 0u32, 0usize); ts];
        for u in 0..ts {
            let s = cells[u] as usize;
            let ns = next[s];
            next[s] += 1;
            let nb = tl - highbit32(ns);
            dt[u] = (s as u8, nb, ((ns << nb) as usize) - ts);
        }
        let mut br = BitReader::new(src).unwrap();
        let mut s1 = br.read(tl) as usize;
        let mut s2 = br.read(tl) as usize;
        br.reload();
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let st = if i % 2 == 0 { &mut s1 } else { &mut s2 };
            let (sym, nb, base) = dt[*st];
            out.push(sym);
            // The last two symbols are the initial states: no bits.
            if i + 2 < len {
                *st = base + br.read(nb) as usize;
                br.reload();
            }
        }
        assert!(br.is_end());
        out
    }
}
