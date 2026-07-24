#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Text-clipping render modes (Tr 4–7, ISO 32000-1 §9.4.3): a glyph run's
//! outlines become a clip (`DisplayOp::PushClipText`) that constrains
//! subsequent painting to the glyph shapes. This is the mechanism behind the
//! owner's print-jacket covers, where full-page white overlays are drawn
//! through a title-glyph clip; without it they flood the page white.

use std::sync::Arc;

use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, FillRule, FontId, FontResource, GlyphRun,
    GlyphRunId, Matrix, PageBounds, PageComplexity, PageFeatures, Paint, PaintId, PathData, PathId,
    PathVerb, PlacedGlyph, Point, Rect, ResourceKey,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;
use pdf_test_support::fonts::minimal_ttf;

const DIM: u32 = 1000;

/// A full-surface rectangle path (user space [0,0]–[1000,1000]).
fn full_rect() -> PathData {
    PathData {
        verbs: Arc::from([PathVerb::MoveTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::Close]),
        points: Arc::from([
            Point { x: 0.0, y: 0.0 },
            Point { x: DIM as f64, y: 0.0 },
            Point { x: DIM as f64, y: DIM as f64 },
            Point { x: 0.0, y: DIM as f64 },
        ]),
    }
}

/// GID 1 of `minimal_ttf` is a triangle; at font_size == units/em and a
/// translate(10,10) its device vertices are (110,60),(910,60),(510,660).
fn triangle_run(render_mode: u8) -> GlyphRun {
    GlyphRun {
        font: FontId(0),
        font_size: 1000.0,
        transform: Matrix::translate(10.0, 10.0),
        glyphs: Arc::from([PlacedGlyph { glyph: 1, x: 0.0, y: 0.0 }]),
        render_mode,
    }
}

fn page(glyph_runs: Vec<GlyphRun>, paints: Vec<Paint>, program: Arc<[u8]>, ops: Vec<DisplayOp>) -> CompiledPage {
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds { crop: Rect { x0: 0.0, y0: 0.0, x1: DIM as f64, y1: DIM as f64 }, rotate: 0 },
        operations: ops.into(),
        paths: Arc::from([full_rect()]),
        paints: paints.into(),
        stroke_styles: Arc::from([]),
        glyph_runs: glyph_runs.into(),
        fonts: Arc::from([FontResource { key: ResourceKey { object_number: 0, generation: 0, variant: 0 }, program, face_index: 0 , synthetic_shear: 0.0, synthetic_embolden_em: 0.0 }]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::TEXT | PageFeatures::CLIPPING,
        complexity: PageComplexity::default(),
    }
}

fn render(page: CompiledPage) -> HostPage {
    let req = RenderRequest {
        page: Arc::new(page),
        transform: PageTransform { matrix: Matrix::IDENTITY },
        crop: None,
        output_size: DeviceSize { width: DIM, height: DIM },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations: AnnotationMode::None,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };
    CpuBackend::default().render_to_host(&req).unwrap().0
}

fn px(host: &HostPage, x: usize, y: usize) -> [u8; 4] {
    let i = y * host.stride + x * 4;
    [host.pixels[i], host.pixels[i + 1], host.pixels[i + 2], host.pixels[i + 3]]
}
fn is_red(p: [u8; 4]) -> bool {
    p[0] > 200 && p[1] < 60 && p[2] < 60
}
fn is_white(p: [u8; 4]) -> bool {
    p == [255, 255, 255, 255]
}

#[test]
fn mode7_text_clip_constrains_a_following_fill_to_glyph_shape() {
    // Tr 7 (clip only): the triangle glyph paints nothing but clips the red
    // full-surface fill to its shape. Interior red, exterior (bbox corners
    // outside the triangle) untouched white.
    let ops = vec![
        DisplayOp::PushClipText { runs: Box::from([GlyphRunId(0)]) },
        DisplayOp::FillPath { path: PathId(0), paint: PaintId(0), rule: FillRule::NonZero, alpha: 1.0, blend: pdf_page_ir::BlendMode::Normal },
    ];
    let host = render(page(vec![triangle_run(7)], vec![Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))], minimal_ttf().into(), ops));

    assert!(is_red(px(&host, 510, 260)), "triangle interior must be filled through the text clip");
    assert!(is_white(px(&host, 160, 610)), "outside the glyph must stay unpainted (clipped out)");
    assert!(is_white(px(&host, 850, 610)), "other bbox corner outside the glyph stays white");
}

#[test]
fn mode4_glyph_paints_and_clips_the_following_fill() {
    // Tr 4 (fill + clip): the glyph run both paints (DrawGlyphRun, black) and,
    // via PushClipText, clips a later red fill to the glyph. Interior ends red
    // (fill over glyph); exterior stays white (fill clipped out).
    let ops = vec![
        DisplayOp::DrawGlyphRun { run: GlyphRunId(0), paint: PaintId(0), alpha: 1.0, blend: pdf_page_ir::BlendMode::Normal, stroke: None },
        DisplayOp::PushClipText { runs: Box::from([GlyphRunId(0)]) },
        DisplayOp::FillPath { path: PathId(0), paint: PaintId(1), rule: FillRule::NonZero, alpha: 1.0, blend: pdf_page_ir::BlendMode::Normal },
    ];
    let paints = vec![Paint::Solid(Color::from_rgb(0.0, 0.0, 0.0)), Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))];
    let host = render(page(vec![triangle_run(4)], paints, minimal_ttf().into(), ops));

    assert!(is_red(px(&host, 510, 260)), "interior filled through the clip");
    assert!(is_white(px(&host, 160, 610)), "exterior clipped out and unpainted");
}

#[test]
fn empty_text_clip_clips_everything_out() {
    // A clip-mode text object that placed no glyph outline yields an empty clip
    // — nothing passes (spec empty-clip-path rule; PDFium's -1,-1,0,0 rect).
    let ops = vec![
        DisplayOp::PushClipText { runs: Box::from([GlyphRunId(0)]) },
        DisplayOp::FillPath { path: PathId(0), paint: PaintId(0), rule: FillRule::NonZero, alpha: 1.0, blend: pdf_page_ir::BlendMode::Normal },
    ];
    // A run with no glyphs contributes no outline.
    let empty_run = GlyphRun { glyphs: Arc::from([]), ..triangle_run(7) };
    let host = render(page(vec![empty_run], vec![Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))], minimal_ttf().into(), ops));

    for (x, y) in [(510, 260), (160, 610), (500, 500), (10, 10)] {
        assert!(is_white(px(&host, x, y)), "empty text clip must let nothing paint at ({x},{y})");
    }
}
