#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 4a tests: the CPU backend paints solid fills from a CompiledPage,
//! anti-aliased, with correct alpha compositing and deterministic output.

use std::sync::Arc;

use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, FillRule, Matrix, PageBounds, PageComplexity,
    PageFeatures, Paint, PathData, PathVerb, Point, Rect,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;

/// Build a CompiledPage with one solid-fill rectangle `[x0,y0]..[x1,y1]`.
#[allow(clippy::too_many_arguments)]
fn rect_page(size: f64, x0: f64, y0: f64, x1: f64, y1: f64, color: Color, alpha: f32, rule: FillRule) -> CompiledPage {
    let verbs: Arc<[PathVerb]> =
        Arc::from([PathVerb::MoveTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::Close]);
    let points: Arc<[Point]> = Arc::from([
        Point { x: x0, y: y0 },
        Point { x: x1, y: y0 },
        Point { x: x1, y: y1 },
        Point { x: x0, y: y1 },
    ]);
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds { crop: Rect { x0: 0.0, y0: 0.0, x1: size, y1: size }, rotate: 0 },
        operations: Arc::from([DisplayOp::FillPath {
            path: pdf_page_ir::PathId(0),
            paint: pdf_page_ir::PaintId(0),
            rule,
            alpha,
            blend: pdf_page_ir::BlendMode::Normal,
        }]),
        paths: Arc::from([PathData { verbs, points }]),
        paints: Arc::from([Paint::Solid(color)]),
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

fn request(page: CompiledPage, dim: u32, background: Background) -> RenderRequest {
    RenderRequest {
        page: Arc::new(page),
        // Identity transform: IR user space maps 1:1 to device pixels (y-down).
        transform: PageTransform { matrix: Matrix::IDENTITY },
        crop: None,
        output_size: DeviceSize { width: dim, height: dim },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background,
        annotations: AnnotationMode::None,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    }
}

fn px(host: &HostPage, x: usize, y: usize) -> [u8; 4] {
    let i = y * host.stride + x * 4;
    [host.pixels[i], host.pixels[i + 1], host.pixels[i + 2], host.pixels[i + 3]]
}

#[test]
fn opaque_fill_paints_interior_and_leaves_background() {
    let page = rect_page(10.0, 2.0, 2.0, 8.0, 8.0, Color::from_rgb(1.0, 0.0, 0.0), 1.0, FillRule::NonZero);
    let backend = CpuBackend::default();
    let (host, stats) = backend.render_to_host(&request(page, 10, Background::White)).unwrap();

    assert_eq!(px(&host, 5, 5), [255, 0, 0, 255], "interior is opaque red");
    assert_eq!(px(&host, 0, 0), [255, 255, 255, 255], "corner stays white");
    assert_eq!(stats.commands, 1);
    assert_eq!(stats.ops_painted, 1);
    // Integer-aligned + opaque → the OpaqueRect fast path (direct row fill, no
    // coverage generation), so no edges are rasterized.
    assert_eq!(stats.edges, 0, "opaque integer rect uses the fast path");
}

#[test]
fn constant_alpha_composites_over_white() {
    // 50% red over white → premultiplied [255,128,128,255] (pink).
    let page = rect_page(10.0, 2.0, 2.0, 8.0, 8.0, Color::from_rgb(1.0, 0.0, 0.0), 0.5, FillRule::NonZero);
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 10, Background::White)).unwrap();
    let p = px(&host, 5, 5);
    assert_eq!(p[0], 255);
    assert!((p[1] as i32 - 128).abs() <= 1, "g={}", p[1]);
    assert!((p[2] as i32 - 128).abs() <= 1, "b={}", p[2]);
    assert_eq!(p[3], 255);
}

#[test]
fn edges_are_antialiased() {
    // A rectangle with a fractional edge at x=4.5 leaves the boundary column
    // partially covered — neither fully red nor fully white.
    let page = rect_page(10.0, 2.0, 2.0, 4.5, 8.0, Color::from_rgb(1.0, 0.0, 0.0), 1.0, FillRule::NonZero);
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 10, Background::White)).unwrap();
    // Column 4 straddles the edge (covers x in [4.0,4.5]) → ~50% red.
    let p = px(&host, 4, 5);
    assert!(p[0] == 255, "r stays 255 (red over white)");
    assert!(p[1] > 100 && p[1] < 200, "partial coverage g={}", p[1]);
}

#[test]
fn output_is_deterministic() {
    let make = || rect_page(20.0, 3.0, 3.0, 17.0, 15.0, Color::from_rgb(0.1, 0.4, 0.9), 0.8, FillRule::NonZero);
    let backend = CpuBackend::default();
    let (a, _) = backend.render_to_host(&request(make(), 20, Background::White)).unwrap();
    let (b, _) = backend.render_to_host(&request(make(), 20, Background::White)).unwrap();
    assert_eq!(a.pixels, b.pixels);
}
