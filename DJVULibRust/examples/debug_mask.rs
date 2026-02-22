//! Debug: Create raw mask and save before/after JB2

use djvu_encoder::encode::jb2::symbol_dict::BitImage;
use djvu_encoder::encode::jb2::{analyze_page, shapes_to_encoder_format};
use std::fs::File;
use std::io::Write;

fn save_bitimage_as_pbm(img: &BitImage, path: &str) -> std::io::Result<()> {
    let width = img.width as u32;
    let height = img.height as u32;
    
    let mut file = File::create(path)?;
    writeln!(file, "P4")?;
    writeln!(file, "{} {}", width, height)?;
    
    for y in 0..height {
        let mut byte = 0u8;
        let mut bit_pos = 7i32;
        for x in 0..width {
            if img.get_pixel_unchecked(x as usize, y as usize) {
                byte |= 1 << bit_pos;
            }
            if bit_pos == 0 {
                file.write_all(&[byte])?;
                byte = 0;
                bit_pos = 7;
            } else {
                bit_pos -= 1;
            }
        }
        if width % 8 != 0 {
            file.write_all(&[byte])?;
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let width = 800u32;
    let height = 600u32;
    
    println!("=== Creating raw mask {}x{} ===", width, height);
    let mut mask = BitImage::new(width, height)?;
    
    // Draw a simple large rectangle to make it obvious
    for row in 100..500 {
        for col in 100..700 {
            // Thick border
            if row < 120 || row > 480 || col < 120 || col > 680 {
                mask.set_usize(col, row, true);
            }
            // Some text-like blocks inside
            if row > 150 && row < 200 && col > 150 && col < 650 {
                if (col / 20) % 2 == 0 {
                    mask.set_usize(col, row, true);
                }
            }
            if row > 250 && row < 300 && col > 150 && col < 650 {
                if (col / 20) % 2 == 0 {
                    mask.set_usize(col, row, true);
                }
            }
            if row > 350 && row < 400 && col > 150 && col < 650 {
                if (col / 20) % 2 == 0 {
                    mask.set_usize(col, row, true);
                }
            }
        }
    }
    
    // Count black pixels
    let mut black_count = 0;
    for y in 0..height {
        for x in 0..width {
            if mask.get_pixel_unchecked(x as usize, y as usize) {
                black_count += 1;
            }
        }
    }
    println!("Black pixels in raw mask: {} ({:.2}%)", black_count, 
             100.0 * black_count as f64 / (width * height) as f64);
    
    // Save raw mask
    save_bitimage_as_pbm(&mask, "/tmp/1_raw_mask.pbm")?;
    println!("Saved: /tmp/1_raw_mask.pbm");
    
    // Now run through JB2 analysis
    println!("\n=== Running connected component analysis ===");
    let dpi = 300;
    let losslevel = 1;
    let cc_image = analyze_page(&mask, dpi, losslevel);
    let shapes = cc_image.extract_shapes();
    println!("Extracted {} shapes", shapes.len());
    
    let (bitmaps, _parents, blits) = shapes_to_encoder_format(shapes, height as i32);
    println!("Converted to {} bitmaps, {} blits", bitmaps.len(), blits.len());
    
    // Print some blit positions
    for (i, blit) in blits.iter().take(10).enumerate() {
        println!("  Blit {}: pos=({}, {}), shape={}", i, blit.0, blit.1, blit.2);
    }
    
    // Reconstruct the mask from blits to see what JB2 would produce
    println!("\n=== Reconstructing mask from JB2 blits ===");
    let mut reconstructed = BitImage::new(width, height)?;
    
    for (left, bottom, shape_idx) in &blits {
        if *shape_idx < bitmaps.len() {
            let shape = &bitmaps[*shape_idx];
            let shape_w = shape.width;
            let shape_h = shape.height;
            
            // Blit coordinates are left, bottom in DjVu coordinate system
            // Convert to top-left for our image (y=0 at top)
            let top = height as i32 - *bottom - shape_h as i32;
            
            for sy in 0..shape_h {
                for sx in 0..shape_w {
                    let dx = *left + sx as i32;
                    let dy = top + sy as i32;
                    
                    if dx >= 0 && dx < width as i32 && dy >= 0 && dy < height as i32 {
                        if shape.get_pixel_unchecked(sx, sy) {
                            reconstructed.set_usize(dx as usize, dy as usize, true);
                        }
                    }
                }
            }
        }
    }
    
    // Count reconstructed black pixels
    let mut recon_black = 0;
    for y in 0..height {
        for x in 0..width {
            if reconstructed.get_pixel_unchecked(x as usize, y as usize) {
                recon_black += 1;
            }
        }
    }
    println!("Black pixels in reconstructed mask: {} ({:.2}%)", recon_black,
             100.0 * recon_black as f64 / (width * height) as f64);
    
    save_bitimage_as_pbm(&reconstructed, "/tmp/2_reconstructed_mask.pbm")?;
    println!("Saved: /tmp/2_reconstructed_mask.pbm");
    
    println!("\n=== Compare ===");
    println!("View with: display /tmp/1_raw_mask.pbm /tmp/2_reconstructed_mask.pbm");
    
    Ok(())
}
