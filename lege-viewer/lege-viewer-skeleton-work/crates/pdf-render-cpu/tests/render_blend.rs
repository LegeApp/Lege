#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Separable blend-mode tests (advice §9): the general compositor blends the
//! source against the backdrop before source-over.

use std::sync::Arc;

use pdf_page_ir::{
    BlendMode, Color, CompiledPage, DeviceSize, DisplayOp, FillRule, Matrix, PageBounds,
    PageComplexity, PageFeatures, Paint, PaintId, PathData, PathId, PathVerb, Point, Rect,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> PathData {
    PathData {
        verbs: Arc::from([PathVerb::MoveTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::Close]),
        points: Arc::from([Point { x: x0, y: y0 }, Point { x: x1, y: y0 }, Point { x: x1, y: y1 }, Point { x: x0, y: y1 }]),
    }
}

/// One blend-mode fill of a rect over the given background.
fn blend_over(bg: Background, color: Color, mode: BlendMode) -> HostPage {
    let page = CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds { crop: Rect { x0: 0.0, y0: 0.0, x1: 12.0, y1: 12.0 }, rotate: 0 },
        operations: Arc::from([DisplayOp::FillPath {
            path: PathId(0),
            paint: PaintId(0),
            rule: FillRule::NonZero,
            alpha: 1.0,
            blend: mode,
        }]),
        paths: Arc::from([rect(2.0, 2.0, 10.0, 10.0)]),
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
    };
    let req = RenderRequest {
        page: Arc::new(page),
        transform: PageTransform { matrix: Matrix::IDENTITY },
        crop: None,
        output_size: DeviceSize { width: 12, height: 12 },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: bg,
        annotations: AnnotationMode::None,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };
    CpuBackend::default().render_to_host(&req).unwrap().0
}

fn center(host: &HostPage) -> [u8; 4] {
    let i = 6 * host.stride + 6 * 4;
    [host.pixels[i], host.pixels[i + 1], host.pixels[i + 2], host.pixels[i + 3]]
}

fn near(a: u8, b: u8) -> bool {
    (a as i32 - b as i32).abs() <= 1
}

#[test]
fn multiply_gray_over_white_darkens() {
    // Multiply(1.0, 0.5) = 0.5 → 128 on every channel.
    let p = center(&blend_over(Background::White, Color::from_rgb(0.5, 0.5, 0.5), BlendMode::Multiply));
    assert!(near(p[0], 128) && near(p[1], 128) && near(p[2], 128), "{p:?}");
}

#[test]
fn screen_black_over_white_stays_white() {
    // Screen(1.0, 0.0) = 1.0 → 255.
    let p = center(&blend_over(Background::White, Color::from_rgb(0.0, 0.0, 0.0), BlendMode::Screen));
    assert_eq!(p, [255, 255, 255, 255]);
}

#[test]
fn difference_white_over_white_is_black() {
    // |1-1| = 0 → black.
    let p = center(&blend_over(Background::White, Color::from_rgb(1.0, 1.0, 1.0), BlendMode::Difference));
    assert!(near(p[0], 0) && near(p[1], 0) && near(p[2], 0), "{p:?}");
}

#[test]
fn multiply_channels_are_independent() {
    // Backdrop mid-gray 0.5; source pure red (1,0,0): Multiply →
    // R=0.5·1=0.5→128, G=0.5·0=0, B=0.5·0=0.
    let bg = Background::Solid(Color::from_rgb(0.5, 0.5, 0.5));
    let p = center(&blend_over(bg, Color::from_rgb(1.0, 0.0, 0.0), BlendMode::Multiply));
    assert!(near(p[0], 128), "r={}", p[0]);
    assert!(near(p[1], 0), "g={}", p[1]);
    assert!(near(p[2], 0), "b={}", p[2]);
}

#[test]
fn normal_blend_unaffected() {
    // A Normal-blend opaque fill still overwrites (fast path), unchanged by
    // the blend machinery.
    let p = center(&blend_over(Background::White, Color::from_rgb(0.2, 0.4, 0.6), BlendMode::Normal));
    assert!(near(p[0], 51) && near(p[1], 102) && near(p[2], 153), "{p:?}");
}

// --- non-separable modes (Hue/Saturation/Color/Luminosity) ------------------

#[test]
fn color_mode_takes_source_hue_backdrop_luminosity() {
    // Color(backdrop gray, source red): keeps red's hue/sat at the gray's
    // luminosity → a desaturated-toward-mid red with r > g == b.
    let bg = Background::Solid(Color::from_rgb(0.5, 0.5, 0.5));
    let p = center(&blend_over(bg, Color::from_rgb(1.0, 0.0, 0.0), BlendMode::Color));
    assert!(p[0] > p[1], "red channel dominates: {p:?}");
    assert!(near(p[1], p[2]), "green≈blue (red hue preserved): {p:?}");
    assert_ne!([p[0], p[1], p[2]], [255, 0, 0], "not the raw source");
}

#[test]
fn luminosity_mode_takes_source_luminosity_backdrop_hue() {
    // Luminosity(backdrop red, source gray): keeps red's hue at the gray's
    // luminosity → r > g == b.
    let bg = Background::Solid(Color::from_rgb(1.0, 0.0, 0.0));
    let p = center(&blend_over(bg, Color::from_rgb(0.5, 0.5, 0.5), BlendMode::Luminosity));
    assert!(p[0] > p[1], "red hue preserved: {p:?}");
    assert!(near(p[1], p[2]), "green≈blue: {p:?}");
}

#[test]
fn nonseparable_is_deterministic() {
    let bg = Background::Solid(Color::from_rgb(0.3, 0.6, 0.2));
    let a = center(&blend_over(bg, Color::from_rgb(0.8, 0.1, 0.4), BlendMode::Saturation));
    let b = center(&blend_over(bg, Color::from_rgb(0.8, 0.1, 0.4), BlendMode::Saturation));
    assert_eq!(a, b);
}

// --- A5: non-Normal blend generalized to images, shadings, tilings ----------

/// Render a single-op page over a solid background.
fn render_page(mut page: CompiledPage, bg: Background) -> HostPage {
    page.bounds = PageBounds { crop: Rect { x0: 0.0, y0: 0.0, x1: 12.0, y1: 12.0 }, rotate: 0 };
    let req = RenderRequest {
        page: Arc::new(page),
        transform: PageTransform { matrix: Matrix::IDENTITY },
        crop: None,
        output_size: DeviceSize { width: 12, height: 12 },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: bg,
        annotations: AnnotationMode::None,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };
    CpuBackend::default().render_to_host(&req).unwrap().0
}

fn bare_page() -> CompiledPage {
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds { crop: Rect { x0: 0.0, y0: 0.0, x1: 12.0, y1: 12.0 }, rotate: 0 },
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
        features: PageFeatures::BASIC_PATHS,
        complexity: PageComplexity::default(),
    }
}

#[test]
fn multiply_image_blends_with_backdrop() {
    // Pure-red 1x1 image over mid-gray: Multiply → (0.5·1, 0.5·0, 0.5·0).
    let mut page = bare_page();
    page.paints = Arc::from([Paint::Solid(Color::BLACK)]);
    page.images = Arc::from([pdf_page_ir::ImageIr {
        key: pdf_page_ir::ResourceKey { object_number: 0, generation: 0, variant: 0 },
        width: 1,
        height: 1,
        is_stencil: false,
        interpolation: pdf_page_ir::InterpolationMode::Nearest,
        soft_mask: None,
        bits_per_component: 8,
        color_space: pdf_page_ir::ImageColorSpace::Rgb,
        decode: None,
        samples: Some(Arc::from(vec![255u8, 0, 0])),
        codec: None,
        codec_data: None,
        codec_parms: None,
        smask: None,
        mask: None,
        smask_in_data: 0,
        lowering_degraded: false,
    }]);
    page.operations = Arc::from([
        DisplayOp::ConcatTransform(Matrix::scale(12.0, 12.0)),
        DisplayOp::DrawImage {
            image: pdf_page_ir::ImageId(0),
            paint: PaintId(0),
            transform: Matrix::IDENTITY,
            alpha: 1.0,
            blend: BlendMode::Multiply,
        },
    ]);
    page.features = PageFeatures::IMAGES;
    let p = center(&render_page(page, Background::Solid(Color::from_rgb(0.5, 0.5, 0.5))));
    assert!(near(p[0], 128), "r={}", p[0]);
    assert!(near(p[1], 0), "g={}", p[1]);
    assert!(near(p[2], 0), "b={}", p[2]);
}

#[test]
fn multiply_shading_fill_blends_with_backdrop() {
    // Constant pure-red axial ramp over mid-gray: Multiply → (128, 0, 0).
    let ramp: Arc<[Color]> = (0..256).map(|_| Color { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }).collect();
    let mut page = bare_page();
    page.paths = Arc::from([rect(0.0, 0.0, 12.0, 12.0)]);
    page.paints = Arc::from([Paint::Shading {
        shading: pdf_page_ir::ShadingId(0),
        matrix: Matrix::IDENTITY,
    }]);
    page.shadings = Arc::from([pdf_page_ir::ShadingResource {
        key: pdf_page_ir::ResourceKey { object_number: 0, generation: 0, variant: 0 },
        shading_type: 2,
        kind: pdf_page_ir::ShadingKind::Axial {
            coords: [0.0, 0.0, 12.0, 0.0],
            domain: [0.0, 1.0],
            extend: [true, true],
            ramp,
            background: None,
        },
        bbox: None,
    }]);
    page.operations = Arc::from([DisplayOp::FillPath {
        path: PathId(0),
        paint: PaintId(0),
        rule: FillRule::NonZero,
        alpha: 1.0,
        blend: BlendMode::Multiply,
    }]);
    page.features = PageFeatures::BASIC_PATHS | PageFeatures::SHADINGS;
    let p = center(&render_page(page, Background::Solid(Color::from_rgb(0.5, 0.5, 0.5))));
    assert!(near(p[0], 128), "r={}", p[0]);
    assert!(near(p[1], 0), "g={}", p[1]);
    assert!(near(p[2], 0), "b={}", p[2]);
}

#[test]
fn multiply_tiling_fill_blends_with_backdrop() {
    // A cell that fills its whole bbox pure red, tiled over the fill, over
    // mid-gray: Multiply → (128, 0, 0) everywhere in the fill.
    let mut cell = bare_page();
    cell.bounds = PageBounds { crop: Rect { x0: 0.0, y0: 0.0, x1: 4.0, y1: 4.0 }, rotate: 0 };
    cell.paths = Arc::from([rect(0.0, 0.0, 4.0, 4.0)]);
    cell.paints = Arc::from([Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))]);
    cell.operations = Arc::from([DisplayOp::FillPath {
        path: PathId(0),
        paint: PaintId(0),
        rule: FillRule::NonZero,
        alpha: 1.0,
        blend: BlendMode::Normal,
    }]);
    let mut page = bare_page();
    page.paths = Arc::from([rect(0.0, 0.0, 12.0, 12.0)]);
    page.paints = Arc::from([Paint::Pattern {
        tiling: pdf_page_ir::TilingId(0),
        matrix: Matrix::IDENTITY,
    }]);
    page.tilings = Arc::from([pdf_page_ir::TilingPattern {
        key: pdf_page_ir::ResourceKey { object_number: 0, generation: 0, variant: 0 },
        uncolored: false,
        under_color: Color::BLACK,
        bbox: [0.0, 0.0, 4.0, 4.0],
        x_step: 4.0,
        y_step: 4.0,
        cell: Arc::new(cell),
    }]);
    page.operations = Arc::from([DisplayOp::FillPath {
        path: PathId(0),
        paint: PaintId(0),
        rule: FillRule::NonZero,
        alpha: 1.0,
        blend: BlendMode::Multiply,
    }]);
    page.features = PageFeatures::BASIC_PATHS | PageFeatures::PATTERNS;
    let p = center(&render_page(page, Background::Solid(Color::from_rgb(0.5, 0.5, 0.5))));
    assert!(near(p[0], 128), "r={}", p[0]);
    assert!(near(p[1], 0), "g={}", p[1]);
    assert!(near(p[2], 0), "b={}", p[2]);
}

// --- A6: knockout groups -----------------------------------------------------

#[test]
fn knockout_group_replaces_earlier_elements_in_overlap() {
    // Two 50%-alpha fills fully overlapping inside a group over white.
    // Non-knockout: blue@0.5 OVER (red@0.5 over white) → G ≈ 64.
    // Knockout: each element composites against the group's initial (transparent)
    // backdrop, blue replacing red in the overlap → blue@0.5 over white → G ≈ 128.
    let render_group = |knockout: bool| {
        let mut page = bare_page();
        page.paths = Arc::from([rect(2.0, 2.0, 10.0, 10.0)]);
        page.paints = Arc::from([
            Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)),
            Paint::Solid(Color::from_rgb(0.0, 0.0, 1.0)),
        ]);
        page.groups = Arc::from([pdf_page_ir::TransparencyGroup {
            isolated: true,
            knockout,
            bounds: Rect { x0: 0.0, y0: 0.0, x1: 12.0, y1: 12.0 },
            opacity: 1.0,
            blend: BlendMode::Normal,
        }]);
        page.operations = Arc::from([
            DisplayOp::BeginTransparencyGroup { group: pdf_page_ir::TransparencyGroupId(0) },
            DisplayOp::FillPath {
                path: PathId(0),
                paint: PaintId(0),
                rule: FillRule::NonZero,
                alpha: 0.5,
                blend: BlendMode::Normal,
            },
            DisplayOp::FillPath {
                path: PathId(0),
                paint: PaintId(1),
                rule: FillRule::NonZero,
                alpha: 0.5,
                blend: BlendMode::Normal,
            },
            DisplayOp::EndTransparencyGroup,
        ]);
        page.features = PageFeatures::BASIC_PATHS | PageFeatures::TRANSPARENCY;
        center(&render_page(page, Background::White))
    };
    let stacked = render_group(false);
    let knocked = render_group(true);
    // Stacked: blue@0.5 over (red@0.5 over white) → (127, 64, 191).
    assert!(
        (stacked[2] as i32 - 191).abs() <= 2 && (stacked[1] as i32 - 64).abs() <= 2,
        "non-knockout stacks: {stacked:?}"
    );
    // Knockout: blue@0.5 over white → (128, 128, 255); red fully knocked out.
    assert!(
        near(knocked[2], 255) && (knocked[1] as i32 - 128).abs() <= 2,
        "knockout replaces: {knocked:?}"
    );
    assert!(near(knocked[0], knocked[1]), "knockout drops the red layer: {knocked:?}");
}
