// examples/single_image_page_encoder_test.rs
//
// Single image DjVu encoder test using the page encoder - equivalent to c44.exe functionality
// Loads a JPEG file (test.jpeg) and encodes it to a complete DjVu document using the proper
// page encoder which should produce the correct BG44 chunk structure.

use djvu_encoder::encode::iw44::{EncoderParams, CrcbMode};
use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use image::RgbImage;
use std::fs;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Single Image DjVu Test (c44.exe equivalent using page encoder) ===\n");

    // Input and output paths
    let input_path = "test.jpeg";
    let output_path = "test_output_page_encoder.djvu";
    
    // Check if input file exists
    if !Path::new(input_path).exists() {
        eprintln!("Error: Input file '{}' not found", input_path);
        eprintln!("Please place a JPEG image named 'test.jpeg' in the current directory");
        return Ok(());
    }

    println!("Loading image: {}", input_path);
    
    // Load the image - always convert to RGB for simplicity
    let img = image::open(input_path)?;
    let rgb_img = img.to_rgb8();
    println!("Loaded RGB image: {}x{}", rgb_img.width(), rgb_img.height());

    // Use low quality settings for testing (same as c44.exe low quality)
    let iw44_params = EncoderParams { 
        decibels: Some(25.0),  // Low quality for fast encoding and smaller files
        crcb_mode: CrcbMode::Half, 
        db_frac: 0.35 
    };

    println!("Encoding with low quality settings...");
    encode_djvu_document(&rgb_img, &iw44_params, output_path)?;

    println!("\n=== DjVu encoding completed successfully! ===");
    println!("Generated DjVu file: {}", output_path);
    
    Ok(())
}

fn encode_djvu_document(
    rgb_img: &RgbImage, 
    params: &EncoderParams, 
    output_path: &str
) -> Result<(), Box<dyn std::error::Error>> {
    
    println!("Creating DjVu document with page encoder...");

    // Get image dimensions
    let (width, height) = rgb_img.dimensions();
    println!("Encoding {}x{} image with page encoder", width, height);

    // Create page components
    let page_components = PageComponents::new()
        .with_background(rgb_img.clone())?;

    // Configure page encoding parameters to match c44.exe
    let page_params = PageEncodeParams {
        dpi: 100,           // c44.exe uses 100 DPI, not 300
        bg_quality: 90,     // High quality for test
        fg_quality: 90,
        use_iw44: true,
        color: true,        // Enable color encoding
        decibels: params.decibels, // Use the same decibel target
    };

    // Encode the page (this will use our fixed chunking: 74, 15, 10 slices)
    let djvu_data = page_components.encode(
        &page_params,
        0,           // page_num
        10000,       // dpm (dots per meter, derived from DPI)
        1,           // rotation (1 = 0°)
        Some(2.2),   // gamma (c44.exe uses 2.2, not 5.0)
    )?;

    // Write the encoded data to file
    fs::write(output_path, &djvu_data)?;
    
    println!("✓ Generated DjVu file: {} ({} bytes, using page encoder)", 
             output_path, djvu_data.len());

    Ok(())
}
