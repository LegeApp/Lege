//! The **frozen surface contract** (roadmap §7 Phase 6).
//!
//! Every backend — the CPU rasterizer now, WGPU later — must produce byte-
//! identical semantics for the conventions below. They are frozen: a new
//! backend adopts them exactly; it must not invent a different alpha or color
//! convention (roadmap §7: "Do not permit the CPU and GPU backends to silently
//! use different alpha or color-space conventions"). The CPU backend is the
//! normative reference, and its `tests/surface_contract.rs` asserts these on
//! exact pixels.
//!
//! # 1. Channel order
//! Output pixels are 4 bytes in memory order **`[R, G, B, A]`** (see
//! [`CHANNEL_ORDER`]). `Gray8` is one byte per pixel.
//!
//! # 2. Premultiplication
//! [`OutputFormat::Rgba8PremultipliedSrgb`](crate::OutputFormat) stores
//! **premultiplied** alpha: each color channel is already multiplied by alpha.
//! [`premultiply`] is the reference conversion from straight to premultiplied.
//! The internal compositing buffer is premultiplied throughout, so RGBA output
//! is a straight copy.
//!
//! # 3. Transfer function
//! Color values are **sRGB-encoded** (gamma space), and compositing happens
//! directly in that space — the convention every 8-bit PDF compositor uses.
//! Linear-light compositing is a deliberate non-goal of the frozen contract
//! (it would be an opt-in future format, not a silent change).
//!
//! # 4. Background application
//! The page background is applied *before* painting: `White` fills opaque
//! white, `Solid(c)` fills the premultiplied color, `Transparent` leaves the
//! surface clear (all zero). Painting then composites over it source-over.
//!
//! # 5. Rounding
//! Float→`u8` uses **round-half-up**: `(v * 255 + 0.5)` clamped to `0..=255`
//! (see [`round_unorm`]). Alpha blending uses the exact integer
//! `round(a * b / 255)` primitive. There is no dithering.
//!
//! # 6. Pixel-center convention
//! A device pixel `(x, y)` is sampled at its **center `(x + 0.5, y + 0.5)`**.
//! Image sampling and shading evaluation both use pixel centers.
//!
//! # 7. Clip-edge behavior
//! Rectangular clips are exact integer-edge rectangles. Non-rectangular clips
//! contribute an 8-bit analytic coverage mask that **multiplies** the paint
//! coverage per pixel (anti-aliased clip edges). Nested clips multiply.
//!
//! # 8. Image interpolation policy
//! Default sampling is **nearest-neighbor**; **bilinear** is used when the
//! image sets `/Interpolate true` (or the backend is asked for higher quality).
//! Samples are read at pixel centers (§6).
//!
//! # 9. Path antialiasing policy
//! Fills and strokes use **analytic exact-area coverage** (no supersampling):
//! a pixel's coverage is the exact fractional area the path covers, in both x
//! and y. This is deterministic — identical inputs yield identical bytes,
//! independent of worker count or render order (asserted by the determinism
//! tests). Stroke geometry is expanded to a fill and covered the same way.
//! There is no dithering, and coverage never depends on floating-point
//! summation order, so the AA is bit-reproducible across runs and machines.
//!
//! Glyphs are covered by the same rule, from **unhinted** outlines: glyph
//! geometry is exactly what the font describes. Grid-fitting moves that
//! geometry, so it is *not* part of the contract — it is an opt-in backend
//! setting (`CpuBackendOptions::hinting`, fonts.md Font Phase 4) that a
//! screen-facing caller enables for crisp small text. A backend that leaves
//! it at its default reproduces the contract exactly; a backend that enables
//! it must still be deterministic, and hinted output is only ever compared
//! against other hinted output at the same ppem.
//!
//! # 10. Device color conversion
//! Device color spaces convert to RGB by a single frozen policy shared by all
//! backends (`pdf_color::gray_to_rgb` / `cmyk_to_rgb`): Gray replicates the
//! component; **CMYK interpolates Adobe's measured 9×9×9×9 table**, the same
//! data and the same fixed-point arithmetic PDFium uses
//! (`pdf_color::cmyk`), and is bit-exact with it.
//!
//! This clause previously froze the naive `R = (1-C)(1-K)` formula. That was
//! wrong, not merely approximate: CMYK is ink, and no arithmetic turns ink
//! into light. It rendered `DeviceCMYK(1, 0.44, 0, 0)` as a searing
//! `rgb(0, 143, 255)` against PDFium's muted blue, on every CMYK page of every
//! document, and stood unnoticed until `tools/pdfium-diff` compared us to the
//! reference. The policy is *changed*, deliberately, and remains frozen: one
//! convention, shared by the CPU and future GPU backends, deterministic and
//! profile-free.
//!
//! Separation/DeviceN evaluate their tint transform into an alternate device
//! space and convert through this same policy. The remaining non-device spaces
//! (ICC/Lab/CalRGB) are approximated by component arity (documented deferral)
//! — never a per-backend choice. A real ICC path would be a separate,
//! explicitly-selected policy.

use crate::OutputFormat;

/// The in-memory channel order of an RGBA output pixel.
pub const CHANNEL_ORDER: [char; 4] = ['R', 'G', 'B', 'A'];

/// Round a unit-interval float to an 8-bit value with the frozen
/// round-half-up rule.
#[inline]
pub fn round_unorm(v: f32) -> u8 {
    (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

/// The exact integer alpha primitive `round(a * b / 255)` for `a, b` in
/// `0..=255` — the multiply every blend uses.
#[inline]
pub fn mul_unorm(a: u16, b: u16) -> u16 {
    let t = a * b + 128;
    (t + (t >> 8)) >> 8
}

/// Reference conversion of a straight-alpha RGBA8 pixel to the frozen
/// premultiplied form.
#[inline]
pub fn premultiply(straight: [u8; 4]) -> [u8; 4] {
    let a = straight[3] as u16;
    [
        mul_unorm(straight[0] as u16, a) as u8,
        mul_unorm(straight[1] as u16, a) as u8,
        mul_unorm(straight[2] as u16, a) as u8,
        straight[3],
    ]
}

impl OutputFormat {
    /// Whether this format stores premultiplied alpha (part of the frozen
    /// contract, not a per-backend choice).
    pub fn is_premultiplied(self) -> bool {
        matches!(self, OutputFormat::Rgba8PremultipliedSrgb)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn round_half_up() {
        assert_eq!(round_unorm(0.0), 0);
        assert_eq!(round_unorm(1.0), 255);
        // 0.5/255 boundary rounds up.
        assert_eq!(round_unorm(0.5), 128);
        assert_eq!(round_unorm(2.0), 255, "clamps above 1.0");
        assert_eq!(round_unorm(-1.0), 0, "clamps below 0.0");
    }

    #[test]
    fn mul_unorm_is_exact_at_extremes() {
        assert_eq!(mul_unorm(255, 255), 255);
        assert_eq!(mul_unorm(0, 255), 0);
        assert_eq!(mul_unorm(128, 255), 128);
        for x in 0..=255u16 {
            assert_eq!(mul_unorm(x, 255), x);
        }
    }

    #[test]
    fn premultiply_reference() {
        // Opaque is unchanged.
        assert_eq!(premultiply([255, 128, 0, 255]), [255, 128, 0, 255]);
        // Fully transparent zeroes the color.
        assert_eq!(premultiply([255, 255, 255, 0]), [0, 0, 0, 0]);
        // 50% red.
        assert_eq!(premultiply([255, 0, 0, 128]), [128, 0, 0, 128]);
    }

    #[test]
    fn channel_order_is_rgba() {
        assert_eq!(CHANNEL_ORDER, ['R', 'G', 'B', 'A']);
        assert!(OutputFormat::Rgba8PremultipliedSrgb.is_premultiplied());
        assert!(!OutputFormat::Gray8.is_premultiplied());
    }
}
