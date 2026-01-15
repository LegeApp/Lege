//! JBIG2 encoder module

use anyhow::{anyhow, Result};

/// Encode binary image data using JBIG2 compression
///
/// # Arguments
/// * `data` - Binary image data (0 for white, 1 for black)
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
///
/// # Returns
/// Encoded JBIG2 data as a vector of bytes
pub fn encode_jbig2(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    use jbig2enc_rust::encode_single_image;

    // Encode the image with PDF mode enabled for proper embedding
    let result = encode_single_image(data, width, height, true)
        .map_err(|e| anyhow!("JBIG2 encoding failed: {}", e))?;

    // For PDF embedding, we need to combine the global and page data
    let mut output = Vec::new();
    if let Some(global) = result.global_data {
        output.extend_from_slice(&global);
    }
    output.extend_from_slice(&result.page_data);

    Ok(output)
}

// Re-export the main jbig2enc-rust crate
pub use jbig2enc_rust::*;
