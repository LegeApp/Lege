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

use std::sync::{Arc, Mutex, Weak};

use pdf_page_ir::{
    DeviceRect, ImageColorSpace, ImageMask, ImageSMask, InterpolationMode, Matrix, Point,
};
use pdf_render_api::{CancellationToken, RenderError};

use crate::surface::{filled_arc, unique_arc_mut, zeroed_arc};

/// Document-scoped cache of source images converted to the RGB8 upload
/// vocabulary. The decoded-image cache retains compressed-codec output; this
/// companion cache retains only color-converted RGB and deliberately holds a
/// weak reference to the decoded samples so it does not pin both copies.
#[derive(Debug)]
pub(crate) struct SharedRgbImageCache {
    state: Mutex<RgbImageCacheState>,
    budget_bytes: usize,
}

#[derive(Debug, Default)]
struct RgbImageCacheState {
    entries: Vec<RgbImageCacheEntry>,
    bytes: usize,
    clock: u64,
}

#[derive(Debug)]
struct RgbImageCacheEntry {
    source: Weak<[u8]>,
    width: u32,
    height: u32,
    bpc: u8,
    color_space: ImageColorSpace,
    decode: Option<Arc<[[f32; 2]]>>,
    rgb: Arc<[u8]>,
    charge: usize,
    last_used: u64,
}

impl SharedRgbImageCache {
    /// Converted scan pages are commonly 3–20 MiB. This keeps a useful
    /// revisit/tile working set without duplicating the decoded cache's
    /// separate 96 MiB budget.
    const DEFAULT_BUDGET_BYTES: usize = crate::MAX_PREPARED_RGB_CONVERSION_BYTES;

    pub(crate) fn new(budget_bytes: usize) -> Self {
        Self {
            state: Mutex::new(RgbImageCacheState::default()),
            budget_bytes,
        }
    }

    pub(crate) fn get_or_convert(
        &self,
        image: &PreparedImage,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Option<Arc<[u8]>>, RenderError> {
        if image.is_direct_rgb8() {
            return Ok(Some(Arc::clone(&image.samples)));
        }
        let Some(rgb_len) = image.rgb8_len() else {
            return Ok(None);
        };
        // Do not turn a small destination tile into an unbounded full-source
        // conversion. Oversized sources keep using the CPU renderer's
        // destination-driven sampler.
        if rgb_len > self.budget_bytes {
            return Ok(None);
        }

        if let Some(rgb) = self.lookup(image) {
            return Ok(Some(rgb));
        }

        let mut converted = zeroed_arc(rgb_len);
        let width = image.width as usize;
        let converted_data = unique_arc_mut(&mut converted);
        for row in 0..image.height {
            if row % 32 == 0 && cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(RenderError::Cancelled);
            }
            for col in 0..image.width {
                let rgba = image
                    .source_pixel(col, row)
                    .ok_or(RenderError::Unsupported(pdf_page_ir::PageFeatures::IMAGES))?;
                let offset = (row as usize * width + col as usize) * 3;
                converted_data[offset..offset + 3].copy_from_slice(&rgba[..3]);
            }
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(RenderError::Cancelled);
        }
        // Another raster worker may have completed the same conversion while
        // this one was outside the mutex. Reuse its Arc so every caller also
        // converges on one GPU-upload-cache identity.
        if let Some(rgb) = self.lookup(image) {
            return Ok(Some(rgb));
        }
        self.insert(image, Arc::clone(&converted));
        Ok(Some(converted))
    }

    fn lookup(&self, image: &PreparedImage) -> Option<Arc<[u8]>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        let mut hit = None;
        let mut removed_bytes = 0usize;
        state.entries.retain_mut(|entry| {
            let Some(source) = entry.source.upgrade() else {
                removed_bytes = removed_bytes.saturating_add(entry.charge);
                return false;
            };
            if hit.is_none()
                && Arc::ptr_eq(&source, &image.samples)
                && entry.width == image.width
                && entry.height == image.height
                && entry.bpc == image.bpc
                && entry.color_space == image.color_space
                && entry.decode == image.decode
            {
                entry.last_used = clock;
                hit = Some(Arc::clone(&entry.rgb));
            }
            true
        });
        state.bytes = state.bytes.saturating_sub(removed_bytes);
        hit
    }

    fn insert(&self, image: &PreparedImage, rgb: Arc<[u8]>) {
        let charge = rgb.len() + std::mem::size_of::<RgbImageCacheEntry>();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        state.entries.push(RgbImageCacheEntry {
            source: Arc::downgrade(&image.samples),
            width: image.width,
            height: image.height,
            bpc: image.bpc,
            color_space: image.color_space.clone(),
            decode: image.decode.clone(),
            rgb,
            charge,
            last_used: clock,
        });
        state.bytes += charge;
        while state.bytes > self.budget_bytes && state.entries.len() > 1 {
            let victim = state
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| index)
                .unwrap_or(0);
            let removed = state.entries.swap_remove(victim);
            state.bytes = state.bytes.saturating_sub(removed.charge);
        }
    }
}

impl Default for SharedRgbImageCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_BUDGET_BYTES)
    }
}

/// Document-scoped cache of normalized alpha8 image masks used by the GPU
/// preparation seam. The source allocation remains weakly held so this cache
/// cannot pin a decoded mask after the document image cache releases it.
#[derive(Debug)]
pub(crate) struct SharedOpacityImageCache {
    state: Mutex<OpacityImageCacheState>,
    budget_bytes: usize,
}

#[derive(Debug, Default)]
struct OpacityImageCacheState {
    entries: Vec<OpacityImageCacheEntry>,
    bytes: usize,
    clock: u64,
}

#[derive(Debug)]
struct OpacityImageCacheEntry {
    source: Weak<[u8]>,
    width: u32,
    height: u32,
    bpc: u8,
    decode: Option<Arc<[[f32; 2]]>>,
    inverted: bool,
    components: u8,
    color_key: Option<Arc<[[u32; 2]]>>,
    alpha: Arc<[u8]>,
    charge: usize,
    last_used: u64,
}

impl SharedOpacityImageCache {
    pub(crate) fn new(budget_bytes: usize) -> Self {
        Self {
            state: Mutex::new(OpacityImageCacheState::default()),
            budget_bytes,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn get_or_expand(
        &self,
        source: &Arc<[u8]>,
        width: u32,
        height: u32,
        bpc: u8,
        decode: Option<&Arc<[[f32; 2]]>>,
        inverted: bool,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Option<Arc<[u8]>>, RenderError> {
        if !(1..=16).contains(&bpc) {
            return Ok(None);
        }
        let Some(alpha_len) = (width as usize).checked_mul(height as usize) else {
            return Ok(None);
        };
        if alpha_len == 0 || alpha_len > self.budget_bytes {
            return Ok(None);
        }
        if let Some(alpha) = self.lookup(source, width, height, bpc, decode, inverted, 1, None) {
            return Ok(Some(alpha));
        }

        let row_bits = (width as usize * bpc as usize).div_ceil(8) * 8;
        let max_value = ((1u64 << bpc) - 1) as f32;
        let decode_pair = decode.and_then(|pairs| pairs.first()).copied();
        let mut expanded = zeroed_arc(alpha_len);
        let expanded_data = unique_arc_mut(&mut expanded);
        for row in 0..height {
            if row % 32 == 0 && cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(RenderError::Cancelled);
            }
            for column in 0..width {
                let bit = row as usize * row_bits + column as usize * bpc as usize;
                let raw = read_bits(source, bit, bpc as usize) as f32 / max_value;
                let decoded = match decode_pair {
                    Some([lo, hi]) => lo + raw * (hi - lo),
                    None => raw,
                }
                .clamp(0.0, 1.0);
                let opacity = if inverted { 1.0 - decoded } else { decoded };
                expanded_data[row as usize * width as usize + column as usize] = to_u8(opacity);
            }
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(RenderError::Cancelled);
        }
        if let Some(alpha) = self.lookup(source, width, height, bpc, decode, inverted, 1, None) {
            return Ok(Some(alpha));
        }
        self.insert(
            source,
            width,
            height,
            bpc,
            decode.cloned(),
            inverted,
            1,
            None,
            Arc::clone(&expanded),
        );
        Ok(Some(expanded))
    }

    /// Expand a PDF colour-key `/Mask` into tight alpha8. The ranges apply to
    /// the base image's raw components before `/Decode`; a pixel is transparent
    /// only when every component lies inside its corresponding range.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn get_or_expand_color_key(
        &self,
        source: &Arc<[u8]>,
        width: u32,
        height: u32,
        bpc: u8,
        components: usize,
        ranges: &Arc<[[u32; 2]]>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Option<Arc<[u8]>>, RenderError> {
        if !(1..=16).contains(&bpc)
            || components == 0
            || components > u8::MAX as usize
            || ranges.len() != components
        {
            return Ok(None);
        }
        let Some(alpha_len) = (width as usize).checked_mul(height as usize) else {
            return Ok(None);
        };
        if alpha_len == 0 || alpha_len > self.budget_bytes {
            return Ok(None);
        }
        let components = components as u8;
        if let Some(alpha) = self.lookup(
            source,
            width,
            height,
            bpc,
            None,
            false,
            components,
            Some(ranges),
        ) {
            return Ok(Some(alpha));
        }

        let Some(row_sample_bits) = (width as usize)
            .checked_mul(components as usize)
            .and_then(|samples| samples.checked_mul(bpc as usize))
        else {
            return Ok(None);
        };
        let row_bits = row_sample_bits.div_ceil(8) * 8;
        let Some(source_bytes) = row_bits
            .checked_div(8)
            .and_then(|bytes| bytes.checked_mul(height as usize))
        else {
            return Ok(None);
        };
        if source.len() < source_bytes {
            return Ok(None);
        }

        let mut expanded = filled_arc(alpha_len, 255);
        let expanded_data = unique_arc_mut(&mut expanded);
        for row in 0..height {
            if row % 32 == 0 && cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(RenderError::Cancelled);
            }
            for column in 0..width {
                let pixel_bit =
                    row as usize * row_bits + column as usize * components as usize * bpc as usize;
                let masked = ranges.iter().enumerate().all(|(component, &[lo, hi])| {
                    let bit = pixel_bit + component * bpc as usize;
                    let raw = read_bits(source, bit, bpc as usize);
                    raw >= lo && raw <= hi
                });
                if masked {
                    expanded_data[row as usize * width as usize + column as usize] = 0;
                }
            }
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(RenderError::Cancelled);
        }

        if let Some(alpha) = self.lookup(
            source,
            width,
            height,
            bpc,
            None,
            false,
            components,
            Some(ranges),
        ) {
            return Ok(Some(alpha));
        }
        self.insert(
            source,
            width,
            height,
            bpc,
            None,
            false,
            components,
            Some(Arc::clone(ranges)),
            Arc::clone(&expanded),
        );
        Ok(Some(expanded))
    }

    #[allow(clippy::too_many_arguments)]
    fn lookup(
        &self,
        source: &Arc<[u8]>,
        width: u32,
        height: u32,
        bpc: u8,
        decode: Option<&Arc<[[f32; 2]]>>,
        inverted: bool,
        components: u8,
        color_key: Option<&Arc<[[u32; 2]]>>,
    ) -> Option<Arc<[u8]>> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        let mut hit = None;
        let mut removed_bytes = 0usize;
        state.entries.retain_mut(|entry| {
            let Some(cached_source) = entry.source.upgrade() else {
                removed_bytes = removed_bytes.saturating_add(entry.charge);
                return false;
            };
            if hit.is_none()
                && Arc::ptr_eq(&cached_source, source)
                && entry.width == width
                && entry.height == height
                && entry.bpc == bpc
                && entry.decode.as_ref() == decode
                && entry.inverted == inverted
                && entry.components == components
                && entry.color_key.as_ref() == color_key
            {
                entry.last_used = clock;
                hit = Some(Arc::clone(&entry.alpha));
            }
            true
        });
        state.bytes = state.bytes.saturating_sub(removed_bytes);
        hit
    }

    #[allow(clippy::too_many_arguments)]
    fn insert(
        &self,
        source: &Arc<[u8]>,
        width: u32,
        height: u32,
        bpc: u8,
        decode: Option<Arc<[[f32; 2]]>>,
        inverted: bool,
        components: u8,
        color_key: Option<Arc<[[u32; 2]]>>,
        alpha: Arc<[u8]>,
    ) {
        let charge = alpha.len() + std::mem::size_of::<OpacityImageCacheEntry>();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        state.entries.push(OpacityImageCacheEntry {
            source: Arc::downgrade(source),
            width,
            height,
            bpc,
            decode,
            inverted,
            components,
            color_key,
            alpha,
            charge,
            last_used: clock,
        });
        state.bytes += charge;
        while state.bytes > self.budget_bytes && state.entries.len() > 1 {
            let victim = state
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| index)
                .unwrap_or(0);
            let removed = state.entries.swap_remove(victim);
            state.bytes = state.bytes.saturating_sub(removed.charge);
        }
    }
}

impl Default for SharedOpacityImageCache {
    fn default() -> Self {
        Self::new(crate::MAX_PREPARED_OPACITY_CONVERSION_BYTES)
    }
}

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

    fn rgb8_len(&self) -> Option<usize> {
        if self.is_stencil || !(1..=16).contains(&self.bpc) {
            return None;
        }
        let source_bytes = self
            .row_bits()
            .checked_mul(self.height as usize)?
            .checked_div(8)?;
        if self.samples.len() < source_bytes {
            return None;
        }
        (self.width as usize)
            .checked_mul(self.height as usize)?
            .checked_mul(3)
    }

    fn is_direct_rgb8(&self) -> bool {
        self.bpc == 8
            && self.decode.is_none()
            && matches!(self.color_space, ImageColorSpace::Rgb)
            && self.rgb8_len().is_some()
    }

    fn source_pixel(&self, col: u32, row: u32) -> Option<[u8; 4]> {
        self.pixel(col, row)
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
        let taps = (cols.hi - cols.lo + 1) as u64 * (rows.hi - rows.lo + 1) as u64;
        Some((mix_bilevel(zero, one, ones_w, weight), taps))
    }

    /// Weighted set-bit count of one packed source row over an axis range:
    /// fractional first/last taps, full interior taps via popcount. Result
    /// is scaled by `AXIS_TAP_SCALE`.
    fn weighted_row_ones(&self, base_bit: usize, cols: &AxisTaps) -> u64 {
        weighted_row_ones_in(&self.samples, base_bit, cols)
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
        for (axis, keep_ge, bound) in [
            (0, true, 0.0),
            (0, false, 1.0),
            (1, true, 0.0),
            (1, false, 1.0),
        ] {
            let mut m = 0usize;
            for i in 0..n {
                let a = poly[i];
                let b = poly[(i + 1) % n];
                let da = if keep_ge {
                    a[axis] - bound
                } else {
                    bound - a[axis]
                };
                let db = if keep_ge {
                    b[axis] - bound
                } else {
                    bound - b[axis]
                };
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
                // The mask has independent sample dimensions but shares the
                // base image's unit-square placement. Derive its own source
                // texels-per-device-pixel footprint from the image inverse;
                // MRC pages commonly attach a 300–500 dpi bilevel mask to a
                // foreground that is then rendered at screen resolution.
                let mask_footprint = self.smask_footprint(sm);
                let sa = sample_smask(sm, p.x, p.y, mask_footprint);
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

    /// This draw's soft-mask footprint in *mask* texels per device pixel.
    ///
    /// The mask has independent sample dimensions but shares the base image's
    /// unit-square placement, so its footprint comes from the image inverse
    /// scaled by the mask's own size. MRC pages commonly attach a 300–500 dpi
    /// bilevel mask to a foreground rendered at screen resolution.
    pub(crate) fn smask_footprint(&self, sm: &ImageSMask) -> [f64; 2] {
        [
            (self.inv.a.abs() + self.inv.c.abs()) * sm.width as f64,
            (self.inv.b.abs() + self.inv.d.abs()) * sm.height as f64,
        ]
    }

    /// Per-axis box taps for the soft mask at mask-texel coordinate `mfx`,
    /// given the footprint from [`Self::smask_footprint`]. Mirrors the halving
    /// and 0.5 floor [`sample_smask`] applies.
    pub(crate) fn smask_box_taps(f: f64, half_footprint: f64, len: u32) -> Option<AxisTaps> {
        axis_box_taps(f, (half_footprint * 0.5).max(0.5), len)
    }

    /// Area-weighted average of the source texels inside this device pixel's
    /// footprint (box filter with fractional-tap weights, PDFium
    /// `CStretchEngine` analog): every texel contributes proportionally to
    /// its overlap with the footprint, so a texel half inside the box counts
    /// half — uniform whole-texel counting rendered minified stencils and
    /// scans measurably bolder than PDFium.
    ///
    /// A stencil averages *coverage* into alpha, which is what gives a
    /// downscaled `/ImageMask` smooth edges instead of a ragged bitmap.
    fn area_average(&self, fx: f64, fy: f64) -> Option<([u8; 4], u32)> {
        // Preserve the separable fixed-point path for ordinary axis-aligned
        // draws (including quarter turns). For a genuinely rotated or sheared
        // draw, the mapped device pixel is a parallelogram: averaging its
        // enclosing rectangle includes texels the pixel never covers and
        // visibly over-blurs diagonal detail.
        if !self.has_axis_aligned_footprint() {
            return self.parallelogram_average(fx, fy);
        }

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

    /// The two source-texel vectors spanned by one device pixel.
    ///
    /// `inv` maps device coordinates to the image unit square. Source rows
    /// increase downward, hence the sign inversion on the Y components.
    fn source_footprint_basis(&self) -> ([f64; 2], [f64; 2]) {
        (
            [
                self.inv.a * self.width as f64,
                -self.inv.b * self.height as f64,
            ],
            [
                self.inv.c * self.width as f64,
                -self.inv.d * self.height as f64,
            ],
        )
    }

    /// True when the footprint is already an axis-aligned rectangle in source
    /// space. The second case covers 90°/270° rotations, where the device axes
    /// are swapped but the texel footprint is still separable.
    fn has_axis_aligned_footprint(&self) -> bool {
        let (dx, dy) = self.source_footprint_basis();
        const EPS: f64 = 1e-9;
        (dx[1].abs() <= EPS && dy[0].abs() <= EPS) || (dx[0].abs() <= EPS && dy[1].abs() <= EPS)
    }

    /// Area-average over the exact source-space parallelogram covered by one
    /// device pixel. Each candidate texel receives its polygon-overlap area as
    /// a fixed-point weight; clipping against individual texel squares also
    /// clips the footprint to the image at its outer boundary.
    fn parallelogram_average(&self, fx: f64, fy: f64) -> Option<([u8; 4], u32)> {
        const WEIGHT_SCALE: f64 = 1_048_576.0;

        let (dx, dy) = self.source_footprint_basis();
        let hx = [dx[0] * 0.5, dx[1] * 0.5];
        let hy = [dy[0] * 0.5, dy[1] * 0.5];
        let footprint = ConvexQuad::new([
            [fx - hx[0] - hy[0], fy - hx[1] - hy[1]],
            [fx + hx[0] - hy[0], fy + hx[1] - hy[1]],
            [fx + hx[0] + hy[0], fy + hx[1] + hy[1]],
            [fx - hx[0] + hy[0], fy - hx[1] + hy[1]],
        ])?;

        // A texel centred at integer `i` occupies [i-.5, i+.5]. Including a
        // zero-area boundary neighbour is harmless; the overlap test drops it.
        let lo_x = clampi((footprint.bounds[0] + 0.5).floor() as i64, self.width);
        let hi_x = clampi((footprint.bounds[2] + 0.5).floor() as i64, self.width).max(lo_x);
        let lo_y = clampi((footprint.bounds[1] + 0.5).floor() as i64, self.height);
        let hi_y = clampi((footprint.bounds[3] + 0.5).floor() as i64, self.height).max(lo_y);

        let direct_rgb = !self.is_stencil
            && self.bpc == 8
            && self.decode.is_none()
            && matches!(self.color_space, ImageColorSpace::Rgb)
            && self.samples.len() >= self.width as usize * self.height as usize * 3;
        let mut acc = [0u64; 4];
        let mut total_weight = 0u64;
        let mut taps = 0u32;

        for row in lo_y..=hi_y {
            for col in lo_x..=hi_x {
                let area = footprint.rect_overlap_area(
                    col as f64 - 0.5,
                    col as f64 + 0.5,
                    row as f64 - 0.5,
                    row as f64 + 0.5,
                );
                if area <= f64::EPSILON {
                    continue;
                }
                // Keep a genuine but sub-quantum sliver represented, matching
                // the axis tap path's minimum non-zero edge weight.
                let weight = ((area * WEIGHT_SCALE).round() as u64).max(1);
                total_weight = total_weight.checked_add(weight)?;
                taps = taps.saturating_add(1);

                let pixel = if direct_rgb {
                    let offset =
                        (row as usize * self.width as usize + col as usize).checked_mul(3)?;
                    let rgb = self.samples.get(offset..offset + 3)?;
                    Some([rgb[0], rgb[1], rgb[2], 255])
                } else {
                    self.pixel(col, row)
                };
                // Transparent stencil texels contribute zero coverage, but
                // their area remains in the denominator.
                if let Some(pixel) = pixel {
                    for channel in 0..4 {
                        acc[channel] += weight * pixel[channel] as u64;
                    }
                }
            }
        }

        let mut out = weighted_average_rgba(acc, total_weight)?;
        if self.is_stencil {
            out[..3].copy_from_slice(&self.stencil_rgb);
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
            // PDFium/MuPDF's fixed-point image stretch truncates the
            // interpolated channel. Rounding to nearest adds a repeatable
            // one-level bright bias over scan-sized images.
            out[ch] = (top * (1.0 - ty) + bot * ty).clamp(0.0, 255.0) as u8;
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
            ImageColorSpace::IccCmyk { transform } => pdf_color::icc::cmyk_to_srgb_with(
                transform.grid as usize,
                [
                    &transform.input_tables[0],
                    &transform.input_tables[1],
                    &transform.input_tables[2],
                    &transform.input_tables[3],
                ],
                &transform.clut,
                [
                    &transform.output_tables[0],
                    &transform.output_tables[1],
                    &transform.output_tables[2],
                ],
                [comp(0), comp(1), comp(2), comp(3)],
            ),
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

/// One mapped device-pixel footprint, with its inward edge equations and
/// bounds prepared once for all candidate source texels.
struct ConvexQuad {
    vertices: [[f64; 2]; 4],
    /// Inward half-plane equations `a*x + b*y + c >= 0`.
    edges: [[f64; 3]; 4],
    /// `[min_x, min_y, max_x, max_y]`.
    bounds: [f64; 4],
    area: f64,
}

impl ConvexQuad {
    fn new(vertices: [[f64; 2]; 4]) -> Option<Self> {
        let mut twice_area = 0.0;
        for index in 0..4 {
            let a = vertices[index];
            let b = vertices[(index + 1) % 4];
            twice_area += a[0] * b[1] - b[0] * a[1];
        }
        if !twice_area.is_finite() || twice_area.abs() <= f64::EPSILON {
            return None;
        }
        let orientation = twice_area.signum();
        let mut edges = [[0.0; 3]; 4];
        for index in 0..4 {
            let a = vertices[index];
            let b = vertices[(index + 1) % 4];
            let ex = b[0] - a[0];
            let ey = b[1] - a[1];
            edges[index] = [
                -orientation * ey,
                orientation * ex,
                orientation * (ey * a[0] - ex * a[1]),
            ];
        }
        let min_x = vertices.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
        let max_x = vertices
            .iter()
            .map(|p| p[0])
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = vertices.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
        let max_y = vertices
            .iter()
            .map(|p| p[1])
            .fold(f64::NEG_INFINITY, f64::max);
        Some(Self {
            vertices,
            edges,
            bounds: [min_x, min_y, max_x, max_y],
            area: twice_area.abs() * 0.5,
        })
    }

    /// Exact overlap with one texel square. Interior and disjoint texels are
    /// classified from the precomputed half planes; only boundary texels pay
    /// for polygon clipping.
    fn rect_overlap_area(&self, x0: f64, x1: f64, y0: f64, y1: f64) -> f64 {
        if x1 <= self.bounds[0]
            || x0 >= self.bounds[2]
            || y1 <= self.bounds[1]
            || y0 >= self.bounds[3]
        {
            return 0.0;
        }
        let corners = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
        let mut rect_inside = true;
        for &[a, b, c] in &self.edges {
            let mut inside = 0u8;
            for point in corners {
                inside += (a * point[0] + b * point[1] + c >= -1e-12) as u8;
            }
            if inside == 0 {
                return 0.0;
            }
            rect_inside &= inside == 4;
        }
        if rect_inside {
            return 1.0;
        }
        if self.vertices.iter().all(|p| {
            p[0] >= x0 - 1e-12 && p[0] <= x1 + 1e-12 && p[1] >= y0 - 1e-12 && p[1] <= y1 + 1e-12
        }) {
            return self.area.clamp(0.0, 1.0);
        }
        polygon_rect_overlap_area_clipped(&self.vertices, x0, x1, y0, y1)
    }
}

/// Slow boundary case for [`ConvexQuad::rect_overlap_area`]. Successive
/// half-plane clips can add at most one vertex each, so twelve stack slots are
/// ample and avoid allocating in the rotated-image pixel loop.
fn polygon_rect_overlap_area_clipped(
    quad: &[[f64; 2]; 4],
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
) -> f64 {
    let mut polygon = [[0.0; 2]; 12];
    polygon[..4].copy_from_slice(quad);
    let mut count = 4usize;
    let mut scratch = [[0.0; 2]; 12];

    for (axis, keep_greater, bound) in [
        (0usize, true, x0),
        (0usize, false, x1),
        (1usize, true, y0),
        (1usize, false, y1),
    ] {
        let mut next_count = 0usize;
        for index in 0..count {
            let a = polygon[index];
            let b = polygon[(index + 1) % count];
            let da = if keep_greater {
                a[axis] - bound
            } else {
                bound - a[axis]
            };
            let db = if keep_greater {
                b[axis] - bound
            } else {
                bound - b[axis]
            };
            if da >= 0.0 {
                scratch[next_count] = a;
                next_count += 1;
            }
            if (da >= 0.0) != (db >= 0.0) {
                let t = da / (da - db);
                scratch[next_count] = [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])];
                next_count += 1;
            }
        }
        if next_count == 0 {
            return 0.0;
        }
        polygon[..next_count].copy_from_slice(&scratch[..next_count]);
        count = next_count;
    }

    let mut twice_area = 0.0;
    for index in 0..count {
        let a = polygon[index];
        let b = polygon[(index + 1) % count];
        twice_area += a[0] * b[1] - b[0] * a[1];
    }
    (twice_area.abs() * 0.5).clamp(0.0, 1.0)
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
            | ImageColorSpace::IccRgb { .. }
            | ImageColorSpace::IccCmyk { .. } => {
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
    stencil_hides_at(sm, stencil_col(sm, u), stencil_row(sm, v))
}

/// The stencil texel column a unit-square `u` point-samples. Depends only on
/// `u`, so an axis-aligned draw can prepare it once per destination column.
pub(crate) fn stencil_col(sm: &ImageSMask, u: f64) -> u32 {
    clampi((u * sm.width as f64) as i64, sm.width)
}

/// See [`stencil_col`]; rows increase downward, hence the `1 - v`.
pub(crate) fn stencil_row(sm: &ImageSMask, v: f64) -> u32 {
    clampi(((1.0 - v) * sm.height as f64) as i64, sm.height)
}

/// The body of [`stencil_hides`] once the texel has been located.
pub(crate) fn stencil_hides_at(sm: &ImageSMask, col: u32, row: u32) -> bool {
    let (col, row) = (col as usize, row as usize);
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
        ImageColorSpace::IccCmyk { transform } => pdf_color::icc::cmyk_to_srgb_with(
            transform.grid as usize,
            [
                &transform.input_tables[0],
                &transform.input_tables[1],
                &transform.input_tables[2],
                &transform.input_tables[3],
            ],
            &transform.clut,
            [
                &transform.output_tables[0],
                &transform.output_tables[1],
                &transform.output_tables[2],
            ],
            [get(0), get(1), get(2), get(3)],
        ),
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
///
/// Minified masks are box-filtered over the destination pixel's fractional
/// source footprint, exactly like their base image. Point-sampling a 1-bit MRC
/// mask otherwise keeps one source bit in ten or twenty and turns a smooth
/// coverage edge into an arbitrary opaque/transparent choice.
/// Packed row stride, in bits, of a soft mask's samples (rows are byte-aligned).
pub(crate) fn smask_row_bits(sm: &ImageSMask) -> usize {
    (sm.width as usize * sm.bits_per_component as usize).div_ceil(8) * 8
}

/// Fractional set-bit coverage of a **one-bit** soft mask over one destination
/// pixel's prepared per-axis tap boxes, in `0..=1`.
///
/// Shared by [`sample_smask`], which derives the taps per pixel, and the
/// executor's axis-aligned MRC fast path, which hoists them per destination
/// column and row — both then produce the identical alpha byte.
pub(crate) fn bilevel_smask_coverage(sm: &ImageSMask, tx: &AxisTaps, ty: &AxisTaps) -> f32 {
    let row_bits = smask_row_bits(sm);
    let weight = tx.total.saturating_mul(ty.total).max(1);
    let mut ones_w = 0u64;
    for row in ty.lo..=ty.hi {
        let row_ones = weighted_row_ones_in(&sm.samples, row as usize * row_bits, tx);
        ones_w = ones_w.saturating_add(ty.weight_at(row).saturating_mul(row_ones));
    }
    ones_w as f32 / weight as f32
}

/// Apply a soft mask's `/Decode` remap to a normalised sample.
pub(crate) fn apply_smask_decode(sm: &ImageSMask, raw: f32) -> f32 {
    match sm.decode.as_ref().and_then(|d| d.first()) {
        Some([lo, hi]) => (lo + raw * (hi - lo)).clamp(0.0, 1.0),
        None => raw,
    }
}

fn sample_smask(sm: &ImageSMask, u: f64, v: f64, footprint: [f64; 2]) -> f32 {
    if sm.width == 0 || sm.height == 0 {
        return 1.0;
    }
    let row_bits = smask_row_bits(sm);
    let maxv = ((1u64 << sm.bits_per_component.min(16)) - 1).max(1) as f32;
    let fx = u * sm.width as f64 - 0.5;
    let fy = (1.0 - v) * sm.height as f64 - 0.5;

    let raw = if footprint[0] > 1.0 || footprint[1] > 1.0 {
        let Some(tx) = PreparedImage::smask_box_taps(fx, footprint[0], sm.width) else {
            return 1.0;
        };
        let Some(ty) = PreparedImage::smask_box_taps(fy, footprint[1], sm.height) else {
            return 1.0;
        };
        let weight = tx.total.saturating_mul(ty.total).max(1);
        if sm.bits_per_component == 1 {
            bilevel_smask_coverage(sm, &tx, &ty)
        } else {
            let bpc = sm.bits_per_component as usize;
            let mut acc = 0.0f64;
            for row in ty.lo..=ty.hi {
                let wy = ty.weight_at(row) as f64;
                for col in tx.lo..=tx.hi {
                    let bit = row as usize * row_bits + col as usize * bpc;
                    let sample = read_bits(&sm.samples, bit, bpc) as f64 / maxv as f64;
                    acc += wy * tx.weight_at(col) as f64 * sample;
                }
            }
            (acc / weight as f64) as f32
        }
    } else {
        let col = clampi((u * sm.width as f64) as i64, sm.width) as usize;
        let row = clampi(((1.0 - v) * sm.height as f64) as i64, sm.height) as usize;
        let bit = row * row_bits + col * sm.bits_per_component as usize;
        read_bits(&sm.samples, bit, sm.bits_per_component as usize) as f32 / maxv
    };
    apply_smask_decode(sm, raw)
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

/// Weighted set-bit count for an arbitrary packed row and horizontal tap
/// range. Full interior bytes use [`count_one_bits`]; only the fractional edge
/// texels are read individually.
pub(crate) fn weighted_row_ones_in(data: &[u8], base_bit: usize, cols: &AxisTaps) -> u64 {
    let bit_at = |x: u32| -> u64 {
        let b = base_bit + x as usize;
        let byte = data.get(b / 8).copied().unwrap_or(0);
        ((byte >> (7 - (b % 8))) & 1) as u64
    };
    if cols.lo == cols.hi {
        return cols.w_lo * bit_at(cols.lo);
    }
    let mut ones = cols.w_lo * bit_at(cols.lo) + cols.w_hi * bit_at(cols.hi);
    if cols.hi - cols.lo >= 2 {
        let interior = count_one_bits(
            data,
            base_bit + cols.lo as usize + 1,
            (cols.hi - cols.lo - 1) as usize,
        ) as u64;
        ones += AXIS_TAP_SCALE * interior;
    }
    ones
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
    fn prepared_quad_overlap_matches_full_polygon_clipping() {
        let footprints = [
            [[0.2, 0.3], [2.8, 0.6], [3.4, 2.1], [0.7, 1.8]],
            // Clockwise input exercises orientation-independent half planes.
            [[0.7, 1.8], [3.4, 2.1], [2.8, 0.6], [0.2, 0.3]],
            // Entirely contained by one texel.
            [[1.2, 1.2], [1.7, 1.3], [1.6, 1.8], [1.1, 1.7]],
            // Axis-aligned edges and source-boundary crossings.
            [[-0.25, 0.25], [2.25, 0.25], [2.25, 1.75], [-0.25, 1.75]],
        ];

        for vertices in footprints {
            let prepared = ConvexQuad::new(vertices).expect("non-degenerate footprint");
            for y in -2..5 {
                for x in -2..6 {
                    let x0 = f64::from(x);
                    let y0 = f64::from(y);
                    let fast = prepared.rect_overlap_area(x0, x0 + 1.0, y0, y0 + 1.0);
                    let clipped =
                        polygon_rect_overlap_area_clipped(&vertices, x0, x0 + 1.0, y0, y0 + 1.0);
                    assert!(
                        (fast - clipped).abs() <= 1e-12,
                        "footprint={vertices:?} texel=({x},{y}) fast={fast} clipped={clipped}"
                    );
                }
            }
        }
    }

    #[test]
    fn sheared_minification_uses_the_parallelogram_not_its_bounding_box() {
        // One mapped device pixel is a slanted strip of area 2:
        //
        //   (0.5,1.5)──(2.5,1.5)
        //       ╲             ╲
        //        (1.5,2.5)──(3.5,2.5)
        //
        // It overlaps source-row texels 1/2/3 with weights .5/1/.5. Only the
        // middle texel is white, so the exact result is half white (128).
        // The old 3×1 bounding box weighted all three equally and returned 85.
        let mut samples = vec![0u8; 5 * 5 * 3];
        let white = (2 * 5 + 2) * 3;
        samples[white..white + 3].fill(255);
        let mut im = img(5, 5, ImageColorSpace::Rgb, samples, None);
        im.inv = Matrix {
            a: 0.4,
            b: 0.0,
            c: 0.2,
            d: -0.2,
            e: 0.0,
            f: 0.0,
        };
        im.footprint = [3.0, 1.0];

        let (rgba, taps) = im.area_average(2.0, 2.0).expect("covered texels");
        assert_eq!(rgba, [128, 128, 128, 255]);
        assert_eq!(taps, 3);
    }

    #[test]
    fn sheared_stencil_minification_averages_exact_coverage() {
        // Same .5/1/.5 strip as above. Stencil sample 0 paints and 1 is
        // transparent, so only the middle texel contributes: 1 / 2 coverage.
        let mut im = img(5, 5, ImageColorSpace::Gray, vec![0; 25], None);
        im.inv = Matrix {
            a: 0.4,
            b: 0.0,
            c: 0.2,
            d: -0.2,
            e: 0.0,
            f: 0.0,
        };
        im.footprint = [3.0, 1.0];
        im.bpc = 1;
        im.is_stencil = true;
        im.samples = Arc::from(vec![0xf8, 0xf8, 0xd8, 0xf8, 0xf8]);
        im.sample_lut = None;
        im.stencil_rgb = [10, 20, 30];

        let (rgba, taps) = im.area_average(2.0, 2.0).expect("covered texels");
        assert_eq!(rgba, [10, 20, 30, 128]);
        assert_eq!(taps, 3);
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
    fn minified_bilevel_soft_mask_averages_coverage_and_applies_decode() {
        // Bits [1, 0, 0], sampled at the middle texel with a two-texel
        // footprint: half of bit 1 plus all of bit 0 plus half of bit 0 gives
        // raw coverage 0.25. `/Decode [1 0]` inverts that to alpha 0.75.
        // Nearest sampling would read the middle zero and incorrectly return
        // fully opaque alpha 1.0.
        let sm = ImageSMask {
            width: 3,
            height: 1,
            bits_per_component: 1,
            decode: Some(Arc::from(vec![[1.0, 0.0]])),
            samples: Arc::from(vec![0b1000_0000]),
            codec: None,
            codec_data: None,
            codec_parms: None,
        };
        let filtered = sample_smask(&sm, 0.5, 0.5, [2.0, 1.0]);
        let nearest = sample_smask(&sm, 0.5, 0.5, [1.0, 1.0]);
        assert!((filtered - 0.75).abs() < 1e-6, "{filtered}");
        assert_eq!(nearest, 1.0);
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
