// High-level IW44 encoder using C++ FFI bridge
use crate::ffi::iw44_ffi::{IW44EncoderWrapper, IW44EncodeParms};
use crate::Result;
use anyhow::Context;

pub struct IW44Encoder {
    encoder: IW44EncoderWrapper,
    width: i32,
    height: i32,
    is_color: bool,
}

#[derive(Debug, Clone)]
pub struct EncoderParams {
    pub slices: Option<i32>,
    pub bytes: Option<i32>, 
    pub decibels: Option<f32>,
}

impl Default for EncoderParams {
    fn default() -> Self {
        Self {
            slices: None,
            bytes: None,
            decibels: Some(25.0), // Default quality
        }
    }
}

impl IW44Encoder {
    /// Create encoder for grayscale image
    pub fn new_grayscale(image_data: &[u8], width: i32, height: i32, mask: Option<&[u8]>) -> Result<Self> {
        let encoder = IW44EncoderWrapper::new_grayscale(image_data, width, height, mask)
            .context("Failed to create IW44 grayscale encoder")?;
        
        Ok(Self {
            encoder,
            width,
            height,
            is_color: false,
        })
    }
    
    /// Create encoder for color image (RGB format)
    pub fn new_color(image_data: &[u8], width: i32, height: i32, mask: Option<&[u8]>) -> Result<Self> {
        let expected_size = (width * height * 3) as usize;
        if image_data.len() != expected_size {
            return Err(anyhow::anyhow!(
                "Image data size mismatch: expected {}, got {}",
                expected_size,
                image_data.len()
            ));
        }
        
        let encoder = IW44EncoderWrapper::new_color(image_data, width, height, mask)
            .context("Failed to create IW44 color encoder")?;
        
        Ok(Self {
            encoder,
            width,
            height,
            is_color: true,
        })
    }
    
    /// Encode a single chunk
    pub fn encode_chunk(&self, params: &EncoderParams) -> Result<Vec<u8>> {
        let parms = IW44EncodeParms {
            slices: params.slices.unwrap_or(0),
            bytes: params.bytes.unwrap_or(0),
            decibels: params.decibels.unwrap_or(25.0),
        };
        
        self.encoder.encode_chunk(&parms)
            .map_err(|e| anyhow::anyhow!("Failed to encode IW44 chunk: {}", e))
    }
    
    /// Encode full image with progressive quality
    pub fn encode_progressive(&self, quality_levels: &[EncoderParams]) -> Result<Vec<Vec<u8>>> {
        let mut chunks = Vec::new();
        
        for params in quality_levels {
            let chunk = self.encode_chunk(params)?;
            if !chunk.is_empty() {
                chunks.push(chunk);
            }
        }
        
        Ok(chunks)
    }
    
    /// Encode full image to target quality
    pub fn encode_to_quality(&self, target_decibels: f32) -> Result<Vec<u8>> {
        let params = EncoderParams {
            slices: None,
            bytes: None,
            decibels: Some(target_decibels),
        };
        
        self.encode_chunk(&params)
    }
    
    /// Encode full image to target size
    pub fn encode_to_size(&self, target_bytes: i32) -> Result<Vec<u8>> {
        let params = EncoderParams {
            slices: None,
            bytes: Some(target_bytes),
            decibels: None,
        };
        
        self.encode_chunk(&params)
    }
    
    /// Get current encoding statistics
    pub fn get_stats(&self) -> EncodingStats {
        EncodingStats {
            slices: self.encoder.get_slices(),
            bytes: self.encoder.get_bytes(),
            width: self.width,
            height: self.height,
            is_color: self.is_color,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncodingStats {
    pub slices: i32,
    pub bytes: i32,
    pub width: i32,
    pub height: i32,
    pub is_color: bool,
}

// Helper functions for common use cases

/// Encode color image with default quality
pub fn encode_color_image(image_data: &[u8], width: i32, height: i32) -> Result<Vec<u8>> {
    let encoder = IW44Encoder::new_color(image_data, width, height, None)?;
    encoder.encode_to_quality(25.0)
}

/// Encode grayscale image with default quality  
pub fn encode_grayscale_image(image_data: &[u8], width: i32, height: i32) -> Result<Vec<u8>> {
    let encoder = IW44Encoder::new_grayscale(image_data, width, height, None)?;
    encoder.encode_to_quality(25.0)
}

/// Convert RGB image to YCrCb channels
pub fn rgb_to_ycrCb_channels(rgb_data: &[u8], width: i32, height: i32) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let pixel_count = (width * height) as usize;
    let mut y_channel = Vec::with_capacity(pixel_count);
    let mut cr_channel = Vec::with_capacity(pixel_count);
    let mut cb_channel = Vec::with_capacity(pixel_count);
    
    for chunk in rgb_data.chunks_exact(3) {
        let r = chunk[0] as f32;
        let g = chunk[1] as f32;
        let b = chunk[2] as f32;
        
        // YCrCb conversion (ITU-R BT.601)
        let y = 0.299 * r + 0.587 * g + 0.114 * b;
        let cr = 0.713 * (r - y) + 128.0;
        let cb = 0.564 * (b - y) + 128.0;
        
        y_channel.push(y.clamp(0.0, 255.0) as u8);
        cr_channel.push(cr.clamp(0.0, 255.0) as u8);
        cb_channel.push(cb.clamp(0.0, 255.0) as u8);
    }
    
    Ok((y_channel, cr_channel, cb_channel))
}
