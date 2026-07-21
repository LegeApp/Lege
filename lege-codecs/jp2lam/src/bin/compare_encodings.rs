use std::path::{Path, PathBuf};
use std::{env, fs};

use image::DynamicImage;
use jp2lam::{EncodeMetrics, EncodeOptions, Image, OutputFormat, encode_with_psnr};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: compare_encodings <image_path> [output_dir]");
        eprintln!();
        eprintln!("Encodes the image at multiple quality levels and writes one file per variant.");
        eprintln!("JP2 files open in IrfanView, Photoshop, etc.");
        std::process::exit(1);
    }

    let src_path = Path::new(&args[1]);
    let out_dir = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| src_path.parent().unwrap_or(Path::new(".")).to_path_buf());

    let stem = src_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");

    let raw = fs::read(src_path).expect("failed to read source image");
    let src_bytes = raw.len();

    let img = image::load_from_memory(&raw).expect("failed to decode source image");
    let (width, height) = (img.width(), img.height());
    let rgb_uncompressed = (width as usize) * (height as usize) * 3;

    let n_pixels = (width * height) as usize;

    println!(
        "Source:  {} ({}x{}, {} bytes on disk, {} bytes raw RGB)",
        src_path.display(),
        width,
        height,
        fmt(src_bytes),
        fmt(rgb_uncompressed),
    );
    println!("Output:  {}", out_dir.display());
    println!();
    println!(
        "{:<36} {:>10} {:>7} {:>8} {:>7}",
        "file", "bytes", "bpp", "PSNR dB", "SSIM"
    );
    println!("{}", "-".repeat(75));

    // Quality sweep: covers the full 0-100 range at meaningful intervals
    let variants: &[(&str, u8)] = &[
        ("q20.jp2", 20),
        ("q30.jp2", 30),
        ("q42.jp2", 42),
        ("q55.jp2", 55),
        ("q62.jp2", 62),
        ("q70.jp2", 70),
        ("q75.jp2", 75),
        ("q80.jp2", 80),
        ("q85.jp2", 85),
        ("q90.jp2", 90),
        ("q95.jp2", 95),
        ("q99.jp2", 99),
        ("lossless.jp2", 100),
    ];

    for (suffix, quality) in variants {
        let filename = format!("{stem}_{suffix}");
        let out_path = out_dir.join(&filename);

        let (bytes, metrics) = encode_jp2_with_psnr(&img, *quality);
        let bpp = bytes.len() as f64 * 8.0 / n_pixels as f64;
        fs::write(&out_path, &bytes).expect("failed to write output");

        let psnr_str = if metrics.psnr_db.is_infinite() {
            "lossless".to_string()
        } else {
            format!("{:.2}", metrics.psnr_db)
        };
        let ssim_str = if metrics.ssim >= 0.9999 && metrics.psnr_db.is_infinite() {
            "1.000".to_string()
        } else {
            format!("{:.4}", metrics.ssim)
        };

        println!(
            "{:<36} {:>10} {:>7.3} {:>8} {:>7}",
            filename,
            fmt(bytes.len()),
            bpp,
            psnr_str,
            ssim_str,
        );
    }
}

fn encode_jp2_with_psnr(img: &DynamicImage, quality: u8) -> (Vec<u8>, EncodeMetrics) {
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    let jp2_img = Image::from_rgb_bytes(w, h, rgb.as_raw()).expect("failed to build jp2lam Image");
    encode_with_psnr(
        &jp2_img,
        &EncodeOptions {
            quality,
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        },
    )
    .expect("jp2lam encode failed")
}

fn fmt(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}
