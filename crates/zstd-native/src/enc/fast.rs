//! The `fast` strategy (levels 1-2 and negative levels): a port of libzstd
//! 1.5.7's `compress/zstd_fast.c` (noDict, extDict and dictMatchState
//! paths), plus the
//! parts of `ZSTD_MatchState_t` / `ZSTD_window_t`
//! (`compress/zstd_compress_internal.h`) and of `zstd_compress.c`'s block
//! loop that drive it.
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
//! With an extDict (the streaming compressor's input buffer wrapped, see
//! the `stream` module), indices below `dictLimit` are at
//! `buf[dict_base.wrapping_add(i)]`: both segments are positions in the
//! same buffer, the extDict usually after the prefix. [`ExtWindow`] holds
//! them as raw pointers for the `_extDict` match finders, which
//! [`build_seq_store`] picks when [`Window::has_ext_dict`] (libzstd's
//! `ZSTD_selectBlockCompressor`).
//!
//! # Dictionaries
//!
//! A dictionary's content (see the `cdict` module) stays in its own buffer:
//! a window that starts with it has `base` in that buffer
//! ([`Window::base_in_dict`]), and the frame's first input segment makes
//! it the extDict ([`Window::dict_base_in_dict`], read through
//! [`MatchState::ext_buf`]). An attached dictionary ([`MatchState::dms`]) is
//! not in the window at all: the `_dictMatchState` match finders read its
//! own window and tables ([`DmsView`]), its index `i` standing for the
//! window's `i + dictIndexDelta`, just below the prefix.
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
    REPCODE1_TO_OFFBASE, SeqStore, count, count_2segments, hash4_ptr, hash5_ptr, hash6_ptr,
    hash7_ptr, hash8_ptr, offset_to_offbase, read_le64, read32,
};

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
/// `ZSTD_INDEXOVERFLOW_MARGIN`: a reused context resets its indices instead
/// of continuing them once they are this close to [`CURRENT_MAX`].
pub(crate) const INDEXOVERFLOW_MARGIN: u32 = 16 << 20;
/// `ZSTD_HASHLOG3_MAX`: the largest `hashLog3`.
pub(crate) const HASHLOG3_MAX: u32 = 17;
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
    /// `base` is a position in the dictionary's buffer, not the caller's
    /// (after loading a dictionary's content, see
    /// [`MatchState::dict`]): the next update is non-contiguous.
    pub(crate) base_in_dict: bool,
    /// `dictBase` is a position in the dictionary's buffer: the extDict
    /// segment is the loaded dictionary.
    pub(crate) dict_base_in_dict: bool,
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
            base_in_dict: false,
            dict_base_in_dict: false,
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
        // After a dictionary's content (in another buffer), the caller's
        // data is never contiguous with it.
        if src != self.next_src || force_non_contiguous || self.base_in_dict {
            // not contiguous
            let distance_from_base = self.next_src.wrapping_sub(self.base);
            self.low_limit = self.dict_limit;
            debug_assert!(distance_from_base == distance_from_base as u32 as usize);
            self.dict_limit = distance_from_base as u32;
            self.dict_base = self.base;
            self.dict_base_in_dict = self.base_in_dict;
            self.base_in_dict = false;
            self.base = src.wrapping_sub(distance_from_base);
            if self.dict_limit - self.low_limit < HASH_READ_SIZE as u32 {
                self.low_limit = self.dict_limit; // too small extDict
            }
            contiguous = false;
        }
        self.next_src = src.wrapping_add(src_size);
        // if input and dictionary overlap: reduce dictionary (area presumed
        // modified by input); a dictionary in its own buffer never overlaps
        let src_end = src.wrapping_add(src_size);
        if !self.dict_base_in_dict
            && (src_end > self.dict_base.wrapping_add(self.low_limit as usize))
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
    /// the dictionary (`*loaded_dict_end = 0`, `*dms = false`) when the
    /// window is reached.
    pub(crate) fn enforce_max_dist(
        &mut self,
        block_end: usize,
        max_dist: u32,
        loaded_dict_end: &mut u32,
        dms: &mut bool,
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
            *dms = false;
        }
    }

    /// `ZSTD_checkDictValidity()`: invalidates the dictionary
    /// (`*loaded_dict_end = 0`, `*dms = false`: no dictMatchState) once the
    /// block reaches beyond the window, or after a non-contiguous update.
    pub(crate) fn check_dict_validity(
        &self,
        block_end: usize,
        max_dist: u32,
        loaded_dict_end: &mut u32,
        dms: &mut bool,
    ) {
        let block_end_idx = block_end.wrapping_sub(self.base) as u32;
        let lde = *loaded_dict_end;
        debug_assert!(block_end_idx >= lde);
        if block_end_idx > lde.wrapping_add(max_dist) || lde != self.dict_limit {
            *loaded_dict_end = 0;
            *dms = false;
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

/// `ZSTD_MatchState_t` without the dictionary parts.
#[derive(Debug)]
pub(crate) struct MatchState {
    /// `window`
    pub(crate) window: Window,
    /// `loadedDictEnd`: 0 without a dictionary.
    pub(crate) loaded_dict_end: u32,
    /// `nextToUpdate`
    pub(crate) next_to_update: u32,
    /// `hashTable`: `1 << cparams.hash_log` entries.
    pub(crate) hash_table: super::table::Table<u32>,
    /// `chainTable`: `1 << cparams.chain_log` entries, none for `fast` and
    /// the row match finder. `dfast` uses it as its short-match hash table,
    /// `btlazy2` as its binary tree.
    pub(crate) chain_table: super::table::Table<u32>,
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
    pub(crate) tag_table: super::table::Table<u8>,
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
    /// `hashLog3`: `min(ZSTD_HASHLOG3_MAX, windowLog)` for the optimal
    /// parser with `minMatch == 3`, else 0 (no table).
    pub(crate) hash_log3: u32,
    /// `hashTable3`: the optimal parser's 3-byte hash table.
    pub(crate) hash_table3: super::table::Table<u32>,
    /// `opt`: the optimal parser's state (btopt and above), allocated on
    /// first use and kept (like libzstd's workspace).
    pub(crate) opt: Option<alloc::boxed::Box<super::opt::OptState>>,
    /// The dictionary in use for the frame: the attached one
    /// (`dictMatchState`, when [`dms`](Self::dms)), or the one whose content
    /// was loaded or copied into this match state (the buffer behind
    /// [`Window::base_in_dict`] and [`Window::dict_base_in_dict`]).
    pub(crate) dict: Option<alloc::sync::Arc<super::cdict::CDict>>,
    /// `dictMatchState != NULL`: the match finders search
    /// [`dict`](Self::dict)'s tables (the `_dictMatchState` variants).
    pub(crate) dms: bool,
}

impl MatchState {
    /// A fresh match state (`ZSTD_reset_matchState` with `ZSTDirp_reset`
    /// and `ZSTDcrp_makeClean`).
    pub(crate) fn new(cparams: CParams) -> Self {
        let mut ms = Self::empty(cparams);
        ms.reset(cparams);
        ms
    }

    /// As [`new`](Self::new), with [`reset_with`](Self::reset_with)'s
    /// arguments.
    pub(crate) fn new_with(cparams: CParams, use_row: bool, for_cdict: bool) -> Self {
        let mut ms = Self::empty(cparams);
        ms.reset_with(cparams, use_row, for_cdict);
        ms
    }

    fn empty(cparams: CParams) -> Self {
        Self {
            window: Window::new(),
            loaded_dict_end: 0,
            next_to_update: 0,
            hash_table: super::table::Table::new(),
            chain_table: super::table::Table::new(),
            cparams,
            force_non_contiguous: false,
            use_row_match_finder: false,
            tag_table: super::table::Table::new(),
            hash_cache: [0; super::row::ROW_HASH_CACHE_SIZE],
            row_hash_log: 0,
            hash_salt: 0,
            hash_salt_entropy: 0,
            lazy_skipping: false,
            hash_log3: 0,
            hash_table3: super::table::Table::new(),
            opt: None,
            dict: None,
            dms: false,
        }
    }

    /// `ZSTD_reset_matchState()` with `ZSTDirp_reset` (indices restart at
    /// `ZSTD_WINDOW_START_INDEX`) and `ZSTDcrp_makeClean` (the hash table is
    /// zeroed), for the CCtx (see [`reset_keep_indices`](Self::reset_keep_indices)
    /// for `ZSTDirp_continue`). With the row match finder, the hash salt
    /// advances (`ZSTD_advanceHashSalt`).
    pub(crate) fn reset(&mut self, cparams: CParams) {
        let use_row = row_match_finder_used(
            cparams.strategy,
            resolve_row_match_finder_mode(ParamSwitch::Auto, &cparams),
        );
        self.reset_with(cparams, use_row, false);
    }

    /// [`reset`](Self::reset) with the row match finder use given (a CCtx
    /// using a CDict takes the CDict's, `params.useRowMatchFinder =
    /// cdict->useRowMatchFinder`), for a CCtx or, with `for_cdict`, a CDict
    /// (`ZSTD_resetTarget_CDict`: no `hashTable3`, no optimal parser state,
    /// a zeroed tag table and a hash salt of 0).
    pub(crate) fn reset_with(&mut self, cparams: CParams, use_row: bool, for_cdict: bool) {
        self.reset_impl(cparams, use_row, for_cdict, false);
    }

    /// [`reset_with`](Self::reset_with) for a CCtx that then compresses
    /// without a dictionary, with libzstd's `ZSTDirp_continue`, the index
    /// policy of a reused context (`ZSTD_resetCCtx_internal`): the window
    /// keeps its indices and is only cleared (`ZSTD_invalidateMatchState`),
    /// so every index stored by earlier frames is below the new window, and
    /// tables of an unchanged size are not zeroed again (libzstd's
    /// `ZSTD_cwksp_clean_tables` only zeroes newly reserved table space).
    /// The match finders never use an index below the window, so the
    /// sequences are the same as after a reset. Falls back to the reset
    /// when the indices come close to the maximum
    /// (`ZSTD_indexTooCloseToMax`), when the table sizes, the strategy or
    /// the match finder change, or after a frame with a dictionary.
    pub(crate) fn reset_keep_indices(&mut self, cparams: CParams, use_row: bool) {
        self.reset_impl(cparams, use_row, false, true);
    }

    fn reset_impl(&mut self, cparams: CParams, use_row: bool, for_cdict: bool, keep: bool) {
        let h_size = 1usize << cparams.hash_log;
        // ZSTD_allocateChainTable: none for `fast` and the row match finder.
        let c_size = if cparams.strategy == Strategy::Fast || use_row {
            0
        } else {
            1usize << cparams.chain_log
        };
        // The optimal parser's tables (hashTable3 only with minMatch 3).
        let hash_log3 = if cparams.min_match == 3 && !for_cdict {
            HASHLOG3_MAX.min(cparams.window_log)
        } else {
            0
        };
        let h3_size = if hash_log3 > 0 {
            1usize << hash_log3
        } else {
            0
        };
        let keep = keep
            && !for_cdict
            && self.dict.is_none()
            && !self.dms
            && !self.window.base_in_dict
            && !self.window.dict_base_in_dict
            && self.cparams.strategy == cparams.strategy
            && self.use_row_match_finder == use_row
            && self.hash_table.len() == h_size
            && self.chain_table.len() == c_size
            && self.hash_table3.len() == h3_size
            // !ZSTD_indexTooCloseToMax
            && self.window.next_src.wrapping_sub(self.window.base)
                <= (CURRENT_MAX - INDEXOVERFLOW_MARGIN) as usize;
        if !keep {
            self.window = Window::new();
        }
        self.lazy_skipping = false;
        self.invalidate();
        if !keep {
            self.hash_table.reset_zeroed(h_size);
            self.chain_table.reset_zeroed(c_size);
            self.hash_table3.reset_zeroed(h3_size);
        }
        self.use_row_match_finder = use_row;
        if use_row {
            if for_cdict {
                self.tag_table.reset_zeroed(h_size);
                self.hash_salt = 0;
            } else {
                // The tag table is "init once" (see its docs): new bytes are
                // zeroed, existing ones are kept.
                self.tag_table.resize(h_size);
                self.advance_hash_salt();
            }
            let row_log = cparams.search_log.clamp(4, 6);
            debug_assert!(cparams.hash_log >= row_log);
            self.row_hash_log = cparams.hash_log - row_log;
        }
        self.hash_log3 = hash_log3;
        if cparams.strategy >= Strategy::BtOpt && self.opt.is_none() && !for_cdict {
            self.opt = Some(alloc::boxed::Box::new(super::opt::OptState::new()));
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
        self.dms = false;
        self.dict = None;
        // force reset of btopt stats
        if let Some(opt) = self.opt.as_deref_mut() {
            opt.stats.lit_length_sum = 0;
        }
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
            reduce_table(&mut self.hash_table3, correction);
            self.next_to_update = self.next_to_update.saturating_sub(correction);
            // invalidate dictionaries on overflow correction
            self.loaded_dict_end = 0;
            self.dms = false;
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
            .check_dict_validity(end, max_dist, &mut self.loaded_dict_end, &mut self.dms);
        self.window
            .enforce_max_dist(start, max_dist, &mut self.loaded_dict_end, &mut self.dms);
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

    /// `ZSTD_fillHashTableForCDict()`: as [`fill_hash_table`](Self::fill_hash_table),
    /// with tagged entries (`ZSTD_writeTaggedIndex`: the index shifted left
    /// by `ZSTD_SHORT_CACHE_TAG_BITS`, a tag from the hash's low bits).
    pub(crate) fn fill_hash_table_for_cdict(
        &mut self,
        buf: &[u8],
        end: usize,
        dtlm: TableLoadMethod,
    ) {
        const FAST_HASH_FILL_STEP: usize = 3;
        let h_bits = self.cparams.hash_log + SHORT_CACHE_TAG_BITS;
        let mls = self.cparams.min_match;
        assert!(end <= buf.len());
        let mut ip = self.window.pos(self.next_to_update);
        while ip + FAST_HASH_FILL_STEP + HASH_READ_SIZE < end + 2 {
            let curr = self.window.index(ip);
            write_tagged_index(
                &mut self.hash_table,
                hash_at_buf(buf, ip, h_bits, mls),
                curr,
            );
            if dtlm == TableLoadMethod::Full {
                for p in 1..FAST_HASH_FILL_STEP {
                    let hash_and_tag = hash_at_buf(buf, ip + p, h_bits, mls);
                    if self.hash_table[hash_and_tag >> SHORT_CACHE_TAG_BITS] == 0 {
                        write_tagged_index(&mut self.hash_table, hash_and_tag, curr + p as u32);
                    }
                }
            }
            ip += FAST_HASH_FILL_STEP;
        }
    }

    /// The buffer holding the extDict segment: the dictionary's content
    /// while the window's extDict is the loaded (or copied) dictionary,
    /// else `buf` (a wrapped stream buffer).
    pub(crate) fn ext_buf<'a>(&'a self, buf: &'a [u8]) -> &'a [u8] {
        match &self.dict {
            Some(d) if self.window.dict_base_in_dict => &d.content,
            _ => buf,
        }
    }
}

/// `ZSTD_SHORT_CACHE_TAG_BITS`: CDict hash tables of `fast` and `dfast`
/// hold `index << 8 | tag`.
pub(crate) const SHORT_CACHE_TAG_BITS: u32 = 8;
/// `ZSTD_SHORT_CACHE_TAG_MASK`.
pub(crate) const SHORT_CACHE_TAG_MASK: u32 = (1 << SHORT_CACHE_TAG_BITS) - 1;

/// `ZSTD_writeTaggedIndex()`: stores `index` with the tag of `hash_and_tag`
/// (a hash of `hashLog + 8` bits) in the slot of its top `hashLog` bits.
#[inline(always)]
pub(crate) fn write_tagged_index(table: &mut [u32], hash_and_tag: usize, index: u32) {
    let hash = hash_and_tag >> SHORT_CACHE_TAG_BITS;
    let tag = hash_and_tag as u32 & SHORT_CACHE_TAG_MASK;
    debug_assert!(index >> (32 - SHORT_CACHE_TAG_BITS) == 0);
    table[hash] = (index << SHORT_CACHE_TAG_BITS) | tag;
}

/// Safe `ZSTD_hashPtr` on a buffer position (reads `HASH_READ_SIZE` bytes).
fn hash_at_buf(buf: &[u8], pos: usize, h_bits: u32, mls: u32) -> usize {
    super::seq_store::hash_at(buf, pos, h_bits, mls)
}

/// `ZSTD_buildSeqStore()` without LDM or an external sequence producer,
/// for every strategy and dictionary mode: fills `ss` with the sequences of
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
    // ZSTD_selectBlockCompressor with ZSTD_matchState_dictMode()
    let last_ll_size = if ms.window.has_ext_dict() {
        match ms.cparams.strategy {
            Strategy::Fast => compress_block_fast_ext_dict(ms, ss, rep, buf, start, end),
            Strategy::DFast => {
                super::dfast::compress_block_double_fast_ext_dict(ms, ss, rep, buf, start, end)
            }
            Strategy::Greedy | Strategy::Lazy | Strategy::Lazy2 | Strategy::BtLazy2 => {
                super::lazy::compress_block_lazy_ext_dict(ms, ss, rep, buf, start, end)
            }
            Strategy::BtOpt | Strategy::BtUltra | Strategy::BtUltra2 => {
                super::opt::compress_block_opt_ext_dict(ms, ss, rep, buf, start, end)
            }
        }
    } else if ms.dms {
        match ms.cparams.strategy {
            Strategy::Fast => compress_block_fast_dict_match_state(ms, ss, rep, buf, start, end),
            Strategy::DFast => super::dfast::compress_block_double_fast_dict_match_state(
                ms, ss, rep, buf, start, end,
            ),
            Strategy::Greedy | Strategy::Lazy | Strategy::Lazy2 | Strategy::BtLazy2 => {
                super::lazy::compress_block_lazy_dict_match_state(ms, ss, rep, buf, start, end)
            }
            Strategy::BtOpt | Strategy::BtUltra | Strategy::BtUltra2 => {
                super::opt::compress_block_opt_dict_match_state(ms, ss, rep, buf, start, end)
            }
        }
    } else {
        match ms.cparams.strategy {
            Strategy::Fast => compress_block_fast(ms, ss, rep, buf, start, end),
            Strategy::DFast => {
                super::dfast::compress_block_double_fast(ms, ss, rep, buf, start, end)
            }
            Strategy::Greedy | Strategy::Lazy | Strategy::Lazy2 | Strategy::BtLazy2 => {
                super::lazy::compress_block_lazy(ms, ss, rep, buf, start, end)
            }
            Strategy::BtOpt | Strategy::BtUltra | Strategy::BtUltra2 => {
                super::opt::compress_block_opt(ms, ss, rep, buf, start, end)
            }
        }
    };
    ss.store_last_literals(&buf[end - last_ll_size..end]);
    true
}

/// A block compressor (`ZSTD_BlockCompressor_f`): finds the sequences of
/// `buf[start..end]`, updates the repcodes, returns the number of trailing
/// literals.
pub(crate) type BlockCompressor =
    fn(&mut MatchState, &mut SeqStore, &mut [u32; 3], &[u8], usize, usize) -> usize;

/// `ZSTD_selectBlockCompressor()` with `ZSTD_matchState_dictMode()`: the
/// block compressor [`build_seq_store`] runs (for LDM, which runs it on
/// the literals between long matches).
pub(crate) fn select_block_compressor(ms: &MatchState) -> BlockCompressor {
    use super::{dfast, lazy, opt};
    if ms.window.has_ext_dict() {
        match ms.cparams.strategy {
            Strategy::Fast => compress_block_fast_ext_dict,
            Strategy::DFast => dfast::compress_block_double_fast_ext_dict,
            Strategy::Greedy | Strategy::Lazy | Strategy::Lazy2 | Strategy::BtLazy2 => {
                lazy::compress_block_lazy_ext_dict
            }
            Strategy::BtOpt | Strategy::BtUltra | Strategy::BtUltra2 => {
                opt::compress_block_opt_ext_dict
            }
        }
    } else if ms.dms {
        match ms.cparams.strategy {
            Strategy::Fast => compress_block_fast_dict_match_state,
            Strategy::DFast => dfast::compress_block_double_fast_dict_match_state,
            Strategy::Greedy | Strategy::Lazy | Strategy::Lazy2 | Strategy::BtLazy2 => {
                lazy::compress_block_lazy_dict_match_state
            }
            Strategy::BtOpt | Strategy::BtUltra | Strategy::BtUltra2 => {
                opt::compress_block_opt_dict_match_state
            }
        }
    } else {
        match ms.cparams.strategy {
            Strategy::Fast => compress_block_fast,
            Strategy::DFast => dfast::compress_block_double_fast,
            Strategy::Greedy | Strategy::Lazy | Strategy::Lazy2 | Strategy::BtLazy2 => {
                lazy::compress_block_lazy
            }
            Strategy::BtOpt | Strategy::BtUltra | Strategy::BtUltra2 => opt::compress_block_opt,
        }
    }
}

/// `ZSTD_compressBlock_fast()`: finds the sequences of the block
/// `buf[start..end]` (no dictionary). The window must have been updated
/// to cover the block ([`MatchState::update_window`]), and every index from
/// `window.dict_limit` up must be a position in `buf`. Picks the
/// `ZSTD_compressBlock_fast_noDict_<mls>_<cmov>` variant like libzstd:
/// `mls = minMatch` (3 and other values use 4), `useCmov = windowLog < 19`
/// (the two variants find the same matches; only the code differs).
/// Updates `rep` and returns the number of trailing literals.
#[inline(never)]
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

/// Loop invariants of the noDict position search ([`fast_search_rust`]).
pub(super) struct FastSearch {
    /// Hash: `(read64(p) * prime) >> shift` (see [`hash_prime`]).
    pub(super) prime: u64,
    pub(super) step: usize,
    /// Return once the new `ip2` is at or past `lim` ([`FAST_LIMIT`]).
    pub(super) lim: usize,
    pub(super) base: *const u8,
    pub(super) ht: *mut u32,
    /// `prefixStartIndex`.
    pub(super) low: u64,
    /// `64 - hashLog`.
    pub(super) shift: u64,
    /// Readable window `[lowest, iend)`, for debug assertions.
    pub(super) lowest: *const u8,
    pub(super) iend: *const u8,
}

/// Position search state of [`fast_search_rust`]. At the top of the loop
/// (in, and out at [`FAST_LIMIT`]): `ip1 = ip0 + 1`, `ip3 = ip2 + 1`,
/// `hash0`/`hash1` the hashes of `ip0`/`ip1`, `match_idx` the table entry
/// of `ip0`.
pub(super) struct FastCursor {
    pub(super) ip0: *const u8,
    pub(super) ip2: *const u8,
    pub(super) hash0: usize,
    pub(super) hash1: usize,
    pub(super) match_idx: u64,
}

/// [`fast_search`] found a repcode match at `ip2` (cursor as at the top of
/// the loop, `ip0`'s entry written).
pub(super) const FAST_REP: u32 = 0;
/// A table match at `ip0` (cursor as at the top of the loop).
pub(super) const FAST_MATCH0: u32 = 1;
/// A table match at the second position: `ip0` is that position,
/// `ip2` holds its `ip1` and `hash1` the hash of that `ip1`; `match_idx` the
/// matching entry.
pub(super) const FAST_MATCH1: u32 = 2;
/// No match, and the new `ip2` is at or past `lim` (cursor at the top of
/// the next iteration): the caller updates the step and limits.
pub(super) const FAST_LIMIT: u32 = 3;
/// `(read64(p) * prime) >> shift`: [`hash_mls`] with `prime =
/// hash_prime::<MLS>()` and `shift = 64 - hBits`.
///
/// # Safety
/// `p..p + 8` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn hash_with(p: *const u8, prime: u64, shift: u64) -> usize {
    debug_assert!(shift < 64);
    // SAFETY: per the caller.
    (unsafe { read_le64(p) }.wrapping_mul(prime) >> shift) as usize
}

/// `ZSTD_hashPtr(p, hBits, mls)` is `(read64(p) * prime) >> (64 - hBits)`
/// with this prime (libzstd's, pre-shifted by `64 - 8 * mls`).
#[inline(always)]
pub(crate) const fn hash_prime<const MLS: u32>() -> u64 {
    use super::seq_store::{PRIME4BYTES, PRIME5BYTES, PRIME6BYTES, PRIME7BYTES, PRIME8BYTES};
    match MLS {
        5 => PRIME5BYTES << 24,
        6 => PRIME6BYTES << 16,
        7 => PRIME7BYTES << 8,
        8 => PRIME8BYTES,
        _ => (PRIME4BYTES as u64) << 32,
    }
}

/// The inner loop of `ZSTD_compressBlock_fast_noDict_generic` (two
/// positions per iteration, see the libzstd source): writes each searched
/// position to the table and returns at the first repcode or table match,
/// or once the new `ip2` reaches `lim`.
///
/// # Safety
/// The invariants of `no_dict_generic`: at entry `ip3 = ip2 + 1 <
/// ilimit`, and `lim <= ilimit - 1` (so every iteration's `ip3` stays below
/// `ilimit`); the index of `ip0` is at least `s.low`; `ip2 - rep_offset1`
/// is readable; the table has `1 << hlog` entries.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn fast_search_rust<const MLS: u32, const USE_CMOV: bool>(
    s: &FastSearch,
    c: &mut FastCursor,
    rep_offset1: u32,
) -> u32 {
    static DUMMY: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
    let chk = |p: *const u8, n: usize| {
        debug_assert!(
            (p as usize) >= (s.lowest as usize) && (p as usize) + n <= (s.iend as usize),
            "read out of the window"
        );
    };
    let base = s.base;
    let low = s.low as u32;
    let ht = s.ht;
    let idx = |p: *const u8| (p as usize).wrapping_sub(base as usize) as u32;
    // SAFETY: per the function contract; table candidates are read only
    // when their index is at least `s.low`.
    unsafe {
        let hash = |p: *const u8| {
            chk(p, HASH_READ_SIZE);
            hash_with(p, hash_prime::<MLS>(), s.shift)
        };
        let match_found = |cur: *const u8, match_idx: u32| -> bool {
            if USE_CMOV {
                // ZSTD_match4Found_cmov
                let valid = match_idx >= low;
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
                let mval = if match_idx >= low {
                    let m = base.wrapping_add(match_idx as usize);
                    chk(m, 4);
                    read32(m)
                } else {
                    read32(cur) ^ 1
                };
                read32(cur) == mval
            }
        };

        let FastCursor {
            mut ip0,
            mut ip2,
            mut hash0,
            mut hash1,
            match_idx,
        } = *c;
        let mut match_idx = match_idx as u32;
        let (step, lim) = (s.step, s.lim);
        loop {
            // load repcode match for ip[2]
            let rep_ptr = ip2.wrapping_sub(rep_offset1 as usize);
            chk(rep_ptr, 4);
            let rval = read32(rep_ptr);

            // write back hash table entry
            *ht.add(hash0) = idx(ip0);

            // check repcode at ip[2]
            if (read32(ip2) == rval) & (rep_offset1 > 0) {
                *c = FastCursor {
                    ip0,
                    ip2,
                    hash0,
                    hash1,
                    match_idx: u64::from(match_idx),
                };
                return FAST_REP;
            }

            if match_found(ip0, match_idx) {
                *c = FastCursor {
                    ip0,
                    ip2,
                    hash0,
                    hash1,
                    match_idx: u64::from(match_idx),
                };
                return FAST_MATCH0;
            }

            // lookup ip[1]
            match_idx = *ht.add(hash1);

            // hash ip[2]
            hash0 = hash1;
            hash1 = hash(ip2);

            // advance to next positions: ip0 = ip1, ip1 = ip2, ip2 = ip3
            ip0 = ip0.wrapping_add(1);

            // write back hash table entry
            *ht.add(hash0) = idx(ip0);

            if match_found(ip0, match_idx) {
                *c = FastCursor {
                    ip0,
                    ip2,
                    hash0,
                    hash1,
                    match_idx: u64::from(match_idx),
                };
                return FAST_MATCH1;
            }

            // lookup ip[1]
            match_idx = *ht.add(hash1);

            // hash ip[2] (= ip3 = ip2 + 1)
            hash0 = hash1;
            hash1 = hash(ip2.wrapping_add(1));

            // advance to next positions
            ip0 = ip2;
            ip2 = ip0.wrapping_add(step);

            if ip2 as usize >= lim {
                *c = FastCursor {
                    ip0,
                    ip2,
                    hash0,
                    hash1,
                    match_idx: u64::from(match_idx),
                };
                return FAST_LIMIT;
            }
        }
    }
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
    const K_STEP_INCR: usize = 1 << (K_SEARCH_STRENGTH - 1);

    let cparams = ms.cparams;
    let hlog = cparams.hash_log;
    // These checks make the raw-pointer accesses below sound.
    assert!(start <= end && end <= buf.len());
    assert!(ms.hash_table.len() == 1usize << hlog && hlog <= 32);
    // With an extDict, only through the extDict variant when the window has
    // moved past it: every read is then in the prefix.
    let window_start = ms.window.pos(ms.window.dict_limit);
    assert!(window_start <= start, "window start is not in the buffer");
    assert!(end.wrapping_sub(ms.window.base) <= u32::MAX as usize);
    // Room for the sequences stored with `store_seq_unchecked`.
    ss.reserve(end - start);

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
        let mut s = FastSearch {
            prime: hash_prime::<MLS>(),
            step: step_size,
            lim: 0,
            base,
            ht,
            low: u64::from(prefix_start_index),
            shift: u64::from(64 - hlog),
            lowest,
            iend,
        };

        'start: loop {
            // The rest of the block in asm once the first repcode is valid
            // (the asm has no `rep_offset1 == 0` case): the whole loop,
            // match handling included, see `block_asm`.
            #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
            if rep_offset1 != 0 && crate::cpu::Features::detect().bmi2 {
                let lit_base = ss.lit_buf.as_mut_ptr();
                let seq_base = ss.sequences.as_mut_ptr();
                let mut ctx = super::block_asm::FastCtx {
                    prime: hash_prime::<MLS>(),
                    step_size,
                    step: step_size,
                    lim: 0,
                    next_step: 0,
                    anchor,
                    iend,
                    ilimit,
                    prefix_start,
                    lit_limit_w: (iend as usize)
                        .saturating_sub(super::seq_store::WILDCOPY_OVERLENGTH),
                    lit: lit_base.add(ss.lit_len),
                    seq: seq_base.add(ss.nb_seq),
                    rep1: u64::from(rep_offset1),
                    rep2: u64::from(rep_offset2),
                    llt: 0,
                    llpos: core::ptr::null_mut(),
                };
                // SAFETY: the invariants of this function at `_start` (see
                // the docs), `ss.reserve(end - start)` above, and the CPU has
                // BMI1/BMI2 (`Features::detect`).
                super::block_asm::fast_block(
                    &mut ctx,
                    ip0,
                    ht,
                    base,
                    u64::from(prefix_start_index),
                    0usize.wrapping_sub(rep_offset1 as usize),
                    u64::from(64 - hlog),
                );
                anchor = ctx.anchor;
                rep_offset1 = ctx.rep1 as u32;
                rep_offset2 = ctx.rep2 as u32;
                debug_assert!(istart <= anchor && anchor <= iend);
                debug_assert!(rep_offset1 != 0 && (ctx.rep1 >> 32) == 0 && (ctx.rep2 >> 32) == 0);
                debug_assert!(ctx.lit >= lit_base && ctx.seq >= seq_base);
                ss.lit_len = ctx.lit as usize - lit_base as usize;
                ss.nb_seq = (ctx.seq as usize - seq_base as usize)
                    / core::mem::size_of::<super::seq_store::SeqDef>();
                debug_assert!(ss.lit_len <= ss.max_nb_lit && ss.nb_seq <= ss.max_nb_seq);
                if ctx.llt != 0 {
                    debug_assert!(ss.long_length_type == super::seq_store::LongLengthType::None);
                    ss.long_length_type = if ctx.llt == 1 {
                        super::seq_store::LongLengthType::LiteralLength
                    } else {
                        super::seq_store::LongLengthType::MatchLength
                    };
                    ss.long_length_pos = ((ctx.llpos as usize - seq_base as usize)
                        / core::mem::size_of::<super::seq_store::SeqDef>())
                        as u32;
                }
                break 'start;
            }

            let mut step = step_size;
            let mut next_step = ip0.wrapping_add(K_STEP_INCR);
            let ip1 = ip0.wrapping_add(1);
            let ip2 = ip0.wrapping_add(step);
            let ip3 = ip2.wrapping_add(1);

            if ip3 >= ilimit {
                break 'start;
            }
            // One limit for the two end-of-iteration tests (`ip2 >=
            // nextStep`, and `ip3 >= ilimit`).
            s.lim = (next_step as usize).min(ilimit as usize - 1);
            s.step = step;

            let hash0 = hash(ip0);
            let mut cur = FastCursor {
                ip0,
                ip2,
                hash0,
                hash1: hash(ip1),
                match_idx: u64::from(*ht.add(hash0)),
            };

            let current0: u32;
            let mut match0: *const u8;
            let mut m_length: usize;
            let offcode: u32;

            loop {
                // SAFETY: the invariants above (`ip3 < ilimit`, `lim <=
                // ilimit - 1`, the window reads debug-checked).
                let exit = fast_search_rust::<MLS, USE_CMOV>(&s, &mut cur, rep_offset1);
                if exit == FAST_LIMIT {
                    // calculate step
                    let ip2 = cur.ip2;
                    if ip2 >= next_step {
                        step += 1;
                        prefetch_l1(cur.ip0.wrapping_add(1 + 64));
                        prefetch_l1(cur.ip0.wrapping_add(1 + 128));
                        next_step = next_step.wrapping_add(K_STEP_INCR);
                    }
                    if ip2.wrapping_add(1) >= ilimit {
                        break 'start;
                    }
                    s.lim = (next_step as usize).min(ilimit as usize - 1);
                    s.step = step;
                    continue;
                }
                ip0 = cur.ip0;
                current0 = idx(ip0);
                if exit == FAST_REP {
                    let ip1 = ip0.wrapping_add(1);
                    ip0 = cur.ip2;
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
                    *ht.add(cur.hash1) = idx(ip1);
                } else {
                    if exit == FAST_MATCH0 {
                        // Write next hash table entry (it's already
                        // calculated).
                        *ht.add(cur.hash1) = idx(ip0.wrapping_add(1));
                    } else if step <= 4 {
                        // FAST_MATCH1: write next hash table entry, since
                        // it's already calculated, unless search resumes at
                        // or before it.
                        *ht.add(cur.hash1) = idx(cur.ip2);
                    }
                    (match0, m_length, offcode) = offset_match(
                        base,
                        cur.match_idx as u32,
                        &mut ip0,
                        anchor,
                        prefix_start,
                        &mut rep_offset1,
                        &mut rep_offset2,
                    );
                }
                break;
            }

            // _match: count the forward length.
            chk(match0, 0);
            m_length += count(ip0.add(m_length), match0.add(m_length), iend);

            ss.store_seq_unchecked(
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
                        ss.store_seq_unchecked(
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

/// `ZSTD_compressBlock_fast_extDict()`: as [`compress_block_fast`], for a
/// window with an extDict segment (a stream whose input buffer wrapped,
/// see the `stream` module). Picks `ZSTD_compressBlock_fast_extDict_<mls>_0`
/// like libzstd (3 and other values use 4).
#[inline(never)]
pub(crate) fn compress_block_fast_ext_dict(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    debug_assert!(ms.cparams.strategy == Strategy::Fast);
    match ms.cparams.min_match {
        5 => ext_dict_generic::<5>(ms, ss, rep, buf, start, end),
        6 => ext_dict_generic::<6>(ms, ss, rep, buf, start, end),
        7 => ext_dict_generic::<7>(ms, ss, rep, buf, start, end),
        _ => ext_dict_generic::<4>(ms, ss, rep, buf, start, end),
    }
}

/// The extDict segment and the prefix of a block's window, as raw
/// pointers (the locals of libzstd's `_extDict` block compressors), after
/// the checks that make reading them sound: both segments are in the
/// buffer.
pub(crate) struct ExtWindow {
    pub(crate) base: *const u8,
    pub(crate) dict_base: *const u8,
    /// `dictStartIndex` (the lowest valid index) and `prefixStartIndex`.
    pub(crate) dict_start_index: u32,
    pub(crate) prefix_start_index: u32,
    pub(crate) dict_start: *const u8,
    /// `dictEnd`: `dictBase + prefixStartIndex`.
    pub(crate) dict_end: *const u8,
    pub(crate) prefix_start: *const u8,
    pub(crate) istart: *const u8,
    pub(crate) iend: *const u8,
}

impl ExtWindow {
    /// The window of the block `buf[start..end]` with the extDict starting
    /// at `dict_start_index` and the prefix at `prefix_start_index`; the
    /// extDict is in `ext` (see [`MatchState::ext_buf`]).
    ///
    /// # Panics
    ///
    /// If a segment is not in its buffer, or the block not in the prefix.
    pub(crate) fn new(
        w: &Window,
        buf: &[u8],
        ext: &[u8],
        start: usize,
        end: usize,
        dict_start_index: u32,
        prefix_start_index: u32,
    ) -> Self {
        assert!(start <= end && end <= buf.len());
        assert!(dict_start_index <= prefix_start_index);
        let prefix_start = w.pos(prefix_start_index);
        assert!(prefix_start <= start, "prefix start is not in the buffer");
        assert!(end.wrapping_sub(w.base) <= u32::MAX as usize);
        let dict_start = w.dict_base.wrapping_add(dict_start_index as usize);
        let dict_end = w.dict_base.wrapping_add(prefix_start_index as usize);
        assert!(
            dict_start <= dict_end && dict_end <= ext.len(),
            "extDict is not in the buffer"
        );
        let bp = buf.as_ptr();
        let ep = ext.as_ptr();
        ExtWindow {
            base: bp.wrapping_add(w.base),
            dict_base: ep.wrapping_add(w.dict_base),
            dict_start_index,
            prefix_start_index,
            dict_start: ep.wrapping_add(dict_start),
            dict_end: ep.wrapping_add(dict_end),
            prefix_start: bp.wrapping_add(prefix_start),
            istart: bp.wrapping_add(start),
            iend: bp.wrapping_add(end),
        }
    }

    /// The index of prefix position `p`.
    #[inline(always)]
    pub(crate) fn idx(&self, p: *const u8) -> u32 {
        (p as usize).wrapping_sub(self.base as usize) as u32
    }

    /// The position of index `i`: in the extDict below `prefixStartIndex`.
    #[inline(always)]
    pub(crate) fn pos(&self, i: u32) -> *const u8 {
        if i < self.prefix_start_index {
            self.dict_base.wrapping_add(i as usize)
        } else {
            self.base.wrapping_add(i as usize)
        }
    }

    /// The end of the segment of index `i` (`dictEnd` or `iend`).
    #[inline(always)]
    pub(crate) fn seg_end(&self, i: u32) -> *const u8 {
        if i < self.prefix_start_index {
            self.dict_end
        } else {
            self.iend
        }
    }

    /// The start of the segment of index `i` (`dictStart` or `prefixStart`).
    #[inline(always)]
    pub(crate) fn seg_start(&self, i: u32) -> *const u8 {
        if i < self.prefix_start_index {
            self.dict_start
        } else {
            self.prefix_start
        }
    }

    /// Debug check that `n` bytes at `p` are inside one segment.
    #[inline(always)]
    pub(crate) fn chk(&self, p: *const u8, n: usize) {
        let (p, e) = (p as usize, (p as usize).wrapping_add(n));
        debug_assert!(
            (p >= self.prefix_start as usize && e <= self.iend as usize)
                || (p >= self.dict_start as usize && e <= self.dict_end as usize),
            "read out of the window"
        );
    }

    /// `ZSTD_count_2segments()` from `ip` against `m`, whose segment ends
    /// at `m_end`.
    ///
    /// # Safety
    /// `ip..iend` and `m..m_end` are in the window.
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(crate) unsafe fn count2(&self, ip: *const u8, m: *const u8, m_end: *const u8) -> usize {
        self.chk(ip, 0);
        self.chk(m, 0);
        // SAFETY: per the caller; the second segment is the prefix.
        unsafe { count_2segments(ip, m, self.iend, m_end, self.prefix_start) }
    }
}

/// `ZSTD_compressBlock_fast_extDict_generic()`.
///
/// Memory safety: `ExtWindow::new` checks that both segments are in
/// `buf`. Table candidates are read only at or above `dictStartIndex`,
/// from the segment of their index; repcodes are read when at least 4
/// bytes below `prefixStartIndex` or in the prefix (`ZSTD_index_overlap_check`)
/// and above `dictStartIndex` (checked at entry, later real offsets);
/// matches are counted up to the end of their segment, then on in the
/// prefix (`ZSTD_count_2segments`). Debug builds check reads with `chk`.
#[allow(unsafe_code)]
#[inline(always)]
fn ext_dict_generic<const MLS: u32>(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    const K_STEP_INCR: usize = 1 << (K_SEARCH_STRENGTH - 1);

    let cparams = ms.cparams;
    let hlog = cparams.hash_log;
    assert!(ms.hash_table.len() == 1usize << hlog && hlog <= 32);
    // support stepSize of 0
    let step_size = cparams.target_length as usize + (cparams.target_length == 0) as usize + 1;
    let end_index = ms.window.index(end);
    let low_limit = ms.lowest_match_index(end_index, cparams.window_log);
    let dict_start_index = low_limit;
    let dict_limit = ms.window.dict_limit;
    let prefix_start_index = dict_limit.max(low_limit);

    // switch to "regular" variant if extDict is invalidated due to maxDistance
    if prefix_start_index == dict_start_index {
        return compress_block_fast(ms, ss, rep, buf, start, end);
    }

    // These checks make the raw-pointer accesses below sound.
    let w = ExtWindow::new(
        &ms.window,
        buf,
        ms.ext_buf(buf),
        start,
        end,
        dict_start_index,
        prefix_start_index,
    );
    let bp = buf.as_ptr();
    let base = w.base;
    let istart = w.istart;
    let iend = w.iend;
    let ilimit = iend.wrapping_sub(8);
    let prefix_start = w.prefix_start;
    let ht = ms.hash_table.as_mut_ptr();
    let mut offset_1 = rep[0];
    let mut offset_2 = rep[1];
    let mut offset_saved1 = 0u32;
    let mut offset_saved2 = 0u32;

    let mut ip0 = istart;
    let mut anchor = istart;

    {
        let curr = w.idx(ip0);
        let max_rep = curr.wrapping_sub(dict_start_index);
        if offset_2 >= max_rep {
            offset_saved2 = offset_2;
            offset_2 = 0;
        }
        if offset_1 >= max_rep {
            offset_saved1 = offset_1;
            offset_1 = 0;
        }
    }

    // SAFETY (whole block): see the function docs. Hash table writes use
    // hashes of `hlog` bits into a table of `1 << hlog` entries.
    unsafe {
        let hash = |p: *const u8| {
            w.chk(p, HASH_READ_SIZE);
            hash_mls::<MLS>(p, hlog)
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
            let mut idx = *ht.add(hash0);

            let current0: u32;
            let mut match0: *const u8;
            let mut m_length: usize;
            let offcode: u32;
            let match_end: *const u8;

            'search: loop {
                {
                    // load repcode match for ip[2]
                    let current2 = w.idx(ip2);
                    let rep_index = current2.wrapping_sub(offset_1);
                    let rep_match = w.pos(rep_index);
                    let rval = if (prefix_start_index.wrapping_sub(rep_index) >= 4) & (offset_1 > 0)
                    {
                        w.chk(rep_match, 4);
                        read32(rep_match)
                    } else {
                        read32(ip2) ^ 1 // guaranteed to not match
                    };

                    // write back hash table entry
                    let c0 = w.idx(ip0);
                    *ht.add(hash0) = c0;

                    // check repcode at ip[2]
                    if read32(ip2) == rval {
                        ip0 = ip2;
                        match0 = rep_match;
                        match_end = w.seg_end(rep_index);
                        debug_assert!(match0 != prefix_start && match0 != w.dict_start);
                        w.chk(match0.wrapping_sub(1), 1);
                        m_length = (*ip0.sub(1) == *match0.sub(1)) as usize;
                        ip0 = ip0.wrapping_sub(m_length);
                        match0 = match0.wrapping_sub(m_length);
                        offcode = REPCODE1_TO_OFFBASE;
                        m_length += 4;
                        current0 = c0;
                        break 'search;
                    }

                    // load match for ip[0], check it
                    if idx >= dict_start_index && {
                        let m = w.pos(idx);
                        w.chk(m, 4);
                        read32(m) == read32(ip0)
                    } {
                        current0 = c0;
                        (match0, m_length, offcode, match_end) = ext_offset_match(
                            &w,
                            idx,
                            c0,
                            &mut ip0,
                            anchor,
                            &mut offset_1,
                            &mut offset_2,
                        );
                        break 'search;
                    }
                }

                // lookup ip[1]
                idx = *ht.add(hash1);

                // hash ip[2]
                hash0 = hash1;
                hash1 = hash(ip2);

                // advance to next positions
                ip0 = ip1;
                ip1 = ip2;
                ip2 = ip3;

                // write back hash table entry
                let c0 = w.idx(ip0);
                *ht.add(hash0) = c0;

                // load match for ip[0], check it
                if idx >= dict_start_index && {
                    let m = w.pos(idx);
                    w.chk(m, 4);
                    read32(m) == read32(ip0)
                } {
                    current0 = c0;
                    (match0, m_length, offcode, match_end) = ext_offset_match(
                        &w,
                        idx,
                        c0,
                        &mut ip0,
                        anchor,
                        &mut offset_1,
                        &mut offset_2,
                    );
                    break 'search;
                }

                // lookup ip[1]
                idx = *ht.add(hash1);

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
            m_length += w.count2(ip0.add(m_length), match0.add(m_length), match_end);

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

            // write next hash table entry
            if ip1 < ip0 {
                *ht.add(hash1) = w.idx(ip1);
            }

            // Fill table and check for immediate repcode.
            if ip0 <= ilimit {
                // Fill Table
                debug_assert!(base.wrapping_add(current0 as usize + 2) > istart);
                *ht.add(hash(base.wrapping_add(current0 as usize + 2))) = current0 + 2;
                *ht.add(hash(ip0.sub(2))) = w.idx(ip0.sub(2));

                while ip0 <= ilimit {
                    let rep_index2 = w.idx(ip0).wrapping_sub(offset_2);
                    let rep_match2 = w.pos(rep_index2);
                    if (index_overlap_check(prefix_start_index, rep_index2) & (offset_2 > 0)) && {
                        w.chk(rep_match2, 4);
                        read32(rep_match2) == read32(ip0)
                    } {
                        let rep_end2 = w.seg_end(rep_index2);
                        let rep_length2 = w.count2(ip0.add(4), rep_match2.add(4), rep_end2) + 4;
                        core::mem::swap(&mut offset_1, &mut offset_2);
                        ss.store_seq(
                            buf,
                            anchor as usize - bp as usize,
                            0,
                            end,
                            REPCODE1_TO_OFFBASE,
                            rep_length2,
                        );
                        *ht.add(hash(ip0)) = w.idx(ip0);
                        ip0 = ip0.add(rep_length2);
                        anchor = ip0;
                        continue;
                    }
                    break;
                }
            }
        }
    }

    // _cleanup: if offset_1 started invalid and became valid, rotate the
    // saved offsets.
    if offset_saved1 != 0 && offset_1 != 0 {
        offset_saved2 = offset_saved1;
    }
    rep[0] = if offset_1 != 0 {
        offset_1
    } else {
        offset_saved1
    };
    rep[1] = if offset_2 != 0 {
        offset_2
    } else {
        offset_saved2
    };

    // Return the last literals size
    iend as usize - anchor as usize
}

/// `ZSTD_index_overlap_check()`: whether a 4-byte read at `rep_index`
/// stays in one segment (at least 4 bytes below `prefix_lowest_index`, or
/// in the prefix).
#[inline(always)]
pub(crate) fn index_overlap_check(prefix_lowest_index: u32, rep_index: u32) -> bool {
    prefix_lowest_index.wrapping_sub(1).wrapping_sub(rep_index) >= 3
}

/// The `_offset:` label of `ZSTD_compressBlock_fast_extDict_generic`:
/// turns a hash-table hit `idx` at `ip0` (index `current0`) into a match,
/// extending it backwards within its segment. Returns `(match0, mLength,
/// offcode, matchEnd)` and moves `ip0` back.
///
/// # Safety
/// `idx >= dictStartIndex` and `idx < current0`.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn ext_offset_match(
    w: &ExtWindow,
    idx: u32,
    current0: u32,
    ip0: &mut *const u8,
    anchor: *const u8,
    offset_1: &mut u32,
    offset_2: &mut u32,
) -> (*const u8, usize, u32, *const u8) {
    let offset = current0 - idx;
    let low_match_ptr = w.seg_start(idx);
    let match_end = w.seg_end(idx);
    let mut match0 = w.pos(idx);
    *offset_2 = *offset_1;
    *offset_1 = offset;
    let offcode = offset_to_offbase(offset);
    let mut m_length = 4;
    // Count the backwards match length.
    // SAFETY: `ip0 > anchor >= istart` and `match0 > low_match_ptr` (the
    // start of its segment) before each read of the byte before them.
    unsafe {
        while ((*ip0 > anchor) & (match0 > low_match_ptr)) && *ip0.sub(1) == *match0.sub(1) {
            *ip0 = ip0.sub(1);
            match0 = match0.sub(1);
            m_length += 1;
        }
    }
    (match0, m_length, offcode, match_end)
}

/// The attached dictionary's match state (libzstd's `dictMatchState`) as
/// the `_dictMatchState` match finders see it: raw pointers into the
/// [`CDict`](super::cdict::CDict)'s content and tables, after the checks
/// that make reading them sound.
pub(crate) struct DmsView {
    /// `dms->window.base`: the position of the dictionary's index 0.
    pub(crate) base: *const u8,
    pub(crate) hash_table: *const u32,
    pub(crate) chain_table: *const u32,
    pub(crate) tag_table: *const u8,
    /// `dms->cParams.hashLog`, `chainLog`, and `dms->rowHashLog`.
    pub(crate) hash_log: u32,
    pub(crate) chain_log: u32,
    pub(crate) row_hash_log: u32,
    /// `dms->window.dictLimit` and `lowLimit` (equal for a CDict: its
    /// content is one segment).
    pub(crate) dict_limit: u32,
    pub(crate) low_limit: u32,
    /// `dms->window.nextSrc`: the end of the content, and its index
    /// (`dmsSize`, `dictHighLimit`).
    pub(crate) end: *const u8,
    pub(crate) high_limit: u32,
    /// The first readable byte: `base + lowLimit`.
    pub(crate) start: *const u8,
}

impl DmsView {
    /// The view of `cd`'s match state.
    ///
    /// # Panics
    ///
    /// If its window is not in its content, or a table has not the size
    /// of its parameters.
    pub(crate) fn new(cd: &super::cdict::CDict) -> Self {
        let ms = &cd.ms;
        let w = &ms.window;
        let cp = &ms.cparams;
        let content = cd.content.as_ptr();
        assert!(w.low_limit <= w.dict_limit);
        let start = w.pos(w.low_limit);
        let end = w.next_src;
        assert!(
            start <= end && end <= cd.content.len(),
            "dictionary window is not in its content"
        );
        assert!(cp.hash_log + SHORT_CACHE_TAG_BITS <= 32);
        assert!(ms.hash_table.len() == 1usize << cp.hash_log);
        assert!(ms.chain_table.is_empty() || ms.chain_table.len() == 1usize << cp.chain_log);
        assert!(ms.tag_table.is_empty() || ms.tag_table.len() == ms.hash_table.len());
        DmsView {
            base: content.wrapping_add(w.base),
            hash_table: ms.hash_table.as_ptr(),
            chain_table: ms.chain_table.as_ptr(),
            tag_table: ms.tag_table.as_ptr(),
            hash_log: cp.hash_log,
            chain_log: cp.chain_log,
            row_hash_log: ms.row_hash_log,
            dict_limit: w.dict_limit,
            low_limit: w.low_limit,
            end: content.wrapping_add(end),
            high_limit: w.index(end),
            start: content.wrapping_add(start),
        }
    }

    /// Debug check that `n` bytes at `p` are in the dictionary's content.
    #[inline(always)]
    pub(crate) fn chk(&self, p: *const u8, n: usize) {
        let (p, e) = (p as usize, (p as usize).wrapping_add(n));
        debug_assert!(
            p >= self.start as usize && e <= self.end as usize,
            "read out of the dictionary"
        );
    }
}

/// `ZSTD_comparePackedTags()`: whether the tags (low 8 bits) match.
#[inline(always)]
pub(crate) fn compare_packed_tags(packed_tag1: usize, packed_tag2: usize) -> bool {
    (packed_tag1 as u32 & SHORT_CACHE_TAG_MASK) == (packed_tag2 as u32 & SHORT_CACHE_TAG_MASK)
}

/// `ZSTD_compressBlock_fast_dictMatchState()`: as [`compress_block_fast`],
/// with an attached dictionary ([`MatchState::dms`]) whose tagged hash
/// table is searched where the window's has no candidate. Picks
/// `ZSTD_compressBlock_fast_dictMatchState_<mls>_0` like libzstd (3 and
/// other values use 4).
#[inline(never)]
pub(crate) fn compress_block_fast_dict_match_state(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    debug_assert!(ms.cparams.strategy == Strategy::Fast);
    match ms.cparams.min_match {
        5 => dms_generic::<5>(ms, ss, rep, buf, start, end),
        6 => dms_generic::<6>(ms, ss, rep, buf, start, end),
        7 => dms_generic::<7>(ms, ss, rep, buf, start, end),
        _ => dms_generic::<4>(ms, ss, rep, buf, start, end),
    }
}

/// `ZSTD_compressBlock_fast_dictMatchState_generic()`.
///
/// Memory safety: the window is one segment in `buf` (no extDict:
/// `ZSTD_matchState_dictMode`) from `prefixStart`, the dictionary's content
/// another, checked by [`DmsView::new`]. Window candidates are read only at
/// or above `prefixStartIndex`, dictionary candidates only above its start
/// (`dictMatchIndex > dictStartIndex`, the tagged index of a hashed
/// position, at least 8 bytes before its end); repcodes are at most
/// `dictAndPrefixLength` at entry (asserted), so their index maps into the
/// dictionary or the prefix, and `ZSTD_index_overlap_check` keeps 4-byte
/// reads in one segment. Matches are counted to the end of their segment,
/// then on in the prefix (`ZSTD_count_2segments`). Debug builds check reads
/// with `chk`.
#[allow(unsafe_code)]
#[inline(always)]
fn dms_generic<const MLS: u32>(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    const K_STEP_INCR: usize = 1 << K_SEARCH_STRENGTH;
    let cparams = ms.cparams;
    let hlog = cparams.hash_log;
    // These checks make the raw-pointer accesses below sound.
    assert!(start <= end && end <= buf.len());
    assert!(ms.hash_table.len() == 1usize << hlog && hlog <= 32);
    assert!(!ms.window.has_ext_dict());
    let window_start = ms.window.pos(ms.window.dict_limit);
    assert!(window_start <= start, "window start is not in the buffer");
    assert!(end.wrapping_sub(ms.window.base) <= u32::MAX as usize);
    let cd = ms.dict.clone().expect("attached dictionary");
    let d = DmsView::new(&cd);
    debug_assert_eq!(ms.loaded_dict_end, ms.window.dict_limit);

    // support stepSize of 0
    let step_size = cparams.target_length as usize + (cparams.target_length == 0) as usize;
    let bp = buf.as_ptr();
    let base = bp.wrapping_add(ms.window.base);
    let istart = bp.wrapping_add(start);
    let mut ip0 = istart;
    let mut ip1 = ip0.wrapping_add(step_size);
    let mut anchor = istart;
    let prefix_start_index = ms.window.dict_limit;
    let prefix_start = base.wrapping_add(prefix_start_index as usize);
    let iend = bp.wrapping_add(end);
    let ilimit = iend.wrapping_sub(HASH_READ_SIZE);
    let mut offset_1 = rep[0];
    let mut offset_2 = rep[1];

    let dict_hash_table = d.hash_table;
    let dict_start_index = d.dict_limit;
    let dict_base = d.base;
    let dict_start = dict_base.wrapping_add(dict_start_index as usize);
    let dict_end = d.end;
    // ensure there will be no underflow when translating a dict index into
    // a local index
    assert!(prefix_start_index >= d.high_limit);
    let dict_index_delta = prefix_start_index - d.high_limit;
    let dict_and_prefix_length =
        (start - window_start) as u32 + (dict_end as usize - dict_start as usize) as u32;
    let dict_hbits = d.hash_log + SHORT_CACHE_TAG_BITS;

    let ht = ms.hash_table.as_mut_ptr();
    let idx = |p: *const u8| (p as usize).wrapping_sub(base as usize) as u32;
    // Debug check that `n` bytes at `p` are in the window or the dictionary.
    let chk = |p: *const u8, n: usize| {
        if (p as usize) >= (dict_start as usize) && (p as usize) < (dict_end as usize) {
            d.chk(p, n);
        } else {
            debug_assert!(
                (p as usize) >= (prefix_start as usize) && (p as usize) + n <= (iend as usize),
                "read out of the window"
            );
        }
    };

    // init
    ip0 = ip0.wrapping_add((dict_and_prefix_length == 0) as usize);
    // dictMatchState repCode checks don't currently handle repCode == 0
    // disabling.
    assert!(offset_1 <= dict_and_prefix_length && offset_2 <= dict_and_prefix_length);
    assert!(step_size >= 1);

    // SAFETY (whole block): see the function docs. Hash table writes use
    // hashes of `hlog` bits into a table of `1 << hlog` entries, dictionary
    // reads hashes of `dict_hbits - 8` bits into a table of that size
    // (asserted above and by `DmsView::new`).
    unsafe {
        let hash = |p: *const u8| {
            chk(p, HASH_READ_SIZE);
            hash_mls::<MLS>(p, hlog)
        };
        let dict_hash = |p: *const u8| hash_mls::<MLS>(p, dict_hbits);
        // the position of a repcode's match: in the dictionary below the
        // prefix
        let rep_pos = |rep_index: u32| {
            if rep_index < prefix_start_index {
                dict_base.wrapping_add(rep_index.wrapping_sub(dict_index_delta) as usize)
            } else {
                base.wrapping_add(rep_index as usize)
            }
        };

        // Outer search loop
        'outer: while ip1 <= ilimit {
            // repcode check at (ip0 + 1) is safe because ip0 < ip1
            let m_length: usize;
            let mut hash0 = hash(ip0);

            let dict_hash_and_tag0 = dict_hash(ip0);
            let mut dict_match_index_and_tag =
                *dict_hash_table.add(dict_hash_and_tag0 >> SHORT_CACHE_TAG_BITS);
            let mut dict_tags_match =
                compare_packed_tags(dict_match_index_and_tag as usize, dict_hash_and_tag0);

            let mut match_index = *ht.add(hash0);
            let mut curr = idx(ip0);
            let mut step = step_size;
            let mut next_step = ip0.wrapping_add(K_STEP_INCR);

            // Inner search loop
            loop {
                let mtch = base.wrapping_add(match_index as usize);
                let rep_index = curr.wrapping_add(1).wrapping_sub(offset_1);
                let rep_match = rep_pos(rep_index);
                let hash1 = hash(ip1);
                let dict_hash_and_tag1 = dict_hash(ip1);
                *ht.add(hash0) = curr; // update hash table

                if index_overlap_check(prefix_start_index, rep_index) && {
                    chk(rep_match, 4);
                    read32(rep_match) == read32(ip0.add(1))
                } {
                    let rep_match_end = if rep_index < prefix_start_index {
                        dict_end
                    } else {
                        iend
                    };
                    m_length = count_2segments(
                        ip0.add(1 + 4),
                        rep_match.add(4),
                        iend,
                        rep_match_end,
                        prefix_start,
                    ) + 4;
                    ip0 = ip0.add(1);
                    ss.store_seq(
                        buf,
                        anchor as usize - bp as usize,
                        ip0 as usize - anchor as usize,
                        end,
                        REPCODE1_TO_OFFBASE,
                        m_length,
                    );
                    break;
                }

                if dict_tags_match {
                    // Found a possible dict match
                    let dict_match_index = dict_match_index_and_tag >> SHORT_CACHE_TAG_BITS;
                    let mut dict_match = dict_base.wrapping_add(dict_match_index as usize);
                    if dict_match_index > dict_start_index && {
                        chk(dict_match, 4);
                        read32(dict_match) == read32(ip0)
                    } {
                        // To replicate extDict parse behavior, we only use
                        // dict matches when the normal matchIndex is invalid
                        if match_index <= prefix_start_index {
                            let offset = curr - dict_match_index - dict_index_delta;
                            let mut ml = count_2segments(
                                ip0.add(4),
                                dict_match.add(4),
                                iend,
                                dict_end,
                                prefix_start,
                            ) + 4;
                            while ((ip0 > anchor) & (dict_match > dict_start))
                                && *ip0.sub(1) == *dict_match.sub(1)
                            {
                                ip0 = ip0.sub(1);
                                dict_match = dict_match.sub(1);
                                ml += 1;
                            } // catch up
                            offset_2 = offset_1;
                            offset_1 = offset;
                            m_length = ml;
                            ss.store_seq(
                                buf,
                                anchor as usize - bp as usize,
                                ip0 as usize - anchor as usize,
                                end,
                                offset_to_offbase(offset),
                                m_length,
                            );
                            break;
                        }
                    }
                }

                if match_index >= prefix_start_index && {
                    chk(mtch, 4);
                    read32(ip0) == read32(mtch)
                } {
                    // found a regular match of size >= 4
                    let mut mtch = mtch;
                    let offset = (ip0 as usize - mtch as usize) as u32;
                    let mut ml = count(ip0.add(4), mtch.add(4), iend) + 4;
                    while ((ip0 > anchor) & (mtch > prefix_start)) && *ip0.sub(1) == *mtch.sub(1) {
                        ip0 = ip0.sub(1);
                        mtch = mtch.sub(1);
                        ml += 1;
                    } // catch up
                    offset_2 = offset_1;
                    offset_1 = offset;
                    m_length = ml;
                    ss.store_seq(
                        buf,
                        anchor as usize - bp as usize,
                        ip0 as usize - anchor as usize,
                        end,
                        offset_to_offbase(offset),
                        m_length,
                    );
                    break;
                }

                // Prepare for next iteration
                dict_match_index_and_tag =
                    *dict_hash_table.add(dict_hash_and_tag1 >> SHORT_CACHE_TAG_BITS);
                dict_tags_match =
                    compare_packed_tags(dict_match_index_and_tag as usize, dict_hash_and_tag1);
                match_index = *ht.add(hash1);

                if ip1 >= next_step {
                    step += 1;
                    next_step = next_step.wrapping_add(K_STEP_INCR);
                }
                ip0 = ip1;
                ip1 = ip1.wrapping_add(step);
                if ip1 > ilimit {
                    break 'outer;
                }

                curr = idx(ip0);
                hash0 = hash1;
            } // end inner search loop

            // match found
            debug_assert!(m_length != 0);
            ip0 = ip0.add(m_length);
            anchor = ip0;

            if ip0 <= ilimit {
                // Fill Table
                debug_assert!(base.wrapping_add(curr as usize + 2) > istart);
                *ht.add(hash(base.wrapping_add(curr as usize + 2))) = curr + 2;
                *ht.add(hash(ip0.sub(2))) = idx(ip0.sub(2));

                // check immediate repcode
                while ip0 <= ilimit {
                    let current2 = idx(ip0);
                    let rep_index2 = current2.wrapping_sub(offset_2);
                    let rep_match2 = rep_pos(rep_index2);
                    if index_overlap_check(prefix_start_index, rep_index2) && {
                        chk(rep_match2, 4);
                        read32(rep_match2) == read32(ip0)
                    } {
                        let rep_end2 = if rep_index2 < prefix_start_index {
                            dict_end
                        } else {
                            iend
                        };
                        let rep_length2 = count_2segments(
                            ip0.add(4),
                            rep_match2.add(4),
                            iend,
                            rep_end2,
                            prefix_start,
                        ) + 4;
                        core::mem::swap(&mut offset_1, &mut offset_2);
                        ss.store_seq(
                            buf,
                            anchor as usize - bp as usize,
                            0,
                            end,
                            REPCODE1_TO_OFFBASE,
                            rep_length2,
                        );
                        *ht.add(hash(ip0)) = current2;
                        ip0 = ip0.add(rep_length2);
                        anchor = ip0;
                        continue;
                    }
                    break;
                }
            }

            // Prepare for next iteration
            debug_assert!(ip0 == anchor);
            ip1 = ip0.wrapping_add(step_size);
        }
    }

    // _cleanup: save reps for next block
    rep[0] = offset_1;
    rep[1] = offset_2;

    // Return the last literals size
    iend as usize - anchor as usize
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::params::{CParams, one_shot_cparams};
    use super::super::seq_store::{
        MINMATCH, REP_START_VALUE, SeqStore, offbase_is_offset, offbase_to_offset,
    };
    use super::*;
    use alloc::vec;
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

    /// As [`roundtrip`], but like the streaming compressor: the blocks go
    /// through a ring buffer of `window + block_size` bytes (the `stream`
    /// module's input buffer), so the window becomes non-contiguous (an
    /// extDict) every time the ring wraps. Returns the total number of
    /// sequences and the number of blocks compressed with an extDict.
    pub(crate) fn roundtrip_ring(
        src: &[u8],
        cparams: CParams,
        block_size: usize,
    ) -> (usize, usize) {
        let window = 1usize << cparams.window_log;
        let ring_size = window + block_size;
        let mut ring = alloc::vec![0u8; ring_size + 32];
        let mut ms = MatchState::new(cparams);
        let mut ss = SeqStore::new(block_size, cparams.min_match);
        let mut rep = REP_START_VALUE;
        let mut dec_rep = REP_START_VALUE;
        let mut out = Vec::new();
        let (mut pos, mut at, mut nb_seq, mut ext) = (0, 0, 0, 0);
        while pos < src.len() {
            let bs = block_size.min(src.len() - pos);
            if at + bs > ring_size {
                at = 0;
            }
            ring[at..at + bs].copy_from_slice(&src[pos..pos + bs]);
            ms.update_window(at, bs);
            ms.prepare_block(at, bs);
            ext += usize::from(ms.window.has_ext_dict());
            let mut next_rep = rep;
            if build_seq_store(&mut ms, &mut ss, &mut next_rep, &ring, at, at + bs) {
                replay(&ss, &mut dec_rep, &mut out);
                rep = next_rep;
                nb_seq += ss.nb_seq;
                assert_eq!(&dec_rep[..2], &rep[..2]);
            } else {
                out.extend_from_slice(&src[pos..pos + bs]);
            }
            assert_eq!(&out[..], &src[..pos + bs], "block at {pos}");
            pos += bs;
            at += bs;
        }
        (nb_seq, ext)
    }

    #[test]
    fn roundtrip_ring_variants() {
        let mut src = texty(150_000, 7);
        src.extend_from_slice(&random(5000, 8));
        src.extend_from_slice(&src.clone()[..60_000]);
        src.extend_from_slice(&texty(100_000, 17));
        for mls in 3..=8 {
            for (window_log, bs) in [(10, 1 << 10), (14, 3000), (16, 1 << 15), (17, 1 << 17)] {
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
                    let (n, ext) = roundtrip_ring(&src, cp, bs);
                    assert!(
                        n > 1000 && ext > 0,
                        "mls {mls} wlog {window_log}: {n} {ext}"
                    );
                }
            }
        }
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
        w.enforce_max_dist(1 << 19, 1 << 17, &mut lde, &mut false);
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

    #[test]
    fn keep_indices() {
        let src = texty(1000, 11);
        let cp = one_shot_cparams(1, 1000);
        let mut ms = MatchState::new(cp);
        ms.update_window(0, src.len());
        ms.fill_hash_table(&src, src.len(), TableLoadMethod::Fast);
        let filled = ms.hash_table.iter().filter(|&&v| v != 0).count();
        assert!(filled > 0);
        // same parameters: the indices continue past the first frame, the
        // table is kept
        ms.reset_keep_indices(cp, false);
        assert!(!ms.window.has_ext_dict());
        assert_eq!(ms.window.dict_limit, 1002);
        assert_eq!(ms.next_to_update, 1002);
        assert_eq!(ms.hash_table.iter().filter(|&&v| v != 0).count(), filled);
        ms.update_window(0, src.len());
        assert_eq!(ms.window.index(0), 1002);
        assert_eq!(ms.window.dict_limit, 1002);
        assert!(!ms.window.has_ext_dict());
        // other parameters: a reset
        let cp2 = one_shot_cparams(3, 1000);
        ms.reset_keep_indices(cp2, false);
        assert_eq!(ms.window, Window::new());
        assert!(ms.hash_table.iter().all(|&v| v == 0));
        // indices close to the maximum: a reset
        ms.reset_keep_indices(cp, false);
        ms.window.next_src = ms
            .window
            .base
            .wrapping_add((CURRENT_MAX - INDEXOVERFLOW_MARGIN) as usize + 1);
        ms.hash_table[0] = 7;
        ms.reset_keep_indices(cp, false);
        assert_eq!(ms.window, Window::new());
        assert_eq!(ms.hash_table[0], 0);
    }
}
