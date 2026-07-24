//! Backend-neutral postprocessing graph (roadmap §10 Stage C).
//!
//! The downstream pipeline (grayscale → tone → resize → binarize → 1-bit pack
//! for e-ink/archival output) is described once as data; the CPU executor
//! ([`CpuPostprocess`]) runs it today, and the GPU backend later executes the
//! same graph without readback between steps (Stage D: read back only the
//! packed 1-bit page).
//!
//! The graph operates on the frozen [`pdf_render_api::HostPage`] surface
//! contract: `Rgba8PremultipliedSrgb` (premultiplied RGBA; unpainted areas
//! composite over an implicit white paper backdrop) or `Gray8`. Ops apply in
//! order; each op documents which formats it accepts and what it produces.
//! Executors return typed errors — never panic — so a malformed pipeline
//! degrades to a reported failure, not a crashed page job.

use pdf_render_api::{HostPage, RenderedPage};

mod cpu;

pub use cpu::CpuPostprocess;

/// One postprocess operation. Parameter structs grow with implementations;
/// the *vocabulary* is fixed now so pipeline descriptions are stable.
#[derive(Debug, Clone)]
pub enum PostprocessOp {
    /// Crop to a pixel rectangle of the current surface (both formats).
    /// Pixels are copied 1:1 — resolution (dpi) is preserved.
    Crop(CropSpec),
    /// Resample to a new pixel size (both formats). Downscale uses
    /// area-correct fractional-tap weights (`ResizeFilter::Box` is an exact
    /// area average; the convolution filters scale their kernel support by
    /// the shrink ratio, `fast_image_resize`-style).
    Resize(ResizeSpec),
    /// RGBA → Gray8 luminance (premultiplied input is composited over white
    /// first, matching the renderer's paper backdrop). Gray8 input is a
    /// no-op.
    ConvertToGray(GraySpec),
    /// Map every sample through a 256-entry LUT (both formats; RGBA is
    /// un-premultiplied, mapped per color channel, re-premultiplied — alpha
    /// is never remapped). [`ToneCurve::invert`] and
    /// [`ToneCurve::brightness_contrast`] build the common curves.
    ApplyToneCurve(ToneCurve),
    /// Global Otsu threshold → bilevel Gray8 (0 / 255). Gray8 only.
    Otsu(OtsuSpec),
    /// Local Sauvola threshold → bilevel Gray8 (0 / 255). Gray8 only.
    Sauvola(SauvolaSpec),
    /// Per-pixel blend of the global Otsu and local Sauvola thresholds →
    /// bilevel Gray8 (0 / 255). Gray8 only.
    FuseThresholds(FusionSpec),
    /// Quantize Gray8 to bilevel (0 / 255) with optional error diffusion or
    /// ordered dithering. Gray8 only.
    Dither(DitherSpec),
    /// Pack Gray8 into 1-bit-per-pixel rows (MSB-first; **1 = black ink** —
    /// samples below 128 are ink). Must be the final op; the run then yields
    /// [`PostprocessOutput::PackedMono`]. Gray8 only.
    PackMonochrome,
}

/// Pixel rectangle for [`PostprocessOp::Crop`], in surface coordinates
/// (origin top-left). Must lie fully inside the current surface and be
/// non-empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropSpec {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct ResizeSpec {
    pub width: u32,
    pub height: u32,
    pub filter: ResizeFilter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeFilter {
    Nearest,
    /// Area average: every source pixel contributes proportionally to its
    /// overlap with the destination pixel's footprint — the correct default
    /// for scan minification (the same weighting as the renderer's image
    /// box filter).
    Box,
    Bilinear,
    CatmullRom,
    Lanczos3,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GraySpec {
    /// Use Rec.709 luma when false; equal-weight average when true.
    pub flat_weights: bool,
}

#[derive(Debug, Clone)]
pub struct ToneCurve {
    /// 256-entry LUT.
    pub lut: Box<[u8; 256]>,
}

impl ToneCurve {
    /// The identity curve.
    pub fn identity() -> Self {
        let mut lut = [0u8; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            *v = i as u8;
        }
        Self { lut: Box::new(lut) }
    }

    /// Photometric inversion: `v → 255 − v`.
    pub fn invert() -> Self {
        let mut lut = [0u8; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            *v = 255 - i as u8;
        }
        Self { lut: Box::new(lut) }
    }

    /// Linear brightness/contrast. `brightness` and `contrast` are in
    /// `[-1, 1]`; `(0, 0)` is identity. Contrast pivots around mid-gray:
    /// `v' = (v − 127.5)·(1 + contrast) + 127.5 + brightness·255`, clamped.
    pub fn brightness_contrast(brightness: f32, contrast: f32) -> Self {
        let gain = (1.0 + contrast.clamp(-1.0, 1.0)).max(0.0);
        let offset = brightness.clamp(-1.0, 1.0) * 255.0;
        let mut lut = [0u8; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            let out = (i as f32 - 127.5) * gain + 127.5 + offset;
            *v = out.round().clamp(0.0, 255.0) as u8;
        }
        Self { lut: Box::new(lut) }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OtsuSpec {}

#[derive(Debug, Clone, Copy)]
pub struct SauvolaSpec {
    /// Local window edge length in pixels (forced odd, minimum 3).
    pub window: u32,
    /// Sensitivity, typically `0.2 ..= 0.5`.
    pub k: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct FusionSpec {
    /// Weight of the global (Otsu) threshold vs the local (Sauvola) one,
    /// in `[0, 1]`: `t(x,y) = w·T_otsu + (1 − w)·t_sauvola(x, y)`.
    pub global_weight: f32,
    /// Sauvola window for the local component (forced odd, minimum 3).
    pub window: u32,
    /// Sauvola `k` for the local component.
    pub k: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DitherSpec {
    /// Hard threshold at 128 (`v ≥ 128` → white).
    None,
    FloydSteinberg,
    Bayer4,
}

/// An ordered pipeline of operations.
#[derive(Debug, Clone, Default)]
pub struct PostprocessGraph {
    pub ops: Vec<PostprocessOp>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PostprocessError {
    #[error("operation not supported by this executor: {0}")]
    Unsupported(&'static str),
    #[error("format mismatch: {0}")]
    FormatMismatch(&'static str),
    #[error("invalid operation parameters: {0}")]
    InvalidParams(&'static str),
    #[error("execution failure: {0}")]
    Failed(String),
}

/// Result of a postprocess run. `PackedMono` is the 1-bit page the encoding
/// stage consumes (240 KB at 1200×1600 vs 7.7 MB RGBA). Bits are MSB-first
/// within each byte; **1 = black ink**; rows are byte-aligned (`stride`
/// bytes each).
#[derive(Debug, Clone)]
pub enum PostprocessOutput {
    Page(RenderedPage),
    PackedMono {
        width: u32,
        height: u32,
        stride: usize,
        bits: std::sync::Arc<[u8]>,
    },
}

/// Executor contract, mirrored by CPU (Phase 6) and GPU (Stage C/D)
/// implementations.
pub trait PostprocessBackend: Send + Sync + std::fmt::Debug {
    fn supports(&self, graph: &PostprocessGraph) -> bool;
    fn execute(
        &self,
        source: &HostPage,
        graph: &PostprocessGraph,
    ) -> Result<PostprocessOutput, PostprocessError>;
}
