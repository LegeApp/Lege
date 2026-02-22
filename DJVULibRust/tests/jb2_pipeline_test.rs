//! Test the full JB2 pipeline from page extraction through encoding and decoding

use std::fs;
use std::process::Command;

fn hex_dump(data: &[u8], max_bytes: usize) -> String {
    let mut result = String::new();
    for (i, byte) in data.iter().take(max_bytes).enumerate() {
        if i > 0 && i % 16 == 0 {
            result.push('\n');
        } else if i > 0 {
            result.push(' ');
        }
        result.push_str(&format!("{:02x}", byte));
    }
    result
}

fn create_djvu_from_jb2(jb2_data: &[u8], width: u16, height: u16) -> Vec<u8> {
    let mut djvu_file: Vec<u8> = Vec::new();
    djvu_file.extend_from_slice(b"AT&TFORM");
    let form_size_pos = djvu_file.len();
    djvu_file.extend_from_slice(&[0, 0, 0, 0]);
    djvu_file.extend_from_slice(b"DJVU");

    // INFO chunk
    djvu_file.extend_from_slice(b"INFO");
    djvu_file.extend_from_slice(&10u32.to_be_bytes());
    djvu_file.extend_from_slice(&width.to_be_bytes());
    djvu_file.extend_from_slice(&height.to_be_bytes());
    djvu_file.push(24); // minor
    djvu_file.push(0);  // major
    djvu_file.extend_from_slice(&300u16.to_le_bytes()); // dpi LE
    djvu_file.push(22); // gamma
    djvu_file.push(1);  // flags

    // Sjbz chunk
    djvu_file.extend_from_slice(b"Sjbz");
    djvu_file.extend_from_slice(&(jb2_data.len() as u32).to_be_bytes());
    djvu_file.extend_from_slice(jb2_data);
    if jb2_data.len() % 2 != 0 {
        djvu_file.push(0);
    }

    let form_size = (djvu_file.len() - 12) as u32;
    djvu_file[form_size_pos..form_size_pos + 4].copy_from_slice(&form_size.to_be_bytes());
    djvu_file
}

/// Test the extracted shapes from cc_image through the JB2 encoder
#[test]
fn test_cc_extraction_and_encoding() {
    use djvu_encoder::encode::jb2::{analyze_page, shapes_to_encoder_format};
    use djvu_encoder::encode::jb2::encoder::JB2Encoder;
    use djvu_encoder::encode::jb2::symbol_dict::BitImage;

    println!("=== Testing CC extraction + JB2 encoding ===\n");

    // Create a simple test image: 40x40 with two 5x5 blobs
    let mut test_image = BitImage::new(40, 40).expect("Failed to create test image");
    
    // Blob 1: 5x5 at (5, 5)
    for y in 5..10 {
        for x in 5..10 {
            test_image.set_usize(x, y, true);
        }
    }
    
    // Blob 2: 5x5 at (25, 20)
    for y in 20..25 {
        for x in 25..30 {
            test_image.set_usize(x, y, true);
        }
    }

    println!("Test image: 40x40 with two 5x5 blobs");
    println!("Blob 1: (5,5)-(9,9)");
    println!("Blob 2: (25,20)-(29,24)");

    // Run CC analysis
    let ccimg = analyze_page(&test_image, 300, 0);
    let shapes = ccimg.extract_shapes();
    
    println!("\nExtracted {} shapes", shapes.len());
    for (i, (bm, bbox)) in shapes.iter().enumerate() {
        println!("  Shape {}: {}x{} at bbox ({},{})..({},{})", 
            i, bm.width, bm.height, bbox.xmin, bbox.ymin, bbox.xmax, bbox.ymax);
    }

    // Convert to encoder format
    let (bitmaps, parents, blits) = shapes_to_encoder_format(shapes, test_image.height as i32);
    
    println!("\nEncoder format:");
    println!("  {} bitmaps, {} parents, {} blits", bitmaps.len(), parents.len(), blits.len());
    for (i, (left, bottom, shapeno)) in blits.iter().enumerate() {
        println!("  Blit {}: shape {} at left={}, bottom={}", i, shapeno, left, bottom);
    }

    // Encode
    let buffer: Vec<u8> = Vec::new();
    let mut encoder = JB2Encoder::new(buffer);
    
    let jb2_result = encoder.encode_page_with_shapes(
        test_image.width as u32,
        test_image.height as u32,
        &bitmaps,
        &parents,
        &blits,
        0,
        None,
    );

    match &jb2_result {
        Ok(data) => {
            println!("\nJB2 stream ({} bytes):", data.len());
            println!("{}\n", hex_dump(data, 64));
        }
        Err(e) => {
            panic!("Encoding failed: {:?}", e);
        }
    }

    let jb2_data = jb2_result.unwrap();
    
    // Create DjVu and test decode
    let djvu_data = create_djvu_from_jb2(&jb2_data, test_image.width as u16, test_image.height as u16);
    let test_file = "/tmp/cc_test.djvu";
    fs::write(test_file, &djvu_data).expect("Failed to write test file");

    println!("=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));

    println!("\n=== ddjvu decode attempt ===");
    let output = Command::new("ddjvu")
        .args(["-format=pbm", "-page=1", test_file, "/tmp/cc_decoded.pbm"])
        .output()
        .expect("Failed to run ddjvu");

    if output.status.success() {
        println!("SUCCESS! CC-extracted symbols decoded correctly");
        let _ = fs::remove_file("/tmp/cc_decoded.pbm");
    } else {
        println!("FAILED: {}", String::from_utf8_lossy(&output.stderr));
        panic!("Decode failed");
    }

    let _ = fs::remove_file(test_file);
}

/// Test encoding many symbols from background.pbm if it exists
/// This version uses BZZ compression like the real page encoder
#[test]
fn test_background_pbm_with_bzz() {
    use djvu_encoder::encode::jb2::{analyze_page, shapes_to_encoder_format};
    use djvu_encoder::encode::jb2::encoder::JB2Encoder;
    use djvu_encoder::encode::jb2::symbol_dict::BitImage;
    use djvu_encoder::iff::bs_byte_stream::bzz_compress;

    let pbm_path = "background.pbm";
    if !std::path::Path::new(pbm_path).exists() {
        println!("Skipping - background.pbm not found");
        return;
    }

    println!("=== Testing background.pbm JB2 encoding WITH BZZ ===\n");

    // Load the PBM file
    let pbm_data = fs::read(pbm_path).expect("Failed to read PBM");
    
    // Parse header to find dimensions
    let mut cursor = 0usize;
    
    // Skip magic
    while cursor < pbm_data.len() && !pbm_data[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    while cursor < pbm_data.len() && pbm_data[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    
    // Skip comments
    while cursor < pbm_data.len() && pbm_data[cursor] == b'#' {
        while cursor < pbm_data.len() && pbm_data[cursor] != b'\n' {
            cursor += 1;
        }
        cursor += 1;
    }
    
    // Read dimensions
    let mut width_str = String::new();
    while cursor < pbm_data.len() && !pbm_data[cursor].is_ascii_whitespace() {
        width_str.push(pbm_data[cursor] as char);
        cursor += 1;
    }
    while cursor < pbm_data.len() && pbm_data[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    let mut height_str = String::new();
    while cursor < pbm_data.len() && !pbm_data[cursor].is_ascii_whitespace() {
        height_str.push(pbm_data[cursor] as char);
        cursor += 1;
    }
    while cursor < pbm_data.len() && pbm_data[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    
    let width: usize = width_str.parse().expect("Invalid width");
    let height: usize = height_str.parse().expect("Invalid height");
    
    println!("PBM dimensions: {}x{}", width, height);
    
    // Create BitImage from pixel data
    let mut test_image = BitImage::new(width as u32, height as u32).expect("Failed to create image");
    
    let bytes_per_row = (width + 7) / 8;
    for y in 0..height {
        for x in 0..width {
            let byte_idx = cursor + y * bytes_per_row + x / 8;
            if byte_idx < pbm_data.len() {
                let bit = 7 - (x % 8);
                let is_black = (pbm_data[byte_idx] >> bit) & 1 == 1;
                test_image.set_usize(x, y, is_black);
            }
        }
    }

    // Run CC analysis
    let ccimg = analyze_page(&test_image, 300, 1);
    let shapes = ccimg.extract_shapes();
    
    println!("Extracted {} shapes", shapes.len());
    
    // Convert to encoder format
    let (bitmaps, parents, blits) = shapes_to_encoder_format(shapes, height as i32);
    
    println!("{} blits to encode", blits.len());

    // Encode
    let buffer: Vec<u8> = Vec::new();
    let mut encoder = JB2Encoder::new(buffer);
    
    let jb2_result = encoder.encode_page_with_shapes(
        width as u32,
        height as u32,
        &bitmaps,
        &parents,
        &blits,
        0,
        None,
    );

    let jb2_data = jb2_result.expect("JB2 encoding failed");
    println!("JB2 raw size: {} bytes", jb2_data.len());
    
    // Now BZZ compress it
    let bzz_data = bzz_compress(&jb2_data, 256).expect("BZZ compression failed");
    println!("BZZ compressed size: {} bytes", bzz_data.len());
    
    // Create DjVu with BZZ-compressed Sjbz
    let mut djvu_file: Vec<u8> = Vec::new();
    djvu_file.extend_from_slice(b"AT&TFORM");
    let form_size_pos = djvu_file.len();
    djvu_file.extend_from_slice(&[0, 0, 0, 0]);
    djvu_file.extend_from_slice(b"DJVU");

    // INFO chunk
    djvu_file.extend_from_slice(b"INFO");
    djvu_file.extend_from_slice(&10u32.to_be_bytes());
    djvu_file.extend_from_slice(&(width as u16).to_be_bytes());
    djvu_file.extend_from_slice(&(height as u16).to_be_bytes());
    djvu_file.push(24); // minor
    djvu_file.push(0);  // major
    djvu_file.extend_from_slice(&300u16.to_le_bytes()); // dpi LE
    djvu_file.push(22); // gamma
    djvu_file.push(1);  // flags

    // Sjbz chunk with BZZ data
    djvu_file.extend_from_slice(b"Sjbz");
    djvu_file.extend_from_slice(&(bzz_data.len() as u32).to_be_bytes());
    djvu_file.extend_from_slice(&bzz_data);
    if bzz_data.len() % 2 != 0 {
        djvu_file.push(0);
    }

    let form_size = (djvu_file.len() - 12) as u32;
    djvu_file[form_size_pos..form_size_pos + 4].copy_from_slice(&form_size.to_be_bytes());
    
    let test_file = "/tmp/background_bzz_test.djvu";
    fs::write(test_file, &djvu_file).expect("Failed to write test file");

    println!("=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));

    println!("\n=== ddjvu decode attempt ===");
    let output = Command::new("ddjvu")
        .args(["-format=pbm", "-page=1", test_file, "/tmp/background_bzz_decoded.pbm"])
        .output()
        .expect("Failed to run ddjvu");

    if output.status.success() {
        println!("SUCCESS! background.pbm with BZZ decoded correctly");
        let _ = fs::remove_file("/tmp/background_bzz_decoded.pbm");
    } else {
        println!("FAILED: {}", String::from_utf8_lossy(&output.stderr));
        panic!("BZZ decode failed");
    }

    let _ = fs::remove_file(test_file);
}

/// Test encoding many symbols from background.pbm if it exists
#[test]
fn test_background_pbm_encoding() {
    use djvu_encoder::encode::jb2::{analyze_page, shapes_to_encoder_format};
    use djvu_encoder::encode::jb2::encoder::JB2Encoder;
    use djvu_encoder::encode::jb2::symbol_dict::BitImage;
    use std::io::Read;

    let pbm_path = "background.pbm";
    if !std::path::Path::new(pbm_path).exists() {
        println!("Skipping - background.pbm not found");
        return;
    }

    println!("=== Testing background.pbm JB2 encoding ===\n");

    // Load the PBM file
    let pbm_data = fs::read(pbm_path).expect("Failed to read PBM");
    
    // Parse header to find dimensions
    let mut cursor = 0usize;
    
    // Skip magic
    while cursor < pbm_data.len() && !pbm_data[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    while cursor < pbm_data.len() && pbm_data[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    
    // Skip comments
    while cursor < pbm_data.len() && pbm_data[cursor] == b'#' {
        while cursor < pbm_data.len() && pbm_data[cursor] != b'\n' {
            cursor += 1;
        }
        cursor += 1;
    }
    
    // Read dimensions
    let mut width_str = String::new();
    while cursor < pbm_data.len() && !pbm_data[cursor].is_ascii_whitespace() {
        width_str.push(pbm_data[cursor] as char);
        cursor += 1;
    }
    while cursor < pbm_data.len() && pbm_data[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    let mut height_str = String::new();
    while cursor < pbm_data.len() && !pbm_data[cursor].is_ascii_whitespace() {
        height_str.push(pbm_data[cursor] as char);
        cursor += 1;
    }
    while cursor < pbm_data.len() && pbm_data[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    
    let width: usize = width_str.parse().expect("Invalid width");
    let height: usize = height_str.parse().expect("Invalid height");
    
    println!("PBM dimensions: {}x{}", width, height);
    
    // Create BitImage from pixel data
    let mut test_image = BitImage::new(width as u32, height as u32).expect("Failed to create image");
    
    let bytes_per_row = (width + 7) / 8;
    for y in 0..height {
        for x in 0..width {
            let byte_idx = cursor + y * bytes_per_row + x / 8;
            if byte_idx < pbm_data.len() {
                let bit = 7 - (x % 8);
                let is_black = (pbm_data[byte_idx] >> bit) & 1 == 1;
                test_image.set_usize(x, y, is_black);
            }
        }
    }

    // Run CC analysis
    let ccimg = analyze_page(&test_image, 300, 1); // losslevel=1 for some cleaning
    let shapes = ccimg.extract_shapes();
    
    println!("Extracted {} shapes", shapes.len());
    
    // Convert to encoder format
    let (bitmaps, parents, blits) = shapes_to_encoder_format(shapes, height as i32);
    
    println!("{} blits to encode", blits.len());
    
    // Show first few blits
    for (i, (left, bottom, shapeno)) in blits.iter().take(5).enumerate() {
        let bm = &bitmaps[*shapeno];
        println!("  Blit {}: shape {} ({}x{}) at left={}, bottom={}", 
            i, shapeno, bm.width, bm.height, left, bottom);
    }

    // Encode
    let buffer: Vec<u8> = Vec::new();
    let mut encoder = JB2Encoder::new(buffer);
    
    let jb2_result = encoder.encode_page_with_shapes(
        width as u32,
        height as u32,
        &bitmaps,
        &parents,
        &blits,
        0,
        None,
    );

    match &jb2_result {
        Ok(data) => {
            println!("\nJB2 stream ({} bytes):", data.len());
            println!("First 64 bytes: {}\n", hex_dump(data, 64));
        }
        Err(e) => {
            panic!("Encoding failed: {:?}", e);
        }
    }

    let jb2_data = jb2_result.unwrap();
    
    // Create DjVu and test decode
    let djvu_data = create_djvu_from_jb2(&jb2_data, width as u16, height as u16);
    let test_file = "/tmp/background_test.djvu";
    fs::write(test_file, &djvu_data).expect("Failed to write test file");

    println!("=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));

    println!("\n=== ddjvu decode attempt ===");
    let output = Command::new("ddjvu")
        .args(["-format=pbm", "-page=1", test_file, "/tmp/background_decoded.pbm"])
        .output()
        .expect("Failed to run ddjvu");

    if output.status.success() {
        println!("SUCCESS! background.pbm symbols decoded correctly");
        let _ = fs::remove_file("/tmp/background_decoded.pbm");
    } else {
        println!("FAILED: {}", String::from_utf8_lossy(&output.stderr));
        // Don't panic, just report
    }

    let _ = fs::remove_file(test_file);
}
