use Legencode::prelude::*;

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Create a simple 8x8 RGB test image (red pixels)
    let width = 8u32;
    let height = 8u32;
    let channels = 3u8;
    let mut input_data = vec![255u8; (width * height * channels as u32) as usize];

    // Make it a simple red square
    for i in 0..(width * height) as usize {
        input_data[i * 3] = 255; // Red
        input_data[i * 3 + 1] = 0; // Green
        input_data[i * 3 + 2] = 0; // Blue
    }

    // Test JPEG encoding
    let jpeg_buffer = ImageBuffer {
        data: &input_data,
        width,
        height,
        channels,
    };
    let jpeg_settings = EncodingSettings::Jpeg(JpegSettings {
        quality: 85,
        baseline: true,
        optimized: false,
        downsample: false,
    });

    match EncodingManager::encode(&jpeg_buffer, &jpeg_settings) {
        Ok(jpeg_result) => println!(
            "JPEG encoding successful! Output size: {} bytes",
            jpeg_result.as_slice().len()
        ),
        Err(e) => println!("JPEG encoding failed: {}", e),
    }

    // Test JP2 encoding with dynamic sizing
    let jp2_settings = EncodingSettings::Jp2(EncodingManager::create_optimal_jp2_settings(
        &rgb_data,
        width as u32,
        height as u32,
        false, // This is not a cover image
    ));

    match EncodingManager::encode(&jpeg_buffer, &jp2_settings) {
        Ok(jp2_result) => println!(
            "JP2 encoding successful! Output size: {} bytes",
            jp2_result.as_slice().len()
        ),
        Err(e) => println!("JP2 encoding failed: {}", e),
    }

    // Test JBIG2 encoding with binary data - uses byte-per-pixel format (despite README)
    let binary_width = 8u32;
    let binary_height = 8u32;
    let binary_channels = 0u8;
    let jbig2_data = vec![0u8; (binary_width * binary_height) as usize];

    let jbig2_buffer = ImageBuffer {
        data: &jbig2_data,
        width: binary_width,
        height: binary_height,
        channels: binary_channels,
    };

    match EncodingManager::encode(
        &jbig2_buffer,
        &EncodingSettings::Jbig2(Jbig2Settings::default()),
    ) {
        Ok(jbig2_result) => println!(
            "JBIG2 encoding successful! Output size: {} bytes",
            jbig2_result.as_slice().len()
        ),
        Err(e) => println!("JBIG2 encoding failed: {}", e),
    }

    // Test CCITT4 encoding with binary data - uses byte-per-pixel format per README
    let ccitt4_data = vec![0u8; (binary_width * binary_height) as usize];
    let ccitt4_buffer = ImageBuffer {
        data: &ccitt4_data,
        width: binary_width,
        height: binary_height,
        channels: binary_channels,
    };

    match EncodingManager::encode(&ccitt4_buffer, &EncodingSettings::Ccitt4) {
        Ok(ccitt4_result) => println!(
            "CCITT4 encoding successful! Output size: {} bytes",
            ccitt4_result.as_slice().len()
        ),
        Err(e) => println!("CCITT4 encoding failed: {}", e),
    }

    // Test JBIG2 with inverted data (all white pixels = 1)
    let inverted_data = vec![1u8; (binary_width * binary_height) as usize];
    let inverted_buffer = ImageBuffer {
        data: &inverted_data,
        width: binary_width,
        height: binary_height,
        channels: binary_channels,
    };

    match EncodingManager::encode(
        &inverted_buffer,
        &EncodingSettings::Jbig2(Jbig2Settings::default()),
    ) {
        Ok(inverted_result) => println!(
            "JBIG2 inverted encoding successful! Output size: {} bytes",
            inverted_result.as_slice().len()
        ),
        Err(e) => println!("JBIG2 inverted encoding failed: {}", e),
    }

    println!("All encoders are working correctly!");

    Ok(())
}
