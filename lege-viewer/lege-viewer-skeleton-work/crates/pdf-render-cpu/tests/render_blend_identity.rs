#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! A5 byte-identity guard: generalizing non-Normal-blend/group compositing
//! must leave **Normal-blend-only** content bit-for-bit unchanged.
//!
//! The golden FNV-1a-64 hashes below were captured from the renderer *before*
//! the A5 generalization (commit 9ba44d1) and must stay green through it and
//! A6. The pages deliberately cross every op family the generalization
//! touches: solid fills (opaque, translucent, path-clipped), an image blit,
//! a shading-pattern fill, a tiling fill, and transparency groups with a
//! soft mask — all with `BlendMode::Normal` everywhere.

use std::sync::Arc;

use pdf_page_ir::{
    BlendMode, Color, CompiledPage, DeviceSize, DisplayOp, FillRule, ImageColorSpace, ImageIr,
    InterpolationMode, Matrix, PageBounds, PageComplexity, PageFeatures, Paint, PathData, PathVerb,
    Point, Rect, ResourceKey, ShadingKind, ShadingResource, TilingPattern, TransparencyGroup,
};
use pdf_render_api::{
    AnnotationMode, Background, OutputFormat, OutputResidency, PageTransform, RenderLimits,
    RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;

fn rect_path(x0: f64, y0: f64, x1: f64, y1: f64) -> PathData {
    PathData {
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
    }
}

fn key() -> ResourceKey {
    ResourceKey { object_number: 0, generation: 0, variant: 0 }
}

fn empty_page(size: f64) -> CompiledPage {
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds {
            crop: Rect { x0: 0.0, y0: 0.0, x1: size, y1: size },
            rotate: 0,
        },
        operations: Arc::from([]),
        paths: Arc::from([]),
        paints: Arc::from([]),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::empty(),
        complexity: PageComplexity::default(),
    }
}

fn fill(path: u32, paint: u32, alpha: f32) -> DisplayOp {
    DisplayOp::FillPath {
        path: pdf_page_ir::PathId(path),
        paint: pdf_page_ir::PaintId(paint),
        rule: FillRule::NonZero,
        alpha,
        blend: BlendMode::Normal,
    }
}

fn render_hash(page: CompiledPage, dim: u32) -> u64 {
    let request = RenderRequest {
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
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request).unwrap();
    // FNV-1a 64 over the raw pixel bytes.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in host.pixels.iter() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Solid fills: opaque, translucent overlap, and a path-clipped fill.
fn fills_page() -> CompiledPage {
    let mut page = empty_page(16.0);
    page.paths = Arc::from([
        rect_path(1.0, 1.0, 12.0, 12.0),
        rect_path(6.0, 6.0, 15.0, 15.0),
        rect_path(3.5, 0.0, 8.5, 16.0),
        // Non-rectangular clip: a triangle.
        PathData {
            verbs: Arc::from([PathVerb::MoveTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::Close]),
            points: Arc::from([
                Point { x: 2.0, y: 14.0 },
                Point { x: 14.0, y: 14.0 },
                Point { x: 8.0, y: 2.0 },
            ]),
        },
    ]);
    page.paints = Arc::from([
        Paint::Solid(Color::from_rgb(0.9, 0.7, 0.2)),
        Paint::Solid(Color::from_rgb(0.1, 0.3, 0.8)),
        Paint::Solid(Color::from_rgb(0.8, 0.1, 0.1)),
    ]);
    page.operations = Arc::from([
        fill(0, 0, 1.0),
        fill(1, 1, 0.5),
        DisplayOp::PushClip { path: pdf_page_ir::PathId(3), rule: FillRule::NonZero },
        fill(2, 2, 0.75),
        DisplayOp::PopClip,
    ]);
    page.features = PageFeatures::BASIC_PATHS | PageFeatures::CLIPPING;
    page
}

/// An RGB image magnified over a colored background fill.
fn image_page() -> CompiledPage {
    let mut page = empty_page(16.0);
    page.paths = Arc::from([rect_path(0.0, 0.0, 16.0, 16.0)]);
    page.paints = Arc::from([Paint::Solid(Color::from_rgb(0.2, 0.6, 0.4))]);
    let samples: Vec<u8> = vec![
        250, 10, 10, 10, 250, 10, //
        10, 10, 250, 200, 200, 40,
    ];
    page.images = Arc::from([ImageIr {
        key: key(),
        width: 2,
        height: 2,
        is_stencil: false,
        interpolation: InterpolationMode::Nearest,
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
    }]);
    page.operations = Arc::from([
        fill(0, 0, 1.0),
        DisplayOp::DrawImage {
            image: pdf_page_ir::ImageId(0),
            paint: pdf_page_ir::PaintId(0),
            transform: Matrix::scale(12.0, 12.0).then(Matrix::translate(2.0, 2.0)),
            alpha: 0.8,
            blend: BlendMode::Normal,
        },
    ]);
    page.features = PageFeatures::BASIC_PATHS | PageFeatures::IMAGES;
    page
}

/// An axial shading-pattern fill over a background.
fn shading_page() -> CompiledPage {
    let mut page = empty_page(16.0);
    let ramp: Arc<[Color]> = (0..256)
        .map(|i| {
            let v = i as f32 / 255.0;
            Color { r: v, g: 1.0 - v, b: 0.3, a: 1.0 }
        })
        .collect();
    page.paths = Arc::from([rect_path(0.0, 0.0, 16.0, 16.0), rect_path(2.0, 2.0, 14.0, 14.0)]);
    page.paints = Arc::from([
        Paint::Solid(Color::from_rgb(0.5, 0.5, 0.9)),
        Paint::Shading { shading: pdf_page_ir::ShadingId(0), matrix: Matrix::IDENTITY },
    ]);
    page.shadings = Arc::from([ShadingResource {
        key: key(),
        shading_type: 2,
        kind: ShadingKind::Axial {
            coords: [2.0, 0.0, 14.0, 0.0],
            domain: [0.0, 1.0],
            extend: [true, true],
            ramp,
            background: None,
        },
        bbox: None,
    }]);
    page.operations = Arc::from([fill(0, 0, 1.0), fill(1, 1, 0.9)]);
    page.features = PageFeatures::BASIC_PATHS | PageFeatures::SHADINGS;
    page
}

/// A colored tiling fill over a background.
fn tiling_page() -> CompiledPage {
    let mut page = empty_page(16.0);
    let mut cell = empty_page(4.0);
    cell.paths = Arc::from([rect_path(0.0, 0.0, 2.5, 2.5)]);
    cell.paints = Arc::from([Paint::Solid(Color::from_rgb(0.9, 0.2, 0.5))]);
    cell.operations = Arc::from([fill(0, 0, 0.9)]);
    cell.features = PageFeatures::BASIC_PATHS;
    page.paths = Arc::from([rect_path(0.0, 0.0, 16.0, 16.0), rect_path(1.0, 1.0, 15.0, 15.0)]);
    page.paints = Arc::from([
        Paint::Solid(Color::from_rgb(0.3, 0.7, 0.9)),
        Paint::Pattern { tiling: pdf_page_ir::TilingId(0), matrix: Matrix::IDENTITY },
    ]);
    page.tilings = Arc::from([TilingPattern {
        key: key(),
        uncolored: false,
        under_color: Color::BLACK,
        bbox: [0.0, 0.0, 4.0, 4.0],
        x_step: 4.0,
        y_step: 4.0,
        cell: Arc::new(cell),
    }]);
    page.operations = Arc::from([fill(0, 0, 1.0), fill(1, 1, 1.0)]);
    page.features = PageFeatures::BASIC_PATHS | PageFeatures::PATTERNS;
    page
}

/// Isolated and non-isolated transparency groups (Normal blend) over content.
fn groups_page() -> CompiledPage {
    let mut page = empty_page(16.0);
    page.paths = Arc::from([
        rect_path(0.0, 0.0, 16.0, 16.0),
        rect_path(2.0, 2.0, 10.0, 10.0),
        rect_path(6.0, 6.0, 14.0, 14.0),
    ]);
    page.paints = Arc::from([
        Paint::Solid(Color::from_rgb(0.85, 0.85, 0.6)),
        Paint::Solid(Color::from_rgb(0.9, 0.1, 0.1)),
        Paint::Solid(Color::from_rgb(0.1, 0.2, 0.9)),
    ]);
    page.groups = Arc::from([
        TransparencyGroup {
            isolated: true,
            knockout: false,
            bounds: Rect { x0: 0.0, y0: 0.0, x1: 16.0, y1: 16.0 },
            opacity: 0.5,
            blend: BlendMode::Normal,
        },
        TransparencyGroup {
            isolated: false,
            knockout: false,
            bounds: Rect { x0: 0.0, y0: 0.0, x1: 16.0, y1: 16.0 },
            opacity: 0.8,
            blend: BlendMode::Normal,
        },
    ]);
    page.operations = Arc::from([
        fill(0, 0, 1.0),
        DisplayOp::BeginTransparencyGroup { group: pdf_page_ir::TransparencyGroupId(0) },
        fill(1, 1, 1.0),
        DisplayOp::EndTransparencyGroup,
        DisplayOp::BeginTransparencyGroup { group: pdf_page_ir::TransparencyGroupId(1) },
        fill(2, 2, 0.7),
        DisplayOp::EndTransparencyGroup,
    ]);
    page.features = PageFeatures::BASIC_PATHS | PageFeatures::TRANSPARENCY;
    page
}

#[test]
fn normal_only_content_is_byte_stable() {
    let cases: [(&str, CompiledPage, u64); 5] = [
        ("fills", fills_page(), GOLDEN_FILLS),
        ("image", image_page(), GOLDEN_IMAGE),
        ("shading", shading_page(), GOLDEN_SHADING),
        ("tiling", tiling_page(), GOLDEN_TILING),
        ("groups", groups_page(), GOLDEN_GROUPS),
    ];
    let mut failures = Vec::new();
    for (name, page, golden) in cases {
        let h = render_hash(page, 16);
        if h != golden {
            failures.push(format!("{name}: got {h:#018x}, golden {golden:#018x}"));
        }
    }
    assert!(
        failures.is_empty(),
        "Normal-blend-only render changed:\n{}",
        failures.join("\n")
    );
}

// Golden hashes captured before the A5 generalization (see module docs).
const GOLDEN_FILLS: u64 = 0x4904e6b7969beeee;
const GOLDEN_IMAGE: u64 = 0xd30382e33f4ac69e;
const GOLDEN_SHADING: u64 = 0x141537cdc18a8505;
const GOLDEN_TILING: u64 = 0x03ffb0158db53fee;
const GOLDEN_GROUPS: u64 = 0xe55f497b11c47fe5;
