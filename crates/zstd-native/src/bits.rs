//! Backward bit reader (libzstd's `BIT_DStream_t`).
//!
//! zstd's entropy-coded streams are written forward and read backward,
//! starting from the last byte, whose highest set bit is an end marker.
//! Bits before the start of the stream read as zero; a stream is valid only
//! if decoding consumes exactly all of its bits ([`BitReader::is_end`]).
//!
//! The reader holds raw pointers instead of a slice and an index: in the
//! sequence loop every register counts, and this is libzstd's footprint
//! (container, consumed count, current pointer, start).
#![allow(unsafe_code)]

use core::marker::PhantomData;

use crate::error::{Error, Result};

#[inline(always)]
pub(crate) fn read_u64_le(src: &[u8], pos: usize) -> u64 {
    debug_assert!(pos + 8 <= src.len());
    // SAFETY: every caller keeps `pos + 8 <= src.len()` (asserted above in
    // debug builds); the read is unaligned.
    unsafe { u64::from_le(src.as_ptr().add(pos).cast::<u64>().read_unaligned()) }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum Status {
    /// At least 57 bits are in the container.
    Unfinished = 0,
    /// The start of the stream was reached; the container may hold fewer.
    EndOfBuffer = 1,
    /// Every bit has been consumed.
    Completed = 2,
    /// More bits were consumed than the stream has.
    Overflow = 3,
}

/// Invariant: `start <= ip` and `ip + 8 <= start + len` for streams of at
/// least 8 bytes; `ip == start` for shorter ones (whose container was
/// assembled byte by byte and is never reloaded from memory).
#[derive(Clone, Copy, Debug)]
pub(crate) struct BitReader<'a> {
    container: u64,
    /// Bits consumed from the top of `container`.
    consumed: u64,
    /// Start of the 8-byte window.
    ip: *const u8,
    start: *const u8,
    _src: PhantomData<&'a [u8]>,
}

impl<'a> BitReader<'a> {
    #[inline]
    pub(crate) fn new(src: &'a [u8]) -> Result<Self> {
        let Some(&last) = src.last() else {
            return Err(Error::Corrupted);
        };
        if last == 0 {
            return Err(Error::Corrupted);
        }
        let marker = u64::from(last.leading_zeros()) + 1;
        let start = src.as_ptr();
        if src.len() >= 8 {
            let pos = src.len() - 8;
            Ok(BitReader {
                container: read_u64_le(src, pos),
                consumed: marker,
                // SAFETY: `pos + 8 <= len`.
                ip: unsafe { start.add(pos) },
                start,
                _src: PhantomData,
            })
        } else {
            let mut container = 0u64;
            for (i, &b) in src.iter().enumerate() {
                container |= u64::from(b) << (8 * i);
            }
            let consumed = marker + (8 - src.len() as u64) * 8;
            Ok(BitReader {
                container,
                consumed,
                ip: start,
                start,
                _src: PhantomData,
            })
        }
    }

    /// Restarts a reader in the state a fast loop left: the window at
    /// `pos` (`pos + 8 <= src.len()`) with `consumed` bits used.
    #[inline]
    pub(crate) fn from_parts(src: &'a [u8], pos: usize, consumed: u32) -> Self {
        let container = read_u64_le(src, pos);
        // SAFETY: `pos + 8 <= len` (checked by `read_u64_le` in debug builds
        // and guaranteed by the callers).
        let ip = unsafe { src.as_ptr().add(pos) };
        BitReader {
            container,
            consumed: u64::from(consumed),
            ip,
            start: src.as_ptr(),
            _src: PhantomData,
        }
    }

    /// The next `n` bits (0..=56) without consuming them.
    #[inline(always)]
    pub(crate) fn peek(&self, n: u32) -> u64 {
        // libzstd's BIT_getMiddleBits: well defined even when the stream
        // is overconsumed (the result is then garbage that `is_end` rejects).
        let start = 64u64.wrapping_sub(self.consumed).wrapping_sub(u64::from(n)) & 63;
        (self.container >> start) & ((1u64 << n) - 1)
    }

    /// The next `n` bits, `1 <= n <= 56`: `(container << consumed) >> (64 - n)`.
    #[inline(always)]
    pub(crate) fn peek_fast(&self, n: u32) -> u64 {
        debug_assert!((1..=56).contains(&n));
        (self.container << (self.consumed & 63)) >> ((64 - n) & 63)
    }

    #[inline(always)]
    pub(crate) fn skip(&mut self, n: u32) {
        self.consumed += u64::from(n);
    }

    #[inline(always)]
    pub(crate) fn read(&mut self, n: u32) -> u64 {
        let v = self.peek(n);
        self.consumed += u64::from(n);
        v
    }

    /// `read` for `n >= 1`.
    #[inline(always)]
    pub(crate) fn read_fast(&mut self, n: u32) -> u64 {
        let v = self.peek_fast(n);
        self.consumed += u64::from(n);
        v
    }

    /// Refills the container (libzstd's `BIT_reloadDStream`).
    #[inline(always)]
    pub(crate) fn reload(&mut self) -> Status {
        if self.consumed > 64 {
            return Status::Overflow;
        }
        if self.ip as usize - self.start as usize >= 8 {
            // SAFETY: `consumed <= 64` moves back at most 8 bytes, staying at
            // or after `start`; the 8-byte window stays inside the stream.
            unsafe {
                self.ip = self.ip.sub((self.consumed >> 3) as usize);
                self.consumed &= 7;
                self.container = u64::from_le(self.ip.cast::<u64>().read_unaligned());
            }
            return Status::Unfinished;
        }
        self.reload_slow()
    }

    /// [`reload`](Self::reload) with the slow path inlined, for short
    /// streams (Huffman weights) where it is the common case.
    #[inline(always)]
    pub(crate) fn reload_short(&mut self) -> Status {
        if self.consumed > 64 {
            return Status::Overflow;
        }
        if self.ip as usize - self.start as usize >= 8 {
            // SAFETY: as in `reload`.
            unsafe {
                self.ip = self.ip.sub((self.consumed >> 3) as usize);
                self.consumed &= 7;
                self.container = u64::from_le(self.ip.cast::<u64>().read_unaligned());
            }
            return Status::Unfinished;
        }
        self.reload_slow_body()
    }

    #[cold]
    #[inline(never)]
    fn reload_slow(&mut self) -> Status {
        self.reload_slow_body()
    }

    #[inline(always)]
    fn reload_slow_body(&mut self) -> Status {
        let pos = self.ip as usize - self.start as usize;
        if pos == 0 {
            return if self.consumed < 64 {
                Status::EndOfBuffer
            } else {
                Status::Completed
            };
        }
        let mut nb = (self.consumed >> 3) as usize;
        let mut status = Status::Unfinished;
        if nb > pos {
            nb = pos;
            status = Status::EndOfBuffer;
        }
        // SAFETY: `nb <= pos`, so the window moves back to at most `start`.
        unsafe {
            self.ip = self.ip.sub(nb);
            self.consumed -= (nb * 8) as u64;
            self.container = u64::from_le(self.ip.cast::<u64>().read_unaligned());
        }
        status
    }

    /// True when exactly every bit of the stream has been consumed.
    #[inline]
    pub(crate) fn is_end(&self) -> bool {
        self.ip == self.start && self.consumed == 64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_zero_end() {
        assert!(BitReader::new(&[]).is_err());
        assert!(BitReader::new(&[1, 0]).is_err());
    }

    #[test]
    fn reads_backward() {
        // Written forward LSB-first: 0b101 (3 bits), 0b11 (2 bits), marker.
        let data = [0b0011_1101u8];
        let mut br = BitReader::new(&data).unwrap();
        assert_eq!(br.read(2), 0b11);
        assert_eq!(br.read(3), 0b101);
        assert!(br.is_end());
    }

    #[test]
    fn long_stream_end() {
        let mut data = [0u8; 20];
        data[19] = 0x80;
        let mut br = BitReader::new(&data).unwrap();
        let mut total = 0;
        while total < 19 * 8 + 7 {
            let n = (19 * 8 + 7 - total).min(25);
            assert_eq!(br.read(n), 0);
            total += n;
            br.reload();
        }
        assert!(br.is_end());
        assert_eq!(br.reload(), Status::Completed);
    }
}
