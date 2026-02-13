// examples/single_page_djvu_test.rs
//
// Comprehensive test for single-page DjVu generation
// Tests various image types and verifies successful encoding

use djvu_encoder::doc::{PageComponents, PageEncodeParams};
use image::{RgbImage, Rgb};
use std::fs;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Single Page DjVu Generation Test ===\n");

    // Create output directory
    let output_dir = "test_output_single_page";
    if Path::new(output_dir).exists() {
        fs::remove_dir_all(output_dir)?;
    }
    fs::create_dir_all(output_dir)?;
    println!("Created output directory: {}", output_dir);

    // Test 1: Solid color image (RGB)
    println!("\n--- Test 1: Solid Blue RGB Image ---");
    test_solid_color_rgb(output_dir)?;

    // Test 2: Gradient image (RGB)
    println!("\n--- Test 2: RGB Gradient Image ---");
    test_gradient_rgb(output_dir)?;

    // Test 3: Checkerboard pattern (RGB)
    println!("\n--- Test 3: RGB Checkerboard Pattern ---");
    test_checkerboard_rgb(output_dir)?;

    // Test 4: Different quality settings
    println!("\n--- Test 4: Different Quality Settings ---");
    test_quality_settings(output_dir)?;

    println!("\n=== All tests completed successfully! ===");
    println!("Check the '{}' directory for generated DjVu files.", output_dir);
    
    Ok(())
}

fn test_solid_color_rgb(output_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    let width = 100;
    let height = 100;
    
    // Create a solid blue image
    let mut img = RgbImage::new(width, height);
    for pixel in img.pixels_mut() {
        *pixel = Rgb([0, 0, 255]); // Pure blue
    }
    
    println!("Created {}x{} solid blue image", width, height);
    
    // Create page components with background
    let page_components = PageComponents::new()
        .with_background(img)?;
    
    // Encode with default parameters
    let params = PageEncodeParams::default();
    let djvu_data = page_components.encode(&params, 1, 11811, 1, Some(2.2))?;
    
    let output_path = format!("{}/solid_blue.djvu", output_dir);
    fs::write(&output_path, djvu_data)?;
    
    println!("✓ Generated DjVu file: {} ({} bytes)", output_path, fs::metadata(&output_path)?.len());
    
    Ok(())
}

fn test_gradient_rgb(output_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    let width = 200;
    let height = 150;
    
    // Create a horizontal gradient from red to green
    let mut img = RgbImage::new(width, height);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let red = (255.0 * (1.0 - x as f32 / width as f32)) as u8;
        let green = (255.0 * (x as f32 / width as f32)) as u8;
        let blue = (255.0 * (y as f32 / height as f32)) as u8;
        *pixel = Rgb([red, green, blue]);
    }
    
    println!("Created {}x{} RGB gradient image", width, height);
    
    let page_components = PageComponents::new()
        .with_background(img)?;
    
    let params = PageEncodeParams::default();
    let djvu_data = page_components.encode(&params, 1, 11811, 1, Some(2.2))?;
    
    let output_path = format!("{}/gradient_rgb.djvu", output_dir);
    fs::write(&output_path, djvu_data)?;
    
    println!("✓ Generated DjVu file: {} ({} bytes)", output_path, fs::metadata(&output_path)?.len());
    
    Ok(())
}

fn test_checkerboard_rgb(output_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    let width = 160;
    let height = 160;
    let square_size = 20;
    
    // Create a checkerboard pattern
    let mut img = RgbImage::new(width, height);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let is_black = (x / square_size + y / square_size) % 2 == 0;
        *pixel = if is_black {
            Rgb([0, 0, 0])      // Black
        } else {
            Rgb([255, 255, 255]) // White
        };
    }
    
    println!("Created {}x{} checkerboard pattern", width, height);
    
    let page_components = PageComponents::new()
        .with_background(img)?;
    
    let params = PageEncodeParams::default();
    let djvu_data = page_components.encode(&params, 1, 11811, 1, Some(2.2))?;
    
    let output_path = format!("{}/checkerboard.djvu", output_dir);
    fs::write(&output_path, djvu_data)?;
    
    println!("✓ Generated DjVu file: {} ({} bytes)", output_path, fs::metadata(&output_path)?.len());
    
    Ok(())
}

fn test_quality_settings(output_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    let width = 100;
    let height = 100;
    
    // Create a test image with some detail
    let mut img = RgbImage::new(width, height);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let r = ((x * 255) / width) as u8;
        let g = ((y * 255) / height) as u8;
        let b = (((x + y) * 255) / (width + height)) as u8;
        *pixel = Rgb([r, g, b]);
    }
    
    let qualities = [60, 80, 95];
    
    for &quality in &qualities {
        println!("Testing background quality: {}", quality);
        
        let params = PageEncodeParams {
            dpi: 300,
            bg_quality: quality,
            fg_quality: 90,
            use_iw44: true,
            color: true,
            decibels: None,
        };
        
        let page_components = PageComponents::new()
            .with_background(img.clone())?;
        let djvu_data = page_components.encode(&params, 1, 11811, 1, Some(2.2))?;
        
        let output_path = format!("{}/quality_{}.djvu", output_dir, quality);
        fs::write(&output_path, djvu_data)?;
        
        println!("✓ Generated DjVu file: {} ({} bytes)", output_path, fs::metadata(&output_path)?.len());
    }
    
    Ok(())
}
