//! Debug: Full JB2 encode/decode cycle

use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use djvu_encoder::encode::jb2::symbol_dict::BitImage;
use djvu_encoder::image::image_formats::{Pixel, Pixmap};
use std::fs;
use std::fs::File;
use std::io::Write;
use std::process::Command;

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

    // Create a simple obvious mask
    println!("=== Creating test mask ===");
    let mut mask = BitImage::new(width, height)?;

    // Draw a big X across the image - very obvious pattern
    for i in 0..600 {
        let x1 = (i * 800 / 600) as usize;
        let x2 = 800 - 1 - x1;
        let y = i as usize;

        // Make lines thick (5 pixels)
        for dx in 0..5 {
            for dy in 0..5 {
                if x1 + dx < 800 && y + dy < 600 {
                    mask.set_usize(x1 + dx, y + dy, true);
                }
                if x2 >= dx && y + dy < 600 {
                    mask.set_usize(x2 - dx, y + dy, true);
                }
            }
        }
    }

    // Add a border too
    for x in 0..800 {
        for t in 0..10 {
            mask.set_usize(x, t, true);
            mask.set_usize(x, 599 - t, true);
        }
    }
    for y in 0..600 {
        for t in 0..10 {
            mask.set_usize(t, y, true);
            mask.set_usize(799 - t, y, true);
        }
    }

    save_bitimage_as_pbm(&mask, "/tmp/input_mask.pbm")?;
    println!("Saved input mask: /tmp/input_mask.pbm");

    // Create a simple gradient background
    let background = Pixmap::from_fn(width, height, |x, y| {
        let r = ((x * 255 / width) % 256) as u8;
        let g = ((y * 255 / height) % 256) as u8;
        let b = 128u8;
        Pixel::new(r, g, b)
    });

    // Encode as compound DjVu
    println!("\n=== Encoding compound DjVu ===");
    let mut page = PageComponents::new_with_dimensions(width, height);
    page = page.with_background(background)?;
    page = page.with_jb2_auto_extract(mask)?;

    let params = PageEncodeParams::default();
    let djvu_bytes = page.encode(&params, 1, 300, 1, Some(2.2))?;

    fs::write("/tmp/test_compound.djvu", &djvu_bytes)?;
    println!(
        "Encoded: /tmp/test_compound.djvu ({} bytes)",
        djvu_bytes.len()
    );

    // Dump structure
    println!("\n=== djvudump ===");
    let output = Command::new("djvudump")
        .arg("/tmp/test_compound.djvu")
        .output()?;
    println!("{}", String::from_utf8_lossy(&output.stdout));

    // Extract the mask
    println!("=== Extracting mask layer ===");
    let output = Command::new("ddjvu")
        .args([
            "-mode=mask",
            "-format=pbm",
            "/tmp/test_compound.djvu",
            "/tmp/output_mask.pbm",
        ])
        .output()?;
    if !output.status.success() {
        println!("ddjvu error: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Compare
    println!("\n=== Comparing masks ===");
    let input_meta = fs::metadata("/tmp/input_mask.pbm")?;
    let output_meta = fs::metadata("/tmp/output_mask.pbm")?;
    println!("Input mask:  {} bytes", input_meta.len());
    println!("Output mask: {} bytes", output_meta.len());

    // Show the masks side by side using convert
    println!("\nView masks:");
    println!("  Input:  display /tmp/input_mask.pbm");
    println!("  Output: display /tmp/output_mask.pbm");
    println!("  DjVu:   djview /tmp/test_compound.djvu");

    Ok(())
}
