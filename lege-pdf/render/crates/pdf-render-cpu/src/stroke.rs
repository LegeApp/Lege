//! Stroke expansion (performance advice §4E): convert a stroked polyline into
//! fill geometry once, during lowering, so strokes reuse the whole coverage +
//! span pipeline with no separate raster path.
//!
//! Each segment becomes a quad; joins and caps add small polygons (round =
//! disk/arc, bevel = triangles, miter = apex quad within the miter limit,
//! square cap = an extension quad). Every emitted polygon is oriented to the
//! same winding sign so a non-zero fill unions them correctly. Overlap at
//! joins is harmless — non-zero coverage saturates.
//!
//! Stroking is done in device space with a scalar half-width (exact for
//! uniform-scale CTMs; anisotropic/rotated pens are approximated until a
//! user-space stroker lands — documented).

use pdf_page_ir::{LineCap, LineJoin};

const ROUND_SEGMENTS: usize = 16;

/// Expand one polyline into stroke fill polygons appended to the arenas.
#[allow(clippy::too_many_arguments)]
pub fn expand_stroke(
    poly: &[[f32; 2]],
    closed: bool,
    hw: f32,
    cap: LineCap,
    join: LineJoin,
    miter_limit: f32,
    points: &mut Vec<[f32; 2]>,
    subpaths: &mut Vec<(usize, usize)>,
) {
    if hw <= 0.0 {
        return;
    }
    // Drop consecutive duplicate points (zero-length segments break normals).
    let mut pts: Vec<[f32; 2]> = Vec::with_capacity(poly.len());
    for &p in poly {
        if pts.last().map(|q| dist2(*q, p) > 1e-12).unwrap_or(true) {
            pts.push(p);
        }
    }
    if closed && pts.len() > 1 && pts.last().is_some_and(|&last| dist2(pts[0], last) <= 1e-12) {
        pts.pop();
    }
    if pts.len() < 2 {
        // Degenerate: a round/square cap on a dot, else nothing.
        if pts.len() == 1 && matches!(cap, LineCap::Round) {
            push_disk(pts[0], hw, points, subpaths);
        }
        return;
    }

    let n = pts.len();
    let seg_count = if closed { n } else { n - 1 };
    for i in 0..seg_count {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        let d = unit(sub(b, a));
        let nrm = [-d[1] * hw, d[0] * hw];
        push_quad(
            add(a, nrm),
            add(b, nrm),
            sub(b, nrm),
            sub(a, nrm),
            points,
            subpaths,
        );
    }

    // Joins at interior vertices (and all vertices when closed).
    let join_range = if closed { 0..n } else { 1..n - 1 };
    for i in join_range {
        let prev = pts[(i + n - 1) % n];
        let v = pts[i];
        let next = pts[(i + 1) % n];
        emit_join(prev, v, next, hw, join, miter_limit, points, subpaths);
    }

    // Caps on open ends.
    if !closed {
        emit_cap(pts[1], pts[0], hw, cap, points, subpaths); // start
        emit_cap(pts[n - 2], pts[n - 1], hw, cap, points, subpaths); // end
    }
}

/// Split a polyline into the "on" dash pieces (open subpaths) per a device-
/// space dash `pattern` and `phase`. An empty/zero pattern returns the whole
/// polyline unchanged.
pub fn dash_polyline(
    poly: &[[f32; 2]],
    closed: bool,
    pattern: &[f32],
    phase: f32,
) -> Vec<Vec<[f32; 2]>> {
    let total: f32 = pattern.iter().copied().filter(|d| *d >= 0.0).sum();
    if pattern.is_empty() || total <= 0.0 {
        return vec![poly.to_vec()];
    }
    let mut pts = poly.to_vec();
    if closed && pts.len() > 1 {
        pts.push(pts[0]);
    }
    if pts.len() < 2 {
        return Vec::new();
    }

    // DoS guard: a dash period far finer than the path emits one "on" piece per
    // period, so a sub-pixel period (e.g. `[0.0001] 0 d`, which clears the
    // all-zero `total <= 0` check above) would allocate unboundedly — a 14 KB
    // corpus file drove a 24 GiB `result` before aborting. Such a dash is
    // invisible at any real scale, so render solid, as PDFium effectively does.
    let path_len: f32 = pts.windows(2).map(|w| dist(w[0], w[1])).sum();
    const MAX_DASH_PERIODS: f32 = 100_000.0;
    if path_len / total > MAX_DASH_PERIODS {
        return vec![poly.to_vec()];
    }

    let mut idx = 0usize;
    let mut remaining = pattern[0];
    let mut on = true;
    // Consume the phase by walking the pattern. The dash state is periodic with
    // `total` (even-length pattern) or `2·total` (odd-length: one pass leaves
    // the on/off flipped), so reduce the phase into one cycle first — otherwise
    // a large phase against a small period walks the pattern unboundedly here.
    let cycle = if pattern.len() % 2 == 1 {
        total * 2.0
    } else {
        total
    };
    let mut ph = phase.max(0.0);
    if cycle > 0.0 {
        ph %= cycle;
    }
    while ph > 0.0 {
        if ph >= remaining {
            ph -= remaining;
            idx = (idx + 1) % pattern.len();
            remaining = pattern[idx];
            on = !on;
        } else {
            remaining -= ph;
            ph = 0.0;
        }
    }

    let mut result: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut cur: Vec<[f32; 2]> = Vec::new();
    for w in 0..pts.len() - 1 {
        let a = pts[w];
        let b = pts[w + 1];
        let seglen = dist(a, b);
        if seglen < 1e-9 {
            continue;
        }
        let dir = [(b[0] - a[0]) / seglen, (b[1] - a[1]) / seglen];
        let mut pos = 0.0;
        while pos < seglen - 1e-9 {
            let step = (seglen - pos).min(remaining.max(0.0));
            let p_start = [a[0] + dir[0] * pos, a[1] + dir[1] * pos];
            pos += step;
            let p_end = [a[0] + dir[0] * pos, a[1] + dir[1] * pos];
            if on {
                if cur.is_empty() {
                    cur.push(p_start);
                }
                cur.push(p_end);
            }
            remaining -= step;
            if remaining <= 1e-6 {
                if on && cur.len() >= 2 {
                    result.push(std::mem::take(&mut cur));
                }
                cur.clear();
                idx = (idx + 1) % pattern.len();
                remaining = pattern[idx];
                on = !on;
            }
        }
    }
    if on && cur.len() >= 2 {
        result.push(cur);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn emit_join(
    prev: [f32; 2],
    v: [f32; 2],
    next: [f32; 2],
    hw: f32,
    join: LineJoin,
    miter_limit: f32,
    points: &mut Vec<[f32; 2]>,
    subpaths: &mut Vec<(usize, usize)>,
) {
    let d0 = unit(sub(v, prev));
    let d1 = unit(sub(next, v));
    let n0 = [-d0[1] * hw, d0[0] * hw];
    let n1 = [-d1[1] * hw, d1[0] * hw];
    match join {
        LineJoin::Round => push_disk(v, hw, points, subpaths),
        LineJoin::Bevel => {
            // Fill both sides; the inner one is redundant but harmless.
            push_tri(v, add(v, n0), add(v, n1), points, subpaths);
            push_tri(v, sub(v, n0), sub(v, n1), points, subpaths);
        }
        LineJoin::Miter => {
            let cross = d0[0] * d1[1] - d0[1] * d1[0];
            let sign = if cross < 0.0 { 1.0 } else { -1.0 }; // outer side
            let p0 = add(v, scale(n0, sign));
            let p1 = add(v, scale(n1, sign));
            if let Some(apex) = intersect(p0, d0, p1, d1) {
                let miter_len = dist(apex, v) / hw;
                if miter_len <= miter_limit {
                    push_quad(v, p0, apex, p1, points, subpaths);
                    return;
                }
            }
            // Over the miter limit (or parallel): bevel.
            push_tri(v, add(v, n0), add(v, n1), points, subpaths);
            push_tri(v, sub(v, n0), sub(v, n1), points, subpaths);
        }
    }
}

/// Emit a cap at endpoint `end`; `inner` is the adjacent point (direction of
/// the line coming into the cap).
fn emit_cap(
    inner: [f32; 2],
    end: [f32; 2],
    hw: f32,
    cap: LineCap,
    points: &mut Vec<[f32; 2]>,
    subpaths: &mut Vec<(usize, usize)>,
) {
    match cap {
        LineCap::Butt => {}
        LineCap::Round => push_disk(end, hw, points, subpaths),
        LineCap::Square => {
            let d = unit(sub(end, inner)); // outward direction
            let nrm = [-d[1] * hw, d[0] * hw];
            let ext = add(end, scale(d, hw));
            push_quad(
                add(end, nrm),
                add(ext, nrm),
                sub(ext, nrm),
                sub(end, nrm),
                points,
                subpaths,
            );
        }
    }
}

// --- polygon emission (each oriented to positive signed area) ---------------

fn push_quad(
    a: [f32; 2],
    b: [f32; 2],
    c: [f32; 2],
    d: [f32; 2],
    points: &mut Vec<[f32; 2]>,
    subpaths: &mut Vec<(usize, usize)>,
) {
    push_poly(&[a, b, c, d], points, subpaths);
}

fn push_tri(
    a: [f32; 2],
    b: [f32; 2],
    c: [f32; 2],
    points: &mut Vec<[f32; 2]>,
    subpaths: &mut Vec<(usize, usize)>,
) {
    push_poly(&[a, b, c], points, subpaths);
}

fn push_disk(c: [f32; 2], r: f32, points: &mut Vec<[f32; 2]>, subpaths: &mut Vec<(usize, usize)>) {
    let mut poly = Vec::with_capacity(ROUND_SEGMENTS);
    for i in 0..ROUND_SEGMENTS {
        let t = i as f32 / ROUND_SEGMENTS as f32 * std::f32::consts::TAU;
        poly.push([c[0] + r * t.cos(), c[1] + r * t.sin()]);
    }
    push_poly(&poly, points, subpaths);
}

/// Append a polygon, forcing positive signed area so all stroke polygons share
/// one winding sign (non-zero union).
fn push_poly(poly: &[[f32; 2]], points: &mut Vec<[f32; 2]>, subpaths: &mut Vec<(usize, usize)>) {
    if poly.len() < 3 {
        return;
    }
    let start = points.len();
    if signed_area(poly) >= 0.0 {
        points.extend_from_slice(poly);
    } else {
        points.extend(poly.iter().rev().copied());
    }
    subpaths.push((start, points.len()));
}

fn signed_area(poly: &[[f32; 2]]) -> f32 {
    let mut a = 0.0;
    for i in 0..poly.len() {
        let p = poly[i];
        let q = poly[(i + 1) % poly.len()];
        a += p[0] * q[1] - q[0] * p[1];
    }
    a * 0.5
}

// --- vector helpers ---------------------------------------------------------

#[inline]
fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}
#[inline]
fn add(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] + b[0], a[1] + b[1]]
}
#[inline]
fn scale(a: [f32; 2], s: f32) -> [f32; 2] {
    [a[0] * s, a[1] * s]
}
#[inline]
fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let d = sub(a, b);
    d[0] * d[0] + d[1] * d[1]
}
#[inline]
fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    dist2(a, b).sqrt()
}
#[inline]
fn unit(a: [f32; 2]) -> [f32; 2] {
    let len = (a[0] * a[0] + a[1] * a[1]).sqrt();
    if len > 1e-12 {
        [a[0] / len, a[1] / len]
    } else {
        [0.0, 0.0]
    }
}

/// Intersection of line (`p0` + t·`d0`) with (`p1` + u·`d1`).
fn intersect(p0: [f32; 2], d0: [f32; 2], p1: [f32; 2], d1: [f32; 2]) -> Option<[f32; 2]> {
    let denom = d0[0] * d1[1] - d0[1] * d1[0];
    if denom.abs() < 1e-9 {
        return None;
    }
    let dx = p1[0] - p0[0];
    let dy = p1[1] - p0[1];
    let t = (dx * d1[1] - dy * d1[0]) / denom;
    Some([p0[0] + t * d0[0], p0[1] + t * d0[1]])
}

#[cfg(test)]
mod dash_tests {
    use super::dash_polyline;

    /// A dash period far finer than the path renders solid rather than emitting
    /// an unbounded number of pieces (the 467170761.pdf 24 GiB abort).
    #[test]
    fn sub_period_dash_renders_solid_and_bounded() {
        let poly = [[0.0f32, 0.0], [10_000.0, 0.0]];
        // period 0.0002 over a 10k path = 50M periods; must not allocate per piece.
        let out = dash_polyline(&poly, false, &[0.0001, 0.0001], 0.0);
        assert_eq!(
            out.len(),
            1,
            "sub-pixel dash should collapse to one solid run"
        );
        assert_eq!(out[0].len(), 2, "solid run is the original polyline");
    }

    /// A tiny nonzero single-entry pattern (clears the all-zero total guard) is
    /// also bounded.
    #[test]
    fn tiny_single_entry_pattern_is_bounded() {
        let poly = [[0.0f32, 0.0], [50_000.0, 0.0]];
        let out = dash_polyline(&poly, false, &[0.0001], 0.0);
        assert_eq!(out.len(), 1);
    }

    /// A normal, visible dash still splits into multiple on-pieces.
    #[test]
    fn normal_dash_still_dashes() {
        let poly = [[0.0f32, 0.0], [100.0, 0.0]];
        // 10-on / 10-off over 100 units → 5 on-pieces.
        let out = dash_polyline(&poly, false, &[10.0, 10.0], 0.0);
        assert_eq!(out.len(), 5, "expected 5 dash pieces, got {}", out.len());
        for piece in &out {
            assert!(piece.len() >= 2);
        }
    }

    /// A huge phase against a small period must not walk the pattern unboundedly.
    #[test]
    fn large_phase_is_reduced() {
        let poly = [[0.0f32, 0.0], [100.0, 0.0]];
        let out = dash_polyline(&poly, false, &[10.0, 10.0], 1e9);
        // Just needs to terminate and produce a sane result.
        assert!(out.len() <= 6);
    }
}
