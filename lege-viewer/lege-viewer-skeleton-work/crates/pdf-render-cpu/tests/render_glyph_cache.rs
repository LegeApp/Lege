#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase B integration: a small, axis-aligned pure-fill glyph run renders its
//! real shape through the shared glyph coverage cache (populate-then-blit), and
//! repeat occurrences on the same backend are served from the cache and produce
//! identical pixels.

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

fn request(program: Arc<[u8]>, dim: u32) -> RenderRequest {
    // GID 1 (the triangle) at font size 100 (ppem 100 ≤ the cache cap), placed
    // by translate(10,10). Font units → text space at scale 0.1, so the device
    // triangle is (20,15),(100,15),(60,75).
    let run = GlyphRun {
        font: FontId(0),
        font_size: 100.0,
        transform: Matrix::translate(10.0, 10.0),
        glyphs: Arc::from([PlacedGlyph { glyph: 1, x: 0.0, y: 0.0 }]),
        render_mode: 0,
    };
    let page = CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds {
            crop: Rect { x0: 0.0, y0: 0.0, x1: dim as f64, y1: dim as f64 },
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
            key: ResourceKey { object_number: 0, generation: 0, variant: 0 },
            program,
            face_index: 0, synthetic_shear: 0.0, synthetic_embolden_em: 0.0,
        }]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::TEXT,
        complexity: PageComplexity::default(),
    };
    RenderRequest {
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
    }
}

fn painted(host: &HostPage, x: usize, y: usize) -> bool {
    let i = y * host.stride + x * 4;
    host.pixels[i] < 128
}

#[test]
fn small_glyph_renders_through_cache_and_repeats_identically() {
    let backend = CpuBackend::default();
    let req = request(minimal_ttf().into(), 128);

    // First render: cache miss populates, then blits.
    let a = backend.render_to_host(&req).unwrap().0;
    // Interior of the device triangle is painted; a corner outside it is not.
    assert!(painted(&a, 60, 35), "triangle interior painted via cache blit");
    assert!(!painted(&a, 95, 70), "bottom-right corner outside the triangle");
    assert!(!painted(&a, 22, 70), "bottom-left corner outside the triangle");

    // Second render on the SAME backend: served from the glyph cache, and
    // byte-identical to the first (a cache hit is deterministic).
    let b = backend.render_to_host(&req).unwrap().0;
    assert_eq!(a.pixels, b.pixels, "cache-hit render matches the populating render");
}
