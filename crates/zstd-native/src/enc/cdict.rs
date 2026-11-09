//! Compression dictionaries: libzstd 1.5.7's `ZSTD_CDict` and how a
//! compression context uses one (`compress/zstd_compress.c`).
//!
//! # Loading (`ZSTD_compress_insertDictionary`)
//!
//! A dictionary is either a zstd-format one (magic `0xEC30A437`: an ID,
//! entropy tables and repcodes, `ZSTD_loadCEntropy`, then content) or raw
//! content (anything else, `ZSTD_dct_auto`); one under 8 bytes is ignored.
//! The content goes into the match finder's tables
//! (`ZSTD_loadDictionaryContent`): the window takes it as its first
//! segment, at most the last `ZSTD_CURRENT_MAX` bytes (16 MiB for the
//! tagged tables of a `fast` or `dfast` CDict), and the tables get at most
//! the last `1 << max(hashLog + 3, chainLog + 1)` bytes. The content stays
//! in the [`CDict`]'s buffer: a match state's window positions are then in
//! that buffer ([`Window::base_in_dict`](super::fast::Window::base_in_dict)),
//! and the first segment of a frame's input, in another buffer, is never
//! contiguous with it: the dictionary becomes the extDict.
//!
//! # A CDict (`ZSTD_createCDict`)
//!
//! Its parameters are the level's for an unknown source size and the
//! dictionary's size (`ZSTD_cpm_createCDict`); its tables are filled with
//! `ZSTD_dtlm_full`, `fast` and `dfast` ones with tagged indices
//! (`ZSTD_tfp_forCDict`: `index << 8 | tag`, the "short cache"); its row
//! match finder (if any) is unsalted.
//!
//! # Using it (`ZSTD_compressBegin_internal`)
//!
//! For small sources (under 128 KB, or under 6 times the dictionary, or
//! unknown, or always for a dictionary loaded into a context, whose CDict
//! has no level), the context starts from the CDict, by
//! - attaching it (`ZSTD_resetCCtx_byAttachingCDict`) when the source is at
//!   most `attachDictSizeCutoffs[strategy]` (8 KB for `fast`, `btultra`,
//!   `btultra2`, 16 KB for `dfast`, 32 KB for the others) or of unknown
//!   size: the context's own tables are sized for the source, and the match
//!   finders' `_dictMatchState` variants also search the CDict's tables;
//! - or copying its tables (`ZSTD_resetCCtx_byCopyingCDict`): the context
//!   then has the CDict's parameters (with the frame's window), and the
//!   dictionary is the extDict.
//!
//! Otherwise the context is reset with the frame's own parameters and the
//! content is loaded into its tables again (`ZSTD_dtlm_fast`). Either way
//! the first block starts with the dictionary's entropy tables and
//! repcodes, and the frame header holds its ID.
//!
//! Not ported: dedicated dictionary search (`ZSTD_c_enableDedicatedDictSearch`,
//! off by default), the `ZSTD_c_forceAttachDict` and `ZSTD_c_dictIDFlag`
//! parameters (their defaults are what this does), prefix dictionaries
//! (`ZSTD_CCtx_refPrefix`).
//!
//! With long distance matching, a raw-content dictionary loaded into the
//! context's tables is also loaded into the LDM table (see the `ldm`
//! module); a zstd-format one, or one attached or copied from a CDict, is
//! not (as in libzstd).

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::compress::EntropyTables;
use super::fast::{
    CURRENT_MAX, HASH_READ_SIZE, MatchState, SHORT_CACHE_TAG_BITS, TableLoadMethod,
    WINDOW_START_INDEX,
};
use super::huf_enc::{HufRepeat, huf_read_ctable};
use super::params::{
    CONTENTSIZE_UNKNOWN, CParamMode, CParams, ParamSwitch, Strategy, adjust_cparams_internal,
    get_cparams_internal, resolve_row_match_finder_mode, row_match_finder_used,
};
use super::seq_store::{REP_START_VALUE, hash_at, highbit32};
use super::sequences::FseRepeat;
use crate::error::{Error, Result};
use crate::fse::{LL_FSE_LOG, MAX_LL, MAX_ML, MAX_OFF, ML_FSE_LOG};

/// `ZSTD_MAGIC_DICTIONARY`.
const DICT_MAGIC: u32 = 0xEC30_A437;
/// `OffFSELog`.
const OFF_FSE_LOG: u32 = 8;
/// `ZSTD_USE_CDICT_PARAMS_SRCSIZE_CUTOFF`.
const USE_CDICT_PARAMS_SRCSIZE_CUTOFF: u64 = 128 << 10;
/// `ZSTD_USE_CDICT_PARAMS_DICTSIZE_MULTIPLIER`.
const USE_CDICT_PARAMS_DICTSIZE_MULTIPLIER: u64 = 6;

/// `attachDictSizeCutoffs`: per strategy, the source size up to which a
/// CDict is attached rather than copied.
const ATTACH_DICT_SIZE_CUTOFFS: [u64; 10] = [
    8 << 10,  // unused
    8 << 10,  // ZSTD_fast
    16 << 10, // ZSTD_dfast
    32 << 10, // ZSTD_greedy
    32 << 10, // ZSTD_lazy
    32 << 10, // ZSTD_lazy2
    32 << 10, // ZSTD_btlazy2
    32 << 10, // ZSTD_btopt
    8 << 10,  // ZSTD_btultra
    8 << 10,  // ZSTD_btultra2
];

/// libzstd's `ZSTD_CDict`: a dictionary prepared for compression at a
/// level (its content, its entropy tables and its match finder tables).
pub(crate) struct CDict {
    /// The dictionary as given (`dictContent`; `dictContentSize` is its
    /// length, entropy header included).
    pub(crate) content: Vec<u8>,
    /// `dictID` (0 for raw content).
    pub(crate) dict_id: u32,
    /// `compressionLevel`: the level frames using it take; 0
    /// (`ZSTD_NO_CLEVEL`) for a dictionary loaded into a compressor, whose
    /// level is the compressor's.
    pub(crate) level: i32,
    /// `matchState.cParams` and `useRowMatchFinder` (resolved).
    pub(crate) cparams: CParams,
    pub(crate) use_row: bool,
    /// `matchState`: the tables, with window positions in `content`.
    pub(crate) ms: MatchState,
    /// `cBlockState`: the entropy tables and repcodes a frame starts with.
    pub(crate) entropy: EntropyTables,
    pub(crate) rep: [u32; 3],
    /// Where the content loaded into the tables starts (after a zstd
    /// dictionary's header); `None` for a dictionary under 8 bytes.
    load_start: Option<usize>,
}

impl core::fmt::Debug for CDict {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CDict")
            .field("dict_id", &self.dict_id)
            .field("level", &self.level)
            .field("content_len", &self.content.len())
            .field("cparams", &self.cparams)
            .finish_non_exhaustive()
    }
}

impl CDict {
    /// `ZSTD_createCDict(dict, dictSize, level)` (`ZSTD_dct_auto`): the
    /// parameters are `level`'s, and frames using it take `level` (the
    /// default 3 for 0). With `local`, `ZSTD_initLocalDict` instead (the
    /// dictionary of `ZSTD_CCtx_loadDictionary`): the same tables, but no
    /// level of its own (`ZSTD_NO_CLEVEL`).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidDictionary`] if a zstd-format dictionary is corrupt.
    pub(crate) fn new(data: &[u8], level: i32, local: bool) -> Result<CDict> {
        let dict_size = data.len() as u64;
        // ZSTD_getCParamsFromCCtxParams(.., ZSTD_CONTENTSIZE_UNKNOWN,
        // dictSize, ZSTD_cpm_createCDict): ZSTD_getCParams_internal, then
        // one more adjustment
        let unknown = CONTENTSIZE_UNKNOWN;
        let mode = CParamMode::CreateCDict;
        let cp = get_cparams_internal(level, unknown, dict_size, mode);
        let cparams = adjust_cparams_internal(cp, unknown, dict_size, mode, ParamSwitch::Auto);
        let use_row = row_match_finder_used(
            cparams.strategy,
            resolve_row_match_finder_mode(ParamSwitch::Auto, &cparams),
        );
        let mut ms = MatchState::new_with(cparams, use_row, true);
        let mut entropy = EntropyTables::new();
        let mut rep = REP_START_VALUE;
        // ZSTD_compress_insertDictionary (ZSTD_dtlm_full, ZSTD_tfp_forCDict)
        let (dict_id, load_start) = if data.len() < 8 {
            (0, None)
        } else if u32::from_le_bytes(data[..4].try_into().unwrap()) != DICT_MAGIC {
            (0, Some(0)) // raw content dictionary detected
        } else {
            let id = u32::from_le_bytes(data[4..8].try_into().unwrap());
            let e_size;
            (entropy, rep, e_size) = load_c_entropy(data)?;
            (id, Some(e_size))
        };
        if let Some(s) = load_start {
            load_dictionary_content(&mut ms, data, s, data.len(), TableLoadMethod::Full, true);
        }
        let level = if local {
            0
        } else if level == 0 {
            super::params::CLEVEL_DEFAULT
        } else {
            level
        };
        Ok(CDict {
            content: data.to_vec(),
            dict_id,
            level,
            cparams,
            use_row,
            ms,
            entropy,
            rep,
            load_start,
        })
    }

    /// `ZSTD_compress_insertDictionary` of this dictionary into a
    /// context's (fresh) match state `ms` (`ZSTD_dtlm_fast`,
    /// `ZSTD_tfp_forCCtx`): loads its content into `ms`'s tables, the
    /// content staying in `self` (`ms.dict`). The entropy tables and
    /// repcodes (`self.entropy`, `self.rep`) are the caller's to take.
    pub(crate) fn load_into(self: &Arc<CDict>, ms: &mut MatchState) {
        if let Some(s) = self.load_start {
            ms.dict = Some(self.clone());
            let end = self.content.len();
            load_dictionary_content(ms, &self.content, s, end, TableLoadMethod::Fast, false);
        }
    }

    /// A raw-content dictionary of at least 8 bytes (loaded with
    /// `ZSTD_loadDictionaryContent` directly, which also loads it into the
    /// LDM table).
    pub(crate) fn is_raw_content(&self) -> bool {
        self.load_start == Some(0)
    }

    /// `cdict->matchState.window` holds content (`cdictLen != 0` in
    /// `ZSTD_resetCCtx_byAttachingCDict`): whether attaching makes sense.
    pub(crate) fn has_window_content(&self) -> bool {
        let w = &self.ms.window;
        w.index(w.next_src) != w.dict_limit
    }
}

/// `ZSTD_shouldAttachDict()` (no dedicated dictionary search, no
/// `ZSTD_c_forceAttachDict`, no `forceWindow`).
pub(crate) fn should_attach(cd: &CDict, pledged_src_size: u64) -> bool {
    let cutoff = ATTACH_DICT_SIZE_CUTOFFS[cd.cparams.strategy as usize];
    pledged_src_size <= cutoff || pledged_src_size == CONTENTSIZE_UNKNOWN
}

/// The condition of `ZSTD_compressBegin_internal` to start a frame of
/// `pledged_src_size` bytes from the CDict's tables (attached or copied)
/// rather than loading its content into tables with the frame's own
/// parameters.
pub(crate) fn use_cdict_tables(cd: &CDict, pledged_src_size: u64) -> bool {
    let len = cd.content.len() as u64;
    len > 0
        && (pledged_src_size < USE_CDICT_PARAMS_SRCSIZE_CUTOFF
            || pledged_src_size < len * USE_CDICT_PARAMS_DICTSIZE_MULTIPLIER
            || pledged_src_size == CONTENTSIZE_UNKNOWN
            || cd.level == 0)
}

/// The parameters of a frame of `pledged` bytes (`None`: unknown) at
/// `level` with the dictionary `cd` (`ZSTD_CCtx_init_compressStream2`:
/// `ZSTD_getCParamMode`, then `ZSTD_getCParamsFromCCtxParams` with the
/// dictionary's size and `window_log` as `ZSTD_c_windowLog`, 0 for the
/// level's). With `force_ldm` (`ZSTD_c_enableLongDistanceMatching` set),
/// the window log defaults to `ZSTD_LDM_DEFAULT_WINDOW_LOG` instead of the
/// level's.
pub(crate) fn frame_cparams(
    level: i32,
    cd: Option<&CDict>,
    pledged: Option<u64>,
    window_log: u32,
    force_ldm: bool,
) -> CParams {
    let size = pledged.unwrap_or(CONTENTSIZE_UNKNOWN);
    let dict_size = cd.map_or(0, |c| c.content.len() as u64);
    let mode = match cd {
        Some(cd) if should_attach(cd, size) => CParamMode::AttachDict,
        _ => CParamMode::NoAttachDict,
    };
    let mut cp = get_cparams_internal(level, size, dict_size, mode);
    if force_ldm {
        cp.window_log = super::ldm::LDM_DEFAULT_WINDOW_LOG;
    }
    if window_log != 0 {
        cp.window_log = window_log; // ZSTD_overrideCParams
    }
    adjust_cparams_internal(cp, size, dict_size, mode, ParamSwitch::Auto)
}

/// `ZSTD_dictNCountRepeat()`: a dictionary's FSE table is `valid` only if
/// it covers every symbol up to `max_symbol_value` with a non-zero
/// probability.
fn dict_ncount_repeat(
    norm: &[i16],
    dict_max_symbol_value: u32,
    max_symbol_value: u32,
) -> FseRepeat {
    if dict_max_symbol_value < max_symbol_value {
        return FseRepeat::Check;
    }
    if norm[..=max_symbol_value as usize].contains(&0) {
        return FseRepeat::Check;
    }
    FseRepeat::Valid
}

/// `ZSTD_loadCEntropy()`: reads a zstd-format dictionary's entropy tables
/// and repcodes (after its magic number and ID). Returns them and the size
/// of the header (where the content starts).
///
/// # Errors
///
/// [`Error::InvalidDictionary`] if they are corrupt.
fn load_c_entropy(dict: &[u8]) -> Result<(EntropyTables, [u32; 3], usize)> {
    let bad = Error::InvalidDictionary;
    let mut e = EntropyTables::new();
    let mut p = 8;

    // literals: only a table with a weight for every byte is valid
    e.huf.repeat = HufRepeat::Check;
    let (huf_size, has_zero_weights) = huf_read_ctable(&mut e.huf.table, &dict[p..])?;
    if !has_zero_weights && e.huf.table.max_symbol_value == 255 {
        e.huf.repeat = HufRepeat::Valid;
    }
    p += huf_size;

    // offsets: fill all offset symbols to avoid garbage at end of table;
    // validity is checked once the content size is known
    let mut of_norm = [0i16; MAX_OFF + 1];
    let (of_max, of_log, n) =
        crate::fse::read_ncount(&dict[p..], MAX_OFF, &mut of_norm).map_err(|_| bad)?;
    if of_log > OFF_FSE_LOG {
        return Err(bad);
    }
    e.fse
        .of
        .build(&of_norm, MAX_OFF as u32, of_log)
        .map_err(|_| bad)?;
    p += n;

    let mut ml_norm = [0i16; MAX_ML + 1];
    let (ml_max, ml_log, n) =
        crate::fse::read_ncount(&dict[p..], MAX_ML, &mut ml_norm).map_err(|_| bad)?;
    if ml_log > ML_FSE_LOG {
        return Err(bad);
    }
    e.fse
        .ml
        .build(&ml_norm, ml_max as u32, ml_log)
        .map_err(|_| bad)?;
    e.fse.ml_repeat = dict_ncount_repeat(&ml_norm, ml_max as u32, MAX_ML as u32);
    p += n;

    let mut ll_norm = [0i16; MAX_LL + 1];
    let (ll_max, ll_log, n) =
        crate::fse::read_ncount(&dict[p..], MAX_LL, &mut ll_norm).map_err(|_| bad)?;
    if ll_log > LL_FSE_LOG {
        return Err(bad);
    }
    e.fse
        .ll
        .build(&ll_norm, ll_max as u32, ll_log)
        .map_err(|_| bad)?;
    e.fse.ll_repeat = dict_ncount_repeat(&ll_norm, ll_max as u32, MAX_LL as u32);
    p += n;

    if p + 12 > dict.len() {
        return Err(bad);
    }
    let mut rep = [0u32; 3];
    for (i, r) in rep.iter_mut().enumerate() {
        *r = u32::from_le_bytes(dict[p + 4 * i..p + 4 * i + 4].try_into().unwrap());
    }
    p += 12;

    let dict_content_size = dict.len() - p;
    // All offset values <= dictContentSize + 128 KB must be representable
    // for a valid table
    let offcode_max = if dict_content_size as u64 <= u64::from(u32::MAX) - (128 << 10) {
        // the minimum offset code required to represent maxOffset
        highbit32(dict_content_size as u32 + (128 << 10))
    } else {
        MAX_OFF as u32
    };
    e.fse.of_repeat = dict_ncount_repeat(&of_norm, of_max as u32, offcode_max.min(MAX_OFF as u32));

    // All repCodes must be <= dictContentSize and != 0
    if rep
        .iter()
        .any(|&r| r == 0 || r as usize > dict_content_size)
    {
        return Err(bad);
    }
    Ok((e, rep, p))
}

/// `ZSTD_CDictIndicesAreTagged()`.
fn indices_are_tagged(cp: &CParams) -> bool {
    matches!(cp.strategy, Strategy::Fast | Strategy::DFast)
}

/// `ZSTD_loadDictionaryContent()` (without its LDM part,
/// `LdmState::load_dictionary`; no `forceWindow`, no
/// `deterministicRefPrefix`): makes `buf[start..end]` the first segment of
/// `ms`'s (fresh) window and fills its tables with it, for a CDict
/// (`for_cdict`: tagged `fast`/`dfast` tables) or a context.
pub(crate) fn load_dictionary_content(
    ms: &mut MatchState,
    buf: &[u8],
    start: usize,
    end: usize,
    dtlm: TableLoadMethod,
    for_cdict: bool,
) {
    load_content(ms, buf, start, end, dtlm, for_cdict, true);
}

/// `ZSTD_compress_insertDictionary` of a raw-content prefix with
/// `ZSTD_c_forceMaxWindow` set (`ZSTD_dtlm_fast`, `ZSTD_tfp_forCCtx`), as
/// the multithreaded compressor loads a job's overlap with the previous
/// job (`ZSTDMT_compressionJob`): `buf[start..end]` is followed in `buf`
/// by the data to compress, so the window stays contiguous, and the
/// prefix is part of the window rather than a dictionary
/// (`loadedDictEnd = 0`). Prefixes under 8 bytes are ignored.
pub(crate) fn load_prefix(ms: &mut MatchState, buf: &[u8], start: usize, end: usize) {
    if end - start >= 8 {
        load_content(ms, buf, start, end, TableLoadMethod::Fast, false, false);
    }
}

/// [`load_dictionary_content`] (`own_buffer`: the content is in the
/// dictionary's buffer), or the prefix of [`load_prefix`].
fn load_content(
    ms: &mut MatchState,
    buf: &[u8],
    mut start: usize,
    end: usize,
    dtlm: TableLoadMethod,
    for_cdict: bool,
    own_buffer: bool,
) {
    let cp = ms.cparams;
    debug_assert!(ms.window.is_empty());
    {
        // Ensure large dictionaries can't cause index overflow: allow
        // indices up to exactly ZSTD_CURRENT_MAX
        let mut max_dict_size = CURRENT_MAX - WINDOW_START_INDEX;
        if indices_are_tagged(&cp) && for_cdict {
            // "short cache" tables hold the index in their top 24 bits
            let short_cache_max_dict_size =
                (1u32 << (32 - SHORT_CACHE_TAG_BITS)) - WINDOW_START_INDEX;
            max_dict_size = max_dict_size.min(short_cache_max_dict_size);
        }
        // If the dictionary is too large, only load its suffix.
        if end - start > max_dict_size as usize {
            start = end - max_dict_size as usize;
        }
    }
    ms.window.update(start, end - start, false);
    // the window's positions are now in the dictionary's buffer (a
    // prefix's are in the caller's)
    ms.window.base_in_dict = own_buffer;

    // If the dict is larger than we can reasonably index in our tables,
    // only load the suffix.
    {
        let log = (cp.hash_log + 3).max(cp.chain_log + 1).min(31);
        let max_dict_size = 1usize << log;
        if end - start > max_dict_size {
            start = end - max_dict_size;
        }
    }
    ms.next_to_update = ms.window.index(start);
    // forceWindow (a prefix): no dictionary end, the window rules apply
    ms.loaded_dict_end = if own_buffer { ms.window.index(end) } else { 0 };
    ms.force_non_contiguous = false;

    if end - start <= HASH_READ_SIZE {
        return;
    }
    ms.overflow_correct_if_needed(start, end);

    match cp.strategy {
        Strategy::Fast if for_cdict => ms.fill_hash_table_for_cdict(buf, end, dtlm),
        Strategy::Fast => ms.fill_hash_table(buf, end, dtlm),
        Strategy::DFast if for_cdict => {
            super::dfast::fill_double_hash_table_for_cdict(ms, buf, end, dtlm)
        }
        Strategy::DFast => super::dfast::fill_double_hash_table(ms, buf, end, dtlm),
        Strategy::Greedy | Strategy::Lazy | Strategy::Lazy2 => {
            if ms.use_row_match_finder {
                ms.tag_table.fill(0);
                row_update_for_dict(ms, buf, end - HASH_READ_SIZE);
            } else {
                insert_hash_chain_for_dict(ms, buf, end - HASH_READ_SIZE);
            }
        }
        // we want the dictionary table fully sorted
        Strategy::BtLazy2 | Strategy::BtOpt | Strategy::BtUltra | Strategy::BtUltra2 => {
            super::opt::update_tree_for_dict(ms, buf, end)
        }
    }
    ms.next_to_update = ms.window.index(end);
}

/// `ZSTD_insertAndFindFirstIndex()` for a dictionary's content: inserts
/// the positions from `nextToUpdate` up to `target` (excluded) into the
/// hash chains (`mls = minMatch`).
fn insert_hash_chain_for_dict(ms: &mut MatchState, buf: &[u8], target: usize) {
    let cp = ms.cparams;
    let chain_mask = (1u32 << cp.chain_log) - 1;
    let target = ms.window.index(target);
    for idx in ms.next_to_update..target {
        let h = hash_at(buf, ms.window.pos(idx), cp.hash_log, cp.min_match);
        ms.chain_table[(idx & chain_mask) as usize] = ms.hash_table[h];
        ms.hash_table[h] = idx;
    }
    ms.next_to_update = target;
}

/// `ZSTD_row_update()` for a dictionary's content (no hash cache, no
/// skipping): inserts the positions from `nextToUpdate` up to `target`
/// (excluded) into the rows (`mls = min(minMatch, 6)`).
fn row_update_for_dict(ms: &mut MatchState, buf: &[u8], target: usize) {
    let cp = ms.cparams;
    let row_log = cp.search_log.clamp(4, 6);
    let row_mask = (1u32 << row_log) - 1;
    let mls = cp.min_match.min(6);
    let h_bits = ms.row_hash_log + super::row::ROW_HASH_TAG_BITS;
    let salt = ms.hash_salt;
    let target = ms.window.index(target);
    for idx in ms.next_to_update..target {
        let p = ms.window.pos(idx);
        let hash = hash_salted_at(buf, p, h_bits, mls, salt);
        let rel_row = ((hash >> super::row::ROW_HASH_TAG_BITS) << row_log) as usize;
        // ZSTD_row_nextIndex: the row's head moves backwards, skipping 0
        let head = &mut ms.tag_table[rel_row];
        let mut next = u32::from(head.wrapping_sub(1)) & row_mask;
        if next == 0 {
            next = row_mask;
        }
        *head = next as u8;
        ms.tag_table[rel_row + next as usize] = hash as u8;
        ms.hash_table[rel_row + next as usize] = idx;
    }
    ms.next_to_update = target;
}

/// `ZSTD_hashPtrSalted()` on a buffer position (reads 8 bytes; `mls` 5
/// and 6, else 4).
fn hash_salted_at(buf: &[u8], pos: usize, h_bits: u32, mls: u32, salt: u64) -> u32 {
    use super::seq_store::{PRIME4BYTES, PRIME5BYTES, PRIME6BYTES};
    let v = u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap());
    match mls {
        5 => (((v << (64 - 40)).wrapping_mul(PRIME5BYTES) ^ salt) >> (64 - h_bits)) as u32,
        6 => (((v << (64 - 48)).wrapping_mul(PRIME6BYTES) ^ salt) >> (64 - h_bits)) as u32,
        _ => ((v as u32).wrapping_mul(PRIME4BYTES) ^ salt as u32) >> (32 - h_bits),
    }
}

/// A dictionary prepared for compression at a level (libzstd's
/// `ZSTD_CDict`, `ZSTD_createCDict`): parsed and indexed once, then used by
/// any number of [`Compressor`](crate::Compressor)s,
/// [`StreamEncoder`](crate::StreamEncoder)s and [`Encoder`](crate::Encoder)s
/// (cloning it is cheap: the tables are shared). Frames compressed with it
/// take its level, and are byte-identical to libzstd's with the same
/// CDict (`ZSTD_CCtx_refCDict`, the `zstd` crate's
/// `with_prepared_dictionary`).
///
/// Like libzstd's `ZSTD_dct_auto`, data that starts with the dictionary
/// magic number (`0xEC30A437`) is a zstd-format dictionary (as made by
/// `zstd --train`: ID, entropy tables, repcodes, content), anything else
/// raw content. Dictionaries under 8 bytes are ignored (no ID, no
/// content).
///
/// ```
/// use zstd_native::{Compressor, EncoderDictionary};
///
/// let dict = EncoderDictionary::new(b"common words, common phrases", 3)?;
/// let mut c = Compressor::with_prepared_dictionary(&dict);
/// let compressed = c.compress(b"common phrases, common words")?;
///
/// let mut d = zstd_native::Decompressor::with_dictionary(zstd_native::Dictionary::new(
///     b"common words, common phrases",
/// )?);
/// assert_eq!(d.decompress(&compressed)?, b"common phrases, common words");
/// # Ok::<(), zstd_native::Error>(())
/// ```
#[derive(Clone)]
pub struct EncoderDictionary {
    pub(crate) inner: Arc<CDict>,
}

impl core::fmt::Debug for EncoderDictionary {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EncoderDictionary")
            .field("id", &self.inner.dict_id)
            .field("level", &self.inner.level)
            .field("len", &self.inner.content.len())
            .finish_non_exhaustive()
    }
}

impl EncoderDictionary {
    /// Prepares `dict` for compression at `level` (see
    /// [`compress`](crate::compress); 0 means the default, 3).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidDictionary`] if a zstd-format dictionary is corrupt.
    pub fn new(dict: &[u8], level: i32) -> Result<EncoderDictionary> {
        Ok(EncoderDictionary {
            inner: Arc::new(CDict::new(dict, level, false)?),
        })
    }

    /// The dictionary ID written in the frames (0 for raw content: none).
    pub fn id(&self) -> u32 {
        self.inner.dict_id
    }

    /// The level frames compressed with it take.
    pub fn level(&self) -> i32 {
        self.inner.level
    }
}

/// A compressor's dictionary (libzstd's `cctx->localDict` and
/// `cctx->cdict`).
#[derive(Clone, Debug, Default)]
pub(crate) enum DictSlot {
    /// No dictionary.
    #[default]
    None,
    /// `ZSTD_CCtx_loadDictionary`: the dictionary, and its CDict once made
    /// (`ZSTD_initLocalDict`, at the start of the first frame, with the
    /// compressor's level then; kept for later frames).
    Local {
        data: Arc<[u8]>,
        cdict: Option<Arc<CDict>>,
    },
    /// `ZSTD_CCtx_refCDict`: frames take its level.
    Prepared(Arc<CDict>),
}

impl DictSlot {
    /// `ZSTD_CCtx_loadDictionary(dict)`: no dictionary if `dict` is
    /// empty. A zstd-format dictionary's header is checked now (libzstd
    /// checks it when the first frame starts).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidDictionary`] if a zstd-format dictionary is corrupt.
    pub(crate) fn local(dict: &[u8]) -> Result<DictSlot> {
        if dict.is_empty() {
            return Ok(DictSlot::None);
        }
        if dict.len() >= 8 && u32::from_le_bytes(dict[..4].try_into().unwrap()) == DICT_MAGIC {
            load_c_entropy(dict)?;
        }
        Ok(DictSlot::Local {
            data: Arc::from(dict),
            cdict: None,
        })
    }

    /// The dictionary of the next frame and the level it takes, for a
    /// compressor at `level` (`ZSTD_CCtx_init_compressStream2`).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidDictionary`] if a loaded zstd-format dictionary is
    /// corrupt (checked when it was loaded).
    pub(crate) fn for_frame(&mut self, level: i32) -> Result<(Option<Arc<CDict>>, i32)> {
        match self {
            DictSlot::None => Ok((None, level)),
            DictSlot::Local { data, cdict } => {
                if cdict.is_none() {
                    *cdict = Some(Arc::new(CDict::new(data, level, true)?));
                }
                Ok((cdict.clone(), level))
            }
            // Let the cdict's compression level take priority over the
            // requested params.
            DictSlot::Prepared(cd) => Ok((Some(cd.clone()), cd.level)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_dictionary() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
        let cd = CDict::new(&data, 3, false).unwrap();
        assert_eq!(cd.dict_id, 0);
        assert_eq!(cd.level, 3);
        assert!(cd.has_window_content());
        // the window covers the content
        let w = &cd.ms.window;
        assert_eq!(w.index(w.next_src) - w.dict_limit, 5000);
        assert_eq!(cd.ms.loaded_dict_end, w.index(w.next_src));
        // dfast at level 3: tagged indices in both tables
        assert_eq!(cd.cparams.strategy, Strategy::DFast);
        assert!(
            cd.ms
                .hash_table
                .iter()
                .any(|&v| v >> SHORT_CACHE_TAG_BITS >= 2)
        );
    }

    #[test]
    fn send_sync() {
        fn check<T: Send + Sync>() {}
        check::<EncoderDictionary>();
        check::<crate::Compressor>();
        check::<crate::StreamEncoder>();
    }

    #[test]
    fn tiny_and_corrupt() {
        let cd = CDict::new(b"abc", 1, false).unwrap();
        assert!(!cd.has_window_content());
        assert_eq!(CDict::new(b"abc", 0, true).unwrap().level, 0);
        assert_eq!(CDict::new(b"abcdefgh", 0, false).unwrap().level, 3);
        let mut bad = DICT_MAGIC.to_le_bytes().to_vec();
        bad.extend_from_slice(&[1, 2, 3, 4, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(matches!(
            CDict::new(&bad, 3, false),
            Err(Error::InvalidDictionary)
        ));
        assert!(DictSlot::local(&bad).is_err());
        assert!(matches!(DictSlot::local(&[]), Ok(DictSlot::None)));
    }

    #[test]
    fn attach_cutoffs() {
        let data = alloc::vec![7u8; 1000];
        for (level, cutoff) in [(1, 8 << 10), (3, 16 << 10), (9, 32 << 10), (19, 8 << 10)] {
            let cd = CDict::new(&data, level, false).unwrap();
            assert!(should_attach(&cd, cutoff));
            assert!(!should_attach(&cd, cutoff + 1));
            assert!(should_attach(&cd, CONTENTSIZE_UNKNOWN));
        }
        let cd = CDict::new(&data, 3, false).unwrap();
        assert!(use_cdict_tables(&cd, (128 << 10) - 1));
        assert!(!use_cdict_tables(&cd, 128 << 10));
        assert!(use_cdict_tables(&cd, CONTENTSIZE_UNKNOWN));
        let local = CDict::new(&data, 3, true).unwrap();
        assert!(use_cdict_tables(&local, 1 << 30));
    }

    /// Every strategy's dictMatchState, extDict (copied or loaded tables)
    /// match finders round trip through the decoder with the dictionary,
    /// and find matches in the dictionary.
    #[test]
    fn round_trip_every_mode() {
        use super::super::compress::{FrameCompressor, compress_bound};
        use super::super::fast::tests::{MIRI, texty};
        // Miri: a smaller dictionary and input, and fewer levels, one per
        // strategy. Every level attaches (600 bytes: the dictMatchState
        // match finders take seconds per KB there); fast, dfast and greedy
        // also copy (1 KiB over their attach cutoff, under 128 KiB: no frame
        // loads the dictionary into its own tables; the extDict match
        // finders of every strategy run in the `roundtrip_ring_variants`
        // tests). Past 10 KB, the input repeats its start: long matches,
        // cheap there. Whether the dictionary helps is not checked under
        // Miri.
        let (dict_len, pieces) = if MIRI { (4000, 8) } else { (20_000, 40) };
        let dict = texty(dict_len, 1);
        // the input repeats parts of the dictionary, with other text
        let mut src = texty(if MIRI { 2000 } else { 30_000 }, 2);
        for i in 0..pieces {
            let at = (i * 4999) % (dict_len - 1000);
            src.extend_from_slice(&dict[at..at + 700]);
            src.extend_from_slice(&texty(300, 3 + i as u64));
        }
        if MIRI {
            while src.len() < 34 << 10 {
                src.extend_from_within(..5000);
            }
        } else {
            src.extend_from_slice(&texty(200_000, 50));
        }
        let ddict = crate::Dictionary::new(&dict).unwrap();
        let levels: &[i32] = if MIRI {
            &[1, 3, 5, 7, 9, 15, 16, 19, 22]
        } else {
            &[-3, 1, 2, 3, 4, 5, 7, 8, 9, 12, 15, 16, 17, 19, 22]
        };
        for &level in levels {
            let cd = Arc::new(CDict::new(&dict, level, false).unwrap());
            let sizes = if MIRI && level <= 5 {
                let cutoff = ATTACH_DICT_SIZE_CUTOFFS[cd.cparams.strategy as usize];
                alloc::vec![600, cutoff as usize + 1024]
            } else if MIRI {
                alloc::vec![600]
            } else {
                alloc::vec![5_000, 20_000, 60_000, src.len()]
            };
            let largest = sizes[sizes.len() - 1];
            for &n in &sizes {
                let data = &src[..n];
                let pledged = Some(n as u64);
                let cp = frame_cparams(cd.level, Some(&cd), pledged, 0, false);
                let mut slot = None;
                let fc = FrameCompressor::start_frame(&mut slot, cp, Some(&cd), pledged, false);
                let attached = fc.ms.dms;
                let copied_or_loaded = fc.ms.window.base_in_dict;
                assert_eq!(attached, should_attach(&cd, n as u64), "L{level} n={n}");
                assert_eq!(copied_or_loaded, !attached, "L{level} n={n}");
                let mut out = alloc::vec![0u8; compress_bound(n)];
                let c = fc.compress(&mut out, data).unwrap();
                let mut d = crate::Decompressor::with_dictionary(ddict.clone());
                assert_eq!(d.decompress(&out[..c]).unwrap(), data, "L{level} n={n}");
                if n == largest && !MIRI {
                    // the dictionary helps with its pieces
                    let plain = crate::compress(data, cd.level).unwrap();
                    assert!(c < plain.len(), "L{level} n={n}: {c} vs {}", plain.len());
                }
            }
        }
    }
}
