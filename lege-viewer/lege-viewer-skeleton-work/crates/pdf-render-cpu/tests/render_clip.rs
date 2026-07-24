#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Clipping tests (advice §8): rectangular clips by arithmetic bounds, and
//! non-rectangular path clips via a lazily-built Alpha8 mask.

use std::sync::Arc;

use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, FillRule, Matrix, PageBounds, PageComplexity,
    PageFeatures, Paint, PaintId, PathData, PathId, PathVerb, Point, Rect,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;

fn rect_path(x0: f64, y0: f64, x1: f64, y1: f64) -> PathData {
    PathData {
        verbs: Arc::from([PathVerb::MoveTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::Close]),
        points: Arc::from([
            Point { x: x0, y: y0 },
            Point { x: x1, y: y0 },
            Point { x: x1, y: y1 },
            Point { x: x0, y: y1 },
        ]),
    }
}

fn tri_path(a: Point, b: Point, c: Point) -> PathData {
    PathData {
        verbs: Arc::from([PathVerb::MoveTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::Close]),
        points: Arc::from([a, b, c]),
    }
}

/// Assemble a page from paths, one solid paint, and an op list.
fn page(size: f64, paths: Vec<PathData>, ops: Vec<DisplayOp>, features: PageFeatures) -> CompiledPage {
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds { crop: Rect { x0: 0.0, y0: 0.0, x1: size, y1: size }, rotate: 0 },
        operations: ops.into(),
        paths: paths.into(),
        paints: Arc::from([Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))]),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features,
        complexity: PageComplexity::default(),
    }
}

fn render(page: CompiledPage, dim: u32) -> HostPage {
    let req = RenderRequest {
        page: Arc::new(page),
        transform: PageTransform { matrix: Matrix::IDENTITY },
        crop: None,
        output_size: DeviceSize { width: dim, height: dim },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations: AnnotationMode::None,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };
    let (host, _) = CpuBackend::default().render_to_host(&req).unwrap();
    host
}

fn px(host: &HostPage, x: usize, y: usize) -> [u8; 4] {
    let i = y * host.stride + x * 4;
    [host.pixels[i], host.pixels[i + 1], host.pixels[i + 2], host.pixels[i + 3]]
}

fn red(p: [u8; 4]) -> bool {
    p == [255, 0, 0, 255]
}
fn white(p: [u8; 4]) -> bool {
    p == [255, 255, 255, 255]
}

#[test]
fn rectangular_clip_limits_the_fill() {
    // Fill the whole 20×20 page red, clipped to the rect [5,5]-[15,15].
    let ops = vec![
        DisplayOp::PushClip { path: PathId(0), rule: FillRule::NonZero },
        DisplayOp::FillPath {
            path: PathId(1),
            paint: PaintId(0),
            rule: FillRule::NonZero,
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        },
        DisplayOp::PopClip,
    ];
    let paths = vec![rect_path(5.0, 5.0, 15.0, 15.0), rect_path(0.0, 0.0, 20.0, 20.0)];
    let host = render(page(20.0, paths, ops, PageFeatures::BASIC_PATHS | PageFeatures::CLIPPING), 20);

    assert!(red(px(&host, 10, 10)), "inside clip");
    assert!(white(px(&host, 2, 2)), "outside clip stays white");
    assert!(white(px(&host, 17, 17)), "outside clip stays white");
    // Right at the clip boundary column 4 (outside) is white, column 5 red.
    assert!(white(px(&host, 4, 10)));
    assert!(red(px(&host, 5, 10)));
}

#[test]
fn path_clip_uses_a_mask() {
    // Clip a full-page red fill to a triangle; the mask carves the diagonal.
    let clip = tri_path(Point { x: 0.0, y: 0.0 }, Point { x: 20.0, y: 0.0 }, Point { x: 0.0, y: 20.0 });
    let ops = vec![
        DisplayOp::PushClip { path: PathId(0), rule: FillRule::NonZero },
        DisplayOp::FillPath {
            path: PathId(1),
            paint: PaintId(0),
            rule: FillRule::NonZero,
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        },
        DisplayOp::PopClip,
    ];
    let paths = vec![clip, rect_path(0.0, 0.0, 20.0, 20.0)];
    let host = render(page(20.0, paths, ops, PageFeatures::BASIC_PATHS | PageFeatures::CLIPPING), 20);

    // Near the origin corner: inside the triangle → red.
    assert!(red(px(&host, 2, 2)), "inside triangle");
    // Bottom-right corner: outside the triangle → white.
    assert!(white(px(&host, 18, 18)), "outside triangle");
    // A point clearly on the far side of the diagonal.
    assert!(white(px(&host, 15, 15)), "beyond diagonal");
}

#[test]
fn nested_clips_intersect() {
    // Two nested rect clips → the fill survives only in their intersection
    // [8,4]-[12,16] ∩ [4,8]-[16,12] = [8,8]-[12,12].
    let ops = vec![
        DisplayOp::PushClip { path: PathId(0), rule: FillRule::NonZero },
        DisplayOp::PushClip { path: PathId(1), rule: FillRule::NonZero },
        DisplayOp::FillPath {
            path: PathId(2),
            paint: PaintId(0),
            rule: FillRule::NonZero,
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        },
        DisplayOp::PopClip,
        DisplayOp::PopClip,
    ];
    let paths = vec![
        rect_path(8.0, 4.0, 12.0, 16.0),
        rect_path(4.0, 8.0, 16.0, 12.0),
        rect_path(0.0, 0.0, 20.0, 20.0),
    ];
    let host = render(page(20.0, paths, ops, PageFeatures::BASIC_PATHS | PageFeatures::CLIPPING), 20);

    assert!(red(px(&host, 10, 10)), "in intersection");
    assert!(white(px(&host, 10, 6)), "clipped by second rect");
    assert!(white(px(&host, 6, 10)), "clipped by first rect");
}
