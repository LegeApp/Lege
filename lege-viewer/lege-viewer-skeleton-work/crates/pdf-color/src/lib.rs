//! Color space semantics and conversion policy.
//!
//! This crate models *what a color space means*; conversion machinery with
//! per-worker contexts (the LittleCMS lesson — no global CMS context) comes
//! with Phase 2/6. Any CMS or LUT state will live in worker contexts or
//! explicitly synchronized caches, never in these descriptors.

use pdf_object::ObjectId;

/// The PDF color space families (ISO 32000-1 §8.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpaceFamily {
    DeviceGray,
    DeviceRgb,
    DeviceCmyk,
    CalGray,
    CalRgb,
    Lab,
    IccBased,
    Indexed,
    Separation,
    DeviceN,
    Pattern,
}

impl ColorSpaceFamily {
    /// Number of components, where fixed by the family.
    pub fn component_count(self) -> Option<u8> {
        match self {
            ColorSpaceFamily::DeviceGray | ColorSpaceFamily::CalGray => Some(1),
            ColorSpaceFamily::DeviceRgb | ColorSpaceFamily::CalRgb | ColorSpaceFamily::Lab => {
                Some(3)
            }
            ColorSpaceFamily::DeviceCmyk => Some(4),
            _ => None,
        }
    }
}

/// A resolved color space description (semantic; no conversion state).
#[derive(Debug, Clone)]
pub struct ColorSpaceDesc {
    pub family: ColorSpaceFamily,
    pub components: u8,
    /// Backing object for parameterized spaces (ICC stream, tint transform,
    /// base space, ...), for cache keying per blueprint §8.5.
    pub object: Option<ObjectId>,
}

/// Convert a DeviceGray component to straight RGB (ISO 32000-1 §8.6.4.2).
pub mod cmyk;
pub mod icc;
pub(crate) mod cie;

pub fn gray_to_rgb(g: f32) -> [f32; 3] {
    [g, g, g]
}

/// CIE 1976 L\*a\*b\* → sRGB (ISO 32000-1 §8.6.5.4), ported from PDFium's
/// `CPDF_LabCS::GetRGB` (`core/fpdfapi/page/cpdf_colorspace.cpp` lines
/// 871-900) so the differential oracle agrees.
///
/// This is the one CIE space where component-arity fallback was *badly* wrong:
/// L\* runs `[0, 100]` and a\*/b\* run roughly `[-100, 100]`, so feeding raw
/// Lab components into an RGB triple (as the old `Lab → DeviceRGB` arity guess
/// did) is nonsense — real Lab content rendered wildly off. This does the
/// actual conversion.
///
/// - `l`, `a`, `b`: the L\*, a\*, b\* components.
/// - `range`: the `/Range` `[aMin aMax bMin bMax]` (default
///   `[-100 100 -100 100]`); a\*/b\* are clamped to it and L\* to `[0, 100]`,
///   mirroring the component clamping PDFium performs before `GetRGB`.
/// - `white_point`: **accepted but unused**, kept for a future colorimetric
///   mode. PDFium's `GetRGB` does *not* use the dict `/WhitePoint`: it bakes a
///   fixed D50-ish white (the `0.957`/`1.0889` constants below) into the
///   Lab→XYZ step and calls the plain (non-white-point) `XYZ_to_sRGB`. Using
///   the dict white here would *disagree* with the oracle.
///
/// Non-finite inputs are treated as `0.0` (no panics on garbage).
pub fn lab_to_rgb(l: f32, a: f32, b: f32, white_point: [f32; 3], range: [f32; 4]) -> [f32; 3] {
    // Kept for a future colorimetric mode; PDFium's math ignores it (see above).
    let _ = white_point;

    // Clamp to the component ranges, exactly as PDFium's colour-setting path
    // does before GetRGB. `/Range` may be malformed (min > max, NaN); order it
    // so `f32::clamp` (which panics on min > max) is always safe.
    let (a_lo, a_hi) = ordered(cie::sanitize(range[0]), cie::sanitize(range[1]));
    let (b_lo, b_hi) = ordered(cie::sanitize(range[2]), cie::sanitize(range[3]));
    let l = cie::sanitize(l).clamp(0.0, 100.0);
    let a = cie::sanitize(a).clamp(a_lo, a_hi);
    let b = cie::sanitize(b).clamp(b_lo, b_hi);

    // Lab → XYZ, PDFium's exact form (note the baked 0.957 / 1.0889 white and
    // the 0.2069 / 0.1379 / 0.12842 piecewise linear toe — these are PDFium's
    // constants, not the textbook 6/29 form; port them verbatim for parity).
    let m = (l + 16.0) / 116.0;
    let ll = m + a / 500.0;
    let n = m - b / 200.0;
    let x = if ll < 0.2069 { 0.957 * 0.12842 * (ll - 0.1379) } else { 0.957 * ll * ll * ll };
    let y = if m < 0.2069 { 0.12842 * (m - 0.1379) } else { m * m * m };
    let z = if n < 0.2069 { 1.0889 * 0.12842 * (n - 0.1379) } else { 1.0889 * n * n * n };

    cie::xyz_to_srgb(x, y, z)
}

/// CalRGB → sRGB (ISO 32000-1 §8.6.5.3), ported from PDFium's
/// `CPDF_CalRGBCS::GetRGB` (`core/fpdfapi/page/cpdf_colorspace.cpp` lines
/// 784-810) so the differential oracle agrees.
///
/// The chain is: per-component `powf` gamma → the 3×3 `/Matrix` (ABC → XYZ) →
/// [`cie::xyz_to_srgb_white_point`] chromatic adaptation to `/WhitePoint`. It
/// is **not** a pass-through: even an "identity" CalRGB (gamma 1, identity
/// matrix) is treated as XYZ and adapted, and every channel passes through the
/// sampled sRGB transfer table.
///
/// - `abc`: the three colour components.
/// - `gamma`: `/Gamma`; the caller must supply the spec default `[1, 1, 1]`
///   when the dict omits it (PDFium skips `powf` entirely in that case, which
///   `powf(x, 1.0) == x` reproduces exactly).
/// - `matrix`: `/Matrix` in column-per-component order
///   `[XA YA ZA XB YB ZB XC YC ZC]`; the caller must supply the identity
///   `[1 0 0 0 1 0 0 0 1]` when the dict omits it (PDFium's absent-matrix path
///   sets `XYZ = ABC`, which the identity reproduces).
/// - `white_point`: `/WhitePoint` `[Xw Yw Zw]` (required by the spec).
///
/// Degenerate cases mirror PDFium: a singular `/Matrix` or a `/WhitePoint`
/// with a zero component collapses the adaptation matrix and yields black (via
/// the determinant guard in [`cie::xyz_to_srgb_white_point`]). Divergence for
/// NaN-tolerance (house rule): non-finite `abc` are treated as `0.0` and a
/// non-finite `gamma` as `1.0` up front, so a poisoned coefficient cannot
/// propagate a NaN into the raster; PDFium would let it through and land on 0
/// only by way of the final clamp. The sRGB transfer itself is the ultimate
/// backstop — it clamps and sanitises every output channel.
pub fn calrgb_to_rgb(abc: [f32; 3], white_point: [f32; 3], gamma: [f32; 3], matrix: [f32; 9]) -> [f32; 3] {
    // NaN-tolerance: sanitise components (→0) and gamma (→1, the identity/
    // default) so `powf` cannot manufacture a NaN from garbage. See the
    // divergence note above.
    let g = gamma.map(|x| if x.is_finite() { x } else { 1.0 });
    let a = cie::sanitize(abc[0]).powf(g[0]);
    let b = cie::sanitize(abc[1]).powf(g[1]);
    let c = cie::sanitize(abc[2]).powf(g[2]);

    // ABC → XYZ through the /Matrix (column-per-component), exactly as
    // CPDF_CalRGBCS::GetRGB indexes it.
    let m = matrix;
    let x = m[0] * a + m[3] * b + m[6] * c;
    let y = m[1] * a + m[4] * b + m[7] * c;
    let z = m[2] * a + m[5] * b + m[8] * c;

    cie::xyz_to_srgb_white_point(x, y, z, white_point[0], white_point[1], white_point[2])
}

/// CalGray → gray (ISO 32000-1 §8.6.5.2).
///
/// **Policy: pass the single component straight through.** Ported from
/// `CPDF_CalGray::GetRGB` (cpdf_colorspace.cpp lines 723-727), which returns
/// `{gray, gray, gray}` and ignores `/Gamma` entirely. `gamma` is accepted but
/// unused, kept for a future colorimetric mode. Non-finite input → `0.0`.
pub fn calgray_to_gray(a: f32, gamma: f32) -> f32 {
    let _ = gamma;
    cie::sanitize(a).clamp(0.0, 1.0)
}

/// Order a `(min, max)` pair, tolerating a malformed (inverted) range so a
/// later `f32::clamp` never sees `min > max` (which would panic).
#[inline]
fn ordered(lo: f32, hi: f32) -> (f32, f32) {
    if lo <= hi { (lo, hi) } else { (hi, lo) }
}
/// Convert DeviceCMYK to RGB using Adobe's measured table (`cmyk` module),
/// matching PDFium bit for bit.
///
/// CMYK describes ink on paper, and no formula turns ink into light. The
/// naive `R = (1-C)(1-K)` that stood here is arithmetic wearing colour's
/// clothes: it renders `DeviceCMYK(1, 0.44, 0, 0)` as a searing
/// `rgb(0, 143, 255)` where the measured answer is a muted blue. The
/// differential oracle caught that against PDFium on its first run, on a real
/// book cover — and it applied to every CMYK page of every document.
///
/// So the policy is now PDFium's: interpolate a 9×9×9×9 grid of measured
/// Adobe values. It remains a *single frozen convention* shared by the CPU and
/// future GPU backends (see `pdf_render_api::contract` §10), it is still
/// deterministic and profile-free, and a real ICC / device-link path is still
/// a future opt-in — never a silent change to this function.
pub fn cmyk_to_rgb(c: f32, m: f32, y: f32, k: f32) -> [f32; 3] {
    cmyk::adobe_cmyk_to_srgb(c, m, y, k)
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ColorError {
    #[error("unknown color space family: {0}")]
    UnknownFamily(String),
    #[error("malformed color space definition: {0}")]
    Malformed(&'static str),
    #[error("component count {found} invalid for {family:?}")]
    BadComponentCount { family: ColorSpaceFamily, found: u8 },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// The frozen device-CMYK conversion (Phase 6F): exact, deterministic
    /// values at the primaries. Changing these is a deliberate policy change.
    #[test]
    fn cmyk_policy_is_the_measured_table() {
        // The policy is Adobe's measured grid (PDFium's), not the naive
        // formula this used to assert. Ink is not arithmetic: full K is a
        // very dark grey, not #000, and pure cyan is nothing like
        // rgb(0, 255, 255).
        let px = |v: [f32; 3]| v.map(|x| (x * 255.0).round() as i32);
        assert_eq!(px(cmyk_to_rgb(0.0, 0.0, 0.0, 0.0)), [255, 255, 255], "no ink → paper");
        // Full K is a very dark *grey*, not #000 — it is measured ink. It is
        // [35, 31, 32] rather than the grid's [34, 30, 31] because 255 does
        // not land exactly on grid point 8 in 8.8 fixed point (255<<8 =
        // 65280, 8<<13 = 65536), leaving a residual the interpolation
        // applies. PDFium computes the same value; verified swatch-for-swatch
        // against it with tools/pdfium-diff.
        assert_eq!(px(cmyk_to_rgb(0.0, 0.0, 0.0, 1.0)), [35, 31, 32], "full K is measured ink");

        let cyan = px(cmyk_to_rgb(1.0, 0.0, 0.0, 0.0));
        assert!(cyan[0] < 60 && cyan[2] > 180, "cyan is an ink, not [0,255,255]: {cyan:?}");
        let magenta = px(cmyk_to_rgb(0.0, 1.0, 0.0, 0.0));
        assert!(magenta[0] > 180 && magenta[1] < 80, "magenta: {magenta:?}");
        let yellow = px(cmyk_to_rgb(0.0, 0.0, 1.0, 0.0));
        assert!(yellow[0] > 180 && yellow[2] < 90, "yellow: {yellow:?}");

        // Still a pure function of its inputs, shared by every backend.
        assert_eq!(cmyk_to_rgb(0.5, 0.1, 0.0, 0.2), cmyk_to_rgb(0.5, 0.1, 0.0, 0.2));
    }

    #[test]
    fn gray_policy_is_identity_triple() {
        assert_eq!(gray_to_rgb(0.0), [0.0, 0.0, 0.0]);
        assert_eq!(gray_to_rgb(1.0), [1.0, 1.0, 1.0]);
        assert_eq!(gray_to_rgb(0.5), [0.5, 0.5, 0.5]);
    }
}
