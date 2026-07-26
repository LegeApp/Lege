//! CIE-based colour → sRGB, ported from PDFium's
//! `core/fpdfapi/page/cpdf_colorspace.cpp`.
//!
//! The device conversions elsewhere in this crate are a *frozen convention*
//! shared by every backend (see `cmyk.rs`); the CIE ones are the same idea for
//! `/Lab`, `/CalRGB`, `/CalGray`. Because the differential oracle is PDFium,
//! these port PDFium's exact arithmetic — including its two hard-coded sRGB
//! gamma lookup tables — rather than the textbook CIE formulae, so the oracle
//! agrees pixel for pixel.
//!
//! # Provenance
//! - [`SRGB_SAMPLES_1`] / [`SRGB_SAMPLES_2`] are `kSRGBSamples1` / `kSRGBSamples2`
//!   (cpdf_colorspace.cpp lines 54-85), **verbatim**.
//! - [`rgb_conversion`] is `RGB_Conversion` (lines 369-376).
//! - [`xyz_to_srgb`] is `XYZ_to_sRGB` (lines 378-383).
//!
//! These tables are empirical data (the sRGB transfer curve sampled at
//! 1024 points); like the CMYK grid they cannot be derived, only copied.

/// `kSRGBSamples1`, verbatim from PDFium. Indexed by `scale` in `0..192`.
#[rustfmt::skip]
static SRGB_SAMPLES_1: [u8; 192] = [
    0, 3, 6, 10, 13, 15, 18, 20, 22, 23, 25, 27, 28, 30, 31,
    32, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47,
    48, 49, 49, 50, 51, 52, 53, 53, 54, 55, 56, 56, 57, 58, 58,
    59, 60, 61, 61, 62, 62, 63, 64, 64, 65, 66, 66, 67, 67, 68,
    68, 69, 70, 70, 71, 71, 72, 72, 73, 73, 74, 74, 75, 76, 76,
    77, 77, 78, 78, 79, 79, 79, 80, 80, 81, 81, 82, 82, 83, 83,
    84, 84, 85, 85, 85, 86, 86, 87, 87, 88, 88, 88, 89, 89, 90,
    90, 91, 91, 91, 92, 92, 93, 93, 93, 94, 94, 95, 95, 95, 96,
    96, 97, 97, 97, 98, 98, 98, 99, 99, 99, 100, 100, 101, 101, 101,
    102, 102, 102, 103, 103, 103, 104, 104, 104, 105, 105, 106, 106, 106, 107,
    107, 107, 108, 108, 108, 109, 109, 109, 110, 110, 110, 110, 111, 111, 111,
    112, 112, 112, 113, 113, 113, 114, 114, 114, 115, 115, 115, 115, 116, 116,
    116, 117, 117, 117, 118, 118, 118, 118, 119, 119, 119, 120,
];

/// `kSRGBSamples2`, verbatim from PDFium. Indexed by `scale / 4 - 48` for
/// `scale` in `192..=1023`.
#[rustfmt::skip]
static SRGB_SAMPLES_2: [u8; 208] = [
    120, 121, 122, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135,
    136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 148, 149,
    150, 151, 152, 153, 154, 155, 155, 156, 157, 158, 159, 159, 160, 161, 162,
    163, 163, 164, 165, 166, 167, 167, 168, 169, 170, 170, 171, 172, 173, 173,
    174, 175, 175, 176, 177, 178, 178, 179, 180, 180, 181, 182, 182, 183, 184,
    185, 185, 186, 187, 187, 188, 189, 189, 190, 190, 191, 192, 192, 193, 194,
    194, 195, 196, 196, 197, 197, 198, 199, 199, 200, 200, 201, 202, 202, 203,
    203, 204, 205, 205, 206, 206, 207, 208, 208, 209, 209, 210, 210, 211, 212,
    212, 213, 213, 214, 214, 215, 215, 216, 216, 217, 218, 218, 219, 219, 220,
    220, 221, 221, 222, 222, 223, 223, 224, 224, 225, 226, 226, 227, 227, 228,
    228, 229, 229, 230, 230, 231, 231, 232, 232, 233, 233, 234, 234, 235, 235,
    236, 236, 237, 237, 238, 238, 238, 239, 239, 240, 240, 241, 241, 242, 242,
    243, 243, 244, 244, 245, 245, 246, 246, 246, 247, 247, 248, 248, 249, 249,
    250, 250, 251, 251, 251, 252, 252, 253, 253, 254, 254, 255, 255,
];

/// Replace a non-finite value (NaN/±inf from a malformed content stream or
/// resource) with `0.0`. PDF numbers are untrusted; a poisoned coefficient
/// must not propagate NaN through the whole colour pipeline.
#[inline]
pub(crate) fn sanitize(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

/// One linear-light channel → sRGB via PDFium's sampled transfer curve.
///
/// Port of `RGB_Conversion` (cpdf_colorspace.cpp lines 369-376): clamp to
/// `[0, 1]`, quantise to a 0..1023 index, and read the pre-tabulated sRGB byte.
/// This is what makes the oracle agree — the textbook `pow(x, 1/2.4)` sRGB
/// transfer would drift from PDFium by a code value or two.
#[inline]
pub(crate) fn rgb_conversion(component: f32) -> f32 {
    // std::clamp in C++ is UB on NaN; define it as 0.0 here.
    let c = sanitize(component).clamp(0.0, 1.0);
    // `f32 as i32` saturates in Rust (0 for NaN, already excluded), matching
    // the effect of `std::max(static_cast<int>(...), 0)`.
    let scale = ((c * 1023.0) as i32).max(0);
    if scale < 192 {
        f32::from(SRGB_SAMPLES_1[scale as usize]) / 255.0
    } else {
        // scale in 192..=1023 → index 0..=207.
        f32::from(SRGB_SAMPLES_2[(scale / 4 - 48) as usize]) / 255.0
    }
}

/// CIE 1931 XYZ → sRGB with PDFium's fixed matrix and sampled transfer.
///
/// Port of `XYZ_to_sRGB` (cpdf_colorspace.cpp lines 378-383). The matrix is the
/// standard D65 XYZ→linear-sRGB, rounded to PDFium's 4-decimal constants; the
/// per-channel [`rgb_conversion`] applies the gamma table. This is the
/// non-white-point variant PDFium's Lab path uses (it bakes its own white into
/// the Lab→XYZ step, below).
pub(crate) fn xyz_to_srgb(x: f32, y: f32, z: f32) -> [f32; 3] {
    let r = 3.2410 * x - 1.5374 * y - 0.4986 * z;
    let g = -0.9692 * x + 1.8760 * y + 0.0416 * z;
    let b = 0.0556 * x - 0.2040 * y + 1.0570 * z;
    [rgb_conversion(r), rgb_conversion(g), rgb_conversion(b)]
}

/// A row-major 3×3 matrix `[a b c d e f g h i]`, the layout of PDFium's
/// `Matrix_3by3` (cpdf_colorspace.cpp lines 309-367).
type Mat3 = [f32; 9];

/// Inverse of a 3×3 matrix, port of `Matrix_3by3::Inverse` (lines 333-343).
///
/// PDFium returns the **all-zeros** default matrix when the determinant is
/// below `float` epsilon (a singular matrix); we mirror that exactly. That
/// zero matrix maps any XYZ to `(0,0,0)` → black, which is how PDFium
/// degrades a singular CalRGB `/Matrix` or a `/WhitePoint` with a zero
/// component (its diagonal collapses `M` to rank-deficient). Mirroring the
/// guard is also our NaN-safety: there is no blind divide-by-tiny.
fn mat3_inverse(m: Mat3) -> Mat3 {
    let [a, b, c, d, e, f, g, h, i] = m;
    let det = a * (e * i - f * h) - b * (i * d - f * g) + c * (d * h - e * g);
    if det.abs() < f32::EPSILON {
        return [0.0; 9];
    }
    [
        (e * i - f * h) / det,
        -(b * i - c * h) / det,
        (b * f - c * e) / det,
        -(d * i - f * g) / det,
        (a * i - c * g) / det,
        -(a * f - c * d) / det,
        (d * h - e * g) / det,
        -(a * h - b * g) / det,
        (a * e - b * d) / det,
    ]
}

/// `self * other`, port of `Matrix_3by3::Multiply` (lines 345-351).
fn mat3_mul(m: Mat3, n: Mat3) -> Mat3 {
    [
        m[0] * n[0] + m[1] * n[3] + m[2] * n[6],
        m[0] * n[1] + m[1] * n[4] + m[2] * n[7],
        m[0] * n[2] + m[1] * n[5] + m[2] * n[8],
        m[3] * n[0] + m[4] * n[3] + m[5] * n[6],
        m[3] * n[1] + m[4] * n[4] + m[5] * n[7],
        m[3] * n[2] + m[4] * n[5] + m[5] * n[8],
        m[6] * n[0] + m[7] * n[3] + m[8] * n[6],
        m[6] * n[1] + m[7] * n[4] + m[8] * n[7],
        m[6] * n[2] + m[7] * n[5] + m[8] * n[8],
    ]
}

/// `M · v`, port of `Matrix_3by3::TransformVector` (lines 353-356).
fn mat3_transform(m: Mat3, v: [f32; 3]) -> [f32; 3] {
    [
        m[0] * v[0] + m[1] * v[1] + m[2] * v[2],
        m[3] * v[0] + m[4] * v[1] + m[5] * v[2],
        m[6] * v[0] + m[7] * v[1] + m[8] * v[2],
    ]
}

/// CIE XYZ → sRGB with a Bradford-free von-Kries-style adaptation to a given
/// white point. Port of `XYZ_to_sRGB_WhitePoint` (cpdf_colorspace.cpp lines
/// 385-412).
///
/// It builds `M = RGB_xyz · diag(RGB_xyz⁻¹ · white)`, the linear-sRGB→XYZ
/// matrix scaled so the white point maps to `(1,1,1)`, then applies `M⁻¹` and
/// the sampled sRGB transfer ([`rgb_conversion`]). A singular `M` (e.g. a
/// white point with a zero component) yields black via [`mat3_inverse`]'s
/// guard, matching PDFium.
pub(crate) fn xyz_to_srgb_white_point(
    x: f32,
    y: f32,
    z: f32,
    xw: f32,
    yw: f32,
    zw: f32,
) -> [f32; 3] {
    // sRGB primary chromaticities; `1 - x - y` for the z-row, computed at
    // runtime in f32 exactly as PDFium does.
    let (rx, ry) = (0.64f32, 0.33f32);
    let (gx, gy) = (0.30f32, 0.60f32);
    let (bx, by) = (0.15f32, 0.06f32);
    let rgb_xyz: Mat3 = [
        rx,
        gx,
        bx,
        ry,
        gy,
        by,
        1.0 - rx - ry,
        1.0 - gx - gy,
        1.0 - bx - by,
    ];

    let rgb_sum = mat3_transform(mat3_inverse(rgb_xyz), [xw, yw, zw]);
    let diag: Mat3 = [
        rgb_sum[0], 0.0, 0.0, 0.0, rgb_sum[1], 0.0, 0.0, 0.0, rgb_sum[2],
    ];
    let m = mat3_mul(rgb_xyz, diag);
    let rgb = mat3_transform(mat3_inverse(m), [x, y, z]);
    [
        rgb_conversion(rgb[0]),
        rgb_conversion(rgb[1]),
        rgb_conversion(rgb[2]),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn tables_have_pdfium_lengths_and_bounds() {
        // Index math depends on these exact lengths (192 / 208).
        assert_eq!(SRGB_SAMPLES_1.len(), 192);
        assert_eq!(SRGB_SAMPLES_2.len(), 208);
        // The two tables meet at 120 and the curve ends at 255 (full white).
        assert_eq!(SRGB_SAMPLES_1[191], 120);
        assert_eq!(SRGB_SAMPLES_2[0], 120);
        assert_eq!(SRGB_SAMPLES_2[207], 255);
    }

    #[test]
    fn rgb_conversion_endpoints_and_nan() {
        assert_eq!(rgb_conversion(0.0), 0.0);
        assert_eq!(rgb_conversion(1.0), 1.0); // scale 1023 → table 255 → 1.0
        assert_eq!(rgb_conversion(2.0), 1.0); // clamps high
        assert_eq!(rgb_conversion(-1.0), 0.0); // clamps low
        assert_eq!(rgb_conversion(f32::NAN), 0.0); // garbage → 0
    }

    #[test]
    fn xyz_white_maps_to_white() {
        // D65 white XYZ ≈ (0.9505, 1.0, 1.089) → sRGB white.
        let [r, g, b] = xyz_to_srgb(0.9505, 1.0, 1.089);
        assert!(r > 0.99 && g > 0.99 && b > 0.99, "{r} {g} {b}");
    }
}
