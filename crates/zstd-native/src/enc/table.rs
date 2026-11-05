//! Cache-line-aligned match finder tables, on huge pages when large.
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
//! aligned element.
//!
//! # Huge pages
//!
//! The tables are read at random, so with 4 KiB pages nearly every access
//! misses the L1 TLB (level 11: about one miss per input byte). On Linux
//! x86-64 a table of at least 8 MiB (`huge::MIN_TABLE`: levels 9 and up
//! for large inputs) is therefore 2 MiB-aligned (over-allocating by 2 MiB
//! of address space, which is never touched when the allocator maps it
//! fresh) and its whole 2 MiB pages are marked `madvise(MADV_HUGEPAGE)`,
//! so that transparent huge pages back them in the default `madvise` THP
//! mode too. On the EPYC host that is worth 3% at level 9, 6% at level 11
//! and 11% at level 15 (libzstd's workspace is plain `malloc` memory).
//! Where THP is disabled the call does nothing; the contents and results
//! never change.
use alloc::vec::Vec;
use core::fmt;
use core::ops::{Deref, DerefMut};

/// The alignment of a [`Table`]'s first element, in bytes.
pub(crate) const TABLE_ALIGN: usize = 64;

/// A zero-initialised table of `T` whose first element is
/// [`TABLE_ALIGN`]-aligned (2 MiB-aligned and on huge pages when large;
/// see the module docs). Derefs to `[T]`.
pub(crate) struct Table<T> {
    /// The allocation: `len` entries from `off`, plus up to one cache line
    /// (or huge page) of padding.
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
        let bytes = size * elem;
        let align = if huge::ENABLED && bytes >= huge::MIN_TABLE {
            huge::PAGE
        } else {
            TABLE_ALIGN
        };
        let buf = alloc::vec![T::default(); size + align / elem];
        let misalign = buf.as_ptr() as usize % align;
        // `buf` is aligned to `T`, so the distance to the next aligned
        // address is a whole number of entries.
        let off = (align - misalign) % align / elem;
        debug_assert!(off + size <= buf.len());
        if align == huge::PAGE {
            // the whole huge pages of the table
            huge::advise(buf[off..].as_ptr().cast(), bytes / huge::PAGE * huge::PAGE);
        }
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

/// Transparent huge pages for large tables (Linux x86-64).
mod huge {
    /// The huge page size.
    pub(super) const PAGE: usize = 2 << 20;

    /// The smallest table put on huge pages. Below it (levels 5-8 for
    /// large inputs: 2-4 MiB hash tables) the tables mostly hit the L2 TLB
    /// and L3 cache anyway, and huge pages measured 2% slower at levels 5
    /// and 7 on the EPYC host (the hardware prefetchers then run past 4 KiB
    /// boundaries, which may be why).
    pub(super) const MIN_TABLE: usize = 8 << 20;

    pub(super) const ENABLED: bool = cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "std",
        not(miri)
    ));

    /// `madvise(p, len, MADV_HUGEPAGE)`; errors are ignored.
    #[cfg(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "std",
        not(miri)
    ))]
    #[allow(unsafe_code)]
    pub(super) fn advise(p: *const u8, len: usize) {
        const SYS_MADVISE: usize = 28;
        const MADV_HUGEPAGE: usize = 14;
        if len == 0 {
            return;
        }
        debug_assert!((p as usize).is_multiple_of(PAGE) && len.is_multiple_of(PAGE));
        // SAFETY: madvise(MADV_HUGEPAGE) only sets a flag on the mapping
        // of `p..p + len` (inside a live allocation); it never changes its
        // contents or validity. The syscall clobbers rcx and r11.
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") SYS_MADVISE => _,
                in("rdi") p,
                in("rsi") len,
                in("rdx") MADV_HUGEPAGE,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
    }

    #[cfg(not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "std",
        not(miri)
    )))]
    pub(super) fn advise(_p: *const u8, _len: usize) {}
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
    fn large_tables_on_huge_page_boundaries() {
        let entries = huge::MIN_TABLE / 4;
        for size in [entries, entries + 7, 3 * entries / 2] {
            let mut t = Table::<u32>::alloc(size);
            let want = if huge::ENABLED {
                huge::PAGE
            } else {
                TABLE_ALIGN
            };
            assert_eq!(t.as_ptr() as usize % want, 0);
            assert!(t.iter().all(|&v| v == 0));
            t[size - 1] = 7;
            t.resize(size + 1);
            assert_eq!(t[size - 1], 7);
            assert_eq!(t.as_ptr() as usize % want, 0);
        }
        // below the threshold: cache-line aligned only
        let t = Table::<u32>::alloc(entries - 1);
        assert_eq!(t.as_ptr() as usize % TABLE_ALIGN, 0);
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
