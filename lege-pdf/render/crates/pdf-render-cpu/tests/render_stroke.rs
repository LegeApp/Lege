#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Stroke tests (advice §4E): stroke-to-fill expansion, width, caps/joins,
//! and dashes, all reusing the fill pipeline.

use std::sync::Arc;

use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, LineCap, LineJoin, Matrix, PageBounds,
    PageComplexity, PageFeatures, Paint, PaintId, PathData, PathId, PathVerb, Point, Rect,
    StrokeStyle, StrokeStyleId,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;

fn line(pts: &[(f64, f64)]) -> PathData {
    let mut verbs = vec![PathVerb::MoveTo];
    verbs.extend(std::iter::repeat_n(PathVerb::LineTo, pts.len() - 1));
    PathData {
        verbs: verbs.into(),
        points: pts
            .iter()
            .map(|&(x, y)| Point { x, y })
            .collect::<Vec<_>>()
            .into(),
    }
}

fn style(width: f64, cap: LineCap, join: LineJoin, dash: &[f64]) -> StrokeStyle {
    StrokeStyle {
        width,
        cap,
        join,
        miter_limit: 10.0,
        dash_pattern: dash.to_vec().into(),
        dash_phase: 0.0,
    }
}

fn page(size: f64, path: PathData, style: StrokeStyle) -> CompiledPage {
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        content_bounds: None,
        bounds: PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: size,
                y1: size,
            },
            rotate: 0,
        },
        operations: Arc::from([DisplayOp::StrokePath {
            path: PathId(0),
            paint: PaintId(0),
            style: StrokeStyleId(0),
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        }]),
        paths: Arc::from([path]),
        paints: Arc::from([Paint::Solid(Color::from_rgb(0.0, 0.0, 0.0))]),
        stroke_styles: Arc::from([style]),
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

fn render(page: CompiledPage, dim: u32) -> HostPage {
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
    // Any non-white pixel (black stroke, possibly AA-gray).
    host.pixels[i] < 250 || host.pixels[i + 1] < 250 || host.pixels[i + 2] < 250
}

#[test]
fn horizontal_line_becomes_a_band() {
    // A width-6 horizontal line at y=10 covers roughly y in [7,13].
    let host = render(
        page(
            20.0,
            line(&[(2.0, 10.0), (18.0, 10.0)]),
            style(6.0, LineCap::Butt, LineJoin::Miter, &[]),
        ),
        20,
    );
    assert!(painted(&host, 10, 10), "line center");
    assert!(painted(&host, 10, 8), "within half-width above");
    assert!(painted(&host, 10, 12), "within half-width below");
    assert!(!painted(&host, 10, 2), "far above is clear");
    // Butt cap: nothing painted well before the start x.
    assert!(!painted(&host, 0, 10), "butt cap does not extend");
}

#[test]
fn square_cap_extends_past_the_end() {
    // Square cap should paint just beyond the endpoint x=15 (by half-width=2).
    let host = render(
        page(
            20.0,
            line(&[(5.0, 10.0), (15.0, 10.0)]),
            style(4.0, LineCap::Square, LineJoin::Miter, &[]),
        ),
        20,
    );
    assert!(
        painted(&host, 16, 10),
        "square cap extends past the endpoint"
    );
    assert!(!painted(&host, 18, 10), "but not arbitrarily far");
}

#[test]
fn miter_join_fills_the_corner() {
    // An L-shape; the outer miter corner near (14,14) is filled.
    let host = render(
        page(
            24.0,
            line(&[(4.0, 4.0), (14.0, 4.0), (14.0, 14.0)]),
            style(4.0, LineCap::Butt, LineJoin::Miter, &[]),
        ),
        24,
    );
    // The miter apex fills the outer corner beyond both segment bands: pixel
    // (15,3) lies in the miter quad (x∈[14,16], y∈[2,4]) but outside the
    // horizontal band (y∈[2,6], x≤14) and vertical band (x∈[12,16], y≥4).
    assert!(painted(&host, 15, 3), "miter apex fills the outer corner");
}

#[test]
fn dashes_produce_gaps() {
    // 4-on / 4-off dash along a long horizontal line: the run has both painted
    // and unpainted stretches.
    let host = render(
        page(
            40.0,
            line(&[(2.0, 20.0), (38.0, 20.0)]),
            style(4.0, LineCap::Butt, LineJoin::Miter, &[4.0, 4.0]),
        ),
        40,
    );
    let mut on = 0;
    let mut off = 0;
    for x in 2..38 {
        if painted(&host, x, 20) {
            on += 1;
        } else {
            off += 1;
        }
    }
    assert!(on > 4, "some dashes painted ({on})");
    assert!(off > 4, "some gaps present ({off})");
}

#[test]
fn thin_line_stays_visible() {
    // Width 0 (hairline) must render ~1px, not vanish.
    let host = render(
        page(
            20.0,
            line(&[(2.0, 10.0), (18.0, 10.0)]),
            style(0.0, LineCap::Butt, LineJoin::Miter, &[]),
        ),
        20,
    );
    assert!(painted(&host, 10, 10), "hairline is visible");
}

/// Like `page`, but concatenates `ctm` before stroking.
fn page_with_ctm(size: f64, ctm: Matrix, path: PathData, style: StrokeStyle) -> CompiledPage {
    let mut p = page(size, path, style);
    p.operations = Arc::from([
        DisplayOp::ConcatTransform(ctm),
        DisplayOp::StrokePath {
            path: PathId(0),
            paint: PaintId(0),
            style: StrokeStyleId(0),
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        },
    ]);
    p
}

/// Painted pixels along a device row.
fn row_run(host: &HostPage, y: usize, dim: usize) -> usize {
    (0..dim).filter(|&x| painted(host, x, y)).count()
}

/// Painted pixels down a device column.
fn col_run(host: &HostPage, x: usize, dim: usize) -> usize {
    (0..dim).filter(|&y| painted(host, x, y)).count()
}

#[test]
fn anisotropic_ctm_stretches_the_pen_into_an_ellipse() {
    // A stroke pen is a circle in *user* space, so under scale(8, 1) it must
    // land as an ellipse eight times wider than it is tall. A vertical line's
    // thickness is measured horizontally and so scales by 8; a horizontal
    // line's is measured vertically and so scales by 1. One scalar cannot do
    // both: the old sqrt(|det|) drew each of them sqrt(8) = 2.83 wide.
    let ctm = Matrix::scale(8.0, 1.0);

    // Vertical line at user x=4 -> device x=32, thickness 1*8 = 8px.
    let v = page_with_ctm(
        64.0,
        ctm,
        line(&[(4.0, 8.0), (4.0, 56.0)]),
        style(1.0, LineCap::Butt, LineJoin::Miter, &[]),
    );
    let hv = render(v, 64);
    let w = row_run(&hv, 32, 64);
    assert!(
        (7..=10).contains(&w),
        "vertical line under scale(8,1) should be ~8px wide, got {w}"
    );

    // Horizontal line at user y=32 -> device thickness 1*1 = 1px.
    let h = page_with_ctm(
        64.0,
        ctm,
        line(&[(1.0, 32.0), (7.0, 32.0)]),
        style(1.0, LineCap::Butt, LineJoin::Miter, &[]),
    );
    let hh = render(h, 64);
    let t = col_run(&hh, 32, 64);
    assert!(
        (1..=3).contains(&t),
        "horizontal line under scale(8,1) should be ~1px thick, got {t}"
    );

    // The point of the whole exercise: these must differ, and by roughly 8x.
    assert!(w > t * 3, "pen must be anisotropic: {w} wide vs {t} tall");
}

/// Count every painted pixel in a `dim × dim` grid.
fn painted_count(host: &HostPage, dim: usize) -> usize {
    (0..dim)
        .map(|y| (0..dim).filter(|&x| painted(host, x, y)).count())
        .sum()
}

#[test]
fn absurd_width_does_not_flood_the_page() {
    // A garbage line width — the shape a bad-deflate content stream decodes to
    // (`set-line-width 11016766` in 4778.pdf) — must not paint the whole page.
    // It falls back to a hairline instead of a page-covering pen.
    let dim = 64;
    let host = render(
        page(
            64.0,
            line(&[(20.0, 30.0), (44.0, 30.0)]),
            style(11_016_766.0, LineCap::Butt, LineJoin::Miter, &[]),
        ),
        dim,
    );
    let n = painted_count(&host, dim as usize);
    assert!(
        n < (dim * dim / 4) as usize,
        "absurd width flooded the page: {n}/{} px",
        dim * dim
    );
    assert!(
        painted(&host, 32, 30),
        "the line still renders as a hairline"
    );
}

#[test]
fn garbage_anisotropic_ctm_does_not_flood() {
    // A mangled CTM with a huge column magnitude (as decoded from corrupt
    // content) folds into the m1 scale, so a normal-width stroke would expand
    // into a page-covering smear. The device-extent clamp catches it.
    let ctm = Matrix {
        a: 50_288_544.0,
        b: 0.0,
        c: 0.0,
        d: 0.028_854_4,
        e: 0.0,
        f: 0.0,
    };
    // Path chosen so the segment lands on the page under this CTM
    // (device x = a·ux ≈ 32, device y = d·uy ≈ 32..43).
    let path = line(&[(6.36e-7, 1100.0), (6.36e-7, 1500.0)]);
    let dim = 64;
    let host = render(
        page_with_ctm(
            64.0,
            ctm,
            path,
            style(24.552, LineCap::Butt, LineJoin::Miter, &[]),
        ),
        dim,
    );
    let n = painted_count(&host, dim as usize);
    assert!(
        n < (dim * dim / 4) as usize,
        "garbage CTM flooded the page: {n}/{} px",
        dim * dim
    );
}

#[test]
fn off_viewport_coordinates_are_dropped() {
    // A path point light-years off the viewport (corrupt input) is dropped
    // rather than rasterized as a streak across the page.
    let dim = 64;
    let host = render(
        page(
            64.0,
            line(&[(10.0, 10.0), (1.0e9, 1.0e9)]),
            style(2.0, LineCap::Butt, LineJoin::Miter, &[]),
        ),
        dim,
    );
    let n = painted_count(&host, dim as usize);
    assert!(
        n < (dim * dim / 4) as usize,
        "off-viewport garbage flooded the page: {n}/{} px",
        dim * dim
    );
}

#[test]
fn legitimate_thick_stroke_is_not_clamped() {
    // Guard the clamp's threshold: a genuinely thick stroke (well within the
    // viewport) must render at its real width, not be mistaken for garbage.
    let dim = 64;
    let host = render(
        page(
            64.0,
            line(&[(32.0, 8.0), (32.0, 56.0)]),
            style(12.0, LineCap::Butt, LineJoin::Miter, &[]),
        ),
        dim,
    );
    let w = row_run(&host, 32, dim as usize);
    assert!(
        (10..=14).contains(&w),
        "12px stroke should stay ~12px wide, got {w}"
    );
}

#[test]
fn uniform_ctm_keeps_a_circular_pen() {
    // The decomposition must not disturb the common case: under a uniform
    // scale m2 is the identity and the pen stays round.
    let ctm = Matrix::scale(4.0, 4.0);
    let v = page_with_ctm(
        64.0,
        ctm,
        line(&[(4.0, 2.0), (4.0, 14.0)]),
        style(1.0, LineCap::Butt, LineJoin::Miter, &[]),
    );
    let hv = render(v, 64);
    let w = row_run(&hv, 32, 64);
    let h = page_with_ctm(
        64.0,
        ctm,
        line(&[(2.0, 8.0), (14.0, 8.0)]),
        style(1.0, LineCap::Butt, LineJoin::Miter, &[]),
    );
    let hh = render(h, 64);
    let t = col_run(&hh, 32, 64);
    assert_eq!(
        w, t,
        "uniform scale: horizontal and vertical thickness must match"
    );
    assert!(
        (3..=6).contains(&w),
        "width 1 under scale(4) should be ~4px, got {w}"
    );
}
