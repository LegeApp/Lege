//! Test multiple NEW_MARK records to ensure relative location encoding works with multiple blits

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

#[test]
fn test_two_symbols_same_row() {
    use djvu_encoder::encode::jb2::encoder::JB2Encoder;
    use djvu_encoder::encode::jb2::symbol_dict::BitImage;

    println!("=== Testing TWO NEW_MARK records on same row ===\n");

    let page_width = 20u32;
    let page_height = 10u32;

    // Create a 3x3 symbol (filled square)
    let mut symbol = BitImage::new(3, 3).expect("Failed to create symbol");
    for y in 0..3 {
        for x in 0..3 {
            symbol.set_usize(x, y, true);
        }
    }

    // Two blits: both on same row (similar y, left progressing right)
    // First at (1, 6), second at (10, 6) - same row in DjVu coords
    let shapes = vec![symbol.clone()];
    let parents = vec![-1i32];
    let blits = vec![
        (1i32, 6i32, 0usize),  // First symbol
        (10i32, 6i32, 0usize), // Second symbol, same row (should trigger MATCHED_COPY with same-row relative)
    ];

    let buffer: Vec<u8> = Vec::new();
    let mut encoder = JB2Encoder::new(buffer);

    let jb2_result = encoder.encode_page_with_shapes(
        page_width,
        page_height,
        &shapes,
        &parents,
        &blits,
        0,
        None,
    );

    match &jb2_result {
        Ok(data) => {
            println!("Rust JB2 stream ({} bytes):", data.len());
            println!("{}\n", hex_dump(data, 64));
        }
        Err(e) => {
            println!("Rust encoder failed: {:?}", e);
            panic!("Encoding failed");
        }
    }

    let jb2_data = jb2_result.unwrap();

    let djvu_data = create_djvu_from_jb2(&jb2_data, page_width as u16, page_height as u16);
    let test_file = "/tmp/two_symbols_same_row.djvu";
    fs::write(test_file, &djvu_data).expect("Failed to write test file");

    println!("=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));

    println!("\n=== ddjvu decode attempt ===");
    let output = Command::new("ddjvu")
        .args(["-format=pbm", "-page=1", test_file, "/tmp/two_symbols_decoded.pbm"])
        .output()
        .expect("Failed to run ddjvu");

    if output.status.success() {
        println!("SUCCESS! Two symbols decoded correctly");
        let _ = fs::remove_file("/tmp/two_symbols_decoded.pbm");
    } else {
        println!("FAILED: {}", String::from_utf8_lossy(&output.stderr));
        panic!("Decode failed");
    }

    let _ = fs::remove_file(test_file);
}

#[test]
fn test_two_symbols_different_rows() {
    use djvu_encoder::encode::jb2::encoder::JB2Encoder;
    use djvu_encoder::encode::jb2::symbol_dict::BitImage;

    println!("=== Testing TWO NEW_MARK records on different rows ===\n");

    let page_width = 20u32;
    let page_height = 20u32;

    // Create a 3x3 symbol (filled square)
    let mut symbol = BitImage::new(3, 3).expect("Failed to create symbol");
    for y in 0..3 {
        for x in 0..3 {
            symbol.set_usize(x, y, true);
        }
    }

    // Two blits on different rows
    // First at (10, 14), second at (5, 5) - new row (left < last_left)
    let shapes = vec![symbol.clone()];
    let parents = vec![-1i32];
    let blits = vec![
        (10i32, 14i32, 0usize), // First symbol at bottom-right area
        (5i32, 5i32, 0usize),   // Second symbol, new row (left < last_left)
    ];

    let buffer: Vec<u8> = Vec::new();
    let mut encoder = JB2Encoder::new(buffer);

    let jb2_result = encoder.encode_page_with_shapes(
        page_width,
        page_height,
        &shapes,
        &parents,
        &blits,
        0,
        None,
    );

    match &jb2_result {
        Ok(data) => {
            println!("Rust JB2 stream ({} bytes):", data.len());
            println!("{}\n", hex_dump(data, 64));
        }
        Err(e) => {
            println!("Rust encoder failed: {:?}", e);
            panic!("Encoding failed");
        }
    }

    let jb2_data = jb2_result.unwrap();

    let djvu_data = create_djvu_from_jb2(&jb2_data, page_width as u16, page_height as u16);
    let test_file = "/tmp/two_symbols_diff_row.djvu";
    fs::write(test_file, &djvu_data).expect("Failed to write test file");

    println!("=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));

    println!("\n=== ddjvu decode attempt ===");
    let output = Command::new("ddjvu")
        .args(["-format=pbm", "-page=1", test_file, "/tmp/two_symbols_diff_decoded.pbm"])
        .output()
        .expect("Failed to run ddjvu");

    if output.status.success() {
        println!("SUCCESS! Two symbols on different rows decoded correctly");
        let _ = fs::remove_file("/tmp/two_symbols_diff_decoded.pbm");
    } else {
        println!("FAILED: {}", String::from_utf8_lossy(&output.stderr));
        panic!("Decode failed");
    }

    let _ = fs::remove_file(test_file);
}
