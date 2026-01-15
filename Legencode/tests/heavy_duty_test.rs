use Legencode::color::binarization::binarize_image_raw;
use Legencode::types::BinarizationOptions;

#[test]
fn test_traditional_sauvola_binarization() {
    // Create a simple test image (128x128 RGB)
    let width = 128;
    let height = 128;
    let mut test_image = vec![0u8; width * height * 3];

    // Create a simple pattern (gradient with some text-like features)
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 3;
            let intensity = ((x + y) % 256) as u8;
            test_image[idx] = intensity; // R
            test_image[idx + 1] = intensity; // G
            test_image[idx + 2] = intensity; // B
        }
    }

    // Test traditional Sauvola
    let options = BinarizationOptions {
        use_heavy_duty: false,
        ..Default::default()
    };

    let result = binarize_image_raw(&test_image, width, height, &options);
    assert_eq!(result.len(), width * height);

    // Check that we get actual binary values (0 or 255)
    for &pixel in &result {
        assert!(pixel == 0 || pixel == 255);
    }

    println!("Traditional Sauvola binarization test passed");
}

#[test]
fn test_heavy_duty_availability() {
    // This test just checks if heavy-duty option can be set without panicking
    let options = BinarizationOptions {
        use_heavy_duty: true,
        patch_percentage: 10.0,
        ..Default::default()
    };

    // Just verify the options can be created
    assert!(options.use_heavy_duty);
    assert_eq!(options.patch_percentage, 10.0);

    println!("Heavy-duty options configuration test passed");
}

#[test]
#[ignore] // This test requires the ONNX model to be present
fn test_heavy_duty_sauvola_binarization() {
    // Create a simple test image (256x256 RGB) - minimum size for ONNX model
    let width = 256;
    let height = 256;
    let mut test_image = vec![0u8; width * height * 3];

    // Create a simple pattern
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 3;
            let intensity = ((x + y) % 256) as u8;
            test_image[idx] = intensity; // R
            test_image[idx + 1] = intensity; // G
            test_image[idx + 2] = intensity; // B
        }
    }

    // Test heavy-duty Sauvola with small patch size for testing
    let options = BinarizationOptions {
        use_heavy_duty: true,
        patch_percentage: 50.0, // Large patches for testing
        ..Default::default()
    };

    let result = binarize_image_raw(&test_image, width, height, &options);
    assert_eq!(result.len(), width * height);

    // Check that we get actual binary values (0 or 255)
    for &pixel in &result {
        assert!(pixel == 0 || pixel == 255);
    }

    println!("Heavy-duty Sauvola binarization test passed");
}
