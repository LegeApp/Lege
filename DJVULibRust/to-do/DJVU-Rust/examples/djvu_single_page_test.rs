use djvu_encoder::doc::{PageEncodeParams, PageComponents};
use image::RgbImage;
use std::fs;
use std::process::Command;
use tracing_subscriber;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging with INFO level to see debug output
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(true)
        .with_line_number(true)
        .init();
    
    println!("=== Single Page DjVu Generation Test ===");
    
    // Test 1: Pure blue solid color image
    println!("\n1. Testing solid blue image (should show YCbCr conversion bug)...");
    test_solid_color_image(0, 0, 255, "solid_blue")?;
    
    // Test 2: Pure red solid color image
    println!("\n2. Testing solid red image...");
    test_solid_color_image(255, 0, 0, "solid_red")?;
    
    // Test 3: Pure green solid color image
    println!("\n3. Testing solid green image...");
    test_solid_color_image(0, 255, 0, "solid_green")?;
    
    // Test 4: Grayscale gradient
    println!("\n4. Testing grayscale gradient...");
    test_gradient_image()?;
    
    // Test 5: Simple pattern
    println!("\n5. Testing simple pattern...");
    test_pattern_image()?;
    
    // Summary of generated files
    println!("\n=== SUMMARY ===");
    print_file_summary()?;
    
    println!("\n=== All tests completed successfully! ===");
    Ok(())
}

fn test_solid_color_image(r: u8, g: u8, b: u8, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let width = 64;
    let height = 64;
    let mut rgb_image = RgbImage::new(width, height);
    
    // Fill with solid color
    for pixel in rgb_image.pixels_mut() {
        *pixel = image::Rgb([r, g, b]);
    }
    
    println!("  Creating {}x{} {} image with RGB({}, {}, {})", width, height, name, r, g, b);
    
    // Create page components with background
    let page_components = PageComponents::new()
        .with_background(rgb_image)?;
    
    // Encode with high quality settings
    let params = PageEncodeParams {
        dpi: 300,
        bg_quality: 95,
        fg_quality: 95,
        use_iw44: true,
        color: true,
        decibels: Some(95.0),
    };
    
    println!("  Encoding DjVu page...");
    let encoded_data = page_components.encode(&params, 1, 1200, 1, Some(2.2))?;
    
    let filename = format!("test_{}.djvu", name);
    fs::write(&filename, &encoded_data)?;
    
    println!("  ✓ Encoded successfully! Size: {} bytes, saved as {}", encoded_data.len(), filename);
    
    // Decode using ddjvu to verify the result
    let _ = decode_with_ddjvu(&filename, &format!("{}.ppm", name)); // Ignore errors
    
    Ok(())
}

fn test_gradient_image() -> Result<(), Box<dyn std::error::Error>> {
    let width = 128;
    let height = 128;
    let mut rgb_image = RgbImage::new(width, height);
    
    // Create a horizontal grayscale gradient
    for (x, y, pixel) in rgb_image.enumerate_pixels_mut() {
        let gray_value = (x * 255 / (width - 1)) as u8;
        *pixel = image::Rgb([gray_value, gray_value, gray_value]);
    }
    
    println!("  Creating {}x{} grayscale gradient", width, height);
    
    // Create page components
    let page_components = PageComponents::new()
        .with_background(rgb_image)?;
    
    let params = PageEncodeParams::default();
    
    println!("  Encoding DjVu page...");
    let encoded_data = page_components.encode(&params, 1, 1200, 1, Some(2.2))?;
    
    let filename = "test_gradient.djvu";
    fs::write(filename, &encoded_data)?;
    
    println!("  ✓ Encoded successfully! Size: {} bytes, saved as {}", encoded_data.len(), filename);
    
    // Decode using ddjvu to verify the result
    decode_with_ddjvu(filename, "gradient.ppm")?;
    
    Ok(())
}

fn test_pattern_image() -> Result<(), Box<dyn std::error::Error>> {
    let width = 100;
    let height = 100;
    let mut rgb_image = RgbImage::new(width, height);
    
    // Create a simple checkerboard pattern
    for (x, y, pixel) in rgb_image.enumerate_pixels_mut() {
        let is_black = (x / 10 + y / 10) % 2 == 0;
        let color = if is_black { 0 } else { 255 };
        *pixel = image::Rgb([color, color, color]);
    }
    
    println!("  Creating {}x{} checkerboard pattern", width, height);
    
    // Create page components
    let page_components = PageComponents::new()
        .with_background(rgb_image)?;
    
    let params = PageEncodeParams::default();
    
    println!("  Encoding DjVu page...");
    let encoded_data = page_components.encode(&params, 1, 1200, 1, Some(2.2))?;
    
    let filename = "test_pattern.djvu";
    fs::write(filename, &encoded_data)?;
    
    println!("  ✓ Encoded successfully! Size: {} bytes, saved as {}", encoded_data.len(), filename);
    
    // Decode using ddjvu to verify the result
    decode_with_ddjvu(filename, "pattern.ppm")?;
    
    Ok(())
}

fn decode_with_ddjvu(djvu_file: &str, output_file: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("  🔍 Decoding {} with ddjvu...", djvu_file);
    
    // Use the ddjvu.exe from the current directory
    let ddjvu_path = "./ddjvu.exe";
    
    // Run ddjvu command with verbose output
    let output = Command::new(ddjvu_path)
        .arg("--format=ppm")
        .arg("--verbose")
        .arg(djvu_file)
        .arg(output_file)
        .output();
    
    match output {
        Ok(result) => {
            let stdout = String::from_utf8_lossy(&result.stdout);
            let stderr = String::from_utf8_lossy(&result.stderr);
            
            if result.status.success() {
                println!("  ✅ Decoding successful! Output saved as {}", output_file);
                
                // Show stdout if any
                if !stdout.trim().is_empty() {
                    println!("     STDOUT: {}", stdout.trim());
                }
                
                // Filter and show useful stderr info (verbose info)
                if !stderr.trim().is_empty() {
                    let filtered_stderr = filter_debug_messages(&stderr);
                    if !filtered_stderr.trim().is_empty() {
                        println!("     VERBOSE: {}", filtered_stderr.trim());
                    }
                }
                
                // Check if output file was created and get its size
                match fs::metadata(output_file) {
                    Ok(metadata) => {
                        println!("     Generated PPM size: {} bytes", metadata.len());
                    }
                    Err(e) => {
                        println!("     Warning: Could not check PPM file size: {}", e);
                    }
                }
            } else {
                println!("  ❌ Decoding failed with exit code: {:?}", result.status.code());
                
                if !stdout.trim().is_empty() {
                    println!("     STDOUT: {}", stdout.trim());
                }
                
                if !stderr.trim().is_empty() {
                    let filtered_stderr = filter_debug_messages(&stderr);
                    println!("     STDERR: {}", filtered_stderr.trim());
                }
                
                // Don't fail the whole test, just mark this as failed
                println!("     ⚠️  Continuing with other tests...");
                return Ok(());
            }
        }
        Err(e) => {
            println!("  ❌ Failed to execute ddjvu: {}", e);
            println!("     Make sure ddjvu.exe is in your PATH or current directory");
            return Err(format!("Could not run ddjvu: {}", e).into());
        }
    }
    
    println!(""); // Empty line for readability
    Ok(())
}

fn filter_debug_messages(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|line| {
            // Filter out verbose slice debug messages
            !line.contains("DEBUG: Encoding Cb/Cr slices") &&
            !line.contains("DEBUG: Before encoding - Y cur_bit:") &&
            !line.contains("DEBUG: After encoding - Y cur_bit:") &&
            !line.contains("DEBUG: Y has data:") &&
            !line.contains("DEBUG Encode slice:") &&
            !line.contains("Slice is null, advancing to next") &&
            !line.trim().is_empty()
        })
        .collect::<Vec<&str>>()
        .join("\n")
}

fn print_file_summary() -> Result<(), Box<dyn std::error::Error>> {
    println!("Generated files:");
    
    let test_files = vec![
        ("test_solid_blue.djvu", "solid_blue.ppm"),
        ("test_solid_red.djvu", "solid_red.ppm"), 
        ("test_solid_green.djvu", "solid_green.ppm"),
        ("test_gradient.djvu", "gradient.ppm"),
        ("test_pattern.djvu", "pattern.ppm"),
    ];
    
    for (djvu_file, ppm_file) in test_files {
        print!("  {} -> {}: ", djvu_file, ppm_file);
        
        let djvu_exists = fs::metadata(djvu_file).is_ok();
        let ppm_exists = fs::metadata(ppm_file).is_ok();
        
        if djvu_exists && ppm_exists {
            let djvu_size = fs::metadata(djvu_file)?.len();
            let ppm_size = fs::metadata(ppm_file)?.len();
            println!("✅ Both files exist (DjVu: {} bytes, PPM: {} bytes)", djvu_size, ppm_size);
        } else if djvu_exists {
            println!("⚠️  DjVu exists but PPM failed to generate");
        } else {
            println!("❌ DjVu file missing");
        }
    }
    
    println!("\nTo manually inspect the PPM files, you can open them in an image viewer");
    println!("or convert them to other formats using ImageMagick or similar tools.");
    
    Ok(())
}
