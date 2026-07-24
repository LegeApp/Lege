//! The CPU postprocess executor (Phase 6 / Stage C reference
//! implementation).
//!
//! Every op is implemented with plain scalar loops over a tightly-packed
//! working raster. The executor never panics: malformed inputs and
//! out-of-range parameters return typed [`PostprocessError`]s.

use std::sync::Arc;

use pdf_render_api::{HostPage, OutputFormat, RenderedPage};

use crate::{
    CropSpec, DitherSpec, FusionSpec, GraySpec, OtsuSpec, PostprocessBackend, PostprocessError,
    PostprocessGraph, PostprocessOp, PostprocessOutput, ResizeFilter, ResizeSpec, SauvolaSpec,
    ToneCurve,
};

/// CPU implementation of [`PostprocessBackend`]. Stateless and cheap to
/// construct.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuPostprocess;

impl PostprocessBackend for CpuPostprocess {
    fn supports(&self, graph: &PostprocessGraph) -> bool {
        validate(graph).is_ok()
    }

    fn execute(
        &self,
        source: &HostPage,
        graph: &PostprocessGraph,
    ) -> Result<PostprocessOutput, PostprocessError> {
        validate(graph)?;
        let mut raster = Raster::from_host(source)?;
        for op in &graph.ops {
            match op {
                PostprocessOp::Crop(spec) => raster = crop(&raster, spec)?,
                PostprocessOp::Resize(spec) => raster = resize(&raster, spec)?,
                PostprocessOp::ConvertToGray(spec) => raster = to_gray(&raster, spec),
                PostprocessOp::ApplyToneCurve(curve) => tone_curve(&mut raster, curve),
                PostprocessOp::Otsu(spec) => otsu(&mut raster, spec)?,
                PostprocessOp::Sauvola(spec) => sauvola(&mut raster, spec)?,
                PostprocessOp::FuseThresholds(spec) => fuse_thresholds(&mut raster, spec)?,
                PostprocessOp::Dither(spec) => dither(&mut raster, spec)?,
                PostprocessOp::PackMonochrome => {
                    // `validate` guarantees this is the final op.
                    return pack_monochrome(&raster);
                }
            }
        }
        Ok(PostprocessOutput::Page(RenderedPage::Host(
            raster.into_host(),
        )))
    }
}

/// Graph-shape checks shared by `supports` and `execute`.
fn validate(graph: &PostprocessGraph) -> Result<(), PostprocessError> {
    for (i, op) in graph.ops.iter().enumerate() {
        match op {
            PostprocessOp::PackMonochrome if i + 1 != graph.ops.len() => {
                return Err(PostprocessError::InvalidParams(
                    "PackMonochrome must be the final operation",
                ));
            }
            PostprocessOp::Crop(c) if c.width == 0 || c.height == 0 => {
                return Err(PostprocessError::InvalidParams("empty crop rectangle"));
            }
            PostprocessOp::Resize(r) if r.width == 0 || r.height == 0 => {
                return Err(PostprocessError::InvalidParams("empty resize target"));
            }
            PostprocessOp::FuseThresholds(f)
                if !(0.0..=1.0).contains(&f.global_weight) || !f.k.is_finite() =>
            {
                return Err(PostprocessError::InvalidParams(
                    "fusion global_weight must be in [0, 1] and k finite",
                ));
            }
            PostprocessOp::Sauvola(s) if !s.k.is_finite() => {
                return Err(PostprocessError::InvalidParams("sauvola k must be finite"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// The executor's working surface: tightly packed (`stride == width · bpp`).
#[derive(Debug, Clone)]
struct Raster {
    width: u32,
    height: u32,
    format: OutputFormat,
    data: Vec<u8>,
}

impl Raster {
    fn bpp(&self) -> usize {
        self.format.bytes_per_pixel()
    }

    fn from_host(page: &HostPage) -> Result<Self, PostprocessError> {
        let bpp = page.format.bytes_per_pixel();
        let (w, h) = (page.width as usize, page.height as usize);
        if w == 0 || h == 0 {
            return Err(PostprocessError::FormatMismatch("empty source page"));
        }
        let row_bytes = w
            .checked_mul(bpp)
            .ok_or(PostprocessError::FormatMismatch("source row overflow"))?;
        if page.stride < row_bytes {
            return Err(PostprocessError::FormatMismatch(
                "source stride shorter than a pixel row",
            ));
        }
        let needed = page
            .stride
            .checked_mul(h - 1)
            .and_then(|v| v.checked_add(row_bytes))
            .ok_or(PostprocessError::FormatMismatch("source size overflow"))?;
        if page.pixels.len() < needed {
            return Err(PostprocessError::FormatMismatch(
                "source pixel buffer shorter than stride × height",
            ));
        }
        let mut data = Vec::with_capacity(row_bytes * h);
        for row in 0..h {
            let start = row * page.stride;
            data.extend_from_slice(&page.pixels[start..start + row_bytes]);
        }
        Ok(Self {
            width: page.width,
            height: page.height,
            format: page.format,
            data,
        })
    }

    fn into_host(self) -> HostPage {
        let stride = self.width as usize * self.bpp();
        HostPage {
            width: self.width,
            height: self.height,
            stride,
            format: self.format,
            pixels: Arc::from(self.data.into_boxed_slice()),
        }
    }

    fn require_gray(&self, what: &'static str) -> Result<(), PostprocessError> {
        if self.format == OutputFormat::Gray8 {
            Ok(())
        } else {
            let _ = what;
            Err(PostprocessError::FormatMismatch(
                "operation requires a Gray8 surface (insert ConvertToGray first)",
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Crop

fn crop(src: &Raster, spec: &CropSpec) -> Result<Raster, PostprocessError> {
    let x1 = spec
        .x
        .checked_add(spec.width)
        .ok_or(PostprocessError::InvalidParams("crop overflow"))?;
    let y1 = spec
        .y
        .checked_add(spec.height)
        .ok_or(PostprocessError::InvalidParams("crop overflow"))?;
    if x1 > src.width || y1 > src.height {
        return Err(PostprocessError::InvalidParams(
            "crop rectangle exceeds the surface",
        ));
    }
    let bpp = src.bpp();
    let src_row = src.width as usize * bpp;
    let out_row = spec.width as usize * bpp;
    let mut data = Vec::with_capacity(out_row * spec.height as usize);
    for row in spec.y..y1 {
        let start = row as usize * src_row + spec.x as usize * bpp;
        data.extend_from_slice(&src.data[start..start + out_row]);
    }
    Ok(Raster {
        width: spec.width,
        height: spec.height,
        format: src.format,
        data,
    })
}

// ---------------------------------------------------------------------------
// Grayscale conversion

fn to_gray(src: &Raster, spec: &GraySpec) -> Raster {
    if src.format == OutputFormat::Gray8 {
        return src.clone();
    }
    let mut data = Vec::with_capacity(src.width as usize * src.height as usize);
    for px in src.data.chunks_exact(4) {
        // Premultiplied over the white paper backdrop: c' = c + (255 − a).
        let paper = 255 - px[3] as u32;
        let r = px[0] as u32 + paper;
        let g = px[1] as u32 + paper;
        let b = px[2] as u32 + paper;
        let y = if spec.flat_weights {
            (r + g + b + 1) / 3
        } else {
            // Rec.709 luma, integer form (weights sum to 10 000).
            (2126 * r + 7152 * g + 722 * b + 5000) / 10000
        };
        data.push(y.min(255) as u8);
    }
    Raster {
        width: src.width,
        height: src.height,
        format: OutputFormat::Gray8,
        data,
    }
}

// ---------------------------------------------------------------------------
// Tone curve

fn tone_curve(raster: &mut Raster, curve: &ToneCurve) {
    match raster.format {
        OutputFormat::Gray8 => {
            for v in &mut raster.data {
                *v = curve.lut[*v as usize];
            }
        }
        OutputFormat::Rgba8PremultipliedSrgb => {
            for px in raster.data.chunks_exact_mut(4) {
                let a = px[3] as u32;
                if a == 0 {
                    continue;
                }
                for c in px.iter_mut().take(3) {
                    // Un-premultiply (round-to-nearest), map, re-premultiply.
                    let straight = ((*c as u32 * 255 + a / 2) / a).min(255);
                    let mapped = curve.lut[straight as usize] as u32;
                    *c = ((mapped * a + 127) / 255) as u8;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Resize

/// Reconstruction kernels. `support` is in destination-pixel units before
/// downscale stretching.
fn kernel_support(filter: ResizeFilter) -> f32 {
    match filter {
        ResizeFilter::Nearest => 0.5,
        ResizeFilter::Box => 0.5,
        ResizeFilter::Bilinear => 1.0,
        ResizeFilter::CatmullRom => 2.0,
        ResizeFilter::Lanczos3 => 3.0,
    }
}

fn kernel_eval(filter: ResizeFilter, t: f32) -> f32 {
    let t = t.abs();
    match filter {
        ResizeFilter::Nearest | ResizeFilter::Box => {
            if t <= 0.5 { 1.0 } else { 0.0 }
        }
        ResizeFilter::Bilinear => (1.0 - t).max(0.0),
        ResizeFilter::CatmullRom => {
            // Catmull-Rom spline (B = 0, C = 0.5).
            if t < 1.0 {
                1.5 * t * t * t - 2.5 * t * t + 1.0
            } else if t < 2.0 {
                -0.5 * t * t * t + 2.5 * t * t - 4.0 * t + 2.0
            } else {
                0.0
            }
        }
        ResizeFilter::Lanczos3 => {
            if t < 1e-6 {
                1.0
            } else if t < 3.0 {
                let pt = std::f32::consts::PI * t;
                3.0 * (pt.sin() / pt) * ((pt / 3.0).sin() / (pt / 3.0))
            } else {
                0.0
            }
        }
    }
}

/// Per-destination-index tap list: first source index + normalized weights.
struct Taps {
    start: usize,
    weights: Vec<f32>,
}

/// Compute one axis's taps. For downscale (`ratio > 1`) the kernel is
/// stretched by the shrink ratio so every covered source pixel contributes
/// fractionally by overlap — `Box` then degenerates to an exact area
/// average, which is the same weighting the renderer's minification filter
/// uses.
fn axis_taps(src_len: u32, dst_len: u32, filter: ResizeFilter) -> Vec<Taps> {
    let ratio = src_len as f32 / dst_len as f32;
    let stretch = ratio.max(1.0);
    let support = kernel_support(filter) * stretch;
    let mut all = Vec::with_capacity(dst_len as usize);
    for d in 0..dst_len {
        // Destination pixel center in source coordinates (edge space).
        let center = (d as f32 + 0.5) * ratio;
        let overlap_mode = matches!(filter, ResizeFilter::Box | ResizeFilter::Nearest);
        // Overlap (area) taps cover every source pixel intersecting the
        // footprint; point-sample taps cover source centers within support.
        let (lo, hi) = if overlap_mode {
            (
                (center - support).floor() as i64,
                (center + support).ceil() as i64 - 1,
            )
        } else {
            (
                (center - support + 0.5).floor() as i64,
                (center + support - 0.5).ceil() as i64,
            )
        };
        let lo_c = lo.clamp(0, src_len as i64 - 1);
        let hi_c = hi.clamp(lo_c, src_len as i64 - 1);
        let mut weights = Vec::with_capacity((hi_c - lo_c + 1) as usize);
        let mut sum = 0.0f32;
        for s in lo_c..=hi_c {
            // `Box`'s hard-edged kernel needs the *overlap* of the source
            // pixel [s, s+1) with the footprint, not a point sample, to be
            // an exact area average; smooth kernels use the center sample.
            let w = if overlap_mode {
                let left = (center - support).max(s as f32);
                let right = (center + support).min(s as f32 + 1.0);
                (right - left).max(0.0)
            } else {
                kernel_eval(filter, (s as f32 + 0.5 - center) / stretch)
            };
            weights.push(w);
            sum += w;
        }
        if sum <= f32::EPSILON {
            // Degenerate footprint: fall back to the nearest source pixel.
            let nearest = (center - 0.5).round().clamp(0.0, src_len as f32 - 1.0) as usize;
            all.push(Taps {
                start: nearest,
                weights: vec![1.0],
            });
            continue;
        }
        for w in &mut weights {
            *w /= sum;
        }
        all.push(Taps {
            start: lo_c as usize,
            weights,
        });
    }
    all
}

fn resize(src: &Raster, spec: &ResizeSpec) -> Result<Raster, PostprocessError> {
    let ch = src.bpp();
    if spec.width == src.width && spec.height == src.height {
        return Ok(src.clone());
    }
    if spec.filter == ResizeFilter::Nearest {
        return Ok(resize_nearest(src, spec.width, spec.height));
    }
    let (sw, sh) = (src.width as usize, src.height as usize);
    let (dw, dh) = (spec.width as usize, spec.height as usize);
    let h_taps = axis_taps(src.width, spec.width, spec.filter);
    let v_taps = axis_taps(src.height, spec.height, spec.filter);

    // Horizontal pass: src (sw × sh) → mid (dw × sh), f32 channels.
    let mut mid = vec![0.0f32; dw * sh * ch];
    for row in 0..sh {
        let src_row = &src.data[row * sw * ch..(row + 1) * sw * ch];
        let mid_row = &mut mid[row * dw * ch..(row + 1) * dw * ch];
        for (d, taps) in h_taps.iter().enumerate() {
            let mut acc = [0.0f32; 4];
            for (i, &w) in taps.weights.iter().enumerate() {
                let s = (taps.start + i).min(sw - 1) * ch;
                for c in 0..ch {
                    acc[c] += w * src_row[s + c] as f32;
                }
            }
            mid_row[d * ch..d * ch + ch].copy_from_slice(&acc[..ch]);
        }
    }

    // Vertical pass: mid (dw × sh) → dst (dw × dh), rounded to u8.
    let mut data = vec![0u8; dw * dh * ch];
    for (d, taps) in v_taps.iter().enumerate() {
        let dst_row = &mut data[d * dw * ch..(d + 1) * dw * ch];
        for x in 0..dw {
            let mut acc = [0.0f32; 4];
            for (i, &w) in taps.weights.iter().enumerate() {
                let s = (taps.start + i).min(sh - 1);
                let base = s * dw * ch + x * ch;
                for c in 0..ch {
                    acc[c] += w * mid[base + c];
                }
            }
            let out = &mut dst_row[x * ch..x * ch + ch];
            for c in 0..ch {
                out[c] = acc[c].round().clamp(0.0, 255.0) as u8;
            }
            // Negative-lobe kernels can break the premultiplied invariant
            // (color > alpha); restore it so downstream ops stay valid.
            if ch == 4 {
                let a = out[3];
                for c in out.iter_mut().take(3) {
                    *c = (*c).min(a);
                }
            }
        }
    }
    Ok(Raster {
        width: spec.width,
        height: spec.height,
        format: src.format,
        data,
    })
}

fn resize_nearest(src: &Raster, dw: u32, dh: u32) -> Raster {
    let ch = src.bpp();
    let (sw, sh) = (src.width as usize, src.height as usize);
    let mut data = Vec::with_capacity(dw as usize * dh as usize * ch);
    for y in 0..dh {
        let sy = (((y as f64 + 0.5) * sh as f64 / dh as f64) as usize).min(sh - 1);
        let row = &src.data[sy * sw * ch..(sy + 1) * sw * ch];
        for x in 0..dw {
            let sx = (((x as f64 + 0.5) * sw as f64 / dw as f64) as usize).min(sw - 1);
            data.extend_from_slice(&row[sx * ch..sx * ch + ch]);
        }
    }
    Raster {
        width: dw,
        height: dh,
        format: src.format,
        data,
    }
}

// ---------------------------------------------------------------------------
// Thresholding

/// Otsu's global threshold over a Gray8 histogram. Returns the class
/// boundary `t`: samples `> t` are white. Deterministic (first maximum
/// wins).
fn otsu_threshold(data: &[u8]) -> u8 {
    let mut hist = [0u64; 256];
    for &v in data {
        hist[v as usize] += 1;
    }
    let total: u64 = data.len() as u64;
    if total == 0 {
        return 127;
    }
    let sum_all: u64 = hist
        .iter()
        .enumerate()
        .map(|(v, &n)| v as u64 * n)
        .sum::<u64>();
    let mut w0 = 0u64; // background weight
    let mut sum0 = 0u64;
    let mut best_t = 127u8;
    let mut best_var = -1.0f64;
    for (t, &n) in hist.iter().enumerate().take(255) {
        w0 += n;
        if w0 == 0 {
            continue;
        }
        let w1 = total - w0;
        if w1 == 0 {
            break;
        }
        sum0 += t as u64 * n;
        let m0 = sum0 as f64 / w0 as f64;
        let m1 = (sum_all - sum0) as f64 / w1 as f64;
        let var = w0 as f64 * w1 as f64 * (m0 - m1) * (m0 - m1);
        if var > best_var {
            best_var = var;
            best_t = t as u8;
        }
    }
    best_t
}

fn otsu(raster: &mut Raster, _spec: &OtsuSpec) -> Result<(), PostprocessError> {
    raster.require_gray("Otsu")?;
    let t = otsu_threshold(&raster.data);
    for v in &mut raster.data {
        *v = if *v > t { 255 } else { 0 };
    }
    Ok(())
}

/// Integral images (sum, sum of squares) with a leading zero row/column so
/// any window is four reads. Sized `(w + 1) × (h + 1)`.
struct Integral {
    sum: Vec<u64>,
    sq: Vec<u64>,
    stride: usize,
}

impl Integral {
    fn build(data: &[u8], w: usize, h: usize) -> Self {
        let stride = w + 1;
        let mut sum = vec![0u64; stride * (h + 1)];
        let mut sq = vec![0u64; stride * (h + 1)];
        for y in 0..h {
            let mut row_sum = 0u64;
            let mut row_sq = 0u64;
            for x in 0..w {
                let v = data[y * w + x] as u64;
                row_sum += v;
                row_sq += v * v;
                let i = (y + 1) * stride + x + 1;
                sum[i] = sum[i - stride] + row_sum;
                sq[i] = sq[i - stride] + row_sq;
            }
        }
        Self { sum, sq, stride }
    }

    /// Mean and standard deviation of the inclusive pixel window
    /// `[x0, x1] × [y0, y1]`.
    fn stats(&self, x0: usize, x1: usize, y0: usize, y1: usize) -> (f64, f64) {
        let s = self.stride;
        let read = |t: &Vec<u64>| {
            (t[(y1 + 1) * s + x1 + 1] + t[y0 * s + x0]) as f64
                - (t[y0 * s + x1 + 1] + t[(y1 + 1) * s + x0]) as f64
        };
        let n = ((x1 - x0 + 1) * (y1 - y0 + 1)) as f64;
        let mean = read(&self.sum) / n;
        let var = (read(&self.sq) / n - mean * mean).max(0.0);
        (mean, var.sqrt())
    }
}

/// Sauvola's local threshold: `t = m·(1 + k·(s/128 − 1))`.
fn sauvola_map(
    data: &[u8],
    w: usize,
    h: usize,
    window: u32,
    k: f32,
    mut threshold: impl FnMut(usize, f64) -> u8,
) {
    let win = (window.max(3) | 1) as usize; // odd, ≥ 3
    let half = win / 2;
    let integral = Integral::build(data, w, h);
    let mut out_idx = 0usize;
    for y in 0..h {
        let y0 = y.saturating_sub(half);
        let y1 = (y + half).min(h - 1);
        for x in 0..w {
            let x0 = x.saturating_sub(half);
            let x1 = (x + half).min(w - 1);
            let (mean, std) = integral.stats(x0, x1, y0, y1);
            let t = mean * (1.0 + k as f64 * (std / 128.0 - 1.0));
            let _ = threshold(out_idx, t);
            out_idx += 1;
        }
    }
}

fn sauvola(raster: &mut Raster, spec: &SauvolaSpec) -> Result<(), PostprocessError> {
    raster.require_gray("Sauvola")?;
    let (w, h) = (raster.width as usize, raster.height as usize);
    let src = raster.data.clone();
    let data = &mut raster.data;
    sauvola_map(&src, w, h, spec.window, spec.k, |i, t| {
        data[i] = if (src[i] as f64) > t { 255 } else { 0 };
        data[i]
    });
    Ok(())
}

fn fuse_thresholds(raster: &mut Raster, spec: &FusionSpec) -> Result<(), PostprocessError> {
    raster.require_gray("FuseThresholds")?;
    let (w, h) = (raster.width as usize, raster.height as usize);
    let global_t = otsu_threshold(&raster.data) as f64;
    let gw = spec.global_weight as f64;
    let src = raster.data.clone();
    let data = &mut raster.data;
    sauvola_map(&src, w, h, spec.window, spec.k, |i, local_t| {
        let t = gw * global_t + (1.0 - gw) * local_t;
        data[i] = if (src[i] as f64) > t { 255 } else { 0 };
        data[i]
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Dithering

/// Bayer 4×4 ordered-dither matrix (values 0..16).
const BAYER4: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

fn dither(raster: &mut Raster, spec: &DitherSpec) -> Result<(), PostprocessError> {
    raster.require_gray("Dither")?;
    let (w, h) = (raster.width as usize, raster.height as usize);
    match spec {
        DitherSpec::None => {
            for v in &mut raster.data {
                *v = if *v >= 128 { 255 } else { 0 };
            }
        }
        DitherSpec::Bayer4 => {
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    // Threshold in (0, 255): cell (m + 0.5) / 16 · 255.
                    let t = ((BAYER4[y & 3][x & 3] as u16 * 2 + 1) * 255 / 32) as u8;
                    raster.data[i] = if raster.data[i] > t { 255 } else { 0 };
                }
            }
        }
        DitherSpec::FloydSteinberg => {
            // Classic raster-order error diffusion (7/16, 3/16, 5/16, 1/16).
            let mut err = vec![0i32; w * h];
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    let v = raster.data[i] as i32 + err[i] / 16;
                    let out = if v >= 128 { 255 } else { 0 };
                    raster.data[i] = out as u8;
                    let e = v - out;
                    if x + 1 < w {
                        err[i + 1] += e * 7;
                    }
                    if y + 1 < h {
                        if x > 0 {
                            err[i + w - 1] += e * 3;
                        }
                        err[i + w] += e * 5;
                        if x + 1 < w {
                            err[i + w + 1] += e;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 1-bit packing

fn pack_monochrome(raster: &Raster) -> Result<PostprocessOutput, PostprocessError> {
    raster.require_gray("PackMonochrome")?;
    let (w, h) = (raster.width as usize, raster.height as usize);
    let stride = w.div_ceil(8);
    let mut bits = vec![0u8; stride * h];
    for y in 0..h {
        let row = &raster.data[y * w..(y + 1) * w];
        let out = &mut bits[y * stride..(y + 1) * stride];
        for (x, &v) in row.iter().enumerate() {
            if v < 128 {
                out[x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    Ok(PostprocessOutput::PackedMono {
        width: raster.width,
        height: raster.height,
        stride,
        bits: Arc::from(bits.into_boxed_slice()),
    })
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::PostprocessOutput;

    fn gray_page(width: u32, height: u32, pixels: &[u8]) -> HostPage {
        assert_eq!(pixels.len(), (width * height) as usize);
        HostPage {
            width,
            height,
            stride: width as usize,
            format: OutputFormat::Gray8,
            pixels: Arc::from(pixels.to_vec().into_boxed_slice()),
        }
    }

    fn rgba_page(width: u32, height: u32, pixels: &[u8]) -> HostPage {
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        HostPage {
            width,
            height,
            stride: width as usize * 4,
            format: OutputFormat::Rgba8PremultipliedSrgb,
            pixels: Arc::from(pixels.to_vec().into_boxed_slice()),
        }
    }

    fn run(page: &HostPage, ops: Vec<PostprocessOp>) -> PostprocessOutput {
        CpuPostprocess
            .execute(page, &PostprocessGraph { ops })
            .expect("pipeline must succeed")
    }

    fn out_gray(out: &PostprocessOutput) -> (u32, u32, Vec<u8>) {
        match out {
            PostprocessOutput::Page(RenderedPage::Host(h)) => {
                assert_eq!(h.format, OutputFormat::Gray8);
                (h.width, h.height, h.pixels.to_vec())
            }
            other => panic!("expected a Gray8 page, got {other:?}"),
        }
    }

    #[test]
    fn convert_to_gray_rec709_and_flat() {
        // Opaque premultiplied red, green, blue, white.
        let page = rgba_page(
            4,
            1,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        );
        let out = run(
            &page,
            vec![PostprocessOp::ConvertToGray(GraySpec { flat_weights: false })],
        );
        let (_, _, px) = out_gray(&out);
        assert_eq!(px, vec![54, 182, 18, 255]); // Rec.709 weights
        let out = run(
            &page,
            vec![PostprocessOp::ConvertToGray(GraySpec { flat_weights: true })],
        );
        let (_, _, px) = out_gray(&out);
        assert_eq!(px, vec![85, 85, 85, 255]);
    }

    #[test]
    fn convert_to_gray_composites_transparent_over_white() {
        // Fully transparent pixel must read as paper white.
        let page = rgba_page(1, 1, &[0, 0, 0, 0]);
        let (_, _, px) = out_gray(&run(
            &page,
            vec![PostprocessOp::ConvertToGray(GraySpec::default())],
        ));
        assert_eq!(px, vec![255]);
    }

    #[test]
    fn tone_curve_invert_gray() {
        let page = gray_page(3, 1, &[0, 100, 255]);
        let (_, _, px) = out_gray(&run(
            &page,
            vec![PostprocessOp::ApplyToneCurve(ToneCurve::invert())],
        ));
        assert_eq!(px, vec![255, 155, 0]);
    }

    #[test]
    fn tone_curve_rgba_ignores_alpha_and_respects_premultiplication() {
        // Half-transparent mid-gray: premultiplied (64, 64, 64, 128) is
        // straight 127-ish; inversion must invert color, not alpha.
        let page = rgba_page(1, 1, &[64, 64, 64, 128]);
        let out = run(
            &page,
            vec![PostprocessOp::ApplyToneCurve(ToneCurve::invert())],
        );
        match out {
            PostprocessOutput::Page(RenderedPage::Host(h)) => {
                let px = h.pixels.to_vec();
                assert_eq!(px[3], 128, "alpha must be untouched");
                // straight ≈ 128 (64·255/128 = 127.5 → 128), inverted = 127,
                // re-premultiplied = 127·128/255 ≈ 64.
                assert_eq!(px[0], 64);
            }
            other => panic!("unexpected output {other:?}"),
        }
    }

    #[test]
    fn brightness_contrast_curve_endpoints() {
        let c = ToneCurve::brightness_contrast(0.0, 0.0);
        assert_eq!(c.lut[0], 0);
        assert_eq!(c.lut[255], 255);
        assert_eq!(c.lut[128], 128);
        let brighter = ToneCurve::brightness_contrast(0.2, 0.0);
        assert_eq!(brighter.lut[0], 51);
        assert_eq!(brighter.lut[255], 255); // clamped
        let contrasty = ToneCurve::brightness_contrast(0.0, 1.0);
        assert_eq!(contrasty.lut[0], 0);
        assert_eq!(contrasty.lut[64], 1); // (64−127.5)·2+127.5 = 0.5 → 1
        assert_eq!(contrasty.lut[255], 255);
    }

    #[test]
    fn crop_preserves_pixels_and_size() {
        #[rustfmt::skip]
        let page = gray_page(4, 3, &[
            0, 1, 2, 3,
            4, 5, 6, 7,
            8, 9, 10, 11,
        ]);
        let (w, h, px) = out_gray(&run(
            &page,
            vec![PostprocessOp::Crop(CropSpec { x: 1, y: 1, width: 2, height: 2 })],
        ));
        assert_eq!((w, h), (2, 2));
        assert_eq!(px, vec![5, 6, 9, 10]);
    }

    #[test]
    fn crop_out_of_bounds_is_a_typed_error() {
        let page = gray_page(2, 2, &[0, 0, 0, 0]);
        let err = CpuPostprocess
            .execute(
                &page,
                &PostprocessGraph {
                    ops: vec![PostprocessOp::Crop(CropSpec { x: 1, y: 0, width: 2, height: 1 })],
                },
            )
            .unwrap_err();
        assert!(matches!(err, PostprocessError::InvalidParams(_)));
    }

    #[test]
    fn box_downscale_is_exact_area_average() {
        // 4×2 → 2×1: each output pixel averages a 2×2 block exactly.
        #[rustfmt::skip]
        let page = gray_page(4, 2, &[
            0, 100, 200, 40,
            50, 250, 60, 100,
        ]);
        let (w, h, px) = out_gray(&run(
            &page,
            vec![PostprocessOp::Resize(ResizeSpec {
                width: 2,
                height: 1,
                filter: ResizeFilter::Box,
            })],
        ));
        assert_eq!((w, h), (2, 1));
        assert_eq!(px, vec![100, 100]); // (0+100+50+250)/4, (200+40+60+100)/4
    }

    #[test]
    fn box_downscale_fractional_taps_weight_by_overlap() {
        // 3 → 2: output 0 covers [0, 1.5) = pixel0 + half of pixel1.
        let page = gray_page(3, 1, &[30, 90, 150]);
        let (_, _, px) = out_gray(&run(
            &page,
            vec![PostprocessOp::Resize(ResizeSpec {
                width: 2,
                height: 1,
                filter: ResizeFilter::Box,
            })],
        ));
        // (30·1 + 90·0.5) / 1.5 = 50; (90·0.5 + 150·1) / 1.5 = 130.
        assert_eq!(px, vec![50, 130]);
    }

    #[test]
    fn nearest_upscale_replicates_pixels() {
        let page = gray_page(2, 1, &[10, 200]);
        let (w, _, px) = out_gray(&run(
            &page,
            vec![PostprocessOp::Resize(ResizeSpec {
                width: 4,
                height: 1,
                filter: ResizeFilter::Nearest,
            })],
        ));
        assert_eq!(w, 4);
        assert_eq!(px, vec![10, 10, 200, 200]);
    }

    #[test]
    fn bilinear_upscale_interpolates_flat_field_exactly() {
        let page = gray_page(2, 2, &[80, 80, 80, 80]);
        let (_, _, px) = out_gray(&run(
            &page,
            vec![PostprocessOp::Resize(ResizeSpec {
                width: 5,
                height: 3,
                filter: ResizeFilter::Bilinear,
            })],
        ));
        assert!(px.iter().all(|&v| v == 80), "flat field must stay flat: {px:?}");
    }

    #[test]
    fn catmullrom_and_lanczos_preserve_flat_fields() {
        let page = gray_page(8, 8, &[120; 64]);
        for filter in [ResizeFilter::CatmullRom, ResizeFilter::Lanczos3] {
            let (_, _, px) = out_gray(&run(
                &page,
                vec![PostprocessOp::Resize(ResizeSpec { width: 5, height: 5, filter })],
            ));
            assert!(
                px.iter().all(|&v| (v as i32 - 120).abs() <= 1),
                "{filter:?} drifted on a flat field: {px:?}"
            );
        }
    }

    #[test]
    fn otsu_separates_bimodal_histogram() {
        let mut pixels = vec![20u8; 32];
        pixels.extend_from_slice(&[220u8; 32]);
        let page = gray_page(8, 8, &pixels);
        let (_, _, px) = out_gray(&run(&page, vec![PostprocessOp::Otsu(OtsuSpec::default())]));
        assert!(px[..32].iter().all(|&v| v == 0));
        assert!(px[32..].iter().all(|&v| v == 255));
    }

    #[test]
    fn sauvola_binarizes_dark_text_on_bright_paper() {
        // Bright field with one dark pixel: the dark pixel must go to ink.
        let mut pixels = vec![230u8; 49];
        pixels[24] = 10; // center of 7×7
        let page = gray_page(7, 7, &pixels);
        let (_, _, px) = out_gray(&run(
            &page,
            vec![PostprocessOp::Sauvola(SauvolaSpec { window: 5, k: 0.3 })],
        ));
        assert_eq!(px[24], 0, "dark center must binarize to ink");
        assert_eq!(px[0], 255, "far corner paper must stay white");
    }

    #[test]
    fn fused_threshold_blends_global_and_local() {
        let mut pixels = vec![230u8; 49];
        pixels[24] = 10;
        let page = gray_page(7, 7, &pixels);
        let (_, _, px) = out_gray(&run(
            &page,
            vec![PostprocessOp::FuseThresholds(FusionSpec {
                global_weight: 0.5,
                window: 5,
                k: 0.3,
            })],
        ));
        assert_eq!(px[24], 0);
        assert_eq!(px[0], 255);
        assert!(px.iter().all(|&v| v == 0 || v == 255));
    }

    #[test]
    fn dither_none_thresholds_at_128() {
        let page = gray_page(3, 1, &[127, 128, 129]);
        let (_, _, px) = out_gray(&run(&page, vec![PostprocessOp::Dither(DitherSpec::None)]));
        assert_eq!(px, vec![0, 255, 255]);
    }

    #[test]
    fn floyd_steinberg_preserves_mean_ink() {
        // A flat 25% gray field must dither to ≈ 25% white pixels.
        let page = gray_page(16, 16, &[64; 256]);
        let (_, _, px) = out_gray(&run(
            &page,
            vec![PostprocessOp::Dither(DitherSpec::FloydSteinberg)],
        ));
        let whites = px.iter().filter(|&&v| v == 255).count();
        assert!(px.iter().all(|&v| v == 0 || v == 255));
        assert!(
            (whites as i64 - 64).abs() <= 8,
            "64/256 pixels should be white, got {whites}"
        );
    }

    #[test]
    fn bayer4_is_deterministic_and_bilevel() {
        let page = gray_page(8, 8, &[130; 64]);
        let a = out_gray(&run(&page, vec![PostprocessOp::Dither(DitherSpec::Bayer4)]));
        let b = out_gray(&run(&page, vec![PostprocessOp::Dither(DitherSpec::Bayer4)]));
        assert_eq!(a.2, b.2);
        assert!(a.2.iter().all(|&v| v == 0 || v == 255));
        assert!(a.2.contains(&0) && a.2.contains(&255));
    }

    #[test]
    fn pack_monochrome_bits_msb_first_one_is_ink() {
        // Row: ink, paper, ink, paper, paper, paper, paper, paper, ink.
        let mut pixels = vec![255u8; 9];
        pixels[0] = 0;
        pixels[2] = 0;
        pixels[8] = 0;
        let page = gray_page(9, 1, &pixels);
        match run(&page, vec![PostprocessOp::PackMonochrome]) {
            PostprocessOutput::PackedMono { width, height, stride, bits } => {
                assert_eq!((width, height, stride), (9, 1, 2));
                assert_eq!(&bits[..], &[0b1010_0000, 0b1000_0000]);
            }
            other => panic!("expected PackedMono, got {other:?}"),
        }
    }

    #[test]
    fn pack_monochrome_must_be_last() {
        let graph = PostprocessGraph {
            ops: vec![
                PostprocessOp::PackMonochrome,
                PostprocessOp::Dither(DitherSpec::None),
            ],
        };
        assert!(!CpuPostprocess.supports(&graph));
        let page = gray_page(1, 1, &[0]);
        assert!(matches!(
            CpuPostprocess.execute(&page, &graph),
            Err(PostprocessError::InvalidParams(_))
        ));
    }

    #[test]
    fn threshold_on_rgba_is_a_format_mismatch() {
        let page = rgba_page(1, 1, &[255, 255, 255, 255]);
        for op in [
            PostprocessOp::Otsu(OtsuSpec::default()),
            PostprocessOp::Sauvola(SauvolaSpec { window: 5, k: 0.3 }),
            PostprocessOp::Dither(DitherSpec::None),
            PostprocessOp::PackMonochrome,
        ] {
            let err = CpuPostprocess
                .execute(&page, &PostprocessGraph { ops: vec![op] })
                .unwrap_err();
            assert!(matches!(err, PostprocessError::FormatMismatch(_)));
        }
    }

    #[test]
    fn strided_input_rows_are_honored() {
        // 2×2 gray page with a 4-byte stride: padding must be ignored.
        let page = HostPage {
            width: 2,
            height: 2,
            stride: 4,
            format: OutputFormat::Gray8,
            pixels: Arc::from(vec![1u8, 2, 99, 99, 3, 4, 99, 99].into_boxed_slice()),
        };
        let (_, _, px) = out_gray(&run(&page, vec![]));
        assert_eq!(px, vec![1, 2, 3, 4]);
    }

    #[test]
    fn short_pixel_buffer_is_a_typed_error() {
        let page = HostPage {
            width: 4,
            height: 4,
            stride: 4,
            format: OutputFormat::Gray8,
            pixels: Arc::from(vec![0u8; 8].into_boxed_slice()),
        };
        assert!(matches!(
            CpuPostprocess.execute(&page, &PostprocessGraph::default()),
            Err(PostprocessError::FormatMismatch(_))
        ));
    }

    /// The scan-cleaning chain end to end: RGBA render → gray → tone →
    /// crop → downscale → binarize → packed 1-bit page.
    #[test]
    fn chained_scan_cleaning_pipeline() {
        // 8×8 opaque RGBA: left half dark ink, right half light paper.
        let mut pixels = Vec::with_capacity(8 * 8 * 4);
        for _y in 0..8 {
            for x in 0..8 {
                let v: u8 = if x < 4 { 30 } else { 220 };
                pixels.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let page = rgba_page(8, 8, &pixels);
        let out = run(
            &page,
            vec![
                PostprocessOp::ConvertToGray(GraySpec::default()),
                PostprocessOp::ApplyToneCurve(ToneCurve::brightness_contrast(0.0, 0.2)),
                PostprocessOp::Crop(CropSpec { x: 0, y: 0, width: 8, height: 4 }),
                PostprocessOp::Resize(ResizeSpec {
                    width: 4,
                    height: 2,
                    filter: ResizeFilter::Box,
                }),
                PostprocessOp::Otsu(OtsuSpec::default()),
                PostprocessOp::PackMonochrome,
            ],
        );
        match out {
            PostprocessOutput::PackedMono { width, height, stride, bits } => {
                assert_eq!((width, height, stride), (4, 2, 1));
                // Left two columns ink (11), right two paper (00) → 1100_0000.
                assert_eq!(&bits[..], &[0b1100_0000, 0b1100_0000]);
            }
            other => panic!("expected PackedMono, got {other:?}"),
        }
    }
}
