//! Common utility functions for integration tests

use jbig2enc_rust as jbig2;
use jbig2enc_rust::jbig2sym::BitImage;
use std::io::{BufRead, BufReader, Read, Seek};

// Define the PBM file constants
pub const TEST_IMAGE_PBM: &str = "tests/fixtures/test_image.pbm";
pub const TEST_IMAGE1_PBM: &str = "tests/fixtures/test_image1.pbm";

/// Load a PBM file and convert to BitImage
pub fn load_pbm(path: &str) -> BitImage {
    let mut file = std::fs::File::open(path).expect("Failed to open PBM file");

    // Get file length first
    let file_len = file.metadata().unwrap().len();

    let mut reader = BufReader::new(&mut file);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "P4", "Only raw PBM (P4) supported");

    // Skip comments and get dimensions
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if !line.starts_with('#') && !line.trim().is_empty() {
            break;
        }
    }

    let dims: Vec<usize> = line
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(dims.len(), 2, "Invalid PBM dimensions");
    let (width, height) = (dims[0], dims[1]);

    // Read binary data
    let start_pos = reader.stream_position().unwrap();
    let data_len = file_len - start_pos;
    let bytes_per_row = (width + 7) / 8;
    let expected_data_len = bytes_per_row * height;
    assert_eq!(
        data_len as usize, expected_data_len,
        "Unexpected PBM data length"
    );

    let mut data = vec![0u8; expected_data_len];
    reader.read_exact(&mut data).unwrap();

    // Convert to BitImage (invert since PBM uses 1 for black, 0 for white)
    let mut img = BitImage::new(width as u32, height as u32).unwrap();
    for y in 0..height {
        for x in 0..width {
            let byte = data[y * bytes_per_row + x / 8];
            let bit = 7 - (x % 8);
            let is_black = (byte & (1 << bit)) != 0;
            img.set(x as u32, y as u32, is_black);
        }
    }
    img
}

/// Create a test pattern BitImage of 8x8 pixels
pub fn load_test_pbm() -> BitImage {
    let mut img = BitImage::new(8, 8).unwrap();
    for x in (0..8).step_by(2) {
        img.set(x, 0, true); // 0xAA
    }
    for x in (1..8).step_by(2) {
        img.set(x, 1, true); // 0x55
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbm_packing_row_major_msb_first() {
        let img = load_test_pbm();
        let packed = img.to_packed_words();
        assert_eq!(packed[0], 0xAA000000);
        assert_eq!(packed[1], 0x55000000);
    }
}
