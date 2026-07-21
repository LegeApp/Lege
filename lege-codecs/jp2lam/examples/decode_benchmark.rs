use jp2lam::{
    ColorSpace, Component, DecodeOutputFormat, DecodeRegion, DecodeRequest, DecodeResolution,
    DecodeResult, EncodeOptions, Image, Jp2DecodeStats, Jp2Decoder, OutputFormat, decode_jp2,
    decode_jp2_with_stats, encode,
};
use std::hint::black_box;
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    let iterations = args
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .or_else(|| {
            std::env::var("JP2LAM_DECODE_BENCH_ITERS")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(7)
        .max(1);

    let (label, encoded) = match first {
        Some(path) => {
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|error| panic!("failed to read {path}: {error}"));
            (path, bytes)
        }
        None => {
            let width = env_dimension("JP2LAM_DECODE_BENCH_WIDTH", 2048);
            let height = env_dimension("JP2LAM_DECODE_BENCH_HEIGHT", 2048);
            let image = synthetic_gray(width, height);
            // Optional fixed tiling (JP2LAM_DECODE_BENCH_TILE=<edge>) so region
            // decode can be measured against a multi-tile fixture, where the
            // tile-skip path scales toward the region area.
            let options = match std::env::var("JP2LAM_DECODE_BENCH_TILE")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|&edge| edge > 0)
            {
                Some(edge) => EncodeOptions {
                    tile_policy: jp2lam::TilePolicy::Fixed {
                        width: edge,
                        height: edge,
                    },
                    ..EncodeOptions::photo(75, OutputFormat::Jp2)
                },
                None => EncodeOptions::photo(75, OutputFormat::Jp2),
            };
            let bytes = encode(&image, &options).expect("encode synthetic benchmark fixture");
            (format!("synthetic-gray-{width}x{height}-q75"), bytes)
        }
    };
    if let Some(path) = std::env::var_os("JP2LAM_DECODE_BENCH_FIXTURE_OUT") {
        std::fs::write(&path, &encoded)
            .unwrap_or_else(|error| panic!("failed to write {}: {error}", path.to_string_lossy()));
    }

    let warm = decode_jp2(black_box(&encoded)).expect("warm-up decode");
    let expected_hash = image_hash(&warm);
    drop(warm);

    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let image = decode_jp2(black_box(&encoded)).expect("benchmark decode");
        let elapsed = start.elapsed();
        assert_eq!(image_hash(black_box(&image)), expected_hash);
        samples.push(elapsed);
    }
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let total: Duration = samples.iter().sum();
    let mean = total / samples.len() as u32;
    let megapixels = f64::from(warm_dimensions(&encoded).0)
        * f64::from(warm_dimensions(&encoded).1)
        / 1_000_000.0;

    println!("fixture={label}");
    println!("compressed_bytes={}", encoded.len());
    println!("iterations={iterations}");
    println!("median_ms={:.3}", median.as_secs_f64() * 1_000.0);
    println!("mean_ms={:.3}", mean.as_secs_f64() * 1_000.0);
    println!(
        "median_megapixels_per_second={:.3}",
        megapixels / median.as_secs_f64()
    );
    println!("output_hash={expected_hash:016x}");

    let (profiled, stats) = decode_jp2_with_stats(&encoded).expect("profiled decode");
    assert_eq!(image_hash(&profiled), expected_hash);
    print_stats(&stats);

    if let Some(reduce) = std::env::var("JP2LAM_DECODE_BENCH_REDUCE")
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
    {
        benchmark_reduced(&encoded, iterations, reduce);
    }

    if std::env::var_os("JP2LAM_DECODE_BENCH_REGION").is_some() {
        benchmark_region(&encoded, iterations);
    }
}

/// Compare a full packed decode against a centered quarter-area region decode
/// (half width, half height). Region decode time should scale toward the region
/// area rather than the full image area.
fn benchmark_region(encoded: &[u8], iterations: usize) {
    let (width, height) = warm_dimensions(encoded);
    let region = DecodeRegion {
        x: width / 4,
        y: height / 4,
        width: (width / 2).max(1),
        height: (height / 2).max(1),
    };
    let mut decoder = Jp2Decoder::new();

    let full_request = DecodeRequest {
        output: DecodeOutputFormat::Gray8,
        ..Default::default()
    };
    let region_request = DecodeRequest {
        output: DecodeOutputFormat::Gray8,
        region: Some(region),
        ..Default::default()
    };

    fn median(
        decoder: &mut Jp2Decoder,
        encoded: &[u8],
        request: &DecodeRequest,
        iterations: usize,
    ) -> (Duration, u32, u32) {
        // Warm-up.
        let _ = decoder.decode(encoded, request).expect("warm region decode");
        let mut samples = Vec::with_capacity(iterations);
        let mut dims = (0u32, 0u32);
        for _ in 0..iterations {
            let start = Instant::now();
            let result = decoder.decode(black_box(encoded), request).expect("decode");
            samples.push(start.elapsed());
            if let DecodeResult::Raster(raster) = &result {
                dims = (raster.width, raster.height);
            }
        }
        samples.sort_unstable();
        (samples[samples.len() / 2], dims.0, dims.1)
    }

    let (full_ms, full_w, full_h) = median(&mut decoder, encoded, &full_request, iterations);
    let (region_ms, region_w, region_h) =
        median(&mut decoder, encoded, &region_request, iterations);
    println!("region.full_ms={:.3}", full_ms.as_secs_f64() * 1_000.0);
    println!("region.full_dims={full_w}x{full_h}");
    println!("region.region_ms={:.3}", region_ms.as_secs_f64() * 1_000.0);
    println!("region.region_dims={region_w}x{region_h}");
    println!(
        "region.area_fraction={:.3}",
        f64::from(region_w) * f64::from(region_h) / (f64::from(full_w) * f64::from(full_h)).max(1.0)
    );
    println!(
        "region.time_fraction={:.3}",
        region_ms.as_secs_f64() / full_ms.as_secs_f64().max(f64::MIN_POSITIVE)
    );
}

fn warm_dimensions(encoded: &[u8]) -> (u32, u32) {
    let metadata = jp2lam::inspect_jp2(encoded).expect("inspect benchmark fixture");
    (metadata.width, metadata.height)
}

fn env_dimension(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&value| value > 0)
        .unwrap_or(default)
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

fn benchmark_reduced(encoded: &[u8], iterations: usize, reduce: u8) {
    let request = DecodeRequest {
        resolution: DecodeResolution::ReduceLevels(reduce),
        output: DecodeOutputFormat::Gray8,
        ..Default::default()
    };
    let mut decoder = Jp2Decoder::new();
    let DecodeResult::Raster(warm) = decoder
        .decode(encoded, &request)
        .expect("warm reduced decode")
    else {
        panic!("reduced packed request returned a native image");
    };
    let expected_hash = bytes_hash(&warm.data);
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let DecodeResult::Raster(raster) = decoder
            .decode(black_box(encoded), &request)
            .expect("reduced decode")
        else {
            panic!("reduced packed request returned a native image");
        };
        samples.push(start.elapsed());
        assert_eq!(bytes_hash(black_box(&raster.data)), expected_hash);
    }
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    println!("reduced.levels={reduce}");
    println!("reduced.width={}", warm.width);
    println!("reduced.height={}", warm.height);
    println!("reduced.median_ms={:.3}", median.as_secs_f64() * 1_000.0);
    println!("reduced.output_hash={expected_hash:016x}");
}

fn bytes_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}
