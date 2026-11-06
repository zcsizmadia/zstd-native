//! Run-time CPU feature detection.
//!
//! Hot loops are compiled twice: a generic version and one with
//! `#[target_feature]` (BMI2 on x86-64, where `shrx`/`shlx`/`bzhi` remove
//! the flag dependencies of variable shifts, the same trick as libzstd's
//! `DYNAMIC_BMI2`). [`Features::detect`] picks one once per call into the
//! decoder. Without `std`, features come from the compile-time target
//! features. The `portable` feature always selects the generic code.

use core::sync::atomic::{AtomicU8, Ordering};

/// The CPU features the decoder's specialized code paths use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Features {
    /// x86-64 BMI1 + BMI2 (+ LZCNT and SSSE3, which every BMI2 CPU has;
    /// the BMI2 sequence loop uses SSSE3 shuffles for short match offsets).
    pub bmi2: bool,
    /// Use the `asm!` Huffman loops: x86-64 with BMI2, not AMD (unless
    /// disabled with [`set_asm`] for testing and A/B measurements). On
    /// Intel (Alder Lake) they are 12% faster than the Rust loops; on AMD
    /// (Zen 3) 6% slower (their `movb %ah` stores).
    pub huf_asm: bool,
}

impl Features {
    #[inline]
    pub(crate) fn detect() -> Self {
        let bmi2 = bmi2();
        let huf_asm = bmi2
            && match ASM.load(Ordering::Relaxed) {
                ASM_ON => true,
                ASM_OFF => false,
                _ => !amd(),
            };
        Features { bmi2, huf_asm }
    }
}

#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[inline]
fn bmi2() -> bool {
    #[cfg(feature = "std")]
    {
        std::is_x86_feature_detected!("bmi2")
            && std::is_x86_feature_detected!("bmi1")
            && std::is_x86_feature_detected!("lzcnt")
            && std::is_x86_feature_detected!("ssse3")
    }
    #[cfg(not(feature = "std"))]
    {
        cfg!(all(
            target_feature = "bmi2",
            target_feature = "bmi1",
            target_feature = "lzcnt",
            target_feature = "ssse3"
        ))
    }
}

#[cfg(not(all(target_arch = "x86_64", not(feature = "portable"))))]
#[inline]
fn bmi2() -> bool {
    false
}

/// Names of the specialized paths the running CPU selects, for tests that
/// must prove they don't only exercise the generic code
/// (`ZSTD_NATIVE_REQUIRE_SIMD`).
#[doc(hidden)]
pub fn selected_paths() -> &'static [&'static str] {
    let f = Features::detect();
    match (f.bmi2, f.huf_asm) {
        (true, true) => &["bmi2", "asm"],
        (true, false) => &["bmi2"],
        _ => &[],
    }
}

const ASM_AUTO: u8 = 0;
const ASM_ON: u8 = 1;
const ASM_OFF: u8 = 2;
static ASM: AtomicU8 = AtomicU8::new(ASM_AUTO);

/// Forces the assembly Huffman loops on (`Some(true)`, where the CPU has
/// BMI2) or off (`Some(false)`), or back to the per-vendor default (`None`).
/// For tests and benchmarks; results are identical either way.
#[doc(hidden)]
pub fn set_asm(mode: Option<bool>) {
    let v = match mode {
        None => ASM_AUTO,
        Some(true) => ASM_ON,
        Some(false) => ASM_OFF,
    };
    ASM.store(v, Ordering::Relaxed);
}

/// AMD or Hygon CPU (CPUID vendor string), where the Rust Huffman loops
/// beat the asm ones.
/// Cached `amd()`: 0 = not checked yet, 1 = AMD/Hygon, 2 = other.
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
static VENDOR: AtomicU8 = AtomicU8::new(0);

#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
fn amd() -> bool {
    match VENDOR.load(Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    // SAFETY: CPUID leaf 0 exists on every x86-64 CPU.
    #[allow(unsafe_code, unused_unsafe)]
    let r = unsafe { core::arch::x86_64::__cpuid(0) };
    let vendor = (
        r.ebx.to_le_bytes(),
        r.edx.to_le_bytes(),
        r.ecx.to_le_bytes(),
    );
    let amd = vendor == (*b"Auth", *b"enti", *b"cAMD") || vendor == (*b"Hygo", *b"nGen", *b"uine");
    VENDOR.store(if amd { 1 } else { 2 }, Ordering::Relaxed);
    amd
}

#[cfg(not(all(target_arch = "x86_64", not(feature = "portable"))))]
fn amd() -> bool {
    false
}
