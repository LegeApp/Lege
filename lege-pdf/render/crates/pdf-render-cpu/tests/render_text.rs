#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Font Phase 1 text rendering: a DrawGlyphRun paints synthetic rectangle
//! glyphs at the placed positions, with gaps between glyphs (fonts.md).

use std::sync::Arc;

use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, FontId, FontResource, GlyphRun, Matrix, PageBounds,
    PageComplexity, PageFeatures, Paint, PaintId, PlacedGlyph, Rect, ResourceKey,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;

fn glyph(g: u32, x: f64) -> PlacedGlyph {
    PlacedGlyph {
        glyph: g,
        x,
        y: 0.0,
    }
}

fn render_run(glyphs: Vec<PlacedGlyph>, font_size: f64, render_mode: u8, dim: u32) -> HostPage {
    let run = GlyphRun {
        font: FontId(0),
        font_size,
        // text → user: shift into the surface.
        transform: Matrix::translate(4.0, 4.0),
        glyphs: glyphs.into(),
        render_mode,
    };
    let page = CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        content_bounds: None,
        bounds: PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: dim as f64,
                y1: dim as f64,
            },
            rotate: 0,
        },
        operations: Arc::from([DisplayOp::DrawGlyphRun {
            run: pdf_page_ir::GlyphRunId(0),
            paint: PaintId(0),
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
            stroke: None,
        }]),
        paths: Arc::from([]),
        paints: Arc::from([Paint::Solid(Color::from_rgb(0.0, 0.0, 0.0))]),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([run]),
        fonts: Arc::from([FontResource {
            key: ResourceKey {
                object_number: 0,
                generation: 0,
                variant: 0,
            },
            program: Vec::new().into(),
            face_index: 0,
            synthetic_shear: 0.0,
            synthetic_embolden_em: 0.0,
        }]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::TEXT,
        complexity: PageComplexity::default(),
    };
    let req = RenderRequest {
        page: Arc::new(page),
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
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };
    CpuBackend::default().render_to_host(&req).unwrap().0
}

fn painted(host: &HostPage, x: usize, y: usize) -> bool {
    let i = y * host.stride + x * 4;
    host.pixels[i] < 250 || host.pixels[i + 1] < 250 || host.pixels[i + 2] < 250
}

/// Any painted pixel in column `x` across rows `[y0,y1)`.
fn column_painted(host: &HostPage, x: usize, y0: usize, y1: usize) -> bool {
    (y0..y1).any(|y| painted(host, x, y))
}

#[test]
fn glyphs_paint_boxes_with_gaps() {
    // Three glyphs at x = 0, 24, 48, size 20 → boxes ~14 wide, ~4px gaps.
    let host = render_run(
        vec![glyph(65, 0.0), glyph(66, 24.0), glyph(67, 48.0)],
        20.0,
        0,
        80,
    );

    // A box exists near the first glyph (user x≈5..19, y≈4..16).
    assert!(column_painted(&host, 10, 4, 16), "first glyph box");
    // A box exists near the second glyph (shifted by advance 24).
    assert!(column_painted(&host, 34, 4, 16), "second glyph box");
    // The gap between the first two boxes is blank.
    assert!(
        !column_painted(&host, 24, 4, 16),
        "gap between glyphs is empty"
    );
}

#[test]
fn invisible_render_mode_paints_nothing() {
    // Render mode 3 = invisible text (OCR layers).
    let host = render_run(vec![glyph(65, 0.0), glyph(66, 24.0)], 20.0, 3, 80);
    let any = (0..80).any(|y| (0..80).any(|x| painted(&host, x, y)));
    assert!(!any, "invisible text paints nothing");
}

#[test]
fn spaces_leave_gaps() {
    // A space (code 32) between two glyphs draws no box.
    let host = render_run(
        vec![glyph(65, 0.0), glyph(32, 24.0), glyph(67, 48.0)],
        20.0,
        0,
        80,
    );
    // Column over the space glyph position (user x≈28) stays blank.
    assert!(
        !column_painted(&host, 28, 4, 16),
        "space glyph draws no box"
    );
    // But the real glyphs on either side paint.
    assert!(column_painted(&host, 10, 4, 16), "glyph before space");
    assert!(column_painted(&host, 58, 4, 16), "glyph after space");
}
