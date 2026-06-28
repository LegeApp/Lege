//! A module for accumulating concurrently processed pages and assembling them into a final PDF.
//!
//! This provides two main mechanisms:
//! 1. `PageAccumulator`: A thread-safe collector that gathers `Page` objects from
//!    multiple worker threads, storing them in order until the document is complete.
//! 2. `StreamingPdfBuilder`: An incremental PDF builder for very large documents,
//!    which constructs the PDF page-by-page to keep memory usage low.
//!
//! The main entry point is `assemble_pdf`, which intelligently chooses between a simple
//! in-memory assembly and the `StreamingPdfBuilder` based on the total page count.

use anyhow::{Result, anyhow};
use encoding_rs::WINDOWS_1252;
use flate2::{Compression, write::ZlibEncoder};
use lopdf::{
    Bookmark, Dictionary, Document, Object, ObjectId, SaveOptions, Stream, StringFormat,
    content::{Content, Operation},
};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::{BTreeMap, HashMap, hash_map::Entry};
use std::fs;
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::unicode_font::{UnicodeFontData, get_unicode_font};

// Use Arc<[u8]> for image data to allow cheap, thread-safe sharing of byte buffers
// without deep copies. This is crucial for performance in a concurrent system.
pub type SharedImageData = Arc<[u8]>;

/// Defines the content of a detected region on a page, primarily an encoded image.
/// The `format` string is essential for `lopdf` to create the correct image dictionary.
#[derive(Clone, Debug)]
pub enum ContentType {
    EncodedImage {
        data: SharedImageData,
        pixel_width: u32, // Actual pixel dimensions for image dictionary
        pixel_height: u32,
        /// The image format, e.g., "jpeg", "jbig2", "ccitt".
        format: String,
    },
    Jbig2ImageWithGlobals {
        page_data: SharedImageData,
        global_data: SharedImageData,
        pixel_width: u32, // Actual pixel dimensions for image dictionary
        pixel_height: u32,
    },
}

/// Represents an image region that overlays the base content layer
#[derive(Clone, Debug)]
pub struct ImageRegion {
    /// Position relative to page (in PDF points)
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// The image content (typically JPEG or JP2)
    pub content: ContentType,
    /// Original pixel coordinates for debugging/validation
    pub pixel_bbox: [u32; 4], // [x1, y1, x2, y2] in pixels
}

impl ContentType {
    pub fn width(&self) -> u32 {
        match self {
            ContentType::EncodedImage { pixel_width, .. } => *pixel_width,
            ContentType::Jbig2ImageWithGlobals { pixel_width, .. } => *pixel_width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            ContentType::EncodedImage { pixel_height, .. } => *pixel_height,
            ContentType::Jbig2ImageWithGlobals { pixel_height, .. } => *pixel_height,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            ContentType::EncodedImage { data, .. } => data.is_empty(),
            ContentType::Jbig2ImageWithGlobals { page_data, .. } => page_data.is_empty(),
        }
    }

    /// Get the image data as bytes for external use
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            ContentType::EncodedImage { data, .. } => data,
            ContentType::Jbig2ImageWithGlobals { page_data, .. } => page_data,
        }
    }
}

/// Represents a single element (like an image or text block) to be placed on a PDF page.
#[derive(Clone, Debug)]
pub struct ContentElement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub content: ContentType,
}

/// Represents a fully processed page, containing all its elements and dimensions.
/// This struct is lightweight and suitable for storing in the accumulator.
#[derive(Clone, Debug)]
pub struct Page {
    pub width: f32,
    pub height: f32,
    pub elements: Vec<ContentElement>,
    pub hocr_text: Option<String>, // HOCR text data for searchable text layer
    pub index: usize,              // Page index (0-based)
    /// Raw binarized image data (0 or 255 per pixel) for DJVU JB2 layer export.
    /// Only populated when DJVU output is requested. For PDF, use JBIG2 in elements.
    pub binarized: Option<Vec<u8>>,
}

struct CachedUnicodeFont {
    font: UnicodeFontData,
    font_id: Option<ObjectId>,
    font_descriptor_id: Option<ObjectId>,
    font_file_id: Option<ObjectId>,
    to_unicode_id: Option<ObjectId>,
    cid_font_id: Option<ObjectId>,
}

impl CachedUnicodeFont {
    fn new(font: UnicodeFontData) -> Self {
        Self {
            font,
            font_id: None,
            font_descriptor_id: None,
            font_file_id: None,
            to_unicode_id: None,
            cid_font_id: None,
        }
    }

    fn font_name_bytes(&self) -> Vec<u8> {
        to_pdf_name(&self.font.post_script_name)
    }
}

fn identity_to_unicode_cmap() -> Vec<u8> {
    const CMAP: &str = concat!(
        "/CIDInit /ProcSet findresource begin\n",
        "12 dict begin\n",
        "begincmap\n",
        "/CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> def\n",
        "/CMapName /Adobe-Identity-UCS def\n",
        "/CMapType 2 def\n",
        "1 begincodespacerange\n",
        "<0000> <FFFF>\n",
        "endcodespacerange\n",
        "1 beginbfrange\n",
        "<0000> <FFFF> <0000>\n",
        "endbfrange\n",
        "endcmap\n",
        "CMapName currentdict /CMap defineresource pop\n",
        "end\n",
        "end\n",
    );

    CMAP.as_bytes().to_vec()
}

fn to_pdf_name(name: &str) -> Vec<u8> {
    if name.is_empty() {
        return b"EmbeddedUnicode".to_vec();
    }

    let mut bytes = Vec::with_capacity(name.len());
    for ch in name.chars() {
        let b = ch as u32;
        let valid = ch.is_ascii()
            && (ch as u8) >= b'!'
            && (ch as u8) <= b'~'
            && !matches!(
                ch,
                '#' | '%' | '(' | ')' | '<' | '>' | '[' | ']' | '{' | '}' | '/'
            );

        if valid && b <= u8::MAX as u32 {
            bytes.push(ch as u8);
        } else {
            bytes.push(b'_');
        }
    }

    if bytes.is_empty() {
        b"EmbeddedUnicode".to_vec()
    } else {
        bytes
    }
}

use std::sync::atomic::AtomicU64;

// Global memory monitoring
static TOTAL_ACCUMULATED_MEMORY: AtomicU64 = AtomicU64::new(0);

/// A thread-safe accumulator for collecting processed `Page` objects from multiple workers.
///
/// It uses a `BTreeMap` to ensure pages are stored in the correct order, regardless of
/// the order in which they are received.
#[derive(Clone, Debug)]
pub struct PageAccumulator {
    /// The shared, thread-safe map of pages.
    pages: Arc<Mutex<BTreeMap<usize, Page>>>,
    /// The total number of pages expected for the document.
    total_pages: usize,
    /// Track approximate memory used by this accumulator
    approx_memory_used: Arc<AtomicU64>,
}

impl PageAccumulator {
    /// Creates a new accumulator for a document with a known total number of pages.
    pub fn new(total_pages: usize) -> Self {
        Self {
            pages: Arc::new(Mutex::new(BTreeMap::new())),
            total_pages,
            approx_memory_used: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Estimate memory usage of a page in bytes
    fn estimate_page_memory_usage(&self, page: &Page) -> u64 {
        let mut total = 0u64;

        // Base page struct fields
        total += std::mem::size_of::<Page>() as u64;

        // Elements
        total += (std::mem::size_of::<ContentElement>() as u64) * page.elements.len() as u64;
        for element in &page.elements {
            match &element.content {
                ContentType::EncodedImage { data, .. } => {
                    total += data.len() as u64;
                }
                ContentType::Jbig2ImageWithGlobals {
                    page_data,
                    global_data,
                    ..
                } => {
                    total += page_data.len() as u64;
                    total += global_data.len() as u64;
                }
            }
        }

        // HOCR text
        if let Some(ref text) = page.hocr_text {
            total += text.len() as u64;
        }

        // Binarized data
        if let Some(ref binarized) = page.binarized {
            total += binarized.len() as u64;
        }

        total
    }

    /// Adds a processed page to the collection.
    ///
    /// # Arguments
    /// * `page_index`: The 0-based index of the page.
    /// * `page`: The processed `Page` object.
    ///
    /// # Returns
    /// `true` if all expected pages have been collected, `false` otherwise.
    pub fn add_page(&self, page_index: usize, page: Page) -> bool {
        // Estimate memory usage before inserting
        let page_memory = self.estimate_page_memory_usage(&page);
        let old_memory = self
            .approx_memory_used
            .load(std::sync::atomic::Ordering::Relaxed);
        let new_memory = old_memory + page_memory;

        // Update the accumulator's memory tracking
        self.approx_memory_used
            .store(new_memory, std::sync::atomic::Ordering::Relaxed);

        // Update the global memory counter
        TOTAL_ACCUMULATED_MEMORY.fetch_add(page_memory, std::sync::atomic::Ordering::Relaxed);

        let mut pages = self.pages.lock().unwrap();
        // The BTreeMap handles out-of-order insertion gracefully.
        pages.insert(page_index, page);
        let current_count = pages.len();
        let is_complete = current_count == self.total_pages;

        is_complete
    }

    /// Checks if all pages have been collected.
    pub fn is_complete(&self) -> bool {
        self.pages.lock().unwrap().len() == self.total_pages
    }

    /// Get approximate memory usage in bytes
    pub fn get_approx_memory_usage(&self) -> u64 {
        self.approx_memory_used
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Check if memory usage is approaching safe limits
    pub fn is_memory_over_limit(&self, max_mb: u64) -> bool {
        let current_mb = self.get_approx_memory_usage() / (1024 * 1024);
        current_mb > max_mb
    }

    /// Returns a list of page indices that have not yet been collected. Useful for diagnostics
    /// when the pipeline appears hung or the final page never triggers assembly.
    pub fn missing_indices(&self) -> Vec<usize> {
        let guard = self.pages.lock().unwrap();
        let mut missing = Vec::new();
        for idx in 0..self.total_pages {
            if !guard.contains_key(&idx) {
                missing.push(idx);
            }
        }
        missing
    }

    /// Returns the total number of pages expected for this document.
    pub fn total_pages(&self) -> usize {
        self.total_pages
    }

    /// Retrieves all pages, sorted by page number, consuming the accumulator.
    ///
    /// This should only be called once all pages have been collected.
    pub fn into_pages(self) -> Vec<Page> {
        let guard = self.pages.lock().unwrap();
        // BTreeMap's values are already sorted by key (page_index).
        guard.values().cloned().collect()
    }

    /// Retrieves all pages without consuming the accumulator.
    /// Safe to call when you just need a snapshot of current pages.
    pub fn snapshot_pages(&self) -> Vec<Page> {
        let guard = self.pages.lock().unwrap();
        guard.values().cloned().collect()
    }

    /// Get a single page by index if it has been collected.
    pub fn get_page(&self, index: usize) -> Option<Page> {
        let guard = self.pages.lock().unwrap();
        guard.get(&index).cloned()
    }
}

/// A builder for creating PDF documents incrementally, suitable for large documents.
/// It adds pages one by one, managing object IDs and resources efficiently.
pub struct StreamingPdfBuilder {
    doc: Document,
    page_ids: Vec<ObjectId>,
    global_resources: HashMap<Vec<u8>, ObjectId>, // Shared resources like JBIG2 globals
    unicode_font: Option<CachedUnicodeFont>,
    bookmarks: Vec<crate::pagerender::OwnedBookmarkNode>,
    /// source page index → output page index (empty = identity mapping for non-reflow)
    source_to_output_idx: HashMap<usize, usize>,
}

impl StreamingPdfBuilder {
    /// Creates a new StreamingPdfBuilder with an empty document.
    pub fn new() -> Self {
        Self {
            doc: Document::new(),
            page_ids: Vec::new(),
            global_resources: HashMap::new(),
            unicode_font: get_unicode_font().map(CachedUnicodeFont::new),
            bookmarks: Vec::new(),
            source_to_output_idx: HashMap::new(),
        }
    }

    /// Store bookmarks to be written when finalize() is called.
    pub fn set_bookmarks(&mut self, bookmarks: Vec<crate::pagerender::OwnedBookmarkNode>) {
        self.bookmarks = bookmarks;
    }

    /// For reflow output: map source page index → output page index so bookmark destinations
    /// can be wired to the correct output page. Non-reflow paths leave this empty (identity assumed).
    pub fn set_source_to_output_map(&mut self, map: HashMap<usize, usize>) {
        self.source_to_output_idx = map;
    }

    fn ensure_unicode_font(&mut self) -> Result<Option<ObjectId>> {
        let Some(cache) = self.unicode_font.as_mut() else {
            return Ok(None);
        };

        if let Some(font_id) = cache.font_id {
            return Ok(Some(font_id));
        }

        let font_bytes: &[u8] = &cache.font.data;

        let mut font_stream_dict = Dictionary::new();
        font_stream_dict.set("Length1", Object::Integer(font_bytes.len() as i64));
        // FontFile2 must not have a Subtype entry (Subtype is only valid for FontFile3)
        let font_file_id = self
            .doc
            .add_object(Stream::new(font_stream_dict, font_bytes.to_vec()));

        let metrics = &cache.font.metrics;
        let mut descriptor = Dictionary::new();
        descriptor.set("Type", Object::Name(b"FontDescriptor".to_vec()));
        descriptor.set("FontName", Object::Name(cache.font_name_bytes()));
        descriptor.set("Ascent", Object::Integer(metrics.ascent as i64));
        descriptor.set("Descent", Object::Integer(metrics.descent as i64));
        descriptor.set("CapHeight", Object::Integer(metrics.cap_height as i64));
        descriptor.set("ItalicAngle", Object::Real(metrics.italic_angle));

        let mut flags = 32; // Nonsymbolic
        if metrics.italic_angle.abs() > f32::EPSILON {
            flags |= 64; // Italic
        }
        descriptor.set("Flags", Object::Integer(flags));
        descriptor.set("StemV", Object::Integer(80));
        descriptor.set(
            "FontBBox",
            Object::Array(vec![
                Object::Integer(metrics.bbox.x_min as i64),
                Object::Integer(metrics.bbox.y_min as i64),
                Object::Integer(metrics.bbox.x_max as i64),
                Object::Integer(metrics.bbox.y_max as i64),
            ]),
        );
        descriptor.set("FontFile2", Object::Reference(font_file_id));

        let descriptor_id = self.doc.add_object(descriptor);

        let mut cid_system_info = Dictionary::new();
        cid_system_info.set(
            "Registry",
            Object::String(b"Adobe".to_vec(), StringFormat::Literal),
        );
        cid_system_info.set(
            "Ordering",
            Object::String(b"Identity".to_vec(), StringFormat::Literal),
        );
        cid_system_info.set("Supplement", Object::Integer(0));

        let mut cid_font = Dictionary::new();
        cid_font.set("Type", Object::Name(b"Font".to_vec()));
        cid_font.set("Subtype", Object::Name(b"CIDFontType2".to_vec()));
        cid_font.set("BaseFont", Object::Name(cache.font_name_bytes()));
        cid_font.set("CIDSystemInfo", Object::Dictionary(cid_system_info));
        cid_font.set("FontDescriptor", Object::Reference(descriptor_id));
        cid_font.set("CIDToGIDMap", Object::Name(b"Identity".to_vec()));
        cid_font.set("DW", Object::Integer(1000));

        let cid_font_id = self.doc.add_object(cid_font);

        let to_unicode_stream = Stream::new(Dictionary::new(), identity_to_unicode_cmap());
        let to_unicode_id = self.doc.add_object(to_unicode_stream);

        let mut type0_font = Dictionary::new();
        type0_font.set("Type", Object::Name(b"Font".to_vec()));
        type0_font.set("Subtype", Object::Name(b"Type0".to_vec()));
        type0_font.set("BaseFont", Object::Name(cache.font_name_bytes()));
        type0_font.set("Encoding", Object::Name(b"Identity-H".to_vec()));
        type0_font.set(
            "DescendantFonts",
            Object::Array(vec![Object::Reference(cid_font_id)]),
        );
        type0_font.set("ToUnicode", Object::Reference(to_unicode_id));

        let font_id = self.doc.add_object(type0_font);

        cache.font_file_id = Some(font_file_id);
        cache.font_descriptor_id = Some(descriptor_id);
        cache.cid_font_id = Some(cid_font_id);
        cache.to_unicode_id = Some(to_unicode_id);
        cache.font_id = Some(font_id);

        Ok(Some(font_id))
    }

    /// Adds a processed page to the document.
    pub fn add_page(&mut self, page: &Page) -> Result<()> {
        let embedded_font_id = self.ensure_unicode_font()?;
        let page_id = self.doc.new_object_id();
        let mut page_dict = Dictionary::new();
        page_dict.set("Type", Object::Name(b"Page".to_vec()));
        let has_ocr_content = page
            .hocr_text
            .as_ref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false);
        // Note: PDF/A metadata is added during finalize when the document catalog exists.

        // Create content stream for the page
        let mut operations = Vec::new();
        let mut xobject_dict = Dictionary::new();

        // Add HOCR text layer if available (invisible overlay for OCR)
        if let Some(hocr_text) = &page.hocr_text {
            if !hocr_text.trim().is_empty() {
                match self.add_hocr_text_layer(hocr_text, page.width as f32, page.height as f32) {
                    Ok(text_ops) => operations.extend(text_ops),
                    Err(e) => log::warn!("Failed to add HOCR text layer: {}", e),
                }
            }
        }

        for element in &page.elements {
            let xobject_name = format!("Im{}", xobject_dict.len() + 1);
            // Create the image/Form XObject stream and register it under the chosen name
            let xobject_id = self.add_content_to_pdf(&element.content, &mut xobject_dict)?;
            xobject_dict.set::<&str, _>(xobject_name.as_ref(), Object::Reference(xobject_id));

            // Add drawing operation
            operations.push(Operation::new("q", vec![])); // Save graphics state

            // PDF transformation matrix: [a b c d e f] where:
            // a = horizontal scaling, d = vertical scaling
            // e = horizontal translation, f = vertical translation
            // Note: PDF coordinates are bottom-left origin, need to flip Y coordinate
            // XObjects are normalized to 1x1 unit space, so we scale them to the element dimensions
            let pdf_y = page.height - element.y - element.height; // Convert from top-left to bottom-left

            crate::bbox_trace!(
                "ASSEMBLY page={} {} obj={:?} raster_tl=({:.1},{:.1}) size=({:.1},{:.1}) pdf_ty={:.1} content_px={}x{}",
                page.index,
                xobject_name,
                xobject_id,
                element.x,
                element.y,
                element.width,
                element.height,
                pdf_y,
                element.content.width(),
                element.content.height()
            );

            operations.push(Operation::new(
                "cm",
                vec![
                    element.width.into(),  // Scale X to element width
                    0.into(),              // XY skew = 0
                    0.into(),              // YX skew = 0
                    element.height.into(), // Scale Y to element height
                    element.x.into(),      // X translation
                    pdf_y.into(),          // Y translation (flipped coordinate)
                ],
            ));

            // Use a Name object for the Do operator
            operations.push(Operation::new(
                "Do",
                vec![Object::Name(xobject_name.as_bytes().to_vec())],
            ));

            operations.push(Operation::new("Q", vec![])); // Restore graphics state
        }

        // Set up resources
        let mut resources = Dictionary::new();
        resources.set("XObject", Object::Dictionary(xobject_dict));

        // Add font resources for HOCR text layer
        let mut font_dict = Dictionary::new();

        if let Some(font_id) = embedded_font_id {
            font_dict.set("F0", Object::Reference(font_id));
        }

        let mut helvetica_font = Dictionary::new();
        helvetica_font.set("Type", Object::Name(b"Font".to_vec()));
        helvetica_font.set("Subtype", Object::Name(b"Type1".to_vec()));
        helvetica_font.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
        font_dict.set("F1", Object::Dictionary(helvetica_font));
        resources.set("Font", Object::Dictionary(font_dict));

        page_dict.set("Resources", Object::Dictionary(resources));

        // Explicitly set page size to match the provided dimensions
        // MediaBox defines the page boundary: [llx lly urx ury]
        page_dict.set(
            "MediaBox",
            Object::Array(vec![
                0.into(),           // lower-left x
                0.into(),           // lower-left y
                page.width.into(),  // upper-right x
                page.height.into(), // upper-right y
            ]),
        );

        // Create content stream with optional compression
        let content = Content { operations };
        let encoded_content = content.encode()?;

        // Add Flate compression for content streams when they contain HOCR text data
        let content_stream =
            if page.hocr_text.is_some() && !page.hocr_text.as_ref().unwrap().trim().is_empty() {
                // HOCR present — compress stream content
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(&encoded_content)?;
                let compressed_content = encoder.finish()?;
                let mut stream_dict = Dictionary::new();
                stream_dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
                stream_dict.set("Length", Object::Integer(compressed_content.len() as i64));
                self.doc
                    .add_object(Stream::new(stream_dict, compressed_content))
            } else {
                // No HOCR — uncompressed stream
                self.doc
                    .add_object(Stream::new(Dictionary::new(), encoded_content))
            };

        page_dict.set("Contents", Object::Reference(content_stream));

        self.doc
            .objects
            .insert(page_id, Object::Dictionary(page_dict));
        self.page_ids.push(page_id);

        Ok(())
    }

    /// Adds content (image or MRC) to the PDF and returns its object ID.
    fn add_content_to_pdf(
        &mut self,
        content: &ContentType,
        _xobject_dict: &mut Dictionary,
    ) -> Result<ObjectId> {
        match content {
            ContentType::EncodedImage {
                data,
                pixel_width,
                pixel_height,
                format,
            } => {
                let mut dict = Dictionary::new();
                dict.set("Type", Object::Name(b"XObject".to_vec()));
                dict.set("Subtype", Object::Name(b"Image".to_vec()));
                dict.set("Width", Object::Integer(*pixel_width as i64));
                dict.set("Height", Object::Integer(*pixel_height as i64));
                dict.set("Length", Object::Integer(data.len() as i64));
                // Default interpolation for continuous-tone images; overridden for 1-bit images below
                dict.set("Interpolate", true);

                match format.as_ref() {
                    "jpeg" => {
                        // Trust the format tag: 3 components
                        dict.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
                        dict.set("BitsPerComponent", Object::Integer(8));
                        dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
                        // JPEG is already compressed, store raw bytes without re-encoding
                        let stream = Stream::new(dict, data.to_vec());
                        return Ok(self.doc.add_object(stream));
                    }
                    "jpeg-gray" => {
                        dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
                        dict.set("BitsPerComponent", Object::Integer(8));
                        dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
                        // JPEG is already compressed, store raw bytes without re-encoding
                        let stream = Stream::new(dict, data.to_vec());
                        return Ok(self.doc.add_object(stream));
                    }
                    "jp2" => {
                        // Trust the format tag: 3 components
                        dict.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
                        dict.set("BitsPerComponent", Object::Integer(8));
                        dict.set("Filter", Object::Name(b"JPXDecode".to_vec()));
                        // JP2 is already compressed, store raw bytes without re-encoding
                        let stream = Stream::new(dict, data.to_vec());
                        return Ok(self.doc.add_object(stream));
                    }
                    "jp2-gray" => {
                        dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
                        dict.set("BitsPerComponent", Object::Integer(8));
                        dict.set("Filter", Object::Name(b"JPXDecode".to_vec()));
                        // JP2 is already compressed, store raw bytes without re-encoding
                        let stream = Stream::new(dict, data.to_vec());
                        return Ok(self.doc.add_object(stream));
                    }
                    "jbig2" => {
                        dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
                        dict.set("BitsPerComponent", Object::Integer(1));
                        // Interpolation should be disabled for 1-bit bilevel images to avoid artifacts
                        dict.set("Interpolate", false);
                        dict.set("Filter", Object::Name(b"JBIG2Decode".to_vec()));
                        // Match jbig2enc-rust tests/pdf_variants (decode_inverted=false → [0 1]) for
                        // streams from flush_pdf_split(); required for correct polarity with this encoder.
                        dict.set("Decode", vec![0.into(), 1.into()]);
                        // JBIG2 is already compressed, store raw bytes without re-encoding
                        let stream = Stream::new(dict, data.to_vec());
                        return Ok(self.doc.add_object(stream));
                    }
                    "ccitt" | "ccitt4" => {
                        dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
                        dict.set("BitsPerComponent", Object::Integer(1));
                        // Interpolation should be disabled for 1-bit bilevel images to avoid artifacts
                        dict.set("Interpolate", false);
                        dict.set("Filter", Object::Name(b"CCITTFaxDecode".to_vec()));
                        let mut params = Dictionary::new();
                        params.set("K", Object::Integer(-1)); // T.6 Group 4
                        params.set("EndOfBlock", false);
                        params.set("EncodedByteAlign", false);
                        params.set("Columns", Object::Integer(*pixel_width as i64));
                        // Rows is recommended for CCITT Fax decode to ensure proper line count
                        params.set("Rows", Object::Integer(*pixel_height as i64));
                        params.set("BlackIs1", true); // CCITT4 encoded bits: 1=black, 0=white
                        dict.set("DecodeParms", Object::Dictionary(params));
                        // Keep paper white and ink black for this encoder's 1=black bitstream.
                        dict.set("Decode", vec![1.into(), 0.into()]);
                        // CCITT4 data is already compressed, store raw bytes without re-encoding
                        let stream = Stream::new(dict, data.to_vec());
                        return Ok(self.doc.add_object(stream));
                    }
                    "indexed8" => {
                        // Indexed color space with RGB palette
                        // Data format: [768 bytes RGB palette] + [pixel indices]
                        if data.len() < 768 {
                            return Err(anyhow!(
                                "Indexed8 data too short: need at least 768 bytes for palette"
                            ));
                        }

                        let palette_data = &data[0..768];
                        let pixel_data = &data[768..];

                        // Create palette stream object
                        let palette_stream = Stream::new(Dictionary::new(), palette_data.to_vec());
                        let palette_id = self.doc.add_object(palette_stream);

                        // Set up Indexed color space: [/Indexed /DeviceRGB 255 palette_ref]
                        let colorspace = Object::Array(vec![
                            Object::Name(b"Indexed".to_vec()),
                            Object::Name(b"DeviceRGB".to_vec()),
                            Object::Integer(255), // Maximum index value (0-255)
                            Object::Reference(palette_id),
                        ]);
                        dict.set("ColorSpace", colorspace);
                        dict.set("BitsPerComponent", Object::Integer(8));

                        // Create stream with only the pixel data
                        let stream = Stream::new(dict, pixel_data.to_vec());
                        return Ok(self.doc.add_object(stream));
                    }
                    _ => {
                        return Err(anyhow!(
                            "Unsupported image format for PDF creation: {}",
                            format
                        ));
                    }
                }
            }
            ContentType::Jbig2ImageWithGlobals {
                page_data,
                global_data,
                pixel_width,
                pixel_height,
            } => {
                let mut dict = Dictionary::new();
                dict.set("Type", Object::Name(b"XObject".to_vec()));
                dict.set("Subtype", Object::Name(b"Image".to_vec()));
                dict.set("Width", Object::Integer(*pixel_width as i64));
                dict.set("Height", Object::Integer(*pixel_height as i64));
                dict.set("Length", Object::Integer(page_data.len() as i64));
                dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
                dict.set("BitsPerComponent", Object::Integer(1));
                // Interpolation should be disabled for 1-bit bilevel images to avoid artifacts
                dict.set("Interpolate", false);
                dict.set("Filter", Object::Name(b"JBIG2Decode".to_vec()));
                // Keep JBIG2 polarity consistent with the jbig2enc-rust
                // flush_pdf_split() test expectations: decode_inverted=false
                // corresponds to Decode [0 1]. Halftone polarity is handled
                // upstream by inverting the grayscale input before encoding.
                dict.set("Decode", vec![0.into(), 1.into()]);

                // Only attach JBIG2Globals when non-empty; empty stream breaks many viewers.
                if !global_data.is_empty() {
                    let global_bytes = global_data.to_vec();
                    let global_id = match self.global_resources.entry(global_bytes.clone()) {
                        Entry::Occupied(existing) => *existing.get(),
                        Entry::Vacant(vacant) => {
                            let global_stream =
                                Stream::new(Dictionary::new(), global_bytes.clone());
                            let id = self.doc.add_object(global_stream);
                            vacant.insert(id);
                            id
                        }
                    };
                    let mut decode_params = Dictionary::new();
                    decode_params.set("JBIG2Globals", Object::Reference(global_id));
                    dict.set("DecodeParms", Object::Dictionary(decode_params));
                }

                // JBIG2 is already compressed, store raw bytes without re-encoding
                let stream = Stream::new(dict, page_data.to_vec());
                Ok(self.doc.add_object(stream))
            }
        }
    }

    /// Adds invisible HOCR text layer for OCR text searchability
    fn add_hocr_text_layer(
        &mut self,
        hocr_text: &str,
        _page_width: f32,
        page_height: f32,
    ) -> Result<Vec<Operation>> {
        let mut operations = Vec::new();

        // Parse HOCR to extract line positions, then deduplicate adjacent repeats
        let mut lines = parse_hocr(hocr_text)?;
        dedup_adjacent_repeats(&mut lines);

        if lines.is_empty() {
            return Ok(operations);
        }

        let use_unicode_font = self
            .unicode_font
            .as_ref()
            .and_then(|cache| cache.font_id)
            .is_some();
        let font_resource = if use_unicode_font {
            b"F0".to_vec()
        } else {
            b"F1".to_vec()
        };

        // Begin text object with proper text state reset
        operations.push(Operation::new("BT", vec![])); // Begin text
        operations.push(Operation::new("Tr", vec![Object::Integer(3)])); // Invisible text
        operations.push(Operation::new("Tc", vec![Object::Real(0.0)])); // Zero character spacing
        operations.push(Operation::new("Tw", vec![Object::Real(0.0)])); // Zero word spacing
        operations.push(Operation::new("Tz", vec![Object::Real(100.0)])); // 100% horizontal scale (no stretch)
        operations.push(Operation::new(
            "Tf",
            vec![
                Object::Name(font_resource.clone()),
                Object::Real(1.0), // We'll scale with Tm
            ],
        ));

        fn emit_invisible_text(
            operations: &mut Vec<Operation>,
            text: &str,
            x: f32,
            y_pdf: f32,
            font_scale: f32,
            use_unicode_font: bool,
        ) {
            operations.push(Operation::new(
                "Tm",
                vec![
                    Object::Real(font_scale),
                    Object::Real(0.0),
                    Object::Real(0.0),
                    Object::Real(font_scale),
                    Object::Real(x),
                    Object::Real(y_pdf),
                ],
            ));

            let encoded = if use_unicode_font {
                let mut bytes = Vec::with_capacity(text.len() * 2);
                for u in text.encode_utf16() {
                    bytes.extend_from_slice(&u.to_be_bytes());
                }
                bytes
            } else {
                let (cow, _, _) = WINDOWS_1252.encode(text);
                cow.into_owned()
            };

            operations.push(Operation::new(
                "Tj",
                vec![Object::String(encoded, lopdf::StringFormat::Hexadecimal)],
            ));
        }

        for line in lines {
            if !line.words.is_empty() {
                let last_word_index = line.words.len().saturating_sub(1);
                for (i, word) in line.words.iter().enumerate() {
                    let mut word_text = word.text.clone();
                    if i < last_word_index {
                        word_text.push(' ');
                    }
                    emit_invisible_text(
                        &mut operations,
                        &word_text,
                        word.x,
                        page_height - (word.y + word.height),
                        word.height.max(1.0),
                        use_unicode_font,
                    );
                }
                continue;
            }

            if let Some(line_text) = line.raw_text.as_deref() {
                if line_text.is_empty() {
                    continue;
                }
                emit_invisible_text(
                    &mut operations,
                    line_text,
                    line.x,
                    page_height - line.baseline,
                    line.height.max(1.0),
                    use_unicode_font,
                );
            }
        }

        operations.push(Operation::new("ET", vec![])); // End text

        Ok(operations)
    }

    pub fn finalize(mut self, output_path: &str, has_ocr_content: bool) -> Result<()> {
        let mut doc = self.doc;

        // Create Pages dictionary
        let pages_id = doc.add_object(Dictionary::new());
        let pages_dict = doc.get_object_mut(pages_id)?.as_dict_mut()?;
        pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
        pages_dict.set(
            "Kids",
            Object::Array(
                self.page_ids
                    .iter()
                    .map(|&id| Object::Reference(id))
                    .collect(),
            ),
        );
        pages_dict.set("Count", Object::Integer(self.page_ids.len() as i64));

        // Update each page with Parent reference
        for &page_id in &self.page_ids {
            let page_dict = doc.get_object_mut(page_id)?.as_dict_mut()?;
            page_dict.set("Parent", Object::Reference(pages_id));
        }

        // Create Catalog
        let catalog_id = doc.add_object(Dictionary::new());
        let catalog_dict = doc.get_object_mut(catalog_id)?.as_dict_mut()?;
        catalog_dict.set("Type", Object::Name(b"Catalog".to_vec()));
        catalog_dict.set("Pages", Object::Reference(pages_id));

        // PDF/A-1B metadata: applied here so the catalog exists before we modify it.
        // Date format per PDF spec section 7.9.4: D:YYYYMMDDHHmmSSOHH'mm'
        if has_ocr_content {
            doc.version = "1.4".to_string();

            let output_intent = Dictionary::from_iter([
                ("Type", Object::Name(b"OutputIntent".to_vec())),
                ("S", Object::Name(b"GTS_PDFA1".to_vec())),
                (
                    "OutputConditionIdentifier",
                    Object::string_literal("sRGB IEC61966-2.1"),
                ),
                ("Info", Object::string_literal("sRGB IEC61966-2.1")),
                (
                    "OutputCondition",
                    Object::string_literal("sRGB IEC61966-2.1"),
                ),
                (
                    "RegistryName",
                    Object::string_literal("http://www.color.org"),
                ),
            ]);
            let output_intent_id = doc.add_object(output_intent);

            let catalog_dict = doc.get_object_mut(catalog_id)?.as_dict_mut()?;
            catalog_dict.set(
                "OutputIntents",
                Object::Array(vec![Object::Reference(output_intent_id)]),
            );
            catalog_dict.set(
                "MarkInfo",
                Object::Dictionary(Dictionary::from_iter([("Marked", Object::Boolean(true))])),
            );

            let now = chrono::Local::now();
            let total_secs = now.offset().local_minus_utc();
            let abs_secs = total_secs.unsigned_abs();
            let tz_h = abs_secs / 3600;
            let tz_m = (abs_secs % 3600) / 60;
            let sign = if total_secs >= 0 { '+' } else { '-' };
            let pdf_date = format!(
                "D:{}{}{:02}'{:02}'",
                now.format("%Y%m%d%H%M%S"),
                sign,
                tz_h,
                tz_m
            );

            let mut info = Dictionary::new();
            info.set("Producer", Object::string_literal("Lege PDF Generator"));
            info.set("Creator", Object::string_literal("Lege"));
            info.set("CreationDate", Object::string_literal(pdf_date.clone()));
            info.set("ModDate", Object::string_literal(pdf_date));
            info.set("Title", Object::string_literal("Document"));
            info.set("Author", Object::string_literal("Lege"));
            info.set("Subject", Object::string_literal("PDF/A Document"));
            info.set("Keywords", Object::string_literal("PDF/A-1b"));
            info.set("Trapped", Object::Name(b"False".to_vec()));
            let info_id = doc.add_object(info);
            doc.trailer.set("Info", Object::Reference(info_id));
        }

        // Build bookmark outline if any were supplied
        if !self.bookmarks.is_empty() {
            let page_ids = &self.page_ids;
            let source_map = &self.source_to_output_idx;
            // Helper closure: source page index → ObjectId in the output document
            let resolve_page = |src: usize| -> Option<ObjectId> {
                if src == usize::MAX {
                    return None;
                }
                let out_idx = if source_map.is_empty() {
                    src
                } else {
                    *source_map.get(&src)?
                };
                page_ids.get(out_idx).copied()
            };

            fn add_tree(
                doc: &mut Document,
                nodes: &[crate::pagerender::OwnedBookmarkNode],
                parent: Option<u32>,
                resolve: &dyn Fn(usize) -> Option<ObjectId>,
            ) {
                for node in nodes {
                    let page_id = match resolve(node.source_page) {
                        Some(id) => id,
                        None => continue,
                    };
                    let bm = Bookmark::new(node.title.clone(), [0.0, 0.0, 0.0], 0, page_id);
                    let id = doc.add_bookmark(bm, parent);
                    if !node.children.is_empty() {
                        add_tree(doc, &node.children, Some(id), resolve);
                    }
                }
            }

            add_tree(&mut doc, &self.bookmarks, None, &resolve_page);

            if let Some(outline_id) = doc.build_outline() {
                let catalog_dict = doc.get_object_mut(catalog_id)?.as_dict_mut()?;
                catalog_dict.set("Outlines", Object::Reference(outline_id));
            }
        }

        doc.trailer.set("Root", Object::Reference(catalog_id));

        // Save strategy: lighter xref/stream settings when there is no OCR text; full modern
        // compression when a text layer is present (object streams help text-heavy PDFs).
        let options = if !has_ocr_content {
            SaveOptions::builder()
                .use_object_streams(false)
                .use_xref_streams(false)
                .compression_level(1)
                .build()
        } else {
            SaveOptions::builder()
                .use_object_streams(true)
                .use_xref_streams(true)
                .max_objects_per_stream(200)
                .compression_level(9)
                .build()
        };

        let mut file = std::fs::File::create(output_path)?;
        doc.save_with_options(&mut file, options)?;

        Ok(())
    }
}

/// Represents a word from HOCR output with bounding box coordinates
#[derive(Debug, Clone)]
pub struct HocrWord {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Represents a line of HOCR text composed of multiple words
#[derive(Debug, Clone)]
pub struct HocrLine {
    pub words: Vec<HocrWord>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub baseline: f32,
    // Average width of single-character tokens in this line, used by spacing heuristics
    pub avg_char_width: f32,
    // Optional raw line text (from OCR engine) for authoritative spacing
    pub raw_text: Option<String>,
}

/// The main assembly function that creates the final PDF from a list of pages.
///
/// It decides whether to build the PDF entirely in memory or to use the
/// `StreamingPdfBuilder` based on a page count threshold.
///
/// # Arguments
/// * `pages`: A vector of `Page` objects, sorted in the correct order.
/// * `output_path`: The path to save the final PDF file.
/// * `streaming_threshold`: The number of pages above which streaming mode will be used.
pub fn assemble_pdf(pages: &[Page], output_path: &str, streaming_threshold: usize) -> Result<()> {
    if pages.is_empty() {
        return Err(anyhow!(
            "assemble_pdf called with zero pages; aborting PDF creation (output_path={})",
            output_path
        ));
    }
    // Check if any pages contain OCR text content
    let has_ocr_content = pages.iter().any(|page| {
        page.hocr_text
            .as_ref()
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false)
    });

    let result = if pages.len() > streaming_threshold {
        // Use streaming for large documents to conserve memory.
        let mut builder = StreamingPdfBuilder::new();
        for page in pages {
            builder.add_page(page)?;
        }
        builder.finalize(output_path, has_ocr_content)
    } else {
        // Use simpler in-memory creation for smaller documents.
        create_pdf_in_memory(pages, output_path, has_ocr_content)
    };

    // Verify output file was created and has size
    result?;
    match fs::metadata(output_path) {
        Ok(meta) => {
            let size = meta.len();
            if size == 0 {
                return Err(anyhow!(
                    "Final PDF exists but is zero bytes: {}",
                    output_path
                ));
            }
        }
        Err(e) => {
            return Err(anyhow!(
                "Final PDF missing after finalize ({}): {}",
                e,
                output_path
            ));
        }
    }

    Ok(())
}

/// Creates a PDF document entirely in memory. Best for smaller documents.
/// This function mirrors the logic from `pageintake.txt` but is adapted
/// to use the helper methods from `StreamingPdfBuilder` for consistency.
fn create_pdf_in_memory(pages: &[Page], output_path: &str, has_ocr_content: bool) -> Result<()> {
    let mut builder = StreamingPdfBuilder::new();
    for page in pages {
        builder.add_page(page)?;
    }
    builder.finalize(output_path, has_ocr_content)
}

pub fn parse_hocr(hocr: &str) -> Result<Vec<HocrLine>> {
    let mut reader = Reader::from_str(hocr);

    let mut words = Vec::new();
    let mut raw_lines: Vec<(f32, f32, f32, f32, String)> = Vec::new();
    let mut buf = Vec::new();
    let mut current_word_text = String::new();
    let mut current_line_text = String::new();
    let mut in_ocr_word = false;
    let mut in_ocr_line = false;
    let mut current_index: Option<usize> = None;
    let mut current_line_bbox: Option<(f32, f32, f32, f32)> = None;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) if e.name().as_ref() == b"span" => {
                let mut is_word = false;
                let mut is_line = false;
                let mut bbox: Option<(f32, f32, f32, f32)> = None;

                for attr in e.attributes() {
                    let attr = attr?;
                    match attr.key.as_ref() {
                        b"class" => {
                            if attr
                                .value
                                .as_ref()
                                .split(|&b| b == b' ')
                                .any(|c| c == b"ocrx_word")
                            {
                                is_word = true;
                            }
                            if attr
                                .value
                                .as_ref()
                                .split(|&b| b == b' ')
                                .any(|c| c == b"ocr_line")
                            {
                                is_line = true;
                            }
                        }
                        b"title" => {
                            if let Ok(title_str) = attr.unescape_value() {
                                if let Some(after_bbox) = title_str.strip_prefix("bbox ") {
                                    let bbox_part =
                                        after_bbox.split(';').next().unwrap_or(after_bbox);
                                    let mut nums = bbox_part
                                        .split_whitespace()
                                        .filter_map(|s| s.parse::<f32>().ok());
                                    if let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                                        (nums.next(), nums.next(), nums.next(), nums.next())
                                    {
                                        bbox = Some((x1, y1, x2, y2));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }

                if is_word {
                    if let Some((x1, y1, x2, y2)) = bbox {
                        in_ocr_word = true;
                        current_word_text.clear();
                        words.push(HocrWord {
                            text: String::new(),
                            x: x1,
                            y: y1,
                            width: (x2 - x1).max(0.0),
                            height: (y2 - y1).max(0.0),
                        });
                        current_index = Some(words.len() - 1);
                    } else {
                        in_ocr_word = false;
                        current_index = None;
                    }
                } else if is_line {
                    if let Some((x1, y1, x2, y2)) = bbox {
                        in_ocr_line = true;
                        current_line_bbox = Some((x1, y1, x2, y2));
                        current_line_text.clear();
                    } else {
                        in_ocr_line = false;
                        current_line_bbox = None;
                    }
                }
            }
            Event::Text(e) => {
                if let Ok(s) = std::str::from_utf8(e.as_ref()) {
                    if in_ocr_word {
                        current_word_text.push_str(s);
                    }
                    if in_ocr_line {
                        current_line_text.push_str(s);
                    }
                }
            }
            Event::End(ref e) if e.name().as_ref() == b"span" => {
                if in_ocr_word {
                    if let Some(idx) = current_index {
                        if let Some(word) = words.get_mut(idx) {
                            word.text = current_word_text.trim().to_string();
                        }
                    }
                    in_ocr_word = false;
                    current_index = None;
                    current_word_text.clear();
                } else if in_ocr_line {
                    if let Some((x1, y1, x2, y2)) = current_line_bbox.take() {
                        let txt = current_line_text.trim().to_string();
                        if !txt.is_empty() {
                            raw_lines.push((x1, y1, x2, y2, txt));
                        }
                    }
                    in_ocr_line = false;
                    current_line_text.clear();
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    words.retain(|w| !w.text.trim().is_empty());

    let mut lines = group_words_into_lines(words);

    // Fix for open punctuation that might have been broken by previous processing
    let open_punct = "([{\"'`";

    // Attach raw line text (if present) by matching bounding boxes
    if !raw_lines.is_empty() {
        for line in lines.iter_mut() {
            let lx1 = line.x;
            let ly1 = line.y;
            let lx2 = line.x + line.width;
            let ly2 = line.y + line.height;
            let l_area = (lx2 - lx1).max(0.0) * (ly2 - ly1).max(0.0);
            let mut best_idx: Option<usize> = None;
            let mut best_score: f32 = 0.0;

            // First try to find a raw line that matches our line's bounding box
            for (i, (rx1, ry1, rx2, ry2, _)) in raw_lines.iter().enumerate() {
                let ix = (lx2.min(*rx2) - lx1.max(*rx1)).max(0.0);
                let iy = (ly2.min(*ry2) - ly1.max(*ry1)).max(0.0);
                let inter = ix * iy;
                let score = if l_area > 0.0 { inter / l_area } else { 0.0 };
                if score > best_score {
                    best_score = score;
                    best_idx = Some(i);
                }
            }

            // If we found a good match, use that raw text
            if let Some(i) = best_idx.take() {
                if best_score > 0.2 {
                    line.raw_text = Some(raw_lines[i].4.clone());
                }
            }
        }
    }

    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_orders_pages_by_index() {
        // Create accumulator for 6 pages
        let acc = PageAccumulator::new(6);

        // Helper to make a minimal Page
        fn make_page(idx: usize) -> Page {
            Page {
                width: 100.0,
                height: 100.0,
                elements: Vec::new(),
                hocr_text: None,
                index: idx,
                binarized: None,
            }
        }

        // Submit pages in out-of-order arrival: 3,0,2,1,4,5
        for &idx in &[3usize, 0, 2, 1, 4, 5] {
            acc.add_page(idx, make_page(idx));
        }

        // Snapshot should be in ascending index order
        let snapshot_indices: Vec<usize> = acc.snapshot_pages().iter().map(|p| p.index).collect();
        assert_eq!(snapshot_indices, vec![0, 1, 2, 3, 4, 5]);

        // Consuming into_pages returns ordered pages as well
        let consumed_indices: Vec<usize> =
            acc.clone().into_pages().iter().map(|p| p.index).collect();
        assert_eq!(consumed_indices, vec![0, 1, 2, 3, 4, 5]);
    }
}

fn group_words_into_lines(mut words: Vec<HocrWord>) -> Vec<HocrLine> {
    if words.is_empty() {
        return Vec::new();
    }

    words.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    struct LineGroup {
        words: Vec<HocrWord>,
        min_x: f32,
        max_x: f32,
        min_y: f32,
        max_y: f32,
    }

    let mut groups: Vec<LineGroup> = Vec::new();

    for word in words.into_iter() {
        let word_mid = word.y + word.height * 0.5;
        let mut target = None;

        for (idx, group) in groups.iter().enumerate() {
            let line_mid = (group.min_y + group.max_y) * 0.5;
            let line_height = (group.max_y - group.min_y).max(word.height).max(1.0);
            if (line_mid - word_mid).abs() <= line_height * 0.45 {
                target = Some(idx);
                break;
            }
        }

        let idx = if let Some(idx) = target {
            idx
        } else {
            groups.push(LineGroup {
                min_x: word.x,
                max_x: word.x + word.width,
                min_y: word.y,
                max_y: word.y + word.height,
                words: Vec::new(),
            });
            groups.len() - 1
        };

        let group = &mut groups[idx];
        group.min_x = group.min_x.min(word.x);
        group.max_x = group.max_x.max(word.x + word.width);
        group.min_y = group.min_y.min(word.y);
        group.max_y = group.max_y.max(word.y + word.height);
        group.words.push(word);
    }

    let mut lines: Vec<HocrLine> = groups
        .into_iter()
        .map(|mut group| {
            group.words.sort_by(|a, b| {
                a.x.partial_cmp(&b.x)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal))
            });

            let min_x = group.min_x;
            let max_x = group.max_x;
            let min_y = group.min_y;
            let max_y = group.max_y;

            // Estimate average single-character width for improved spacing decisions
            let mut char_width_sum = 0.0f32;
            let mut char_width_count = 0usize;
            for w in &group.words {
                if w.text.chars().count() == 1 {
                    char_width_sum += w.width;
                    char_width_count += 1;
                }
            }
            let line_height = (max_y - min_y).max(1.0);
            let avg_char_width = if char_width_count > 0 {
                char_width_sum / (char_width_count as f32)
            } else {
                // Fallback: assume avg char width ~ half of line height
                line_height * 0.5
            };

            HocrLine {
                x: min_x,
                y: min_y,
                width: (max_x - min_x).max(0.0),
                height: line_height,
                baseline: max_y,
                avg_char_width,
                words: group.words,
                raw_text: None,
            }
        })
        .collect();

    lines.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    lines
}

// Remove adjacent repeated words introduced by tiling overlaps (same text, high vertical
// overlap, and small horizontal gap). Operates per-line to avoid cross-line removals.
fn dedup_adjacent_repeats(lines: &mut Vec<HocrLine>) {
    for line in lines.iter_mut() {
        if line.words.is_empty() {
            continue;
        }
        let mut cleaned: Vec<HocrWord> = Vec::with_capacity(line.words.len());
        for w in line.words.iter() {
            if let Some(prev) = cleaned.last() {
                let same_text = prev.text.trim() == w.text.trim();
                let gap = w.x - (prev.x + prev.width);
                let v_overlap =
                    ((prev.y + prev.height).min(w.y + w.height) - prev.y.max(w.y)).max(0.0);
                let v_overlap_ratio = v_overlap / prev.height.max(w.height).max(1.0);
                if same_text && gap < line.height * 0.25 && v_overlap_ratio > 0.5 {
                    // Skip duplicate
                    continue;
                }
            }
            cleaned.push(w.clone());
        }
        line.words = cleaned;
    }
}
