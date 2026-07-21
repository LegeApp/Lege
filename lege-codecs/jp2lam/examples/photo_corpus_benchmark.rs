//! Photographic corpus qualification harness for `jp2lam-hd-encode-plan.md`.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --example photo_corpus_benchmark -- \
//!   test-set /tmp/jp2lam-photo-corpus 0.5,1.0,2.0
//! ```
//!
//! The JP2 outputs and ImageMagick-decoded PNGs are written below the supplied
//! output directory. CSV is written to stdout. ImageMagick is intentionally the
//! reconstruction path so the encoder's internal decoder is not the oracle.

use jp2lam::{
    EncodeOptions, ImageView, OutputFormat, RateControl, ResourceLimits, TilePolicy,
    encode_view_to_writer,
};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn main() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let corpus = PathBuf::from(args.first().map(String::as_str).unwrap_or("test-set"));
    let output = PathBuf::from(
        args.get(1)
            .map(String::as_str)
            .unwrap_or("/tmp/jp2lam-photo-corpus"),
    );
    let rates = parse_rates(args.get(2).map(String::as_str).unwrap_or("1.0"))?;
    let max_files = args.get(3).and_then(|value| value.parse::<usize>().ok());
    fs::create_dir_all(&output).map_err(|error| error.to_string())?;

    let mut inputs = Vec::new();
    collect_pngs(&corpus, &mut inputs)?;
    inputs.sort();
    if let Some(limit) = max_files {
        inputs.truncate(limit);
    }
    if inputs.is_empty() {
        return Err(format!("no PNG images found below {}", corpus.display()));
    }

    println!(
        "source,width,height,megapixels,mode,requested_quality,target_bpp,target_bytes,output_bytes,actual_bpp,target_error_percent,encode_ms,peak_rss_bytes,psnr_rgb_db,psnr_r_db,psnr_g_db,psnr_b_db"
    );
    for (index, input) in inputs.iter().enumerate() {
        for &rate in &rates {
            run_case(index, input, &output, rate)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum CorpusRate {
    Bpp(f32),
    Quality(u8),
}

fn run_case(index: usize, input: &Path, output: &Path, rate: CorpusRate) -> Result<(), String> {
    let source = image::open(input)
        .map_err(|error| format!("load {}: {error}", input.display()))?
        .into_rgb8();
    let (width, height) = source.dimensions();
    let pixels = u64::from(width) * u64::from(height);
    let (mode, quality, bpp, target_bytes, rate_control, rate_tag) = match rate {
        CorpusRate::Bpp(bpp) => (
            "bpp",
            0,
            bpp,
            (pixels as f64 * f64::from(bpp) / 8.0).round() as u64,
            RateControl::TargetBitsPerPixel(bpp),
            format!("bpp_{bpp:.3}"),
        ),
        CorpusRate::Quality(quality) => (
            "quality",
            quality,
            0.0,
            0,
            RateControl::Quality(quality),
            format!("q_{quality}"),
        ),
    };
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("image");
    let tag = format!("{index:02}_{stem}_{rate_tag}").replace('.', "_");
    let jp2_path = output.join(format!("{tag}.jp2"));
    let decoded_path = output.join(format!("{tag}_decoded.png"));

    let view = ImageView::from_rgb8_interleaved(width, height, source.as_raw())
        .map_err(|error| error.to_string())?;
    let options = EncodeOptions {
        rate_control: Some(rate_control),
        format: OutputFormat::Jp2,
        tile_policy: TilePolicy::Auto,
        resource_limits: ResourceLimits {
            max_working_memory: Some(512 * 1024 * 1024),
            encoded_store_memory_limit: Some(64 * 1024 * 1024),
            ..Default::default()
        },
        ..Default::default()
    };
    let start = Instant::now();
    let mut file = File::create(&jp2_path).map_err(|error| error.to_string())?;
    encode_view_to_writer(view, &options, &mut file).map_err(|error| error.to_string())?;
    drop(file);
    let encode_ms = start.elapsed().as_secs_f64() * 1000.0;
    let output_bytes = fs::metadata(&jp2_path)
        .map_err(|error| error.to_string())?
        .len();

    let status = Command::new("magick")
        .arg(&jp2_path)
        .args(["-depth", "8"])
        .arg(&decoded_path)
        .status()
        .map_err(|error| format!("run ImageMagick: {error}"))?;
    if !status.success() {
        return Err(format!(
            "ImageMagick failed to decode {}",
            jp2_path.display()
        ));
    }
    let decoded = image::open(&decoded_path)
        .map_err(|error| format!("load independent decode: {error}"))?
        .into_rgb8();
    if decoded.dimensions() != (width, height) {
        return Err("independent decoder returned different dimensions".into());
    }
    let psnr = rgb_psnr(source.as_raw(), decoded.as_raw());
    let actual_bpp = output_bytes as f64 * 8.0 / pixels as f64;
    let target_error = if target_bytes == 0 {
        f64::NAN
    } else {
        (output_bytes as f64 - target_bytes as f64) * 100.0 / target_bytes as f64
    };
    println!(
        "{},{width},{height},{:.3},{mode},{quality},{bpp:.4},{target_bytes},{output_bytes},{actual_bpp:.4},{target_error:.4},{encode_ms:.3},{},{:.4},{:.4},{:.4},{:.4}",
        input.display(),
        pixels as f64 / 1_000_000.0,
        peak_rss_bytes().unwrap_or(0),
        psnr[3],
        psnr[0],
        psnr[1],
        psnr[2],
    );
    Ok(())
}

fn rgb_psnr(source: &[u8], decoded: &[u8]) -> [f64; 4] {
    let mut squared_error = [0u128; 4];
    for (source_pixel, decoded_pixel) in source.chunks_exact(3).zip(decoded.chunks_exact(3)) {
        for channel in 0..3 {
            let error = i32::from(source_pixel[channel]) - i32::from(decoded_pixel[channel]);
            let square = (error * error) as u128;
            squared_error[channel] += square;
            squared_error[3] += square;
        }
    }
    let pixels = source.len() / 3;
    [
        psnr_from_sse(squared_error[0], pixels),
        psnr_from_sse(squared_error[1], pixels),
        psnr_from_sse(squared_error[2], pixels),
        psnr_from_sse(squared_error[3], pixels * 3),
    ]
}

fn psnr_from_sse(sse: u128, samples: usize) -> f64 {
    if sse == 0 {
        return f64::INFINITY;
    }
    let mse = sse as f64 / samples as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

fn parse_rates(value: &str) -> Result<Vec<CorpusRate>, String> {
    value
        .split(',')
        .map(|part| {
            if let Some(quality) = part.strip_prefix('q') {
                let quality = quality
                    .parse::<u8>()
                    .map_err(|_| format!("invalid quality `{part}`"))?;
                if quality > 99 {
                    return Err(format!("lossy quality must be 0..=99, got {quality}"));
                }
                return Ok(CorpusRate::Quality(quality));
            }
            let bpp = part
                .parse::<f32>()
                .map_err(|_| format!("invalid bpp `{part}`"))?;
            if !bpp.is_finite() || bpp <= 0.0 {
                return Err(format!("bpp must be finite and positive, got {bpp}"));
            }
            Ok(CorpusRate::Bpp(bpp))
        })
        .collect()
}

fn collect_pngs(directory: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            collect_pngs(&path, out)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn peak_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    kib.checked_mul(1024)
}
