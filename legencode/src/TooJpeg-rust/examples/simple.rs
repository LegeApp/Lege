//! A simple example of using the toojpeg crate to encode an image to JPEG

use std::fs::File;
use std::io::Write;
use toojpeg::{encode_jpeg, EncodeOptions, ImageFormat};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a simple 2x2 RGB image (red, green, blue, white)
    // Pixel order: (0,0), (1,0), (0,1), (1,1)
    let pixels = vec![
        // First row
        255, 0, 0, // Red at (0,0)
        0, 255, 0, // Green at (1,0)
        // Second row
        0, 0, 255, // Blue at (0,1)
        255, 255, 255, // White at (1,1)
    ];

    // Set up encoding options
    let options = EncodeOptions {
        width: 2,
        height: 2,
        format: ImageFormat::RGB,
        quality: 90,
        baseline: true,   // Use baseline encoding for maximum compatibility
        optimized: true,  // Use optimized Huffman tables
        downsample: true, // Use chroma subsampling for better compression
    };

    // Encode to JPEG in memory
    let mut output = Vec::new();
    encode_jpeg(&pixels, options, &mut output).map_err(|e| {
        eprintln!("JPEG encoding failed: {}", e);
        std::io::Error::new(std::io::ErrorKind::Other, e)
    })?;

    // Save to file
    let output_path = "output.jpg";
    File::create(output_path)?.write_all(&output)?;

    println!(
        "Successfully wrote {} ({} bytes)",
        output_path,
        output.len()
    );
    println!("You can open the file to view the 2x2 test pattern.");

    Ok(())
}
