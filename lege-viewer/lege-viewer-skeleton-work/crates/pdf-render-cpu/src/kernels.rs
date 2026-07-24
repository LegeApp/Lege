//! The scalar kernel set — the isolated, contiguous arithmetic inner loops
//! (performance advice §7). Orchestration (clipping, paint dispatch, resource
//! lookup) happens *outside* these functions; a kernel receives contiguous
//! buffers and does nothing but math over a span.
//!
//! Each kernel is a simple counted loop with no iterator adapters, closures,
//! per-pixel format branches, or allocation — written to translate cleanly to
//! SIMD later. The public [`KernelSet`] is a table of function pointers chosen
//! **once** (per backend/worker), never per pixel; a SIMD build swaps the
//! table (`KernelSet::sse41()`, `::avx2()`, …) without touching the executor.
//!
//! Pixels are RGBA8 with **premultiplied alpha**, in memory byte order
//! `[R, G, B, A]`. Alpha math uses the shared [`mul_div_255`] primitive.

/// `round(a * b / 255)` for `a, b` in `0..=255`, exact and branchless.
#[inline(always)]
pub fn mul_div_255(a: u16, b: u16) -> u16 {
    let t = a * b + 128;
    (t + (t >> 8)) >> 8
}

/// Overwrite `dst` (one span, `len*4` bytes) with an opaque premultiplied
/// pixel. No destination read — the fast path for opaque, fully-covered spans.
pub fn fill_opaque(dst: &mut [u8], premul: [u8; 4]) {
    for px in dst.chunks_exact_mut(4) {
        px[0] = premul[0];
        px[1] = premul[1];
        px[2] = premul[2];
        px[3] = premul[3];
    }
}

/// Source-over a straight-alpha color `rgb` at constant coverage-alpha `a`
/// over a full span. Reads and writes premultiplied destination.
pub fn blend_const(dst: &mut [u8], rgb: [u8; 3], a: u8) {
    if a == 0 {
        return;
    }
    let ia = 255 - a as u16;
    let sr = mul_div_255(rgb[0] as u16, a as u16) as u8;
    let sg = mul_div_255(rgb[1] as u16, a as u16) as u8;
    let sb = mul_div_255(rgb[2] as u16, a as u16) as u8;
    for px in dst.chunks_exact_mut(4) {
        px[0] = sr + mul_div_255(px[0] as u16, ia) as u8;
        px[1] = sg + mul_div_255(px[1] as u16, ia) as u8;
        px[2] = sb + mul_div_255(px[2] as u16, ia) as u8;
        px[3] = a + mul_div_255(px[3] as u16, ia) as u8;
    }
}

/// Source-over a straight-alpha color `rgb` with per-pixel coverage `cov`,
/// scaled by constant `alpha`. `cov.len()` must equal the pixel count and
/// `dst.len()` must be `cov.len()*4`.
pub fn blend_mask(dst: &mut [u8], cov: &[u8], rgb: [u8; 3], alpha: u8) {
    debug_assert_eq!(dst.len(), cov.len() * 4);
    for (px, &c) in dst.chunks_exact_mut(4).zip(cov) {
        if c == 0 {
            continue;
        }
        let a = if alpha == 255 { c as u16 } else { mul_div_255(c as u16, alpha as u16) };
        if a == 0 {
            continue;
        }
        let ia = 255 - a;
        px[0] = (mul_div_255(rgb[0] as u16, a) + mul_div_255(px[0] as u16, ia)) as u8;
        px[1] = (mul_div_255(rgb[1] as u16, a) + mul_div_255(px[1] as u16, ia)) as u8;
        px[2] = (mul_div_255(rgb[2] as u16, a) + mul_div_255(px[2] as u16, ia)) as u8;
        px[3] = (a + mul_div_255(px[3] as u16, ia)) as u8;
    }
}

/// A separable per-channel blend function on straight components in `[0, 1]`
/// (ISO 32000-1 §11.3.5). Selected once per command, never branched per pixel.
pub type BlendFn = fn(f32, f32) -> f32;

/// The blend function for a mode, or `None` for `Normal` (use the fast
/// source-over path) and the non-separable modes (not yet implemented).
pub fn separable_blend(mode: pdf_page_ir::BlendMode) -> Option<BlendFn> {
    use pdf_page_ir::BlendMode as M;
    Some(match mode {
        M::Multiply => |b, s| b * s,
        M::Screen => |b, s| b + s - b * s,
        M::Overlay => |b, s| hard_light(s, b),
        M::Darken => |b, s| b.min(s),
        M::Lighten => |b, s| b.max(s),
        M::ColorDodge => |b, s| {
            if b == 0.0 {
                0.0
            } else if s >= 1.0 {
                1.0
            } else {
                (b / (1.0 - s)).min(1.0)
            }
        },
        M::ColorBurn => |b, s| {
            if b >= 1.0 {
                1.0
            } else if s <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - b) / s).min(1.0)
            }
        },
        M::HardLight => |b, s| hard_light(b, s),
        M::SoftLight => |b, s| soft_light(b, s),
        M::Difference => |b, s| (b - s).abs(),
        M::Exclusion => |b, s| b + s - 2.0 * b * s,
        // Normal → fast path; non-separable → deferred.
        _ => return None,
    })
}

fn hard_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 { b * (2.0 * s) } else { screen(b, 2.0 * s - 1.0) }
}

fn screen(b: f32, s: f32) -> f32 {
    b + s - b * s
}

fn soft_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        b - (1.0 - 2.0 * s) * b * (1.0 - b)
    } else {
        let d = if b <= 0.25 { ((16.0 * b - 12.0) * b + 4.0) * b } else { b.sqrt() };
        b + (2.0 * s - 1.0) * (d - b)
    }
}

/// A non-separable blend function on straight RGB triples in `[0, 1]`
/// (ISO 32000-1 §11.3.5.3 — Hue/Saturation/Color/Luminosity).
pub type NonSepBlendFn = fn([f32; 3], [f32; 3]) -> [f32; 3];

/// The non-separable blend function for a mode, or `None`.
pub fn nonseparable_blend(mode: pdf_page_ir::BlendMode) -> Option<NonSepBlendFn> {
    use pdf_page_ir::BlendMode as M;
    Some(match mode {
        M::Hue => |cb, cs| set_lum(set_sat(cs, sat(cb)), lum(cb)),
        M::Saturation => |cb, cs| set_lum(set_sat(cb, sat(cs)), lum(cb)),
        M::Color => |cb, cs| set_lum(cs, lum(cb)),
        M::Luminosity => |cb, cs| set_lum(cb, lum(cs)),
        _ => return None,
    })
}

fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn clip_color(mut c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    if n < 0.0 {
        for ch in &mut c {
            *ch = l + (*ch - l) * l / (l - n);
        }
    }
    if x > 1.0 {
        for ch in &mut c {
            *ch = l + (*ch - l) * (1.0 - l) / (x - l);
        }
    }
    c
}

fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    clip_color([c[0] + d, c[1] + d, c[2] + d])
}

fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_sat(mut c: [f32; 3], s: f32) -> [f32; 3] {
    // Indices of min/mid/max channels.
    let (mut lo, mut mid, mut hi) = (0usize, 1usize, 2usize);
    if c[lo] > c[mid] {
        std::mem::swap(&mut lo, &mut mid);
    }
    if c[mid] > c[hi] {
        std::mem::swap(&mut mid, &mut hi);
    }
    if c[lo] > c[mid] {
        std::mem::swap(&mut lo, &mut mid);
    }
    if c[hi] > c[lo] {
        c[mid] = (c[mid] - c[lo]) * s / (c[hi] - c[lo]);
        c[hi] = s;
    } else {
        c[mid] = 0.0;
        c[hi] = 0.0;
    }
    c[lo] = 0.0;
    c
}

/// Composite one premultiplied pixel: given the straight source `cs`, source
/// alpha `a_s`, backdrop alpha `da`, and the pre-computed blended straight
/// color `b`, write the premultiplied source-over result (ISO §11.3.6).
#[inline(always)]
pub(crate) fn composite_px(px: &mut [u8], cs: [f32; 3], a_s: f32, b: [f32; 3], da: f32) {
    for ch in 0..3 {
        let dc_pm = px[ch] as f32 / 255.0;
        let mixed = (1.0 - da) * cs[ch] + da * b[ch];
        let out = a_s * mixed + (1.0 - a_s) * dc_pm;
        px[ch] = (out * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    }
    let out_a = a_s + (1.0 - a_s) * da;
    px[3] = (out_a * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
}

/// General source-over composite of a straight color `rgb` with per-pixel
/// coverage `cov` (scaled by `alpha`) under a *separable* blend mode. The slow,
/// correct path (per-pixel un-premultiply + f32 blend); Normal-blend painting
/// never comes here (advice §5).
pub fn blend_span(dst: &mut [u8], cov: &[u8], rgb: [u8; 3], alpha: u8, blend: BlendFn) {
    debug_assert_eq!(dst.len(), cov.len() * 4);
    let cs = [rgb[0] as f32 / 255.0, rgb[1] as f32 / 255.0, rgb[2] as f32 / 255.0];
    for (px, &c) in dst.chunks_exact_mut(4).zip(cov) {
        if c == 0 {
            continue;
        }
        let a_s = c as f32 / 255.0 * (alpha as f32 / 255.0);
        if a_s <= 0.0 {
            continue;
        }
        let da = px[3] as f32 / 255.0;
        let inv_da = if da > 0.0 { 1.0 / da } else { 0.0 };
        let b = [
            blend(px[0] as f32 / 255.0 * inv_da, cs[0]),
            blend(px[1] as f32 / 255.0 * inv_da, cs[1]),
            blend(px[2] as f32 / 255.0 * inv_da, cs[2]),
        ];
        composite_px(px, cs, a_s, b, da);
    }
}

/// As [`blend_span`], for a *non-separable* blend mode (whole-triple blend).
pub fn blend_span_nonsep(dst: &mut [u8], cov: &[u8], rgb: [u8; 3], alpha: u8, blend: NonSepBlendFn) {
    debug_assert_eq!(dst.len(), cov.len() * 4);
    let cs = [rgb[0] as f32 / 255.0, rgb[1] as f32 / 255.0, rgb[2] as f32 / 255.0];
    for (px, &c) in dst.chunks_exact_mut(4).zip(cov) {
        if c == 0 {
            continue;
        }
        let a_s = c as f32 / 255.0 * (alpha as f32 / 255.0);
        if a_s <= 0.0 {
            continue;
        }
        let da = px[3] as f32 / 255.0;
        let inv_da = if da > 0.0 { 1.0 / da } else { 0.0 };
        let cb = [px[0] as f32 / 255.0 * inv_da, px[1] as f32 / 255.0 * inv_da, px[2] as f32 / 255.0 * inv_da];
        let b = blend(cb, cs);
        composite_px(px, cs, a_s, b, da);
    }
}

/// A table of the span kernels, selected once. Extended with image/glyph/mask
/// kernels in later checkpoints; a SIMD variant provides the same signatures.
#[derive(Debug, Clone, Copy)]
pub struct KernelSet {
    pub fill_opaque: fn(&mut [u8], [u8; 4]),
    pub blend_const: fn(&mut [u8], [u8; 3], u8),
    pub blend_mask: fn(&mut [u8], &[u8], [u8; 3], u8),
}

impl KernelSet {
    /// The portable scalar reference kernels.
    pub const fn scalar() -> Self {
        Self { fill_opaque, blend_const, blend_mask }
    }

    /// Choose the best kernel set for this CPU. Scalar only today; a SIMD
    /// build performs one-time runtime feature detection here.
    pub fn select() -> Self {
        Self::scalar()
    }
}

impl Default for KernelSet {
    fn default() -> Self {
        Self::scalar()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn mul_div_255_is_exact_at_extremes() {
        assert_eq!(mul_div_255(255, 255), 255);
        assert_eq!(mul_div_255(0, 255), 0);
        assert_eq!(mul_div_255(255, 0), 0);
        assert_eq!(mul_div_255(128, 255), 128);
        // 255*x/255 == x for all x.
        for x in 0..=255u16 {
            assert_eq!(mul_div_255(x, 255), x);
        }
    }

    #[test]
    fn blend_const_over_white_is_source_over() {
        // 50% red over opaque white → premultiplied (255,128,128,255).
        let mut dst = [255u8, 255, 255, 255];
        blend_const(&mut dst, [255, 0, 0], 128);
        assert_eq!(dst[0], 255);
        assert!((dst[1] as i32 - 128).abs() <= 1);
        assert!((dst[2] as i32 - 128).abs() <= 1);
        assert_eq!(dst[3], 255);
    }

    #[test]
    fn blend_mask_zero_coverage_is_noop() {
        let mut dst = [10u8, 20, 30, 40];
        blend_mask(&mut dst, &[0], [255, 255, 255], 255);
        assert_eq!(dst, [10, 20, 30, 40]);
    }

    #[test]
    fn fill_opaque_overwrites() {
        let mut dst = [0u8; 12];
        fill_opaque(&mut dst, [1, 2, 3, 255]);
        assert_eq!(dst, [1, 2, 3, 255, 1, 2, 3, 255, 1, 2, 3, 255]);
    }
}
