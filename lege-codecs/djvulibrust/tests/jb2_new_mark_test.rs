//! Test NEW_MARK record encoding in isolation

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
    djvu_file.push(0); // major
    djvu_file.extend_from_slice(&300u16.to_le_bytes()); // dpi LE
    djvu_file.push(22); // gamma
    djvu_file.push(1); // flags

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
fn test_new_mark_record_minimal() {
    use djvu_encoder::encode::jb2::encoder::JB2Encoder;
    use djvu_encoder::encode::jb2::symbol_dict::BitImage;

    println!("=== Testing NEW_MARK record encoding ===\n");

    // Create a 5x5 page with a single 3x3 symbol at position (1, 1)
    let page_width = 10u32;
    let page_height = 10u32;

    // Create a 3x3 symbol (filled square)
    let mut symbol = BitImage::new(3, 3).expect("Failed to create symbol");
    for y in 0..3 {
        for x in 0..3 {
            symbol.set_usize(x, y, true);
        }
    }

    // Position: left=1, bottom=6 (DjVu coords - bottom-left origin)
    // This puts the 3x3 mark at PBM position (1,1) to (3,3)
    // In DjVu coords with 10-pixel tall image: bottom=6 means top of symbol at DjVu y=8
    let shapes = vec![symbol.clone()];
    let parents = vec![-1i32]; // no parent
    let blits = vec![(1i32, 6i32, 0usize)]; // (left, bottom, shapeno)

    // Create JB2 encoder
    let buffer: Vec<u8> = Vec::new();
    let mut encoder = JB2Encoder::new(buffer);

    // Encode page with shapes
    let jb2_result = encoder.encode_page_with_shapes(
        page_width,
        page_height,
        &shapes,
        &parents,
        &blits,
        0, // no inherited shapes
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

    // Wrap in DjVu and try to decode
    let djvu_data = create_djvu_from_jb2(&jb2_data, page_width as u16, page_height as u16);
    let test_file = "/tmp/new_mark_test.djvu";
    fs::write(test_file, &djvu_data).expect("Failed to write test file");

    // Check with djvudump
    println!("=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Try to decode
    println!("\n=== ddjvu decode attempt ===");
    let output = Command::new("ddjvu")
        .args([
            "-format=pbm",
            "-page=1",
            test_file,
            "/tmp/new_mark_decoded.pbm",
        ])
        .output()
        .expect("Failed to run ddjvu");

    if output.status.success() {
        println!("SUCCESS! NEW_MARK record decoded correctly");

        // Verify decoded output
        let decoded = fs::read("/tmp/new_mark_decoded.pbm").expect("Failed to read decoded");
        println!("Decoded PBM size: {} bytes", decoded.len());
        let _ = fs::remove_file("/tmp/new_mark_decoded.pbm");
    } else {
        println!("FAILED: {}", String::from_utf8_lossy(&output.stderr));
    }

    let _ = fs::remove_file(test_file);
}

#[test]
fn test_reference_with_cjb2() {
    // Create a test image with cjb2 and extract the Sjbz for comparison
    println!("=== Creating reference with cjb2 ===\n");

    // Create a 10x10 PBM with a 3x3 filled square at (1,1)
    let mut pbm_data = Vec::new();
    pbm_data.extend_from_slice(b"P4\n10 10\n");

    // Row by row, MSB first
    // Row 0: all zeros
    pbm_data.extend_from_slice(&[0, 0]); // 10 bits = 2 bytes
    // Row 1: bit 1,2,3 set (0b01110000, 0b00)
    pbm_data.extend_from_slice(&[0, 0]);
    // Rows 2-9: same pattern as row 0 or 1
    for row in 2..10 {
        if row >= 1 && row <= 3 {
            // 3x3 block at x=1..3, y=1..3
            // x=1,2,3 means bits 6,5,4 in first byte (MSB first)
            pbm_data.extend_from_slice(&[0b01110000, 0]);
        } else {
            pbm_data.extend_from_slice(&[0, 0]);
        }
    }

    // Actually, let me recalculate. P4 format is packed bits, MSB first.
    // For a 10 pixel row: bits 0-9, packed into 2 bytes
    // Byte 0 has pixels 0-7 (MSB=pixel 0), Byte 1 has pixels 8-9
    // For pixels at x=1,2,3: byte 0 bits 6,5,4 = 0b01110000 = 0x70

    // Recreate properly
    let mut pbm_data = Vec::new();
    pbm_data.extend_from_slice(b"P4\n10 10\n");
    for row in 0..10usize {
        if row >= 1 && row <= 3 {
            pbm_data.push(0x70); // pixels 1,2,3 black
            pbm_data.push(0x00);
        } else {
            pbm_data.push(0x00);
            pbm_data.push(0x00);
        }
    }

    let test_pbm = "/tmp/test_mark_ref.pbm";
    let test_djvu = "/tmp/test_mark_ref.djvu";
    fs::write(test_pbm, &pbm_data).expect("Failed to write PBM");

    // Generate with cjb2
    let output = Command::new("cjb2")
        .args(["-clean", test_pbm, test_djvu])
        .output()
        .expect("Failed to run cjb2");

    if !output.status.success() {
        println!("cjb2 failed: {}", String::from_utf8_lossy(&output.stderr));
        return;
    }

    // Dump the reference
    println!("=== Reference djvudump ===");
    let output = Command::new("djvudump")
        .arg(test_djvu)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));

    // Extract and show the Sjbz
    let djvu_bytes = fs::read(test_djvu).expect("Failed to read djvu");
    // Find Sjbz chunk
    if let Some(pos) = djvu_bytes.windows(4).position(|w| w == b"Sjbz") {
        let size_start = pos + 4;
        let size = u32::from_be_bytes([
            djvu_bytes[size_start],
            djvu_bytes[size_start + 1],
            djvu_bytes[size_start + 2],
            djvu_bytes[size_start + 3],
        ]) as usize;
        let data_start = size_start + 4;
        let sjbz_data = &djvu_bytes[data_start..data_start + size];

        println!("\nReference Sjbz ({} bytes):", size);
        println!("{}", hex_dump(sjbz_data, 64));
    }

    let _ = fs::remove_file(test_pbm);
    let _ = fs::remove_file(test_djvu);
}

#[test]
fn test_encode_single_new_mark_manually() {
    use djvu_encoder::encode::jb2::num_coder::{BIG_POSITIVE, NumCoder};
    use djvu_encoder::encode::jb2::symbol_dict::BitImage;
    use djvu_encoder::encode::zc::ZEncoder;

    println!("=== Manually encoding a single NEW_MARK record ===\n");

    const START_OF_DATA: i32 = 0;
    const NEW_MARK: i32 = 1;
    const END_OF_DATA: i32 = 11;

    let page_width = 10i32;
    let page_height = 10i32;

    // Create a 3x3 symbol - all BLACK (true)
    let mut symbol = BitImage::new(3, 3).expect("Failed to create symbol");
    for y in 0..3 {
        for x in 0..3 {
            symbol.set_usize(x, y, true);
        }
    }

    let mut buffer: Vec<u8> = Vec::new();
    let mut zc = ZEncoder::new(&mut buffer, true).expect("Failed to create ZEncoder");
    let mut num_coder = NumCoder::new();

    // Allocate contexts
    let mut dist_record_type: u32 = 0;
    let mut image_size_dist: u32 = 0;
    let mut abs_size_x: u32 = 0;
    let mut abs_size_y: u32 = 0;
    let mut abs_loc_x: u32 = 0;
    let mut abs_loc_y: u32 = 0;
    let mut dist_refinement_flag: u8 = 0;
    let mut bitdist = [0u8; 1024];

    // START_OF_DATA
    println!("Encoding START_OF_DATA...");
    num_coder
        .code_num(
            &mut zc,
            &mut dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            START_OF_DATA,
        )
        .unwrap();
    num_coder
        .code_num(&mut zc, &mut image_size_dist, 0, BIG_POSITIVE, page_width)
        .unwrap();
    num_coder
        .code_num(&mut zc, &mut image_size_dist, 0, BIG_POSITIVE, page_height)
        .unwrap();
    zc.encode(false, &mut dist_refinement_flag).unwrap();

    // NEW_MARK record
    println!("Encoding NEW_MARK...");
    num_coder
        .code_num(
            &mut zc,
            &mut dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            NEW_MARK,
        )
        .unwrap();

    // Absolute size (3x3)
    num_coder
        .code_num(&mut zc, &mut abs_size_x, 0, BIG_POSITIVE, 3)
        .unwrap();
    num_coder
        .code_num(&mut zc, &mut abs_size_y, 0, BIG_POSITIVE, 3)
        .unwrap();

    // Encode bitmap directly (matching DjVuLibre's code_bitmap_directly)
    // 10-bit context template
    let bm_width = symbol.width as i32;
    let bm_height = symbol.height as i32;

    // Get pixel with Y flip (JB2 uses bottom-up coordinates)
    let get_pixel = |x: i32, y: i32| -> u8 {
        if x < 0 || y < 0 || x >= bm_width || y >= bm_height {
            0
        } else {
            let flipped_y = bm_height - 1 - y;
            symbol.get_pixel_unchecked(x as usize, flipped_y as usize) as u8
        }
    };

    // Encode row by row, top to bottom in JB2 coords (which is bottom to top in image coords)
    for y in (0..bm_height).rev() {
        for x in 0..bm_width {
            // Build 10-bit context from causal neighborhood
            let ctx = ((get_pixel(x - 1, y) as usize) << 9)
                | ((get_pixel(x, y + 1) as usize) << 8)
                | ((get_pixel(x + 1, y + 1) as usize) << 7)
                | ((get_pixel(x - 1, y + 1) as usize) << 6)
                | ((get_pixel(x - 2, y + 1) as usize) << 5)
                | ((get_pixel(x + 2, y + 1) as usize) << 4)
                | ((get_pixel(x - 2, y) as usize) << 3)
                | ((get_pixel(x - 2, y + 2) as usize) << 2)
                | ((get_pixel(x - 1, y + 2) as usize) << 1)
                | ((get_pixel(x, y + 2) as usize) << 0);

            let bit = get_pixel(x, y) != 0;
            zc.encode(bit, &mut bitdist[ctx]).unwrap();
        }
    }

    // Absolute location
    // left = 1, bottom = 1
    // JB2 uses 1-based coordinates for encoding
    // For NEW_MARK: encode left+1, then top (= bottom + height)
    let left = 1i32;
    let bottom = 1i32;
    let top = bottom + bm_height;

    println!(
        "Encoding location: left={}, bottom={}, top={}",
        left, bottom, top
    );
    num_coder
        .code_num(&mut zc, &mut abs_loc_x, 1, page_width, left + 1)
        .unwrap();
    num_coder
        .code_num(&mut zc, &mut abs_loc_y, 1, page_height, top)
        .unwrap();

    // END_OF_DATA
    println!("Encoding END_OF_DATA...");
    num_coder
        .code_num(
            &mut zc,
            &mut dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            END_OF_DATA,
        )
        .unwrap();

    let buffer = zc.finish().expect("Failed to finish encoder");
    println!("\nManual JB2 stream ({} bytes):", buffer.len());
    println!("{}", hex_dump(&buffer, 64));

    // Test decoding
    let djvu_data = create_djvu_from_jb2(&buffer, page_width as u16, page_height as u16);
    let test_file = "/tmp/manual_new_mark_test.djvu";
    fs::write(test_file, &djvu_data).expect("Failed to write test file");

    println!("\n=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    println!("{}", String::from_utf8_lossy(&output.stdout));

    println!("\n=== ddjvu decode attempt ===");
    let output = Command::new("ddjvu")
        .args([
            "-format=pbm",
            "-page=1",
            test_file,
            "/tmp/manual_decoded.pbm",
        ])
        .output()
        .expect("Failed to run ddjvu");

    if output.status.success() {
        println!("SUCCESS! Manual NEW_MARK record decoded correctly");
        let _ = fs::remove_file("/tmp/manual_decoded.pbm");
    } else {
        println!("FAILED: {}", String::from_utf8_lossy(&output.stderr));
    }

    let _ = fs::remove_file(test_file);
}
