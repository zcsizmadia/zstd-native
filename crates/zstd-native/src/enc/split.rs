//! The post-block splitter (libzstd's `zstd_compress.c`:
//! `ZSTD_compressBlock_splitBlock`, `ZSTD_compressBlock_splitBlock_internal`,
//! `ZSTD_deriveBlockSplits`, `ZSTD_deriveBlockSplitsHelper`,
//! `ZSTD_deriveSeqStoreChunk`, `ZSTD_buildBlockEntropyStats`,
//! `ZSTD_estimateBlockSize*`, `ZSTD_seqStore_resolveOffCodes`,
//! `ZSTD_compressSeqStore_singleBlock`).
//!
//! libzstd enables it by default for `btopt` and above with
//! `windowLog >= 17` (`ZSTD_resolveBlockSplitterMode`). The match finder
//! runs on the whole block; then the sequences are split recursively in
//! halves while the estimated compressed size of the two halves is smaller
//! than the whole (at least 300 sequences per half-to-be), and each
//! partition is emitted as its own block, with its own entropy tables.
//!
//! A partition stored raw or as RLE breaks the repcode history the
//! following partitions' sequences were found with: two histories are
//! kept (the compressor's, `cRep`, updated by every partition, and the
//! decoder's, `dRep`, only by the compressed ones), and a repcode that
//! resolves differently in both is replaced by its raw offset.

use super::compress::{
    BLOCK_HEADER_SIZE, FrameCompressor, LONGNBSEQ, RLE_MAX_LENGTH, SeqChunk, SeqStats,
    build_sequences_statistics, entropy_compress_seq_store, is_rle,
};
use super::fse_enc::FseCTable;
use super::hist::{hist_count, hist_count_fast};
use super::huf_enc::{
    HUF_FLAGS_OPTIMAL_DEPTH, HufCTable, HufRepeat, huf_build_ctable, huf_estimate_compressed_size,
    huf_optimal_table_log, huf_validate_ctable, huf_write_ctable,
};
use super::literals::{HufCTables, STRAT_BTULTRA};
use super::seq_store::{
    LongLengthType, MINMATCH, REP_NUM, SeqStore, offbase_is_repcode, update_rep,
};
use super::sequences::{
    COST_ERROR, FseRepeat, SET_BASIC, SET_COMPRESSED, SET_REPEAT, SET_RLE, cross_entropy_cost,
    fse_bit_cost,
};
use crate::error::{Error, Result};
use crate::fse::{
    LL_BITS, LL_DEFAULT_NORM, MAX_LL, MAX_ML, MAX_OFF, ML_BITS, ML_DEFAULT_NORM, OF_DEFAULT_NORM,
};

/// `MIN_SEQUENCES_BLOCK_SPLITTING`: no split of fewer sequences.
const MIN_SEQUENCES_BLOCK_SPLITTING: usize = 300;
/// `ZSTD_MAX_NB_BLOCK_SPLITS`.
const MAX_NB_BLOCK_SPLITS: usize = 196;
/// `ZSTD_MAX_HUF_HEADER_SIZE`: `hufDesBuffer`'s size.
const MAX_HUF_HEADER_SIZE: usize = 128;
/// `ZSTD_MAX_FSE_HEADERS_SIZE`: `fseTablesBuffer`'s size.
const MAX_FSE_HEADERS_SIZE: usize =
    ((MAX_ML + 1) * 9 + (MAX_LL + 1) * 9 + (MAX_OFF + 1) * 8).div_ceil(8);
/// `LitHufLog`.
const LIT_HUF_LOG: u32 = 11;
/// `COMPRESS_LITERALS_SIZE_MIN`.
const COMPRESS_LITERALS_SIZE_MIN: usize = 63;

/// `ZSTD_countSeqStoreLiteralsBytes()` for the chunk's sequences.
fn count_literals_bytes(ss: &SeqStore, ch: &SeqChunk) -> usize {
    let mut n = 0usize;
    for (i, s) in ss.sequences[ch.seq_start..ch.seq_end].iter().enumerate() {
        n += usize::from(s.lit_length);
        if i == ch.long_length_pos as usize && ch.long_length_type == LongLengthType::LiteralLength
        {
            n += 0x10000;
        }
    }
    n
}

/// `ZSTD_countSeqStoreMatchBytes()` for the chunk's sequences.
fn count_match_bytes(ss: &SeqStore, ch: &SeqChunk) -> usize {
    let mut n = 0usize;
    for (i, s) in ss.sequences[ch.seq_start..ch.seq_end].iter().enumerate() {
        n += usize::from(s.ml_base) + MINMATCH as usize;
        if i == ch.long_length_pos as usize && ch.long_length_type == LongLengthType::MatchLength {
            n += 0x10000;
        }
    }
    n
}

/// `ZSTD_deriveSeqStoreChunk()`: the sequences `start_idx..end_idx` of
/// the block, with their literals (the last chunk also gets the block's
/// last literals).
fn derive_chunk(ss: &SeqStore, start_idx: usize, end_idx: usize) -> SeqChunk {
    let full = SeqChunk::full(ss);
    let mut lit_start = 0;
    if start_idx > 0 {
        lit_start = count_literals_bytes(
            ss,
            &SeqChunk {
                seq_end: start_idx,
                ..full
            },
        );
    }
    // Move the long length into the chunk's numbering if it is inside (or
    // just past) it.
    let (mut llt, mut llpos) = (full.long_length_type, full.long_length_pos);
    if llt != LongLengthType::None {
        if (llpos as usize) < start_idx || (llpos as usize) > end_idx {
            llt = LongLengthType::None;
        } else {
            llpos -= start_idx as u32;
        }
    }
    let mut ch = SeqChunk {
        seq_start: start_idx,
        seq_end: end_idx,
        lit_start,
        lit_end: full.lit_end,
        long_length_type: llt,
        long_length_pos: llpos,
    };
    if end_idx != ss.nb_seq {
        ch.lit_end = lit_start + count_literals_bytes(ss, &ch);
    }
    ch
}

/// `ZSTD_resolveRepcodeToRawOffset()`: the offset a repcode `off_base`
/// stands for with the history `rep` (0 for "repeat 1 minus 1" with
/// `rep[0] == 1`, an invalid offset).
fn resolve_repcode_to_raw_offset(rep: &[u32; 3], off_base: u32, ll0: bool) -> u32 {
    debug_assert!(offbase_is_repcode(off_base));
    let adjusted_rep_code = off_base - 1 + u32::from(ll0); // 0..=3
    if adjusted_rep_code == REP_NUM {
        debug_assert!(ll0);
        return rep[0].wrapping_sub(1);
    }
    rep[adjusted_rep_code as usize]
}

/// `ZSTD_seqStore_resolveOffCodes()`: replaces the chunk's repcodes that
/// mean another offset for the decoder (whose history `d_rep` skipped the
/// raw and RLE partitions) than for the compressor (`c_rep`) by the
/// compressor's raw offset; updates both histories.
fn resolve_off_codes(d_rep: &mut [u32; 3], c_rep: &mut [u32; 3], ss: &mut SeqStore, ch: &SeqChunk) {
    let nb_seq = ch.nb_seq();
    let long_lit_len_idx = if ch.long_length_type == LongLengthType::LiteralLength {
        ch.long_length_pos as usize
    } else {
        nb_seq
    };
    for (idx, seq) in ss.sequences[ch.seq_start..ch.seq_end]
        .iter_mut()
        .enumerate()
    {
        let ll0 = seq.lit_length == 0 && idx != long_lit_len_idx;
        let off_base = seq.off_base;
        debug_assert!(off_base > 0);
        if offbase_is_repcode(off_base) {
            let d_raw_offset = resolve_repcode_to_raw_offset(d_rep, off_base, ll0);
            let c_raw_offset = resolve_repcode_to_raw_offset(c_rep, off_base, ll0);
            // Adjust the simulated decompression history on a mismatch:
            // use the offset the compressor meant.
            if d_raw_offset != c_raw_offset {
                // OFFSET_TO_OFFBASE (its `> 0` assert is off in release
                // builds of libzstd)
                seq.off_base = c_raw_offset.wrapping_add(REP_NUM);
            }
        }
        // The compression history always follows the unmodified seqStore;
        // the decompression one the possibly replaced offsets.
        update_rep(d_rep, seq.off_base, ll0);
        update_rep(c_rep, off_base, ll0);
    }
}

/// `ZSTD_noCompressBlock()`: a raw block (header included).
fn no_compress_block(dst: &mut [u8], src: &[u8], last: u32) -> Result<usize> {
    let n = src.len();
    if n + BLOCK_HEADER_SIZE > dst.len() {
        return Err(Error::OutputTooSmall);
    }
    let h = last + ((n as u32) << 3);
    dst[..3].copy_from_slice(&h.to_le_bytes()[..3]);
    dst[3..3 + n].copy_from_slice(src);
    Ok(n + BLOCK_HEADER_SIZE)
}

/// `ZSTD_rleCompressBlock()`: an RLE block (header included).
fn rle_compress_block(dst: &mut [u8], byte: u8, src_size: usize, last: u32) -> Result<usize> {
    if dst.len() < 4 {
        return Err(Error::OutputTooSmall);
    }
    let h = last + (1 << 1) + ((src_size as u32) << 3);
    dst[..3].copy_from_slice(&h.to_le_bytes()[..3]);
    dst[3] = byte;
    Ok(4)
}

/// `ZSTD_buildBlockEntropyStats_literals()` (literals compression on):
/// chooses the literals' encoding and builds `next`'s Huffman table.
/// Returns the encoding type and the table description size.
fn build_entropy_stats_literals(
    src: &[u8],
    prev: &HufCTables,
    next: &mut HufCTables,
    optimal_depth: bool,
) -> Result<(u32, usize)> {
    let src_size = src.len();
    let mut max_symbol_value = 255u32;
    let mut repeat = prev.repeat;
    // Prepare next assuming the existing table is reused.
    next.clone_from(prev);

    // small? don't even attempt compression (speed opt)
    let min_lit_size = if prev.repeat == HufRepeat::Valid {
        6
    } else {
        COMPRESS_LITERALS_SIZE_MIN
    };
    if src_size <= min_lit_size {
        return Ok((SET_BASIC, 0));
    }

    // Scan input and build symbol stats
    let mut count = [0u32; 256];
    let largest = hist_count(&mut count, &mut max_symbol_value, src)? as usize;
    if largest == src_size {
        return Ok((SET_RLE, 0)); // only one literal symbol
    }
    if largest <= (src_size >> 7) + 4 {
        return Ok((SET_BASIC, 0)); // heuristic: likely not compressible
    }

    // Validate the previous Huffman table
    if repeat == HufRepeat::Check && !huf_validate_ctable(&prev.table, &count, max_symbol_value) {
        repeat = HufRepeat::None;
    }

    // Build Huffman Tree
    next.table = HufCTable::new();
    let flags = if optimal_depth {
        HUF_FLAGS_OPTIMAL_DEPTH
    } else {
        0
    };
    let huff_log = huf_optimal_table_log(
        LIT_HUF_LOG,
        src_size,
        max_symbol_value,
        &mut next.table,
        &count,
        flags,
    );
    debug_assert!(huff_log <= LIT_HUF_LOG);
    let huff_log = huf_build_ctable(&mut next.table, &count, max_symbol_value, huff_log)?;

    // Build and write the CTable
    let new_c_size = huf_estimate_compressed_size(&next.table, &count, max_symbol_value);
    let mut des = [0u8; MAX_HUF_HEADER_SIZE];
    let h_size = huf_write_ctable(&mut des, &next.table, max_symbol_value, huff_log)?;
    // Check against repeating the previous CTable
    if repeat != HufRepeat::None {
        let old_c_size = huf_estimate_compressed_size(&prev.table, &count, max_symbol_value);
        if old_c_size < src_size && (old_c_size <= h_size + new_c_size || h_size + 12 >= src_size) {
            next.clone_from(prev);
            return Ok((SET_REPEAT, 0));
        }
    }
    if new_c_size + h_size >= src_size {
        next.clone_from(prev);
        return Ok((SET_BASIC, 0));
    }
    next.repeat = HufRepeat::Check;
    Ok((SET_COMPRESSED, h_size))
}

/// `ZSTD_estimateBlockSize_literal()`.
fn estimate_literals_size(
    literals: &[u8],
    huf: &HufCTable,
    h_type: u32,
    huf_des_size: usize,
    write_entropy: bool,
) -> usize {
    let lit_size = literals.len();
    let header_size = 3 + usize::from(lit_size >= 1024) + usize::from(lit_size >= 16 * 1024);
    let single_stream = lit_size < 256;
    match h_type {
        SET_BASIC => lit_size,
        SET_RLE => 1,
        _ => {
            let mut count = [0u32; 256];
            let mut max_symbol_value = 255u32;
            if hist_count(&mut count, &mut max_symbol_value, literals).is_err() {
                return lit_size;
            }
            let mut estimate = huf_estimate_compressed_size(huf, &count, max_symbol_value);
            if write_entropy {
                estimate += huf_des_size;
            }
            if !single_stream {
                estimate += 6; // multi-stream huffman uses 6-byte jump table
            }
            estimate + header_size
        }
    }
}

/// `ZSTD_estimateBlockSize_symbolType()`: estimated bytes of one code
/// stream (FSE symbols plus extra bits; offset codes are their own number
/// of extra bits).
#[allow(clippy::too_many_arguments)]
fn estimate_symbol_type_size<const N: usize, const S: usize>(
    encoding: u32,
    codes: &[u8],
    max_code: u32,
    ct: &FseCTable<N, S>,
    additional_bits: Option<&[u8]>,
    default_norm: &[i16],
    default_norm_log: u32,
) -> usize {
    let nb_seq = codes.len();
    let mut count = [0u32; 256];
    let mut max = max_code;
    hist_count_fast(&mut count, &mut max, codes); // can't fail
    let mut bits = match encoding {
        SET_BASIC => cross_entropy_cost(default_norm, default_norm_log, &count, max),
        SET_RLE => 0,
        _ => fse_bit_cost(ct, &count, max),
    };
    if bits == COST_ERROR {
        return nb_seq * 10;
    }
    match additional_bits {
        Some(ab) => {
            bits += codes
                .iter()
                .map(|&c| usize::from(ab[c as usize]))
                .sum::<usize>()
        }
        None => bits += codes.iter().map(|&c| usize::from(c)).sum::<usize>(),
    }
    bits >> 3
}

impl FrameCompressor {
    /// `ZSTD_buildEntropyStatisticsAndEstimateSubBlockSize()`: builds the
    /// chunk's entropy tables into `next` (from `prev`) and estimates its
    /// compressed size, header included. `None` for libzstd's errors.
    fn estimate_sub_block_size(&mut self, ch: &SeqChunk) -> Option<usize> {
        let strategy = self.cp.strategy as u32;
        // ZSTD_buildBlockEntropyStats (literals compression is never
        // disabled for the strategies using the splitter)
        let (h_type, huf_des_size) = build_entropy_stats_literals(
            &self.ss.lit_buf[ch.lit_start..ch.lit_end],
            &self.prev.huf,
            &mut self.next.huf,
            strategy >= STRAT_BTULTRA,
        )
        .ok()?;
        let nb_seq = ch.nb_seq();
        let stats = if nb_seq != 0 {
            let mut fse_buf = [0u8; MAX_FSE_HEADERS_SIZE];
            build_sequences_statistics(
                &mut fse_buf,
                &mut self.ss,
                ch,
                &self.prev.fse,
                &mut self.next.fse,
                strategy,
            )
            .ok()?
        } else {
            // ZSTD_buildDummySequencesStatistics
            self.next.fse.ll_repeat = FseRepeat::None;
            self.next.fse.of_repeat = FseRepeat::None;
            self.next.fse.ml_repeat = FseRepeat::None;
            SeqStats {
                ll_type: SET_BASIC,
                of_type: SET_BASIC,
                ml_type: SET_BASIC,
                size: 0,
                last_count_size: 0,
            }
        };

        // ZSTD_estimateBlockSize
        let literals_size = estimate_literals_size(
            &self.ss.lit_buf[ch.lit_start..ch.lit_end],
            &self.next.huf.table,
            h_type,
            huf_des_size,
            h_type == SET_COMPRESSED,
        );
        let (s, e) = (ch.seq_start, ch.seq_end);
        let fse = &self.next.fse;
        let mut seq_size = 1 + 1 + usize::from(nb_seq >= 128) + usize::from(nb_seq >= LONGNBSEQ);
        seq_size += estimate_symbol_type_size(
            stats.of_type,
            &self.ss.of_code[s..e],
            MAX_OFF as u32,
            &fse.of,
            None,
            &OF_DEFAULT_NORM,
            5,
        );
        seq_size += estimate_symbol_type_size(
            stats.ll_type,
            &self.ss.ll_code[s..e],
            MAX_LL as u32,
            &fse.ll,
            Some(&LL_BITS),
            &LL_DEFAULT_NORM,
            6,
        );
        seq_size += estimate_symbol_type_size(
            stats.ml_type,
            &self.ss.ml_code[s..e],
            MAX_ML as u32,
            &fse.ml,
            Some(&ML_BITS),
            &ML_DEFAULT_NORM,
            6,
        );
        seq_size += stats.size; // writeSeqEntropy
        Some(seq_size + literals_size + BLOCK_HEADER_SIZE)
    }

    /// `ZSTD_deriveBlockSplitsHelper()`: splits `start_idx..end_idx` in
    /// halves, recursively, while the halves are estimated smaller than
    /// the whole.
    fn derive_block_splits_helper(
        &mut self,
        splits: &mut [u32; MAX_NB_BLOCK_SPLITS],
        nb_splits: &mut usize,
        start_idx: usize,
        end_idx: usize,
    ) {
        let mid_idx = (start_idx + end_idx) / 2;
        debug_assert!(end_idx >= start_idx);
        if end_idx - start_idx < MIN_SEQUENCES_BLOCK_SPLITTING || *nb_splits >= MAX_NB_BLOCK_SPLITS
        {
            return; // too few sequences
        }
        let full = derive_chunk(&self.ss, start_idx, end_idx);
        let first_half = derive_chunk(&self.ss, start_idx, mid_idx);
        let second_half = derive_chunk(&self.ss, mid_idx, end_idx);
        let original = self.estimate_sub_block_size(&full);
        let first = self.estimate_sub_block_size(&first_half);
        let second = self.estimate_sub_block_size(&second_half);
        let (Some(original), Some(first), Some(second)) = (original, first, second) else {
            return;
        };
        if first + second < original {
            self.derive_block_splits_helper(splits, nb_splits, start_idx, mid_idx);
            splits[*nb_splits] = mid_idx as u32;
            *nb_splits += 1;
            self.derive_block_splits_helper(splits, nb_splits, mid_idx, end_idx);
        }
    }

    /// `ZSTD_deriveBlockSplits()`: fills `partitions` with the split
    /// points (sequence indices) followed by `nb_seq`; returns the number
    /// of splits.
    fn derive_block_splits(
        &mut self,
        partitions: &mut [u32; MAX_NB_BLOCK_SPLITS],
        nb_seq: usize,
    ) -> usize {
        if nb_seq <= 4 {
            return 0; // refuse to split anything with less than 4 sequences
        }
        let mut nb_splits = 0;
        self.derive_block_splits_helper(partitions, &mut nb_splits, 0, nb_seq);
        partitions[nb_splits] = nb_seq as u32;
        nb_splits
    }

    /// `ZSTD_compressSeqStore_singleBlock()`: writes the chunk `ch` (the
    /// sequences of `src`, a whole block or a partition) as one block,
    /// header included. `next_rep` is the repcode history the match finder
    /// ended the whole block with.
    #[allow(clippy::too_many_arguments)]
    fn compress_seq_store_single_block(
        &mut self,
        ch: &SeqChunk,
        d_rep: &mut [u32; 3],
        c_rep: &mut [u32; 3],
        dst: &mut [u8],
        src: &[u8],
        last: u32,
        is_partition: bool,
        next_rep: &[u32; 3],
    ) -> Result<usize> {
        // In case of an RLE or raw block, the simulated decompression
        // repcode history must be reset.
        let d_rep_original = *d_rep;
        if is_partition {
            resolve_off_codes(d_rep, c_rep, &mut self.ss, ch);
        }
        if dst.len() < BLOCK_HEADER_SIZE {
            return Err(Error::OutputTooSmall);
        }
        let mut c_seqs_size = entropy_compress_seq_store(
            &mut dst[BLOCK_HEADER_SIZE..],
            &mut self.ss,
            ch,
            &self.prev,
            &mut self.next,
            &self.cp,
            src.len(),
        )?;
        // libzstd doesn't emit its first block as RLE (zstd <= 1.4.3
        // decoders would reject it).
        if !self.first_block && c_seqs_size < RLE_MAX_LENGTH && is_rle(src) {
            c_seqs_size = 1;
        }
        let c_size = if c_seqs_size == 0 {
            *d_rep = d_rep_original;
            no_compress_block(dst, src, last)?
        } else if c_seqs_size == 1 {
            *d_rep = d_rep_original;
            rle_compress_block(dst, src[0], src.len(), last)?
        } else {
            // ZSTD_blockState_confirmRepcodesAndEntropyTables
            core::mem::swap(&mut self.prev, &mut self.next);
            self.rep = *next_rep;
            let h = last + (2 << 1) + ((c_seqs_size as u32) << 3);
            dst[..3].copy_from_slice(&h.to_le_bytes()[..3]);
            BLOCK_HEADER_SIZE + c_seqs_size
        };
        if self.prev.fse.of_repeat == FseRepeat::Valid {
            self.prev.fse.of_repeat = FseRepeat::Check;
        }
        Ok(c_size)
    }

    /// `ZSTD_compressBlock_splitBlock()`: compresses the block
    /// `src[start..end]` as one or more blocks; returns their total size,
    /// headers included.
    pub(super) fn compress_block_split(
        &mut self,
        dst: &mut [u8],
        src: &[u8],
        start: usize,
        end: usize,
        last: u32,
    ) -> Result<usize> {
        let mut next_rep = self.rep;
        if !self.build_block_seq_store(&mut next_rep, src, start, end)? {
            if self.prev.fse.of_repeat == FseRepeat::Valid {
                self.prev.fse.of_repeat = FseRepeat::Check;
            }
            return no_compress_block(dst, &src[start..end], last);
        }
        let nb_seq = self.ss.nb_seq;
        self.split_block_internal(dst, &src[start..end], last, nb_seq, &next_rep)
    }

    /// `ZSTD_compressBlock_splitBlock_internal()`.
    fn split_block_internal(
        &mut self,
        dst: &mut [u8],
        block: &[u8],
        last: u32,
        nb_seq: usize,
        next_rep: &[u32; 3],
    ) -> Result<usize> {
        let block_size = block.len();
        let mut partitions = [0u32; MAX_NB_BLOCK_SPLITS];
        let num_splits = self.derive_block_splits(&mut partitions, nb_seq);

        // Partitions emitted raw or RLE may invalidate the repcode history:
        // `c_rep` follows the compressor (every partition), `d_rep` the
        // decoder (compressed partitions only, with replaced repcodes) and
        // is what the next block starts with.
        let mut d_rep = self.rep;
        let mut c_rep = self.rep;

        if num_splits == 0 {
            let ch = SeqChunk::full(&self.ss);
            return self.compress_seq_store_single_block(
                &ch, &mut d_rep, &mut c_rep, dst, block, last, false, next_rep,
            );
        }

        let mut c_size = 0usize;
        let mut ip = 0usize;
        let mut src_bytes_total = 0usize;
        let mut curr = derive_chunk(&self.ss, 0, partitions[0] as usize);
        for i in 0..=num_splits {
            let last_partition = i == num_splits;
            let mut last_block_entire_src = 0;
            let mut src_bytes =
                count_literals_bytes(&self.ss, &curr) + count_match_bytes(&self.ss, &curr);
            src_bytes_total += src_bytes;
            let next = if last_partition {
                // the final partition gets the last literals
                src_bytes += block_size - src_bytes_total;
                last_block_entire_src = last;
                curr
            } else {
                derive_chunk(&self.ss, partitions[i] as usize, partitions[i + 1] as usize)
            };
            let c_size_chunk = self.compress_seq_store_single_block(
                &curr,
                &mut d_rep,
                &mut c_rep,
                &mut dst[c_size..],
                &block[ip..ip + src_bytes],
                last_block_entire_src,
                true,
                next_rep,
            )?;
            ip += src_bytes;
            c_size += c_size_chunk;
            curr = next;
        }
        // cRep and dRep may have diverged: the next block uses dRep.
        self.rep = d_rep;
        Ok(c_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enc::seq_store::{REP_START_VALUE, SeqDef};

    fn store(seqs: &[(u32, usize, usize)], last_lits: usize) -> SeqStore {
        let n: usize = seqs.iter().map(|s| s.1 + s.2).sum::<usize>() + last_lits;
        let src: alloc::vec::Vec<u8> = (0..n + 64).map(|i| i as u8).collect();
        let mut ss = SeqStore::new(1 << 17, 3);
        let mut pos = 0;
        for &(off_base, ll, ml) in seqs {
            ss.store_seq(&src, pos, ll, n, off_base, ml);
            pos += ll + ml;
        }
        ss.store_last_literals(&src[pos..pos + last_lits]);
        ss
    }

    #[test]
    fn chunks() {
        let ss = store(&[(10, 5, 4), (1, 0, 3), (20, 70_000, 5), (2, 7, 3)], 9);
        assert_eq!(ss.long_length_type, LongLengthType::LiteralLength);
        assert_eq!(ss.long_length_pos, 2);
        let a = derive_chunk(&ss, 0, 2);
        assert_eq!((a.lit_start, a.lit_end), (0, 5));
        // long length just past the chunk: kept (as in libzstd), not counted
        assert_eq!(a.long_length_type, LongLengthType::LiteralLength);
        assert_eq!(a.long_length_pos, 2);
        assert_eq!(count_match_bytes(&ss, &a), 7);
        let b = derive_chunk(&ss, 2, 4);
        assert_eq!((b.lit_start, b.lit_end), (5, 5 + 70_007 + 9));
        assert_eq!(b.long_length_pos, 0);
        assert_eq!(count_literals_bytes(&ss, &b), 70_007);
        let c = derive_chunk(&ss, 3, 4);
        assert_eq!(c.long_length_type, LongLengthType::None);
        assert_eq!((c.lit_start, c.lit_end), (70_005, 70_005 + 7 + 9));
    }

    #[test]
    fn resolve_offcodes() {
        let mut ss = store(&[(1, 0, 4), (2, 3, 4), (3, 0, 4)], 0);
        let ch = SeqChunk::full(&ss);
        // Same histories: nothing changes.
        let (mut d, mut c) = (REP_START_VALUE, REP_START_VALUE);
        resolve_off_codes(&mut d, &mut c, &mut ss, &ch);
        assert_eq!(d, c);
        assert_eq!(ss.sequences[0].off_base, 1);
        // Diverged: repcodes become the compressor's offsets.
        let mut ss = store(&[(1, 0, 4), (2, 3, 4), (3, 0, 4)], 0);
        let mut d = [1, 4, 8];
        let mut c = [100, 200, 300];
        resolve_off_codes(&mut d, &mut c, &mut ss, &ch);
        let offs: alloc::vec::Vec<u32> =
            ss.sequences().iter().map(|s: &SeqDef| s.off_base).collect();
        // ll0 repcode 1 = rep[1] = 200; then rep 2 (ll 3) = rep[1] = 100;
        // then ll0 repcode 3 = rep[0] - 1 = 99 in both: kept.
        assert_eq!(offs, [203, 103, 3]);
        assert_eq!(d, c);
        assert_eq!(c, [99, 100, 200]);
    }
}
