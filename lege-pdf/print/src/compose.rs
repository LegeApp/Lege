//! Turn a [`Sheet`](crate::Sheet) into pixels using `lege-pdf-read`.
//!
//! A sheet may carry several source pages, so this composes: allocate one
//! sheet raster at the device resolution, render each placement, blit it into
//! place. Composition happens in horizontal bands so a 1200-DPI A4 sheet is
//! not a single 390 MB allocation.

use crate::{PrintError, Sheet};

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

/// A composed sheet raster, top-down, tightly packed.
#[derive(Debug, Clone)]
pub struct SheetRaster {
    pub width: u32,
    pub height: u32,
    /// 1 for grayscale, 3 for RGB.
    pub channels: u8,
    pub pixels: Vec<u8>,
}

/// Compose one sheet.
pub fn compose_sheet(
    session: &lege_pdf_read::RenderSession,
    sheet: &Sheet,
    options: &ComposeOptions,
) -> Result<SheetRaster, PrintError> {
    let _ = (session, sheet, options);
    unimplemented!("phase 2")
}
