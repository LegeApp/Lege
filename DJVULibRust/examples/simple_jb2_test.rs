//! Test: Simple JB2 encoding with just a few shapes

use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use djvu_encoder::encode::jb2::symbol_dict::BitImage;
use djvu_encoder::image::image_formats::{Pixmap, Pixel};
use std::fs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Simple JB2 Test ===\n");
    
    let width = 200u32;
    let height = 100u32;
    
    // Create a simple background
    let background = Pixmap::from_pixel(width, height, Pixel::white());
    
    // Create a simple mask with just 3 shapes
    let mut mask = BitImage::new(width, height)?;
    
    // Shape 1: small rect at top-left
    for y in 10..30 {
        for x in 10..50 {
            mask.set_usize(x, y, true);
        }
    }
    
    // Shape 2: small rect in middle
    for y in 40..60 {
        for x in 80..120 {
            mask.set_usize(x, y, true);
        }
    }
    
    // Shape 3: small rect at bottom-right
    for y in 70..90 {
        for x in 150..190 {
            mask.set_usize(x, y, true);
        }
    }
    
    println!("Created mask with 3 shapes...");
    
    // Build the page
    let mut page = PageComponents::new_with_dimensions(width, height);
    page = page.with_background(background)?;
    page = page.with_jb2_auto_extract(mask)?;
    
    // Encode
    let params = PageEncodeParams::default();
    let encoded_page = page.encode(&params, 1, 300, 1, Some(2.2))?;
    
    // Write
    let output_path = "/tmp/simple_jb2_test.djvu";
    fs::write(output_path, &encoded_page)?;
    println!("Written to: {}", output_path);
    
    // Verify with djvudump
    let output = std::process::Command::new("djvudump")
        .arg(output_path)
        .output()?;
    
    if output.status.success() {
        println!("{}", String::from_utf8_lossy(&output.stdout));
    }
    
    // Decode mask
    let decoded_path = "/tmp/simple_jb2_test_mask.pbm";
    let output = std::process::Command::new("ddjvu")
        .args(["-mode=mask", "-format=pbm", output_path, decoded_path])
        .output()?;
    
    if output.status.success() {
        println!("✓ Mask decoded to: {}", decoded_path);
        
        // Count black pixels in decoded mask
        let pbm_data = fs::read(decoded_path)?;
        let black_pixels: usize = pbm_data.iter().map(|&b| b.count_ones() as usize).sum();
        println!("Decoded mask has approximately {} black pixels", black_pixels);
    } else {
        eprintln!("ddjvu error: {}", String::from_utf8_lossy(&output.stderr));
    }
    
    Ok(())
}
