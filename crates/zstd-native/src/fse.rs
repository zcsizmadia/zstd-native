//! Finite State Entropy: normalized counts, decoding tables, and the
//! sequence (literal length / match length / offset) tables.

use crate::bits::{BitReader, Status};
use crate::cpu::Features;
use crate::error::{Error, Result};

pub(crate) const FSE_MIN_TABLELOG: u32 = 5;
pub(crate) const FSE_TABLELOG_ABSOLUTE_MAX: u32 = 15;

pub(crate) const MAX_LL: usize = 35;
pub(crate) const MAX_ML: usize = 52;
pub(crate) const MAX_OFF: usize = 31;
pub(crate) const LL_FSE_LOG: u32 = 9;
pub(crate) const ML_FSE_LOG: u32 = 9;
pub(crate) const OFF_FSE_LOG: u32 = 8;

#[inline(always)]
const fn highbit32(v: u32) -> u32 {
    31 - v.leading_zeros()
}

/// `src[i..i + 4]` as little-endian, zero past the end.
#[inline(always)]
fn read_le32_padded(src: &[u8], i: usize) -> u32 {
    if i + 4 <= src.len() {
        u32::from_le_bytes(src[i..i + 4].try_into().unwrap())
    } else {
        let mut b = [0u8; 4];
        if i < src.len() {
            b[..src.len() - i].copy_from_slice(&src[i..]);
        }
        u32::from_le_bytes(b)
    }
}

/// Parses an FSE table description (libzstd's `FSE_readNCount`).
///
/// Fills `norm[..=max_symbol]` (missing symbols get 0) and returns
/// `(max_symbol_used, table_log, bytes_consumed)`.
pub(crate) fn read_ncount(
    src: &[u8],
    max_symbol: usize,
    norm: &mut [i16],
) -> Result<(usize, u32, usize)> {
    let max_sv1 = max_symbol + 1;
    norm[..max_sv1].fill(0);
    let mut bitpos: usize = 0;
    let bits = |bitpos: usize| read_le32_padded(src, bitpos >> 3) >> (bitpos & 7);

    let table_log = (bits(0) & 0xF) + FSE_MIN_TABLELOG;
    if table_log > FSE_TABLELOG_ABSOLUTE_MAX {
        return Err(Error::Corrupted);
    }
    bitpos += 4;
    let mut remaining: i32 = (1 << table_log) + 1;
    let mut threshold: i32 = 1 << table_log;
    let mut nb_bits: u32 = table_log + 1;
    let mut sym: usize = 0;
    let mut previous0 = false;

    loop {
        if previous0 {
            // Runs of zero-probability symbols: 2-bit repeat codes, 3 = more.
            loop {
                let r = (bits(bitpos) & 3) as usize;
                bitpos += 2;
                sym += r;
                if r != 3 {
                    break;
                }
            }
            if sym >= max_sv1 {
                break;
            }
        }
        let max = 2 * threshold - 1 - remaining;
        let v = bits(bitpos);
        let mut count: i32;
        if ((v & (threshold as u32 - 1)) as i32) < max {
            count = (v & (threshold as u32 - 1)) as i32;
            bitpos += nb_bits as usize - 1;
        } else {
            count = (v & (2 * threshold as u32 - 1)) as i32;
            if count >= threshold {
                count -= max;
            }
            bitpos += nb_bits as usize;
        }
        count -= 1;
        if count >= 0 {
            remaining -= count;
        } else {
            remaining += count;
        }
        norm[sym] = count as i16;
        sym += 1;
        previous0 = count == 0;
        if remaining < threshold {
            if remaining <= 1 {
                break;
            }
            nb_bits = highbit32(remaining as u32) + 1;
            threshold = 1 << (nb_bits - 1);
        }
        if sym >= max_sv1 {
            break;
        }
    }
    if remaining != 1 || sym > max_sv1 {
        return Err(Error::Corrupted);
    }
    let consumed = bitpos.div_ceil(8);
    if consumed > src.len() {
        return Err(Error::Corrupted);
    }
    Ok((sym - 1, table_log, consumed))
}

/// libzstd's `FSE_TABLESTEP`: the stride of the symbol spread.
#[inline(always)]
pub(crate) const fn table_step(table_size: u32) -> u32 {
    (table_size >> 1) + (table_size >> 3) + 3
}

/// The inverse of [`table_step`] modulo `table_size` (meta/zstd#4836's
/// `FSE_invTableStep`). The step is odd for every table of 16 states or
/// more, so the spread without low-probability symbols, which puts the
/// `i`-th symbol of the sorted layout at `(i * step) & mask`, is a
/// permutation: cell `u` holds sorted symbol `(u * inv) & mask`. Walking
/// the cells in order and reading the sorted layout with stride `inv`
/// fuses the spread with the table build (no scattered writes, no
/// read-back), with the same table.
#[inline(always)]
pub(crate) const fn inv_table_step(table_size: u32) -> u32 {
    let step = table_step(table_size);
    // odd * odd == 1 (mod 8): 3 correct low bits; each Newton step doubles
    // them (6, 12, 24 >= FSE_TABLELOG_ABSOLUTE_MAX).
    let mut inv = step;
    inv = inv.wrapping_mul(2u32.wrapping_sub(step.wrapping_mul(inv)));
    inv = inv.wrapping_mul(2u32.wrapping_sub(step.wrapping_mul(inv)));
    inv = inv.wrapping_mul(2u32.wrapping_sub(step.wrapping_mul(inv)));
    debug_assert!(step.wrapping_mul(inv) & (table_size - 1) == 1);
    inv & (table_size - 1)
}

/// Lays the symbols out over the table (libzstd's spread), writing the
/// symbol of every cell to `cells` and each symbol's first state to `next`.
const fn spread(norm: &[i16], max_sv1: usize, table_log: u32, cells: &mut [u8], next: &mut [u16]) {
    let table_size = 1usize << table_log;
    let mask = table_size - 1;
    let step = (table_size >> 1) + (table_size >> 3) + 3;
    let mut high = table_size - 1;
    let mut s = 0;
    while s < max_sv1 {
        if norm[s] == -1 {
            cells[high] = s as u8;
            high = high.wrapping_sub(1);
            next[s] = 1;
        } else {
            next[s] = norm[s] as u16;
        }
        s += 1;
    }
    let mut position = 0usize;
    s = 0;
    while s < max_sv1 {
        let n = norm[s];
        let mut i = 0;
        while i < n {
            cells[position] = s as u8;
            position = (position + step) & mask;
            while position > high {
                position = (position + step) & mask;
            }
            i += 1;
        }
        s += 1;
    }
}

// ---------------------------------------------------------------------------
// Plain FSE (Huffman weights)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default, Debug)]
struct FseEntry {
    new_state: u16,
    symbol: u8,
    nb_bits: u8,
}

const WEIGHT_MAX_LOG: u32 = 6;

/// Decodes the FSE-compressed Huffman weights (libzstd's
/// `FSE_decompress_wksp` with `maxLog` 6), returning how many weights were
/// written to `out` (at most 255).
pub(crate) fn decompress_weights(src: &[u8], out: &mut [u8; 256]) -> Result<usize> {
    let mut norm = [0i16; 256];
    let (max_sym, table_log, hsize) = read_ncount(src, 255, &mut norm)?;
    if table_log > WEIGHT_MAX_LOG {
        return Err(Error::Corrupted);
    }
    let table_size = 1usize << table_log;
    let mut cells = [0u8; 1 << WEIGHT_MAX_LOG];
    let mut next = [0u16; 256];
    spread(&norm, max_sym + 1, table_log, &mut cells, &mut next);
    let mut dt = [FseEntry::default(); 1 << WEIGHT_MAX_LOG];
    for u in 0..table_size {
        let s = cells[u] as usize;
        let ns = next[s] as u32;
        next[s] += 1;
        let nb = table_log - highbit32(ns);
        dt[u] = FseEntry {
            symbol: s as u8,
            nb_bits: nb as u8,
            new_state: ((ns << nb) - table_size as u32) as u16,
        };
    }

    let mut br = BitReader::new(&src[hsize..])?;
    let mut s1 = br.read(table_log) as usize;
    br.reload_short();
    let mut s2 = br.read(table_log) as usize;
    br.reload_short();
    if br.reload_short() == Status::Overflow {
        return Err(Error::Corrupted);
    }
    let dec = |state: &mut usize, br: &mut BitReader<'_>| -> u8 {
        let e = dt[*state];
        *state = e.new_state as usize + br.read(u32::from(e.nb_bits)) as usize;
        e.symbol
    };
    const OMAX: usize = 255;
    let mut n = 0usize;
    // libzstd's loop, kept as is: where it stops decides the weight count.
    while (br.reload_short() == Status::Unfinished) & (n < OMAX - 3) {
        out[n] = dec(&mut s1, &mut br);
        out[n + 1] = dec(&mut s2, &mut br);
        out[n + 2] = dec(&mut s1, &mut br);
        out[n + 3] = dec(&mut s2, &mut br);
        n += 4;
    }
    loop {
        if n > OMAX - 2 {
            return Err(Error::Corrupted);
        }
        out[n] = dec(&mut s1, &mut br);
        n += 1;
        if br.reload_short() == Status::Overflow {
            out[n] = dec(&mut s2, &mut br);
            n += 1;
            break;
        }
        if n > OMAX - 2 {
            return Err(Error::Corrupted);
        }
        out[n] = dec(&mut s2, &mut br);
        n += 1;
        if br.reload_short() == Status::Overflow {
            out[n] = dec(&mut s1, &mut br);
            n += 1;
            break;
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Sequence tables
// ---------------------------------------------------------------------------

/// One cell of a sequence decoding table (libzstd's `ZSTD_seqSymbol`).
#[derive(Clone, Copy, Default, Debug)]
#[repr(C, align(8))]
pub(crate) struct SeqSymbol {
    pub next_state: u16,
    pub nb_add_bits: u8,
    pub nb_bits: u8,
    pub base: u32,
}

const ZERO: SeqSymbol = SeqSymbol {
    next_state: 0,
    nb_add_bits: 0,
    nb_bits: 0,
    base: 0,
};

pub(crate) const LL_BASE: [u32; MAX_LL + 1] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    0x80, 0x100, 0x200, 0x400, 0x800, 0x1000, 0x2000, 0x4000, 0x8000, 0x10000,
];
pub(crate) const LL_BITS: [u8; MAX_LL + 1] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
pub(crate) const ML_BASE: [u32; MAX_ML + 1] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 0x83, 0x103, 0x203,
    0x403, 0x803, 0x1003, 0x2003, 0x4003, 0x8003, 0x10003,
];
pub(crate) const ML_BITS: [u8; MAX_ML + 1] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
/// Offset codes: `base + bits` is the offset value minus 3 for codes >= 2
/// (libzstd's `OF_base`), and codes 0/1 are repeat offsets.
pub(crate) const OF_BASE: [u32; MAX_OFF + 1] = {
    let mut t = [0u32; MAX_OFF + 1];
    t[1] = 1;
    let mut i = 2;
    while i <= MAX_OFF {
        t[i] = (1u32 << i) - 3;
        i += 1;
    }
    t
};
pub(crate) const OF_BITS: [u8; MAX_OFF + 1] = {
    let mut t = [0u8; MAX_OFF + 1];
    let mut i = 0;
    while i <= MAX_OFF {
        t[i] = i as u8;
        i += 1;
    }
    t
};

pub(crate) const LL_DEFAULT_NORM: [i16; MAX_LL + 1] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
pub(crate) const ML_DEFAULT_NORM: [i16; MAX_ML + 1] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
pub(crate) const OF_DEFAULT_NORM: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

/// Builds a sequence decoding table (libzstd's `ZSTD_buildFSETable`) whose
/// `next_state` values include `state_offset`, the table's position in the
/// combined table ([`SEQ_TABLE_SIZE`]).
pub(crate) const fn build_seq_table(
    dt: &mut [SeqSymbol],
    norm: &[i16],
    max_sv1: usize,
    table_log: u32,
    base: &[u32],
    bits: &[u8],
    state_offset: u16,
) {
    let table_size = 1usize << table_log;
    let mut cells = [0u8; 1 << LL_FSE_LOG];
    let mut next = [0u16; MAX_ML + 1];
    spread(norm, max_sv1, table_log, &mut cells, &mut next);
    let mut u = 0;
    while u < table_size {
        let s = cells[u] as usize;
        let ns = next[s] as u32;
        next[s] += 1;
        let nb = table_log - highbit32(ns);
        dt[u] = SeqSymbol {
            next_state: ((ns << nb) - table_size as u32) as u16 + state_offset,
            nb_add_bits: bits[s],
            nb_bits: nb as u8,
            base: base[s],
        };
        u += 1;
    }
}

/// Run-time version of [`build_seq_table`] with libzstd's fast spread for
/// tables without low-probability (-1) symbols: lay the symbols down in
/// order 8 bytes at a time, then build the cells in order, reading that
/// layout with the inverse stride ([`inv_table_step`], meta/zstd#4836).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_seq_table_fast(
    dt: &mut [SeqSymbol],
    norm: &[i16],
    max_sv1: usize,
    table_log: u32,
    base: &[u32],
    bits: &[u8],
    state_offset: u16,
    cpu: Features,
) {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    if cpu.bmi2 {
        // SAFETY: the caller detected BMI2 (and BMI1/LZCNT) on this CPU.
        #[allow(unsafe_code)]
        unsafe {
            build_fast_bmi2(dt, norm, max_sv1, table_log, base, bits, state_offset)
        };
        return;
    }
    let _ = cpu;
    build_fast_body(dt, norm, max_sv1, table_log, base, bits, state_offset);
}

/// With LZCNT for the per-cell `highbit`.
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[target_feature(enable = "bmi1,bmi2,lzcnt")]
fn build_fast_bmi2(
    dt: &mut [SeqSymbol],
    norm: &[i16],
    max_sv1: usize,
    table_log: u32,
    base: &[u32],
    bits: &[u8],
    state_offset: u16,
) {
    build_fast_body(dt, norm, max_sv1, table_log, base, bits, state_offset);
}

#[inline(always)]
fn build_fast_body(
    dt: &mut [SeqSymbol],
    norm: &[i16],
    max_sv1: usize,
    table_log: u32,
    base: &[u32],
    bits: &[u8],
    state_offset: u16,
) {
    let norm = &norm[..max_sv1];
    if norm.contains(&-1) {
        build_seq_table(dt, norm, max_sv1, table_log, base, bits, state_offset);
        return;
    }
    let table_size = 1usize << table_log;
    let mask = table_size - 1;
    let mut spread = [0u8; (1 << LL_FSE_LOG) + 8];
    let mut pos = 0usize;
    for (s, &n) in norm.iter().enumerate() {
        let n = n as usize;
        let v = (s as u64 * 0x0101_0101_0101_0101).to_le_bytes();
        let mut i = 0;
        loop {
            spread[pos + i..pos + i + 8].copy_from_slice(&v);
            i += 8;
            if i >= n {
                break;
            }
        }
        pos += n;
    }
    debug_assert_eq!(pos, table_size);
    // 64-entry local tables indexed with `s & 63` (every code is under 53):
    // no bounds checks in the per-cell loop.
    let mut next = [0u16; 64];
    let mut sym_base = [0u32; 64];
    let mut sym_bits = [0u8; 64];
    for (s, &n) in norm.iter().enumerate() {
        next[s] = n as u16;
        sym_base[s] = base[s];
        sym_bits[s] = bits[s];
    }
    // Fused spread and build (meta/zstd#4836): cell `u` holds the sorted
    // symbol at `(u * inv) & mask` (see `inv_table_step`).
    let inv = inv_table_step(table_size as u32) as usize;
    let mut si = 0usize;
    for d in dt[..table_size].iter_mut() {
        // `si <= mask < 512`: the `& 511` only drops the bounds check.
        let s = (spread[si & ((1 << LL_FSE_LOG) - 1)] & 63) as usize;
        si = (si + inv) & mask;
        let ns = u32::from(next[s]);
        next[s] += 1;
        let nb = table_log - highbit32(ns);
        *d = SeqSymbol {
            next_state: ((ns << nb) - table_size as u32) as u16 + state_offset,
            nb_add_bits: sym_bits[s],
            nb_bits: nb as u8,
            base: sym_base[s],
        };
    }
    debug_assert_eq!(si, 0, "visited every cell");
}

/// The three tables live in one array, so the sequence loop needs a single
/// base pointer: literal lengths, then offsets, then match lengths. Each
/// region always holds its code's current table, which makes "repeat"
/// mode free.
pub(crate) const LL_REGION: usize = 0;
pub(crate) const OF_REGION: usize = 1 << LL_FSE_LOG;
pub(crate) const ML_REGION: usize = OF_REGION + (1 << OFF_FSE_LOG);
pub(crate) const SEQ_TABLE_SIZE: usize = ML_REGION + (1 << ML_FSE_LOG);

const fn default_table<const N: usize>(
    norm: &[i16],
    log: u32,
    base: &[u32],
    bits: &[u8],
    region: usize,
) -> [SeqSymbol; N] {
    let mut t = [ZERO; N];
    build_seq_table(&mut t, norm, norm.len(), log, base, bits, region as u16);
    t
}

static LL_DEFAULT: [SeqSymbol; 64] =
    default_table(&LL_DEFAULT_NORM, 6, &LL_BASE, &LL_BITS, LL_REGION);
static OF_DEFAULT: [SeqSymbol; 32] =
    default_table(&OF_DEFAULT_NORM, 5, &OF_BASE, &OF_BITS, OF_REGION);
static ML_DEFAULT: [SeqSymbol; 64] =
    default_table(&ML_DEFAULT_NORM, 6, &ML_BASE, &ML_BITS, ML_REGION);

/// Kind of code: limits, values, its region and predefined table.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CodeKind {
    pub max: usize,
    pub max_log: u32,
    pub base: &'static [u32],
    pub bits: &'static [u8],
    pub region: usize,
    pub default: &'static [SeqSymbol],
    pub default_log: u32,
    /// Index in [`SeqTables::log`].
    pub index: usize,
}

pub(crate) const LL_KIND: CodeKind = CodeKind {
    max: MAX_LL,
    max_log: LL_FSE_LOG,
    base: &LL_BASE,
    bits: &LL_BITS,
    region: LL_REGION,
    default: &LL_DEFAULT,
    default_log: 6,
    index: 0,
};
pub(crate) const OF_KIND: CodeKind = CodeKind {
    max: MAX_OFF,
    max_log: OFF_FSE_LOG,
    base: &OF_BASE,
    bits: &OF_BITS,
    region: OF_REGION,
    default: &OF_DEFAULT,
    default_log: 5,
    index: 1,
};
pub(crate) const ML_KIND: CodeKind = CodeKind {
    max: MAX_ML,
    max_log: ML_FSE_LOG,
    base: &ML_BASE,
    bits: &ML_BITS,
    region: ML_REGION,
    default: &ML_DEFAULT,
    default_log: 6,
    index: 2,
};

/// The current literal length, offset and match length tables.
#[derive(Clone)]
pub(crate) struct SeqTables {
    pub cells: [SeqSymbol; SEQ_TABLE_SIZE],
    /// Table logs, in `CodeKind::index` order.
    pub log: [u32; 3],
}

impl core::fmt::Debug for SeqTables {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SeqTables")
            .field("log", &self.log)
            .finish_non_exhaustive()
    }
}

impl SeqTables {
    pub(crate) const fn new() -> Self {
        SeqTables {
            cells: [ZERO; SEQ_TABLE_SIZE],
            log: [0; 3],
        }
    }

    /// Installs the predefined table for `kind`.
    pub(crate) fn set_default(&mut self, kind: CodeKind) {
        self.cells[kind.region..kind.region + kind.default.len()].copy_from_slice(kind.default);
        self.log[kind.index] = kind.default_log;
    }

    /// Builds `kind`'s table from normalized counts.
    pub(crate) fn build(
        &mut self,
        kind: CodeKind,
        norm: &[i16],
        max_sv1: usize,
        log: u32,
        cpu: Features,
    ) {
        let region = &mut self.cells[kind.region..kind.region + (1 << kind.max_log)];
        build_seq_table_fast(
            region,
            norm,
            max_sv1,
            log,
            kind.base,
            kind.bits,
            kind.region as u16,
            cpu,
        );
        self.log[kind.index] = log;
    }

    /// Reads one table description of the sequences header (libzstd's
    /// `ZSTD_buildSeqTable`) and installs it. Returns the bytes used.
    pub(crate) fn read(
        &mut self,
        mode: u8,
        kind: CodeKind,
        src: &[u8],
        repeat_ok: bool,
        cpu: Features,
    ) -> Result<usize> {
        match mode {
            // Predefined
            0 => {
                self.set_default(kind);
                Ok(0)
            }
            // RLE
            1 => {
                let &sym = src.first().ok_or(Error::Truncated)?;
                let sym = sym as usize;
                if sym > kind.max {
                    return Err(Error::Corrupted);
                }
                self.cells[kind.region] = SeqSymbol {
                    next_state: kind.region as u16,
                    nb_add_bits: kind.bits[sym],
                    nb_bits: 0,
                    base: kind.base[sym],
                };
                self.log[kind.index] = 0;
                Ok(1)
            }
            // FSE compressed
            2 => {
                let mut norm = [0i16; MAX_ML + 1];
                let (max_sym, log, used) = read_ncount(src, kind.max, &mut norm)?;
                if log > kind.max_log {
                    return Err(Error::Corrupted);
                }
                self.build(kind, &norm, max_sym + 1, log, cpu);
                Ok(used)
            }
            // Repeat: the region still holds the previous table.
            _ => {
                if !repeat_ok {
                    return Err(Error::Corrupted);
                }
                Ok(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tables_match_rfc() {
        // RFC 8878 appendix A, first rows of each predefined table.
        let ll = &LL_DEFAULT;
        assert_eq!((ll[0].base, ll[0].nb_bits, ll[0].next_state), (0, 4, 0));
        assert_eq!((ll[1].base, ll[1].nb_bits, ll[1].next_state), (0, 4, 16));
        assert_eq!((ll[2].base, ll[2].nb_bits, ll[2].next_state), (1, 5, 32));
        assert_eq!((ll[63].base, ll[63].nb_bits), (0x2000, 6));
        let of = &OF_DEFAULT;
        let off = OF_REGION as u16;
        assert_eq!(
            (of[0].nb_add_bits, of[0].nb_bits, of[0].next_state),
            (0, 5, off)
        );
        assert_eq!(
            (of[1].nb_add_bits, of[1].nb_bits, of[1].next_state),
            (6, 4, off)
        );
        let ml = &ML_DEFAULT;
        let off = ML_REGION as u16;
        assert_eq!((ml[0].base, ml[0].nb_bits, ml[0].next_state), (3, 6, off));
        assert_eq!((ml[1].base, ml[1].nb_bits, ml[1].next_state), (4, 4, off));
    }
}

#[cfg(test)]
mod build_tests {
    use super::*;

    /// The fast spread must give exactly the reference table.
    #[test]
    #[allow(clippy::needless_range_loop)]
    fn fast_build_matches_reference() {
        let mut r = 12345u64;
        for case in 0..2000 {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            let log = 5 + (r % 5) as u32;
            let max_sv1 = 2 + (r >> 8) as usize % (MAX_ML - 1);
            // Random positive distribution summing to 1 << log (some -1s when odd case).
            let mut norm = [0i16; MAX_ML + 1];
            let mut left = 1i32 << log;
            let low = case % 3 == 0;
            for s in 0..max_sv1 {
                r ^= r << 13;
                r ^= r >> 7;
                r ^= r << 17;
                if left <= 0 {
                    break;
                }
                if low && s % 7 == 3 {
                    norm[s] = -1;
                    left -= 1;
                    continue;
                }
                let v = if s + 1 == max_sv1 {
                    left
                } else {
                    (r % (left as u64 / 2 + 1)) as i32
                };
                norm[s] = v as i16;
                left -= v;
            }
            if left > 0 {
                norm[0] += left as i16;
            }
            if norm[0] < 0 {
                continue;
            }
            let mut a = [ZERO; 512];
            let mut b = [ZERO; 512];
            build_seq_table(&mut a, &norm, max_sv1, log, &ML_BASE, &ML_BITS, 7);
            build_seq_table_fast(
                &mut b,
                &norm,
                max_sv1,
                log,
                &ML_BASE,
                &ML_BITS,
                7,
                Features::detect(),
            );
            for u in 0..1 << log {
                assert_eq!(
                    (a[u].next_state, a[u].nb_bits, a[u].base, a[u].nb_add_bits),
                    (b[u].next_state, b[u].nb_bits, b[u].base, b[u].nb_add_bits),
                    "case {case} cell {u}"
                );
            }
        }
    }

    /// `inv_table_step` inverts the spread stride for every table log.
    #[test]
    fn inv_table_step_inverts_step() {
        for log in FSE_MIN_TABLELOG..=FSE_TABLELOG_ABSOLUTE_MAX {
            let size = 1u32 << log;
            let inv = inv_table_step(size);
            assert!(inv < size);
            assert_eq!(
                table_step(size).wrapping_mul(inv) & (size - 1),
                1,
                "log {log}"
            );
        }
    }

    /// The fused build gives the reference table for every table log of
    /// each code kind, over many distributions (flat, skewed, with and
    /// without -1 symbols; meta/zstd#4836's `FUZ_genNormalizedCount`).
    #[test]
    fn fused_build_matches_reference_every_kind() {
        let mut r = 0x9E37_79B9_7F4A_7C15u64;
        let mut rand = move |n: u64| {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            (r >> 16) % n
        };
        for kind in [LL_KIND, OF_KIND, ML_KIND] {
            for log in FSE_MIN_TABLELOG..=kind.max_log {
                for low in [false, true] {
                    for case in 0..if cfg!(miri) { 2 } else { 200 } {
                        let max_sv1 = 2 + rand(kind.max as u64) as usize;
                        let mut norm = [0i16; MAX_ML + 1];
                        let mut remaining = 1u32 << log;
                        if low {
                            for n in norm[1..max_sv1].iter_mut() {
                                if remaining > 1 && rand(4) == 0 {
                                    *n = -1;
                                    remaining -= 1;
                                }
                            }
                        }
                        let hot = 1 + rand(max_sv1 as u64);
                        while remaining > 0 {
                            let s = if rand(2) == 0 {
                                rand(hot)
                            } else {
                                rand(max_sv1 as u64)
                            } as usize;
                            if norm[s] >= 0 {
                                norm[s] += 1;
                                remaining -= 1;
                            }
                        }
                        let off = kind.region as u16;
                        let mut a = [ZERO; 512];
                        let mut b = [ZERO; 512];
                        build_seq_table(&mut a, &norm, max_sv1, log, kind.base, kind.bits, off);
                        build_seq_table_fast(
                            &mut b,
                            &norm,
                            max_sv1,
                            log,
                            kind.base,
                            kind.bits,
                            off,
                            Features::detect(),
                        );
                        for u in 0..1 << log {
                            assert_eq!(
                                (a[u].next_state, a[u].nb_bits, a[u].base, a[u].nb_add_bits),
                                (b[u].next_state, b[u].nb_bits, b[u].base, b[u].nb_add_bits),
                                "log {log} low {low} case {case} cell {u}"
                            );
                        }
                    }
                }
            }
        }
    }
}
