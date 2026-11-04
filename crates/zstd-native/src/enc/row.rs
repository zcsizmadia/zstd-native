//! The row-based match finder of the greedy, lazy and lazy2 strategies
//! (`windowLog > 14`): a port of libzstd 1.5.7's "(SIMD) Row-based
//! matchfinder" (`compress/zstd_lazy.c`, `ZSTD_RowFindBestMatch` and
//! helpers), noDict, extDict and dictMatchState paths (the attached
//! dictionary's rows are hashed unsalted: a CDict's salt is 0). The parser
//! is in [`lazy`](super::lazy).
//!
//! The hash table is split into rows of `1 << rowLog` indices (`rowLog =
//! clamp(searchLog, 4, 6)`); a position's salted hash
//! (`ZSTD_hashPtrSalted`, `rowHashLog + 8` bits) selects a row and an
//! 8-bit tag. The tag table mirrors the hash table byte for byte: entry 0
//! of each tag row holds the row's head (the most recently written slot),
//! the others the tags of the stored indices. A row is a circular buffer
//! written backwards from the head, skipping slot 0. Matching tags are found
//! with SSE2 on x86_64 (16 bytes at a time) or SWAR elsewhere (or with the
//! `portable` feature); both give the same bit mask.
//!
//! # Stale tags
//!
//! libzstd keeps the tag table between frames of a reused context (only
//! the hash table is cleared, and the salt changes), so a row's head and
//! its unwritten slots may hold tags from an earlier frame. That never
//! changes the result: the match scan visits a row's slots from the head
//! up, i.e. the written slots newest first, then the slots not written in
//! this frame; those hold index 0, below any valid index, which ends the
//! scan whether or not their tag matches. This port therefore never clears
//! the tag table either (the `stale_tags` test checks the claim).

use super::lazy::Searcher;
use super::seq_store::{
    PRIME4BYTES, PRIME5BYTES, PRIME6BYTES, REP_NUM, count, read_le32, read_le64, read32,
};
use crate::enc::fast::prefetch_l1;

/// `ZSTD_ROW_HASH_CACHE_SIZE`: hashes of the next positions kept ahead.
pub(crate) const ROW_HASH_CACHE_SIZE: usize = 8;
const ROW_HASH_CACHE_MASK: u32 = ROW_HASH_CACHE_SIZE as u32 - 1;
/// `ZSTD_ROW_HASH_TAG_BITS`.
pub(crate) const ROW_HASH_TAG_BITS: u32 = 8;
/// `ZSTD_ROW_HASH_MAX_ENTRIES`.
const ROW_HASH_MAX_ENTRIES: usize = 64;

/// `ZSTD_hashPtrSalted()` for `mls` 4 to 6 (5 and 6 read 8 bytes).
///
/// # Safety
/// `p..p + 8` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(crate) unsafe fn hash_salted<const MLS: u32>(p: *const u8, h_bits: u32, salt: u64) -> u32 {
    debug_assert!(h_bits <= 32 && h_bits > 0);
    // SAFETY: per the caller.
    unsafe {
        match MLS {
            5 => {
                ((((read_le64(p) << (64 - 40)).wrapping_mul(PRIME5BYTES)) ^ salt) >> (64 - h_bits))
                    as u32
            }
            6 => {
                ((((read_le64(p) << (64 - 48)).wrapping_mul(PRIME6BYTES)) ^ salt) >> (64 - h_bits))
                    as u32
            }
            _ => ((read_le32(p).wrapping_mul(PRIME4BYTES)) ^ salt as u32) >> (32 - h_bits),
        }
    }
}

/// `ZSTD_row_nextIndex()`: the next slot to write in a row (cycling
/// backwards through `1..rowEntries`), stored as the row's new head.
///
/// # Safety
/// `tag_row` must be writable.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn row_next_index(tag_row: *mut u8, row_mask: u32) -> u32 {
    // SAFETY: per the caller.
    unsafe {
        let mut next = u32::from((*tag_row).wrapping_sub(1)) & row_mask;
        next += if next == 0 { row_mask } else { 0 }; // skip first position
        *tag_row = next as u8;
        next
    }
}

/// `ZSTD_row_prefetch()`.
#[inline(always)]
fn row_prefetch<const ROW_LOG: u32>(hash_table: *const u32, tag_table: *const u8, rel_row: u32) {
    let rel_row = rel_row as usize;
    prefetch_l1(hash_table.wrapping_add(rel_row).cast());
    if ROW_LOG >= 5 {
        prefetch_l1(hash_table.wrapping_add(rel_row + 16).cast());
    }
    prefetch_l1(tag_table.wrapping_add(rel_row));
    if ROW_LOG == 6 {
        prefetch_l1(tag_table.wrapping_add(rel_row + 32));
    }
}

/// `ZSTD_row_fillHashCache()`: hashes the positions from `idx` (up to
/// [`ROW_HASH_CACHE_SIZE`], not beyond `ilimit`) into the hash cache.
///
/// # Safety
/// `idx` is in the window and `ilimit + 8` is readable.
#[allow(unsafe_code)]
#[inline(always)]
pub(super) unsafe fn fill_hash_cache<const MLS: u32, const ROW_LOG: u32>(
    s: &mut Searcher,
    idx: u32,
    ilimit: *const u8,
) {
    let p = s.pos(idx) as usize;
    let max_elems_to_prefetch = if p > ilimit as usize {
        0
    } else {
        ilimit as usize - p + 1
    };
    let lim = idx + ROW_HASH_CACHE_SIZE.min(max_elems_to_prefetch) as u32;
    for i in idx..lim {
        let p = s.pos(i);
        s.chk(p, 8);
        // SAFETY: `p <= ilimit`, readable for 8 bytes per the caller.
        let hash =
            unsafe { hash_salted::<MLS>(p, s.row_hash_log + ROW_HASH_TAG_BITS, s.hash_salt) };
        let row = (hash >> ROW_HASH_TAG_BITS) << ROW_LOG;
        row_prefetch::<ROW_LOG>(s.hash_table, s.tag_table, row);
        s.hash_cache[(i & ROW_HASH_CACHE_MASK) as usize] = hash;
    }
}

/// `ZSTD_row_nextCachedHash()`: returns the cached hash of `idx` and
/// replaces it with the hash of `idx + ROW_HASH_CACHE_SIZE`.
///
/// # Safety
/// `idx + ROW_HASH_CACHE_SIZE` is in the window, readable for 8 bytes.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn next_cached_hash<const MLS: u32, const ROW_LOG: u32>(s: &mut Searcher, idx: u32) -> u32 {
    let p = s.pos(idx).wrapping_add(ROW_HASH_CACHE_SIZE);
    s.chk(p, 8);
    // SAFETY: per the caller.
    let new_hash =
        unsafe { hash_salted::<MLS>(p, s.row_hash_log + ROW_HASH_TAG_BITS, s.hash_salt) };
    let row = (new_hash >> ROW_HASH_TAG_BITS) << ROW_LOG;
    row_prefetch::<ROW_LOG>(s.hash_table, s.tag_table, row);
    let slot = &mut s.hash_cache[(idx & ROW_HASH_CACHE_MASK) as usize];
    let hash = *slot;
    *slot = new_hash;
    hash
}

/// `ZSTD_row_update_internalImpl()` with the hash cache: inserts the
/// positions `[start, end)`.
///
/// # Safety
/// The positions up to `end - 1 + ROW_HASH_CACHE_SIZE` are in the window,
/// readable for 8 bytes; the hash cache holds the hashes from `start`.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn update_internal_impl<const MLS: u32, const ROW_LOG: u32>(
    s: &mut Searcher,
    start: u32,
    end: u32,
) {
    let row_mask = (1u32 << ROW_LOG) - 1;
    for idx in start..end {
        // SAFETY: per the caller; the row (from a hash of `rowHashLog + 8`
        // bits) is within the tables of `1 << (rowHashLog + rowLog)`
        // entries (asserted by the parser).
        unsafe {
            let hash = next_cached_hash::<MLS, ROW_LOG>(s, idx);
            debug_assert_eq!(
                hash,
                hash_salted::<MLS>(s.pos(idx), s.row_hash_log + ROW_HASH_TAG_BITS, s.hash_salt)
            );
            let rel_row = ((hash >> ROW_HASH_TAG_BITS) << ROW_LOG) as usize;
            let tag_row = s.tag_table.add(rel_row);
            let pos = row_next_index(tag_row, row_mask) as usize;
            *tag_row.add(pos) = hash as u8;
            *s.hash_table.add(rel_row + pos) = idx;
        }
    }
}

/// `ZSTD_row_update_internal()` with the hash cache: inserts the positions
/// from `nextToUpdate` up to `ip` (excluded), only the first 96 and the
/// last 32 of them after a long match.
///
/// # Safety
/// `ip + ROW_HASH_CACHE_SIZE + 8` is readable; `nextToUpdate <= ip`.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn update_internal<const MLS: u32, const ROW_LOG: u32>(s: &mut Searcher, ip: *const u8) {
    const SKIP_THRESHOLD: u32 = 384;
    const MAX_MATCH_START_POSITIONS_TO_UPDATE: u32 = 96;
    const MAX_MATCH_END_POSITIONS_TO_UPDATE: u32 = 32;
    let mut idx = s.next_to_update;
    let target = s.idx(ip);
    debug_assert!(target >= idx);
    // SAFETY: all inserted positions are before `ip`; see the caller.
    unsafe {
        if target - idx > SKIP_THRESHOLD {
            let bound = idx + MAX_MATCH_START_POSITIONS_TO_UPDATE;
            update_internal_impl::<MLS, ROW_LOG>(s, idx, bound);
            idx = target - MAX_MATCH_END_POSITIONS_TO_UPDATE;
            fill_hash_cache::<MLS, ROW_LOG>(s, idx, ip.wrapping_add(1));
        }
        update_internal_impl::<MLS, ROW_LOG>(s, idx, target);
    }
    s.next_to_update = target;
}

/// `ZSTD_row_getMatchMask()`: bit `i` is set when tag slot
/// `(head + i) % rowEntries` holds `tag`.
///
/// # Safety
/// `tag_row..tag_row + (1 << ROW_LOG)` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn match_mask<const ROW_LOG: u32>(tag_row: *const u8, tag: u8, head: u32) -> u64 {
    // SAFETY: per the caller.
    let matches = unsafe {
        #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
        {
            match_bits_sse2::<ROW_LOG>(tag_row, tag)
        }
        #[cfg(not(all(target_arch = "x86_64", not(feature = "portable"))))]
        {
            match_bits_swar::<ROW_LOG>(tag_row, tag)
        }
    };
    match ROW_LOG {
        4 => u64::from((matches as u16).rotate_right(head)),
        5 => u64::from((matches as u32).rotate_right(head)),
        _ => matches.rotate_right(head),
    }
}

/// `ZSTD_row_getSSEMask()` before the rotation: bit `i` is set when tag
/// slot `i` holds `tag`.
///
/// # Safety
/// `tag_row..tag_row + (1 << ROW_LOG)` must be readable.
#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code, unused_unsafe)]
#[inline(always)]
unsafe fn match_bits_sse2<const ROW_LOG: u32>(tag_row: *const u8, tag: u8) -> u64 {
    use core::arch::x86_64::{
        __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
    };
    let nb_chunks = 1usize << (ROW_LOG - 4);
    let mut matches = 0u64;
    // SAFETY: SSE2 is part of the x86_64 baseline; the loads are of
    // `16 * nb_chunks = 1 << ROW_LOG` bytes, readable per the caller.
    unsafe {
        let comparison_mask = _mm_set1_epi8(tag as i8);
        for i in 0..nb_chunks {
            let chunk = _mm_loadu_si128(tag_row.add(16 * i).cast::<__m128i>());
            let equal_mask = _mm_cmpeq_epi8(chunk, comparison_mask);
            matches |= u64::from(_mm_movemask_epi8(equal_mask) as u16) << (16 * i);
        }
    }
    matches
}

/// The SWAR branch of `ZSTD_row_getMatchMask()` before the rotation (the
/// generic code, also used with the `portable` feature): as
/// [`match_bits_sse2`].
///
/// # Safety
/// `tag_row..tag_row + (1 << ROW_LOG)` must be readable.
#[allow(unsafe_code)]
#[inline(always)]
unsafe fn match_bits_swar<const ROW_LOG: u32>(tag_row: *const u8, tag: u8) -> u64 {
    const X01: u64 = u64::MAX / 0xFF;
    const X80: u64 = X01 << 7;
    // Moves the high bit of each byte to the top 8 bits, in byte order.
    const EXTRACT_MAGIC: u64 = (u64::MAX / 0x7F) >> 8;
    let splat_char = u64::from(tag).wrapping_mul(X01);
    let mut matches = 0u64;
    let mut i = (1usize << ROW_LOG) - 8;
    loop {
        // SAFETY: `i + 8 <= 1 << ROW_LOG`, readable per the caller.
        let mut chunk = unsafe { read_le64(tag_row.add(i)) } ^ splat_char;
        // high bit set in each byte that differs from the tag
        chunk = (((chunk | X80).wrapping_sub(X01)) | chunk) & X80;
        matches <<= 8;
        matches |= chunk.wrapping_mul(EXTRACT_MAGIC) >> 56;
        if i == 0 {
            break;
        }
        i -= 8;
    }
    !matches
}

/// `ZSTD_RowFindBestMatch()` (noDict, extDict with `EXT`, dictMatchState
/// with `DMS`: then the attached dictionary's row too, with the attempts
/// left): the longest match at `ip` among
/// the row candidates whose tag matches (at most `1 << min(searchLog,
/// rowLog)`). Inserts the positions up to `ip` included (only `ip` in lazy
/// skipping mode) and collects the hash in `hashSaltEntropy`. Returns the
/// length (`< 4` if none) and stores the offBase in `offset_ptr`.
///
/// # Safety
/// `ip + ROW_HASH_CACHE_SIZE + 8 <= ilimit`, `ilimit` the block end (see
/// the `lazy` module docs); the hash cache holds the hashes from
/// `nextToUpdate`.
#[allow(unsafe_code)]
#[inline(never)]
pub(super) unsafe fn row_find_best_match<
    const MLS: u32,
    const ROW_LOG: u32,
    const EXT: bool,
    const DMS: bool,
>(
    s: &mut Searcher,
    ip: *const u8,
    ilimit: *const u8,
    offset_ptr: &mut u32,
) -> usize {
    let row_entries = 1u32 << ROW_LOG;
    let row_mask = row_entries - 1;
    let curr = s.idx(ip);
    let low_limit = s.lowest_match_index(curr);
    // nb of searches is capped at nb entries per row
    let capped_search_log = s.search_log.min(ROW_LOG);
    let mut nb_attempts = 1u32 << capped_search_log;
    let mut ml = 4 - 1;

    // SAFETY: see the module docs of `lazy`. The row (from a hash of
    // `rowHashLog + 8` bits) is within the tables of
    // `1 << (rowHashLog + rowLog)` entries; candidates are at or above
    // `low_limit` and before `ip`, and `ip + ml < ilimit` holds before each
    // 4-byte read at `ml - 3`.
    unsafe {
        // the attached dictionary's row (unsalted hash; its rows are
        // within its tables of `1 << (rowHashLog + rowLog)` entries)
        let (dms_tag, dms_rel_row) = if DMS {
            s.chk(ip, 8);
            let d = s.dms;
            let dms_hash = hash_salted::<MLS>(ip, d.row_hash_log + ROW_HASH_TAG_BITS, 0);
            let dms_rel_row = (dms_hash >> ROW_HASH_TAG_BITS) << ROW_LOG;
            row_prefetch::<ROW_LOG>(d.hash_table, d.tag_table, dms_rel_row);
            (dms_hash as u8, dms_rel_row as usize)
        } else {
            (0, 0)
        };

        // Update the hashTable and tagTable up to (but not including) ip
        let hash = if !s.lazy_skipping {
            update_internal::<MLS, ROW_LOG>(s, ip);
            next_cached_hash::<MLS, ROW_LOG>(s, curr)
        } else {
            // Stop inserting every position when in the lazy skipping
            // mode. The hash cache is also not kept up to date then.
            s.chk(ip, 8);
            let h = hash_salted::<MLS>(ip, s.row_hash_log + ROW_HASH_TAG_BITS, s.hash_salt);
            s.next_to_update = curr;
            h
        };
        s.hash_salt_entropy = s.hash_salt_entropy.wrapping_add(hash); // collect salt entropy

        // Get the hash for ip, compute the appropriate row
        let rel_row = ((hash >> ROW_HASH_TAG_BITS) << ROW_LOG) as usize;
        let tag = hash as u8;
        let row = s.hash_table.add(rel_row);
        let tag_row = s.tag_table.add(rel_row);
        let head = u32::from(*tag_row) & row_mask;
        let mut match_buffer = [0u32; ROW_HASH_MAX_ENTRIES];
        let mut num_matches = 0usize;
        let mut matches = match_mask::<ROW_LOG>(tag_row, tag, head);

        // Cycle through the matches and prefetch
        while matches > 0 && nb_attempts > 0 {
            let match_pos = (head + matches.trailing_zeros()) & row_mask;
            let match_index = *row.add(match_pos as usize);
            matches &= matches - 1;
            if match_pos == 0 {
                continue;
            }
            if match_index < low_limit {
                break;
            }
            if EXT {
                prefetch_l1(s.ext_pos(match_index));
            } else {
                prefetch_l1(s.pos(match_index));
            }
            match_buffer[num_matches] = match_index;
            num_matches += 1;
            nb_attempts -= 1;
        }

        // Speed opt: insert current byte into hashtable too. This allows us
        // to avoid one iteration of the loop in update_internal() at the
        // next search.
        {
            let pos = row_next_index(tag_row, row_mask) as usize;
            *tag_row.add(pos) = tag;
            *row.add(pos) = s.next_to_update;
            s.next_to_update += 1;
        }

        // Return the longest match
        for &match_index in &match_buffer[..num_matches] {
            let mut current_ml = 0;
            debug_assert!(match_index < curr && match_index >= low_limit);
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
            // Save best solution
            if current_ml > ml {
                ml = current_ml;
                *offset_ptr = curr - match_index + REP_NUM;
                if ip.add(current_ml) == ilimit {
                    break; // best possible, avoids read overflow on next attempt
                }
            }
        }

        if DMS {
            let d = s.dms;
            let dms_lowest_index = d.dict_limit;
            let dms_base = s.dict_base;
            let dms_index_delta = s.dict_limit - d.high_limit;
            let dms_row = d.hash_table.add(dms_rel_row);
            let dms_tag_row = d.tag_table.add(dms_rel_row);
            let head = u32::from(*dms_tag_row) & row_mask;
            let mut match_buffer = [0u32; ROW_HASH_MAX_ENTRIES];
            let mut num_matches = 0usize;
            let mut matches = match_mask::<ROW_LOG>(dms_tag_row, dms_tag, head);

            while matches > 0 && nb_attempts > 0 {
                let match_pos = (head + matches.trailing_zeros()) & row_mask;
                let match_index = *dms_row.add(match_pos as usize);
                matches &= matches - 1;
                if match_pos == 0 {
                    continue;
                }
                if match_index < dms_lowest_index {
                    break;
                }
                prefetch_l1(dms_base.wrapping_add(match_index as usize));
                match_buffer[num_matches] = match_index;
                num_matches += 1;
                nb_attempts -= 1;
            }

            // Return the longest match
            for &match_index in &match_buffer[..num_matches] {
                let mut current_ml = 0;
                debug_assert!(match_index >= dms_lowest_index);
                let m = dms_base.wrapping_add(match_index as usize);
                s.chk(m, 4);
                if read32(m) == read32(ip) {
                    current_ml = s.count_ext(ip.add(4), m.add(4), ilimit) + 4;
                }
                if current_ml > ml {
                    ml = current_ml;
                    debug_assert!(curr > match_index + dms_index_delta);
                    *offset_ptr = curr - (match_index + dms_index_delta) + REP_NUM;
                    if ip.add(current_ml) == ilimit {
                        break;
                    }
                }
            }
        }
    }
    ml
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_bits(row: &[u8], tag: u8) -> u64 {
        row.iter()
            .enumerate()
            .filter(|&(_, &b)| b == tag)
            .fold(0, |m, (i, _)| m | 1 << i)
    }

    #[test]
    #[allow(unsafe_code)]
    fn match_masks() {
        let mut s = 0x1234_5678_9abc_def1u64;
        for round in 0..2000 {
            let mut row = [0u8; 64];
            for b in &mut row {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                // few distinct values, so that tags often match
                *b = (s >> 40) as u8 % if round % 2 == 0 { 4 } else { 255 };
            }
            let tag = row[round % 64];
            let head = (round as u32 * 7) % 64;
            // SAFETY: `row` has 64 readable bytes.
            unsafe {
                let p = row.as_ptr();
                let n16 = naive_bits(&row[..16], tag);
                let n32 = naive_bits(&row[..32], tag);
                let n64 = naive_bits(&row, tag);
                assert_eq!(match_bits_swar::<4>(p, tag) & 0xFFFF, n16);
                assert_eq!(match_bits_swar::<5>(p, tag) & 0xFFFF_FFFF, n32);
                assert_eq!(match_bits_swar::<6>(p, tag), n64);
                #[cfg(target_arch = "x86_64")]
                {
                    assert_eq!(match_bits_sse2::<4>(p, tag), n16);
                    assert_eq!(match_bits_sse2::<5>(p, tag), n32);
                    assert_eq!(match_bits_sse2::<6>(p, tag), n64);
                }
                let h = head % 16;
                assert_eq!(
                    match_mask::<4>(p, tag, h),
                    u64::from((n16 as u16).rotate_right(h))
                );
                assert_eq!(match_mask::<6>(p, tag, head), n64.rotate_right(head));
            }
        }
    }

    #[test]
    fn next_index_cycles() {
        let mut head = 0u8;
        let mut seen = alloc::vec::Vec::new();
        for _ in 0..31 {
            // SAFETY: `head` is a valid byte.
            #[allow(unsafe_code)]
            seen.push(unsafe { row_next_index(&mut head, 15) });
        }
        let want: alloc::vec::Vec<u32> = (1..=15).rev().cycle().take(31).collect();
        assert_eq!(seen, want);
    }
}
