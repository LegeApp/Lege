//! Page encoding functionality for DjVu documents

use crate::annotations::{Annotations, hidden_text::HiddenText};
use crate::encode::{
    iw44::encoder::{EncoderParams as IW44EncoderParams, IWEncoder},
    jb2::encoder::JB2Encoder,
    symbol_dict::BitImage,
};
use crate::iff::{bs_byte_stream::bzz_compress, iff::IffWriter};
use crate::image::image_formats::{Bitmap, GrayPixel, Pixel, Pixmap};
use crate::{DjvuError, Result};
use byteorder::{BigEndian, WriteBytesExt};
use log::debug;
use std::io::{self, Write};
use std::sync::Arc;

fn blit_bit_image(dst: &mut BitImage, src: &BitImage, x0: u32, y0: u32) {
    let x0 = x0 as usize;
    let y0 = y0 as usize;
    for y in 0..src.height {
        let dy = y0 + y;
        if dy >= dst.height {
            continue;
        }
        for x in 0..src.width {
            let dx = x0 + x;
            if dx >= dst.width {
                continue;
            }
            let v = src.get_pixel_unchecked(x, y);
            dst.set_usize(dx, dy, v);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn from_dimensions(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PageLayer {
    IW44Background { image: Pixmap, rect: Rect },
    JB2Foreground { image: BitImage, rect: Rect },
    JB2Mask { image: BitImage, rect: Rect },
}

#[derive(Clone)]
pub struct EncodedPage {
    pub page_num: usize,
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

impl EncodedPage {
    pub fn new(page_num: usize, data: Vec<u8>, width: u32, height: u32) -> Self {
        Self {
            page_num,
            data: Arc::new(data),
            width,
            height,
        }
    }

    pub fn from_components(
        page_num: usize,
        components: PageComponents,
        params: &PageEncodeParams,
        dpi: u32,
        gamma: Option<f32>,
    ) -> Result<Self> {
        let (width, height) = components.dimensions();
        let dpm = (dpi * 100 / 254) as u32;
        let rotation = if width >= height { 1 } else { 1 };
        let data = components.encode(params, (page_num + 1) as u32, dpm, rotation, gamma)?;
        Ok(Self {
            page_num,
            data: Arc::new(data),
            width,
            height,
        })
    }
}

/// Configuration for page encoding
#[derive(Debug, Clone)]
pub struct PageEncodeParams {
    /// Dots per inch (default: 300)
    pub dpi: u32,
    /// Background quality (0-100, higher is better quality)
    pub bg_quality: u8,
    /// Foreground quality (0-100, higher is better quality)
    pub fg_quality: u8,
    /// Whether to use IW44 for background (true) or JB2 (false)
    pub use_iw44: bool,
    /// Whether to encode in color (true) or grayscale (false)
    pub color: bool,
    /// Target SNR in dB for IW44 encoding (overrides bg_quality if set)
    pub decibels: Option<f32>,
    /// Maximum slices per chunk (default: 74, like C44)
    pub slices: Option<usize>,
    /// Maximum bytes per chunk (default: None)
    pub bytes: Option<usize>,
    /// Fraction of blocks used for quality estimation (default: 0.35)
    pub db_frac: f32,
    /// Lossless encoding mode (default: false)
    pub lossless: bool,
    /// Quantization multiplier for IW44 (default: 1.0, range: 0.5-2.0)
    /// Lower = more coefficients = better quality but larger files
    /// Higher = fewer coefficients = smaller files but lower quality
    pub quant_multiplier: Option<f32>,
}

impl Default for PageEncodeParams {
    fn default() -> Self {
        Self {
            dpi: 300,
            bg_quality: 90,
            fg_quality: 90,
            use_iw44: true, // Default to IW44 for background
            color: true,    // Default to color encoding
            decibels: None,
            slices: Some(74), // C44 default
            bytes: None,
            db_frac: 0.35,
            lossless: false,
            quant_multiplier: None, // Use C++ default
        }
    }
}

/// Represents a single page's components for encoding.
///
/// Use `PageComponents::new()` to create an empty page, then add components
/// like background, foreground, and mask using the `with_*` methods.
/// The dimensions of the first image added will set the dimensions for the page.
pub struct PageComponents {
    /// Page width in pixels
    width: u32,
    /// Page height in pixels
    height: u32,
    /// Optional background image data (for IW44)
    pub background: Option<Pixmap>,
    /// Optional foreground image data (for JB2)
    pub foreground: Option<BitImage>,
    /// Optional mask data (bitonal)
    pub mask: Option<BitImage>,
    /// JB2 shape dictionary (bitonal symbol images)
    /// Used for manual JB2 encoding without connected component analysis
    pub jb2_shapes: Option<Vec<BitImage>>,
    /// JB2 blit positions: (left, bottom, shape_index)
    /// Used for manual JB2 encoding without connected component analysis
    pub jb2_blits: Option<Vec<(i32, i32, usize)>>,
    /// Optional text/annotations
    pub text: Option<String>,
    pub layers: Vec<PageLayer>,
    /// Optional hidden text layer (TXTa/TXTz)
    pub text_layer: Option<HiddenText>,
    /// Optional hyperlink/annotation layer (ANTa/ANTz)
    pub annotations: Option<Annotations>,
    /// Optional shared JB2 dictionary for cross-page symbol sharing
    pub shared_dict: Option<std::sync::Arc<crate::encode::jb2::symbol_dict::SharedDict>>,
}

impl Default for PageComponents {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            background: None,
            foreground: None,
            mask: None,
            text: None,
            layers: Vec::new(),
            text_layer: None,
            annotations: None,
            shared_dict: None,
            jb2_shapes: None,
            jb2_blits: None,
        }
    }
}

impl PageComponents {
    /// Creates a new, empty page.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_with_dimensions(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            background: None,
            foreground: None,
            mask: None,
            text: None,
            layers: Vec::new(),
            text_layer: None,
            annotations: None,
            shared_dict: None,
            jb2_shapes: None,
            jb2_blits: None,
        }
    }

    /// Sets a shared JB2 dictionary for cross-page symbol sharing.
    ///
    /// When encoding multiple pages with shared symbols (e.g., common fonts),
    /// using a shared dictionary allows referencing previously encoded shapes
    /// instead of re-encoding them, improving compression.
    pub fn with_shared_dict(
        mut self,
        dict: std::sync::Arc<crate::encode::jb2::symbol_dict::SharedDict>,
    ) -> Self {
        self.shared_dict = Some(dict);
        self
    }

    /// Returns the dimensions of the page.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Checks and sets the page dimensions if they are not already set.
    /// Returns an error if the new dimensions conflict with existing ones.
    fn check_and_set_dimensions(&mut self, new_dims: (u32, u32)) -> Result<()> {
        if self.width == 0 && self.height == 0 {
            self.width = new_dims.0;
            self.height = new_dims.1;
        } else if self.width != new_dims.0 || self.height != new_dims.1 {
            return Err(DjvuError::InvalidOperation(format!(
                "Dimension mismatch: expected {}x{}, got {}x{}",
                self.width, self.height, new_dims.0, new_dims.1
            )));
        }
        Ok(())
    }

    pub fn add_iw44_background(mut self, image: Pixmap, rect: Rect) -> Result<Self> {
        let new_dims = (rect.x + rect.width, rect.y + rect.height);
        self.check_and_set_dimensions(new_dims)?;
        if image.width() != rect.width || image.height() != rect.height {
            return Err(DjvuError::InvalidOperation(
                "Background layer dimensions do not match rect".to_string(),
            ));
        }

        if rect.x == 0 && rect.y == 0 && rect.width == self.width && rect.height == self.height {
            self.background = Some(image.clone());
        } else {
            let mut canvas = self
                .background
                .take()
                .unwrap_or_else(|| Pixmap::from_pixel(self.width, self.height, Pixel::white()));
            for y in 0..rect.height {
                for x in 0..rect.width {
                    let px = image.get_pixel(x, y);
                    canvas.put_pixel(rect.x + x, rect.y + y, px);
                }
            }
            self.background = Some(canvas);
        }

        self.layers.push(PageLayer::IW44Background { image, rect });
        Ok(self)
    }

    pub fn add_jb2_foreground(mut self, image: BitImage, rect: Rect) -> Result<Self> {
        let new_dims = (rect.x + rect.width, rect.y + rect.height);
        self.check_and_set_dimensions(new_dims)?;
        if image.width as u32 != rect.width || image.height as u32 != rect.height {
            return Err(DjvuError::InvalidOperation(
                "Foreground layer dimensions do not match rect".to_string(),
            ));
        }

        let mut dest = match self.foreground.take() {
            Some(d) => d,
            None => BitImage::new(self.width, self.height).map_err(|e| {
                DjvuError::InvalidOperation(format!("Failed to allocate foreground bitmap: {e}"))
            })?,
        };
        blit_bit_image(&mut dest, &image, rect.x, rect.y);
        self.foreground = Some(dest);
        self.layers.push(PageLayer::JB2Foreground { image, rect });
        Ok(self)
    }

    pub fn add_jb2_mask(mut self, image: BitImage, rect: Rect) -> Result<Self> {
        let new_dims = (rect.x + rect.width, rect.y + rect.height);
        self.check_and_set_dimensions(new_dims)?;
        if image.width as u32 != rect.width || image.height as u32 != rect.height {
            return Err(DjvuError::InvalidOperation(
                "Mask layer dimensions do not match rect".to_string(),
            ));
        }

        let mut dest = match self.mask.take() {
            Some(d) => d,
            None => BitImage::new(self.width, self.height).map_err(|e| {
                DjvuError::InvalidOperation(format!("Failed to allocate mask bitmap: {e}"))
            })?,
        };
        blit_bit_image(&mut dest, &image, rect.x, rect.y);
        self.mask = Some(dest);
        self.layers.push(PageLayer::JB2Mask { image, rect });
        Ok(self)
    }

    /// Adds a background image to the page.
    pub fn with_background(self, image: Pixmap) -> Result<Self> {
        let rect = Rect::from_dimensions(image.width(), image.height());
        self.add_iw44_background(image, rect)
    }

    /// Adds a foreground image to the page.
    pub fn with_foreground(self, image: BitImage) -> Result<Self> {
        let rect = Rect::from_dimensions(image.width as u32, image.height as u32);
        self.add_jb2_foreground(image, rect)
    }

    /// Adds a mask to the page.
    pub fn with_mask(self, image: BitImage) -> Result<Self> {
        let rect = Rect::from_dimensions(image.width as u32, image.height as u32);
        self.add_jb2_mask(image, rect)
    }

    /// Adds text/annotations to the page.
    pub fn with_text(mut self, text: String) -> Self {
        self.text = Some(text);
        self
    }

    /// Adds a hidden text layer for OCR/searchability.
    pub fn with_text_layer(mut self, text_layer: HiddenText) -> Self {
        self.text_layer = Some(text_layer);
        self
    }

    /// Adds JB2 data manually (shapes and blit positions).
    ///
    /// This allows encoding JB2 without connected component analysis.
    /// Users provide pre-extracted shapes and their positions on the page.
    ///
    /// # Arguments
    /// * `shapes` - Vector of bitonal symbol images (the dictionary)
    /// * `blits` - Vector of (left, bottom, shape_index) tuples indicating where each symbol appears
    ///
    /// # Example
    /// ```ignore
    /// let shape1 = BitImage::new(10, 10).unwrap();
    /// let shape2 = BitImage::new(12, 12).unwrap();
    /// let shapes = vec![shape1, shape2];
    /// let blits = vec![
    ///     (0, 0, 0),    // First blit uses shape1
    ///     (20, 0, 1),   // Second blit uses shape2
    ///     (40, 0, 0),   // Third blit reuses shape1
    /// ];
    /// let page = PageComponents::new_with_dimensions(100, 100)
    ///     .with_jb2_manual(shapes, blits);
    /// ```
    pub fn with_jb2_manual(mut self, shapes: Vec<BitImage>, blits: Vec<(i32, i32, usize)>) -> Self {
        self.jb2_shapes = Some(shapes);
        self.jb2_blits = Some(blits);
        self
    }

    /// Adds JB2 data by automatically extracting connected components from a bitonal image.
    ///
    /// Requires the `symboldict` feature to be enabled.
    /// Uses the `lutz` crate for connected component analysis and symbol matching.
    ///
    /// # Example
    /// ```ignore
    /// {
    ///     let bitimage = create_bitonal_text_image();
    ///     let page = PageComponents::new_with_dimensions(800, 600)
    ///         .with_jb2_auto_extract(bitimage)?;
    /// }
    /// ```
    pub fn with_jb2_auto_extract(mut self, image: BitImage) -> Result<Self> {
        use crate::encode::jb2::{analyze_page, shapes_to_encoder_format};

        // Run connected component analysis
        let dpi = 300; // Default DPI
        let losslevel = 1; // Enable some cleaning
        let cc_image = analyze_page(&image, dpi, losslevel);

        // Extract shapes
        let shapes = cc_image.extract_shapes();

        // Convert to encoder format
        let (bitmaps, _parents, blits) = shapes_to_encoder_format(shapes, image.height as i32);

        self.jb2_shapes = Some(bitmaps);
        self.jb2_blits = Some(blits);
        Ok(self)
    }

    /// Adds hyperlink/annotation data.
    pub fn with_annotations(mut self, annotations: Annotations) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Encodes the page to a byte vector using the given parameters
    pub fn encode(
        &self,
        params: &PageEncodeParams,
        page_num: u32,
        dpm: u32,
        rotation: u8,       // 1=0°, 6=90°CCW, 2=180°, 5=90°CW
        gamma: Option<f32>, // If None, use 2.2
    ) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        {
            let mut cursor = io::Cursor::new(&mut output);
            let mut writer = IffWriter::new(&mut cursor);

            // Write AT&T magic bytes first
            writer.write_magic_bytes()?;

            // Start the FORM:DJVU chunk
            writer.put_chunk("FORM:DJVU")?;

            // Write INFO chunk (required for all pages)
            self.write_info_chunk(
                &mut writer,
                params.dpi as u16,
                page_num,
                dpm,
                rotation,
                gamma,
            )?;

            // --- BG44: Always emit a blank background for bitonal/JB2 pages ---
            let mut wrote_bg44 = false;
            if let Some(bg_img) = &self.background {
                if params.use_iw44 {
                    self.encode_iw44_background(bg_img, &mut writer, params)?;
                    wrote_bg44 = true;
                } else {
                    return Err(DjvuError::InvalidOperation(
                        "JB2 background encoding requires a bitonal image. Use foreground instead."
                            .to_string(),
                    ));
                }
            }
            // If no background but JB2 content exists, emit an all-white BG44
            if !wrote_bg44
                && (self.foreground.is_some() || self.mask.is_some() || self.jb2_shapes.is_some())
            {
                let (w, h) = (self.width, self.height);
                let white_bg = Pixmap::from_pixel(w, h, Pixel::white());
                self.encode_iw44_background(&white_bg, &mut writer, params)?;
            }

            // --- Djbz + Sjbz: JB2 encoding ---
            let mut num_blits = 0;
            let mut encoded_sjbz: Option<Vec<u8>> = None;

            // JB2 can come from three sources (in priority order):
            // 1. Manual jb2_shapes/jb2_blits (always available, no feature required)
            // 2. Auto-extracted from foreground (requires symboldict feature)
            // 3. Auto-extracted from mask (requires symboldict feature)

            let _jb2_encoded =
                if let (Some(shapes), Some(blits)) = (&self.jb2_shapes, &self.jb2_blits) {
                    num_blits = blits.len();
                    // Manual JB2 encoding (no feature required)
                    use crate::encode::jb2::encoder::JB2Encoder;
                    let parents: Vec<i32> = vec![-1; shapes.len()];

                    // --- Sjbz ---
                    let mut page_encoder = JB2Encoder::new(Vec::new());
                    let sjbz_raw = page_encoder
                        .encode_page_with_shapes(
                            self.width,
                            self.height,
                            shapes,
                            &parents,
                            blits,
                            0,
                            None,
                        )
                        .map_err(|e| DjvuError::EncodingError(e.to_string()))?;

                    encoded_sjbz = Some(sjbz_raw);
                    true
                } else {
                    false
                };

            // Auto-extraction fallback (only if manual JB2 wasn't used)
            if !_jb2_encoded {
                if let Some(fg_img) = &self.foreground {
                    // Auto-extract from foreground (requires symboldict feature)
                    use crate::encode::jb2::{
                        analyze_page, encoder::JB2Encoder, shapes_to_encoder_format,
                    };

                    let mut page_encoder = JB2Encoder::new(Vec::new());

                    // Run connected component analysis
                    let dpi = 300;
                    let losslevel = 1;
                    let cc_image = analyze_page(fg_img, dpi, losslevel);
                    let shapes = cc_image.extract_shapes();
                    let (dictionary, parents, blits) =
                        shapes_to_encoder_format(shapes, self.height as i32);
                    num_blits = blits.len();

                    // --- Sjbz ---
                    let sjbz_raw = page_encoder
                        .encode_page_with_shapes(
                            self.width,
                            self.height,
                            &dictionary,
                            &parents,
                            &blits,
                            0,
                            None,
                        )
                        .map_err(|e| DjvuError::EncodingError(e.to_string()))?;

                    encoded_sjbz = Some(sjbz_raw);
                } else if let Some(mask_img) = &self.mask {
                    // Auto-extract from mask (requires symboldict feature)
                    use crate::encode::jb2::{
                        analyze_page, encoder::JB2Encoder, shapes_to_encoder_format,
                    };

                    let mut page_encoder = JB2Encoder::new(Vec::new());

                    // Run connected component analysis
                    let dpi = 300;
                    let losslevel = 1;
                    let cc_image = analyze_page(mask_img, dpi, losslevel);
                    let shapes = cc_image.extract_shapes();
                    let (dictionary, parents, blits) =
                        shapes_to_encoder_format(shapes, self.height as i32);
                    num_blits = blits.len();

                    // --- Sjbz ---
                    let sjbz_raw = page_encoder
                        .encode_page_with_shapes(
                            self.width,
                            self.height,
                            &dictionary,
                            &parents,
                            &blits,
                            0,
                            None,
                        )
                        .map_err(|e| DjvuError::EncodingError(e.to_string()))?;

                    encoded_sjbz = Some(sjbz_raw);
                }
            }

            // --- FGbz: Foreground colors for compound images ---
            // Must be written BEFORE Sjbz to inform viewer of colors?
            // Spec says no strict order, but standard is BG44 -> FGbz -> Sjbz.

            let has_jb2 = encoded_sjbz.is_some();
            if wrote_bg44 && has_jb2 {
                // Determine if we have blits to color
                if num_blits > 0 {
                    // Write FGbz with correspondence (Version 0x80 | 0)
                    writer.put_chunk("FGbz")?;

                    // Version 0 with correspondence bit (0x80)
                    writer.write_u8(0x80)?;

                    // Palette size: 1 (black)
                    writer.write_u16::<BigEndian>(1)?;
                    writer.write_all(&[0x00, 0x00, 0x00])?; // Black BGR

                    // Correspondence Data (per DjVuPalette.cpp)
                    // nDataSize: INT24 = number of blits (NOT compressed size)
                    let n = num_blits as u32;
                    writer.write_u8(((n >> 16) & 0xFF) as u8)?;
                    writer.write_u8(((n >> 8) & 0xFF) as u8)?;
                    writer.write_u8((n & 0xFF) as u8)?;

                    // Indices: BZZ encoded stream of INT16 indices (big-endian)
                    // Since we have only 1 color (index 0), all blits get index 0.
                    // Each index is written as a 16-bit big-endian integer.
                    let mut index_bytes = Vec::with_capacity(num_blits * 2);
                    for _ in 0..num_blits {
                        index_bytes.push(0u8); // High byte of index 0
                        index_bytes.push(0u8); // Low byte of index 0
                    }
                    let compressed_indices = bzz_compress(&index_bytes, 50).map_err(|e| {
                        DjvuError::EncodingError(format!("FGbz compression failed: {e}"))
                    })?;
                    writer.write_all(&compressed_indices)?;

                    writer.close_chunk()?;
                } else {
                    // Fallback for 0 blits: Write simple black FGbz palette
                    // Format: BYTE version | INT16 nPaletteSize | BYTE3 bgrColor
                    let fgbz_data: [u8; 6] = [
                        0x00, // Version (no correspondence data)
                        0x00, 0x01, // nPaletteSize = 1 (big-endian)
                        0x00, 0x00, 0x00, // BGR color = black
                    ];
                    writer.put_chunk("FGbz")?;
                    writer.write_all(&fgbz_data)?;
                    writer.close_chunk()?;
                }
            }

            // --- Write Delayed Sjbz ---
            if let Some(sjbz_data) = encoded_sjbz {
                // Write raw JB2 stream (already ZP-compressed, no BZZ needed)
                writer.put_chunk("Sjbz")?;
                writer.write_all(&sjbz_data)?;
                writer.close_chunk()?;
            }

            // --- TXTa/TXTz: Hidden text layer ---
            // NOTE: Text layer encoding is NON-FATAL. If it fails, we skip the TXTz chunk
            // rather than failing the entire page. This prevents OCR coordinate issues
            // from breaking the visual output.
            if let Some(text_layer) = &self.text_layer {
                let mut txt_buf = Vec::new();
                let tl = text_layer;
                match tl.encode(&mut txt_buf) {
                    Ok(()) => {
                        // Use BZZ compression for DJVU spec compliance (100KB blocks)
                        match bzz_compress(&txt_buf, 100) {
                            Ok(data) => {
                                writer.put_chunk("TXTz")?;
                                writer.write_all(&data)?;
                                writer.close_chunk()?;
                            }
                            Err(_e) => {
                                #[cfg(feature = "debug-logging")]
                                eprintln!(
                                    "[page_encoder] Warning: BZZ compression for TXTz failed: {e}. Skipping text layer."
                                );
                            }
                        }
                    }
                    Err(_e) => {
                        // Log but don't fail - page will still be viewable without searchable text
                        #[cfg(feature = "debug-logging")]
                        eprintln!(
                            "[page_encoder] Warning: Failed to encode hidden text: {e}. Skipping text layer."
                        );
                    }
                }
            }

            // --- ANTa/ANTz: Hyperlink/annotation layer ---
            if let Some(annotations) = &self.annotations {
                let mut ann_buf = Vec::new();
                annotations.encode(&mut ann_buf).map_err(|e| {
                    DjvuError::InvalidOperation(format!("Failed to encode annotations: {e}"))
                })?;
                // Use BZZ compression for DJVU spec compliance (100KB blocks)
                let data = bzz_compress(&ann_buf, 100).map_err(|e| {
                    DjvuError::EncodingError(format!("BZZ compression failed: {e}"))
                })?;
                writer.put_chunk("ANTz")?;
                writer.write_all(&data)?;
                writer.close_chunk()?;
            }

            // Write text/annotations if present (legacy plain text)
            if let Some(text) = &self.text {
                self.write_text_chunk(text, &mut writer)?;
            }

            // Close the FORM:DJVU chunk
            writer.close_chunk()?;
        }
        Ok(output)
    }

    /// Writes the INFO chunk as per DjVu spec (10 bytes)
    /// Format: width(2,BE) height(2,BE) minor_ver(1) major_ver(1) dpi(2,LE) gamma(1) flags(1)
    fn write_info_chunk(
        &self,
        writer: &mut IffWriter,
        dpi: u16,
        _page_num: u32,
        _dpm: u32,
        rotation: u8,       // 1=0°, 6=90°CCW, 2=180°, 5=90°CW
        gamma: Option<f32>, // If None, use 2.2
    ) -> Result<()> {
        use byteorder::LittleEndian;

        writer.put_chunk("INFO")?;

        // Width and height (2 bytes each, big-endian)
        writer.write_u16::<BigEndian>(self.width as u16)?;
        writer.write_u16::<BigEndian>(self.height as u16)?;

        // Minor version (1 byte, currently 24 per C44 reference)
        writer.write_u8(24)?;

        // Major version (1 byte, currently 0 per spec)
        writer.write_u8(0)?;

        // DPI (2 bytes, little-endian per spec)
        writer.write_u16::<LittleEndian>(dpi)?;

        // Gamma (1 byte, gamma * 10)
        let gamma_val = gamma.map_or(22, |g| (g * 10.0 + 0.5) as u8); // Default gamma = 2.2
        writer.write_u8(gamma_val)?;

        // Flags (1 byte: bits 0-2 = rotation, bits 3-7 = reserved)
        let flags = rotation & 0x07; // Ensure only bottom 3 bits are used
        writer.write_u8(flags)?;

        writer.close_chunk()?;
        Ok(())
    }

    /// Encodes the background using IW44 (wavelet)
    fn encode_iw44_background(
        &self,
        img: &Pixmap,
        writer: &mut IffWriter,
        params: &PageEncodeParams,
    ) -> Result<()> {
        let crcb_mode = if params.color {
            // C++ c44.exe uses CRCBnormal by default, not CRCBfull
            crate::encode::iw44::encoder::CrcbMode::Normal
        } else {
            crate::encode::iw44::encoder::CrcbMode::None
        };

        // Debug: Check input image properties
        let (w, h) = img.dimensions();
        let raw_data = img.as_raw();
        debug!("Input image {}x{}, {} bytes", w, h, raw_data.len());

        // Check some sample pixels
        if raw_data.len() >= 9 {
            debug!(
                "First 3 pixels: RGB({},{},{}) RGB({},{},{}) RGB({},{},{})",
                raw_data[0],
                raw_data[1],
                raw_data[2],
                raw_data[3],
                raw_data[4],
                raw_data[5],
                raw_data[6],
                raw_data[7],
                raw_data[8]
            );
        }

        let iw44_params = IW44EncoderParams {
            decibels: params.decibels,
            crcb_mode,
            slices: params.slices,
            bytes: params.bytes,
            db_frac: params.db_frac,
            lossless: params.lossless,
            quant_multiplier: params.quant_multiplier.unwrap_or(1.0),
        };

        // If a mask is present, convert it to Bitmap and pass to IWEncoder for mask-aware encoding
        let mask_gray = if let Some(mask_bitimg) = &self.mask {
            // Convert BitImage to Bitmap (1=masked, 0=unmasked)
            let (mw, mh) = (mask_bitimg.width as u32, mask_bitimg.height as u32);
            let mut mask_pixels = Vec::with_capacity((mw * mh) as usize);
            for y in 0..mh {
                for x in 0..mw {
                    let pixel_value = if mask_bitimg.get_pixel_unchecked(x as usize, y as usize) {
                        1
                    } else {
                        0
                    };
                    mask_pixels.push(GrayPixel::new(pixel_value));
                }
            }
            Some(Bitmap::from_vec(mw, mh, mask_pixels))
        } else {
            None
        };

        if mask_gray.is_some() {
            debug!("Using mask-aware IW44 encoding for background");
        }

        let mut encoder = if params.color {
            IWEncoder::from_rgb(img, mask_gray.as_ref(), iw44_params)
        } else {
            let gray = img.to_bitmap();
            IWEncoder::from_gray(&gray, mask_gray.as_ref(), iw44_params)
        }
        .map_err(|e| DjvuError::EncodingError(e.to_string()))?;

        // Choose the correct chunk type for IW44 background images:
        // - BG44 for background layer (the main use case for IW44 in DjVu pages)
        // - FG44 for foreground layer (has mask)
        // Note: PM44/BM44 are for standalone IW44 files, not DjVu page backgrounds
        let iw_chunk_id = if self.mask.is_some() {
            "FG44"
        } else {
            "BG44" // Use BG44 for background images in DjVu pages
        };

        // Encode and write IW44 data - use consistent slice limit for all chunks
        let mut chunk_count = 0;
        let slices_per_chunk = params.slices.unwrap_or(74);
        let mut total_slices_encoded = 0;
        let total_slices_target = slices_per_chunk; // For now, match first chunk limit

        loop {
            // Check if we've reached total slice target
            if total_slices_encoded >= total_slices_target {
                debug!(
                    "Reached total slice target {}, stopping",
                    total_slices_target
                );
                break;
            }

            // Use consistent slice limit for all chunks
            let (iw44_stream, more) = encoder
                .encode_chunk(slices_per_chunk)
                .map_err(|e| DjvuError::EncodingError(e.to_string()))?;

            if iw44_stream.is_empty() {
                break;
            }

            chunk_count += 1;
            writer.put_chunk(iw_chunk_id)?;
            writer.write_all(&iw44_stream)?;
            writer.close_chunk()?;

            // Count slices in this chunk (from header)
            if iw44_stream.len() >= 2 {
                total_slices_encoded += iw44_stream[1] as usize;
            }

            if !more {
                break;
            }
        }
        debug!("Completed IW44 encoding with {} chunks", chunk_count);

        Ok(())
    }

    /// Encodes the foreground using JB2
    fn _encode_jb2_foreground(
        &self,
        img: &BitImage,
        writer: &mut IffWriter,
        _quality: u8,
    ) -> Result<()> {
        // Create JB2 encoder and encode as single page (non-symbol data)
        let mut jb2_encoder = JB2Encoder::new(Vec::new());
        let jb2_raw = jb2_encoder.encode_single_page(img)?;

        // BZZ-compress the JB2 data as required by DjVu spec (§3.2.5)
        let sjbz_payload =
            bzz_compress(&jb2_raw, 256).map_err(|e| DjvuError::EncodingError(e.to_string()))?;

        // Write Sjbz chunk for JB2 bitmap data (shapes and positions)
        // Note: FGbz is for JB2 colors, Sjbz is for the actual bitmap content
        writer.put_chunk("Sjbz")?;
        writer.write_all(&sjbz_payload)?;
        writer.close_chunk()?;

        Ok(())
    }

    /// Encodes the mask using JB2
    fn _encode_jb2_mask(&self, img: &BitImage, writer: &mut IffWriter) -> Result<()> {
        // Create JB2 encoder and encode as single page (non-symbol data)
        let mut jb2_encoder = JB2Encoder::new(Vec::new());
        let jb2_raw = jb2_encoder.encode_single_page(img)?;

        // BZZ-compress the JB2 data as required by DjVu spec
        let sjbz_payload =
            bzz_compress(&jb2_raw, 256).map_err(|e| DjvuError::EncodingError(e.to_string()))?;

        // Write Sjbz chunk
        writer.put_chunk("Sjbz")?;
        writer.write_all(&sjbz_payload)?;
        writer.close_chunk()?;

        Ok(())
    }

    /// Writes the text/annotations chunk
    fn write_text_chunk(&self, text: &str, writer: &mut IffWriter) -> Result<()> {
        writer.put_chunk("TXTa")?;
        writer.write_all(text.as_bytes())?;
        writer.close_chunk()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::symbol_dict::BitImage;
    use crate::image::image_formats::{Pixel, Pixmap};

    #[test]
    fn test_page_encoding_with_builder() {
        // Create a simple white background image
        let bg_image = Pixmap::from_pixel(100, 200, Pixel::white());

        // Use the builder pattern to create the page
        let page = PageComponents::new()
            .with_background(bg_image)
            .unwrap()
            .with_text("Hello, DjVu!".to_string());

        assert_eq!(page.dimensions(), (100, 200));

        // Encode with default parameters
        let params = PageEncodeParams::default();
        let result = page.encode(&params, 1, 300, 1, Some(2.2));

        assert!(result.is_ok());
        let encoded = result.unwrap();

        // Basic validation of the encoded data
        assert!(!encoded.is_empty());
        // Check for FORM:DJVU header
        assert_eq!(&encoded[0..8], b"AT&TFORM");
        // Check for INFO chunk
        assert!(encoded.windows(4).any(|w| w == b"INFO"));
        // Check for BG44 chunk (since this is a page background, not PM44)
        assert!(encoded.windows(4).any(|w| w == b"BG44"));
        // Check for text chunk
        assert!(encoded.windows(4).any(|w| w == b"TXTa"));
    }

    #[test]
    fn test_dimension_mismatch() {
        let bg_image = Pixmap::new(100, 200);
        let fg_image = BitImage::new(101, 201); // Different dimensions

        let result = PageComponents::new()
            .with_background(bg_image)
            .unwrap()
            .with_foreground(fg_image.unwrap());

        assert!(result.is_err());
        if let Err(DjvuError::InvalidOperation(msg)) = result {
            assert!(msg.contains("Dimension mismatch"));
        } else {
            panic!("Expected a DimensionMismatch error");
        }
    }
}
