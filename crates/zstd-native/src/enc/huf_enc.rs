//! Huffman encoding (libzstd's compress/huf_compress.c): the parts zstd's
//! literal compression (`ZSTD_compressLiterals`) uses.
//!
//! Faithful ports of libzstd 1.5.7, so tables, headers and bitstreams are
//! byte-identical: the same bucket + quick sort (whose tie order decides
//! the tree), tree building, height limiting, header (weights) encoding and
//! the double-container bit writer. The 64-bit code paths are used on every
//! target (libzstd's `size_t` container), so results match a 64-bit
//! libzstd build.
//!
//! # Return conventions (as libzstd)
//!
//! The `compress` functions return `Ok(0)` for "not compressible" (or "didn't
//! fit"), `Ok(n)` for the compressed size, and `Err` where libzstd returns an
//! error code ([`Error::Corrupted`] for `GENERIC` and the other internal
//! errors, [`Error::OutputTooSmall`] for `dstSize_tooSmall`).
//! `ZSTD_compressLiterals` treats an error like 0 (raw literals).

// Index loops mirror libzstd's.
#![allow(clippy::needless_range_loop)]

use super::fse_enc::{
    FseCTable, fse_compress_using_ctable, fse_normalize_count, fse_optimal_table_log,
    fse_optimal_table_log_internal, fse_write_ncount, highbit32,
};
use super::hist::{hist_count, hist_count_simple, hist_largest_count};
use crate::error::{Error, Result};

/// libzstd's `HUF_TABLELOG_MAX`.
pub(crate) const HUF_TABLELOG_MAX: u32 = 12;
/// libzstd's `HUF_TABLELOG_DEFAULT`.
pub(crate) const HUF_TABLELOG_DEFAULT: u32 = 11;
/// libzstd's `HUF_SYMBOLVALUE_MAX`.
pub(crate) const HUF_SYMBOLVALUE_MAX: u32 = 255;
/// libzstd's `HUF_BLOCKSIZE_MAX`.
pub(crate) const HUF_BLOCKSIZE_MAX: usize = 128 * 1024;
/// libzstd's `HUF_CTABLEBOUND`: largest table description.
pub(crate) const HUF_CTABLEBOUND: usize = 129;

/// libzstd's `HUF_flags_bmi2` (accepted, no effect: the output is the same).
pub(crate) const HUF_FLAGS_BMI2: u32 = 1 << 0;
/// libzstd's `HUF_flags_optimalDepth`: search the table log that gives the
/// smallest output (`ZSTD_compressLiterals` sets it from `ZSTD_btultra` up).
pub(crate) const HUF_FLAGS_OPTIMAL_DEPTH: u32 = 1 << 1;
/// libzstd's `HUF_flags_preferRepeat`: reuse the previous table when it is
/// valid, without building a new one (`strategy < ZSTD_lazy` and at most
/// 1 KB of literals).
pub(crate) const HUF_FLAGS_PREFER_REPEAT: u32 = 1 << 2;
/// libzstd's `HUF_flags_suspectUncompressible`: sample the first and last
/// 4 KB before counting everything (inputs of 40 KB or more).
pub(crate) const HUF_FLAGS_SUSPECT_UNCOMPRESSIBLE: u32 = 1 << 3;

/// libzstd's `HUF_repeat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum HufRepeat {
    /// Can't use the previous table.
    #[default]
    None,
    /// Can use the previous table, after checking it covers the symbols.
    Check,
    /// Can use the previous table, assumed valid.
    Valid,
}

/// libzstd's `HUF_CElt` table (`HUF_CTABLE_SIZE_ST(255)` `size_t`s): the
/// header (`HUF_CTableHeader`) unpacked, and per symbol
/// `nbBits | value << (64 - nbBits)` (bits 0..8: code length, top bits:
/// code).
#[derive(Clone)]
pub(crate) struct HufCTable {
    /// Header `tableLog`: the longest code.
    pub table_log: u32,
    /// Header `maxSymbolValue`.
    pub max_symbol_value: u32,
    pub elts: [u64; 256],
}

impl core::fmt::Debug for HufCTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HufCTable")
            .field("table_log", &self.table_log)
            .field("max_symbol_value", &self.max_symbol_value)
            .finish_non_exhaustive()
    }
}

impl Default for HufCTable {
    fn default() -> Self {
        Self::new()
    }
}

impl HufCTable {
    pub(crate) const fn new() -> Self {
        HufCTable {
            table_log: 0,
            max_symbol_value: 0,
            elts: [0; 256],
        }
    }

    /// libzstd's `HUF_getNbBitsFromCTable`: the code length of `symbol`, 0
    /// if above the table's `max_symbol_value`.
    #[inline]
    pub(crate) fn nb_bits(&self, symbol: u32) -> u32 {
        if symbol > self.max_symbol_value {
            return 0;
        }
        get_nb_bits(self.elts[symbol as usize])
    }
}

#[inline(always)]
fn get_nb_bits(elt: u64) -> u32 {
    (elt & 0xFF) as u32
}

/// libzstd's `HUF_readCTable` (with `HUF_readStats`): reads a Huffman tree
/// description (a dictionary's literals table) into `ct`. Returns the
/// bytes read and whether some symbol has a zero weight (`hasZeroWeights`);
/// `ct.max_symbol_value` is the last symbol described.
///
/// # Errors
///
/// [`Error::InvalidDictionary`] if the description is corrupt.
pub(crate) fn huf_read_ctable(ct: &mut HufCTable, src: &[u8]) -> Result<(usize, bool)> {
    let bad = Error::InvalidDictionary;
    let mut weights = [0u8; 256];
    let &header = src.first().ok_or(bad)?;
    let mut i_size = header as usize;
    let o_size;
    if i_size >= 128 {
        // special header
        o_size = i_size - 127;
        i_size = o_size.div_ceil(2);
        if i_size + 1 > src.len() || o_size >= 256 {
            return Err(bad);
        }
        for n in (0..o_size).step_by(2) {
            let b = src[1 + n / 2];
            weights[n] = b >> 4;
            weights[n + 1] = b & 15;
        }
    } else {
        // header compressed with FSE (normal case)
        if i_size + 1 > src.len() {
            return Err(bad);
        }
        o_size =
            crate::fse::decompress_weights(&src[1..1 + i_size], &mut weights).map_err(|_| bad)?;
        if o_size >= 256 {
            return Err(bad);
        }
    }

    // collect weight stats
    let mut rank_val = [0u32; HUF_TABLELOG_MAX as usize + 2];
    let mut weight_total = 0u32;
    for &w in &weights[..o_size] {
        if u32::from(w) > HUF_TABLELOG_MAX {
            return Err(bad);
        }
        rank_val[w as usize] += 1;
        weight_total += (1 << w) >> 1;
    }
    if weight_total == 0 {
        return Err(bad);
    }
    // get last non-null symbol weight (implied, total must be 2^n)
    let table_log = highbit32(weight_total) + 1;
    if table_log > HUF_TABLELOG_MAX {
        return Err(bad);
    }
    let rest = (1u32 << table_log) - weight_total;
    let verif = 1u32 << highbit32(rest);
    let last_weight = highbit32(rest) + 1;
    if verif != rest {
        return Err(bad); // last value must be a clean power of 2
    }
    weights[o_size] = last_weight as u8;
    rank_val[last_weight as usize] += 1;
    // check tree construction validity
    if rank_val[1] < 2 || rank_val[1] & 1 != 0 {
        return Err(bad);
    }
    let nb_symbols = o_size + 1;
    let has_zero_weights = rank_val[0] > 0;

    *ct = HufCTable::new();
    ct.table_log = table_log;
    ct.max_symbol_value = nb_symbols as u32 - 1;
    // fill nbBits
    for n in 0..nb_symbols {
        let w = u32::from(weights[n]);
        ct.elts[n] = u64::from(if w != 0 { table_log + 1 - w } else { 0 });
    }
    // fill val
    let mut nb_per_rank = [0u16; HUF_TABLELOG_MAX as usize + 2];
    let mut val_per_rank = [0u16; HUF_TABLELOG_MAX as usize + 2];
    for n in 0..nb_symbols {
        nb_per_rank[get_nb_bits(ct.elts[n]) as usize] += 1;
    }
    // determine starting value per rank
    val_per_rank[table_log as usize + 1] = 0; // for w==0
    let mut min = 0u16;
    for n in (1..=table_log as usize).rev() {
        val_per_rank[n] = min; // get starting value within each rank
        min += nb_per_rank[n];
        min >>= 1;
    }
    // assign value within rank, symbol order
    for n in 0..nb_symbols {
        let r = get_nb_bits(ct.elts[n]) as usize;
        set_value(&mut ct.elts[n], u64::from(val_per_rank[r]));
        val_per_rank[r] = val_per_rank[r].wrapping_add(1);
    }
    Ok((i_size + 1, has_zero_weights))
}

/// libzstd's `HUF_setValue` on an element whose length is set.
#[inline(always)]
fn set_value(elt: &mut u64, value: u64) {
    let nb_bits = get_nb_bits(*elt);
    if nb_bits > 0 {
        debug_assert!((value >> nb_bits) == 0);
        *elt |= value << (64 - nb_bits);
    }
}

// ---------------------------------------------------------------------------
// Table description (weights)
// ---------------------------------------------------------------------------

/// libzstd's `HUF_compressWeights`: FSE compression of Huffman weights (all
/// `<= 12`) with a table log of at most 6.
///
/// Returns the size, 0 when not compressible (or no room for the
/// bitstream), 1 when every weight is the same (rle), or an error from the
/// table description.
pub(crate) fn huf_compress_weights(dst: &mut [u8], weights: &[u8]) -> Result<usize> {
    const MAX_FSE_TABLELOG_FOR_HUFF_HEADER: u32 = 6;
    let wt_size = weights.len();
    if wt_size <= 1 {
        return Ok(0); // not compressible
    }
    let mut count = [0u32; 256];
    let mut max_symbol_value = HUF_TABLELOG_MAX;
    let max_count = hist_count_simple(&mut count, &mut max_symbol_value, weights);
    if max_count as usize == wt_size {
        return Ok(1); // only a single symbol: rle
    }
    if max_count == 1 {
        return Ok(0); // each symbol present at most once: not compressible
    }
    let table_log =
        fse_optimal_table_log(MAX_FSE_TABLELOG_FOR_HUFF_HEADER, wt_size, max_symbol_value);
    let mut norm = [0i16; HUF_TABLELOG_MAX as usize + 1];
    fse_normalize_count(
        &mut norm,
        table_log,
        &count,
        wt_size,
        max_symbol_value,
        false,
    )?;

    let h_size = fse_write_ncount(dst, &norm, max_symbol_value, table_log)?;

    let mut ct = FseCTable::<64, { HUF_TABLELOG_MAX as usize + 1 }>::new();
    ct.build(&norm, max_symbol_value, table_log)?;
    let c_size = fse_compress_using_ctable(&mut dst[h_size..], weights, &ct);
    if c_size == 0 {
        return Ok(0); // not enough space for the compressed data
    }
    Ok(h_size + c_size)
}

/// libzstd's `HUF_writeCTable_wksp`: writes the table description (the
/// weights of symbols `0..max_symbol_value`, FSE-compressed when that is
/// smaller, else 4 bits each) and returns its size.
pub(crate) fn huf_write_ctable(
    dst: &mut [u8],
    ct: &HufCTable,
    max_symbol_value: u32,
    huff_log: u32,
) -> Result<usize> {
    if max_symbol_value > HUF_SYMBOLVALUE_MAX || huff_log > HUF_TABLELOG_MAX {
        return Err(Error::Corrupted);
    }
    let msv = max_symbol_value as usize;

    // Convert to weights.
    let mut bits_to_weight = [0u8; HUF_TABLELOG_MAX as usize + 1];
    for n in 1..=huff_log as usize {
        bits_to_weight[n] = (huff_log as usize + 1 - n) as u8;
    }
    let mut huff_weight = [0u8; 256];
    for n in 0..msv {
        huff_weight[n] = bits_to_weight[get_nb_bits(ct.elts[n]) as usize];
    }

    // Attempt weights compression by FSE.
    if dst.is_empty() {
        return Err(Error::OutputTooSmall);
    }
    let h_size = huf_compress_weights(&mut dst[1..], &huff_weight[..msv])?;
    if (h_size > 1) & (h_size < msv / 2) {
        // FSE compressed.
        dst[0] = h_size as u8;
        return Ok(h_size + 1);
    }

    // Raw values, 4 bits each (max 15).
    if max_symbol_value > 256 - 128 {
        return Err(Error::Corrupted); // should not happen: likely incompressible
    }
    if msv.div_ceil(2) + 1 > dst.len() {
        return Err(Error::OutputTooSmall);
    }
    dst[0] = (128 + max_symbol_value).wrapping_sub(1) as u8;
    huff_weight[msv] = 0; // only read as the pair of the last odd symbol
    for n in (0..msv).step_by(2) {
        dst[n / 2 + 1] = (huff_weight[n] << 4) + huff_weight[n + 1];
    }
    Ok(msv.div_ceil(2) + 1)
}

// ---------------------------------------------------------------------------
// Tree building
// ---------------------------------------------------------------------------

/// libzstd's `nodeElt`.
#[derive(Clone, Copy, Default, Debug)]
struct NodeElt {
    count: u32,
    parent: u16,
    byte: u8,
    nb_bits: u8,
}

/// libzstd's `huffNodeTable`: `2 * 256` nodes. libzstd's `huffNode` is
/// `huffNode0 + 1` (index -1 is a sentinel), so huffNode index `i` is
/// `nodes[i + 1]` here.
type NodeTable = [NodeElt; 2 * (HUF_SYMBOLVALUE_MAX as usize + 1)];

/// `huffNode[i]` (see [`NodeTable`]).
macro_rules! h {
    ($nodes:expr, $i:expr) => {
        $nodes[($i + 1) as usize]
    };
}

const STARTNODE: i32 = HUF_SYMBOLVALUE_MAX as i32 + 1;
const RANK_POSITION_TABLE_SIZE: usize = 192;
const RANK_POSITION_MAX_COUNT_LOG: u32 = 32;
const RANK_POSITION_LOG_BUCKETS_BEGIN: u32 =
    (RANK_POSITION_TABLE_SIZE as u32 - 1) - RANK_POSITION_MAX_COUNT_LOG - 1; // 158
const RANK_POSITION_DISTINCT_COUNT_CUTOFF: u32 =
    RANK_POSITION_LOG_BUCKETS_BEGIN + highbit32(RANK_POSITION_LOG_BUCKETS_BEGIN);

/// libzstd's `HUF_getIndex`: bucket of a count.
#[inline(always)]
fn get_index(count: u32) -> u32 {
    if count < RANK_POSITION_DISTINCT_COUNT_CUTOFF {
        count
    } else {
        highbit32(count) + RANK_POSITION_LOG_BUCKETS_BEGIN
    }
}

/// libzstd's `HUF_insertionSort` on `arr[low..=high]`, descending counts.
fn insertion_sort(arr: &mut [NodeElt], low: i32, high: i32) {
    let size = high - low + 1;
    if size <= 1 {
        return;
    }
    let arr = &mut arr[low as usize..];
    for i in 1..size as usize {
        let key = arr[i];
        let mut j = i as isize - 1;
        while j >= 0 && arr[j as usize].count < key.count {
            arr[(j + 1) as usize] = arr[j as usize];
            j -= 1;
        }
        arr[(j + 1) as usize] = key;
    }
}

/// libzstd's `HUF_quickSortPartition` (rightmost pivot).
fn quick_sort_partition(arr: &mut [NodeElt], low: i32, high: i32) -> i32 {
    let pivot = arr[high as usize].count;
    let mut i = low - 1;
    for j in low..high {
        if arr[j as usize].count > pivot {
            i += 1;
            arr.swap(i as usize, j as usize);
        }
    }
    arr.swap((i + 1) as usize, high as usize);
    i + 1
}

/// libzstd's `HUF_simpleQuickSort`, descending counts.
fn simple_quick_sort(arr: &mut [NodeElt], mut low: i32, mut high: i32) {
    const INSERTION_SORT_THRESHOLD: i32 = 8;
    if high - low < INSERTION_SORT_THRESHOLD {
        insertion_sort(arr, low, high);
        return;
    }
    while low < high {
        let idx = quick_sort_partition(arr, low, high);
        if idx - low < high - idx {
            simple_quick_sort(arr, low, idx - 1);
            low = idx + 1;
        } else {
            simple_quick_sort(arr, idx + 1, high);
            high = idx - 1;
        }
    }
}

/// libzstd's `HUF_sort` for the symbols of `nz[..k]` (the symbols with a
/// non-zero count, in symbol order): sorted by decreasing count into
/// `huff_node[..k]` (`count` and `byte` set, `parent` and `nb_bits` 0).
/// Bucket sort: one bucket per count below 165 (symbol order kept), log2
/// buckets above, each sorted with libzstd's (unstable) quick sort.
///
/// libzstd sorts every symbol up to `maxSymbolValue`; those with a zero
/// count all go to the last bucket, after the others, and get no code. The
/// order of the others is the same without them, and on a small block most
/// of the work: zero counts pile into one bucket, whose counter is then a
/// chain of dependent increments through memory. The running sum of the
/// bucket starts is kept in a register (libzstd's prefix sum reads each
/// sum back from memory) and stops at the largest count's bucket.
fn huf_sort(huff_node: &mut [NodeElt], count: &[u32; 256], nz: &[u8]) {
    let k = nz.len();
    // Bucket sizes, counted in two tables (symbols of equal counts, common
    // on small blocks, would otherwise make a chain of increments of one
    // counter), then the bucket starts.
    let mut size = [[0u16; RANK_POSITION_TABLE_SIZE]; 2];
    let mut max_index = 0;
    let (pairs, last) = nz.as_chunks::<2>();
    for &[a, b] in pairs {
        let (i, j) = (
            get_index(count[a as usize]) as usize,
            get_index(count[b as usize]) as usize,
        );
        debug_assert!(i.max(j) < RANK_POSITION_TABLE_SIZE - 2);
        size[0][i] += 1;
        size[1][j] += 1;
        max_index = max_index.max(i).max(j);
    }
    for &a in last {
        let i = get_index(count[a as usize]) as usize;
        size[0][i] += 1;
        max_index = max_index.max(i);
    }
    // Decreasing counts: bucket `i` starts after every bucket above it (the
    // non-zero counts are in buckets 1 and above).
    let mut start = [0u16; RANK_POSITION_TABLE_SIZE];
    let mut acc = 0u16;
    for i in (1..=max_index).rev() {
        start[i] = acc;
        acc += size[0][i] + size[1][i];
    }
    debug_assert_eq!(usize::from(acc), k);
    for &s in nz {
        let c = count[s as usize];
        let i = get_index(c) as usize;
        let pos = start[i] as usize;
        start[i] += 1;
        huff_node[pos] = NodeElt {
            count: c,
            parent: 0,
            byte: s,
            nb_bits: 0,
        };
    }
    // `start[i]` is now the end of bucket `i`. libzstd quick sorts buckets
    // `RANK_POSITION_DISTINCT_COUNT_CUTOFF - 1` (count 164, sorting of
    // equal counts, which moves them around when there are more than 8)
    // to the last log2 bucket.
    let first = RANK_POSITION_DISTINCT_COUNT_CUTOFF as usize - 1;
    if max_index >= first {
        let mut begin = 0;
        for i in (first..=max_index).rev() {
            let end = start[i] as usize;
            if end - begin > 1 {
                simple_quick_sort(&mut huff_node[begin..], 0, (end - begin - 1) as i32);
            }
            begin = end;
        }
    }
    debug_assert!(huff_node[..k].windows(2).all(|w| w[0].count >= w[1].count));
}

/// libzstd's `HUF_buildTree`: an unlimited-depth Huffman tree over the
/// sorted nodes, of which `huffNode[last + 1..]` have a zero count. Returns
/// the index of the last (smallest) non-zero symbol.
fn huf_build_tree(nodes: &mut NodeTable, last: u32) -> i32 {
    let mut node_nb = STARTNODE;
    let mut non_null_rank = last as i32;
    // libzstd assumes at least one non-zero count (two, really).
    while non_null_rank > 0 && h!(nodes, non_null_rank).count == 0 {
        non_null_rank -= 1;
    }
    let mut low_s = non_null_rank;
    let node_root = node_nb + low_s - 1;
    let mut low_n = node_nb;
    h!(nodes, node_nb).count = h!(nodes, low_s)
        .count
        .wrapping_add(h!(nodes, low_s - 1).count);
    h!(nodes, low_s).parent = node_nb as u16;
    h!(nodes, low_s - 1).parent = node_nb as u16;
    node_nb += 1;
    low_s -= 2;
    for n in node_nb..=node_root {
        h!(nodes, n).count = 1 << 30;
    }
    h!(nodes, -1).count = 1 << 31; // fake entry, strong barrier

    // Create parents.
    while node_nb <= node_root {
        let n1 = if h!(nodes, low_s).count < h!(nodes, low_n).count {
            low_s -= 1;
            low_s + 1
        } else {
            low_n += 1;
            low_n - 1
        };
        let n2 = if h!(nodes, low_s).count < h!(nodes, low_n).count {
            low_s -= 1;
            low_s + 1
        } else {
            low_n += 1;
            low_n - 1
        };
        h!(nodes, node_nb).count = h!(nodes, n1).count.wrapping_add(h!(nodes, n2).count);
        h!(nodes, n1).parent = node_nb as u16;
        h!(nodes, n2).parent = node_nb as u16;
        node_nb += 1;
    }

    // Distribute weights (unlimited tree height).
    h!(nodes, node_root).nb_bits = 0;
    let mut n = node_root - 1;
    while n >= STARTNODE {
        let p = i32::from(h!(nodes, n).parent);
        h!(nodes, n).nb_bits = h!(nodes, p).nb_bits.wrapping_add(1);
        n -= 1;
    }
    for n in 0..=non_null_rank {
        let p = i32::from(h!(nodes, n).parent);
        h!(nodes, n).nb_bits = h!(nodes, p).nb_bits.wrapping_add(1);
    }
    non_null_rank
}

/// libzstd's `HUF_setMaxHeight`: limits the code lengths to
/// `target_nb_bits`, keeping a complete prefix code. Returns the longest
/// code length after the change.
fn huf_set_max_height(nodes: &mut NodeTable, last_non_null: u32, target_nb_bits: u32) -> u32 {
    let largest_bits = u32::from(h!(nodes, last_non_null as i32).nb_bits);
    // Early exit: no element above target.
    if largest_bits <= target_nb_bits {
        return largest_bits;
    }

    // There are several too-large elements (at least 2).
    let mut total_cost: i32 = 0;
    let base_cost: u32 = 1 << (largest_bits - target_nb_bits);
    let mut n = last_non_null as i32;

    // Adjust ranks above target to target, adding up the cost.
    while u32::from(h!(nodes, n).nb_bits) > target_nb_bits {
        total_cost = total_cost.wrapping_add(
            base_cost.wrapping_sub(1 << (largest_bits - u32::from(h!(nodes, n).nb_bits))) as i32,
        );
        h!(nodes, n).nb_bits = target_nb_bits as u8;
        n -= 1;
    }
    debug_assert!(u32::from(h!(nodes, n).nb_bits) <= target_nb_bits);
    // n ends at the index of the smallest symbol using < target bits.
    while u32::from(h!(nodes, n).nb_bits) == target_nb_bits {
        n -= 1;
    }

    // Renormalize the cost from 2^largestBits to 2^targetNbBits (a multiple
    // of baseCost).
    debug_assert!((total_cost as u32 & (base_cost - 1)) == 0);
    total_cost >>= largest_bits - target_nb_bits;
    debug_assert!(total_cost > 0);

    // Repay the normalized cost.
    const NO_SYMBOL: u32 = 0xF0F0_F0F0;
    let mut rank_last = [NO_SYMBOL; HUF_TABLELOG_MAX as usize + 2];

    // Position of the last (smallest) symbol per rank.
    {
        let mut current_nb_bits = target_nb_bits;
        let mut pos = n;
        while pos >= 0 {
            let nb = u32::from(h!(nodes, pos).nb_bits);
            if nb < current_nb_bits {
                current_nb_bits = nb;
                rank_last[(target_nb_bits - current_nb_bits) as usize] = pos as u32;
            }
            pos -= 1;
        }
    }

    while total_cost > 0 {
        // Try to reduce the next power of 2 above totalCost: gains back
        // half the rank.
        let mut n_bits_to_decrease = highbit32(total_cost as u32) + 1;
        while n_bits_to_decrease > 1 {
            let high_pos = rank_last[n_bits_to_decrease as usize];
            let low_pos = rank_last[n_bits_to_decrease as usize - 1];
            if high_pos == NO_SYMBOL {
                n_bits_to_decrease -= 1;
                continue;
            }
            // Decrease highPos if there are no lowPos symbols or it isn't
            // cheaper to remove 2 lowPos than 1 highPos.
            if low_pos == NO_SYMBOL {
                break;
            }
            let high_total = h!(nodes, high_pos as i32).count;
            let low_total = h!(nodes, low_pos as i32).count.wrapping_mul(2);
            if high_total <= low_total {
                break;
            }
            n_bits_to_decrease -= 1;
        }
        // Only when no rank-1 symbol is left: find the closest one.
        while n_bits_to_decrease <= HUF_TABLELOG_MAX
            && rank_last[n_bits_to_decrease as usize] == NO_SYMBOL
        {
            n_bits_to_decrease += 1;
        }
        let nbd = n_bits_to_decrease as usize;
        debug_assert!(rank_last[nbd] != NO_SYMBOL);
        // Lengthen it to gain back half the rank cost.
        total_cost -= 1 << (n_bits_to_decrease - 1);
        h!(nodes, rank_last[nbd] as i32).nb_bits += 1;

        // Fix up the new rank: if it was empty, this symbol is its smallest.
        if rank_last[nbd - 1] == NO_SYMBOL {
            rank_last[nbd - 1] = rank_last[nbd];
        }
        // Fix up the old rank.
        if rank_last[nbd] == 0 {
            // Reached the largest symbol.
            rank_last[nbd] = NO_SYMBOL;
        } else {
            rank_last[nbd] -= 1;
            if u32::from(h!(nodes, rank_last[nbd] as i32).nb_bits)
                != target_nb_bits.wrapping_sub(n_bits_to_decrease)
            {
                rank_last[nbd] = NO_SYMBOL; // this rank is now empty
            }
        }
    }

    // Cost correction can overshoot: add back on the smallest rank.
    while total_cost < 0 {
        if rank_last[1] == NO_SYMBOL {
            // No rank-1 symbol (targetNbBits-1): make one from the largest
            // rank-0 symbol (targetNbBits).
            while u32::from(h!(nodes, n).nb_bits) == target_nb_bits {
                n -= 1;
            }
            h!(nodes, n + 1).nb_bits -= 1;
            debug_assert!(n >= 0);
            rank_last[1] = (n + 1) as u32;
            total_cost += 1;
            continue;
        }
        h!(nodes, rank_last[1] as i32 + 1).nb_bits -= 1;
        rank_last[1] += 1;
        total_cost += 1;
    }

    target_nb_bits
}

/// libzstd's `HUF_buildCTableFromTree`: canonical codes from the lengths.
/// `coded` holds, in symbol order, every symbol that may have a code (the
/// others of `0..=max_symbol_value` get a zero length, as in libzstd, whose
/// loops visit them all to assign nothing).
fn huf_build_ctable_from_tree(
    ct: &mut HufCTable,
    nodes: &NodeTable,
    non_null_rank: i32,
    coded: &[u8],
    max_symbol_value: u32,
    max_nb_bits: u32,
) {
    let mut val_per_rank = [0u16; HUF_TABLELOG_MAX as usize + 1];
    let alphabet_size = max_symbol_value as usize + 1;
    // Two tables: neighbours in the sorted order mostly have the same
    // length, and one counter would make a chain of increments.
    let mut nb_per_rank2 = [[0u16; HUF_TABLELOG_MAX as usize + 1]; 2];
    let leaves = &nodes[1..=non_null_rank as usize + 1];
    let (pairs, last) = leaves.as_chunks::<2>();
    for [a, b] in pairs {
        nb_per_rank2[0][a.nb_bits as usize] += 1;
        nb_per_rank2[1][b.nb_bits as usize] += 1;
    }
    for a in last {
        nb_per_rank2[0][a.nb_bits as usize] += 1;
    }
    // Starting value per rank.
    let mut min: u16 = 0;
    for n in (1..=max_nb_bits as usize).rev() {
        val_per_rank[n] = min;
        min += nb_per_rank2[0][n] + nb_per_rank2[1][n];
        min >>= 1;
    }
    ct.elts[..alphabet_size].fill(0);
    for n in 0..=non_null_rank {
        let node = h!(nodes, n);
        ct.elts[node.byte as usize] = u64::from(node.nb_bits); // HUF_setNbBits
    }
    for &s in coded {
        let elt = &mut ct.elts[s as usize];
        let r = get_nb_bits(*elt) as usize;
        set_value(elt, u64::from(val_per_rank[r]));
        val_per_rank[r] = val_per_rank[r].wrapping_add(1);
    }
    ct.table_log = max_nb_bits;
    ct.max_symbol_value = max_symbol_value;
}

/// libzstd's `HUF_buildCTable_wksp`: builds a Huffman table from
/// `count[0..=max_symbol_value]` (at least two non-zero counts) with codes
/// of at most `max_nb_bits` (0 means [`HUF_TABLELOG_DEFAULT`]). Returns the
/// longest code length (the table log).
pub(crate) fn huf_build_ctable(
    ct: &mut HufCTable,
    count: &[u32; 256],
    max_symbol_value: u32,
    mut max_nb_bits: u32,
) -> Result<u32> {
    if max_nb_bits == 0 {
        max_nb_bits = HUF_TABLELOG_DEFAULT;
    }
    if max_symbol_value > HUF_SYMBOLVALUE_MAX {
        return Err(Error::Corrupted);
    }
    // The symbols with a non-zero count, in symbol order (branch-free).
    let mut nz = [0u8; 256];
    let mut k = 0;
    for (s, &c) in count[..=max_symbol_value as usize].iter().enumerate() {
        nz[k & 0xFF] = s as u8;
        k += usize::from(c != 0);
    }
    // Not in libzstd (whose callers never ask for it): more symbols than
    // codes of `max_nb_bits` bits is impossible, and libzstd's height
    // limiting would go out of bounds.
    if k as u64 > 1u64 << max_nb_bits.min(32) {
        return Err(Error::Corrupted);
    }
    let mut nodes: NodeTable = [NodeElt::default(); 2 * (HUF_SYMBOLVALUE_MAX as usize + 1)];

    // Sort, decreasing order.
    huf_sort(&mut nodes[1..], count, &nz[..k]);

    // Build the tree.
    let non_null_rank = huf_build_tree(&mut nodes, k.saturating_sub(1) as u32);
    // With no non-zero count (never asked for), libzstd gives symbol 0 a
    // code.
    let coded: &[u8] = if k == 0 { &[0] } else { &nz[..k] };
    if max_nb_bits > HUF_TABLELOG_MAX
        && u32::from(h!(nodes, non_null_rank).nb_bits) > HUF_TABLELOG_MAX
    {
        // libzstd ends with the same error (a result above
        // HUF_TABLELOG_MAX); this skips its out-of-range height limiting.
        return Err(Error::Corrupted);
    }

    // Determine and enforce maxTableLog.
    let max_nb_bits = huf_set_max_height(&mut nodes, non_null_rank as u32, max_nb_bits);
    if max_nb_bits > HUF_TABLELOG_MAX {
        return Err(Error::Corrupted);
    }
    huf_build_ctable_from_tree(
        ct,
        &nodes,
        non_null_rank,
        coded,
        max_symbol_value,
        max_nb_bits,
    );
    Ok(max_nb_bits)
}

/// libzstd's `HUF_estimateCompressedSize`: bytes for the counts with `ct`
/// (rounded down).
pub(crate) fn huf_estimate_compressed_size(
    ct: &HufCTable,
    count: &[u32; 256],
    max_symbol_value: u32,
) -> usize {
    let mut nb_bits = 0usize;
    for s in 0..=max_symbol_value as usize {
        nb_bits += get_nb_bits(ct.elts[s]) as usize * count[s] as usize;
    }
    nb_bits >> 3
}

/// libzstd's `HUF_validateCTable`: true if `ct` has a code for every
/// symbol with a non-zero count.
pub(crate) fn huf_validate_ctable(
    ct: &HufCTable,
    count: &[u32; 256],
    max_symbol_value: u32,
) -> bool {
    if ct.max_symbol_value < max_symbol_value {
        return false;
    }
    let mut bad = false;
    for s in 0..=max_symbol_value as usize {
        bad |= (count[s] != 0) & (get_nb_bits(ct.elts[s]) == 0);
    }
    !bad
}

/// libzstd's `HUF_cardinality`: number of non-zero counts.
pub(crate) fn huf_cardinality(count: &[u32; 256], max_symbol_value: u32) -> u32 {
    count[..=max_symbol_value as usize]
        .iter()
        .filter(|&&c| c != 0)
        .count() as u32
}

/// libzstd's `HUF_minTableLog`.
pub(crate) fn huf_min_table_log(symbol_cardinality: u32) -> u32 {
    highbit32(symbol_cardinality) + 1
}

/// Scratch output for the optimal depth search (libzstd uses the rest of
/// its ~4.8 KB workspace). Any size that holds the largest description
/// gives the same sizes.
const OPTIMAL_DEPTH_SCRATCH: usize = 1024;

/// libzstd's `HUF_optimalTableLog`. Without [`HUF_FLAGS_OPTIMAL_DEPTH`],
/// libzstd's cheap FSE-based estimate; with it, tries every table log from
/// the minimum up (building tables into `scratch`) and keeps the one with
/// the smallest header + estimated data size.
pub(crate) fn huf_optimal_table_log(
    max_table_log: u32,
    src_size: usize,
    max_symbol_value: u32,
    scratch: &mut HufCTable,
    count: &[u32; 256],
    flags: u32,
) -> u32 {
    debug_assert!(src_size > 1);
    if flags & HUF_FLAGS_OPTIMAL_DEPTH == 0 {
        // Cheap evaluation, based on FSE.
        return fse_optimal_table_log_internal(max_table_log, src_size, max_symbol_value, 1);
    }

    let mut dst = [0u8; OPTIMAL_DEPTH_SCRATCH];
    let symbol_cardinality = huf_cardinality(count, max_symbol_value);
    let min_table_log = huf_min_table_log(symbol_cardinality);
    let mut opt_size = usize::MAX - 1;
    let mut opt_log = max_table_log;

    // Search until the size increases.
    for opt_log_guess in min_table_log..=max_table_log {
        let Ok(max_bits) = huf_build_ctable(scratch, count, max_symbol_value, opt_log_guess) else {
            continue;
        };
        if max_bits < opt_log_guess && opt_log_guess > min_table_log {
            break;
        }
        let Ok(h_size) = huf_write_ctable(&mut dst, scratch, max_symbol_value, max_bits) else {
            continue;
        };
        let new_size = huf_estimate_compressed_size(scratch, count, max_symbol_value) + h_size;
        if new_size > opt_size + 1 {
            break;
        }
        if new_size < opt_size {
            opt_size = new_size;
            opt_log = opt_log_guess;
        }
    }
    debug_assert!(opt_log <= HUF_TABLELOG_MAX);
    opt_log
}

// ---------------------------------------------------------------------------
// Bitstream (HUF_CStream_t)
// ---------------------------------------------------------------------------

/// libzstd's `HUF_CStream_t`: two containers filled from the top (the new
/// code goes into the top bits after a right shift), the second merged into
/// the first, so two groups of symbols are encoded without a dependency.
/// Elements are `HUF_CElt`s: `nbBits` in the low bits, code in the top
/// bits; the "fast" add ORs the whole element (the low bits are then noise
/// that the callers keep out of the flushed bits, as in libzstd).
///
/// Invariant: `ptr <= end` and `end + 8 == dst.len()` (as `BitWriter`).
/// A clamping flush keeps it whatever is added; the unclamped flush of
/// the fast loops relies on the caller's size bound: `dst` holds
/// `src.len() * table_log` bits plus 8 bytes and no code is longer than
/// `table_log` bits, so `ptr`, which is the number of whole bytes added,
/// never passes `end`.
struct HufCStream<'a> {
    container: [u64; 2],
    bit_pos: [u64; 2],
    ptr: usize,
    end: usize,
    dst: &'a mut [u8],
}

/// libzstd's `HUF_endMark`: one bit, 1.
const END_MARK: u64 = 1 | (1 << 63);

impl<'a> HufCStream<'a> {
    /// libzstd's `HUF_initCStream`; `None` when `dst.len() <= 8`.
    #[inline(always)]
    fn new(dst: &'a mut [u8]) -> Option<Self> {
        if dst.len() <= 8 {
            return None;
        }
        Some(HufCStream {
            container: [0; 2],
            bit_pos: [0; 2],
            ptr: 0,
            end: dst.len() - 8,
            dst,
        })
    }

    /// libzstd's `HUF_addBits`.
    #[inline(always)]
    fn add_bits<const IDX: usize>(&mut self, elt: u64, fast: bool) {
        let nb_bits = get_nb_bits(elt);
        debug_assert!(nb_bits <= HUF_TABLELOG_MAX);
        self.container[IDX] >>= nb_bits;
        self.container[IDX] |= if fast { elt } else { elt & !0xFF };
        // Only the low 8 bits of bit_pos are read: the code bits added
        // here are noise.
        self.bit_pos[IDX] = self.bit_pos[IDX].wrapping_add(elt);
        debug_assert!((self.bit_pos[IDX] & 0xFF) <= 64);
    }

    /// libzstd's `HUF_zeroIndex1`.
    #[inline(always)]
    fn zero_index1(&mut self) {
        self.container[1] = 0;
        self.bit_pos[1] = 0;
    }

    /// libzstd's `HUF_mergeIndex1`.
    #[inline(always)]
    fn merge_index1(&mut self) {
        let p1 = (self.bit_pos[1] & 0xFF) as u32;
        debug_assert!(p1 < 64);
        self.container[0] = self.container[0].wrapping_shr(p1);
        self.container[0] |= self.container[1];
        self.bit_pos[0] = self.bit_pos[0].wrapping_add(self.bit_pos[1]);
        debug_assert!((self.bit_pos[0] & 0xFF) <= 64);
    }

    /// libzstd's `HUF_flushBits`: `CLAMP` is its safe variant; without it
    /// (`kFastFlush`) the caller guarantees the stream fits, see
    /// [`huf_compress1x_using_ctable`].
    #[inline(always)]
    #[allow(unsafe_code)]
    fn flush_bits<const CLAMP: bool>(&mut self) {
        // The upper bits of bit_pos are noise.
        let nb_bits = (self.bit_pos[0] & 0xFF) as u32;
        let nb_bytes = (nb_bits >> 3) as usize;
        // The top nb_bits bits of the container are the ones we need
        // (nb_bits is 0 only for a table missing a symbol it is asked to
        // encode, a caller bug: then the wrap shifts by 0, like x86 does
        // for libzstd's shift by 64).
        let bits = self.container[0].wrapping_shr(64u32.wrapping_sub(nb_bits));
        self.bit_pos[0] &= 7;
        debug_assert!(nb_bits <= 64);
        debug_assert!(self.ptr <= self.end && self.end + 8 == self.dst.len());
        // SAFETY: `ptr <= end == dst.len() - 8` (struct invariant, kept by
        // the clamp below or by the caller's size bound, see the struct
        // doc), so the 8 bytes at `ptr` are inside `dst`.
        unsafe {
            self.dst
                .as_mut_ptr()
                .add(self.ptr)
                .cast::<u64>()
                .write_unaligned(bits.to_le());
        }
        self.ptr += nb_bytes;
        if CLAMP {
            if self.ptr > self.end {
                self.ptr = self.end;
            }
        } else {
            debug_assert!(self.ptr <= self.end);
        }
    }

    /// libzstd's `HUF_closeCStream`: the size, or 0 if it didn't fit.
    #[inline(always)]
    fn close(mut self) -> usize {
        self.add_bits::<0>(END_MARK, false);
        self.flush_bits::<true>();
        let nb_bits = self.bit_pos[0] & 0xFF;
        if self.ptr >= self.end {
            return 0; // overflow
        }
        self.ptr + usize::from(nb_bits > 0)
    }
}

/// libzstd's `HUF_compress1X_usingCTable_internal_body_loop` (`kFastFlush`
/// dropped: both flushes clamp here). Encodes `src` backward, `UNROLL`
/// symbols per container fill.
#[inline(always)]
fn compress1x_loop<const UNROLL: usize, const LAST_FAST: bool, const CLAMP: bool>(
    bc: &mut HufCStream<'_>,
    src: &[u8],
    ct: &[u64; 256],
) {
    let mut n = src.len();
    // Join to UNROLL.
    let rem = n % UNROLL;
    if rem > 0 {
        for _ in 0..rem {
            n -= 1;
            bc.add_bits::<0>(ct[src[n] as usize], false);
        }
        bc.flush_bits::<CLAMP>();
    }
    debug_assert!(n.is_multiple_of(UNROLL));

    // Join to 2 * UNROLL.
    if !n.is_multiple_of(2 * UNROLL) {
        for u in 1..UNROLL {
            bc.add_bits::<0>(ct[src[n - u] as usize], true);
        }
        bc.add_bits::<0>(ct[src[n - UNROLL] as usize], LAST_FAST);
        bc.flush_bits::<CLAMP>();
        n -= UNROLL;
    }
    debug_assert!(n.is_multiple_of(2 * UNROLL));

    for chunk in src[..n].rchunks_exact(2 * UNROLL) {
        // UNROLL symbols into container 0.
        for u in 1..UNROLL {
            bc.add_bits::<0>(ct[chunk[2 * UNROLL - u] as usize], true);
        }
        bc.add_bits::<0>(ct[chunk[UNROLL] as usize], LAST_FAST);
        bc.flush_bits::<CLAMP>();
        // UNROLL symbols into container 1 (no dependency on container 0).
        bc.zero_index1();
        for u in 1..UNROLL {
            bc.add_bits::<1>(ct[chunk[UNROLL - u] as usize], true);
        }
        bc.add_bits::<1>(ct[chunk[0] as usize], LAST_FAST);
        bc.merge_index1();
        bc.flush_bits::<CLAMP>();
    }
}

/// libzstd's `HUF_tightCompressBound`.
#[inline]
fn tight_compress_bound(src_size: usize, table_log: u32) -> usize {
    ((src_size * table_log as usize) >> 3) + 8
}

/// libzstd's `HUF_compress1X_usingCTable`: one stream. Returns the size, or
/// 0 if it didn't fit (or `dst` is under 9 bytes).
///
/// No BMI2 variant (libzstd has one): on Zen 3 it saves instructions but
/// not cycles, measured on x-ray, dickens, mozilla and xml.
pub(crate) fn huf_compress1x_using_ctable(dst: &mut [u8], src: &[u8], ct: &HufCTable) -> usize {
    compress1x_checked(dst, src, ct, codes_fit(ct))
}

/// True if no code of `ct` is longer than `ct.table_log` bits. Every table
/// built or read here satisfies it; it is checked anyway, as the unclamped
/// flushes of the fast loops rely on it. Once per table use (not once per
/// stream: four times for four streams), and in plain 64-bit adds and ORs,
/// which vectorize with SSE2 (an early-exit scan doesn't, nor do 64-bit
/// compares, which need SSE4.2): `len + 255 - table_log`, below 512, has
/// bit 8 set if and only if `len > table_log`.
#[inline]
fn codes_fit(ct: &HufCTable) -> bool {
    let bias = 0xFF - u64::from(ct.table_log.min(0xFF));
    let over = ct.elts.iter().fold(0, |acc, &e| acc | ((e & 0xFF) + bias));
    over & 0x100 == 0
}

/// [`huf_compress1x_using_ctable`] with `codes_fit` = [`codes_fit`]`(ct)`.
#[inline(never)]
fn compress1x_checked(dst: &mut [u8], src: &[u8], ct: &HufCTable, codes_fit: bool) -> usize {
    debug_assert_eq!(codes_fit, self::codes_fit(ct));
    let table_log = ct.table_log;
    let dst_size = dst.len();
    if dst_size < 8 {
        return 0; // not enough space to compress
    }
    let Some(mut bc) = HufCStream::new(dst) else {
        return 0;
    };
    let elts = &ct.elts;
    if dst_size < tight_compress_bound(src.len(), table_log) || table_log > 11 || !codes_fit {
        compress1x_loop::<4, false, true>(&mut bc, src, elts);
    } else {
        // libzstd's 64-bit choices, with its `kFastFlush`: the output has
        // room for `src.len() * table_log` bits (`tight_compress_bound`)
        // and every code is at most `table_log` bits (`codes_fit`), so no
        // flush can pass `end` and the clamp is skipped.
        match table_log {
            11 => compress1x_loop::<5, false, false>(&mut bc, src, elts),
            10 => compress1x_loop::<5, true, false>(&mut bc, src, elts),
            9 => compress1x_loop::<6, false, false>(&mut bc, src, elts),
            8 => compress1x_loop::<7, false, false>(&mut bc, src, elts),
            7 => compress1x_loop::<8, false, false>(&mut bc, src, elts),
            _ => compress1x_loop::<9, true, false>(&mut bc, src, elts),
        }
    }
    bc.close()
}

/// libzstd's `HUF_compress4X_usingCTable`: four streams after a 6-byte
/// jump table. Returns the size, or 0 if it didn't fit, `src` is under 12
/// bytes, or a stream is over 65535 bytes.
pub(crate) fn huf_compress4x_using_ctable(dst: &mut [u8], src: &[u8], ct: &HufCTable) -> usize {
    let segment_size = src.len().div_ceil(4); // first 3 segments
    if dst.len() < 6 + 1 + 1 + 1 + 8 {
        return 0; // minimum space to compress successfully
    }
    if src.len() < 12 {
        return 0; // no saving possible: too small input
    }
    let mut op = 6; // jump table
    let mut ip = 0;
    let fit = codes_fit(ct);
    for i in 0..4 {
        let seg = if i < 3 {
            &src[ip..ip + segment_size]
        } else {
            &src[ip..]
        };
        let c_size = compress1x_checked(&mut dst[op..], seg, ct, fit);
        if c_size == 0 || c_size > 65535 {
            return 0;
        }
        if i < 3 {
            dst[2 * i..2 * i + 2].copy_from_slice(&(c_size as u16).to_le_bytes());
        }
        op += c_size;
        ip += segment_size;
    }
    op
}

/// libzstd's `HUF_compressCTable_internal`: streams at `dst[op..]`; 0 if
/// they didn't fit or the total (`op` + streams) isn't below
/// `src.len() - 1`.
fn huf_compress_ctable_internal(
    dst: &mut [u8],
    op: usize,
    src: &[u8],
    four_streams: bool,
    ct: &HufCTable,
) -> usize {
    let c_size = if four_streams {
        huf_compress4x_using_ctable(&mut dst[op..], src, ct)
    } else {
        huf_compress1x_using_ctable(&mut dst[op..], src, ct)
    };
    if c_size == 0 {
        return 0; // uncompressible
    }
    let total = op + c_size;
    // Check compressibility.
    if total >= src.len() - 1 {
        return 0;
    }
    total
}

const SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE: usize = 4096;
const SUSPECT_INCOMPRESSIBLE_SAMPLE_RATIO: usize = 10;

/// libzstd's `HUF_compress_internal`.
#[allow(clippy::too_many_arguments)]
fn huf_compress_internal(
    dst: &mut [u8],
    src: &[u8],
    mut max_symbol_value: u32,
    mut huff_log: u32,
    four_streams: bool,
    old_huf_table: &mut HufCTable,
    repeat: &mut HufRepeat,
    flags: u32,
) -> Result<usize> {
    let src_size = src.len();
    if src_size == 0 {
        return Ok(0); // uncompressed
    }
    if dst.is_empty() {
        return Ok(0); // cannot fit anything within dst budget
    }
    if src_size > HUF_BLOCKSIZE_MAX {
        return Err(Error::Corrupted); // srcSize_wrong
    }
    if huff_log > HUF_TABLELOG_MAX || max_symbol_value > HUF_SYMBOLVALUE_MAX {
        return Err(Error::Corrupted);
    }
    if max_symbol_value == 0 {
        max_symbol_value = HUF_SYMBOLVALUE_MAX;
    }
    if huff_log == 0 {
        huff_log = HUF_TABLELOG_DEFAULT;
    }

    // Heuristic: if the old table is valid, use it for small inputs.
    if flags & HUF_FLAGS_PREFER_REPEAT != 0 && *repeat == HufRepeat::Valid {
        return Ok(huf_compress_ctable_internal(
            dst,
            0,
            src,
            four_streams,
            old_huf_table,
        ));
    }

    let mut count = [0u32; 256];

    // If uncompressible data is suspected, sample first.
    if flags & HUF_FLAGS_SUSPECT_UNCOMPRESSIBLE != 0
        && src_size >= SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE * SUSPECT_INCOMPRESSIBLE_SAMPLE_RATIO
    {
        // libzstd counts each sample with `HIST_count_simple` (one table,
        // slow on repetitive bytes); only the largest counts are used, and
        // the lane counter gives the same ones.
        let largest_total = hist_largest_count(&src[..SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE]) as usize
            + hist_largest_count(&src[src_size - SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE..]) as usize;
        if largest_total <= ((2 * SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE) >> 7) + 4 {
            return Ok(0); // heuristic: probably not compressible enough
        }
    }

    // Scan the input and build the symbol stats.
    {
        let largest = hist_count(&mut count, &mut max_symbol_value, src)? as usize;
        if largest == src_size {
            dst[0] = src[0];
            return Ok(1); // single symbol, rle
        }
        if largest <= (src_size >> 7) + 4 {
            return Ok(0); // heuristic: probably not compressible enough
        }
    }

    // Check the validity of the previous table.
    if *repeat == HufRepeat::Check && !huf_validate_ctable(old_huf_table, &count, max_symbol_value)
    {
        *repeat = HufRepeat::None;
    }
    // Heuristic: use the existing table for small inputs.
    if flags & HUF_FLAGS_PREFER_REPEAT != 0 && *repeat != HufRepeat::None {
        return Ok(huf_compress_ctable_internal(
            dst,
            0,
            src,
            four_streams,
            old_huf_table,
        ));
    }

    // Build the Huffman tree.
    let mut table = HufCTable::new();
    huff_log = huf_optimal_table_log(
        huff_log,
        src_size,
        max_symbol_value,
        &mut table,
        &count,
        flags,
    );
    huff_log = huf_build_ctable(&mut table, &count, max_symbol_value, huff_log)?;

    // Write the table description.
    let h_size = huf_write_ctable(dst, &table, max_symbol_value, huff_log)?;
    // Is the previous table better?
    if *repeat != HufRepeat::None {
        let old_size = huf_estimate_compressed_size(old_huf_table, &count, max_symbol_value);
        let new_size = huf_estimate_compressed_size(&table, &count, max_symbol_value);
        if old_size <= h_size + new_size || h_size + 12 >= src_size {
            return Ok(huf_compress_ctable_internal(
                dst,
                0,
                src,
                four_streams,
                old_huf_table,
            ));
        }
    }

    // Use the new table.
    if h_size + 12 >= src_size {
        return Ok(0);
    }
    *repeat = HufRepeat::None;
    *old_huf_table = table; // save the new table
    Ok(huf_compress_ctable_internal(
        dst,
        h_size,
        src,
        four_streams,
        old_huf_table,
    ))
}

/// libzstd's `HUF_compress1X_repeat`: single-stream literals, considering
/// `huf_table` (the previous block's table, libzstd's `nextHuf->CTable`
/// copied from `prevHuf`) when `*repeat != None`.
///
/// `max_symbol_value` 0 means 255, `huff_log` 0 means
/// [`HUF_TABLELOG_DEFAULT`] (zstd passes 255 and `LitHufLog` = 11).
///
/// Returns:
/// - `Ok(0)`: not compressible (also when the result isn't smaller than
///   `src.len() - 1`, or didn't fit);
/// - `Ok(1)`: every byte is `src[0]` (rle; written to `dst[0]`) — or a real
///   1-byte result, which `ZSTD_compressLiterals` tells apart;
/// - `Ok(n)`: `n` bytes in `dst`: the table description then the stream,
///   or only the stream when the previous table was used — then `*repeat`
///   is left `!= None`. When a new table is used, `*repeat` becomes `None`
///   and `huf_table` holds the new table (even if the result is then 0);
/// - `Err`: as libzstd's error codes (callers treat it like 0).
pub(crate) fn huf_compress1x_repeat(
    dst: &mut [u8],
    src: &[u8],
    max_symbol_value: u32,
    huff_log: u32,
    huf_table: &mut HufCTable,
    repeat: &mut HufRepeat,
    flags: u32,
) -> Result<usize> {
    huf_compress_internal(
        dst,
        src,
        max_symbol_value,
        huff_log,
        false,
        huf_table,
        repeat,
        flags,
    )
}

/// libzstd's `HUF_compress4X_repeat`: [`huf_compress1x_repeat`] with four
/// streams (and a jump table).
pub(crate) fn huf_compress4x_repeat(
    dst: &mut [u8],
    src: &[u8],
    max_symbol_value: u32,
    huff_log: u32,
    huf_table: &mut HufCTable,
    repeat: &mut HufRepeat,
    flags: u32,
) -> Result<usize> {
    huf_compress_internal(
        dst,
        src,
        max_symbol_value,
        huff_log,
        true,
        huf_table,
        repeat,
        flags,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::Features;
    use crate::fse::decompress_weights;
    use crate::huf::{HufTable, decode_1x, decode_4x};
    use alloc::boxed::Box;
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

    /// Skewed random bytes: `kind` picks the shape.
    fn skewed(r: &mut Rng, len: usize, kind: u64) -> Vec<u8> {
        let alpha = 2 + r.below(254);
        (0..len)
            .map(|_| match kind {
                0 => {
                    let a = r.below(alpha);
                    let b = r.below(alpha);
                    (a * b / alpha) as u8
                }
                1 => {
                    // Geometric-ish: very long codes for rare symbols.
                    let x = r.next();
                    (x.trailing_zeros() as u64 * 7 % 256) as u8
                }
                2 => b"etaoinshrdlu  \n"[r.below(15) as usize],
                _ => r.below(alpha) as u8,
            })
            .collect()
    }

    fn histogram(src: &[u8]) -> ([u32; 256], u32) {
        let mut count = [0u32; 256];
        let mut msv = 255;
        hist_count_simple(&mut count, &mut msv, src);
        (count, msv)
    }

    #[test]
    fn compress_weights_round_trip() {
        let mut r = Rng(12345);
        let mut done = 0;
        for iter in 0..4000 {
            let n = 2 + r.below(254) as usize;
            let maxw = 1 + r.below(12);
            let w: Vec<u8> = (0..n)
                .map(|_| {
                    let a = r.below(maxw + 1) as u8;
                    let b = r.below(maxw + 1) as u8;
                    a.min(b)
                })
                .collect();
            let mut buf = [0u8; 512];
            let size = huf_compress_weights(&mut buf, &w).unwrap();
            if size <= 1 {
                continue;
            }
            let mut out = [0u8; 256];
            let got = decompress_weights(&buf[..size], &mut out)
                .unwrap_or_else(|e| panic!("iter {iter}: {e:?}"));
            assert_eq!(&out[..got], &w[..], "iter {iter}");
            done += 1;
        }
        assert!(done > 1000, "{done}");
    }

    /// libzstd's `HUF_sort` as written there (every symbol, zero counts
    /// included, prefix sum over all buckets): the reference for
    /// [`huf_sort`].
    fn reference_sort(huff_node: &mut [NodeElt], count: &[u32; 256], max_symbol_value: u32) {
        #[derive(Clone, Copy, Default)]
        struct RankPos {
            base: u16,
            curr: u16,
        }
        let max_sv1 = max_symbol_value as usize + 1;
        let mut rank_position = [RankPos::default(); RANK_POSITION_TABLE_SIZE];
        for &c in &count[..max_sv1] {
            rank_position[get_index(c) as usize].base += 1;
        }
        for n in (1..RANK_POSITION_TABLE_SIZE).rev() {
            rank_position[n - 1].base += rank_position[n].base;
            rank_position[n - 1].curr = rank_position[n - 1].base;
        }
        for (n, &c) in count[..max_sv1].iter().enumerate() {
            let r = get_index(c) as usize + 1;
            let pos = rank_position[r].curr as usize;
            rank_position[r].curr += 1;
            huff_node[pos].count = c;
            huff_node[pos].byte = n as u8;
        }
        for rp in &rank_position
            [RANK_POSITION_DISTINCT_COUNT_CUTOFF as usize..RANK_POSITION_TABLE_SIZE - 1]
        {
            let bucket_size = i32::from(rp.curr) - i32::from(rp.base);
            if bucket_size > 1 {
                simple_quick_sort(&mut huff_node[rp.base as usize..], 0, bucket_size - 1);
            }
        }
    }

    /// libzstd's `HUF_buildCTable_wksp` and `HUF_buildCTableFromTree` as
    /// written there (every symbol sorted and visited): the reference for
    /// [`huf_build_ctable`].
    fn reference_build_ctable(
        ct: &mut HufCTable,
        count: &[u32; 256],
        max_symbol_value: u32,
        max_nb_bits: u32,
    ) -> Result<u32> {
        if huf_cardinality(count, max_symbol_value) as u64 > 1u64 << max_nb_bits.min(32) {
            return Err(Error::Corrupted);
        }
        let mut nodes: NodeTable = [NodeElt::default(); 512];
        reference_sort(&mut nodes[1..], count, max_symbol_value);
        let non_null_rank = huf_build_tree(&mut nodes, max_symbol_value);
        if max_nb_bits > HUF_TABLELOG_MAX
            && u32::from(h!(nodes, non_null_rank).nb_bits) > HUF_TABLELOG_MAX
        {
            return Err(Error::Corrupted);
        }
        let max_nb_bits = huf_set_max_height(&mut nodes, non_null_rank as u32, max_nb_bits);
        if max_nb_bits > HUF_TABLELOG_MAX {
            return Err(Error::Corrupted);
        }
        let mut nb_per_rank = [0u16; HUF_TABLELOG_MAX as usize + 1];
        let mut val_per_rank = [0u16; HUF_TABLELOG_MAX as usize + 1];
        let alphabet_size = max_symbol_value as i32 + 1;
        for n in 0..=non_null_rank {
            nb_per_rank[h!(nodes, n).nb_bits as usize] += 1;
        }
        let mut min: u16 = 0;
        for n in (1..=max_nb_bits as usize).rev() {
            val_per_rank[n] = min;
            min += nb_per_rank[n];
            min >>= 1;
        }
        for n in 0..alphabet_size {
            let node = h!(nodes, n);
            ct.elts[node.byte as usize] = u64::from(node.nb_bits);
        }
        for elt in &mut ct.elts[..alphabet_size as usize] {
            let r = get_nb_bits(*elt) as usize;
            set_value(elt, u64::from(val_per_rank[r]));
            val_per_rank[r] = val_per_rank[r].wrapping_add(1);
        }
        ct.table_log = max_nb_bits;
        ct.max_symbol_value = max_symbol_value;
        Ok(max_nb_bits)
    }

    /// Counts that stress the sort: many zeros, ties in the distinct-count
    /// buckets, more than 8 symbols of count 164 (quick sorted by
    /// libzstd), ties in the log2 buckets, 0, 1 and 2 symbols.
    fn sort_counts(r: &mut Rng, msv: u32) -> [u32; 256] {
        let mut count = [0u32; 256];
        let kind = r.below(7);
        let palette: Vec<u32> = (0..1 + r.below(6))
            .map(|_| 165 + r.below(5000) as u32)
            .collect();
        for c in &mut count[..=msv as usize] {
            *c = match kind {
                0 => r.below(20) as u32,
                1 => [0, 0, 0, 1, 2, 3][r.below(6) as usize],
                2 => [0, 163, 164, 164, 164, 165][r.below(6) as usize],
                3 => palette[r.below(palette.len() as u64) as usize] * (r.below(2) as u32),
                4 => r.below(100_000) as u32,
                // (sums stay far below 2^32, as for a block)
                5 => (r.next() as u32 >> 10) >> r.below(22),
                _ => 0,
            };
        }
        if kind == 6 {
            // 0, 1 or 2 non-zero counts
            for _ in 0..r.below(3) {
                count[r.below(u64::from(msv) + 1) as usize] = 1 + r.below(1000) as u32;
            }
        }
        count
    }

    #[test]
    fn sort_matches_reference() {
        let mut r = Rng(99);
        for _ in 0..if cfg!(miri) { 2 } else { 3000 } {
            let msv = r.below(256) as u32;
            let count = sort_counts(&mut r, msv);
            let nz: Vec<u8> = (0..=msv)
                .filter(|&s| count[s as usize] != 0)
                .map(|s| s as u8)
                .collect();
            let mut want = [NodeElt::default(); 512];
            reference_sort(&mut want[1..], &count, msv);
            let mut got = [NodeElt::default(); 512];
            huf_sort(&mut got[1..], &count, &nz);
            for i in 0..=msv as usize {
                let (g, w) = (got[i + 1], want[i + 1]);
                if i < nz.len() {
                    assert_eq!((g.count, g.byte), (w.count, w.byte), "msv {msv} at {i}");
                    assert_eq!((g.parent, g.nb_bits), (0, 0));
                } else {
                    assert_eq!(w.count, 0);
                }
            }
        }
        assert_eq!(RANK_POSITION_DISTINCT_COUNT_CUTOFF, 165);
    }

    #[test]
    fn codes_fit_scan() {
        let mut r = Rng(5);
        for _ in 0..if cfg!(miri) { 4 } else { 2000 } {
            let mut ct = HufCTable::new();
            ct.table_log = r.below(14) as u32;
            if r.below(8) == 0 {
                ct.table_log = 200 + r.below(1000) as u32;
            }
            let longest = r.below(14);
            for e in ct.elts.iter_mut() {
                // random code bits, a length up to `longest`
                *e = (r.next() << 8) | r.below(longest + 1);
            }
            if r.below(3) == 0 {
                ct.elts[r.below(256) as usize] = (r.next() << 8) | (200 + r.below(56));
            }
            let want = ct
                .elts
                .iter()
                .all(|&e| u64::from(get_nb_bits(e)) <= u64::from(ct.table_log));
            assert_eq!(codes_fit(&ct), want);
        }
    }

    #[test]
    fn build_ctable_matches_reference() {
        let mut r = Rng(1999);
        for iter in 0..if cfg!(miri) { 2 } else { 5000 } {
            let msv = r.below(256) as u32;
            let count = sort_counts(&mut r, msv);
            let card = huf_cardinality(&count, msv);
            let need = huf_min_table_log(card.max(1));
            let max_bits = need + r.below(u64::from(14 - need.min(13))) as u32;
            // the same garbage table to start from
            let mut want = HufCTable::new();
            for e in want.elts.iter_mut() {
                *e = r.next();
            }
            let mut got = want.clone();
            let w = reference_build_ctable(&mut want, &count, msv, max_bits);
            let g = huf_build_ctable(&mut got, &count, msv, max_bits);
            assert_eq!(g, w, "iter {iter}");
            if w.is_ok() {
                assert_eq!(got.elts, want.elts, "iter {iter}");
                assert_eq!(got.table_log, want.table_log);
                assert_eq!(got.max_symbol_value, want.max_symbol_value);
            }
        }
    }

    /// Code lengths: complete prefix code, limited height, a code for
    /// every present symbol.
    #[test]
    fn build_ctable_properties() {
        let mut r = Rng(4242);
        for iter in 0..2000 {
            let len = 2 + r.below(20000) as usize;
            let src = skewed(&mut r, len, iter % 4);
            let (count, msv) = histogram(&src);
            if count.iter().filter(|&&c| c != 0).count() < 2 {
                continue;
            }
            let need = huf_min_table_log(huf_cardinality(&count, msv)).min(12);
            let max_bits = (5 + (r.below(8) as u32)).max(need); // 5..=12
            let mut ct = HufCTable::new();
            let tl = huf_build_ctable(&mut ct, &count, msv, max_bits).unwrap();
            assert!(tl <= max_bits, "iter {iter}");
            assert_eq!(ct.table_log, tl);
            assert_eq!(ct.max_symbol_value, msv);
            let mut kraft = 0u64;
            let mut longest = 0;
            for s in 0..=msv as usize {
                let nb = get_nb_bits(ct.elts[s]);
                assert_eq!(nb != 0, count[s] != 0, "iter {iter} s {s}");
                if nb != 0 {
                    kraft += 1 << (tl - nb);
                    longest = longest.max(nb);
                }
            }
            assert_eq!(kraft, 1 << tl, "iter {iter}");
            assert_eq!(longest, tl);
            assert!(huf_validate_ctable(&ct, &count, msv));
        }
    }

    /// Builds a table, writes it, reads it back with the decoder, and
    /// decodes 1X / 4X streams.
    #[test]
    fn write_read_and_decode_round_trip() {
        let mut r = Rng(0xDEAD_BEEF);
        let cpu = Features::detect();
        let mut dt = Box::new(HufTable::new());
        for iter in 0..1500 {
            let len = 12 + r.below(if iter % 5 == 0 { 128 * 1024 - 12 } else { 3000 }) as usize;
            let src = skewed(&mut r, len, iter % 4);
            let (count, msv) = histogram(&src);
            if count.iter().filter(|&&c| c != 0).count() < 2 {
                continue;
            }
            let mut ct = HufCTable::new();
            let need = huf_min_table_log(huf_cardinality(&count, msv)).min(12);
            let max_bits = if iter % 3 == 0 {
                11
            } else {
                6 + r.below(7) as u32
            }
            .max(need);
            let tl = huf_build_ctable(&mut ct, &count, msv, max_bits).unwrap();
            let mut hdr = [0u8; 256];
            let h_size = match huf_write_ctable(&mut hdr, &ct, msv, tl) {
                Ok(h) => h,
                Err(_) => {
                    // Only when the raw form is impossible (> 128 symbols).
                    assert!(msv > 128);
                    continue;
                }
            };
            assert!(h_size <= HUF_CTABLEBOUND);
            let used = dt.read(&hdr[..h_size]).unwrap();
            assert_eq!(used, h_size, "iter {iter}");

            let estimate = huf_estimate_compressed_size(&ct, &count, msv);
            let mut buf = alloc::vec![0u8; len + len / 2 + 64];
            // 1X
            let c1 = huf_compress1x_using_ctable(&mut buf, &src, &ct);
            assert!(c1 > 0);
            assert!(c1 >= estimate && c1 <= estimate + 2, "iter {iter}");
            let mut out = alloc::vec![0u8; len];
            decode_1x(&mut out, &buf[..c1], &dt).unwrap_or_else(|e| panic!("iter {iter}: {e:?}"));
            assert_eq!(out, src, "iter {iter}");
            // Tight buffers (the bounds-checked loop): the same bytes, or 0
            // when the complete bytes reach `capacity - 8`
            // (HUF_closeCStream's `ptr >= endPtr`).
            for extra in 7..=9 {
                let mut tight = alloc::vec![0u8; c1 + extra];
                let ct1 = huf_compress1x_using_ctable(&mut tight, &src, &ct);
                if extra == 9 {
                    assert_eq!(ct1, c1, "iter {iter}");
                }
                if ct1 != 0 {
                    assert_eq!(ct1, c1);
                    assert_eq!(&tight[..c1], &buf[..c1], "iter {iter}");
                }
            }
            // 4X
            let c4 = huf_compress4x_using_ctable(&mut buf, &src, &ct);
            if len <= 4 * 65535 {
                assert!(c4 > 0);
                for x2 in [false, true] {
                    out.fill(0);
                    decode_4x(&mut out, &buf[..c4], &mut dt, x2, cpu)
                        .unwrap_or_else(|e| panic!("iter {iter}: {e:?}"));
                    assert_eq!(out, src, "iter {iter}");
                }
            }
        }
    }

    /// Small outputs never panic and return 0 or a valid stream.
    #[test]
    fn small_outputs() {
        let mut r = Rng(5);
        let mut dt = Box::new(HufTable::new());
        for iter in 0..300 {
            let len = 12 + r.below(400) as usize;
            let src = skewed(&mut r, len, iter % 4);
            let (count, msv) = histogram(&src);
            if count.iter().filter(|&&c| c != 0).count() < 2 {
                continue;
            }
            let mut ct = HufCTable::new();
            let tl = huf_build_ctable(&mut ct, &count, msv, 11).unwrap();
            let mut hdr = [0u8; 256];
            let Ok(h_size) = huf_write_ctable(&mut hdr, &ct, msv, tl) else {
                continue;
            };
            dt.read(&hdr[..h_size]).unwrap();
            for cap in 0..len {
                let mut buf = alloc::vec![0u8; cap];
                let c = huf_compress1x_using_ctable(&mut buf, &src, &ct);
                if c > 0 {
                    let mut out = alloc::vec![0u8; len];
                    decode_1x(&mut out, &buf[..c], &dt).unwrap();
                    assert_eq!(out, src);
                }
                let _ = huf_compress4x_using_ctable(&mut buf, &src, &ct);
                let _ = huf_write_ctable(&mut buf, &ct, msv, tl);
            }
        }
    }

    /// Full literal compression (HUF_compress4X_repeat / 1X_repeat) with
    /// repeat tables, decoded like the zstd decoder would.
    #[test]
    fn compress_repeat_round_trip() {
        let mut r = Rng(31337);
        let cpu = Features::detect();
        let mut dt = Box::new(HufTable::new());
        let mut prev = HufCTable::new();
        let mut prev_repeat = HufRepeat::None;
        let mut stats = [0usize; 4];
        for iter in 0..1500u64 {
            let len = 1 + r.below(if iter % 7 == 0 { 128 * 1024 } else { 2000 }) as usize;
            let src = skewed(&mut r, len, (iter / 3) % 4);
            let four = len >= 256 && iter % 2 == 0;
            let flags = match iter % 4 {
                0 => 0,
                1 => HUF_FLAGS_OPTIMAL_DEPTH,
                2 => HUF_FLAGS_PREFER_REPEAT,
                _ => HUF_FLAGS_SUSPECT_UNCOMPRESSIBLE | HUF_FLAGS_BMI2,
            };
            let mut table = prev.clone();
            let mut repeat = prev_repeat;
            let mut dst = alloc::vec![0u8; len + 300];
            let res = if four {
                huf_compress4x_repeat(&mut dst, &src, 255, 11, &mut table, &mut repeat, flags)
            } else {
                huf_compress1x_repeat(&mut dst, &src, 255, 11, &mut table, &mut repeat, flags)
            };
            let Ok(c) = res else {
                stats[3] += 1;
                continue;
            };
            if c == 0 {
                stats[0] += 1;
                continue;
            }
            if c == 1 && src.iter().all(|&b| b == src[0]) {
                assert_eq!(dst[0], src[0]);
                continue;
            }
            assert!(c < len - 1);
            let streams = if repeat != HufRepeat::None {
                // Previous table reused: its description was read before.
                stats[1] += 1;
                &dst[..c]
            } else {
                stats[2] += 1;
                let used = dt
                    .read(&dst[..c])
                    .unwrap_or_else(|e| panic!("iter {iter}: {e:?}"));
                prev = table.clone();
                prev_repeat = HufRepeat::Check;
                &dst[used..c]
            };
            let mut out = alloc::vec![0u8; len];
            if four {
                decode_4x(&mut out, streams, &mut dt, iter % 3 == 0, cpu)
            } else {
                decode_1x(&mut out, streams, &dt)
            }
            .unwrap_or_else(|e| panic!("iter {iter}: {e:?}"));
            assert_eq!(out, src, "iter {iter}");
        }
        // Every path was taken.
        assert!(stats[0] > 0 && stats[1] > 0 && stats[2] > 0, "{stats:?}");
    }

    /// The optimal depth search returns a table log in range that is no
    /// worse (by libzstd's estimate) than the default.
    #[test]
    fn optimal_depth() {
        let mut r = Rng(8);
        for iter in 0..300 {
            let len = 100 + r.below(50_000) as usize;
            let src = skewed(&mut r, len, iter % 4);
            let (count, msv) = histogram(&src);
            let card = huf_cardinality(&count, msv);
            if card < 2 {
                continue;
            }
            let mut scratch = HufCTable::new();
            let log =
                huf_optimal_table_log(11, len, msv, &mut scratch, &count, HUF_FLAGS_OPTIMAL_DEPTH);
            assert!(
                log >= huf_min_table_log(card).min(11) && log <= 11,
                "iter {iter}"
            );
            let cheap = huf_optimal_table_log(11, len, msv, &mut scratch, &count, 0);
            assert!((5..=11).contains(&cheap));
        }
    }
}
