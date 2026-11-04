//! The lazy strategies (greedy, lazy, lazy2 and btlazy2: levels 4/5 to
//! 12/15 depending on the input size): a port of libzstd 1.5.7's
//! `compress/zstd_lazy.c`, noDict and extDict paths. The parser
//! (`ZSTD_compressBlock_lazy_generic`, or `_extDict_generic`) is shared;
//! the match finders are the hash chain (`ZSTD_HcFindBestMatch`, here), the
//! binary tree of btlazy2 (`ZSTD_DUBT_findBestMatch`, here) and the
//! row-based finder (`ZSTD_RowFindBestMatch`, in [`row`](super::row)),
//! which libzstd picks for greedy to lazy2 when `windowLog > 14`.
//!
//! Like libzstd's templates, the parser is monomorphized on the search
//! method, the parse depth (0 greedy, 1 lazy, 2 lazy2 and btlazy2), `mls`
//! (`minMatch` bounded to 4..=6) and, for the row finder, `rowLog`
//! (`searchLog` bounded to 4..=6), and the search functions on `EXT`
//! (extDict or not); they are not inlined (`FORCE_NOINLINE` in libzstd).
//!
//! It shares the match state, window and block loop with `fast` (see that
//! module's docs). Table use: `hashTable` holds `minMatch`-byte hashes
//! (row: rows of `1 << rowLog` indices), `chainTable` the hash chains
//! (`1 << chainLog` entries) or btlazy2's binary tree (pairs of entries,
//! `1 << (chainLog - 1)` nodes), `tagTable` the row finder's tags.
//!
//! # Memory safety
//!
//! The search runs on raw pointers, like libzstd. Every index stored in
//! the tables is that of a position already passed by the parser (the
//! parser's search positions only increase, and a stored match always ends
//! after the last searched position), so a candidate index that passes the
//! `>= lowLimit` check is a position in the window before the current one:
//! in the prefix at or above `dictLimit`, in the extDict below it (only
//! with an extDict, whose search functions take `EXT` and read it through
//! `dictBase`; both segments are checked to be in the buffer at entry). All forward reads stop
//! at the block end (`iend`); hashing reads 8 bytes at positions at most
//! `iend - 8` (`ilimit`). Debug builds check every read with `chk`.

use super::fast::{ExtWindow, K_SEARCH_STRENGTH, MatchState, hash_mls, index_overlap_check};
use super::params::Strategy;
use super::row::{self, ROW_HASH_CACHE_SIZE};
use super::seq_store::{
    REP_NUM, REPCODE1_TO_OFFBASE, SeqStore, count, count_2segments, highbit32, read32,
};

/// `ZSTD_DUBT_UNSORTED_MARK`: marks a binary-tree node not sorted yet.
/// Real indices start at `ZSTD_WINDOW_START_INDEX` (2).
pub(crate) const DUBT_UNSORTED_MARK: u32 = 1;
/// `kLazySkippingStep`.
const LAZY_SKIPPING_STEP: usize = 8;
/// The initial "offset found" of a search: libzstd's `999999999`, which
/// enters the gain heuristics through `ZSTD_highbit32`.
const OFFBASE_UNSET: u32 = 999_999_999;

/// `searchMethod_e` values, as const generic arguments.
pub(super) const SEARCH_HASH_CHAIN: u8 = 0;
pub(super) const SEARCH_BINARY_TREE: u8 = 1;
pub(super) const SEARCH_ROW_HASH: u8 = 2;

/// The match state as the search functions see it during one block: raw
/// table pointers, the parameters, and the mutable scalar state (written
/// back to the [`MatchState`] at the end of the block).
pub(super) struct Searcher {
    pub(super) base: *const u8,
    pub(super) hash_table: *mut u32,
    pub(super) chain_table: *mut u32,
    pub(super) tag_table: *mut u8,
    pub(super) hash_log: u32,
    pub(super) chain_log: u32,
    pub(super) row_hash_log: u32,
    pub(super) search_log: u32,
    pub(super) window_log: u32,
    /// `window.lowLimit`
    pub(super) low_limit: u32,
    pub(super) loaded_dict_end: u32,
    pub(super) hash_salt: u64,
    pub(super) next_to_update: u32,
    pub(super) lazy_skipping: bool,
    pub(super) hash_cache: [u32; ROW_HASH_CACHE_SIZE],
    pub(super) hash_salt_entropy: u32,
    /// The window start (the prefix start with an extDict) and the block
    /// end: every read is in between, or in the extDict (debug-checked by
    /// [`Searcher::chk`]).
    pub(super) lowest: *const u8,
    pub(super) iend: *const u8,
    /// The extDict (`window.dictBase`, `window.dictLimit`), its readable
    /// part `dict_lowest..dict_end` (empty without an extDict) and the
    /// prefix start (`base + dictLimit`).
    pub(super) dict_base: *const u8,
    pub(super) dict_limit: u32,
    pub(super) dict_lowest: *const u8,
    pub(super) dict_end: *const u8,
    pub(super) prefix_start: *const u8,
}

impl Searcher {
    /// The index of position `p`.
    #[inline(always)]
    pub(super) fn idx(&self, p: *const u8) -> u32 {
        (p as usize).wrapping_sub(self.base as usize) as u32
    }

    /// The position of index `i`.
    #[inline(always)]
    pub(super) fn pos(&self, i: u32) -> *const u8 {
        self.base.wrapping_add(i as usize)
    }

    /// Debug check that `n` bytes at `p` are inside the readable window.
    #[inline(always)]
    pub(super) fn chk(&self, p: *const u8, n: usize) {
        let (p, e) = (p as usize, (p as usize).wrapping_add(n));
        debug_assert!(
            (p >= self.lowest as usize && e <= self.iend as usize)
                || (p >= self.dict_lowest as usize && e <= self.dict_end as usize),
            "read out of the window"
        );
    }

    /// The binary trees' match count (`ZSTD_insertDUBT1`,
    /// `ZSTD_DUBT_findBestMatch`): extends the `match_length` bytes known
    /// common between `ip` and the candidate `match_index` up to `iend`;
    /// returns the new length and a pointer to the candidate's byte after
    /// it (`match[matchLength]`, the next byte to compare). With `EXT`, a
    /// candidate in the extDict is counted on into the prefix
    /// (`ZSTD_count_2segments`), unless `in_dict` (`ip` itself is in the
    /// extDict and `iend` its end).
    ///
    /// # Safety
    /// `ip..iend` is in the window, the candidate (from `match_length`) too.
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(super) unsafe fn tree_count<const EXT: bool>(
        &self,
        ip: *const u8,
        in_dict: bool,
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
            let in_prefix = match_index as usize + match_length >= self.dict_limit as usize;
            if in_prefix || in_dict {
                // both in the current segment, or both in the extDict
                let mp = if in_prefix { self.base } else { self.dict_base }
                    .wrapping_add(match_index as usize + match_length);
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

    /// The position of index `i` with an extDict: in it below `dictLimit`.
    #[inline(always)]
    pub(super) fn ext_pos(&self, i: u32) -> *const u8 {
        if i < self.dict_limit {
            self.dict_base.wrapping_add(i as usize)
        } else {
            self.base.wrapping_add(i as usize)
        }
    }

    /// `ZSTD_count_2segments(ip, m, ilimit, dictEnd, prefixStart)`: the
    /// length of a match from the extDict position `m`.
    ///
    /// # Safety
    /// `ip..ilimit` is in the prefix, `m..dict_end` in the extDict.
    #[allow(unsafe_code)]
    #[inline(always)]
    pub(super) unsafe fn count_ext(&self, ip: *const u8, m: *const u8, ilimit: *const u8) -> usize {
        self.chk(m, 0);
        // SAFETY: per the caller.
        unsafe { count_2segments(ip, m, ilimit, self.dict_end, self.prefix_start) }
    }

    /// `ZSTD_getLowestMatchIndex()`.
    #[inline(always)]
    pub(super) fn lowest_match_index(&self, curr: u32) -> u32 {
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

// ---------------------------------------------------------------------------
// Hash chain

/// `ZSTD_insertAndFindFirstIndex_internal()`: inserts the positions from
/// `nextToUpdate` up to `ip` (excluded; only the first one in lazy
/// skipping mode) into the hash chains, and returns the chain head of `ip`.
///
/// # Safety
/// `ip + 8 <= iend`, and `nextToUpdate` is in the window.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn insert_and_find_first_index<const MLS: u32>(s: &mut Searcher, ip: *const u8) -> u32 {
    let hash_log = s.hash_log;
    let chain_mask = (1u32 << s.chain_log) - 1;
    let target = s.idx(ip);
    let mut idx = s.next_to_update;
    // SAFETY: positions from nextToUpdate (in the window) to ip are
    // readable for 8 bytes; hashes have `hash_log` bits and are masked
    // indices into tables of `1 << hash_log` / `1 << chain_log` entries.
    unsafe {
        while idx < target {
            let p = s.pos(idx);
            s.chk(p, 8);
            let h = hash_mls::<MLS>(p, hash_log);
            *s.chain_table.add((idx & chain_mask) as usize) = *s.hash_table.add(h);
            *s.hash_table.add(h) = idx;
            idx += 1;
            // Stop inserting every position when in the lazy skipping mode.
            if s.lazy_skipping {
                break;
            }
        }
        s.next_to_update = target;
        s.chk(ip, 8);
        *s.hash_table.add(hash_mls::<MLS>(ip, hash_log))
    }
}

/// `ZSTD_HcFindBestMatch()` (noDict, or extDict with `EXT`): the longest
/// match at `ip` along its hash chain, at most `1 << searchLog`
/// candidates. Returns its length (`< 4` if none) and stores its offBase
/// in `offset_ptr`.
///
/// # Safety
/// `ip + 8 <= ilimit`, `ilimit` the block end (see the module docs).
#[allow(unsafe_code)]
#[inline(never)]
unsafe fn hc_find_best_match<const MLS: u32, const EXT: bool>(
    s: &mut Searcher,
    ip: *const u8,
    ilimit: *const u8,
    offset_ptr: &mut u32,
) -> usize {
    let chain_size = 1u32 << s.chain_log;
    let chain_mask = chain_size - 1;
    let curr = s.idx(ip);
    let low_limit = s.lowest_match_index(curr);
    let min_chain = curr.saturating_sub(chain_size);
    let mut nb_attempts = 1u32 << s.search_log;
    let mut ml = 4 - 1;

    // SAFETY: see the module docs; candidates are at or above `low_limit`
    // (in the window) and before `ip`, and `ip + ml < ilimit` holds before
    // each 4-byte read at `ml - 3` (the loop stops when a match reaches
    // `ilimit`).
    unsafe {
        let mut match_index = insert_and_find_first_index::<MLS>(s, ip);
        while match_index >= low_limit && nb_attempts > 0 {
            let mut current_ml = 0;
            debug_assert!(match_index < curr);
            if !EXT || match_index >= s.dict_limit {
                let m = s.pos(match_index);
                s.chk(m.wrapping_add(ml - 3), 4);
                // read 4B starting from (match + ml + 1 - sizeof(U32))
                if read32(m.add(ml - 3)) == read32(ip.add(ml - 3)) {
                    current_ml = count(ip, m, ilimit);
                }
            } else {
                let m = s.dict_base.wrapping_add(match_index as usize);
                s.chk(m, 4);
                // assumption: matchIndex <= dictLimit - 4 (by table construction)
                if read32(m) == read32(ip) {
                    current_ml = s.count_ext(ip.add(4), m.add(4), ilimit) + 4;
                }
            }
            // save best solution
            if current_ml > ml {
                ml = current_ml;
                *offset_ptr = curr - match_index + REP_NUM;
                if ip.add(current_ml) == ilimit {
                    break; // best possible, avoids read overflow on next attempt
                }
            }
            if match_index <= min_chain {
                break;
            }
            match_index = *s.chain_table.add((match_index & chain_mask) as usize);
            nb_attempts -= 1;
        }
    }
    ml
}

// ---------------------------------------------------------------------------
// Binary tree (btlazy2)

/// `ZSTD_updateDUBT()`: adds the positions from `nextToUpdate` up to `ip`
/// (excluded) to the hash table and, unsorted, to the tree.
///
/// # Safety
/// `ip + 8 <= iend`, and `nextToUpdate` is in the window.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn update_dubt<const MLS: u32>(s: &mut Searcher, ip: *const u8) {
    let hash_log = s.hash_log;
    let bt = s.chain_table;
    let bt_mask = (1u32 << (s.chain_log - 1)) - 1;
    let target = s.idx(ip);
    // SAFETY: as `insert_and_find_first_index`; tree nodes are pairs at
    // `2 * (idx & bt_mask)` in a table of `2 << btLog` entries.
    unsafe {
        for idx in s.next_to_update..target {
            let p = s.pos(idx);
            s.chk(p, 8);
            let h = hash_mls::<MLS>(p, hash_log);
            let match_index = *s.hash_table.add(h);
            let next_candidate = bt.add(2 * (idx & bt_mask) as usize);
            *s.hash_table.add(h) = idx; // update hash table
            *next_candidate = match_index; // update BT like a chain
            *next_candidate.add(1) = DUBT_UNSORTED_MARK;
        }
    }
    s.next_to_update = target;
}

/// `ZSTD_insertDUBT1()` (noDict): sorts the already inserted but unsorted
/// position `curr` into the tree.
///
/// # Safety
/// `curr` is a position in the window before `input_end` (the block end).
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn insert_dubt1<const EXT: bool>(
    s: &Searcher,
    curr: u32,
    input_end: *const u8,
    mut nb_compares: u32,
    bt_low: u32,
) {
    let bt = s.chain_table;
    let bt_mask = (1u32 << (s.chain_log - 1)) - 1;
    let mut common_length_smaller = 0usize;
    let mut common_length_larger = 0usize;
    // with an extDict, `curr` may be in it: then compare within it
    let ip = if EXT { s.ext_pos(curr) } else { s.pos(curr) };
    let iend = if !EXT || curr >= s.dict_limit {
        input_end
    } else {
        s.dict_end
    };
    let mut dummy32 = 0u32;
    let dummy: *mut u32 = &mut dummy32;
    let window_valid = s.low_limit;
    let max_distance = 1u32 << s.window_log;
    let window_low = if curr.wrapping_sub(window_valid) > max_distance {
        curr - max_distance
    } else {
        window_valid
    };
    debug_assert!(curr >= bt_low);
    debug_assert!((ip as usize) < (iend as usize));

    // SAFETY: tree accesses are masked; candidates are above `window_low`
    // (in the window) and before `curr`; the byte compared after a common
    // prefix is before `iend` (the loop stops when the prefix reaches it).
    unsafe {
        let mut smaller_ptr = bt.add(2 * (curr & bt_mask) as usize);
        let mut larger_ptr = smaller_ptr.add(1);
        // this candidate is unsorted: the next sorted candidate is reached
        // through *smaller_ptr, while *larger_ptr contains the previous
        // unsorted candidate (already saved, can be overwritten)
        let mut match_index = *smaller_ptr;

        while nb_compares > 0 && match_index > window_low {
            let next_ptr = bt.add(2 * (match_index & bt_mask) as usize);
            // guaranteed minimum nb of common bytes
            let mut match_length = common_length_smaller.min(common_length_larger);
            debug_assert!(match_index < curr);
            let m_ml;
            (match_length, m_ml) = s.tree_count::<EXT>(
                ip,
                EXT && curr < s.dict_limit,
                match_index,
                match_length,
                iend,
            );

            if ip.add(match_length) == iend {
                // equal: no way to know if inf or sup; drop, to guarantee
                // consistency
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
}

/// `ZSTD_DUBT_findBestMatch()` (noDict): sorts the unsorted candidates of
/// `ip`'s hash, then searches the tree for the best match, inserting `ip`.
///
/// # Safety
/// `ip + 8 <= iend`, `iend` the block end.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn dubt_find_best_match<const MLS: u32, const EXT: bool>(
    s: &mut Searcher,
    ip: *const u8,
    iend: *const u8,
    off_base_ptr: &mut u32,
) -> usize {
    let bt = s.chain_table;
    let bt_mask = (1u32 << (s.chain_log - 1)) - 1;
    let curr = s.idx(ip);
    let window_low = s.lowest_match_index(curr);
    let bt_low = curr.saturating_sub(bt_mask);
    let unsort_limit = bt_low.max(window_low);
    let mut nb_compares = 1u32 << s.search_log;
    let mut nb_candidates = nb_compares;
    let mut previous_candidate = 0u32;

    // SAFETY: as `insert_dubt1`; the hash has `hash_log` bits.
    unsafe {
        s.chk(ip, 8);
        let h = hash_mls::<MLS>(ip, s.hash_log);
        let mut match_index = *s.hash_table.add(h);
        let mut next_candidate = bt.add(2 * (match_index & bt_mask) as usize);
        let mut unsorted_mark = next_candidate.add(1);

        // reach end of unsorted candidates list
        while match_index > unsort_limit
            && *unsorted_mark == DUBT_UNSORTED_MARK
            && nb_candidates > 1
        {
            // the unsortedMark becomes a reversed chain, to move up back to
            // the original position
            *unsorted_mark = previous_candidate;
            previous_candidate = match_index;
            match_index = *next_candidate;
            next_candidate = bt.add(2 * (match_index & bt_mask) as usize);
            unsorted_mark = next_candidate.add(1);
            nb_candidates -= 1;
        }

        // nullify last candidate if it's still unsorted (simplification,
        // detrimental to compression ratio, beneficial for speed)
        if match_index > unsort_limit && *unsorted_mark == DUBT_UNSORTED_MARK {
            *next_candidate = 0;
            *unsorted_mark = 0;
        }

        // batch sort stacked candidates
        match_index = previous_candidate;
        while match_index != 0 {
            let next_candidate_idx = *bt.add(2 * (match_index & bt_mask) as usize + 1);
            insert_dubt1::<EXT>(s, match_index, iend, nb_candidates, unsort_limit);
            match_index = next_candidate_idx;
            nb_candidates += 1;
        }

        // find longest match
        let mut common_length_smaller = 0usize;
        let mut common_length_larger = 0usize;
        let mut smaller_ptr = bt.add(2 * (curr & bt_mask) as usize);
        let mut larger_ptr = smaller_ptr.add(1);
        let mut match_end_idx = curr + 8 + 1;
        let mut dummy32 = 0u32;
        let dummy: *mut u32 = &mut dummy32;
        let mut best_length = 0usize;

        match_index = *s.hash_table.add(h);
        *s.hash_table.add(h) = curr; // update hash table

        while nb_compares > 0 && match_index > window_low {
            let next_ptr = bt.add(2 * (match_index & bt_mask) as usize);
            // guaranteed minimum nb of common bytes
            let mut match_length = common_length_smaller.min(common_length_larger);
            debug_assert!(match_index < curr);
            let m_ml;
            (match_length, m_ml) = s.tree_count::<EXT>(ip, false, match_index, match_length, iend);

            if match_length > best_length {
                if match_length > (match_end_idx - match_index) as usize {
                    match_end_idx = match_index + match_length as u32;
                }
                if 4 * (match_length - best_length) as i32
                    > highbit32(curr - match_index + 1) as i32 - highbit32(*off_base_ptr) as i32
                {
                    best_length = match_length;
                    *off_base_ptr = curr - match_index + REP_NUM;
                }
                if ip.add(match_length) == iend {
                    // equal: no way to know if inf or sup; drop, to
                    // guarantee consistency
                    break;
                }
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

        debug_assert!(match_end_idx > curr + 8);
        s.next_to_update = match_end_idx - 8; // skip repetitive patterns
        best_length
    }
}

/// `ZSTD_BtFindBestMatch()` (noDict).
///
/// # Safety
/// `ip + 8 <= iend`, `iend` the block end.
#[allow(unsafe_code)]
#[inline(never)]
unsafe fn bt_find_best_match<const MLS: u32, const EXT: bool>(
    s: &mut Searcher,
    ip: *const u8,
    iend: *const u8,
    off_base_ptr: &mut u32,
) -> usize {
    if (ip as usize) < (s.pos(s.next_to_update) as usize) {
        return 0; // skipped area
    }
    // SAFETY: per the caller.
    unsafe {
        update_dubt::<MLS>(s, ip);
        dubt_find_best_match::<MLS, EXT>(s, ip, iend, off_base_ptr)
    }
}

// ---------------------------------------------------------------------------
// Parser

/// `ZSTD_searchMax()`: the longest match at `ip` with the block's method.
///
/// # Safety
/// `ip + 8 <= iend` (`+ 16` for the row finder), `iend` the block end.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn search_max<const MLS: u32, const METHOD: u8, const ROW_LOG: u32, const EXT: bool>(
    s: &mut Searcher,
    ip: *const u8,
    iend: *const u8,
    offset_ptr: &mut u32,
) -> usize {
    // SAFETY: per the caller.
    unsafe {
        match METHOD {
            SEARCH_HASH_CHAIN => hc_find_best_match::<MLS, EXT>(s, ip, iend, offset_ptr),
            SEARCH_BINARY_TREE => bt_find_best_match::<MLS, EXT>(s, ip, iend, offset_ptr),
            _ => row::row_find_best_match::<MLS, ROW_LOG, EXT>(s, ip, iend, offset_ptr),
        }
    }
}

/// `ZSTD_compressBlock_<strategy>[_row]()` (noDict): finds the sequences
/// of the block `buf[start..end]`; the same contract as
/// [`compress_block_fast`](super::fast::compress_block_fast). Picks the
/// variant like libzstd: depth from the strategy, the row finder when
/// [`MatchState::use_row_match_finder`], `mls = clamp(minMatch, 4, 6)` and
/// `rowLog = clamp(searchLog, 4, 6)`.
#[inline(never)]
pub(crate) fn compress_block_lazy(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    macro_rules! go {
        ($method:expr, $depth:literal) => {
            match (
                ms.cparams.min_match.clamp(4, 6),
                ms.cparams.search_log.clamp(4, 6),
            ) {
                (4, 4) => lazy_generic::<4, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
                (4, 5) => lazy_generic::<4, { $method }, 5, $depth>(ms, ss, rep, buf, start, end),
                (4, _) => lazy_generic::<4, { $method }, 6, $depth>(ms, ss, rep, buf, start, end),
                (5, 4) => lazy_generic::<5, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
                (5, 5) => lazy_generic::<5, { $method }, 5, $depth>(ms, ss, rep, buf, start, end),
                (5, _) => lazy_generic::<5, { $method }, 6, $depth>(ms, ss, rep, buf, start, end),
                (_, 4) => lazy_generic::<6, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
                (_, 5) => lazy_generic::<6, { $method }, 5, $depth>(ms, ss, rep, buf, start, end),
                (_, _) => lazy_generic::<6, { $method }, 6, $depth>(ms, ss, rep, buf, start, end),
            }
        };
    }
    // The row log only matters for the row finder: one instance otherwise.
    macro_rules! go_mls {
        ($method:expr, $depth:literal) => {
            match ms.cparams.min_match.clamp(4, 6) {
                4 => lazy_generic::<4, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
                5 => lazy_generic::<5, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
                _ => lazy_generic::<6, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
            }
        };
    }
    let row = ms.use_row_match_finder;
    match ms.cparams.strategy {
        Strategy::Greedy if row => go!(SEARCH_ROW_HASH, 0),
        Strategy::Greedy => go_mls!(SEARCH_HASH_CHAIN, 0),
        Strategy::Lazy if row => go!(SEARCH_ROW_HASH, 1),
        Strategy::Lazy => go_mls!(SEARCH_HASH_CHAIN, 1),
        Strategy::Lazy2 if row => go!(SEARCH_ROW_HASH, 2),
        Strategy::Lazy2 => go_mls!(SEARCH_HASH_CHAIN, 2),
        Strategy::BtLazy2 => go_mls!(SEARCH_BINARY_TREE, 2),
        s => unreachable!("not a lazy strategy: {s:?}"),
    }
}

/// `ZSTD_compressBlock_lazy_generic()` with `dictMode == ZSTD_noDict`.
#[allow(unsafe_code)]
#[inline(always)]
fn lazy_generic<const MLS: u32, const METHOD: u8, const ROW_LOG: u32, const DEPTH: u32>(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    let cparams = ms.cparams;
    // These checks make the raw-pointer accesses below sound.
    assert!(start <= end && end <= buf.len());
    assert!(cparams.hash_log <= 32 && ms.hash_table.len() == 1usize << cparams.hash_log);
    match METHOD {
        SEARCH_ROW_HASH => {
            assert!(ms.use_row_match_finder);
            assert!(ms.tag_table.len() == ms.hash_table.len());
            assert!(ms.row_hash_log + ROW_LOG == cparams.hash_log);
            assert!(ms.row_hash_log + row::ROW_HASH_TAG_BITS <= 32);
            assert!(cparams.search_log.clamp(4, 6) == ROW_LOG);
        }
        SEARCH_BINARY_TREE => {
            assert!(cparams.chain_log >= 1 && cparams.chain_log <= 32);
            assert!(ms.chain_table.len() == 1usize << cparams.chain_log);
        }
        _ => {
            assert!(cparams.chain_log <= 32);
            assert!(ms.chain_table.len() == 1usize << cparams.chain_log);
        }
    }
    assert!(cparams.search_log < 32);
    assert!(!ms.window.has_ext_dict());
    let window_start = ms.window.pos(ms.window.dict_limit);
    assert!(window_start <= start, "window start is not in the buffer");
    assert!(ms.window.pos(ms.next_to_update) >= window_start);
    assert!(end.wrapping_sub(ms.window.base) <= u32::MAX as usize);

    let bp = buf.as_ptr();
    let base = bp.wrapping_add(ms.window.base);
    let istart = bp.wrapping_add(start);
    let iend = bp.wrapping_add(end);
    let ilimit = if METHOD == SEARCH_ROW_HASH {
        iend.wrapping_sub(8 + ROW_HASH_CACHE_SIZE)
    } else {
        iend.wrapping_sub(8)
    };
    let prefix_lowest_index = ms.window.dict_limit;
    let prefix_lowest = base.wrapping_add(prefix_lowest_index as usize);
    let lowest = bp.wrapping_add(window_start);

    let mut s = Searcher {
        base,
        hash_table: ms.hash_table.as_mut_ptr(),
        chain_table: ms.chain_table.as_mut_ptr(),
        tag_table: ms.tag_table.as_mut_ptr(),
        hash_log: cparams.hash_log,
        chain_log: cparams.chain_log,
        row_hash_log: ms.row_hash_log,
        search_log: cparams.search_log,
        window_log: cparams.window_log,
        low_limit: ms.window.low_limit,
        loaded_dict_end: ms.loaded_dict_end,
        hash_salt: ms.hash_salt,
        next_to_update: ms.next_to_update,
        // Reset the lazy skipping state
        lazy_skipping: false,
        hash_cache: ms.hash_cache,
        hash_salt_entropy: ms.hash_salt_entropy,
        lowest,
        iend,
        dict_base: base,
        dict_limit: prefix_lowest_index,
        dict_lowest: lowest,
        dict_end: lowest,
        prefix_start: prefix_lowest,
    };

    let mut ip = istart;
    let mut anchor = istart;
    let mut offset_1 = rep[0];
    let mut offset_2 = rep[1];
    let mut offset_saved1 = 0u32;
    let mut offset_saved2 = 0u32;

    // dictAndPrefixLength == 0: skip the first byte of the frame
    ip = ip.wrapping_add((ip == prefix_lowest) as usize);
    {
        let curr = s.idx(ip);
        let window_low = ms.lowest_prefix_index(curr, cparams.window_log);
        let max_rep = curr.wrapping_sub(window_low);
        if offset_2 > max_rep {
            offset_saved2 = offset_2;
            offset_2 = 0;
        }
        if offset_1 > max_rep {
            offset_saved1 = offset_1;
            offset_1 = 0;
        }
    }

    // SAFETY (whole block): see the module docs. The parser reads at `ip`
    // and `ip + 1` (4 bytes) with `ip < ilimit`, and at `ip - offset` for
    // repcodes that are valid offsets into the window (checked against it
    // at entry, later offsets of found matches); the search functions'
    // contracts hold since every searched `ip` is at most `ilimit`.
    unsafe {
        if METHOD == SEARCH_ROW_HASH {
            let ntu = s.next_to_update;
            row::fill_hash_cache::<MLS, ROW_LOG>(&mut s, ntu, ilimit);
        }

        // Match Loop
        while ip < ilimit {
            let mut match_length = 0usize;
            let mut off_base = REPCODE1_TO_OFFBASE;
            let mut start = ip.add(1);

            // check repCode
            let mut store_now = false;
            if offset_1 > 0 && {
                let r = ip.add(1).wrapping_sub(offset_1 as usize);
                s.chk(r, 4);
                read32(r) == read32(ip.add(1))
            } {
                match_length = count(ip.add(1 + 4), ip.add(1 + 4).sub(offset_1 as usize), iend) + 4;
                store_now = DEPTH == 0;
            }

            if !store_now {
                // first search (depth 0)
                {
                    let mut offbase_found = OFFBASE_UNSET;
                    let ml2 = search_max::<MLS, METHOD, ROW_LOG, false>(
                        &mut s,
                        ip,
                        iend,
                        &mut offbase_found,
                    );
                    if ml2 > match_length {
                        match_length = ml2;
                        start = ip;
                        off_base = offbase_found;
                    }
                }

                if match_length < 4 {
                    // jump faster over incompressible sections
                    let step = ((ip as usize - anchor as usize) >> K_SEARCH_STRENGTH) + 1;
                    ip = ip.add(step);
                    // Enter the lazy skipping mode once we are skipping more
                    // than 8 bytes at a time: only the searched positions
                    // are then inserted.
                    s.lazy_skipping = step > LAZY_SKIPPING_STEP;
                    continue;
                }

                // let's try to find a better solution
                if DEPTH >= 1 {
                    while ip < ilimit {
                        ip = ip.add(1);
                        if off_base != 0 && offset_1 > 0 && {
                            let r = ip.wrapping_sub(offset_1 as usize);
                            s.chk(r, 4);
                            read32(ip) == read32(r)
                        } {
                            let ml_rep =
                                count(ip.add(4), ip.add(4).sub(offset_1 as usize), iend) + 4;
                            let gain2 = (ml_rep * 3) as i64;
                            let gain1 = (match_length * 3) as i64 - highbit32(off_base) as i64 + 1;
                            if ml_rep >= 4 && gain2 > gain1 {
                                match_length = ml_rep;
                                off_base = REPCODE1_TO_OFFBASE;
                                start = ip;
                            }
                        }
                        {
                            let mut ofb_candidate = OFFBASE_UNSET;
                            let ml2 = search_max::<MLS, METHOD, ROW_LOG, false>(
                                &mut s,
                                ip,
                                iend,
                                &mut ofb_candidate,
                            );
                            // raw approx
                            let gain2 = (ml2 * 4) as i64 - highbit32(ofb_candidate) as i64;
                            let gain1 = (match_length * 4) as i64 - highbit32(off_base) as i64 + 4;
                            if ml2 >= 4 && gain2 > gain1 {
                                match_length = ml2;
                                off_base = ofb_candidate;
                                start = ip;
                                continue; // search a better one
                            }
                        }

                        // let's find an even better one
                        if DEPTH == 2 && ip < ilimit {
                            ip = ip.add(1);
                            if off_base != 0 && offset_1 > 0 && {
                                let r = ip.wrapping_sub(offset_1 as usize);
                                s.chk(r, 4);
                                read32(ip) == read32(r)
                            } {
                                let ml_rep =
                                    count(ip.add(4), ip.add(4).sub(offset_1 as usize), iend) + 4;
                                let gain2 = (ml_rep * 4) as i64;
                                let gain1 =
                                    (match_length * 4) as i64 - highbit32(off_base) as i64 + 1;
                                if ml_rep >= 4 && gain2 > gain1 {
                                    match_length = ml_rep;
                                    off_base = REPCODE1_TO_OFFBASE;
                                    start = ip;
                                }
                            }
                            {
                                let mut ofb_candidate = OFFBASE_UNSET;
                                let ml2 = search_max::<MLS, METHOD, ROW_LOG, false>(
                                    &mut s,
                                    ip,
                                    iend,
                                    &mut ofb_candidate,
                                );
                                // raw approx
                                let gain2 = (ml2 * 4) as i64 - highbit32(ofb_candidate) as i64;
                                let gain1 =
                                    (match_length * 4) as i64 - highbit32(off_base) as i64 + 7;
                                if ml2 >= 4 && gain2 > gain1 {
                                    match_length = ml2;
                                    off_base = ofb_candidate;
                                    start = ip;
                                    continue;
                                }
                            }
                        }
                        break; // nothing found: store previous solution
                    }
                }

                // catch up
                if off_base > REP_NUM {
                    let offset = off_base - REP_NUM;
                    // only search for offset within prefix
                    while (start > anchor)
                        && (start.wrapping_sub(offset as usize) > prefix_lowest)
                        && {
                            let m = start.wrapping_sub(offset as usize);
                            s.chk(m.wrapping_sub(1), 1);
                            *start.sub(1) == *m.sub(1)
                        }
                    {
                        start = start.sub(1);
                        match_length += 1;
                    }
                    offset_2 = offset_1;
                    offset_1 = offset;
                }
            }

            // store sequence
            {
                let lit_length = start as usize - anchor as usize;
                ss.store_seq(
                    buf,
                    anchor as usize - bp as usize,
                    lit_length,
                    end,
                    off_base,
                    match_length,
                );
                ip = start.add(match_length);
                anchor = ip;
            }
            if s.lazy_skipping {
                // We've found a match, disable lazy skipping mode, and
                // refill the hash cache.
                if METHOD == SEARCH_ROW_HASH {
                    let ntu = s.next_to_update;
                    row::fill_hash_cache::<MLS, ROW_LOG>(&mut s, ntu, ilimit);
                }
                s.lazy_skipping = false;
            }

            // check immediate repcode
            while ip <= ilimit && offset_2 > 0 && {
                let r = ip.wrapping_sub(offset_2 as usize);
                s.chk(r, 4);
                read32(ip) == read32(r)
            } {
                // store sequence
                match_length = count(ip.add(4), ip.add(4).sub(offset_2 as usize), iend) + 4;
                // swap repcodes
                core::mem::swap(&mut offset_1, &mut offset_2);
                ss.store_seq(
                    buf,
                    anchor as usize - bp as usize,
                    0,
                    end,
                    REPCODE1_TO_OFFBASE,
                    match_length,
                );
                ip = ip.add(match_length);
                anchor = ip;
            }
        }
    }

    ms.next_to_update = s.next_to_update;
    ms.lazy_skipping = s.lazy_skipping;
    ms.hash_cache = s.hash_cache;
    ms.hash_salt_entropy = s.hash_salt_entropy;

    // If offset_1 started invalid (offset_saved1 != 0) and became valid
    // (offset_1 != 0), rotate the saved offsets (see `fast`).
    if offset_saved1 != 0 && offset_1 != 0 {
        offset_saved2 = offset_saved1;
    }
    // save reps for next block
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

/// `ZSTD_compressBlock_<strategy>_extDict[_row]()`: as
/// [`compress_block_lazy`], for a window with an extDict segment (see
/// [`compress_block_fast_ext_dict`](super::fast::compress_block_fast_ext_dict)).
#[inline(never)]
pub(crate) fn compress_block_lazy_ext_dict(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    macro_rules! go {
        ($method:expr, $depth:literal) => {
            match (
                ms.cparams.min_match.clamp(4, 6),
                ms.cparams.search_log.clamp(4, 6),
            ) {
                (4, 4) => ext_generic::<4, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
                (4, 5) => ext_generic::<4, { $method }, 5, $depth>(ms, ss, rep, buf, start, end),
                (4, _) => ext_generic::<4, { $method }, 6, $depth>(ms, ss, rep, buf, start, end),
                (5, 4) => ext_generic::<5, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
                (5, 5) => ext_generic::<5, { $method }, 5, $depth>(ms, ss, rep, buf, start, end),
                (5, _) => ext_generic::<5, { $method }, 6, $depth>(ms, ss, rep, buf, start, end),
                (_, 4) => ext_generic::<6, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
                (_, 5) => ext_generic::<6, { $method }, 5, $depth>(ms, ss, rep, buf, start, end),
                (_, _) => ext_generic::<6, { $method }, 6, $depth>(ms, ss, rep, buf, start, end),
            }
        };
    }
    macro_rules! go_mls {
        ($method:expr, $depth:literal) => {
            match ms.cparams.min_match.clamp(4, 6) {
                4 => ext_generic::<4, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
                5 => ext_generic::<5, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
                _ => ext_generic::<6, { $method }, 4, $depth>(ms, ss, rep, buf, start, end),
            }
        };
    }
    let row = ms.use_row_match_finder;
    match ms.cparams.strategy {
        Strategy::Greedy if row => go!(SEARCH_ROW_HASH, 0),
        Strategy::Greedy => go_mls!(SEARCH_HASH_CHAIN, 0),
        Strategy::Lazy if row => go!(SEARCH_ROW_HASH, 1),
        Strategy::Lazy => go_mls!(SEARCH_HASH_CHAIN, 1),
        Strategy::Lazy2 if row => go!(SEARCH_ROW_HASH, 2),
        Strategy::Lazy2 => go_mls!(SEARCH_HASH_CHAIN, 2),
        Strategy::BtLazy2 => go_mls!(SEARCH_BINARY_TREE, 2),
        s => unreachable!("not a lazy strategy: {s:?}"),
    }
}

/// `ZSTD_compressBlock_lazy_extDict_generic()`.
///
/// Memory safety: as the noDict parser (see the module docs), with
/// candidates and repcodes below `dictLimit` read from the extDict
/// (`dictBase + index`, checked to be in the buffer at entry): the window
/// check (`>= lowLimit`, or `offset <= curr - windowLow` for repcodes)
/// keeps them at or above `lowLimit`, `ZSTD_index_overlap_check` keeps
/// 4-byte repcode reads inside one segment, and matches are counted up to
/// the extDict end, then on in the prefix (`ZSTD_count_2segments`).
#[allow(unsafe_code)]
#[inline(always)]
fn ext_generic<const MLS: u32, const METHOD: u8, const ROW_LOG: u32, const DEPTH: u32>(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    let cparams = ms.cparams;
    // These checks make the raw-pointer accesses below sound.
    assert!(start <= end && end <= buf.len());
    assert!(cparams.hash_log <= 32 && ms.hash_table.len() == 1usize << cparams.hash_log);
    match METHOD {
        SEARCH_ROW_HASH => {
            assert!(ms.use_row_match_finder);
            assert!(ms.tag_table.len() == ms.hash_table.len());
            assert!(ms.row_hash_log + ROW_LOG == cparams.hash_log);
            assert!(ms.row_hash_log + row::ROW_HASH_TAG_BITS <= 32);
            assert!(cparams.search_log.clamp(4, 6) == ROW_LOG);
        }
        SEARCH_BINARY_TREE => {
            assert!(cparams.chain_log >= 1 && cparams.chain_log <= 32);
            assert!(ms.chain_table.len() == 1usize << cparams.chain_log);
        }
        _ => {
            assert!(cparams.chain_log <= 32);
            assert!(ms.chain_table.len() == 1usize << cparams.chain_log);
        }
    }
    assert!(cparams.search_log < 32);
    let dict_limit = ms.window.dict_limit;
    let low_limit = ms.window.low_limit;
    let w = ExtWindow::new(&ms.window, buf, start, end, low_limit, dict_limit);
    assert!(ms.next_to_update >= dict_limit);

    let bp = buf.as_ptr();
    let base = w.base;
    let dict_base = w.dict_base;
    let istart = w.istart;
    let iend = w.iend;
    let ilimit = if METHOD == SEARCH_ROW_HASH {
        iend.wrapping_sub(8 + ROW_HASH_CACHE_SIZE)
    } else {
        iend.wrapping_sub(8)
    };
    let prefix_start = w.prefix_start;
    let dict_end = w.dict_end;
    let dict_start = w.dict_start;
    let window_log = cparams.window_log;

    let mut s = Searcher {
        base,
        hash_table: ms.hash_table.as_mut_ptr(),
        chain_table: ms.chain_table.as_mut_ptr(),
        tag_table: ms.tag_table.as_mut_ptr(),
        hash_log: cparams.hash_log,
        chain_log: cparams.chain_log,
        row_hash_log: ms.row_hash_log,
        search_log: cparams.search_log,
        window_log,
        low_limit,
        loaded_dict_end: ms.loaded_dict_end,
        hash_salt: ms.hash_salt,
        next_to_update: ms.next_to_update,
        // Reset the lazy skipping state
        lazy_skipping: false,
        hash_cache: ms.hash_cache,
        hash_salt_entropy: ms.hash_salt_entropy,
        lowest: prefix_start,
        iend,
        dict_base,
        dict_limit,
        dict_lowest: dict_start,
        dict_end,
        prefix_start,
    };

    let mut ip = istart;
    let mut anchor = istart;
    let mut offset_1 = rep[0];
    let mut offset_2 = rep[1];

    // The repcode `offset` at index `curr` (position `ip`): its match
    // length if it matches 4 bytes there (libzstd's repcode checks).
    // SAFETY (for the callers): `ip + 4 <= iend`.
    let rep_match = |s: &Searcher, ip: *const u8, curr: u32, offset: u32| -> usize {
        let window_low = s.lowest_match_index(curr);
        let rep_index = curr.wrapping_sub(offset);
        let rep_match = s.ext_pos(rep_index);
        // `offset <= curr - windowLow`: `curr > repIndex >= windowLow`
        if index_overlap_check(dict_limit, rep_index) & (offset <= curr.wrapping_sub(window_low))
            && {
                s.chk(rep_match, 4);
                // SAFETY: per the closure's contract and the checks above.
                unsafe { read32(ip) == read32(rep_match) }
            }
        {
            let rep_end = if rep_index < dict_limit {
                dict_end
            } else {
                iend
            };
            s.chk(rep_match, 0);
            // SAFETY: both ranges are in their segment.
            unsafe { count_2segments(ip.add(4), rep_match.add(4), iend, rep_end, prefix_start) + 4 }
        } else {
            0
        }
    };

    // init
    ip = ip.wrapping_add((ip == prefix_start) as usize);

    // SAFETY (whole block): see the function docs.
    unsafe {
        if METHOD == SEARCH_ROW_HASH {
            let ntu = s.next_to_update;
            row::fill_hash_cache::<MLS, ROW_LOG>(&mut s, ntu, ilimit);
        }

        // Match Loop
        while ip < ilimit {
            let mut match_length = 0usize;
            let mut off_base = REPCODE1_TO_OFFBASE;
            let mut start = ip.add(1);
            let mut curr = s.idx(ip);

            // check repCode
            let mut store_now = false;
            {
                let ml = rep_match(&s, ip.add(1), curr + 1, offset_1);
                if ml != 0 {
                    // repcode detected we should take it
                    match_length = ml;
                    store_now = DEPTH == 0;
                }
            }

            if !store_now {
                // first search (depth 0)
                {
                    let mut ofb_candidate = OFFBASE_UNSET;
                    let ml2 = search_max::<MLS, METHOD, ROW_LOG, true>(
                        &mut s,
                        ip,
                        iend,
                        &mut ofb_candidate,
                    );
                    if ml2 > match_length {
                        match_length = ml2;
                        start = ip;
                        off_base = ofb_candidate;
                    }
                }

                if match_length < 4 {
                    let step = (ip as usize - anchor as usize) >> K_SEARCH_STRENGTH;
                    // jump faster over incompressible sections
                    ip = ip.add(step + 1);
                    // Enter the lazy skipping mode once we are skipping more
                    // than 8 bytes at a time.
                    s.lazy_skipping = step > LAZY_SKIPPING_STEP;
                    continue;
                }

                // let's try to find a better solution
                if DEPTH >= 1 {
                    while ip < ilimit {
                        ip = ip.add(1);
                        curr += 1;
                        // check repCode
                        if off_base != 0 {
                            let rep_length = rep_match(&s, ip, curr, offset_1);
                            let gain2 = (rep_length * 3) as i64;
                            let gain1 = (match_length * 3) as i64 - highbit32(off_base) as i64 + 1;
                            if rep_length >= 4 && gain2 > gain1 {
                                match_length = rep_length;
                                off_base = REPCODE1_TO_OFFBASE;
                                start = ip;
                            }
                        }

                        // search match, depth 1
                        {
                            let mut ofb_candidate = OFFBASE_UNSET;
                            let ml2 = search_max::<MLS, METHOD, ROW_LOG, true>(
                                &mut s,
                                ip,
                                iend,
                                &mut ofb_candidate,
                            );
                            // raw approx
                            let gain2 = (ml2 * 4) as i64 - highbit32(ofb_candidate) as i64;
                            let gain1 = (match_length * 4) as i64 - highbit32(off_base) as i64 + 4;
                            if ml2 >= 4 && gain2 > gain1 {
                                match_length = ml2;
                                off_base = ofb_candidate;
                                start = ip;
                                continue; // search a better one
                            }
                        }

                        // let's find an even better one
                        if DEPTH == 2 && ip < ilimit {
                            ip = ip.add(1);
                            curr += 1;
                            // check repCode
                            if off_base != 0 {
                                let rep_length = rep_match(&s, ip, curr, offset_1);
                                let gain2 = (rep_length * 4) as i64;
                                let gain1 =
                                    (match_length * 4) as i64 - highbit32(off_base) as i64 + 1;
                                if rep_length >= 4 && gain2 > gain1 {
                                    match_length = rep_length;
                                    off_base = REPCODE1_TO_OFFBASE;
                                    start = ip;
                                }
                            }

                            // search match, depth 2
                            {
                                let mut ofb_candidate = OFFBASE_UNSET;
                                let ml2 = search_max::<MLS, METHOD, ROW_LOG, true>(
                                    &mut s,
                                    ip,
                                    iend,
                                    &mut ofb_candidate,
                                );
                                // raw approx
                                let gain2 = (ml2 * 4) as i64 - highbit32(ofb_candidate) as i64;
                                let gain1 =
                                    (match_length * 4) as i64 - highbit32(off_base) as i64 + 7;
                                if ml2 >= 4 && gain2 > gain1 {
                                    match_length = ml2;
                                    off_base = ofb_candidate;
                                    start = ip;
                                    continue;
                                }
                            }
                        }
                        break; // nothing found: store previous solution
                    }
                }

                // catch up
                if off_base > REP_NUM {
                    let offset = off_base - REP_NUM;
                    let match_index = s.idx(start).wrapping_sub(offset);
                    let mut m = s.ext_pos(match_index);
                    let m_start = if match_index < dict_limit {
                        dict_start
                    } else {
                        prefix_start
                    };
                    while (start > anchor) && (m > m_start) && {
                        s.chk(m.wrapping_sub(1), 1);
                        *start.sub(1) == *m.sub(1)
                    } {
                        start = start.sub(1);
                        m = m.sub(1);
                        match_length += 1;
                    }
                    offset_2 = offset_1;
                    offset_1 = offset;
                }
            }

            // store sequence
            {
                let lit_length = start as usize - anchor as usize;
                ss.store_seq(
                    buf,
                    anchor as usize - bp as usize,
                    lit_length,
                    end,
                    off_base,
                    match_length,
                );
                ip = start.add(match_length);
                anchor = ip;
            }
            if s.lazy_skipping {
                // We've found a match, disable lazy skipping mode, and
                // refill the hash cache.
                if METHOD == SEARCH_ROW_HASH {
                    let ntu = s.next_to_update;
                    row::fill_hash_cache::<MLS, ROW_LOG>(&mut s, ntu, ilimit);
                }
                s.lazy_skipping = false;
            }

            // check immediate repcode
            while ip <= ilimit {
                let rep_current = s.idx(ip);
                let ml = rep_match(&s, ip, rep_current, offset_2);
                if ml == 0 {
                    break;
                }
                // repcode detected we should take it
                match_length = ml;
                core::mem::swap(&mut offset_1, &mut offset_2); // swap offset history
                ss.store_seq(
                    buf,
                    anchor as usize - bp as usize,
                    0,
                    end,
                    REPCODE1_TO_OFFBASE,
                    match_length,
                );
                ip = ip.add(match_length);
                anchor = ip;
            }
        }
    }

    ms.next_to_update = s.next_to_update;
    ms.lazy_skipping = s.lazy_skipping;
    ms.hash_cache = s.hash_cache;
    ms.hash_salt_entropy = s.hash_salt_entropy;

    // Save reps for next block
    rep[0] = offset_1;
    rep[1] = offset_2;

    // Return the last literals size
    iend as usize - anchor as usize
}

#[cfg(test)]
mod tests {
    use super::super::fast::tests::{random, roundtrip, roundtrip_ring, texty};
    use super::super::fast::{MatchState, build_seq_store, reduce_table_btlazy2};
    use super::super::params::{CParams, Strategy, block_size_max, one_shot_cparams};
    use super::super::seq_store::{REP_START_VALUE, SeqDef, SeqStore};
    use super::DUBT_UNSORTED_MARK;
    use alloc::vec::Vec;

    const LAZY: [Strategy; 4] = [
        Strategy::Greedy,
        Strategy::Lazy,
        Strategy::Lazy2,
        Strategy::BtLazy2,
    ];

    fn inputs() -> [Vec<u8>; 6] {
        [
            texty(1000, 1),
            texty(300_000, 3),
            random(50_000, 4),
            alloc::vec![0u8; 200_000],
            (0..70_000u32).map(|i| (i % 251) as u8).collect(),
            {
                // incompressible stretches (lazy skipping), then matches
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
            for level in 4..=15 {
                let cp = one_shot_cparams(level, src.len() as u64);
                if !LAZY.contains(&cp.strategy) {
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
        for strategy in LAZY {
            // mls 4, 5, 6 (3 and 7 are clamped to 4 and 6)
            for mls in 3..=7 {
                // rowLog 4, 5, 6 (searchLog 1 and 7 are clamped)
                for search_log in [1, 4, 5, 6, 7] {
                    // window logs up to 14 use the hash chain, above it
                    // the row finder (greedy..lazy2)
                    for window_log in [10, 14, 15, 18, 22] {
                        let cp = CParams {
                            window_log,
                            chain_log: 16,
                            hash_log: 16,
                            search_log,
                            min_match: mls,
                            target_length: 8,
                            strategy,
                        };
                        let ms = MatchState::new(cp);
                        let row = ms.use_row_match_finder;
                        assert_eq!(row, strategy != Strategy::BtLazy2 && window_log > 14);
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
        for level in [5, 7, 9, 10] {
            let cp = one_shot_cparams(level, src.len() as u64);
            for bs in [1, 6, 7, 8, 9, 16, 17, 20, 33, 100, 1000] {
                roundtrip(&src, cp, bs);
            }
        }
        for n in 0..60 {
            let s = texty(n, 10);
            for level in [5, 9, 10] {
                roundtrip(&s, one_shot_cparams(level, n as u64), 1 << 17);
            }
        }
    }

    /// The sequences of `src` with `ms` (already reset), one-shot style.
    fn sequences(ms: &mut MatchState, src: &[u8]) -> Vec<(SeqDef, usize)> {
        let cp = ms.cparams;
        let bs = block_size_max(&cp, src.len() as u64);
        let mut ss = SeqStore::new(bs, cp.min_match);
        let mut rep = REP_START_VALUE;
        let mut out = Vec::new();
        ms.update_window(0, src.len());
        let mut pos = 0;
        while pos < src.len() {
            let n = bs.min(src.len() - pos);
            ms.prepare_block(pos, n);
            if build_seq_store(ms, &mut ss, &mut rep, src, pos, pos + n) {
                out.extend(ss.sequences().iter().map(|&s| (s, ss.lit_len)));
            }
            pos += n;
        }
        out
    }

    #[test]
    fn stale_tags() {
        // libzstd doesn't clear the tag table between frames; the content
        // must not matter (see the `row` module docs).
        let src = texty(400_000, 11);
        let other = random(400_000, 12);
        for level in [5, 6, 8, 12] {
            let cp = one_shot_cparams(level, src.len() as u64);
            let mut fresh = MatchState::new(cp);
            assert!(fresh.use_row_match_finder);
            let want = sequences(&mut fresh, &src);
            // Same salt, garbage tags (and heads).
            let mut ms = MatchState::new(cp);
            let mut s = 99u64;
            for t in ms.tag_table.iter_mut() {
                *t = super::super::fast::tests::xorshift(&mut s) as u8 % 7;
            }
            assert_eq!(sequences(&mut ms, &src), want, "L{level} garbage tags");
            // Tags of an earlier frame with the same salt.
            let mut ms = MatchState::new(cp);
            sequences(&mut ms, &other);
            let salt = ms.hash_salt;
            ms.reset(cp);
            ms.hash_salt = salt;
            assert_eq!(sequences(&mut ms, &src), want, "L{level} stale tags");
        }
    }

    #[test]
    fn salt_advances() {
        let src = texty(100_000, 13);
        let cp = one_shot_cparams(6, src.len() as u64);
        let mut ms = MatchState::new(cp);
        let first = ms.hash_salt;
        assert_ne!(first, 0);
        sequences(&mut ms, &src);
        assert_ne!(ms.hash_salt_entropy, 0);
        ms.reset(cp);
        assert_ne!(ms.hash_salt, first);
        ms.clear_hash_salt();
        ms.reset(cp);
        assert_eq!(ms.hash_salt, first);
        // The hash chain leaves the salt alone.
        let small = one_shot_cparams(6, 10_000);
        let salt = ms.hash_salt;
        ms.reset(small);
        assert!(!ms.use_row_match_finder);
        assert_eq!(ms.hash_salt, salt);
    }

    #[test]
    fn reduce_keeps_unsorted_mark() {
        let mut t = [0, 1, 2, 5, 1000, 1001, DUBT_UNSORTED_MARK, 1 << 30];
        reduce_table_btlazy2(&mut t, 999);
        assert_eq!(t, [0, 1, 0, 0, 0, 2, 1, (1 << 30) - 999]);
    }

    /// The extDict variants (hash chain, row and binary tree), through a
    /// wrapping ring buffer like the streaming compressor's.
    #[test]
    fn roundtrip_ring_variants() {
        let mut src = texty(150_000, 7);
        src.extend_from_slice(&random(5000, 8));
        src.extend_from_slice(&src.clone()[..60_000]);
        src.extend_from_slice(&texty(60_000, 17));
        for strategy in LAZY {
            for mls in [4, 5, 6] {
                for search_log in [1, 5, 6] {
                    // window logs up to 14 use the hash chain, above it the
                    // row finder (greedy..lazy2)
                    for (window_log, bs) in [(12, 4096), (14, 3000), (15, 1 << 13), (17, 1 << 17)] {
                        let cp = CParams {
                            window_log,
                            chain_log: 16,
                            hash_log: 16,
                            search_log,
                            min_match: mls,
                            target_length: 8,
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
