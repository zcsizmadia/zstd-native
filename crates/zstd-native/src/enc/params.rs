//! Compression levels and parameter selection: libzstd 1.5.7's
//! `compress/clevels.h` table and `ZSTD_getCParams_internal`,
//! `ZSTD_getCParamRowSize`, `ZSTD_adjustCParams_internal`,
//! `ZSTD_dictAndWindowLog` and `ZSTD_cycleLog` (`compress/zstd_compress.c`).
//!
//! # One-shot parameter mode
//!
//! `ZSTD_compress` / `ZSTD_compressCCtx` go through
//! `ZSTD_compress_usingDict`, which calls
//! `ZSTD_getParams_internal(level, srcSize, 0, ZSTD_cpm_noAttachDict)`.
//! `ZSTD_compress2` (`ZSTD_e_end` with the whole input) goes through
//! `ZSTD_CCtx_init_compressStream2`, where `ZSTD_getCParamMode` returns
//! `ZSTD_cpm_noAttachDict` without a CDict, and then
//! `ZSTD_getCParamsFromCCtxParams`, which runs `ZSTD_getCParams_internal` and
//! then `ZSTD_adjustCParams_internal` a second time (with
//! `useRowMatchFinder` still `auto`); the second pass changes nothing (see the
//! `adjust_is_idempotent` test). Both paths therefore select
//! [`one_shot_cparams`]`(level, src_size)`. Without a dictionary
//! `ZSTD_cpm_noAttachDict` and `ZSTD_cpm_unknown` behave the same.

/// `ZSTD_strategy` (zstd.h), with libzstd's numeric values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub(crate) enum Strategy {
    /// `ZSTD_fast`
    Fast = 1,
    /// `ZSTD_dfast`
    DFast = 2,
    /// `ZSTD_greedy`
    Greedy = 3,
    /// `ZSTD_lazy`
    Lazy = 4,
    /// `ZSTD_lazy2`
    Lazy2 = 5,
    /// `ZSTD_btlazy2`
    BtLazy2 = 6,
    /// `ZSTD_btopt`
    BtOpt = 7,
    /// `ZSTD_btultra`
    BtUltra = 8,
    /// `ZSTD_btultra2`
    BtUltra2 = 9,
}

/// `ZSTD_compressionParameters` (zstd.h), same field order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CParams {
    /// `windowLog`: largest match distance is `1 << window_log`.
    pub(crate) window_log: u32,
    /// `chainLog`
    pub(crate) chain_log: u32,
    /// `hashLog`
    pub(crate) hash_log: u32,
    /// `searchLog`
    pub(crate) search_log: u32,
    /// `minMatch`
    pub(crate) min_match: u32,
    /// `targetLength` (the acceleration for `fast`)
    pub(crate) target_length: u32,
    /// `strategy`
    pub(crate) strategy: Strategy,
}

/// `ZSTD_CParamMode_e`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CParamMode {
    /// `ZSTD_cpm_noAttachDict`: one-shot and streaming without a CDict.
    NoAttachDict,
    /// `ZSTD_cpm_attachDict`
    AttachDict,
    /// `ZSTD_cpm_createCDict`
    CreateCDict,
    /// `ZSTD_cpm_unknown`: `ZSTD_getCParams`, `ZSTD_adjustParams`.
    Unknown,
}

/// `ZSTD_ParamSwitch_e`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParamSwitch {
    /// `ZSTD_ps_auto`
    Auto,
    /// `ZSTD_ps_enable`
    Enable,
    /// `ZSTD_ps_disable`
    Disable,
}

/// `ZSTD_CONTENTSIZE_UNKNOWN`.
pub(crate) const CONTENTSIZE_UNKNOWN: u64 = u64::MAX;
/// `ZSTD_MAX_CLEVEL`.
pub(crate) const MAX_CLEVEL: i32 = 22;
/// `ZSTD_CLEVEL_DEFAULT`.
pub(crate) const CLEVEL_DEFAULT: i32 = 3;
/// `ZSTD_BLOCKSIZE_MAX`.
pub(crate) const BLOCKSIZE_MAX: usize = 1 << 17;
/// `ZSTD_TARGETLENGTH_MAX`.
pub(crate) const TARGETLENGTH_MAX: u32 = BLOCKSIZE_MAX as u32;
/// `ZSTD_WINDOWLOG_MAX` (30 on 32-bit targets, 31 on 64-bit).
pub(crate) const WINDOWLOG_MAX: u32 = crate::frame::WINDOWLOG_MAX;
/// `ZSTD_WINDOWLOG_ABSOLUTEMIN`.
pub(crate) const WINDOWLOG_ABSOLUTEMIN: u32 = 10;
/// `ZSTD_HASHLOG_MIN`.
pub(crate) const HASHLOG_MIN: u32 = 6;
/// `ZSTD_ROW_HASH_TAG_BITS` (zstd_lazy.h).
pub(crate) const ROW_HASH_TAG_BITS: u32 = 8;
/// `ZSTD_SHORT_CACHE_TAG_BITS` (zstd_compress_internal.h).
const SHORT_CACHE_TAG_BITS: u32 = 8;

/// `ZSTD_minCLevel()`: `-ZSTD_TARGETLENGTH_MAX`.
pub(crate) const fn min_clevel() -> i32 {
    -(TARGETLENGTH_MAX as i32)
}

/// `ZSTD_maxCLevel()`.
pub(crate) const fn max_clevel() -> i32 {
    MAX_CLEVEL
}

/// `ZSTD_defaultCLevel()`.
pub(crate) const fn default_clevel() -> i32 {
    CLEVEL_DEFAULT
}

const fn cp(w: u32, c: u32, h: u32, s: u32, l: u32, tl: u32, strategy: Strategy) -> CParams {
    CParams {
        window_log: w,
        chain_log: c,
        hash_log: h,
        search_log: s,
        min_match: l,
        target_length: tl,
        strategy,
    }
}

use Strategy::{
    BtLazy2 as BTLAZY2, BtOpt as BTOPT, BtUltra as BTULTRA, BtUltra2 as BTULTRA2, DFast as DFAST,
    Fast as FAST, Greedy as GREEDY, Lazy as LAZY, Lazy2 as LAZY2,
};

/// `ZSTD_defaultCParameters` (clevels.h): `[tableID][level]`, row 0 is the
/// base for negative levels. Table 0: sources > 256 KB (or unknown),
/// 1: <= 256 KB, 2: <= 128 KB, 3: <= 16 KB.
#[rustfmt::skip]
pub(crate) static DEFAULT_CPARAMETERS: [[CParams; MAX_CLEVEL as usize + 1]; 4] = [
    [   // "default" - for any srcSize > 256 KB
        //  W,  C,  H,  S,  L, TL, strat
        cp(19, 12, 13,  1,  6,  1, FAST    ),  // base for negative levels
        cp(19, 13, 14,  1,  7,  0, FAST    ),  // level  1
        cp(20, 15, 16,  1,  6,  0, FAST    ),  // level  2
        cp(21, 16, 17,  1,  5,  0, DFAST   ),  // level  3
        cp(21, 18, 18,  1,  5,  0, DFAST   ),  // level  4
        cp(21, 18, 19,  3,  5,  2, GREEDY  ),  // level  5
        cp(21, 18, 19,  3,  5,  4, LAZY    ),  // level  6
        cp(21, 19, 20,  4,  5,  8, LAZY    ),  // level  7
        cp(21, 19, 20,  4,  5, 16, LAZY2   ),  // level  8
        cp(22, 20, 21,  4,  5, 16, LAZY2   ),  // level  9
        cp(22, 21, 22,  5,  5, 16, LAZY2   ),  // level 10
        cp(22, 21, 22,  6,  5, 16, LAZY2   ),  // level 11
        cp(22, 22, 23,  6,  5, 32, LAZY2   ),  // level 12
        cp(22, 22, 22,  4,  5, 32, BTLAZY2 ),  // level 13
        cp(22, 22, 23,  5,  5, 32, BTLAZY2 ),  // level 14
        cp(22, 23, 23,  6,  5, 32, BTLAZY2 ),  // level 15
        cp(22, 22, 22,  5,  5, 48, BTOPT   ),  // level 16
        cp(23, 23, 22,  5,  4, 64, BTOPT   ),  // level 17
        cp(23, 23, 22,  6,  3, 64, BTULTRA ),  // level 18
        cp(23, 24, 22,  7,  3,256, BTULTRA2),  // level 19
        cp(25, 25, 23,  7,  3,256, BTULTRA2),  // level 20
        cp(26, 26, 24,  7,  3,512, BTULTRA2),  // level 21
        cp(27, 27, 25,  9,  3,999, BTULTRA2),  // level 22
    ],
    [   // for srcSize <= 256 KB
        //  W,  C,  H,  S,  L,  T, strat
        cp(18, 12, 13,  1,  5,  1, FAST    ),  // base for negative levels
        cp(18, 13, 14,  1,  6,  0, FAST    ),  // level  1
        cp(18, 14, 14,  1,  5,  0, DFAST   ),  // level  2
        cp(18, 16, 16,  1,  4,  0, DFAST   ),  // level  3
        cp(18, 16, 17,  3,  5,  2, GREEDY  ),  // level  4
        cp(18, 17, 18,  5,  5,  2, GREEDY  ),  // level  5
        cp(18, 18, 19,  3,  5,  4, LAZY    ),  // level  6
        cp(18, 18, 19,  4,  4,  4, LAZY    ),  // level  7
        cp(18, 18, 19,  4,  4,  8, LAZY2   ),  // level  8
        cp(18, 18, 19,  5,  4,  8, LAZY2   ),  // level  9
        cp(18, 18, 19,  6,  4,  8, LAZY2   ),  // level 10
        cp(18, 18, 19,  5,  4, 12, BTLAZY2 ),  // level 11
        cp(18, 19, 19,  7,  4, 12, BTLAZY2 ),  // level 12
        cp(18, 18, 19,  4,  4, 16, BTOPT   ),  // level 13
        cp(18, 18, 19,  4,  3, 32, BTOPT   ),  // level 14
        cp(18, 18, 19,  6,  3,128, BTOPT   ),  // level 15
        cp(18, 19, 19,  6,  3,128, BTULTRA ),  // level 16
        cp(18, 19, 19,  8,  3,256, BTULTRA ),  // level 17
        cp(18, 19, 19,  6,  3,128, BTULTRA2),  // level 18
        cp(18, 19, 19,  8,  3,256, BTULTRA2),  // level 19
        cp(18, 19, 19, 10,  3,512, BTULTRA2),  // level 20
        cp(18, 19, 19, 12,  3,512, BTULTRA2),  // level 21
        cp(18, 19, 19, 13,  3,999, BTULTRA2),  // level 22
    ],
    [   // for srcSize <= 128 KB
        //  W,  C,  H,  S,  L,  T, strat
        cp(17, 12, 12,  1,  5,  1, FAST    ),  // base for negative levels
        cp(17, 12, 13,  1,  6,  0, FAST    ),  // level  1
        cp(17, 13, 15,  1,  5,  0, FAST    ),  // level  2
        cp(17, 15, 16,  2,  5,  0, DFAST   ),  // level  3
        cp(17, 17, 17,  2,  4,  0, DFAST   ),  // level  4
        cp(17, 16, 17,  3,  4,  2, GREEDY  ),  // level  5
        cp(17, 16, 17,  3,  4,  4, LAZY    ),  // level  6
        cp(17, 16, 17,  3,  4,  8, LAZY2   ),  // level  7
        cp(17, 16, 17,  4,  4,  8, LAZY2   ),  // level  8
        cp(17, 16, 17,  5,  4,  8, LAZY2   ),  // level  9
        cp(17, 16, 17,  6,  4,  8, LAZY2   ),  // level 10
        cp(17, 17, 17,  5,  4,  8, BTLAZY2 ),  // level 11
        cp(17, 18, 17,  7,  4, 12, BTLAZY2 ),  // level 12
        cp(17, 18, 17,  3,  4, 12, BTOPT   ),  // level 13
        cp(17, 18, 17,  4,  3, 32, BTOPT   ),  // level 14
        cp(17, 18, 17,  6,  3,256, BTOPT   ),  // level 15
        cp(17, 18, 17,  6,  3,128, BTULTRA ),  // level 16
        cp(17, 18, 17,  8,  3,256, BTULTRA ),  // level 17
        cp(17, 18, 17, 10,  3,512, BTULTRA ),  // level 18
        cp(17, 18, 17,  5,  3,256, BTULTRA2),  // level 19
        cp(17, 18, 17,  7,  3,512, BTULTRA2),  // level 20
        cp(17, 18, 17,  9,  3,512, BTULTRA2),  // level 21
        cp(17, 18, 17, 11,  3,999, BTULTRA2),  // level 22
    ],
    [   // for srcSize <= 16 KB
        //  W,  C,  H,  S,  L,  T, strat
        cp(14, 12, 13,  1,  5,  1, FAST    ),  // base for negative levels
        cp(14, 14, 15,  1,  5,  0, FAST    ),  // level  1
        cp(14, 14, 15,  1,  4,  0, FAST    ),  // level  2
        cp(14, 14, 15,  2,  4,  0, DFAST   ),  // level  3
        cp(14, 14, 14,  4,  4,  2, GREEDY  ),  // level  4
        cp(14, 14, 14,  3,  4,  4, LAZY    ),  // level  5
        cp(14, 14, 14,  4,  4,  8, LAZY2   ),  // level  6
        cp(14, 14, 14,  6,  4,  8, LAZY2   ),  // level  7
        cp(14, 14, 14,  8,  4,  8, LAZY2   ),  // level  8
        cp(14, 15, 14,  5,  4,  8, BTLAZY2 ),  // level  9
        cp(14, 15, 14,  9,  4,  8, BTLAZY2 ),  // level 10
        cp(14, 15, 14,  3,  4, 12, BTOPT   ),  // level 11
        cp(14, 15, 14,  4,  3, 24, BTOPT   ),  // level 12
        cp(14, 15, 14,  5,  3, 32, BTULTRA ),  // level 13
        cp(14, 15, 15,  6,  3, 64, BTULTRA ),  // level 14
        cp(14, 15, 15,  7,  3,256, BTULTRA ),  // level 15
        cp(14, 15, 15,  5,  3, 48, BTULTRA2),  // level 16
        cp(14, 15, 15,  6,  3,128, BTULTRA2),  // level 17
        cp(14, 15, 15,  7,  3,256, BTULTRA2),  // level 18
        cp(14, 15, 15,  8,  3,256, BTULTRA2),  // level 19
        cp(14, 15, 15,  8,  3,512, BTULTRA2),  // level 20
        cp(14, 15, 15,  9,  3,512, BTULTRA2),  // level 21
        cp(14, 15, 15, 10,  3,999, BTULTRA2),  // level 22
    ],
];

/// `ZSTD_highbit32`.
#[inline]
const fn highbit32(v: u32) -> u32 {
    31 - v.leading_zeros()
}

/// `ZSTD_cycleLog()`: the chain table's cycle (binary-tree strategies use
/// half of it per position). Condition: `hash_log > 1`.
pub(crate) const fn cycle_log(hash_log: u32, strategy: Strategy) -> u32 {
    let bt_scale = (strategy as u32 >= Strategy::BtLazy2 as u32) as u32;
    hash_log - bt_scale
}

/// `ZSTD_dictAndWindowLog()`: a window log large enough for the source and
/// the dictionary. `src_size` must not be [`CONTENTSIZE_UNKNOWN`].
pub(crate) fn dict_and_window_log(window_log: u32, src_size: u64, dict_size: u64) -> u32 {
    let max_window_size = 1u64 << WINDOWLOG_MAX;
    if dict_size == 0 {
        return window_log;
    }
    debug_assert!(window_log <= WINDOWLOG_MAX);
    debug_assert!(src_size != CONTENTSIZE_UNKNOWN);
    let window_size = 1u64 << window_log;
    let dict_and_window_size = dict_size + window_size;
    if window_size >= dict_size.wrapping_add(src_size) {
        window_log
    } else if dict_and_window_size >= max_window_size {
        WINDOWLOG_MAX
    } else {
        highbit32((dict_and_window_size as u32) - 1) + 1
    }
}

/// `ZSTD_rowMatchFinderSupported()`.
pub(crate) const fn row_match_finder_supported(strategy: Strategy) -> bool {
    strategy as u32 >= Strategy::Greedy as u32 && strategy as u32 <= Strategy::Lazy2 as u32
}

/// `ZSTD_rowMatchFinderUsed()`; `mode` must be resolved (not `Auto`).
pub(crate) fn row_match_finder_used(strategy: Strategy, mode: ParamSwitch) -> bool {
    debug_assert!(mode != ParamSwitch::Auto);
    row_match_finder_supported(strategy) && mode == ParamSwitch::Enable
}

/// `ZSTD_resolveBlockSplitterMode()` with the default (`ZSTD_ps_auto`):
/// the post-block splitter (`ZSTD_compressBlock_splitBlock`) is used for
/// `btopt` and above with `windowLog >= 17`.
pub(crate) fn block_splitter_enabled(cparams: &CParams) -> bool {
    cparams.strategy >= Strategy::BtOpt && cparams.window_log >= 17
}

/// `ZSTD_resolveEnableLdm()` with the default (`ZSTD_ps_auto`): long
/// distance matching is enabled for `btopt` and above with
/// `windowLog >= 27`.
pub(crate) fn ldm_enabled(cparams: &CParams) -> bool {
    cparams.strategy >= Strategy::BtOpt && cparams.window_log >= 27
}

/// `ZSTD_resolveRowMatchFinderMode()`.
pub(crate) fn resolve_row_match_finder_mode(mode: ParamSwitch, cparams: &CParams) -> ParamSwitch {
    if mode != ParamSwitch::Auto {
        return mode;
    }
    if !row_match_finder_supported(cparams.strategy) {
        return ParamSwitch::Disable;
    }
    if cparams.window_log > 14 {
        ParamSwitch::Enable
    } else {
        ParamSwitch::Disable
    }
}

/// `ZSTD_CDictIndicesAreTagged()`: fast and dfast CDicts tag their indices.
const fn cdict_indices_are_tagged(cparams: &CParams) -> bool {
    matches!(cparams.strategy, Strategy::Fast | Strategy::DFast)
}

/// `ZSTD_adjustCParams_internal()`: shrinks the parameters for a known
/// source size (`src_size` may be [`CONTENTSIZE_UNKNOWN`]; 0 means 0).
/// `cpar` must be valid (within libzstd's bounds). All block compressors are
/// assumed built in (no `ZSTD_EXCLUDE_*` cascade).
pub(crate) fn adjust_cparams_internal(
    mut cpar: CParams,
    mut src_size: u64,
    mut dict_size: u64,
    mode: CParamMode,
    mut use_row_match_finder: ParamSwitch,
) -> CParams {
    const MIN_SRC_SIZE: u64 = 513; // (1<<9) + 1
    const HASH_SIZE_MIN: u32 = 1 << HASHLOG_MIN;
    let max_window_resize: u64 = 1u64 << (WINDOWLOG_MAX - 1);

    match mode {
        CParamMode::Unknown | CParamMode::NoAttachDict => {}
        CParamMode::CreateCDict => {
            if dict_size != 0 && src_size == CONTENTSIZE_UNKNOWN {
                src_size = MIN_SRC_SIZE;
            }
        }
        CParamMode::AttachDict => dict_size = 0,
    }

    // resize windowLog if input is small enough, to use less memory
    if src_size <= max_window_resize && dict_size <= max_window_resize {
        let t_size = (src_size + dict_size) as u32;
        let src_log = if t_size < HASH_SIZE_MIN {
            HASHLOG_MIN
        } else {
            highbit32(t_size - 1) + 1
        };
        if cpar.window_log > src_log {
            cpar.window_log = src_log;
        }
    }
    if src_size != CONTENTSIZE_UNKNOWN {
        let dict_and_window_log = dict_and_window_log(cpar.window_log, src_size, dict_size);
        let cycle_log = cycle_log(cpar.chain_log, cpar.strategy);
        if cpar.hash_log > dict_and_window_log + 1 {
            cpar.hash_log = dict_and_window_log + 1;
        }
        if cycle_log > dict_and_window_log {
            cpar.chain_log -= cycle_log - dict_and_window_log;
        }
    }

    if cpar.window_log < WINDOWLOG_ABSOLUTEMIN {
        cpar.window_log = WINDOWLOG_ABSOLUTEMIN;
    }

    if mode == CParamMode::CreateCDict && cdict_indices_are_tagged(&cpar) {
        const MAX_SHORT_CACHE_HASH_LOG: u32 = 32 - SHORT_CACHE_TAG_BITS;
        if cpar.hash_log > MAX_SHORT_CACHE_HASH_LOG {
            cpar.hash_log = MAX_SHORT_CACHE_HASH_LOG;
        }
        if cpar.chain_log > MAX_SHORT_CACHE_HASH_LOG {
            cpar.chain_log = MAX_SHORT_CACHE_HASH_LOG;
        }
    }

    // Unless explicitly disabled, conservatively assume the row match
    // finder is enabled.
    if use_row_match_finder == ParamSwitch::Auto {
        use_row_match_finder = ParamSwitch::Enable;
    }
    if row_match_finder_used(cpar.strategy, use_row_match_finder) {
        // Switch to 32-entry rows if searchLog is 5 (or more)
        let row_log = cpar.search_log.clamp(4, 6);
        let max_row_hash_log = 32 - ROW_HASH_TAG_BITS;
        let max_hash_log = max_row_hash_log + row_log;
        debug_assert!(cpar.hash_log >= row_log);
        if cpar.hash_log > max_hash_log {
            cpar.hash_log = max_hash_log;
        }
    }

    cpar
}

/// `ZSTD_getCParamRowSize()`: the size that picks the parameter table.
pub(crate) fn get_cparam_row_size(src_size_hint: u64, mut dict_size: u64, mode: CParamMode) -> u64 {
    if mode == CParamMode::AttachDict {
        dict_size = 0;
    }
    let unknown = src_size_hint == CONTENTSIZE_UNKNOWN;
    let added_size = if unknown && dict_size > 0 { 500 } else { 0 };
    if unknown && dict_size == 0 {
        CONTENTSIZE_UNKNOWN
    } else {
        src_size_hint
            .wrapping_add(dict_size)
            .wrapping_add(added_size)
    }
}

/// `ZSTD_getCParams_internal()`: the parameters for a level, a source size
/// (0 means 0, [`CONTENTSIZE_UNKNOWN`] for unknown) and a dictionary size.
pub(crate) fn get_cparams_internal(
    compression_level: i32,
    src_size_hint: u64,
    dict_size: u64,
    mode: CParamMode,
) -> CParams {
    const KB: u64 = 1 << 10;
    let r_size = get_cparam_row_size(src_size_hint, dict_size, mode);
    let table_id = (r_size <= 256 * KB) as usize
        + (r_size <= 128 * KB) as usize
        + (r_size <= 16 * KB) as usize;
    let row = if compression_level == 0 {
        CLEVEL_DEFAULT
    } else {
        // negative levels use row 0, the base for fast mode
        compression_level.clamp(0, MAX_CLEVEL)
    };
    let mut cp = DEFAULT_CPARAMETERS[table_id][row as usize];
    // acceleration factor
    if compression_level < 0 {
        let clamped = compression_level.max(min_clevel());
        cp.target_length = clamped.unsigned_abs();
    }
    adjust_cparams_internal(cp, src_size_hint, dict_size, mode, ParamSwitch::Auto)
}

/// The parameters libzstd's one-shot functions (`ZSTD_compress`,
/// `ZSTD_compressCCtx`, `ZSTD_compress2`) use for `src_size` bytes without a
/// dictionary: `ZSTD_getCParams_internal(level, srcSize, 0,
/// ZSTD_cpm_noAttachDict)`.
pub(crate) fn one_shot_cparams(compression_level: i32, src_size: u64) -> CParams {
    get_cparams_internal(compression_level, src_size, 0, CParamMode::NoAttachDict)
}

/// `ZSTD_resetCCtx_internal`'s block size (`cctx->blockSizeMax`):
/// `MIN(maxBlockSize, windowSize)` with
/// `windowSize = MAX(1, MIN(1 << windowLog, pledgedSrcSize))` and the
/// default `maxBlockSize` (`ZSTD_BLOCKSIZE_MAX`).
pub(crate) fn block_size_max(cparams: &CParams, pledged_src_size: u64) -> usize {
    let window_size = (1u64 << cparams.window_log).min(pledged_src_size).max(1);
    (BLOCKSIZE_MAX as u64).min(window_size) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(min_clevel(), -131072);
        assert_eq!(max_clevel(), 22);
        assert_eq!(default_clevel(), 3);
    }

    #[test]
    fn table_rows() {
        // Spot checks against clevels.h.
        assert_eq!(DEFAULT_CPARAMETERS[0][1], cp(19, 13, 14, 1, 7, 0, FAST));
        assert_eq!(
            DEFAULT_CPARAMETERS[0][22],
            cp(27, 27, 25, 9, 3, 999, BTULTRA2)
        );
        assert_eq!(DEFAULT_CPARAMETERS[1][3], cp(18, 16, 16, 1, 4, 0, DFAST));
        assert_eq!(DEFAULT_CPARAMETERS[2][0], cp(17, 12, 12, 1, 5, 1, FAST));
        assert_eq!(
            DEFAULT_CPARAMETERS[3][15],
            cp(14, 15, 15, 7, 3, 256, BTULTRA)
        );
    }

    #[test]
    fn one_shot_small() {
        // 1000 bytes, level 1: table 3 row 1 = {14,14,15,1,5,0,fast};
        // srcLog = highbit(999)+1 = 10 -> W=10; hashLog <= 11; chainLog
        // 14 - (14 - 10) = 10.
        assert_eq!(one_shot_cparams(1, 1000), cp(10, 10, 11, 1, 5, 0, FAST));
        // 100 bytes: srcLog = 7 -> W=7, H=8, C=7, then W raised to 10.
        assert_eq!(one_shot_cparams(1, 100), cp(10, 7, 8, 1, 5, 0, FAST));
        // 0 bytes: srcLog = HASHLOG_MIN = 6 -> W=6, H=7, C=6, W=10.
        assert_eq!(one_shot_cparams(1, 0), cp(10, 6, 7, 1, 5, 0, FAST));
    }

    #[test]
    fn one_shot_tables() {
        // 1 MiB, level 1: table 0, srcLog = 20 -> unchanged.
        assert_eq!(one_shot_cparams(1, 1 << 20), DEFAULT_CPARAMETERS[0][1]);
        // 200 KB: table 1, srcLog = 18 -> unchanged.
        assert_eq!(one_shot_cparams(1, 204_800), DEFAULT_CPARAMETERS[1][1]);
        // 100 KB, level 5: table 2 {17,16,17,3,4,2,greedy}; srcLog 17.
        assert_eq!(one_shot_cparams(5, 102_400), DEFAULT_CPARAMETERS[2][5]);
        // 16 KB exactly: table 3, level 1 {14,14,15,...}; srcLog 14.
        assert_eq!(one_shot_cparams(1, 16 * 1024), DEFAULT_CPARAMETERS[3][1]);
        // 16 KB + 1: table 2 row 1 {17,12,13,...}; srcLog = 15 -> W=15.
        assert_eq!(
            one_shot_cparams(1, 16 * 1024 + 1),
            cp(15, 12, 13, 1, 6, 0, FAST)
        );
        // 65 KB, level 2: table 2 row 2 {17,13,15,1,5,0,fast}; srcLog 17.
        assert_eq!(
            one_shot_cparams(2, 65 * 1024),
            cp(17, 13, 15, 1, 5, 0, FAST)
        );
        // 40000 bytes, level 1: table 2 row 1; srcLog = highbit(39999)+1 = 16
        assert_eq!(one_shot_cparams(1, 40_000), cp(16, 12, 13, 1, 6, 0, FAST));
    }

    #[test]
    fn levels_out_of_range() {
        assert_eq!(one_shot_cparams(0, 4 << 20), DEFAULT_CPARAMETERS[0][3]);
        assert_eq!(
            one_shot_cparams(100, 1 << 30),
            one_shot_cparams(22, 1 << 30)
        );
        // Negative levels: row 0 with targetLength = -level.
        assert_eq!(one_shot_cparams(-5, 1 << 20), cp(19, 12, 13, 1, 6, 5, FAST));
        assert_eq!(one_shot_cparams(-1_000_000, 1 << 20).target_length, 131072);
    }

    #[test]
    fn large_and_unknown() {
        // Level 22, 10 GB: no window resize (src > 2^30); dictAndWindowLog
        // = 27; cycleLog = 27 - 1 = 26 <= 27; hashLog 25 <= 28.
        assert_eq!(one_shot_cparams(22, 10 << 30), DEFAULT_CPARAMETERS[0][22]);
        // Unknown size: table 0, unchanged.
        for level in 1..=22 {
            assert_eq!(
                get_cparams_internal(level, CONTENTSIZE_UNKNOWN, 0, CParamMode::Unknown),
                DEFAULT_CPARAMETERS[0][level as usize]
            );
        }
        // Level 19, 1 MiB: table 0 {23,24,22,7,3,256,btultra2}; srcLog 20 ->
        // W=20, H<=21 -> 21, cycleLog = 24-1 = 23 > 20 -> C = 24-3 = 21.
        assert_eq!(
            one_shot_cparams(19, 1 << 20),
            cp(20, 21, 21, 7, 3, 256, BTULTRA2)
        );
    }

    #[test]
    fn row_match_finder_cap() {
        // A lazy2 config with a huge hash log is capped to 24 + rowLog.
        let p = cp(27, 27, 30, 5, 5, 16, LAZY2);
        let unknown = CONTENTSIZE_UNKNOWN;
        let a = adjust_cparams_internal(p, unknown, 0, CParamMode::Unknown, ParamSwitch::Auto);
        assert_eq!(a.hash_log, 29);
        let a = adjust_cparams_internal(p, unknown, 0, CParamMode::Unknown, ParamSwitch::Disable);
        assert_eq!(a.hash_log, 30);
        assert_eq!(
            resolve_row_match_finder_mode(ParamSwitch::Auto, &p),
            ParamSwitch::Enable
        );
    }

    #[test]
    fn adjust_is_idempotent() {
        // ZSTD_compress2 adjusts twice; the second pass must change nothing.
        let sizes = [
            0u64,
            1,
            100,
            513,
            1000,
            5000,
            16384,
            16385,
            100_000,
            131_073,
            300_000,
            1 << 24,
        ];
        for level in -3..=22 {
            for &size in &sizes {
                let p = one_shot_cparams(level, size);
                let mode = CParamMode::NoAttachDict;
                let q = adjust_cparams_internal(p, size, 0, mode, ParamSwitch::Auto);
                assert_eq!(p, q, "level {level} size {size}");
            }
        }
    }

    #[test]
    fn dict_window_log() {
        assert_eq!(dict_and_window_log(17, 1000, 0), 17);
        assert_eq!(dict_and_window_log(17, 1000, 1000), 17);
        // window 2^10, dict 4000 + src 1000 > 1024 -> highbit(4000+1024-1)+1 = 13
        assert_eq!(dict_and_window_log(10, 1000, 4000), 13);
        assert_eq!(dict_and_window_log(30, 1 << 31, 1 << 31), WINDOWLOG_MAX);
        assert_eq!(cycle_log(20, FAST), 20);
        assert_eq!(cycle_log(20, BTLAZY2), 19);
    }

    #[test]
    fn block_size() {
        let p = one_shot_cparams(1, 1000);
        assert_eq!(block_size_max(&p, 1000), 1000);
        assert_eq!(block_size_max(&p, 0), 1);
        let p = one_shot_cparams(1, 1 << 20);
        assert_eq!(block_size_max(&p, 1 << 20), BLOCKSIZE_MAX);
    }
}
