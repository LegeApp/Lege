use Legencode::jp2_encoder::{encode_to_target_size, jp2_config};
use Legencode::streamline::EncodingResult as LegeEncodingResult;
use Legencode::streamline::{
    EncodingManager, EncodingSettings, ImageBuffer as LegeImageBuffer, JpegSettings,
};
use image::{DynamicImage, GenericImageView, Rgb, RgbImage};
use std::fs::File;
use std::io::Write;
use std::path::Path;

fn solve_rate_for_target_size(
    image_data: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    target_bytes: usize,
) -> Result<(f32, Vec<u8>, usize), Box<dyn std::error::Error>> {
    let (rate, encoded) = encode_to_target_size(image_data, width, height, channels, target_bytes)?;
    let sz = encoded.len();
    Ok((rate, encoded, sz))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <input_image>", args[0]);
        std::process::exit(1);
    }

    let input_path = Path::new(&args[1]);
    if !input_path.exists() {
        eprintln!("Error: Input file does not exist: {}", input_path.display());
        std::process::exit(1);
    }

    // Create output directory
    let output_dir = Path::new("compression_test_results");
    std::fs::create_dir_all(output_dir)?;

    // Load the input image
    println!("Loading image: {}", input_path.display());
    let img = image::open(input_path)?;

    // Convert to RGB8
    let (width, height) = img.dimensions();
    let img_rgb8 = match img {
        DynamicImage::ImageRgba8(img) => {
            let mut rgb = RgbImage::new(width, height);
            for (x, y, pixel) in img.enumerate_pixels() {
                let p = pixel.0;
                rgb.put_pixel(x, y, Rgb([p[0], p[1], p[2]]));
            }
            rgb
        }
        DynamicImage::ImageRgba16(img) => {
            let mut rgb = RgbImage::new(width, height);
            for (x, y, pixel) in img.enumerate_pixels() {
                let p = pixel.0;
                rgb.put_pixel(
                    x,
                    y,
                    Rgb([(p[0] >> 8) as u8, (p[1] >> 8) as u8, (p[2] >> 8) as u8]),
                );
            }
            rgb
        }
        DynamicImage::ImageLuma8(img) => {
            let mut rgb = RgbImage::new(width, height);
            for (x, y, pixel) in img.enumerate_pixels() {
                let l = pixel.0[0];
                rgb.put_pixel(x, y, Rgb([l, l, l]));
            }
            rgb
        }
        DynamicImage::ImageLuma16(img) => {
            let mut rgb = RgbImage::new(width, height);
            for (x, y, pixel) in img.enumerate_pixels() {
                let l = (pixel.0[0] >> 8) as u8;
                rgb.put_pixel(x, y, Rgb([l, l, l]));
            }
            rgb
        }
        DynamicImage::ImageRgb8(img) => img,
        DynamicImage::ImageRgb16(img) => {
            let mut rgb = RgbImage::new(width, height);
            for (x, y, pixel) in img.enumerate_pixels() {
                rgb.put_pixel(
                    x,
                    y,
                    Rgb([
                        (pixel.0[0] >> 8) as u8,
                        (pixel.0[1] >> 8) as u8,
                        (pixel.0[2] >> 8) as u8,
                    ]),
                );
            }
            rgb
        }
        _ => {
            eprintln!("Unsupported image format");
            std::process::exit(1);
        }
    };

    // Get raw pixel data from the converted RGB8 image
    let image_data = img_rgb8.into_raw();
    let (width, height, channels) = (width, height, 3u8);

    let mut results = Vec::new();

    // Test JPEG compression
    println!("\nTesting JPEG compression...");
    for quality in (10..=90).step_by(10) {
        let settings = JpegSettings {
            quality,
            baseline: true,
            optimized: true,
            downsample: true, // Enable 4:2:0 chroma subsampling
        };

        let output_path = output_dir.join(format!(
            "{}_jpeg_q{:02}.jpg",
            input_path.file_stem().unwrap().to_string_lossy(),
            quality
        ));

        let buffer = LegeImageBuffer {
            data: &image_data,
            width,
            height,
            channels,
        };
        match EncodingManager::encode(&buffer, &EncodingSettings::Jpeg(settings)) {
            Ok(LegeEncodingResult::Standard(encoded_data)) => {
                std::fs::write(&output_path, &encoded_data)?;
                let file_size = std::fs::metadata(&output_path)?.len();
                let ratio = (image_data.len() as f64) / (file_size as f64);
                results.push((
                    format!("JPEG Q{:02}", quality),
                    output_path.clone(),
                    file_size,
                    ratio,
                    quality as f32,
                ));
                println!(
                    "  - Q{:02}: {} KB (Compression: {:.1}x)",
                    quality,
                    file_size / 1024,
                    ratio
                );
            }
            Ok(_) => eprintln!("  - Q{:02} failed: unexpected variant", quality),
            Err(e) => eprintln!("  - Q{:02} failed: {}", quality, e),
        }
    }

    // JP2 target-size search: 10, 15, 30, 45 KB
    println!("\nTesting JP2 at target sizes (approx.): 10, 15, 30, 45 KB...");
    let targets_kb = [10usize, 15, 30, 45];
    for &kb in &targets_kb {
        let target_bytes = kb * 1024;
        match solve_rate_for_target_size(&image_data, width, height, channels, target_bytes) {
            Ok((rate, data, sz)) => {
                let output_path = output_dir.join(format!(
                    "{}_jp2_target_{:02}kb.jp2",
                    input_path.file_stem().unwrap().to_string_lossy(),
                    kb
                ));
                std::fs::write(&output_path, &data)?;
                let ratio = (image_data.len() as f64) / (sz as f64);
                results.push((
                    format!("JP2 ~{:02}KB (r≈{:.1})", kb, rate),
                    output_path,
                    sz as u64,
                    ratio,
                    rate,
                ));
                println!(
                    "  - Target {:>2} KB -> {:>5.1} KB (r≈{:.1}, comp: {:.1}x)",
                    kb,
                    sz as f64 / 1024.0,
                    rate,
                    ratio
                );
            }
            Err(e) => eprintln!("  - Target {:>2} KB failed: {}", kb, e),
        }
    }

    // Also test production scaler targets for image and cover
    println!("\nTesting JP2 with production scaler targets (image and cover)...");
    for &(is_cover, label) in &[(false, "image"), (true, "cover")] {
        let target_bytes = jp2_config::calculate_jp2_target_bytes_content_aware(
            &image_data,
            width,
            height,
            3,
            is_cover,
        );
        match encode_to_target_size(&image_data, width, height, channels, target_bytes) {
            Ok((rate, data)) => {
                let output_path = output_dir.join(format!(
                    "{}_jp2_{}_scaled.jp2",
                    input_path.file_stem().unwrap().to_string_lossy(),
                    label
                ));
                std::fs::write(&output_path, &data)?;
                let sz = std::fs::metadata(&output_path)?.len() as usize;
                let ratio = (image_data.len() as f64) / (sz as f64);
                results.push((
                    format!("JP2 {} scaled (r≈{:.1})", label, rate),
                    output_path,
                    sz as u64,
                    ratio,
                    rate,
                ));
                println!(
                    "  - {} target -> {:>5.1} KB (r≈{:.1}, comp: {:.1}x)",
                    label,
                    sz as f64 / 1024.0,
                    rate,
                    ratio
                );
            }
            Err(e) => eprintln!("  - {} scaled failed: {}", label, e),
        }
    }

    // Generate a summary report
    let report_path = output_dir.join("compression_report.txt");
    let mut report = File::create(&report_path)?;
    let original_size = image_data.len();

    writeln!(report, "Compression Test Results")?;
    writeln!(report, "=======================\n")?;
    writeln!(report, "Input image: {}", input_path.display())?;
    writeln!(
        report,
        "Dimensions: {}x{} ({} channels)",
        width, height, channels
    )?;
    writeln!(
        report,
        "Original size: {:.1} KB\n",
        original_size as f64 / 1024.0
    )?;
    writeln!(
        report,
        "{:<18} {:>10} {:>12} {:>10} {}",
        "Format", "Size (KB)", "Ratio", "Q", "Path"
    )?;
    writeln!(
        report,
        "{:-<18} {:-<10} {:-<12} {:-<10} {}",
        "", "", "", "", ""
    )?;

    // Sort results by file size (ascending)
    results.sort_by(|a, b| a.2.cmp(&b.2));

    for (format, path, size, ratio, quality) in &results {
        writeln!(
            report,
            "{:<18} {:>9.1} {:>11.1}x {:>9.1} {}",
            format,
            *size as f64 / 1024.0,
            ratio,
            quality,
            path.file_name().unwrap().to_string_lossy()
        )?;
    }

    println!(
        "\nTest complete! Results saved to: {}",
        report_path.display()
    );
    println!("Output directory: {}", output_dir.display());

    Ok(())
}

#[test]
fn test_compression() -> Result<(), Box<dyn std::error::Error>> {
    // This is just a placeholder test to ensure the example compiles
    // You can run this with: cargo test --example compression_tester -- --nocapture
    println!("Compression tester example works!");
    Ok(())
}
