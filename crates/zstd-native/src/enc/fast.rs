//! The `fast` strategy (levels 1-2 and negative levels): a port of libzstd
//! 1.5.7's `compress/zstd_fast.c` (no-dictionary paths), plus the parts of
//! `ZSTD_MatchState_t` / `ZSTD_window_t` (`compress/zstd_compress_internal.h`)
//! and of `zstd_compress.c`'s block loop that drive it.
//!
//! # Positions and indices
//!
//! libzstd indexes everything relative to `window.base`, a pointer such
//! that `base + index` is the byte at that index. Here the caller owns one
//! window buffer (for one-shot compression: the whole input) and all
//! "pointers" are positions in that buffer, as `usize`. `Window::base` is
//! the (wrapping) position of index 0, so the byte at index `i` is
//! `buf[base.wrapping_add(i)]` and the index of position `p` is
//! `p.wrapping_sub(base) as u32`. As in libzstd, the first byte of a fresh
//! window gets index `ZSTD_WINDOW_START_INDEX` (2), i.e. `base` is
//! "position -2".
//!
//! # Driving it (libzstd's one-shot path, `ZSTD_compress2`/`ZSTD_compress`)
//!
//! 1. `ZSTD_resetCCtx_internal` -> `ZSTD_reset_matchState` with
//!    `ZSTDirp_reset` / `ZSTDcrp_makeClean`: [`MatchState::new`] or
//!    [`MatchState::reset`] (fresh window, zeroed hash table of
//!    `1 << hashLog` entries, `nextToUpdate = dictLimit`).
//! 2. `ZSTD_compressContinue_internal`: [`MatchState::update_window`] once
//!    with the whole input (`ZSTD_window_update`; on a non-contiguous
//!    update `nextToUpdate = dictLimit`).
//! 3. `ZSTD_compress_frameChunk`, for each block `[start, end)`:
//!    [`MatchState::prepare_block`] (`ZSTD_overflowCorrectIfNeeded`,
//!    `ZSTD_checkDictValidity`, `ZSTD_window_enforceMaxDist` with the block
//!    *start*, `nextToUpdate` clamp), then [`build_seq_store`]
//!    (`ZSTD_buildSeqStore`: skips blocks under 7 bytes, resets the seq
//!    store, runs [`compress_block_fast`] with `rep` = the previous block's
//!    repcodes, stores the last literals).

use super::params::{
    CParams, ParamSwitch, Strategy, cycle_log, resolve_row_match_finder_mode, row_match_finder_used,
};
use super::seq_store::{
    REPCODE1_TO_OFFBASE, SeqStore, count, hash4_ptr, hash5_ptr, hash6_ptr, hash7_ptr, hash8_ptr,
    offset_to_offbase, read32,
};
use alloc::vec;
use alloc::vec::Vec;

/// `ZSTD_WINDOW_START_INDEX`: the index of a fresh window's first byte.
pub(crate) const WINDOW_START_INDEX: u32 = 2;
/// `HASH_READ_SIZE`: the match finders read up to 8 bytes per position.
pub(crate) const HASH_READ_SIZE: usize = 8;
/// `kSearchStrength`.
pub(crate) const K_SEARCH_STRENGTH: u32 = 8;
/// `ZSTD_CURRENT_MAX`: indices above this trigger overflow correction.
pub(crate) const CURRENT_MAX: u32 = if usize::BITS == 64 {
    3500 << 20
} else {
    2000 << 20
};
/// `ZSTD_CHUNKSIZE_MAX`.
pub(crate) const CHUNKSIZE_MAX: u32 = u32::MAX - CURRENT_MAX;
/// `MIN_CBLOCK_SIZE + ZSTD_blockHeaderSize + 1 + 1`: `ZSTD_buildSeqStore`
/// doesn't try to compress smaller blocks.
pub(crate) const MIN_SEQ_STORE_BLOCK: usize = 2 + 3 + 1 + 1;

/// A position in no buffer: the initial `base` (libzstd's `" "` string)
/// such that `base + 2` is never a real buffer position, so the first
/// `update_window` is non-contiguous, as in libzstd.
const INIT_BASE: usize = usize::MAX - 15;

/// `ZSTD_window_t`, with positions in the caller's buffer instead of
/// pointers (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    /// `nextSrc`: the position after the last update.
    pub(crate) next_src: usize,
    /// `base`: the position of index 0 (wrapping).
    pub(crate) base: usize,
    /// `dictBase`: the extDict segment's base (wrapping).
    pub(crate) dict_base: usize,
    /// `dictLimit`: below it, the data is in the extDict segment.
    pub(crate) dict_limit: u32,
    /// `lowLimit`: below it, no valid data.
    pub(crate) low_limit: u32,
    /// `nbOverflowCorrections`.
    pub(crate) nb_overflow_corrections: u32,
}

impl Window {
    /// `ZSTD_window_init()`.
    pub(crate) fn new() -> Self {
        Self {
            next_src: INIT_BASE + WINDOW_START_INDEX as usize,
            base: INIT_BASE,
            dict_base: INIT_BASE,
            dict_limit: WINDOW_START_INDEX,
            low_limit: WINDOW_START_INDEX,
            nb_overflow_corrections: 0,
        }
    }

    /// The index of buffer position `pos`.
    #[inline(always)]
    pub(crate) fn index(&self, pos: usize) -> u32 {
        pos.wrapping_sub(self.base) as u32
    }

    /// The buffer position of index `idx`.
    #[inline(always)]
    pub(crate) fn pos(&self, idx: u32) -> usize {
        self.base.wrapping_add(idx as usize)
    }

    /// `ZSTD_window_clear()`.
    pub(crate) fn clear(&mut self) {
        let end = self.next_src.wrapping_sub(self.base) as u32;
        self.low_limit = end;
        self.dict_limit = end;
    }

    /// `ZSTD_window_isEmpty()`.
    pub(crate) fn is_empty(&self) -> bool {
        self.dict_limit == WINDOW_START_INDEX
            && self.low_limit == WINDOW_START_INDEX
            && self.next_src.wrapping_sub(self.base) == WINDOW_START_INDEX as usize
    }

    /// `ZSTD_window_hasExtDict()`.
    pub(crate) fn has_ext_dict(&self) -> bool {
        self.low_limit < self.dict_limit
    }

    /// `ZSTD_window_update()`: appends `[src, src + src_size)`. Returns
    /// whether the segment was contiguous with the previous one.
    pub(crate) fn update(
        &mut self,
        src: usize,
        src_size: usize,
        force_non_contiguous: bool,
    ) -> bool {
        let mut contiguous = true;
        if src_size == 0 {
            return contiguous;
        }
        if src != self.next_src || force_non_contiguous {
            // not contiguous
            let distance_from_base = self.next_src.wrapping_sub(self.base);
            self.low_limit = self.dict_limit;
            debug_assert!(distance_from_base == distance_from_base as u32 as usize);
            self.dict_limit = distance_from_base as u32;
            self.dict_base = self.base;
            self.base = src.wrapping_sub(distance_from_base);
            if self.dict_limit - self.low_limit < HASH_READ_SIZE as u32 {
                self.low_limit = self.dict_limit; // too small extDict
            }
            contiguous = false;
        }
        self.next_src = src.wrapping_add(src_size);
        // if input and dictionary overlap: reduce dictionary (area presumed
        // modified by input)
        let src_end = src.wrapping_add(src_size);
        if (src_end > self.dict_base.wrapping_add(self.low_limit as usize))
            & (src < self.dict_base.wrapping_add(self.dict_limit as usize))
        {
            let high_input_idx = src_end.wrapping_sub(self.dict_base);
            let low_limit_max = if high_input_idx > self.dict_limit as usize {
                self.dict_limit
            } else {
                high_input_idx as u32
            };
            self.low_limit = low_limit_max;
        }
        contiguous
    }

    /// `ZSTD_window_needOverflowCorrection()` (without the fuzzing-only
    /// `ZSTD_WINDOW_OVERFLOW_CORRECT_FREQUENTLY`).
    pub(crate) fn need_overflow_correction(&self, src_end: usize) -> bool {
        let curr = src_end.wrapping_sub(self.base) as u32;
        curr > CURRENT_MAX
    }

    /// `ZSTD_window_correctOverflow()`: moves `base` forward so that `src`
    /// gets a small index; returns the correction to subtract from every
    /// stored index.
    pub(crate) fn correct_overflow(&mut self, cycle_log: u32, max_dist: u32, src: usize) -> u32 {
        let cycle_size = 1u32 << cycle_log;
        let cycle_mask = cycle_size - 1;
        let curr = src.wrapping_sub(self.base) as u32;
        let current_cycle = curr & cycle_mask;
        // Ensure newCurrent - maxDist >= ZSTD_WINDOW_START_INDEX.
        let current_cycle_correction = if current_cycle < WINDOW_START_INDEX {
            cycle_size.max(WINDOW_START_INDEX)
        } else {
            0
        };
        let new_current = current_cycle + current_cycle_correction + max_dist.max(cycle_size);
        let correction = curr - new_current;
        debug_assert!(max_dist & max_dist.wrapping_sub(1) == 0);
        debug_assert!(curr & cycle_mask == new_current & cycle_mask);
        debug_assert!(curr > new_current);
        debug_assert!(correction > 1 << 28);

        self.base = self.base.wrapping_add(correction as usize);
        self.dict_base = self.dict_base.wrapping_add(correction as usize);
        if self.low_limit < correction + WINDOW_START_INDEX {
            self.low_limit = WINDOW_START_INDEX;
        } else {
            self.low_limit -= correction;
        }
        if self.dict_limit < correction + WINDOW_START_INDEX {
            self.dict_limit = WINDOW_START_INDEX;
        } else {
            self.dict_limit -= correction;
        }
        debug_assert!(new_current >= max_dist);
        debug_assert!(new_current - max_dist >= WINDOW_START_INDEX);
        debug_assert!(self.low_limit <= new_current);
        debug_assert!(self.dict_limit <= new_current);
        self.nb_overflow_corrections += 1;
        correction
    }

    /// `ZSTD_window_enforceMaxDist()`: raises `lowLimit` (and `dictLimit`)
    /// so that indices stay within `max_dist` of `block_end`. Invalidates
    /// the dictionary (`*loaded_dict_end = 0`) when the window is reached.
    pub(crate) fn enforce_max_dist(
        &mut self,
        block_end: usize,
        max_dist: u32,
        loaded_dict_end: &mut u32,
    ) {
        let block_end_idx = block_end.wrapping_sub(self.base) as u32;
        if block_end_idx > max_dist.wrapping_add(*loaded_dict_end) {
            let new_low_limit = block_end_idx - max_dist;
            if self.low_limit < new_low_limit {
                self.low_limit = new_low_limit;
            }
            if self.dict_limit < self.low_limit {
                self.dict_limit = self.low_limit;
            }
            *loaded_dict_end = 0;
        }
    }

    /// `ZSTD_checkDictValidity()` (without a dictMatchState, which this
    /// port doesn't support yet).
    pub(crate) fn check_dict_validity(
        &self,
        block_end: usize,
        max_dist: u32,
        loaded_dict_end: &mut u32,
    ) {
        let block_end_idx = block_end.wrapping_sub(self.base) as u32;
        let lde = *loaded_dict_end;
        debug_assert!(block_end_idx >= lde);
        if block_end_idx > lde.wrapping_add(max_dist) || lde != self.dict_limit {
            *loaded_dict_end = 0;
        }
    }
}

impl Default for Window {
    fn default() -> Self {
        Self::new()
    }
}

/// `ZSTD_reduceTable()`: subtracts `reducer_value` from every index,
/// zeroing the ones that would drop below `ZSTD_WINDOW_START_INDEX`.
pub(crate) fn reduce_table(table: &mut [u32], reducer_value: u32) {
    let reducer_threshold = reducer_value + WINDOW_START_INDEX;
    for v in table {
        *v = if *v < reducer_threshold {
            0
        } else {
            *v - reducer_value
        };
    }
}

/// `ZSTD_reduceTable_btlazy2()`: as [`reduce_table`], but keeps
/// `ZSTD_DUBT_UNSORTED_MARK`.
pub(crate) fn reduce_table_btlazy2(table: &mut [u32], reducer_value: u32) {
    use super::lazy::DUBT_UNSORTED_MARK;
    let reducer_threshold = reducer_value + WINDOW_START_INDEX;
    for v in table {
        *v = if *v == DUBT_UNSORTED_MARK {
            DUBT_UNSORTED_MARK
        } else if *v < reducer_threshold {
            0
        } else {
            *v - reducer_value
        };
    }
}

/// `ZSTD_bitmix()`: mixes the bits of a 64-bit value (from XXH3's rrmxmx).
fn bitmix(mut val: u64, len: u64) -> u64 {
    const K: u64 = 0x9FB2_1C65_1E98_DF25;
    val ^= val.rotate_right(49) ^ val.rotate_right(24);
    val = val.wrapping_mul(K);
    val ^= (val >> 35).wrapping_add(len);
    val = val.wrapping_mul(K);
    val ^ (val >> 28)
}

/// `ZSTD_dictTableLoadMethod_e`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TableLoadMethod {
    /// `ZSTD_dtlm_fast`: every third position.
    Fast,
    /// `ZSTD_dtlm_full`: also the positions in between, if their slot is
    /// empty.
    Full,
}

/// `ZSTD_MatchState_t` without the dictionary and optimal-parser parts.
#[derive(Debug)]
pub(crate) struct MatchState {
    /// `window`
    pub(crate) window: Window,
    /// `loadedDictEnd`: 0 without a dictionary.
    pub(crate) loaded_dict_end: u32,
    /// `nextToUpdate`
    pub(crate) next_to_update: u32,
    /// `hashTable`: `1 << cparams.hash_log` entries.
    pub(crate) hash_table: Vec<u32>,
    /// `chainTable`: `1 << cparams.chain_log` entries, none for `fast` and
    /// the row match finder. `dfast` uses it as its short-match hash table,
    /// `btlazy2` as its binary tree.
    pub(crate) chain_table: Vec<u32>,
    /// `cParams`
    pub(crate) cparams: CParams,
    /// `forceNonContiguous`
    pub(crate) force_non_contiguous: bool,
    /// Whether the row-based match finder is used (libzstd's resolved
    /// `useRowMatchFinder`: greedy to lazy2 with `windowLog > 14`).
    pub(crate) use_row_match_finder: bool,
    /// `tagTable`: the row match finder's tags, `1 << hash_log` bytes when
    /// it is used. libzstd allocates it "init once": it is not cleared
    /// between frames of a reused context, only salted differently. Its
    /// stale content never changes the result (see `lazy::row`), so it is
    /// not cleared here either.
    pub(crate) tag_table: Vec<u8>,
    /// `hashCache`: the row hashes of the next positions.
    pub(crate) hash_cache: [u32; super::row::ROW_HASH_CACHE_SIZE],
    /// `rowHashLog`: `hash_log - rowLog`.
    pub(crate) row_hash_log: u32,
    /// `hashSalt`: salts the row hashes; advanced on every reset that uses
    /// the row match finder.
    pub(crate) hash_salt: u64,
    /// `hashSaltEntropy`: sum of the row hashes searched, for the next salt.
    pub(crate) hash_salt_entropy: u32,
    /// `lazySkipping`
    pub(crate) lazy_skipping: bool,
}

impl MatchState {
    /// A fresh match state (`ZSTD_reset_matchState` with `ZSTDirp_reset`
    /// and `ZSTDcrp_makeClean`).
    pub(crate) fn new(cparams: CParams) -> Self {
        let mut ms = Self {
            window: Window::new(),
            loaded_dict_end: 0,
            next_to_update: 0,
            hash_table: Vec::new(),
            chain_table: Vec::new(),
            cparams,
            force_non_contiguous: false,
            use_row_match_finder: false,
            tag_table: Vec::new(),
            hash_cache: [0; super::row::ROW_HASH_CACHE_SIZE],
            row_hash_log: 0,
            hash_salt: 0,
            hash_salt_entropy: 0,
            lazy_skipping: false,
        };
        ms.reset(cparams);
        ms
    }

    /// `ZSTD_reset_matchState()` with `ZSTDirp_reset` (indices restart at
    /// `ZSTD_WINDOW_START_INDEX`) and `ZSTDcrp_makeClean` (the hash table is
    /// zeroed), for the CCtx. libzstd's `ZSTDirp_continue` (reusing a
    /// context) only shifts the indices; the match finders only compare
    /// indices with each other (the tables are cleared), so it produces the
    /// same sequences and this port always resets. With the row match
    /// finder, the hash salt advances (`ZSTD_advanceHashSalt`).
    pub(crate) fn reset(&mut self, cparams: CParams) {
        self.window = Window::new();
        self.lazy_skipping = false;
        self.invalidate();
        let h_size = 1usize << cparams.hash_log;
        clear_table(&mut self.hash_table, h_size);
        let use_row = row_match_finder_used(
            cparams.strategy,
            resolve_row_match_finder_mode(ParamSwitch::Auto, &cparams),
        );
        // ZSTD_allocateChainTable: none for `fast` and the row match finder.
        let c_size = if cparams.strategy == Strategy::Fast || use_row {
            0
        } else {
            1usize << cparams.chain_log
        };
        clear_table(&mut self.chain_table, c_size);
        self.use_row_match_finder = use_row;
        if use_row {
            // The tag table is "init once" (see its docs): new bytes are
            // zeroed, existing ones are kept.
            self.tag_table.resize(h_size, 0);
            self.advance_hash_salt();
            let row_log = cparams.search_log.clamp(4, 6);
            debug_assert!(cparams.hash_log >= row_log);
            self.row_hash_log = cparams.hash_log - row_log;
        }
        self.cparams = cparams;
        self.force_non_contiguous = false;
    }

    /// `ZSTD_advanceHashSalt()`: mixes the collected entropy into the salt.
    fn advance_hash_salt(&mut self) {
        self.hash_salt = bitmix(self.hash_salt, 8) ^ bitmix(u64::from(self.hash_salt_entropy), 4);
    }

    /// Forgets the hash salt history, as for a newly created libzstd
    /// context (`hashSalt` and `hashSaltEntropy` start at 0).
    pub(crate) fn clear_hash_salt(&mut self) {
        self.hash_salt = 0;
        self.hash_salt_entropy = 0;
    }

    /// `ZSTD_invalidateMatchState()`.
    pub(crate) fn invalidate(&mut self) {
        self.window.clear();
        self.next_to_update = self.window.dict_limit;
        self.loaded_dict_end = 0;
    }

    /// The window-update step of `ZSTD_compressContinue_internal`: appends
    /// `[src, src + src_size)` of the caller's buffer to the window.
    pub(crate) fn update_window(&mut self, src: usize, src_size: usize) {
        if !self.window.update(src, src_size, self.force_non_contiguous) {
            self.force_non_contiguous = false;
            self.next_to_update = self.window.dict_limit;
        }
    }

    /// `ZSTD_getLowestMatchIndex()`.
    #[inline(always)]
    pub(crate) fn lowest_match_index(&self, curr: u32, window_log: u32) -> u32 {
        let max_distance = 1u32 << window_log;
        let lowest_valid = self.window.low_limit;
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

    /// `ZSTD_getLowestPrefixIndex()`.
    #[inline(always)]
    pub(crate) fn lowest_prefix_index(&self, curr: u32, window_log: u32) -> u32 {
        let max_distance = 1u32 << window_log;
        let lowest_valid = self.window.dict_limit;
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

    /// `ZSTD_overflowCorrectIfNeeded()`: for the block `[ip, iend)`.
    /// Reachable for inputs over `ZSTD_CURRENT_MAX` (3500 MiB on 64-bit).
    pub(crate) fn overflow_correct_if_needed(&mut self, ip: usize, iend: usize) {
        let cycle_log = cycle_log(self.cparams.chain_log, self.cparams.strategy);
        let max_dist = 1u32 << self.cparams.window_log;
        if self.window.need_overflow_correction(iend) {
            let correction = self.window.correct_overflow(cycle_log, max_dist, ip);
            // ZSTD_reduceIndex (the chain table is empty when not
            // allocated; the row match finder's tags hold no indices).
            reduce_table(&mut self.hash_table, correction);
            if self.cparams.strategy == Strategy::BtLazy2 {
                reduce_table_btlazy2(&mut self.chain_table, correction);
            } else {
                reduce_table(&mut self.chain_table, correction);
            }
            self.next_to_update = self.next_to_update.saturating_sub(correction);
            self.loaded_dict_end = 0;
        }
    }

    /// The per-block preamble of `ZSTD_compress_frameChunk` for the block
    /// `[start, start + block_size)`: overflow correction,
    /// `ZSTD_checkDictValidity` (with the block end),
    /// `ZSTD_window_enforceMaxDist` (with the block *start*, as libzstd
    /// does) and the `nextToUpdate >= lowLimit` clamp.
    pub(crate) fn prepare_block(&mut self, start: usize, block_size: usize) {
        let max_dist = 1u32 << self.cparams.window_log;
        let end = start + block_size;
        self.overflow_correct_if_needed(start, end);
        self.window
            .check_dict_validity(end, max_dist, &mut self.loaded_dict_end);
        self.window
            .enforce_max_dist(start, max_dist, &mut self.loaded_dict_end);
        if self.next_to_update < self.window.low_limit {
            self.next_to_update = self.window.low_limit;
        }
    }

    /// `ZSTD_fillHashTable()` for a CCtx (`ZSTD_fillHashTableForCCtx`, the
    /// no-dictionary table layout): inserts positions from `nextToUpdate`
    /// up to `end - HASH_READ_SIZE` of `buf` (used to load a dictionary's
    /// content). libzstd always uses [`TableLoadMethod::Fast`] here.
    pub(crate) fn fill_hash_table(&mut self, buf: &[u8], end: usize, dtlm: TableLoadMethod) {
        const FAST_HASH_FILL_STEP: usize = 3;
        let h_bits = self.cparams.hash_log;
        let mls = self.cparams.min_match;
        assert!(end <= buf.len());
        let mut ip = self.window.pos(self.next_to_update);
        // `ip + 3 < iend + 2` with `iend = end - HASH_READ_SIZE`.
        while ip + FAST_HASH_FILL_STEP + HASH_READ_SIZE < end + 2 {
            let curr = self.window.index(ip);
            let hash0 = hash_at_buf(buf, ip, h_bits, mls);
            self.hash_table[hash0] = curr;
            if dtlm == TableLoadMethod::Full {
                for p in 1..FAST_HASH_FILL_STEP {
                    let hash = hash_at_buf(buf, ip + p, h_bits, mls);
                    if self.hash_table[hash] == 0 {
                        self.hash_table[hash] = curr + p as u32;
                    }
                }
            }
            ip += FAST_HASH_FILL_STEP;
        }
    }
}

/// Resizes `table` to `size` zeroed entries, reusing its allocation.
fn clear_table(table: &mut Vec<u32>, size: usize) {
    if table.len() == size {
        table.fill(0);
    } else {
        *table = vec![0; size];
    }
}

/// Safe `ZSTD_hashPtr` on a buffer position (reads `HASH_READ_SIZE` bytes).
fn hash_at_buf(buf: &[u8], pos: usize, h_bits: u32, mls: u32) -> usize {
    super::seq_store::hash_at(buf, pos, h_bits, mls)
}

/// `ZSTD_buildSeqStore()` for the `fast` strategy without a dictionary,
/// LDM or an external sequence producer: fills `ss` with the sequences of
/// the block `buf[start..end]` and its last literals. `rep` must hold the
/// previous block's repcodes on entry (libzstd copies `prevCBlock->rep` to
/// `nextCBlock->rep`) and holds this block's on return. Returns `false`
/// (`ZSTDbss_noCompress`) for blocks under 7 bytes, which libzstd stores
/// raw without running the match finder.
pub(crate) fn build_seq_store(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> bool {
    debug_assert!(end - start <= super::params::BLOCKSIZE_MAX);
    if end - start < MIN_SEQ_STORE_BLOCK {
        return false;
    }
    ss.reset();
    // limited update after a very long match
    let curr = ms.window.index(start);
    if curr > ms.next_to_update + 384 {
        ms.next_to_update = curr - 192u32.min(curr - ms.next_to_update - 384);
    }
    let last_ll_size = match ms.cparams.strategy {
        Strategy::Fast => compress_block_fast(ms, ss, rep, buf, start, end),
        Strategy::DFast => super::dfast::compress_block_double_fast(ms, ss, rep, buf, start, end),
        Strategy::Greedy | Strategy::Lazy | Strategy::Lazy2 | Strategy::BtLazy2 => {
            super::lazy::compress_block_lazy(ms, ss, rep, buf, start, end)
        }
        Strategy::BtOpt | Strategy::BtUltra | Strategy::BtUltra2 => {
            unimplemented!("strategy {:?} (issue #9)", ms.cparams.strategy)
        }
    };
    ss.store_last_literals(&buf[end - last_ll_size..end]);
    true
}

/// `ZSTD_compressBlock_fast()`: finds the sequences of the block
/// `buf[start..end]` (no dictionary). The window must have been updated
/// to cover the block ([`MatchState::update_window`]), and every index from
/// `window.dict_limit` up must be a position in `buf`. Picks the
/// `ZSTD_compressBlock_fast_noDict_<mls>_<cmov>` variant like libzstd:
/// `mls = minMatch` (3 and other values use 4), `useCmov = windowLog < 19`
/// (the two variants find the same matches; only the code differs).
/// Updates `rep` and returns the number of trailing literals.
pub(crate) fn compress_block_fast(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    debug_assert!(ms.cparams.strategy == Strategy::Fast);
    let use_cmov = ms.cparams.window_log < 19;
    match (ms.cparams.min_match, use_cmov) {
        (5, true) => no_dict_generic::<5, true>(ms, ss, rep, buf, start, end),
        (6, true) => no_dict_generic::<6, true>(ms, ss, rep, buf, start, end),
        (7, true) => no_dict_generic::<7, true>(ms, ss, rep, buf, start, end),
        (_, true) => no_dict_generic::<4, true>(ms, ss, rep, buf, start, end),
        (5, false) => no_dict_generic::<5, false>(ms, ss, rep, buf, start, end),
        (6, false) => no_dict_generic::<6, false>(ms, ss, rep, buf, start, end),
        (7, false) => no_dict_generic::<7, false>(ms, ss, rep, buf, start, end),
        (_, false) => no_dict_generic::<4, false>(ms, ss, rep, buf, start, end),
    }
}

/// `ZSTD_hashPtr` with a compile-time `mls`.
///
/// # Safety
/// `p..p + 8` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn hash_mls<const MLS: u32>(p: *const u8, h_bits: u32) -> usize {
    // SAFETY: per the caller.
    unsafe {
        match MLS {
            5 => hash5_ptr(p, h_bits),
            6 => hash6_ptr(p, h_bits),
            7 => hash7_ptr(p, h_bits),
            8 => hash8_ptr(p, h_bits),
            _ => hash4_ptr(p, h_bits),
        }
    }
}

/// `PREFETCH_L1`.
#[inline(always)]
pub(crate) fn prefetch_l1(p: *const u8) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: prefetching never faults, whatever the address.
    #[allow(unsafe_code, unused_unsafe)]
    unsafe {
        core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(p.cast::<i8>());
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = p;
}

/// `ZSTD_compressBlock_fast_noDict_generic()`; see the libzstd source for
/// the pipelining scheme (hash, table lookup, match read and compare
/// interleaved over positions ip0..ip3).
///
/// Memory safety: every read is of a position in
/// `[base + prefixLowest, iend)`: the current positions stay below
/// `ilimit = iend - HASH_READ_SIZE` before an 8-byte hash read; candidate
/// matches are read only when `matchIdx >= prefixStartIndex`
/// (`>= dictLimit`); repcodes are checked against the window at entry
/// (`maxRep`) and are later real offsets of earlier matches, so
/// `ip - rep` stays at or above the window start (debug-asserted).
#[allow(unsafe_code)]
#[inline(always)]
fn no_dict_generic<const MLS: u32, const USE_CMOV: bool>(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    static DUMMY: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
    const K_STEP_INCR: usize = 1 << (K_SEARCH_STRENGTH - 1);

    let cparams = ms.cparams;
    let hlog = cparams.hash_log;
    // These checks make the raw-pointer accesses below sound.
    assert!(start <= end && end <= buf.len());
    assert!(ms.hash_table.len() == 1usize << hlog && hlog <= 32);
    assert!(!ms.window.has_ext_dict());
    let window_start = ms.window.pos(ms.window.dict_limit);
    assert!(window_start <= start, "window start is not in the buffer");
    assert!(end.wrapping_sub(ms.window.base) <= u32::MAX as usize);

    let step_size = cparams.target_length as usize + (cparams.target_length == 0) as usize + 1;
    let bp = buf.as_ptr();
    let base = bp.wrapping_add(ms.window.base);
    let istart = bp.wrapping_add(start);
    let end_index = ms.window.index(end);
    let prefix_start_index = ms.lowest_prefix_index(end_index, cparams.window_log);
    let prefix_start = base.wrapping_add(prefix_start_index as usize);
    let iend = bp.wrapping_add(end);
    let ilimit = iend.wrapping_sub(HASH_READ_SIZE);
    let lowest = bp.wrapping_add(window_start);
    let ht = ms.hash_table.as_mut_ptr();
    let idx = |p: *const u8| (p as usize).wrapping_sub(base as usize) as u32;
    // Debug check that `n` bytes at `p` are inside the readable window.
    let chk = |p: *const u8, n: usize| {
        debug_assert!(
            (p as usize) >= (lowest as usize) && (p as usize) + n <= (iend as usize),
            "read out of the window"
        );
    };

    let mut anchor = istart;
    let mut ip0 = istart;

    let mut rep_offset1 = rep[0];
    let mut rep_offset2 = rep[1];
    let mut offset_saved1 = 0u32;
    let mut offset_saved2 = 0u32;

    if ip0 == prefix_start {
        ip0 = ip0.wrapping_add(1);
    }
    {
        let curr = idx(ip0);
        let window_low = ms.lowest_prefix_index(curr, cparams.window_log);
        let max_rep = curr.wrapping_sub(window_low);
        if rep_offset2 > max_rep {
            offset_saved2 = rep_offset2;
            rep_offset2 = 0;
        }
        if rep_offset1 > max_rep {
            offset_saved1 = rep_offset1;
            rep_offset1 = 0;
        }
    }

    // SAFETY (whole block): see the function docs. Hash table writes use
    // hashes of `hlog` bits into a table of `1 << hlog` entries (asserted
    // above); reads are debug-checked with `chk`.
    unsafe {
        let hash = |p: *const u8| {
            chk(p, HASH_READ_SIZE);
            hash_mls::<MLS>(p, hlog)
        };
        let match_found = |cur: *const u8, match_idx: u32| -> bool {
            if USE_CMOV {
                // ZSTD_match4Found_cmov
                let valid = match_idx >= prefix_start_index;
                // A branchless select, as libzstd forces with inline asm
                // (`ZSTD_selectAddr`): the condition is unpredictable.
                let mval_addr = core::hint::select_unpredictable(
                    valid,
                    base.wrapping_add(match_idx as usize),
                    DUMMY.as_ptr(),
                );
                if valid {
                    chk(mval_addr, 4);
                }
                if read32(cur) != read32(mval_addr) {
                    return false;
                }
                valid
            } else {
                // ZSTD_match4Found_branch
                let mval = if match_idx >= prefix_start_index {
                    let m = base.wrapping_add(match_idx as usize);
                    chk(m, 4);
                    read32(m)
                } else {
                    read32(cur) ^ 1
                };
                read32(cur) == mval
            }
        };

        'start: loop {
            let mut step = step_size;
            let mut next_step = ip0.wrapping_add(K_STEP_INCR);
            let mut ip1 = ip0.wrapping_add(1);
            let mut ip2 = ip0.wrapping_add(step);
            let mut ip3 = ip2.wrapping_add(1);

            if ip3 >= ilimit {
                break 'start;
            }

            let mut hash0 = hash(ip0);
            let mut hash1 = hash(ip1);
            let mut match_idx = *ht.add(hash0);

            let current0: u32;
            let mut match0: *const u8;
            let mut m_length: usize;
            let offcode: u32;

            loop {
                // load repcode match for ip[2]
                let rep_ptr = ip2.wrapping_sub(rep_offset1 as usize);
                chk(rep_ptr, 4);
                let rval = read32(rep_ptr);

                // write back hash table entry
                let c0 = idx(ip0);
                *ht.add(hash0) = c0;

                // check repcode at ip[2]
                if (read32(ip2) == rval) & (rep_offset1 > 0) {
                    ip0 = ip2;
                    match0 = ip0.wrapping_sub(rep_offset1 as usize);
                    chk(match0.wrapping_sub(1), 1);
                    m_length = (*ip0.sub(1) == *match0.sub(1)) as usize;
                    ip0 = ip0.wrapping_sub(m_length);
                    match0 = match0.wrapping_sub(m_length);
                    offcode = REPCODE1_TO_OFFBASE;
                    m_length += 4;
                    // Write next hash table entry: it's already calculated.
                    // This write is known to be safe because ip1 is before
                    // the repcode (ip2).
                    *ht.add(hash1) = idx(ip1);
                    current0 = c0;
                    break;
                }

                if match_found(ip0, match_idx) {
                    // Write next hash table entry (it's already calculated).
                    *ht.add(hash1) = idx(ip1);
                    current0 = c0;
                    (match0, m_length, offcode) = offset_match(
                        base,
                        match_idx,
                        &mut ip0,
                        anchor,
                        prefix_start,
                        &mut rep_offset1,
                        &mut rep_offset2,
                    );
                    break;
                }

                // lookup ip[1]
                match_idx = *ht.add(hash1);

                // hash ip[2]
                hash0 = hash1;
                hash1 = hash(ip2);

                // advance to next positions
                ip0 = ip1;
                ip1 = ip2;
                ip2 = ip3;

                // write back hash table entry
                let c0 = idx(ip0);
                *ht.add(hash0) = c0;

                if match_found(ip0, match_idx) {
                    // Write next hash table entry, since it's already
                    // calculated, unless search resumes at or before it.
                    if step <= 4 {
                        *ht.add(hash1) = idx(ip1);
                    }
                    current0 = c0;
                    (match0, m_length, offcode) = offset_match(
                        base,
                        match_idx,
                        &mut ip0,
                        anchor,
                        prefix_start,
                        &mut rep_offset1,
                        &mut rep_offset2,
                    );
                    break;
                }

                // lookup ip[1]
                match_idx = *ht.add(hash1);

                // hash ip[2]
                hash0 = hash1;
                hash1 = hash(ip2);

                // advance to next positions
                ip0 = ip1;
                ip1 = ip2;
                ip2 = ip0.wrapping_add(step);
                ip3 = ip1.wrapping_add(step);

                // calculate step
                if ip2 >= next_step {
                    step += 1;
                    prefetch_l1(ip1.wrapping_add(64));
                    prefetch_l1(ip1.wrapping_add(128));
                    next_step = next_step.wrapping_add(K_STEP_INCR);
                }

                if ip3 >= ilimit {
                    break 'start;
                }
            }

            // _match: count the forward length.
            chk(match0, 0);
            m_length += count(ip0.add(m_length), match0.add(m_length), iend);

            ss.store_seq(
                buf,
                anchor as usize - bp as usize,
                ip0 as usize - anchor as usize,
                end,
                offcode,
                m_length,
            );

            ip0 = ip0.add(m_length);
            anchor = ip0;

            // Fill table and check for immediate repcode.
            if ip0 <= ilimit {
                // Fill Table
                debug_assert!(base.wrapping_add(current0 as usize + 2) > istart);
                *ht.add(hash(base.wrapping_add(current0 as usize + 2))) = current0 + 2;
                *ht.add(hash(ip0.sub(2))) = idx(ip0.sub(2));

                if rep_offset2 > 0 {
                    // rep_offset2==0 means rep_offset2 is invalidated
                    while ip0 <= ilimit && {
                        let r = ip0.wrapping_sub(rep_offset2 as usize);
                        chk(r, 4);
                        read32(ip0) == read32(r)
                    } {
                        // store sequence
                        let r = ip0.wrapping_sub(rep_offset2 as usize);
                        let r_length = count(ip0.add(4), r.add(4), iend) + 4;
                        core::mem::swap(&mut rep_offset1, &mut rep_offset2);
                        *ht.add(hash(ip0)) = idx(ip0);
                        ip0 = ip0.add(r_length);
                        ss.store_seq(
                            buf,
                            anchor as usize - bp as usize,
                            0,
                            end,
                            REPCODE1_TO_OFFBASE,
                            r_length,
                        );
                        anchor = ip0;
                    }
                }
            }
        }
    }

    // _cleanup: restore the repcodes that were invalid at the start.
    if offset_saved1 != 0 && rep_offset1 != 0 {
        offset_saved2 = offset_saved1;
    }
    rep[0] = if rep_offset1 != 0 {
        rep_offset1
    } else {
        offset_saved1
    };
    rep[1] = if rep_offset2 != 0 {
        rep_offset2
    } else {
        offset_saved2
    };

    // Return the last literals size
    iend as usize - anchor as usize
}

/// The `_offset:` label of `ZSTD_compressBlock_fast_noDict_generic`: turns
/// a hash-table hit at `ip0` into a match, extending it backwards. Returns
/// `(match0, mLength, offcode)` and moves `ip0` back.
///
/// # Safety
/// `base + match_idx` must be a valid candidate (`>= prefix_start`) and
/// `ip0 > match`; the backward extension reads stay above `anchor` and
/// `prefix_start`.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn offset_match(
    base: *const u8,
    match_idx: u32,
    ip0: &mut *const u8,
    anchor: *const u8,
    prefix_start: *const u8,
    rep_offset1: &mut u32,
    rep_offset2: &mut u32,
) -> (*const u8, usize, u32) {
    let mut match0 = base.wrapping_add(match_idx as usize);
    *rep_offset2 = *rep_offset1;
    *rep_offset1 = (*ip0 as usize - match0 as usize) as u32;
    let offcode = offset_to_offbase(*rep_offset1);
    let mut m_length = 4;
    // Count the backwards match length.
    // SAFETY: `ip0 > anchor >= istart` and `match0 > prefix_start` before
    // each read of the byte before them.
    unsafe {
        while ((*ip0 > anchor) & (match0 > prefix_start)) && *ip0.sub(1) == *match0.sub(1) {
            *ip0 = ip0.sub(1);
            match0 = match0.sub(1);
            m_length += 1;
        }
    }
    (match0, m_length, offcode)
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::params::{CParams, one_shot_cparams};
    use super::super::seq_store::{
        MINMATCH, REP_START_VALUE, SeqStore, offbase_is_offset, offbase_to_offset,
    };
    use super::*;
    use alloc::vec::Vec;

    /// Replays the sequences of `ss` (with repcode history `rep`) onto
    /// `out`, as a decoder would.
    fn replay(ss: &SeqStore, rep: &mut [u32; 3], out: &mut Vec<u8>) {
        let lits = ss.literals();
        let mut lp = 0;
        for i in 0..ss.nb_seq {
            let len = ss.sequence_length(i);
            let s = ss.sequences[i];
            out.extend_from_slice(&lits[lp..lp + len.lit_length as usize]);
            lp += len.lit_length as usize;
            let ll0 = len.lit_length == 0;
            let offset = if offbase_is_offset(s.off_base) {
                let o = offbase_to_offset(s.off_base);
                rep[2] = rep[1];
                rep[1] = rep[0];
                rep[0] = o;
                o
            } else {
                let r = s.off_base - 1 + ll0 as u32;
                if r == 0 {
                    rep[0]
                } else {
                    let o = if r == 3 { rep[0] - 1 } else { rep[r as usize] };
                    if r >= 2 {
                        rep[2] = rep[1];
                    }
                    rep[1] = rep[0];
                    rep[0] = o;
                    o
                }
            };
            assert!(len.match_length >= MINMATCH);
            let from = out.len() - offset as usize;
            for k in 0..len.match_length as usize {
                let b = out[from + k];
                out.push(b);
            }
        }
        out.extend_from_slice(&lits[lp..]);
    }

    /// Compresses `src` like libzstd's one-shot frame loop (block splitting
    /// at `block_size`), replays the sequences and checks they rebuild
    /// `src`. Returns the total number of sequences.
    pub(crate) fn roundtrip(src: &[u8], cparams: CParams, block_size: usize) -> usize {
        let mut ms = MatchState::new(cparams);
        let mut ss = SeqStore::new(block_size, cparams.min_match);
        ms.update_window(0, src.len());
        let mut rep = REP_START_VALUE;
        let mut dec_rep = REP_START_VALUE;
        let mut out = Vec::new();
        let mut pos = 0;
        let mut nb_seq = 0;
        while pos < src.len() {
            let bs = block_size.min(src.len() - pos);
            ms.prepare_block(pos, bs);
            let mut next_rep = rep;
            if build_seq_store(&mut ms, &mut ss, &mut next_rep, src, pos, pos + bs) {
                // Entropy coding must replay with the previous block's
                // repcodes; the encoder's are updated by the match finder.
                replay(&ss, &mut dec_rep, &mut out);
                rep = next_rep;
                nb_seq += ss.nb_seq;
                // The block-level repcodes must match what the decoder
                // derives from the sequences.
                assert_eq!(&dec_rep[..2], &rep[..2]);
            } else {
                out.extend_from_slice(&src[pos..pos + bs]);
            }
            assert_eq!(&out[..], &src[..pos + bs], "block at {pos}");
            pos += bs;
        }
        nb_seq
    }

    pub(crate) fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Text-like data: words from a small vocabulary, with some noise.
    pub(crate) fn texty(n: usize, seed: u64) -> Vec<u8> {
        const WORDS: [&[u8]; 12] = [
            b"the ",
            b"quick ",
            b"brown ",
            b"fox ",
            b"jumps ",
            b"over ",
            b"lazy ",
            b"dog ",
            b"zstd ",
            b"compress ",
            b"block ",
            b"\n",
        ];
        let mut s = seed | 1;
        let mut v = Vec::with_capacity(n + 16);
        while v.len() < n {
            let r = xorshift(&mut s);
            if r.is_multiple_of(17) {
                v.push((r >> 8) as u8);
            } else {
                v.extend_from_slice(WORDS[(r >> 16) as usize % WORDS.len()]);
            }
        }
        v.truncate(n);
        v
    }

    pub(crate) fn random(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n).map(|_| xorshift(&mut s) as u8).collect()
    }

    #[test]
    fn roundtrip_levels() {
        let inputs: [Vec<u8>; 7] = [
            texty(1000, 1),
            texty(100_000, 2),
            texty(300_000, 3),
            random(50_000, 4),
            vec![0u8; 200_000],
            (0..70_000u32).map(|i| (i % 251) as u8).collect(),
            {
                let mut v = random(20_000, 5);
                v.extend_from_within(..);
                v.extend_from_slice(&texty(30_000, 6));
                v
            },
        ];
        for src in &inputs {
            for level in [-50, -5, -1, 1, 2] {
                let cp = one_shot_cparams(level, src.len() as u64);
                if cp.strategy != Strategy::Fast {
                    continue; // level 2 is dfast for some sizes
                }
                let bs = super::super::params::block_size_max(&cp, src.len() as u64);
                roundtrip(src, cp, bs);
            }
        }
    }

    #[test]
    fn roundtrip_all_variants() {
        let mut src = texty(150_000, 7);
        src.extend_from_slice(&random(5000, 8));
        src.extend_from_slice(&src.clone()[..40_000]);
        for mls in 3..=8 {
            for window_log in [10, 14, 18, 19, 22] {
                for target_length in [0, 1, 7] {
                    let cp = CParams {
                        window_log,
                        chain_log: 16,
                        hash_log: 14,
                        search_log: 1,
                        min_match: mls,
                        target_length,
                        strategy: Strategy::Fast,
                    };
                    let n = roundtrip(&src, cp, (1 << 17).min(1 << window_log));
                    assert!(n > 1000, "mls {mls} wlog {window_log}: {n} sequences");
                }
            }
        }
    }

    #[test]
    fn small_blocks() {
        let src = texty(5000, 9);
        let cp = one_shot_cparams(1, src.len() as u64);
        for bs in [1, 6, 7, 8, 9, 20, 33, 100, 1000] {
            roundtrip(&src, cp, bs);
        }
        for n in 0..40 {
            let s = texty(n, 10);
            roundtrip(&s, one_shot_cparams(1, n as u64), 1 << 17);
        }
    }

    #[test]
    fn repetitive_matches() {
        // Zeros: one long rep match per block after the first position.
        let src = vec![0u8; 300_000];
        let cp = one_shot_cparams(1, src.len() as u64);
        let mut ms = MatchState::new(cp);
        let mut ss = SeqStore::new(1 << 17, cp.min_match);
        ms.update_window(0, src.len());
        let mut rep = REP_START_VALUE;
        ms.prepare_block(0, 1 << 17);
        assert!(build_seq_store(
            &mut ms,
            &mut ss,
            &mut rep,
            &src,
            0,
            1 << 17
        ));
        // First byte skipped (ip0 == prefixStart); repcode 1 (offset 1)
        // found at ip2 = 3, back-extended to position 2, to the block end.
        // rep[1] = 4 was out of the window at the start and is restored.
        assert_eq!(ss.nb_seq, 1);
        assert_eq!(ss.sequences[0].off_base, REPCODE1_TO_OFFBASE);
        assert_eq!(ss.sequence_length(0).lit_length, 2);
        assert_eq!(ss.sequence_length(0).match_length, (1 << 17) - 2);
        assert_eq!(rep, [1, 4, 8]);
    }

    #[test]
    fn window_indices() {
        let mut w = Window::new();
        assert!(w.is_empty());
        assert!(!w.update(0, 100, false));
        assert_eq!(w.index(0), WINDOW_START_INDEX);
        assert_eq!((w.dict_limit, w.low_limit), (2, 2));
        assert!(w.update(100, 50, false));
        assert_eq!(w.index(149), 151);
        // A non-contiguous update makes the old data the extDict.
        assert!(!w.update(1000, 50, false));
        assert_eq!(w.dict_limit, 152);
        assert_eq!(w.low_limit, 2);
        assert_eq!(w.index(1000), 152);
        assert!(w.has_ext_dict());

        let mut w = Window::new();
        w.update(0, 1 << 20, false);
        let mut lde = 0;
        w.enforce_max_dist(1 << 19, 1 << 17, &mut lde);
        assert_eq!(w.low_limit, (1 << 19) + 2 - (1 << 17));
        assert_eq!(w.dict_limit, w.low_limit);
    }

    #[test]
    fn overflow_correction() {
        let cp = one_shot_cparams(1, 1 << 30);
        let mut ms = MatchState::new(cp);
        // Pretend 3.5 GiB were consumed: the window covers positions
        // [0, 4 GiB) of an imaginary buffer.
        let big = (CURRENT_MAX as usize) + 1000;
        ms.window.update(0, big + (1 << 17), false);
        ms.hash_table[0] = 5;
        ms.hash_table[1] = (big - 10) as u32 + 2;
        let start = big;
        ms.prepare_block(start, 1 << 17);
        assert_eq!(ms.window.nb_overflow_corrections, 1);
        let curr = ms.window.index(start);
        let max_dist = 1u32 << cp.window_log;
        assert!(curr < 2 * max_dist + (1 << cp.chain_log) + 2);
        assert_eq!(ms.hash_table[0], 0);
        assert_eq!(ms.window.index(big - 10), ms.hash_table[1]);
        assert!(ms.window.low_limit >= WINDOW_START_INDEX);
    }

    #[test]
    fn fill_table() {
        let src = texty(1000, 11);
        let cp = one_shot_cparams(1, 1000);
        let mut ms = MatchState::new(cp);
        ms.update_window(0, src.len());
        ms.fill_hash_table(&src, src.len(), TableLoadMethod::Fast);
        let h = super::super::seq_store::hash_at(&src, 3, cp.hash_log, cp.min_match);
        assert!(ms.hash_table[h] >= 5); // position 3 (index 5) or a later one
        assert!(
            ms.hash_table
                .iter()
                .all(|&v| v == 0 || (2..1002).contains(&v))
        );
    }
}
