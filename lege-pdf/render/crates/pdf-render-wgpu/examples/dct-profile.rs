//! Focused real-PDF DCT decode, upload-cache, and image-paint benchmark.
//!
//! Page numbers are zero-based:
//! `WGPU_REQUIRE_REAL_GPU=1 cargo run --release -p pdf-render-wgpu --example dct-profile -- file.pdf 0 2 7`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{DeviceSize, DisplayOp, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, OutputFormat, OutputResidency, PageTransform, RenderBackend,
    RenderColorPolicy, RenderLimits, RenderQuality, RenderRequest, SupportLevel,
};
use pdf_render_cpu::CpuBackend;
use pdf_render_wgpu::WgpuBackend;
use pdf_source::{MmapSource, PdfSource};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(
        args.next()
            .ok_or("usage: dct-profile <file.pdf> [zero-based-page] [scale] [warm-runs]")?,
    );
    let page = args
        .next()
        .and_then(|value| value.to_string_lossy().parse::<u32>().ok())
        .unwrap_or(0);
    let scale = args
        .next()
        .and_then(|value| value.to_string_lossy().parse::<f64>().ok())
        .unwrap_or(2.0);
    let runs = args
        .next()
        .and_then(|value| value.to_string_lossy().parse::<usize>().ok())
        .unwrap_or(7)
        .max(1);

    let source: Arc<dyn PdfSource> = Arc::new(MmapSource::open(&path)?);
    let snapshot = DocumentSnapshot::open(source, DocumentLimits::default())?;
    let mut parse = ParseContext::new();
    let compiled = pdf_content::PageCompiler::new()
        .with_annotations(true)
        .compile(&snapshot, PageIndex(page), &mut parse)?;
    let request = request(Arc::new(compiled), scale);

    let cpu = CpuBackend::default();
    let gpu = WgpuBackend::new()?;
    println!("file: {}", path.display());
    println!("page: {page}, scale: {scale}, runs: {runs}");
    println!("adapter: {:?}", gpu.adapter_info());
    println!(
        "page features: {:?}, operations: {}, images: {}",
        request.page.features,
        request.page.operations.len(),
        request.page.images.len()
    );
    print_image_diagnostics(&request);
    if !matches!(gpu.supports(&request.page, &request), SupportLevel::Native) {
        print_operation_diagnostics(&request);
        return Err("page is not eligible for the experimental RGB8 image-only GPU path".into());
    }

    let cold_cpu_start = Instant::now();
    let cold_cpu = CpuBackend::default().render_to_host(&request)?.0;
    let cold_cpu_elapsed = cold_cpu_start.elapsed();

    let warm_cpu_reference = cpu.render_to_host(&request)?.0;
    let mut cpu_warm_times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let _ = cpu.render_to_host(&request)?;
        cpu_warm_times.push(started.elapsed());
    }

    gpu.clear_upload_cache();
    let (cold_gpu_page, cold_gpu) = gpu.render_to_host_measured(&request)?;
    let mut gpu_warm_total = Vec::with_capacity(runs);
    let mut gpu_warm_prepare = Vec::with_capacity(runs);
    let mut gpu_warm_execute = Vec::with_capacity(runs);
    let mut warm_hits = 0u32;
    let mut warm_reused = 0u64;
    for _ in 0..runs {
        let (_, stats) = gpu.render_to_host_measured(&request)?;
        gpu_warm_total.push(stats.total);
        gpu_warm_prepare.push(stats.prepare);
        gpu_warm_execute.push(stats.gpu_and_readback);
        warm_hits += stats.cache_hits;
        warm_reused += stats.reused_bytes;
    }

    let quality = compare_rgb(&warm_cpu_reference.pixels, &cold_gpu_page.pixels);
    let cpu_warm = median(&mut cpu_warm_times);
    let gpu_warm = median(&mut gpu_warm_total);
    println!("output: {}x{}", cold_gpu_page.width, cold_gpu_page.height);
    println!(
        "cpu cold total:          {:.3} ms",
        millis(cold_cpu_elapsed)
    );
    println!("cpu warm total median:   {:.3} ms", millis(cpu_warm));
    println!(
        "gpu cold prepare:        {:.3} ms",
        millis(cold_gpu.prepare)
    );
    println!(
        "gpu cold paint/readback: {:.3} ms",
        millis(cold_gpu.gpu_and_readback)
    );
    println!(
        "gpu cold total:          {:.3} ms (uploaded {:.2} MiB)",
        millis(cold_gpu.total),
        cold_gpu.uploaded_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "gpu commands:            {} image, {} path; {} edges, {} band refs",
        cold_gpu.image_draws,
        cold_gpu.path_draws,
        cold_gpu.path_edges,
        cold_gpu.band_edge_references
    );
    println!(
        "gpu path batches:        {} batches, {} dispatches, {} active tiles, {} tile refs, depth {}",
        cold_gpu.path_batches,
        cold_gpu.path_dispatches,
        cold_gpu.active_path_tiles,
        cold_gpu.tile_path_references,
        cold_gpu.max_tile_depth
    );
    println!(
        "gpu path packing:        {:.2} MiB geometry, {:.2} MiB masks",
        cold_gpu.packed_path_bytes as f64 / (1024.0 * 1024.0),
        cold_gpu.packed_mask_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "gpu warm prepare median: {:.3} ms",
        millis(median(&mut gpu_warm_prepare))
    );
    println!(
        "gpu warm paint/readback: {:.3} ms",
        millis(median(&mut gpu_warm_execute))
    );
    println!("gpu warm total median:   {:.3} ms", millis(gpu_warm));
    println!(
        "warm speedup vs CPU:     {:.2}x",
        cpu_warm.as_secs_f64() / gpu_warm.as_secs_f64()
    );
    println!(
        "warm upload reuse:       {warm_hits} hit(s), {:.2} MiB",
        warm_reused as f64 / (1024.0 * 1024.0)
    );
    println!(
        "CPU/GPU RGB difference: mean_abs={:.3}, max={}, changed_channels={:.2}%",
        quality.mean_absolute, quality.max_absolute, quality.changed_percent
    );
    println!("cache: {:?}", gpu.upload_cache_telemetry());
    println!("path cache: {:?}", gpu.path_upload_cache_telemetry());
    drop(cold_cpu);
    Ok(())
}

fn print_image_diagnostics(request: &RenderRequest) {
    for (index, image) in request.page.images.iter().enumerate() {
        println!(
            "image {index}: {}x{} bpc={} color={:?} codec={:?} stencil={} \
             legacy-soft-mask={} smask={} mask={:?} smask-in-data={}",
            image.width,
            image.height,
            image.bits_per_component,
            image.color_space,
            image.codec,
            image.is_stencil,
            image.soft_mask.is_some(),
            image.smask.is_some(),
            image.mask,
            image.smask_in_data,
        );
    }
}

fn print_operation_diagnostics(request: &RenderRequest) {
    let mut state = 0usize;
    let mut glyphs = 0usize;
    let mut images = 0usize;
    let mut other = 0usize;
    let mut glyph_modes = [0usize; 8];
    let mut zero_alpha_glyphs = 0usize;
    for operation in request.page.operations.iter() {
        match operation {
            DisplayOp::Save
            | DisplayOp::Restore
            | DisplayOp::ConcatTransform(_)
            | DisplayOp::BeginPaintOrigin(_)
            | DisplayOp::EndPaintOrigin => state += 1,
            DisplayOp::DrawImage { .. } => images += 1,
            DisplayOp::DrawGlyphRun {
                run, alpha, stroke, ..
            } => {
                glyphs += 1;
                let mode = request.page.glyph_runs[run.index()].render_mode as usize;
                if mode < glyph_modes.len() {
                    glyph_modes[mode] += 1;
                }
                let fill_invisible = *alpha <= 0.0;
                let stroke_invisible = stroke.as_ref().is_none_or(|value| value.alpha <= 0.0);
                if fill_invisible && stroke_invisible {
                    zero_alpha_glyphs += 1;
                }
            }
            _ => other += 1,
        }
    }
    eprintln!(
        "ineligible operation summary: state={state}, images={images}, glyphs={glyphs} \
         (zero-alpha={zero_alpha_glyphs}, modes={glyph_modes:?}), other={other}"
    );
}

fn request(page: Arc<pdf_page_ir::CompiledPage>, scale: f64) -> RenderRequest {
    let crop = page.bounds.crop;
    let (crop_width, crop_height) = ((crop.x1 - crop.x0) * scale, (crop.y1 - crop.y0) * scale);
    let (width, height) = match page.bounds.rotate {
        90 | 270 => (crop_height, crop_width),
        _ => (crop_width, crop_height),
    };
    let matrix = display_matrix(&page.bounds, scale);
    RenderRequest {
        page,
        transform: PageTransform { matrix },
        crop: None,
        output_size: DeviceSize {
            width: width.ceil().max(1.0) as u32,
            height: height.ceil().max(1.0) as u32,
        },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        color_policy: RenderColorPolicy::Original,
        annotations: AnnotationMode::StaticAppearances,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    }
}

fn display_matrix(bounds: &pdf_page_ir::PageBounds, scale: f64) -> Matrix {
    let crop = bounds.crop;
    match bounds.rotate {
        90 => Matrix {
            a: 0.0,
            b: scale,
            c: scale,
            d: 0.0,
            e: -crop.y0 * scale,
            f: -crop.x0 * scale,
        },
        180 => Matrix {
            a: -scale,
            b: 0.0,
            c: 0.0,
            d: scale,
            e: crop.x1 * scale,
            f: -crop.y0 * scale,
        },
        270 => Matrix {
            a: 0.0,
            b: -scale,
            c: -scale,
            d: 0.0,
            e: crop.y1 * scale,
            f: crop.x1 * scale,
        },
        _ => Matrix {
            a: scale,
            b: 0.0,
            c: 0.0,
            d: -scale,
            e: -crop.x0 * scale,
            f: crop.y1 * scale,
        },
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
