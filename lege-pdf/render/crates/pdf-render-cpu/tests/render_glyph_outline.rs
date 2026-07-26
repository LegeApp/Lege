#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Font Phase 2 integration: a glyph run whose FontResource carries an
//! embedded outline program renders the real glyph shape (via Skrifa), not a
//! synthetic box.

use std::sync::Arc;

use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, FontId, FontResource, GlyphRun, GlyphRunId, Matrix,
    PageBounds, PageComplexity, PageFeatures, Paint, PaintId, PlacedGlyph, Rect, ResourceKey,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;
use pdf_test_support::fonts::minimal_ttf;

fn render(program: Arc<[u8]>, dim: u32) -> HostPage {
    render_synth(program, dim, 0.0, 0.0)
}

fn render_synth(program: Arc<[u8]>, dim: u32, shear: f32, embolden_em: f32) -> HostPage {
    // One glyph (GID 1 = the triangle) at pen (0,0), size == units/em so the
    // font-unit coordinates map 1:1 into text space, then translate(10,10).
    let run = GlyphRun {
        font: FontId(0),
        font_size: 1000.0,
        transform: Matrix::translate(10.0, 10.0),
        glyphs: Arc::from([PlacedGlyph {
            glyph: 1,
            x: 0.0,
            y: 0.0,
        }]),
        render_mode: 0,
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
            run: GlyphRunId(0),
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
            program,
            face_index: 0,
            synthetic_shear: shear,
            synthetic_embolden_em: embolden_em,
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
    host.pixels[i] < 128
}

#[test]
fn embedded_outline_renders_the_glyph_shape() {
    // Triangle device vertices (110,60),(910,60),(510,660).
    let host = render(minimal_ttf().into(), 1000);

    // A point inside the triangle is painted.
    assert!(painted(&host, 510, 260), "triangle interior");
    // A bounding-box corner outside the triangle stays white — this is what
    // distinguishes a real outline from a filled placement box.
    assert!(
        !painted(&host, 160, 610),
        "outside the triangle (bbox corner)"
    );
    assert!(
        !painted(&host, 850, 610),
        "outside the triangle (other corner)"
    );
}

/// Render the triangle glyph in stroke render mode 1, with a black stroke of
/// the given user-space width over a white page.
fn render_stroke(width: f64, dim: u32) -> HostPage {
    let run = GlyphRun {
        font: FontId(0),
        font_size: 1000.0,
        transform: Matrix::translate(10.0, 10.0),
        glyphs: Arc::from([PlacedGlyph {
            glyph: 1,
            x: 0.0,
            y: 0.0,
        }]),
        render_mode: 1,
    };
    let style = pdf_page_ir::StrokeStyle {
        width,
        cap: pdf_page_ir::LineCap::Butt,
        join: pdf_page_ir::LineJoin::Miter,
        miter_limit: 10.0,
        dash_pattern: Arc::from([]),
        dash_phase: 0.0,
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
            run: GlyphRunId(0),
            paint: PaintId(0),
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
            stroke: Some(pdf_page_ir::GlyphStroke {
                paint: PaintId(0),
                style: pdf_page_ir::StrokeStyleId(0),
                alpha: 1.0,
            }),
        }]),
        paths: Arc::from([]),
        paints: Arc::from([Paint::Solid(Color::from_rgb(0.0, 0.0, 0.0))]),
        stroke_styles: Arc::from([style]),
        glyph_runs: Arc::from([run]),
        fonts: Arc::from([FontResource {
            key: ResourceKey {
                object_number: 0,
                generation: 0,
                variant: 0,
            },
            program: minimal_ttf().into(),
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

#[test]
fn stroke_render_mode_outlines_the_glyph() {
    // Tr 1 strokes the glyph outline: the triangle's *edges* paint but its
    // interior stays white — the opposite of the fill mode, which fills the
    // interior. (zusammenleben_englisch_2018 title: `1 Tr`.)
    let fill = render(minimal_ttf().into(), 1000);
    let stroke = render_stroke(20.0, 1000);
    // Fill paints the centroid; a thin stroke does not.
    assert!(painted(&fill, 510, 200), "fill paints the interior");
    assert!(
        !painted(&stroke, 510, 200),
        "stroke leaves the interior white"
    );
    // The stroke still marks the outline: the bottom edge near a vertex.
    assert!(
        painted(&stroke, 200, 68) || painted(&stroke, 150, 68),
        "stroke marks the base edge"
    );
    // A stroke paints far fewer pixels than a solid fill of the same glyph.
    assert!(
        painted_count(&stroke) < painted_count(&fill) / 2,
        "stroke ink {} must be well under fill ink {}",
        painted_count(&stroke),
        painted_count(&fill)
    );
}

#[test]
fn no_program_falls_back_to_boxes() {
    // An empty program → the synthetic-box path still renders something.
    let host = render(Vec::new().into(), 1000);
    let any = (0..1000)
        .step_by(20)
        .any(|y| (0..1000).step_by(20).any(|x| painted(&host, x, y)));
    assert!(any, "box fallback still paints");
}

fn painted_count(host: &HostPage) -> usize {
    let mut n = 0;
    for y in 0..host.height as usize {
        for x in 0..host.width as usize {
            if painted(host, x, y) {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn synthetic_embolden_grows_the_glyph() {
    // PDFium's weight-700 level (70/1000 em) must visibly thicken the
    // triangle: more painted pixels, and a point just outside the original
    // hypotenuse becomes ink.
    let base = render(minimal_ttf().into(), 1000);
    let bold = render_synth(minimal_ttf().into(), 1000, 0.0, 0.07);
    let (nb, nbold) = (painted_count(&base), painted_count(&bold));
    assert!(
        nbold as f64 > nb as f64 * 1.10,
        "embolden must grow coverage: base {nb}, bold {nbold}"
    );
    // Interior stays painted; far outside stays white (growth is local).
    assert!(painted(&bold, 510, 260), "interior still painted");
    assert!(!painted(&bold, 60, 900), "far corner stays white");
}

#[test]
fn synthetic_oblique_shears_toward_the_ascender() {
    // A 12-degree shear moves high-y (font space) points right: the apex
    // (font (500,650) -> device x ~648) paints where the upright glyph
    // does not.
    let base = render(minimal_ttf().into(), 1000);
    let slanted = render_synth(minimal_ttf().into(), 1000, 0.2125566, 0.0);
    // Near the apex (font y=630 -> row ~640) the upright triangle spans
    // device x ~[500, 540]; the shear adds ~0.2126*630 = 134.
    let (x, y) = (650usize, 640usize);
    assert!(
        !painted(&base, x, y),
        "upright glyph does not reach ({x},{y})"
    );
    assert!(slanted.pixels.len() == base.pixels.len());
    assert!(painted(&slanted, x, y), "sheared glyph reaches ({x},{y})");
    // And the un-sheared apex position is vacated.
    assert!(painted(&base, 510, 640), "upright apex column painted");
    assert!(
        !painted(&slanted, 510, 640),
        "sheared glyph vacates the upright apex"
    );
}
