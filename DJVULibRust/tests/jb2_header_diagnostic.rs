//! Diagnostic test to analyze JB2 START_OF_DATA encoding
//!
//! This test generates a minimal JB2 stream and compares it byte-by-byte
//! with a reference from DjVuLibre's cjb2 tool.

use std::fs;
use std::process::Command;

/// Parse JB2 header bytes and decode START_OF_DATA record
/// Returns (width, height) if successful
fn decode_jb2_start_of_data(jb2_data: &[u8]) -> Result<(u32, u32), String> {
    // JB2 uses ZP-coder arithmetic coding, so we need to decode it
    // This is a simplified decoder just for the START_OF_DATA record

    // For now, let's use djvudump to parse and extract info
    // Actually, we can use djvused or djvudump

    // Let's create a minimal DjVu with this JB2 and use djvuinfo
    Ok((0, 0)) // placeholder
}

/// Extract raw Sjbz chunk from a DjVu file
fn extract_sjbz_chunk(djvu_path: &str) -> Result<Vec<u8>, String> {
    // Use djvuextract to get the raw Sjbz data
    let output = Command::new("djvuextract")
        .args([djvu_path, "Sjbz=sjbz_temp.jb2"])
        .output()
        .map_err(|e| format!("Failed to run djvuextract: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "djvuextract failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let data = fs::read("sjbz_temp.jb2").map_err(|e| format!("Failed to read extracted JB2: {}", e))?;
    let _ = fs::remove_file("sjbz_temp.jb2");
    Ok(data)
}

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

#[test]
fn test_compare_jb2_headers() {
    // Step 1: Create a small test PBM image (10x10)
    let test_pbm = "/tmp/test_jb2_header.pbm";
    let test_ref_djvu = "/tmp/test_jb2_ref.djvu";
    let test_rust_djvu = "/tmp/test_jb2_rust.djvu";

    // Create a simple 10x10 PBM
    let pbm_content = b"P4\n10 10\n\xff\xc0\xff\xc0\xff\xc0\xff\xc0\xff\xc0\xff\xc0\xff\xc0\xff\xc0\xff\xc0\xff\xc0";
    fs::write(test_pbm, pbm_content).expect("Failed to write test PBM");

    // Step 2: Generate reference DjVu with cjb2
    println!("=== Generating reference DjVu with cjb2 ===");
    let output = Command::new("cjb2")
        .args(["-clean", test_pbm, test_ref_djvu])
        .output()
        .expect("Failed to run cjb2");

    if !output.status.success() {
        eprintln!(
            "cjb2 failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        panic!("cjb2 failed to generate reference");
    }

    // Step 3: Dump reference DjVu info
    println!("\n=== Reference DjVu (from cjb2) ===");
    let output = Command::new("djvudump")
        .arg(test_ref_djvu)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));

    // Step 4: Extract Sjbz from reference
    println!("\n=== Extracting Sjbz from reference ===");
    let ref_sjbz = extract_sjbz_chunk(test_ref_djvu);
    match &ref_sjbz {
        Ok(data) => {
            println!("Reference Sjbz size: {} bytes", data.len());
            println!("First 64 bytes:\n{}", hex_dump(data, 64));
        }
        Err(e) => {
            println!("Failed to extract reference Sjbz: {}", e);
        }
    }

    // Step 5: Now generate our own DjVu using the Rust encoder
    println!("\n=== Generating Rust DjVu ===");

    // Load the same PBM we created
    let pbm_data = fs::read(test_pbm).expect("Failed to read test PBM");

    // Parse PBM - P4 format (binary)
    let pbm_str = String::from_utf8_lossy(&pbm_data);
    let lines: Vec<&str> = pbm_str.lines().collect();

    // Parse dimensions
    let dims: Vec<u32> = lines[1]
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    let width = dims[0];
    let height = dims[1];
    println!("PBM dimensions: {}x{}", width, height);

    // Create a BitImage from the PBM data
    use djvu_encoder::encode::jb2::symbol_dict::BitImage;

    // Find where binary data starts (after header)
    let header_end = pbm_data
        .windows(2)
        .position(|w| w[0] == b'\n' && w.iter().any(|&b| b > 0x39 || b < 0x20))
        .map(|p| p + 1)
        .unwrap_or(0);

    // Actually, for P4 format, data starts after the dimensions line
    let mut pos = 0;
    let mut newlines = 0;
    for (i, &b) in pbm_data.iter().enumerate() {
        if b == b'\n' {
            newlines += 1;
            if newlines == 2 {
                pos = i + 1;
                break;
            }
        }
    }

    let pixel_data = &pbm_data[pos..];
    println!("Pixel data starts at byte {}, length {}", pos, pixel_data.len());

    // Convert P4 (packed bits) to BitImage
    let mut bit_image = BitImage::new(width, height).expect("Failed to create BitImage");
    let bytes_per_row = (width as usize + 7) / 8;

    for y in 0..height as usize {
        for x in 0..width as usize {
            let byte_idx = y * bytes_per_row + x / 8;
            let bit_idx = 7 - (x % 8);
            if byte_idx < pixel_data.len() {
                let is_black = (pixel_data[byte_idx] >> bit_idx) & 1 != 0;
                bit_image.set_usize(x, y, is_black);
            }
        }
    }

    // Step 6: Generate JB2 stream using our encoder
    use djvu_encoder::encode::jb2::encoder::JB2Encoder;

    let buffer: Vec<u8> = Vec::new();
    let mut encoder = JB2Encoder::new(buffer);

    let jb2_result = encoder.encode_single_page(&bit_image);
    match &jb2_result {
        Ok(data) => {
            println!("\nRust JB2 stream size: {} bytes", data.len());
            println!("First 64 bytes:\n{}", hex_dump(data, 64));
        }
        Err(e) => {
            println!("Rust encoder failed: {:?}", e);
        }
    }

    // Step 7: Build a complete DjVu file with the Rust JB2 stream
    if let Ok(jb2_data) = jb2_result {
        // Create minimal DjVu structure
        // FORM:DJVU { INFO, Sjbz }

        let mut djvu_file: Vec<u8> = Vec::new();

        // Placeholder for outer FORM (we'll fill size later)
        djvu_file.extend_from_slice(b"AT&TFORM");
        let form_size_pos = djvu_file.len();
        djvu_file.extend_from_slice(&[0, 0, 0, 0]); // placeholder
        djvu_file.extend_from_slice(b"DJVU");

        // INFO chunk (10 bytes) - format per DjVu spec:
        // Width(2), Height(2), Minor(1), Major(1), DPI(2, LE), Gamma(1), Flags(1)
        djvu_file.extend_from_slice(b"INFO");
        djvu_file.extend_from_slice(&10u32.to_be_bytes());
        djvu_file.extend_from_slice(&(width as u16).to_be_bytes()); // width (BE)
        djvu_file.extend_from_slice(&(height as u16).to_be_bytes()); // height (BE)
        djvu_file.push(24); // minor version
        djvu_file.push(0);  // major version
        djvu_file.extend_from_slice(&300u16.to_le_bytes()); // dpi (LE!)
        djvu_file.push(22); // gamma * 10
        djvu_file.push(1);  // flags

        // Sjbz chunk
        djvu_file.extend_from_slice(b"Sjbz");
        djvu_file.extend_from_slice(&(jb2_data.len() as u32).to_be_bytes());
        djvu_file.extend_from_slice(&jb2_data);

        // Pad if odd
        if jb2_data.len() % 2 != 0 {
            djvu_file.push(0);
        }

        // Fix FORM size (total - 8 for "AT&TFORM")
        let form_size = (djvu_file.len() - 12) as u32; // -12 for "AT&TFORM" + 4-byte size
        djvu_file[form_size_pos..form_size_pos + 4].copy_from_slice(&form_size.to_be_bytes());

        // Write out
        fs::write(test_rust_djvu, &djvu_file).expect("Failed to write Rust DjVu");

        println!("\n=== Rust DjVu structure ===");
        let output = Command::new("djvudump")
            .arg(test_rust_djvu)
            .output()
            .expect("Failed to run djvudump");
        println!("{}", String::from_utf8_lossy(&output.stdout));

        // Try to decode
        println!("\n=== Attempting ddjvu decode ===");
        let output = Command::new("ddjvu")
            .args(["-format=pbm", "-page=1", test_rust_djvu, "/tmp/rust_decoded.pbm"])
            .output()
            .expect("Failed to run ddjvu");

        if output.status.success() {
            println!("SUCCESS! ddjvu decoded the Rust-generated DjVu");
        } else {
            println!(
                "FAILED: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    // Cleanup
    let _ = fs::remove_file(test_pbm);
    let _ = fs::remove_file(test_ref_djvu);
    let _ = fs::remove_file(test_rust_djvu);
    let _ = fs::remove_file("/tmp/rust_decoded.pbm");
}

#[test]
fn test_minimal_jb2_stream() {
    // Create the absolute minimal JB2 stream:
    // START_OF_DATA(width=10, height=10, no_refine) + END_OF_DATA

    use djvu_encoder::encode::jb2::num_coder::NumCoder;
    use djvu_encoder::encode::zc::ZEncoder;

    println!("=== Testing minimal JB2 stream encoding ===");

    let mut buffer: Vec<u8> = Vec::new();
    let mut zc = ZEncoder::new(&mut buffer, true).expect("Failed to create ZEncoder");
    let mut num_coder = NumCoder::new();

    // Allocate contexts
    let mut dist_record_type: u32 = 0;
    let mut image_size_dist: u32 = 0;
    let mut dist_refinement_flag: u8 = 0;

    // Constants
    const START_OF_DATA: i32 = 0;
    const END_OF_DATA: i32 = 11;
    const BIG_POSITIVE: i32 = 262_142;

    // Encode START_OF_DATA (record type 0)
    println!("Encoding record type 0 (START_OF_DATA)...");
    num_coder
        .code_num(&mut zc, &mut dist_record_type, START_OF_DATA, END_OF_DATA, START_OF_DATA)
        .expect("Failed to encode record type");

    // Encode width = 10
    println!("Encoding width = 10...");
    num_coder
        .code_num(&mut zc, &mut image_size_dist, 0, BIG_POSITIVE, 10)
        .expect("Failed to encode width");

    // Encode height = 10
    println!("Encoding height = 10...");
    num_coder
        .code_num(&mut zc, &mut image_size_dist, 0, BIG_POSITIVE, 10)
        .expect("Failed to encode height");

    // Encode refinement flag = false
    println!("Encoding refinement flag = false...");
    zc.encode(false, &mut dist_refinement_flag)
        .expect("Failed to encode refinement flag");

    // Encode END_OF_DATA (record type 11)
    println!("Encoding record type 11 (END_OF_DATA)...");
    num_coder
        .code_num(&mut zc, &mut dist_record_type, START_OF_DATA, END_OF_DATA, END_OF_DATA)
        .expect("Failed to encode end record type");

    // Finish encoding
    let buffer = zc.finish().expect("Failed to finish encoder");

    println!("\nMinimal JB2 stream ({} bytes):", buffer.len());
    println!("{}", hex_dump(&buffer, 64));

    // Now wrap this in a DjVu file and try to decode
    let test_file = "/tmp/minimal_jb2_test.djvu";

    let mut djvu_file: Vec<u8> = Vec::new();
    djvu_file.extend_from_slice(b"AT&TFORM");
    let form_size_pos = djvu_file.len();
    djvu_file.extend_from_slice(&[0, 0, 0, 0]);
    djvu_file.extend_from_slice(b"DJVU");

    // INFO chunk (10 bytes) - format per DjVu spec:
    // Width(2), Height(2), Minor(1), Major(1), DPI(2, LE), Gamma(1), Flags(1)
    djvu_file.extend_from_slice(b"INFO");
    djvu_file.extend_from_slice(&10u32.to_be_bytes());
    djvu_file.extend_from_slice(&10u16.to_be_bytes()); // width (BE)
    djvu_file.extend_from_slice(&10u16.to_be_bytes()); // height (BE)
    djvu_file.push(24); // minor version
    djvu_file.push(0);  // major version
    djvu_file.extend_from_slice(&300u16.to_le_bytes()); // dpi (LE!)
    djvu_file.push(22); // gamma * 10
    djvu_file.push(1);  // flags

    // Sjbz chunk
    djvu_file.extend_from_slice(b"Sjbz");
    djvu_file.extend_from_slice(&(buffer.len() as u32).to_be_bytes());
    djvu_file.extend_from_slice(&buffer);
    if buffer.len() % 2 != 0 {
        djvu_file.push(0);
    }

    let form_size = (djvu_file.len() - 12) as u32;
    djvu_file[form_size_pos..form_size_pos + 4].copy_from_slice(&form_size.to_be_bytes());

    fs::write(test_file, &djvu_file).expect("Failed to write test file");

    // Try djvudump
    println!("\n=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Try ddjvu
    println!("\n=== ddjvu decode attempt ===");
    let output = Command::new("ddjvu")
        .args(["-format=pbm", "-page=1", test_file, "/tmp/minimal_decoded.pbm"])
        .output()
        .expect("Failed to run ddjvu");

    if output.status.success() {
        println!("SUCCESS! Minimal JB2 stream decoded correctly");
        let _ = fs::remove_file("/tmp/minimal_decoded.pbm");
    } else {
        println!("FAILED: {}", String::from_utf8_lossy(&output.stderr));
    }

    let _ = fs::remove_file(test_file);
}
