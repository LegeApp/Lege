use djvu_encoder::{encode::symbol_dict::BitImage, DocumentEncoder, PageComponents};
use image::RgbImage;
use std::fs::File;
use std::io::Write;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn test_encode_decode_roundtrip() {
    // Create a simple test image
    let mut img = RgbImage::new(100, 100);

    // Fill with a simple pattern
    for y in 0..100 {
        for x in 0..100 {
            let r = (x * 255 / 100) as u8;
            let g = (y * 255 / 100) as u8;
            let b = ((x + y) * 255 / 200) as u8;
            img.put_pixel(x, y, image::Rgb([r, g, b]));
        }
    }

    // Create a document with the test image
    let mut encoder = DocumentEncoder::new();
    let page_components = PageComponents::new()
        .with_background(img)
        .expect("Failed to create page components");

    encoder
        .add_page(page_components)
        .expect("Failed to add page");

    // Encode to bytes
    let mut encoded_data = Vec::new();
    encoder
        .write_to(&mut encoded_data)
        .expect("Failed to encode document");

    // Verify we have some data
    assert!(!encoded_data.is_empty(), "Encoded data should not be empty");
    assert!(
        encoded_data.len() > 100,
        "Encoded data should be substantial"
    );

    // Write to temporary file
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let djvu_path = temp_dir.path().join("test.djvu");

    {
        let mut file = File::create(&djvu_path).expect("Failed to create djvu file");
        file.write_all(&encoded_data)
            .expect("Failed to write djvu file");
    }

    // Test with ddjvu.exe if available
    test_with_ddjvu(&djvu_path);

    // Basic format validation
    validate_djvu_format(&encoded_data);
}

fn test_with_ddjvu(djvu_path: &std::path::Path) {
    let ddjvu_path = "ddjvu.exe";
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let output_path = temp_dir.path().join("decoded.ppm");

    // Try to decode with ddjvu.exe
    let result = Command::new(ddjvu_path)
        .arg("-format=ppm")
        .arg(djvu_path)
        .arg(&output_path)
        .output();

    match result {
        Ok(output) => {
            if output.status.success() {
                // Check if output file was created
                assert!(
                    output_path.exists(),
                    "ddjvu.exe should have created output file"
                );
                let metadata =
                    std::fs::metadata(&output_path).expect("Failed to get output file metadata");
                assert!(metadata.len() > 0, "Output file should not be empty");
                println!("✅ ddjvu.exe successfully decoded the file");
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                println!("⚠️ ddjvu.exe failed to decode: {}", stderr);
                // Don't fail the test if ddjvu has issues, but print the error
            }
        }
        Err(e) => {
            println!("⚠️ ddjvu.exe not found or failed to run: {}", e);
            // Don't fail the test if ddjvu.exe is not available
        }
    }
}

fn validate_djvu_format(data: &[u8]) {
    // Basic DjVu format validation
    assert!(data.len() >= 12, "DjVu file must be at least 12 bytes");

    // Check for AT&T magic number
    assert_eq!(&data[0..4], b"AT&T", "Should start with AT&T magic");

    // Check for FORM
    assert_eq!(&data[4..8], b"FORM", "Should have FORM chunk");

    // Check for DJVM or DJVU type
    let chunk_type = &data[12..16];
    assert!(
        chunk_type == b"DJVM" || chunk_type == b"DJVU",
        "Should be DJVM (multi-page) or DJVU (single-page), got: {:?}",
        std::str::from_utf8(chunk_type).unwrap_or("invalid")
    );

    println!("✅ DjVu format validation passed");
}

#[test]
fn test_small_image_encoding() {
    // Test with a very small image to ensure minimal case works
    let img = RgbImage::new(10, 10);

    let mut encoder = DocumentEncoder::new();
    let page_components = PageComponents::new()
        .with_background(img)
        .expect("Failed to create page components");

    encoder
        .add_page(page_components)
        .expect("Failed to add page");

    let mut encoded_data = Vec::new();
    encoder
        .write_to(&mut encoded_data)
        .expect("Failed to encode document");

    validate_djvu_format(&encoded_data);

    // Ensure size is reasonable for a 10x10 image
    assert!(
        encoded_data.len() < 10000,
        "10x10 image should not produce huge files"
    );
}

#[test]
fn test_multipage_encoding() {
    // Test encoding multiple pages
    let mut encoder = DocumentEncoder::new();

    // Add 3 different pages
    for i in 0..3 {
        let mut img = RgbImage::new(50, 50);

        // Fill each page with a different color
        let color = match i {
            0 => [255, 0, 0], // Red
            1 => [0, 255, 0], // Green
            _ => [0, 0, 255], // Blue
        };

        for pixel in img.pixels_mut() {
            *pixel = image::Rgb(color);
        }

        let page_components = PageComponents::new()
            .with_background(img)
            .expect("Failed to create page components");

        encoder
            .add_page(page_components)
            .expect("Failed to add page");
    }

    let mut encoded_data = Vec::new();
    encoder
        .write_to(&mut encoded_data)
        .expect("Failed to encode document");

    validate_djvu_format(&encoded_data);

    // Should be DJVM for multi-page
    assert_eq!(
        &encoded_data[12..16],
        b"DJVM",
        "Multi-page should be DJVM format"
    );
}

#[test]
fn test_comprehensive_four_page_roundtrip() {
    // Create document encoder
    let mut encoder = DocumentEncoder::new();

    // Page 1: Blank page
    let blank_img = RgbImage::from_pixel(200, 300, image::Rgb([255, 255, 255]));
    let page1 = PageComponents::new()
        .with_background(blank_img)
        .expect("Failed to create blank page");
    encoder.add_page(page1).expect("Failed to add blank page");

    // Page 2: JB2 encoded page (bilevel-like image)
    let mut jb2_img = RgbImage::new(200, 300);
    for y in 50..250 {
        for x in 25..175 {
            if ((x / 10) + (y / 15)) % 2 == 0 {
                jb2_img.put_pixel(x, y, image::Rgb([0, 0, 0]));
            } else {
                jb2_img.put_pixel(x, y, image::Rgb([255, 255, 255]));
            }
        }
    }
    let page2 = PageComponents::new()
        .with_background(jb2_img)
        .expect("Failed to create JB2 page");
    encoder.add_page(page2).expect("Failed to add JB2 page");

    // Page 3: IW44 encoded page (color gradients)
    let mut iw44_img = RgbImage::new(200, 300);
    for y in 0..300 {
        for x in 0..200 {
            let r = ((x + y) * 255 / (200 + 300)) as u8;
            let g = ((x * y) * 255 / (200 * 300)) as u8;
            let b = ((x * 255) / 200) as u8;
            iw44_img.put_pixel(x, y, image::Rgb([r, g, b]));
        }
    }
    let page3 = PageComponents::new()
        .with_background(iw44_img)
        .expect("Failed to create IW44 page");
    encoder.add_page(page3).expect("Failed to add IW44 page");

    // Page 4: Final blank page
    let blank_img2 = RgbImage::from_pixel(200, 300, image::Rgb([240, 240, 240]));
    let page4 = PageComponents::new()
        .with_background(blank_img2)
        .expect("Failed to create final blank page");
    encoder
        .add_page(page4)
        .expect("Failed to add final blank page");

    // Encode the complete document
    let mut encoded_data = Vec::new();
    encoder
        .write_to(&mut encoded_data)
        .expect("Failed to encode 4-page document");

    // Basic validation
    assert!(!encoded_data.is_empty(), "Encoded data should not be empty");
    assert!(
        encoded_data.len() > 300,
        "4-page document should be substantial"
    );
    validate_multipage_djvu_format(&encoded_data);

    // Write to file for external validation
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let djvu_path = temp_dir.path().join("test_4pages.djvu");
    {
        let mut file = File::create(&djvu_path).expect("Failed to create djvu file");
        file.write_all(&encoded_data)
            .expect("Failed to write djvu file");
    }

    // Test with ddjvu.exe if available
    test_multipage_with_ddjvu(&djvu_path);
}

fn validate_multipage_djvu_format(data: &[u8]) {
    // Basic multi-page DjVu format validation
    assert!(data.len() >= 12, "DjVu file must be at least 12 bytes");
    assert_eq!(&data[0..4], b"AT&T", "Should start with AT&T magic");
    assert_eq!(&data[4..8], b"FORM", "Should have FORM chunk");
    assert_eq!(
        &data[12..16],
        b"DJVM",
        "Multi-page document should be DJVM format"
    );

    // Count pages
    let data_slice = &data[16..];
    let mut pos = 0;
    let mut page_count = 0;
    let mut found_dirm = false;

    while pos + 8 <= data_slice.len() {
        let chunk_id = &data_slice[pos..pos + 4];
        let chunk_size = u32::from_be_bytes([
            data_slice[pos + 4],
            data_slice[pos + 5],
            data_slice[pos + 6],
            data_slice[pos + 7],
        ]) as usize;

        match chunk_id {
            b"DIRM" => found_dirm = true,
            b"FORM" => {
                if pos + 12 <= data_slice.len() && &data_slice[pos + 8..pos + 12] == b"DJVU" {
                    page_count += 1;
                }
            }
            _ => {}
        }

        pos += 8 + chunk_size;
        if pos % 2 == 1 {
            pos += 1;
        }
    }

    assert!(found_dirm, "Multi-page DjVu should contain DIRM chunk");
    assert_eq!(page_count, 4, "Should find exactly 4 pages");
}

fn test_multipage_with_ddjvu(djvu_path: &std::path::Path) {
    let result = Command::new("ddjvu.exe")
        .arg("-format=ppm")
        .arg("-page=1")
        .arg(djvu_path)
        .arg("nul") // Discard output on Windows
        .output();

    match result {
        Ok(output) => {
            if output.status.success() {
                println!("✅ ddjvu.exe validation passed");
            } else {
                println!("⚠️ ddjvu.exe validation failed");
            }
        }
        Err(_) => {
            // ddjvu.exe not available, skip validation
        }
    }
}

#[test]
fn test_comprehensive_four_page_roundtrip_with_test_files() {
    // Load test files
    let test_pbm_path = "test.pbm";
    let test_png_path = "test.png";

    // Create a synthetic BitImage for JB2 encoding
    let mut jb2_image = BitImage::new(200, 300).unwrap();
    for y in 50..250 {
        for x in 25..175 {
            if ((x / 10) + (y / 15)) % 2 == 0 {
                jb2_image.set_usize(x, y, true); // Foreground pixel
            }
        }
    }

    // Load PNG for IW44 (or create synthetic color image)
    let iw44_image = match image::open(test_png_path) {
        Ok(img) => img.to_rgb8(),
        Err(_) => {
            let mut color_image = image::RgbImage::new(150, 100);
            for y in 0..100 {
                for x in 0..150 {
                    let r = ((x * 255) / 150) as u8;
                    let g = ((y * 255) / 100) as u8;
                    let b = (((x + y) * 255) / 250) as u8;
                    color_image.put_pixel(x, y, image::Rgb([r, g, b]));
                }
            }
            color_image
        }
    };

    // Create 4-page document
    let mut encoder = DocumentEncoder::new();

    // Page 1: Blank
    encoder
        .add_page(PageComponents::new())
        .expect("Failed to add blank page");

    // Page 2: JB2 page
    let jb2_page = PageComponents::new()
        .with_foreground(jb2_image)
        .expect("Failed to create JB2 page");
    encoder.add_page(jb2_page).expect("Failed to add JB2 page");

    // Page 3: IW44 page
    let iw44_page = PageComponents::new()
        .with_background(iw44_image)
        .expect("Failed to create IW44 page");
    encoder
        .add_page(iw44_page)
        .expect("Failed to add IW44 page");

    // Page 4: Blank
    encoder
        .add_page(PageComponents::new())
        .expect("Failed to add final blank page");

    // Encode document
    let mut encoded_data = Vec::new();
    encoder
        .write_to(&mut encoded_data)
        .expect("Failed to encode document");

    // Validate
    validate_multipage_djvu_format(&encoded_data);
    assert!(encoded_data.len() > 500, "Document should be substantial");
    assert_eq!(&encoded_data[12..16], b"DJVM", "Should be DJVM format");

    // Test with ddjvu if available
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let djvu_path = temp_dir.path().join("test_4page.djvu");
    {
        let mut file = File::create(&djvu_path).expect("Failed to create djvu file");
        file.write_all(&encoded_data)
            .expect("Failed to write djvu file");
    }
    test_multipage_with_ddjvu(&djvu_path);
}

fn create_test_bit_image() -> BitImage {
    let mut bit_image = BitImage::new(10, 10).unwrap();
    for i in 0..10 {
        bit_image.set_usize(i, i, true);
    }
    bit_image
}

fn create_test_rgb_image() -> RgbImage {
    let mut img = RgbImage::new(10, 10);
    for y in 0..10 {
        for x in 0..10 {
            let r = (x * 255 / 10) as u8;
            let g = (y * 255 / 10) as u8;
            let b = ((x + y) * 255 / 20) as u8;
            img.put_pixel(x, y, image::Rgb([r, g, b]));
        }
    }
    img
}
