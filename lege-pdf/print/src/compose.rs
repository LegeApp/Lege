//! Turn a [`Sheet`] into pixels using `lege-pdf-read`.
//!
//! A sheet may carry several source pages, so this composes: allocate one
//! sheet raster at the device resolution, render each placement, blit it into
//! place. Composition happens in horizontal bands so a 1200-DPI A4 sheet is
//! not a single 390 MB allocation.
//!
//! # How a placement is drawn
//!
//! [`Placement::transform`](crate::Placement) maps source page points to
//! sheet points, and both spaces are y-up. The renderer, by contrast, hands
//! back a top-down raster of the *whole* display page. Composition therefore
//! works in three steps:
//!
//! 1. Compose `transform` with the sheet's points-to-pixels matrix to get
//!    `F`, page points to sheet pixels.
//! 2. Choose the page raster size from `F`'s column magnitudes, so one page
//!    pixel is one sheet pixel along each axis. All scaling — uniform or
//!    per-axis — is absorbed here, by the renderer, at full quality.
//! 3. What is left, `T`: page pixels to sheet pixels, whose linear part has
//!    unit-length columns. In practice that is a signed permutation — a
//!    quarter turn and/or a mirror — so it is snapped to one exactly and the
//!    blit becomes a pure pixel copy with no resampling at all.
//!
//! A transform that is *not* of that shape (a shear, or a rotation that is
//! not a multiple of 90 degrees) is still drawn rather than refused: `T` is
//! kept as-is and each destination pixel is filled by nearest-neighbour
//! sampling of the page raster, which step 2 already sized to the placement's
//! bounding box. That is lower quality than the exact path, and it is the
//! only case in which composition resamples.

use std::collections::HashMap;
use std::sync::Arc;

use lege_pdf_read::{
    CompiledDocumentPage, DeviceCrop, RasterFormat, RasterPlane, RasterProduct, RenderSession,
};

use crate::paper::Rect;
use crate::{Matrix, PrintError, Sheet};

/// Highest composition resolution accepted. Past this the DPI arithmetic
/// stops being meaningful well before the pixel budget is what stops you.
const MAX_COMPOSE_DPI: f64 = 10_000.0;

/// How far a normalized placement transform may stray from an exact signed
/// permutation before composition falls back to resampling.
///
/// The normalization in step 2 above rounds the page raster to whole pixels,
/// so the residual is on the order of one part in the raster's width. A
/// hundred-pixel-wide placement therefore lands within a percent, and the
/// tolerance has to be looser than that.
const QUARTER_TURN_TOLERANCE: f64 = 0.02;

/// How a sheet is turned into pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComposeOptions {
    /// Composition resolution. 300 is the default; above 600 the driver's own
    /// scaling is generally indistinguishable at arm's length.
    pub dpi: f64,
    /// Emit one byte per pixel instead of three.
    pub grayscale: bool,
    /// Rows per band. Bands bound the working allocation.
    pub band_rows: u32,
    /// Refuse a sheet larger than this many pixels.
    pub max_pixels: u64,
}

impl Default for ComposeOptions {
    fn default() -> Self {
        Self {
            dpi: 300.0,
            grayscale: false,
            band_rows: 256,
            max_pixels: 400_000_000,
        }
    }
}

impl ComposeOptions {
    /// 1 for grayscale, 3 for RGB.
    #[must_use]
    pub const fn channels(&self) -> u8 {
        if self.grayscale { 1 } else { 3 }
    }

    fn raster_format(&self) -> RasterFormat {
        if self.grayscale {
            RasterFormat::Gray8
        } else {
            RasterFormat::Rgb8
        }
    }

    fn validate(&self) -> Result<(), PrintError> {
        if !self.dpi.is_finite() || self.dpi <= 0.0 {
            return Err(PrintError::InvalidOptions(format!(
                "composition dpi {} must be finite and greater than zero",
                self.dpi
            )));
        }
        if self.dpi > MAX_COMPOSE_DPI {
            return Err(PrintError::InvalidOptions(format!(
                "composition dpi {} exceeds {MAX_COMPOSE_DPI}",
                self.dpi
            )));
        }
        if self.band_rows == 0 {
            return Err(PrintError::InvalidOptions(
                "band_rows must be at least 1".into(),
            ));
        }
        Ok(())
    }
}

/// A composed sheet raster, top-down, tightly packed.
#[derive(Debug, Clone)]
pub struct SheetRaster {
    pub width: u32,
    pub height: u32,
    /// 1 for grayscale, 3 for RGB.
    pub channels: u8,
    pub pixels: Vec<u8>,
}

impl SheetRaster {
    /// Bytes in one row.
    #[must_use]
    pub fn stride(&self) -> usize {
        self.width as usize * self.channels as usize
    }

    /// The pixel at `(x, y)`, as `channels` bytes, or `None` off the raster.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<&[u8]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let channels = self.channels as usize;
        let offset = y as usize * self.stride() + x as usize * channels;
        self.pixels.get(offset..offset + channels)
    }
}

/// One horizontal strip of a composed sheet, handed to the caller as soon as
/// it is finished.
///
/// Both target spooler APIs accept banded delivery, so a spooler can stream a
/// sheet to the device without ever holding the whole raster.
#[derive(Debug, Clone)]
pub struct Band {
    /// Row of the sheet this band starts at.
    pub y: u32,
    /// Rows in this band. The last band of a sheet may be short.
    pub height: u32,
    /// Sheet width in pixels. Every band of a sheet has the same width.
    pub width: u32,
    /// 1 for grayscale, 3 for RGB.
    pub channels: u8,
    /// `width * height * channels` bytes, top-down, tightly packed.
    pub pixels: Vec<u8>,
}

/// Pixel size of `sheet` at `options.dpi`, before anything is rendered.
///
/// Useful on its own: a GUI sizing a preview widget, or a spooler declaring
/// a bitmap to the driver, wants this without paying for the composition.
/// Both dimensions are at least 1.
pub fn sheet_pixel_size(sheet: &Sheet, options: &ComposeOptions) -> Result<(u32, u32), PrintError> {
    options.validate()?;
    let bounds = sheet.bounds.normalized();
    let scale = options.dpi / 72.0;
    let width = points_to_pixels(bounds.width(), scale);
    let height = points_to_pixels(bounds.height(), scale);
    if u64::from(width) * u64::from(height) > options.max_pixels {
        return Err(PrintError::SheetTooLarge {
            width,
            height,
            max_pixels: options.max_pixels,
        });
    }
    Ok((width, height))
}

/// Compose one sheet, handing each finished band to `f`.
///
/// Bands arrive top to bottom, are `options.band_rows` tall except possibly
/// the last, and are only ever borrowed for the duration of the call — a
/// spooler that wants to keep one takes `band.pixels` by clone.
///
/// Source pages are compiled once per sheet and rendered once per placement,
/// lazily: a placement is rasterized when the first band it touches is
/// composed and dropped after the last, so a sheet never holds more page
/// rasters than the band currently crossing them.
pub fn compose_sheet_banded(
    session: &RenderSession,
    sheet: &Sheet,
    options: &ComposeOptions,
    mut f: impl FnMut(&Band) -> Result<(), PrintError>,
) -> Result<(), PrintError> {
    let (width, height) = sheet_pixel_size(sheet, options)?;
    let channels = options.channels();
    let channel_count = channels as usize;
    let band_bytes = (width as usize)
        .checked_mul(options.band_rows.min(height) as usize)
        .and_then(|pixels| pixels.checked_mul(channel_count))
        .ok_or(PrintError::SheetTooLarge {
            width,
            height,
            max_pixels: options.max_pixels,
        })?;

    let plans = plan_placements(session, sheet, options, width, height)?;

    let mut compiled: HashMap<u32, Arc<CompiledDocumentPage>> = HashMap::new();
    let mut rendered: Vec<Option<PageRaster>> = Vec::new();
    rendered.resize_with(plans.len(), || None);

    let mut pixels: Vec<u8> = Vec::new();
    let mut y = 0u32;
    while y < height {
        let rows = options.band_rows.min(height - y);
        pixels.clear();
        pixels.reserve(band_bytes);
        pixels.resize(width as usize * rows as usize * channel_count, 0xFF);

        for (slot, plan) in plans.iter().enumerate() {
            if plan.dest.y1 <= y || plan.dest.y0 >= y + rows {
                continue;
            }
            if rendered[slot].is_none() {
                rendered[slot] = Some(render_placement(session, &mut compiled, plan, options)?);
            }
            let Some(source) = rendered[slot].as_ref() else {
                continue;
            };
            blit(&mut pixels, width, y, rows, channel_count, plan, source);
        }

        let mut band = Band {
            y,
            height: rows,
            width,
            channels,
            pixels: std::mem::take(&mut pixels),
        };
        f(&band)?;
        // Recycle the allocation: the callback only borrowed it, and a
        // hundred bands should not be a hundred allocations.
        pixels = std::mem::take(&mut band.pixels);
        y += rows;

        for (slot, plan) in plans.iter().enumerate() {
            if plan.dest.y1 <= y {
                rendered[slot] = None;
            }
        }
    }
    Ok(())
}

/// Compose one sheet into a single raster.
///
/// This is [`compose_sheet_banded`] accumulating into one buffer, and is the
/// right entry point when the whole sheet is wanted at once — a preview, a
/// PNG on disk, a driver that takes a complete bitmap. A spooler that can
/// stream should prefer the banded form.
pub fn compose_sheet(
    session: &RenderSession,
    sheet: &Sheet,
    options: &ComposeOptions,
) -> Result<SheetRaster, PrintError> {
    let (width, height) = sheet_pixel_size(sheet, options)?;
    let channels = options.channels();
    let capacity = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(channels as usize))
        .ok_or(PrintError::SheetTooLarge {
            width,
            height,
            max_pixels: options.max_pixels,
        })?;

    let mut pixels = Vec::with_capacity(capacity);
    compose_sheet_banded(session, sheet, options, |band| {
        pixels.extend_from_slice(&band.pixels);
        Ok(())
    })?;
    Ok(SheetRaster {
        width,
        height,
        channels,
        pixels,
    })
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// A half-open rectangle of whole pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelRect {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl PixelRect {
    const EMPTY: Self = Self {
        x0: 0,
        y0: 0,
        x1: 0,
        y1: 0,
    };

    fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    fn intersect(self, other: Self) -> Self {
        let x0 = self.x0.max(other.x0);
        let y0 = self.y0.max(other.y0);
        Self {
            x0,
            y0,
            x1: self.x1.min(other.x1).max(x0),
            y1: self.y1.min(other.y1).max(y0),
        }
    }

    fn width(self) -> u32 {
        self.x1 - self.x0
    }

    fn height(self) -> u32 {
        self.y1 - self.y0
    }
}

/// Everything needed to draw one placement, computed before any rendering.
#[derive(Debug, Clone)]
struct PlacementPlan {
    source_page: u32,
    /// Size of the full page raster this placement's geometry is expressed
    /// against, in pixels.
    page_width: u32,
    page_height: u32,
    /// The sub-rectangle of that raster actually needed once clipping is
    /// applied. Rendering only this is what keeps an `ActualSize` job from
    /// rasterizing a poster to print its corner.
    source_crop: PixelRect,
    /// Sheet pixels to page raster pixels. The blit is an inverse map, so
    /// this is the direction that gets stored.
    inverse: Matrix,
    /// Where this placement lands on the sheet, already clipped.
    dest: PixelRect,
}

fn plan_placements(
    session: &RenderSession,
    sheet: &Sheet,
    options: &ComposeOptions,
    width: u32,
    height: u32,
) -> Result<Vec<PlacementPlan>, PrintError> {
    let bounds = sheet.bounds.normalized();
    let scale = options.dpi / 72.0;
    // Sheet points (y up, origin at the sheet's lower-left) to sheet pixels
    // (y down, origin at the top-left).
    let device = Matrix::new(
        scale,
        0.0,
        0.0,
        -scale,
        -bounds.x0 * scale,
        bounds.y1 * scale,
    );
    let sheet_rect = PixelRect {
        x0: 0,
        y0: 0,
        x1: width,
        y1: height,
    };
    let imageable =
        pixel_rect_from_points(sheet.imageable.normalized(), bounds, scale).intersect(sheet_rect);

    let mut geometries: HashMap<u32, (f64, f64)> = HashMap::new();
    let mut plans = Vec::with_capacity(sheet.placements.len());
    for placement in &sheet.placements {
        let (page_w, page_h) = match geometries.get(&placement.source_page) {
            Some(&size) => size,
            None => {
                let geometry = session.page_geometry(placement.source_page)?;
                let size = (geometry.display_width(), geometry.display_height());
                geometries.insert(placement.source_page, size);
                size
            }
        };
        if !(page_w > 0.0 && page_h > 0.0) {
            continue;
        }

        let clip =
            pixel_rect_from_points(placement.clip.normalized(), bounds, scale).intersect(imageable);
        if clip.is_empty() {
            continue;
        }

        // Page points to sheet pixels.
        let page_to_sheet = placement.transform.then(device);
        let page_width = points_to_pixels(page_w, column_length(page_to_sheet.a, page_to_sheet.b));
        let page_height = points_to_pixels(page_h, column_length(page_to_sheet.c, page_to_sheet.d));

        // Page raster pixels to page points: undo the raster scale and the
        // renderer's top-down y.
        let raster_to_page = Matrix::new(
            page_w / f64::from(page_width),
            0.0,
            0.0,
            -page_h / f64::from(page_height),
            0.0,
            page_h,
        );
        let forward = raster_to_page.then(page_to_sheet);
        let forward = snap_to_signed_permutation(forward).unwrap_or(forward);
        let Some(inverse) = invert(forward) else {
            // A degenerate transform paints nothing; there is no sensible
            // raster for it and refusing the whole sheet would be worse.
            continue;
        };

        let dest = bounding_pixel_rect(forward, page_width, page_height)
            .intersect(clip)
            .intersect(sheet_rect);
        if dest.is_empty() {
            continue;
        }

        let source_crop = bounding_pixel_rect_of_dest(inverse, dest).intersect(PixelRect {
            x0: 0,
            y0: 0,
            x1: page_width,
            y1: page_height,
        });
        if source_crop.is_empty() {
            continue;
        }

        let source_pixels = u64::from(source_crop.width()) * u64::from(source_crop.height());
        if source_pixels > options.max_pixels {
            return Err(PrintError::SheetTooLarge {
                width: source_crop.width(),
                height: source_crop.height(),
                max_pixels: options.max_pixels,
            });
        }

        plans.push(PlacementPlan {
            source_page: placement.source_page,
            page_width,
            page_height,
            source_crop,
            inverse,
            dest,
        });
    }
    Ok(plans)
}

/// Length of one column of a 2x2 linear part: how many sheet pixels one point
/// along that page axis covers.
fn column_length(x: f64, y: f64) -> f64 {
    let length = x.hypot(y);
    if length.is_finite() { length } else { 0.0 }
}

fn points_to_pixels(points: f64, scale: f64) -> u32 {
    let pixels = (points * scale).round();
    if !pixels.is_finite() || pixels < 1.0 {
        1
    } else if pixels >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        pixels as u32
    }
}

/// A rectangle in sheet points to whole sheet pixels, rounding to nearest so
/// that a cell boundary lands on the same pixel from both sides.
fn pixel_rect_from_points(rect: Rect, bounds: Rect, scale: f64) -> PixelRect {
    let x0 = (rect.x0 - bounds.x0) * scale;
    let x1 = (rect.x1 - bounds.x0) * scale;
    // y is flipped: the top of the rectangle is the smaller pixel row.
    let y0 = (bounds.y1 - rect.y1) * scale;
    let y1 = (bounds.y1 - rect.y0) * scale;
    pixel_rect_from_floats(x0.round(), y0.round(), x1.round(), y1.round())
}

fn pixel_rect_from_floats(x0: f64, y0: f64, x1: f64, y1: f64) -> PixelRect {
    let clamp = |v: f64| -> u32 {
        if !v.is_finite() || v < 0.0 {
            0
        } else if v >= f64::from(u32::MAX) {
            u32::MAX
        } else {
            v as u32
        }
    };
    let (x0, x1) = (clamp(x0.min(x1)), clamp(x0.max(x1)));
    let (y0, y1) = (clamp(y0.min(y1)), clamp(y0.max(y1)));
    PixelRect { x0, y0, x1, y1 }
}

/// The sheet-pixel bounding box of a page raster under `forward`.
fn bounding_pixel_rect(forward: Matrix, page_width: u32, page_height: u32) -> PixelRect {
    let w = f64::from(page_width);
    let h = f64::from(page_height);
    bounding_box(forward, [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)])
}

/// The page-raster bounding box of a sheet-pixel rectangle under `inverse`.
fn bounding_pixel_rect_of_dest(inverse: Matrix, dest: PixelRect) -> PixelRect {
    let x0 = f64::from(dest.x0);
    let y0 = f64::from(dest.y0);
    let x1 = f64::from(dest.x1);
    let y1 = f64::from(dest.y1);
    bounding_box(inverse, [(x0, y0), (x1, y0), (x0, y1), (x1, y1)])
}

fn bounding_box(matrix: Matrix, corners: [(f64, f64); 4]) -> PixelRect {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for (x, y) in corners {
        let (px, py) = matrix.apply(x, y);
        if !px.is_finite() || !py.is_finite() {
            return PixelRect::EMPTY;
        }
        min_x = min_x.min(px);
        min_y = min_y.min(py);
        max_x = max_x.max(px);
        max_y = max_y.max(py);
    }
    // Outward: the sampling loop bounds-checks anyway, and a half-pixel lost
    // at an edge is a visible seam between N-up cells.
    pixel_rect_from_floats(min_x.floor(), min_y.floor(), max_x.ceil(), max_y.ceil())
}

/// Snap a normalized page-pixels-to-sheet-pixels matrix to an exact signed
/// permutation when it is within [`QUARTER_TURN_TOLERANCE`] of one.
///
/// This is what makes the common case exact: after snapping, every
/// destination pixel centre maps to exactly one source pixel centre, so the
/// nearest-neighbour blit copies bytes rather than resampling them.
fn snap_to_signed_permutation(matrix: Matrix) -> Option<Matrix> {
    let near_zero = |v: f64| v.abs() <= QUARTER_TURN_TOLERANCE;
    let near_unit = |v: f64| (v.abs() - 1.0).abs() <= QUARTER_TURN_TOLERANCE;
    if !matrix.e.is_finite() || !matrix.f.is_finite() {
        return None;
    }
    let (e, f) = (matrix.e.round(), matrix.f.round());
    if near_zero(matrix.b) && near_zero(matrix.c) && near_unit(matrix.a) && near_unit(matrix.d) {
        Some(Matrix::new(
            matrix.a.signum(),
            0.0,
            0.0,
            matrix.d.signum(),
            e,
            f,
        ))
    } else if near_zero(matrix.a)
        && near_zero(matrix.d)
        && near_unit(matrix.b)
        && near_unit(matrix.c)
    {
        Some(Matrix::new(
            0.0,
            matrix.b.signum(),
            matrix.c.signum(),
            0.0,
            e,
            f,
        ))
    } else {
        None
    }
}

fn invert(matrix: Matrix) -> Option<Matrix> {
    let det = matrix.a * matrix.d - matrix.b * matrix.c;
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let inverse = Matrix::new(
        matrix.d / det,
        -matrix.b / det,
        -matrix.c / det,
        matrix.a / det,
        (matrix.c * matrix.f - matrix.d * matrix.e) / det,
        (matrix.b * matrix.e - matrix.a * matrix.f) / det,
    );
    if [
        inverse.a, inverse.b, inverse.c, inverse.d, inverse.e, inverse.f,
    ]
    .iter()
    .all(|v| v.is_finite())
    {
        Some(inverse)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Rendering and blitting
// ---------------------------------------------------------------------------

/// One placement's page raster, positioned in the full page raster's pixel
/// grid so the blit can index it without knowing about the crop.
#[derive(Debug, Clone)]
struct PageRaster {
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
    stride: usize,
    channels: usize,
    pixels: Arc<[u8]>,
}

fn render_placement(
    session: &RenderSession,
    compiled: &mut HashMap<u32, Arc<CompiledDocumentPage>>,
    plan: &PlacementPlan,
    options: &ComposeOptions,
) -> Result<PageRaster, PrintError> {
    let page = match compiled.get(&plan.source_page) {
        Some(page) => Arc::clone(page),
        None => {
            let page = session.compile(plan.source_page)?;
            compiled.insert(plan.source_page, Arc::clone(&page));
            page
        }
    };

    let crop = plan.source_crop;
    let mut product = RasterProduct {
        width: crop.width(),
        height: crop.height(),
        format: options.raster_format(),
        crop: None,
    };
    let whole_page =
        crop.x0 == 0 && crop.y0 == 0 && crop.x1 == plan.page_width && crop.y1 == plan.page_height;
    if !whole_page {
        product = product.with_crop(DeviceCrop {
            x: i32::try_from(crop.x0).unwrap_or(i32::MAX),
            y: i32::try_from(crop.y0).unwrap_or(i32::MAX),
            width: crop.width(),
            height: crop.height(),
            page_width: plan.page_width,
            page_height: plan.page_height,
        });
    }

    let plane = session.render(&page, &product)?;
    let (width, height, stride, channels, pixels) = match plane {
        RasterPlane::Rgb8(surface) => (
            surface.width,
            surface.height,
            surface.stride,
            3usize,
            surface.pixels,
        ),
        RasterPlane::Gray8(surface) => (
            surface.width,
            surface.height,
            surface.stride,
            1usize,
            surface.pixels,
        ),
    };
    Ok(PageRaster {
        origin_x: crop.x0,
        origin_y: crop.y0,
        width,
        height,
        stride,
        channels,
        pixels,
    })
}

/// Copy the part of `source` that falls inside this band.
///
/// Sampling is nearest-neighbour on the inverse map. For the snapped signed
/// permutation that is exact — each destination pixel centre lands on one
/// source pixel centre — and for the general fallback it is a genuine
/// nearest-neighbour resample.
fn blit(
    dest: &mut [u8],
    dest_width: u32,
    band_y: u32,
    band_rows: u32,
    channels: usize,
    plan: &PlacementPlan,
    source: &PageRaster,
) {
    if channels != source.channels {
        return;
    }
    let y_start = plan.dest.y0.max(band_y);
    let y_end = plan.dest.y1.min(band_y + band_rows);
    let x_start = plan.dest.x0.min(dest_width);
    let x_end = plan.dest.x1.min(dest_width);
    if x_end <= x_start {
        return;
    }
    let inverse = plan.inverse;
    let row_stride = dest_width as usize * channels;

    for y in y_start..y_end {
        let centre_y = f64::from(y) + 0.5;
        let mut sample_x =
            inverse.a * (f64::from(x_start) + 0.5) + inverse.c * centre_y + inverse.e;
        let mut sample_y =
            inverse.b * (f64::from(x_start) + 0.5) + inverse.d * centre_y + inverse.f;
        let row_base = (y - band_y) as usize * row_stride;

        for x in x_start..x_end {
            let u = sample_x.floor();
            let v = sample_y.floor();
            sample_x += inverse.a;
            sample_y += inverse.b;
            if !(u >= 0.0 && v >= 0.0) {
                continue;
            }
            let u = u as u32;
            let v = v as u32;
            if u < source.origin_x || v < source.origin_y {
                continue;
            }
            let (su, sv) = (
                (u - source.origin_x) as usize,
                (v - source.origin_y) as usize,
            );
            if su >= source.width as usize || sv >= source.height as usize {
                continue;
            }
            let source_offset = sv * source.stride + su * channels;
            let dest_offset = row_base + x as usize * channels;
            let Some(bytes) = source.pixels.get(source_offset..source_offset + channels) else {
                continue;
            };
            let Some(slot) = dest.get_mut(dest_offset..dest_offset + channels) else {
                continue;
            };
            slot.copy_from_slice(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_its_own_inverse() {
        let inverse = invert(Matrix::IDENTITY);
        assert_eq!(inverse, Some(Matrix::IDENTITY));
    }

    #[test]
    fn inverse_round_trips_a_quarter_turn_and_a_shift() {
        let matrix = Matrix::rotate_degrees(90).then(Matrix::translate(30.0, -7.0));
        let Some(inverse) = invert(matrix) else {
            unreachable!("a rotation and a translation are invertible")
        };
        let (x, y) = matrix.apply(4.0, 9.0);
        let (rx, ry) = inverse.apply(x, y);
        assert!((rx - 4.0).abs() < 1e-9, "{rx}");
        assert!((ry - 9.0).abs() < 1e-9, "{ry}");
    }

    #[test]
    fn degenerate_transforms_do_not_invert() {
        assert_eq!(invert(Matrix::scale(0.0, 1.0)), None);
    }

    #[test]
    fn near_unit_scales_snap_to_an_exact_permutation() {
        let snapped = snap_to_signed_permutation(Matrix::new(1.004, 0.0, 0.0, -0.997, 12.4, 8.6));
        assert_eq!(snapped, Some(Matrix::new(1.0, 0.0, 0.0, -1.0, 12.0, 9.0)));
    }

    #[test]
    fn quarter_turns_snap_to_the_transposed_permutation() {
        let snapped = snap_to_signed_permutation(Matrix::new(0.003, -1.002, 0.998, 0.0, 5.0, 5.0));
        assert_eq!(snapped, Some(Matrix::new(0.0, -1.0, 1.0, 0.0, 5.0, 5.0)));
    }

    #[test]
    fn shears_do_not_snap() {
        assert_eq!(
            snap_to_signed_permutation(Matrix::new(0.9, 0.4, 0.4, 0.9, 0.0, 0.0)),
            None
        );
    }

    #[test]
    fn pixel_rect_intersection_never_inverts() {
        let a = PixelRect {
            x0: 0,
            y0: 0,
            x1: 10,
            y1: 10,
        };
        let b = PixelRect {
            x0: 20,
            y0: 20,
            x1: 30,
            y1: 30,
        };
        let overlap = a.intersect(b);
        assert!(overlap.is_empty());
        assert!(overlap.x1 >= overlap.x0 && overlap.y1 >= overlap.y0);
    }

    #[test]
    fn points_map_to_pixels_with_the_y_axis_flipped() {
        let bounds = Rect::new(0.0, 0.0, 100.0, 200.0);
        let rect = pixel_rect_from_points(Rect::new(0.0, 150.0, 50.0, 200.0), bounds, 1.0);
        assert_eq!(
            rect,
            PixelRect {
                x0: 0,
                y0: 0,
                x1: 50,
                y1: 50
            }
        );
    }
}
