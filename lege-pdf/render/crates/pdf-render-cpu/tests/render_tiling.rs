#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 6B: the CPU backend renders tiling patterns (PatternType 1) by
//! replicating the compiled cell across the fill shape.

use std::sync::Arc;

use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, FillRule, FontId, FontResource, GlyphRun, LineCap,
    LineJoin, Matrix, PageBounds, PageComplexity, PageFeatures, Paint, PathData, PathVerb,
    PlacedGlyph, Point, Rect, ResourceKey, StrokeStyle, TilingId, TilingPattern,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
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

/// A cell CompiledPage that fills a 2x2 red square at its origin.
fn red_cell(color: Color) -> CompiledPage {
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
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        }]),
        paths: Arc::from([rect_path(0.0, 0.0, 2.0, 2.0)]),
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

fn page_with_tiling(tiling: TilingPattern) -> CompiledPage {
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        content_bounds: None,
        bounds: PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 12.0,
                y1: 12.0,
            },
            rotate: 0,
        },
        operations: Arc::from([DisplayOp::FillPath {
            path: pdf_page_ir::PathId(0),
            paint: pdf_page_ir::PaintId(0),
            rule: FillRule::NonZero,
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        }]),
        // Fill the rectangle [0,0]..[12,12] minus a margin: use [1,1]..[11,11].
        paths: Arc::from([rect_path(1.0, 1.0, 11.0, 11.0)]),
        paints: Arc::from([Paint::Pattern {
            tiling: TilingId(0),
            matrix: Matrix::IDENTITY,
        }]),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([tiling]),
        features: PageFeatures::PATTERNS,
        complexity: PageComplexity::default(),
    }
}

fn page_with_tiling_stroke(tiling: TilingPattern) -> CompiledPage {
    let line = PathData {
        verbs: Arc::from([PathVerb::MoveTo, PathVerb::LineTo]),
        points: Arc::from([Point { x: 2.0, y: 6.0 }, Point { x: 10.0, y: 6.0 }]),
    };
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        content_bounds: None,
        bounds: PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 12.0,
                y1: 12.0,
            },
            rotate: 0,
        },
        operations: Arc::from([DisplayOp::StrokePath {
            path: pdf_page_ir::PathId(0),
            paint: pdf_page_ir::PaintId(0),
            style: pdf_page_ir::StrokeStyleId(0),
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        }]),
        paths: Arc::from([line]),
        paints: Arc::from([Paint::Pattern {
            tiling: TilingId(0),
            matrix: Matrix::IDENTITY,
        }]),
        stroke_styles: Arc::from([StrokeStyle {
            width: 4.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 10.0,
            dash_pattern: Arc::from([]),
            dash_phase: 0.0,
        }]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([tiling]),
        features: PageFeatures::BASIC_PATHS | PageFeatures::PATTERNS,
        complexity: PageComplexity::default(),
    }
}

fn page_with_tiling_text(tiling: TilingPattern) -> CompiledPage {
    let run = GlyphRun {
        font: FontId(0),
        font_size: 8.0,
        transform: Matrix::translate(1.0, 1.0),
        glyphs: Arc::from([PlacedGlyph {
            glyph: b'A' as u32,
            x: 0.0,
            y: 0.0,
        }]),
        render_mode: 0,
    };
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        content_bounds: None,
        bounds: PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 12.0,
                y1: 12.0,
            },
            rotate: 0,
        },
        operations: Arc::from([DisplayOp::DrawGlyphRun {
            run: pdf_page_ir::GlyphRunId(0),
            paint: pdf_page_ir::PaintId(0),
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
            stroke: None,
        }]),
        paths: Arc::from([]),
        paints: Arc::from([Paint::Pattern {
            tiling: TilingId(0),
            matrix: Matrix::IDENTITY,
        }]),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([run]),
        // Empty program deliberately exercises the fallback glyph geometry;
        // real outlines enter the same finish_tiled_fill path.
        fonts: Arc::from([FontResource {
            key: ResourceKey {
                object_number: 0,
                generation: 0,
                variant: 0,
            },
            program: Arc::from([]),
            face_index: 0,
            synthetic_shear: 0.0,
            synthetic_embolden_em: 0.0,
        }]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([tiling]),
        features: PageFeatures::TEXT | PageFeatures::PATTERNS,
        complexity: PageComplexity::default(),
    }
}

fn request(page: CompiledPage, dim: u32) -> RenderRequest {
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
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
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

#[test]
fn colored_tiling_replicates_cell_across_fill() {
    // 4x4 cell with a 2x2 red square at origin; step 4 in both axes.
    let tiling = TilingPattern {
        key: ResourceKey {
            object_number: 0,
            generation: 0,
            variant: 0,
        },
        uncolored: false,
        under_color: Color::BLACK,
        bbox: [0.0, 0.0, 4.0, 4.0],
        x_step: 4.0,
        y_step: 4.0,
        cell: Arc::new(red_cell(Color::from_rgb(1.0, 0.0, 0.0))),
    };
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page_with_tiling(tiling), 12))
        .unwrap();

    // Tiles at x/y in {0,4,8}; the red square occupies [0,2) within each cell.
    // Inside the fill [1,11): pixel (1,1) is inside cell(0,0) red block.
    assert_eq!(px(&host, 1, 1), [255, 0, 0, 255], "first tile's square");
    // Pixel (5,5) is inside cell(1,1) red block ([4,6)).
    assert_eq!(px(&host, 5, 5), [255, 0, 0, 255], "second tile's square");
    // Pixel (3,3) is in the cell gap ([2,4)) → white background shows.
    assert_eq!(
        px(&host, 3, 3),
        [255, 255, 255, 255],
        "cell gap stays white"
    );
    // Outside the fill shape (0,0) stays white.
    assert_eq!(
        px(&host, 0, 0),
        [255, 255, 255, 255],
        "outside fill untouched"
    );
}

#[test]
fn uncolored_tiling_uses_under_color() {
    // Uncolored cell: its paint color is ignored; the under-color (green) is
    // painted through the cell's alpha stencil.
    let tiling = TilingPattern {
        key: ResourceKey {
            object_number: 0,
            generation: 0,
            variant: 0,
        },
        uncolored: true,
        under_color: Color::from_rgb(0.0, 1.0, 0.0),
        bbox: [0.0, 0.0, 4.0, 4.0],
        x_step: 4.0,
        y_step: 4.0,
        // Cell painted black; uncolored → recolored to the under-color.
        cell: Arc::new(red_cell(Color::BLACK)),
    };
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page_with_tiling(tiling), 12))
        .unwrap();
    assert_eq!(
        px(&host, 1, 1),
        [0, 255, 0, 255],
        "stencil painted with under-color"
    );
    assert_eq!(px(&host, 3, 3), [255, 255, 255, 255], "gap stays white");
}

#[test]
fn tiling_pattern_paints_stroke_outline() {
    let tiling = TilingPattern {
        key: ResourceKey {
            object_number: 0,
            generation: 0,
            variant: 0,
        },
        uncolored: false,
        under_color: Color::BLACK,
        bbox: [0.0, 0.0, 2.0, 2.0],
        x_step: 2.0,
        y_step: 2.0,
        cell: Arc::new(red_cell(Color::from_rgb(1.0, 0.0, 0.0))),
    };
    let (host, _) = CpuBackend::default()
        .render_to_host(&request(page_with_tiling_stroke(tiling), 12))
        .unwrap();

    assert_eq!(px(&host, 6, 6), [255, 0, 0, 255], "stroke band center");
    assert_eq!(px(&host, 3, 5), [255, 0, 0, 255], "stroke band interior");
    assert_eq!(px(&host, 6, 1), [255, 255, 255, 255], "above the band");
    assert_eq!(px(&host, 6, 10), [255, 255, 255, 255], "below the band");
    assert_eq!(px(&host, 1, 6), [255, 255, 255, 255], "left of the band");
}

#[test]
fn tiling_pattern_paints_text_fill() {
    let tiling = TilingPattern {
        key: ResourceKey {
            object_number: 0,
            generation: 0,
            variant: 0,
        },
        uncolored: false,
        under_color: Color::BLACK,
        // The existing red cell paints [0,2]²; make that the complete tile so
        // any glyph-covered destination pixel is red.
        bbox: [0.0, 0.0, 2.0, 2.0],
        x_step: 2.0,
        y_step: 2.0,
        cell: Arc::new(red_cell(Color::from_rgb(1.0, 0.0, 0.0))),
    };
    let (host, _) = CpuBackend::default()
        .render_to_host(&request(page_with_tiling_text(tiling), 12))
        .unwrap();

    let red_pixels = (0..12)
        .flat_map(|y| (0..12).map(move |x| (x, y)))
        .filter(|&(x, y)| px(&host, x, y) == [255, 0, 0, 255])
        .count();
    assert!(
        red_pixels > 0,
        "the tiling cell must paint through glyph coverage"
    );
    assert!(
        red_pixels < 20,
        "the pattern must stay clipped to the glyph, got {red_pixels} red pixels"
    );
    assert_eq!(
        px(&host, 10, 10),
        [255, 255, 255, 255],
        "outside the glyph remains untouched"
    );
}
