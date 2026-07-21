//! Portable SIMD kernels using the `wide` crate, one submodule per primitive
//! group. Every kernel must be bit-exact with `scalar` — see each
//! submodule's tests.
//!
//! Two entry points, matching the policy in `crate::simd`'s module doc:
//! - `setup_all` — force every kernel on, including measured regressions.
//!   Used for `DJVU_PRIMITIVES=wide` (explicit testing/benchmarking).
//! - `setup_auto` — install only kernels proven faster than scalar. Used
//!   for `DJVU_PRIMITIVES=auto` (the default).

mod color;
mod iw44;

use super::Primitives;

pub(super) fn setup_all(primitives: &mut Primitives) {
    color::setup(primitives);
    iw44::setup_all(primitives);
    primitives.backend = "wide";
}

pub(super) fn setup_auto(primitives: &mut Primitives) {
    // color::setup intentionally omitted — measured ~2x slower than scalar.
    iw44::setup_auto(primitives);
    primitives.backend = "wide-auto";
}
