#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Transparency-group tests (advice §9): the group's content composites into
//! a bounded offscreen surface, then composites back with the group's
//! opacity/blend as a unit.

use std::sync::Arc;

use pdf_page_ir::{
    BlendMode, Color, CompiledPage, DeviceSize, DisplayOp, FillRule, MaskKind, Matrix, PageBounds,
    PageComplexity, PageFeatures, Paint, PaintId, PathData, PathId, PathVerb, Point, Rect,
    TransparencyGroup, TransparencyGroupId,
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

fn fill(path: u32, paint: u32) -> DisplayOp {
    DisplayOp::FillPath {
        path: PathId(path),
        paint: PaintId(paint),
        rule: FillRule::NonZero,
        alpha: 1.0,
        blend: BlendMode::Normal,
    }
}

fn render(paths: Vec<PathData>, paints: Vec<Paint>, ops: Vec<DisplayOp>, groups: Vec<TransparencyGroup>, dim: u32) -> HostPage {
    let page = CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds { crop: Rect { x0: 0.0, y0: 0.0, x1: dim as f64, y1: dim as f64 }, rotate: 0 },
        operations: ops.into(),
        paths: paths.into(),
        paints: paints.into(),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: groups.into(),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::TRANSPARENCY,
        complexity: PageComplexity::default(),
    };
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
    CpuBackend::default().render_to_host(&req).unwrap().0
}

fn px(host: &HostPage, x: usize, y: usize) -> [u8; 4] {
    let i = y * host.stride + x * 4;
    [host.pixels[i], host.pixels[i + 1], host.pixels[i + 2], host.pixels[i + 3]]
}

fn near(p: [u8; 4], q: [u8; 4]) -> bool {
    (0..4).all(|i| (p[i] as i32 - q[i] as i32).abs() <= 1)
}

fn group(opacity: f32, blend: BlendMode) -> TransparencyGroup {
    TransparencyGroup {
        isolated: true,
        knockout: false,
        bounds: Rect { x0: 0.0, y0: 0.0, x1: 20.0, y1: 20.0 },
        opacity,
        blend,
    }
}

fn group_ni(opacity: f32, blend: BlendMode) -> TransparencyGroup {
    TransparencyGroup { isolated: false, ..group(opacity, blend) }
}

fn fill_with(path: u32, paint: u32, blend: BlendMode) -> DisplayOp {
    DisplayOp::FillPath { path: PathId(path), paint: PaintId(paint), rule: FillRule::NonZero, alpha: 1.0, blend }
}

#[test]
fn group_opacity_applies_to_the_whole_group() {
    // A group at opacity 0.5 containing one opaque red rect → 50% red over
    // white = premultiplied pink (255,128,128,255).
    let ops = vec![
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(0) },
        fill(0, 0),
        DisplayOp::EndTransparencyGroup,
    ];
    let host = render(
        vec![rect(4.0, 4.0, 16.0, 16.0)],
        vec![Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))],
        ops,
        vec![group(0.5, BlendMode::Normal)],
        20,
    );
    assert!(near(px(&host, 10, 10), [255, 128, 128, 255]), "{:?}", px(&host, 10, 10));
    assert!(near(px(&host, 1, 1), [255, 255, 255, 255]), "outside group");
}

#[test]
fn group_opacity_does_not_compound_on_overlap() {
    // The defining property: two overlapping OPAQUE rects inside a 50% group
    // are composited opaque *within the group*, so their overlap is also 50%
    // — not the darker double-alpha two separate 50% fills would give.
    let ops = vec![
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(0) },
        fill(0, 0),
        fill(1, 0),
        DisplayOp::EndTransparencyGroup,
    ];
    let host = render(
        vec![rect(2.0, 2.0, 12.0, 12.0), rect(8.0, 8.0, 18.0, 18.0)],
        vec![Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))],
        ops,
        vec![group(0.5, BlendMode::Normal)],
        20,
    );
    let solo = px(&host, 4, 4); // only first rect
    let overlap = px(&host, 10, 10); // both rects
    assert!(near(solo, [255, 128, 128, 255]), "solo {solo:?}");
    assert!(near(overlap, solo), "overlap must match solo (no compounding): {overlap:?} vs {solo:?}");
}

#[test]
fn full_opacity_group_is_transparent_passthrough() {
    // opacity 1.0, Normal blend, isolated → identical to painting directly.
    let ops = vec![
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(0) },
        fill(0, 0),
        DisplayOp::EndTransparencyGroup,
    ];
    let host = render(
        vec![rect(4.0, 4.0, 16.0, 16.0)],
        vec![Paint::Solid(Color::from_rgb(0.2, 0.4, 0.6))],
        ops,
        vec![group(1.0, BlendMode::Normal)],
        20,
    );
    assert!(near(px(&host, 10, 10), [51, 102, 153, 255]), "{:?}", px(&host, 10, 10));
}

#[test]
fn non_isolated_multiply_group_composites_against_page_backdrop() {
    // The Young-Turks gold-loss shape, minimized: an orange backdrop is painted
    // on the page, then a NON-isolated Normal wrapper group contains a
    // NON-isolated Multiply group with a blue fill. Per ISO 32000-1 §11.4.7 the
    // Multiply must apply against the backdrop showing through the (non-isolated)
    // wrapper: Multiply((1,0.5,0),(0,0.5,1)) = (0,0.25,0) = (0,64,0). Rendering
    // the wrapper isolated (transparent) makes the inner Multiply hit
    // transparency, degrade to source-over, and paint plain blue — the gold loss.
    let ops = vec![
        fill(0, 0), // orange backdrop directly on the page
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(0) }, // wrapper, Normal, non-isolated
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(1) }, // Multiply, non-isolated
        fill_with(1, 1, BlendMode::Normal), // blue fill (Normal inside the group)
        DisplayOp::EndTransparencyGroup,
        DisplayOp::EndTransparencyGroup,
    ];
    let host = render(
        vec![rect(2.0, 2.0, 18.0, 18.0), rect(6.0, 6.0, 14.0, 14.0)],
        vec![
            Paint::Solid(Color::from_rgb(1.0, 0.5, 0.0)),
            Paint::Solid(Color::from_rgb(0.0, 0.5, 1.0)),
        ],
        ops,
        vec![group_ni(1.0, BlendMode::Normal), group_ni(1.0, BlendMode::Multiply)],
        20,
    );
    assert!(near(px(&host, 10, 10), [0, 64, 0, 255]), "gold-loss repro: {:?}", px(&host, 10, 10));
    // Backdrop area covered by the wrapper bounds but not the Multiply fill:
    // orange must survive untouched (no Multiply halo from the seeded wrapper).
    assert!(near(px(&host, 3, 3), [255, 128, 0, 255]), "backdrop halo: {:?}", px(&host, 3, 3));
}

#[test]
fn non_isolated_normal_group_is_backdrop_passthrough() {
    // A non-isolated Normal group must be a pure passthrough: an orange backdrop
    // with a half-covering blue fill inside the group looks identical to drawing
    // the blue directly. Guards the seed+lerp composite-back against double-count.
    let ops = vec![
        fill(0, 0), // orange backdrop
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(0) },
        fill(1, 1), // blue over part of it
        DisplayOp::EndTransparencyGroup,
    ];
    let host = render(
        vec![rect(2.0, 2.0, 18.0, 18.0), rect(2.0, 2.0, 10.0, 18.0)],
        vec![
            Paint::Solid(Color::from_rgb(1.0, 0.5, 0.0)),
            Paint::Solid(Color::from_rgb(0.0, 0.5, 1.0)),
        ],
        ops,
        vec![group_ni(1.0, BlendMode::Normal)],
        20,
    );
    assert!(near(px(&host, 6, 10), [0, 128, 255, 255]), "blue area: {:?}", px(&host, 6, 10));
    assert!(near(px(&host, 14, 10), [255, 128, 0, 255]), "orange area: {:?}", px(&host, 14, 10));
}

#[test]
fn nested_groups_multiply_opacity() {
    // Outer 0.5 group containing an inner 0.5 group with a red rect →
    // effective 0.25 red over white → (255, 191, 191).
    let ops = vec![
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(0) },
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(1) },
        fill(0, 0),
        DisplayOp::EndTransparencyGroup,
        DisplayOp::EndTransparencyGroup,
    ];
    let host = render(
        vec![rect(4.0, 4.0, 16.0, 16.0)],
        vec![Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))],
        ops,
        vec![group(0.5, BlendMode::Normal), group(0.5, BlendMode::Normal)],
        20,
    );
    // 0.25 red over white: r=255, g=b=255*(1-0.25)=191.
    assert!(near(px(&host, 10, 10), [255, 191, 191, 255]), "{:?}", px(&host, 10, 10));
}

// --- Soft mask vs. transparency group (ISO 32000-1 §11.6.6) ----------------
//
// The soft mask in force when a group XObject is invoked modulates the
// *group's* composite; inside the group the mask starts out None. Getting only
// half of that right is what made `0041790.pdf` p50 render a drop shadow across
// the whole page instead of a band along one diagonal: the Multiply shadow was
// composited unmasked, darkening the artwork beneath it to 26% everywhere.

/// White over the left half → luminosity 255 there, 0 (outside the content)
/// on the right. `paths[0]` must be that left-half rect.
fn left_half_mask() -> Vec<DisplayOp> {
    vec![
        DisplayOp::BeginSoftMask { kind: MaskKind::Luminosity, transfer: None },
        fill(0, 1),
        DisplayOp::EndSoftMask,
    ]
}

#[test]
fn soft_mask_gates_a_group_composite() {
    let mut ops = left_half_mask();
    ops.extend([
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(0) },
        fill(1, 0), // opaque red, full page
        DisplayOp::EndTransparencyGroup,
    ]);
    let host = render(
        vec![rect(0.0, 0.0, 10.0, 20.0), rect(0.0, 0.0, 20.0, 20.0)],
        vec![Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)), Paint::Solid(Color::from_rgb(1.0, 1.0, 1.0))],
        ops,
        vec![group(1.0, BlendMode::Normal)],
        20,
    );
    assert!(near(px(&host, 5, 10), [255, 0, 0, 255]), "left half unmasked: {:?}", px(&host, 5, 10));
    assert!(
        near(px(&host, 15, 10), [255, 255, 255, 255]),
        "right half masked out of the group composite: {:?}",
        px(&host, 15, 10)
    );
}

#[test]
fn soft_mask_gates_a_non_isolated_group_composite() {
    // The seeded (non-isolated, Normal-blend) composite path is a lerp rather
    // than a source-over, so it masks by a different route and needs its own
    // cover.
    let mut ops = left_half_mask();
    ops.extend([
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(0) },
        fill(1, 0),
        DisplayOp::EndTransparencyGroup,
    ]);
    let host = render(
        vec![rect(0.0, 0.0, 10.0, 20.0), rect(0.0, 0.0, 20.0, 20.0)],
        vec![Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)), Paint::Solid(Color::from_rgb(1.0, 1.0, 1.0))],
        ops,
        vec![group_ni(1.0, BlendMode::Normal)],
        20,
    );
    assert!(near(px(&host, 5, 10), [255, 0, 0, 255]), "left half unmasked: {:?}", px(&host, 5, 10));
    assert!(
        near(px(&host, 15, 10), [255, 255, 255, 255]),
        "right half keeps the backdrop: {:?}",
        px(&host, 15, 10)
    );
}

#[test]
fn group_content_starts_with_the_soft_mask_reset() {
    // §11.6.6: the mask must not also apply to the content *inside* the group,
    // or it lands twice. Both halves would darken beyond the single
    // application asserted here if the reset were missing.
    //
    // Group opacity 0.5 over white: masked-in half is 50% red.
    let mut ops = left_half_mask();
    ops.extend([
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(0) },
        fill(1, 0),
        DisplayOp::EndTransparencyGroup,
    ]);
    let host = render(
        vec![rect(0.0, 0.0, 10.0, 20.0), rect(0.0, 0.0, 20.0, 20.0)],
        vec![Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)), Paint::Solid(Color::from_rgb(1.0, 1.0, 1.0))],
        ops,
        vec![group(0.5, BlendMode::Normal)],
        20,
    );
    assert!(near(px(&host, 5, 10), [255, 128, 128, 255]), "one application of 0.5: {:?}", px(&host, 5, 10));
    assert!(near(px(&host, 15, 10), [255, 255, 255, 255]), "masked out: {:?}", px(&host, 15, 10));
}

#[test]
fn unmasked_group_composite_is_unchanged() {
    // The no-mask path must be byte-identical to before the mask plumbing.
    let ops = vec![
        DisplayOp::BeginTransparencyGroup { group: TransparencyGroupId(0) },
        fill(0, 0),
        DisplayOp::EndTransparencyGroup,
    ];
    let host = render(
        vec![rect(0.0, 0.0, 20.0, 20.0)],
        vec![Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))],
        ops,
        vec![group(0.5, BlendMode::Normal)],
        20,
    );
    assert!(near(px(&host, 10, 10), [255, 128, 128, 255]), "{:?}", px(&host, 10, 10));
}
