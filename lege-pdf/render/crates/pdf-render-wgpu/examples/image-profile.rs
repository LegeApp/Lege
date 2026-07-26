//! Focused decoded-RGB image paint benchmark.
//!
//! Run with:
//! `WGPU_REQUIRE_REAL_GPU=1 cargo run --release -p pdf-render-wgpu --example image-profile`

use std::sync::Arc;
use std::time::{Duration, Instant};

use pdf_page_ir::{
    BlendMode, Color, CompiledPage, DeviceSize, DisplayOp, ImageColorSpace, ImageIr,
    InterpolationMode, Matrix, PageBounds, PageComplexity, PageFeatures, Paint, Rect, ResourceKey,
};
use pdf_render_api::{
    AnnotationMode, Background, OutputFormat, OutputResidency, PageTransform, RenderColorPolicy,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;
use pdf_render_wgpu::WgpuBackend;

const SOURCE_WIDTH: u32 = 2400;
const SOURCE_HEIGHT: u32 = 3200;
const OUTPUT_WIDTH: u32 = 1200;
const OUTPUT_HEIGHT: u32 = 1600;
const RUNS: usize = 7;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let request = request();
    let cpu = CpuBackend::default();
    let gpu = WgpuBackend::new()?;
    eprintln!("adapter: {:?}", gpu.adapter_info());

    let cpu_warm = cpu.render_to_host(&request)?.0;
    let gpu_warm = gpu.render_to_host_measured(&request)?.0;
    let quality = compare_rgb(&cpu_warm.pixels, &gpu_warm.pixels);
    gpu.clear_upload_cache();
    let (_, gpu_cold) = gpu.render_to_host_measured(&request)?;

    let mut cpu_times = Vec::with_capacity(RUNS);
    let mut gpu_total = Vec::with_capacity(RUNS);
    let mut gpu_prepare = Vec::with_capacity(RUNS);
    let mut gpu_execute = Vec::with_capacity(RUNS);
    let mut warm_cache_hits = 0u32;
    let mut warm_reused_bytes = 0u64;
    for _ in 0..RUNS {
        let started = Instant::now();
        let _ = cpu.render_to_host(&request)?;
        cpu_times.push(started.elapsed());

        let (_, stats) = gpu.render_to_host_measured(&request)?;
        gpu_total.push(stats.total);
        gpu_prepare.push(stats.prepare);
        gpu_execute.push(stats.gpu_and_readback);
        warm_cache_hits += stats.cache_hits;
        warm_reused_bytes += stats.reused_bytes;
    }

    let cpu_median = median(&mut cpu_times);
    let gpu_median = median(&mut gpu_total);
    println!(
        "decoded RGB8 {}x{} -> {}x{}, {} runs",
        SOURCE_WIDTH, SOURCE_HEIGHT, OUTPUT_WIDTH, OUTPUT_HEIGHT, RUNS
    );
    println!("cpu total median:        {:.3} ms", millis(cpu_median));
    println!(
        "gpu cold total:          {:.3} ms (uploaded {:.2} MiB)",
        millis(gpu_cold.total),
        gpu_cold.uploaded_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "gpu prepare median:      {:.3} ms",
        millis(median(&mut gpu_prepare))
    );
    println!(
        "gpu paint/readback med.: {:.3} ms",
        millis(median(&mut gpu_execute))
    );
    println!("gpu total median:        {:.3} ms", millis(gpu_median));
    println!(
        "gpu warm transfer:       {} cache hit(s), {:.2} MiB reused",
        warm_cache_hits,
        warm_reused_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "end-to-end speedup:      {:.2}x",
        cpu_median.as_secs_f64() / gpu_median.as_secs_f64()
    );
    println!(
        "CPU/GPU RGB difference: mean_abs={:.3}, max={}, changed_channels={:.2}%",
        quality.mean_absolute, quality.max_absolute, quality.changed_percent
    );
    Ok(())
}

fn request() -> RenderRequest {
    let mut samples = vec![0u8; SOURCE_WIDTH as usize * SOURCE_HEIGHT as usize * 3];
    for y in 0..SOURCE_HEIGHT as usize {
        for x in 0..SOURCE_WIDTH as usize {
            let offset = (y * SOURCE_WIDTH as usize + x) * 3;
            let paper = 205 + ((x / 19 + y / 23) % 35) as u8;
            let stroke = x % 173 < 7 || y % 211 < 5;
            samples[offset] = if stroke { 25 } else { paper };
            samples[offset + 1] = if stroke { 30 } else { paper.saturating_sub(8) };
            samples[offset + 2] = if stroke { 38 } else { paper.saturating_sub(18) };
        }
    }
    let image = ImageIr {
        key: ResourceKey {
            object_number: 1,
            generation: 0,
            variant: 0,
        },
        width: SOURCE_WIDTH,
        height: SOURCE_HEIGHT,
        is_stencil: false,
        interpolation: InterpolationMode::Bilinear,
        soft_mask: None,
        bits_per_component: 8,
        color_space: ImageColorSpace::Rgb,
        decode: None,
        samples: Some(Arc::from(samples)),
        codec: None,
        codec_data: None,
        codec_parms: None,
        smask: None,
        mask: None,
        smask_in_data: 0,
        lowering_degraded: false,
    };
    let page = CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: OUTPUT_WIDTH as f64,
                y1: OUTPUT_HEIGHT as f64,
            },
            rotate: 0,
        },
        content_bounds: None,
        operations: Arc::from([
            DisplayOp::ConcatTransform(Matrix::scale(OUTPUT_WIDTH as f64, OUTPUT_HEIGHT as f64)),
            DisplayOp::DrawImage {
                image: pdf_page_ir::ImageId(0),
                paint: pdf_page_ir::PaintId(0),
                transform: Matrix::IDENTITY,
                alpha: 1.0,
                blend: BlendMode::Normal,
            },
        ]),
        paths: Arc::from([]),
        paints: Arc::from([Paint::Solid(Color::BLACK)]),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from([image]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::IMAGES,
        complexity: PageComplexity {
            image_pixels: SOURCE_WIDTH as u64 * SOURCE_HEIGHT as u64,
            ..PageComplexity::default()
        },
    };
    RenderRequest {
        page: Arc::new(page),
        transform: PageTransform {
            matrix: Matrix::IDENTITY,
        },
        crop: None,
        output_size: DeviceSize {
            width: OUTPUT_WIDTH,
            height: OUTPUT_HEIGHT,
        },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        color_policy: RenderColorPolicy::Original,
        annotations: AnnotationMode::None,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    }
}

fn median(values: &mut [Duration]) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

struct QualityDifference {
    mean_absolute: f64,
    max_absolute: u8,
    changed_percent: f64,
}

fn compare_rgb(cpu: &[u8], gpu: &[u8]) -> QualityDifference {
    let mut total = 0u64;
    let mut changed = 0u64;
    let mut max_absolute = 0u8;
    let mut channels = 0u64;
    for (cpu_pixel, gpu_pixel) in cpu.chunks_exact(4).zip(gpu.chunks_exact(4)) {
        for channel in 0..3 {
            let difference = cpu_pixel[channel].abs_diff(gpu_pixel[channel]);
            total += u64::from(difference);
            changed += u64::from(difference != 0);
            max_absolute = max_absolute.max(difference);
            channels += 1;
        }
    }
    QualityDifference {
        mean_absolute: total as f64 / channels as f64,
        max_absolute,
        changed_percent: changed as f64 * 100.0 / channels as f64,
    }
}
