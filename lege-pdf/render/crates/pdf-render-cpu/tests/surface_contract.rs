#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 6D: conformance tests for the frozen surface contract
//! (`pdf_render_api::contract`). The CPU backend is the normative reference;
//! these assert the frozen conventions on exact pixels.

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

fn rect_page(x0: f64, y0: f64, x1: f64, y1: f64, color: Color, alpha: f32) -> CompiledPage {
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        content_bounds: None,
        bounds: PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 4.0,
                y1: 4.0,
            },
            rotate: 0,
        },
        operations: Arc::from([DisplayOp::FillPath {
            path: pdf_page_ir::PathId(0),
            paint: pdf_page_ir::PaintId(0),
            rule: FillRule::NonZero,
            alpha,
            blend: pdf_page_ir::BlendMode::Normal,
        }]),
        paths: Arc::from([PathData {
            verbs: Arc::from([
                PathVerb::MoveTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::Close,
            ]),
            points: Arc::from([
                Point { x: x0, y: y0 },
                Point { x: x1, y: y0 },
                Point { x: x1, y: y1 },
                Point { x: x0, y: y1 },
            ]),
        }]),
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

fn empty_page() -> CompiledPage {
    let mut p = rect_page(0.0, 0.0, 0.0, 0.0, Color::BLACK, 1.0);
    p.operations = Arc::from([]);
    p
}

fn request(
    page: CompiledPage,
    dim: u32,
    format: OutputFormat,
    background: Background,
) -> RenderRequest {
    RenderRequest {
        page: Arc::new(page),
        transform: PageTransform {
            matrix: Matrix::IDENTITY,
        },
        crop: None,
        output_size: DeviceSize {
            width: dim,
            height: dim,
        },
        output_format: format,
        background,
        annotations: AnnotationMode::None,
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    }
}

fn px(host: &HostPage, x: usize, y: usize) -> [u8; 4] {
    let i = y * host.stride + x * 4;
    [
        host.pixels[i],
        host.pixels[i + 1],
        host.pixels[i + 2],
        host.pixels[i + 3],
    ]
}

fn render(page: CompiledPage, dim: u32, format: OutputFormat, bg: Background) -> HostPage {
    CpuBackend::default()
        .render_to_host(&request(page, dim, format, bg))
        .unwrap()
        .0
}

// §2/§4: background application + premultiplied output.
#[test]
fn backgrounds_are_applied_before_painting() {
    let white = render(
        empty_page(),
        2,
        OutputFormat::Rgba8PremultipliedSrgb,
        Background::White,
    );
    assert!(
        white.pixels.iter().all(|&b| b == 0xFF),
        "white is opaque 0xFF"
    );

    let clear = render(
        empty_page(),
        2,
        OutputFormat::Rgba8PremultipliedSrgb,
        Background::Transparent,
    );
    assert!(
        clear.pixels.iter().all(|&b| b == 0x00),
        "transparent is all zero"
    );

    let solid = render(
        empty_page(),
        2,
        OutputFormat::Rgba8PremultipliedSrgb,
        Background::Solid(Color::from_rgb(1.0, 0.0, 0.0)),
    );
    assert_eq!(
        px(&solid, 0, 0),
        [255, 0, 0, 255],
        "solid red, premultiplied opaque"
    );
}

// §2: straight color + alpha → premultiplied output.
#[test]
fn output_is_premultiplied() {
    // Opaque-red at 50% constant alpha over a transparent surface: coverage 1,
    // alpha 0.5 → premultiplied [128, 0, 0, 128].
    let page = rect_page(0.0, 0.0, 4.0, 4.0, Color::from_rgb(1.0, 0.0, 0.0), 0.5);
    let host = render(
        page,
        4,
        OutputFormat::Rgba8PremultipliedSrgb,
        Background::Transparent,
    );
    let p = px(&host, 2, 2);
    assert_eq!(p[3], 128, "alpha 0.5");
    assert_eq!(p[0], 128, "red premultiplied by alpha");
    assert_eq!([p[1], p[2]], [0, 0]);
    // Matches the api's reference premultiply of straight [255,0,0,128].
    assert_eq!(p, pdf_render_api::contract::premultiply([255, 0, 0, 128]));
}

// §5/§6/§9: analytic coverage + pixel centers → exact half coverage.
#[test]
fn half_covered_edge_is_exact() {
    // Opaque red covering x in [0, 0.5] over transparent → column 0 has exactly
    // 50% area coverage → premultiplied [128, 0, 0, 128]; column 1 empty.
    let page = rect_page(0.0, 0.0, 0.5, 4.0, Color::from_rgb(1.0, 0.0, 0.0), 1.0);
    let host = render(
        page,
        4,
        OutputFormat::Rgba8PremultipliedSrgb,
        Background::Transparent,
    );
    let c0 = px(&host, 0, 1);
    assert!(
        (c0[0] as i32 - 128).abs() <= 1 && (c0[3] as i32 - 128).abs() <= 1,
        "col0 ~50%: {c0:?}"
    );
    assert_eq!(px(&host, 1, 1), [0, 0, 0, 0], "col1 uncovered");
}

// §1: Gray8 downconversion path.
#[test]
fn gray8_output_has_one_byte_per_pixel() {
    let host = render(empty_page(), 3, OutputFormat::Gray8, Background::White);
    assert_eq!(host.stride, 3, "one byte per pixel");
    assert_eq!(host.pixels.len(), 9);
    assert!(host.pixels.iter().all(|&b| b == 0xFF), "white → 255");
}

// §9: determinism — identical inputs yield byte-identical output.
#[test]
fn rendering_is_deterministic() {
    let make = || rect_page(0.7, 0.3, 3.4, 2.9, Color::from_rgb(0.2, 0.6, 0.9), 0.8);
    let a = render(
        make(),
        4,
        OutputFormat::Rgba8PremultipliedSrgb,
        Background::White,
    );
    let b = render(
        make(),
        4,
        OutputFormat::Rgba8PremultipliedSrgb,
        Background::White,
    );
    assert_eq!(a.pixels, b.pixels);
}
