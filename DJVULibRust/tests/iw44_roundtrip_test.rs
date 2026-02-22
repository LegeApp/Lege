//! Test IW44 encoding roundtrip with C++ decoder
//!
//! This test encodes test2.png using Rust IW44 encoder and saves it to a file
//! that can be decoded with the C++ ddjvu tool.

use djvu_encoder::encode::iw44::encoder::{IWEncoder, EncoderParams, CrcbMode};
use djvu_encoder::iff::iff::IffWriter;
use image::RgbImage;
use std::fs;
use std::io::Cursor;
use std::io::Write;
use std::path::Path;

const TEST_PNG_PATH: &str = "test-files-misc/test2.png";
const OUTPUT_DJVU_PATH: &str = "target/test_iw44_roundtrip.djvu";

fn create_iw44_djvu(rgb_image: &RgbImage) -> Vec<u8> {
    let (width, height) = rgb_image.dimensions();
    
    let params = EncoderParams {
        decibels: None,
        slices: Some(74),
        bytes: None,
        crcb_mode: CrcbMode::Full,
        db_frac: 0.35,
        lossless: false,
        quant_multiplier: 1.0,
    };

    let mut encoder = IWEncoder::from_rgb(rgb_image, None, params)
        .expect("Failed to create IW44 encoder");

    // Collect all chunks
    let mut chunks = Vec::new();
    loop {
        let (chunk_data, more) = encoder.encode_chunk(74)
            .expect("Failed to encode chunk");
        
        if chunk_data.is_empty() {
            break;
        }
        
        println!("Encoded chunk: {} bytes", chunk_data.len());
        chunks.push(chunk_data);
        
        if !more {
            break;
        }
    }

    let mut djvu = Vec::new();
    {
        let mut cursor = Cursor::new(&mut djvu);
        let mut writer = IffWriter::new(&mut cursor);
        writer.write_magic_bytes().expect("write AT&T magic");
        writer.put_chunk("FORM:DJVU").expect("open FORM:DJVU");

        writer.put_chunk("INFO").expect("open INFO");
        writer
            .write_all(&(width as u16).to_be_bytes())
            .expect("write width");
        writer
            .write_all(&(height as u16).to_be_bytes())
            .expect("write height");
        writer.write_all(&[24]).expect("write minor");
        writer.write_all(&[0]).expect("write major");
        let dpi: u16 = 300;
        writer
            .write_all(&dpi.to_le_bytes())
            .expect("write dpi");
        writer.write_all(&[22]).expect("write gamma");
        writer.write_all(&[1]).expect("write flags");
        writer.close_chunk().expect("close INFO");

        for chunk in &chunks {
            writer.put_chunk("BG44").expect("open BG44");
            writer.write_all(chunk).expect("write BG44 payload");
            writer.close_chunk().expect("close BG44");
        }

        writer.close_chunk().expect("close FORM:DJVU");
    }

    djvu
}

#[test]
fn test_iw44_roundtrip_encoding() {
    println!("\n=== IW44 Roundtrip Encoding Test ===");
    
    let png_path = Path::new(TEST_PNG_PATH);
    if !png_path.exists() {
        println!("Skipping test: {} not found", TEST_PNG_PATH);
        return;
    }
    
    // Load image
    let img = image::open(png_path).expect("Failed to load PNG");
    let rgb_image = img.to_rgb8();
    println!("Loaded image: {}x{}", rgb_image.width(), rgb_image.height());
    
    // For faster testing, resize if too large
    let rgb_image = if rgb_image.width() > 256 || rgb_image.height() > 256 {
        println!("Resizing to 256x256 for faster test...");
        image::imageops::resize(&rgb_image, 256, 256, image::imageops::FilterType::Triangle)
    } else {
        rgb_image
    };
    println!("Test image: {}x{}", rgb_image.width(), rgb_image.height());
    
    // Encode
    let djvu_data = create_iw44_djvu(&rgb_image);
    println!("Created DJVU: {} bytes", djvu_data.len());
    
    // Write to file
    fs::create_dir_all("target").ok();
    fs::write(OUTPUT_DJVU_PATH, &djvu_data).expect("Failed to write DJVU file");
    println!("Written to: {}", OUTPUT_DJVU_PATH);
    
    // Verify basic structure
    assert!(!djvu_data.is_empty(), "DJVU data should not be empty");
    assert_eq!(&djvu_data[0..4], b"AT&T", "Should start with AT&T magic");
    assert_eq!(&djvu_data[4..8], b"FORM", "Should have FORM chunk");
    
    println!("\nTest completed. Run the following to decode:");
    println!("  D:\\tools\\djvulibre\\ddjvu.exe -format=ppm {} target\\test_decoded.ppm", OUTPUT_DJVU_PATH);
    println!("  D:\\tools\\djvulibre\\djvudump.exe {}", OUTPUT_DJVU_PATH);
}

fn main() {
    test_iw44_roundtrip_encoding();
}
