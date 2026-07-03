//! A Rust library for encoding DjVu documents.
//!
//! This crate provides a high-level builder API for creating multi-page DjVu documents
//! from image data with coordinate-based layer positioning.
//!
//! # Quick Start
//!
//! ```ignore
//! use djvu_encoder::{DjvuBuilder, PageBuilder};
//!
//! // Create a document with 10 pages
//! let doc = DjvuBuilder::new(10)
//!     .with_dpi(300)
//!     .with_quality(90)
//!     .build();
//!
//! // Add pages (can be done out-of-order, even in parallel)
//! for i in 0..10 {
//!     let page = PageBuilder::new(i, 2480, 3508)  // A4 @ 300dpi
//!         .with_background(load_background_image(i))
//!         .with_foreground(load_text_layer(i), 50, 100)
//!         .build()?;
//!     doc.add_page(page)?;
//! }
//!
//! // Finalize and save
//! let djvu_bytes = doc.finalize()?;
//! std::fs::write("output.djvu", djvu_bytes)?;
//! ```
//!
//! # Features
//!
//! - **Coordinate-based layers**: Position image layers at specific coordinates
//! - **Out-of-order processing**: Add pages in any sequence
//! - **Thread-safe**: Safe to use from multiple threads
//! - **Automatic masking**: Handles JB2/IW44 layer overlaps
//! - **Optional parallelism**: Enable `rayon` feature for parallel encoding
//!
//! # Image Formats
//!
//! - **Pixmap (RGB/grayscale)**: For IW44 background layers (photos, scans)
//! - **Bitmap (bilevel)**: For JB2 foreground layers (text, graphics)

// Core modules
pub mod annotations;
pub mod doc;
pub mod encode;
pub mod iff;
pub mod image;
pub(crate) mod simd;
pub mod utils;

// Public builder API
pub use doc::{DjvuBuilder, DjvuDocument, ImageLayer, LayerData, Page, PageBuilder};

// Advanced types (for custom encoding workflows)
pub use doc::{PageComponents, PageEncodeParams};

// Image types
pub use image::image_formats::{Bitmap, GrayPixel, Pixel, Pixmap};

// Error types
pub use utils::error::{DjvuError, Result};

/// Name of the active SIMD primitives backend ("scalar", "wide", or "avx2"),
/// selected once at startup via the `DJVU_PRIMITIVES` env var. Diagnostic
/// only — see `llm-docs/SIMD_AND_PARALLELISM_PLAN.md`.
#[doc(hidden)]
pub fn active_primitives_backend() -> &'static str {
    simd::PRIMITIVES.backend
}

/// Name of the active page-component parallelism backend.
#[doc(hidden)]
pub fn active_parallel_backend() -> &'static str {
    if cfg!(feature = "rayon") {
        "rayon"
    } else {
        "serial"
    }
}

// Constants
pub const DJVU_VERSION: &str = "0.10.0";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert_eq!(DJVU_VERSION, "0.10.0");
    }

    #[test]
    fn test_public_api_builder() {
        let doc = DjvuBuilder::new(1).with_dpi(300).build();
        assert_eq!(doc.total_pages(), 1);
        assert_eq!(doc.pages_ready(), 0);
        assert!(!doc.is_complete());
    }

    #[test]
    fn test_page_builder() {
        let page = PageBuilder::new(0, 100, 100);
        assert_eq!(page.dimensions(), (100, 100));
        assert_eq!(page.page_number(), 0);
    }

    #[test]
    fn test_djvm_dirm_offsets_match_page_positions() -> Result<()> {
        use byteorder::{BigEndian, ReadBytesExt};
        use std::io::Cursor;
        use std::io::Read;

        let white = Pixel::white();
        let bg = Pixmap::from_pixel(1, 1, white);

        let doc = DjvuBuilder::new(2).with_dpi(300).build();
        let page0 = PageBuilder::new(0, 1, 1)
            .with_background(bg.clone())?
            .build()?;
        let page1 = PageBuilder::new(1, 1, 1).with_background(bg)?.build()?;

        doc.add_page(page0)?;
        doc.add_page(page1)?;

        let djvu_bytes = doc.finalize()?;
        // IFF structure: AT&T(4) + FORM(4) + size(4) + DJVM(4) = 16-byte header
        assert!(djvu_bytes.starts_with(b"AT&TFORM"));
        assert_eq!(&djvu_bytes[12..16], b"DJVM");

        // Parse DIRM chunk header (starts at offset 16)
        let mut cursor = Cursor::new(&djvu_bytes);
        cursor.set_position(16);
        let mut id = [0u8; 4];
        cursor.read_exact(&mut id)?;
        assert_eq!(&id, b"DIRM");
        let dirm_size = cursor.read_u32::<BigEndian>()? as usize;
        let dirm_data_start = cursor.position() as usize;
        let dirm_data_end = dirm_data_start + dirm_size;
        let dirm_pad = dirm_size % 2;

        // Read DIRM offsets (bundled header)
        let dirm_data = &djvu_bytes[dirm_data_start..dirm_data_end];
        let version = dirm_data[0];
        assert!(version & 0x80 != 0, "DIRM should be bundled");
        let file_count = u16::from_be_bytes([dirm_data[1], dirm_data[2]]) as usize;
        assert_eq!(file_count, 2);

        let offsets_start = 3;
        let first_offset = u32::from_be_bytes([
            dirm_data[offsets_start],
            dirm_data[offsets_start + 1],
            dirm_data[offsets_start + 2],
            dirm_data[offsets_start + 3],
        ]) as usize;

        // First page should start right after DIRM chunk (NAVM is disabled)
        let first_page_pos = dirm_data_end + dirm_pad;

        assert_eq!(
            first_offset, first_page_pos,
            "DIRM offset should match actual page position"
        );

        // Verify the page chunk starts with FORM
        assert_eq!(
            &djvu_bytes[first_page_pos..first_page_pos + 4],
            b"FORM",
            "Page data should start with FORM chunk"
        );

        Ok(())
    }
}
