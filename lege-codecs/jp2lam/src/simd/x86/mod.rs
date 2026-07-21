//! x86 runtime dispatch layer for primitive kernels.
//!
//! This mirrors the bpg-rs shape: portable `wide` kernels are installed first,
//! then x86-specific kernels may overwrite individual entries when runtime CPU
//! feature detection proves they are available. No AVX2 overrides are installed
//! yet because the current measured `wide` kernels did not beat scalar on the
//! synthetic benchmark.

use super::Primitives;

pub(crate) fn setup(primitives: &mut Primitives, mode: &str) {
    if avx2_enabled(mode) && avx2_available() {
        avx2::setup(primitives);
    }
}

fn avx2_enabled(mode: &str) -> bool {
    matches!(mode, "auto" | "simd" | "x86" | "avx2")
}

#[cfg(target_arch = "x86")]
fn avx2_available() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}

#[cfg(target_arch = "x86_64")]
fn avx2_available() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}

mod avx2 {
    use super::Primitives;

    pub(super) fn setup(_primitives: &mut Primitives) {
        // Intentionally empty until a post-wide flamegraph identifies a kernel
        // where hand-written AVX2 is a measured win.
    }
}
