//! DjVu-compatible JB2 encoder following the official DjVu specification
//!
//! This implements the JB2 encoding as specified in Appendix 2 of the DjVu specification,
//! producing a single Sjbz chunk with arithmetically encoded records.

use crate::encode::zc::ZEncoder;
use crate::encode::jb2::error::Jb2Error;
use crate::encode::jb2::symbol_dict::BitImage;
use crate::encode::jb2::num_coder::NumCoder;
use std::io::Write;

// Context allocation as per DjVu specification Table 7
const RECORD_TYPE_CONTEXT: usize = 0;
const IMAGE_SIZE_CONTEXT: usize = 1;
const MATCHING_SYMBOL_CONTEXT: usize = 2;
const SYMBOL_WIDTH_CONTEXT: usize = 3;
const SYMBOL_HEIGHT_CONTEXT: usize = 4;
const SYMBOL_WIDTH_DIFF_CONTEXT: usize = 5;
const SYMBOL_HEIGHT_DIFF_CONTEXT: usize = 6;
const SYMBOL_COLUMN_CONTEXT: usize = 7;
const SYMBOL_ROW_CONTEXT: usize = 8;
const SAME_LINE_COLUMN_OFFSET_CONTEXT: usize = 9;
const SAME_LINE_ROW_OFFSET_CONTEXT: usize = 10;
const NEW_LINE_COLUMN_OFFSET_CONTEXT: usize = 11;
const NEW_LINE_ROW_OFFSET_CONTEXT: usize = 12;
const COMMENT_LENGTH_CONTEXT: usize = 13;
const COMMENT_OCTET_CONTEXT: usize = 14;

// Direct bitmap coding contexts (1024 contexts for 2^10 template)
const DIRECT_BITMAP_BASE_CONTEXT: usize = 15;
const DIRECT_BITMAP_CONTEXTS: usize = 1024;

// Refinement bitmap coding contexts (2048 contexts for 2^11 template) 
const REFINEMENT_BITMAP_BASE_CONTEXT: usize = DIRECT_BITMAP_BASE_CONTEXT + DIRECT_BITMAP_CONTEXTS;
const REFINEMENT_BITMAP_CONTEXTS: usize = 2048;

// Total contexts needed
const TOTAL_CONTEXTS: usize = REFINEMENT_BITMAP_BASE_CONTEXT + REFINEMENT_BITMAP_CONTEXTS;

// Record types as per DjVu specification Table 6
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum RecordType {
    StartOfImage = 0,
    NewSymbolAddToImageAndLibrary = 1,
    NewSymbolAddToLibraryOnly = 2,
    NewSymbolAddToImageOnly = 3,
    MatchedSymbolWithRefinementAddToImageAndLibrary = 4,
    MatchedSymbolWithRefinementAddToLibraryOnly = 5,
    MatchedSymbolWithRefinementAddToImageOnly = 6,
    MatchedSymbolCopyToImage = 7,
    NonSymbolData = 8,
    SharedDictionaryOrNumcoderReset = 9,
    Comment = 10,
    EndOfData = 11,
}

/// DjVu-compatible JB2 encoder
pub struct DjvuJb2Encoder<W: Write> {
    _writer: W,
    image_width: u32,
    image_height: u32,
    _symbol_library: Vec<BitImage>,
}

impl<W: Write> DjvuJb2Encoder<W> {
    /// Create a new DjVu JB2 encoder
    pub fn new(writer: W) -> Self {
        Self {
            _writer: writer,
            image_width: 0,
            image_height: 0,
            _symbol_library: Vec::new(),
        }
    }

    /// Encode a bitmap as a single-page DjVu JB2 stream
    pub fn encode_single_page(&mut self, image: &BitImage) -> Result<Vec<u8>, Jb2Error> {
        self.image_width = image.width as u32;
        self.image_height = image.height as u32;

        let mut buffer = Vec::new();
        
        // Create ZP encoder
        let mut zc = ZEncoder::new(&mut buffer, true)?;

        // Initialize contexts for JB2
        let mut contexts = vec![0u8; TOTAL_CONTEXTS];

        // Encode start of image record
        self.encode_start_of_image(&mut zc, &mut contexts)?;

        // For simplicity, encode the entire image as a single "non-symbol data" record
        self.encode_non_symbol_data(&mut zc, &mut contexts, image, 0, 0)?;

        // Encode end of data record
        self.encode_end_of_data(&mut zc, &mut contexts)?;

        // Flush the encoder
        zc.finish()?;

        Ok(buffer)
    }

    /// Encode start of image record (record type 0)
    fn encode_start_of_image(&mut self, zc: &mut ZEncoder<&mut Vec<u8>>, contexts: &mut [u8]) -> Result<(), Jb2Error> {
        // Encode record type
        self.encode_integer(zc, contexts, RECORD_TYPE_CONTEXT, RecordType::StartOfImage as i32, 0, 11)?;
        
        // Encode image size: WIDTH FIRST, HEIGHT SECOND (per DjVu spec Table 8)
        // Both values are coded as (value - 1) with valid range 0-65534
        self.encode_integer(zc, contexts, IMAGE_SIZE_CONTEXT, (self.image_width - 1) as i32, 0, 65534)?;
        self.encode_integer(zc, contexts, IMAGE_SIZE_CONTEXT, (self.image_height - 1) as i32, 0, 65534)?;
        
        // Encode eventual image refinement flag (0 = no refinement)
        zc.encode(false, &mut contexts[0])?; // Use context 0 for this flag
        
        Ok(())
    }

    /// Encode non-symbol data record (record type 8)
    fn encode_non_symbol_data(
        &mut self, 
        zc: &mut ZEncoder<&mut Vec<u8>>, 
        contexts: &mut [u8],
        bitmap: &BitImage,
        abs_x: i32,
        abs_y: i32
    ) -> Result<(), Jb2Error> {
        // Encode record type
        self.encode_integer(zc, contexts, RECORD_TYPE_CONTEXT, RecordType::NonSymbolData as i32, 0, 11)?;
        
        // Encode absolute symbol size
        self.encode_integer(zc, contexts, SYMBOL_WIDTH_CONTEXT, bitmap.width as i32, 1, 65535)?;
        self.encode_integer(zc, contexts, SYMBOL_HEIGHT_CONTEXT, bitmap.height as i32, 1, 65535)?;
        
        // Encode bitmap by direct coding
        self.encode_bitmap_direct(zc, contexts, bitmap)?;
        
        // Encode absolute location
        self.encode_integer(zc, contexts, SYMBOL_COLUMN_CONTEXT, abs_x, 0, self.image_width as i32)?;
        self.encode_integer(zc, contexts, SYMBOL_ROW_CONTEXT, abs_y, 0, self.image_height as i32)?;
        
        Ok(())
    }

    /// Encode end of data record (record type 11)
    fn encode_end_of_data(&mut self, zc: &mut ZEncoder<&mut Vec<u8>>, contexts: &mut [u8]) -> Result<(), Jb2Error> {
        // Encode record type only
        self.encode_integer(zc, contexts, RECORD_TYPE_CONTEXT, RecordType::EndOfData as i32, 0, 11)?;
        Ok(())
    }

    /// Encode bitmap using direct coding with 10-bit context template
    fn encode_bitmap_direct(
        &mut self, 
        zc: &mut ZEncoder<&mut Vec<u8>>, 
        contexts: &mut [u8],
        bitmap: &BitImage
    ) -> Result<(), Jb2Error> {
        // Direct coding uses a 10-bit template context
        // For simplicity, we'll use a basic raster-scan encoding
        // In a full implementation, this would use the DjVu template matching
        
        for y in 0..bitmap.height {
            for x in 0..bitmap.width {
                let pixel = bitmap.get_pixel_unchecked(x, y);
                
                // Calculate 10-bit context from surrounding pixels
                let context = self.calculate_direct_context(bitmap, x, y);
                let context_index = DIRECT_BITMAP_BASE_CONTEXT + (context as usize);
                
                // Encode the pixel
                zc.encode(pixel, &mut contexts[context_index])?;
            }
        }
        
        Ok(())
    }

    /// Calculate 10-bit context for direct bitmap coding
    fn calculate_direct_context(&self, bitmap: &BitImage, x: usize, y: usize) -> u32 {
        // Simplified 10-bit context calculation
        // In a full implementation, this would use the exact DjVu template
        let mut context = 0u32;
        
        // Sample some neighboring pixels to create context
        let neighbors = [
            (-1, -1), (0, -1), (1, -1),
            (-1,  0),
            (-2, -1), (-1, -2), (0, -2), (1, -2), (2, -1), (2, 0)
        ];
        
        for (i, &(dx, dy)) in neighbors.iter().enumerate() {
            let nx = x as i32 + dx;
            let ny = y as i32 + dy;
            
            if nx >= 0 && ny >= 0 && (nx as usize) < bitmap.width && (ny as usize) < bitmap.height {
                if bitmap.get_pixel_unchecked(nx as usize, ny as usize) {
                    context |= 1 << i;
                }
            }
            // If out of bounds, assume white (0)
        }
        
        context & 0x3FF // Mask to 10 bits
    }

    /// Encode integer using DjVu's 4-phase multivalue extension
    fn encode_integer(
        &mut self,
        zc: &mut ZEncoder<&mut Vec<u8>>,
        contexts: &mut [u8],
        base_context: usize,
        value: i32,
        low: i32,
        high: i32,
    ) -> Result<(), Jb2Error> {
        // Use the spec-compatible integer encoder from NumCoder
        let num_coder = NumCoder::new(0, 255); // Dummy values since we're using the static method
        num_coder.encode_integer(zc, contexts, base_context, value, low, high)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_pattern_encoding() {
        // Create a simple 10x10 pattern
        let mut image = BitImage::new(10, 10).unwrap();
        
        // Set center pixel
        image.set_usize(5, 5, true);
        
        // Create encoder
        let mut encoder = DjvuJb2Encoder::new(Vec::new());
        
        // Encode
        let result = encoder.encode_single_page(&image);
        assert!(result.is_ok());
        
        let data = result.unwrap();
        assert!(!data.is_empty());
        println!("Encoded {} bytes for 10x10 single pixel", data.len());
    }
}
