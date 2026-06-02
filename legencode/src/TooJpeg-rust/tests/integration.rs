use std::fs::File;
use std::io::{self, Read, Write};
use toojpeg::{encode_jpeg, EncodeOptions, ImageFormat};

#[test]
fn test_encode_rgb_image() -> io::Result<()> {
    // Create a simple 2x2 RGB image (red, green, blue, white)
    let pixels = vec![
        255, 0, 0, // Red
        0, 255, 0, // Green
        0, 0, 255, // Blue
        255, 255, 255, // White
    ];

    let options = EncodeOptions {
        width: 2,
        height: 2,
        format: ImageFormat::RGB,
        quality: 90,
        ..Default::default()
    };

    let mut output = Vec::new();
    encode_jpeg(&pixels, options, &mut output).unwrap();

    // Basic validation of JPEG output
    assert!(output.len() > 100); // Should be at least 100 bytes
    assert_eq!(&output[0..2], [0xFF, 0xD8]); // JPEG SOI marker

    // Check for JFIF marker
    assert!(output.windows(2).any(|w| w == [0xFF, 0xE0]));

    // Save for manual inspection if needed
    let mut file = File::create("test_rgb.jpg")?;
    file.write_all(&output)?;

    Ok(())
}

#[test]
fn test_encode_rgba_image() -> io::Result<()> {
    // Create a simple 2x2 RGBA image (red, green, blue, white with alpha)
    let pixels = vec![
        255, 0, 0, 255, // Red
        0, 255, 0, 128, // Green with alpha
        0, 0, 255, 0, // Blue with alpha (fully transparent)
        255, 255, 255, 255, // White
    ];

    let options = EncodeOptions {
        width: 2,
        height: 2,
        format: ImageFormat::RGBA,
        quality: 90,
        ..Default::default()
    };

    let mut output = Vec::new();
    encode_jpeg(&pixels, options, &mut output).unwrap();

    // Basic validation of JPEG output
    assert!(output.len() > 100);
    assert_eq!(&output[0..2], [0xFF, 0xD8]);

    // Save for manual inspection if needed
    let mut file = File::create("test_rgba.jpg")?;
    file.write_all(&output)?;

    Ok(())
}

#[test]
fn test_encode_grayscale_image() -> io::Result<()> {
    // Create a simple 2x2 grayscale image (black, dark gray, light gray, white)
    let pixels = vec![
        0,   // Black
        85,  // Dark gray
        170, // Light gray
        255, // White
    ];

    let options = EncodeOptions {
        width: 2,
        height: 2,
        format: ImageFormat::Gray,
        quality: 90,
        ..Default::default()
    };

    let mut output = Vec::new();
    encode_jpeg(&pixels, options, &mut output).unwrap();

    // Basic validation of JPEG output
    assert!(output.len() > 50); // Grayscale should be smaller than RGB
    assert_eq!(&output[0..2], [0xFF, 0xD8]);

    // Save for manual inspection if needed
    let mut file = File::create("test_gray.jpg")?;
    file.write_all(&output)?;

    Ok(())
}

#[test]
fn test_encode_large_image() -> io::Result<()> {
    // Test with a larger image (16x16)
    let width = 16;
    let height = 16;
    let mut pixels = vec![0; width * height * 3];

    // Create a simple gradient pattern
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) as usize * 3;
            pixels[idx] = (x * 255 / (width - 1)) as u8; // R
            pixels[idx + 1] = (y * 255 / (height - 1)) as u8; // G
            pixels[idx + 2] = 128; // B
        }
    }

    let options = EncodeOptions {
        width: width as u32,
        height: height as u32,
        format: ImageFormat::RGB,
        quality: 85,
        ..Default::default()
    };

    let mut output = Vec::new();
    encode_jpeg(&pixels, options, &mut output).unwrap();

    // Basic validation
    assert!(output.len() > 500);
    assert_eq!(&output[0..2], [0xFF, 0xD8]);

    // Save for manual inspection if needed
    let mut file = File::create("test_large.jpg")?;
    file.write_all(&output)?;

    Ok(())
}

#[test]
fn test_invalid_dimensions() {
    let pixels = vec![0u8; 100]; // Not enough for 10x10 RGB

    let options = EncodeOptions {
        width: 10,
        height: 10,
        format: ImageFormat::RGB,
        ..Default::default()
    };

    let mut output = Vec::new();
    let result = encode_jpeg(&pixels, options, &mut output);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "Input buffer too small for specified dimensions and format"
    );
}

#[test]
fn test_quality_settings() -> io::Result<()> {
    let width = 32;
    let height = 32;
    let mut pixels = vec![0; width * height * 3];

    // Create a simple pattern
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) as usize * 3;
            pixels[idx] = (x * 255 / (width - 1)) as u8; // R gradient
            pixels[idx + 1] = (y * 255 / (height - 1)) as u8; // G gradient
            pixels[idx + 2] = ((x + y) * 255 / (width + height - 2)) as u8; // B gradient
        }
    }

    // Test different quality settings
    for &quality in &[10, 50, 90] {
        let options = EncodeOptions {
            width: width as u32,
            height: height as u32,
            format: ImageFormat::RGB,
            quality,
            ..Default::default()
        };

        let mut output = Vec::new();
        encode_jpeg(&pixels, options, &mut output).unwrap();

        // Lower quality should produce smaller files
        if quality > 10 {
            let prev_size = output.len();

            // Higher quality should produce larger files
            let options = EncodeOptions {
                quality: quality + 10,
                ..options
            };

            let mut output_higher = Vec::new();
            encode_jpeg(&pixels, options, &mut output_higher).unwrap();

            assert!(output_higher.len() >= prev_size);
        }
    }

    Ok(())
}
