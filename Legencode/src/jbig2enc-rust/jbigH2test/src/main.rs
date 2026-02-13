mod jbig2arith;
mod jbig2shared;
mod jbig2sym;

use std::time::Instant;
use crate::jbig2sym::BitImage;
use crate::jbig2arith::Jbig2ArithCoder;

#[test]
fn test_generic_region() -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    
    // A simple 4x4 bitmap pattern from the original test.
    let bitmap: [u32; 4] = [
        0b10000000_00000000_00000000_00000000, // 0x80000000
        0b11000000_00000000_00000000_00000000, // 0xC0000000
        0b01100000_00000000_00000000_00000000, // 0x60000000
        0b00110000_00000000_00000000_00000000, // 0x30000000
    ];
    let width = 4;
    let height = 4;
    let template = 0;
    let at_pixels: Vec<(i8, i8)> = vec![]; // No adaptive pixels

    // The expected output from the original test.
    let expected_region_output = [
        0xE8, 0x63, 0xFF, 0xFF, 0xAC
    ];

    println!("Starting generic region encoding test...");

    // Create the BitImage from the bitmap pattern.
    let mut img = BitImage::new(width as u32, height as u32)?;
    for y in 0..height {
        for x in 0..width {
            let word = bitmap[y];
            let bit = (word >> (31 - x)) & 1;
            img.set(x as u32, y as u32, bit == 1);
        }
    }

    // Encode the generic region.
    // Note: encode_generic_payload is a static method.
    let encoded = Jbig2ArithCoder::encode_generic_payload(&img, template, &at_pixels)
        .expect("Generic region encoding failed");

    println!("Final encoded data ({} bytes): {:?}", encoded.len(), encoded);
    
    // Verify the output matches the expected vector.
    assert_eq!(encoded, &expected_region_output[..], "Generic region arithmetic coder output does not match expected vector");
    
    println!("Test passed!");

    let duration = start.elapsed();
    println!("Test test_generic_region took: {:?}", duration);
    Ok(())
}

#[test]
fn test_arithmetic_coder_annex_h2() {
    let start = Instant::now();
    
    // Input test data from JBIG2 Annex H.2
    let test_data = [
        0x00, 0x02, 0x00, 0x51, 0x00, 0x00, 0x00, 0xC0, 
        0x03, 0x52, 0x87, 0x2A, 0xAA, 0xAA, 0xAA, 0xAA,
        0x82, 0xC0, 0x20, 0x00, 0xFC, 0xD7, 0x9E, 0xF6, 
        0xBF, 0x7F, 0xED, 0x90, 0x4F, 0x46, 0xA3, 0xBF
    ];

    // Expected output from JBIG2 Annex H.2
    let expected_output = [
        0x84, 0xC7, 0x3B, 0xFC, 0xE1, 0xA1, 0x43, 0x04, 0x02, 0x20, 0x00, 0x00, 0x41, 0x0D,
        0xBB, 0x86, 0xF4, 0x31, 0x7F, 0xFF, 0x88, 0xFF, 0x37, 0x47, 0x1A, 0xDB, 0x6A, 0xDF,
    ];

    // Initialize coder with single context
    let mut coder = Jbig2ArithCoder::new();
    
    // Encode the test data using a single context
    let cx = 0;
    for (_, &byte) in test_data.iter().enumerate() {
        for bit in 0..8 {
            let bit_val = (byte >> (7 - bit)) & 1 != 0;
            coder.encode_bit(cx, bit_val);
        }
    }
    
    // Flush the encoder to finalize the encoding
    coder.flush(false);
    
    // Get the encoded data
    let encoded = coder.as_bytes();
    
    // Compare with expected output
    assert_eq!(encoded, &expected_output[..], "Arithmetic coder output does not match JBIG2 Annex H.2 test vector");
    
    let duration = start.elapsed();
    println!("Test test_arithmetic_coder_annex_h2 took: {:?}", duration);
}