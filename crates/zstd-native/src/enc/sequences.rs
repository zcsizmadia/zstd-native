//! Sequences section (libzstd's `compress/zstd_compress_sequences.c`):
//! choosing each code's table encoding, building and writing the FSE
//! tables, and the backward sequence bitstream.

use super::bitwriter::BitWriter;
use super::fse_enc::{
    FSE_NCOUNTBOUND, FseCState, FseCTable, fse_normalize_count, fse_optimal_table_log,
    fse_write_ncount,
};
use super::literals::STRAT_LAZY;
use crate::error::{Error, Result};
use crate::fse::{LL_BITS, ML_BITS};

pub(crate) type LlCTable = FseCTable<512, 36>;
pub(crate) type OfCTable = FseCTable<256, 32>;
pub(crate) type MlCTable = FseCTable<512, 53>;

/// Table encodings (libzstd's `SymbolEncodingType_e`).
pub(crate) const SET_BASIC: u32 = 0;
pub(crate) const SET_RLE: u32 = 1;
pub(crate) const SET_COMPRESSED: u32 = 2;
pub(crate) const SET_REPEAT: u32 = 3;

/// libzstd's `FSE_repeat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FseRepeat {
    None,
    Check,
    Valid,
}

/// libzstd's `ZSTD_fseCTables_t`.
#[derive(Clone)]
pub(crate) struct FseCTables {
    pub ll: LlCTable,
    pub of: OfCTable,
    pub ml: MlCTable,
    pub ll_repeat: FseRepeat,
    pub of_repeat: FseRepeat,
    pub ml_repeat: FseRepeat,
}

impl FseCTables {
    pub(crate) fn new() -> Self {
        FseCTables {
            ll: LlCTable::default(),
            of: OfCTable::default(),
            ml: MlCTable::default(),
            ll_repeat: FseRepeat::None,
            of_repeat: FseRepeat::None,
            ml_repeat: FseRepeat::None,
        }
    }
}

const K_INVERSE_PROBABILITY_LOG256: [u32; 256] = [
    0, 2048, 1792, 1642, 1536, 1453, 1386, 1329, 1280, 1236, 1197, 1162, 1130, 1100, 1073, 1047,
    1024, 1001, 980, 960, 941, 923, 906, 889, 874, 859, 844, 830, 817, 804, 791, 779, 768, 756,
    745, 734, 724, 714, 704, 694, 685, 676, 667, 658, 650, 642, 633, 626, 618, 610, 603, 595, 588,
    581, 574, 567, 561, 554, 548, 542, 535, 529, 523, 517, 512, 506, 500, 495, 489, 484, 478, 473,
    468, 463, 458, 453, 448, 443, 438, 434, 429, 424, 420, 415, 411, 407, 402, 398, 394, 390, 386,
    382, 377, 373, 370, 366, 362, 358, 354, 350, 347, 343, 339, 336, 332, 329, 325, 322, 318, 315,
    311, 308, 305, 302, 298, 295, 292, 289, 286, 282, 279, 276, 273, 270, 267, 264, 261, 258, 256,
    253, 250, 247, 244, 241, 239, 236, 233, 230, 228, 225, 222, 220, 217, 215, 212, 209, 207, 204,
    202, 199, 197, 194, 192, 190, 187, 185, 182, 180, 178, 175, 173, 171, 168, 166, 164, 162, 159,
    157, 155, 153, 151, 149, 146, 144, 142, 140, 138, 136, 134, 132, 130, 128, 126, 123, 121, 119,
    117, 115, 114, 112, 110, 108, 106, 104, 102, 100, 98, 96, 94, 93, 91, 89, 87, 85, 83, 82, 80,
    78, 76, 74, 73, 71, 69, 67, 66, 64, 62, 61, 59, 57, 55, 54, 52, 50, 49, 47, 46, 44, 42, 41, 39,
    37, 36, 34, 33, 31, 30, 28, 26, 25, 23, 22, 20, 19, 17, 16, 14, 13, 11, 10, 8, 7, 5, 4, 2, 1,
];

/// libzstd's error values compare as huge sizes in the cost comparisons.
pub(crate) const COST_ERROR: usize = usize::MAX;

/// libzstd's `ZSTD_useLowProbCount`.
fn use_low_prob_count(nb_seq: usize) -> bool {
    nb_seq >= 2048
}

/// libzstd's `ZSTD_NCountCost`.
fn ncount_cost(count: &[u32], max: u32, nb_seq: usize, fse_log: u32) -> usize {
    let mut wksp = [0u8; FSE_NCOUNTBOUND];
    let mut norm = [0i16; 64];
    let table_log = fse_optimal_table_log(fse_log, nb_seq, max);
    if fse_normalize_count(
        &mut norm,
        table_log,
        count,
        nb_seq,
        max,
        use_low_prob_count(nb_seq),
    )
    .is_err()
    {
        return COST_ERROR;
    }
    fse_write_ncount(&mut wksp, &norm, max, table_log).unwrap_or(COST_ERROR)
}

/// libzstd's `ZSTD_entropyCost`.
fn entropy_cost(count: &[u32], max: u32, total: usize) -> usize {
    let mut cost = 0u32;
    for &c in &count[..=max as usize] {
        let mut norm = ((256 * c as usize) / total) as u32;
        if c != 0 && norm == 0 {
            norm = 1;
        }
        cost = cost.wrapping_add(c.wrapping_mul(K_INVERSE_PROBABILITY_LOG256[norm as usize]));
    }
    (cost >> 8) as usize
}

/// libzstd's `ZSTD_fseBitCost`.
pub(crate) fn fse_bit_cost<const N: usize, const S: usize>(
    ct: &FseCTable<N, S>,
    count: &[u32],
    max: u32,
) -> usize {
    const ACCURACY_LOG: u32 = 8;
    if ct.max_symbol_value < max {
        return COST_ERROR;
    }
    let table_log = ct.table_log;
    let bad_cost = (table_log + 1) << ACCURACY_LOG;
    let mut cost = 0usize;
    for s in 0..=max {
        let c = count[s as usize];
        let bit_cost = ct.bit_cost(s, ACCURACY_LOG);
        if c == 0 {
            continue;
        }
        if bit_cost >= bad_cost {
            return COST_ERROR;
        }
        cost += c as usize * bit_cost as usize;
    }
    cost >> ACCURACY_LOG
}

/// libzstd's `ZSTD_crossEntropyCost`.
pub(crate) fn cross_entropy_cost(
    norm: &[i16],
    accuracy_log: u32,
    count: &[u32],
    max: u32,
) -> usize {
    let shift = 8 - accuracy_log;
    let mut cost = 0usize;
    for s in 0..=max as usize {
        let norm_acc = if norm[s] != -1 { norm[s] as u32 } else { 1 };
        let norm256 = norm_acc << shift;
        cost += count[s] as usize * K_INVERSE_PROBABILITY_LOG256[norm256 as usize] as usize;
    }
    cost >> 8
}

/// libzstd's `ZSTD_selectEncodingType`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn select_encoding_type<const N: usize, const S: usize>(
    repeat: &mut FseRepeat,
    count: &[u32],
    max: u32,
    most_frequent: usize,
    nb_seq: usize,
    fse_log: u32,
    prev: &FseCTable<N, S>,
    default_norm: &[i16],
    default_norm_log: u32,
    default_allowed: bool,
    strategy: u32,
) -> u32 {
    if most_frequent == nb_seq {
        *repeat = FseRepeat::None;
        if default_allowed && nb_seq <= 2 {
            return SET_BASIC;
        }
        return SET_RLE;
    }
    if strategy < STRAT_LAZY {
        if default_allowed {
            let static_fse_nb_seq_max = 1000;
            let mult = 10 - strategy as usize;
            let base_log = 3;
            let dynamic_fse_nb_seq_min = ((1usize << default_norm_log) * mult) >> base_log;
            if *repeat == FseRepeat::Valid && nb_seq < static_fse_nb_seq_max {
                return SET_REPEAT;
            }
            if nb_seq < dynamic_fse_nb_seq_min || most_frequent < (nb_seq >> (default_norm_log - 1))
            {
                *repeat = FseRepeat::None;
                return SET_BASIC;
            }
        }
    } else {
        let basic_cost = if default_allowed {
            cross_entropy_cost(default_norm, default_norm_log, count, max)
        } else {
            COST_ERROR
        };
        let repeat_cost = if *repeat != FseRepeat::None {
            fse_bit_cost(prev, count, max)
        } else {
            COST_ERROR
        };
        let ncount = ncount_cost(count, max, nb_seq, fse_log);
        let compressed_cost = (ncount << 3).wrapping_add(entropy_cost(count, max, nb_seq));
        if basic_cost <= repeat_cost && basic_cost <= compressed_cost {
            *repeat = FseRepeat::None;
            return SET_BASIC;
        }
        if repeat_cost <= compressed_cost {
            return SET_REPEAT;
        }
    }
    *repeat = FseRepeat::Check;
    SET_COMPRESSED
}

/// libzstd's `ZSTD_buildCTable`: builds `next` for the chosen encoding and
/// writes its description (RLE byte or normalized counts) to `dst`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_ctable<const N: usize, const S: usize>(
    dst: &mut [u8],
    next: &mut FseCTable<N, S>,
    fse_log: u32,
    encoding: u32,
    count: &mut [u32],
    max: u32,
    codes: &[u8],
    default_norm: &[i16],
    default_norm_log: u32,
    default_max: u32,
    prev: &FseCTable<N, S>,
) -> Result<usize> {
    match encoding {
        SET_RLE => {
            next.build_rle(max as u8);
            let Some(d) = dst.first_mut() else {
                return Err(Error::OutputTooSmall);
            };
            *d = codes[0];
            Ok(1)
        }
        SET_REPEAT => {
            next.clone_from(prev);
            Ok(0)
        }
        SET_BASIC => {
            next.build(default_norm, default_max, default_norm_log)?;
            Ok(0)
        }
        _ => {
            let mut nb_seq_1 = codes.len();
            let table_log = fse_optimal_table_log(fse_log, codes.len(), max);
            let last = codes[codes.len() - 1] as usize;
            if count[last] > 1 {
                count[last] -= 1;
                nb_seq_1 -= 1;
            }
            let mut norm = [0i16; 64];
            fse_normalize_count(
                &mut norm,
                table_log,
                count,
                nb_seq_1,
                max,
                use_low_prob_count(nb_seq_1),
            )?;
            let n = fse_write_ncount(dst, &norm, max, table_log)?;
            next.build(&norm, max, table_log)?;
            Ok(n)
        }
    }
}

/// One sequence as libzstd's `SeqDef` stores it: `off_base` (offset + 3,
/// or a repeat code 1-3), the literal length and match length - 3 (both
/// truncated to 16 bits; the code tables carry the long-length fixup).
pub(crate) trait SeqFields {
    fn off_base(&self) -> u32;
    fn lit_length(&self) -> u32;
    fn ml_base(&self) -> u32;
}

/// libzstd's `ZSTD_encodeSequences` (64-bit container: `longOffsets` never
/// applies). Returns the bitstream size.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_sequences<Q: SeqFields>(
    dst: &mut [u8],
    ml_ct: &MlCTable,
    ml_codes: &[u8],
    of_ct: &OfCTable,
    of_codes: &[u8],
    ll_ct: &LlCTable,
    ll_codes: &[u8],
    seqs: &[Q],
) -> Result<usize> {
    let n_seq = seqs.len();
    let mut bw = BitWriter::new(dst).ok_or(Error::OutputTooSmall)?;
    let last = n_seq - 1;
    let mut ml_state = FseCState::new2(ml_ct, u32::from(ml_codes[last]));
    let mut of_state = FseCState::new2(of_ct, u32::from(of_codes[last]));
    let mut ll_state = FseCState::new2(ll_ct, u32::from(ll_codes[last]));
    bw.add_bits(
        u64::from(seqs[last].lit_length()),
        u32::from(LL_BITS[ll_codes[last] as usize]),
    );
    bw.add_bits(
        u64::from(seqs[last].ml_base()),
        u32::from(ML_BITS[ml_codes[last] as usize]),
    );
    bw.add_bits(u64::from(seqs[last].off_base()), u32::from(of_codes[last]));
    bw.flush();
    for n in (0..last).rev() {
        let ll_code = ll_codes[n];
        let of_code = of_codes[n];
        let ml_code = ml_codes[n];
        let ll_bits = u32::from(LL_BITS[ll_code as usize]);
        let of_bits = u32::from(of_code);
        let ml_bits = u32::from(ML_BITS[ml_code as usize]);
        of_state.encode(&mut bw, of_ct, u32::from(of_code));
        ml_state.encode(&mut bw, ml_ct, u32::from(ml_code));
        ll_state.encode(&mut bw, ll_ct, u32::from(ll_code));
        if of_bits + ml_bits + ll_bits >= 64 - 7 - (9 + 9 + 8) {
            bw.flush();
        }
        bw.add_bits(u64::from(seqs[n].lit_length()), ll_bits);
        bw.add_bits(u64::from(seqs[n].ml_base()), ml_bits);
        if of_bits + ml_bits + ll_bits > 56 {
            bw.flush();
        }
        bw.add_bits(u64::from(seqs[n].off_base()), of_bits);
        bw.flush();
    }
    ml_state.flush(&mut bw);
    of_state.flush(&mut bw);
    ll_state.flush(&mut bw);
    match bw.close() {
        0 => Err(Error::OutputTooSmall),
        n => Ok(n),
    }
}
