#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Mesh shading parsing (types 1, 4, 5, 6, 7) end to end: real PDFs with
//! shading streams compile to the mesh `ShadingKind` IR variants with
//! decoded vertices/patches and resolved RGBA colors (PDFium
//! `cpdf_meshstream.cpp` semantics: `/BitsPer*` unpacking, `/Decode`
//! dequantization, per-vertex byte alignment, edge-sharing flags).

use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{CompiledPage, PageFeatures, ShadingKind};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed")
}

fn compile(snapshot: &DocumentSnapshot) -> CompiledPage {
    let mut ctx = ParseContext::new();
    PageCompiler::new().compile(snapshot, PageIndex(0), &mut ctx).expect("compile failed")
}

/// One-page PDF whose content is `/Sh1 sh` and whose `/Sh1` is a shading
/// stream with dictionary `shading_dict` and raw mesh `data`.
fn mesh_pdf(shading_dict: &str, data: &[u8]) -> CompiledPage {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    b.add_stream(5, shading_dict, data);
    b.finish_classic_xref("/Root 1 0 R");
    compile(&open(b.into_bytes()))
}

fn assert_rgb(c: &pdf_page_ir::Color, r: f32, g: f32, b: f32, what: &str) {
    assert!(
        (c.r - r).abs() < 0.02 && (c.g - g).abs() < 0.02 && (c.b - b).abs() < 0.02,
        "{what}: expected ({r},{g},{b}), got ({},{},{})",
        c.r,
        c.g,
        c.b
    );
}

// 8-bit everything: one type-4 vertex is [flag, x, y, r, g, b] — 6 bytes,
// naturally byte-aligned.
const T4_DICT: &str = "/ShadingType 4/ColorSpace/DeviceRGB/BitsPerCoordinate 8\
    /BitsPerComponent 8/BitsPerFlag 8/Decode[0 100 0 100 0 1 0 1 0 1]";

#[test]
fn type4_free_form_triangles_decode() {
    #[rustfmt::skip]
    let data: &[u8] = &[
        0, 0, 0, 255, 0, 0,       // flag 0: (0,0) red — starts a triangle
        0, 255, 0, 0, 255, 0,     // (100,0) green
        0, 128, 255, 0, 0, 255,   // (50.2,100) blue
        1, 0, 255, 255, 255, 255, // flag 1: share edge (v1,v2), add (0,100) white
    ];
    let page = mesh_pdf(T4_DICT, data);

    assert!(page.features.contains(PageFeatures::SHADINGS));
    assert_eq!(page.shadings.len(), 1);
    assert_eq!(page.shadings[0].shading_type, 4);
    let ShadingKind::MeshTriangles { triangles, .. } = &page.shadings[0].kind else {
        panic!("expected MeshTriangles, got {:?}", page.shadings[0].kind);
    };
    assert_eq!(triangles.len(), 2);
    let t0 = &triangles[0];
    assert!((t0[0].x, t0[0].y) == (0.0, 0.0), "v0 at origin: {:?}", (t0[0].x, t0[0].y));
    assert_rgb(&t0[0].color, 1.0, 0.0, 0.0, "v0 red");
    assert!((t0[1].x - 100.0).abs() < 0.01 && t0[1].y == 0.0);
    assert_rgb(&t0[1].color, 0.0, 1.0, 0.0, "v1 green");
    assert!((t0[2].x - 50.196).abs() < 0.01 && (t0[2].y - 100.0).abs() < 0.01);
    assert_rgb(&t0[2].color, 0.0, 0.0, 1.0, "v2 blue");
    // Flag 1 shares (v1, v2) of the previous triangle.
    let t1 = &triangles[1];
    assert!((t1[0].x - 100.0).abs() < 0.01 && t1[0].y == 0.0, "shared v1");
    assert!((t1[1].x - 50.196).abs() < 0.01, "shared v2");
    assert!(t1[2].x == 0.0 && (t1[2].y - 100.0).abs() < 0.01, "new vertex");
    assert_rgb(&t1[2].color, 1.0, 1.0, 1.0, "new vertex white");
}

#[test]
fn type4_function_maps_single_parametric_component() {
    // One color component per vertex, routed through an exponential
    // black→white function: components = 1 regardless of the RGB space.
    let dict = "/ShadingType 4/ColorSpace/DeviceRGB/BitsPerCoordinate 8\
        /BitsPerComponent 8/BitsPerFlag 8/Decode[0 100 0 100 0 1]\
        /Function<</FunctionType 2/Domain[0 1]/C0[0 0 0]/C1[1 1 1]/N 1>>";
    #[rustfmt::skip]
    let data: &[u8] = &[
        0, 0, 0, 0,       // (0,0) t=0 → black
        0, 255, 0, 255,   // (100,0) t=1 → white
        0, 128, 255, 128, // (50,100) t≈0.5 → mid gray
    ];
    let page = mesh_pdf(dict, data);
    let ShadingKind::MeshTriangles { triangles, .. } = &page.shadings[0].kind else {
        panic!("expected MeshTriangles");
    };
    assert_eq!(triangles.len(), 1);
    assert_rgb(&triangles[0][0].color, 0.0, 0.0, 0.0, "t=0 black");
    assert_rgb(&triangles[0][1].color, 1.0, 1.0, 1.0, "t=1 white");
    let mid = &triangles[0][2].color;
    assert!(mid.r > 0.4 && mid.r < 0.6, "t=0.5 mid gray: {}", mid.r);
}

#[test]
fn type5_lattice_rows_pair_into_triangles() {
    // 2 rows × 3 vertices, gray colorspace: vertex = [x, y, g], 3 bytes.
    let dict = "/ShadingType 5/ColorSpace/DeviceGray/BitsPerCoordinate 8\
        /BitsPerComponent 8/VerticesPerRow 3/Decode[0 100 0 100 0 1]";
    #[rustfmt::skip]
    let data: &[u8] = &[
        0, 0, 0,     128, 0, 128,   255, 0, 255, // row 0: y=0
        0, 255, 0,   128, 255, 128, 255, 255, 255, // row 1: y=100
    ];
    let page = mesh_pdf(dict, data);
    assert_eq!(page.shadings[0].shading_type, 5);
    let ShadingKind::MeshTriangles { triangles, .. } = &page.shadings[0].kind else {
        panic!("expected MeshTriangles");
    };
    // (rows−1) × (verts−1) cells × 2 triangles.
    assert_eq!(triangles.len(), 4);
    // First cell's first triangle contains the two leading row-0 vertices.
    let t = &triangles[0];
    assert!((t[0].x, t[0].y) == (0.0, 0.0));
    assert_rgb(&t[0].color, 0.0, 0.0, 0.0, "lattice v(0,0) black");
    assert!((t[1].x - 50.196).abs() < 0.01 && t[1].y == 0.0);
}

// Coons/tensor square [0,100]²: boundary p1..p12 counter-clockwise from the
// C(0,0) corner, colors red/green/blue/yellow at the four corners.
#[rustfmt::skip]
const PATCH_BOUNDARY: [[u8; 2]; 12] = [
    [0, 0], [0, 85], [0, 170], [0, 255],       // p1..p4:   left edge (u=0)
    [85, 255], [170, 255], [255, 255],         // p5..p7:   top edge (v=1)
    [255, 170], [255, 85], [255, 0],           // p8..p10:  right edge (u=1)
    [170, 0], [85, 0],                         // p11..p12: bottom edge (v=0)
];
const PATCH_COLORS: [[u8; 3]; 4] =
    [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];

const T6_DICT: &str = "/ShadingType 6/ColorSpace/DeviceRGB/BitsPerCoordinate 8\
    /BitsPerComponent 8/BitsPerFlag 8/Decode[0 100 0 100 0 1 0 1 0 1]";

fn coons_patch_bytes() -> Vec<u8> {
    let mut d = vec![0u8]; // flag 0
    for p in PATCH_BOUNDARY {
        d.extend_from_slice(&p);
    }
    for c in PATCH_COLORS {
        d.extend_from_slice(&c);
    }
    d
}

#[test]
fn type6_coons_patch_decodes_with_computed_interior() {
    let page = mesh_pdf(T6_DICT, &coons_patch_bytes());
    assert_eq!(page.shadings[0].shading_type, 6);
    let ShadingKind::MeshPatches { patches, .. } = &page.shadings[0].kind else {
        panic!("expected MeshPatches, got {:?}", page.shadings[0].kind);
    };
    assert_eq!(patches.len(), 1);
    let p = &patches[0];
    // Corner control points land on the four square corners
    // (row-major p[i][j] = points[i*4+j], i along u).
    assert_eq!(p.points[0], [0.0, 0.0], "P(0,0)");
    assert_eq!(p.points[3], [0.0, 100.0], "P(0,3)");
    assert_eq!(p.points[15], [100.0, 100.0], "P(3,3)");
    assert_eq!(p.points[12], [100.0, 0.0], "P(3,0)");
    // Coons interior points computed via §8.7.4.5.8: for a flat square
    // they land strictly inside it.
    for interior in [p.points[5], p.points[6], p.points[9], p.points[10]] {
        assert!(
            interior[0] > 0.0 && interior[0] < 100.0 && interior[1] > 0.0 && interior[1] < 100.0,
            "interior point inside the square: {interior:?}"
        );
    }
    // Corner colors in IR order [C(0,0), C(0,1), C(1,1), C(1,0)].
    assert_rgb(&p.colors[0], 1.0, 0.0, 0.0, "C(0,0) red");
    assert_rgb(&p.colors[1], 0.0, 1.0, 0.0, "C(0,1) green");
    assert_rgb(&p.colors[2], 0.0, 0.0, 1.0, "C(1,1) blue");
    assert_rgb(&p.colors[3], 1.0, 1.0, 0.0, "C(1,0) yellow");
}

#[test]
fn type6_edge_sharing_flag_reuses_previous_edge_and_colors() {
    // Second patch with flag 2: its first edge is the previous patch's
    // p7..p10 (the u=1 edge) and its first two corner colors are the
    // previous c3 (blue) and c4 (yellow). 8 new points + 2 new colors.
    let mut d = coons_patch_bytes();
    d.push(2); // flag 2
    #[rustfmt::skip]
    let new_points: [[u8; 2]; 8] = [
        [255, 170], [255, 85], [255, 0],   // continuation of the boundary
        [170, 0], [85, 0], [0, 0],
        [85, 85], [170, 170],
    ];
    for p in new_points {
        d.extend_from_slice(&p);
    }
    d.extend_from_slice(&[255, 255, 255]); // c3: white
    d.extend_from_slice(&[0, 0, 0]); // c4: black
    let page = mesh_pdf(T6_DICT, &d);
    let ShadingKind::MeshPatches { patches, .. } = &page.shadings[0].kind else {
        panic!("expected MeshPatches");
    };
    assert_eq!(patches.len(), 2);
    let p = &patches[1];
    // Shared edge: new p1..p4 = old p7..p10 → new P(0,0) = old (100,100).
    assert_eq!(p.points[0], [100.0, 100.0], "shared edge start");
    assert_eq!(p.points[3], [100.0, 0.0], "shared edge end");
    // Shared colors: C(0,0) = old C(1,1) blue, C(0,1) = old C(1,0) yellow.
    assert_rgb(&p.colors[0], 0.0, 0.0, 1.0, "shared C(1,1) blue");
    assert_rgb(&p.colors[1], 1.0, 1.0, 0.0, "shared C(1,0) yellow");
    assert_rgb(&p.colors[2], 1.0, 1.0, 1.0, "new white");
    assert_rgb(&p.colors[3], 0.0, 0.0, 0.0, "new black");
}

#[test]
fn type7_tensor_patch_reads_sixteen_points() {
    let dict = "/ShadingType 7/ColorSpace/DeviceRGB/BitsPerCoordinate 8\
        /BitsPerComponent 8/BitsPerFlag 8/Decode[0 100 0 100 0 1 0 1 0 1]";
    let mut d = vec![0u8];
    for p in PATCH_BOUNDARY {
        d.extend_from_slice(&p);
    }
    // Four explicit interior points (p13..p16 → P11, P12, P22, P21).
    for p in [[85u8, 85], [85, 170], [170, 170], [170, 85]] {
        d.extend_from_slice(&p);
    }
    for c in PATCH_COLORS {
        d.extend_from_slice(&c);
    }
    let page = mesh_pdf(dict, &d);
    assert_eq!(page.shadings[0].shading_type, 7);
    let ShadingKind::MeshPatches { patches, .. } = &page.shadings[0].kind else {
        panic!("expected MeshPatches");
    };
    assert_eq!(patches.len(), 1);
    let p = &patches[0];
    let close = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.5 && (a[1] - b[1]).abs() < 0.5;
    assert!(close(p.points[5], [33.3, 33.3]), "P(1,1): {:?}", p.points[5]);
    assert!(close(p.points[6], [33.3, 66.7]), "P(1,2): {:?}", p.points[6]);
    assert!(close(p.points[10], [66.7, 66.7]), "P(2,2): {:?}", p.points[10]);
    assert!(close(p.points[9], [66.7, 33.3]), "P(2,1): {:?}", p.points[9]);
}

#[test]
fn type1_function_based_samples_a_grid() {
    // Type 1 is a plain dictionary (no stream): identity-ish PostScript-free
    // setup using a Type 2 exponential per axis is impossible (2-in), so use
    // a sampled 2-in function: 2×2 grid, gray output = x quadrant.
    // FunctionType 0 with Size [2 2], BitsPerSample 8, samples row-major:
    // (0,0)→0, (1,0)→255, (0,1)→0, (1,1)→255 — gray ramps left→right.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    b.add_object(
        5,
        "<</ShadingType 1/ColorSpace/DeviceGray/Domain[0 1 0 1]\
         /Matrix[100 0 0 100 0 0]/Function 6 0 R>>",
    );
    b.add_stream(
        6,
        "/FunctionType 0/Domain[0 1 0 1]/Range[0 1]/Size[2 2]/BitsPerSample 8",
        &[0, 255, 0, 255],
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    assert_eq!(page.shadings.len(), 1);
    assert_eq!(page.shadings[0].shading_type, 1);
    let ShadingKind::FunctionGrid { domain, matrix, grid_w, grid_h, colors, .. } =
        &page.shadings[0].kind
    else {
        panic!("expected FunctionGrid, got {:?}", page.shadings[0].kind);
    };
    assert_eq!(*domain, [0.0, 1.0, 0.0, 1.0]);
    assert_eq!((matrix.a, matrix.d), (100.0, 100.0));
    assert_eq!(colors.len(), (grid_w * grid_h) as usize);
    // Left column dark, right column light (gray = x).
    let left = &colors[0];
    let right = &colors[(grid_w - 1) as usize];
    assert!(left.r < 0.1, "left dark: {}", left.r);
    assert!(right.r > 0.9, "right light: {}", right.r);
}

#[test]
fn malformed_mesh_degrades_to_unsupported_not_panic() {
    // Truncated data, bogus bit widths, missing /Decode: each must degrade
    // to the background hook (dropped for `sh`), never panic.
    for (dict, data) in [
        (T4_DICT, &[0u8, 1][..]), // truncated mid-vertex
        (
            "/ShadingType 4/ColorSpace/DeviceRGB/BitsPerCoordinate 7\
             /BitsPerComponent 8/BitsPerFlag 8/Decode[0 1 0 1 0 1 0 1 0 1]",
            &[0u8; 32][..], // invalid BitsPerCoordinate
        ),
        (
            "/ShadingType 6/ColorSpace/DeviceRGB/BitsPerCoordinate 8\
             /BitsPerComponent 8/BitsPerFlag 8",
            &[0u8; 64][..], // missing /Decode
        ),
        (
            "/ShadingType 5/ColorSpace/DeviceGray/BitsPerCoordinate 8\
             /BitsPerComponent 8/VerticesPerRow 999999/Decode[0 1 0 1 0 1]",
            &[0u8; 16][..], // hostile row width
        ),
    ] {
        let page = mesh_pdf(dict, data);
        // `sh` with an unusable shading paints nothing; compile stays green.
        assert!(
            page.shadings.is_empty()
                || matches!(page.shadings[0].kind, ShadingKind::Unsupported { .. }),
            "malformed mesh must degrade: {dict}"
        );
    }
}
