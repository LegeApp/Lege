//! Comprehensive test suite for comparing JBIG2 lossless vs symbol mode encoding
//!
//! This test suite provides detailed comparisons between:
//! - Lossless mode (no symbol dictionaries)
//! - Symbol mode (with symbol dictionaries)
//!
//! Including performance metrics, compression ratios, and correctness validation.

use jbig2enc_rust as jbig2;
use std::fmt;
use std::time::Instant;

// Import the common test utilities
mod common;
use common::{load_pbm, TEST_IMAGE1_PBM, TEST_IMAGE_PBM};

use jbig2::jbig2sym::BitImage;
use jbig2::{encode_single_image, encode_single_image_lossless, Jbig2EncodeResult};

/// Configuration for test mode selection
#[derive(Debug, Clone, Copy)]
enum TestMode {
    LosslessOnly,
    SymbolOnly,
    Both,
}

/// Statistics for a single encoding test
#[derive(Debug, Clone)]
struct EncodingStats {
    mode_name: String,
    input_size: usize,
    output_size: usize,
    global_dict_size: Option<usize>,
    total_output_size: usize,
    compression_ratio: f64,
    encoding_time_ms: u128,
    success: bool,
    error_message: Option<String>,
}

impl EncodingStats {
    fn new(mode_name: String, input_size: usize) -> Self {
        Self {
            mode_name,
            input_size,
            output_size: 0,
            global_dict_size: None,
            total_output_size: 0,
            compression_ratio: 0.0,
            encoding_time_ms: 0,
            success: false,
            error_message: None,
        }
    }

    fn mark_success(&mut self, result: &Jbig2EncodeResult, encoding_time: u128) {
        self.output_size = result.page_data.len();
        self.global_dict_size = result.global_data.as_ref().map(|d| d.len());
        self.total_output_size = self.output_size + self.global_dict_size.unwrap_or(0);
        self.compression_ratio = self.input_size as f64 / self.total_output_size as f64;
        self.encoding_time_ms = encoding_time;
        self.success = true;
    }

    fn mark_failure(&mut self, error: &str, encoding_time: u128) {
        self.encoding_time_ms = encoding_time;
        self.success = false;
        self.error_message = Some(error.to_string());
    }
}

impl fmt::Display for EncodingStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.success {
            writeln!(f, "=== {} Results ===", self.mode_name)?;
            writeln!(f, "  Input size:         {} bytes", self.input_size)?;
            writeln!(f, "  Page data size:     {} bytes", self.output_size)?;
            if let Some(dict_size) = self.global_dict_size {
                writeln!(f, "  Global dict size:   {} bytes", dict_size)?;
            }
            writeln!(f, "  Total output size:  {} bytes", self.total_output_size)?;
            writeln!(f, "  Compression ratio:  {:.2}x", self.compression_ratio)?;
            writeln!(f, "  Encoding time:      {} ms", self.encoding_time_ms)?;
            writeln!(f, "  Status:             ✓ SUCCESS")?;
        } else {
            writeln!(f, "=== {} Results ===", self.mode_name)?;
            writeln!(f, "  Input size:         {} bytes", self.input_size)?;
            writeln!(f, "  Encoding time:      {} ms", self.encoding_time_ms)?;
            writeln!(f, "  Status:             ✗ FAILED")?;
            if let Some(error) = &self.error_message {
                writeln!(f, "  Error:              {}", error)?;
            }
        }
        Ok(())
    }
}

/// Comparison results between different modes
#[derive(Debug)]
struct ComparisonResults {
    lossless: Option<EncodingStats>,
    symbol: Option<EncodingStats>,
}

impl ComparisonResults {
    fn new() -> Self {
        Self {
            lossless: None,
            symbol: None,
        }
    }

    fn print_comparison(&self) {
        println!("\n{}", "=".repeat(80));
        println!("                    JBIG2 ENCODING MODE COMPARISON");
        println!("{}", "=".repeat(80));

        if let Some(ref lossless) = self.lossless {
            print!("{}", lossless);
        }

        if let Some(ref symbol) = self.symbol {
            print!("{}", symbol);
        }

        // Print comparison if both modes were tested
        if let (Some(ref lossless), Some(ref symbol)) = (&self.lossless, &self.symbol) {
            if lossless.success && symbol.success {
                println!("=== Comparison ===");

                let size_diff = symbol.total_output_size as i64 - lossless.total_output_size as i64;
                let size_percent = (size_diff as f64 / lossless.total_output_size as f64) * 100.0;

                let time_diff = symbol.encoding_time_ms as i64 - lossless.encoding_time_ms as i64;
                let time_percent = (time_diff as f64 / lossless.encoding_time_ms as f64) * 100.0;

                let compression_diff = symbol.compression_ratio - lossless.compression_ratio;

                println!(
                    "  Size difference:    {} bytes ({:+.1}%)",
                    size_diff, size_percent
                );
                println!(
                    "  Time difference:    {} ms ({:+.1}%)",
                    time_diff, time_percent
                );
                println!("  Compression diff:   {:+.2}x", compression_diff);

                if symbol.total_output_size < lossless.total_output_size {
                    println!("  Winner (size):      Symbol mode");
                } else if symbol.total_output_size > lossless.total_output_size {
                    println!("  Winner (size):      Lossless mode");
                } else {
                    println!("  Winner (size):      Tie");
                }

                if symbol.encoding_time_ms < lossless.encoding_time_ms {
                    println!("  Winner (speed):     Symbol mode");
                } else if symbol.encoding_time_ms > lossless.encoding_time_ms {
                    println!("  Winner (speed):     Lossless mode");
                } else {
                    println!("  Winner (speed):     Tie");
                }
            }
        }

        println!("{}", "=".repeat(80));
    }
}

/// Convert a BitImage to the binary format expected by the encode functions
fn bitimage_to_binary(img: &BitImage) -> Vec<u8> {
    let mut binary = Vec::with_capacity(img.width as usize * img.height as usize);

    for y in 0..img.height {
        for x in 0..img.width {
            binary.push(if img.get(x as u32, y as u32) { 1 } else { 0 });
        }
    }

    binary
}

/// Test lossless mode encoding
fn test_lossless_mode(input: &[u8], width: u32, height: u32) -> EncodingStats {
    let mut stats = EncodingStats::new("Lossless Mode".to_string(), input.len());

    let start = Instant::now();

    match encode_single_image_lossless(input, width, height, false) {
        Ok(result) => {
            let elapsed = start.elapsed().as_millis();
            stats.mark_success(&result, elapsed);
            println!("✓ Lossless encoding completed in {} ms", elapsed);

            // Save the JBIG2 output to file for decoding verification
            let output_file = "test_output_lossless.jb2";
            if let Err(e) = std::fs::write(output_file, &result.page_data) {
                println!("⚠️  Warning: Could not save lossless output: {}", e);
            } else {
                println!(
                    "📁 Saved lossless output to: {} ({} bytes)",
                    output_file,
                    result.page_data.len()
                );

                // Test decoding with jbig2dec (standalone JBIG2 files)
                let mut cmd = jbig2dec_command();
                match cmd
                    .args(&["-o", "decoded_lossless.pbm", output_file])
                    .output()
                {
                    Ok(output) => {
                        if output.status.success() {
                            println!("✅ Lossless JBIG2 successfully decoded with jbig2dec");
                        } else {
                            println!(
                                "❌ Lossless JBIG2 decode failed: {}",
                                String::from_utf8_lossy(&output.stderr)
                            );
                        }
                    }
                    Err(e) => {
                        println!("⚠️  Could not run jbig2dec: {}", e);
                    }
                }
            }
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis();
            stats.mark_failure(&e.to_string(), elapsed);
            println!("✗ Lossless encoding failed: {}", e);
        }
    }

    stats
}

/// Test symbol mode encoding
fn test_symbol_mode(input: &[u8], width: u32, height: u32) -> EncodingStats {
    let mut stats = EncodingStats::new("Symbol Mode".to_string(), input.len());

    let start = Instant::now();

    // DEBUG-ONLY PROBE: PDF mode fragments are NOT a valid standalone JBIG2 file.
    // We inspect sizes; optional concatenation guarded by SAVE_PDF_PROBE env var.
    println!("🔧 DEBUG: Inspecting PDF-mode fragment sizes (not a standalone file)");
    match encode_single_image(input, width, height, true) {
        Ok(pdf_result) => {
            if let Some(ref dict) = pdf_result.global_data {
                println!(
                    "🔧 PDF mode: global_data = {} bytes, page_data = {} bytes",
                    dict.len(),
                    pdf_result.page_data.len()
                );

                if std::env::var("SAVE_PDF_PROBE").is_ok() {
                    let mut combined_pdf = dict.clone();
                    combined_pdf.extend_from_slice(&pdf_result.page_data);
                    let test_file = "test_output_pdf_combined.jb2";
                    match std::fs::write(test_file, &combined_pdf) {
                        Ok(_) => println!(
                            "📁 Saved debug-only PDF probe: {} ({} bytes)",
                            test_file,
                            combined_pdf.len()
                        ),
                        Err(e) => println!("⚠️  Could not save PDF probe: {}", e),
                    }
                }
            }
        }
        Err(e) => println!("🔧 PDF mode failed: {}", e),
    }

    // Now test standalone mode
    match encode_single_image(input, width, height, false) {
        Ok(result) => {
            let elapsed = start.elapsed().as_millis();
            stats.mark_success(&result, elapsed);
            println!("✓ Symbol encoding completed in {} ms", elapsed);

            // Debug: Check if page_data is empty or has content
            if result.page_data.is_empty() {
                println!("⚠️  WARNING: Symbol mode produced empty page_data!");
            } else {
                println!("📊 Symbol mode page_data: {} bytes", result.page_data.len());
            }

            // Save the JBIG2 output to file for decoding verification
            let output_file = "test_output_symbol.jb2";

            // In standalone mode (pdf_mode=false), combine global and page data if present
            let complete_data = if let Some(ref dict) = result.global_data {
                println!("📖 Global data present: {} bytes (combining with page data for standalone file)", dict.len());
                let mut combined = dict.clone();
                combined.extend_from_slice(&result.page_data);
                combined
            } else {
                println!("📖 No global data - using page data only");
                result.page_data.clone()
            };

            if let Err(e) = std::fs::write(output_file, &complete_data) {
                println!("⚠️  Warning: Could not save symbol output: {}", e);
            } else {
                println!(
                    "📁 Saved symbol output to: {} ({} bytes)",
                    output_file,
                    complete_data.len()
                );

                // Test decoding with jbig2dec (standalone JBIG2 files)
                let mut cmd = jbig2dec_command();
                match cmd
                    .args(&["-o", "decoded_symbol.pbm", output_file])
                    .output()
                {
                    Ok(output) => {
                        if output.status.success() {
                            println!("✅ Symbol JBIG2 successfully decoded with jbig2dec");
                        } else {
                            println!(
                                "❌ Symbol JBIG2 decode failed: {}",
                                String::from_utf8_lossy(&output.stderr)
                            );
                        }
                    }
                    Err(e) => {
                        println!("⚠️  Could not run jbig2dec: {}", e);
                    }
                }
            }
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis();
            stats.mark_failure(&e.to_string(), elapsed);
            println!("✗ Symbol encoding failed: {}", e);
        }
    }

    stats
}

/// Run encoding comparison test on a given image
fn run_encoding_test(img: &BitImage, test_name: &str, mode: TestMode) -> ComparisonResults {
    println!("\n{}", "=".repeat(60));
    println!("Testing: {}", test_name);
    println!(
        "Image size: {}x{} pixels ({} bytes)",
        img.width,
        img.height,
        img.width as usize * img.height as usize
    );
    println!("{}", "=".repeat(60));

    let binary_data = bitimage_to_binary(img);
    let mut results = ComparisonResults::new();

    match mode {
        TestMode::LosslessOnly => {
            results.lossless = Some(test_lossless_mode(
                &binary_data,
                img.width as u32,
                img.height as u32,
            ));
        }
        TestMode::SymbolOnly => {
            results.symbol = Some(test_symbol_mode(
                &binary_data,
                img.width as u32,
                img.height as u32,
            ));
        }
        TestMode::Both => {
            results.lossless = Some(test_lossless_mode(
                &binary_data,
                img.width as u32,
                img.height as u32,
            ));
            results.symbol = Some(test_symbol_mode(
                &binary_data,
                img.width as u32,
                img.height as u32,
            ));
        }
    }

    results.print_comparison();
    results
}

/// Create test images of various types and complexities

/// Create a simple 8x8 checkerboard pattern
fn create_small_checkerboard() -> BitImage {
    let mut img = BitImage::new(8, 8).expect("Failed to create 8x8 image");
    for y in 0..8 {
        for x in 0..8 {
            img.set(x, y, (x + y) % 2 == 0);
        }
    }
    img
}

/// Create a larger 64x64 checkerboard pattern (good for symbol detection)
fn create_large_checkerboard() -> BitImage {
    let mut img = BitImage::new(64, 64).expect("Failed to create 64x64 image");
    for y in 0..64 {
        for x in 0..64 {
            img.set(x, y, (x + y) % 2 == 0);
        }
    }
    img
}

/// Create an image with repeated text-like patterns (good for symbol dictionaries)
fn create_text_pattern() -> BitImage {
    let mut img = BitImage::new(64, 32).expect("Failed to create text pattern");

    // Create a simple "letter" pattern (5x7 pixels)
    let letter_pattern = [
        0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b00000,
    ];

    // Repeat this pattern across the image
    for repeat_x in 0..6 {
        for repeat_y in 0..4 {
            let base_x = repeat_x * 10;
            let base_y = repeat_y * 8;

            for (row, &pattern) in letter_pattern.iter().enumerate() {
                for bit in 0..5 {
                    let x = base_x + bit;
                    let y = base_y + row;
                    if x < 64 && y < 32 {
                        let pixel = (pattern >> (4 - bit)) & 1 == 1;
                        img.set(x as u32, y as u32, pixel);
                    }
                }
            }
        }
    }

    img
}

/// Create a half black, half white image (minimal compression potential)
fn create_half_split() -> BitImage {
    let mut img = BitImage::new(32, 32).expect("Failed to create half split");
    for y in 0..32 {
        for x in 0..32 {
            img.set(x, y, x < 16);
        }
    }
    img
}

/// Main test functions

#[test]
fn test_small_patterns() {
    println!("\n🧪 TESTING SMALL PATTERNS");

    // Test simple small checkerboard
    let small_checker = create_small_checkerboard();
    run_encoding_test(&small_checker, "8x8 Checkerboard", TestMode::Both);

    // Test simple split pattern
    let half_split = create_half_split();
    run_encoding_test(&half_split, "32x32 Half Split", TestMode::Both);
}

#[test]
fn test_medium_patterns() {
    println!("\n🧪 TESTING MEDIUM PATTERNS");

    // Test larger checkerboard (should benefit from symbol detection)
    let large_checker = create_large_checkerboard();
    run_encoding_test(&large_checker, "64x64 Checkerboard", TestMode::Both);

    // Test text-like pattern (should really benefit from symbol dictionaries)
    let text_pattern = create_text_pattern();
    run_encoding_test(&text_pattern, "64x32 Repeated Text Pattern", TestMode::Both);
}

#[test]
fn test_real_images() {
    println!("\n🧪 TESTING REAL IMAGES");

    // Test with actual PBM test images if available
    if std::path::Path::new(TEST_IMAGE_PBM).exists() {
        let test_img = load_pbm(TEST_IMAGE_PBM);
        run_encoding_test(&test_img, "Real PBM Test Image (Small)", TestMode::Both);
    } else {
        println!(
            "⚠️  Skipping real image test - {} not found",
            TEST_IMAGE_PBM
        );
    }

    if std::path::Path::new(TEST_IMAGE1_PBM).exists() {
        let test_img1 = load_pbm(TEST_IMAGE1_PBM);
        run_encoding_test(&test_img1, "Real PBM Test Image (Large)", TestMode::Both);
    } else {
        println!(
            "⚠️  Skipping real image test - {} not found",
            TEST_IMAGE1_PBM
        );
    }

    // Test with the specified test image - this is the main test as requested
    let test_image2_path = "test_image2.pbm";
    if std::path::Path::new(test_image2_path).exists() {
        println!("\n📋 RUNNING REQUESTED TEST WITH SPECIFIED IMAGE");
        println!("🎯 Testing file: {}", test_image2_path);
        let test_img2 = load_pbm(test_image2_path);
        run_encoding_test(
            &test_img2,
            "Test Image 2 (Specified by User)",
            TestMode::Both,
        );
    } else {
        println!(
            "❌ ERROR: Requested test image not found - {}",
            test_image2_path
        );
        println!("   Please ensure the test image exists in the current directory");
    }
}

#[test]
fn test_specified_image_only() {
    println!("\n🎯 FOCUSED TEST: USER-SPECIFIED IMAGE ONLY");
    println!("==================================================");

    let test_image2_path = "test_image2.pbm";
    if std::path::Path::new(test_image2_path).exists() {
        println!("📁 Loading test image: {}", test_image2_path);
        let test_img2 = load_pbm(test_image2_path);
        println!(
            "✅ Image loaded successfully: {}x{} pixels",
            test_img2.width, test_img2.height
        );

        println!("\n🔄 Running comprehensive comparison...");
        let results = run_encoding_test(&test_img2, "User Specified Test Image", TestMode::Both);

        println!("\n📊 FINAL SUMMARY FOR SPECIFIED IMAGE");
        println!("=====================================");

        if let (Some(ref lossless), Some(ref symbol)) = (&results.lossless, &results.symbol) {
            if lossless.success && symbol.success {
                println!("✅ Both encoding modes completed successfully!");
                println!();
                println!("📈 KEY METRICS:");
                println!(
                    "   Lossless mode:  {} bytes, {:.2}x compression, {} ms",
                    lossless.total_output_size,
                    lossless.compression_ratio,
                    lossless.encoding_time_ms
                );
                println!(
                    "   Symbol mode:    {} bytes, {:.2}x compression, {} ms",
                    symbol.total_output_size, symbol.compression_ratio, symbol.encoding_time_ms
                );

                let size_savings =
                    lossless.total_output_size as i64 - symbol.total_output_size as i64;
                let size_percent =
                    (size_savings as f64 / lossless.total_output_size as f64) * 100.0;

                println!();
                if size_savings > 0 {
                    println!(
                        "🏆 Symbol mode is BETTER: {} bytes smaller ({:.1}% reduction)",
                        size_savings, size_percent
                    );
                } else if size_savings < 0 {
                    println!(
                        "🏆 Lossless mode is BETTER: {} bytes smaller ({:.1}% reduction)",
                        -size_savings, -size_percent
                    );
                } else {
                    println!("🤝 Both modes produced identical output sizes");
                }

                if symbol.encoding_time_ms < lossless.encoding_time_ms {
                    println!(
                        "⚡ Symbol mode is FASTER by {} ms",
                        lossless.encoding_time_ms - symbol.encoding_time_ms
                    );
                } else if symbol.encoding_time_ms > lossless.encoding_time_ms {
                    println!(
                        "⚡ Lossless mode is FASTER by {} ms",
                        symbol.encoding_time_ms - lossless.encoding_time_ms
                    );
                } else {
                    println!("⚡ Both modes have identical encoding times");
                }
            } else {
                println!("❌ One or both encoding modes failed!");
                if !lossless.success {
                    println!("   Lossless error: {:?}", lossless.error_message);
                }
                if !symbol.success {
                    println!("   Symbol error: {:?}", symbol.error_message);
                }
            }
        }
    } else {
        println!(
            "❌ ERROR: Specified test image not found - {}",
            test_image2_path
        );
        println!("   Current directory contents:");
        if let Ok(entries) = std::fs::read_dir(".") {
            for entry in entries.flatten() {
                if let Some(ext) = entry.path().extension() {
                    if ext == "pbm" {
                        println!("     Found: {}", entry.path().display());
                    }
                }
            }
        }
    }
}

#[test]
fn test_lossless_only() {
    println!("\n🧪 TESTING LOSSLESS MODE ONLY");

    let patterns = [
        ("8x8 Checkerboard", create_small_checkerboard()),
        ("64x64 Checkerboard", create_large_checkerboard()),
        ("Text Pattern", create_text_pattern()),
        ("Half Split", create_half_split()),
    ];

    for (name, img) in patterns.iter() {
        run_encoding_test(img, name, TestMode::LosslessOnly);
    }
}

#[test]
fn test_symbol_only() {
    println!("\n🧪 TESTING SYMBOL MODE ONLY");

    let patterns = [
        ("8x8 Checkerboard", create_small_checkerboard()),
        ("64x64 Checkerboard", create_large_checkerboard()),
        ("Text Pattern", create_text_pattern()),
        ("Half Split", create_half_split()),
    ];

    for (name, img) in patterns.iter() {
        run_encoding_test(img, name, TestMode::SymbolOnly);
    }
}

#[test]
fn test_comprehensive_comparison() {
    println!("\n🧪 COMPREHENSIVE MODE COMPARISON");
    println!("This test compares both encoding modes across multiple pattern types");

    let test_cases = [
        ("Small Checkerboard (8x8)", create_small_checkerboard()),
        ("Large Checkerboard (64x64)", create_large_checkerboard()),
        ("Repeated Text Pattern", create_text_pattern()),
        ("Simple Half Split", create_half_split()),
    ];

    let mut all_results = Vec::new();

    for (name, img) in test_cases.iter() {
        let results = run_encoding_test(img, name, TestMode::Both);
        all_results.push((name.to_string(), results));
    }

    // Print summary
    println!("\n{}", "=".repeat(80));
    println!("                         SUMMARY OF ALL TESTS");
    println!("{}", "=".repeat(80));

    for (name, results) in all_results.iter() {
        println!("\n{}", name);
        if let (Some(ref lossless), Some(ref symbol)) = (&results.lossless, &results.symbol) {
            if lossless.success && symbol.success {
                let size_winner = if symbol.total_output_size < lossless.total_output_size {
                    "Symbol"
                } else if symbol.total_output_size > lossless.total_output_size {
                    "Lossless"
                } else {
                    "Tie"
                };

                let speed_winner = if symbol.encoding_time_ms < lossless.encoding_time_ms {
                    "Symbol"
                } else if symbol.encoding_time_ms > lossless.encoding_time_ms {
                    "Lossless"
                } else {
                    "Tie"
                };

                println!(
                    "  Size winner:  {} ({} vs {} bytes)",
                    size_winner, symbol.total_output_size, lossless.total_output_size
                );
                println!(
                    "  Speed winner: {} ({} vs {} ms)",
                    speed_winner, symbol.encoding_time_ms, lossless.encoding_time_ms
                );
            } else {
                println!("  Status: One or both modes failed");
            }
        }
    }

    println!("{}", "=".repeat(80));
}

// Helper function to run individual mode tests from command line if needed
pub fn run_single_test(test_name: &str, mode: TestMode) {
    let img = match test_name {
        "small_checker" => create_small_checkerboard(),
        "large_checker" => create_large_checkerboard(),
        "text_pattern" => create_text_pattern(),
        "half_split" => create_half_split(),
        _ => {
            println!("Unknown test: {}", test_name);
            return;
        }
    };

    run_encoding_test(&img, test_name, mode);
}

// Helper to resolve a local jbig2dec binary when running tests
fn jbig2dec_command() -> std::process::Command {
    #[cfg(windows)]
    {
        let local = std::path::Path::new(".\\jbig2dec.exe");
        if local.exists() {
            return std::process::Command::new(local);
        }
        std::process::Command::new("jbig2dec.exe")
    }
    #[cfg(not(windows))]
    {
        let local = std::path::Path::new("./jbig2dec");
        if local.exists() {
            return std::process::Command::new(local);
        }
        std::process::Command::new("jbig2dec")
    }
}
