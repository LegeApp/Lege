//! Page data types collected by the processing pipeline and handed to the PDF
//! writer.
//!
//! This module used to also contain a lopdf-based PDF builder
//! (`StreamingPdfBuilder`) and an in-memory `PageAccumulator`. Both were
//! replaced by the `lege-pdf-write` crate (arrival-order, append-only,
//! zero-copy) driven by the writer actor in `pipeline::helper_functions`. What
//! remains here are the plain page/element/content DTOs the pipeline builds;
//! the `accumulator::Page` → `lege_pdf_write::PdfPageArtifact` conversion lives
//! in `crate::pdf_artifact`, and hOCR parsing moved to `crate::hocr`.

use std::sync::Arc;

// Use Arc<[u8]> for image data to allow cheap, thread-safe sharing of byte
// buffers without deep copies. This is crucial for performance in a concurrent
// system.
pub type SharedImageData = Arc<[u8]>;

/// Defines the content of a detected region on a page, primarily an encoded
/// image. The `format` string selects the PDF image dictionary downstream (see
/// `crate::pdf_artifact`).
#[derive(Clone, Debug)]
pub enum ContentType {
    EncodedImage {
        data: SharedImageData,
        pixel_width: u32,
        pixel_height: u32,
        /// The image format, e.g. "jpeg", "jbig2", "ccitt".
        format: String,
    },
    Jbig2ImageWithGlobals {
        page_data: SharedImageData,
        global_data: SharedImageData,
        pixel_width: u32,
        pixel_height: u32,
    },
    /// JBIG2 stencil mask for MRC (grayscale) pages: emitted as an
    /// `/ImageMask true` XObject painted with a solid fill color (black) so the
    /// ink cores paint over a grayscale background element while paper stays
    /// transparent. Draw AFTER the background element on the same page.
    Jbig2Mask {
        page_data: SharedImageData,
        global_data: SharedImageData, // may be empty
        pixel_width: u32,
        pixel_height: u32,
    },
}

impl ContentType {
    pub fn width(&self) -> u32 {
        match self {
            ContentType::EncodedImage { pixel_width, .. } => *pixel_width,
            ContentType::Jbig2ImageWithGlobals { pixel_width, .. } => *pixel_width,
            ContentType::Jbig2Mask { pixel_width, .. } => *pixel_width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            ContentType::EncodedImage { pixel_height, .. } => *pixel_height,
            ContentType::Jbig2ImageWithGlobals { pixel_height, .. } => *pixel_height,
            ContentType::Jbig2Mask { pixel_height, .. } => *pixel_height,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            ContentType::EncodedImage { data, .. } => data.is_empty(),
            ContentType::Jbig2ImageWithGlobals { page_data, .. } => page_data.is_empty(),
            ContentType::Jbig2Mask { page_data, .. } => page_data.is_empty(),
        }
    }

    /// Get the image data as bytes for external use.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            ContentType::EncodedImage { data, .. } => data,
            ContentType::Jbig2ImageWithGlobals { page_data, .. } => page_data,
            ContentType::Jbig2Mask { page_data, .. } => page_data,
        }
    }

    /// True for stencil-mask content that must be painted with a fill color
    /// (rather than drawn as an opaque image).
    pub fn is_image_mask(&self) -> bool {
        matches!(self, ContentType::Jbig2Mask { .. })
    }
}

/// Represents an image region that overlays the base content layer.
#[derive(Clone, Debug)]
pub struct ImageRegion {
    /// Position relative to page (in PDF points).
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// The image content (typically JPEG or JP2).
    pub content: ContentType,
    /// Original pixel coordinates for debugging/validation.
    pub pixel_bbox: [u32; 4], // [x1, y1, x2, y2] in pixels
}

/// Represents a single element (like an image) to be placed on a PDF page.
#[derive(Clone, Debug)]
pub struct ContentElement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub content: ContentType,
}

/// Represents a fully processed page, containing all its elements and
/// dimensions. Lightweight and suitable for passing across the pipeline to the
/// writer actor.
#[derive(Clone, Debug)]
pub struct Page {
    pub width: f32,
    pub height: f32,
    pub elements: Vec<ContentElement>,
    pub hocr_text: Option<String>, // HOCR text data for searchable text layer
    pub index: usize,              // Page index (0-based)
    /// Raw binarized image data (0 or 255 per pixel) for DJVU JB2 layer export.
    /// Only populated when DJVU output is requested. For PDF, use JBIG2 in
    /// elements.
    pub binarized: Option<Vec<u8>>,
}
