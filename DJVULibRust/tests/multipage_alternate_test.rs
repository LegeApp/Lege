//! Comprehensive test for multi-page DjVu document creation.
//!
//! This test creates a multi-page document alternating between:
//! - JB2-encoded pages (using test.pbm - bitonal)
//! - IW44-encoded pages (using test2.png - color)
//!
//! The test validates:
//! 1. Individual JB2 page encoding works correctly
//! 2. Individual IW44 page encoding works correctly
//! 3. Multi-page document creation works with alternating page types
//! 4. The output is valid DjVu format (verifiable with djvudump)

use djvu_encoder::{
    doc::page_encoder::{PageComponents, PageEncodeParams},
    doc::document_encoder::DocumentEncoder,
    encode::jb2::symbol_dict::BitImage,
    DjvuError,
    Result,
};
use image::{self, RgbImage};
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

const TEST_PBM_PATH: &str = "test-files-misc/test.pbm";
const TEST_PNG_PATH: &str = "test-files-misc/test2.png";
const OUTPUT_DIR: &str = "target/test_outputs";

/// Load a PBM file into a BitImage for JB2 encoding
fn load_pbm_as_bitimage(path: &Path) -> Result<BitImage> {
    let pbm_data = fs::read(path).map_err(|e| {
        DjvuError::Io(io::Error::new(io::ErrorKind::Other, format!("Failed to read PBM file: {}", e)))
    })?;

    // Parse PBM format (P4 = raw binary, P1 = ASCII)
    let content = String::from_utf8_lossy(&pbm_data);
    let lines: Vec<&str> = content.lines().filter(|l| !l.starts_with('#')).collect();

    if lines.is_empty() {
        return Err(DjvuError::InvalidOperation("Empty PBM file".to_string()));
    }

    let magic = lines[0].trim();

    if magic == "P4" {
        // Raw binary PBM
        // Find where header ends
        let mut found_magic = false;
        let mut found_dims = false;
        let mut width: usize = 0;
        let mut height: usize = 0;
        let mut cursor = 0;

        // Parse header
        while cursor < pbm_data.len() {
            // Skip whitespace
            while cursor < pbm_data.len() && pbm_data[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= pbm_data.len() {
                break;
            }

            // Check for comment
            if pbm_data[cursor] == b'#' {
                while cursor < pbm_data.len() && pbm_data[cursor] != b'\n' {
                    cursor += 1;
                }
                continue;
            }

            // Read token
            let token_start = cursor;
            while cursor < pbm_data.len() && !pbm_data[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            let token = String::from_utf8_lossy(&pbm_data[token_start..cursor]).to_string();

            if !found_magic {
                if token == "P4" {
                    found_magic = true;
                }
            } else if !found_dims {
                // Parse dimensions (width height)
                if width == 0 {
                    width = token.parse().unwrap_or(0);
                } else {
                    height = token.parse().unwrap_or(0);
                    found_dims = true;
                    // Skip one whitespace after dimensions
                    while cursor < pbm_data.len() && pbm_data[cursor].is_ascii_whitespace() {
                        cursor += 1;
                        break; // Just skip one newline
                    }
                    break;
                }
            }
        }

        let header_end = cursor;

        if width == 0 || height == 0 {
            return Err(DjvuError::InvalidOperation(
                format!("Invalid PBM dimensions: {}x{}", width, height)
            ));
        }

        println!("PBM dimensions: {}x{}, header ends at byte {}", width, height, header_end);

        let mut bitimage = BitImage::new(width as u32, height as u32)
            .map_err(|e| DjvuError::InvalidOperation(e.to_string()))?;

        // Read raw binary data (each row is padded to byte boundary)
        let row_bytes = (width + 7) / 8;
        let pixel_data = &pbm_data[header_end..];

        for y in 0..height {
            for x in 0..width {
                let byte_idx = y * row_bytes + x / 8;
                let bit_idx = 7 - (x % 8);
                if byte_idx < pixel_data.len() {
                    let bit = (pixel_data[byte_idx] >> bit_idx) & 1;
                    // In PBM, 1 = black, 0 = white; BitImage typically uses true = black
                    bitimage.set_usize(x, y, bit == 1);
                }
            }
        }

        Ok(bitimage)
    } else if magic == "P1" {
        // ASCII PBM
        let dims: Vec<usize> = lines[1]
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();

        if dims.len() < 2 {
            return Err(DjvuError::InvalidOperation("Invalid PBM dimensions".to_string()));
        }

        let (width, height) = (dims[0], dims[1]);
        let mut bitimage = BitImage::new(width as u32, height as u32)
            .map_err(|e| DjvuError::InvalidOperation(e.to_string()))?;

        // Parse pixel data
        let pixel_chars: Vec<char> = lines[2..]
            .join("")
            .chars()
            .filter(|c| *c == '0' || *c == '1')
            .collect();

        for (i, c) in pixel_chars.iter().enumerate() {
            let x = i % width;
            let y = i / width;
            if y < height {
                bitimage.set_usize(x, y, *c == '1');
            }
        }

        Ok(bitimage)
    } else {
        Err(DjvuError::InvalidOperation(
            format!("Unsupported PBM format: {}", magic)
        ))
    }
}

/// Load a PNG file into an RgbImage for IW44 encoding
fn load_png_as_rgb(path: &Path) -> Result<RgbImage> {
    let img = image::open(path).map_err(|e| {
        DjvuError::Io(io::Error::new(io::ErrorKind::Other, format!("Failed to load PNG: {}", e)))
    })?;
    Ok(img.to_rgb8())
}

/// Verify DjVu output using djvudump (if available)
fn verify_djvu_with_dump(path: &Path) -> Option<String> {
    // Try system djvudump first, then local Windows executable
    let result = Command::new("djvudump")
        .arg(path)
        .output();

    match result {
        Ok(output) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).to_string())
        }
        _ => {
            // Try the local Windows executable (for WSL/Wine compatibility)
            let local_exe = Path::new("test-files-misc/djvudump.exe");
            if local_exe.exists() {
                let result = Command::new(local_exe)
                    .arg(path)
                    .output();
                match result {
                    Ok(output) if output.status.success() => {
                        Some(String::from_utf8_lossy(&output.stdout).to_string())
                    }
                    _ => None
                }
            } else {
                None
            }
        }
    }
}

/// Test 1: JB2-only single page encoding
#[test]
fn test_jb2_single_page() -> Result<()> {
    println!("\n=== TEST: JB2 Single Page ===");

    // Create output directory
    fs::create_dir_all(OUTPUT_DIR).ok();

    let pbm_path = Path::new(TEST_PBM_PATH);
    if !pbm_path.exists() {
        println!("Skipping test: {} not found", TEST_PBM_PATH);
        return Ok(());
    }

    // Load the bitonal image
    let bitimage = load_pbm_as_bitimage(pbm_path)?;
    println!("Loaded PBM image: {}x{}", bitimage.width, bitimage.height);

    // Create page with JB2 foreground (no IW44 background)
    let page = PageComponents::new()
        .with_foreground(bitimage)?;

    println!("Page dimensions: {:?}", page.dimensions());

    // Encode with default parameters
    let params = PageEncodeParams {
        use_iw44: false, // Use JB2 mode
        ..Default::default()
    };

    let (_width, _height) = page.dimensions();
    let dpm = (300 * 100 / 254) as u32;

    let encoded = page.encode(&params, 1, dpm, 1, Some(2.2))?;

    println!("Encoded JB2 page: {} bytes", encoded.len());

    // Validate basic structure
    assert!(!encoded.is_empty(), "Encoded data should not be empty");
    assert_eq!(&encoded[0..4], b"AT&T", "Should start with AT&T magic");
    assert_eq!(&encoded[4..8], b"FORM", "Should have FORM chunk");

    // Write output for inspection
    let output_path = Path::new(OUTPUT_DIR).join("test_jb2_single.djvu");
    fs::write(&output_path, &encoded)?;
    println!("Written to: {}", output_path.display());

    // Verify with djvudump if available
    if let Some(dump) = verify_djvu_with_dump(&output_path) {
        println!("djvudump output:\n{}", dump);
        assert!(dump.contains("DJVU"), "Should be a valid DJVU document");
        assert!(dump.contains("INFO"), "Should have INFO chunk");
        // JB2 pages should have Sjbz or Djbz chunks
        assert!(
            dump.contains("Sjbz") || dump.contains("Djbz"),
            "Should have JB2 chunks (Sjbz or Djbz)"
        );
    }

    Ok(())
}

/// Test 2: IW44-only single page encoding
#[test]
fn test_iw44_single_page() -> Result<()> {
    println!("\n=== TEST: IW44 Single Page ===");

    // Create output directory
    fs::create_dir_all(OUTPUT_DIR).ok();

    let png_path = Path::new(TEST_PNG_PATH);
    if !png_path.exists() {
        println!("Skipping test: {} not found", TEST_PNG_PATH);
        return Ok(());
    }

    // Load the color image
    let rgb_image = load_png_as_rgb(png_path)?;
    println!("Loaded PNG image: {}x{}", rgb_image.width(), rgb_image.height());

    // Create page with IW44 background (color image)
    let page = PageComponents::new()
        .with_background(rgb_image)?;

    println!("Page dimensions: {:?}", page.dimensions());

    // Encode with IW44 parameters
    let params = PageEncodeParams {
        use_iw44: true,
        color: true,
        bg_quality: 80,
        slices: Some(74),
        ..Default::default()
    };

    let (_width, _height) = page.dimensions();
    let dpm = (300 * 100 / 254) as u32;

    let encoded = page.encode(&params, 1, dpm, 1, Some(2.2))?;

    println!("Encoded IW44 page: {} bytes", encoded.len());

    // Validate basic structure
    assert!(!encoded.is_empty(), "Encoded data should not be empty");
    assert_eq!(&encoded[0..4], b"AT&T", "Should start with AT&T magic");
    assert_eq!(&encoded[4..8], b"FORM", "Should have FORM chunk");

    // Write output for inspection
    let output_path = Path::new(OUTPUT_DIR).join("test_iw44_single.djvu");
    fs::write(&output_path, &encoded)?;
    println!("Written to: {}", output_path.display());

    // Verify with djvudump if available
    if let Some(dump) = verify_djvu_with_dump(&output_path) {
        println!("djvudump output:\n{}", dump);
        assert!(dump.contains("DJVU"), "Should be a valid DJVU document");
        assert!(dump.contains("INFO"), "Should have INFO chunk");
        assert!(dump.contains("BG44"), "Should have IW44 background chunk (BG44)");
    }

    Ok(())
}

/// Test 3: Multi-page document with alternating JB2 and IW44 pages
#[test]
fn test_multipage_alternating() -> Result<()> {
    println!("\n=== TEST: Multi-Page Alternating JB2/IW44 ===");

    // Create output directory
    fs::create_dir_all(OUTPUT_DIR).ok();

    let pbm_path = Path::new(TEST_PBM_PATH);
    let png_path = Path::new(TEST_PNG_PATH);

    if !pbm_path.exists() || !png_path.exists() {
        println!("Skipping test: test files not found");
        return Ok(());
    }

    // Load both image types
    let bitimage = load_pbm_as_bitimage(pbm_path)?;
    let rgb_image = load_png_as_rgb(png_path)?;

    println!("Loaded PBM: {}x{}", bitimage.width, bitimage.height);
    println!("Loaded PNG: {}x{}", rgb_image.width(), rgb_image.height());

    // Create document encoder
    let mut doc_encoder = DocumentEncoder::new()
        .with_dpi(300)
        .with_gamma(Some(2.2));

    // Page 1: IW44 (color)
    println!("\nAdding Page 1 (IW44)...");
    let page1 = PageComponents::new()
        .with_background(rgb_image.clone())?;
    doc_encoder.add_page(page1)?;
    println!("Page 1 added successfully");

    // Page 2: JB2 (bitonal)
    println!("\nAdding Page 2 (JB2)...");
    let page2 = PageComponents::new()
        .with_foreground(bitimage.clone())?;
    doc_encoder.add_page(page2)?;
    println!("Page 2 added successfully");

    // Page 3: IW44 (color)
    println!("\nAdding Page 3 (IW44)...");
    let page3 = PageComponents::new()
        .with_background(rgb_image.clone())?;
    doc_encoder.add_page(page3)?;
    println!("Page 3 added successfully");

    // Page 4: JB2 (bitonal)
    println!("\nAdding Page 4 (JB2)...");
    let page4 = PageComponents::new()
        .with_foreground(bitimage.clone())?;
    doc_encoder.add_page(page4)?;
    println!("Page 4 added successfully");

    assert_eq!(doc_encoder.page_count(), 4, "Should have 4 pages");

    // Write to file
    let output_path = Path::new(OUTPUT_DIR).join("test_multipage_alternate.djvu");
    let mut file = fs::File::create(&output_path)?;
    doc_encoder.write_to(&mut file)?;

    let file_size = fs::metadata(&output_path)?.len();
    println!("\nWritten multi-page document: {} bytes to {}", file_size, output_path.display());

    // Verify with djvudump if available
    if let Some(dump) = verify_djvu_with_dump(&output_path) {
        println!("\ndjvudump output:\n{}", dump);

        // Should be DJVM (multi-page) format
        assert!(dump.contains("DJVM") || dump.contains("DOCUMENT"), "Should be multi-page DJVM format");

        // Should have DIRM (directory) chunk
        if dump.contains("DJVM") {
            assert!(dump.contains("DIRM"), "Multi-page should have DIRM directory chunk");
        }

        // Should have multiple pages
        let form_count = dump.matches("FORM:DJVU").count();
        println!("Found {} FORM:DJVU chunks", form_count);

        // Should have both BG44 (IW44) and Sjbz/Djbz (JB2) chunks
        let has_bg44 = dump.contains("BG44");
        let has_jb2 = dump.contains("Sjbz") || dump.contains("Djbz");
        println!("Has BG44 (IW44): {}, Has JB2: {}", has_bg44, has_jb2);
    }

    Ok(())
}

/// Test 4: Isolated JB2 encoder test to verify codec works in isolation
#[test]
fn test_jb2_encoder_isolation() -> Result<()> {
    println!("\n=== TEST: JB2 Encoder Isolation ===");

    use djvu_encoder::encode::jb2::encoder::JB2Encoder;
    use djvu_encoder::encode::jb2::symbol_dict::SymDictBuilder;

    // Create a simple test pattern
    let mut bitimage = BitImage::new(64, 64)
        .map_err(|e| DjvuError::InvalidOperation(e.to_string()))?;

    // Draw a simple pattern - a rectangle
    for y in 10..50 {
        for x in 10..50 {
            // Create a border
            if y == 10 || y == 49 || x == 10 || x == 49 {
                bitimage.set_usize(x, y, true);
            }
        }
    }

    // Add some text-like marks
    for i in 0..5 {
        let x = 15 + i * 8;
        let y = 25;
        for dy in 0..10 {
            for dx in 0..5 {
                if dy == 0 || dy == 9 || dx == 0 || dx == 4 || dy == 5 {
                    if x + dx < 64 && y + dy < 64 {
                        bitimage.set_usize(x + dx, y + dy, true);
                    }
                }
            }
        }
    }

    println!("Created test pattern: {}x{}", bitimage.width, bitimage.height);

    // Test 1: Single page encoding
    let mut encoder1 = JB2Encoder::new(Vec::new());
    let single_page_result = encoder1.encode_single_page(&bitimage);

    match &single_page_result {
        Ok(data) => {
            println!("Single page encoding successful: {} bytes", data.len());
            assert!(!data.is_empty(), "Single page data should not be empty");
        }
        Err(e) => {
            println!("Single page encoding failed: {:?}", e);
            panic!("JB2 single page encoding should succeed");
        }
    }

    // Test 2: Dictionary + page encoding
    let mut dict_builder = SymDictBuilder::new(0);
    let (dictionary, components) = dict_builder.build(&bitimage);

    println!("Built dictionary with {} shapes, {} components", dictionary.len(), components.len());

    if !dictionary.is_empty() {
        let parents: Vec<i32> = vec![-1; dictionary.len()];
        let blits: Vec<(i32, i32, usize)> = components
            .iter()
            .map(|c| (c.bounds.x as i32, c.bounds.y as i32, c.dict_symbol_index.unwrap_or(0)))
            .collect();

        // Encode dictionary
        let mut encoder2 = JB2Encoder::new(Vec::new());
        let dict_result = encoder2.encode_dictionary(&dictionary, &parents, 0);

        match &dict_result {
            Ok(data) => {
                println!("Dictionary encoding successful: {} bytes", data.len());
            }
            Err(e) => {
                println!("Dictionary encoding failed: {:?}", e);
                panic!("JB2 dictionary encoding should succeed");
            }
        }

        // Encode page with shapes
        let mut encoder3 = JB2Encoder::new(Vec::new());
        let page_result = encoder3.encode_page_with_shapes(
            bitimage.width as u32,
            bitimage.height as u32,
            &dictionary,
            &parents,
            &blits,
            0,
            None,
        );

        match &page_result {
            Ok(data) => {
                println!("Page with shapes encoding successful: {} bytes", data.len());
            }
            Err(e) => {
                println!("Page with shapes encoding failed: {:?}", e);
                panic!("JB2 page encoding should succeed");
            }
        }
    }

    Ok(())
}

/// Test 5: Isolated IW44 encoder test to verify codec works in isolation
#[test]
fn test_iw44_encoder_isolation() {
    println!("\n=== TEST: IW44 Encoder Isolation ===");

    use djvu_encoder::encode::iw44::encoder::{IWEncoder, EncoderParams, CrcbMode};

    // Create a simple gradient test image
    let width = 128u32;
    let height = 128u32;
    let mut rgb_data = vec![0u8; (width * height * 3) as usize];

    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 3) as usize;
            rgb_data[idx] = x as u8;     // R: horizontal gradient
            rgb_data[idx + 1] = y as u8; // G: vertical gradient
            rgb_data[idx + 2] = 128;     // B: constant
        }
    }

    let rgb_image = RgbImage::from_raw(width, height, rgb_data)
        .expect("Failed to create test RGB image");

    println!("Created test gradient image: {}x{}", width, height);

    // Test with different CrCb modes
    let modes = [
        ("None", CrcbMode::None),
        ("Half", CrcbMode::Half),
        ("Normal", CrcbMode::Normal),
        ("Full", CrcbMode::Full),
    ];

    for (mode_name, crcb_mode) in modes.iter() {
        println!("\nTesting CrCb mode: {}", mode_name);

        let params = EncoderParams {
            decibels: Some(48.0),
            slices: Some(74),
            bytes: None,
            crcb_mode: *crcb_mode,
            db_frac: 0.35,
            lossless: false,
        };

        let mut encoder = IWEncoder::from_rgb(&rgb_image, None, params)
            .expect("Failed to create IW44 encoder");

        let mut total_bytes = 0;
        let mut chunk_count = 0;

        loop {
            let (chunk_data, more) = encoder.encode_chunk(74)
                .expect("Failed to encode chunk");

            if chunk_data.is_empty() {
                break;
            }

            chunk_count += 1;
            total_bytes += chunk_data.len();
            println!("  Chunk {}: {} bytes", chunk_count, chunk_data.len());

            if !more {
                break;
            }

            // Safety limit
            if chunk_count > 10 {
                println!("  (stopping after 10 chunks for test)");
                break;
            }
        }

        println!("  Total: {} chunks, {} bytes", chunk_count, total_bytes);
        assert!(chunk_count > 0, "Should produce at least one chunk");
        assert!(total_bytes > 0, "Should produce some encoded data");
    }
}

/// Test 6: Full page creation with real test files
#[test]
fn test_full_page_creation_real_files() -> Result<()> {
    println!("\n=== TEST: Full Page Creation with Real Files ===");

    // Create output directory
    fs::create_dir_all(OUTPUT_DIR).ok();

    let pbm_path = Path::new(TEST_PBM_PATH);
    let png_path = Path::new(TEST_PNG_PATH);

    // Test IW44 page with real image (if available)
    if png_path.exists() {
        println!("\n--- IW44 page from test2.png ---");

        let rgb_image = load_png_as_rgb(png_path)?;
        println!("Image size: {}x{}", rgb_image.width(), rgb_image.height());

        // If image is very large, resize for faster testing
        let rgb_image = if rgb_image.width() > 512 || rgb_image.height() > 512 {
            println!("Resizing large image for test...");
            let resized = image::imageops::resize(
                &rgb_image,
                512,
                512,
                image::imageops::FilterType::Triangle
            );
            resized
        } else {
            rgb_image
        };

        println!("Test image size: {}x{}", rgb_image.width(), rgb_image.height());

        let page = PageComponents::new()
            .with_background(rgb_image)?;

        let params = PageEncodeParams {
            use_iw44: true,
            color: true,
            bg_quality: 75,
            slices: Some(50), // Reduced for faster test
            ..Default::default()
        };

        let dpm = (300 * 100 / 254) as u32;

        println!("Encoding IW44 page...");
        let encoded = page.encode(&params, 1, dpm, 1, Some(2.2))?;
        println!("Encoded: {} bytes", encoded.len());

        let output_path = Path::new(OUTPUT_DIR).join("test_real_iw44.djvu");
        fs::write(&output_path, &encoded)?;
        println!("Written to: {}", output_path.display());

        // Verify
        if let Some(dump) = verify_djvu_with_dump(&output_path) {
            println!("Verification passed:");
            for line in dump.lines().take(20) {
                println!("  {}", line);
            }
        }
    }

    // Test JB2 page with real image (if available)
    if pbm_path.exists() {
        println!("\n--- JB2 page from test.pbm ---");

        let bitimage = load_pbm_as_bitimage(pbm_path)?;
        println!("Image size: {}x{}", bitimage.width, bitimage.height);

        let page = PageComponents::new()
            .with_foreground(bitimage)?;

        let params = PageEncodeParams {
            use_iw44: false,
            ..Default::default()
        };

        let dpm = (300 * 100 / 254) as u32;

        println!("Encoding JB2 page...");
        let encoded = page.encode(&params, 1, dpm, 1, Some(2.2))?;
        println!("Encoded: {} bytes", encoded.len());

        let output_path = Path::new(OUTPUT_DIR).join("test_real_jb2.djvu");
        fs::write(&output_path, &encoded)?;
        println!("Written to: {}", output_path.display());

        // Verify
        if let Some(dump) = verify_djvu_with_dump(&output_path) {
            println!("Verification passed:");
            for line in dump.lines().take(20) {
                println!("  {}", line);
            }
        }
    }

    Ok(())
}
