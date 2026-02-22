//! Test to verify reset behavior in JB2 encoding

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
fn test_minimal_without_reset() {
    use djvu_encoder::encode::jb2::num_coder::{NumCoder, BIG_POSITIVE};
    use djvu_encoder::encode::zc::ZEncoder;

    println!("=== Testing WITHOUT reset ===\n");

    const START_OF_DATA: i32 = 0;
    const END_OF_DATA: i32 = 11;

    let mut buffer: Vec<u8> = Vec::new();
    let mut zc = ZEncoder::new(&mut buffer, true).expect("Failed to create ZEncoder");
    let mut num_coder = NumCoder::new();

    // NO RESET

    let mut dist_record_type: u32 = 0;
    let mut image_size_dist: u32 = 0;
    let mut dist_refinement_flag: u8 = 0;

    // START_OF_DATA
    num_coder.code_num(&mut zc, &mut dist_record_type, START_OF_DATA, END_OF_DATA, START_OF_DATA).unwrap();
    num_coder.code_num(&mut zc, &mut image_size_dist, 0, BIG_POSITIVE, 10).unwrap();
    num_coder.code_num(&mut zc, &mut image_size_dist, 0, BIG_POSITIVE, 10).unwrap();
    zc.encode(false, &mut dist_refinement_flag).unwrap();

    // END_OF_DATA
    num_coder.code_num(&mut zc, &mut dist_record_type, START_OF_DATA, END_OF_DATA, END_OF_DATA).unwrap();

    let buffer = zc.finish().expect("Failed to finish encoder");
    println!("WITHOUT reset ({} bytes): {}", buffer.len(), hex_dump(&buffer, 64));

    // Test
    let djvu_data = create_djvu_from_jb2(&buffer, 10, 10);
    fs::write("/tmp/test_no_reset.djvu", &djvu_data).unwrap();
    let output = Command::new("ddjvu")
        .args(["-format=pbm", "/tmp/test_no_reset.djvu", "/tmp/test_no_reset.pbm"])
        .output()
        .unwrap();
    if output.status.success() {
        println!("WITHOUT reset: SUCCESS");
    } else {
        println!("WITHOUT reset: FAILED - {}", String::from_utf8_lossy(&output.stderr));
    }
    let _ = fs::remove_file("/tmp/test_no_reset.djvu");
    let _ = fs::remove_file("/tmp/test_no_reset.pbm");
}

#[test]
fn test_minimal_with_reset() {
    use djvu_encoder::encode::jb2::num_coder::{NumCoder, BIG_POSITIVE};
    use djvu_encoder::encode::zc::ZEncoder;

    println!("=== Testing WITH reset ===\n");

    const START_OF_DATA: i32 = 0;
    const END_OF_DATA: i32 = 11;

    let mut buffer: Vec<u8> = Vec::new();
    let mut zc = ZEncoder::new(&mut buffer, true).expect("Failed to create ZEncoder");
    let mut num_coder = NumCoder::new();

    // RESET CALLED
    num_coder.reset();

    let mut dist_record_type: u32 = 0;
    let mut image_size_dist: u32 = 0;
    let mut dist_refinement_flag: u8 = 0;

    // START_OF_DATA
    num_coder.code_num(&mut zc, &mut dist_record_type, START_OF_DATA, END_OF_DATA, START_OF_DATA).unwrap();
    num_coder.code_num(&mut zc, &mut image_size_dist, 0, BIG_POSITIVE, 10).unwrap();
    num_coder.code_num(&mut zc, &mut image_size_dist, 0, BIG_POSITIVE, 10).unwrap();
    zc.encode(false, &mut dist_refinement_flag).unwrap();

    // END_OF_DATA
    num_coder.code_num(&mut zc, &mut dist_record_type, START_OF_DATA, END_OF_DATA, END_OF_DATA).unwrap();

    let buffer = zc.finish().expect("Failed to finish encoder");
    println!("WITH reset ({} bytes): {}", buffer.len(), hex_dump(&buffer, 64));

    // Test
    let djvu_data = create_djvu_from_jb2(&buffer, 10, 10);
    fs::write("/tmp/test_with_reset.djvu", &djvu_data).unwrap();
    let output = Command::new("ddjvu")
        .args(["-format=pbm", "/tmp/test_with_reset.djvu", "/tmp/test_with_reset.pbm"])
        .output()
        .unwrap();
    if output.status.success() {
        println!("WITH reset: SUCCESS");
    } else {
        println!("WITH reset: FAILED - {}", String::from_utf8_lossy(&output.stderr));
    }
    let _ = fs::remove_file("/tmp/test_with_reset.djvu");
    let _ = fs::remove_file("/tmp/test_with_reset.pbm");
}

#[test]
fn test_minimal_with_fresh_contexts() {
    use djvu_encoder::encode::jb2::num_coder::{NumCoder, BIG_POSITIVE};
    use djvu_encoder::encode::zc::ZEncoder;

    println!("=== Testing with fresh NumContext variables (but NumCoder reset) ===\n");

    const START_OF_DATA: i32 = 0;
    const END_OF_DATA: i32 = 11;

    let mut buffer: Vec<u8> = Vec::new();
    let mut zc = ZEncoder::new(&mut buffer, true).expect("Failed to create ZEncoder");
    let mut num_coder = NumCoder::new();

    // This simulates what encode_page_with_shapes does
    num_coder.reset();

    // Fresh contexts initialized to 0
    let mut dist_record_type: u32 = 0;
    let mut image_size_dist: u32 = 0;
    let mut dist_refinement_flag: u8 = 0;

    // START_OF_DATA
    num_coder.code_num(&mut zc, &mut dist_record_type, START_OF_DATA, END_OF_DATA, START_OF_DATA).unwrap();
    num_coder.code_num(&mut zc, &mut image_size_dist, 0, BIG_POSITIVE, 10).unwrap();
    num_coder.code_num(&mut zc, &mut image_size_dist, 0, BIG_POSITIVE, 10).unwrap();
    zc.encode(false, &mut dist_refinement_flag).unwrap();

    // END_OF_DATA
    num_coder.code_num(&mut zc, &mut dist_record_type, START_OF_DATA, END_OF_DATA, END_OF_DATA).unwrap();

    let buffer = zc.finish().expect("Failed to finish encoder");
    println!("Fresh contexts ({} bytes): {}", buffer.len(), hex_dump(&buffer, 64));

    // This SHOULD work if the issue is only reset
    let djvu_data = create_djvu_from_jb2(&buffer, 10, 10);
    fs::write("/tmp/test_fresh.djvu", &djvu_data).unwrap();
    let output = Command::new("ddjvu")
        .args(["-format=pbm", "/tmp/test_fresh.djvu", "/tmp/test_fresh.pbm"])
        .output()
        .unwrap();
    if output.status.success() {
        println!("Fresh contexts: SUCCESS");
    } else {
        println!("Fresh contexts: FAILED - {}", String::from_utf8_lossy(&output.stderr));
    }
    let _ = fs::remove_file("/tmp/test_fresh.djvu");
    let _ = fs::remove_file("/tmp/test_fresh.pbm");
}
