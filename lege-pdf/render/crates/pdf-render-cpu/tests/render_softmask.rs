#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Soft-mask tests (ISO 32000-1 §11.6.5.2): the mask group's content is
//! rendered offscreen, converted to a per-pixel alpha (luminosity/alpha), and
//! modulates subsequent painting.

use std::sync::Arc;

use pdf_page_ir::{
    BlendMode, Color, CompiledPage, DeviceSize, DisplayOp, FillRule, MaskKind, Matrix, PageBounds,
    PageComplexity, PageFeatures, Paint, PaintId, PathData, PathId, PathVerb, Point, Rect,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> PathData {
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

fn fill(path: u32, paint: u32) -> DisplayOp {
    DisplayOp::FillPath {
        path: PathId(path),
        paint: PaintId(paint),
        rule: FillRule::NonZero,
        alpha: 1.0,
        blend: BlendMode::Normal,
    }
}

fn render(paths: Vec<PathData>, paints: Vec<Paint>, ops: Vec<DisplayOp>, dim: u32) -> HostPage {
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
        operations: ops.into(),
        paths: paths.into(),
        paints: paints.into(),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::TRANSPARENCY,
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

fn px(host: &HostPage, x: usize, y: usize) -> [u8; 4] {
    let i = y * host.stride + x * 4;
    [
        host.pixels[i],
        host.pixels[i + 1],
        host.pixels[i + 2],
        host.pixels[i + 3],
    ]
}
fn red(p: [u8; 4]) -> bool {
    p[0] > 250 && p[1] < 8 && p[2] < 8
}
fn white(p: [u8; 4]) -> bool {
    p == [255, 255, 255, 255]
}

#[test]
fn luminosity_mask_gates_painting() {
    // Mask content: an opaque WHITE rect over the left half [0,10) → luminosity
    // 255 there (unmasked), and 0 elsewhere (outside the mask bounds). Then a
    // full-page RED fill survives only on the left half.
    let ops = vec![
        DisplayOp::BeginSoftMask {
            kind: MaskKind::Luminosity,
            transfer: None,
        },
        fill(0, 0), // white, left half — the mask content
        DisplayOp::EndSoftMask,
        fill(1, 1), // red, full page — masked
    ];
    let paths = vec![rect(0.0, 0.0, 10.0, 20.0), rect(0.0, 0.0, 20.0, 20.0)];
    let paints = vec![
        Paint::Solid(Color::from_rgb(1.0, 1.0, 1.0)),
        Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)),
    ];
    let host = render(paths, paints, ops, 20);

    assert!(
        red(px(&host, 5, 10)),
        "left half painted (mask bright): {:?}",
        px(&host, 5, 10)
    );
    assert!(
        white(px(&host, 15, 10)),
        "right half masked out: {:?}",
        px(&host, 15, 10)
    );
}

#[test]
fn dark_luminosity_mask_blocks_painting() {
    // A BLACK mask rect (luminosity 0) → fully masked; the red fill is blocked.
    let ops = vec![
        DisplayOp::BeginSoftMask {
            kind: MaskKind::Luminosity,
            transfer: None,
        },
        fill(0, 0), // black, covers the paint region
        DisplayOp::EndSoftMask,
        fill(1, 1), // red, masked out by the dark mask
    ];
    let paths = vec![rect(0.0, 0.0, 20.0, 20.0), rect(0.0, 0.0, 20.0, 20.0)];
    let paints = vec![
        Paint::Solid(Color::from_rgb(0.0, 0.0, 0.0)),
        Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)),
    ];
    let host = render(paths, paints, ops, 20);

    assert!(
        white(px(&host, 10, 10)),
        "dark mask blocks the fill: {:?}",
        px(&host, 10, 10)
    );
}

#[test]
fn empty_soft_mask_blocks_painting_instead_of_disabling_the_mask() {
    // A real empty Alpha/Luminosity mask has zero coverage everywhere. It is
    // distinct from `/SMask /None`, which is represented by `ClearSoftMask`.
    let ops = vec![
        DisplayOp::BeginSoftMask {
            kind: MaskKind::Alpha,
            transfer: None,
        },
        DisplayOp::EndSoftMask,
        fill(0, 0),
    ];
    let host = render(
        vec![rect(0.0, 0.0, 20.0, 20.0)],
        vec![Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))],
        ops,
        20,
    );

    assert!(
        white(px(&host, 10, 10)),
        "empty real soft mask blocks the fill: {:?}",
        px(&host, 10, 10)
    );
}

#[test]
fn clear_soft_mask_restores_unmasked_painting() {
    // Set a dark (blocking) mask, then /SMask /None, then paint red → the fill
    // is unmasked and shows.
    let ops = vec![
        DisplayOp::BeginSoftMask {
            kind: MaskKind::Luminosity,
            transfer: None,
        },
        fill(0, 0), // black mask
        DisplayOp::EndSoftMask,
        DisplayOp::ClearSoftMask,
        fill(1, 1), // red, no longer masked
    ];
    let paths = vec![rect(0.0, 0.0, 20.0, 20.0), rect(0.0, 0.0, 20.0, 20.0)];
    let paints = vec![
        Paint::Solid(Color::from_rgb(0.0, 0.0, 0.0)),
        Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)),
    ];
    let host = render(paths, paints, ops, 20);

    assert!(
        red(px(&host, 10, 10)),
        "cleared mask → unmasked fill: {:?}",
        px(&host, 10, 10)
    );
}

// --- A6: /BC backdrop color -------------------------------------------------

#[test]
fn bc_white_backdrop_unmasks_outside_mask_content() {
    // Same left-half white mask content, but with a WHITE /BC backdrop:
    // outside the mask content the luminosity is the backdrop's (255), so the
    // red fill now shows on BOTH halves. (With the default black backdrop the
    // right half is masked out — `luminosity_mask_gates_painting` above.)
    let ops = vec![
        DisplayOp::BeginSoftMask {
            kind: MaskKind::LuminosityBc {
                backdrop: [255, 255, 255],
            },
            transfer: None,
        },
        fill(0, 0), // white, left half — the mask content
        DisplayOp::EndSoftMask,
        fill(1, 1), // red, full page
    ];
    let paths = vec![rect(0.0, 0.0, 10.0, 20.0), rect(0.0, 0.0, 20.0, 20.0)];
    let paints = vec![
        Paint::Solid(Color::from_rgb(1.0, 1.0, 1.0)),
        Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)),
    ];
    let host = render(paths, paints, ops, 20);
    assert!(
        red(px(&host, 5, 10)),
        "inside mask content: {:?}",
        px(&host, 5, 10)
    );
    assert!(
        red(px(&host, 15, 10)),
        "outside content takes /BC luminosity: {:?}",
        px(&host, 15, 10)
    );
}

#[test]
fn bc_backdrop_composites_translucent_mask_content_against_it() {
    // A half-transparent BLACK mask rect over a WHITE /BC backdrop composites
    // to mid-gray → the red fill paints at roughly half strength (over white
    // → pink), instead of being fully blocked as over a black backdrop.
    let ops = vec![
        DisplayOp::BeginSoftMask {
            kind: MaskKind::LuminosityBc {
                backdrop: [255, 255, 255],
            },
            transfer: None,
        },
        DisplayOp::FillPath {
            path: PathId(0),
            paint: PaintId(0),
            rule: FillRule::NonZero,
            alpha: 0.5,
            blend: BlendMode::Normal,
        },
        DisplayOp::EndSoftMask,
        fill(1, 1),
    ];
    let paths = vec![rect(0.0, 0.0, 20.0, 20.0), rect(0.0, 0.0, 20.0, 20.0)];
    let paints = vec![
        Paint::Solid(Color::from_rgb(0.0, 0.0, 0.0)),
        Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)),
    ];
    let host = render(paths, paints, ops, 20);
    let p = px(&host, 10, 10);
    assert!(p[0] > 250, "red fully present in R: {p:?}");
    assert!(
        (p[1] as i32 - 128).abs() <= 4 && (p[2] as i32 - 128).abs() <= 4,
        "half-masked red over white is pink: {p:?}"
    );
}

#[test]
fn transfer_lut_inverts_a_luminosity_mask() {
    // /TR = inverting function, sampled to lut[i] = 255 - i. The white
    // left-half mask (luminosity 255) becomes 0 (blocked) and the outside
    // (luminosity 0) becomes 255 (unmasked) — the exact inverse of
    // `luminosity_mask_gates_painting`.
    let invert: pdf_page_ir::TransferLut = {
        let mut lut = [0u8; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            *v = 255 - i as u8;
        }
        Arc::new(lut)
    };
    let ops = vec![
        DisplayOp::BeginSoftMask {
            kind: MaskKind::Luminosity,
            transfer: Some(invert),
        },
        fill(0, 0), // white, left half — the mask content
        DisplayOp::EndSoftMask,
        fill(1, 1), // red, full page — masked through the inverting LUT
    ];
    let paths = vec![rect(0.0, 0.0, 10.0, 20.0), rect(0.0, 0.0, 20.0, 20.0)];
    let paints = vec![
        Paint::Solid(Color::from_rgb(1.0, 1.0, 1.0)),
        Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)),
    ];
    let host = render(paths, paints, ops, 20);

    assert!(
        white(px(&host, 5, 10)),
        "left half now blocked: {:?}",
        px(&host, 5, 10)
    );
    assert!(
        red(px(&host, 15, 10)),
        "right half now painted: {:?}",
        px(&host, 15, 10)
    );
}
