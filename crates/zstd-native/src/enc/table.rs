//! Cache-line-aligned match finder tables.
//!
//! libzstd's workspace (`ZSTD_cwksp`) places its tables at 64-byte
//! boundaries (`ZSTD_CWKSP_ALIGNMENT_BYTES`). The row match finder depends
//! on it: one row of its hash table (16, 32 or 64 indices) and of its tag
//! table is read as a unit, and a row that straddles cache lines costs a
//! second miss. A `Vec` is only aligned to its element (and `malloc` to 16
//! bytes), so where a table lands depends on unrelated allocations: on the
//! EPYC host the same binary ran whole-input level 5 at 119 or 138 MiB/s
//! depending only on the length of its path (`argv[0]` moves the heap).
//! [`Table`] over-allocates by one cache line and starts at the first
//! aligned element, without `unsafe`.

use alloc::vec::Vec;
use core::fmt;
use core::ops::{Deref, DerefMut};

/// The alignment of a [`Table`]'s first element, in bytes.
pub(crate) const TABLE_ALIGN: usize = 64;

/// A zero-initialised table of `T` whose first element is
/// [`TABLE_ALIGN`]-aligned. Derefs to `[T]`.
pub(crate) struct Table<T> {
    /// The allocation: `len` entries from `off`, plus up to one cache line
    /// of padding.
    buf: Vec<T>,
    /// The index of the first aligned entry in `buf`.
    off: usize,
    /// The number of entries.
    len: usize,
}

impl<T: Copy + Default> Table<T> {
    /// An empty table (no allocation).
    pub(crate) const fn new() -> Self {
        Self {
            buf: Vec::new(),
            off: 0,
            len: 0,
        }
    }

    /// `size` zeroed entries in a new allocation.
    fn alloc(size: usize) -> Self {
        if size == 0 {
            return Self::new();
        }
        let elem = core::mem::size_of::<T>();
        debug_assert!(elem > 0 && TABLE_ALIGN.is_multiple_of(elem));
        let buf = alloc::vec![T::default(); size + TABLE_ALIGN / elem];
        let misalign = buf.as_ptr() as usize % TABLE_ALIGN;
        // `buf` is aligned to `T`, so the distance to the next cache line is
        // a whole number of entries.
        let off = (TABLE_ALIGN - misalign) % TABLE_ALIGN / elem;
        debug_assert!(off + size <= buf.len());
        Self {
            buf,
            off,
            len: size,
        }
    }

    /// Resizes to `size` zeroed entries, reusing the allocation when the size
    /// does not change.
    pub(crate) fn reset_zeroed(&mut self, size: usize) {
        if self.len == size {
            self.fill(T::default());
        } else {
            *self = Self::alloc(size);
        }
    }

    /// Resizes to `size` entries like `Vec::resize(size, 0)`: the first
    /// `min(len, size)` entries are kept, new ones are zero.
    pub(crate) fn resize(&mut self, size: usize) {
        if self.len == size {
            return;
        }
        let mut t = Self::alloc(size);
        let keep = self.len.min(size);
        t[..keep].copy_from_slice(&self[..keep]);
        *self = t;
    }
}

impl<T: Copy + Default> Default for Table<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Deref for Table<T> {
    type Target = [T];
    #[inline(always)]
    fn deref(&self) -> &[T] {
        &self.buf[self.off..self.off + self.len]
    }
}

impl<T> DerefMut for Table<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.buf[self.off..self.off + self.len]
    }
}

impl<T: fmt::Debug> fmt::Debug for Table<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Table").field("len", &self.len).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_and_zeroed() {
        for size in [1usize, 3, 16, 1000, 1 << 16] {
            let t = Table::<u32>::alloc(size);
            assert_eq!(t.len(), size);
            assert_eq!(t.as_ptr() as usize % TABLE_ALIGN, 0);
            assert!(t.iter().all(|&v| v == 0));
            let t = Table::<u8>::alloc(size);
            assert_eq!(t.len(), size);
            assert_eq!(t.as_ptr() as usize % TABLE_ALIGN, 0);
        }
        assert!(Table::<u32>::alloc(0).is_empty());
    }

    #[test]
    fn reset_and_resize() {
        let mut t = Table::<u8>::new();
        t.resize(10);
        t[..10].copy_from_slice(&[1; 10]);
        t.resize(10); // unchanged: kept
        assert!(t.iter().all(|&v| v == 1));
        t.resize(20); // grown: prefix kept, the rest zero
        assert_eq!(&t[..10], &[1; 10]);
        assert_eq!(&t[10..], &[0; 10]);
        assert_eq!(t.as_ptr() as usize % TABLE_ALIGN, 0);
        t.resize(5);
        assert_eq!(&t[..], &[1; 5]);
        t.reset_zeroed(5);
        assert_eq!(&t[..], &[0; 5]);
        t.reset_zeroed(0);
        assert!(t.is_empty());
    }
}
