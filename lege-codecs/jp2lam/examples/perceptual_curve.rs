//! Local photographic quality-curve harness.
//!
//! Prints one CSV row per input/quality pair using the crate decoder as the
//! reconstruction path and Butteraugli + SSIMULACRA2 as perceptual graders.
//!
//! Usage:
//!   cargo run --release --example perceptual_curve -- test-set 25,50,75,90,98 7

use butteraugli::{ButteraugliParams, Img, RGB8, butteraugli};
use jp2lam::{
    DecodeLimits, DecodeRequest, DecodeResult, EncodeOptions, Image, Jp2Decoder, OutputFormat,
    RateControl, encode,
};
use ssimulacra2::{ColorPrimaries, Rgb, TransferCharacteristic, compute_frame_ssimulacra2};
use std::fs;
use std::path::{Path, PathBuf};

fn main() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let corpus = PathBuf::from(args.first().map(String::as_str).unwrap_or("test-set"));
    let qualities = parse_qualities(args.get(1).map(String::as_str).unwrap_or("25,50,75,90,98"))?;
    let max_files = args.get(2).and_then(|value| value.parse::<usize>().ok());

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
        "source,width,height,megapixels,quality,bytes,bpp,psnr_rgb_db,butteraugli,ssimulacra2"
    );
    let mut decoder = Jp2Decoder::new();
    for input in inputs {
        let source = image::open(&input)
            .map_err(|error| format!("load {}: {error}", input.display()))?
            .into_rgb8();
        let (width, height) = source.dimensions();
        let pixels = u64::from(width) * u64::from(height);
        let jp2_image = Image::from_rgb_bytes(width, height, source.as_raw())
            .map_err(|error| error.to_string())?;

        for &quality in &qualities {
            let bytes = encode(
                &jp2_image,
                &EncodeOptions {
                    rate_control: Some(RateControl::Quality(quality)),
                    format: OutputFormat::Jp2,
                    ..Default::default()
                },
            )
            .map_err(|error| format!("encode {} q{quality}: {error}", input.display()))?;
            let decoded = decoder
                .decode(
                    &bytes,
                    &DecodeRequest {
                        limits: DecodeLimits {
                            max_input_bytes: bytes.len().saturating_add(1024 * 1024),
                            max_pixels: pixels.saturating_add(1),
                            max_working_bytes: usize::MAX,
                            ..DecodeLimits::default()
                        },
                        ..DecodeRequest::default()
                    },
                )
                .map_err(|error| format!("decode {} q{quality}: {error}", input.display()))?;
            let DecodeResult::Native(decoded) = decoded else {
                return Err("perceptual harness expected native planar decode".into());
            };
            let reconstructed = decoded_rgb8(&decoded)?;
            let psnr = rgb_psnr(source.as_raw(), &reconstructed);
            let butteraugli = butteraugli_score(
                source.as_raw(),
                &reconstructed,
                width as usize,
                height as usize,
            )?;
            let ssimulacra2 = ssimulacra2_score(
                source.as_raw(),
                &reconstructed,
                width as usize,
                height as usize,
            )?;
            println!(
                "{},{width},{height},{:.3},{quality},{},{:.5},{psnr:.5},{butteraugli:.6},{ssimulacra2:.6}",
                input.display(),
                pixels as f64 / 1_000_000.0,
                bytes.len(),
                bytes.len() as f64 * 8.0 / pixels as f64,
            );
        }
    }
    Ok(())
}

fn decoded_rgb8(image: &Image) -> Result<Vec<u8>, String> {
    if image.components.len() != 3 {
        return Err(format!(
            "expected three decoded components, got {}",
            image.components.len()
        ));
    }
    let pixels = usize::try_from(u64::from(image.width) * u64::from(image.height))
        .map_err(|_| "decoded image area exceeds usize".to_string())?;
    if image
        .components
        .iter()
        .any(|component| component.data.len() != pixels)
    {
        return Err("decoded component size does not match image area".into());
    }
    let mut rgb = Vec::with_capacity(pixels * 3);
    for index in 0..pixels {
        for component in &image.components {
            rgb.push(component.data[index].clamp(0, 255) as u8);
        }
    }
    Ok(rgb)
}

fn butteraugli_score(
    source: &[u8],
    decoded: &[u8],
    width: usize,
    height: usize,
) -> Result<f64, String> {
    let source = source
        .chunks_exact(3)
        .map(|pixel| RGB8::new(pixel[0], pixel[1], pixel[2]))
        .collect::<Vec<_>>();
    let decoded = decoded
        .chunks_exact(3)
        .map(|pixel| RGB8::new(pixel[0], pixel[1], pixel[2]))
        .collect::<Vec<_>>();
    butteraugli(
        Img::new(source, width, height).as_ref(),
        Img::new(decoded, width, height).as_ref(),
        &ButteraugliParams::default(),
    )
    .map(|result| result.score)
    .map_err(|error| error.to_string())
}

fn ssimulacra2_score(
    source: &[u8],
    decoded: &[u8],
    width: usize,
    height: usize,
) -> Result<f64, String> {
    let source = Rgb::new(
        source
            .chunks_exact(3)
            .map(|pixel| {
                [
                    pixel[0] as f32 / 255.0,
                    pixel[1] as f32 / 255.0,
                    pixel[2] as f32 / 255.0,
                ]
            })
            .collect(),
        width,
        height,
        TransferCharacteristic::SRGB,
        ColorPrimaries::BT709,
    )
    .map_err(|error| error.to_string())?;
    let decoded = Rgb::new(
        decoded
            .chunks_exact(3)
            .map(|pixel| {
                [
                    pixel[0] as f32 / 255.0,
                    pixel[1] as f32 / 255.0,
                    pixel[2] as f32 / 255.0,
                ]
            })
            .collect(),
        width,
        height,
        TransferCharacteristic::SRGB,
        ColorPrimaries::BT709,
    )
    .map_err(|error| error.to_string())?;
    compute_frame_ssimulacra2(source, decoded).map_err(|error| error.to_string())
}

fn rgb_psnr(source: &[u8], decoded: &[u8]) -> f64 {
    let squared_error = source
        .iter()
        .zip(decoded)
        .map(|(&source, &decoded)| {
            let error = i32::from(source) - i32::from(decoded);
            (error * error) as u128
        })
        .sum::<u128>();
    if squared_error == 0 {
        return f64::INFINITY;
    }
    let mse = squared_error as f64 / source.len() as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

fn parse_qualities(value: &str) -> Result<Vec<u8>, String> {
    value
        .split(',')
        .map(|part| {
            let quality = part
                .parse::<u8>()
                .map_err(|_| format!("invalid quality `{part}`"))?;
            if quality > 99 {
                return Err(format!("lossy quality must be 0..=99, got {quality}"));
            }
            Ok(quality)
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
