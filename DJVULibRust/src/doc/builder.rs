//! Public builder API for creating DjVu documents
//!
//! This module provides the main public API for creating DjVu documents with
//! coordinate-based image layers and out-of-order page processing.
//!
//! # Examples
//!
//! ```ignore
//! use djvu_encoder::{DjvuBuilder, PageBuilder, ImageLayer};
//!
//! // Create a 10-page document
//! let doc = DjvuBuilder::new(10)
//!     .with_dpi(300)
//!     .with_quality(90)
//!     .build();
//!
//! // Add pages in any order
//! for page_num in 0..10 {
//!     let page = PageBuilder::new(page_num, 2480, 3508)
//!         .with_background(load_photo(page_num))
//!         .with_foreground(load_text(page_num), 100, 100)
//!         .build()?;
//!     doc.add_page(page)?;
//! }
//!
//! // Finalize and get bytes
//! let djvu_bytes = doc.finalize()?;
//! std::fs::write("output.djvu", djvu_bytes)?;
//! ```

use crate::doc::encoder::DocumentEncoder;
use crate::doc::page_collection::PageCollection;
use crate::doc::page_encoder::PageEncodeParams;
use crate::doc::page_encoder::{EncodedPage, PageComponents, Rect};
use crate::encode::symbol_dict::BitImage;
use crate::image::image_formats::{Bitmap, Pixmap};
use crate::annotations::{Annotations, hidden_text::HiddenText};
use crate::{DjvuError, Result};
use std::sync::Arc;

// ============================================================================
// Image Layers
// ============================================================================

/// Represents a positioned image layer on a page
#[derive(Debug, Clone)]
pub struct ImageLayer {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub data: LayerData,
}

/// The type and data of an image layer
#[derive(Debug, Clone)]
pub enum LayerData {
    /// IW44-encoded background layer (RGB color or grayscale photo)
    Background(Pixmap),
    /// JB2-encoded foreground layer (bitonal text/graphics)
    Foreground(Bitmap),
    /// JB2-encoded mask layer (bitonal mask)
    Mask(Bitmap),
}

impl ImageLayer {
    /// Creates a background layer from RGB/grayscale image data
    ///
    /// # Arguments
    /// * `data` - Pixmap containing RGB or grayscale pixel data
    /// * `x`, `y` - Position on the page (top-left corner)
    pub fn background(data: Pixmap, x: u32, y: u32) -> Self {
        let (width, height) = data.dimensions();
        Self {
            x,
            y,
            width,
            height,
            data: LayerData::Background(data),
        }
    }

    /// Creates a foreground layer from bitonal bitmap data
    ///
    /// # Arguments
    /// * `data` - Bitmap containing bilevel pixel data (for text, graphics)
    /// * `x`, `y` - Position on the page (top-left corner)
    pub fn foreground(data: Bitmap, x: u32, y: u32) -> Self {
        let (width, height) = data.dimensions();
        Self {
            x,
            y,
            width,
            height,
            data: LayerData::Foreground(data),
        }
    }

    /// Creates a mask layer from bitonal bitmap data
    ///
    /// # Arguments
    /// * `data` - Bitmap containing mask data
    /// * `x`, `y` - Position on the page (top-left corner)
    pub fn mask(data: Bitmap, x: u32, y: u32) -> Self {
        let (width, height) = data.dimensions();
        Self {
            x,
            y,
            width,
            height,
            data: LayerData::Mask(data),
        }
    }

    /// Gets the bounding rectangle of this layer
    pub fn bounds(&self) -> (u32, u32, u32, u32) {
        (self.x, self.y, self.width, self.height)
    }

    /// Checks if this layer overlaps with another layer
    pub fn overlaps_with(&self, other: &ImageLayer) -> bool {
        let (x1, y1, w1, h1) = self.bounds();
        let (x2, y2, w2, h2) = other.bounds();

        let no_overlap = x1 + w1 <= x2 || x2 + w2 <= x1 || y1 + h1 <= y2 || y2 + h2 <= y1;
        !no_overlap
    }
}

// ============================================================================
// Page Builder
// ============================================================================

/// Builder for creating a DjVu page with positioned layers
///
/// # Example
/// ```ignore
/// let page = PageBuilder::new(0, 1000, 1400)
///     .with_background(photo_data)
///     .with_foreground(text_data, 50, 100)
///     .with_ocr_words(vec![
///         ("Hello".to_string(), 100, 200, 150, 50),
///         ("World".to_string(), 260, 200, 180, 50),
///     ])
///     .build()?;
/// ```
pub struct PageBuilder {
    page_num: usize,
    width: u32,
    height: u32,
    layers: Vec<ImageLayer>,
    text_layer: Option<HiddenText>,
    annotations: Option<Annotations>,
}

impl PageBuilder {
    /// Creates a new page builder
    ///
    /// # Arguments
    /// * `page_num` - Zero-based page number
    /// * `width`, `height` - Page dimensions in pixels
    pub fn new(page_num: usize, width: u32, height: u32) -> Self {
        Self {
            page_num,
            width,
            height,
            layers: Vec::new(),
            text_layer: None,
            annotations: None,
        }
    }

    /// Adds an image layer to the page
    pub fn add_layer(mut self, layer: ImageLayer) -> Self {
        self.layers.push(layer);
        self
    }

    /// Convenience: adds a background layer covering the entire page
    pub fn with_background(self, data: Pixmap) -> Result<Self> {
        if data.width() != self.width || data.height() != self.height {
            return Err(DjvuError::InvalidOperation(format!(
                "Background size {}x{} doesn't match page size {}x{}",
                data.width(),
                data.height(),
                self.width,
                self.height
            )));
        }
        Ok(self.add_layer(ImageLayer::background(data, 0, 0)))
    }

    /// Convenience: adds a foreground layer at the specified position
    pub fn with_foreground(self, data: Bitmap, x: u32, y: u32) -> Self {
        self.add_layer(ImageLayer::foreground(data, x, y))
    }

    /// Convenience: adds a mask layer at the specified position
    pub fn with_mask(self, data: Bitmap, x: u32, y: u32) -> Self {
        self.add_layer(ImageLayer::mask(data, x, y))
    }

    /// Returns the configured page dimensions
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Returns the page number
    pub fn page_number(&self) -> usize {
        self.page_num
    }

    /// Returns a reference to the layers
    pub fn layers(&self) -> &[ImageLayer] {
        &self.layers
    }

    /// Detects if masking is needed (JB2 foreground overlaps IW44 background)
    pub fn needs_masking(&self) -> bool {
        for fg_layer in &self.layers {
            if matches!(fg_layer.data, LayerData::Foreground(_)) {
                for bg_layer in &self.layers {
                    if matches!(bg_layer.data, LayerData::Background(_)) {
                        if fg_layer.overlaps_with(bg_layer) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Adds OCR/hidden text layer from coordinate-based word list
    ///
    /// # Arguments
    /// * `words` - Vector of (text, x, y, width, height) tuples for each word
    ///
    /// # Example
    /// ```ignore
    /// let page = PageBuilder::new(0, 2550, 3300)
    ///     .with_ocr_words(vec![
    ///         ("Hello".to_string(), 100, 200, 150, 50),
    ///         ("World".to_string(), 260, 200, 180, 50),
    ///     ])
    ///     .build()?;
    /// ```
    pub fn with_ocr_words(mut self, words: Vec<(String, u16, u16, u16, u16)>) -> Self {
        self.text_layer = Some(HiddenText::from_word_boxes(
            self.width as u16,
            self.height as u16,
            words,
        ));
        self
    }

    /// Adds a custom HiddenText layer (for advanced hierarchical text structures)
    pub fn with_hidden_text(mut self, text: HiddenText) -> Self {
        self.text_layer = Some(text);
        self
    }

    /// Adds a rectangular hyperlink to the page
    ///
    /// # Arguments
    /// * `url` - Target URL
    /// * `x`, `y`, `w`, `h` - Rectangular clickable area
    /// * `comment` - Optional tooltip/comment text
    ///
    /// # Example
    /// ```ignore
    /// let page = PageBuilder::new(0, 2550, 3300)
    ///     .with_hyperlink(
    ///         "https://example.com",
    ///         100, 500, 300, 100,
    ///         "Click here"
    ///     )
    ///     .build()?;
    /// ```
    pub fn with_hyperlink(
        mut self,
        url: impl Into<String>,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        comment: impl Into<String>,
    ) -> Self {
        use crate::annotations::{Hyperlink, AnnotationShape};
        
        let mut annotations = self.annotations.take().unwrap_or_default();
        annotations.hyperlinks.push(Hyperlink {
            shape: AnnotationShape::Rect { x, y, w, h },
            url: url.into(),
            comment: comment.into(),
            target: String::new(),
        });
        self.annotations = Some(annotations);
        self
    }

    /// Adds custom annotations (for advanced usage)
    pub fn with_annotations(mut self, annotations: Annotations) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Consumes the builder and returns the constructed page
    pub fn build(self) -> Result<Page> {
        if self.layers.is_empty() {
            return Err(DjvuError::InvalidOperation(
                "Page must have at least one layer".to_string(),
            ));
        }

        // Validate all layers fit within page bounds
        for layer in &self.layers {
            if layer.x + layer.width > self.width || layer.y + layer.height > self.height {
                return Err(DjvuError::InvalidOperation(format!(
                    "Layer at ({}, {}) with size {}x{} exceeds page bounds {}x{}",
                    layer.x, layer.y, layer.width, layer.height, self.width, self.height
                )));
            }
        }

        Ok(Page {
            page_num: self.page_num,
            width: self.width,
            height: self.height,
            layers: self.layers,
            text_layer: self.text_layer,
            annotations: self.annotations,
        })
    }
}

/// A fully constructed page ready for encoding
#[derive(Debug, Clone)]
pub struct Page {
    page_num: usize,
    width: u32,
    height: u32,
    layers: Vec<ImageLayer>,
    text_layer: Option<HiddenText>,
    annotations: Option<Annotations>,
}

impl Page {
    pub fn page_number(&self) -> usize {
        self.page_num
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn layers(&self) -> &[ImageLayer] {
        &self.layers
    }

    /// Converts this page to PageComponents for internal encoding
    pub(crate) fn to_components(&self) -> Result<PageComponents> {
        let mut components = PageComponents::new_with_dimensions(self.width, self.height);

        for layer in &self.layers {
            match &layer.data {
                LayerData::Background(pixmap) => {
                    let rect = Rect::new(layer.x, layer.y, layer.width, layer.height);
                    components = components.add_iw44_background(pixmap.clone(), rect)?;
                }
                LayerData::Foreground(bitmap) => {
                    let bit_image = bitmap_to_bitimage(bitmap)?;
                    let rect = Rect::new(layer.x, layer.y, layer.width, layer.height);
                    components = components.add_jb2_foreground(bit_image, rect)?;
                }
                LayerData::Mask(bitmap) => {
                    let bit_image = bitmap_to_bitimage(bitmap)?;
                    let rect = Rect::new(layer.x, layer.y, layer.width, layer.height);
                    components = components.add_jb2_mask(bit_image, rect)?;
                }
            }
        }

        // Add text layer and annotations
        if let Some(ref text) = self.text_layer {
            components.text_layer = Some(text.clone());
        }
        if let Some(ref annot) = self.annotations {
            components.annotations = Some(annot.clone());
        }

        Ok(components)
    }
}

/// Helper: convert Bitmap to BitImage
fn bitmap_to_bitimage(bitmap: &Bitmap) -> Result<BitImage> {
    let (width, height) = bitmap.dimensions();
    let mut bit_image = BitImage::new(width, height)
        .map_err(|e| DjvuError::InvalidOperation(format!("Failed to create BitImage: {}", e)))?;

    for y in 0..height {
        for x in 0..width {
            let pixel = bitmap.get_pixel(x, y);
            // Threshold: 0 = white, 1 = black
            let bit = pixel.y < 128;
            bit_image.set_usize(x as usize, y as usize, bit);
        }
    }

    Ok(bit_image)
}

// ============================================================================
// Document Builder
// ============================================================================

/// Main document builder for creating DjVu documents
///
/// Supports out-of-order page insertion and thread-safe operation.
pub struct DjvuBuilder {
    collection: Arc<PageCollection>,
    params: PageEncodeParams,
    dpi: u32,
    gamma: Option<f32>,
}

impl DjvuBuilder {
    /// Creates a new document builder
    ///
    /// # Arguments
    /// * `total_pages` - Total number of pages (numbered 0..total_pages-1)
    pub fn new(total_pages: usize) -> Self {
        Self {
            collection: Arc::new(PageCollection::new(total_pages)),
            params: PageEncodeParams::default(),
            dpi: 300,
            gamma: Some(2.2),
        }
    }

    /// Sets encoding parameters
    pub fn with_params(mut self, params: PageEncodeParams) -> Self {
        self.params = params;
        self
    }

    /// Sets DPI (dots per inch)
    pub fn with_dpi(mut self, dpi: u32) -> Self {
        self.dpi = dpi;
        self.params.dpi = dpi;
        self
    }

    /// Sets gamma correction value
    pub fn with_gamma(mut self, gamma: f32) -> Self {
        self.gamma = Some(gamma);
        self
    }

    /// Sets quality (0-100, higher = better)
    pub fn with_quality(mut self, quality: u8) -> Self {
        self.params.bg_quality = quality;
        self.params.fg_quality = quality;
        self
    }

    /// Enables lossless encoding
    pub fn with_lossless(mut self, lossless: bool) -> Self {
        self.params.lossless = lossless;
        self
    }

    /// Sets target quality in decibels (overrides quality setting)
    /// WARNING: This can cause early termination - prefer with_slices() for reliable encoding
    pub fn with_decibels(mut self, db: f32) -> Self {
        self.params.decibels = Some(db);
        self
    }

    /// Sets the number of slices for IW44 encoding (controls quality)
    /// More slices = better quality, larger files
    /// Default: 74 (C44 standard), recommended range: 50-120
    pub fn with_slices(mut self, slices: usize) -> Self {
        self.params.slices = Some(slices);
        self
    }

    /// Consumes the builder and returns the document
    pub fn build(self) -> DjvuDocument {
        DjvuDocument {
            collection: self.collection,
            params: self.params,
            dpi: self.dpi,
            gamma: self.gamma,
        }
    }
}

/// A DjVu document under construction
///
/// Thread-safe, supports out-of-order page insertion.
pub struct DjvuDocument {
    collection: Arc<PageCollection>,
    params: PageEncodeParams,
    dpi: u32,
    gamma: Option<f32>,
}

impl DjvuDocument {
    /// Total number of pages
    pub fn total_pages(&self) -> usize {
        self.collection.len()
    }

    /// Number of pages added so far
    pub fn pages_ready(&self) -> usize {
        self.collection.ready_count()
    }

    /// Check if a specific page is ready
    pub fn is_page_ready(&self, page_num: usize) -> bool {
        self.collection.is_page_ready(page_num)
    }

    /// Check if all pages are ready
    pub fn is_complete(&self) -> bool {
        self.collection.is_complete()
    }

    /// Add a page (thread-safe, out-of-order)
    pub fn add_page(&self, page: Page) -> Result<()> {
        let page_num = page.page_number();
        let components = page.to_components()?;

        let encoded = EncodedPage::from_components(
            page_num,
            components,
            &self.params,
            self.dpi,
            self.gamma,
        )?;

        self.collection.insert_page(page_num, encoded)
    }

    /// Finalize and return DjVu file bytes
    pub fn finalize(&self) -> Result<Vec<u8>> {
        if !self.is_complete() {
            return Err(DjvuError::InvalidOperation(format!(
                "Document incomplete: {} of {} pages ready",
                self.pages_ready(),
                self.total_pages()
            )));
        }

        let pages = self
            .collection
            .collect_all()
            .ok_or_else(|| DjvuError::InvalidOperation("Failed to collect pages".to_string()))?;

        // Use internal encoder to assemble the document
        DocumentEncoder::assemble_pages(&pages)
    }
}
