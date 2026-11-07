//! Sequences section (libzstd's `compress/zstd_compress_sequences.c`):
//! choosing each code's table encoding, building and writing the FSE
//! tables, and the backward sequence bitstream.

use super::bitwriter::BitWriter;
use super::fse_enc::{
    FSE_NCOUNTBOUND, FseCState, FseCTable, fse_normalize_count, fse_optimal_table_log,
    fse_write_ncount,
};
use super::literals::STRAT_LAZY;
use super::seq_store::SeqDef;
use crate::error::{Error, Result};
use crate::fse::{LL_BITS, ML_BITS};

/// The sequence tables: `MaxLL + 1` (36), `MaxOff + 1` (32) and `MaxML + 1` (53)
/// symbols, with the symbol dimension rounded up to a power of two so
/// [`FseCState::encode`] masks the symbol instead of bounds-checking it
/// (every code is in range, so the mask changes nothing).
pub(crate) type LlCTable = FseCTable<512, 64>;
pub(crate) type OfCTable = FseCTable<256, 32>;
pub(crate) type MlCTable = FseCTable<512, 64>;

/// libzstd's `LL_bits` and `ML_bits`, padded to 256 entries so a `u8` code
/// indexes them without a bounds check (the padding is never read).
const LL_BITS_256: [u8; 256] = pad256(&LL_BITS);
const ML_BITS_256: [u8; 256] = pad256(&ML_BITS);

const fn pad256<const N: usize>(t: &[u8; N]) -> [u8; 256] {
    let mut r = [0u8; 256];
    let mut i = 0;
    while i < N {
        r[i] = t[i];
        i += 1;
    }
    r
}

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

/// libzstd's `ZSTD_encodeSequences` (64-bit container: `longOffsets` never
/// applies) with the tables of `cts`. Returns the bitstream size.
///
/// On x86-64 CPUs with BMI2 the loop is [`super::seq_asm`]'s.
pub(crate) fn encode_sequences(
    dst: &mut [u8],
    cts: &FseCTables,
    ll_codes: &[u8],
    of_codes: &[u8],
    ml_codes: &[u8],
    seqs: &[SeqDef],
) -> Result<usize> {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if crate::cpu::Features::detect().bmi2 {
        return encode_sequences_body::<true>(dst, cts, ll_codes, of_codes, ml_codes, seqs);
    }
    encode_sequences_body::<false>(dst, cts, ll_codes, of_codes, ml_codes, seqs)
}

/// The low `n` bits of `v` (`n < 64`).
#[inline(always)]
fn low_bits(v: u64, n: u32) -> u64 {
    debug_assert!(n < 64);
    v & !(u64::MAX << n)
}

/// The most a sequence advances the write position by: 7 pending bits and
/// at most 8 + 9 + 9 state bits and 16 + 16 + 31 extra bits.
const MAX_SEQ_BYTES: usize = (7 + 26 + 63) / 8;

/// [`encode_sequences`]; with `ASM` (BMI2 detected), the sequences whose
/// writes fit in `dst` go through [`super::seq_asm::encode_seqs`].
///
/// libzstd adds each sequence's six fields to the bit container one by one
/// (the three FSE state outputs, then the literal length, match length and
/// offset extra bits), so each field waits for the container's bit
/// position after the one before. Both loops here first pack the fields
/// into one value with shifts local to the sequence, then add and flush it
/// once: the same bits in the same order, so the same bitstream (where a
/// flush happens never changes the output). A sequence of more than 56
/// bits (long lengths with a large offset) is added in parts.
fn encode_sequences_body<const ASM: bool>(
    dst: &mut [u8],
    cts: &FseCTables,
    ll_codes: &[u8],
    of_codes: &[u8],
    ml_codes: &[u8],
    seqs: &[SeqDef],
) -> Result<usize> {
    let n_seq = seqs.len();
    let mut bw = BitWriter::new(dst).ok_or(Error::OutputTooSmall)?;
    let last = n_seq - 1;
    let (ll_codes, of_codes, ml_codes) =
        (&ll_codes[..n_seq], &of_codes[..n_seq], &ml_codes[..n_seq]);
    let mut st = [
        FseCState::new2(&cts.ml, u32::from(ml_codes[last])),
        FseCState::new2(&cts.of, u32::from(of_codes[last])),
        FseCState::new2(&cts.ll, u32::from(ll_codes[last])),
    ];
    bw.add_bits(
        u64::from(seqs[last].lit_length),
        u32::from(LL_BITS_256[ll_codes[last] as usize]),
    );
    bw.add_bits(
        u64::from(seqs[last].ml_base),
        u32::from(ML_BITS_256[ml_codes[last] as usize]),
    );
    bw.add_bits(u64::from(seqs[last].off_base), u32::from(of_codes[last]));
    bw.flush();
    #[allow(unused_mut)] // moved by the asm loop on x86-64
    let mut i = last;
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if ASM {
        // as many sequences as surely fit
        let k = (bw.room() / MAX_SEQ_BYTES).min(i);
        if k > 0 {
            let ctx = super::seq_asm::SeqCtx {
                ll_codes: ll_codes.as_ptr(),
                of_codes: of_codes.as_ptr(),
                ml_codes: ml_codes.as_ptr(),
                seqs: seqs.as_ptr(),
                stop: i - k,
                ll_bits: *LL_BITS_256.first_chunk().unwrap(),
                ml_bits: *ML_BITS_256.first_chunk().unwrap(),
            };
            let mut states = [st[0].value, st[1].value, st[2].value];
            let (mut container, mut bit_pos, mut out) = bw.raw_state();
            // SAFETY: BMI2 was detected (the caller's `ASM`); `ctx.stop <
            // i <= n_seq`, the length of `seqs` and of the code slices;
            // the states come from these tables (`new2`); `bit_pos <= 7`
            // after the flush above, and `room() >= k * MAX_SEQ_BYTES` for
            // the `k` sequences written.
            #[allow(unsafe_code)]
            unsafe {
                super::seq_asm::encode_seqs(
                    &ctx,
                    cts,
                    &mut states,
                    &mut container,
                    &mut bit_pos,
                    &mut out,
                    i,
                );
            }
            bw.set_raw_state(container, bit_pos, out);
            for (s, v) in st.iter_mut().zip(states) {
                s.value = v;
            }
            i -= k;
        }
    }
    // the rest (all of it without `ASM`), with the end-of-buffer clamp
    let rest = seqs[..i]
        .iter()
        .zip(&ll_codes[..i])
        .zip(&of_codes[..i])
        .zip(&ml_codes[..i]);
    for (((seq, &ll_code), &of_code), &ml_code) in rest.rev() {
        let ll_bits = u32::from(LL_BITS_256[ll_code as usize]);
        let of_bits = u32::from(of_code);
        let ml_bits = u32::from(ML_BITS_256[ml_code as usize]);
        debug_assert!(ll_bits <= 16 && ml_bits <= 16 && of_bits <= 31);
        let (of_v, of_n) = st[1].step(&cts.of, u32::from(of_code));
        let (ml_v, ml_n) = st[0].step(&cts.ml, u32::from(ml_code));
        let (ll_v, ll_n) = st[2].step(&cts.ll, u32::from(ll_code));
        // the state outputs: at most 8 + 9 + 9 bits
        let mut p = low_bits(of_v, of_n);
        let mut n = of_n;
        p |= low_bits(ml_v, ml_n) << n;
        n += ml_n;
        p |= low_bits(ll_v, ll_n) << n;
        n += ll_n;
        let lit = low_bits(u64::from(seq.lit_length), ll_bits);
        let mlb = low_bits(u64::from(seq.ml_base), ml_bits);
        let off = low_bits(u64::from(seq.off_base), of_bits);
        // After a flush at most 7 bits are pending, so 56 more fit.
        let total = n + ll_bits + ml_bits + of_bits;
        if total <= 56 {
            p |= lit << n;
            n += ll_bits;
            p |= mlb << n;
            n += ml_bits;
            p |= off << n;
            bw.add_bits_fast(p, total);
            bw.flush();
        } else {
            // at most 26 + 16, then 16 + 31 bits
            bw.add_bits_fast(p | (lit << n), n + ll_bits);
            bw.flush();
            bw.add_bits_fast(mlb | (off << ml_bits), ml_bits + of_bits);
            bw.flush();
        }
    }
    let [ml_state, of_state, ll_state] = st;
    ml_state.flush(&mut bw);
    of_state.flush(&mut bw);
    ll_state.flush(&mut bw);
    match bw.close() {
        0 => Err(Error::OutputTooSmall),
        n => Ok(n),
    }
}

#[cfg(test)]
mod tests {
    use super::super::seq_store::{highbit32, ll_code, ml_code};
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// `ZSTD_encodeSequences` field by field, as libzstd writes it.
    fn reference(
        dst: &mut [u8],
        cts: &FseCTables,
        ll_codes: &[u8],
        of_codes: &[u8],
        ml_codes: &[u8],
        seqs: &[SeqDef],
    ) -> Result<usize> {
        let (ll_ct, of_ct, ml_ct) = (&cts.ll, &cts.of, &cts.ml);
        let mut bw = BitWriter::new(dst).ok_or(Error::OutputTooSmall)?;
        let last = seqs.len() - 1;
        let mut ml_state = FseCState::new2(ml_ct, u32::from(ml_codes[last]));
        let mut of_state = FseCState::new2(of_ct, u32::from(of_codes[last]));
        let mut ll_state = FseCState::new2(ll_ct, u32::from(ll_codes[last]));
        bw.add_bits(
            u64::from(seqs[last].lit_length),
            u32::from(LL_BITS_256[ll_codes[last] as usize]),
        );
        bw.add_bits(
            u64::from(seqs[last].ml_base),
            u32::from(ML_BITS_256[ml_codes[last] as usize]),
        );
        bw.add_bits(u64::from(seqs[last].off_base), u32::from(of_codes[last]));
        bw.flush();
        for i in (0..last).rev() {
            let seq = &seqs[i];
            let (ll_code, of_code, ml_code) = (ll_codes[i], of_codes[i], ml_codes[i]);
            let ll_bits = u32::from(LL_BITS_256[ll_code as usize]);
            let of_bits = u32::from(of_code);
            let ml_bits = u32::from(ML_BITS_256[ml_code as usize]);
            of_state.encode(&mut bw, of_ct, u32::from(of_code));
            ml_state.encode(&mut bw, ml_ct, u32::from(ml_code));
            ll_state.encode(&mut bw, ll_ct, u32::from(ll_code));
            if of_bits + ml_bits + ll_bits >= 64 - 7 - (9 + 9 + 8) {
                bw.flush();
            }
            bw.add_bits(u64::from(seq.lit_length), ll_bits);
            bw.add_bits(u64::from(seq.ml_base), ml_bits);
            if of_bits + ml_bits + ll_bits > 56 {
                bw.flush();
            }
            bw.add_bits(u64::from(seq.off_base), of_bits);
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

    /// A compressed table for the codes' histogram.
    fn table<const N: usize, const S: usize>(codes: &[u8], log: u32, ct: &mut FseCTable<N, S>) {
        let mut count = [0u32; 256];
        for &c in codes {
            count[c as usize] += 1;
        }
        let max = (0..256u32).rev().find(|&s| count[s as usize] != 0).unwrap();
        let mut norm = [0i16; 256];
        let log = fse_optimal_table_log(log, codes.len(), max);
        fse_normalize_count(&mut norm, log, &count, codes.len(), max, false).unwrap();
        ct.build(&norm[..=max as usize], max, log).unwrap();
    }

    /// Both loops write libzstd's bitstream: random sequences with short
    /// or long lengths (up to the 16-bit stored ones) and small or large
    /// offsets (the parts paths), into outputs that fit, fit in part (the
    /// asm loop stops early and the clamped loop overflows) or are too
    /// small.
    #[test]
    fn packed_matches_field_by_field() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let rounds = if cfg!(miri) { 2 } else { 400 };
        for round in 0..rounds {
            let n = 2 + (rnd() % 300) as usize;
            let long = round % 2 == 1;
            let far = round % 4 >= 2;
            let mut seqs = Vec::with_capacity(n);
            for _ in 0..n {
                let mut len = || {
                    let r = rnd();
                    if long && r % 3 == 0 {
                        (r >> 8) as u16
                    } else {
                        (r >> 8) as u16 % 40
                    }
                };
                let (lit_length, ml_base) = (len(), len());
                let bits = if far { 1 + rnd() % 31 } else { 1 + rnd() % 17 };
                let off_base = ((rnd() >> 33) as u32 & ((1u32 << bits) - 1)).max(1);
                seqs.push(SeqDef {
                    off_base,
                    lit_length,
                    ml_base,
                });
            }
            let ll: Vec<u8> = seqs
                .iter()
                .map(|s| ll_code(u32::from(s.lit_length)) as u8)
                .collect();
            let ml: Vec<u8> = seqs
                .iter()
                .map(|s| ml_code(u32::from(s.ml_base)) as u8)
                .collect();
            let of: Vec<u8> = seqs.iter().map(|s| highbit32(s.off_base) as u8).collect();
            let mut cts = FseCTables::new();
            table(&ll, 9, &mut cts.ll);
            table(&ml, 9, &mut cts.ml);
            table(&of, 8, &mut cts.of);
            let cap = n * 16 + 64;
            for size in [cap, cap / 4, cap / 8, 9] {
                let mut want = vec![0u8; size];
                let r = reference(&mut want, &cts, &ll, &of, &ml, &seqs);
                let mut a = vec![0u8; size];
                let ra = encode_sequences(&mut a, &cts, &ll, &of, &ml, &seqs);
                let mut b = vec![0u8; size];
                let rb = encode_sequences_body::<false>(&mut b, &cts, &ll, &of, &ml, &seqs);
                assert_eq!(ra, r, "round {round} size {size}");
                assert_eq!(rb, r, "generic, round {round} size {size}");
                if let Ok(k) = r {
                    assert_eq!(a[..k], want[..k], "round {round}");
                    assert_eq!(b[..k], want[..k], "generic, round {round}");
                }
            }
        }
    }
}
