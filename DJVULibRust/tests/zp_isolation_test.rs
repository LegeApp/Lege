//! Isolated test for ZP encoder to debug "Unexpected End Of File" errors

use std::io::Cursor;

#[test]
fn test_zp_encoder_basic() {
    use djvu_encoder::encode::zc::zcodec::ZEncoder;

    // Create encoder
    let buffer = Vec::new();
    let mut encoder =
        ZEncoder::new(Cursor::new(buffer), true).expect("Failed to create ZP encoder");

    let mut ctx: u8 = 0;

    // Encode a simple pattern
    for i in 0..100 {
        encoder.encode(i % 2 == 0, &mut ctx).expect("Encode failed");
    }

    // Finish and get output
    let output = encoder.finish().expect("Finish failed");
    let data = output.into_inner();

    println!("ZP encoded {} bits -> {} bytes", 100, data.len());
    println!("Data: {:02X?}", &data);

    // The output should not be empty
    assert!(!data.is_empty(), "ZP output should not be empty");
}

#[test]
fn test_zp_encoder_iwencoder() {
    use djvu_encoder::encode::zc::zcodec::ZEncoder;

    // Create encoder
    let buffer = Vec::new();
    let mut encoder =
        ZEncoder::new(Cursor::new(buffer), true).expect("Failed to create ZP encoder");

    // Test IWencoder path (used for IW44 coefficient encoding)
    for i in 0..100 {
        encoder.IWencoder(i % 3 == 0).expect("IWencoder failed");
    }

    // Finish and get output
    let output = encoder.finish().expect("Finish failed");
    let data = output.into_inner();

    println!("IW44-mode ZP encoded {} bits -> {} bytes", 100, data.len());
    println!("Data: {:02X?}", &data);

    assert!(!data.is_empty(), "ZP output should not be empty");
}

#[test]
fn test_zp_encoder_larger_data() {
    use djvu_encoder::encode::zc::zcodec::ZEncoder;

    let buffer = Vec::new();
    let mut encoder =
        ZEncoder::new(Cursor::new(buffer), true).expect("Failed to create ZP encoder");

    let mut ctx: u8 = 0;

    // Encode a larger pattern (simulating actual image data)
    for i in 0..10000 {
        let bit = (i % 7) < 3; // Some pattern
        encoder.encode(bit, &mut ctx).expect("Encode failed");
    }

    let output = encoder.finish().expect("Finish failed");
    let data = output.into_inner();

    println!("ZP encoded 10000 bits -> {} bytes", data.len());
    println!("First 32 bytes: {:02X?}", &data[..data.len().min(32)]);
    println!(
        "Last 32 bytes: {:02X?}",
        &data[data.len().saturating_sub(32)..]
    );

    assert!(!data.is_empty(), "ZP output should not be empty");
    // Rough compression ratio check - should be less than 10000/8 = 1250 bytes
    // due to compression
    println!(
        "Compression ratio: {:.2}x",
        10000.0 / 8.0 / data.len() as f64
    );
}
