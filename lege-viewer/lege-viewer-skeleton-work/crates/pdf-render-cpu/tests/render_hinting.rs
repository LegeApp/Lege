#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Font Phase 4: the hinting policy reaches the rasterizer.
//!
//! Grid-fitting is verified against the outline engine in `pdf-font`; what
//! matters here is that the policy is honoured end-to-end — that `Auto`
//! actually moves pixels at small sizes, backs off above the ppem threshold,
//! and never touches non-axis-aligned text.

use std::sync::Arc;

use pdf_font::{HintingPolicy, StandardFont};
use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, FontId, FontResource, GlyphRun, Matrix,
    PageBounds, PageComplexity, PageFeatures, Paint, PlacedGlyph, Rect, ResourceKey,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::{CpuBackend, CpuBackendOptions};

/// One glyph run of "Hamburg" in bundled Helvetica at `size`, under `text`.
fn page(size: f64, text: Matrix) -> CompiledPage {
    let face = StandardFont::Helvetica;
    let prog = face.program().unwrap();
    let data = face.program_data();
    let mut glyphs = Vec::new();
    let mut x = 0.0f64;
    for c in "Hamburg".chars() {
        let gid = prog.gid_for_char(c).unwrap();
        glyphs.push(PlacedGlyph { glyph: gid, x, y: 0.0 });
        x += prog.advance(gid).unwrap_or(500.0) as f64 / 1000.0 * size;
    }
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds { crop: Rect { x0: 0.0, y0: 0.0, x1: 200.0, y1: 200.0 }, rotate: 0 },
        operations: Arc::from([DisplayOp::DrawGlyphRun {
            run: pdf_page_ir::GlyphRunId(0),
            paint: pdf_page_ir::PaintId(0),
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
            stroke: None,
        }]),
        paths: Arc::from([]),
        paints: Arc::from([Paint::Solid(Color::BLACK)]),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([GlyphRun {
            font: FontId(0),
            font_size: size,
            transform: text,
            glyphs: Arc::from(glyphs),
            render_mode: 0,
        }]),
        fonts: Arc::from([FontResource {
            key: ResourceKey { object_number: 0, generation: 0, variant: 0 },
            program: data,
            face_index: 0, synthetic_shear: 0.0, synthetic_embolden_em: 0.0,
        }]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::TEXT,
        complexity: PageComplexity::default(),
    }
}

fn render(page: CompiledPage, hinting: HintingPolicy) -> HostPage {
    let backend = CpuBackend::new(CpuBackendOptions { hinting, ..Default::default() });
    let req = RenderRequest {
        page: Arc::new(page),
        transform: PageTransform { matrix: Matrix::IDENTITY },
        crop: None,
        output_size: DeviceSize { width: 200, height: 200 },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations: AnnotationMode::None,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };
    backend.render_to_host(&req).unwrap().0
}

fn differs(a: &HostPage, b: &HostPage) -> bool {
    a.pixels != b.pixels
}

/// Upright text: PDF y-up flipped into device y-down, baseline at y=100.
fn upright(_size: f64) -> Matrix {
    Matrix { a: 1.0, b: 0.0, c: 0.0, d: -1.0, e: 10.0, f: 100.0 }
}

#[test]
fn auto_hinting_changes_small_text_but_not_large() {
    let small = 12.0;
    let a = render(page(small, upright(small)), HintingPolicy::None);
    let b = render(page(small, upright(small)), HintingPolicy::Auto);
    assert!(differs(&a, &b), "Auto must grid-fit 12px text");

    // Above the threshold Auto is a no-op, so output matches None exactly.
    let big = pdf_font::AUTO_HINT_MAX_PPEM as f64 + 10.0;
    let c = render(page(big, upright(big)), HintingPolicy::None);
    let d = render(page(big, upright(big)), HintingPolicy::Auto);
    assert!(!differs(&c, &d), "Auto must not hint above AUTO_HINT_MAX_PPEM");
}

#[test]
fn embedded_hinting_applies_at_any_size() {
    let big = pdf_font::AUTO_HINT_MAX_PPEM as f64 + 10.0;
    let a = render(page(big, upright(big)), HintingPolicy::None);
    let b = render(page(big, upright(big)), HintingPolicy::Embedded);
    assert!(differs(&a, &b), "Embedded honours the font at large sizes too");
}

#[test]
fn rotated_text_is_never_hinted() {
    // A rotated run has no pixel grid to fit; Auto must fall through to the
    // exact unhinted outline, matching None byte for byte.
    let size = 12.0;
    let rot = 0.3f64;
    let m = Matrix {
        a: rot.cos(),
        b: rot.sin(),
        c: -rot.sin(),
        d: -rot.cos(),
        e: 20.0,
        f: 120.0,
    };
    let a = render(page(size, m), HintingPolicy::None);
    let b = render(page(size, m), HintingPolicy::Auto);
    assert!(!differs(&a, &b), "rotated text must not be hinted");
}

#[test]
fn hinted_rendering_is_deterministic() {
    let size = 12.0;
    let a = render(page(size, upright(size)), HintingPolicy::Auto);
    let b = render(page(size, upright(size)), HintingPolicy::Auto);
    assert!(!differs(&a, &b), "same input, same bytes");
}
