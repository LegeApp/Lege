//! x86_64 AVX2 backend — intentionally empty for now.
//!
//! jp2lam's own measurements (llm-docs/SIMD_AND_PARALLELISM_PLAN.md) found no
//! justification for hand-written AVX2 kernels until profiling on `wide`
//! shows a real gap `wide` can't close. Same rule here: this stub exists so
//! the dispatch plumbing and `DJVU_PRIMITIVES=avx2` mode are in place ahead
//! of need, not because there's a kernel to install yet.

use super::Primitives;

pub(super) fn setup(_primitives: &mut Primitives, _mode: &str) {
    if avx2_available() {
        // No AVX2 kernels yet — nothing to install.
    }
}

fn avx2_available() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}
