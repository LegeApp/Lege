// examples/single_image_iw44_test.rs
//
// Single image DjVu encoder test - equivalent to c44.exe functionality
// Loads a JPEG file (test.jpeg) and encodes it to a complete DjVu document

use djvu_encoder::encode::iw44::{EncoderParams, CrcbMode};
use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use image::{RgbImage, GrayImage};
use std::fs;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Single Image DjVu Test (c44.exe equivalent) ===\n");

    // Input and output paths
    let input_path = "test.jpeg";
    let output_path = "test_output.djvu";
    
    // Check if input file exists
    if !Path::new(input_path).exists() {
        eprintln!("Error: Input file '{}' not found!", input_path);
        eprintln!("Please place a JPEG file named 'test.jpeg' in the current directory.");
        std::process::exit(1);
    }

    println!("Loading image: {}", input_path);
    
    // Load the image
    let img = match image::open(input_path)? {
        image::DynamicImage::ImageRgb8(rgb_img) => {
            println!("Loaded RGB image: {}x{}", rgb_img.width(), rgb_img.height());
            ImageData::Rgb(rgb_img)
        },
        image::DynamicImage::ImageLuma8(gray_img) => {
            println!("Loaded grayscale image: {}x{}", gray_img.width(), gray_img.height());
            ImageData::Gray(gray_img)
        },
        other => {
            println!("Converting image format to RGB...");
            let rgb_img = other.to_rgb8();
            println!("Converted to RGB: {}x{}", rgb_img.width(), rgb_img.height());
            ImageData::Rgb(rgb_img)
        }
    };

    // Basic encoder parameters for testing - force more slices
    let params = EncoderParams { 
        decibels: None, // Remove quality target to force all bitplanes
        crcb_mode: CrcbMode::Full, // Changed from Half to Full to get crcb_delay=0
        db_frac: 0.35 
    };

    println!("Encoding image to DjVu...");
    encode_djvu_document(&img, &params, output_path)?;

    println!("\n=== DjVu encoding completed successfully! ===");
    println!("Generated DjVu file: {}", output_path);
    
    Ok(())
}

enum ImageData {
    Rgb(RgbImage),
    Gray(GrayImage),
}

fn encode_djvu_document(
    img_data: &ImageData, 
    params: &EncoderParams, 
    output_path: &str
) -> Result<(), Box<dyn std::error::Error>> {
    
    // Convert EncoderParams to PageEncodeParams
    let page_params = PageEncodeParams {
        dpi: 100,
        bg_quality: 60,
        fg_quality: 70,     
        use_iw44: true,     
        color: true,        
        decibels: params.decibels, // Use the quality target from input params
    };

    // Create page components and add the background image
    let page_components = match img_data {
        ImageData::Rgb(rgb_img) => {
            PageComponents::new()
                .with_background(rgb_img.clone())?
        },
        ImageData::Gray(gray_img) => {
            // Convert grayscale to RGB for background encoding
            let rgb_img = RgbImage::from_fn(gray_img.width(), gray_img.height(), |x, y| {
                let gray_val = gray_img.get_pixel(x, y)[0];
                image::Rgb([gray_val, gray_val, gray_val])
            });
            PageComponents::new()
                .with_background(rgb_img)?
        }
    };

    // Encode the page
    let djvu_data = page_components.encode(
        &page_params,
        1,              // page_num
        100,            // dpm (dots per meter, matches 100 DPI)
        1,              // rotation (1 = 0 degrees)
        Some(2.2),      // gamma (match c44.exe)
    )?;

    // Write to file
    fs::write(output_path, &djvu_data)?;
    
    println!("✓ Generated DjVu file: {} ({} bytes)", 
             output_path, djvu_data.len());
    
    // Show file size comparison
    if let Ok(input_metadata) = fs::metadata("test.jpeg") {
        let input_size = input_metadata.len();
        let compression_ratio = input_size as f64 / djvu_data.len() as f64;
        println!("  Compression: {} bytes → {} bytes (ratio: {:.2}:1)", 
                 input_size, djvu_data.len(), compression_ratio);
    }

    Ok(())
}


