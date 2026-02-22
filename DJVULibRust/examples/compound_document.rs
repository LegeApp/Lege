//! Example: Create a compound DjVu document with IW44 background + JB2 text mask
//! 
//! This demonstrates the successful encoding of:
//! - BG44 chunk: Wavelet-compressed background image
//! - FGbz chunk: Foreground color palette (black)
//! - Sjbz chunk: JB2-encoded text mask
//!
//! Output: /tmp/compound_document_example.djvu

use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use djvu_encoder::encode::jb2::symbol_dict::BitImage;
use djvu_encoder::image::image_formats::{Pixmap, Pixel};
use std::fs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Creating Compound DjVu Document ===\n");
    
    let width = 800u32;
    let height = 600u32;
    
    // --- Create a gradient background image ---
    println!("Creating background (gradient)...");
    let background = Pixmap::from_fn(width, height, |x, y| {
        let r = ((x * 255 / width) % 256) as u8;
        let g = ((y * 255 / height) % 256) as u8;
        let b = (((x + y) * 128 / (width + height)) % 256) as u8;
        Pixel::new(r, g, b)
    });
    
    // --- Create a text-like foreground mask ---
    println!("Creating foreground mask (synthetic text)...");
    let mut mask = BitImage::new(width, height)?;
    
    // Draw title-like pattern at top
    for row in 30..80 {
        for col in 100..700 {
            if row > 35 && row < 75 && (col % 25 < 18) {
                mask.set_usize(col, row, true);
            }
        }
    }
    
    // Draw multiple "text lines" - using unambiguous boxes
    for line_num in 0..8 {
        let y_offset = 150 + (line_num * 50);
        let x_offset = 80;
        
        // Create "words" (simple rectangular blocks)
        for word in 0..6 {
            let word_x = x_offset + (word * 110);
            let word_width = 80;
            
            // Draw a solid rectangle for each word
            for row in 0..30 {
                for col in 0..word_width {
                    let x = word_x + col;
                    let y = y_offset + row;
                    
                    if x < width as usize && y < height as usize {
                        // Solid rectangle (except for a small hole in the middle to prove it's a mask)
                        if !(row > 10 && row < 20 && col > 10 && col < 70) {
                            mask.set_usize(x, y, true);
                        }
                    }
                }
            }
        }
    }
    
    // --- DEBUG: Export mask to PBM to verify content ---
    {
        println!("DEBUG: Exporting mask to /tmp/compound_debug_mask.pbm...");
        let header = format!("P4\n{} {}\n", width, height);
        let mut pbm_data = Vec::new();
        pbm_data.extend_from_slice(header.as_bytes());
        
        // BitImage bits are packed MSB0. P4 expects packed bits, row-aligned.
        // Since 800 is divisible by 8, no padding needed per row.
        // We can access the underlying storage if we can get it, or iterate manually.
        // BitVec storage access is tricky if private.
        // We'll reconstruct bytes manually to be safe and independent of internal storage.
        
        for y in 0..height {
            let mut byte = 0u8;
            for x in 0..width {
                if mask.get_pixel_unchecked(x as usize, y as usize) {
                    byte |= 1 << (7 - (x % 8));
                }
                if x % 8 == 7 {
                    pbm_data.push(byte);
                    byte = 0;
                }
            }
            // If width not multiple of 8, pad last byte (not needed for 800, but good practice)
            if width % 8 != 0 {
                pbm_data.push(byte);
            }
        }
        fs::write("/tmp/compound_debug_mask.pbm", &pbm_data)?;
    }
    
    // --- Build the page ---
    println!("Building page with background + text mask...");
    let mut page = PageComponents::new_with_dimensions(width, height);
    page = page.with_background(background)?;
    page = page.with_jb2_auto_extract(mask)?;
    
    // --- Encode the page ---
    println!("Encoding page (IW44 + JB2)...");
    let params = PageEncodeParams::default();
    let encoded_page = page.encode(&params, 1, 300, 1, Some(2.2))?;
    
    println!("✓ Encoded page: {} bytes\n", encoded_page.len());
    
    // --- Write to file ---
    let output_path = "/tmp/compound_document_example.djvu";
    fs::write(output_path, &encoded_page)?;
    println!("✓ Written to: {}\n", output_path);
    
    // --- Verify with djvudump ---
    println!("=== Verifying structure with djvudump ===\n");
    let output = std::process::Command::new("djvudump")
        .arg(output_path)
        .output()?;
    
    if output.status.success() {
        println!("{}", String::from_utf8_lossy(&output.stdout));
    } else {
        eprintln!("djvudump error: {}", String::from_utf8_lossy(&output.stderr));
    }
    
    // --- Decode to verify correctness ---
    println!("=== Decoding with ddjvu ===\n");
    let decoded_path = "/tmp/compound_document_example_decoded.ppm";
    let output = std::process::Command::new("ddjvu")
        .args(["-format=ppm", output_path, decoded_path])
        .output()?;
    
    if output.status.success() {
        println!("✓ Successfully decoded to: {}\n", decoded_path);
        
        if let Ok(metadata) = fs::metadata(decoded_path) {
            println!("Decoded image size: {} bytes", metadata.len());
        }
    } else {
        eprintln!("ddjvu error: {}", String::from_utf8_lossy(&output.stderr));
        return Err("Failed to decode document".into());
    }
    
    println!("=== SUCCESS ===");
    println!("Compound document created successfully!");
    println!("Original: {}", output_path);
    println!("Decoded:  {}", decoded_path);
    println!("\nYou can view the document with:");
    println!("  djview {}", output_path);
    
    Ok(())
}
