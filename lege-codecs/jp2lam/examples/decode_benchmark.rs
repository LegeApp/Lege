//! Decode-side micro-benchmark for jp2lam.
//!
//! Defaults measure the **renderer-relevant** path: a persistent [`Jp2Decoder`]
//! session writing packed 8-bit output into caller-owned storage with a
//! budgeted worker count — not the compatibility `decode_jp2()` planar/serial
//! path (which remains available via `JP2LAM_DECODE_BENCH_LEGACY=1`).
//!
//! Usage:
//! ```text
//! cargo run --release --example decode_benchmark -- [fixture.jp2|fixture.png]
//! ```
//!
//! Environment:
//! - `JP2LAM_DECODE_BENCH_ITERS` (default 7)
//! - `JP2LAM_DECODE_BENCH_THREADS` (default 4; use 1 for serial)
//! - `JP2LAM_DECODE_BENCH_OUTPUT` = `gray8` | `rgb8` | `rgbx8` | `bgra8` | `native` (default auto)
//! - `JP2LAM_DECODE_BENCH_REDUCE` = discard N highest wavelet levels
//! - `JP2LAM_DECODE_BENCH_REGION` = set to measure quarter-image ROI
//! - `JP2LAM_DECODE_BENCH_LEGACY=1` = time `decode_jp2` planar serial
//! - `JP2LAM_DECODE_BENCH_FIXTURE_OUT` = write encoded JP2 bytes
//! - `JP2LAM_DECODE_BENCH_WIDTH` / `HEIGHT` for synthetic gray when no file given

use jp2lam::{
    ColorSpace, Component, DecodeConcurrency, DecodeOutputFormat, DecodeRegion, DecodeRequest,
    DecodeResolution, DecodeResult, DecodeTarget, EncodeOptions, Image, Jp2DecodeStats, Jp2Decoder,
    OutputFormat, decode_jp2, decode_jp2_with_stats, encode, inspect_jp2,
};
use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    let iterations = env_usize("JP2LAM_DECODE_BENCH_ITERS", 7).max(1);
    let threads = env_usize("JP2LAM_DECODE_BENCH_THREADS", 4).max(1);
    let legacy = env_flag("JP2LAM_DECODE_BENCH_LEGACY");

    let (label, encoded) = match first {
        Some(path) => load_or_encode_fixture(&path),
        None => {
            let width = env_dimension("JP2LAM_DECODE_BENCH_WIDTH", 2048);
            let height = env_dimension("JP2LAM_DECODE_BENCH_HEIGHT", 2048);
            let image = synthetic_gray(width, height);
            let options = tile_options_from_env(approx_photo(75, OutputFormat::Jp2));
            let bytes = encode(&image, &options).expect("encode synthetic benchmark fixture");
            (format!("synthetic-gray-{width}x{height}-q75"), bytes)
        }
    };
    if let Some(path) = std::env::var_os("JP2LAM_DECODE_BENCH_FIXTURE_OUT") {
        std::fs::write(&path, &encoded)
            .unwrap_or_else(|error| panic!("failed to write {}: {error}", path.to_string_lossy()));
        println!("wrote_fixture={}", path.to_string_lossy());
    }

    let meta = inspect_jp2(&encoded).expect("inspect fixture");
    let (width, height) = (meta.width, meta.height);
    let megapixels = f64::from(width) * f64::from(height) / 1_000_000.0;
    let auto_output = auto_output_format(&meta);
    let output = parse_output_format(auto_output);
    let concurrency = if threads <= 1 {
        DecodeConcurrency::Serial
    } else {
        DecodeConcurrency::Budgeted(threads)
    };
    let reduce = std::env::var("JP2LAM_DECODE_BENCH_REDUCE")
        .ok()
        .and_then(|value| value.parse::<u8>().ok());
    let resolution = match reduce {
        Some(levels) => DecodeResolution::ReduceLevels(levels),
        None => DecodeResolution::Full,
    };

    println!("fixture={label}");
    println!("source_dims={width}x{height}");
    println!("components={}", meta.codestream.siz.components.len());
    println!("colorspace={:?}", meta.colorspace);
    println!("compressed_bytes={}", encoded.len());
    println!("iterations={iterations}");
    println!("threads={threads}");
    println!("legacy_planar_serial={legacy}");
    println!("output={output:?}");
    println!("resolution={resolution:?}");

    if legacy {
        run_legacy_planar(&encoded, iterations, megapixels);
    } else {
        run_session_path(
            &encoded,
            iterations,
            megapixels,
            &DecodeRequest {
                resolution,
                output,
                concurrency,
                ..Default::default()
            },
        );
    }

    // Stage breakdown uses the stats path (intentionally serializes Tier-1);
    // report it separately so it is not confused with production timing.
    let (profiled, stats) = decode_jp2_with_stats(&encoded).expect("profiled decode");
    println!("stats_note=stats_path_serializes_tier1_not_production_wall_time");
    print_stats(&stats);
    drop(profiled);

    if let Some(levels) = reduce {
        benchmark_reduced(&encoded, iterations, levels, threads, output);
    }

    if env_flag("JP2LAM_DECODE_BENCH_REGION") {
        benchmark_region(&encoded, iterations, threads, output);
    }
}

fn load_or_encode_fixture(path: &str) -> (String, Vec<u8>) {
    let bytes =
        std::fs::read(path).unwrap_or_else(|error| panic!("failed to read {path}: {error}"));
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "jp2" | "j2k" | "j2c" | "jpc") {
        return (path.to_string(), bytes);
    }
    // PNG / other raster: encode as irreversible photo JP2 (~quality 75 ≈ 20:1 class).
    let image = load_raster_image(&bytes, path);
    let quality = env_usize("JP2LAM_DECODE_BENCH_QUALITY", 75).min(100) as u8;
    let options = tile_options_from_env(approx_photo(quality, OutputFormat::Jp2));
    let encoded = encode(&image, &options).expect("encode raster to JP2");
    (
        format!("{path}->jp2-q{quality}-{}x{}", image.width, image.height),
        encoded,
    )
}

fn load_raster_image(bytes: &[u8], path: &str) -> Image {
    let dyn_img = image::load_from_memory(bytes)
        .unwrap_or_else(|error| panic!("failed to decode raster {path}: {error}"));
    let rgb = dyn_img.to_rgb8();
    let (width, height) = rgb.dimensions();
    Image::from_rgb_bytes(width, height, rgb.as_raw()).expect("build Image from RGB")
}

fn tile_options_from_env(base: EncodeOptions) -> EncodeOptions {
    match std::env::var("JP2LAM_DECODE_BENCH_TILE")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|&edge| edge > 0)
    {
        Some(edge) => EncodeOptions {
            tile_policy: jp2lam::TilePolicy::Fixed {
                width: edge,
                height: edge,
            },
            ..base
        },
        None => base,
    }
}

fn auto_output_format(meta: &jp2lam::DecodeMetadata) -> DecodeOutputFormat {
    match std::env::var("JP2LAM_DECODE_BENCH_OUTPUT")
        .ok()
        .as_deref()
        .map(str::trim)
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("gray8") => DecodeOutputFormat::Gray8,
        Some("rgb8") => DecodeOutputFormat::Rgb8,
        Some("rgbx8") => DecodeOutputFormat::Rgbx8,
        Some("bgra8") => DecodeOutputFormat::Bgra8,
        Some("native") => DecodeOutputFormat::NativePlanarI32,
        Some(other) => panic!("unknown JP2LAM_DECODE_BENCH_OUTPUT={other}"),
        None => match meta.colorspace {
            ColorSpace::Gray => DecodeOutputFormat::Gray8,
            ColorSpace::Srgb | ColorSpace::YCbCr => DecodeOutputFormat::Rgb8,
            ColorSpace::Cmyk => DecodeOutputFormat::Cmyk8,
            _ => DecodeOutputFormat::NativePlanarI32,
        },
    }
}

fn parse_output_format(format: DecodeOutputFormat) -> DecodeOutputFormat {
    format
}

fn run_legacy_planar(encoded: &[u8], iterations: usize, megapixels: f64) {
    let warm = decode_jp2(black_box(encoded)).expect("warm-up decode");
    let expected_hash = image_hash(&warm);
    drop(warm);

    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let image = decode_jp2(black_box(encoded)).expect("benchmark decode");
        samples.push(start.elapsed());
        assert_eq!(image_hash(black_box(&image)), expected_hash);
    }
    report_timings("legacy_planar_serial", &samples, megapixels, expected_hash);
}

fn run_session_path(encoded: &[u8], iterations: usize, megapixels: f64, request: &DecodeRequest) {
    let metadata = inspect_jp2(encoded).expect("inspect session fixture");
    if request.output != DecodeOutputFormat::NativePlanarI32
        && metadata.container_palette_channels.is_none()
    {
        let (width, height) = metadata
            .decoded_dimensions(request.resolution)
            .expect("select output dimensions");
        let stride = width as usize * request.output.bytes_per_pixel();
        let mut output = vec![0u8; stride * height as usize];
        let mut decoder = Jp2Decoder::new();
        decoder
            .decode_into(
                encoded,
                request,
                DecodeTarget {
                    data: &mut output,
                    width,
                    height,
                    stride,
                    format: request.output,
                    premultiplied: false,
                },
            )
            .expect("warm session decode_into");
        let expected_hash = bytes_hash(&output);

        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let start = Instant::now();
            decoder
                .decode_into(
                    black_box(encoded),
                    request,
                    DecodeTarget {
                        data: black_box(&mut output),
                        width,
                        height,
                        stride,
                        format: request.output,
                        premultiplied: false,
                    },
                )
                .expect("session decode_into");
            samples.push(start.elapsed());
            assert_eq!(bytes_hash(black_box(&output)), expected_hash);
        }
        report_timings("session_packed_into", &samples, megapixels, expected_hash);
        return;
    }

    let mut decoder = Jp2Decoder::new();
    let warm = decoder
        .decode(encoded, request)
        .expect("warm session decode");
    let expected_hash = result_hash(&warm);
    drop(warm);

    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let result = decoder
            .decode(black_box(encoded), request)
            .expect("session decode");
        samples.push(start.elapsed());
        assert_eq!(result_hash(black_box(&result)), expected_hash);
    }
    report_timings("session_packed", &samples, megapixels, expected_hash);
}

fn report_timings(label: &str, samples: &[Duration], megapixels: f64, hash: u64) {
    let mut samples = samples.to_vec();
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let total: Duration = samples.iter().sum();
    let mean = total / samples.len() as u32;
    println!("path={label}");
    println!("median_ms={:.3}", median.as_secs_f64() * 1_000.0);
    println!("mean_ms={:.3}", mean.as_secs_f64() * 1_000.0);
    println!(
        "median_megapixels_per_second={:.3}",
        megapixels / median.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    println!("output_hash={hash:016x}");
}

fn benchmark_region(encoded: &[u8], iterations: usize, threads: usize, output: DecodeOutputFormat) {
    let (width, height) = warm_dimensions(encoded);
    let region = DecodeRegion {
        x: width / 4,
        y: height / 4,
        width: (width / 2).max(1),
        height: (height / 2).max(1),
    };
    let concurrency = if threads <= 1 {
        DecodeConcurrency::Serial
    } else {
        DecodeConcurrency::Budgeted(threads)
    };
    let mut decoder = Jp2Decoder::new();
    let full_request = DecodeRequest {
        output,
        concurrency,
        ..Default::default()
    };
    let region_request = DecodeRequest {
        output,
        region: Some(region),
        concurrency,
        ..Default::default()
    };

    let (full_ms, full_w, full_h) =
        median_request(&mut decoder, encoded, &full_request, iterations);
    let (region_ms, region_w, region_h) =
        median_request(&mut decoder, encoded, &region_request, iterations);
    println!("region.full_ms={:.3}", full_ms.as_secs_f64() * 1_000.0);
    println!("region.full_dims={full_w}x{full_h}");
    println!("region.region_ms={:.3}", region_ms.as_secs_f64() * 1_000.0);
    println!("region.region_dims={region_w}x{region_h}");
    println!(
        "region.area_fraction={:.3}",
        f64::from(region_w) * f64::from(region_h)
            / (f64::from(full_w) * f64::from(full_h)).max(1.0)
    );
    println!(
        "region.time_fraction={:.3}",
        region_ms.as_secs_f64() / full_ms.as_secs_f64().max(f64::MIN_POSITIVE)
    );
}

fn median_request(
    decoder: &mut Jp2Decoder,
    encoded: &[u8],
    request: &DecodeRequest,
    iterations: usize,
) -> (Duration, u32, u32) {
    let _ = decoder.decode(encoded, request).expect("warm");
    let mut samples = Vec::with_capacity(iterations);
    let mut dims = (0u32, 0u32);
    for _ in 0..iterations {
        let start = Instant::now();
        let result = decoder.decode(black_box(encoded), request).expect("decode");
        samples.push(start.elapsed());
        dims = result_dims(&result);
    }
    samples.sort_unstable();
    (samples[samples.len() / 2], dims.0, dims.1)
}

fn warm_dimensions(encoded: &[u8]) -> (u32, u32) {
    let metadata = inspect_jp2(encoded).expect("inspect benchmark fixture");
    (metadata.width, metadata.height)
}

fn env_dimension(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&value| value > 0)
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&value| value > 0)
        .unwrap_or(default)
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

fn synthetic_gray(width: u32, height: u32) -> Image {
    let mut data = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        for x in 0..width {
            let gradient = (x * 173 / width.max(1) + y * 79 / height.max(1)) as i32;
            let checker = ((((x / 11) ^ (y / 7)) & 1) * 29) as i32;
            let texture = ((x.wrapping_mul(17) ^ y.wrapping_mul(31)) & 15) as i32;
            data.push((gradient + checker + texture).clamp(0, 255));
        }
    }
    Image {
        width,
        height,
        colorspace: ColorSpace::Gray,
        components: vec![Component {
            data,
            width,
            height,
            precision: 8,
            signed: false,
            dx: 1,
            dy: 1,
        }],
    }
}

fn image_hash(image: &Image) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for component in &image.components {
        for &sample in &component.data {
            for byte in sample.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    hash
}

fn result_hash(result: &DecodeResult) -> u64 {
    match result {
        DecodeResult::Native(image) => image_hash(image),
        DecodeResult::Raster(raster) => bytes_hash(&raster.data),
    }
}

fn result_dims(result: &DecodeResult) -> (u32, u32) {
    match result {
        DecodeResult::Native(image) => (image.width, image.height),
        DecodeResult::Raster(raster) => (raster.width, raster.height),
    }
}

fn print_stats(stats: &Jp2DecodeStats) {
    let ms = |ns: u64| ns as f64 / 1_000_000.0;
    println!(
        "stats.container_parse_ms={:.3}",
        ms(stats.container_parse_ns)
    );
    println!(
        "stats.codestream_parse_ms={:.3}",
        ms(stats.codestream_parse_ns)
    );
    println!("stats.tile_plan_ms={:.3}", ms(stats.tile_plan_ns));
    println!("stats.tier2_setup_ms={:.3}", ms(stats.tier2_setup_ns));
    println!(
        "stats.tier2_packets_ms={:.3}",
        ms(stats.tier2_packet_headers_ns)
    );
    println!("stats.tier2_concat_ms={:.3}", ms(stats.tier2_concat_ns));
    println!("stats.tier1_ms={:.3}", ms(stats.tier1_total_ns));
    println!(
        "stats.tier1_significance_ms={:.3}",
        ms(stats.tier1_significance_ns)
    );
    println!(
        "stats.tier1_refinement_ms={:.3}",
        ms(stats.tier1_refinement_ns)
    );
    println!("stats.tier1_cleanup_ms={:.3}", ms(stats.tier1_cleanup_ns));
    println!(
        "stats.tier1_block_output_ms={:.3}",
        ms(stats.tier1_block_output_ns)
    );
    println!("stats.dequantize_ms={:.3}", ms(stats.dequantize_ns));
    println!("stats.dwt_ms={:.3}", ms(stats.dwt_total_ns));
    println!("stats.dwt_horizontal_ms={:.3}", ms(stats.dwt_horizontal_ns));
    println!("stats.dwt_vertical_ms={:.3}", ms(stats.dwt_vertical_ns));
    println!(
        "stats.dwt_levels_ms={:?}",
        stats
            .dwt_level_ns
            .iter()
            .map(|&ns| ms(ns))
            .collect::<Vec<_>>()
    );
    println!("stats.inverse_mct_ms={:.3}", ms(stats.inverse_mct_ns));
    println!("stats.finalize_ms={:.3}", ms(stats.finalize_ns));
    println!("stats.tile_stitch_ms={:.3}", ms(stats.tile_stitch_ns));
    println!("stats.packets={}", stats.packets);
    println!("stats.codeblocks={}", stats.codeblocks);
    println!("stats.codeword_bytes={}", stats.codeword_bytes);
    println!("stats.coefficient_pixels={}", stats.coefficient_pixels);
    println!("stats.output_pixels={}", stats.output_pixels);
    println!("stats.peak_scratch_bytes={}", stats.peak_scratch_bytes);
}

fn benchmark_reduced(
    encoded: &[u8],
    iterations: usize,
    reduce: u8,
    threads: usize,
    output: DecodeOutputFormat,
) {
    let concurrency = if threads <= 1 {
        DecodeConcurrency::Serial
    } else {
        DecodeConcurrency::Budgeted(threads)
    };
    let request = DecodeRequest {
        resolution: DecodeResolution::ReduceLevels(reduce),
        output,
        concurrency,
        ..Default::default()
    };
    let mut decoder = Jp2Decoder::new();
    let warm = decoder.decode(encoded, &request).expect("warm reduced");
    let expected_hash = result_hash(&warm);
    let (w, h) = result_dims(&warm);
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let result = decoder
            .decode(black_box(encoded), &request)
            .expect("reduced decode");
        samples.push(start.elapsed());
        assert_eq!(result_hash(black_box(&result)), expected_hash);
    }
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    println!("reduced.levels={reduce}");
    println!("reduced.width={w}");
    println!("reduced.height={h}");
    println!("reduced.median_ms={:.3}", median.as_secs_f64() * 1_000.0);
    println!("reduced.output_hash={expected_hash:016x}");
}

fn bytes_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Legacy open-loop photo preset: this harness compares rates, not verified quality.
fn approx_photo(quality: u8, format: jp2lam::OutputFormat) -> jp2lam::EncodeOptions {
    let mut options = jp2lam::EncodeOptions::photo(quality, format);
    if quality < 100 {
        options.rate_control = Some(jp2lam::RateControl::ApproxQuality(quality));
    }
    options
}
