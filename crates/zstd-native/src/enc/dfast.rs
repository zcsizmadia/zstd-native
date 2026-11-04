//! The `dfast` strategy (levels 3-4, and level 2 for some input sizes): a
//! port of libzstd 1.5.7's `compress/zstd_double_fast.c` (no-dictionary
//! path). It shares the match state, window and block loop with `fast`
//! (see that module's docs); `hashTable` holds 8-byte hashes ("long") and
//! `chainTable` `minMatch`-byte hashes ("short").

use super::fast::{
    HASH_READ_SIZE, K_SEARCH_STRENGTH, MatchState, TableLoadMethod, hash_mls, prefetch_l1,
};
use super::params::Strategy;
use super::seq_store::{
    REPCODE1_TO_OFFBASE, SeqStore, count, hash_at, hash8_ptr, offset_to_offbase, read_le64, read32,
};

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
#[inline(never)]
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
    // ~random data, unlikely to match: read instead of the table candidate
    // when that is invalid, to avoid an unpredictable branch.
    static DUMMY: [u8; 10] = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0xe2, 0xb4];
    // how many positions to search before increasing step size
    const K_STEP_INCR: usize = 1 << K_SEARCH_STRENGTH;

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

        // Outer loop: one iteration per match found and stored.
        'outer: loop {
            let mut step = 1usize;
            let mut next_step = ip.wrapping_add(K_STEP_INCR);
            let mut ip1 = ip.wrapping_add(step);

            if ip1 > ilimit {
                break 'outer;
            }

            let mut hl0 = hash_l(ip);
            let mut idxl0 = *hash_long.add(hl0);
            let mut matchl0 = base.wrapping_add(idxl0 as usize);
            let mut hl1 = 0usize;
            let mut curr: u32;

            // Inner loop: one iteration per search / position. Yields
            // `Some((offset, mLength))` for a match to store (with `ip`
            // moved to its start), `None` for a stored repcode match.
            let found: Option<(u32, usize)>;
            let m_length: usize;
            loop {
                let hs0 = hash_s(ip);
                let idxs0 = *hash_small.add(hs0);
                curr = idx(ip);
                let mut matchs0 = base.wrapping_add(idxs0 as usize);

                // update hash tables
                *hash_long.add(hl0) = curr;
                *hash_small.add(hs0) = curr;

                // check noDict repcode
                if offset_1 > 0 && {
                    let r = ip.wrapping_add(1).wrapping_sub(offset_1 as usize);
                    chk(r, 4);
                    read32(r) == read32(ip.add(1))
                } {
                    let r = ip.add(1 + 4).sub(offset_1 as usize);
                    let m = count(ip.add(1 + 4), r, iend) + 4;
                    ip = ip.add(1);
                    ss.store_seq(
                        buf,
                        anchor as usize - bp as usize,
                        ip as usize - anchor as usize,
                        end,
                        REPCODE1_TO_OFFBASE,
                        m,
                    );
                    found = None;
                    m_length = m;
                    break;
                }

                hl1 = hash_l(ip1);

                // check prefix long match (the branchless select is
                // libzstd's ZSTD_selectAddr)
                let l0_valid = idxl0 >= prefix_lowest_index;
                let matchl0_safe =
                    core::hint::select_unpredictable(l0_valid, matchl0, DUMMY.as_ptr());
                if l0_valid {
                    chk(matchl0_safe, 8);
                }
                if read_le64(matchl0_safe) == read_le64(ip) && l0_valid {
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
                    break;
                }

                let idxl1 = *hash_long.add(hl1);
                let matchl1 = base.wrapping_add(idxl1 as usize);

                // check prefix short match
                let s0_valid = idxs0 >= prefix_lowest_index;
                let matchs0_safe =
                    core::hint::select_unpredictable(s0_valid, matchs0, DUMMY.as_ptr());
                if s0_valid {
                    chk(matchs0_safe, 4);
                }
                if read32(matchs0_safe) == read32(ip) && s0_valid {
                    // _search_next_long: short match found, check for a
                    // longer one
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
                    break;
                }

                if ip1 >= next_step {
                    prefetch_l1(ip1.wrapping_add(64));
                    prefetch_l1(ip1.wrapping_add(128));
                    step += 1;
                    next_step = next_step.wrapping_add(K_STEP_INCR);
                }
                ip = ip1;
                ip1 = ip1.wrapping_add(step);

                hl0 = hl1;
                idxl0 = idxl1;
                matchl0 = matchl1;

                if ip1 > ilimit {
                    break 'outer;
                }
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

                ss.store_seq(
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
                    ss.store_seq(
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

#[cfg(test)]
mod tests {
    use super::super::fast::tests::{random, roundtrip, texty};
    use super::super::params::{CParams, Strategy, block_size_max, one_shot_cparams};
    use alloc::vec::Vec;

    #[test]
    fn roundtrip_levels() {
        let inputs: [Vec<u8>; 5] = [
            texty(1000, 1),
            texty(300_000, 3),
            random(50_000, 4),
            alloc::vec![0u8; 200_000],
            (0..70_000u32).map(|i| (i % 251) as u8).collect(),
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
        let mut src = texty(150_000, 7);
        src.extend_from_slice(&random(5000, 8));
        src.extend_from_slice(&src.clone()[..40_000]);
        for mls in 3..=8 {
            for window_log in [10, 14, 18, 22] {
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
                assert!(n > 1000, "mls {mls} wlog {window_log}: {n} sequences");
            }
        }
    }
}
