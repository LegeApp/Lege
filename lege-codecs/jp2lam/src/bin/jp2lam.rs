use chrono::Local;
use image::{DynamicImage, GenericImageView, GrayImage, RgbImage};
use jp2lam::{
    BatchDecoder, BatchEncoder, ColorSpace, Component, DisplayColor, DisplayProfile, EncodeOptions,
    Image, OutputFormat, PerceptualEffort, PerceptualTarget, RateControl, decode_jp2, encode,
};
use std::env;
use std::fs::{self, File};
use std::io::{Cursor, Read, Seek};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

#[derive(Debug, Clone, PartialEq)]
struct CliArgs {
    command: CliCommand,
}

#[derive(Debug, Clone, PartialEq)]
enum CliCommand {
    Encode {
        input_path: PathBuf,
        quality: Option<u8>,
        ssimulacra2: Option<f64>,
        effort: Option<PerceptualEffort>,
        approx: bool,
        display: Option<DisplayProfile>,
    },
    EncodeDir {
        input_dir: PathBuf,
        output_dir: Option<PathBuf>,
        quality: Option<u8>,
        ssimulacra2: Option<f64>,
        effort: Option<PerceptualEffort>,
        approx: bool,
        display: Option<DisplayProfile>,
    },
    Decode {
        input_path: PathBuf,
        output_path: Option<PathBuf>,
    },
    DecodeDir {
        input_dir: PathBuf,
        output_dir: Option<PathBuf>,
    },
    DecodeZip {
        input_path: PathBuf,
        output_dir: Option<PathBuf>,
    },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = parse_args(env::args().skip(1).collect())?;
    match cli.command {
        CliCommand::Encode {
            input_path,
            quality,
            ssimulacra2,
            effort,
            approx,
            display,
        } => run_encode(input_path, quality, ssimulacra2, effort, approx, display),
        CliCommand::EncodeDir {
            input_dir,
            output_dir,
            quality,
            ssimulacra2,
            effort,
            approx,
            display,
        } => run_encode_dir(
            input_dir,
            output_dir,
            quality,
            ssimulacra2,
            effort,
            approx,
            display,
        ),
        CliCommand::Decode {
            input_path,
            output_path,
        } => run_decode(input_path, output_path),
        CliCommand::DecodeDir {
            input_dir,
            output_dir,
        } => run_decode_dir(input_dir, output_dir),
        CliCommand::DecodeZip {
            input_path,
            output_dir,
        } => run_decode_zip(input_path, output_dir),
    }
}

fn run_encode(
    input_path: PathBuf,
    quality: Option<u8>,
    ssimulacra2: Option<f64>,
    effort: Option<PerceptualEffort>,
    approx: bool,
    display: Option<DisplayProfile>,
) -> Result<(), String> {
    if !input_path.exists() {
        return Err(format!("input file not found: {}", input_path.display()));
    }
    let decoded =
        image::open(&input_path).map_err(|err| format!("failed to open input image: {err}"))?;
    let image = to_jp2lam_image(decoded)?;
    let (options, label) =
        encode_options_from_cli(&image, quality, ssimulacra2, effort, approx, display)?;

    let encoded = encode(&image, &options).map_err(|err| format!("encode failed: {err}"))?;

    let output_dir = PathBuf::from("output");
    fs::create_dir_all(&output_dir)
        .map_err(|err| format!("failed to create output directory: {err}"))?;

    let stem = input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("image");
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let output_path = output_dir.join(format!("{stem}_{label}_{timestamp}.jp2"));

    fs::write(&output_path, encoded)
        .map_err(|err| format!("failed to write output file: {err}"))?;

    println!("{label} output={}", output_path.display());
    Ok(())
}

fn run_encode_dir(
    input_dir: PathBuf,
    output_dir: Option<PathBuf>,
    quality: Option<u8>,
    ssimulacra2: Option<f64>,
    effort: Option<PerceptualEffort>,
    approx: bool,
    display: Option<DisplayProfile>,
) -> Result<(), String> {
    if !input_dir.is_dir() {
        return Err(format!(
            "input directory not found: {}",
            input_dir.display()
        ));
    }
    let files = collect_sorted_files(&input_dir, is_supported_encode_input)?;
    if files.is_empty() {
        return Err(format!(
            "input directory contains no supported images: {}",
            input_dir.display()
        ));
    }
    let output_dir = output_dir.unwrap_or_else(|| default_encode_dir_output_dir(&input_dir));
    fs::create_dir_all(&output_dir)
        .map_err(|err| format!("failed to create output directory: {err}"))?;

    let first = image::open(&files[0])
        .map_err(|err| format!("failed to open input image {}: {err}", files[0].display()))?;
    let first_image = to_jp2lam_image(first)?;
    let (options, label) =
        encode_options_from_cli(&first_image, quality, ssimulacra2, effort, approx, display)?;
    let mut encoder = BatchEncoder::new(options);

    for input_path in &files {
        let decoded = image::open(input_path)
            .map_err(|err| format!("failed to open input image {}: {err}", input_path.display()))?;
        let image = to_jp2lam_image(decoded)?;
        encode_dir_image(&mut encoder, &image, input_path, &output_dir, &label)?;
    }

    println!(
        "batch encode count={} {label} output={}",
        encoder.encoded_count(),
        output_dir.display()
    );
    Ok(())
}

fn encode_dir_image(
    encoder: &mut BatchEncoder,
    image: &Image,
    input_path: &Path,
    output_dir: &Path,
    label: &str,
) -> Result<(), String> {
    let encoded = encoder
        .encode_one(image)
        .map_err(|err| format!("encode failed for {}: {err}", input_path.display()))?;
    let output_path = output_dir.join(format!("{}_{label}.jp2", file_stem(input_path)));
    fs::write(&output_path, encoded).map_err(|err| {
        format!(
            "failed to write output file {}: {err}",
            output_path.display()
        )
    })?;
    println!(
        "encoded {} -> {}",
        input_path.display(),
        output_path.display()
    );
    Ok(())
}

fn run_decode(input_path: PathBuf, output_path: Option<PathBuf>) -> Result<(), String> {
    if !input_path.exists() {
        return Err(format!("input file not found: {}", input_path.display()));
    }
    let bytes = fs::read(&input_path).map_err(|err| format!("failed to read input file: {err}"))?;
    let image = decode_jp2(&bytes).map_err(|err| format!("decode failed: {err}"))?;
    let output_path = output_path.unwrap_or_else(|| default_decode_output_path(&input_path));
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create output directory: {err}"))?;
        }
    }
    write_image_png(&image, &output_path)?;
    println!("decoded output={}", output_path.display());
    Ok(())
}

fn run_decode_dir(input_dir: PathBuf, output_dir: Option<PathBuf>) -> Result<(), String> {
    if !input_dir.is_dir() {
        return Err(format!(
            "input directory not found: {}",
            input_dir.display()
        ));
    }
    let files = collect_sorted_files(&input_dir, is_supported_decode_input)?;
    if files.is_empty() {
        return Err(format!(
            "input directory contains no JP2/J2K files: {}",
            input_dir.display()
        ));
    }
    let output_dir = output_dir.unwrap_or_else(|| default_decode_dir_output_dir(&input_dir));
    fs::create_dir_all(&output_dir)
        .map_err(|err| format!("failed to create output directory: {err}"))?;

    let mut decoder = BatchDecoder::new();
    for input_path in files {
        let bytes = fs::read(&input_path)
            .map_err(|err| format!("failed to read input file {}: {err}", input_path.display()))?;
        let image = decoder
            .decode_one(&bytes)
            .map_err(|err| format!("decode failed for {}: {err}", input_path.display()))?;
        let output_path = output_dir.join(format!("{}.png", file_stem(&input_path)));
        write_image_png(&image, &output_path)?;
        println!(
            "decoded {} -> {}",
            input_path.display(),
            output_path.display()
        );
    }

    println!(
        "batch decode count={} output={}",
        decoder.decoded_count(),
        output_dir.display()
    );
    Ok(())
}

#[derive(Debug, Default)]
struct ZipDecodeStats {
    archives: usize,
    decoded: usize,
    failed: usize,
}

fn run_decode_zip(input_path: PathBuf, output_dir: Option<PathBuf>) -> Result<(), String> {
    if !input_path.exists() {
        return Err(format!("input archive not found: {}", input_path.display()));
    }
    let output_dir = output_dir.unwrap_or_else(|| default_decode_zip_output_dir(&input_path));
    fs::create_dir_all(&output_dir)
        .map_err(|err| format!("failed to create output directory: {err}"))?;

    let file = File::open(&input_path).map_err(|err| {
        format!(
            "failed to open input archive {}: {err}",
            input_path.display()
        )
    })?;
    let mut stats = ZipDecodeStats::default();
    decode_zip_reader(file, &output_dir, Path::new(""), &mut stats)?;

    println!(
        "zip decode archives={} decoded={} failed={} output={}",
        stats.archives,
        stats.decoded,
        stats.failed,
        output_dir.display()
    );
    if stats.failed > 0 {
        Err(format!(
            "decoded {} JP2 images but {} JP2 images failed",
            stats.decoded, stats.failed
        ))
    } else if stats.decoded == 0 {
        Err("archive did not contain any JP2 images".to_string())
    } else {
        Ok(())
    }
}

fn decode_zip_reader<R: Read + Seek>(
    reader: R,
    output_dir: &Path,
    archive_prefix: &Path,
    stats: &mut ZipDecodeStats,
) -> Result<(), String> {
    stats.archives += 1;
    let mut archive =
        ZipArchive::new(reader).map_err(|err| format!("failed to read ZIP archive: {err}"))?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| format!("failed to read ZIP entry {index}: {err}"))?;
        if entry.is_dir() {
            continue;
        }
        let Some(entry_path) = entry.enclosed_name().map(PathBuf::from) else {
            eprintln!("skipping unsafe ZIP entry path: {}", entry.name());
            continue;
        };
        let lower_name = entry_path.to_string_lossy().to_ascii_lowercase();
        if lower_name.ends_with(".jp2")
            || lower_name.ends_with(".j2k")
            || lower_name.ends_with(".j2c")
        {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|err| format!("failed to read {}: {err}", entry_path.display()))?;
            let output_path = decoded_entry_output_path(output_dir, archive_prefix, &entry_path);
            match decode_jp2(&bytes) {
                Ok(image) => {
                    if let Some(parent) = output_path.parent() {
                        fs::create_dir_all(parent).map_err(|err| {
                            format!(
                                "failed to create output directory {}: {err}",
                                parent.display()
                            )
                        })?;
                    }
                    write_image_png(&image, &output_path)?;
                    stats.decoded += 1;
                    println!(
                        "decoded {} -> {}",
                        entry_path.display(),
                        output_path.display()
                    );
                }
                Err(err) => {
                    stats.failed += 1;
                    eprintln!("decode failed {}: {err}", entry_path.display());
                }
            }
        } else if lower_name.ends_with(".zip") {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(|err| {
                format!("failed to read nested ZIP {}: {err}", entry_path.display())
            })?;
            let nested_prefix = archive_prefix.join(strip_zip_extension(&entry_path));
            decode_zip_reader(Cursor::new(bytes), output_dir, &nested_prefix, stats)?;
        }
    }

    Ok(())
}

fn decoded_entry_output_path(
    output_dir: &Path,
    archive_prefix: &Path,
    entry_path: &Path,
) -> PathBuf {
    let entry_path = entry_path_without_duplicate_archive_dir(archive_prefix, entry_path);
    let relative = archive_prefix.join(entry_path);
    output_dir.join(relative).with_extension("png")
}

fn entry_path_without_duplicate_archive_dir<'a>(
    archive_prefix: &Path,
    entry_path: &'a Path,
) -> &'a Path {
    let Some(prefix_name) = archive_prefix.file_name() else {
        return entry_path;
    };
    let mut components = entry_path.components();
    let Some(std::path::Component::Normal(first)) = components.next() else {
        return entry_path;
    };
    if first == prefix_name {
        components.as_path()
    } else {
        entry_path
    }
}

fn strip_zip_extension(path: &Path) -> PathBuf {
    let mut stripped = path.to_path_buf();
    if stripped
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        stripped.set_extension("");
    }
    stripped
}

fn default_decode_output_path(input_path: &Path) -> PathBuf {
    let stem = file_stem(input_path);
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    PathBuf::from("output").join(format!("{stem}_decoded_{timestamp}.png"))
}

fn default_decode_zip_output_dir(input_path: &Path) -> PathBuf {
    let stem = file_stem(input_path);
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    PathBuf::from("output").join(format!("{stem}_decoded_{timestamp}"))
}

fn default_encode_dir_output_dir(input_dir: &Path) -> PathBuf {
    let stem = file_stem(input_dir);
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    PathBuf::from("output").join(format!("{stem}_encoded_{timestamp}"))
}

fn default_decode_dir_output_dir(input_dir: &Path) -> PathBuf {
    let stem = file_stem(input_dir);
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    PathBuf::from("output").join(format!("{stem}_decoded_{timestamp}"))
}

/// Scale one decoded sample to an 8-bit PNG value. `Image` components carry
/// raw samples at their coded precision, so a 1-bit bitonal scan arrives as
/// 0/1 and a 16-bit component as 0..65535; clamping either to 0..255 would
/// write a black page or a blown-out one. Same rounding as the library's
/// packed-u8 path (`scale_unsigned_to_u8`), so the two outputs agree.
fn sample_to_u8(sample: i32, precision: u32) -> u8 {
    let max_sample = (1u32 << precision.clamp(1, 31)) - 1;
    let sample = sample.clamp(0, max_sample as i32) as u32;
    if max_sample == 255 {
        sample as u8
    } else {
        ((u64::from(sample) * 255 + u64::from(max_sample) / 2) / u64::from(max_sample.max(1)))
            .min(255) as u8
    }
}

fn write_image_png(image: &Image, output_path: &Path) -> Result<(), String> {
    match image.colorspace {
        ColorSpace::Gray => write_gray_png(image, output_path),
        ColorSpace::Rgb | ColorSpace::Srgb => write_rgb_png(image, output_path),
        ColorSpace::Cmyk => write_cmyk_png(image, output_path),
        ColorSpace::Yuv | ColorSpace::YCbCr => Err(format!(
            "PNG output for {:?} decoded images is not implemented",
            image.colorspace
        )),
    }
}

/// PNG output for a decoded CMYK image via the naive multiplicative
/// CMYK→RGB conversion (`R = (255-C)(255-K)/255`, …). This is a diagnostic
/// preview, not a color-managed conversion — the PDF pipeline maps the raw
/// CMYK samples through its own DeviceCMYK table.
fn write_cmyk_png(image: &Image, output_path: &Path) -> Result<(), String> {
    if image.components.len() < 4 {
        return Err("decoded CMYK image has fewer than 4 components".to_string());
    }
    let pixel_count = image
        .width
        .checked_mul(image.height)
        .ok_or_else(|| "decoded image dimensions overflow".to_string())?
        as usize;
    let (c, m, y, k) = (
        &image.components[0].data,
        &image.components[1].data,
        &image.components[2].data,
        &image.components[3].data,
    );
    if [c, m, y, k].iter().any(|p| p.len() != pixel_count) {
        return Err("decoded CMYK component length does not match image dimensions".to_string());
    }
    let mut bytes = Vec::with_capacity(pixel_count * 3);
    for i in 0..pixel_count {
        let kk = 255 - i32::from(sample_to_u8(k[i], image.components[3].precision));
        for (index, ch) in [c, m, y].into_iter().enumerate() {
            let inv = 255 - i32::from(sample_to_u8(ch[i], image.components[index].precision));
            bytes.push(((inv * kk) / 255) as u8);
        }
    }
    let png = RgbImage::from_raw(image.width, image.height, bytes).ok_or_else(|| {
        "decoded CMYK component length does not match image dimensions".to_string()
    })?;
    png.save(output_path)
        .map_err(|err| format!("failed to write PNG: {err}"))
}

fn write_gray_png(image: &Image, output_path: &Path) -> Result<(), String> {
    let component = image
        .components
        .first()
        .ok_or_else(|| "decoded grayscale image has no component".to_string())?;
    let bytes = component
        .data
        .iter()
        .map(|&sample| sample_to_u8(sample, component.precision))
        .collect::<Vec<_>>();
    let png = GrayImage::from_raw(image.width, image.height, bytes).ok_or_else(|| {
        "decoded grayscale component length does not match image dimensions".to_string()
    })?;
    png.save(output_path)
        .map_err(|err| format!("failed to write PNG: {err}"))
}

fn write_rgb_png(image: &Image, output_path: &Path) -> Result<(), String> {
    if image.components.len() < 3 {
        return Err("decoded RGB image has fewer than 3 components".to_string());
    }
    let pixel_count = image
        .width
        .checked_mul(image.height)
        .ok_or_else(|| "decoded image dimensions overflow".to_string())?
        as usize;
    let r = &image.components[0].data;
    let g = &image.components[1].data;
    let b = &image.components[2].data;
    if r.len() != pixel_count || g.len() != pixel_count || b.len() != pixel_count {
        return Err("decoded RGB component length does not match image dimensions".to_string());
    }
    let mut bytes = Vec::with_capacity(pixel_count * 3);
    for i in 0..pixel_count {
        for (plane, component) in [r, g, b].into_iter().zip(&image.components) {
            bytes.push(sample_to_u8(plane[i], component.precision));
        }
    }
    let png = RgbImage::from_raw(image.width, image.height, bytes).ok_or_else(|| {
        "decoded RGB component length does not match image dimensions".to_string()
    })?;
    png.save(output_path)
        .map_err(|err| format!("failed to write PNG: {err}"))
}

fn collect_sorted_files(
    input_dir: &Path,
    predicate: fn(&Path) -> bool,
) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for entry in fs::read_dir(input_dir)
        .map_err(|err| format!("failed to read directory {}: {err}", input_dir.display()))?
    {
        let entry = entry.map_err(|err| format!("failed to read directory entry: {err}"))?;
        let path = entry.path();
        if path.is_file() && predicate(&path) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn is_supported_encode_input(path: &Path) -> bool {
    matches!(
        extension_lower(path).as_deref(),
        Some("png" | "jpg" | "jpeg" | "bmp" | "tif" | "tiff" | "pnm" | "pgm" | "ppm")
    )
}

fn is_supported_decode_input(path: &Path) -> bool {
    matches!(
        extension_lower(path).as_deref(),
        Some("jp2" | "j2k" | "j2c")
    )
}

fn extension_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("image")
        .to_string()
}

fn parse_args(args: Vec<String>) -> Result<CliArgs, String> {
    let mut args = args.as_slice();
    if let [bin_name, rest @ ..] = args {
        if bin_name.eq_ignore_ascii_case("jp2lam") {
            args = rest;
        }
    }

    if let Some((command, rest)) = args.split_first() {
        if command == "encode" {
            return parse_encode_args(rest);
        }
        if command == "encode-dir" || command == "encode_dir" {
            return parse_encode_dir_args(rest);
        }
        if command == "decode" {
            return parse_decode_args(rest);
        }
        if command == "decode-dir" || command == "decode_dir" {
            return parse_decode_dir_args(rest);
        }
        if command == "decode-zip" || command == "decode_zip" {
            return parse_decode_zip_args(rest);
        }
    }
    parse_encode_args(args)
}

fn parse_encode_args(args: &[String]) -> Result<CliArgs, String> {
    let usage = usage();

    let mut input_path: Option<PathBuf> = None;
    let mut quality: Option<u8> = None;
    let mut ssimulacra2: Option<f64> = None;
    let mut effort: Option<PerceptualEffort> = None;
    let mut approx = false;
    let mut display: Option<DisplayProfile> = None;
    let mut resample = false;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--approx" {
            approx = true;
            i += 1;
            continue;
        }
        if arg == "--resample" {
            resample = true;
            i += 1;
            continue;
        }
        if arg == "--quality" || arg == "-q" {
            let Some(next) = args.get(i + 1) else {
                return Err(format!("{usage}\nmissing value after {arg}"));
            };
            quality = Some(parse_quality(next)?);
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--quality=") {
            quality = Some(parse_quality(value)?);
            i += 1;
            continue;
        }
        if let Some((score, consumed)) = parse_ssimulacra2_flag(args, i)? {
            ssimulacra2 = Some(score);
            i += consumed;
            continue;
        }
        if let Some((parsed, consumed)) = parse_effort_flag(args, i)? {
            effort = Some(parsed);
            i += consumed;
            continue;
        }
        if let Some((parsed, consumed)) = parse_display_flag(args, i)? {
            display = Some(parsed);
            i += consumed;
            continue;
        }

        if is_quality_token(arg) {
            quality = Some(parse_quality(arg)?);
        } else if input_path.is_none() {
            input_path = Some(PathBuf::from(arg));
        } else {
            return Err(format!("{usage}\nunrecognized extra argument: {arg}"));
        }
        i += 1;
    }

    let Some(input_path) = input_path else {
        return Err(usage.to_string());
    };
    reject_quality_and_ssim_together(quality, ssimulacra2, effort, approx, display)?;
    let display = apply_resample_flag(display, resample)?;

    Ok(CliArgs {
        command: CliCommand::Encode {
            input_path,
            quality,
            ssimulacra2,
            effort,
            approx,
            display,
        },
    })
}

fn parse_encode_dir_args(args: &[String]) -> Result<CliArgs, String> {
    let usage = usage();

    let mut input_dir: Option<PathBuf> = None;
    let mut output_dir: Option<PathBuf> = None;
    let mut quality: Option<u8> = None;
    let mut ssimulacra2: Option<f64> = None;
    let mut effort: Option<PerceptualEffort> = None;
    let mut approx = false;
    let mut display: Option<DisplayProfile> = None;
    let mut resample = false;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--approx" {
            approx = true;
            i += 1;
            continue;
        }
        if arg == "--resample" {
            resample = true;
            i += 1;
            continue;
        }
        if arg == "--quality" || arg == "-q" {
            let Some(next) = args.get(i + 1) else {
                return Err(format!("{usage}\nmissing value after {arg}"));
            };
            quality = Some(parse_quality(next)?);
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--quality=") {
            quality = Some(parse_quality(value)?);
            i += 1;
            continue;
        }
        if let Some((score, consumed)) = parse_ssimulacra2_flag(args, i)? {
            ssimulacra2 = Some(score);
            i += consumed;
            continue;
        }
        if let Some((parsed, consumed)) = parse_effort_flag(args, i)? {
            effort = Some(parsed);
            i += consumed;
            continue;
        }
        if let Some((parsed, consumed)) = parse_display_flag(args, i)? {
            display = Some(parsed);
            i += consumed;
            continue;
        }
        if is_quality_token(arg) {
            quality = Some(parse_quality(arg)?);
        } else if input_dir.is_none() {
            input_dir = Some(PathBuf::from(arg));
        } else if output_dir.is_none() {
            output_dir = Some(PathBuf::from(arg));
        } else {
            return Err(format!(
                "{usage}\nunrecognized extra encode-dir argument: {arg}"
            ));
        }
        i += 1;
    }

    let Some(input_dir) = input_dir else {
        return Err(usage.to_string());
    };
    reject_quality_and_ssim_together(quality, ssimulacra2, effort, approx, display)?;
    let display = apply_resample_flag(display, resample)?;
    Ok(CliArgs {
        command: CliCommand::EncodeDir {
            input_dir,
            output_dir,
            quality,
            ssimulacra2,
            effort,
            approx,
            display,
        },
    })
}

fn parse_decode_args(args: &[String]) -> Result<CliArgs, String> {
    let usage = usage();
    let [input, rest @ ..] = args else {
        return Err(usage.to_string());
    };
    let output_path = match rest {
        [] => None,
        [output] => Some(PathBuf::from(output)),
        [extra, ..] => {
            return Err(format!(
                "{usage}\nunrecognized extra decode argument: {extra}"
            ));
        }
    };
    Ok(CliArgs {
        command: CliCommand::Decode {
            input_path: PathBuf::from(input),
            output_path,
        },
    })
}

fn parse_decode_dir_args(args: &[String]) -> Result<CliArgs, String> {
    let usage = usage();
    let [input, rest @ ..] = args else {
        return Err(usage.to_string());
    };
    let output_dir = match rest {
        [] => None,
        [output] => Some(PathBuf::from(output)),
        [extra, ..] => {
            return Err(format!(
                "{usage}\nunrecognized extra decode-dir argument: {extra}"
            ));
        }
    };
    Ok(CliArgs {
        command: CliCommand::DecodeDir {
            input_dir: PathBuf::from(input),
            output_dir,
        },
    })
}

fn parse_decode_zip_args(args: &[String]) -> Result<CliArgs, String> {
    let usage = usage();
    let [input, rest @ ..] = args else {
        return Err(usage.to_string());
    };
    let output_dir = match rest {
        [] => None,
        [output] => Some(PathBuf::from(output)),
        [extra, ..] => {
            return Err(format!(
                "{usage}\nunrecognized extra decode-zip argument: {extra}"
            ));
        }
    };
    Ok(CliArgs {
        command: CliCommand::DecodeZip {
            input_path: PathBuf::from(input),
            output_dir,
        },
    })
}

fn usage() -> &'static str {
    "usage: cargo run --features cli --bin jp2lam -- <input> [q0..q100 [--effort fast|balanced|quality] | --approx q0..q100 | --ssimulacra2 <score> [--effort fast|balanced|quality]] [--display eink:WxH|tablet:WxH [--resample]]\n       cargo run --features cli --bin jp2lam -- encode <input> [q0..q100 [--effort fast|balanced|quality] | --approx q0..q100 | --ssimulacra2 <score> [--effort fast|balanced|quality]] [--display eink:WxH|tablet:WxH [--resample]]\n       cargo run --features cli --bin jp2lam -- encode-dir <input-dir> [output-dir] [q0..q100 [--effort fast|balanced|quality] | --approx q0..q100 | --ssimulacra2 <score> [--effort fast|balanced|quality]] [--display eink:WxH|tablet:WxH [--resample]]\n       cargo run --features cli --bin jp2lam -- decode <input.jp2> [output.png]\n       cargo run --features cli --bin jp2lam -- decode-dir <input-dir> [output-dir]\n       cargo run --features cli --bin jp2lam -- decode-zip <archive.zip> [output-dir]"
}

fn encode_options_from_cli(
    image: &Image,
    quality: Option<u8>,
    ssimulacra2: Option<f64>,
    effort: Option<PerceptualEffort>,
    approx: bool,
    display: Option<DisplayProfile>,
) -> Result<(EncodeOptions, String), String> {
    // A display target measures the floor at the size and color mode the reader
    // will actually see, and is capped at one encode plus one correction.
    if let Some(display) = display {
        let effort = effort.unwrap_or(PerceptualEffort::Fast);
        let score = ssimulacra2
            .or_else(|| quality.map(f64::from))
            .ok_or("--display needs a quality (qN) or --ssimulacra2")?;
        let target =
            PerceptualTarget::for_display(score, display, effort).map_err(|err| err.to_string())?;
        let options = EncodeOptions {
            rate_control: Some(RateControl::Perceptual(target)),
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        };
        let label = format!(
            "s{score}-{}{}x{}-{}{}",
            display_label(display.color),
            display.max_width,
            display.max_height,
            effort_label(effort),
            if display.resample_source {
                "-resampled"
            } else {
                ""
            }
        );
        return Ok((options, label));
    }
    if let Some(score) = ssimulacra2 {
        let effort = effort.unwrap_or_default();
        let target = PerceptualTarget::new(score, effort).map_err(|err| err.to_string())?;
        let options = EncodeOptions {
            rate_control: Some(RateControl::Perceptual(target)),
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        };
        let label = format!("ssim{score}-{}", effort_label(effort));
        return Ok((options, label));
    }
    // Explicit qN is the canonical verified scale (SSIMULACRA2 floor of N); the
    // content classifier's auto number and --approx are the legacy open-loop preset.
    if let Some(level) = quality.filter(|_| !approx) {
        if level >= 100 {
            let options = EncodeOptions {
                quality: 100,
                format: OutputFormat::Jp2,
                profile: Default::default(),
                ..Default::default()
            };
            return Ok((options, "lossless".to_string()));
        }
        let effort = effort.unwrap_or_default();
        let options = EncodeOptions {
            rate_control: Some(RateControl::Quality { level, effort }),
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        };
        return Ok((options, format!("q{level}-{}", effort_label(effort))));
    }
    let (quality, auto_note) = match quality {
        Some(q) => (q, String::new()),
        None => {
            let (class, q) = jp2lam::content::auto_class_and_quality(image);
            (q, format!(" (auto: {})", class.label()))
        }
    };
    let options = EncodeOptions {
        quality,
        format: OutputFormat::Jp2,
        profile: Default::default(),
        ..Default::default()
    };
    Ok((options, format!("approx-q{quality}{auto_note}")))
}

fn effort_label(effort: PerceptualEffort) -> &'static str {
    match effort {
        PerceptualEffort::Fast => "fast",
        PerceptualEffort::Balanced => "balanced",
        PerceptualEffort::Quality => "quality",
    }
}

fn display_label(color: DisplayColor) -> &'static str {
    match color {
        DisplayColor::Eink => "eink",
        DisplayColor::Tablet => "tablet",
    }
}

/// `--resample` only means anything alongside `--display`: it moves the encode
/// itself down to the panel box, so the emitted image has the display size.
fn apply_resample_flag(
    display: Option<DisplayProfile>,
    resample: bool,
) -> Result<Option<DisplayProfile>, String> {
    if !resample {
        return Ok(display);
    }
    let Some(profile) = display else {
        return Err("--resample needs --display".into());
    };
    Ok(Some(profile.resampling_source()))
}

fn parse_display_flag(
    args: &[String],
    i: usize,
) -> Result<Option<(DisplayProfile, usize)>, String> {
    let arg = &args[i];
    if arg == "--display" {
        let Some(next) = args.get(i + 1) else {
            return Err("missing value after --display".into());
        };
        return Ok(Some((parse_display(next)?, 2)));
    }
    if let Some(value) = arg.strip_prefix("--display=") {
        return Ok(Some((parse_display(value)?, 1)));
    }
    Ok(None)
}

/// `eink:WxH` or `tablet:WxH`: the largest size the reader ever renders the
/// image at, and the panel's color mode.
fn parse_display(value: &str) -> Result<DisplayProfile, String> {
    let invalid = || format!("invalid --display `{value}`, expected eink:WxH or tablet:WxH");
    let lowered = value.trim().to_ascii_lowercase();
    let (mode, size) = lowered.split_once(':').ok_or_else(invalid)?;
    let (width, height) = size.split_once('x').ok_or_else(invalid)?;
    let width = width.parse::<u32>().map_err(|_| invalid())?;
    let height = height.parse::<u32>().map_err(|_| invalid())?;
    if width == 0 || height == 0 {
        return Err(format!(
            "--display size must be non-zero, got {width}x{height}"
        ));
    }
    match mode {
        "eink" => Ok(DisplayProfile::eink(width, height)),
        "tablet" => Ok(DisplayProfile::tablet(width, height)),
        _ => Err(invalid()),
    }
}

fn parse_ssimulacra2_flag(args: &[String], i: usize) -> Result<Option<(f64, usize)>, String> {
    let arg = &args[i];
    if arg == "--ssimulacra2" {
        let Some(next) = args.get(i + 1) else {
            return Err("missing value after --ssimulacra2".into());
        };
        return Ok(Some((parse_ssim_score(next)?, 2)));
    }
    if let Some(value) = arg.strip_prefix("--ssimulacra2=") {
        return Ok(Some((parse_ssim_score(value)?, 1)));
    }
    Ok(None)
}

fn parse_effort_flag(
    args: &[String],
    i: usize,
) -> Result<Option<(PerceptualEffort, usize)>, String> {
    let arg = &args[i];
    if arg == "--effort" {
        let Some(next) = args.get(i + 1) else {
            return Err("missing value after --effort".into());
        };
        return Ok(Some((parse_effort(next)?, 2)));
    }
    if let Some(value) = arg.strip_prefix("--effort=") {
        return Ok(Some((parse_effort(value)?, 1)));
    }
    Ok(None)
}

fn parse_ssim_score(value: &str) -> Result<f64, String> {
    let score = value
        .parse::<f64>()
        .map_err(|_| format!("invalid SSIMULACRA2 score `{value}`"))?;
    if !score.is_finite() || !(0.0..=100.0).contains(&score) {
        return Err(format!(
            "SSIMULACRA2 score must be finite and in 0..=100, got {value}"
        ));
    }
    Ok(score)
}

fn parse_effort(value: &str) -> Result<PerceptualEffort, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "fast" => Ok(PerceptualEffort::Fast),
        "balanced" => Ok(PerceptualEffort::Balanced),
        "quality" => Ok(PerceptualEffort::Quality),
        _ => Err(format!(
            "invalid effort `{value}`; expected fast, balanced, or quality"
        )),
    }
}

fn reject_quality_and_ssim_together(
    quality: Option<u8>,
    ssimulacra2: Option<f64>,
    effort: Option<PerceptualEffort>,
    approx: bool,
    display: Option<DisplayProfile>,
) -> Result<(), String> {
    if quality.is_some() && ssimulacra2.is_some() {
        return Err("cannot combine --quality with --ssimulacra2".into());
    }
    if approx && (ssimulacra2.is_some() || effort.is_some()) {
        return Err(
            "--approx is the unmeasured legacy preset; it takes no --effort or --ssimulacra2"
                .into(),
        );
    }
    if approx && display.is_some() {
        return Err("--approx is the unmeasured legacy preset; it takes no --display".into());
    }
    if display.is_some() && quality.is_none() && ssimulacra2.is_none() {
        return Err("--display needs a quality (qN) or --ssimulacra2".into());
    }
    if effort.is_some() && quality.is_none() && ssimulacra2.is_none() {
        return Err(
            "--effort needs a quality (qN) or --ssimulacra2; auto quality is an approximate preset"
                .into(),
        );
    }
    Ok(())
}

fn is_quality_token(value: &str) -> bool {
    let trimmed = value.trim();
    let number = trimmed
        .strip_prefix('q')
        .or_else(|| trimmed.strip_prefix('Q'))
        .unwrap_or(trimmed);
    !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())
}

fn parse_quality(value: &str) -> Result<u8, String> {
    let trimmed = value.trim();
    let number = trimmed
        .strip_prefix('q')
        .or_else(|| trimmed.strip_prefix('Q'))
        .unwrap_or(trimmed);
    let quality = number
        .parse::<u8>()
        .map_err(|_| format!("invalid quality `{value}`; expected q0..q100"))?;
    if quality > 100 {
        return Err(format!("invalid quality `{value}`; expected q0..q100"));
    }
    Ok(quality)
}

fn to_jp2lam_image(decoded: DynamicImage) -> Result<Image, String> {
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return Err("input image has zero width or height".to_string());
    }

    let channels = decoded.color().channel_count();
    if channels <= 2 {
        let gray = decoded.to_luma8();
        let data = gray.as_raw().iter().map(|&v| i32::from(v)).collect();
        Ok(Image {
            width,
            height,
            components: vec![Component {
                data,
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }],
            colorspace: ColorSpace::Gray,
        })
    } else {
        let rgb = decoded.to_rgb8();
        let mut r = Vec::with_capacity((width * height) as usize);
        let mut g = Vec::with_capacity((width * height) as usize);
        let mut b = Vec::with_capacity((width * height) as usize);
        for px in rgb.as_raw().chunks_exact(3) {
            r.push(i32::from(px[0]));
            g.push(i32::from(px[1]));
            b.push(i32::from(px[2]));
        }
        Ok(Image {
            width,
            height,
            components: vec![
                component_from_u8_plane(r, width, height),
                component_from_u8_plane(g, width, height),
                component_from_u8_plane(b, width, height),
            ],
            colorspace: ColorSpace::Srgb,
        })
    }
}

fn component_from_u8_plane(data: Vec<i32>, width: u32, height: u32) -> Component {
    Component {
        data,
        width,
        height,
        precision: 8,
        signed: false,
        dx: 1,
        dy: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{CliCommand, DisplayProfile, PerceptualEffort, parse_args, parse_quality};
    use std::path::PathBuf;

    #[test]
    fn parses_plain_input_with_default_quality() {
        let args = parse_args(vec!["file.png".to_string()]).expect("args");
        assert_eq!(
            args.command,
            CliCommand::Encode {
                input_path: PathBuf::from("file.png"),
                quality: None,
                ssimulacra2: None,
                effort: None,
                approx: false,
                display: None,
            }
        );
    }

    #[test]
    fn parses_one_shot_quality_forms() {
        for token in ["q50", "Q50", "50", "--quality=q50"] {
            let args = parse_args(vec!["file.png".to_string(), token.to_string()]).expect(token);
            assert_eq!(
                args.command,
                CliCommand::Encode {
                    input_path: PathBuf::from("file.png"),
                    quality: Some(50),
                    ssimulacra2: None,
                    effort: None,
                    approx: false,
                    display: None,
                }
            );
        }

        let args = parse_args(vec![
            "jp2lam".to_string(),
            "encode".to_string(),
            "file.png".to_string(),
            "--quality".to_string(),
            "75".to_string(),
        ])
        .expect("explicit quality");
        assert_eq!(
            args.command,
            CliCommand::Encode {
                input_path: PathBuf::from("file.png"),
                quality: Some(75),
                ssimulacra2: None,
                effort: None,
                approx: false,
                display: None,
            }
        );
    }

    #[test]
    fn parses_decode_subcommand() {
        let args =
            parse_args(vec!["decode".to_string(), "page.jp2".to_string()]).expect("decode args");
        assert_eq!(
            args.command,
            CliCommand::Decode {
                input_path: PathBuf::from("page.jp2"),
                output_path: None
            }
        );

        let args = parse_args(vec![
            "jp2lam".to_string(),
            "decode".to_string(),
            "page.jp2".to_string(),
            "page.png".to_string(),
        ])
        .expect("decode with output");
        assert_eq!(
            args.command,
            CliCommand::Decode {
                input_path: PathBuf::from("page.jp2"),
                output_path: Some(PathBuf::from("page.png"))
            }
        );
    }

    #[test]
    fn parses_encode_dir_subcommand() {
        let args = parse_args(vec![
            "encode-dir".to_string(),
            "pages".to_string(),
            "encoded".to_string(),
            "q85".to_string(),
        ])
        .expect("encode dir args");
        assert_eq!(
            args.command,
            CliCommand::EncodeDir {
                input_dir: PathBuf::from("pages"),
                output_dir: Some(PathBuf::from("encoded")),
                quality: Some(85),
                ssimulacra2: None,
                effort: None,
                approx: false,
                display: None,
            }
        );
    }

    #[test]
    fn parses_decode_dir_subcommand() {
        let args = parse_args(vec![
            "decode-dir".to_string(),
            "jp2".to_string(),
            "decoded".to_string(),
        ])
        .expect("decode dir args");
        assert_eq!(
            args.command,
            CliCommand::DecodeDir {
                input_dir: PathBuf::from("jp2"),
                output_dir: Some(PathBuf::from("decoded"))
            }
        );
    }

    #[test]
    fn parses_decode_zip_subcommand() {
        let args = parse_args(vec![
            "decode-zip".to_string(),
            "pages.zip".to_string(),
            "decoded".to_string(),
        ])
        .expect("decode zip args");
        assert_eq!(
            args.command,
            CliCommand::DecodeZip {
                input_path: PathBuf::from("pages.zip"),
                output_dir: Some(PathBuf::from("decoded"))
            }
        );
    }

    #[test]
    fn rejects_quality_outside_range() {
        assert!(parse_quality("q101").is_err());
        assert!(parse_quality("qabc").is_err());
    }

    #[test]
    fn parses_ssimulacra2_and_effort() {
        let args = parse_args(vec![
            "encode".to_string(),
            "file.png".to_string(),
            "--ssimulacra2".to_string(),
            "90".to_string(),
            "--effort".to_string(),
            "fast".to_string(),
        ])
        .expect("ssim args");
        assert_eq!(
            args.command,
            CliCommand::Encode {
                input_path: PathBuf::from("file.png"),
                quality: None,
                ssimulacra2: Some(90.0),
                effort: Some(PerceptualEffort::Fast),
                approx: false,
                display: None,
            }
        );
    }

    #[test]
    fn rejects_quality_combined_with_ssimulacra2() {
        let err = parse_args(vec![
            "file.png".to_string(),
            "q50".to_string(),
            "--ssimulacra2=90".to_string(),
        ])
        .expect_err("combined");
        assert!(err.contains("cannot combine"), "{err}");
    }

    #[test]
    fn quality_with_effort_and_approx_parse() {
        let args = parse_args(vec![
            "file.png".to_string(),
            "q80".to_string(),
            "--effort".to_string(),
            "balanced".to_string(),
        ])
        .expect("quality with effort");
        assert_eq!(
            args.command,
            CliCommand::Encode {
                input_path: PathBuf::from("file.png"),
                quality: Some(80),
                ssimulacra2: None,
                effort: Some(PerceptualEffort::Balanced),
                approx: false,
                display: None,
            }
        );
        let args = parse_args(vec![
            "file.png".to_string(),
            "--approx".to_string(),
            "q80".to_string(),
        ])
        .expect("approx preset");
        assert_eq!(
            args.command,
            CliCommand::Encode {
                input_path: PathBuf::from("file.png"),
                quality: Some(80),
                ssimulacra2: None,
                effort: None,
                approx: true,
                display: None,
            }
        );
        let err = parse_args(vec![
            "file.png".to_string(),
            "--approx".to_string(),
            "q80".to_string(),
            "--effort=fast".to_string(),
        ])
        .expect_err("approx takes no effort");
        assert!(err.contains("--approx"), "{err}");
        let err = parse_args(vec!["file.png".to_string(), "--effort=fast".to_string()])
            .expect_err("effort needs a quality");
        assert!(err.contains("--effort needs"), "{err}");
    }

    #[test]
    fn parses_display_targets() {
        let args = parse_args(vec![
            "file.png".to_string(),
            "q75".to_string(),
            "--display".to_string(),
            "eink:400x300".to_string(),
        ])
        .expect("display target");
        assert_eq!(
            args.command,
            CliCommand::Encode {
                input_path: PathBuf::from("file.png"),
                quality: Some(75),
                ssimulacra2: None,
                effort: None,
                display: Some(DisplayProfile::eink(400, 300)),
                approx: false,
            }
        );

        let args = parse_args(vec![
            "file.png".to_string(),
            "--ssimulacra2=72.5".to_string(),
            "--display=TABLET:1024x768".to_string(),
        ])
        .expect("tablet target");
        let CliCommand::Encode { display, .. } = args.command else {
            panic!("expected encode");
        };
        assert_eq!(display, Some(DisplayProfile::tablet(1024, 768)));

        for bad in ["eink", "eink:0x300", "phone:400x300", "eink:400"] {
            assert!(
                parse_args(vec![
                    "file.png".to_string(),
                    "q75".to_string(),
                    format!("--display={bad}"),
                ])
                .is_err(),
                "{bad} must be rejected"
            );
        }
        let err = parse_args(vec![
            "file.png".to_string(),
            "--display=eink:400x300".to_string(),
        ])
        .expect_err("no floor");
        assert!(err.contains("--display needs a quality"), "{err}");
    }

    #[test]
    fn display_target_defaults_to_fast_effort() {
        let image = super::Image::from_gray_bytes(8, 8, &[128u8; 64]).expect("gray");
        let (options, label) = super::encode_options_from_cli(
            &image,
            Some(75),
            None,
            None,
            false,
            Some(DisplayProfile::eink(400, 300)),
        )
        .expect("options");
        let Some(jp2lam::RateControl::Perceptual(target)) = options.rate_control else {
            panic!("expected a perceptual target");
        };
        assert_eq!(target.effort, PerceptualEffort::Fast);
        assert_eq!(target.score, 75.0);
        assert_eq!(target.display, Some(DisplayProfile::eink(400, 300)));
        assert_eq!(label, "s75-eink400x300-fast");
    }
}
