//! Blocks and frames (libzstd's `zstd_compress.c`: `ZSTD_entropyCompressSeqStore`,
//! `ZSTD_compressBlock_internal`, `ZSTD_compress_frameChunk`,
//! `ZSTD_writeFrameHeader`, `ZSTD_writeEpilogue`), one-shot.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::cdict::{CDict, DictSlot, frame_cparams, should_attach, use_cdict_tables};
use super::fast::{MatchState, SHORT_CACHE_TAG_BITS, build_seq_store};
use super::hist::hist_count_fast;
use super::huf_enc::HufRepeat;
use super::ldm::{LdmState, RawSeq, RawSeqStore, build_seq_store_ext, build_seq_store_ldm};
use super::literals::{HufCTables, compress_literals, min_gain};
use super::opt::DictCosts;
use super::params::{
    CONTENTSIZE_UNKNOWN, CParamMode, CParams, ParamSwitch, Strategy, adjust_cparams_internal,
    block_size_max, block_splitter_enabled, ldm_enabled, one_shot_cparams,
    resolve_row_match_finder_mode, row_match_finder_used,
};
use super::presplit::{FpStats, optimal_block_size};
use super::seq_store::{
    LongLengthType, MAX_LL, MAX_ML, REP_START_VALUE, SeqDef, SeqStore, fill_codes, max_nb_seq,
};
use super::sequences::{
    FseCTables, FseRepeat, SET_COMPRESSED, SeqFields, build_ctable, encode_sequences,
    select_encoding_type,
};
use crate::error::{Error, Result};
use crate::fse::{
    LL_DEFAULT_NORM, MAX_LL as FSE_MAX_LL, MAX_ML as FSE_MAX_ML, MAX_OFF as FSE_MAX_OFF,
    ML_DEFAULT_NORM, OF_DEFAULT_NORM,
};

pub(super) const BLOCK_HEADER_SIZE: usize = 3;
const MIN_CBLOCK_SIZE: usize = 2;
pub(super) const LONGNBSEQ: usize = 0x7F00;
pub(super) const DEFAULT_MAX_OFF: u32 = 28;
pub(super) const RLE_MAX_LENGTH: usize = 25;
const SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO: usize = 20;

impl SeqFields for SeqDef {
    #[inline(always)]
    fn off_base(&self) -> u32 {
        self.off_base
    }
    #[inline(always)]
    fn lit_length(&self) -> u32 {
        u32::from(self.lit_length)
    }
    #[inline(always)]
    fn ml_base(&self) -> u32 {
        u32::from(self.ml_base)
    }
}

/// libzstd's `ZSTD_entropyCTables_t`.
#[derive(Clone)]
pub(crate) struct EntropyTables {
    pub huf: HufCTables,
    pub fse: FseCTables,
}

impl EntropyTables {
    pub(crate) fn new() -> Self {
        EntropyTables {
            huf: HufCTables::new(),
            fse: FseCTables::new(),
        }
    }
}

/// libzstd's `ZSTD_COMPRESSBOUND`: the largest compressed size of `n` bytes.
pub const fn compress_bound(n: usize) -> usize {
    n + (n >> 8)
        + if n < (128 << 10) {
            ((128 << 10) - n) >> 11
        } else {
            0
        }
}

/// libzstd's `ZSTD_literalsCompressionIsDisabled` (default mode: only the
/// fast strategy with acceleration, i.e. negative levels).
fn literals_compression_disabled(cp: &CParams) -> bool {
    cp.strategy == Strategy::Fast && cp.target_length > 0
}

/// A run of sequences of the seq store and their literals: the whole
/// block, or a partition of it for the block splitter (libzstd derives a
/// `SeqStore_t` with moved pointers, `ZSTD_deriveSeqStoreChunk`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeqChunk {
    /// The sequences `seq_start..seq_end` (and their codes).
    pub(crate) seq_start: usize,
    pub(crate) seq_end: usize,
    /// The literals `lit_start..lit_end` of the literal buffer.
    pub(crate) lit_start: usize,
    pub(crate) lit_end: usize,
    /// The long length, relative to `seq_start` (it may be `seq_end -
    /// seq_start`, just past the chunk, as in libzstd).
    pub(crate) long_length_type: LongLengthType,
    pub(crate) long_length_pos: u32,
}

impl SeqChunk {
    /// The whole store.
    pub(crate) fn full(ss: &SeqStore) -> Self {
        SeqChunk {
            seq_start: 0,
            seq_end: ss.nb_seq,
            lit_start: 0,
            lit_end: ss.lit_len,
            long_length_type: ss.long_length_type,
            long_length_pos: ss.long_length_pos,
        }
    }

    /// The number of sequences.
    #[inline]
    pub(crate) fn nb_seq(&self) -> usize {
        self.seq_end - self.seq_start
    }
}

/// `ZSTD_seqToCodes()` on a chunk: fills the code tables for its
/// sequences. Returns libzstd's `longOffsets` (only possible on 32-bit
/// targets).
pub(crate) fn chunk_to_codes(ss: &mut SeqStore, ch: &SeqChunk) -> bool {
    let (s, e) = (ch.seq_start, ch.seq_end);
    let long_offsets = fill_codes(
        &ss.sequences[s..e],
        &mut ss.ll_code[s..e],
        &mut ss.of_code[s..e],
        &mut ss.ml_code[s..e],
    );
    // The long length may be just past a partition (libzstd writes its code
    // there too; it is the right code for that sequence).
    let p = s + ch.long_length_pos as usize;
    match ch.long_length_type {
        LongLengthType::LiteralLength if p < ss.ll_code.len() => ss.ll_code[p] = MAX_LL,
        LongLengthType::MatchLength if p < ss.ml_code.len() => ss.ml_code[p] = MAX_ML,
        _ => {}
    }
    long_offsets
}

/// libzstd's `ZSTD_symbolEncodingTypeStats_t`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SeqStats {
    pub(crate) ll_type: u32,
    pub(crate) of_type: u32,
    pub(crate) ml_type: u32,
    /// Size of the table descriptions written.
    pub(crate) size: usize,
    /// Size of the last compressed table description (0 if none).
    pub(crate) last_count_size: usize,
}

/// libzstd's `ZSTD_buildSequencesStatistics`: computes the codes of the
/// chunk's sequences (at least one), chooses each code's table encoding,
/// builds `next`'s tables and writes their descriptions to `dst`.
pub(crate) fn build_sequences_statistics(
    dst: &mut [u8],
    ss: &mut SeqStore,
    ch: &SeqChunk,
    prev: &FseCTables,
    next: &mut FseCTables,
    strategy: u32,
) -> Result<SeqStats> {
    let nb_seq = ch.nb_seq();
    debug_assert!(nb_seq != 0);
    chunk_to_codes(ss, ch);
    let mut count = [0u32; 256];
    let mut stats = SeqStats {
        ll_type: 0,
        of_type: 0,
        ml_type: 0,
        size: 0,
        last_count_size: 0,
    };
    let mut op = 0usize;
    let (ll_codes, of_codes, ml_codes) = (
        &ss.ll_code[ch.seq_start..ch.seq_end],
        &ss.of_code[ch.seq_start..ch.seq_end],
        &ss.ml_code[ch.seq_start..ch.seq_end],
    );

    // build CTable for Literal Lengths
    let mut max = FSE_MAX_LL as u32;
    let most = hist_count_fast(&mut count, &mut max, ll_codes) as usize;
    next.ll_repeat = prev.ll_repeat;
    stats.ll_type = select_encoding_type(
        &mut next.ll_repeat,
        &count,
        max,
        most,
        nb_seq,
        9,
        &prev.ll,
        &LL_DEFAULT_NORM,
        6,
        true,
        strategy,
    );
    let n = build_ctable(
        &mut dst[op..],
        &mut next.ll,
        9,
        stats.ll_type,
        &mut count,
        max,
        ll_codes,
        &LL_DEFAULT_NORM,
        6,
        FSE_MAX_LL as u32,
        &prev.ll,
    )?;
    if stats.ll_type == SET_COMPRESSED {
        stats.last_count_size = n;
    }
    op += n;

    // build CTable for Offsets
    let mut max = FSE_MAX_OFF as u32;
    let most = hist_count_fast(&mut count, &mut max, of_codes) as usize;
    // the basic table can only be used if max <= DefaultMaxOff, otherwise
    // the offsets are too large
    let default_allowed = max <= DEFAULT_MAX_OFF;
    next.of_repeat = prev.of_repeat;
    stats.of_type = select_encoding_type(
        &mut next.of_repeat,
        &count,
        max,
        most,
        nb_seq,
        8,
        &prev.of,
        &OF_DEFAULT_NORM,
        5,
        default_allowed,
        strategy,
    );
    let n = build_ctable(
        &mut dst[op..],
        &mut next.of,
        8,
        stats.of_type,
        &mut count,
        max,
        of_codes,
        &OF_DEFAULT_NORM,
        5,
        DEFAULT_MAX_OFF,
        &prev.of,
    )?;
    if stats.of_type == SET_COMPRESSED {
        stats.last_count_size = n;
    }
    op += n;

    // build CTable for MatchLengths
    let mut max = FSE_MAX_ML as u32;
    let most = hist_count_fast(&mut count, &mut max, ml_codes) as usize;
    next.ml_repeat = prev.ml_repeat;
    stats.ml_type = select_encoding_type(
        &mut next.ml_repeat,
        &count,
        max,
        most,
        nb_seq,
        9,
        &prev.ml,
        &ML_DEFAULT_NORM,
        6,
        true,
        strategy,
    );
    let n = build_ctable(
        &mut dst[op..],
        &mut next.ml,
        9,
        stats.ml_type,
        &mut count,
        max,
        ml_codes,
        &ML_DEFAULT_NORM,
        6,
        FSE_MAX_ML as u32,
        &prev.ml,
    )?;
    if stats.ml_type == SET_COMPRESSED {
        stats.last_count_size = n;
    }
    op += n;
    stats.size = op;
    Ok(stats)
}

/// libzstd's `ZSTD_entropyCompressSeqStore` (with `_internal` and
/// `_wExtLitBuffer`) on the chunk `ch` of `ss`, from a block (or
/// partition) of `block_size` bytes. Returns 0 when it should be stored
/// raw.
pub(crate) fn entropy_compress_seq_store(
    dst: &mut [u8],
    ss: &mut SeqStore,
    ch: &SeqChunk,
    prev: &EntropyTables,
    next: &mut EntropyTables,
    cp: &CParams,
    block_size: usize,
) -> Result<usize> {
    let r = entropy_compress_internal(dst, ss, ch, prev, next, cp);
    let c = match r {
        Ok(0) => return Ok(0),
        Err(Error::OutputTooSmall) if block_size <= dst.len() => return Ok(0),
        Err(e) => return Err(e),
        Ok(c) => c,
    };
    let max_c = block_size - min_gain(block_size, cp.strategy as u32);
    if c >= max_c {
        return Ok(0);
    }
    Ok(c)
}

fn entropy_compress_internal(
    dst: &mut [u8],
    ss: &mut SeqStore,
    ch: &SeqChunk,
    prev: &EntropyTables,
    next: &mut EntropyTables,
    cp: &CParams,
) -> Result<usize> {
    let strategy = cp.strategy as u32;
    let nb_seq = ch.nb_seq();
    let lit_size = ch.lit_end - ch.lit_start;
    let mut op = 0usize;
    {
        let suspect = nb_seq == 0 || lit_size / nb_seq >= SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO;
        op += compress_literals(
            dst,
            &ss.lit_buf[ch.lit_start..ch.lit_end],
            &prev.huf,
            &mut next.huf,
            strategy,
            literals_compression_disabled(cp),
            suspect,
        )?;
    }
    if dst.len() - op < 3 + 1 {
        return Err(Error::OutputTooSmall);
    }
    if nb_seq < 128 {
        dst[op] = nb_seq as u8;
        op += 1;
    } else if nb_seq < LONGNBSEQ {
        dst[op] = ((nb_seq >> 8) + 0x80) as u8;
        dst[op + 1] = nb_seq as u8;
        op += 2;
    } else {
        dst[op] = 0xFF;
        dst[op + 1..op + 3].copy_from_slice(&((nb_seq - LONGNBSEQ) as u16).to_le_bytes());
        op += 3;
    }
    if nb_seq == 0 {
        next.fse.clone_from(&prev.fse);
        return Ok(op);
    }

    let seq_head = op;
    op += 1;
    let stats =
        build_sequences_statistics(&mut dst[op..], ss, ch, &prev.fse, &mut next.fse, strategy)?;
    op += stats.size;
    dst[seq_head] = ((stats.ll_type << 6) + (stats.of_type << 4) + (stats.ml_type << 2)) as u8;

    let (s, e) = (ch.seq_start, ch.seq_end);
    let bitstream = encode_sequences(
        &mut dst[op..],
        &next.fse.ml,
        &ss.ml_code[s..e],
        &next.fse.of,
        &ss.of_code[s..e],
        &next.fse.ll,
        &ss.ll_code[s..e],
        &ss.sequences[s..e],
    )?;
    op += bitstream;
    // zstd <= 1.3.4 decoders reject a last table description under 4 bytes
    // together with the bitstream: libzstd stores such blocks raw.
    if stats.last_count_size != 0 && stats.last_count_size + bitstream < 4 {
        return Ok(0);
    }
    Ok(op)
}

/// libzstd's `ZSTD_isRLE`.
pub(super) fn is_rle(src: &[u8]) -> bool {
    src.iter().all(|&b| b == src[0])
}

/// libzstd's `ZSTD_compressionStage_e` (`cctx->stage`) for a started frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameStage {
    /// `ZSTDcs_init`: the frame header is not written yet.
    Init,
    /// `ZSTDcs_ongoing`: blocks are being written.
    Ongoing,
    /// `ZSTDcs_ending`: the last block was written.
    Ending,
}

/// Compression state for one frame at a time (libzstd's `ZSTD_CCtx`
/// without the streaming buffers, see [`StreamEncoder`](super::stream::StreamEncoder)).
pub(crate) struct FrameCompressor {
    pub(super) cp: CParams,
    pub(super) ms: MatchState,
    pub(super) ss: SeqStore,
    /// `blockState.prevCBlock->entropy`
    pub(super) prev: Box<EntropyTables>,
    /// `blockState.nextCBlock->entropy`
    pub(super) next: Box<EntropyTables>,
    /// `blockState.prevCBlock->rep`
    pub(super) rep: [u32; 3],
    fp: Box<FpStats>,
    /// `isFirstBlock`
    pub(super) first_block: bool,
    /// The post-block splitter is enabled (`ZSTD_blockSplitterEnabled`).
    split: bool,
    /// `blockSizeMax`
    pub(super) block_size_max: usize,
    /// `stage`
    pub(super) stage: FrameStage,
    /// `pledgedSrcSizePlusOne - 1`: the frame's content size, if known.
    pledged: Option<u64>,
    /// `consumedSrcSize` and `producedCSize` for the frame.
    consumed: u64,
    produced: u64,
    /// `dictID`: the frame's dictionary ID (0: none written).
    dict_id: u32,
    /// `ldmState` (kept between frames for its tables), used when
    /// [`ldm_on`](Self::ldm_on).
    pub(super) ldm: Option<Box<LdmState>>,
    /// Long distance matching is enabled for the frame
    /// (`appliedParams.ldmParams.enableLdm`).
    pub(super) ldm_on: bool,
    /// `externSeqStore`: long matches given from outside (a multithreaded
    /// job's, see the `mt` module), used while not empty.
    pub(super) ext: RawSeqStore,
    /// Scratch space for [`build_seq_store_ext`].
    ext_view: Vec<RawSeq>,
}

impl FrameCompressor {
    pub(crate) fn new(level: i32, src_size: usize) -> Self {
        let cp = one_shot_cparams(level, src_size as u64);
        Self::with_params(cp, Some(src_size as u64), ldm_enabled(&cp))
    }

    /// A compressor for a frame of `pledged` bytes (`None`: unknown) with
    /// the resolved parameters `cp`, with long distance matching if `ldm`.
    pub(crate) fn with_params(cp: CParams, pledged: Option<u64>, ldm: bool) -> Self {
        let bsm = block_size_max(&cp, pledged.unwrap_or(CONTENTSIZE_UNKNOWN));
        FrameCompressor {
            ldm: ldm.then(|| Box::new(LdmState::new(&cp, bsm))),
            ldm_on: ldm,
            cp,
            ms: MatchState::new(cp),
            ss: SeqStore::new(bsm, cp.min_match),
            prev: Box::new(EntropyTables::new()),
            next: Box::new(EntropyTables::new()),
            rep: REP_START_VALUE,
            fp: Box::new(FpStats::new()),
            first_block: true,
            split: block_splitter_enabled(&cp),
            block_size_max: bsm,
            stage: FrameStage::Init,
            pledged,
            consumed: 0,
            produced: 0,
            dict_id: 0,
            ext: RawSeqStore::default(),
            ext_view: Vec::new(),
        }
    }

    /// Prepares for a new frame of `src_size` bytes at `level`, reusing the
    /// buffers (libzstd's `ZSTD_resetCCtx_internal` with a clean match state
    /// and `ZSTD_reset_compressedBlockState`).
    pub(crate) fn reset(&mut self, level: i32, src_size: usize) {
        let cp = one_shot_cparams(level, src_size as u64);
        self.reset_with_params(cp, Some(src_size as u64), ldm_enabled(&cp), true);
    }

    /// As [`reset`](Self::reset), with resolved parameters, a frame of
    /// `pledged` bytes (`None`: unknown) and long distance matching if
    /// `ldm`. With `keep_indices` (no dictionary follows), the match state
    /// continues its indices when it can
    /// ([`MatchState::reset_keep_indices`]).
    pub(crate) fn reset_with_params(
        &mut self,
        cp: CParams,
        pledged: Option<u64>,
        ldm: bool,
        keep_indices: bool,
    ) {
        let use_row = row_match_finder_used(
            cp.strategy,
            resolve_row_match_finder_mode(ParamSwitch::Auto, &cp),
        );
        self.reset_full(cp, pledged, use_row, &cp, ldm, keep_indices);
    }

    /// `ZSTD_resetCCtx_internal`: [`reset_with_params`](Self::reset_with_params)
    /// with the row match finder use given and the post-block splitter
    /// resolved from the frame's parameters `frame_cp` (with a CDict, `cp`
    /// holds the CDict's table parameters instead, from which the LDM
    /// parameters are derived too).
    fn reset_full(
        &mut self,
        cp: CParams,
        pledged: Option<u64>,
        use_row: bool,
        frame_cp: &CParams,
        ldm: bool,
        keep_indices: bool,
    ) {
        let bsm = block_size_max(&cp, pledged.unwrap_or(CONTENTSIZE_UNKNOWN));
        self.ldm_on = ldm;
        if ldm {
            match self.ldm.as_deref_mut() {
                Some(l) => l.reset(&cp, bsm),
                None => self.ldm = Some(Box::new(LdmState::new(&cp, bsm))),
            }
        }
        self.cp = cp;
        if keep_indices {
            self.ms.reset_keep_indices(cp, use_row);
        } else {
            self.ms.reset_with(cp, use_row, false);
        }
        if let Some(o) = self.ms.opt.as_deref_mut() {
            o.dict_costs = None;
        }
        if self.ss.max_nb_lit < bsm || self.ss.max_nb_seq < max_nb_seq(bsm, cp.min_match) {
            self.ss = SeqStore::new(bsm, cp.min_match);
        } else {
            self.ss.reset();
        }
        // Table contents don't matter while the repeat modes say none.
        for e in [&mut self.prev, &mut self.next] {
            e.huf.repeat = HufRepeat::None;
            e.fse.ll_repeat = FseRepeat::None;
            e.fse.of_repeat = FseRepeat::None;
            e.fse.ml_repeat = FseRepeat::None;
        }
        self.rep = REP_START_VALUE;
        self.first_block = true;
        self.split = block_splitter_enabled(frame_cp);
        self.block_size_max = bsm;
        self.stage = FrameStage::Init;
        self.pledged = pledged;
        self.consumed = 0;
        self.produced = 0;
        self.dict_id = 0;
        self.ext.seq.clear();
        self.ext.pos = 0;
        self.ext.pos_in_sequence = 0;
    }

    /// Starts a frame of `pledged` bytes (`None`: unknown) with the frame
    /// parameters `frame_cp` (see [`frame_cparams`](super::cdict::frame_cparams))
    /// and the dictionary `cdict`: libzstd's `ZSTD_compressBegin_internal`,
    /// which starts from the CDict's tables (attached or copied, see the
    /// `cdict` module) or loads its content into tables with the frame's
    /// parameters. Reuses `slot`'s compressor, or makes one. With `ldm`,
    /// long distance matching is enabled; a raw-content dictionary loaded
    /// into the tables is loaded into the LDM table too.
    pub(crate) fn start_frame<'a>(
        slot: &'a mut Option<Box<FrameCompressor>>,
        frame_cp: CParams,
        cdict: Option<&Arc<CDict>>,
        pledged: Option<u64>,
        ldm: bool,
    ) -> &'a mut FrameCompressor {
        Self::start_frame_impl(slot, frame_cp, cdict, pledged, ldm, true)
    }

    /// As [`start_frame`](Self::start_frame) without a dictionary, for a
    /// frame whose prefix is then loaded into the match state
    /// ([`load_prefix`](super::cdict::load_prefix)): the match state's
    /// indices restart (no [`MatchState::reset_keep_indices`]).
    pub(crate) fn start_frame_for_prefix(
        slot: &mut Option<Box<FrameCompressor>>,
        frame_cp: CParams,
        pledged: Option<u64>,
        ldm: bool,
    ) -> &mut FrameCompressor {
        Self::start_frame_impl(slot, frame_cp, None, pledged, ldm, false)
    }

    fn start_frame_impl<'a>(
        slot: &'a mut Option<Box<FrameCompressor>>,
        frame_cp: CParams,
        cdict: Option<&Arc<CDict>>,
        pledged: Option<u64>,
        ldm: bool,
        keep_indices: bool,
    ) -> &'a mut FrameCompressor {
        let fc = match slot {
            Some(f) => f,
            None => {
                // (no LDM tables yet: the reset below makes them)
                let f = slot.insert(Box::new(FrameCompressor::with_params(
                    frame_cp, pledged, false,
                )));
                // a new context: its one reset below must advance the hash
                // salt from 0
                f.ms.clear_hash_salt();
                f
            }
        };
        let size = pledged.unwrap_or(CONTENTSIZE_UNKNOWN);
        match cdict {
            Some(cd) if use_cdict_tables(cd, size) => {
                if should_attach(cd, size) {
                    fc.attach_cdict(frame_cp, cd, pledged, ldm);
                } else {
                    fc.copy_cdict(frame_cp, cd, pledged, ldm);
                }
            }
            _ => {
                fc.reset_with_params(frame_cp, pledged, ldm, keep_indices && cdict.is_none());
                if let Some(cd) = cdict {
                    // ZSTD_compress_insertDictionary
                    cd.load_into(&mut fc.ms);
                    // ZSTD_loadDictionaryContent's LDM part (a zstd-format
                    // dictionary's content is loaded without the LDM state)
                    if ldm && cd.is_raw_content() {
                        let l = fc.ldm.as_deref_mut().expect("LDM state");
                        l.load_dictionary(cd);
                    }
                    fc.take_block_state(cd);
                }
            }
        }
        fc
    }

    /// `ZSTD_resetCCtx_byAttachingCDict()`: the context's tables are sized
    /// for the source only, the match finders search the CDict's too
    /// (dictMatchState).
    fn attach_cdict(
        &mut self,
        frame_cp: CParams,
        cd: &Arc<CDict>,
        pledged: Option<u64>,
        ldm: bool,
    ) {
        let size = pledged.unwrap_or(CONTENTSIZE_UNKNOWN);
        // Resize working context table params for input only, since the
        // dict has its own tables.
        let mut cp = adjust_cparams_internal(
            cd.cparams,
            size,
            cd.content.len() as u64,
            CParamMode::AttachDict,
            resolve_row_match_finder_mode(ParamSwitch::Auto, &frame_cp),
        );
        cp.window_log = frame_cp.window_log;
        self.reset_full(cp, pledged, cd.use_row, &frame_cp, ldm, false);
        debug_assert!(self.cp.strategy == cd.cparams.strategy);
        // don't even attach dictionaries with no contents
        if cd.has_window_content() {
            let w = &cd.ms.window;
            let cdict_end = w.index(w.next_src);
            let ms = &mut self.ms;
            ms.dict = Some(cd.clone());
            ms.dms = true;
            // prep working match state so dict matches never have negative
            // indices when they are translated to the working context's
            // index space
            if ms.window.dict_limit < cdict_end {
                ms.window.next_src = ms.window.base.wrapping_add(cdict_end as usize);
                ms.window.clear();
            }
            // loadedDictEnd is expressed within the referential of the
            // active context
            ms.loaded_dict_end = ms.window.dict_limit;
        }
        self.take_block_state(cd);
    }

    /// `ZSTD_resetCCtx_byCopyingCDict()`: the context gets the CDict's
    /// table parameters (with the frame's window), tables and window; the
    /// dictionary is then its extDict.
    fn copy_cdict(&mut self, frame_cp: CParams, cd: &Arc<CDict>, pledged: Option<u64>, ldm: bool) {
        let mut cp = cd.cparams;
        cp.window_log = frame_cp.window_log;
        self.reset_full(cp, pledged, cd.use_row, &frame_cp, ldm, false);
        let ms = &mut self.ms;
        let src = &cd.ms;
        // copy tables, removing the tags of a fast or dfast CDict's
        let tagged = matches!(cp.strategy, Strategy::Fast | Strategy::DFast);
        let copy = |dst: &mut [u32], src: &[u32]| {
            debug_assert_eq!(dst.len(), src.len());
            if tagged {
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = s >> SHORT_CACHE_TAG_BITS;
                }
            } else {
                dst.copy_from_slice(src);
            }
        };
        copy(&mut ms.hash_table, &src.hash_table);
        copy(&mut ms.chain_table, &src.chain_table);
        if cd.use_row {
            ms.tag_table.copy_from_slice(&src.tag_table);
            ms.hash_salt = src.hash_salt;
        }
        // copy dictionary offsets (the window is in the CDict's buffer)
        ms.window = src.window;
        ms.next_to_update = src.next_to_update;
        ms.loaded_dict_end = src.loaded_dict_end;
        ms.dict = Some(cd.clone());
        self.take_block_state(cd);
    }

    /// The dictionary's ID, and its entropy tables and repcodes as the
    /// previous block's (`cdict->cBlockState`), with what the optimal
    /// parser seeds its statistics from.
    fn take_block_state(&mut self, cd: &CDict) {
        self.dict_id = cd.dict_id;
        (*self.prev).clone_from(&cd.entropy);
        self.rep = cd.rep;
        if let Some(o) = self.ms.opt.as_deref_mut() {
            o.dict_costs = DictCosts::new(&cd.entropy);
        }
    }

    /// `ZSTD_buildSeqStore()` for the block `src[start..end]`, with the
    /// long distance matcher when enabled (see
    /// [`build_seq_store`] for `rep` and the result).
    ///
    /// # Errors
    ///
    /// Only with LDM, see [`LdmState::generate_sequences`].
    #[inline]
    pub(super) fn build_block_seq_store(
        &mut self,
        rep: &mut [u32; 3],
        src: &[u8],
        start: usize,
        end: usize,
    ) -> Result<bool> {
        if !self.ext.seq.is_empty() {
            return Ok(build_seq_store_ext(
                &mut self.ms,
                &mut self.ss,
                rep,
                &mut self.ext,
                &mut self.ext_view,
                src,
                start,
                end,
            ));
        }
        match self.ldm.as_deref_mut().filter(|_| self.ldm_on) {
            None => Ok(build_seq_store(
                &mut self.ms,
                &mut self.ss,
                rep,
                src,
                start,
                end,
            )),
            Some(l) => build_seq_store_ldm(&mut self.ms, &mut self.ss, rep, l, src, start, end),
        }
    }

    /// libzstd's `ZSTD_compressBlock_internal`: 0 = store raw, 1 = RLE
    /// (`dst[0]` holds the byte), else a compressed block's size.
    fn compress_block(
        &mut self,
        dst: &mut [u8],
        src: &[u8],
        start: usize,
        end: usize,
    ) -> Result<usize> {
        let mut next_rep = self.rep;
        let c_size = if !self.build_block_seq_store(&mut next_rep, src, start, end)? {
            0
        } else {
            let ch = SeqChunk::full(&self.ss);
            let mut c = entropy_compress_seq_store(
                dst,
                &mut self.ss,
                &ch,
                &self.prev,
                &mut self.next,
                &self.cp,
                end - start,
            )?;
            if !self.first_block && c < RLE_MAX_LENGTH && is_rle(&src[start..end]) {
                c = 1;
                dst[0] = src[start];
            }
            c
        };
        if c_size > 1 {
            core::mem::swap(&mut self.prev, &mut self.next);
            self.rep = next_rep;
        }
        if self.prev.fse.of_repeat == FseRepeat::Valid {
            self.prev.fse.of_repeat = FseRepeat::Check;
        }
        Ok(c_size)
    }

    /// libzstd's `ZSTD_compress_frameChunk`: compresses `buf[start..end]`
    /// (already in the window) as blocks of at most `blockSizeMax` bytes,
    /// the pre-splitter deciding their sizes; with `last_chunk`, the last
    /// one ends the frame. Returns the size written.
    fn frame_chunk(
        &mut self,
        dst: &mut [u8],
        buf: &[u8],
        start: usize,
        end: usize,
        last_chunk: bool,
    ) -> Result<usize> {
        let bsm = self.block_size_max;
        let mut savings = self.consumed as i64 - self.produced as i64;
        let mut op = 0usize;
        let mut pos = start;
        while pos < end {
            let bs = optimal_block_size(
                &buf[pos..end],
                bsm,
                0,
                self.cp.strategy,
                savings,
                &mut self.fp,
            );
            let last = u32::from(last_chunk && bs == end - pos);
            if dst.len() - op < BLOCK_HEADER_SIZE + MIN_CBLOCK_SIZE + 1 {
                return Err(Error::OutputTooSmall);
            }
            self.ms.prepare_block(pos, bs);
            let c_size = if self.split {
                self.compress_block_split(&mut dst[op..], buf, pos, pos + bs, last)?
            } else {
                let c =
                    self.compress_block(&mut dst[op + BLOCK_HEADER_SIZE..], buf, pos, pos + bs)?;
                if c == 0 {
                    // ZSTD_noCompressBlock
                    if bs + BLOCK_HEADER_SIZE > dst.len() - op {
                        return Err(Error::OutputTooSmall);
                    }
                    let h = last + ((bs as u32) << 3);
                    dst[op..op + 3].copy_from_slice(&h.to_le_bytes()[..3]);
                    dst[op + 3..op + 3 + bs].copy_from_slice(&buf[pos..pos + bs]);
                    bs + BLOCK_HEADER_SIZE
                } else {
                    let h = if c == 1 {
                        last + (1 << 1) + ((bs as u32) << 3)
                    } else {
                        last + (2 << 1) + ((c as u32) << 3)
                    };
                    dst[op..op + 3].copy_from_slice(&h.to_le_bytes()[..3]);
                    c + BLOCK_HEADER_SIZE
                }
            };
            savings += bs as i64 - c_size as i64;
            pos += bs;
            op += c_size;
            self.first_block = false;
        }
        if last_chunk && op > 0 {
            self.stage = FrameStage::Ending;
        }
        Ok(op)
    }

    /// libzstd's `ZSTD_compressContinue_internal` (frame mode): writes the
    /// frame header on the first call, appends `buf[start..end]` to the
    /// window and compresses it. Returns the size written.
    ///
    /// # Errors
    ///
    /// [`Error::OutputTooSmall`], or [`Error::SrcSizeWrong`] when more than
    /// the pledged size was given.
    pub(super) fn compress_continue(
        &mut self,
        dst: &mut [u8],
        buf: &[u8],
        start: usize,
        end: usize,
        last_chunk: bool,
    ) -> Result<usize> {
        let mut fh_size = 0;
        if self.stage == FrameStage::Init {
            fh_size = write_frame_header(dst, &self.cp, self.pledged, self.dict_id)?;
            self.stage = FrameStage::Ongoing;
        }
        if start == end {
            return Ok(fh_size);
        }
        self.ms.update_window(start, end - start);
        if self.ldm_on {
            let l = self.ldm.as_deref_mut().expect("LDM state");
            l.update_window(start, end - start);
        }
        let c_size = self.frame_chunk(&mut dst[fh_size..], buf, start, end, last_chunk)?;
        self.consumed += (end - start) as u64;
        self.produced += (c_size + fh_size) as u64;
        if self.pledged.is_some_and(|p| self.consumed > p) {
            return Err(Error::SrcSizeWrong);
        }
        Ok(c_size + fh_size)
    }

    /// libzstd's `ZSTD_compressEnd_public`: [`compress_continue`](Self::compress_continue)
    /// with the last chunk, then `ZSTD_writeEpilogue` (an empty last block
    /// if the last block isn't written yet).
    ///
    /// # Errors
    ///
    /// As `compress_continue`, and [`Error::SrcSizeWrong`] when the frame
    /// holds less than the pledged size.
    pub(super) fn compress_end(
        &mut self,
        dst: &mut [u8],
        buf: &[u8],
        start: usize,
        end: usize,
    ) -> Result<usize> {
        let mut op = self.compress_continue(dst, buf, start, end, true)?;
        debug_assert!(self.stage != FrameStage::Init);
        if self.stage != FrameStage::Ending {
            // one last empty raw block, marked last
            if dst.len() - op < BLOCK_HEADER_SIZE {
                return Err(Error::OutputTooSmall);
            }
            dst[op..op + 3].copy_from_slice(&1u32.to_le_bytes()[..3]);
            op += BLOCK_HEADER_SIZE;
        }
        self.stage = FrameStage::Init;
        if self.pledged.is_some_and(|p| self.consumed != p) {
            return Err(Error::SrcSizeWrong);
        }
        Ok(op)
    }

    /// Compresses `src` as one frame into `dst`; returns the frame size.
    pub(crate) fn compress(&mut self, dst: &mut [u8], src: &[u8]) -> Result<usize> {
        debug_assert_eq!(self.pledged, Some(src.len() as u64));
        self.compress_end(dst, src, 0, src.len())
    }
}

/// libzstd's `ZSTD_writeFrameHeader` (no checksum): with the content size
/// when `pledged` holds it, and the dictionary ID unless it is 0.
fn write_frame_header(
    dst: &mut [u8],
    cp: &CParams,
    pledged: Option<u64>,
    dict_id: u32,
) -> Result<usize> {
    if dst.len() < 18 {
        return Err(Error::OutputTooSmall);
    }
    let window_size = 1u64 << cp.window_log;
    let src_size = pledged.unwrap_or(0);
    let content_size_flag = pledged.is_some();
    let single = content_size_flag && window_size >= src_size;
    let wlog_byte = ((cp.window_log - 10) << 3) as u8;
    let fcs_code = if content_size_flag {
        u8::from(src_size >= 256)
            + u8::from(src_size >= 65536 + 256)
            + u8::from(src_size >= 0xFFFF_FFFF)
    } else {
        0
    };
    dst[..4].copy_from_slice(&crate::frame::MAGIC.to_le_bytes());
    let dict_id_size_code =
        u8::from(dict_id > 0) + u8::from(dict_id >= 256) + u8::from(dict_id >= 65536);
    dst[4] = dict_id_size_code + (u8::from(single) << 5) + (fcs_code << 6);
    let mut pos = 5;
    if !single {
        dst[pos] = wlog_byte;
        pos += 1;
    }
    match dict_id_size_code {
        0 => {}
        1 => {
            dst[pos] = dict_id as u8;
            pos += 1;
        }
        2 => {
            dst[pos..pos + 2].copy_from_slice(&(dict_id as u16).to_le_bytes());
            pos += 2;
        }
        _ => {
            dst[pos..pos + 4].copy_from_slice(&dict_id.to_le_bytes());
            pos += 4;
        }
    }
    match fcs_code {
        0 => {
            if single {
                dst[pos] = src_size as u8;
                pos += 1;
            }
        }
        1 => {
            dst[pos..pos + 2].copy_from_slice(&((src_size - 256) as u16).to_le_bytes());
            pos += 2;
        }
        2 => {
            dst[pos..pos + 4].copy_from_slice(&(src_size as u32).to_le_bytes());
            pos += 4;
        }
        _ => {
            dst[pos..pos + 8].copy_from_slice(&src_size.to_le_bytes());
            pos += 8;
        }
    }
    Ok(pos)
}

/// Compresses `src` at `level` into `dst` with `fc` (or a new compressor).
/// With `fresh`, a reused `fc` behaves like a new one, as libzstd's
/// one-shot `ZSTD_compress` (a new context per call); otherwise like a
/// reused libzstd context, whose row match finder hash salt changes from
/// frame to frame.
pub(crate) fn compress_to_slice_with(
    fc: &mut Option<Box<FrameCompressor>>,
    src: &[u8],
    dst: &mut [u8],
    level: i32,
    fresh: bool,
) -> Result<usize> {
    let fc = match fc {
        Some(f) => {
            if fresh {
                f.ms.clear_hash_salt();
            }
            f.reset(level, src.len());
            f
        }
        None => fc.insert(Box::new(FrameCompressor::new(level, src.len()))),
    };
    fc.compress(dst, src)
}

/// As [`compress_to_slice_with`], into a new vector.
pub(crate) fn compress_with(
    fc: &mut Option<Box<FrameCompressor>>,
    src: &[u8],
    level: i32,
    fresh: bool,
) -> Result<Vec<u8>> {
    let mut out = alloc::vec![0u8; compress_bound(src.len())];
    let n = compress_to_slice_with(fc, src, &mut out, level, fresh)?;
    out.truncate(n);
    Ok(out)
}

/// As [`compress_to_slice_with`], with the compressor's dictionary `dict`
/// (libzstd's `ZSTD_compress2` on a context with a loaded or referenced
/// dictionary; with `fresh`, a new context).
pub(crate) fn compress_dict_to_slice_with(
    fc: &mut Option<Box<FrameCompressor>>,
    dict: &mut DictSlot,
    src: &[u8],
    dst: &mut [u8],
    level: i32,
    fresh: bool,
) -> Result<usize> {
    let (cd, level) = dict.for_frame(level)?;
    let pledged = Some(src.len() as u64);
    let frame_cp = frame_cparams(level, cd.as_deref(), pledged, 0, false);
    if let Some(f) = fc.as_deref_mut().filter(|_| fresh) {
        f.ms.clear_hash_salt();
    }
    let ldm = ldm_enabled(&frame_cp);
    FrameCompressor::start_frame(fc, frame_cp, cd.as_ref(), pledged, ldm).compress(dst, src)
}

/// As [`compress_dict_to_slice_with`], into a new vector.
pub(crate) fn compress_dict_with(
    fc: &mut Option<Box<FrameCompressor>>,
    dict: &mut DictSlot,
    src: &[u8],
    level: i32,
    fresh: bool,
) -> Result<Vec<u8>> {
    let mut out = alloc::vec![0u8; compress_bound(src.len())];
    let n = compress_dict_to_slice_with(fc, dict, src, &mut out, level, fresh)?;
    out.truncate(n);
    Ok(out)
}
