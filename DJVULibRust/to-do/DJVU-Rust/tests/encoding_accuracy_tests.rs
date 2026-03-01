#[cfg(test)]
mod encoding_accuracy_tests {
    use djvu_encoder::doc::{PageEncodeParams, PageComponents};
    use djvu_encoder::utils::color_checker::{check_solid_color, RgbColor};
    use image::RgbImage;
    use std::fs;
    use std::process::Command;
    use std::path::Path;
    use tempfile::TempDir;

    /// Test that verifies the complete encoding/decoding pipeline produces accurate colors
    /// This test will fail if there are any YCbCr conversion or encoding issues
    #[test]
    fn test_encoding_decoding_color_accuracy() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let temp_path = temp_dir.path();

        println!("🧪 Testing complete DjVu encoding/decoding color accuracy pipeline...");
        
        // Test solid colors that are likely to reveal YCbCr conversion issues
        let test_cases = vec![
            (255, 0, 0, "red"),      // Pure red
            (0, 255, 0, "green"),    // Pure green  
            (0, 0, 255, "blue"),     // Pure blue
            (255, 255, 255, "white"), // White
            (0, 0, 0, "black"),      // Black
            (255, 255, 0, "yellow"), // Yellow (high chroma)
            (255, 0, 255, "magenta"), // Magenta (high chroma)
            (0, 255, 255, "cyan"),   // Cyan (high chroma)
        ];

        let mut all_passed = true;

        for (r, g, b, name) in test_cases {
            println!("\n📊 Testing {} RGB({}, {}, {})...", name, r, g, b);
            
            let success = test_single_color_roundtrip(
                temp_path, 
                r, g, b, 
                name,
                10,  // tolerance: allow small deviations due to compression
                85.0 // min_percentage: at least 85% of pixels should match
            );
            
            if !success {
                println!("❌ {} test FAILED", name);
                all_passed = false;
            } else {
                println!("✅ {} test PASSED", name);
            }
        }

        // Test a gradient to ensure smooth transitions work
        println!("\n📊 Testing gradient encoding...");
        let gradient_success = test_gradient_roundtrip(temp_path);
        if !gradient_success {
            println!("❌ Gradient test FAILED");
            all_passed = false;
        } else {
            println!("✅ Gradient test PASSED");
        }

        // Test a pattern to ensure sharp edges work
        println!("\n📊 Testing pattern encoding...");
        let pattern_success = test_pattern_roundtrip(temp_path);
        if !pattern_success {
            println!("❌ Pattern test FAILED");
            all_passed = false;
        } else {
            println!("✅ Pattern test PASSED");
        }

        if !all_passed {
            panic!("❌ One or more encoding accuracy tests failed! This indicates issues with the DjVu encoding pipeline.");
        }

        println!("\n🎉 All encoding accuracy tests PASSED! DjVu pipeline is working correctly.");
    }

    fn test_single_color_roundtrip(
        temp_path: &Path,
        r: u8, g: u8, b: u8,
        name: &str,
        tolerance: u32,
        min_percentage: f64
    ) -> bool {
        // Create a solid color image
        let width = 64;
        let height = 64;
        let mut rgb_image = RgbImage::new(width, height);
        
        for pixel in rgb_image.pixels_mut() {
            *pixel = image::Rgb([r, g, b]);
        }

        // Encode to DjVu
        let page_components = match PageComponents::new().with_background(rgb_image) {
            Ok(components) => components,
            Err(e) => {
                println!("  ❌ Failed to create page components: {}", e);
                return false;
            }
        };

        // Use high quality settings to minimize compression artifacts
        let params = PageEncodeParams {
            dpi: 300,
            bg_quality: 95,
            fg_quality: 95,
            use_iw44: true,
            color: true,
            decibels: Some(95.0),
        };

        let encoded_data = match page_components.encode(&params, 1, 1200, 1, Some(2.2)) {
            Ok(data) => data,
            Err(e) => {
                println!("  ❌ Failed to encode DjVu: {}", e);
                return false;
            }
        };

        println!("  📁 Encoded {} bytes", encoded_data.len());

        // Save DjVu file
        let djvu_path = temp_path.join(format!("test_{}.djvu", name));
        if let Err(e) = fs::write(&djvu_path, &encoded_data) {
            println!("  ❌ Failed to write DjVu file: {}", e);
            return false;
        }

        // Analyze DjVu structure
        analyze_djvu_structure(&djvu_path, name);

        // Decode using ddjvu
        let ppm_path = temp_path.join(format!("{}.ppm", name));
        if !decode_with_ddjvu(&djvu_path, &ppm_path) {
            println!("  ❌ Failed to decode DjVu file");
            return false;
        }

        // Check color accuracy
        let expected_color = RgbColor::new(r, g, b);
        match check_solid_color(&ppm_path, expected_color, tolerance, min_percentage) {
            Ok(true) => {
                println!("  ✅ Color accuracy verified");
                true
            },
            Ok(false) => {
                println!("  ❌ Color accuracy check failed");
                false
            },
            Err(e) => {
                println!("  ❌ Failed to check color accuracy: {}", e);
                false
            }
        }
    }

    fn test_gradient_roundtrip(temp_path: &Path) -> bool {
        // Create a horizontal gradient from black to white
        let width = 128;
        let height = 64;
        let mut rgb_image = RgbImage::new(width, height);
        
        for (x, _y, pixel) in rgb_image.enumerate_pixels_mut() {
            let gray_value = (x * 255 / (width - 1)) as u8;
            *pixel = image::Rgb([gray_value, gray_value, gray_value]);
        }

        // Encode to DjVu
        let page_components = match PageComponents::new().with_background(rgb_image) {
            Ok(components) => components,
            Err(e) => {
                println!("  ❌ Failed to create page components: {}", e);
                return false;
            }
        };

        let params = PageEncodeParams {
            dpi: 300,
            bg_quality: 95,
            fg_quality: 95,
            use_iw44: true,
            color: true,
            decibels: Some(95.0),
        };

        let encoded_data = match page_components.encode(&params, 1, 1200, 1, Some(2.2)) {
            Ok(data) => data,
            Err(e) => {
                println!("  ❌ Failed to encode DjVu: {}", e);
                return false;
            }
        };

        println!("  📁 Encoded {} bytes", encoded_data.len());

        // Save and decode
        let djvu_path = temp_path.join("test_gradient.djvu");
        if let Err(e) = fs::write(&djvu_path, &encoded_data) {
            println!("  ❌ Failed to write gradient DjVu file: {}", e);
            return false;
        }

        // Analyze DjVu structure
        analyze_djvu_structure(&djvu_path, "gradient");

        let ppm_path = temp_path.join("gradient.ppm");
        if !decode_with_ddjvu(&djvu_path, &ppm_path) {
            println!("  ❌ Failed to decode gradient DjVu file");
            return false;
        }

        // For gradients, we just check if the decode was successful
        // More sophisticated gradient analysis could be added here
        println!("  ✅ Gradient roundtrip successful");
        true
    }

    fn test_pattern_roundtrip(temp_path: &Path) -> bool {
        // Create a checkerboard pattern
        let width = 64;
        let height = 64;
        let mut rgb_image = RgbImage::new(width, height);
        
        for (x, y, pixel) in rgb_image.enumerate_pixels_mut() {
            let is_white = (x / 8 + y / 8) % 2 == 0;
            let color = if is_white { 255 } else { 0 };
            *pixel = image::Rgb([color, color, color]);
        }

        // Encode to DjVu
        let page_components = match PageComponents::new().with_background(rgb_image) {
            Ok(components) => components,
            Err(e) => {
                println!("  ❌ Failed to create page components: {}", e);
                return false;
            }
        };

        let params = PageEncodeParams {
            dpi: 300,
            bg_quality: 95,
            fg_quality: 95,
            use_iw44: true,
            color: true,
            decibels: Some(95.0),
        };

        let encoded_data = match page_components.encode(&params, 1, 1200, 1, Some(2.2)) {
            Ok(data) => data,
            Err(e) => {
                println!("  ❌ Failed to encode DjVu: {}", e);
                return false;
            }
        };

        println!("  📁 Encoded {} bytes", encoded_data.len());

        // Save and decode
        let djvu_path = temp_path.join("test_pattern.djvu");
        if let Err(e) = fs::write(&djvu_path, &encoded_data) {
            println!("  ❌ Failed to write pattern DjVu file: {}", e);
            return false;
        }

        // Analyze DjVu structure
        analyze_djvu_structure(&djvu_path, "pattern");

        let ppm_path = temp_path.join("pattern.ppm");
        if !decode_with_ddjvu(&djvu_path, &ppm_path) {
            println!("  ❌ Failed to decode pattern DjVu file");
            return false;
        }

        // For patterns, we just check if the decode was successful
        // More sophisticated pattern analysis could be added here
        println!("  ✅ Pattern roundtrip successful");
        true
    }

    /// Analyze DjVu file structure using djvudump and print JSON output
    fn analyze_djvu_structure(djvu_path: &Path, name: &str) {
        println!("  📋 Analyzing DjVu structure for {}...", name);
        
        let output = Command::new("./djvudump.exe")
            .arg("-j")
            .arg(djvu_path)
            .output();
        
        match output {
            Ok(result) => {
                if result.status.success() {
                    let json_output = String::from_utf8_lossy(&result.stdout);
                    println!("  🔍 DjVu structure JSON for {}:", name);
                    println!("  {}", json_output.trim());
                } else {
                    let error_output = String::from_utf8_lossy(&result.stderr);
                    println!("  ⚠️  djvudump failed for {}: {}", name, error_output.trim());
                }
            }
            Err(e) => {
                println!("  ⚠️  Failed to run djvudump for {}: {}", name, e);
            }
        }
    }

    fn decode_with_ddjvu(djvu_path: &Path, ppm_path: &Path) -> bool {
    let output = Command::new("./ddjvu.exe")
        .arg("-format=ppm")
        .arg("-page=1")
        .arg(djvu_path)
        .arg(ppm_path)
        .output();
        
        match output {
            Ok(result) => {
                if result.status.success() {
                    ppm_path.exists()
                } else {
                    let error_output = String::from_utf8_lossy(&result.stderr);
                    println!("  🔧 ddjvu error: {}", error_output.trim());
                    false
                }
            }
            Err(e) => {
                println!("  🔧 Failed to run ddjvu: {}", e);
                false
            }
        }
    }

}
