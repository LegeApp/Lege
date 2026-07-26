#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! A dependency-free benchmark harness for the CPU rasterizer (roadmap §15).
//!
//! Run with `cargo bench -p pdf-render-cpu`. It renders synthetic pages of
//! known complexity many times and reports per-stage timing and throughput —
//! the stable signal for iterative performance work on the coverage kernel and
//! compositor. Because it is a plain `main`, there is no external bench
//! framework and no added dependency weight.
//!
//! It is a throughput probe, not a statistical benchmark: compare runs on the
//! same machine, watch the trend, and drill in with a profiler when a change
//! moves the number.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, FillRule, Matrix, PageBounds, PageComplexity,
    PageFeatures, Paint, PaintId, PathData, PathId, PathVerb, Point, Rect,
};
use pdf_render_api::{
    AnnotationMode, Background, OutputFormat, OutputResidency, PageTransform, RenderColorPolicy,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::{CpuBackend, CpuWorkerContext};

/// Tiny deterministic PRNG so the synthetic page is identical across runs.
struct Lcg(u64);
impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32) / (1u64 << 31) as f32
    }
}

/// Build a page of `n` axis-aligned rectangles plus `tris` triangles across a
/// `size`×`size` user space — a coverage-heavy, edge-heavy workload.
fn synthetic_page(size: f64, n: usize, tris: usize) -> CompiledPage {
    let mut rng = Lcg(0x1234_5678);
    let mut verbs: Vec<PathVerb> = Vec::new();
    let mut points: Vec<Point> = Vec::new();
    let mut paths: Vec<PathData> = Vec::new();
    let mut paints: Vec<Paint> = Vec::new();
    let mut ops: Vec<DisplayOp> = Vec::new();

    let push_path =
        |verbs: Vec<PathVerb>, points: Vec<Point>, paths: &mut Vec<PathData>| -> PathId {
            let id = PathId(paths.len() as u32);
            paths.push(PathData {
                verbs: verbs.into(),
                points: points.into(),
            });
            id
        };

    for _ in 0..n {
        let x = rng.next_f32() as f64 * size * 0.9;
        let y = rng.next_f32() as f64 * size * 0.9;
        let w = 4.0 + rng.next_f32() as f64 * (size * 0.15);
        let h = 4.0 + rng.next_f32() as f64 * (size * 0.15);
        verbs.clear();
        points.clear();
        verbs.extend_from_slice(&[
            PathVerb::MoveTo,
            PathVerb::LineTo,
            PathVerb::LineTo,
            PathVerb::LineTo,
            PathVerb::Close,
        ]);
        points.extend_from_slice(&[
            Point { x, y },
            Point { x: x + w, y },
            Point { x: x + w, y: y + h },
            Point { x, y: y + h },
        ]);
        let pid = push_path(verbs.clone(), points.clone(), &mut paths);
        let paint = PaintId(paints.len() as u32);
        paints.push(Paint::Solid(Color {
            r: rng.next_f32(),
            g: rng.next_f32(),
            b: rng.next_f32(),
            a: 1.0,
        }));
        ops.push(DisplayOp::FillPath {
            path: pid,
            paint,
            rule: FillRule::NonZero,
            alpha: 0.6 + rng.next_f32() * 0.4,
            blend: pdf_page_ir::BlendMode::Normal,
        });
    }

    for _ in 0..tris {
        let x = rng.next_f32() as f64 * size;
        let y = rng.next_f32() as f64 * size;
        let r = 10.0 + rng.next_f32() as f64 * (size * 0.2);
        verbs.clear();
        points.clear();
        verbs.extend_from_slice(&[
            PathVerb::MoveTo,
            PathVerb::LineTo,
            PathVerb::LineTo,
            PathVerb::Close,
        ]);
        points.extend_from_slice(&[
            Point { x, y },
            Point {
                x: x + r,
                y: y + r * 0.5,
            },
            Point {
                x: x - r * 0.3,
                y: y + r,
            },
        ]);
        let pid = push_path(verbs.clone(), points.clone(), &mut paths);
        let paint = PaintId(paints.len() as u32);
        paints.push(Paint::Solid(Color {
            r: rng.next_f32(),
            g: rng.next_f32(),
            b: rng.next_f32(),
            a: 1.0,
        }));
        ops.push(DisplayOp::FillPath {
            path: pid,
            paint,
            rule: FillRule::NonZero,
            alpha: 0.8,
            blend: pdf_page_ir::BlendMode::Normal,
        });
    }

    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        content_bounds: None,
        bounds: PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: size,
                y1: size,
            },
            rotate: 0,
        },
        operations: ops.into(),
        paths: paths.into(),
        paints: paints.into(),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::BASIC_PATHS,
        complexity: PageComplexity::default(),
    }
}

fn request(page: Arc<CompiledPage>, dim: u32, quality: RenderQuality) -> RenderRequest {
    RenderRequest {
        page,
        transform: PageTransform {
            matrix: Matrix::IDENTITY,
        },
        crop: None,
        output_size: DeviceSize {
            width: dim,
            height: dim,
        },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations: AnnotationMode::None,
        color_policy: RenderColorPolicy::Original,
        quality,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    }
}

fn bench_case(name: &str, dim: u32, page: Arc<CompiledPage>, quality: RenderQuality, iters: usize) {
    let backend = CpuBackend::default();
    let mut ctx = CpuWorkerContext::new();
    let req = request(page, dim, quality);

    // Warm up (allocations, caches).
    let (_, warm) = backend.render_with(&req, &mut ctx).unwrap();

    let mut raster = Duration::ZERO;
    let mut lower = Duration::ZERO;
    let start = Instant::now();
    for _ in 0..iters {
        let (_, s) = backend.render_with(&req, &mut ctx).unwrap();
        raster += s.raster;
        lower += s.lower;
    }
    let wall = start.elapsed();

    let pps = iters as f64 / wall.as_secs_f64();
    println!(
        "{name:<28} {dim}x{dim} q={:?}  {iters}it  wall {:>7.1}ms  \
         lower {:>5.2}ms/pg  raster {:>6.2}ms/pg  {:>6.1} pg/s  \
         (cmds {}, edges {}, covered {})",
        quality,
        wall.as_secs_f64() * 1e3,
        lower.as_secs_f64() / iters as f64 * 1e3,
        raster.as_secs_f64() / iters as f64 * 1e3,
        pps,
        warm.commands,
        warm.edges,
        warm.covered_pixels,
    );
}

fn main() {
    println!("pdf-render-cpu rasterizer benchmark\n");
    let small = Arc::new(synthetic_page(512.0, 500, 100));
    let big = Arc::new(synthetic_page(1024.0, 2000, 400));

    bench_case(
        "500-rect page",
        512,
        small.clone(),
        RenderQuality::Normal,
        200,
    );
    bench_case(
        "500-rect page (draft)",
        512,
        small,
        RenderQuality::Draft,
        200,
    );
    bench_case(
        "2000-rect page",
        1024,
        big.clone(),
        RenderQuality::Normal,
        50,
    );
    bench_case(
        "2000-rect page (draft)",
        1024,
        big,
        RenderQuality::Draft,
        50,
    );
}
