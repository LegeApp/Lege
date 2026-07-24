//! CPU image rasterization (Phase 6 item C).
//!
//! An image occupies the unit square in user space; the CTM maps it to device
//! space. Rendering inverts that map: each device pixel in the image's device
//! bounds is projected back to `(u, v)` in the unit square, the source sample
//! is read (bpc unpack → `/Decode` → color-space conversion → `/SMask` alpha),
//! and composited source-over. Nearest and bilinear sampling are supported.
//!
//! Codec-encoded images (DCT/JPX/JBIG2/CCITT) carry no samples and never reach
//! here — the page's `NEEDS_*` feature routes them away at preflight.

use std::sync::Arc;

use pdf_page_ir::{
    DeviceRect, ImageColorSpace, ImageMask, ImageSMask, InterpolationMode, Matrix, Point,
};

/// An image prepared for one render request.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    /// Diagnostic attribution only: the containing construct this operation
    /// came from. Never read while painting — see
    /// [`crate::attribution`].
    pub origin: pdf_page_ir::PaintOrigin,
    pub bounds: DeviceRect,
    pub clip: Option<u32>,
    pub clip_has_mask: bool,
    /// Device → unit-square transform (inverse of the image CTM).
    pub inv: Matrix,
    pub width: u32,
    pub height: u32,
    pub bpc: u8,
    pub color_space: ImageColorSpace,
    pub decode: Option<Arc<[[f32; 2]]>>,
    pub samples: Arc<[u8]>,
    /// Predecoded colors for low-bit-depth single-component images. This
    /// sample. This hoists `/Decode`, Indexed/Tint lookup, and color conversion
    /// out of the per-destination-pixel sampling loop.
    pub sample_lut: Option<Arc<[[u8; 4]]>>,
    pub smask: Option<Arc<ImageSMask>>,
    /// Explicit hard `/Mask` (color-key or stencil). Applied per source texel
    /// as an all-or-nothing cut-out; independent of `smask` (only one is ever
    /// set — `/SMask` wins at build time).
    pub mask: Option<ImageMask>,
    pub interpolation: InterpolationMode,
    /// Source texels one device pixel covers, per axis. Above 1 the image is
    /// being *minified* and one sample per pixel throws information away.
    pub footprint: [f64; 2],
    /// `/ImageMask` stencil painted with `stencil_rgb`.
    pub is_stencil: bool,
    pub stencil_rgb: [u8; 3],
    pub alpha: u8,
    pub blend: pdf_page_ir::BlendMode,
}

impl PreparedImage {
    /// Source samples per pixel.
    fn components(&self) -> usize {
        if self.is_stencil {
            1
        } else {
            self.color_space.components()
        }
    }

    /// Row stride in bits (rows are byte-aligned).
    fn row_bits(&self) -> usize {
        let bits = self.width as usize * self.components() * self.bpc as usize;
        bits.div_ceil(8) * 8
    }

    /// Whether this image can use the executor's prepared axis-aligned
    /// bilevel box-filter path.
    pub(crate) fn is_binary_box_filterable(&self) -> bool {
        !self.is_stencil
            && self.bpc == 1
            && self.sample_lut.as_ref().is_some_and(|lut| lut.len() >= 2)
            && self.smask.is_none()
            && self.mask.is_none()
            && (self.footprint[0] > 1.0 || self.footprint[1] > 1.0)
            && self.inv.b.abs() < 1e-12
            && self.inv.c.abs() < 1e-12
    }

    /// The two prepared RGBA entries (zero-bit, one-bit) of a bilevel image's
    /// lookup table, for the executor's summed-area box-filter path.
    pub(crate) fn binary_box_lut(&self) -> Option<([u8; 4], [u8; 4])> {
        let lut = self.sample_lut.as_ref()?;
        let zero = lut.first().copied().unwrap_or([0, 0, 0, 255]);
        let one = lut.get(1).copied().unwrap_or(zero);
        Some((zero, one))
    }

    /// Packed row stride in bits (rows are byte-aligned), for the executor's
    /// summed-area box-filter path.
    pub(crate) fn packed_row_bits(&self) -> usize {
        self.row_bits()
    }

    /// Convert an opaque 8-bit CMYK source (no `/Decode`) to a packed RGB8
    /// buffer, one pixel through the *exact* same `cmyk_to_rgb` + `to_u8` path
    /// `pixel()` uses. Box-averaging this buffer is byte-identical to the
    /// generic path's convert-each-tap-then-average, because each source pixel
    /// yields the identical RGB triple either way — only the conversion is
    /// hoisted out of the per-destination-pixel loop into one tight pass.
    ///
    /// `None` if the image is not that shape or exceeds the size cap (then the
    /// generic path runs, unchanged).
    pub(crate) fn cmyk_source_as_rgb8(&self) -> Option<Vec<u8>> {
        // 64 Mpx -> 192 MiB of RGB8; above this the generic per-tap path (whose
        // work is bounded by the footprint, not the source area) is preferred.
        const MAX_CONVERT_PIXELS: usize = 64 * 1024 * 1024;
        if self.is_stencil
            || self.bpc != 8
            || self.decode.is_some()
            || !matches!(self.color_space, ImageColorSpace::Cmyk)
        {
            return None;
        }
        let n = (self.width as usize).checked_mul(self.height as usize)?;
        if n == 0 || n > MAX_CONVERT_PIXELS || self.samples.len() < n * 4 {
            return None;
        }
        let mut out = vec![0u8; n * 3];
        for (cmyk, rgb) in self.samples[..n * 4]
            .chunks_exact(4)
            .zip(out.chunks_exact_mut(3))
        {
            let converted = pdf_color::cmyk_to_rgb(
                (cmyk[0] as f32 / 255.0).clamp(0.0, 1.0),
                (cmyk[1] as f32 / 255.0).clamp(0.0, 1.0),
                (cmyk[2] as f32 / 255.0).clamp(0.0, 1.0),
                (cmyk[3] as f32 / 255.0).clamp(0.0, 1.0),
            );
            rgb[0] = to_u8(converted[0]);
            rgb[1] = to_u8(converted[1]);
            rgb[2] = to_u8(converted[2]);
        }
        Some(out)
    }

    /// Weighted box average of a prepared one-bit image over one destination
    /// pixel's fractional source footprint.
    ///
    /// The executor prepares the per-axis tap ranges (with fractional edge
    /// weights) once per destination column/row. This method therefore
    /// performs only packed-bit population counts, the fractional edge-tap
    /// corrections, and the two-entry color-table mix in the pixel loop.
    pub(crate) fn binary_box_average(
        &self,
        cols: &AxisTaps,
        rows: &AxisTaps,
    ) -> Option<([u8; 4], u64)> {
        let lut = self.sample_lut.as_ref()?;
        let zero = lut.first().copied().unwrap_or([0, 0, 0, 255]);
        let one = lut.get(1).copied().unwrap_or(zero);
        let weight = cols.total.checked_mul(rows.total)?;
        if weight == 0 {
            return None;
        }

        let row_bits = self.row_bits();
        let mut ones_w = 0u64;
        for source_y in rows.lo..=rows.hi {
            let base = source_y as usize * row_bits;
            let row_ones = self.weighted_row_ones(base, cols);
            let wy = rows.weight_at(source_y);
            ones_w += wy * row_ones;
        }
        let taps =
            (cols.hi - cols.lo + 1) as u64 * (rows.hi - rows.lo + 1) as u64;
        Some((mix_bilevel(zero, one, ones_w, weight), taps))
    }

    /// Weighted set-bit count of one packed source row over an axis range:
    /// fractional first/last taps, full interior taps via popcount. Result
    /// is scaled by `AXIS_TAP_SCALE`.
    fn weighted_row_ones(&self, base_bit: usize, cols: &AxisTaps) -> u64 {
        let bit_at = |x: u32| -> u64 {
            let b = base_bit + x as usize;
            let byte = self.samples.get(b / 8).copied().unwrap_or(0);
            ((byte >> (7 - (b % 8))) & 1) as u64
        };
        if cols.lo == cols.hi {
            return cols.w_lo * bit_at(cols.lo);
        }
        let mut ones = cols.w_lo * bit_at(cols.lo) + cols.w_hi * bit_at(cols.hi);
        if cols.hi - cols.lo >= 2 {
            let interior =
                count_one_bits(&self.samples, base_bit + cols.lo as usize + 1,
                    (cols.hi - cols.lo - 1) as usize) as u64;
            ones += AXIS_TAP_SCALE * interior;
        }
        ones
    }

    /// Straight RGBA at device point `(dx, dy)`, or `None` when the point is
    /// outside the unit square (or a stencil's transparent sample).
    pub fn shade(&self, dx: f64, dy: f64) -> Option<[u8; 4]> {
        self.shade_with_taps(dx, dy).0
    }

    /// Profiling variant that also returns base-image source texels sampled.
    #[cfg(feature = "profiling")]
    pub fn shade_profiled(&self, dx: f64, dy: f64) -> (Option<[u8; 4]>, u64) {
        self.shade_with_taps(dx, dy)
    }

    /// Sample for an *edge* device pixel whose center may fall outside the
    /// image quad: the sample point is clamped to the nearest in-quad point,
    /// so the partial-coverage sliver takes the nearest edge texel's color
    /// (A10 image-edge anti-aliasing; PDFium's edge behavior analog).
    pub(crate) fn shade_clamped(&self, dx: f64, dy: f64) -> Option<[u8; 4]> {
        let p = self.inv.apply(Point { x: dx, y: dy });
        let ux = p.x.clamp(0.0, 1.0 - 1e-9);
        let uy = p.y.clamp(0.0, 1.0 - 1e-9);
        self.shade_uv(ux, uy).0
    }

    /// Fractional coverage (0..=255) of the device pixel whose center is
    /// `(dx, dy)` against the image's device-space quad (A10).
    ///
    /// `None` means the pixel square is provably interior — the caller paints
    /// at full weight, byte-identical to the pre-A10 behavior. `Some(0)`
    /// means fully outside. Handles rotated/sheared placements exactly: the
    /// pixel square maps under `inv` to a parallelogram in unit-image space,
    /// which is clipped against `[0,1]²` (Sutherland–Hodgman); the coverage
    /// is the clipped area over the parallelogram's area.
    pub(crate) fn edge_coverage(&self, dx: f64, dy: f64) -> Option<u16> {
        // Half-extent of the mapped pixel square per uv axis (bbox bound).
        let mu = 0.5 * (self.inv.a.abs() + self.inv.c.abs());
        let mv = 0.5 * (self.inv.b.abs() + self.inv.d.abs());
        let p = self.inv.apply(Point { x: dx, y: dy });
        if p.x >= mu && p.x <= 1.0 - mu && p.y >= mv && p.y <= 1.0 - mv {
            return None; // fully interior — full weight, untouched path
        }
        let full = (self.inv.a * self.inv.d - self.inv.b * self.inv.c).abs();
        if full <= 1e-18 {
            return Some(0);
        }
        // The pixel square's corners in uv space.
        let mut poly: [[f64; 2]; 8] = [[0.0; 2]; 8];
        let mut n = 4usize;
        for (i, (cx, cy)) in [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)]
            .into_iter()
            .enumerate()
        {
            poly[i] = [
                p.x + cx * self.inv.a + cy * self.inv.c,
                p.y + cx * self.inv.b + cy * self.inv.d,
            ];
        }
        // Clip against u≥0, u≤1, v≥0, v≤1.
        let mut scratch: [[f64; 2]; 8] = [[0.0; 2]; 8];
        for (axis, keep_ge, bound) in
            [(0, true, 0.0), (0, false, 1.0), (1, true, 0.0), (1, false, 1.0)]
        {
            let mut m = 0usize;
            for i in 0..n {
                let a = poly[i];
                let b = poly[(i + 1) % n];
                let da = if keep_ge { a[axis] - bound } else { bound - a[axis] };
                let db = if keep_ge { b[axis] - bound } else { bound - b[axis] };
                if da >= 0.0 {
                    scratch[m] = a;
                    m += 1;
                }
                if (da >= 0.0) != (db >= 0.0) {
                    let t = da / (da - db);
                    scratch[m] = [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])];
                    m += 1;
                }
                if m >= scratch.len() {
                    break; // numeric degeneracy guard; polygon is convex ≤ 8
                }
            }
            poly[..m].copy_from_slice(&scratch[..m]);
            n = m;
            if n == 0 {
                return Some(0);
            }
        }
        // Shoelace area of the clipped polygon.
        let mut area2 = 0.0f64;
        for i in 0..n {
            let a = poly[i];
            let b = poly[(i + 1) % n];
            area2 += a[0] * b[1] - b[0] * a[1];
        }
        let cov = (area2.abs() * 0.5 / full).clamp(0.0, 1.0);
        Some((cov * 255.0 + 0.5) as u16)
    }

    fn shade_with_taps(&self, dx: f64, dy: f64) -> (Option<[u8; 4]>, u64) {
        let p = self.inv.apply(Point { x: dx, y: dy });
        if p.x < 0.0 || p.x >= 1.0 || p.y < 0.0 || p.y >= 1.0 {
            return (None, 0);
        }
        self.shade_uv(p.x, p.y)
    }

    /// The sampling body shared by [`Self::shade_with_taps`] (in-range
    /// centers) and [`Self::shade_clamped`] (edge pixels): `(px, py)` is a
    /// point inside the unit image square.
    fn shade_uv(&self, px: f64, py: f64) -> (Option<[u8; 4]>, u64) {
        let p = Point { x: px, y: py };
        // Column increases with u; row increases downward (row 0 at the top).
        let fx = p.x * self.width as f64 - 0.5;
        let fy = (1.0 - p.y) * self.height as f64 - 0.5;

        // Minification: average the source texels this device pixel covers.
        // Point-sampling a 300dpi scan onto a screen keeps one texel in nine
        // and discards the rest, which breaks strokes and drops thin glyphs.
        // PDFium area-averages here regardless of `/Interpolate`, and so do
        // we — the flag chooses the *magnification* filter, not whether to
        // throw away detail (ISO 32000-1 §8.9.5.2 calls it a quality hint).
        let (rgba, taps) = if self.footprint[0] > 1.0 || self.footprint[1] > 1.0 {
            let Some((rgba, taps)) = self.area_average(fx, fy) else {
                return (None, 0);
            };
            (rgba, taps as u64)
        } else {
            match self.interpolation {
                InterpolationMode::Nearest => {
                    let col = clampi(fx.round() as i64, self.width);
                    let row = clampi(fy.round() as i64, self.height);
                    let Some(rgba) = self.pixel(col, row) else {
                        return (None, 1);
                    };
                    (rgba, 1)
                }
                InterpolationMode::Bilinear => {
                    let Some(rgba) = self.bilinear(fx, fy) else {
                        return (None, 4);
                    };
                    (rgba, 4)
                }
            }
        };
        // An explicit hard `/Mask` cuts the texel out entirely (the reverse
        // polarity of a soft mask). Tested per source texel at the sample
        // point, independent of the soft-mask path.
        if let Some(mask) = &self.mask
            && self.mask_hides(mask, fx, fy, p.x, p.y)
        {
            return (None, taps);
        }
        // The soft mask (sampled in the unit square) scales alpha.
        let a = match &self.smask {
            Some(sm) => {
                let sa = sample_smask(sm, p.x, p.y);
                (rgba[3] as f32 * sa) as u8
            }
            None => rgba[3],
        };
        if a == 0 {
            return (None, taps);
        }
        (Some([rgba[0], rgba[1], rgba[2], a]), taps)
    }

    /// Per-axis fractional box taps for this destination pixel's footprint
    /// (see [`axis_box_taps`]).
    pub(crate) fn box_taps_x(&self, fx: f64) -> Option<AxisTaps> {
        axis_box_taps(fx, (self.footprint[0] * 0.5).max(0.5), self.width)
    }

    /// See [`Self::box_taps_x`].
    pub(crate) fn box_taps_y(&self, fy: f64) -> Option<AxisTaps> {
        axis_box_taps(fy, (self.footprint[1] * 0.5).max(0.5), self.height)
    }

    /// Area-weighted average of the source texels inside this device pixel's
    /// footprint (box filter with fractional-tap weights, PDFium
    /// `CStretchEngine` analog): every texel contributes proportionally to
    /// its overlap with the footprint, so a texel half inside the box counts
    /// half — uniform whole-texel counting rendered minified stencils and
    /// scans measurably bolder than PDFium.
    ///
    /// The footprint of a rotated draw is a parallelogram; its axis-aligned
    /// bounding box is used instead, which over-blurs slightly at an angle and
    /// is exactly right for the axis-aligned case that covers essentially
    /// every scanned page.
    ///
    /// A stencil averages *coverage* into alpha, which is what gives a
    /// downscaled `/ImageMask` smooth edges instead of a ragged bitmap.
    fn area_average(&self, fx: f64, fy: f64) -> Option<([u8; 4], u32)> {
        let tx = self.box_taps_x(fx)?;
        let ty = self.box_taps_y(fy)?;
        let weight = tx.total.checked_mul(ty.total)?;
        if weight == 0 {
            return None;
        }
        let taps = (tx.hi - tx.lo + 1) * (ty.hi - ty.lo + 1);

        let mut acc = [0u64; 4];

        // Decoded JPEG/JPX RGB8 is already in the exact output component
        // representation. Summing those bytes directly avoids three generic
        // bit reads, normalization, color-space dispatch, and float-to-byte
        // conversions for every source tap.
        if !self.is_stencil
            && self.bpc == 8
            && self.decode.is_none()
            && matches!(self.color_space, ImageColorSpace::Rgb)
            && self.samples.len() >= self.width as usize * self.height as usize * 3
        {
            let stride = self.width as usize * 3;
            for row in ty.lo..=ty.hi {
                let wy = ty.weight_at(row);
                let start = row as usize * stride + tx.lo as usize * 3;
                let end = row as usize * stride + (tx.hi as usize + 1) * 3;
                let src = self.samples.get(start..end)?;
                for (col, rgb) in (tx.lo..).zip(src.chunks_exact(3)) {
                    let w = wy * tx.weight_at(col);
                    acc[0] += w * rgb[0] as u64;
                    acc[1] += w * rgb[1] as u64;
                    acc[2] += w * rgb[2] as u64;
                    acc[3] += w * 255;
                }
            }
            return weighted_average_rgba(acc, weight).map(|rgba| (rgba, taps));
        }

        // Gray/Indexed/Tint images have one raw component. The prepared LUT
        // resolves that component to RGBA once, rather than repeating color
        // conversion for every tap in every destination pixel.
        if let Some(lut) = &self.sample_lut {
            let row_bits = self.row_bits();
            if self.bpc == 1 {
                let zero = lut.first().copied().unwrap_or([0, 0, 0, 255]);
                let one = lut.get(1).copied().unwrap_or(zero);
                let mut ones_w = 0u64;
                for row in ty.lo..=ty.hi {
                    let base = row as usize * row_bits;
                    ones_w += ty.weight_at(row) * self.weighted_row_ones(base, &tx);
                }
                return Some((mix_bilevel(zero, one, ones_w, weight), taps));
            }
            for row in ty.lo..=ty.hi {
                let wy = ty.weight_at(row);
                for col in tx.lo..=tx.hi {
                    let w = wy * tx.weight_at(col);
                    let bit = row as usize * row_bits + col as usize * self.bpc as usize;
                    let raw = read_bits(&self.samples, bit, self.bpc as usize) as usize;
                    let p = lut.get(raw).copied().unwrap_or([0, 0, 0, 255]);
                    for c in 0..4 {
                        acc[c] += w * p[c] as u64;
                    }
                }
            }
            return weighted_average_rgba(acc, weight).map(|rgba| (rgba, taps));
        }

        for row in ty.lo..=ty.hi {
            let wy = ty.weight_at(row);
            for col in tx.lo..=tx.hi {
                // A masked-out stencil texel contributes nothing, which is
                // exactly its zero coverage — but its weight still counts
                // toward the divisor, or the average would ignore the gaps.
                if let Some(p) = self.pixel(col, row) {
                    let w = wy * tx.weight_at(col);
                    for c in 0..4 {
                        acc[c] += w * p[c] as u64;
                    }
                }
            }
        }
        let out = weighted_average_rgba(acc, weight)?;
        // A stencil's colour is constant; only its coverage varies, so keep
        // the paint colour and let the averaged alpha carry the edge.
        if self.is_stencil {
            return Some((
                [
                    self.stencil_rgb[0],
                    self.stencil_rgb[1],
                    self.stencil_rgb[2],
                    out[3],
                ],
                taps,
            ));
        }
        Some((out, taps))
    }

    /// Bilinearly interpolate the four source pixels around `(fx, fy)`.
    fn bilinear(&self, fx: f64, fy: f64) -> Option<[u8; 4]> {
        let x0 = fx.floor();
        let y0 = fy.floor();
        let tx = (fx - x0) as f32;
        let ty = (fy - y0) as f32;
        let c = |ix: i64, iy: i64| self.pixel(clampi(ix, self.width), clampi(iy, self.height));
        let p00 = c(x0 as i64, y0 as i64)?;
        let p10 = c(x0 as i64 + 1, y0 as i64)?;
        let p01 = c(x0 as i64, y0 as i64 + 1)?;
        let p11 = c(x0 as i64 + 1, y0 as i64 + 1)?;
        let mut out = [0u8; 4];
        for ch in 0..4 {
            let top = p00[ch] as f32 * (1.0 - tx) + p10[ch] as f32 * tx;
            let bot = p01[ch] as f32 * (1.0 - tx) + p11[ch] as f32 * tx;
            out[ch] = (top * (1.0 - ty) + bot * ty).round().clamp(0.0, 255.0) as u8;
        }
        Some(out)
    }

    /// One source pixel as straight RGBA, or `None` for a transparent stencil
    /// sample.
    fn pixel(&self, col: u32, row: u32) -> Option<[u8; 4]> {
        let ncomp = self.components();
        let row_bits = self.row_bits();
        let maxv = ((1u64 << self.bpc.min(16)) - 1).max(1) as f32;
        let read = |c: usize| -> u32 {
            let bit = row as usize * row_bits + (col as usize * ncomp + c) * self.bpc as usize;
            read_bits(&self.samples, bit, self.bpc as usize)
        };

        if self.is_stencil {
            // 1 = masked (default), 0 = paint; /Decode [1 0] inverts.
            let mut v = read(0);
            if self
                .decode
                .as_ref()
                .map(|d| d.first().map(|p| p[0] > p[1]).unwrap_or(false))
                .unwrap_or(false)
            {
                v = 1 - v;
            }
            if v == 0 {
                return Some([
                    self.stencil_rgb[0],
                    self.stencil_rgb[1],
                    self.stencil_rgb[2],
                    255,
                ]);
            }
            return None;
        }

        if let Some(lut) = &self.sample_lut {
            return lut.get(read(0) as usize).copied();
        }

        // The common decoded JPEG/JPX formats need no `/Decode` transform.
        // Return their bytes directly instead of round-tripping through f32.
        if self.bpc == 8
            && self.decode.is_none()
            && self.samples.len() >= self.width as usize * self.height as usize * self.components()
        {
            let pixel = row as usize * self.width as usize + col as usize;
            match self.color_space {
                ImageColorSpace::Rgb => {
                    let offset = pixel * 3;
                    let rgb = self.samples.get(offset..offset + 3)?;
                    return Some([rgb[0], rgb[1], rgb[2], 255]);
                }
                ImageColorSpace::Gray => {
                    let g = *self.samples.get(pixel)?;
                    return Some([g, g, g, 255]);
                }
                _ => {}
            }
        }

        // Normalize each component with the /Decode remap (default [0,1]).
        let comp = |c: usize| -> f32 {
            let raw = read(c) as f32 / maxv;
            match self.decode.as_ref().and_then(|d| d.get(c)) {
                Some([lo, hi]) => lo + raw * (hi - lo),
                None => raw,
            }
        };

        let rgb = match &self.color_space {
            ImageColorSpace::Gray => {
                let g = comp(0).clamp(0.0, 1.0);
                [g, g, g]
            }
            ImageColorSpace::Rgb => [
                comp(0).clamp(0.0, 1.0),
                comp(1).clamp(0.0, 1.0),
                comp(2).clamp(0.0, 1.0),
            ],
            ImageColorSpace::Cmyk => pdf_color::cmyk_to_rgb(
                comp(0).clamp(0.0, 1.0),
                comp(1).clamp(0.0, 1.0),
                comp(2).clamp(0.0, 1.0),
                comp(3).clamp(0.0, 1.0),
            ),
            // Samples arrive normalised to 0..1; Lab's axes are not. Map them
            // onto the space's own ranges before converting — L* over 0..100
            // and a*/b* over `/Range` — which is exactly the default `/Decode`
            // for a Lab image (ISO 32000-1 Table 89).
            ImageColorSpace::Lab { white_point, range } => {
                let axis = |v: f32, lo: f32, hi: f32| lo + v.clamp(0.0, 1.0) * (hi - lo);
                pdf_color::lab_to_rgb(
                    axis(comp(0), 0.0, 100.0),
                    axis(comp(1), range[0], range[1]),
                    axis(comp(2), range[2], range[3]),
                    *white_point,
                    *range,
                )
            }
            ImageColorSpace::Indexed {
                base,
                hival,
                lookup,
            } => {
                // `/Decode` remaps the sample to a palette *index* (default is
                // identity `[0, 2^bpc-1]`). When present — e.g. `[0 255]` on a
                // 1-bit image, where sample 1 must select index 255, not 1 —
                // `comp(0)` yields the index directly (its remap is in index
                // units here); ignoring it reads the wrong palette entry and
                // inverts bilevel scans.
                let idx = if self.decode.is_some() {
                    comp(0).round().clamp(0.0, *hival as f32) as u32 as usize
                } else {
                    read(0).min(*hival) as usize
                };
                indexed_rgb(base, *hival, lookup, idx)
            }
            ImageColorSpace::TintLut { rgb } => tint_lut_rgb(rgb, comp(0)),
            ImageColorSpace::TintLut2 { rgb } => tint_lut2_rgb(rgb, comp(0), comp(1)),
            ImageColorSpace::IccRgb { trc, matrix } => {
                pdf_color::icc::to_srgb_with(trc, matrix, [comp(0), comp(1), comp(2)])
            }
        };
        Some([to_u8(rgb[0]), to_u8(rgb[1]), to_u8(rgb[2]), 255])
    }

    /// True when the explicit `/Mask` makes the texel at `(fx, fy)` fully
    /// transparent. `(fx, fy)` is the base-image texel coordinate; `(u, v)` is
    /// the unit-square point used to resample an independent stencil.
    fn mask_hides(&self, mask: &ImageMask, fx: f64, fy: f64, u: f64, v: f64) -> bool {
        match mask {
            ImageMask::ColorKey(ranges) => {
                let col = clampi(fx.round() as i64, self.width);
                let row = clampi(fy.round() as i64, self.height);
                self.color_key_hit(ranges, col, row)
            }
            ImageMask::Stencil(sm) => stencil_hides(sm, u, v),
        }
    }

    /// Color-key test: every component's RAW sample (before `/Decode`) must lie
    /// within its `[min, max]`. A range count that disagrees with the component
    /// count means a malformed mask — treat as no mask (never hide).
    fn color_key_hit(&self, ranges: &[[u32; 2]], col: u32, row: u32) -> bool {
        let ncomp = self.components();
        if ranges.len() != ncomp {
            return false;
        }
        let row_bits = self.row_bits();
        for (c, &[lo, hi]) in ranges.iter().enumerate() {
            let bit = row as usize * row_bits + (col as usize * ncomp + c) * self.bpc as usize;
            let raw = read_bits(&self.samples, bit, self.bpc as usize);
            if raw < lo || raw > hi {
                return false;
            }
        }
        true
    }
}

/// Build a raw-sample-to-RGBA table for low-bit-depth single-component images.
/// Restricting this to 1/2/4 bpc keeps preparation cheap for documents with
/// many ordinary 8-bit grayscale images while covering bilevel/Indexed scans,
/// where generic per-tap color conversion is disproportionately expensive.
pub(crate) fn build_sample_lut(
    bits_per_component: u8,
    color_space: &ImageColorSpace,
    decode: Option<&[[f32; 2]]>,
    is_stencil: bool,
) -> Option<Arc<[[u8; 4]]>> {
    if is_stencil || bits_per_component == 0 || bits_per_component > 4 {
        return None;
    }
    if color_space.components() != 1 {
        return None;
    }

    let entries = 1usize << bits_per_component;
    let maxv = (entries - 1).max(1) as f32;
    let mut lut = Vec::with_capacity(entries);
    for raw in 0..entries {
        let normalized = raw as f32 / maxv;
        let decoded = match decode.and_then(|d| d.first()) {
            Some([lo, hi]) => lo + normalized * (hi - lo),
            None => normalized,
        };
        let rgb = match color_space {
            ImageColorSpace::Gray => {
                let g = decoded.clamp(0.0, 1.0);
                [g, g, g]
            }
            ImageColorSpace::Indexed {
                base,
                hival,
                lookup,
            } => {
                let idx = if decode.is_some() {
                    decoded.round().clamp(0.0, *hival as f32) as usize
                } else {
                    raw.min(*hival as usize)
                };
                indexed_rgb(base, *hival, lookup, idx)
            }
            ImageColorSpace::TintLut { rgb } => tint_lut_rgb(rgb, decoded),
            // Multi-component spaces have no single-sample LUT.
            ImageColorSpace::Rgb
            | ImageColorSpace::Cmyk
            | ImageColorSpace::Lab { .. }
            | ImageColorSpace::TintLut2 { .. }
            | ImageColorSpace::IccRgb { .. } => {
                return None;
            }
        };
        lut.push([to_u8(rgb[0]), to_u8(rgb[1]), to_u8(rgb[2]), 255]);
    }
    Some(lut.into())
}

/// Fixed-point scale for fractional box-filter tap weights. 4096 keeps the
/// arithmetic integral (deterministic and memoizable) while resolving an
/// edge tap's overlap to 1/4096 of a texel.
pub(crate) const AXIS_TAP_SCALE: u64 = 4096;

/// One axis of a destination pixel's source footprint: the inclusive texel
/// range `[lo, hi]` plus fractional first/last weights in `AXIS_TAP_SCALE`
/// units (interior taps weigh a full `AXIS_TAP_SCALE`). `total` is the sum
/// of all tap weights along the axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AxisTaps {
    pub lo: u32,
    pub hi: u32,
    pub w_lo: u64,
    pub w_hi: u64,
    pub total: u64,
}

impl AxisTaps {
    /// Weight of texel `i` (must be within `lo..=hi`).
    #[inline]
    pub(crate) fn weight_at(&self, i: u32) -> u64 {
        if i == self.lo {
            self.w_lo
        } else if i == self.hi {
            self.w_hi
        } else {
            AXIS_TAP_SCALE
        }
    }
}

/// Compute one axis's box-filter taps: the footprint `[f − half, f + half]`
/// (texel-center coordinates around `f`) is intersected with each texel, and
/// each texel's weight is its overlap fraction. Texels the box only grazes
/// therefore contribute proportionally instead of counting whole — the
/// area-correct minification weighting (PDFium `CStretchEngine` analog).
///
/// The box is clipped to the image, so edge-of-image pixels normalize over
/// the in-image weight only. `None` when the clipped box is empty.
pub(crate) fn axis_box_taps(f: f64, half: f64, len: u32) -> Option<AxisTaps> {
    if len == 0 || !f.is_finite() {
        return None;
    }
    // Texel j covers [j, j+1) in edge coordinates; the pixel center `f` sits
    // at edge coordinate f + 0.5.
    let left = (f + 0.5 - half).max(0.0);
    let right = (f + 0.5 + half).min(len as f64);
    if right <= left {
        // Footprint entirely off-image: take the nearest edge texel whole
        // (matches the previous clamped behavior for callers that sample
        // slightly outside).
        let nearest = clampi(f.round() as i64, len);
        return Some(AxisTaps {
            lo: nearest,
            hi: nearest,
            w_lo: AXIS_TAP_SCALE,
            w_hi: AXIS_TAP_SCALE,
            total: AXIS_TAP_SCALE,
        });
    }
    let lo = clampi(left.floor() as i64, len);
    let hi = clampi((right.ceil() - 1.0) as i64, len).max(lo);
    let scale = AXIS_TAP_SCALE as f64;
    if lo == hi {
        let w = (((right - left).min(1.0) * scale).round() as u64).max(1);
        return Some(AxisTaps {
            lo,
            hi,
            w_lo: w,
            w_hi: w,
            total: w,
        });
    }
    let w_lo = ((((lo + 1) as f64 - left).clamp(0.0, 1.0) * scale).round() as u64).max(1);
    let w_hi = (((right - hi as f64).clamp(0.0, 1.0) * scale).round() as u64).max(1);
    let interior = (hi - lo).saturating_sub(1) as u64;
    Some(AxisTaps {
        lo,
        hi,
        w_lo,
        w_hi,
        total: w_lo + w_hi + interior * AXIS_TAP_SCALE,
    })
}

/// Mix a bilevel image's two LUT colors by the weighted ones fraction
/// (`ones_w / weight`), rounding to nearest.
#[inline]
pub(crate) fn mix_bilevel(zero: [u8; 4], one: [u8; 4], ones_w: u64, weight: u64) -> [u8; 4] {
    if ones_w == 0 {
        return zero;
    }
    if ones_w >= weight {
        return one;
    }
    let zeros_w = weight - ones_w;
    let mut out = [0u8; 4];
    for c in 0..4 {
        let acc = zero[c] as u64 * zeros_w + one[c] as u64 * ones_w;
        out[c] = ((acc + weight / 2) / weight) as u8;
    }
    out
}

#[inline]
fn weighted_average_rgba(acc: [u64; 4], weight: u64) -> Option<[u8; 4]> {
    (weight != 0).then(|| {
        [
            ((acc[0] + weight / 2) / weight).min(255) as u8,
            ((acc[1] + weight / 2) / weight).min(255) as u8,
            ((acc[2] + weight / 2) / weight).min(255) as u8,
            ((acc[3] + weight / 2) / weight).min(255) as u8,
        ]
    })
}

/// Stencil `/Mask` test at unit-square `(u, v)`: sample the 1-bit mask (its own
/// geometry) and return whether the base pixel is masked out. A mask sample of
/// 1 (after the mask's `/Decode`, default `[0 1]`) hides; 0 paints — the
/// reverse of an `/SMask` luminosity.
fn stencil_hides(sm: &ImageSMask, u: f64, v: f64) -> bool {
    if sm.width == 0 || sm.height == 0 {
        return false;
    }
    let col = clampi((u * sm.width as f64) as i64, sm.width) as usize;
    let row = clampi(((1.0 - v) * sm.height as f64) as i64, sm.height) as usize;
    let bpc = sm.bits_per_component.max(1) as usize;
    let row_bits = (sm.width as usize * bpc).div_ceil(8) * 8;
    let bit = row * row_bits + col * bpc;
    let raw = read_bits(&sm.samples, bit, bpc);
    // `/Decode [1 0]` swaps the polarity.
    let inverted = sm
        .decode
        .as_ref()
        .and_then(|d| d.first())
        .map(|p| p[0] > p[1])
        .unwrap_or(false);
    if inverted { raw == 0 } else { raw != 0 }
}

/// Look up an Indexed palette entry and convert it through the base space.
fn indexed_rgb(base: &ImageColorSpace, _hival: u32, lookup: &[u8], idx: usize) -> [f32; 3] {
    let bn = base.components();
    let off = idx * bn;
    let get = |c: usize| {
        lookup
            .get(off + c)
            .map(|b| *b as f32 / 255.0)
            .unwrap_or(0.0)
    };
    match base {
        ImageColorSpace::Gray => {
            let g = get(0);
            [g, g, g]
        }
        ImageColorSpace::Rgb => [get(0), get(1), get(2)],
        ImageColorSpace::Cmyk => pdf_color::cmyk_to_rgb(get(0), get(1), get(2), get(3)),
        // A Lab palette entry: bytes are the normalised axes (the Indexed
        // lookup is always 8-bit), so map them onto the space's ranges.
        ImageColorSpace::Lab { white_point, range } => {
            let axis = |v: f32, lo: f32, hi: f32| lo + v * (hi - lo);
            pdf_color::lab_to_rgb(
                axis(get(0), 0.0, 100.0),
                axis(get(1), range[0], range[1]),
                axis(get(2), range[2], range[3]),
                *white_point,
                *range,
            )
        }
        // Nested Indexed is not valid; treat as gray.
        ImageColorSpace::Indexed { .. } => {
            let g = get(0);
            [g, g, g]
        }
        // An Indexed palette over a Separation base: each palette entry is a
        // tint; route it through the baked LUT.
        ImageColorSpace::TintLut { rgb } => tint_lut_rgb(rgb, get(0)),
        ImageColorSpace::TintLut2 { rgb } => tint_lut2_rgb(rgb, get(0), get(1)),
        ImageColorSpace::IccRgb { trc, matrix } => {
            pdf_color::icc::to_srgb_with(trc, matrix, [get(0), get(1), get(2)])
        }
    }
}

/// Map a normalized tint (`0..1`, `/Decode` already applied) through a baked
/// 256-entry sample→sRGB `/Separation` table.
/// Map a normalised pair of tints through a baked `256 x 256 x 3` table.
fn tint_lut2_rgb(rgb: &[u8], a: f32, b: f32) -> [f32; 3] {
    let ia = ((a.clamp(0.0, 1.0) * 255.0).round() as usize).min(255);
    let ib = ((b.clamp(0.0, 1.0) * 255.0).round() as usize).min(255);
    let o = (ia * 256 + ib) * 3;
    match rgb.get(o..o + 3) {
        Some(p) => [
            p[0] as f32 / 255.0,
            p[1] as f32 / 255.0,
            p[2] as f32 / 255.0,
        ],
        None => [0.0, 0.0, 0.0],
    }
}

fn tint_lut_rgb(rgb: &[u8], tint: f32) -> [f32; 3] {
    let idx = ((tint.clamp(0.0, 1.0) * 255.0).round() as usize).min(255);
    let o = idx * 3;
    match rgb.get(o..o + 3) {
        Some(p) => [
            p[0] as f32 / 255.0,
            p[1] as f32 / 255.0,
            p[2] as f32 / 255.0,
        ],
        None => [0.0, 0.0, 0.0],
    }
}

/// Sample a grayscale soft mask at unit-square `(u, v)` → alpha in `[0, 1]`.
fn sample_smask(sm: &ImageSMask, u: f64, v: f64) -> f32 {
    if sm.width == 0 || sm.height == 0 {
        return 1.0;
    }
    let col = clampi((u * sm.width as f64) as i64, sm.width) as usize;
    let row = clampi(((1.0 - v) * sm.height as f64) as i64, sm.height) as usize;
    let row_bits = (sm.width as usize * sm.bits_per_component as usize).div_ceil(8) * 8;
    let bit = row * row_bits + col * sm.bits_per_component as usize;
    let maxv = ((1u64 << sm.bits_per_component.min(16)) - 1).max(1) as f32;
    let raw = read_bits(&sm.samples, bit, sm.bits_per_component as usize) as f32 / maxv;
    match sm.decode.as_ref().and_then(|d| d.first()) {
        Some([lo, hi]) => (lo + raw * (hi - lo)).clamp(0.0, 1.0),
        None => raw,
    }
}

/// Read a big-endian `bits`-wide field starting at bit offset `bit`.
fn read_bits(data: &[u8], bit: usize, bits: usize) -> u32 {
    // The byte-aligned widths are the overwhelming majority of real images and
    // are where the sampler's time goes: the minification filter reads every
    // texel of a device pixel's footprint, so a bit-at-a-time loop costs ~24
    // bounds-checked reads per 8-bit RGB texel. Take them whole.
    if bit.is_multiple_of(8) {
        match bits {
            8 => return data.get(bit / 8).copied().unwrap_or(0) as u32,
            16 => {
                let i = bit / 8;
                let hi = data.get(i).copied().unwrap_or(0) as u32;
                let lo = data.get(i + 1).copied().unwrap_or(0) as u32;
                return (hi << 8) | lo;
            }
            _ => {}
        }
    }
    // 1/2/4 bpc, or an unaligned start: walk the bits.
    let mut v: u32 = 0;
    for i in 0..bits {
        let b = bit + i;
        let byte = b / 8;
        let shift = 7 - (b % 8);
        let bitval = data.get(byte).map(|x| (x >> shift) & 1).unwrap_or(0);
        v = (v << 1) | bitval as u32;
    }
    v
}

/// Count set bits in an arbitrary MSB-first bit range. Full source bytes use
/// `count_ones`; only the two boundary fragments are masked individually.
fn count_one_bits(data: &[u8], start_bit: usize, bit_len: usize) -> u32 {
    if bit_len == 0 {
        return 0;
    }

    let mut bit = start_bit;
    let end = start_bit.saturating_add(bit_len);
    let mut count = 0u32;

    if !bit.is_multiple_of(8) {
        let take = (8 - bit % 8).min(end - bit);
        let offset = bit % 8;
        let mask = (0xffu8 >> offset) & (0xffu8 << (8 - offset - take));
        count += (data.get(bit / 8).copied().unwrap_or(0) & mask).count_ones();
        bit += take;
    }

    while bit + 8 <= end {
        count += data.get(bit / 8).copied().unwrap_or(0).count_ones();
        bit += 8;
    }

    if bit < end {
        let take = end - bit;
        let mask = 0xffu8 << (8 - take);
        count += (data.get(bit / 8).copied().unwrap_or(0) & mask).count_ones();
    }
    count
}

#[inline]
fn clampi(v: i64, len: u32) -> u32 {
    // `.max(0)` keeps this total for `len == 0` (i64::clamp panics when
    // min > max); lowering rejects zero-dimension images, this is the backstop.
    v.clamp(0, (len as i64 - 1).max(0)) as u32
}

#[inline]
fn to_u8(v: f32) -> u8 {
    (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use pdf_page_ir::{BlendMode, DeviceRect};

    /// A minimal `PreparedImage` with an identity device→unit-square map, so a
    /// device point `(dx, dy)` in `[0,1)` samples directly. Nearest, no
    /// minification (footprint 1), no soft mask.
    fn img(
        width: u32,
        height: u32,
        color_space: ImageColorSpace,
        samples: Vec<u8>,
        mask: Option<ImageMask>,
    ) -> PreparedImage {
        let sample_lut = build_sample_lut(8, &color_space, None, false);
        PreparedImage {
            origin: pdf_page_ir::PaintOrigin::PageContent,
            bounds: DeviceRect {
                x: 0,
                y: 0,
                width,
                height,
            },
            clip: None,
            clip_has_mask: false,
            inv: Matrix::IDENTITY,
            width,
            height,
            bpc: 8,
            color_space,
            decode: None,
            samples: Arc::from(samples),
            sample_lut,
            smask: None,
            mask,
            interpolation: InterpolationMode::Nearest,
            footprint: [1.0, 1.0],
            is_stencil: false,
            stencil_rgb: [0, 0, 0],
            alpha: 255,
            blend: BlendMode::Normal,
        }
    }

    /// A 1-bit stencil `/Mask` payload of `width x height` from packed bytes.
    fn stencil(width: u32, height: u32, samples: Vec<u8>, decode: Option<[f32; 2]>) -> ImageMask {
        ImageMask::Stencil(Arc::new(ImageSMask {
            width,
            height,
            bits_per_component: 1,
            decode: decode.map(|d| Arc::from(vec![d])),
            samples: Arc::from(samples),
            codec: None,
            codec_data: None,
            codec_parms: None,
        }))
    }

    // 2x1 RGB: left texel red, right texel blue.
    fn rgb_2x1() -> Vec<u8> {
        vec![255, 0, 0, 0, 0, 255]
    }

    #[test]
    fn color_key_hides_in_range_texel_paints_out_of_range() {
        // Key exactly the red texel's raw samples (255, 0, 0).
        let mask = ImageMask::ColorKey(Arc::from(vec![[255, 255], [0, 0], [0, 0]]));
        let im = img(2, 1, ImageColorSpace::Rgb, rgb_2x1(), Some(mask));
        // Left (red) is in range → fully transparent.
        assert_eq!(im.shade(0.25, 0.5), None, "in-range texel is masked out");
        // Right (blue) is out of range → paints opaque blue.
        let blue = im.shade(0.75, 0.5).expect("out-of-range texel paints");
        assert_eq!(&blue[0..3], &[0, 0, 255]);
        assert_eq!(blue[3], 255);
    }

    #[test]
    fn color_key_all_components_must_match() {
        // Range covers red's R and G but not its B (B must be in [10,20], is 0).
        let mask = ImageMask::ColorKey(Arc::from(vec![[255, 255], [0, 0], [10, 20]]));
        let im = img(2, 1, ImageColorSpace::Rgb, rgb_2x1(), Some(mask));
        // Not every component in range → NOT masked; red paints.
        let red = im.shade(0.25, 0.5).expect("partial match does not mask");
        assert_eq!(&red[0..3], &[255, 0, 0]);
    }

    #[test]
    fn malformed_color_key_is_ignored() {
        // Two ranges for a 3-component image: the sampler must ignore it.
        let mask = ImageMask::ColorKey(Arc::from(vec![[0, 255], [0, 255]]));
        let im = img(2, 1, ImageColorSpace::Rgb, rgb_2x1(), Some(mask));
        let red = im
            .shade(0.25, 0.5)
            .expect("malformed mask ignored, texel paints");
        assert_eq!(&red[0..3], &[255, 0, 0]);
    }

    #[test]
    fn stencil_one_masks_zero_paints() {
        // 2x1 stencil: left bit 1 (masked), right bit 0 (paint) → 0b1000_0000.
        let mask = stencil(2, 1, vec![0x80], None);
        let im = img(2, 1, ImageColorSpace::Rgb, rgb_2x1(), Some(mask));
        assert_eq!(im.shade(0.25, 0.5), None, "mask sample 1 → transparent");
        let blue = im.shade(0.75, 0.5).expect("mask sample 0 → paints");
        assert_eq!(&blue[0..3], &[0, 0, 255]);
    }

    #[test]
    fn stencil_decode_inverts_polarity() {
        // Same bits, but /Decode [1 0] swaps: bit 1 → paint, bit 0 → masked.
        let mask = stencil(2, 1, vec![0x80], Some([1.0, 0.0]));
        let im = img(2, 1, ImageColorSpace::Rgb, rgb_2x1(), Some(mask));
        let red = im
            .shade(0.25, 0.5)
            .expect("inverted: mask sample 1 → paints");
        assert_eq!(&red[0..3], &[255, 0, 0]);
        assert_eq!(
            im.shade(0.75, 0.5),
            None,
            "inverted: mask sample 0 → transparent"
        );
    }

    #[test]
    fn no_mask_paints_normally() {
        let im = img(2, 1, ImageColorSpace::Rgb, rgb_2x1(), None);
        assert!(im.shade(0.25, 0.5).is_some());
        assert!(im.shade(0.75, 0.5).is_some());
    }

    #[test]
    fn axis_box_taps_weight_edge_texels_by_overlap() {
        // Footprint [0.5, 2.5] over a 4-texel axis: half of texel 0, all of
        // texel 1, half of texel 2.
        let t = axis_box_taps(1.0, 1.0, 4).unwrap();
        assert_eq!((t.lo, t.hi), (0, 2));
        assert_eq!(t.w_lo, AXIS_TAP_SCALE / 2);
        assert_eq!(t.w_hi, AXIS_TAP_SCALE / 2);
        assert_eq!(t.total, 2 * AXIS_TAP_SCALE);
        assert_eq!(t.weight_at(1), AXIS_TAP_SCALE);
    }

    #[test]
    fn axis_box_taps_clip_to_the_image_and_renormalize() {
        // Footprint [-0.5, 1.5] clips to [0, 1.5]: texel 0 whole + half of 1.
        let t = axis_box_taps(0.0, 1.0, 4).unwrap();
        assert_eq!((t.lo, t.hi), (0, 1));
        assert_eq!(t.w_lo, AXIS_TAP_SCALE);
        assert_eq!(t.w_hi, AXIS_TAP_SCALE / 2);
        assert_eq!(t.total, AXIS_TAP_SCALE + AXIS_TAP_SCALE / 2);
    }

    #[test]
    fn area_average_weights_partial_edge_texels() {
        // 3×1 gray [0, 255, 0], footprint 2: the box [0.5, 2.5] takes half of
        // each dark edge texel → (255·1)/(0.5+1+0.5) = 127.5 → 128. Uniform
        // whole-texel counting (the old weighting) would say 255/3 = 85.
        let mut im = img(3, 1, ImageColorSpace::Gray, vec![0, 255, 0], None);
        im.footprint = [2.0, 1.0];
        assert_eq!(im.shade(0.5, 0.5), Some([128, 128, 128, 255]));
    }

    #[test]
    fn one_bit_minified_average_weights_edges_fractionally() {
        // Same geometry through the packed-bit LUT path: bits [0, 1, 0].
        let color_space = ImageColorSpace::Gray;
        let mut im = img(3, 1, color_space.clone(), vec![0b0100_0000], None);
        im.bpc = 1;
        im.sample_lut = build_sample_lut(1, &color_space, None, false);
        im.footprint = [2.0, 1.0];
        assert_eq!(im.shade(0.5, 0.5), Some([128, 128, 128, 255]));
    }

    #[test]
    fn one_bit_range_count_handles_partial_and_full_bytes() {
        let data = [0b1011_0010, 0b1110_0001, 0b0101_0101];
        for start in 0..24 {
            for len in 0..=24 - start {
                let expected = (start..start + len)
                    .map(|bit| ((data[bit / 8] >> (7 - bit % 8)) & 1) as u32)
                    .sum();
                assert_eq!(
                    count_one_bits(&data, start, len),
                    expected,
                    "start={start} len={len}"
                );
            }
        }
    }
}
