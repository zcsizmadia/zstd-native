//! The `dfast` strategy (levels 3-4, and level 2 for some input sizes): a
//! port of libzstd 1.5.7's `compress/zstd_double_fast.c` (noDict, extDict
//! and dictMatchState paths). It shares the match state, window and block
//! loop with `fast` (see that module's docs); `hashTable` holds 8-byte
//! hashes ("long") and `chainTable` `minMatch`-byte hashes ("short").

use super::fast::{
    DmsView, ExtWindow, HASH_READ_SIZE, K_SEARCH_STRENGTH, MatchState, SHORT_CACHE_TAG_BITS,
    TableLoadMethod, cand_match, compare_packed_tags, hash_mls, hash_prime, hash_with,
    index_overlap_check, write_tagged_index,
};
use super::params::Strategy;
use super::seq_store::{
    PRIME8BYTES, REPCODE1_TO_OFFBASE, SeqStore, count, count_2segments, hash_at, hash8_ptr,
    offset_to_offbase, read_le64, read32,
};
use crate::cpu::prefetch_l1;

/// `ZSTD_fillDoubleHashTableForCDict()`: as [`fill_double_hash_table`],
/// with tagged entries (see
/// [`MatchState::fill_hash_table_for_cdict`]).
pub(crate) fn fill_double_hash_table_for_cdict(
    ms: &mut MatchState,
    buf: &[u8],
    end: usize,
    dtlm: TableLoadMethod,
) {
    const FAST_HASH_FILL_STEP: usize = 3;
    let h_bits_l = ms.cparams.hash_log + SHORT_CACHE_TAG_BITS;
    let h_bits_s = ms.cparams.chain_log + SHORT_CACHE_TAG_BITS;
    let mls = ms.cparams.min_match;
    assert!(end <= buf.len() && end >= HASH_READ_SIZE);
    let iend = end - HASH_READ_SIZE;
    let mut ip = ms.window.pos(ms.next_to_update);
    while ip + FAST_HASH_FILL_STEP - 1 <= iend {
        let curr = ms.window.index(ip);
        for i in 0..FAST_HASH_FILL_STEP {
            let sm_hash_and_tag = hash_at(buf, ip + i, h_bits_s, mls);
            let lg_hash_and_tag = hash_at(buf, ip + i, h_bits_l, 8);
            if i == 0 {
                write_tagged_index(&mut ms.chain_table, sm_hash_and_tag, curr);
            }
            if i == 0 || ms.hash_table[lg_hash_and_tag >> SHORT_CACHE_TAG_BITS] == 0 {
                write_tagged_index(&mut ms.hash_table, lg_hash_and_tag, curr + i as u32);
            }
            // Only load extra positions for ZSTD_dtlm_full
            if dtlm == TableLoadMethod::Fast {
                break;
            }
        }
        ip += FAST_HASH_FILL_STEP;
    }
}

/// `ZSTD_fillDoubleHashTable()` for a CCtx (`ZSTD_fillDoubleHashTableForCCtx`):
/// inserts positions from `nextToUpdate` up to `end - HASH_READ_SIZE` of
/// `buf` (used to load a dictionary's content).
pub(crate) fn fill_double_hash_table(
    ms: &mut MatchState,
    buf: &[u8],
    end: usize,
    dtlm: TableLoadMethod,
) {
    const FAST_HASH_FILL_STEP: usize = 3;
    let h_bits_l = ms.cparams.hash_log;
    let h_bits_s = ms.cparams.chain_log;
    let mls = ms.cparams.min_match;
    assert!(end <= buf.len() && end >= HASH_READ_SIZE);
    let iend = end - HASH_READ_SIZE;
    let mut ip = ms.window.pos(ms.next_to_update);
    while ip + FAST_HASH_FILL_STEP - 1 <= iend {
        let curr = ms.window.index(ip);
        for i in 0..FAST_HASH_FILL_STEP {
            let sm_hash = hash_at(buf, ip + i, h_bits_s, mls);
            let lg_hash = hash_at(buf, ip + i, h_bits_l, 8);
            if i == 0 {
                ms.chain_table[sm_hash] = curr;
            }
            if i == 0 || ms.hash_table[lg_hash] == 0 {
                ms.hash_table[lg_hash] = curr + i as u32;
            }
            if dtlm == TableLoadMethod::Fast {
                break;
            }
        }
        ip += FAST_HASH_FILL_STEP;
    }
}

/// `ZSTD_compressBlock_doubleFast()`: finds the sequences of the block
/// `buf[start..end]` (no dictionary); the same contract as
/// [`compress_block_fast`](super::fast::compress_block_fast). Picks the
/// `ZSTD_compressBlock_doubleFast_noDict_<mls>` variant like libzstd
/// (3 and other values use 4).
#[inline(always)]
pub(crate) fn compress_block_double_fast(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    debug_assert!(ms.cparams.strategy == Strategy::DFast);
    match ms.cparams.min_match {
        5 => no_dict_generic::<5>(ms, ss, rep, buf, start, end),
        6 => no_dict_generic::<6>(ms, ss, rep, buf, start, end),
        7 => no_dict_generic::<7>(ms, ss, rep, buf, start, end),
        _ => no_dict_generic::<4>(ms, ss, rep, buf, start, end),
    }
}

/// Loop invariants of the noDict position search ([`search_rust`]).
pub(super) struct Search {
    /// Return once the next position is at or past `lim`
    /// ([`EXIT_LIMIT`]).
    pub(super) lim: usize,
    pub(super) step: usize,
    /// Short hash: `(read64(p) * prime_s) >> shift_s` (see
    /// [`hash_prime`]).
    pub(super) prime_s: u64,
    /// Long hash: `(read64(p) * prime_l) >> shift_l` (`ZSTD_hash8Ptr`).
    pub(super) prime_l: u64,
    pub(super) base: *const u8,
    pub(super) hash_long: *mut u32,
    pub(super) hash_small: *mut u32,
    /// `prefixLowestIndex`.
    pub(super) low: u64,
    /// `64 - chainLog`.
    pub(super) shift_s: u64,
    /// `64 - hashLog`.
    pub(super) shift_l: u64,
    /// Readable window `[lowest, iend)`, for debug assertions.
    pub(super) lowest: *const u8,
    pub(super) iend: *const u8,
}

/// Position search state, in and out of [`search_rust`].
pub(super) struct Cursor {
    pub(super) ip: *const u8,
    pub(super) ip1: *const u8,
    /// In: `hl0` (the long hash of `ip`). Out: `hl1` (of `ip1`), except at
    /// [`EXIT_REP`].
    pub(super) hl: usize,
    /// In: `idxl0` (the long candidate of `ip`). Out: `idxl0` at
    /// [`EXIT_REP`] / [`EXIT_LONG`], `idxl1` (of `ip1`) at [`EXIT_SHORT`] /
    /// [`EXIT_LIMIT`].
    pub(super) idxl: u64,
    /// Out: `idxs0` (the short candidate of `ip`).
    pub(super) idxs: u64,
}

/// [`search`] found a repcode match at `ip + 1`.
pub(super) const EXIT_REP: u32 = 0;
/// [`search`] found a long (8-byte) match at `ip`.
pub(super) const EXIT_LONG: u32 = 1;
/// [`search`] found a short (4-byte) match at `ip`.
pub(super) const EXIT_SHORT: u32 = 2;
/// No match at `ip`, and the next position (`ip1`, now `c.ip`, with its
/// `hl`/`idxl` in `c`) is at or past `lim`: the caller updates the step and
/// limits, sets `c.ip1` and continues.
pub(super) const EXIT_LIMIT: u32 = 3;
/// The inner loop of `ZSTD_compressBlock_doubleFast_noDict_generic`
/// (positions `ip`, `ip1 = ip + step`, ...): inserts each position in both
/// tables, checks the repcode at `ip + 1`, then the long and the short
/// candidates at `ip`, until one matches or the next position reaches
/// `lim`. Returns the `EXIT_*` code with `c` describing the position (see
/// [`Cursor`]).
///
/// # Safety
/// The invariants of `no_dict_generic`: `ip < ip1 <= iend - 8`, and
/// `ip + step <= iend - 8` for every position `ip < lim`; the index of
/// `ip` is at least `s.low`; `ip + 1 - offset_1` is readable; the tables
/// have `1 << h_bits` entries.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn search_rust<const MLS: u32>(s: &Search, c: &mut Cursor, offset_1: u32) -> u32 {
    // ~random data, unlikely to match: read instead of the table candidate
    // when that is invalid, to avoid an unpredictable branch.
    static DUMMY: [u8; 10] = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0xe2, 0xb4];
    let chk = |p: *const u8, n: usize| {
        debug_assert!(
            (p as usize) >= (s.lowest as usize) && (p as usize) + n <= (s.iend as usize),
            "read out of the window"
        );
    };
    let mut ip = c.ip;
    let mut ip1 = c.ip1;
    let mut hl0 = c.hl;
    let mut idxl0 = c.idxl as u32;
    let (step, lim, low) = (s.step, s.lim, s.low as u32);
    // SAFETY: per the function contract; candidates are only read when
    // their index is at least `s.low`.
    unsafe {
        loop {
            chk(ip, HASH_READ_SIZE);
            chk(ip1, HASH_READ_SIZE);
            let curr = (ip as usize).wrapping_sub(s.base as usize) as u32;
            debug_assert!(curr >= low);
            let hs0 = hash_with(ip, hash_prime::<MLS>(), s.shift_s);
            let idxs0 = *s.hash_small.add(hs0);

            // update hash tables
            *s.hash_long.add(hl0) = curr;
            *s.hash_small.add(hs0) = curr;

            // check noDict repcode (`ip + 1 - 0` is readable, so the bytes
            // are compared first: one branch per position)
            let r = ip.wrapping_add(1).wrapping_sub(offset_1 as usize);
            chk(r, 4);
            if (read32(r) ^ read32(ip.add(1))) | u32::from(offset_1 == 0) == 0 {
                *c = Cursor {
                    ip,
                    ip1,
                    hl: hl0,
                    idxl: u64::from(idxl0),
                    idxs: u64::from(idxs0),
                };
                return EXIT_REP;
            }

            let hl1 = hash_with(ip1, PRIME8BYTES, s.shift_l);

            // check prefix long match (the branchless select is libzstd's
            // ZSTD_selectAddr)
            let matchl0 = s.base.wrapping_add(idxl0 as usize);
            let l0_valid = idxl0 >= low;
            let matchl0_safe = core::hint::select_unpredictable(l0_valid, matchl0, DUMMY.as_ptr());
            if l0_valid {
                chk(matchl0_safe, 8);
            }
            if read_le64(matchl0_safe) == read_le64(ip) && l0_valid {
                *c = Cursor {
                    ip,
                    ip1,
                    hl: hl1,
                    idxl: u64::from(idxl0),
                    idxs: u64::from(idxs0),
                };
                return EXIT_LONG;
            }

            let idxl1 = *s.hash_long.add(hl1);

            // check prefix short match
            let matchs0 = s.base.wrapping_add(idxs0 as usize);
            let s0_valid = idxs0 >= low;
            let matchs0_safe = core::hint::select_unpredictable(s0_valid, matchs0, DUMMY.as_ptr());
            if s0_valid {
                chk(matchs0_safe, 4);
            }
            if read32(matchs0_safe) == read32(ip) && s0_valid {
                *c = Cursor {
                    ip,
                    ip1,
                    hl: hl1,
                    idxl: u64::from(idxl1),
                    idxs: u64::from(idxs0),
                };
                return EXIT_SHORT;
            }

            ip = ip1;
            ip1 = ip1.wrapping_add(step);
            hl0 = hl1;
            idxl0 = idxl1;
            if ip as usize >= lim {
                *c = Cursor {
                    ip,
                    ip1,
                    hl: hl0,
                    idxl: u64::from(idxl0),
                    idxs: u64::from(idxs0),
                };
                return EXIT_LIMIT;
            }
        }
    }
}

/// `ZSTD_compressBlock_doubleFast_noDict_generic()`.
///
/// Memory safety: as for `fast`: the current positions `ip`/`ip1` stay at
/// or below `ilimit = iend - HASH_READ_SIZE` before 8-byte reads; table
/// candidates are read only when their index is at or above
/// `prefixLowestIndex` (`>= dictLimit`, so in `buf`) and below the current
/// index; repcodes are checked against the window at entry and later are
/// real offsets of earlier matches (debug-asserted with `chk`).
#[allow(unsafe_code)]
#[inline(always)]
fn no_dict_generic<const MLS: u32>(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    // how many positions to search before increasing step size
    const K_STEP_INCR: usize = 1 << K_SEARCH_STRENGTH;

    let cparams = ms.cparams;
    let h_bits_l = cparams.hash_log;
    let h_bits_s = cparams.chain_log;
    // These checks make the raw-pointer accesses below sound.
    assert!(start <= end && end <= buf.len());
    assert!(ms.hash_table.len() == 1usize << h_bits_l && h_bits_l <= 32);
    assert!(ms.chain_table.len() == 1usize << h_bits_s && h_bits_s <= 32);
    // With an extDict, only through the extDict variant when the window has
    // moved past it: every read is then in the prefix.
    let window_start = ms.window.pos(ms.window.dict_limit);
    assert!(window_start <= start, "window start is not in the buffer");
    assert!(end.wrapping_sub(ms.window.base) <= u32::MAX as usize);
    // Room for the sequences stored with `store_seq_unchecked`.
    ss.reserve(end - start);

    let bp = buf.as_ptr();
    let base = bp.wrapping_add(ms.window.base);
    let istart = bp.wrapping_add(start);
    let end_index = ms.window.index(end);
    let prefix_lowest_index = ms.lowest_prefix_index(end_index, cparams.window_log);
    let prefix_lowest = base.wrapping_add(prefix_lowest_index as usize);
    let iend = bp.wrapping_add(end);
    let ilimit = iend.wrapping_sub(HASH_READ_SIZE);
    let lowest = bp.wrapping_add(window_start);
    let hash_long = ms.hash_table.as_mut_ptr();
    let hash_small = ms.chain_table.as_mut_ptr();
    let idx = |p: *const u8| (p as usize).wrapping_sub(base as usize) as u32;
    // Debug check that `n` bytes at `p` are inside the readable window.
    let chk = |p: *const u8, n: usize| {
        debug_assert!(
            (p as usize) >= (lowest as usize) && (p as usize) + n <= (iend as usize),
            "read out of the window"
        );
    };

    let mut anchor = istart;
    let mut ip = istart;
    let mut offset_1 = rep[0];
    let mut offset_2 = rep[1];
    let mut offset_saved1 = 0u32;
    let mut offset_saved2 = 0u32;

    // init
    if ip == prefix_lowest {
        ip = ip.wrapping_add(1);
    }
    {
        let current = idx(ip);
        let window_low = ms.lowest_prefix_index(current, cparams.window_log);
        let max_rep = current.wrapping_sub(window_low);
        if offset_2 > max_rep {
            offset_saved2 = offset_2;
            offset_2 = 0;
        }
        if offset_1 > max_rep {
            offset_saved1 = offset_1;
            offset_1 = 0;
        }
    }

    // SAFETY (whole block): see the function docs. Table writes use hashes
    // of `h_bits_l` / `h_bits_s` bits into tables of that many entries
    // (asserted above); reads are debug-checked with `chk`.
    unsafe {
        let hash_l = |p: *const u8| {
            chk(p, HASH_READ_SIZE);
            hash8_ptr(p, h_bits_l)
        };
        let hash_s = |p: *const u8| {
            chk(p, HASH_READ_SIZE);
            hash_mls::<MLS>(p, h_bits_s)
        };

        let mut s = Search {
            lim: 0,
            step: 1,
            prime_s: hash_prime::<MLS>(),
            prime_l: PRIME8BYTES,
            base,
            hash_long,
            hash_small,
            low: u64::from(prefix_lowest_index),
            shift_s: u64::from(64 - h_bits_s),
            shift_l: u64::from(64 - h_bits_l),
            lowest,
            iend,
        };

        // Outer loop: one iteration per match found and stored.
        'outer: loop {
            // The rest of the block in asm once the first repcode is valid
            // (the asm has no `offset_1 == 0` case): the whole loop, match
            // handling included, see `block_asm`.
            #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
            if offset_1 != 0 && crate::cpu::Features::detect().bmi2 {
                use super::seq_store::{LongLengthType, SeqDef, WILDCOPY_OVERLENGTH};
                let lit_base = ss.lit_buf.as_mut_ptr();
                let seq_base = ss.sequences.as_mut_ptr();
                let mut ctx = super::block_asm::DfastCtx {
                    prime_s: hash_prime::<MLS>(),
                    prime_l: PRIME8BYTES,
                    shift_s: u64::from(64 - h_bits_s),
                    shift_l: u64::from(64 - h_bits_l),
                    low: u64::from(prefix_lowest_index),
                    rep_disp: 1usize.wrapping_sub(offset_1 as usize),
                    step: 1,
                    lim: 0,
                    next_step: 0,
                    anchor,
                    iend,
                    ilimit,
                    prefix_lowest,
                    lit_limit_w: (iend as usize).saturating_sub(WILDCOPY_OVERLENGTH),
                    lit: lit_base.add(ss.lit_len),
                    seq: seq_base.add(ss.nb_seq),
                    off1: u64::from(offset_1),
                    off2: u64::from(offset_2),
                    llt: 0,
                    llpos: core::ptr::null_mut(),
                    curr_pos: core::ptr::null(),
                };
                // SAFETY: the invariants of this function at the top of
                // the outer loop (see the docs), `ss.reserve(end - start)`
                // above, and the CPU has BMI1/BMI2 (`Features::detect`).
                super::block_asm::dfast_block(&mut ctx, ip, hash_long, hash_small, base);
                anchor = ctx.anchor;
                offset_1 = ctx.off1 as u32;
                offset_2 = ctx.off2 as u32;
                debug_assert!(istart <= anchor && anchor <= iend);
                debug_assert!(offset_1 != 0 && (ctx.off1 >> 32) == 0 && (ctx.off2 >> 32) == 0);
                debug_assert!(ctx.lit >= lit_base && ctx.seq >= seq_base);
                ss.lit_len = ctx.lit as usize - lit_base as usize;
                ss.nb_seq = (ctx.seq as usize - seq_base as usize) / size_of::<SeqDef>();
                debug_assert!(ss.lit_len <= ss.max_nb_lit && ss.nb_seq <= ss.max_nb_seq);
                if ctx.llt != 0 {
                    debug_assert!(ss.long_length_type == LongLengthType::None);
                    ss.long_length_type = if ctx.llt == 1 {
                        LongLengthType::LiteralLength
                    } else {
                        LongLengthType::MatchLength
                    };
                    ss.long_length_pos =
                        ((ctx.llpos as usize - seq_base as usize) / size_of::<SeqDef>()) as u32;
                }
                break 'outer;
            }

            let mut step = 1usize;
            let mut next_step = ip.wrapping_add(K_STEP_INCR);
            let mut ip1 = ip.wrapping_add(step);

            if ip1 > ilimit {
                break 'outer;
            }
            // One limit for the two end-of-position tests (`ip1 >=
            // nextStep`, and `ip1 + step > ilimit` after advancing): while
            // `ip1 < lim` neither fires.
            s.lim = (next_step as usize).min((ilimit as usize + 1).wrapping_sub(step));
            s.step = step;

            let hl0 = hash_l(ip);
            let mut cur = Cursor {
                ip,
                ip1,
                hl: hl0,
                idxl: u64::from(*hash_long.add(hl0)),
                idxs: 0,
            };
            let hl1: usize;
            let curr: u32;

            // Inner loop: the position search, until it finds a candidate
            // (or a step / limit check is due). Yields `Some((offset,
            // mLength))` for a match to store (with `ip` moved to its
            // start), `None` for a stored repcode match.
            let found: Option<(u32, usize)>;
            let m_length: usize;
            loop {
                // SAFETY: the invariants above (`ip1 <= ilimit`, `lim`, the
                // window reads debug-checked).
                let exit = search_rust::<MLS>(&s, &mut cur, offset_1);
                if exit == EXIT_LIMIT {
                    ip1 = cur.ip;
                    if ip1 >= next_step {
                        prefetch_l1(ip1.wrapping_add(64));
                        prefetch_l1(ip1.wrapping_add(128));
                        step += 1;
                        next_step = next_step.wrapping_add(K_STEP_INCR);
                    }
                    if ip1.wrapping_add(step) > ilimit {
                        break 'outer;
                    }
                    s.lim = (next_step as usize).min((ilimit as usize + 1).wrapping_sub(step));
                    s.step = step;
                    // `cur` is at ip1 already
                    cur.ip1 = ip1.wrapping_add(step);
                    continue;
                }
                ip = cur.ip;
                ip1 = cur.ip1;
                curr = idx(ip);
                hl1 = cur.hl;
                if exit == EXIT_REP {
                    let r = ip.add(1 + 4).sub(offset_1 as usize);
                    let m = count(ip.add(1 + 4), r, iend) + 4;
                    ip = ip.add(1);
                    ss.store_seq_unchecked(
                        buf,
                        anchor as usize - bp as usize,
                        ip as usize - anchor as usize,
                        end,
                        REPCODE1_TO_OFFBASE,
                        m,
                    );
                    found = None;
                    m_length = m;
                } else if exit == EXIT_LONG {
                    let mut matchl0 = base.wrapping_add(cur.idxl as usize);
                    chk(matchl0, 8);
                    let mut m = count(ip.add(8), matchl0.add(8), iend) + 8;
                    let offset = (ip as usize - matchl0 as usize) as u32;
                    // catch up
                    while ((ip > anchor) & (matchl0 > prefix_lowest))
                        && *ip.sub(1) == *matchl0.sub(1)
                    {
                        ip = ip.sub(1);
                        matchl0 = matchl0.sub(1);
                        m += 1;
                    }
                    found = Some((offset, m));
                    m_length = m;
                } else {
                    // _search_next_long: short match found, check for a
                    // longer one
                    let idxl1 = cur.idxl as u32;
                    let matchl1 = base.wrapping_add(idxl1 as usize);
                    let mut matchs0 = base.wrapping_add(cur.idxs as usize);
                    chk(matchs0, 4);
                    let mut m = count(ip.add(4), matchs0.add(4), iend) + 4;
                    let mut offset = (ip as usize - matchs0 as usize) as u32;

                    // check long match at +1 position
                    if idxl1 > prefix_lowest_index && {
                        chk(matchl1, 8);
                        read_le64(matchl1) == read_le64(ip1)
                    } {
                        let l1len = count(ip1.add(8), matchl1.add(8), iend) + 8;
                        if l1len > m {
                            // use the long match instead
                            ip = ip1;
                            m = l1len;
                            offset = (ip as usize - matchl1 as usize) as u32;
                            matchs0 = matchl1;
                        }
                    }

                    // complete backward
                    while ((ip > anchor) & (matchs0 > prefix_lowest))
                        && *ip.sub(1) == *matchs0.sub(1)
                    {
                        ip = ip.sub(1);
                        matchs0 = matchs0.sub(1);
                        m += 1;
                    }
                    found = Some((offset, m));
                    m_length = m;
                }
                break;
            }

            if let Some((offset, m)) = found {
                // _match_found
                offset_2 = offset_1;
                offset_1 = offset;

                if step < 4 {
                    // ip1 < the new ip (a match is at least 4 bytes long),
                    // so this position is safe to insert.
                    *hash_long.add(hl1) = idx(ip1);
                }

                ss.store_seq_unchecked(
                    buf,
                    anchor as usize - bp as usize,
                    ip as usize - anchor as usize,
                    end,
                    offset_to_offbase(offset),
                    m,
                );
            }

            // _match_stored
            ip = ip.add(m_length);
            anchor = ip;

            if ip <= ilimit {
                // Complementary insertion, done after the ilimit test as
                // candidates could be > iend - 8.
                let index_to_insert = curr + 2;
                let p = base.wrapping_add(index_to_insert as usize);
                *hash_long.add(hash_l(p)) = index_to_insert;
                *hash_long.add(hash_l(ip.sub(2))) = idx(ip.sub(2));
                *hash_small.add(hash_s(p)) = index_to_insert;
                *hash_small.add(hash_s(ip.sub(1))) = idx(ip.sub(1));

                // check immediate repcode
                while ip <= ilimit && offset_2 > 0 && {
                    let r = ip.wrapping_sub(offset_2 as usize);
                    chk(r, 4);
                    read32(ip) == read32(r)
                } {
                    // store sequence
                    let r_length = count(ip.add(4), ip.add(4).sub(offset_2 as usize), iend) + 4;
                    core::mem::swap(&mut offset_1, &mut offset_2);
                    *hash_small.add(hash_s(ip)) = idx(ip);
                    *hash_long.add(hash_l(ip)) = idx(ip);
                    ss.store_seq_unchecked(
                        buf,
                        anchor as usize - bp as usize,
                        0,
                        end,
                        REPCODE1_TO_OFFBASE,
                        r_length,
                    );
                    ip = ip.add(r_length);
                    anchor = ip;
                }
            }
        }
    }

    // _cleanup: if offset_1 started invalid and became valid, rotate the
    // saved offsets (see `fast`).
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

/// `ZSTD_compressBlock_doubleFast_extDict()`: as
/// [`compress_block_double_fast`], for a window with an extDict segment
/// (see [`compress_block_fast_ext_dict`](super::fast::compress_block_fast_ext_dict)).
#[inline(always)]
pub(crate) fn compress_block_double_fast_ext_dict(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    debug_assert!(ms.cparams.strategy == Strategy::DFast);
    let end_index = ms.window.index(end);
    let low_limit = ms.lowest_match_index(end_index, ms.cparams.window_log);
    // if extDict is invalidated due to maxDistance, switch to "regular" variant
    if ms.window.dict_limit.max(low_limit) == low_limit {
        return compress_block_double_fast(ms, ss, rep, buf, start, end);
    }
    ext_dict(ms, ss, rep, buf, start, end, low_limit)
}

/// The extDict match finder proper, out of line (with a BMI2 version).
#[inline(never)]
fn ext_dict(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
    low_limit: u32,
) -> usize {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if crate::cpu::Features::detect().bmi2 {
        // SAFETY: BMI2 (with BMI1 and LZCNT) was detected on this CPU.
        #[allow(unsafe_code)]
        return unsafe { ext_dict_bmi2(ms, ss, rep, buf, start, end, low_limit) };
    }
    ext_dict_mls(ms, ss, rep, buf, start, end, low_limit)
}

/// [`ext_dict`] compiled with BMI1, BMI2 and LZCNT (`shrx` hashes, `tzcnt`
/// match lengths).
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[inline(never)]
#[target_feature(enable = "bmi1,bmi2,lzcnt")]
fn ext_dict_bmi2(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
    low_limit: u32,
) -> usize {
    ext_dict_mls(ms, ss, rep, buf, start, end, low_limit)
}

/// The `ZSTD_compressBlock_doubleFast_extDict_<mls>` variant like libzstd
/// (3 and other values use 4).
#[inline(always)]
fn ext_dict_mls(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
    low_limit: u32,
) -> usize {
    // Candidate checks with a `cmov` (meta/zstd#4764 always uses it in
    // extDict mode) once the window is about as large as the short table.
    // With smaller windows nearly every candidate is stale, so the
    // branch on its index predicts well and is faster. Measured on Zen 3
    // (cycles, flush every 4 KiB): the branch is 1-8% faster up to
    // `windowLog = chainLog - 1` (and at level 2's 32 KiB window), the
    // `cmov` 3-10% faster from `chainLog` up and with the default windows.
    let use_cmov = ms.cparams.window_log >= ms.cparams.chain_log.max(16);
    match (ms.cparams.min_match, use_cmov) {
        (5, true) => ext_dict_generic::<5, true>(ms, ss, rep, buf, start, end, low_limit),
        (6, true) => ext_dict_generic::<6, true>(ms, ss, rep, buf, start, end, low_limit),
        (7, true) => ext_dict_generic::<7, true>(ms, ss, rep, buf, start, end, low_limit),
        (_, true) => ext_dict_generic::<4, true>(ms, ss, rep, buf, start, end, low_limit),
        (5, false) => ext_dict_generic::<5, false>(ms, ss, rep, buf, start, end, low_limit),
        (6, false) => ext_dict_generic::<6, false>(ms, ss, rep, buf, start, end, low_limit),
        (7, false) => ext_dict_generic::<7, false>(ms, ss, rep, buf, start, end, low_limit),
        (_, false) => ext_dict_generic::<4, false>(ms, ss, rep, buf, start, end, low_limit),
    }
}

/// `ZSTD_compressBlock_doubleFast_extDict_generic()`, with the same parse
/// and the candidate checks of meta/zstd#4764 (which keep the output): a
/// table candidate's segment and validity are selected without branches,
/// an invalid candidate reads a dummy array instead, and its index is
/// tested only once the bytes match (rare, so predictable). In extDict
/// mode the valid range moves with every block, so stale entries are
/// common. (#4764's other change, its noDict-like pipelined loop, changes
/// the parse; loading the next position's table entries ahead within this
/// parse, and prefetching them a few positions ahead, measured slower: the
/// loop is bound by its instruction count, not by the table misses.)
///
/// Memory safety: as `fast`'s extDict variant ([`ExtWindow`] checks both
/// segments; candidates are read above `dictStartIndex`, in the segment of
/// their index; repcodes pass `ZSTD_index_overlap_check` and the window
/// check before being read).
///
/// `low_limit` is `ZSTD_getLowestMatchIndex()` at the block's end, computed
/// once by [`compress_block_double_fast_ext_dict`].
#[allow(unsafe_code)]
#[inline(always)]
fn ext_dict_generic<const MLS: u32, const CMOV: bool>(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
    low_limit: u32,
) -> usize {
    let cparams = ms.cparams;
    let h_bits_l = cparams.hash_log;
    let h_bits_s = cparams.chain_log;
    assert!(ms.hash_table.len() == 1usize << h_bits_l && (1..=32).contains(&h_bits_l));
    assert!(ms.chain_table.len() == 1usize << h_bits_s && (1..=32).contains(&h_bits_s));
    debug_assert_eq!(
        low_limit,
        ms.lowest_match_index(ms.window.index(end), cparams.window_log)
    );
    let dict_start_index = low_limit;
    let dict_limit = ms.window.dict_limit;
    let prefix_start_index = dict_limit.max(low_limit);
    debug_assert!(prefix_start_index != dict_start_index);

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
    let dict_base = w.dict_base;
    let istart = w.istart;
    let iend = w.iend;
    let ilimit = iend.wrapping_sub(HASH_READ_SIZE);
    let hash_long = ms.hash_table.as_mut_ptr();
    let hash_small = ms.chain_table.as_mut_ptr();
    let prime_s = hash_prime::<MLS>();
    let shift_s = u64::from(64 - h_bits_s);
    let shift_l = u64::from(64 - h_bits_l);
    let mut offset_1 = rep[0];
    let mut offset_2 = rep[1];

    let mut ip = istart;
    let mut anchor = istart;

    // The whole loop in asm (`block_asm::dfast_ext_block`, same parse and
    // checks as the `CMOV` loop below): 3-6% faster, see `block_asm`.
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if CMOV && crate::cpu::Features::detect().bmi2 {
        use super::seq_store::{LongLengthType, SeqDef, WILDCOPY_OVERLENGTH};
        // Room for the sequences the asm stores (no per-sequence checks).
        ss.reserve(end - start);
        let lit_base = ss.lit_buf.as_mut_ptr();
        let seq_base = ss.sequences.as_mut_ptr();
        let mut ctx = super::block_asm::DfastExtCtx {
            prime_s,
            prime_l: PRIME8BYTES,
            shift_s,
            shift_l,
            prefix_start_index: u64::from(prefix_start_index),
            dict_start_index: u64::from(dict_start_index),
            anchor,
            iend,
            ilimit,
            prefix_start: w.prefix_start,
            dict_start: w.dict_start,
            dict_end: w.dict_end,
            lit_limit_w: (iend as usize).saturating_sub(WILDCOPY_OVERLENGTH),
            // SAFETY: `lit_len <= max_nb_lit` and `nb_seq <= max_nb_seq`,
            // inside the buffers (`reserve`).
            lit: unsafe { lit_base.add(ss.lit_len) },
            seq: unsafe { seq_base.add(ss.nb_seq) },
            off1: u64::from(offset_1),
            off2: u64::from(offset_2),
            llt: 0,
            llpos: core::ptr::null_mut(),
            curr_pos: core::ptr::null(),
            low_match: core::ptr::null(),
            vend: core::ptr::null(),
        };
        // SAFETY: the window checks of `ExtWindow::new` above, the table
        // sizes asserted above, `ss.reserve(end - start)`, `ip = anchor =
        // istart` in the prefix, and the CPU has BMI1/BMI2
        // (`Features::detect`).
        unsafe {
            super::block_asm::dfast_ext_block(&mut ctx, ip, hash_long, hash_small, base, dict_base);
        }
        anchor = ctx.anchor;
        debug_assert!(istart <= anchor && anchor <= iend);
        debug_assert!((ctx.off1 >> 32) == 0 && (ctx.off2 >> 32) == 0);
        debug_assert!(ctx.lit >= lit_base && ctx.seq >= seq_base);
        ss.lit_len = ctx.lit as usize - lit_base as usize;
        ss.nb_seq = (ctx.seq as usize - seq_base as usize) / size_of::<SeqDef>();
        debug_assert!(ss.lit_len <= ss.max_nb_lit && ss.nb_seq <= ss.max_nb_seq);
        if ctx.llt != 0 {
            debug_assert!(ss.long_length_type == LongLengthType::None);
            ss.long_length_type = if ctx.llt == 1 {
                LongLengthType::LiteralLength
            } else {
                LongLengthType::MatchLength
            };
            ss.long_length_pos =
                ((ctx.llpos as usize - seq_base as usize) / size_of::<SeqDef>()) as u32;
        }
        rep[0] = ctx.off1 as u32;
        rep[1] = ctx.off2 as u32;
        return iend as usize - anchor as usize;
    }

    // The position of a valid table candidate `i` (`> dictStartIndex`), or
    // the dummy array: no branch on either test.
    let cand = |i: u32| {
        let seg = core::hint::select_unpredictable(i < prefix_start_index, dict_base, base);
        (i > dict_start_index, seg.wrapping_add(i as usize))
    };

    // SAFETY (whole block): see the function docs. Hash table writes use
    // hashes of `h_bits_l`/`h_bits_s` bits into tables of that many
    // entries (asserted above).
    unsafe {
        let hash_l = |p: *const u8| {
            w.chk(p, 8);
            hash_with(p, PRIME8BYTES, shift_l)
        };
        let hash_s = |p: *const u8| {
            w.chk(p, 8);
            hash_with(p, prime_s, shift_s)
        };

        // The table entries of position `p`: (short hash, long hash, short
        // candidate, long candidate).
        let load = |p: *const u8| {
            let (hs, hl) = (hash_s(p), hash_l(p));
            (hs, hl, *hash_small.add(hs), *hash_long.add(hl))
        };

        // Search Loop
        'search: while ip < ilimit {
            // < instead of <=, because (ip+1)
            let (mut h_small, mut h_long, mut match_index, mut match_long_index) = load(ip);
            let curr: u32;
            let m_length: usize;

            // Positions without a match.
            loop {
                let c = w.idx(ip);
                *hash_small.add(h_small) = c; // update hash table
                *hash_long.add(h_long) = c;

                let next =
                    ip.wrapping_add(((ip as usize - anchor as usize) >> K_SEARCH_STRENGTH) + 1);

                let rep_index = c.wrapping_add(1).wrapping_sub(offset_1); // offset_1 expected <= curr + 1
                if (index_overlap_check(prefix_start_index, rep_index)
                    & (offset_1 <= c.wrapping_add(1).wrapping_sub(dict_start_index)))
                    && {
                        // note: we are searching at curr+1
                        let rep_match = w.pos(rep_index);
                        w.chk(rep_match, 4);
                        read32(rep_match) == read32(ip.add(1))
                    }
                {
                    let rep_match = w.pos(rep_index);
                    let rep_match_end = w.seg_end(rep_index);
                    m_length = w.count2(ip.add(1 + 4), rep_match.add(4), rep_match_end) + 4;
                    ip = ip.add(1);
                    ss.store_seq(
                        buf,
                        anchor as usize - bp as usize,
                        ip as usize - anchor as usize,
                        end,
                        REPCODE1_TO_OFFBASE,
                        m_length,
                    );
                    curr = c;
                    break;
                }

                let (valid, ml) = cand(match_long_index);
                // A valid candidate is read in place (checked here, debug
                // builds only), an invalid one may read the dummy array.
                #[cfg(debug_assertions)]
                if valid {
                    w.chk(ml, 8);
                }
                if cand_match::<CMOV, true>(valid, ml, ip) {
                    let mut match_long = w.pos(match_long_index);
                    let match_end = w.seg_end(match_long_index);
                    let low_match_ptr = w.seg_start(match_long_index);
                    let mut ml = w.count2(ip.add(8), match_long.add(8), match_end) + 8;
                    let offset = c - match_long_index;
                    // catch up
                    while ((ip > anchor) & (match_long > low_match_ptr)) && {
                        w.chk(match_long.wrapping_sub(1), 1);
                        *ip.sub(1) == *match_long.sub(1)
                    } {
                        ip = ip.sub(1);
                        match_long = match_long.sub(1);
                        ml += 1;
                    }
                    m_length = ml;
                    offset_2 = offset_1;
                    offset_1 = offset;
                    ss.store_seq(
                        buf,
                        anchor as usize - bp as usize,
                        ip as usize - anchor as usize,
                        end,
                        offset_to_offbase(offset),
                        m_length,
                    );
                    curr = c;
                    break;
                }

                let (valid, m) = cand(match_index);
                #[cfg(debug_assertions)]
                if valid {
                    w.chk(m, 4);
                }
                if cand_match::<CMOV, false>(valid, m, ip) {
                    let mut mtch = w.pos(match_index);
                    let h3 = hash_l(ip.add(1));
                    let match_index3 = *hash_long.add(h3);
                    let mut match3 = w.pos(match_index3);
                    let offset;
                    let mut ml;
                    *hash_long.add(h3) = c + 1;
                    if match_index3 > dict_start_index && {
                        w.chk(match3, 8);
                        read_le64(match3) == read_le64(ip.add(1))
                    } {
                        let match_end = w.seg_end(match_index3);
                        let low_match_ptr = w.seg_start(match_index3);
                        ml = w.count2(ip.add(9), match3.add(8), match_end) + 8;
                        ip = ip.add(1);
                        offset = c + 1 - match_index3;
                        // catch up
                        while ((ip > anchor) & (match3 > low_match_ptr)) && {
                            w.chk(match3.wrapping_sub(1), 1);
                            *ip.sub(1) == *match3.sub(1)
                        } {
                            ip = ip.sub(1);
                            match3 = match3.sub(1);
                            ml += 1;
                        }
                    } else {
                        let match_end = w.seg_end(match_index);
                        let low_match_ptr = w.seg_start(match_index);
                        ml = w.count2(ip.add(4), mtch.add(4), match_end) + 4;
                        offset = c - match_index;
                        // catch up
                        while ((ip > anchor) & (mtch > low_match_ptr)) && {
                            w.chk(mtch.wrapping_sub(1), 1);
                            *ip.sub(1) == *mtch.sub(1)
                        } {
                            ip = ip.sub(1);
                            mtch = mtch.sub(1);
                            ml += 1;
                        }
                    }
                    m_length = ml;
                    offset_2 = offset_1;
                    offset_1 = offset;
                    ss.store_seq(
                        buf,
                        anchor as usize - bp as usize,
                        ip as usize - anchor as usize,
                        end,
                        offset_to_offbase(offset),
                        m_length,
                    );
                    curr = c;
                    break;
                }

                if next >= ilimit {
                    break 'search;
                }
                ip = next;
                (h_small, h_long, match_index, match_long_index) = load(ip);
            }

            // move to next sequence start
            ip = ip.add(m_length);
            anchor = ip;

            if ip <= ilimit {
                // Complementary insertion, done after the ilimit test as
                // candidates could be > iend - 8.
                let index_to_insert = curr + 2;
                let p = base.wrapping_add(index_to_insert as usize);
                *hash_long.add(hash_l(p)) = index_to_insert;
                *hash_long.add(hash_l(ip.sub(2))) = w.idx(ip.sub(2));
                *hash_small.add(hash_s(p)) = index_to_insert;
                *hash_small.add(hash_s(ip.sub(1))) = w.idx(ip.sub(1));

                // check immediate repcode
                while ip <= ilimit {
                    let current2 = w.idx(ip);
                    let rep_index2 = current2.wrapping_sub(offset_2);
                    let rep_match2 = w.pos(rep_index2);
                    if (index_overlap_check(prefix_start_index, rep_index2)
                        & (offset_2 <= current2.wrapping_sub(dict_start_index)))
                        && {
                            w.chk(rep_match2, 4);
                            read32(rep_match2) == read32(ip)
                        }
                    {
                        let rep_end2 = w.seg_end(rep_index2);
                        let rep_length2 = w.count2(ip.add(4), rep_match2.add(4), rep_end2) + 4;
                        core::mem::swap(&mut offset_1, &mut offset_2);
                        ss.store_seq(
                            buf,
                            anchor as usize - bp as usize,
                            0,
                            end,
                            REPCODE1_TO_OFFBASE,
                            rep_length2,
                        );
                        *hash_small.add(hash_s(ip)) = current2;
                        *hash_long.add(hash_l(ip)) = current2;
                        ip = ip.add(rep_length2);
                        anchor = ip;
                        continue;
                    }
                    break;
                }
            }
        }
    }

    // save reps for next block
    rep[0] = offset_1;
    rep[1] = offset_2;

    // Return the last literals size
    iend as usize - anchor as usize
}

/// `ZSTD_compressBlock_doubleFast_dictMatchState()`: as
/// [`compress_block_double_fast`], with an attached dictionary
/// ([`MatchState::dms`]) whose tagged tables are searched too. Picks the
/// `ZSTD_compressBlock_doubleFast_dictMatchState_<mls>` variant like
/// libzstd (3 and other values use 4). Out of line: inlined into the
/// block loop it was 0.5-1.5% slower on small inputs. (A BMI2 version, as for
/// the extDict loop, measured 1.5-3% slower here.)
#[inline(never)]
pub(crate) fn compress_block_double_fast_dict_match_state(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    debug_assert!(ms.cparams.strategy == Strategy::DFast);
    match ms.cparams.min_match {
        5 => dms_generic::<5>(ms, ss, rep, buf, start, end),
        6 => dms_generic::<6>(ms, ss, rep, buf, start, end),
        7 => dms_generic::<7>(ms, ss, rep, buf, start, end),
        _ => dms_generic::<4>(ms, ss, rep, buf, start, end),
    }
}

/// `ZSTD_compressBlock_doubleFast_dictMatchState_generic()`.
///
/// Memory safety: as `fast`'s dictMatchState variant (the window is one
/// segment in `buf`, the dictionary's content another, checked by
/// [`DmsView::new`]; window candidates are read at or above
/// `prefixLowestIndex`, dictionary candidates above its start; repcodes
/// are at most `dictAndPrefixLength` at entry and pass
/// `ZSTD_index_overlap_check`; matches are counted across with
/// `ZSTD_count_2segments`).
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
    let cparams = ms.cparams;
    let h_bits_l = cparams.hash_log;
    let h_bits_s = cparams.chain_log;
    // These checks make the raw-pointer accesses below sound.
    assert!(start <= end && end <= buf.len());
    assert!(ms.hash_table.len() == 1usize << h_bits_l && h_bits_l <= 32);
    assert!(ms.chain_table.len() == 1usize << h_bits_s && h_bits_s <= 32);
    assert!(!ms.window.has_ext_dict());
    let window_start = ms.window.pos(ms.window.dict_limit);
    assert!(window_start <= start, "window start is not in the buffer");
    assert!(end.wrapping_sub(ms.window.base) <= u32::MAX as usize);
    let cd = ms.dict.clone().expect("attached dictionary");
    let d = DmsView::new(&cd);
    assert!(d.chain_log + SHORT_CACHE_TAG_BITS <= 32);
    assert!(cd.ms.chain_table.len() == 1usize << d.chain_log);

    let bp = buf.as_ptr();
    let base = bp.wrapping_add(ms.window.base);
    let istart = bp.wrapping_add(start);
    let mut ip = istart;
    let mut anchor = istart;
    let end_index = ms.window.index(end);
    // presumes that, if there is a dictionary, it must be using Attach mode
    let prefix_lowest_index = ms.lowest_prefix_index(end_index, cparams.window_log);
    assert!(prefix_lowest_index == ms.window.dict_limit);
    let prefix_lowest = base.wrapping_add(prefix_lowest_index as usize);
    let iend = bp.wrapping_add(end);
    let ilimit = iend.wrapping_sub(HASH_READ_SIZE);
    let mut offset_1 = rep[0];
    let mut offset_2 = rep[1];

    let dict_hash_long = d.hash_table;
    let dict_hash_small = d.chain_table;
    let dict_start_index = d.dict_limit;
    let dict_base = d.base;
    let dict_start = dict_base.wrapping_add(dict_start_index as usize);
    let dict_end = d.end;
    assert!(prefix_lowest_index >= d.high_limit);
    let dict_index_delta = prefix_lowest_index - d.high_limit;
    let dict_h_bits_l = d.hash_log + SHORT_CACHE_TAG_BITS;
    let dict_h_bits_s = d.chain_log + SHORT_CACHE_TAG_BITS;
    let dict_and_prefix_length =
        (start - window_start) as u32 + (dict_end as usize - dict_start as usize) as u32;

    let hash_long = ms.hash_table.as_mut_ptr();
    let hash_small = ms.chain_table.as_mut_ptr();
    let idx = |p: *const u8| (p as usize).wrapping_sub(base as usize) as u32;
    // Debug check that `n` bytes at `p` are in the window or the dictionary.
    let chk = |p: *const u8, n: usize| {
        if (p as usize) >= (dict_start as usize) && (p as usize) < (dict_end as usize) {
            d.chk(p, n);
        } else {
            debug_assert!(
                (p as usize) >= (prefix_lowest as usize) && (p as usize) + n <= (iend as usize),
                "read out of the window"
            );
        }
    };

    // init
    ip = ip.wrapping_add((dict_and_prefix_length == 0) as usize);
    // dictMatchState repCode checks don't currently handle repCode == 0
    // disabling.
    assert!(offset_1 <= dict_and_prefix_length && offset_2 <= dict_and_prefix_length);

    // SAFETY (whole block): see the function docs. Table writes use hashes
    // of `h_bits_l`/`h_bits_s` bits into tables of that many entries,
    // dictionary reads tagged hashes into tables of `dict_h_bits - 8` bits
    // (asserted above and by `DmsView::new`).
    unsafe {
        let hash_l = |p: *const u8| {
            chk(p, 8);
            hash8_ptr(p, h_bits_l)
        };
        let hash_s = |p: *const u8| {
            chk(p, 8);
            hash_mls::<MLS>(p, h_bits_s)
        };
        let rep_pos = |rep_index: u32| {
            if rep_index < prefix_lowest_index {
                dict_base.wrapping_add(rep_index.wrapping_sub(dict_index_delta) as usize)
            } else {
                base.wrapping_add(rep_index as usize)
            }
        };

        // Main Search Loop
        'search: while ip < ilimit {
            // < instead of <=, because repcode check at (ip+1)
            let mut m_length: usize;
            let offset: u32;
            let h2 = hash_l(ip);
            let h = hash_s(ip);
            let dict_hash_and_tag_l = hash8_ptr(ip, dict_h_bits_l);
            let dict_hash_and_tag_s = hash_mls::<MLS>(ip, dict_h_bits_s);
            let dict_match_index_and_tag_l =
                *dict_hash_long.add(dict_hash_and_tag_l >> SHORT_CACHE_TAG_BITS);
            let dict_match_index_and_tag_s =
                *dict_hash_small.add(dict_hash_and_tag_s >> SHORT_CACHE_TAG_BITS);
            let dict_tags_match_l =
                compare_packed_tags(dict_match_index_and_tag_l as usize, dict_hash_and_tag_l);
            let dict_tags_match_s =
                compare_packed_tags(dict_match_index_and_tag_s as usize, dict_hash_and_tag_s);
            let curr = idx(ip);
            let match_index_l = *hash_long.add(h2);
            let mut match_index_s = *hash_small.add(h);
            let mut match_long = base.wrapping_add(match_index_l as usize);
            let mut mtch = base.wrapping_add(match_index_s as usize);
            let rep_index = curr.wrapping_add(1).wrapping_sub(offset_1);
            let rep_match = rep_pos(rep_index);
            // update hash tables
            *hash_long.add(h2) = curr;
            *hash_small.add(h) = curr;

            'found: {
                // check repcode
                if index_overlap_check(prefix_lowest_index, rep_index) && {
                    chk(rep_match, 4);
                    read32(rep_match) == read32(ip.add(1))
                } {
                    let rep_match_end = if rep_index < prefix_lowest_index {
                        dict_end
                    } else {
                        iend
                    };
                    m_length = count_2segments(
                        ip.add(1 + 4),
                        rep_match.add(4),
                        iend,
                        rep_match_end,
                        prefix_lowest,
                    ) + 4;
                    ip = ip.add(1);
                    ss.store_seq(
                        buf,
                        anchor as usize - bp as usize,
                        ip as usize - anchor as usize,
                        end,
                        REPCODE1_TO_OFFBASE,
                        m_length,
                    );
                    break 'found; // _match_stored
                }

                // The prefix candidates are read through `select_addr` and
                // their index tested once the bytes match (meta/zstd#4866):
                // with a short prefix (small inputs), many candidates are
                // below it, an unpredictable branch.
                //
                // Each candidate is debug-checked before the read and again
                // once it matched. The second check is redundant, but keep
                // it: its call (empty in release builds) shapes this loop's
                // register allocation. Without it (#85) the loop spilled
                // differently and L3/L4 dictionary compression ran 2-4%
                // slower on the same instruction count. After changes here,
                // compare the function's size (nm) and speed with main.
                #[cfg(debug_assertions)]
                if match_index_l >= prefix_lowest_index {
                    chk(match_long, 8); // read in place (debug builds only)
                }
                if cand_match::<true, true>(match_index_l >= prefix_lowest_index, match_long, ip) {
                    chk(match_long, 8); // redundant, kept for codegen
                    // check prefix long match
                    m_length = count(ip.add(8), match_long.add(8), iend) + 8;
                    offset = (ip as usize - match_long as usize) as u32;
                    while ((ip > anchor) & (match_long > prefix_lowest))
                        && *ip.sub(1) == *match_long.sub(1)
                    {
                        ip = ip.sub(1);
                        match_long = match_long.sub(1);
                        m_length += 1;
                    } // catch up
                } else if dict_tags_match_l && {
                    // check dictMatchState long match
                    let dict_match_index_l = dict_match_index_and_tag_l >> SHORT_CACHE_TAG_BITS;
                    let dict_match_l = dict_base.wrapping_add(dict_match_index_l as usize);
                    dict_match_l > dict_start && {
                        chk(dict_match_l, 8);
                        read_le64(dict_match_l) == read_le64(ip)
                    }
                } {
                    let dict_match_index_l = dict_match_index_and_tag_l >> SHORT_CACHE_TAG_BITS;
                    let mut dict_match_l = dict_base.wrapping_add(dict_match_index_l as usize);
                    m_length = count_2segments(
                        ip.add(8),
                        dict_match_l.add(8),
                        iend,
                        dict_end,
                        prefix_lowest,
                    ) + 8;
                    offset = curr - dict_match_index_l - dict_index_delta;
                    while ((ip > anchor) & (dict_match_l > dict_start))
                        && *ip.sub(1) == *dict_match_l.sub(1)
                    {
                        ip = ip.sub(1);
                        dict_match_l = dict_match_l.sub(1);
                        m_length += 1;
                    } // catch up
                } else {
                    #[cfg(debug_assertions)]
                    if match_index_s > prefix_lowest_index {
                        chk(mtch, 4); // read in place (debug builds only)
                    }
                    let short_found =
                        if cand_match::<true, false>(match_index_s > prefix_lowest_index, mtch, ip)
                        {
                            // short match candidate
                            chk(mtch, 4); // redundant, kept for codegen (see above)
                            true
                        } else if dict_tags_match_s && match_index_s <= prefix_lowest_index {
                            // check dictMatchState short match
                            let dict_match_index_s =
                                dict_match_index_and_tag_s >> SHORT_CACHE_TAG_BITS;
                            mtch = dict_base.wrapping_add(dict_match_index_s as usize);
                            match_index_s = dict_match_index_s.wrapping_add(dict_index_delta);
                            mtch > dict_start && {
                                chk(mtch, 4);
                                read32(mtch) == read32(ip)
                            }
                        } else {
                            false
                        };

                    if !short_found {
                        ip = ip.add(((ip as usize - anchor as usize) >> K_SEARCH_STRENGTH) + 1);
                        continue 'search;
                    }

                    // _search_next_long:
                    let hl3 = hash_l(ip.add(1));
                    let dict_hash_and_tag_l3 = hash8_ptr(ip.add(1), dict_h_bits_l);
                    let match_index_l3 = *hash_long.add(hl3);
                    let dict_match_index_and_tag_l3 =
                        *dict_hash_long.add(dict_hash_and_tag_l3 >> SHORT_CACHE_TAG_BITS);
                    let dict_tags_match_l3 = compare_packed_tags(
                        dict_match_index_and_tag_l3 as usize,
                        dict_hash_and_tag_l3,
                    );
                    let mut match_l3 = base.wrapping_add(match_index_l3 as usize);
                    *hash_long.add(hl3) = curr + 1;

                    #[cfg(debug_assertions)]
                    if match_index_l3 >= prefix_lowest_index {
                        chk(match_l3, 8); // read in place (debug builds only)
                    }
                    if cand_match::<true, true>(
                        match_index_l3 >= prefix_lowest_index,
                        match_l3,
                        ip.add(1),
                    ) {
                        chk(match_l3, 8); // redundant, kept for codegen (see above)
                        // check prefix long +1 match
                        m_length = count(ip.add(9), match_l3.add(8), iend) + 8;
                        ip = ip.add(1);
                        offset = (ip as usize - match_l3 as usize) as u32;
                        while ((ip > anchor) & (match_l3 > prefix_lowest))
                            && *ip.sub(1) == *match_l3.sub(1)
                        {
                            ip = ip.sub(1);
                            match_l3 = match_l3.sub(1);
                            m_length += 1;
                        } // catch up
                    } else if dict_tags_match_l3 && {
                        // check dict long +1 match
                        let dict_match_index_l3 =
                            dict_match_index_and_tag_l3 >> SHORT_CACHE_TAG_BITS;
                        let dict_match_l3 = dict_base.wrapping_add(dict_match_index_l3 as usize);
                        dict_match_l3 > dict_start && {
                            chk(dict_match_l3, 8);
                            read_le64(dict_match_l3) == read_le64(ip.add(1))
                        }
                    } {
                        let dict_match_index_l3 =
                            dict_match_index_and_tag_l3 >> SHORT_CACHE_TAG_BITS;
                        let mut dict_match_l3 =
                            dict_base.wrapping_add(dict_match_index_l3 as usize);
                        m_length = count_2segments(
                            ip.add(1 + 8),
                            dict_match_l3.add(8),
                            iend,
                            dict_end,
                            prefix_lowest,
                        ) + 8;
                        ip = ip.add(1);
                        offset = curr + 1 - dict_match_index_l3 - dict_index_delta;
                        while ((ip > anchor) & (dict_match_l3 > dict_start))
                            && *ip.sub(1) == *dict_match_l3.sub(1)
                        {
                            ip = ip.sub(1);
                            dict_match_l3 = dict_match_l3.sub(1);
                            m_length += 1;
                        } // catch up
                    } else if match_index_s < prefix_lowest_index {
                        // if no long +1 match, explore the short match we found
                        m_length =
                            count_2segments(ip.add(4), mtch.add(4), iend, dict_end, prefix_lowest)
                                + 4;
                        offset = curr - match_index_s;
                        while ((ip > anchor) & (mtch > dict_start)) && *ip.sub(1) == *mtch.sub(1) {
                            ip = ip.sub(1);
                            mtch = mtch.sub(1);
                            m_length += 1;
                        } // catch up
                    } else {
                        m_length = count(ip.add(4), mtch.add(4), iend) + 4;
                        offset = (ip as usize - mtch as usize) as u32;
                        while ((ip > anchor) & (mtch > prefix_lowest)) && *ip.sub(1) == *mtch.sub(1)
                        {
                            ip = ip.sub(1);
                            mtch = mtch.sub(1);
                            m_length += 1;
                        } // catch up
                    }
                }

                // _match_found:
                offset_2 = offset_1;
                offset_1 = offset;
                ss.store_seq(
                    buf,
                    anchor as usize - bp as usize,
                    ip as usize - anchor as usize,
                    end,
                    offset_to_offbase(offset),
                    m_length,
                );
            }

            // _match_stored: match found
            ip = ip.add(m_length);
            anchor = ip;

            if ip <= ilimit {
                // Complementary insertion: done after iLimit test, as
                // candidates could be > iend-8
                {
                    let index_to_insert = curr + 2;
                    let p = base.wrapping_add(index_to_insert as usize);
                    *hash_long.add(hash_l(p)) = index_to_insert;
                    *hash_long.add(hash_l(ip.sub(2))) = idx(ip.sub(2));
                    *hash_small.add(hash_s(p)) = index_to_insert;
                    *hash_small.add(hash_s(ip.sub(1))) = idx(ip.sub(1));
                }

                // check immediate repcode
                while ip <= ilimit {
                    let current2 = idx(ip);
                    let rep_index2 = current2.wrapping_sub(offset_2);
                    let rep_match2 = rep_pos(rep_index2);
                    if index_overlap_check(prefix_lowest_index, rep_index2) && {
                        chk(rep_match2, 4);
                        read32(rep_match2) == read32(ip)
                    } {
                        let rep_end2 = if rep_index2 < prefix_lowest_index {
                            dict_end
                        } else {
                            iend
                        };
                        let rep_length2 = count_2segments(
                            ip.add(4),
                            rep_match2.add(4),
                            iend,
                            rep_end2,
                            prefix_lowest,
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
                        *hash_small.add(hash_s(ip)) = current2;
                        *hash_long.add(hash_l(ip)) = current2;
                        ip = ip.add(rep_length2);
                        anchor = ip;
                        continue;
                    }
                    break;
                }
            }
        }
    }

    // save reps for next block
    rep[0] = offset_1;
    rep[1] = offset_2;

    // Return the last literals size
    iend as usize - anchor as usize
}

#[cfg(test)]
mod tests {
    use super::super::fast::tests::{
        MIN_SEQUENCES, MIRI, random, ring_input, roundtrip, roundtrip_ring, texty, variants_input,
    };
    use super::super::params::{CParams, Strategy, block_size_max, one_shot_cparams};
    use alloc::vec::Vec;

    #[test]
    fn roundtrip_levels() {
        let k = if MIRI { 50 } else { 1 }; // sizes divided by k
        let inputs: [Vec<u8>; 5] = [
            texty(1000, 1),
            texty(300_000 / k, 3),
            random(50_000 / k, 4),
            alloc::vec![0u8; 200_000 / k],
            (0..(70_000 / k) as u32).map(|i| (i % 251) as u8).collect(),
        ];
        for src in &inputs {
            for level in [2, 3, 4] {
                let cp = one_shot_cparams(level, src.len() as u64);
                if cp.strategy != Strategy::DFast {
                    continue;
                }
                roundtrip(src, cp, block_size_max(&cp, src.len() as u64));
            }
        }
    }

    #[test]
    fn roundtrip_all_variants() {
        let src = variants_input();
        let window_logs: &[u32] = if MIRI { &[10, 22] } else { &[10, 14, 18, 22] };
        for mls in 3..=8 {
            for &window_log in window_logs {
                let cp = CParams {
                    window_log,
                    chain_log: 13,
                    hash_log: 15,
                    search_log: 1,
                    min_match: mls,
                    target_length: 0,
                    strategy: Strategy::DFast,
                };
                let n = roundtrip(&src, cp, (1 << 17).min(1 << window_log));
                assert!(
                    n > MIN_SEQUENCES,
                    "mls {mls} wlog {window_log}: {n} sequences"
                );
            }
        }
    }

    /// The extDict variant, through a wrapping ring buffer like the
    /// streaming compressor's.
    #[test]
    fn roundtrip_ring_variants() {
        let src = if MIRI {
            ring_input(6000, 3000, 3000)
        } else {
            ring_input(150_000, 60_000, 100_000)
        };
        // The ring (window + block size) wraps for every window.
        let windows: &[(u32, usize)] = if MIRI {
            &[(10, 1 << 10), (12, 3000)]
        } else {
            &[(10, 1 << 10), (14, 3000), (16, 1 << 15), (17, 1 << 17)]
        };
        for mls in 3..=8 {
            for &(window_log, bs) in windows {
                let cp = CParams {
                    window_log,
                    chain_log: 13,
                    hash_log: 15,
                    search_log: 1,
                    min_match: mls,
                    target_length: 0,
                    strategy: Strategy::DFast,
                };
                let (n, ext) = roundtrip_ring(&src, cp, bs);
                assert!(
                    n > MIN_SEQUENCES && ext > 0,
                    "mls {mls} wlog {window_log}: {n} {ext}"
                );
            }
        }
    }
}
