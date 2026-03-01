use djvu_encoder::doc::{PageEncodeParams, PageComponents};
use image::RgbImage;
use std::fs;
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Testing Simple vs Complex IW44 Termination ===");
    
    // Test 1: Simple image that works (small, single chunk)
    println!("\n1. Testing simple 32x32 image (single chunk)...");
    test_simple_image()?;
    
    // Test 2: Complex image that fails (multiple chunks)  
    println!("\n2. Testing complex 64x64 gradient (multiple chunks)...");
    test_complex_image()?;
    
    Ok(())
}

fn test_simple_image() -> Result<(), Box<dyn std::error::Error>> {
    let width = 32;
    let height = 32;
    let mut rgb_image = RgbImage::new(width, height);
    
    // Simple solid color
    for pixel in rgb_image.pixels_mut() {
        *pixel = image::Rgb([128, 128, 128]);
    }
    
    let page_components = PageComponents::new()
        .with_background(rgb_image)?;
    
    let params = PageEncodeParams {
        dpi: 300,
        bg_quality: 50, // Lower quality = simpler encoding
        fg_quality: 50,
        use_iw44: true,
        color: true,
        decibels: Some(50.0),
    };
    
    let encoded_data = page_components.encode(&params, 1, 1200, 1, Some(2.2))?;
    
    let filename = "test_simple_32x32.djvu";
    fs::write(filename, &encoded_data)?;
    
    println!("  ✓ Simple image: {} bytes", encoded_data.len());
    
    // Test decode
    decode_and_analyze(filename, "simple.ppm")?;
    
    Ok(())
}

fn test_complex_image() -> Result<(), Box<dyn std::error::Error>> {
    let width = 64;
    let height = 64;
    let mut rgb_image = RgbImage::new(width, height);
    
    // Create a gradient that will need multiple chunks
    for (x, y, pixel) in rgb_image.enumerate_pixels_mut() {
        let r = (x * 255 / (width - 1)) as u8;
        let g = (y * 255 / (height - 1)) as u8;
        let b = ((x + y) * 255 / ((width + height) - 2)) as u8;
        *pixel = image::Rgb([r, g, b]);
    }
    
    let page_components = PageComponents::new()
        .with_background(rgb_image)?;
    
    let params = PageEncodeParams {
        dpi: 300,
        bg_quality: 95, // High quality = complex encoding
        fg_quality: 95,
        use_iw44: true,
        color: true,
        decibels: Some(95.0),
    };
    
    let encoded_data = page_components.encode(&params, 1, 1200, 1, Some(2.2))?;
    
    let filename = "test_complex_64x64.djvu";
    fs::write(filename, &encoded_data)?;
    
    println!("  ✓ Complex image: {} bytes", encoded_data.len());
    
    // Test decode
    decode_and_analyze(filename, "complex.ppm")?;
    
    Ok(())
}

fn decode_and_analyze(djvu_file: &str, output_file: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("  🔍 Analyzing {} structure...", djvu_file);
    
    // First, analyze structure
    let _ = Command::new("./djvudump.exe")
        .arg("-j") 
        .arg("-o")
        .arg(format!("{}.json", djvu_file))
        .arg(djvu_file)
        .output();
    
    // Then decode
    let output = Command::new("./ddjvu.exe")
        .arg("--format=ppm")
        .arg("--verbose")
        .arg(djvu_file)
        .arg(output_file)
        .output()?;
    
    if output.status.success() {
        println!("  ✅ Decoding successful!");
        let file_size = fs::metadata(output_file)?.len();
        println!("     PPM output: {} bytes", file_size);
    } else {
        println!("  ❌ Decoding failed!");
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("     Error: {}", stderr.trim());
    }
    
    Ok(())
}
