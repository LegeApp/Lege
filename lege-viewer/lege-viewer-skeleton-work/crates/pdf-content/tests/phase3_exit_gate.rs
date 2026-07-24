#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 3 exit gate (roadmap §7 Phase 3): the *same* `CompiledPage` is
//! consumed by two simple, independent backends — a debug SVG-like emitter and
//! a minimal CPU bitmap rasterizer — demonstrating the IR is not tied to one
//! rasterizer.
//!
//! Both backends live entirely in this test and share nothing but the public
//! `pdf_page_ir` contract: they walk `DisplayOp`s, maintain their own CTM
//! stack, and read the interned resource tables. Neither touches `pdf-content`
//! internals or any real render crate.

use std::fmt::Write as _;
use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{CompiledPage, DisplayOp, Matrix, Paint, PathData, PathVerb, Point};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn compile_test_page() -> CompiledPage {
    // A red rectangle and a blue stroked line under a translate, plus text.
    let content = b"q 1 0 0 1 30 40 cm \
        1 0 0 rg 0 0 50 20 re f \
        0 0 1 RG 2 w 0 0 m 50 20 l S \
        BT /F1 10 Tf 0 30 Td (Hi) Tj ET Q";
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", content);
    b.add_object(5, "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>");
    b.finish_classic_xref("/Root 1 0 R");
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(b.into_bytes()));
    let snapshot =
        DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed");
    let mut ctx = ParseContext::new();
    PageCompiler::new().compile(&snapshot, PageIndex(0), &mut ctx).expect("compile failed")
}

// --- Shared tiny CTM machinery (each backend keeps its own instance) --------

struct Ctm {
    cur: Matrix,
    stack: Vec<Matrix>,
}

impl Ctm {
    fn new(base: Matrix) -> Self {
        Self { cur: base, stack: Vec::new() }
    }
    fn save(&mut self) {
        self.stack.push(self.cur);
    }
    fn restore(&mut self) {
        if let Some(m) = self.stack.pop() {
            self.cur = m;
        }
    }
    fn concat(&mut self, m: Matrix) {
        self.cur = m.then(self.cur);
    }
    fn map(&self, p: Point) -> Point {
        self.cur.apply(p)
    }
}

/// Flatten a path into device-space polylines (one per subpath), sampling
/// cubics uniformly.
fn flatten(path: &PathData, ctm: &Ctm) -> Vec<Vec<Point>> {
    let mut subpaths = Vec::new();
    let mut cur: Vec<Point> = Vec::new();
    let mut pt = 0usize;
    let mut last = Point::default();
    for verb in path.verbs.iter() {
        match verb {
            PathVerb::MoveTo => {
                if !cur.is_empty() {
                    subpaths.push(std::mem::take(&mut cur));
                }
                last = path.points[pt];
                cur.push(ctm.map(last));
                pt += 1;
            }
            PathVerb::LineTo => {
                last = path.points[pt];
                cur.push(ctm.map(last));
                pt += 1;
            }
            PathVerb::CurveTo => {
                let p0 = last;
                let (c1, c2, p3) = (path.points[pt], path.points[pt + 1], path.points[pt + 2]);
                pt += 3;
                for i in 1..=16 {
                    let t = i as f64 / 16.0;
                    cur.push(ctm.map(cubic(p0, c1, c2, p3, t)));
                }
                last = p3;
            }
            PathVerb::Close => {
                if !cur.is_empty() {
                    subpaths.push(std::mem::take(&mut cur));
                }
            }
        }
    }
    if !cur.is_empty() {
        subpaths.push(cur);
    }
    subpaths
}

fn cubic(p0: Point, c1: Point, c2: Point, p3: Point, t: f64) -> Point {
    let u = 1.0 - t;
    let w = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
    Point {
        x: w[0] * p0.x + w[1] * c1.x + w[2] * c2.x + w[3] * p3.x,
        y: w[0] * p0.y + w[1] * c1.y + w[2] * c2.y + w[3] * p3.y,
    }
}

// --- Backend 1: debug SVG-like emitter --------------------------------------

/// Consumes the IR into an SVG string. Returns the SVG plus the number of fill
/// ops it drew (so the gate can confirm both backends saw the same work).
fn render_svg(page: &CompiledPage) -> (String, usize) {
    let mut ctm = Ctm::new(Matrix::IDENTITY);
    let mut out = String::new();
    let b = &page.bounds.crop;
    // Flip PDF's y-up space into SVG's y-down at the root.
    let _ = writeln!(
        out,
        "<svg viewBox=\"0 0 {} {}\"><g transform=\"matrix(1,0,0,-1,0,{})\">",
        b.width(),
        b.height(),
        b.height()
    );
    let mut fills = 0;
    for op in page.operations.iter() {
        match op {
            DisplayOp::Save => ctm.save(),
            DisplayOp::Restore => ctm.restore(),
            DisplayOp::ConcatTransform(m) => ctm.concat(*m),
            DisplayOp::FillPath { path, paint, .. } => {
                fills += 1;
                let d = svg_path_d(&page.paths[path.index()], &ctm);
                let _ = writeln!(out, "<path d=\"{d}\" fill=\"{}\"/>", css_color(&page.paints[paint.index()]));
            }
            DisplayOp::StrokePath { path, paint, .. } => {
                let d = svg_path_d(&page.paths[path.index()], &ctm);
                let _ = writeln!(
                    out,
                    "<path d=\"{d}\" fill=\"none\" stroke=\"{}\"/>",
                    css_color(&page.paints[paint.index()])
                );
            }
            DisplayOp::DrawGlyphRun { run, .. } => {
                let _ = writeln!(out, "<!-- glyph-run run#{} glyphs={} -->", run.0, page.glyph_runs[run.index()].glyphs.len());
            }
            DisplayOp::DrawImage { image, .. } => {
                let _ = writeln!(out, "<!-- image#{} -->", image.0);
            }
            _ => {}
        }
    }
    out.push_str("</g></svg>");
    (out, fills)
}

fn svg_path_d(path: &PathData, ctm: &Ctm) -> String {
    let mut d = String::new();
    let mut pt = 0usize;
    for verb in path.verbs.iter() {
        match verb {
            PathVerb::MoveTo => {
                let p = ctm.map(path.points[pt]);
                let _ = write!(d, "M{} {} ", n(p.x), n(p.y));
                pt += 1;
            }
            PathVerb::LineTo => {
                let p = ctm.map(path.points[pt]);
                let _ = write!(d, "L{} {} ", n(p.x), n(p.y));
                pt += 1;
            }
            PathVerb::CurveTo => {
                let (c1, c2, p3) =
                    (ctm.map(path.points[pt]), ctm.map(path.points[pt + 1]), ctm.map(path.points[pt + 2]));
                let _ = write!(d, "C{} {} {} {} {} {} ", n(c1.x), n(c1.y), n(c2.x), n(c2.y), n(p3.x), n(p3.y));
                pt += 3;
            }
            PathVerb::Close => d.push_str("Z "),
        }
    }
    d.trim_end().to_string()
}

fn css_color(paint: &Paint) -> String {
    match paint {
        Paint::Solid(c) => format!(
            "#{:02x}{:02x}{:02x}",
            (c.r * 255.0).round() as u8,
            (c.g * 255.0).round() as u8,
            (c.b * 255.0).round() as u8
        ),
        _ => "#000000".to_string(),
    }
}

fn n(v: f64) -> String {
    if v.fract() == 0.0 { format!("{}", v as i64) } else { format!("{v:.3}") }
}

// --- Backend 2: minimal CPU bitmap rasterizer -------------------------------

struct Bitmap {
    w: usize,
    px: Vec<[u8; 4]>,
}

impl Bitmap {
    fn get(&self, x: usize, y: usize) -> [u8; 4] {
        self.px[y * self.w + x]
    }
}

/// Consumes the IR into an RGBA bitmap by scanline-filling every FillPath.
/// Returns the bitmap and the number of fills drawn.
fn render_bitmap(page: &CompiledPage, w: usize, h: usize) -> (Bitmap, usize) {
    let b = &page.bounds.crop;
    // Map page user space (y-up) to pixel space (y-down).
    let sx = w as f64 / b.width();
    let sy = h as f64 / b.height();
    let base = Matrix { a: sx, b: 0.0, c: 0.0, d: -sy, e: -b.x0 * sx, f: b.y1 * sy };
    let mut ctm = Ctm::new(base);
    let mut px = vec![[255u8, 255, 255, 255]; w * h];
    let mut fills = 0;

    for op in page.operations.iter() {
        match op {
            DisplayOp::Save => ctm.save(),
            DisplayOp::Restore => ctm.restore(),
            DisplayOp::ConcatTransform(m) => ctm.concat(*m),
            DisplayOp::FillPath { path, paint, rule, .. } => {
                fills += 1;
                let color = rgba(&page.paints[paint.index()]);
                let subpaths = flatten(&page.paths[path.index()], &ctm);
                fill_polygon(&mut px, w, h, &subpaths, color, matches!(rule, pdf_page_ir::FillRule::EvenOdd));
            }
            _ => {}
        }
    }
    (Bitmap { w, px }, fills)
}

fn rgba(paint: &Paint) -> [u8; 4] {
    match paint {
        Paint::Solid(c) => [
            (c.r * 255.0).round() as u8,
            (c.g * 255.0).round() as u8,
            (c.b * 255.0).round() as u8,
            (c.a * 255.0).round() as u8,
        ],
        _ => [0, 0, 0, 255],
    }
}

/// Non-antialiased scanline polygon fill (nonzero or even-odd).
fn fill_polygon(px: &mut [[u8; 4]], w: usize, h: usize, subpaths: &[Vec<Point>], color: [u8; 4], even_odd: bool) {
    for y in 0..h {
        let yc = y as f64 + 0.5;
        let mut crossings: Vec<(f64, i32)> = Vec::new();
        for sp in subpaths {
            let len = sp.len();
            if len < 2 {
                continue;
            }
            for i in 0..len {
                let a = sp[i];
                let bpt = sp[(i + 1) % len];
                let (y0, y1) = (a.y, bpt.y);
                if (y0 <= yc && y1 > yc) || (y1 <= yc && y0 > yc) {
                    let t = (yc - y0) / (y1 - y0);
                    let x = a.x + t * (bpt.x - a.x);
                    crossings.push((x, if y1 > y0 { 1 } else { -1 }));
                }
            }
        }
        crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut span = |x0: f64, x1: f64| {
            let start = x0.round().max(0.0) as usize;
            let end = (x1.round() as i64).clamp(0, w as i64) as usize;
            for x in start..end.min(w) {
                px[y * w + x] = color;
            }
        };
        if even_odd {
            for pair in crossings.chunks(2) {
                if let [a, b] = pair {
                    span(a.0, b.0);
                }
            }
        } else {
            let mut wind = 0;
            let mut start = 0.0;
            for (x, dir) in crossings {
                let prev = wind;
                wind += dir;
                if prev == 0 && wind != 0 {
                    start = x;
                } else if prev != 0 && wind == 0 {
                    span(start, x);
                }
            }
        }
    }
}

// --- The exit gate ----------------------------------------------------------

#[test]
fn same_compiled_page_feeds_two_independent_backends() {
    let page = compile_test_page();

    let (svg, svg_fills) = render_svg(&page);
    let (bitmap, bmp_fills) = render_bitmap(&page, 200, 200);

    // Both backends consumed the same painter-order work.
    assert_eq!(svg_fills, bmp_fills, "backends disagree on fill count");
    assert!(svg_fills >= 1);

    // SVG backend: the red fill and blue stroke are present, positioned under
    // the translate (30,40) → the rect's first point is (30,40).
    assert!(svg.contains("fill=\"#ff0000\""), "{svg}");
    assert!(svg.contains("stroke=\"#0000ff\""), "{svg}");
    assert!(svg.contains("M30 40"), "{svg}");
    assert!(svg.contains("glyph-run run#0"), "{svg}");

    // Bitmap backend: the red rectangle covers user (30..80, 40..60), which
    // flips to device rows 140..160. A pixel at its center must be red.
    assert_eq!(bitmap.get(55, 150), [255, 0, 0, 255], "center of red rect");
    // A pixel well outside the rectangle stays white.
    assert_eq!(bitmap.get(5, 5), [255, 255, 255, 255], "background");

    // Determinism: each backend reproduces its output exactly.
    assert_eq!(render_svg(&page).0, svg);
    assert_eq!(render_bitmap(&page, 200, 200).1, bmp_fills);
    let bitmap2 = render_bitmap(&page, 200, 200).0;
    assert_eq!(bitmap2.px, bitmap.px);
}

#[test]
fn debug_dump_round_trips_the_same_page() {
    // The third "consumer": the IR's own schema-keyed debug serialization.
    let page = compile_test_page();
    let dump = page.debug_dump();
    assert!(dump.starts_with("CompiledPage schema="));
    assert!(dump.contains("fill path#"));
    assert!(dump.contains("stroke path#"));
    assert!(dump.contains("draw-glyph-run"));
}
