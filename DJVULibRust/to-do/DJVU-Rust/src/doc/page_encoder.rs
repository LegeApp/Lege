//! Page encoding functionality for DjVu documents

use crate::encode::{
    iw44::encoder::{EncoderParams as IW44EncoderParams, IWEncoder},
    jb2::encoder::JB2Encoder,
    symbol_dict::BitImage,
};
use crate::iff::{iff::IffWriter, bs_byte_stream::bzz_compress};
use crate::{DjvuError, Result};
use byteorder::{BigEndian, WriteBytesExt};
use log::debug;
use image::RgbImage;
use lutz::Image;
use std::io::{self, Write};

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
    /// Target SNR in dB for IW44 encoding
    pub decibels: Option<f32>,
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
        }
    }
}

/// Represents a single page's components for encoding.
///
/// Use `PageComponents::new()` to create an empty page, then add components
/// like background, foreground, and mask using the `with_*` methods.
/// The dimensions of the first image added will set the dimensions for the page.
#[derive(Default)]
pub struct PageComponents {
    /// Page width in pixels
    width: u32,
    /// Page height in pixels
    height: u32,
    /// Optional background image data (for IW44)
    pub background: Option<RgbImage>,
    /// Optional foreground image data (for JB2)
    pub foreground: Option<BitImage>,
    /// Optional mask data (bitonal)
    pub mask: Option<BitImage>,
    /// Optional text/annotations
    pub text: Option<String>,
}

impl PageComponents {
    /// Creates a new, empty page.
    pub fn new() -> Self {
        Self::default()
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

    /// Adds a background image to the page.
    pub fn with_background(mut self, image: RgbImage) -> Result<Self> {
        self.check_and_set_dimensions(image.dimensions())?;
        self.background = Some(image);
        Ok(self)
    }

    /// Adds a foreground image to the page.
    pub fn with_foreground(mut self, image: BitImage) -> Result<Self> {
        self.check_and_set_dimensions((image.width(), image.height()))?;
        self.foreground = Some(image);
        Ok(self)
    }

    /// Adds a mask to the page.
    pub fn with_mask(mut self, image: BitImage) -> Result<Self> {
        self.check_and_set_dimensions((image.width(), image.height()))?;
        self.mask = Some(image);
        Ok(self)
    }

    /// Adds text/annotations to the page.
    pub fn with_text(mut self, text: String) -> Self {
        self.text = Some(text);
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
            if !wrote_bg44 && (self.foreground.is_some() || self.mask.is_some()) {
                let (w, h) = (self.width, self.height);
                let white_bg = RgbImage::from_pixel(w, h, image::Rgb([255, 255, 255]));
                self.encode_iw44_background(&white_bg, &mut writer, params)?;
            }

            // --- Djbz + Sjbz: JB2 dictionary and mask/foreground ---
            // If JB2 content is present (foreground or mask), emit Djbz and then Sjbz
            if let Some(fg_img) = &self.foreground {
                use crate::encode::jb2::encoder::JB2Encoder;
                let mut jb2_encoder = JB2Encoder::new(Vec::new());
                // Build dictionary and connected components
                let mut dict_builder = crate::encode::jb2::symbol_dict::SymDictBuilder::new(0);
                let (dictionary, components) = dict_builder.build(fg_img);
                // --- Djbz ---
                let dict_raw = jb2_encoder.encode_dictionary_chunk(&dictionary)
                    .map_err(|e| DjvuError::EncodingError(e.to_string()))?;
                let dict_bzz = bzz_compress(&dict_raw, 256)
                    .map_err(|e| DjvuError::EncodingError(e.to_string()))?;
                writer.put_chunk("Djbz")?;
                writer.write_all(&dict_bzz)?;
                writer.close_chunk()?;
                // --- Sjbz ---
                let sjbz_raw = jb2_encoder.encode_page_chunk(&components)
                    .map_err(|e| DjvuError::EncodingError(e.to_string()))?;
                let sjbz_bzz = bzz_compress(&sjbz_raw, 256)
                    .map_err(|e| DjvuError::EncodingError(e.to_string()))?;
                writer.put_chunk("Sjbz")?;
                writer.write_all(&sjbz_bzz)?;
                writer.close_chunk()?;
            } else if let Some(mask_img) = &self.mask {
                use crate::encode::jb2::encoder::JB2Encoder;
                let mut jb2_encoder = JB2Encoder::new(Vec::new());
                let mut dict_builder = crate::encode::jb2::symbol_dict::SymDictBuilder::new(0);
                let (dictionary, components) = dict_builder.build(mask_img);
                // --- Djbz ---
                let dict_raw = jb2_encoder.encode_dictionary_chunk(&dictionary)
                    .map_err(|e| DjvuError::EncodingError(e.to_string()))?;
                let dict_bzz = bzz_compress(&dict_raw, 256)
                    .map_err(|e| DjvuError::EncodingError(e.to_string()))?;
                writer.put_chunk("Djbz")?;
                writer.write_all(&dict_bzz)?;
                writer.close_chunk()?;
                // --- Sjbz ---
                let sjbz_raw = jb2_encoder.encode_page_chunk(&components)
                    .map_err(|e| DjvuError::EncodingError(e.to_string()))?;
                let sjbz_bzz = bzz_compress(&sjbz_raw, 256)
                    .map_err(|e| DjvuError::EncodingError(e.to_string()))?;
                writer.put_chunk("Sjbz")?;
                writer.write_all(&sjbz_bzz)?;
                writer.close_chunk()?;
            }

            // Write text/annotations if present
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

        // Minor version (1 byte, currently 26 per spec)
        writer.write_u8(26)?;

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
        img: &RgbImage,
        writer: &mut IffWriter,
        params: &PageEncodeParams,
    ) -> Result<()> {
        let crcb_mode = if params.color {
            crate::encode::iw44::encoder::CrcbMode::Full
        } else {
            crate::encode::iw44::encoder::CrcbMode::None
        };

        // Debug: Check input image properties
        let (w, h) = img.dimensions();
        let raw_data = img.as_raw();
        debug!("Input image {}x{}, {} bytes", w, h, raw_data.len());

        // Check some sample pixels
        if raw_data.len() >= 9 {
            debug!("First 3 pixels: RGB({},{},{}) RGB({},{},{}) RGB({},{},{})",
                   raw_data[0], raw_data[1], raw_data[2],
                   raw_data[3], raw_data[4], raw_data[5], 
                   raw_data[6], raw_data[7], raw_data[8]);
        }

        // Configure IW44 encoder with proper quality-based parameters
        // Map quality to decibels if not explicitly set
        let target_decibels = params.decibels.unwrap_or_else(|| {
            // Map quality 0-100 to reasonable dB range (30-100 dB)
            let quality_ratio = params.bg_quality as f32 / 100.0;
            30.0 + quality_ratio * 70.0 // 30-100 dB range
        });
        
        debug!("Configuring IW44 encoder with quality {} -> {:.1} dB",
               params.bg_quality, target_decibels);

        let iw44_params = IW44EncoderParams {
            decibels: Some(target_decibels),
            crcb_mode,
            ..Default::default()
        };

        // If a mask is present, convert it to GrayImage and pass to IWEncoder for mask-aware encoding
        let mask_gray = if let Some(mask_bitimg) = &self.mask {
            // Convert BitImage to GrayImage (1=masked, 0=unmasked)
            let (mw, mh) = (mask_bitimg.width as u32, mask_bitimg.height as u32);
            let mut mask_buf = vec![0u8; (mw * mh) as usize];
            for y in 0..mh {
                for x in 0..mw {
                    mask_buf[(y * mw + x) as usize] = if mask_bitimg.get_pixel_unchecked(x as usize, y as usize) { 1 } else { 0 };
                }
            }
            Some(image::GrayImage::from_raw(mw, mh, mask_buf).expect("Failed to create GrayImage from mask"))
        } else {
            None
        };

        if mask_gray.is_some() {
            debug!("Using mask-aware IW44 encoding for background");
        }

        let mut encoder = IWEncoder::from_rgb(img, mask_gray.as_ref(), iw44_params)
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

        // Encode and write IW44 data in proper chunks according to DjVu spec
        // Loop until encoder says it's done, like c44.exe does
        
        let mut chunk_count = 0;
        let mut more = true;
        
        while more {
            let (iw44_stream, encoder_more) = encoder
                .encode_chunk(74) // Use 74 slices per chunk like c44.exe
                .map_err(|e| DjvuError::EncodingError(e.to_string()))?;

            // An empty stream from the encoder signifies the end of data.
            if iw44_stream.is_empty() {
                debug!("Encoder returned empty chunk, stopping.");
                break;
            }

            chunk_count += 1;
            println!(
                "DEBUG: Writing BG44 chunk {}, 74 slices, {} bytes",
                chunk_count,
                iw44_stream.len()
            );

            writer.put_chunk(iw_chunk_id)?;
            writer.write_all(&iw44_stream)?;
            writer.close_chunk()?;
            
            more = encoder_more;
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
        // Create JB2 encoder and encode
        let mut jb2_encoder = JB2Encoder::new(Vec::new());
        let jb2_raw = jb2_encoder.encode_page(img, 0)?;

        // BZZ-compress the JB2 data as required by DjVu spec (§3.2.5)
        let sjbz_payload = bzz_compress(&jb2_raw, 256)
            .map_err(|e| DjvuError::EncodingError(e.to_string()))?;

        // Write Sjbz chunk for JB2 bitmap data (shapes and positions)
        // Note: FGbz is for JB2 colors, Sjbz is for the actual bitmap content
        writer.put_chunk("Sjbz")?;
        writer.write_all(&sjbz_payload)?;
        writer.close_chunk()?;

        Ok(())
    }

    /// Encodes the mask using JB2
    fn _encode_jb2_mask(&self, img: &BitImage, writer: &mut IffWriter) -> Result<()> {
        // Create JB2 encoder and encode
        let mut jb2_encoder = JB2Encoder::new(Vec::new());
        let jb2_raw = jb2_encoder.encode_page(img, 0)?;

        // BZZ-compress the JB2 data as required by DjVu spec
        let sjbz_payload = bzz_compress(&jb2_raw, 256)
            .map_err(|e| DjvuError::EncodingError(e.to_string()))?;

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
    use image::{Rgb, RgbImage};

    #[test]
    fn test_page_encoding_with_builder() {
        // Create a simple white background image
        let bg_image = RgbImage::from_pixel(100, 200, Rgb([255, 255, 255]));

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
        let bg_image = RgbImage::new(100, 200);
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
