#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 6 item A: the CPU backend rasterizes axial (type 2) and radial
//! (type 3) shadings, both via the `sh` operator (`DrawShading`) and as
//! shading-pattern fills (`Paint::Shading`).

use std::sync::Arc;

use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, FillRule, Matrix, PageBounds, PageComplexity,
    PageFeatures, Paint, PathData, PathVerb, Point, Rect, ShadingId, ShadingKind, ShadingResource,
    ResourceKey,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;

/// A 256-entry grayscale ramp from black to white.
fn gray_ramp() -> Arc<[Color]> {
    (0..256)
        .map(|i| {
            let v = i as f32 / 255.0;
            Color { r: v, g: v, b: v, a: 1.0 }
        })
        .collect()
}

fn shading_resource(kind: ShadingKind, shading_type: u8) -> ShadingResource {
    ShadingResource { key: ResourceKey { object_number: 0, generation: 0, variant: 0 }, shading_type, kind, bbox: None }
}

fn page_with(operations: Vec<DisplayOp>, paints: Vec<Paint>, paths: Vec<PathData>, shadings: Vec<ShadingResource>, size: f64) -> CompiledPage {
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        bounds: PageBounds { crop: Rect { x0: 0.0, y0: 0.0, x1: size, y1: size }, rotate: 0 },
        operations: operations.into(),
        paths: paths.into(),
        paints: paints.into(),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from([]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: shadings.into(),
        tilings: Arc::from([]),
        features: PageFeatures::SHADINGS,
        complexity: PageComplexity::default(),
    }
}

fn request(page: CompiledPage, dim: u32) -> RenderRequest {
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

fn px(host: &HostPage, x: usize, y: usize) -> [u8; 4] {
    let i = y * host.stride + x * 4;
    [host.pixels[i], host.pixels[i + 1], host.pixels[i + 2], host.pixels[i + 3]]
}

#[test]
fn axial_sh_paints_horizontal_gradient() {
    // Black→white axis from x=0 to x=10 across a 10px surface, extended.
    let kind = ShadingKind::Axial {
        coords: [0.0, 0.0, 10.0, 0.0],
        domain: [0.0, 1.0],
        extend: [true, true],
        ramp: gray_ramp(),
        background: None,
    };
    let page = page_with(
        vec![DisplayOp::DrawShading { shading: ShadingId(0), transform: Matrix::IDENTITY }],
        vec![],
        vec![],
        vec![shading_resource(kind, 2)],
        10.0,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 10)).unwrap();

    let left = px(&host, 0, 5)[0];
    let mid = px(&host, 5, 5)[0];
    let right = px(&host, 9, 5)[0];
    assert!(left < mid && mid < right, "monotone gradient: {left} {mid} {right}");
    assert!(left < 40, "left near black, got {left}");
    assert!(right > 210, "right near white, got {right}");
    // Rows are identical (gradient is purely horizontal).
    assert_eq!(px(&host, 3, 1), px(&host, 3, 8));
}

#[test]
fn bbox_clips_the_shading() {
    // A fully-extended axial gradient would cover the whole surface, but the
    // shading's /BBox [5,5,15,15] (§8.7.4.3) clips it to a centred box: pixels
    // outside stay white. The box is symmetric on a 20px surface, so any y-flip
    // in the page transform leaves the device box unchanged.
    let kind = ShadingKind::Axial {
        coords: [0.0, 0.0, 20.0, 0.0],
        domain: [0.0, 1.0],
        extend: [true, true],
        ramp: gray_ramp(),
        background: None,
    };
    let mut res = shading_resource(kind, 2);
    res.bbox = Some([5.0, 5.0, 15.0, 15.0]);
    let page = page_with(
        vec![DisplayOp::DrawShading { shading: ShadingId(0), transform: Matrix::IDENTITY }],
        vec![],
        vec![],
        vec![res],
        20.0,
    );
    let (host, _) = CpuBackend::default().render_to_host(&request(page, 20)).unwrap();
    assert_ne!(px(&host, 10, 10), [255, 255, 255, 255], "inside the bbox is painted");
    assert_eq!(px(&host, 2, 2), [255, 255, 255, 255], "outside the bbox (corner) is clear");
    assert_eq!(px(&host, 18, 10), [255, 255, 255, 255], "right of the bbox is clear");
    assert_eq!(px(&host, 10, 2), [255, 255, 255, 255], "above the bbox is clear");
}

#[test]
fn axial_without_extend_leaves_background_outside_axis() {
    // Axis occupies only x in [4,6]; no extend → outside stays white.
    let kind = ShadingKind::Axial {
        coords: [4.0, 0.0, 6.0, 0.0],
        domain: [0.0, 1.0],
        extend: [false, false],
        ramp: gray_ramp(),
        background: None,
    };
    let page = page_with(
        vec![DisplayOp::DrawShading { shading: ShadingId(0), transform: Matrix::IDENTITY }],
        vec![],
        vec![],
        vec![shading_resource(kind, 2)],
        10.0,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 10)).unwrap();
    assert_eq!(px(&host, 0, 5), [255, 255, 255, 255], "left of axis untouched");
    assert_eq!(px(&host, 9, 5), [255, 255, 255, 255], "right of axis untouched");
    assert!(px(&host, 5, 5)[0] < 200, "inside axis is shaded");
}

#[test]
fn radial_sh_is_radially_symmetric() {
    // Concentric circles centered at (5,5): r0=0 (black) → r1=5 (white).
    let kind = ShadingKind::Radial {
        coords: [5.0, 5.0, 0.0, 5.0, 5.0, 5.0],
        domain: [0.0, 1.0],
        extend: [true, true],
        ramp: gray_ramp(),
        background: None,
    };
    let page = page_with(
        vec![DisplayOp::DrawShading { shading: ShadingId(0), transform: Matrix::IDENTITY }],
        vec![],
        vec![],
        vec![shading_resource(kind, 3)],
        10.0,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 10)).unwrap();
    let center = px(&host, 5, 5)[0];
    let corner = px(&host, 0, 0)[0];
    assert!(center < 60, "center near black, got {center}");
    assert!(corner > 200, "corner (r>5, extended) near white, got {corner}");
    // The four pixels around the (5,5) center are equidistant (0.707) → equal.
    let a = px(&host, 4, 4);
    assert_eq!(a, px(&host, 5, 4));
    assert_eq!(a, px(&host, 4, 5));
    assert_eq!(a, px(&host, 5, 5));
}

#[test]
fn shading_pattern_fills_only_the_path() {
    // A shading pattern fill of a rectangle [2,2]..[8,8]; outside stays white.
    let kind = ShadingKind::Axial {
        coords: [0.0, 0.0, 10.0, 0.0],
        domain: [0.0, 1.0],
        extend: [true, true],
        ramp: gray_ramp(),
        background: None,
    };
    let verbs: Arc<[PathVerb]> =
        Arc::from([PathVerb::MoveTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::Close]);
    let points: Arc<[Point]> = Arc::from([
        Point { x: 2.0, y: 2.0 },
        Point { x: 8.0, y: 2.0 },
        Point { x: 8.0, y: 8.0 },
        Point { x: 2.0, y: 8.0 },
    ]);
    let page = page_with(
        vec![DisplayOp::FillPath {
            path: pdf_page_ir::PathId(0),
            paint: pdf_page_ir::PaintId(0),
            rule: FillRule::NonZero,
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        }],
        vec![Paint::Shading { shading: ShadingId(0), matrix: Matrix::IDENTITY }],
        vec![PathData { verbs, points }],
        vec![shading_resource(kind, 2)],
        10.0,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 10)).unwrap();
    assert_eq!(px(&host, 0, 0), [255, 255, 255, 255], "outside the path stays white");
    // Inside the path: horizontal gradient.
    let l = px(&host, 3, 5)[0];
    let r = px(&host, 7, 5)[0];
    assert!(l < r, "gradient inside path: {l} {r}");
}

#[test]
fn axial_pattern_background_fills_outside_the_axis() {
    // A shading-pattern rectangle fill [1,1]..[9,9] with a narrow axis on x in
    // [4,6], /Extend off, and a green /Background. Off the axis (but inside the
    // path) the background paints; on the axis the gradient shows. (`sh` ignores
    // /Background per §8.7.4.3, so this must be a pattern fill.)
    let kind = ShadingKind::Axial {
        coords: [4.0, 0.0, 6.0, 0.0],
        domain: [0.0, 1.0],
        extend: [false, false],
        ramp: gray_ramp(),
        background: Some(Color { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
    };
    let verbs: Arc<[PathVerb]> =
        Arc::from([PathVerb::MoveTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::LineTo, PathVerb::Close]);
    let points: Arc<[Point]> = Arc::from([
        Point { x: 1.0, y: 1.0 },
        Point { x: 9.0, y: 1.0 },
        Point { x: 9.0, y: 9.0 },
        Point { x: 1.0, y: 9.0 },
    ]);
    let page = page_with(
        vec![DisplayOp::FillPath {
            path: pdf_page_ir::PathId(0),
            paint: pdf_page_ir::PaintId(0),
            rule: FillRule::NonZero,
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        }],
        vec![Paint::Shading { shading: ShadingId(0), matrix: Matrix::IDENTITY }],
        vec![PathData { verbs, points }],
        vec![shading_resource(kind, 2)],
        10.0,
    );
    let (host, _) = CpuBackend::default().render_to_host(&request(page, 10)).unwrap();
    let bg = px(&host, 2, 5);
    assert!(bg[1] > 200 && bg[0] < 80 && bg[2] < 80, "off-axis takes the green background, got {bg:?}");
    let axis = px(&host, 5, 5);
    assert!(axis[0] == axis[1] && axis[1] == axis[2], "on-axis is the gray gradient, got {axis:?}");
    assert_eq!(px(&host, 0, 0), [255, 255, 255, 255], "outside the path stays white");
}

// --- A9: mesh shadings (types 4-7) + function-based type 1 -------------------

use pdf_page_ir::{Matrix as M9, MeshPatch, MeshVertex};

fn mesh_vertex(x: f32, y: f32, r: f32, g: f32, b: f32) -> MeshVertex {
    MeshVertex { x, y, color: Color { r, g, b, a: 1.0 } }
}

#[test]
fn gouraud_triangles_interpolate_and_leave_outside_unpainted() {
    // Two triangles tile the square [2,2]..[14,14]; corners R/G/B/K.
    let v00 = mesh_vertex(2.0, 2.0, 1.0, 0.0, 0.0); // red
    let v10 = mesh_vertex(14.0, 2.0, 0.0, 1.0, 0.0); // green
    let v11 = mesh_vertex(14.0, 14.0, 0.0, 0.0, 1.0); // blue
    let v01 = mesh_vertex(2.0, 14.0, 0.0, 0.0, 0.0); // black
    let kind = ShadingKind::MeshTriangles {
        triangles: Arc::from([[v00, v10, v11], [v00, v11, v01]]),
        background: None,
    };
    let page = page_with(
        vec![DisplayOp::DrawShading { shading: ShadingId(0), transform: M9::IDENTITY }],
        vec![],
        vec![],
        vec![shading_resource(kind, 4)],
        16.0,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 16)).unwrap();

    // Near the red corner.
    let p = px(&host, 3, 3);
    assert!(p[0] > 180 && p[1] < 90 && p[2] < 90, "red corner: {p:?}");
    // Near the blue corner.
    let p = px(&host, 13, 13);
    assert!(p[2] > 180 && p[0] < 90, "blue corner: {p:?}");
    // Outside the mesh: background stays white.
    assert_eq!(px(&host, 0, 0), [255, 255, 255, 255], "outside untouched");
    // On the shared diagonal the two triangles agree: smooth interpolation.
    let mid = px(&host, 8, 8);
    assert!(mid[0] < 200 && mid[2] < 200, "interior interpolated: {mid:?}");
}

#[test]
fn tensor_patch_fills_its_quad_with_bilinear_corners() {
    // A planar tensor patch: control points on the regular grid of the square
    // [2,2]..[14,14] (so the surface IS the square), corner colors
    // C(0,0)=red, C(0,1)=green, C(1,1)=blue, C(1,0)=yellow.
    let mut points = [[0.0f32; 2]; 16];
    for i in 0..4 {
        for j in 0..4 {
            points[i * 4 + j] = [2.0 + i as f32 * 4.0, 2.0 + j as f32 * 4.0];
        }
    }
    let patch = MeshPatch {
        points,
        colors: [
            Color { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
            Color { r: 0.0, g: 1.0, b: 0.0, a: 1.0 },
            Color { r: 0.0, g: 0.0, b: 1.0, a: 1.0 },
            Color { r: 1.0, g: 1.0, b: 0.0, a: 1.0 },
        ],
    };
    let kind = ShadingKind::MeshPatches { patches: Arc::from([patch]), background: None };
    let page = page_with(
        vec![DisplayOp::DrawShading { shading: ShadingId(0), transform: M9::IDENTITY }],
        vec![],
        vec![],
        vec![shading_resource(kind, 7)],
        16.0,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 16)).unwrap();

    // The patch covers its quad; corners take their colors.
    let p = px(&host, 3, 3); // (u≈0, v≈0) → red
    assert!(p[0] > 150 && p[2] < 110, "C(0,0) red-ish: {p:?}");
    let p = px(&host, 3, 13); // (u≈0, v≈1) → green
    assert!(p[1] > 150 && p[0] < 110, "C(0,1) green-ish: {p:?}");
    let p = px(&host, 13, 13); // (u≈1, v≈1) → blue
    assert!(p[2] > 150 && p[1] < 110, "C(1,1) blue-ish: {p:?}");
    let p = px(&host, 13, 3); // (u≈1, v≈0) → yellow
    assert!(p[0] > 150 && p[1] > 150 && p[2] < 110, "C(1,0) yellow-ish: {p:?}");
    assert_eq!(px(&host, 0, 0), [255, 255, 255, 255], "outside untouched");
}

#[test]
fn function_grid_shading_maps_domain_through_matrix() {
    // Type 1: a 2x2 color grid over domain [0,1]x[0,1], /Matrix scaling the
    // domain square onto [0,16]x[0,16]. Quadrants: red | green / blue | white.
    let colors: Arc<[Color]> = Arc::from([
        Color { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
        Color { r: 0.0, g: 1.0, b: 0.0, a: 1.0 },
        Color { r: 0.0, g: 0.0, b: 1.0, a: 1.0 },
        Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
    ]);
    let kind = ShadingKind::FunctionGrid {
        domain: [0.0, 1.0, 0.0, 1.0],
        matrix: M9::scale(16.0, 16.0),
        grid_w: 2,
        grid_h: 2,
        colors,
        background: None,
    };
    let page = page_with(
        vec![DisplayOp::DrawShading { shading: ShadingId(0), transform: M9::IDENTITY }],
        vec![],
        vec![],
        vec![shading_resource(kind, 1)],
        16.0,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 16)).unwrap();

    assert_eq!(px(&host, 4, 4)[..3], [255, 0, 0], "s<0.5, t<0.5 → grid[0]");
    assert_eq!(px(&host, 12, 4)[..3], [0, 255, 0], "s>0.5 → grid[1]");
    assert_eq!(px(&host, 4, 12)[..3], [0, 0, 255], "t>0.5 → grid[2]");
    assert_eq!(px(&host, 12, 12)[..3], [255, 255, 255], "grid[3]");
}
