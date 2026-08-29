//! Page image export.
//!
//! The rest of this crate hands out raw rasters: a [`RasterPlane`] whose
//! caller is expected to know what a stride is. That is the right shape for
//! the processing pipeline, which feeds those bytes straight into
//! binarization and the encoders, but it is the wrong shape for everyone
//! else. "Give me page 3 as a PNG at 300 DPI" is the first thing every
//! consumer of a PDF renderer asks for, and until this module existed the
//! only answers in the tree were a `pdf-cli` subcommand that wrote
//! uncompressed PPM and a benchmark binary with a hand-rolled PNG writer.
//!
//! Two things make this thin rather than another rasterizer:
//!
//! * [`RenderSession::render`] already composites over
//!   [`Background::White`](pdf_render_api::Background::White), so the surface
//!   that comes back is fully opaque and `rgb_surface_from_rgba` drops the
//!   alpha byte without any unpremultiply step. There is no alpha to
//!   preserve, and so no straight-vs-premultiplied decision to get wrong.
//! * `png` and `jpeg-encoder` are already workspace dependencies compiled for
//!   other members, so this adds no crate to the build graph.
//!
//! What this module owns is the arithmetic between the two: turning a DPI
//! into a pixel size via the *display* extent (so a `/Rotate 90` page exports
//! landscape), refusing sizes that would allocate more than the caller
//! budgeted, and packing a renderer surface whose stride may exceed its row
//! width into the tightly-packed buffer both encoders require.

use std::sync::Arc;

use crate::session::{
    CancellationToken, CompiledDocumentPage, GraySurface, PageGeometry, RasterFormat, RasterPlane,
    RasterProduct, ReadError, RenderSession, RgbSurface,
};

/// Resolution used when a caller does not state one. Matches the `pdfr render`
/// default so the two entry points agree.
pub const DEFAULT_EXPORT_DPI: f64 = 150.0;

/// Default ceiling on `width * height` for one exported page, about 100
/// megapixels — a 300 DPI A0 sheet still fits, while a malformed or hostile
/// `/MediaBox` cannot talk the caller into a multi-gigabyte allocation.
pub const DEFAULT_MAX_EXPORT_PIXELS: u64 = 100_000_000;

/// Largest resolution accepted. Beyond this the pixel-size arithmetic stops
/// being meaningful long before the pixel budget is what stops you.
const MAX_EXPORT_DPI: f64 = 10_000.0;

/// Encoded container for an exported page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// Lossless, alpha-free (the surface is already composited over white).
    Png,
    /// Baseline JPEG. `quality` is clamped to `1..=100`.
    ///
    /// JPEG dimensions are 16-bit, so a page wider or taller than 65535
    /// pixels is rejected rather than silently truncated.
    Jpeg { quality: u8 },
}

/// Colour of the exported raster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportColor {
    /// 8-bit RGB.
    Rgb8,
    /// 8-bit grayscale. Cheaper to render and to encode; note the renderer
    /// produces this as a Rec.709 luma of the *composited* page, not a
    /// separate luminance channel.
    Gray8,
}

/// How to export one page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportOptions {
    pub dpi: f64,
    pub format: ImageFormat,
    pub color: ExportColor,
    /// Reject the export when `width * height` exceeds this.
    pub max_pixels: u64,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            dpi: DEFAULT_EXPORT_DPI,
            format: ImageFormat::Png,
            color: ExportColor::Rgb8,
            max_pixels: DEFAULT_MAX_EXPORT_PIXELS,
        }
    }
}

impl ExportOptions {
    /// RGB PNG at `dpi`.
    pub fn png(dpi: f64) -> Self {
        Self {
            dpi,
            format: ImageFormat::Png,
            ..Self::default()
        }
    }

    /// RGB JPEG at `dpi` and `quality`.
    pub fn jpeg(dpi: f64, quality: u8) -> Self {
        Self {
            dpi,
            format: ImageFormat::Jpeg { quality },
            ..Self::default()
        }
    }

    pub fn with_color(mut self, color: ExportColor) -> Self {
        self.color = color;
        self
    }

    pub fn with_max_pixels(mut self, max_pixels: u64) -> Self {
        self.max_pixels = max_pixels;
        self
    }

    fn raster_format(self) -> RasterFormat {
        match self.color {
            ExportColor::Rgb8 => RasterFormat::Rgb8,
            ExportColor::Gray8 => RasterFormat::Gray8,
        }
    }
}

/// Pixel size a page occupies at `dpi`, using the page's *display* extent so
/// that a page rotated by an odd multiple of 90 degrees reports the swapped
/// axes a viewer would show.
///
/// Both dimensions are at least 1: a degenerate `/CropBox` exports a 1-pixel
/// image rather than failing, which keeps page-range exports from aborting
/// partway through on one malformed page.
pub fn pixel_size_for_dpi(
    geometry: PageGeometry,
    dpi: f64,
    max_pixels: u64,
) -> Result<(u32, u32), ReadError> {
    if !dpi.is_finite() || dpi <= 0.0 {
        return Err(ReadError::InvalidExportOptions(
            "dpi must be finite and greater than zero",
        ));
    }
    if dpi > MAX_EXPORT_DPI {
        return Err(ReadError::InvalidExportOptions("dpi exceeds 10000"));
    }

    let scale = dpi / 72.0;
    let to_pixels = |points: f64| -> u32 {
        let pixels = (points * scale).round();
        if !pixels.is_finite() || pixels < 1.0 {
            1
        } else if pixels > u32::MAX as f64 {
            u32::MAX
        } else {
            pixels as u32
        }
    };

    let width = to_pixels(geometry.display_width());
    let height = to_pixels(geometry.display_height());

    if u64::from(width) * u64::from(height) > max_pixels {
        return Err(ReadError::ExportTooLarge {
            width,
            height,
            max_pixels,
        });
    }
    Ok((width, height))
}

impl RenderSession {
    /// Pixel size page `page` would export at, at `dpi`.
    ///
    /// Useful on its own: a caller sizing a thumbnail cache or a print
    /// preview wants this without paying for the render.
    pub fn page_pixel_size(&self, page: u32, dpi: f64) -> Result<(u32, u32), ReadError> {
        pixel_size_for_dpi(self.page_geometry(page)?, dpi, u64::MAX)
    }

    /// Render page `page` and encode it, returning the file bytes.
    ///
    /// This is the whole job in one call: compile, render at the resolution
    /// `options.dpi` implies, encode. Callers holding a
    /// [`CompiledDocumentPage`] they intend to reuse should prefer
    /// [`RenderSession::export_compiled_page`] to skip recompiling.
    pub fn export_page(&self, page: u32, options: &ExportOptions) -> Result<Vec<u8>, ReadError> {
        self.export_page_cancellable(page, options, None)
    }

    /// [`RenderSession::export_page`] with cooperative cancellation.
    pub fn export_page_cancellable(
        &self,
        page: u32,
        options: &ExportOptions,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, ReadError> {
        let compiled = self.compile(page)?;
        self.export_compiled_page(&compiled, options, cancellation)
    }

    /// Export an already-compiled page.
    pub fn export_compiled_page(
        &self,
        page: &Arc<CompiledDocumentPage>,
        options: &ExportOptions,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, ReadError> {
        let geometry = self.page_geometry(page.page_index())?;
        let (width, height) = pixel_size_for_dpi(geometry, options.dpi, options.max_pixels)?;
        let product = RasterProduct {
            width,
            height,
            format: options.raster_format(),
            crop: None,
        };
        let plane = self.render_cancellable(page, &product, cancellation)?;
        encode_plane(&plane, options.format)
    }
}

/// Encode an already-rendered plane.
///
/// Split out from [`RenderSession::export_compiled_page`] so a caller that
/// rendered through [`RenderSession::render_output_plan`] — or that wants one
/// render encoded into two formats — does not have to rasterize twice.
pub fn encode_plane(plane: &RasterPlane, format: ImageFormat) -> Result<Vec<u8>, ReadError> {
    match plane {
        RasterPlane::Rgb8(surface) => encode_rgb(surface, format),
        RasterPlane::Gray8(surface) => encode_gray(surface, format),
    }
}

fn encode_rgb(surface: &RgbSurface, format: ImageFormat) -> Result<Vec<u8>, ReadError> {
    let packed = pack_rows(
        &surface.pixels,
        surface.width,
        surface.height,
        surface.stride,
        3,
    )?;
    match format {
        ImageFormat::Png => encode_png(&packed, surface.width, surface.height, png::ColorType::Rgb),
        ImageFormat::Jpeg { quality } => encode_jpeg(
            &packed,
            surface.width,
            surface.height,
            quality,
            jpeg_encoder::ColorType::Rgb,
        ),
    }
}

fn encode_gray(surface: &GraySurface, format: ImageFormat) -> Result<Vec<u8>, ReadError> {
    let packed = pack_rows(
        &surface.pixels,
        surface.width,
        surface.height,
        surface.stride,
        1,
    )?;
    match format {
        ImageFormat::Png => encode_png(
            &packed,
            surface.width,
            surface.height,
            png::ColorType::Grayscale,
        ),
        ImageFormat::Jpeg { quality } => encode_jpeg(
            &packed,
            surface.width,
            surface.height,
            quality,
            jpeg_encoder::ColorType::Luma,
        ),
    }
}

/// Copy `height` rows of `width * bytes_per_pixel` out of a possibly-padded
/// surface into a tightly-packed buffer.
///
/// Both encoders want packed rows, and the renderer only guarantees
/// `stride >= width * bytes_per_pixel`. When the surface is already packed
/// this borrows instead of copying, which is the common case for RGB (the
/// facade repacks to `width * 3` on the way out) and for full-page grayscale.
fn pack_rows<'a>(
    pixels: &'a Arc<[u8]>,
    width: u32,
    height: u32,
    stride: usize,
    bytes_per_pixel: usize,
) -> Result<std::borrow::Cow<'a, [u8]>, ReadError> {
    let row_bytes = (width as usize)
        .checked_mul(bytes_per_pixel)
        .ok_or(ReadError::InvalidRasterProduct("row size overflow"))?;
    let height = height as usize;

    if stride == row_bytes {
        let needed = row_bytes
            .checked_mul(height)
            .ok_or(ReadError::InvalidRasterProduct("surface size overflow"))?;
        let packed = pixels
            .get(..needed)
            .ok_or_else(|| ReadError::Encode("surface is shorter than its dimensions".into()))?;
        return Ok(std::borrow::Cow::Borrowed(packed));
    }

    let capacity = row_bytes
        .checked_mul(height)
        .ok_or(ReadError::InvalidRasterProduct("surface size overflow"))?;
    let mut packed = Vec::with_capacity(capacity);
    for row in 0..height {
        let start = row
            .checked_mul(stride)
            .ok_or(ReadError::InvalidRasterProduct("row offset overflow"))?;
        let end = start
            .checked_add(row_bytes)
            .ok_or(ReadError::InvalidRasterProduct("row end overflow"))?;
        let row = pixels
            .get(start..end)
            .ok_or_else(|| ReadError::Encode("surface row is out of bounds".into()))?;
        packed.extend_from_slice(row);
    }
    Ok(std::borrow::Cow::Owned(packed))
}

fn encode_png(
    packed: &[u8],
    width: u32,
    height: u32,
    color: png::ColorType,
) -> Result<Vec<u8>, ReadError> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|error| ReadError::Encode(format!("writing PNG header: {error}")))?;
    writer
        .write_image_data(packed)
        .map_err(|error| ReadError::Encode(format!("writing PNG pixels: {error}")))?;
    writer
        .finish()
        .map_err(|error| ReadError::Encode(format!("finishing PNG stream: {error}")))?;
    Ok(out)
}

fn encode_jpeg(
    packed: &[u8],
    width: u32,
    height: u32,
    quality: u8,
    color: jpeg_encoder::ColorType,
) -> Result<Vec<u8>, ReadError> {
    let width = u16::try_from(width)
        .map_err(|_| ReadError::Encode("JPEG width exceeds 65535 pixels".into()))?;
    let height = u16::try_from(height)
        .map_err(|_| ReadError::Encode("JPEG height exceeds 65535 pixels".into()))?;

    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
    encoder
        .encode(packed, width, height, color)
        .map_err(|error| ReadError::Encode(format!("encoding JPEG: {error}")))?;
    Ok(out)
}
