//! Compressed blocks: literals section, sequences section, and the fused
//! sequence decode + execute loop.
//!
//! Output is written through raw pointers so the same code serves a
//! `Vec`'s spare capacity, a caller's slice and the streaming window. The
//! fast path copies in 16-byte chunks that may write up to
//! [`WILDCOPY_OVERLENGTH`] bytes past the sequence end and read that far
//! past the literals, so it only runs while the output has that much room
//! and every literal buffer carries that much slack; the last sequences
//! of a buffer take the exact (slow) path, as in libzstd.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::bits::{BitReader, SeqBits};
use crate::cpu::Features;
use crate::error::{Error, Result};
use crate::fse::{self, LL_KIND, ML_KIND, OF_KIND, SeqSymbol, SeqTables};
use crate::huf::{self, HufDt, HufRef, HufTable};

pub(crate) const WILDCOPY_OVERLENGTH: usize = 32;
pub(crate) const BLOCK_SIZE_MAX: usize = 128 << 10;
const MIN_LITERALS_FOR_4_STREAMS: usize = 6;
const LONGNBSEQ: usize = 0x7F00;
/// The initial size of the literal buffer.
const LIT_BUF_MIN: usize = 4096;

/// Where decoded bytes go. `ptr[..cap]` must be writable (it may be
/// uninitialized); `ptr[..pos]` holds the output so far.
#[derive(Debug)]
pub(crate) struct Output {
    pub ptr: *mut u8,
    pub cap: usize,
    pub pos: usize,
}

/// Entropy state carried between the blocks of a frame.
#[derive(Clone, Debug)]
pub(crate) struct Entropy {
    pub huf: HufTable,
    pub huf_ok: bool,
    pub seq: SeqTables,
    pub fse_ok: bool,
    pub rep: [u32; 3],
}

impl Entropy {
    pub(crate) const fn new() -> Self {
        Entropy {
            huf: HufTable::new(),
            huf_ok: false,
            seq: SeqTables::new(),
            fse_ok: false,
            rep: [1, 4, 8],
        }
    }

    /// State at the start of a frame without a dictionary.
    pub(crate) fn reset(&mut self) {
        self.huf_ok = false;
        self.fse_ok = false;
        self.rep = [1, 4, 8];
    }
}

/// A dictionary's entropy tables and repeat offsets (libzstd's
/// `ZSTD_entropyDTables_t` in a `ZSTD_DDict`), used in place by the
/// frames that start on them: only the single-symbol Huffman table (the
/// decoder builds the double-symbol one into its own space), and both
/// kinds of table always valid.
#[derive(Clone, Debug)]
pub(crate) struct DictEntropy {
    pub huf: HufDt,
    pub seq: SeqTables,
    pub rep: [u32; 3],
}

/// Decoder state for blocks: entropy tables plus the literals buffer.
#[derive(Debug)]
pub(crate) struct BlockDecoder {
    /// The decoder's own tables and the repeat offsets. A frame with a
    /// dictionary starts on the dictionary's tables in place (libzstd's
    /// `ZSTD_copyDDictParameters` points at the `ZSTD_DDict`'s): `huf_dict`
    /// and `seq_dict` say which of `ent.huf` and `ent.seq` the dictionary's
    /// stand in for, until a block brings its own.
    pub ent: Entropy,
    huf_dict: bool,
    seq_dict: bool,
    /// The double-symbol table of the dictionary's Huffman table, built on
    /// its first use after the first frame (`dict_x2_ready`, cleared when
    /// the dictionary changes).
    dict_x2: Option<Box<huf::X2Table>>,
    dict_x2_ready: bool,
    /// Decoded literals, always with `WILDCOPY_OVERLENGTH` readable bytes
    /// after the last literal. Its first `LIT_BUF_MIN` bytes come with the
    /// decoder (libzstd's `DCtx` holds its literal buffer from creation):
    /// a fresh decompressor's first frame doesn't allocate and fault it in.
    lit_buf: Vec<u8>,
    pub cpu: Features,
    /// The dictionary was just installed and its content is likely out of
    /// cache (libzstd's `ddictIsCold`): the next compressed block takes
    /// the prefetching loop. Cleared by that block.
    pub dict_cold: bool,
}

/// The literals of one block.
struct Literals {
    ptr: *const u8,
    len: usize,
}

impl BlockDecoder {
    pub(crate) fn new() -> Self {
        BlockDecoder {
            ent: Entropy::new(),
            huf_dict: false,
            seq_dict: false,
            dict_x2: None,
            dict_x2_ready: false,
            lit_buf: alloc::vec![0; LIT_BUF_MIN],
            cpu: Features::detect(),
            dict_cold: false,
        }
    }

    /// The decoder's dictionary was replaced (or removed).
    pub(crate) fn dict_changed(&mut self) {
        self.dict_x2_ready = false;
    }

    /// Entropy state at the start of a frame: the dictionary's tables and
    /// repeat offsets (`dict`, used in place), or none.
    pub(crate) fn start_frame(&mut self, dict: Option<&DictEntropy>) {
        match dict {
            Some(e) => {
                self.ent.huf_ok = true;
                self.ent.fse_ok = true;
                self.ent.rep = e.rep;
                self.huf_dict = true;
                self.seq_dict = true;
            }
            None => {
                self.ent.reset();
                self.huf_dict = false;
                self.seq_dict = false;
            }
        }
    }

    fn lit_space(&mut self, n: usize) -> &mut [u8] {
        let need = n + WILDCOPY_OVERLENGTH;
        if self.lit_buf.len() < need {
            self.lit_buf.resize(need.max(LIT_BUF_MIN), 0);
        }
        &mut self.lit_buf[..n]
    }

    /// Decodes one compressed block of a frame. `out.pos - prefix` bytes of
    /// the frame are already in the output, and `dict` precedes them.
    /// `dict_ent` is the entropy of the dictionary given to `start_frame`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn decode_block(
        &mut self,
        src: &[u8],
        out: &mut Output,
        prefix: usize,
        dict: &[u8],
        dict_ent: Option<&DictEntropy>,
        block_max: usize,
    ) -> Result<()> {
        if src.len() > block_max {
            return Err(Error::Corrupted);
        }
        let room = (out.cap - out.pos).min(block_max);
        let (lits, used) = self.decode_literals(src, block_max, room, dict_ent)?;
        let src = &src[used..];
        let (nb_seq, used) = self.decode_seq_headers(src, dict_ent)?;
        let seq_src = &src[used..];
        let cold = core::mem::take(&mut self.dict_cold);
        if nb_seq == 0 {
            // Literals only.
            if lits.len > out.cap - out.pos {
                return Err(Error::OutputTooSmall);
            }
            // SAFETY: `lits` points at `lits.len` initialized bytes (in
            // `lit_buf` or `src`), and the output has room for them.
            #[allow(unsafe_code)]
            unsafe {
                ptr::copy_nonoverlapping(lits.ptr, out.ptr.add(out.pos), lits.len)
            };
            out.pos += lits.len;
            return Ok(());
        }
        self.ent.fse_ok = true;
        let tables = match (self.seq_dict, dict_ent) {
            (true, Some(e)) => &e.seq,
            _ => &self.ent.seq,
        };
        let long = use_prefetch_loop(cold);
        let mut rep = self.ent.rep.map(|r| r as usize);
        // A block regenerates at most `block_max` bytes (the format's
        // Block_Maximum_Size). libzstd's one-shot decoder, given the room,
        // puts the literals `block_max + WILDCOPY_OVERLENGTH` after the
        // block and stops the sequences there; its streaming stages decode
        // into a `block_max` buffer. Bound the sequences the same way
        // whatever the output space, so a slice and a growing `Vec` (which
        // reserves `block_max` per block) agree. The slack keeps valid
        // blocks on the fast (wildcopy) path.
        let cap = out.cap;
        let limit = out.pos.saturating_add(block_max + WILDCOPY_OVERLENGTH);
        let start = out.pos;
        out.cap = cap.min(limit);
        let res = if long {
            run_sequences_long(
                self.cpu, tables, &mut rep, seq_src, nb_seq, &lits, out, prefix, dict,
            )
        } else {
            run_sequences(
                self.cpu, tables, &mut rep, seq_src, nb_seq, &lits, out, prefix, dict,
            )
        };
        out.cap = cap;
        self.ent.rep = rep.map(|r| r as u32);
        match res {
            // The limit, not the output space, stopped it.
            Err(Error::OutputTooSmall) if limit <= cap => Err(Error::Corrupted),
            Ok(()) if out.pos - start > block_max => Err(Error::Corrupted),
            res => res,
        }
    }

    /// Literals section (libzstd's `ZSTD_decodeLiteralsBlock`). Returns the
    /// literals and the bytes of `src` used.
    fn decode_literals(
        &mut self,
        src: &[u8],
        block_max: usize,
        room: usize,
        dict_ent: Option<&DictEntropy>,
    ) -> Result<(Literals, usize)> {
        if src.len() < 2 {
            return Err(Error::Corrupted);
        }
        let b0 = src[0];
        let kind = b0 & 3;
        let lhl = (b0 >> 2) & 3;
        match kind {
            // Raw or RLE
            0 | 1 => {
                let (lh, lit_size) = match lhl {
                    1 => (2, (u16::from_le_bytes([src[0], src[1]]) >> 4) as usize),
                    3 => {
                        if src.len() < 3 {
                            return Err(Error::Corrupted);
                        }
                        (
                            3,
                            (u32::from_le_bytes([src[0], src[1], src[2], 0]) >> 4) as usize,
                        )
                    }
                    _ => (1, (b0 >> 3) as usize),
                };
                if lit_size > block_max {
                    return Err(Error::Corrupted);
                }
                if lit_size > room {
                    return Err(Error::OutputTooSmall);
                }
                if kind == 0 {
                    if lh + lit_size + WILDCOPY_OVERLENGTH <= src.len() {
                        // Enough slack after them: use the literals in place.
                        return Ok((
                            Literals {
                                ptr: src[lh..].as_ptr(),
                                len: lit_size,
                            },
                            lh + lit_size,
                        ));
                    }
                    if lh + lit_size > src.len() {
                        return Err(Error::Corrupted);
                    }
                    self.lit_space(lit_size)
                        .copy_from_slice(&src[lh..lh + lit_size]);
                    Ok((
                        Literals {
                            ptr: self.lit_buf.as_ptr(),
                            len: lit_size,
                        },
                        lh + lit_size,
                    ))
                } else {
                    if lh >= src.len() {
                        return Err(Error::Corrupted);
                    }
                    let b = src[lh];
                    self.lit_space(lit_size).fill(b);
                    Ok((
                        Literals {
                            ptr: self.lit_buf.as_ptr(),
                            len: lit_size,
                        },
                        lh + 1,
                    ))
                }
            }
            // Compressed or treeless
            _ => {
                if src.len() < 5 {
                    return Err(Error::Corrupted);
                }
                let lhc = u32::from_le_bytes([src[0], src[1], src[2], src[3]]) as usize;
                let (lh, lit_size, lit_csize, single) = match lhl {
                    0 | 1 => (3, (lhc >> 4) & 0x3FF, (lhc >> 14) & 0x3FF, lhl == 0),
                    2 => (4, (lhc >> 4) & 0x3FFF, lhc >> 18, false),
                    _ => (
                        5,
                        (lhc >> 4) & 0x3FFFF,
                        (lhc >> 22) + ((src[4] as usize) << 10),
                        false,
                    ),
                };
                if lit_size > block_max {
                    return Err(Error::Corrupted);
                }
                if !single && lit_size < MIN_LITERALS_FOR_4_STREAMS {
                    return Err(Error::Corrupted);
                }
                if lit_csize + lh > src.len() {
                    return Err(Error::Corrupted);
                }
                if lit_size > room {
                    return Err(Error::OutputTooSmall);
                }
                let mut csrc = &src[lh..lh + lit_csize];
                if kind == 3 {
                    if !self.ent.huf_ok {
                        return Err(Error::Corrupted);
                    }
                } else {
                    if lit_csize == 0 {
                        return Err(Error::Corrupted);
                    }
                    let h = self.ent.huf.read(csrc)?;
                    if h >= csrc.len() {
                        return Err(Error::Corrupted);
                    }
                    csrc = &csrc[h..];
                    self.ent.huf_ok = true;
                    self.huf_dict = false;
                }
                let cpu = self.cpu;
                let need = lit_size + WILDCOPY_OVERLENGTH;
                if self.lit_buf.len() < need {
                    self.lit_buf.resize(need.max(LIT_BUF_MIN), 0);
                }
                let (table, x2) = match (self.huf_dict, dict_ent) {
                    (true, Some(e)) if self.dict_cold => {
                        // The table of a cold dictionary, at any size
                        // (libzstd prefetches over 768 literals; the
                        // single-stream decode of fewer is a chain of
                        // dependent misses: 4 KiB frames, 1.0-2.0 us
                        // faster). Single-symbol: half the cache lines.
                        e.huf.touch();
                        (HufRef::Shared(&e.huf, None), false)
                    }
                    (true, Some(e)) => {
                        // A dictionary in use for more than one frame: its
                        // double-symbol table (libzstd's dictionaries hold
                        // only that one), built once into our own space.
                        let x2 = e.huf.x2_capable() && huf::prefer_x2_built(lit_size, lit_csize);
                        if x2 && !self.dict_x2_ready {
                            let dt2 = self.dict_x2.get_or_insert_with(|| Box::new([0; 2048]));
                            self.ent.huf.build_x2_of(&e.huf, dt2);
                            self.dict_x2_ready = true;
                        }
                        let dt2 = self.dict_x2.as_deref().filter(|_| x2);
                        (HufRef::Shared(&e.huf, dt2), x2)
                    }
                    (true, None) => return Err(Error::Corrupted),
                    (false, _) => (
                        HufRef::Own(&mut self.ent.huf),
                        !single && huf::prefer_x2(lit_size, lit_csize),
                    ),
                };
                let out = &mut self.lit_buf[..lit_size];
                if single {
                    huf::decode_1x(out, csrc, table.get(), table.shared_x2())?;
                } else {
                    huf::decode_4x(out, csrc, table, x2, cpu)?;
                }
                Ok((
                    Literals {
                        ptr: self.lit_buf.as_ptr(),
                        len: lit_size,
                    },
                    lh + lit_csize,
                ))
            }
        }
    }

    /// Sequences section header (libzstd's `ZSTD_decodeSeqHeaders`).
    fn decode_seq_headers(
        &mut self,
        src: &[u8],
        dict_ent: Option<&DictEntropy>,
    ) -> Result<(usize, usize)> {
        let Some(&b0) = src.first() else {
            return Err(Error::Corrupted);
        };
        let mut ip = 1;
        let mut nb_seq = b0 as usize;
        if nb_seq > 0x7F {
            if nb_seq == 0xFF {
                if ip + 2 > src.len() {
                    return Err(Error::Corrupted);
                }
                nb_seq = u16::from_le_bytes([src[1], src[2]]) as usize + LONGNBSEQ;
                ip += 2;
            } else {
                if ip >= src.len() {
                    return Err(Error::Corrupted);
                }
                nb_seq = ((nb_seq - 0x80) << 8) + src[1] as usize;
                ip += 1;
            }
        }
        if nb_seq == 0 {
            if ip != src.len() {
                return Err(Error::Corrupted);
            }
            return Ok((0, ip));
        }
        let Some(&modes) = src.get(ip) else {
            return Err(Error::Corrupted);
        };
        if modes & 3 != 0 {
            return Err(Error::Corrupted);
        }
        ip += 1;
        if self.seq_dict {
            let Some(e) = dict_ent else {
                return Err(Error::Corrupted);
            };
            if modes & 0xFC == 0xFC {
                // Three repeats: the dictionary's tables stay in use.
                if self.dict_cold {
                    // The tables of a cold dictionary, for any number of
                    // sequences (libzstd: over 24; 256-byte frames measured
                    // 0.3-1.1 us faster without that limit).
                    for kind in [LL_KIND, OF_KIND, ML_KIND] {
                        let n = 1 << e.seq.log[kind.index];
                        touch_area(&e.seq.cells[kind.region..kind.region + n]);
                    }
                }
                return Ok((nb_seq, ip));
            }
            // A new table: the repeated ones join it in our own tables.
            for (shift, kind) in [(6, LL_KIND), (4, OF_KIND), (2, ML_KIND)] {
                if (modes >> shift) & 3 == 3 {
                    self.ent.seq.copy_kind(&e.seq, kind);
                }
            }
            self.seq_dict = false;
        }
        let fse_ok = self.ent.fse_ok;
        let t = &mut self.ent.seq;
        let cpu = self.cpu;
        ip += t.read(modes >> 6, LL_KIND, &src[ip..], fse_ok, cpu)?;
        ip += t.read((modes >> 4) & 3, OF_KIND, &src[ip..], fse_ok, cpu)?;
        ip += t.read((modes >> 2) & 3, ML_KIND, &src[ip..], fse_ok, cpu)?;
        Ok((nb_seq, ip))
    }
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn run_sequences(
    cpu: Features,
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if cpu.bmi2 {
        #[allow(unsafe_code)]
        // SAFETY: the caller detected BMI2 (and BMI1/LZCNT) on this CPU.
        return unsafe { sequences_bmi2(t, rep, src, nb_seq, lits, out, prefix, dict) };
    }
    let _ = cpu;
    sequences_generic(t, rep, src, nb_seq, lits, out, prefix, dict)
}

#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[target_feature(enable = "bmi1,bmi2,lzcnt,ssse3")]
#[allow(clippy::too_many_arguments)]
fn sequences_bmi2(
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    // `cpu.bmi2` includes SSSE3 (see `cpu::bmi2`): the shuffle copies.
    sequences_body::<true>(t, rep, src, nb_seq, lits, out, prefix, dict)
}

#[allow(clippy::too_many_arguments)]
fn sequences_generic(
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    sequences_body::<{ copy::SHUFFLE_BASELINE }>(t, rep, src, nb_seq, lits, out, prefix, dict)
}

/// Selection of the sequence loop for tests and A/B measurements
/// ([`set_seq_loop`]): auto (`use_prefetch_loop`), always the prefetching
/// loop, or never.
static SEQ_LOOP: AtomicU8 = AtomicU8::new(LOOP_AUTO);
const LOOP_AUTO: u8 = 0;
const LOOP_LONG: u8 = 1;
const LOOP_SHORT: u8 = 2;

/// Forces the prefetching sequence loop (`Some(true)`), the fused loop
/// (`Some(false)`), or restores the default (`None`). For tests
/// (every frame through either loop) and A/B measurements in one binary.
#[doc(hidden)]
pub fn set_seq_loop(long: Option<bool>) {
    let v = match long {
        None => LOOP_AUTO,
        Some(true) => LOOP_LONG,
        Some(false) => LOOP_SHORT,
    };
    SEQ_LOOP.store(v, Ordering::Relaxed);
}

#[cfg(test)]
std::thread_local! {
    /// The library's unit tests choose the loop per thread
    /// (`tests::set_thread_seq_loop`): they run in parallel, and the
    /// process-wide [`SEQ_LOOP`] would force it for the others too.
    static THREAD_SEQ_LOOP: core::cell::Cell<u8> = const { core::cell::Cell::new(LOOP_AUTO) };
}

/// Blocks decoded by the prefetching loop, in builds with debug
/// assertions (for tests that must prove they reach it).
#[cfg(debug_assertions)]
static LONG_BLOCKS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The number of blocks the prefetching loop decoded so far (`None`
/// without debug assertions).
#[doc(hidden)]
pub fn long_blocks() -> Option<usize> {
    #[cfg(debug_assertions)]
    return Some(LONG_BLOCKS.load(Ordering::Relaxed));
    #[cfg(not(debug_assertions))]
    None
}

/// Whether a block takes the prefetching loop: as [`set_seq_loop`] forces
/// it, else for the first compressed block after a dictionary was
/// installed (`cold`, libzstd's `ddictIsCold`), on x86-64, where the
/// prefetch exists ([`prefetch_l1`]; elsewhere decoding ahead would be
/// pure overhead).
///
/// libzstd also takes it when the history exceeds 16 MiB and offsets of
/// more than 22 extra bits hold at least 7/256 of the offset table
/// (`ZSTD_getOffsetInfo`). That half of its heuristic is left out: on Zen 3
/// the loop is slower wherever it fires (decoding ahead costs ~20
/// instructions per sequence; the out-of-order window already overlaps
/// most far-match misses). EPYC 7543, cycles against the fused loop: the
/// Silesia files catenated, window log 27, L19 +1.6% with libzstd's choice
/// (63% of the blocks), +9% for every block; the corpus followed by an
/// edited copy (matches 100 MiB back), L19 +3.0%, L9 +0.2% (+4% to +5% for
/// every block).
#[inline]
fn use_prefetch_loop(cold: bool) -> bool {
    let choice = SEQ_LOOP.load(Ordering::Relaxed);
    #[cfg(test)]
    let choice = match THREAD_SEQ_LOOP.with(core::cell::Cell::get) {
        LOOP_AUTO => choice,
        c => c,
    };
    let long = match choice {
        LOOP_LONG => true,
        LOOP_SHORT => false,
        _ => HAS_PREFETCH && cold,
    };
    #[cfg(debug_assertions)]
    if long {
        LONG_BLOCKS.fetch_add(1, Ordering::Relaxed);
    }
    long
}

const HAS_PREFETCH: bool = cfg!(target_arch = "x86_64");

/// Prefetches the cache line at `p` into L1 (any address: a prefetch
/// never faults).
#[inline(always)]
fn prefetch_l1(p: *const u8) {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    // SAFETY: `prefetcht0` only hints the cache; it doesn't dereference
    // `p` (no fault, no memory access visible to the program) and SSE is
    // in the x86-64 baseline.
    #[allow(unsafe_code, unused_unsafe)]
    unsafe {
        use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch::<_MM_HINT_T0>(p.cast());
    }
    #[cfg(not(all(target_arch = "x86_64", not(miri))))]
    let _ = p;
}

/// Loads a value from every cache line of `area`: libzstd's
/// `PREFETCH_AREA` for the tables of a dictionary just installed (likely
/// out of cache), before a block looks them up at random, one dependent
/// miss after the other. With loads instead of libzstd's `prefetcht1`:
/// independent misses that overlap, where the software prefetches did
/// little on Zen 3 (EPYC 7543, 4 KiB frames with a 110 KiB dictionary:
/// 24.5 us per frame with prefetches, 21.0 us with loads).
#[inline]
pub(crate) fn touch_area<T: Copy>(area: &[T]) {
    let step = (64 / core::mem::size_of::<T>()).max(1);
    for x in area.iter().step_by(step) {
        core::hint::black_box(*x);
    }
}

/// libzstd's `ZSTD_prefetchMatch`: prefetches the first two cache lines
/// of `seq`'s match source and returns the output position after `seq`.
/// `pos` is the output position relative to `prefix_start` (where the
/// sequence starts); a match before the prefix is in the dictionary,
/// which ends at `dict_end`. Wrapping arithmetic: a corrupt offset only
/// prefetches a useless address.
#[inline(always)]
fn prefetch_match(pos: usize, seq: &Seq, prefix_start: *const u8, dict_end: *const u8) -> usize {
    let pos = pos.wrapping_add(seq.lit_len);
    let base = if seq.offset > pos {
        dict_end
    } else {
        prefix_start
    };
    let m = base.wrapping_add(pos).wrapping_sub(seq.offset);
    prefetch_l1(m);
    prefetch_l1(m.wrapping_add(64));
    pos.wrapping_add(seq.match_len)
}

/// Out of line and cold, like the loops it calls: it runs once per
/// dictionary installed. Inline (or the loops in the hot text section) it
/// changed how `decode_block` inlines the fused loop and moved the
/// per-block code: 0.2-1% slower one-shot decoding, same instructions.
#[allow(clippy::too_many_arguments)]
#[cold]
#[inline(never)]
fn run_sequences_long(
    cpu: Features,
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if cpu.bmi2 {
        #[allow(unsafe_code)]
        // SAFETY: the caller detected BMI2 (and BMI1/LZCNT/SSSE3) on this CPU.
        return unsafe { sequences_long_bmi2(t, rep, src, nb_seq, lits, out, prefix, dict) };
    }
    let _ = cpu;
    sequences_long_generic(t, rep, src, nb_seq, lits, out, prefix, dict)
}

#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[target_feature(enable = "bmi1,bmi2,lzcnt,ssse3")]
#[allow(clippy::too_many_arguments)]
#[cold]
fn sequences_long_bmi2(
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    sequences_long_body::<true>(t, rep, src, nb_seq, lits, out, prefix, dict)
}

#[allow(clippy::too_many_arguments)]
#[cold]
#[inline(never)]
fn sequences_long_generic(
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    sequences_long_body::<{ copy::SHUFFLE_BASELINE }>(t, rep, src, nb_seq, lits, out, prefix, dict)
}

/// The cell for `state` in the combined table at `tp`.
#[inline(always)]
fn cell(tp: *const SeqSymbol, state: usize) -> SeqSymbol {
    debug_assert!(state < fse::SEQ_TABLE_SIZE);
    // SAFETY: FSE construction keeps every state inside its region:
    // initial states are `log`-bit reads plus the region start, and
    // `next_state + bits` stays below region start + `1 << log` for every
    // cell (see `fse::build_seq_table`). The table outlives the loop.
    #[allow(unsafe_code)]
    unsafe {
        *tp.add(state)
    }
}

#[derive(Clone, Copy)]
struct Seq {
    lit_len: usize,
    match_len: usize,
    offset: usize,
}

/// One sequence (libzstd's `ZSTD_decodeSequence`, 64-bit flavor).
#[inline(always)]
fn decode_sequence<B: SeqBits>(
    br: &mut B,
    tp: *const SeqSymbol,
    ll: &mut usize,
    ml: &mut usize,
    of: &mut usize,
    rep: &mut [usize; 3],
    last: bool,
) -> Seq {
    let llc = cell(tp, *ll);
    let mlc = cell(tp, *ml);
    let ofc = cell(tp, *of);
    let ll_bits = u32::from(llc.nb_add_bits);
    let ml_bits = u32::from(mlc.nb_add_bits);
    let of_bits = u32::from(ofc.nb_add_bits);

    // Extra bits in libzstd's order: offset, match length, (reload),
    // literal length. Reading them together measured slower on text (an
    // extra data-dependent branch, or worse register allocation).
    let mut lit_len = llc.base as usize;
    let mut match_len = mlc.base as usize;
    let offset;
    if of_bits > 1 {
        offset = ofc.base as usize + br.read_fast(of_bits) as usize;
        rep[2] = rep[1];
        rep[1] = rep[0];
        rep[0] = offset;
    } else {
        // Constant indexes only, so the three offsets stay in registers.
        let ll0 = llc.base == 0;
        if of_bits == 0 {
            // Repeat offset 1, or 2 when the literal length is 0.
            if ll0 {
                rep.swap(0, 1);
            }
            offset = rep[0];
        } else {
            // Code 1: repeat offset 2 or 3 (3 or "1 - 1" when ll is 0).
            let o = ll0 as usize + br.read_fast(1) as usize;
            let mut temp = match o {
                0 => rep[1],
                1 => rep[2],
                _ => rep[0].wrapping_sub(1),
            };
            // 0 is invalid: make it huge so execution reports corruption.
            temp = temp.wrapping_sub((temp == 0) as usize);
            if o != 0 {
                rep[2] = rep[1];
            }
            rep[1] = rep[0];
            rep[0] = temp;
            offset = temp;
        }
    }
    if ml_bits > 0 {
        match_len += br.read_fast(ml_bits) as usize;
    }
    if ll_bits + ml_bits + of_bits
        >= 64 - 7 - (fse::LL_FSE_LOG + fse::ML_FSE_LOG + fse::OFF_FSE_LOG)
    {
        br.reload();
    }
    if ll_bits > 0 {
        lit_len += br.read_fast(ll_bits) as usize;
    }
    if !last {
        // One read for the three state updates (LL bits first, then ML, then
        // OF; at most 9 + 9 + 8 bits): one dependency on the consumed count
        // instead of three.
        let (lb, mb, ob) = (
            u32::from(llc.nb_bits),
            u32::from(mlc.nb_bits),
            u32::from(ofc.nb_bits),
        );
        let v = br.read(lb + mb + ob);
        *ll = llc.next_state as usize + (v >> (mb + ob)) as usize;
        *ml = mlc.next_state as usize + ((v >> ob) & ((1 << mb) - 1)) as usize;
        *of = ofc.next_state as usize + (v & ((1 << ob) - 1)) as usize;
        br.reload();
    }
    Seq {
        lit_len,
        match_len,
        offset,
    }
}

#[allow(unsafe_code)]
mod copy {
    //! Copy helpers; every caller guarantees the ranges are in bounds.
    use core::ptr;

    /// Copies 16 bytes; the ranges must not overlap.
    #[inline(always)]
    pub(super) unsafe fn copy16(dst: *mut u8, src: *const u8) {
        // SAFETY: per the caller.
        unsafe { ptr::copy_nonoverlapping(src, dst, 16) }
    }

    #[inline(always)]
    pub(super) unsafe fn copy8(dst: *mut u8, src: *const u8) {
        // SAFETY: per the caller.
        unsafe { ptr::copy_nonoverlapping(src, dst, 8) }
    }

    /// Copies `len` bytes in 16-byte chunks, writing and reading up to 31
    /// bytes past the end. `dst - src >= 16` or the buffers are disjoint.
    #[inline(always)]
    pub(super) unsafe fn wildcopy(dst: *mut u8, src: *const u8, len: usize) {
        // SAFETY: per the caller.
        unsafe {
            copy16(dst, src);
            if len <= 16 {
                return;
            }
            let mut d = dst.add(16);
            let mut s = src.add(16);
            let end = dst.add(len);
            loop {
                copy16(d, s);
                copy16(d.add(16), s.add(16));
                d = d.add(32);
                s = s.add(32);
                if d >= end {
                    break;
                }
            }
        }
    }

    /// Wildcopy for `8 <= dst - src < 16`: 8 bytes at a time.
    #[inline(always)]
    pub(super) unsafe fn wildcopy8(dst: *mut u8, src: *const u8, len: usize) {
        // SAFETY: per the caller.
        unsafe {
            let mut d = dst;
            let mut s = src;
            let end = dst.add(len);
            loop {
                copy8(d, s);
                d = d.add(8);
                s = s.add(8);
                if d >= end {
                    break;
                }
            }
        }
    }

    /// libzstd's `ZSTD_overlapCopy8`: copies 8 bytes from `*m` to `*o` for
    /// an offset under 16, and leaves `*o - *m >= 8`.
    #[inline(always)]
    pub(super) unsafe fn overlap_copy8(o: &mut *mut u8, m: &mut *const u8, offset: usize) {
        const DEC32: [usize; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
        const DEC64: [usize; 8] = [8, 8, 8, 7, 8, 9, 10, 11];
        // SAFETY: per the caller: `*m = *o - offset` with 8 + 8 bytes in bounds.
        unsafe {
            if offset < 8 {
                let sub2 = DEC64[offset];
                *(*o) = **m;
                *(*o).add(1) = *(*m).add(1);
                *(*o).add(2) = *(*m).add(2);
                *(*o).add(3) = *(*m).add(3);
                *m = (*m).add(DEC32[offset]);
                ptr::copy_nonoverlapping(*m, (*o).add(4), 4);
                // `- sub2` can point before the start of the output (it is
                // undone by the `+ 8` below: the net move `dec32 + 8 - dec64`
                // is never negative). Wrapping arithmetic keeps that
                // well-defined (an in-bounds `sub` there is undefined, which
                // Miri reports); it compiles to the same code.
                *m = (*m).wrapping_sub(sub2);
            } else {
                copy8(*o, *m);
            }
            *m = (*m).wrapping_add(8);
            *o = (*o).add(8);
            debug_assert!(*o as usize - *m as usize >= 8);
        }
    }

    /// Whether the generic sequence loop uses the shuffle copies: on
    /// AArch64, whose baseline has NEON. (On x86-64 only the BMI2 loop
    /// does: every BMI2 CPU has SSSE3, and `cpu::bmi2` checks it.)
    pub(super) const SHUFFLE_BASELINE: bool = cfg!(all(
        target_arch = "aarch64",
        target_feature = "neon",
        target_endian = "little",
        not(feature = "portable")
    ));

    /// Matches with offsets under 16 take the shuffle copy above this
    /// length; shorter ones keep the scalar copies inline (two 8-byte
    /// copies at most), so short offset-8 matches in integer data and text
    /// run exactly the code they did before.
    const SHUFFLE_MIN: usize = 16;

    /// Copies a match of `match_len` bytes from `offset` bytes back,
    /// writing up to 31 bytes past its end. `SHUF` selects the shuffle
    /// copy for offsets under 16 ([`shuffle::short_copy`]); the caller
    /// guarantees the CPU has its instructions.
    #[inline(always)]
    pub(super) unsafe fn match_copy<const SHUF: bool>(
        mop: *mut u8,
        offset: usize,
        match_len: usize,
    ) {
        debug_assert!(offset > 0 && match_len > 0);
        // SAFETY: per the caller: `mop - offset .. mop` is written output
        // and `mop .. mop + match_len + 32` is writable.
        unsafe {
            let mut mop = mop;
            let mut mp: *const u8 = mop.sub(offset);
            if offset >= 16 {
                wildcopy(mop, mp, match_len);
            } else if SHUF && match_len > SHUFFLE_MIN {
                shuffle::short_copy(mop, mp, offset, match_len);
            } else {
                overlap_copy8(&mut mop, &mut mp, offset);
                if match_len > 8 {
                    wildcopy8(mop, mp, match_len - 8);
                }
            }
        }
    }

    /// Matches with offsets under 16, 16 bytes per store: one period of
    /// the pattern in a vector register, advanced by a byte shuffle with no
    /// load in the loop (libzstd's scalar loop stores 8 bytes per
    /// iteration and reloads what it just stored). From meta/zstd#4718,
    /// but the first vector is built from two 8-byte loads of already
    /// written bytes (each inside one earlier store, so store-forwarded)
    /// instead of 16 scalar bytes and a 16-byte reload.
    #[cfg(any(
        all(target_arch = "x86_64", not(feature = "portable")),
        all(
            target_arch = "aarch64",
            target_feature = "neon",
            target_endian = "little",
            not(feature = "portable")
        )
    ))]
    mod shuffle {
        #[cfg(target_arch = "aarch64")]
        use core::arch::aarch64::{
            uint8x16_t as V, vcombine_u8, vld1_u8, vld1q_u8, vqtbl1q_u8, vst1q_u8,
        };
        #[cfg(target_arch = "x86_64")]
        use core::arch::x86_64::{
            __m128i as V, _mm_load_si128, _mm_loadl_epi64, _mm_shuffle_epi8, _mm_storeu_si128,
            _mm_unpacklo_epi64,
        };

        /// Shuffle indexes for the short-offset copies (meta/zstd#4718). Row
        /// `o` serves offset `o` (row 0 is unused).
        #[repr(align(16))]
        struct ShufTables {
            /// The first 16 output bytes from the register the copy starts
            /// with. Offsets under 8 (after `overlap_copy8` wrote the first 8
            /// bytes): byte `k` is output byte `k - 8`, so lane `j` takes
            /// `(8 + j) % o`. Offsets 8 to 15: bytes 0-7 are source bytes 0-7
            /// and bytes 8-15 source bytes `o - 8 .. o`, so lane `j` takes
            /// source byte `s = j % o`, at `s` or `s + 16 - o`.
            init: [[u8; 16]; 16],
            /// The next 16 bytes of the pattern from the last 16: lane `j`
            /// takes `(16 + j) % o` (the output repeats with period `o`).
            adv: [[u8; 16]; 16],
        }

        static SHUF_TABLES: ShufTables = {
            let mut init = [[0u8; 16]; 16];
            let mut adv = [[0u8; 16]; 16];
            let mut o = 1;
            while o < 16 {
                let mut j = 0;
                while j < 16 {
                    adv[o][j] = ((16 + j) % o) as u8;
                    init[o][j] = if o < 8 {
                        (8 + j) % o
                    } else if j % o < 8 {
                        j % o
                    } else {
                        j % o + 16 - o
                    } as u8;
                    j += 1;
                }
                o += 1;
            }
            ShufTables { init, adv }
        };

        /// `lo[..8]` then `hi[..8]`.
        #[inline(always)]
        unsafe fn load8x2(lo: *const u8, hi: *const u8) -> V {
            // SAFETY: per the caller, 8 readable bytes at each.
            unsafe {
                #[cfg(target_arch = "x86_64")]
                return _mm_unpacklo_epi64(_mm_loadl_epi64(lo.cast()), _mm_loadl_epi64(hi.cast()));
                #[cfg(target_arch = "aarch64")]
                return vcombine_u8(vld1_u8(lo), vld1_u8(hi));
            }
        }

        /// `t` (16-byte aligned).
        #[inline(always)]
        fn table(t: &[u8; 16]) -> V {
            // SAFETY: 16 readable bytes, aligned (`ShufTables` is
            // `align(16)` and its rows are 16 bytes).
            unsafe {
                #[cfg(target_arch = "x86_64")]
                return _mm_load_si128(t.as_ptr().cast());
                #[cfg(target_arch = "aarch64")]
                return vld1q_u8(t.as_ptr());
            }
        }

        /// Lane `j` of the result is lane `idx[j]` of `v` (`idx[j] < 16`).
        #[inline(always)]
        unsafe fn shuffle(v: V, idx: V) -> V {
            // SAFETY: the caller guarantees SSSE3 (x86-64); NEON is in
            // the AArch64 baseline.
            unsafe {
                #[cfg(target_arch = "x86_64")]
                return _mm_shuffle_epi8(v, idx);
                #[cfg(target_arch = "aarch64")]
                return vqtbl1q_u8(v, idx);
            }
        }

        #[inline(always)]
        unsafe fn store16(dst: *mut u8, v: V) {
            // SAFETY: per the caller, 16 writable bytes.
            unsafe {
                #[cfg(target_arch = "x86_64")]
                _mm_storeu_si128(dst.cast(), v);
                #[cfg(target_arch = "aarch64")]
                vst1q_u8(dst, v);
            }
        }

        /// Copies a match of `match_len` bytes from `mp = mop - offset`,
        /// `1 <= offset < 16`, writing up to 15 bytes past its end. On
        /// x86-64 the CPU must have SSSE3. Cold and out of line, so the
        /// sequence loop keeps the exact code (and registers) of the
        /// scalar copy for the common short matches: inlined, or called
        /// from inside it, it measured 2-5% slower on short offset-8
        /// matches and on nci.
        #[cold]
        #[inline(never)]
        #[cfg_attr(target_arch = "x86_64", target_feature(enable = "ssse3"))]
        pub(in super::super) unsafe fn short_copy(
            mop: *mut u8,
            mp: *const u8,
            offset: usize,
            match_len: usize,
        ) {
            debug_assert!((1..16).contains(&offset));
            debug_assert!(match_len > 8);
            debug_assert_eq!(mop as usize - mp as usize, offset);
            // SAFETY: per the caller: `mp .. mop` is written output and
            // `mop .. mop + match_len + 15` is writable. The loads read only
            // written bytes: for offsets under 8, `o - 8 .. o` (just
            // written by `overlap_copy8`); else `mp .. mp + 8` and
            // `mop - 8 .. mop`, both in `mp .. mop`.
            unsafe {
                let (mut o, r) = if offset < 8 {
                    let (mut o, mut m) = (mop, mp);
                    super::overlap_copy8(&mut o, &mut m, offset);
                    (o, load8x2(o.sub(8), o.sub(8)))
                } else {
                    (mop, load8x2(mp, mop.sub(8)))
                };
                let end = mop as usize + match_len;
                let mut p = shuffle(r, table(&SHUF_TABLES.init[offset]));
                let adv = table(&SHUF_TABLES.adv[offset]);
                loop {
                    store16(o, p);
                    o = o.add(16);
                    if o as usize >= end {
                        break;
                    }
                    p = shuffle(p, adv);
                }
            }
        }
    }

    /// Never selected here (`SHUF` is false): the scalar copy.
    #[cfg(not(any(
        all(target_arch = "x86_64", not(feature = "portable")),
        all(
            target_arch = "aarch64",
            target_feature = "neon",
            target_endian = "little",
            not(feature = "portable")
        )
    )))]
    mod shuffle {
        pub(in super::super) unsafe fn short_copy(
            mop: *mut u8,
            mp: *const u8,
            offset: usize,
            match_len: usize,
        ) {
            let (mut o, mut m) = (mop, mp);
            // SAFETY: per the caller.
            unsafe {
                super::overlap_copy8(&mut o, &mut m, offset);
                super::wildcopy8(o, m, match_len - 8);
            }
        }
    }
}

/// Executes one sequence inside a decode loop: the fast path when the
/// sequence (plus the wildcopy slack) fits in the output and the literals,
/// else `exec_sequence_end`; then `continue`s the loop. The loop's
/// cursors (`op`, `lp`) and bounds are passed by name so the fast and the
/// checked loop share it with everything in registers.
macro_rules! exec_seq {
    ($shuf:ident; $seq:expr; $op:ident, $lp:ident; $base:ident, $cap:ident, $prefix_start:ident,
     $oend_w:ident, $lit_start:ident, $lit_end:ident, $lit_total:expr, $prefix:ident,
     $dict:ident) => {{
        let seq = $seq;
        let seq_len = seq.lit_len + seq.match_len;
        // In u64 so the sums can't wrap on 32-bit targets (free on 64-bit).
        if $lp as u64 + seq.lit_len as u64 > $lit_end as u64
            || $op as u64 + seq_len as u64 > $oend_w as u64
        {
            let o = $op as usize - $base as usize;
            let mut lit = $lp as usize - $lit_start as usize;
            let o = exec_sequence_end(
                $base,
                $cap,
                o,
                seq.lit_len,
                seq.match_len,
                seq.offset,
                $lit_start,
                $lit_total,
                &mut lit,
                $prefix,
                $dict,
            )?;
            // SAFETY: the new positions are within the output and literals.
            unsafe {
                $op = $base.add(o);
                $lp = $lit_start.add(lit);
            }
            continue;
        }
        // SAFETY: the sequence plus WILDCOPY_OVERLENGTH fits in the
        // output, so every wildcopy write is in bounds; the literals have
        // WILDCOPY_OVERLENGTH readable bytes past their end (see
        // `decode_literals`); match sources are checked to lie in the
        // dictionary (`dict_match`) or in already written output.
        unsafe {
            copy::copy16($op, $lp);
            if seq.lit_len > 16 {
                copy::wildcopy($op.add(16), $lp.add(16), seq.lit_len - 16);
            }
            $lp = $lp.add(seq.lit_len);
            let mop = $op.add(seq.lit_len);
            let op_end = $op.add(seq_len);
            let match_len = seq.match_len;
            let offset = seq.offset;
            let in_prefix = mop as usize - $prefix_start as usize;
            if offset > in_prefix {
                dict_match(mop, offset, in_prefix, match_len, $prefix_start, $dict)?;
                $op = op_end;
                continue;
            }
            copy::match_copy::<$shuf>(mop, offset, match_len);
            $op = op_end;
        }
    }};
}

/// The fused decode + execute loop (libzstd's `ZSTD_decompressSequences_body`).
/// (Decoding batches of sequences first and executing them after was
/// measured 15% slower.) `SHUF` enables the byte-shuffle copy for long
/// matches with offsets under 16 ([`copy::match_copy`]); the caller
/// guarantees SSSE3 (x86-64) or NEON (AArch64).
#[inline(always)]
#[allow(clippy::too_many_arguments, unsafe_code)]
fn sequences_body<const SHUF: bool>(
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    let mut br = BitReader::new(src)?;
    let tp = t.cells.as_ptr();
    let mut ll = LL_KIND.region + br.read(t.log[0]) as usize;
    br.reload();
    let mut of = OF_KIND.region + br.read(t.log[1]) as usize;
    br.reload();
    let mut ml = ML_KIND.region + br.read(t.log[2]) as usize;
    br.reload();
    let mut reps = *rep;

    // Pointers rather than base + index: fewer live registers. Addresses
    // are compared as integers so a corrupt length never forms an
    // out-of-bounds pointer.
    let base = out.ptr;
    let cap = out.cap;
    // SAFETY: `out.pos <= cap` and `prefix <= out.pos`.
    let (mut op, prefix_start) = unsafe { (base.add(out.pos), base.add(prefix)) };
    let oend_w = base as usize + cap.saturating_sub(WILDCOPY_OVERLENGTH);
    let lit_start = lits.ptr;
    let lit_end = lits.ptr as usize + lits.len;
    let mut lp = lits.ptr;

    let mut n = nb_seq;
    // Fast loop: the sentinel bit reader (`FastBits`), whose reloads have
    // no bounds checks. A sequence reloads at most twice, 8 bytes each, so
    // it runs while 16 bytes remain before the window.
    if n > 0 && br.pos() >= 16 {
        let mut fb = br.fast();
        // SAFETY: `pos() >= 16`.
        let limit = unsafe { br.start().add(16) };
        // 32 bytes of padding put the loop's first instruction on a
        // 64-byte boundary (measured: 2-4% faster than the other three
        // residues); re-check with objdump after changes to the loop entry.
        align_hot_loop!(32);
        while n > 0 && fb.ip >= limit {
            n -= 1;
            let seq = decode_sequence(&mut fb, tp, &mut ll, &mut ml, &mut of, &mut reps, n == 0);
            exec_seq!(SHUF; seq; op, lp; base, cap, prefix_start, oend_w, lit_start, lit_end, lits.len, prefix, dict);
        }
        br = br.resume(fb);
    }
    // The last sequences, with the checked reader.
    while n > 0 {
        n -= 1;
        let seq = decode_sequence(&mut br, tp, &mut ll, &mut ml, &mut of, &mut reps, n == 0);
        exec_seq!(SHUF; seq; op, lp; base, cap, prefix_start, oend_w, lit_start, lit_end, lits.len, prefix, dict);
    }
    *rep = reps;
    if !br.is_end() {
        return Err(Error::Corrupted);
    }
    // Last literals.
    let last = lit_end - lp as usize;
    let o = op as usize - base as usize;
    if last > cap - o {
        return Err(Error::OutputTooSmall);
    }
    // SAFETY: `last` literals remain and the output has room for them.
    unsafe { ptr::copy_nonoverlapping(lp, op, last) };
    out.pos = o + last;
    Ok(())
}

/// Sequences decoded ahead of execution in the prefetching loop
/// (libzstd's `ADVANCED_SEQS`).
const ADVANCED_SEQS: usize = 8;

/// The prefetching loop (libzstd's `ZSTD_decompressSequencesLong_body`):
/// sequences are decoded [`ADVANCED_SEQS`] ahead of their execution, and
/// each one's match source is prefetched when it is decoded, so that the
/// cache misses of long offsets (or a cold dictionary) overlap. Same
/// execution as [`sequences_body`].
#[inline(always)]
#[allow(clippy::too_many_arguments, unsafe_code)]
fn sequences_long_body<const SHUF: bool>(
    t: &SeqTables,
    rep: &mut [usize; 3],
    src: &[u8],
    nb_seq: usize,
    lits: &Literals,
    out: &mut Output,
    prefix: usize,
    dict: &[u8],
) -> Result<()> {
    let mut br = BitReader::new(src)?;
    let tp = t.cells.as_ptr();
    let mut ll = LL_KIND.region + br.read(t.log[0]) as usize;
    br.reload();
    let mut of = OF_KIND.region + br.read(t.log[1]) as usize;
    br.reload();
    let mut ml = ML_KIND.region + br.read(t.log[2]) as usize;
    br.reload();
    let mut reps = *rep;

    let base = out.ptr;
    let cap = out.cap;
    // SAFETY: `out.pos <= cap` and `prefix <= out.pos`.
    let (mut op, prefix_start) = unsafe { (base.add(out.pos), base.add(prefix)) };
    let oend_w = base as usize + cap.saturating_sub(WILDCOPY_OVERLENGTH);
    let lit_start = lits.ptr;
    let lit_end = lits.ptr as usize + lits.len;
    let mut lp = lits.ptr;
    let dict_end = dict.as_ptr().wrapping_add(dict.len());
    // Output position (relative to the prefix) after the sequences decoded
    // so far, for the prefetches.
    let mut pf = out.pos - prefix;

    let mut ring = [Seq {
        lit_len: 0,
        match_len: 0,
        offset: 0,
    }; ADVANCED_SEQS];
    let advance = nb_seq.min(ADVANCED_SEQS);
    let mut i = 0;
    while i < advance {
        let seq = decode_sequence(
            &mut br,
            tp,
            &mut ll,
            &mut ml,
            &mut of,
            &mut reps,
            i + 1 == nb_seq,
        );
        pf = prefetch_match(pf, &seq, prefix_start, dict_end);
        ring[i] = seq;
        i += 1;
    }
    // Decode sequence `i`, execute sequence `i - ADVANCED_SEQS` (whose ring
    // slot it takes). The sentinel bit reader while 16 bytes remain before
    // the window, as in `sequences_body`. (Counting down instead, as
    // `sequences_body` does, measured 3% slower.)
    if i < nb_seq && br.pos() >= 16 {
        let mut fb = br.fast();
        // SAFETY: `pos() >= 16`.
        let limit = unsafe { br.start().add(16) };
        while i < nb_seq && fb.ip >= limit {
            let seq = decode_sequence(
                &mut fb,
                tp,
                &mut ll,
                &mut ml,
                &mut of,
                &mut reps,
                i + 1 == nb_seq,
            );
            pf = prefetch_match(pf, &seq, prefix_start, dict_end);
            let exec = core::mem::replace(&mut ring[i % ADVANCED_SEQS], seq);
            i += 1;
            exec_seq!(SHUF; exec; op, lp; base, cap, prefix_start, oend_w, lit_start, lit_end, lits.len, prefix, dict);
        }
        br = br.resume(fb);
    }
    while i < nb_seq {
        let seq = decode_sequence(
            &mut br,
            tp,
            &mut ll,
            &mut ml,
            &mut of,
            &mut reps,
            i + 1 == nb_seq,
        );
        pf = prefetch_match(pf, &seq, prefix_start, dict_end);
        let exec = core::mem::replace(&mut ring[i % ADVANCED_SEQS], seq);
        i += 1;
        exec_seq!(SHUF; exec; op, lp; base, cap, prefix_start, oend_w, lit_start, lit_end, lits.len, prefix, dict);
    }
    if !br.is_end() {
        return Err(Error::Corrupted);
    }
    // The last `advance` sequences, still in the ring.
    let mut i = nb_seq - advance;
    while i < nb_seq {
        let exec = ring[i % ADVANCED_SEQS];
        i += 1;
        exec_seq!(SHUF; exec; op, lp; base, cap, prefix_start, oend_w, lit_start, lit_end, lits.len, prefix, dict);
    }
    *rep = reps;
    // Last literals.
    let last = lit_end - lp as usize;
    let o = op as usize - base as usize;
    if last > cap - o {
        return Err(Error::OutputTooSmall);
    }
    // SAFETY: `last` literals remain and the output has room for them.
    unsafe { ptr::copy_nonoverlapping(lp, op, last) };
    out.pos = o + last;
    Ok(())
}

/// Exact-size execution near the end of the buffer (libzstd's
/// `ZSTD_execSequenceEnd`). Returns the new output position.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments, unsafe_code)]
fn exec_sequence_end(
    base: *mut u8,
    cap: usize,
    o: usize,
    lit_len: usize,
    match_len: usize,
    offset: usize,
    lit_base: *const u8,
    lit_total: usize,
    lit: &mut usize,
    prefix: usize,
    dict: &[u8],
) -> Result<usize> {
    let total = lit_len.checked_add(match_len).ok_or(Error::Corrupted)?;
    if total > cap - o {
        return Err(Error::OutputTooSmall);
    }
    if lit_len > lit_total - *lit {
        return Err(Error::Corrupted);
    }
    let o_lit_end = o + lit_len;
    let in_prefix = o_lit_end - prefix;
    if offset > in_prefix + dict.len() {
        return Err(Error::Corrupted);
    }
    // SAFETY: the literal and output ranges were checked above; the match
    // source is in the dictionary or before `op` in the output.
    unsafe {
        ptr::copy_nonoverlapping(lit_base.add(*lit), base.add(o), lit_len);
        *lit += lit_len;
        let mut op = base.add(o_lit_end);
        let mut match_len = match_len;
        if offset > in_prefix {
            let back = offset - in_prefix;
            let dp = dict.as_ptr().add(dict.len() - back);
            let n = back.min(match_len);
            ptr::copy_nonoverlapping(dp, op, n);
            op = op.add(n);
            match_len -= n;
            if match_len == 0 {
                return Ok(o + total);
            }
            // The rest starts at the beginning of the prefix.
            let mp = base.add(prefix);
            overlapping_copy(op, mp, match_len);
        } else {
            let mp = op.sub(offset);
            overlapping_copy(op, mp, match_len);
        }
    }
    Ok(o + total)
}

/// Forward copy where `src` may be less than `len` bytes before `dst`
/// (the copy repeats the pattern, like LZ77).
#[inline]
#[allow(unsafe_code)]
unsafe fn overlapping_copy(dst: *mut u8, src: *const u8, len: usize) {
    let dist = dst as usize - src as usize;
    // SAFETY: per the caller, `src < dst` and both ranges are in bounds.
    unsafe {
        if dist >= len {
            ptr::copy_nonoverlapping(src, dst, len);
        } else {
            for i in 0..len {
                *dst.add(i) = *src.add(i);
            }
        }
    }
}

/// A match that starts in the dictionary (rare: kept out of the hot loop
/// so the dictionary's pointer and length don't take registers there).
/// The output has `WILDCOPY_OVERLENGTH` writable bytes after the match
/// (the fast path of `exec_seq!`).
#[cold]
#[inline(never)]
#[allow(unsafe_code)]
fn dict_match(
    mop: *mut u8,
    offset: usize,
    in_prefix: usize,
    match_len: usize,
    prefix_start: *mut u8,
    dict: &[u8],
) -> Result<()> {
    if offset > in_prefix + dict.len() {
        return Err(Error::Corrupted);
    }
    let back = offset - in_prefix;
    if back >= match_len + WILDCOPY_OVERLENGTH {
        // All in the dictionary, with `WILDCOPY_OVERLENGTH` readable bytes
        // after it: copied in 16-byte chunks rather than by a `memcpy`
        // call (libzstd's `ZSTD_execSequence` calls `memmove`). Most
        // matches of a small frame are in its dictionary.
        // SAFETY: `back <= dict.len()` and the bytes read and written past
        // the match are in the dictionary and in the output's slack; the
        // dictionary is another buffer than the output written (or window
        // history after the output's capacity).
        unsafe { copy::wildcopy(mop, dict.as_ptr().add(dict.len() - back), match_len) };
        return Ok(());
    }
    let n = back.min(match_len);
    // SAFETY: `back <= dict.len()`; the caller checked the output has room
    // for the match; the rest of the match starts at `prefix_start`, which
    // is `offset` bytes before its destination.
    unsafe {
        ptr::copy_nonoverlapping(dict.as_ptr().add(dict.len() - back), mop, n);
        if match_len > n {
            overlapping_copy(mop.add(n), prefix_start, match_len - n);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Whether `copy::match_copy::<true>` may run here.
    fn shuffle_ok() -> bool {
        #[cfg(all(target_arch = "x86_64", not(feature = "portable"), feature = "std"))]
        return std::is_x86_feature_detected!("ssse3");
        // AArch64 (NEON baseline), or the scalar stand-in.
        #[allow(unreachable_code)]
        true
    }

    /// Every offset 1-31 and lengths around the 8/16/32-byte steps, with
    /// exactly `WILDCOPY_OVERLENGTH` bytes of room after the match (then
    /// guard bytes that must stay untouched) and the source at the start
    /// of the buffer.
    fn check_match_copy<const SHUF: bool>() {
        const GUARD: usize = 16;
        let lens: &[usize] = if cfg!(miri) {
            &[1, 3, 8, 9, 16, 17, 33]
        } else {
            &[
                1, 2, 3, 4, 5, 7, 8, 9, 10, 15, 16, 17, 18, 23, 24, 25, 31, 32, 33, 34, 47, 48, 49,
                63, 64, 65, 100, 255,
            ]
        };
        for offset in 1..32 {
            for &len in lens {
                let total = offset + len + WILDCOPY_OVERLENGTH + GUARD;
                let mut buf = vec![0xA5u8; total];
                for (i, b) in buf[..offset].iter_mut().enumerate() {
                    *b = (i * 37 + offset * 11 + 1) as u8;
                }
                let mut want = buf[..offset].to_vec();
                for i in 0..len {
                    want.push(want[i]);
                }
                // SAFETY: `offset` written bytes precede the match and
                // `len + WILDCOPY_OVERLENGTH` bytes follow it.
                unsafe { copy::match_copy::<SHUF>(buf.as_mut_ptr().add(offset), offset, len) };
                assert!(
                    buf[..offset + len] == want[..],
                    "offset {offset} len {len} shuffle {SHUF}"
                );
                assert!(
                    buf[total - GUARD..].iter().all(|&b| b == 0xA5),
                    "offset {offset} len {len} shuffle {SHUF}: wrote past the slack"
                );
            }
        }
    }

    #[test]
    fn match_copy_scalar() {
        check_match_copy::<false>();
    }

    #[test]
    fn match_copy_shuffle() {
        if shuffle_ok() {
            check_match_copy::<true>();
        }
    }

    /// Text-like data with repeats near and far.
    fn text(len: usize) -> Vec<u8> {
        let words: [&[u8]; 8] = [
            b"zstd ",
            b"window ",
            b"prefetch ",
            b"the ",
            b"sequence ",
            b"of ",
            b"long ",
            b"\n",
        ];
        let mut v = Vec::with_capacity(len + 16);
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        while v.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if x.is_multiple_of(7) && v.len() > 100 {
                // Copy an earlier stretch.
                let from = (x >> 20) as usize % (v.len() - 50);
                let n = 8 + (x >> 40) as usize % 40;
                for i in 0..n {
                    v.push(v[from + i]);
                }
            } else {
                v.extend_from_slice(words[(x >> 33) as usize % 8]);
                v.push((x >> 50) as u8);
            }
        }
        v.truncate(len);
        v
    }

    /// Forces the sequence loop as [`set_seq_loop`] does, for this thread
    /// only (the tests run in parallel).
    fn set_thread_seq_loop(long: Option<bool>) {
        let v = match long {
            None => LOOP_AUTO,
            Some(true) => LOOP_LONG,
            Some(false) => LOOP_SHORT,
        };
        THREAD_SEQ_LOOP.with(|c| c.set(v));
    }

    /// The prefetching loop decodes what the fused loop does: blocks of 1
    /// to many sequences (ending with the ring partly full), and matches
    /// reaching into a dictionary.
    #[test]
    fn prefetch_loop_matches_fused() {
        let data = text(if cfg!(miri) { 3000 } else { 300_000 });
        let sizes: &[usize] = if cfg!(miri) {
            &[30, 400, 3000]
        } else {
            &[1, 30, 60, 100, 200, 400, 1000, 3000, 20_000, 300_000]
        };
        let levels: &[i32] = if cfg!(miri) { &[1] } else { &[1, 3, 19] };
        let dict = text(if cfg!(miri) { 500 } else { 20_000 });
        for &n in sizes {
            let src = &data[data.len() - n..];
            for &level in levels {
                let plain = crate::compress(src, level).unwrap();
                let with_dict = crate::compress_with_dictionary(src, &dict, level).unwrap();
                for long in [false, true] {
                    set_thread_seq_loop(Some(long));
                    let got = crate::decompress(&plain).unwrap();
                    assert!(got == src, "{n} bytes L{level} long={long}");
                    let mut d = crate::Decompressor::with_dictionary(crate::Dictionary::raw(&dict));
                    let got = d.decompress(&with_dict).unwrap();
                    assert!(got == src, "{n} bytes L{level} dict long={long}");
                    // Too small an output: an error, not a write past it.
                    let mut buf = vec![0u8; n - 1];
                    assert!(d.decompress_to_slice(&with_dict, &mut buf).is_err());
                }
            }
        }
        set_thread_seq_loop(None);
    }
}
