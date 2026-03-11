//! Test multi-page compound DjVu documents with IW44+JB2
//! This simulates what Lege produces: pages with both background and foreground mask

use djvu_encoder::doc::builder::{DjvuBuilder, PageBuilder};
use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use djvu_encoder::encode::jb2::symbol_dict::BitImage;
use djvu_encoder::image::image_formats::{Bitmap, GrayPixel, Pixel, Pixmap};
use std::fs;
use std::process::Command;

/// Convert BitImage to Bitmap for use with PageBuilder API
fn bitimage_to_bitmap(bit_image: &BitImage) -> Bitmap {
    let width = bit_image.width as u32;
    let height = bit_image.height as u32;
    let mut bitmap = Bitmap::new(width, height);

    for y in 0..height {
        for x in 0..width {
            let bit = bit_image.get_pixel_unchecked(x as usize, y as usize);
            // 1 (true) = black foreground, 0 (false) = white background
            let gray_value = if bit { 0 } else { 255 };
            bitmap.put_pixel(x, y, GrayPixel::new(gray_value));
        }
    }

    bitmap
}

fn create_test_background(width: u32, height: u32, page_num: u32) -> Pixmap {
    // Create a gradient background that varies per page
    Pixmap::from_fn(width, height, |x, y| {
        let r = ((x * 255 / width) + page_num * 30) as u8;
        let g = ((y * 255 / height) + page_num * 20) as u8;
        let b = ((page_num * 50) % 255) as u8;
        Pixel::new(r, g, b)
    })
}

fn create_test_foreground(width: u32, height: u32, page_num: u32) -> BitImage {
    // Create a foreground mask with prominent text-like patterns
    let mut mask = BitImage::new(width, height).expect("Failed to create mask");

    // Add title at top
    for row in 20..60 {
        for col in 50..550 {
            if row > 25 && row < 55 && (col % 20 < 15) {
                mask.set_usize(col, row, true);
            }
        }
    }

    // Add multiple "text lines" that vary by page
    let num_lines = 8 + (page_num % 3);
    for i in 0..num_lines {
        let y_offset = 100 + (i * 80) as usize;
        let x_offset = 50 + ((page_num * 10) % 50) as usize;

        // Create a line of "text" with word-like blocks
        for word in 0..4 {
            let word_start = x_offset + word * 120;
            let word_width = 80 + (word * 10);

            for row in 0..40 {
                for col in 0..word_width {
                    let x = word_start + col;
                    let y = y_offset + row;
                    if x < width as usize && y < height as usize {
                        // Make it look like text with vertical stripes
                        if (col % 12 < 9) && (row > 8 && row < 32) {
                            mask.set_usize(x, y, true);
                        }
                    }
                }
            }
        }
    }

    // Add page number at bottom
    let bottom_y = height as usize - 60;
    for row in 0..30 {
        for col in 250..350 {
            if (col % 15 < 10) && (row > 5 && row < 25) {
                mask.set_usize(col, bottom_y + row, true);
            }
        }
    }

    mask
}

#[test]
fn test_single_page_compound() {
    println!("=== Testing single compound page (IW44 + JB2) ===\n");

    let width = 600u32;
    let height = 800u32;

    // Create background
    let background = create_test_background(width, height, 0);

    // Create foreground mask
    let foreground = create_test_foreground(width, height, 0);

    // Build page with both layers
    let mut page = PageComponents::new_with_dimensions(width, height);
    page = page
        .with_background(background)
        .expect("Failed to add background");
    page = page
        .with_jb2_auto_extract(foreground)
        .expect("Failed to add foreground");

    // Encode
    let params = PageEncodeParams::default();
    let djvu_bytes = page
        .encode(&params, 1, 300, 1, Some(2.2))
        .expect("Failed to encode page");

    println!("Encoded single page: {} bytes", djvu_bytes.len());

    // Write to file
    let test_file = "/tmp/single_compound_page.djvu";
    fs::write(test_file, &djvu_bytes).expect("Failed to write file");

    // Test with djvudump
    println!("\n=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));

    // Test decode
    println!("\n=== ddjvu decode test ===");
    let output = Command::new("ddjvu")
        .args(["-format=ppm", test_file, "/tmp/single_compound_decoded.ppm"])
        .output()
        .expect("Failed to run ddjvu");

    if output.status.success() {
        println!("SUCCESS: Single compound page decoded correctly");
        let _ = fs::remove_file("/tmp/single_compound_decoded.ppm");
    } else {
        println!("FAILED: {}", String::from_utf8_lossy(&output.stderr));
        panic!("Single page decode failed");
    }

    // Test with djview (just check if it can open)
    println!("\n=== Testing with djview ===");
    let output = Command::new("djview").arg("--help").output();

    if output.is_ok() {
        println!("djview is available for manual testing: {}", test_file);
    }

    let _ = fs::remove_file(test_file);
}

#[test]
fn test_multipage_compound_document() {
    println!("=== Testing multi-page compound document (3 pages, IW44 + JB2) ===\n");

    let width = 600u32;
    let height = 800u32;
    let num_pages = 3;

    // Build document using DjvuBuilder
    let doc = DjvuBuilder::new(num_pages as usize).with_dpi(300).build();

    // Create and add each page
    for page_num in 0..num_pages {
        println!("Creating page {}...", page_num + 1);

        let background = create_test_background(width, height, page_num);
        let foreground_bitimage = create_test_foreground(width, height, page_num);

        // Convert BitImage to Bitmap (for PageBuilder API)
        let foreground = bitimage_to_bitmap(&foreground_bitimage);

        let page = PageBuilder::new(page_num as usize, width, height)
            .with_background(background)
            .expect("Failed to add background")
            .with_foreground(foreground, 0, 0)
            .build()
            .expect("Failed to build page");

        doc.add_page(page).expect("Failed to add page");
        println!("  Page {} added", page_num + 1);
    }

    // Finalize document
    println!("\nFinalizing document...");
    let bundled = doc.finalize().expect("Failed to finalize document");

    println!("Bundled document: {} bytes", bundled.len());

    // Write to file
    let test_file = "/tmp/multipage_compound_test.djvu";
    fs::write(test_file, &bundled).expect("Failed to write file");

    // Test with djvudump
    println!("\n=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    let dump_output = String::from_utf8_lossy(&output.stdout);
    println!("{}", dump_output);

    // Verify structure
    assert!(dump_output.contains("FORM:DJVM"), "Missing FORM:DJVM");
    assert!(dump_output.contains("DIRM"), "Missing DIRM chunk");
    assert!(dump_output.contains("BG44"), "Missing BG44 chunks");
    assert!(dump_output.contains("Sjbz"), "Missing Sjbz chunks");

    // Count pages
    let page_count = dump_output.matches("FORM:DJVU").count();
    assert_eq!(page_count, num_pages as usize, "Wrong number of pages");

    // Test decode each page
    for page_num in 1..=num_pages {
        println!("\n=== Testing decode of page {} ===", page_num);
        let output_file = format!("/tmp/multipage_decoded_page_{}.ppm", page_num);
        let output = Command::new("ddjvu")
            .args([
                "-format=ppm",
                &format!("-page={}", page_num),
                test_file,
                &output_file,
            ])
            .output()
            .expect("Failed to run ddjvu");

        if output.status.success() {
            println!("  Page {} decoded successfully", page_num);
            let _ = fs::remove_file(&output_file);
        } else {
            println!(
                "  FAILED to decode page {}: {}",
                page_num,
                String::from_utf8_lossy(&output.stderr)
            );
            panic!("Failed to decode page {}", page_num);
        }
    }

    println!("\nSUCCESS: All {} pages decoded correctly", num_pages);
    println!("Test file available at: {}", test_file);

    // Don't remove test file so user can inspect it
    println!("\nYou can test with: djview {}", test_file);
}

#[test]
fn test_ten_page_compound_document() {
    println!("=== Testing 10-page compound document ===\n");

    let width = 600u32;
    let height = 800u32;
    let num_pages = 10;

    // Build document using DjvuBuilder
    let doc = DjvuBuilder::new(num_pages as usize).with_dpi(300).build();

    // Create and add each page
    for page_num in 0..num_pages {
        if page_num % 3 == 0 {
            println!("Creating page {}...", page_num + 1);
        }

        let background = create_test_background(width, height, page_num);
        let foreground_bitimage = create_test_foreground(width, height, page_num);

        // Convert BitImage to Bitmap (for PageBuilder API)
        let foreground = bitimage_to_bitmap(&foreground_bitimage);

        let page = PageBuilder::new(page_num as usize, width, height)
            .with_background(background)
            .expect("Failed to add background")
            .with_foreground(foreground, 0, 0)
            .build()
            .expect("Failed to build page");

        doc.add_page(page).expect("Failed to add page");
    }

    println!("All {} pages created, finalizing...", num_pages);

    // Finalize document
    let bundled = doc.finalize().expect("Failed to finalize document");

    println!("Bundled document: {} bytes", bundled.len());

    let test_file = "/tmp/ten_page_compound_test.djvu";
    fs::write(test_file, &bundled).expect("Failed to write file");

    // Test structure
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    let dump_output = String::from_utf8_lossy(&output.stdout);

    let page_count = dump_output.matches("FORM:DJVU").count();
    assert_eq!(page_count, num_pages as usize, "Wrong number of pages");

    // Test decode first, middle, and last page
    for page_num in [1, 5, 10] {
        let output_file = format!("/tmp/ten_page_decoded_{}.ppm", page_num);
        let output = Command::new("ddjvu")
            .args([
                "-format=ppm",
                &format!("-page={}", page_num),
                test_file,
                &output_file,
            ])
            .output()
            .expect("Failed to run ddjvu");

        if output.status.success() {
            println!("Page {} decoded successfully", page_num);
            let _ = fs::remove_file(&output_file);
        } else {
            panic!(
                "Failed to decode page {}: {}",
                page_num,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    println!("\nSUCCESS: 10-page document created and verified");
    println!("Test file: {}", test_file);
}
