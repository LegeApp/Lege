#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! CIE-based colour conversion tests (ISO 32000-1 §8.6.5), pinned against the
//! ported PDFium math (`core/fpdfapi/page/cpdf_colorspace.cpp`).
//!
//! Lab and CalRGB get exact-value tests hand-traced through the ported math
//! (the real functional changes); CalGray gets a pass-through pinning test
//! (pass-through IS what `CPDF_CalGray::GetRGB` does).

use pdf_color::{calgray_to_gray, calrgb_to_rgb, lab_to_rgb};

/// Default Lab `/Range` (a\*/b\* in [-100, 100]).
const DEFAULT_RANGE: [f32; 4] = [-100.0, 100.0, -100.0, 100.0];
/// A plausible D50 white point (unused by the port, but realistic input).
const D50: [f32; 3] = [0.9643, 1.0, 0.8251];

fn px(v: [f32; 3]) -> [i32; 3] {
    v.map(|x| (x * 255.0).round() as i32)
}

#[test]
fn lab_lightness_100_is_white() {
    // L*=100, a*=b*=0 → the achromatic top of the Lab axis → sRGB white.
    let rgb = lab_to_rgb(100.0, 0.0, 0.0, D50, DEFAULT_RANGE);
    assert_eq!(rgb, [1.0, 1.0, 1.0], "L*=100 must be white, got {rgb:?}");
}

#[test]
fn lab_lightness_0_is_black() {
    // L*=0 → the bottom of the axis → sRGB black.
    let rgb = lab_to_rgb(0.0, 0.0, 0.0, D50, DEFAULT_RANGE);
    assert_eq!(rgb, [0.0, 0.0, 0.0], "L*=0 must be black, got {rgb:?}");
}

#[test]
fn lab_neutral_midtone_exact_bytes() {
    // L*=50, a*=b*=0 (neutral grey). Hand-traced through PDFium's exact math:
    //   M = (50+16)/116 = 0.568966
    //   X = 0.957·M³ = 0.176277, Y = M³ = 0.184197, Z = 1.0889·M³ = 0.200554
    //   XYZ→sRGB matrix → R1=0.18810, G1=0.18303, B1=0.18421
    //   RGB_Conversion: R1 → scale 192 → kSRGBSamples2[0]=120
    //                   G1 → scale 187 → kSRGBSamples1[187]=118
    //                   B1 → scale 188 → kSRGBSamples1[188]=119
    // This exercises the full Lab→XYZ→sRGB path AND both gamma tables.
    let rgb = lab_to_rgb(50.0, 0.0, 0.0, D50, DEFAULT_RANGE);
    assert_eq!(px(rgb), [120, 118, 119], "midtone grey mismatch: {rgb:?}");
}

#[test]
fn lab_a_b_clamp_to_range() {
    // a*/b* far outside the default range must clamp to ±100, so a huge input
    // equals the boundary input.
    let clamped = lab_to_rgb(60.0, 900.0, -900.0, D50, DEFAULT_RANGE);
    let boundary = lab_to_rgb(60.0, 100.0, -100.0, D50, DEFAULT_RANGE);
    assert_eq!(clamped, boundary, "a*/b* must clamp to /Range");

    // A narrower /Range clamps harder: a*=100 with range aMax=10 == a*=10.
    let narrow = [-10.0, 10.0, -10.0, 10.0];
    assert_eq!(
        lab_to_rgb(60.0, 100.0, 0.0, D50, narrow),
        lab_to_rgb(60.0, 10.0, 0.0, D50, narrow),
        "narrow /Range must clamp a*"
    );
}

#[test]
fn lab_inverted_range_does_not_panic() {
    // A malformed /Range with min > max must not panic (f32::clamp would).
    let bad = [100.0, -100.0, 50.0, -50.0];
    let rgb = lab_to_rgb(50.0, 0.0, 0.0, D50, bad);
    assert!(rgb.iter().all(|c| (0.0..=1.0).contains(c)), "{rgb:?}");
}

#[test]
fn lab_nan_inputs_are_tolerated() {
    // NaN/inf must degrade to a defined in-gamut colour, never NaN or panic.
    let rgb = lab_to_rgb(
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        D50,
        DEFAULT_RANGE,
    );
    assert!(
        rgb.iter().all(|c| c.is_finite() && (0.0..=1.0).contains(c)),
        "{rgb:?}"
    );
    // NaN L* is treated as 0 → black.
    assert_eq!(
        lab_to_rgb(f32::NAN, 0.0, 0.0, D50, DEFAULT_RANGE),
        [0.0, 0.0, 0.0]
    );
}

#[test]
fn calgray_is_pass_through() {
    // PDFium's CPDF_CalGray::GetRGB returns the component unchanged, ignoring
    // /Gamma. Pin that: gamma must not alter the result.
    assert_eq!(calgray_to_gray(0.0, 1.0), 0.0);
    assert_eq!(calgray_to_gray(1.0, 2.2), 1.0);
    assert_eq!(calgray_to_gray(0.42, 0.5), 0.42);
    assert_eq!(
        calgray_to_gray(0.42, 999.0),
        0.42,
        "gamma is ignored (pass-through)"
    );
    // Garbage clamps, never panics.
    assert_eq!(calgray_to_gray(f32::NAN, 1.0), 0.0);
    assert_eq!(calgray_to_gray(5.0, 1.0), 1.0);
    assert_eq!(calgray_to_gray(-5.0, 1.0), 0.0);
}

/// The identity matrix (`XYZ = ABC`).
const IDENTITY_MATRIX: [f32; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
/// D65 white point in XYZ.
const D65: [f32; 3] = [0.9505, 1.0, 1.089];
/// The sRGB → XYZ (D65) matrix, in CalRGB `/Matrix` column-per-component order
/// `[XA YA ZA XB YB ZB XC YC ZC]`. A "properly calibrated" CalRGB uses this.
const SRGB_TO_XYZ: [f32; 9] = [
    0.4124, 0.2126, 0.0193, 0.3576, 0.7152, 0.1192, 0.1805, 0.0722, 0.9505,
];

#[test]
fn calrgb_identity_params_are_not_raw_passthrough() {
    // The coordinator's hypothesis ("identity params reproduce the input") is
    // REFUTED by the ported math — verified, not assumed. Even with gamma 1
    // and the identity matrix, CalRGB treats ABC as XYZ, adapts to the white
    // point, and runs every channel through the sRGB transfer table.
    //
    // With white point (1,1,1), an achromatic input v=(0.5,0.5,0.5) survives
    // the linear stage unchanged (achromatic input == scalar·white ⇒ RGB=v),
    // but the sRGB transfer then lifts 0.5 → RGB_Conversion(0.5) = 187/255.
    // So the byte result is 187, NOT 128 — proof it is not a raw pass-through.
    let white_111 = [1.0, 1.0, 1.0];
    let mid = calrgb_to_rgb([0.5, 0.5, 0.5], white_111, [1.0; 3], IDENTITY_MATRIX);
    assert_eq!(
        px(mid),
        [187, 187, 187],
        "identity params gamma-encode, not pass through: {mid:?}"
    );

    // With the realistic D65 white, an identity matrix even adds a colour cast
    // (input treated as XYZ, adapted): (0.5,0.5,0.5) is no longer neutral.
    let cast = calrgb_to_rgb([0.5, 0.5, 0.5], D65, [1.0; 3], IDENTITY_MATRIX);
    assert_eq!(
        px(cast),
        [204, 183, 180],
        "identity matrix + D65 casts grey: {cast:?}"
    );
}

#[test]
fn calrgb_calibrated_white_is_srgb_white() {
    // A well-formed CalRGB — sRGB→XYZ matrix, gamma 1, D65 white — maps the
    // full-scale input to sRGB white, the point of the chromatic adaptation
    // (XYZ == white ⇒ RGB (1,1,1)).
    let w = calrgb_to_rgb([1.0, 1.0, 1.0], D65, [1.0; 3], SRGB_TO_XYZ);
    assert_eq!(
        px(w),
        [255, 255, 255],
        "calibrated white must be sRGB white: {w:?}"
    );
}

#[test]
fn calrgb_nontrivial_gamma_matrix_exact_bytes() {
    // Non-trivial case, hand-traced through the full ported chain:
    //   gamma 2.2:  a=0.25^2.2=0.04735, b=0.5^2.2=0.21764, c=0.75^2.2=0.53110
    //   sRGB→XYZ:   X=0.19319, Y=0.20405, Z=0.53166
    //   adapt(D65): matrix == sRGB→XYZ(D65) and white == D65, so M⁻¹·XYZ
    //               returns the linear RGB (a,b,c) back.
    //   sRGB xfer:  RGB_Conversion(0.04735)=kSRGBSamples1[48]=61
    //               RGB_Conversion(0.21764)=kSRGBSamples2[7]=128
    //               RGB_Conversion(0.53110)=kSRGBSamples2[87]=192
    // i.e. gamma-2.2-encoded content round-trips to ≈ its own byte values.
    let rgb = calrgb_to_rgb([0.25, 0.5, 0.75], D65, [2.2, 2.2, 2.2], SRGB_TO_XYZ);
    assert_eq!(
        px(rgb),
        [61, 128, 192],
        "non-trivial gamma/matrix mismatch: {rgb:?}"
    );
}

#[test]
fn calrgb_singular_matrix_is_black() {
    // PDFium's Matrix_3by3::Inverse returns the zero matrix on a singular
    // (sub-epsilon determinant) matrix, mapping everything to black. Mirror it.
    let rgb = calrgb_to_rgb([0.5, 0.5, 0.5], D65, [1.0; 3], [0.0; 9]);
    assert_eq!(
        rgb,
        [0.0, 0.0, 0.0],
        "singular /Matrix → black, got {rgb:?}"
    );
}

#[test]
fn calrgb_zero_whitepoint_and_nan_are_finite() {
    // A /WhitePoint with a zero component must not divide-by-zero into a NaN:
    // PDFium's determinant guard keeps the result finite; we mirror it.
    let zero_wp = calrgb_to_rgb(
        [0.5, 0.5, 0.5],
        [0.9505, 1.0, 0.0],
        [1.0; 3],
        IDENTITY_MATRIX,
    );
    assert!(
        zero_wp
            .iter()
            .all(|c| c.is_finite() && (0.0..=1.0).contains(c)),
        "{zero_wp:?}"
    );

    // NaN/inf inputs must degrade to a finite in-gamut colour, never a NaN
    // (house rule); a NaN component is treated as 0.
    let nanned = calrgb_to_rgb([f32::NAN, 0.5, 0.5], D65, [1.0; 3], IDENTITY_MATRIX);
    assert!(
        nanned
            .iter()
            .all(|c| c.is_finite() && (0.0..=1.0).contains(c)),
        "{nanned:?}"
    );
    let zeroed = calrgb_to_rgb([0.0, 0.5, 0.5], D65, [1.0; 3], IDENTITY_MATRIX);
    assert_eq!(nanned, zeroed, "NaN component must behave as 0");
    // A non-finite gamma is treated as the identity (1.0), not propagated.
    let nan_gamma = calrgb_to_rgb([0.5, 0.5, 0.5], D65, [f32::NAN, 1.0, 1.0], IDENTITY_MATRIX);
    assert!(nan_gamma.iter().all(|c| c.is_finite()), "{nan_gamma:?}");
}
