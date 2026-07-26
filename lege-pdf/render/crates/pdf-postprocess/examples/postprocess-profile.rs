#[cfg(not(feature = "gpu"))]
fn main() {
    eprintln!(
        "This profiler requires the GPU executor:\n  \
         cargo run -p pdf-postprocess --release --features gpu \
         --example postprocess-profile -- standard 10 1200 1600"
    );
}

#[cfg(feature = "gpu")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use pdf_postprocess::{
        AdaptivePostprocess, DitherSpec, FusionSpec, GraySpec, OtsuSpec, PostprocessBackend,
        PostprocessGraph, PostprocessOp, PostprocessOutput, PostprocessPreference, ResizeFilter,
        ResizeSpec, ToneCurve,
    };
    use pdf_render_api::{HostPage, OutputFormat, RenderedPage};

    fn output_bytes(output: &PostprocessOutput) -> &[u8] {
        match output {
            PostprocessOutput::Page(RenderedPage::Host(page)) => &page.pixels,
            PostprocessOutput::PackedMono { bits, .. } => bits,
            PostprocessOutput::Page(RenderedPage::Resident(_)) => &[],
        }
    }

    fn digest(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
    }

    fn bench(
        backend: &AdaptivePostprocess,
        page: &HostPage,
        graph: &PostprocessGraph,
        iterations: usize,
    ) -> Result<(Duration, PostprocessOutput), Box<dyn std::error::Error>> {
        let mut last = backend.execute(page, graph)?;
        let started = Instant::now();
        for _ in 0..iterations {
            last = backend.execute(page, graph)?;
        }
        Ok((started.elapsed(), last))
    }

    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("standard");
    let iterations = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(10);
    let width: u32 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(1200);
    let height: u32 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(1600);
    let pixel_count = width as usize * height as usize;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for y in 0..height {
        for x in 0..width {
            let paper = 210 + ((x.wrapping_mul(13) + y.wrapping_mul(7)) % 46) as u8;
            let text = (x / 17 + y / 11) % 19 < 3;
            let value = if text {
                20 + ((x + y) % 50) as u8
            } else {
                paper
            };
            rgba.extend_from_slice(&[value, value.saturating_sub(3), value, 255]);
        }
    }
    let page = HostPage {
        width,
        height,
        stride: width as usize * 4,
        format: OutputFormat::Rgba8PremultipliedSrgb,
        pixels: Arc::from(rgba.into_boxed_slice()),
    };
    let resized_width = (width * 3 / 4).max(1);
    let resized_height = (height * 3 / 4).max(1);
    let mut ops = vec![
        PostprocessOp::ApplyToneCurve(ToneCurve::brightness_contrast(0.01, 0.08)),
        PostprocessOp::ConvertToGray(GraySpec::default()),
        PostprocessOp::Resize(ResizeSpec {
            width: resized_width,
            height: resized_height,
            filter: ResizeFilter::Lanczos3,
        }),
    ];
    match mode {
        "standard" => ops.push(PostprocessOp::Otsu(OtsuSpec::default())),
        "adaptive" => ops.push(PostprocessOp::FuseThresholds(FusionSpec {
            global_weight: 0.35,
            window: 25,
            k: 0.25,
        })),
        "floyd" => ops.push(PostprocessOp::Dither(DitherSpec::FloydSteinberg)),
        _ => {
            return Err(format!("unknown mode `{mode}`; use standard, adaptive, or floyd").into());
        }
    }
    ops.push(PostprocessOp::PackMonochrome);
    let graph = PostprocessGraph { ops };

    let cpu = AdaptivePostprocess::new(PostprocessPreference::Cpu)?;
    let gpu = AdaptivePostprocess::new(PostprocessPreference::Gpu)?;
    let (cpu_elapsed, cpu_output) = bench(&cpu, &page, &graph, iterations)?;
    let (gpu_elapsed, gpu_output) = bench(&gpu, &page, &graph, iterations)?;
    let cpu_bytes = output_bytes(&cpu_output);
    let gpu_bytes = output_bytes(&gpu_output);
    let differing = cpu_bytes
        .iter()
        .zip(gpu_bytes)
        .filter(|(left, right)| left != right)
        .count()
        + cpu_bytes.len().abs_diff(gpu_bytes.len());
    let stats = gpu.last_stats();

    println!("mode={mode} source={width}x{height} iterations={iterations}");
    println!(
        "cpu={:.3}ms/page gpu={:.3}ms/page speedup={:.2}x",
        cpu_elapsed.as_secs_f64() * 1000.0 / iterations as f64,
        gpu_elapsed.as_secs_f64() * 1000.0 / iterations as f64,
        cpu_elapsed.as_secs_f64() / gpu_elapsed.as_secs_f64()
    );
    println!(
        "cpu_hash={:016x} gpu_hash={:016x} differing_bytes={differing}",
        digest(cpu_bytes),
        digest(gpu_bytes)
    );
    if let Some(stats) = stats {
        println!(
            "adapter={} upload={} readback={} last_total={:.3}ms",
            stats.adapter.as_deref().unwrap_or("unknown"),
            stats.uploaded_bytes,
            stats.readback_bytes,
            stats.elapsed.as_secs_f64() * 1000.0
        );
    }
    Ok(())
}
