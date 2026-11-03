//! Run-time CPU feature detection.
//!
//! Hot loops are compiled twice: a generic version and one with
//! `#[target_feature]` (BMI2 on x86-64, where `shrx`/`shlx`/`bzhi` remove
//! the flag dependencies of variable shifts, the same trick as libzstd's
//! `DYNAMIC_BMI2`). [`Features::detect`] picks one once per call into the
//! decoder. Without `std`, features come from the compile-time target
//! features. The `portable` feature always selects the generic code.

/// The CPU features the decoder's specialized code paths use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Features {
    /// x86-64 BMI1 + BMI2 (+ LZCNT, which every BMI2 CPU has).
    pub bmi2: bool,
}

impl Features {
    #[inline]
    pub(crate) fn detect() -> Self {
        Features { bmi2: bmi2() }
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
    }
    #[cfg(not(feature = "std"))]
    {
        cfg!(all(
            target_feature = "bmi2",
            target_feature = "bmi1",
            target_feature = "lzcnt"
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
    if Features::detect().bmi2 {
        &["bmi2"]
    } else {
        &[]
    }
}
