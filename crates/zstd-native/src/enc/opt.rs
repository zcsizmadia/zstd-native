//! The optimal parser (btopt, btultra and btultra2: levels 16-22 for
//! inputs over 256 KB, down to 11 for small ones): a port of libzstd
//! 1.5.7's `compress/zstd_opt.c` (noDict, extDict and dictMatchState
//! paths).
//!
//! - Price model: `ZSTD_rescaleFreqs`, `ZSTD_setBasePrices`,
//!   `ZSTD_updateStats`, `ZSTD_rawLiteralsCost`, `ZSTD_litLengthPrice`,
//!   `ZSTD_getMatchPrice` ([`OptStats`]). btopt (`optLevel` 0) prices in
//!   whole bits (`ZSTD_bitWeight`), btultra and btultra2 (`optLevel` 2) in
//!   fractional bits (`ZSTD_fracWeight`). The statistics carry over from
//!   block to block within a frame (scaled down at each block start).
//! - Match finder: a sorted binary tree in the chain table (`ZSTD_insertBt1`,
//!   `ZSTD_updateTree`, `ZSTD_insertBtAndGetAllMatches`), unlike btlazy2's
//!   lazily sorted one, plus the repcodes and, with `minMatch == 3`, a
//!   3-byte hash table (`hashTable3`) for short matches.
//! - Parser: `ZSTD_compressBlock_opt_generic`, a forward pass pricing every
//!   position reachable from the current one with matches up to
//!   `ZSTD_OPT_NUM` (4096) bytes ahead, then a backward pass storing the
//!   cheapest path.
//! - btultra2 compresses the first block of a frame twice
//!   (`ZSTD_initStats_ultra`): the first pass only seeds the statistics,
//!   then the window forgets it (`base` moves back by the block size).
//!
//! Like libzstd's templates, the parser is monomorphized on `mls`
//! (`minMatch` bounded to 3..=6) and `optLevel`.
//!
//! With a dictionary whose literals table covers every byte value
//! (`HUF_repeat_valid`), the first block's statistics are seeded from the
//! dictionary's entropy tables ([`DictCosts`]). With an attached dictionary
//! (`DMS`), the match finder also checks the repcodes into it and searches
//! its (sorted, read-only) binary tree with the compares left; btultra2
//! uses btultra's dictMatchState variant, as in libzstd.
//!
//! With long distance matching (the `ldm` module), the parser runs with
//! `LDM`: the block's long matches are extra candidates, added to the
//! matches found at the positions they cover when longer
//! (`ZSTD_optLdm_processMatchCandidate`). The non-LDM instances have no
//! trace of it.
//!
//! With an extDict (a stream whose input buffer wrapped), the match finder
//! runs with `EXT`: candidates below `dictLimit` are read from the extDict
//! and counted across into the prefix (`ZSTD_count_2segments`); the parser
//! is the same (libzstd's `dictMode` template parameter). btultra2 uses
//! btultra's extDict variant, as in libzstd.
//!
//! # Memory safety
//!
//! As in `lazy`: every index in the hash tables and the tree is that of a
//! position the parser has already passed (searches only happen at
//! positions at or after `nextToUpdate`, which is past every inserted
//! position), so candidates that pass the `>= windowLow` check (at or
//! above `lowLimit`) are positions in the window before the current one,
//! in the extDict when below `dictLimit`. Forward reads stop at the block end
//! (`iend`); hashing reads 8 bytes at positions at most `iend - 8`.
//! Debug builds check every read with `chk`.

use super::compress::EntropyTables;
use super::fast::{
    DmsView, ExtWindow, HASH_READ_SIZE, MatchState, hash_mls, index_overlap_check, prefetch_l1,
};
use super::hist::hist_count_simple;
use super::huf_enc::HufRepeat;
use super::ldm::RawSeq;
use super::params::{BLOCKSIZE_MAX, Strategy};
use super::seq_store::{
    MAX_LL, MAX_ML, MAX_OFF, MINMATCH, REP_NUM, SeqStore, count, count_2segments, hash3_ptr,
    highbit32, ll_code, ml_code, offset_to_offbase, read32, update_rep,
};
use crate::fse::{LL_BITS, ML_BITS};
use alloc::vec;
use alloc::vec::Vec;

/// `ZSTD_OPT_NUM`: the parser looks at most this far ahead.
pub(crate) const OPT_NUM: u32 = 1 << 12;
/// `ZSTD_OPT_SIZE`: the price and match tables' size.
const OPT_SIZE: usize = OPT_NUM as usize + 3;
/// `ZSTD_LITFREQ_ADD`: literals count more, to adapt faster.
const LITFREQ_ADD: u32 = 2;
/// `ZSTD_MAX_PRICE`.
const MAX_PRICE: i32 = 1 << 30;
/// `ZSTD_PREDEF_THRESHOLD`: first blocks up to this size use static prices.
const PREDEF_THRESHOLD: usize = 8;
/// `BITCOST_ACCURACY`.
const BITCOST_ACCURACY: u32 = 8;
/// `BITCOST_MULTIPLIER`.
const BITCOST_MULTIPLIER: u32 = 1 << BITCOST_ACCURACY;
/// `MaxLit`.
const MAX_LIT: usize = 255;

/// `ZSTD_OptPrice_e`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PriceType {
    /// `zop_dynamic`: prices from the collected statistics.
    Dynamic,
    /// `zop_predef`: static prices (first blocks of at most 8 bytes).
    Predef,
}

/// `ZSTD_optimal_t`: during the forward pass, the cheapest "stretch" (a
/// match followed by `litlen` literals) ending at a position; during the
/// backward pass, a sequence.
#[derive(Clone, Copy, Debug, Default)]
struct Optimal {
    /// Price from the beginning of the segment to this position.
    price: i32,
    /// offBase of the previous match.
    off: u32,
    /// Length of the previous match.
    mlen: u32,
    /// Literals since the previous match.
    litlen: u32,
    /// Repcode history after the previous match.
    rep: [u32; 3],
}

/// The statistics `ZSTD_rescaleFreqs` seeds the first block of a frame
/// with when the previous block's entropy tables come from a dictionary
/// (`symbolCosts->huf.repeatMode == HUF_repeat_valid`): each symbol's
/// frequency is `1 << (scaleLog - bitCost)` (1 for a cost of 0), from the
/// Huffman code lengths (scale 2K) and the FSE tables' `FSE_getMaxNbBits`
/// (scale 1K).
#[derive(Clone, Debug)]
pub(crate) struct DictCosts {
    lit_freq: [u32; MAX_LIT + 1],
    lit_length_freq: [u32; MAX_LL as usize + 1],
    match_length_freq: [u32; MAX_ML as usize + 1],
    off_code_freq: [u32; MAX_OFF as usize + 1],
}

impl DictCosts {
    /// The seed from `e`, the entropy tables a frame starts with; `None`
    /// unless its Huffman table is `HUF_repeat_valid` (only a zstd-format
    /// dictionary with a weight for every byte makes it so at a frame
    /// start).
    pub(crate) fn new(e: &EntropyTables) -> Option<alloc::boxed::Box<DictCosts>> {
        if e.huf.repeat != HufRepeat::Valid {
            return None;
        }
        // `1 << (scaleLog - bitCost)`: libzstd asserts `bitCost <= scaleLog`
        // (shift counts wrap like on x86 otherwise)
        let freq = |bit_cost: u32, scale_log: u32| {
            if bit_cost != 0 {
                1u32.wrapping_shl(scale_log.wrapping_sub(bit_cost))
            } else {
                1 // minimum to calculate cost
            }
        };
        let mut c = alloc::boxed::Box::new(DictCosts {
            lit_freq: [0; MAX_LIT + 1],
            lit_length_freq: [0; MAX_LL as usize + 1],
            match_length_freq: [0; MAX_ML as usize + 1],
            off_code_freq: [0; MAX_OFF as usize + 1],
        });
        // literals from the Huffman table, scaled to 2K
        for (lit, f) in c.lit_freq.iter_mut().enumerate() {
            *f = freq(e.huf.table.nb_bits(lit as u32), 11);
        }
        // sequence symbols from the FSE tables, scaled to 1K
        for (ll, f) in c.lit_length_freq.iter_mut().enumerate() {
            *f = freq(e.fse.ll.max_nb_bits(ll as u32), 10);
        }
        for (ml, f) in c.match_length_freq.iter_mut().enumerate() {
            *f = freq(e.fse.ml.max_nb_bits(ml as u32), 10);
        }
        for (of, f) in c.off_code_freq.iter_mut().enumerate() {
            *f = freq(e.fse.of.max_nb_bits(of as u32), 10);
        }
        Some(c)
    }
}

/// `ZSTD_match_t`.
#[derive(Clone, Copy, Debug, Default)]
struct Match {
    /// offBase (offset + 3, or repcode 1..=3).
    off: u32,
    /// Match length.
    len: u32,
}

/// The statistics part of libzstd's `optState_t`.
#[derive(Clone, Debug)]
pub(crate) struct OptStats {
    lit_freq: [u32; MAX_LIT + 1],
    lit_length_freq: [u32; MAX_LL as usize + 1],
    match_length_freq: [u32; MAX_ML as usize + 1],
    off_code_freq: [u32; MAX_OFF as usize + 1],
    lit_sum: u32,
    /// `litLengthSum`: 0 means "first block of a frame" (reset by
    /// `ZSTD_invalidateMatchState`).
    pub(crate) lit_length_sum: u32,
    match_length_sum: u32,
    off_code_sum: u32,
    lit_sum_base_price: u32,
    lit_length_sum_base_price: u32,
    match_length_sum_base_price: u32,
    off_code_sum_base_price: u32,
    price_type: PriceType,
}

/// libzstd's `optState_t` (the literal compression mode is always "on":
/// the optimal strategies never disable literals compression).
pub(crate) struct OptState {
    pub(crate) stats: OptStats,
    /// `priceTable`
    price_table: Vec<Optimal>,
    /// `matchTable`
    match_table: Vec<Match>,
    /// The first block's seed from the frame's initial entropy tables
    /// (`symbolCosts`), when they come from a dictionary; set when a frame
    /// starts.
    pub(crate) dict_costs: Option<alloc::boxed::Box<DictCosts>>,
}

impl core::fmt::Debug for OptState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OptState")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl OptState {
    pub(crate) fn new() -> Self {
        OptState {
            stats: OptStats {
                lit_freq: [0; MAX_LIT + 1],
                lit_length_freq: [0; MAX_LL as usize + 1],
                match_length_freq: [0; MAX_ML as usize + 1],
                off_code_freq: [0; MAX_OFF as usize + 1],
                lit_sum: 0,
                lit_length_sum: 0,
                match_length_sum: 0,
                off_code_sum: 0,
                lit_sum_base_price: 0,
                lit_length_sum_base_price: 0,
                match_length_sum_base_price: 0,
                off_code_sum_base_price: 0,
                price_type: PriceType::Dynamic,
            },
            price_table: vec![Optimal::default(); OPT_SIZE],
            match_table: vec![Match::default(); OPT_SIZE],
            dict_costs: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Price model

/// `ZSTD_bitWeight()`: the cost of a statistic in whole bits.
#[inline(always)]
fn bit_weight(stat: u32) -> u32 {
    highbit32(stat + 1) * BITCOST_MULTIPLIER
}

/// `ZSTD_fracWeight()`: the cost of a statistic in fractional bits
/// (linear interpolation between powers of 2).
#[inline(always)]
fn frac_weight(raw_stat: u32) -> u32 {
    let stat = raw_stat + 1;
    let hb = highbit32(stat);
    let b_weight = hb * BITCOST_MULTIPLIER;
    // "fractional weight": a value between 1 and 2, in fixed point
    let f_weight = (stat << BITCOST_ACCURACY) >> hb;
    debug_assert!(hb + BITCOST_ACCURACY < 31);
    b_weight + f_weight
}

/// `WEIGHT(stat, optLevel)`.
#[inline(always)]
fn weight<const OPT_LEVEL: u32>(stat: u32) -> u32 {
    if OPT_LEVEL > 0 {
        frac_weight(stat)
    } else {
        bit_weight(stat)
    }
}

/// `ZSTD_downscaleStats()`. With `base1`, every symbol keeps a count of at
/// least 1 (`base_1guaranteed`), else only those with a non-zero count
/// (`base_0possible`). Returns the new sum.
fn downscale_stats(table: &mut [u32], shift: u32, base1: bool) -> u32 {
    debug_assert!(shift < 30);
    let mut sum = 0u32;
    for t in table.iter_mut() {
        let base = if base1 { 1 } else { u32::from(*t > 0) };
        let new_stat = base + (*t >> shift);
        sum += new_stat;
        *t = new_stat;
    }
    sum
}

/// `ZSTD_scaleStats()`: downscales `table` if its sum is above
/// `1 << log_target`; returns the resulting sum.
fn scale_stats(table: &mut [u32], log_target: u32) -> u32 {
    let prev_sum: u32 = table.iter().fold(0u32, |a, &b| a.wrapping_add(b));
    let factor = prev_sum >> log_target;
    debug_assert!(log_target < 30);
    if factor <= 1 {
        return prev_sum;
    }
    downscale_stats(table, highbit32(factor), true)
}

impl OptStats {
    /// `ZSTD_setBasePrices()`.
    fn set_base_prices<const OPT_LEVEL: u32>(&mut self) {
        self.lit_sum_base_price = weight::<OPT_LEVEL>(self.lit_sum);
        self.lit_length_sum_base_price = weight::<OPT_LEVEL>(self.lit_length_sum);
        self.match_length_sum_base_price = weight::<OPT_LEVEL>(self.match_length_sum);
        self.off_code_sum_base_price = weight::<OPT_LEVEL>(self.off_code_sum);
    }

    /// `ZSTD_rescaleFreqs()`: on the first block of a frame
    /// (`litLengthSum == 0`), the statistics from a dictionary's entropy
    /// tables (`dict`, see [`DictCosts`]), or else literal statistics from
    /// the raw block and baseline statistics for the sequence symbols;
    /// afterwards, the accumulated statistics scaled down.
    fn rescale_freqs<const OPT_LEVEL: u32>(&mut self, src: &[u8], dict: Option<&DictCosts>) {
        self.price_type = PriceType::Dynamic;
        if self.lit_length_sum == 0 {
            // first block: init
            if src.len() <= PREDEF_THRESHOLD {
                // heuristic: use pre-defined stats for too small inputs
                self.price_type = PriceType::Predef;
            }
            if let Some(d) = dict {
                // huffman stats covering the full value set: table presumed
                // generated by dictionary
                self.price_type = PriceType::Dynamic;
                self.lit_freq = d.lit_freq;
                self.lit_sum = d.lit_freq.iter().sum();
                self.lit_length_freq = d.lit_length_freq;
                self.lit_length_sum = d.lit_length_freq.iter().sum();
                self.match_length_freq = d.match_length_freq;
                self.match_length_sum = d.match_length_freq.iter().sum();
                self.off_code_freq = d.off_code_freq;
                self.off_code_sum = d.off_code_freq.iter().sum();
                self.set_base_prices::<OPT_LEVEL>();
                return;
            }

            // first block, no dictionary: base initial cost of literals on
            // direct frequency within src
            let mut lit = MAX_LIT as u32;
            hist_count_simple(&mut self.lit_freq, &mut lit, src);
            self.lit_sum = downscale_stats(&mut self.lit_freq, 8, false);

            const BASE_LL_FREQS: [u32; MAX_LL as usize + 1] = [
                4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                1, 1, 1, 1, 1, 1, 1, 1,
            ];
            self.lit_length_freq = BASE_LL_FREQS;
            self.lit_length_sum = BASE_LL_FREQS.iter().sum();

            self.match_length_freq = [1; MAX_ML as usize + 1];
            self.match_length_sum = MAX_ML as u32 + 1;

            const BASE_OFC_FREQS: [u32; MAX_OFF as usize + 1] = [
                6, 2, 1, 1, 2, 3, 4, 4, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                1, 1, 1, 1,
            ];
            self.off_code_freq = BASE_OFC_FREQS;
            self.off_code_sum = BASE_OFC_FREQS.iter().sum();
        } else {
            // new block: scale down accumulated statistics
            self.lit_sum = scale_stats(&mut self.lit_freq, 12);
            self.lit_length_sum = scale_stats(&mut self.lit_length_freq, 11);
            self.match_length_sum = scale_stats(&mut self.match_length_freq, 11);
            self.off_code_sum = scale_stats(&mut self.off_code_freq, 11);
        }
        self.set_base_prices::<OPT_LEVEL>();
    }

    /// `ZSTD_rawLiteralsCost()` for one literal (`LIT_PRICE`).
    #[inline(always)]
    fn lit_price<const OPT_LEVEL: u32>(&self, lit: u8) -> i32 {
        if self.price_type == PriceType::Predef {
            return (6 * BITCOST_MULTIPLIER) as i32; // 6 bits per literal
        }
        // dynamic statistics
        let lit_price_max = self.lit_sum_base_price.wrapping_sub(BITCOST_MULTIPLIER);
        debug_assert!(self.lit_sum_base_price >= BITCOST_MULTIPLIER);
        let lit_price = weight::<OPT_LEVEL>(self.lit_freq[lit as usize]).min(lit_price_max);
        self.lit_sum_base_price.wrapping_sub(lit_price) as i32
    }

    /// `ZSTD_litLengthPrice()` (`LL_PRICE`).
    #[inline(always)]
    fn ll_price<const OPT_LEVEL: u32>(&self, lit_length: u32) -> i32 {
        debug_assert!(lit_length as usize <= BLOCKSIZE_MAX);
        if self.price_type == PriceType::Predef {
            return weight::<OPT_LEVEL>(lit_length) as i32;
        }
        // ZSTD_LLcode() can't compute a price for BLOCKSIZE_MAX, which isn't
        // representable: pretend it costs 1 bit more than BLOCKSIZE_MAX - 1
        // (the block would be all literals).
        if lit_length as usize == BLOCKSIZE_MAX {
            return BITCOST_MULTIPLIER as i32 + self.ll_price::<OPT_LEVEL>(lit_length - 1);
        }
        // dynamic statistics
        let code = ll_code(lit_length) as usize;
        (u32::from(LL_BITS[code]) * BITCOST_MULTIPLIER)
            .wrapping_add(self.lit_length_sum_base_price)
            .wrapping_sub(weight::<OPT_LEVEL>(self.lit_length_freq[code])) as i32
    }

    /// `LL_INCPRICE`: the price change of one more literal.
    #[inline(always)]
    fn ll_incprice<const OPT_LEVEL: u32>(&self, lit_length: u32) -> i32 {
        self.ll_price::<OPT_LEVEL>(lit_length) - self.ll_price::<OPT_LEVEL>(lit_length - 1)
    }

    /// `ZSTD_getMatchPrice()`: the price of the match part (offset and
    /// match length) of a sequence. Below `optLevel` 2, long offsets get a
    /// handicap (favoring decompression speed).
    #[inline(always)]
    fn match_price<const OPT_LEVEL: u32>(&self, off_base: u32, match_length: u32) -> i32 {
        let off_code = highbit32(off_base);
        debug_assert!(match_length >= MINMATCH);
        let ml_base = match_length - MINMATCH;

        if self.price_type == PriceType::Predef {
            // fixed scheme, does not use statistics
            return weight::<OPT_LEVEL>(ml_base).wrapping_add((16 + off_code) * BITCOST_MULTIPLIER)
                as i32;
        }

        // dynamic statistics
        let mut price = (off_code * BITCOST_MULTIPLIER).wrapping_add(
            self.off_code_sum_base_price
                .wrapping_sub(weight::<OPT_LEVEL>(self.off_code_freq[off_code as usize])),
        );
        if OPT_LEVEL < 2 && off_code >= 20 {
            // handicap for long distance offsets, favor decompression speed
            price = price.wrapping_add((off_code - 19) * 2 * BITCOST_MULTIPLIER);
        }

        // match length
        let ml_code = ml_code(ml_base) as usize;
        price = price.wrapping_add(
            (u32::from(ML_BITS[ml_code]) * BITCOST_MULTIPLIER).wrapping_add(
                self.match_length_sum_base_price
                    .wrapping_sub(weight::<OPT_LEVEL>(self.match_length_freq[ml_code])),
            ),
        );

        // heuristic: make matches a bit more costly to favor fewer
        // sequences (faster decompression)
        price = price.wrapping_add(BITCOST_MULTIPLIER / 5);
        price as i32
    }

    /// `ZSTD_updateStats()`.
    #[inline]
    fn update_stats(&mut self, literals: &[u8], off_base: u32, match_length: u32) {
        // literals
        for &l in literals {
            self.lit_freq[l as usize] += LITFREQ_ADD;
        }
        self.lit_sum += literals.len() as u32 * LITFREQ_ADD;

        // literal length
        let ll = ll_code(literals.len() as u32) as usize;
        self.lit_length_freq[ll] += 1;
        self.lit_length_sum += 1;

        // offset code: follows the storeSeq() numeric representation
        let off_code = highbit32(off_base) as usize;
        debug_assert!(off_code <= MAX_OFF as usize);
        self.off_code_freq[off_code] += 1;
        self.off_code_sum += 1;

        // match length
        let ml = ml_code(match_length - MINMATCH) as usize;
        self.match_length_freq[ml] += 1;
        self.match_length_sum += 1;
    }
}

/// `ZSTD_newRep()`.
#[inline(always)]
fn new_rep(rep: [u32; 3], off_base: u32, ll0: bool) -> [u32; 3] {
    let mut r = rep;
    update_rep(&mut r, off_base, ll0);
    r
}

// ---------------------------------------------------------------------------
// Binary tree match finder

/// The match state as the binary tree sees it during one block (see
/// `lazy::Searcher`).
struct Bt {
    base: *const u8,
    hash_table: *mut u32,
    bt: *mut u32,
    hash_table3: *mut u32,
    hash_log: u32,
    hash_log3: u32,
    /// `(1 << (chainLog - 1)) - 1`: the tree has pairs of entries.
    bt_mask: u32,
    search_log: u32,
    window_log: u32,
    /// `window.lowLimit`
    low_limit: u32,
    /// `window.dictLimit`
    dict_limit: u32,
    loaded_dict_end: u32,
    next_to_update: u32,
    /// `MIN(targetLength, ZSTD_OPT_NUM - 1)`
    sufficient_len: u32,
    /// The window start (the prefix start with an extDict) and the block
    /// end: every read is in between, or in the extDict (debug-checked by
    /// [`Bt::chk`]).
    lowest: *const u8,
    iend: *const u8,
    /// The extDict (`window.dictBase`), its readable part
    /// `dict_lowest..dict_end` (empty without an extDict) and the prefix
    /// start. With an attached dictionary (dictMatchState), its
    /// `dms->window.base`, start and `nextSrc` instead.
    dict_base: *const u8,
    dict_lowest: *const u8,
    dict_end: *const u8,
    prefix_start: *const u8,
    /// The attached dictionary's tables (dictMatchState only).
    dms: OptDms,
}

/// The attached dictionary's tables and limits as the optimal parser's
/// dictMatchState match finder uses them.
#[derive(Clone, Copy)]
struct OptDms {
    hash_table: *const u32,
    bt: *const u32,
    /// `dms->cParams.hashLog`, and the mask of its tree (`chainLog - 1`).
    hash_log: u32,
    bt_mask: u32,
    /// `dms->window.lowLimit` and `nextSrc - base` (`dmsHighLimit`).
    low_limit: u32,
    high_limit: u32,
}

impl OptDms {
    const NONE: OptDms = OptDms {
        hash_table: core::ptr::null(),
        bt: core::ptr::null(),
        hash_log: 0,
        bt_mask: 0,
        low_limit: 0,
        high_limit: 0,
    };
}

impl Bt {
    #[inline(always)]
    fn idx(&self, p: *const u8) -> u32 {
        (p as usize).wrapping_sub(self.base as usize) as u32
    }

    #[inline(always)]
    fn pos(&self, i: u32) -> *const u8 {
        self.base.wrapping_add(i as usize)
    }

    /// Debug check that `n` bytes at `p` are inside the readable window.
    #[inline(always)]
    fn chk(&self, p: *const u8, n: usize) {
        let (p, e) = (p as usize, (p as usize).wrapping_add(n));
        debug_assert!(
            (p >= self.lowest as usize && e <= self.iend as usize)
                || (p >= self.dict_lowest as usize && e <= self.dict_end as usize),
            "read out of the window"
        );
    }

    /// `ZSTD_count_2segments(ip, m, ilimit, dictEnd, prefixStart)`: the
    /// length of a match from the extDict position `m`.
    ///
    /// # Safety
    /// `ip..ilimit` is in the prefix, `m..dict_end` in the extDict.
    #[allow(unsafe_code)]
    #[inline(always)]
    unsafe fn count_ext(&self, ip: *const u8, m: *const u8, ilimit: *const u8) -> usize {
        self.chk(m, 0);
        // SAFETY: per the caller.
        unsafe { count_2segments(ip, m, ilimit, self.dict_end, self.prefix_start) }
    }

    /// The tree's match count (`ZSTD_insertBt1`,
    /// `ZSTD_insertBtAndGetAllMatches`): extends the `match_length` bytes
    /// known common between `ip` and the candidate `match_index` up to
    /// `iend`; returns the new length and a pointer to the candidate's byte
    /// after it (`match[matchLength]`). With `EXT`, a candidate in the
    /// extDict is counted on into the prefix (`ZSTD_count_2segments`).
    ///
    /// # Safety
    /// `ip..iend` is in the prefix, the candidate (from `match_length`) in
    /// the window.
    #[allow(unsafe_code)]
    #[inline(always)]
    unsafe fn tree_count<const EXT: bool>(
        &self,
        ip: *const u8,
        match_index: u32,
        match_length: usize,
        iend: *const u8,
    ) -> (usize, *const u8) {
        // SAFETY: per the caller.
        unsafe {
            if !EXT {
                let m = self.pos(match_index);
                self.chk(m, 0);
                let ml = match_length + count(ip.add(match_length), m.add(match_length), iend);
                return (ml, m.add(ml));
            }
            if match_index as usize + match_length >= self.dict_limit as usize {
                let mp = self.base.wrapping_add(match_index as usize + match_length);
                self.chk(mp, 0);
                let n = count(ip.add(match_length), mp, iend);
                (match_length + n, mp.add(n))
            } else {
                let dm = self.dict_base.wrapping_add(match_index as usize);
                let ml =
                    match_length + self.count_ext(ip.add(match_length), dm.add(match_length), iend);
                // prepare for the next read of match[matchLength]
                let m_ml = if match_index as usize + ml >= self.dict_limit as usize {
                    self.base.wrapping_add(match_index as usize + ml)
                } else {
                    dm.add(ml)
                };
                (ml, m_ml)
            }
        }
    }

    /// `ZSTD_getLowestMatchIndex()`.
    #[inline(always)]
    fn lowest_match_index(&self, curr: u32) -> u32 {
        let max_distance = 1u32 << self.window_log;
        let lowest_valid = self.low_limit;
        let within_window = if curr.wrapping_sub(lowest_valid) > max_distance {
            curr - max_distance
        } else {
            lowest_valid
        };
        if self.loaded_dict_end != 0 {
            lowest_valid
        } else {
            within_window
        }
    }
}

/// `ZSTD_readMINMATCH()`: 4 bytes, or the first 3 (shifted) for
/// `length == 3`; only for comparisons.
///
/// # Safety
/// `p..p + 4` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn read_minmatch<const LENGTH: u32>(p: *const u8) -> u32 {
    // SAFETY: per the caller.
    let v = unsafe { read32(p) };
    if LENGTH == 3 {
        if cfg!(target_endian = "little") {
            v << 8
        } else {
            v >> 8
        }
    } else {
        v
    }
}

/// `ZSTD_insertAndFindFirstIndexHash3()`: updates `hashTable3` up to `ip`
/// (excluded) and returns the 3-byte hash head of `ip`.
///
/// # Safety
/// `ip + 4 <= iend`, `*next_to_update3` in the window.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn insert_and_find_first_index_hash3(
    s: &Bt,
    next_to_update3: &mut u32,
    ip: *const u8,
) -> u32 {
    let hash_log3 = s.hash_log3;
    let mut idx = *next_to_update3;
    let target = s.idx(ip);
    // SAFETY: positions from nextToUpdate3 (in the window) up to ip are
    // readable for 4 bytes; hashes have `hash_log3` bits.
    unsafe {
        s.chk(ip, 4);
        let hash3 = hash3_ptr(ip, hash_log3);
        while idx < target {
            let p = s.pos(idx);
            s.chk(p, 4);
            *s.hash_table3.add(hash3_ptr(p, hash_log3)) = idx;
            idx += 1;
        }
        *next_to_update3 = target;
        *s.hash_table3.add(hash3)
    }
}

/// `ZSTD_insertBt1()` (noDict, or extDict with `EXT`): adds the position
/// `ip` to the tree.
/// Returns how many positions to move forward (more than 1 in long
/// repetitive patterns).
///
/// # Safety
/// `ip + 8 <= iend`, `ip` in the window.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn insert_bt1<const MLS: u32, const EXT: bool>(
    s: &Bt,
    ip: *const u8,
    iend: *const u8,
    target: u32,
) -> u32 {
    let bt = s.bt;
    let bt_mask = s.bt_mask;
    let curr = s.idx(ip);
    let bt_low = curr.saturating_sub(bt_mask);
    let mut common_length_smaller = 0usize;
    let mut common_length_larger = 0usize;
    let mut dummy32 = 0u32;
    let dummy: *mut u32 = &mut dummy32;
    // windowLow is based on target because only positions that will be in
    // the window at the end of the tree update are needed.
    let window_low = s.lowest_match_index(target);
    let mut match_end_idx = curr + 8 + 1;
    let mut best_length = 8usize;
    let mut nb_compares = 1u32 << s.search_log;
    debug_assert!(curr <= target);
    debug_assert!(window_low > 0);

    // SAFETY: the hash reads 8 bytes at ip <= iend - 8; tree accesses are
    // masked into a table of `2 << btLog` entries; candidates are at or
    // above `window_low` (in the window) and before `curr`; the byte
    // compared after a common prefix is before `iend` (the loop stops when
    // the prefix reaches it).
    unsafe {
        s.chk(ip, 8);
        let h = hash_mls::<MLS>(ip, s.hash_log);
        let mut match_index = *s.hash_table.add(h);
        let mut smaller_ptr = bt.add(2 * (curr & bt_mask) as usize);
        let mut larger_ptr = smaller_ptr.add(1);
        *s.hash_table.add(h) = curr; // update hash table

        while nb_compares > 0 && match_index >= window_low {
            let next_ptr = bt.add(2 * (match_index & bt_mask) as usize);
            // guaranteed minimum nb of common bytes
            let mut match_length = common_length_smaller.min(common_length_larger);
            debug_assert!(match_index < curr);
            let m_ml;
            (match_length, m_ml) = s.tree_count::<EXT>(ip, match_index, match_length, iend);

            if match_length > best_length {
                best_length = match_length;
                if match_length > (match_end_idx - match_index) as usize {
                    match_end_idx = match_index + match_length as u32;
                }
            }

            if ip.add(match_length) == iend {
                // equal: no way to know if inf or sup; drop, to guarantee
                // consistency (miss a bit of compression)
                break;
            }

            if *m_ml < *ip.add(match_length) {
                // match is smaller than current
                *smaller_ptr = match_index;
                common_length_smaller = match_length;
                if match_index <= bt_low {
                    smaller_ptr = dummy;
                    break;
                }
                smaller_ptr = next_ptr.add(1);
                match_index = *next_ptr.add(1);
            } else {
                // match is larger than current
                *larger_ptr = match_index;
                common_length_larger = match_length;
                if match_index <= bt_low {
                    larger_ptr = dummy;
                    break;
                }
                larger_ptr = next_ptr;
                match_index = *next_ptr;
            }
            nb_compares -= 1;
        }
        *smaller_ptr = 0;
        *larger_ptr = 0;
    }
    let positions = if best_length > 384 {
        192.min((best_length - 384) as u32) // speed optimization
    } else {
        0
    };
    debug_assert!(match_end_idx > curr + 8);
    positions.max(match_end_idx - (curr + 8))
}

/// `ZSTD_updateTree_internal()` (noDict, or extDict with `EXT`): inserts
/// the positions from `nextToUpdate` up to `ip` (excluded).
///
/// # Safety
/// `ip + 8 <= iend`.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn update_tree<const MLS: u32, const EXT: bool>(s: &mut Bt, ip: *const u8, iend: *const u8) {
    let target = s.idx(ip);
    let mut idx = s.next_to_update;
    while idx < target {
        // SAFETY: idx < target: the position is before ip, in the window.
        let forward = unsafe { insert_bt1::<MLS, EXT>(s, s.pos(idx), iend, target) };
        debug_assert!(idx < idx.wrapping_add(forward));
        idx += forward;
    }
    s.next_to_update = target;
}

/// `ZSTD_btGetAllMatches_internal()` with `ZSTD_insertBtAndGetAllMatches()`
/// (noDict, or extDict with `EXT`): fills `matches` with the repcode,
/// 3-byte hash and tree matches at `ip`, of increasing lengths, all at
/// least `length_to_beat` long, and returns their number. `ll0`: the literal length before `ip` is 0.
///
/// # Safety
/// `ip + 8 <= iend`, `iend` the block end.
#[allow(unsafe_code, clippy::too_many_arguments)]
#[inline(never)]
unsafe fn get_all_matches<const MLS: u32, const EXT: bool, const DMS: bool>(
    s: &mut Bt,
    matches: &mut [Match],
    next_to_update3: &mut u32,
    ip: *const u8,
    iend: *const u8,
    rep: &[u32; 3],
    ll0: u32,
    length_to_beat: u32,
) -> u32 {
    if (ip as usize) < (s.pos(s.next_to_update) as usize) {
        return 0; // skipped area
    }
    let ilimit = iend;
    let sufficient_len = s.sufficient_len;
    let min_match = if MLS == 3 { 3 } else { 4 };
    let bt = s.bt;
    let bt_mask = s.bt_mask;
    let mut common_length_smaller = 0usize;
    let mut common_length_larger = 0usize;
    let mut dummy32 = 0u32;
    let dummy: *mut u32 = &mut dummy32;
    let mut mnum = 0usize;
    let mut nb_compares = 1u32 << s.search_log;
    let mut best_length = (length_to_beat - 1) as usize;
    debug_assert!(matches.len() >= OPT_SIZE);
    debug_assert!(ll0 <= 1);

    // SAFETY: see the module docs. Repcode candidates are checked to be in
    // the prefix (`repOffset - 1 < curr - dictLimit`) before being read;
    // the 3-byte hash and tree candidates are at or above `matchLow` and
    // before `curr`; forward reads stop at `ilimit` (the block end).
    unsafe {
        let curr = s.idx(ip);
        // The parser searches consecutive positions: prefetch the next one's
        // hash bucket(s) and its slot in the tree (random accesses into
        // tables of up to 16 and 64 MiB at the high levels, missing every
        // cache) now, so that the misses overlap this position's search.
        // The hash reads 8 bytes at `ip + 1`, in the block when
        // `ip + 1 + 8 <= iend`.
        if ip.add(1 + HASH_READ_SIZE) <= iend {
            let nip = ip.add(1);
            prefetch_l1(s.hash_table.add(hash_mls::<MLS>(nip, s.hash_log)).cast());
            if MLS == 3 {
                prefetch_l1(s.hash_table3.add(hash3_ptr(nip, s.hash_log3)).cast());
            }
            prefetch_l1(bt.add(2 * ((curr + 1) & bt_mask) as usize).cast());
        }

        update_tree::<MLS, EXT>(s, ip, iend);

        let dict_limit = s.dict_limit;
        let bt_low = curr.saturating_sub(bt_mask);
        let window_low = s.lowest_match_index(curr);
        let match_low = if window_low != 0 { window_low } else { 1 };
        let mut match_end_idx = curr + 8 + 1; // farthest referenced position of any match
        s.chk(ip, 8);
        let h = hash_mls::<MLS>(ip, s.hash_log);
        let mut match_index = *s.hash_table.add(h);
        let mut smaller_ptr = bt.add(2 * (curr & bt_mask) as usize);
        let mut larger_ptr = smaller_ptr.add(1);

        // check repCode
        let last_r = REP_NUM + ll0;
        for rep_code in ll0..last_r {
            let rep_offset = if rep_code == REP_NUM {
                rep[0].wrapping_sub(1)
            } else {
                rep[rep_code as usize]
            };
            let rep_index = curr.wrapping_sub(rep_offset);
            let mut rep_len = 0usize;
            debug_assert!(curr >= dict_limit);
            // intentional overflow, discards 0 and -1: equivalent to
            // `curr > repIndex >= dictLimit`
            if rep_offset.wrapping_sub(1) < curr - dict_limit {
                // We must validate the repcode offset because when we're
                // using a dictionary the valid offset range shrinks when the
                // dictionary goes out of bounds.
                if rep_index >= window_low && {
                    let r = ip.sub(rep_offset as usize);
                    s.chk(r, 4);
                    read_minmatch::<MLS>(ip) == read_minmatch::<MLS>(r)
                } {
                    rep_len = count(
                        ip.add(min_match),
                        ip.add(min_match).sub(rep_offset as usize),
                        ilimit,
                    ) + min_match;
                }
            } else if EXT {
                // repIndex < dictLimit || repIndex >= curr
                let rep_match = s.dict_base.wrapping_add(rep_index as usize);
                // `curr > repIndex >= windowLow`, the 4 bytes in the extDict
                if (rep_offset.wrapping_sub(1) < curr - window_low)
                    & index_overlap_check(dict_limit, rep_index)
                    && {
                        s.chk(rep_match, 4);
                        read_minmatch::<MLS>(ip) == read_minmatch::<MLS>(rep_match)
                    }
                {
                    rep_len = s.count_ext(ip.add(min_match), rep_match.add(min_match), ilimit)
                        + min_match;
                }
            } else if DMS {
                // repIndex < dictLimit || repIndex >= curr: in the attached
                // dictionary (`dmsBase + repIndex - dmsIndexDelta`)
                let dms_index_delta = window_low.wrapping_sub(s.dms.high_limit);
                let rep_match = s
                    .dict_base
                    .wrapping_add(rep_index.wrapping_sub(dms_index_delta) as usize);
                // `curr > repIndex >= dmsLowLimit`, the 4 bytes in the
                // dictionary
                if (rep_offset.wrapping_sub(1)
                    < curr.wrapping_sub(s.dms.low_limit.wrapping_add(dms_index_delta)))
                    & index_overlap_check(dict_limit, rep_index)
                    && {
                        s.chk(rep_match, 4);
                        read_minmatch::<MLS>(ip) == read_minmatch::<MLS>(rep_match)
                    }
                {
                    rep_len = s.count_ext(ip.add(min_match), rep_match.add(min_match), ilimit)
                        + min_match;
                }
            }

            // save longer solution
            if rep_len > best_length {
                best_length = rep_len;
                matches[mnum] = Match {
                    off: rep_code - ll0 + 1, // REPCODE_TO_OFFBASE: 1..=3
                    len: rep_len as u32,
                };
                mnum += 1;
                if (rep_len > sufficient_len as usize) | (ip.add(rep_len) == ilimit) {
                    return mnum as u32; // best possible
                }
            }
        }

        // HC3 match finder
        if MLS == 3 && best_length < MLS as usize {
            let match_index3 = insert_and_find_first_index_hash3(s, next_to_update3, ip);
            if (match_index3 >= match_low) & (curr.wrapping_sub(match_index3) < (1 << 18)) {
                // heuristic: longer distance likely too expensive
                debug_assert!(match_index3 < curr);
                let mlen = if !EXT || match_index3 >= dict_limit {
                    let m = s.pos(match_index3);
                    s.chk(m, 0);
                    count(ip, m, ilimit)
                } else {
                    let m = s.dict_base.wrapping_add(match_index3 as usize);
                    s.count_ext(ip, m, ilimit)
                };
                // save best solution
                if mlen >= MLS as usize {
                    best_length = mlen;
                    debug_assert!(mnum == 0); // no prior solution
                    matches[0] = Match {
                        off: offset_to_offbase(curr - match_index3),
                        len: mlen as u32,
                    };
                    mnum = 1;
                    if (mlen > sufficient_len as usize) | (ip.add(mlen) == ilimit) {
                        // best possible length
                        s.next_to_update = curr + 1; // skip insertion
                        return 1;
                    }
                }
            }
        }

        *s.hash_table.add(h) = curr; // update hash table

        while nb_compares > 0 && match_index >= match_low {
            let next_ptr = bt.add(2 * (match_index & bt_mask) as usize);
            // guaranteed minimum nb of common bytes
            let mut match_length = common_length_smaller.min(common_length_larger);
            debug_assert!(curr > match_index);
            let m_ml;
            (match_length, m_ml) = s.tree_count::<EXT>(ip, match_index, match_length, ilimit);

            if match_length > best_length {
                debug_assert!(match_end_idx > match_index);
                if match_length > (match_end_idx - match_index) as usize {
                    match_end_idx = match_index + match_length as u32;
                }
                best_length = match_length;
                matches[mnum] = Match {
                    off: offset_to_offbase(curr - match_index),
                    len: match_length as u32,
                };
                mnum += 1;
                if (match_length > OPT_NUM as usize) | (ip.add(match_length) == ilimit) {
                    // equal: no way to know if inf or sup; drop, to
                    // preserve bt consistency
                    if DMS {
                        nb_compares = 0; // break should also skip searching dms
                    }
                    break;
                }
            }

            if *m_ml < *ip.add(match_length) {
                // match smaller than current
                *smaller_ptr = match_index;
                common_length_smaller = match_length;
                if match_index <= bt_low {
                    smaller_ptr = dummy;
                    break;
                }
                smaller_ptr = next_ptr.add(1);
                match_index = *next_ptr.add(1);
            } else {
                *larger_ptr = match_index;
                common_length_larger = match_length;
                if match_index <= bt_low {
                    larger_ptr = dummy;
                    break;
                }
                larger_ptr = next_ptr;
                match_index = *next_ptr;
            }
            nb_compares -= 1;
        }
        *smaller_ptr = 0;
        *larger_ptr = 0;

        if DMS && nb_compares > 0 {
            // the attached dictionary's tree (sorted, read only), with the
            // compares left; its candidates are positions it inserted (at
            // least 8 bytes before its end), counted on into the prefix
            let d = s.dms;
            let dms_index_delta = window_low.wrapping_sub(d.high_limit);
            let dms_bt_low = if d.bt_mask < d.high_limit - d.low_limit {
                d.high_limit - d.bt_mask
            } else {
                d.low_limit
            };
            let dms_h = hash_mls::<MLS>(ip, d.hash_log);
            let mut dict_match_index = *d.hash_table.add(dms_h);
            common_length_smaller = 0;
            common_length_larger = 0;
            while nb_compares > 0 && dict_match_index > d.low_limit {
                let next_ptr = d.bt.add(2 * (dict_match_index & d.bt_mask) as usize);
                // guaranteed minimum nb of common bytes
                let mut match_length = common_length_smaller.min(common_length_larger);
                let mut m = s.dict_base.wrapping_add(dict_match_index as usize);
                s.chk(m, 0);
                // (a guaranteed length past the dictionary's end counts no
                // more, as in libzstd)
                if m.wrapping_add(match_length) as usize <= s.dict_end as usize {
                    match_length += s.count_ext(ip.add(match_length), m.add(match_length), ilimit);
                }
                if dict_match_index as usize + match_length >= d.high_limit as usize {
                    // to prepare for next usage of match[matchLength]
                    m = s
                        .base
                        .wrapping_add(dict_match_index.wrapping_add(dms_index_delta) as usize);
                }

                if match_length > best_length {
                    let match_index = dict_match_index.wrapping_add(dms_index_delta);
                    if match_length > match_end_idx.wrapping_sub(match_index) as usize {
                        match_end_idx = match_index + match_length as u32;
                    }
                    best_length = match_length;
                    matches[mnum] = Match {
                        off: offset_to_offbase(curr - match_index),
                        len: match_length as u32,
                    };
                    mnum += 1;
                    if (match_length > OPT_NUM as usize) | (ip.add(match_length) == ilimit) {
                        break; // drop, to guarantee consistency
                    }
                }

                if dict_match_index <= dms_bt_low {
                    break; // beyond tree size, stop the search
                }
                let mm = m.wrapping_add(match_length);
                s.chk(mm, 1);
                if *mm < *ip.add(match_length) {
                    common_length_smaller = match_length;
                    dict_match_index = *next_ptr.add(1);
                } else {
                    // match is larger than current
                    common_length_larger = match_length;
                    dict_match_index = *next_ptr;
                }
                nb_compares -= 1;
            }
        }

        debug_assert!(match_end_idx > curr + 8);
        s.next_to_update = match_end_idx - 8; // skip repetitive patterns
    }
    mnum as u32
}

// ---------------------------------------------------------------------------
// Parser

/// `ZSTD_compressBlock_btopt()`, `ZSTD_compressBlock_btultra()` and
/// `ZSTD_compressBlock_btultra2()` (noDict): finds the sequences of the
/// block `buf[start..end]`; the same contract as
/// [`compress_block_fast`](super::fast::compress_block_fast). Picks the
/// variant like libzstd: `optLevel` 0 for btopt, 2 for btultra and
/// btultra2, `mls = clamp(minMatch, 3, 6)`.
#[inline(never)]
pub(crate) fn compress_block_opt(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    match ms.cparams.strategy {
        Strategy::BtOpt => {
            opt_dispatch::<0, false, false, false>(ms, ss, rep, buf, start, end, &[])
        }
        Strategy::BtUltra => {
            opt_dispatch::<2, false, false, false>(ms, ss, rep, buf, start, end, &[])
        }
        Strategy::BtUltra2 => {
            let curr = ms.window.index(start);
            // 2-passes strategy: a first pass over the first block of a
            // frame collects statistics to seed the real pass. Only works
            // when no data was loaded before (no dictionary, no prefix);
            // inputs of at most 8 bytes use the predefined prices anyway.
            let first_block = ms.opt.as_ref().is_some_and(|o| o.stats.lit_length_sum == 0);
            if first_block
                && ms.window.dict_limit == ms.window.low_limit
                && curr == ms.window.dict_limit
                && end - start > PREDEF_THRESHOLD
            {
                init_stats_ultra::<false>(ms, ss, rep, buf, start, end, &[]);
            }
            opt_dispatch::<2, false, false, false>(ms, ss, rep, buf, start, end, &[])
        }
        s => unreachable!("not an optimal strategy: {s:?}"),
    }
}

/// `ZSTD_compressBlock_btopt_dictMatchState()` and
/// `ZSTD_compressBlock_btultra_dictMatchState()` (also for btultra2, as in
/// libzstd): as [`compress_block_opt`], with an attached dictionary
/// ([`MatchState::dms`]) whose repcodes and binary tree are searched too.
#[inline(never)]
pub(crate) fn compress_block_opt_dict_match_state(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    match ms.cparams.strategy {
        Strategy::BtOpt => opt_dispatch::<0, false, true, false>(ms, ss, rep, buf, start, end, &[]),
        Strategy::BtUltra | Strategy::BtUltra2 => {
            opt_dispatch::<2, false, true, false>(ms, ss, rep, buf, start, end, &[])
        }
        s => unreachable!("not an optimal strategy: {s:?}"),
    }
}

/// `ZSTD_updateTree()` for a dictionary's content (`ZSTD_loadDictionaryContent`
/// for btlazy2 and the optimal strategies): inserts the positions from
/// `nextToUpdate` up to `end - HASH_READ_SIZE` of `buf` into the (sorted)
/// binary tree, with `mls = minMatch`.
///
/// # Panics
///
/// If the window is not one segment of `buf` covering those positions, or
/// the tables have not the size of the parameters.
#[allow(unsafe_code)]
pub(crate) fn update_tree_for_dict(ms: &mut MatchState, buf: &[u8], end: usize) {
    let cparams = ms.cparams;
    // These checks make the raw-pointer accesses below sound.
    assert!(end <= buf.len() && end >= HASH_READ_SIZE);
    assert!(cparams.hash_log <= 32 && ms.hash_table.len() == 1usize << cparams.hash_log);
    assert!(cparams.chain_log >= 1 && cparams.chain_log <= 32);
    assert!(ms.chain_table.len() == 1usize << cparams.chain_log);
    assert!(!ms.window.has_ext_dict());
    let window_start = ms.window.pos(ms.window.dict_limit);
    assert!(window_start <= ms.window.pos(ms.next_to_update));
    assert!(end.wrapping_sub(ms.window.base) <= u32::MAX as usize);
    let bp = buf.as_ptr();
    let base = bp.wrapping_add(ms.window.base);
    let iend = bp.wrapping_add(end);
    let mut s = Bt {
        base,
        hash_table: ms.hash_table.as_mut_ptr(),
        bt: ms.chain_table.as_mut_ptr(),
        hash_table3: ms.hash_table3.as_mut_ptr(),
        hash_log: cparams.hash_log,
        hash_log3: ms.hash_log3,
        bt_mask: (1u32 << (cparams.chain_log - 1)) - 1,
        search_log: cparams.search_log,
        window_log: cparams.window_log,
        low_limit: ms.window.low_limit,
        dict_limit: ms.window.dict_limit,
        loaded_dict_end: ms.loaded_dict_end,
        next_to_update: ms.next_to_update,
        sufficient_len: cparams.target_length.min(OPT_NUM - 1),
        lowest: bp.wrapping_add(window_start),
        iend,
        dict_base: base,
        dict_lowest: iend,
        dict_end: iend,
        prefix_start: bp.wrapping_add(window_start),
        dms: OptDms::NONE,
    };
    let ip = iend.wrapping_sub(HASH_READ_SIZE);
    // SAFETY: the positions inserted are in the window, before `ip`, and
    // readable up to `iend` (asserted above); see `update_tree`.
    unsafe {
        match cparams.min_match {
            5 => update_tree::<5, false>(&mut s, ip, iend),
            6 => update_tree::<6, false>(&mut s, ip, iend),
            7 => update_tree::<7, false>(&mut s, ip, iend),
            _ => update_tree::<4, false>(&mut s, ip, iend),
        }
    }
    ms.next_to_update = s.next_to_update;
}

/// `ZSTD_compressBlock_btopt_extDict()` and `ZSTD_compressBlock_btultra_extDict()`
/// (also for btultra2, as in libzstd): as [`compress_block_opt`], for a
/// window with an extDict segment (see
/// [`compress_block_fast_ext_dict`](super::fast::compress_block_fast_ext_dict)).
#[inline(never)]
pub(crate) fn compress_block_opt_ext_dict(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    match ms.cparams.strategy {
        Strategy::BtOpt => opt_dispatch::<0, true, false, false>(ms, ss, rep, buf, start, end, &[]),
        Strategy::BtUltra | Strategy::BtUltra2 => {
            opt_dispatch::<2, true, false, false>(ms, ss, rep, buf, start, end, &[])
        }
        s => unreachable!("not an optimal strategy: {s:?}"),
    }
}

/// `ZSTD_initStats_ultra()`: a first pass over the first block, only to
/// seed the statistics; then the window forgets it (indices move up by the
/// block size, so nothing in the tables is a valid candidate any more).
/// With `LDM`, the pass also takes the block's long matches `ldm` (libzstd's
/// `ms->ldmSeqStore` is set during it).
fn init_stats_ultra<const LDM: bool>(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &[u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
    ldm: &[RawSeq],
) {
    let mut tmp_rep = *rep; // updated rep codes will sink here
    let src_size = end - start;
    debug_assert!(ss.nb_seq == 0); // no ldm sequences stored
    debug_assert!(ms.window.dict_limit == ms.window.low_limit); // no dictionary
    debug_assert!(ms.window.dict_limit.wrapping_sub(ms.next_to_update) <= 1); // no prefix
    // generate stats into ms.opt
    opt_dispatch::<2, false, false, LDM>(ms, ss, &mut tmp_rep, buf, start, end, ldm);
    // invalidate the first scan from history, only keep entropy stats
    ss.reset();
    ms.window.base = ms.window.base.wrapping_sub(src_size);
    ms.window.dict_limit += src_size as u32;
    ms.window.low_limit = ms.window.dict_limit;
    ms.next_to_update = ms.window.dict_limit;
}

/// `ZSTD_compressBlock_btopt()`, `_btultra()`, `_btultra2()` and their
/// `_dictMatchState` and `_extDict` variants with LDM (`ms->ldmSeqStore`
/// set by `ZSTD_ldm_blockCompress`): as [`compress_block_opt`] and the
/// others, with the block's long matches `ldm` as extra candidates
/// (`ZSTD_optLdm_*`). Picks the dictionary mode like
/// [`build_seq_store`](super::fast::build_seq_store).
#[inline(never)]
pub(crate) fn compress_block_opt_ldm(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
    ldm: &[RawSeq],
) -> usize {
    let opt0 = ms.cparams.strategy == Strategy::BtOpt;
    debug_assert!(ms.cparams.strategy >= Strategy::BtOpt);
    if ms.window.has_ext_dict() {
        if opt0 {
            opt_dispatch::<0, true, false, true>(ms, ss, rep, buf, start, end, ldm)
        } else {
            opt_dispatch::<2, true, false, true>(ms, ss, rep, buf, start, end, ldm)
        }
    } else if ms.dms {
        if opt0 {
            opt_dispatch::<0, false, true, true>(ms, ss, rep, buf, start, end, ldm)
        } else {
            opt_dispatch::<2, false, true, true>(ms, ss, rep, buf, start, end, ldm)
        }
    } else if opt0 {
        opt_dispatch::<0, false, false, true>(ms, ss, rep, buf, start, end, ldm)
    } else {
        if ms.cparams.strategy == Strategy::BtUltra2 {
            // see compress_block_opt
            let curr = ms.window.index(start);
            let first_block = ms.opt.as_ref().is_some_and(|o| o.stats.lit_length_sum == 0);
            if first_block
                && ss.nb_seq == 0
                && ms.window.dict_limit == ms.window.low_limit
                && curr == ms.window.dict_limit
                && end - start > PREDEF_THRESHOLD
            {
                init_stats_ultra::<true>(ms, ss, rep, buf, start, end, ldm);
            }
        }
        opt_dispatch::<2, false, false, true>(ms, ss, rep, buf, start, end, ldm)
    }
}

/// Picks the `mls` instance of [`opt_generic`].
#[inline(always)]
fn opt_dispatch<const OPT_LEVEL: u32, const EXT: bool, const DMS: bool, const LDM: bool>(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
    ldm: &[RawSeq],
) -> usize {
    match ms.cparams.min_match.clamp(3, 6) {
        3 => opt_generic::<3, OPT_LEVEL, EXT, DMS, LDM>(ms, ss, rep, buf, start, end, ldm),
        4 => opt_generic::<4, OPT_LEVEL, EXT, DMS, LDM>(ms, ss, rep, buf, start, end, ldm),
        5 => opt_generic::<5, OPT_LEVEL, EXT, DMS, LDM>(ms, ss, rep, buf, start, end, ldm),
        _ => opt_generic::<6, OPT_LEVEL, EXT, DMS, LDM>(ms, ss, rep, buf, start, end, ldm),
    }
}

/// `ZSTD_optLdm_t`: the parser's view of the block's long matches (a copy
/// of `ms->ldmSeqStore`: consuming them here does not move the original),
/// and the current candidate, `[start_pos_in_block, end_pos_in_block)` at
/// `offset`.
struct OptLdm<'a> {
    seq: &'a [RawSeq],
    pos: usize,
    pos_in_sequence: usize,
    start_pos_in_block: u32,
    end_pos_in_block: u32,
    offset: u32,
}

impl<'a> OptLdm<'a> {
    fn new(seq: &'a [RawSeq]) -> Self {
        OptLdm {
            seq,
            pos: 0,
            pos_in_sequence: 0,
            start_pos_in_block: 0,
            end_pos_in_block: 0,
            offset: 0,
        }
    }

    /// `ZSTD_optLdm_skipRawSeqStoreBytes()`: moves `nb_bytes` forward in
    /// the sequences.
    fn skip_bytes(&mut self, nb_bytes: usize) {
        let mut curr_pos = (self.pos_in_sequence + nb_bytes) as u32;
        while curr_pos != 0 && self.pos < self.seq.len() {
            let s = self.seq[self.pos];
            if curr_pos >= s.lit_length + s.match_length {
                curr_pos -= s.lit_length + s.match_length;
                self.pos += 1;
            } else {
                self.pos_in_sequence = curr_pos as usize;
                break;
            }
        }
        if curr_pos == 0 || self.pos == self.seq.len() {
            self.pos_in_sequence = 0;
        }
    }

    /// `ZSTD_opt_getNextMatchAndUpdateSeqStore()`: the next candidate
    /// after `curr_pos_in_block`, with `block_bytes_remaining` bytes left in
    /// the block.
    fn get_next_match(&mut self, curr_pos_in_block: u32, block_bytes_remaining: u32) {
        // Setting match end position to MAX to ensure we never use an LDM
        // during this block
        if self.pos >= self.seq.len() {
            self.start_pos_in_block = u32::MAX;
            self.end_pos_in_block = u32::MAX;
            return;
        }
        // Calculate appropriate bytes left in matchLength and litLength
        // after adjusting based on posInSequence
        let curr_seq = self.seq[self.pos];
        let pis = self.pos_in_sequence as u32;
        debug_assert!(pis <= curr_seq.lit_length + curr_seq.match_length);
        let curr_block_end_pos = curr_pos_in_block + block_bytes_remaining;
        let literals_bytes_remaining = curr_seq.lit_length.saturating_sub(pis);
        let match_bytes_remaining = if literals_bytes_remaining == 0 {
            curr_seq.match_length - (pis - curr_seq.lit_length)
        } else {
            curr_seq.match_length
        };

        // If there are more literal bytes than bytes remaining in block, no
        // ldm is possible
        if literals_bytes_remaining >= block_bytes_remaining {
            self.start_pos_in_block = u32::MAX;
            self.end_pos_in_block = u32::MAX;
            self.skip_bytes(block_bytes_remaining as usize);
            return;
        }

        // Matches may be < minMatch by this process. In that case, we will
        // reject them when we are deciding whether or not to add the ldm
        self.start_pos_in_block = curr_pos_in_block + literals_bytes_remaining;
        self.end_pos_in_block = self.start_pos_in_block + match_bytes_remaining;
        self.offset = curr_seq.offset;

        if self.end_pos_in_block > curr_block_end_pos {
            // Match ends after the block ends, we can't use the whole match
            self.end_pos_in_block = curr_block_end_pos;
            self.skip_bytes((curr_block_end_pos - curr_pos_in_block) as usize);
        } else {
            // Consume nb of bytes equal to size of sequence left
            self.skip_bytes((literals_bytes_remaining + match_bytes_remaining) as usize);
        }
    }

    /// `ZSTD_optLdm_maybeAddMatch()`: adds the candidate to `matches` if it
    /// covers `curr_pos_in_block` with at least `min_match` bytes, and is
    /// longer than the longest match found.
    #[inline]
    fn maybe_add_match(
        &self,
        matches: &mut [Match],
        nb_matches: &mut usize,
        curr_pos_in_block: u32,
        min_match: u32,
    ) {
        let pos_diff = curr_pos_in_block.wrapping_sub(self.start_pos_in_block);
        let candidate_match_length = self
            .end_pos_in_block
            .wrapping_sub(self.start_pos_in_block)
            .wrapping_sub(pos_diff);

        // Ensure that current block position is not outside of the match
        if curr_pos_in_block < self.start_pos_in_block
            || curr_pos_in_block >= self.end_pos_in_block
            || candidate_match_length < min_match
        {
            return;
        }

        let n = *nb_matches;
        if n == 0 || (candidate_match_length > matches[n - 1].len && n < OPT_NUM as usize) {
            matches[n] = Match {
                off: offset_to_offbase(self.offset),
                len: candidate_match_length,
            };
            *nb_matches = n + 1;
        }
    }

    /// `ZSTD_optLdm_processMatchCandidate()`: moves to the next candidate
    /// if past the current one, then maybe adds it to `matches`.
    #[inline]
    fn process_match_candidate(
        &mut self,
        matches: &mut [Match],
        nb_matches: &mut usize,
        curr_pos_in_block: u32,
        remaining_bytes: u32,
        min_match: u32,
    ) {
        if self.pos >= self.seq.len() {
            return;
        }
        if curr_pos_in_block >= self.end_pos_in_block {
            if curr_pos_in_block > self.end_pos_in_block {
                // The position is not necessarily at the end of a match
                // from the ldm seq store, and will often be some bytes over
                // beyond matchEndPosInBlock: correct for these overshoots
                let pos_overshoot = curr_pos_in_block - self.end_pos_in_block;
                self.skip_bytes(pos_overshoot as usize);
            }
            self.get_next_match(curr_pos_in_block, remaining_bytes);
        }
        self.maybe_add_match(matches, nb_matches, curr_pos_in_block, min_match);
    }
}

/// `ZSTD_compressBlock_opt_generic()` with `dictMode` `ZSTD_noDict` (or
/// `ZSTD_extDict` with `EXT`, `ZSTD_dictMatchState` with `DMS`), and with
/// LDM candidates `ldm` when `LDM` (else `ldm` is empty and unused).
#[allow(unsafe_code)]
#[inline(always)]
fn opt_generic<
    const MLS: u32,
    const OPT_LEVEL: u32,
    const EXT: bool,
    const DMS: bool,
    const LDM: bool,
>(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
    ldm: &[RawSeq],
) -> usize {
    let cparams = ms.cparams;
    // These checks make the raw-pointer accesses below sound.
    assert!(start <= end && end <= buf.len());
    assert!(cparams.hash_log <= 32 && ms.hash_table.len() == 1usize << cparams.hash_log);
    assert!(cparams.chain_log >= 1 && cparams.chain_log <= 32);
    assert!(ms.chain_table.len() == 1usize << cparams.chain_log);
    assert!(cparams.search_log < 32);
    if MLS == 3 {
        assert!(ms.hash_log3 >= 1 && ms.hash_log3 <= 32);
        assert!(ms.hash_table3.len() == 1usize << ms.hash_log3);
    }
    assert!(EXT == ms.window.has_ext_dict());
    assert!(!(EXT && DMS));
    let window_start = ms.window.pos(ms.window.dict_limit);
    assert!(window_start <= start, "window start is not in the buffer");
    assert!(ms.next_to_update >= ms.window.dict_limit);
    assert!(end.wrapping_sub(ms.window.base) <= u32::MAX as usize);
    // With an extDict: it is in its buffer (and the prefix in `buf`). With
    // an attached dictionary: its content and tables, checked by
    // `DmsView::new`.
    let cd = if DMS {
        Some(ms.dict.clone().expect("attached dictionary"))
    } else {
        None
    };
    let (dict_base, dict_lowest, dict_end, dms) = if EXT {
        let w = ExtWindow::new(
            &ms.window,
            buf,
            ms.ext_buf(buf),
            start,
            end,
            ms.window.low_limit,
            ms.window.dict_limit,
        );
        (w.dict_base, w.dict_start, w.dict_end, OptDms::NONE)
    } else if let Some(cd) = &cd {
        let d = DmsView::new(cd);
        assert!(d.chain_log >= 1 && d.chain_log <= 32 && d.hash_log <= 32);
        assert!(cd.ms.chain_table.len() == 1usize << d.chain_log);
        assert!(ms.window.dict_limit >= d.high_limit);
        let dms = OptDms {
            hash_table: d.hash_table,
            bt: d.chain_table,
            hash_log: d.hash_log,
            bt_mask: (1u32 << (d.chain_log - 1)) - 1,
            low_limit: d.low_limit,
            high_limit: d.high_limit,
        };
        (d.base, d.start, d.end, dms)
    } else {
        (buf.as_ptr(), buf.as_ptr(), buf.as_ptr(), OptDms::NONE)
    };
    debug_assert!(OPT_LEVEL <= 2);

    let mut st = ms.opt.take().expect("optimal parser state");
    let OptState {
        stats,
        price_table,
        match_table,
        dict_costs,
    } = &mut *st;
    assert!(price_table.len() == OPT_SIZE && match_table.len() == OPT_SIZE);
    let opt = &mut price_table[..];
    let matches = &mut match_table[..];

    let bp = buf.as_ptr();
    let base = bp.wrapping_add(ms.window.base);
    let istart = bp.wrapping_add(start);
    let iend = bp.wrapping_add(end);
    let ilimit = iend.wrapping_sub(8);
    let prefix_start = base.wrapping_add(ms.window.dict_limit as usize);
    let sufficient_len = cparams.target_length.min(OPT_NUM - 1);
    let min_match = if cparams.min_match == 3 { 3 } else { 4 };
    let mut next_to_update3 = ms.next_to_update;

    let mut s = Bt {
        base,
        hash_table: ms.hash_table.as_mut_ptr(),
        bt: ms.chain_table.as_mut_ptr(),
        hash_table3: ms.hash_table3.as_mut_ptr(),
        hash_log: cparams.hash_log,
        hash_log3: ms.hash_log3,
        bt_mask: (1u32 << (cparams.chain_log - 1)) - 1,
        search_log: cparams.search_log,
        window_log: cparams.window_log,
        low_limit: ms.window.low_limit,
        dict_limit: ms.window.dict_limit,
        loaded_dict_end: ms.loaded_dict_end,
        next_to_update: ms.next_to_update,
        sufficient_len,
        lowest: bp.wrapping_add(window_start),
        iend,
        dict_base,
        dict_lowest,
        dict_end,
        prefix_start,
        dms,
    };

    let mut ip = istart;
    let mut anchor = istart;
    let mut last_stretch = Optimal::default();

    let mut opt_ldm = OptLdm::new(ldm);
    if LDM {
        opt_ldm.get_next_match(0, (end - start) as u32);
    }
    // The LDM candidates are within the block (`OptLdm::get_next_match`
    // cuts them at its end), so the matches stay bounded by `iend`.
    let block_pos = |p: *const u8| (p as usize - istart as usize) as u32;
    let block_rest = |p: *const u8| (iend as usize - p as usize) as u32;

    // init
    stats.rescale_freqs::<OPT_LEVEL>(&buf[start..end], dict_costs.as_deref());
    ip = ip.wrapping_add((ip == prefix_start) as usize);

    // SAFETY (whole loop): see the module docs. Matches are searched at
    // `ip < ilimit` and `inr <= ilimit` (8 bytes before the block end);
    // literals are read at positions before `iend` (`ip + cur - 1` with
    // `cur <= last_pos`, `ip + cur < iend` checked); stored sequences stay
    // within the block (their lengths come from matches bounded by `iend`).
    unsafe {
        // Match Loop
        'main: while ip < ilimit {
            let mut cur: u32;
            let mut last_pos: u32;

            'shortest_path: {
                // find first match
                {
                    let litlen = (ip as usize - anchor as usize) as u32;
                    let ll0 = u32::from(litlen == 0);
                    #[allow(unused_mut)]
                    let mut nb_matches = get_all_matches::<MLS, EXT, DMS>(
                        &mut s,
                        matches,
                        &mut next_to_update3,
                        ip,
                        iend,
                        rep,
                        ll0,
                        min_match,
                    ) as usize;
                    if LDM {
                        opt_ldm.process_match_candidate(
                            matches,
                            &mut nb_matches,
                            block_pos(ip),
                            block_rest(ip),
                            min_match,
                        );
                    }
                    if nb_matches == 0 {
                        ip = ip.add(1);
                        continue 'main;
                    }

                    // Match found: store this solution, and eventually find
                    // more candidates. During this forward pass, `opt`
                    // stores stretches: a match followed by N literals.

                    // initialize opt[0]: only literals so far; their price
                    // is static for the forward pass, but the literal
                    // length price varies.
                    opt[0].mlen = 0;
                    opt[0].litlen = litlen;
                    opt[0].price = stats.ll_price::<OPT_LEVEL>(litlen);
                    opt[0].rep = *rep;

                    // large match -> immediate encoding
                    let max_ml = matches[nb_matches - 1].len;
                    let max_off_base = matches[nb_matches - 1].off;
                    if max_ml > sufficient_len {
                        last_stretch.litlen = 0;
                        last_stretch.mlen = max_ml;
                        last_stretch.off = max_off_base;
                        cur = 0;
                        last_pos = max_ml;
                        break 'shortest_path;
                    }

                    // set prices for first matches starting position == 0
                    debug_assert!(opt[0].price >= 0);
                    let mut pos = 1u32;
                    while pos < min_match {
                        let o = &mut opt[pos as usize];
                        o.price = MAX_PRICE;
                        o.mlen = 0;
                        o.litlen = litlen + pos;
                        pos += 1;
                    }
                    for m in &matches[..nb_matches] {
                        let off_base = m.off;
                        let end = m.len;
                        while pos <= end {
                            let match_price = stats.match_price::<OPT_LEVEL>(off_base, pos);
                            let sequence_price = opt[0].price + match_price;
                            let o = &mut opt[pos as usize];
                            o.mlen = pos;
                            o.off = off_base;
                            o.litlen = 0; // end of match
                            o.price = sequence_price + stats.ll_price::<OPT_LEVEL>(0);
                            pos += 1;
                        }
                    }
                    last_pos = pos - 1;
                    opt[pos as usize].price = MAX_PRICE;
                }

                // check further positions
                cur = 1;
                while cur <= last_pos {
                    let inr = ip.add(cur as usize);
                    debug_assert!(cur <= OPT_NUM);
                    let c = cur as usize;

                    // Fix current position with one literal if cheaper
                    {
                        let litlen = opt[c - 1].litlen + 1;
                        s.chk(ip.add(c - 1), 1);
                        let price = opt[c - 1].price
                            + stats.lit_price::<OPT_LEVEL>(*ip.add(c - 1))
                            + stats.ll_incprice::<OPT_LEVEL>(litlen);
                        debug_assert!(price < 1_000_000_000); // overflow check
                        if price <= opt[c].price {
                            let prev_match = opt[c];
                            opt[c] = opt[c - 1];
                            opt[c].litlen = litlen;
                            opt[c].price = price;
                            if OPT_LEVEL >= 1 // additional check only for higher modes
                                && prev_match.litlen == 0 // replace a match
                                && stats.ll_incprice::<OPT_LEVEL>(1) < 0 // ll1 is cheaper than ll0
                                && ip.add(c) < iend
                            {
                                // check next position, in case it would be
                                // cheaper
                                s.chk(ip.add(c), 1);
                                let lit = stats.lit_price::<OPT_LEVEL>(*ip.add(c));
                                let with1literal =
                                    prev_match.price + lit + stats.ll_incprice::<OPT_LEVEL>(1);
                                let with_more_literals =
                                    price + lit + stats.ll_incprice::<OPT_LEVEL>(litlen + 1);
                                if with1literal < with_more_literals
                                    && with1literal < opt[c + 1].price
                                {
                                    // update offset history - before it
                                    // disappears
                                    let prev = (cur - prev_match.mlen) as usize;
                                    debug_assert!(cur >= prev_match.mlen);
                                    let new_reps = new_rep(
                                        opt[prev].rep,
                                        prev_match.off,
                                        opt[prev].litlen == 0,
                                    );
                                    opt[c + 1] = prev_match; // mlen & offbase
                                    opt[c + 1].rep = new_reps;
                                    opt[c + 1].litlen = 1;
                                    opt[c + 1].price = with1literal;
                                    if last_pos < cur + 1 {
                                        last_pos = cur + 1;
                                    }
                                }
                            }
                        }
                    }

                    // Offset history is not updated during match comparison.
                    // Do it here, now that the match is selected and
                    // confirmed.
                    debug_assert!(cur >= opt[c].mlen);
                    if opt[c].litlen == 0 {
                        // just finished a match => alter offset history
                        let prev = (cur - opt[c].mlen) as usize;
                        opt[c].rep = new_rep(opt[prev].rep, opt[c].off, opt[prev].litlen == 0);
                    }

                    // last match must start at a minimum distance of 8 from
                    // oend
                    if inr > ilimit {
                        cur += 1;
                        continue;
                    }
                    if cur == last_pos {
                        break;
                    }
                    if OPT_LEVEL == 0
                        && opt[c + 1].price <= opt[c].price + (BITCOST_MULTIPLIER / 2) as i32
                    {
                        // skip unpromising positions; about ~+6% speed,
                        // -0.01 ratio
                        cur += 1;
                        continue;
                    }

                    debug_assert!(opt[c].price >= 0);
                    {
                        let ll0 = u32::from(opt[c].litlen == 0);
                        let previous_price = opt[c].price;
                        let base_price = previous_price + stats.ll_price::<OPT_LEVEL>(0);
                        let reps = opt[c].rep;
                        #[allow(unused_mut)]
                        let mut nb_matches = get_all_matches::<MLS, EXT, DMS>(
                            &mut s,
                            matches,
                            &mut next_to_update3,
                            inr,
                            iend,
                            &reps,
                            ll0,
                            min_match,
                        ) as usize;
                        if LDM {
                            opt_ldm.process_match_candidate(
                                matches,
                                &mut nb_matches,
                                block_pos(inr),
                                block_rest(inr),
                                min_match,
                            );
                        }
                        if nb_matches == 0 {
                            cur += 1;
                            continue;
                        }

                        let longest_ml = matches[nb_matches - 1].len;
                        if longest_ml > sufficient_len
                            || cur + longest_ml >= OPT_NUM
                            || ip.add((cur + longest_ml) as usize) >= iend
                        {
                            last_stretch.mlen = longest_ml;
                            last_stretch.off = matches[nb_matches - 1].off;
                            last_stretch.litlen = 0;
                            last_pos = cur + longest_ml;
                            break 'shortest_path;
                        }

                        // set prices using matches found at position == cur
                        for match_nb in 0..nb_matches {
                            let offset = matches[match_nb].off;
                            let last_ml = matches[match_nb].len;
                            let start_ml = if match_nb > 0 {
                                matches[match_nb - 1].len + 1
                            } else {
                                min_match
                            };
                            let mut mlen = last_ml;
                            while mlen >= start_ml {
                                // scan downward
                                let pos = cur + mlen;
                                let price =
                                    base_price + stats.match_price::<OPT_LEVEL>(offset, mlen);
                                if pos > last_pos || price < opt[pos as usize].price {
                                    while last_pos < pos {
                                        // fill empty positions, for future
                                        // comparisons
                                        last_pos += 1;
                                        opt[last_pos as usize].price = MAX_PRICE;
                                        // != 0: "not an end of match"
                                        opt[last_pos as usize].litlen = 1;
                                    }
                                    let o = &mut opt[pos as usize];
                                    o.mlen = mlen;
                                    o.off = offset;
                                    o.litlen = 0;
                                    o.price = price;
                                } else if OPT_LEVEL == 0 {
                                    // early update abort; gets ~+10% speed
                                    // for about -0.01 ratio loss
                                    break;
                                }
                                mlen -= 1;
                            }
                        }
                    }
                    opt[last_pos as usize + 1].price = MAX_PRICE;
                    cur += 1;
                }

                last_stretch = opt[last_pos as usize];
                debug_assert!(last_pos >= last_stretch.mlen);
                cur = last_pos - last_stretch.mlen;
            }

            // _shortestPath: cur, last_pos and last_stretch are set
            debug_assert!(opt[0].mlen == 0);
            debug_assert!(last_pos >= last_stretch.mlen);
            debug_assert!(cur == last_pos - last_stretch.mlen);

            if last_stretch.mlen == 0 {
                // no solution: all matches have been converted into
                // literals
                debug_assert!(
                    last_stretch.litlen as usize
                        == (ip as usize - anchor as usize) + last_pos as usize
                );
                ip = ip.add(last_pos as usize);
                continue 'main;
            }
            debug_assert!(last_stretch.off > 0);

            // Update offset history
            if last_stretch.litlen == 0 {
                // finishing on a match: update offset history
                let c = cur as usize;
                *rep = new_rep(opt[c].rep, last_stretch.off, opt[c].litlen == 0);
            } else {
                *rep = last_stretch.rep;
                debug_assert!(cur >= last_stretch.litlen);
                cur -= last_stretch.litlen;
            }

            // Write the shortest path solution. It is stored in `opt` in
            // reverse order, starting from store_end (== cur + 2),
            // partially overwriting `opt`; it changes from stretches (a
            // match followed by literals) to sequences (literals followed
            // by a match).
            {
                let store_end = cur as usize + 2;
                let mut store_start;
                let mut stretch_pos = cur as usize;
                debug_assert!(store_end < OPT_SIZE);
                if last_stretch.litlen > 0 {
                    // last "sequence" is unfinished: just a bunch of literals
                    opt[store_end].litlen = last_stretch.litlen;
                    opt[store_end].mlen = 0;
                    store_start = store_end - 1;
                    opt[store_start] = last_stretch;
                }
                {
                    opt[store_end] = last_stretch; // note: litlen will be fixed
                    store_start = store_end;
                }
                loop {
                    let next_stretch = opt[stretch_pos];
                    opt[store_start].litlen = next_stretch.litlen;
                    if next_stretch.mlen == 0 {
                        // reaching beginning of segment
                        break;
                    }
                    store_start -= 1;
                    opt[store_start] = next_stretch; // note: litlen will be fixed
                    debug_assert!(
                        (next_stretch.litlen + next_stretch.mlen) as usize <= stretch_pos
                    );
                    stretch_pos -= (next_stretch.litlen + next_stretch.mlen) as usize;
                }

                // save sequences
                for (k, o) in opt[store_start..=store_end].iter().enumerate() {
                    let llen = o.litlen as usize;
                    let mlen = o.mlen;
                    let off_base = o.off;
                    let advance = llen + mlen as usize;
                    if mlen == 0 {
                        // only literals => must be the last "sequence",
                        // actually starting a new stream of sequences
                        debug_assert!(store_start + k == store_end);
                        // last "sequence" is a bunch of literals => don't
                        // progress anchor
                        ip = anchor.add(llen);
                        continue; // will finish
                    }
                    let a = anchor as usize - bp as usize;
                    debug_assert!(a + llen <= end);
                    stats.update_stats(&buf[a..a + llen], off_base, mlen);
                    ss.store_seq(buf, a, llen, end, off_base, mlen as usize);
                    anchor = anchor.add(advance);
                    ip = anchor;
                }
            }
            // update all costs
            stats.set_base_prices::<OPT_LEVEL>();
        }
    }

    ms.next_to_update = s.next_to_update;
    ms.opt = Some(st);

    // Return the last literals size
    iend as usize - anchor as usize
}

#[cfg(test)]
mod tests {
    use super::super::fast::tests::{random, roundtrip, roundtrip_ring, texty};
    use super::super::params::{CParams, Strategy, block_size_max, one_shot_cparams};
    use super::*;

    const OPT: [Strategy; 3] = [Strategy::BtOpt, Strategy::BtUltra, Strategy::BtUltra2];

    fn inputs() -> [Vec<u8>; 6] {
        [
            texty(1000, 1),
            texty(300_000, 3),
            random(50_000, 4),
            alloc::vec![0u8; 200_000],
            (0..70_000u32).map(|i| (i % 251) as u8).collect(),
            {
                let mut v = random(20_000, 5);
                v.extend_from_slice(&texty(30_000, 6));
                v.extend_from_within(..25_000);
                v
            },
        ]
    }

    #[test]
    fn roundtrip_levels() {
        for src in &inputs() {
            for level in 11..=22 {
                let cp = one_shot_cparams(level, src.len() as u64);
                if !OPT.contains(&cp.strategy) {
                    continue;
                }
                roundtrip(src, cp, block_size_max(&cp, src.len() as u64));
            }
        }
    }

    #[test]
    fn roundtrip_all_variants() {
        let mut src = texty(150_000, 7);
        src.extend_from_slice(&random(5000, 8));
        src.extend_from_slice(&src.clone()[..40_000]);
        for strategy in OPT {
            // mls 3, 4, 5, 6 (7 is clamped to 6)
            for mls in 3..=7 {
                for (search_log, target_length) in [(1, 4), (4, 32), (7, 256), (9, 999)] {
                    for window_log in [10, 17, 22] {
                        let cp = CParams {
                            window_log,
                            chain_log: 16,
                            hash_log: 15,
                            search_log,
                            min_match: mls,
                            target_length,
                            strategy,
                        };
                        let n = roundtrip(&src, cp, (1 << 17).min(1 << window_log));
                        assert!(
                            n > 1000,
                            "{strategy:?} mls {mls} slog {search_log} wlog {window_log}: {n} sequences"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn small_blocks() {
        let src = texty(5000, 9);
        for level in [16, 18, 19, 22] {
            let cp = one_shot_cparams(level, src.len() as u64);
            for bs in [1, 6, 7, 8, 9, 16, 17, 20, 33, 100, 1000] {
                roundtrip(&src, cp, bs);
            }
        }
        for n in 0..60 {
            let s = texty(n, 10);
            for level in [12, 16, 19, 22] {
                roundtrip(&s, one_shot_cparams(level, n as u64), 1 << 17);
            }
        }
    }

    #[test]
    fn long_matches() {
        // Matches longer than ZSTD_OPT_NUM and than a block, and repetitive
        // patterns (tree insertion skipping).
        let mut src = random(10_000, 11);
        src.extend_from_within(..10_000);
        src.extend(core::iter::repeat_n(b'a', 20_000));
        src.extend_from_slice(&texty(10_000, 12));
        src.extend_from_within(5_000..25_000);
        src.extend((0..200_000u32).map(|i| (i % 3) as u8));
        for level in [16, 17, 18, 19, 22] {
            let cp = one_shot_cparams(level, src.len() as u64);
            roundtrip(&src, cp, block_size_max(&cp, src.len() as u64));
        }
    }

    #[test]
    fn weights() {
        assert_eq!(bit_weight(0), 0);
        assert_eq!(bit_weight(1), 256);
        assert_eq!(bit_weight(3), 512);
        assert_eq!(frac_weight(0), 256);
        assert_eq!(frac_weight(1), 512);
        assert_eq!(frac_weight(2), 256 + 384);
        // monotonic
        let mut prev = 0;
        for s in 0..100_000 {
            let w = frac_weight(s);
            assert!(w >= prev);
            prev = w;
        }
    }

    #[test]
    fn scaling() {
        let mut t = [0u32, 1, 2, 3, 1000, 5000];
        assert_eq!(scale_stats(&mut t, 12), 6006);
        let sum = scale_stats(&mut t, 10);
        // factor 6006 >> 10 = 5: shift 2, every symbol at least 1
        assert_eq!(t, [1, 1, 1, 1, 251, 1251]);
        assert_eq!(sum, 1506);
        let mut t = [0u32, 1, 4, 255, 256];
        assert_eq!(downscale_stats(&mut t, 8, false), 5);
        assert_eq!(t, [0, 1, 1, 1, 2]);
    }

    /// The extDict variants, through a wrapping ring buffer like the
    /// streaming compressor's.
    #[test]
    fn roundtrip_ring_variants() {
        let mut src = texty(100_000, 7);
        src.extend_from_slice(&random(5000, 8));
        src.extend_from_slice(&src.clone()[..40_000]);
        src.extend_from_slice(&texty(50_000, 17));
        for strategy in OPT {
            for mls in 3..=6 {
                for (search_log, target_length) in [(1, 4), (4, 32), (7, 256)] {
                    for (window_log, bs) in [(12, 4096), (15, 5000), (16, 1 << 15)] {
                        let cp = CParams {
                            window_log,
                            chain_log: 16,
                            hash_log: 15,
                            search_log,
                            min_match: mls,
                            target_length,
                            strategy,
                        };
                        let (n, ext) = roundtrip_ring(&src, cp, bs);
                        assert!(
                            n > 1000 && ext > 0,
                            "{strategy:?} mls {mls} slog {search_log} wlog {window_log}: {n} {ext}"
                        );
                    }
                }
            }
        }
    }
}
