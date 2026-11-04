//! Forward bit writer (libzstd's `BIT_CStream_t`, common/bitstream.h).
//!
//! Bits are added LSB-first to a 64-bit container and written to memory
//! by explicit flushes, 8 bytes at a time (only the complete bytes count;
//! the rest is overwritten by the next flush). The stream is read backward
//! by [`crate::bits::BitReader`], starting from the end mark that
//! [`BitWriter::close`] adds.
//!
//! The container is always 64 bits, as in a 64-bit libzstd build, so the
//! overflow threshold (`capacity - 8`) is the same as libzstd on x86-64 /
//! AArch64 on every target.
//!
//! Like libzstd, a write past the end of the buffer is not reported when
//! it happens: the write position is clamped and [`BitWriter::close`]
//! returns 0 ("didn't fit"). Unlike libzstd, [`BitWriter::flush_fast`]
//! clamps as well (one conditional move), so no sequence of calls can write
//! out of bounds; the output is the same.

/// libzstd's `BIT_mask`: the low `n` bits, `n < 32`.
pub(crate) const BIT_MASK: [u32; 32] = {
    let mut m = [0u32; 32];
    let mut i = 0;
    while i < 32 {
        m[i] = ((1u64 << i) - 1) as u32;
        i += 1;
    }
    m
};

/// libzstd's `BIT_CStream_t`.
///
/// Invariant: `pos <= end` and `end + 8 == dst.len()`, so the 8-byte
/// write at `pos` is always inside `dst`.
#[derive(Debug)]
pub(crate) struct BitWriter<'a> {
    container: u64,
    bit_pos: u32,
    pos: usize,
    end: usize,
    dst: &'a mut [u8],
}

impl<'a> BitWriter<'a> {
    /// libzstd's `BIT_initCStream`: `None` when `dst.len() <= 8`
    /// (libzstd's `dstSize_tooSmall`; libzstd's callers turn it into a
    /// "not compressible" 0 or a `dstSize_tooSmall` error).
    #[inline]
    pub(crate) fn new(dst: &'a mut [u8]) -> Option<Self> {
        if dst.len() <= 8 {
            return None;
        }
        Some(BitWriter {
            container: 0,
            bit_pos: 0,
            pos: 0,
            end: dst.len() - 8,
            dst,
        })
    }

    /// libzstd's `BIT_addBits`: adds the low `nb_bits` (`< 32`) of `value`.
    /// The container must have room (at most 63 bits pending).
    #[inline(always)]
    pub(crate) fn add_bits(&mut self, value: u64, nb_bits: u32) {
        debug_assert!(nb_bits < 32);
        debug_assert!(nb_bits + self.bit_pos < 64);
        self.container |= (value & u64::from(BIT_MASK[(nb_bits & 31) as usize])) << self.bit_pos;
        self.bit_pos += nb_bits;
    }

    /// libzstd's `BIT_addBitsFast`: `value` must have no bits at or above
    /// `nb_bits`.
    #[inline(always)]
    pub(crate) fn add_bits_fast(&mut self, value: u64, nb_bits: u32) {
        debug_assert!(nb_bits >= 64 || (value >> nb_bits) == 0);
        debug_assert!(nb_bits + self.bit_pos < 64);
        self.container |= value << self.bit_pos;
        self.bit_pos += nb_bits;
    }

    /// libzstd's `BIT_flushBits`: writes the complete bytes; on overflow
    /// the position sticks at the end and [`close`](Self::close) returns 0.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) fn flush(&mut self) {
        let nb_bytes = (self.bit_pos >> 3) as usize;
        debug_assert!(self.bit_pos < 64);
        debug_assert!(self.pos <= self.end && self.end + 8 == self.dst.len());
        // SAFETY: `pos <= end == dst.len() - 8` (struct invariant, kept by
        // the clamp below), so the 8 bytes at `pos` are inside `dst`.
        unsafe {
            self.dst
                .as_mut_ptr()
                .add(self.pos)
                .cast::<u64>()
                .write_unaligned(self.container.to_le());
        }
        self.pos += nb_bytes;
        if self.pos > self.end {
            self.pos = self.end;
        }
        self.bit_pos &= 7;
        // `nb_bytes <= 7`, so the shift is < 64.
        self.container >>= nb_bytes * 8;
    }

    /// libzstd's `BIT_flushBitsFast` (used when the output is known to be
    /// large enough). Same as [`flush`](Self::flush): the clamp is kept so
    /// the writer stays in bounds whatever the caller does.
    #[inline(always)]
    pub(crate) fn flush_fast(&mut self) {
        self.flush();
    }

    /// libzstd's `BIT_closeCStream`: adds the end mark and flushes.
    /// Returns the stream size in bytes, or 0 if it didn't fit.
    #[inline]
    pub(crate) fn close(mut self) -> usize {
        self.add_bits_fast(1, 1);
        self.flush();
        if self.pos >= self.end {
            return 0;
        }
        self.pos + usize::from(self.bit_pos > 0)
    }

    /// Bits pending in the container (libzstd's `bitPos`).
    #[inline(always)]
    pub(crate) fn bit_pos(&self) -> u32 {
        self.bit_pos
    }

    /// Bytes written so far (libzstd's `ptr - startPtr`).
    #[inline(always)]
    pub(crate) fn written(&self) -> usize {
        self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitReader;

    #[test]
    fn too_small() {
        assert!(BitWriter::new(&mut [0u8; 8]).is_none());
        assert!(BitWriter::new(&mut [0u8; 9]).is_some());
    }

    #[test]
    fn empty_stream() {
        let mut buf = [0u8; 16];
        let w = BitWriter::new(&mut buf).unwrap();
        assert_eq!(w.close(), 1);
        assert_eq!(buf[0], 1);
    }

    #[test]
    fn round_trip_random_fields() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let n = (rnd() % 500) as usize;
            let fields: alloc::vec::Vec<(u64, u32)> = (0..n)
                .map(|_| {
                    let nb = (rnd() % 26) as u32;
                    (rnd(), nb)
                })
                .collect();
            let mut buf = alloc::vec![0u8; n * 4 + 16];
            let mut w = BitWriter::new(&mut buf).unwrap();
            for (i, &(v, nb)) in fields.iter().enumerate() {
                if i % 3 == 0 {
                    w.add_bits(v, nb);
                } else {
                    w.add_bits_fast(v & u64::from(BIT_MASK[nb as usize]), nb);
                }
                w.flush();
            }
            let size = w.close();
            assert!(size > 0);
            let mut br = BitReader::new(&buf[..size]).unwrap();
            for &(v, nb) in fields.iter().rev() {
                assert_eq!(br.read(nb), v & u64::from(BIT_MASK[nb as usize]));
                br.reload();
            }
            assert!(br.is_end());
        }
    }

    #[test]
    fn overflow_returns_zero() {
        // 9-byte buffer: `end` is 1, so one complete byte already overflows.
        let mut buf = [0u8; 9];
        let mut w = BitWriter::new(&mut buf).unwrap();
        w.add_bits(0xFF, 8);
        w.flush();
        assert_eq!(w.close(), 0);
        let mut buf = [0u8; 10];
        let mut w = BitWriter::new(&mut buf).unwrap();
        w.add_bits(0x7F, 7);
        w.flush();
        assert_eq!(w.close(), 1);
        assert_eq!(buf[0], 0xFF);
        // Lots of data into a small buffer: clamps, never panics.
        let mut buf = [0u8; 12];
        let mut w = BitWriter::new(&mut buf).unwrap();
        for _ in 0..100 {
            w.add_bits(0x1234567, 25);
            w.flush();
        }
        assert_eq!(w.close(), 0);
    }
}
