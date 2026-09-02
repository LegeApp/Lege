//! Bitmap → quadratic Bézier outlines for the glyph font.
//!
//! The prototype bitmaps are staircases: their pixel-edge polygons carry one
//! vertex per pixel step, and a font made of them is large and renders with
//! visible steps at any size other than the source resolution. This module
//! fits the boundary with straight lines and quadratic Béziers (the TrueType
//! curve type) to within [`FIT_TOLERANCE`] pixels, keeps corners sharp, and
//! checks its own result by rasterizing the outline back over the bitmap.
//! When the check fails the caller falls back to the staircase, so a bad fit
//! can cost bytes but never fidelity.
//!
//! Pipeline per boundary loop: corner detection on the pixel-corner polygon
//! (turning angle over a short window), then each arc between corners is
//! fitted with a least-squares quadratic under chord-length
//! parametrization, split at the worst point until the fit is within
//! tolerance; arcs that are straight within tolerance become lines.

use jbig2enc_rust::jbig2sym::BitImage;

use crate::encoding::glyphfont::{UNITS_PER_PIXEL, trace_pixel_loops};
use crate::truetype_writer::OutlinePoint;

/// Largest distance (pixels) a fitted curve may stray from the pixel
/// corners it represents. The true boundary lies within half a pixel of the
/// staircase, so a little over that lets a curve pass through the middle of
/// the steps without chasing them.
pub const FIT_TOLERANCE: f64 = 0.6;
/// Corners are judged over this many polygon vertices on each side.
const CORNER_WINDOW: usize = 3;
/// Turning angle (degrees) over the window that marks a corner.
const CORNER_ANGLE_DEG: f64 = 70.0;
/// Loops with fewer pixel corners than this stay staircases: a dot or a
/// speck has no curve worth fitting.
const MIN_LOOP_CORNERS: usize = 12;
/// Segments per quadratic when flattening for the fidelity check.
const FLATTEN_STEPS: usize = 8;

/// Pixel-space point with its TrueType on/off-curve flag, y down.
type FitPoint = (f64, f64, bool);

/// Fit `bitmap`'s boundary with lines and quadratics, in font units with the
/// glyph origin `origin_x` pixels right of the bitmap's left edge and the
/// baseline along the top edge of row `baseline_row` (the conventions of
/// [`trace_outline_at`](crate::encoding::glyphfont::trace_outline_at)).
/// `None` when the fitted outline does not reproduce the bitmap.
pub fn vectorize(
    bitmap: &BitImage,
    origin_x: i32,
    baseline_row: i32,
) -> Option<Vec<Vec<OutlinePoint>>> {
    let contours = fit_bitmap(bitmap)?;
    let to_unit = |v: f64| -> i16 {
        (v * UNITS_PER_PIXEL as f64)
            .round()
            .clamp(-32768.0, 32767.0) as i16
    };
    Some(
        contours
            .into_iter()
            .map(|c| {
                c.into_iter()
                    .map(|(x, y, on_curve)| OutlinePoint {
                        x: to_unit(x + origin_x as f64),
                        y: to_unit(baseline_row as f64 - y),
                        on_curve,
                    })
                    .collect()
            })
            .collect(),
    )
}

/// The fitted contours in pixel coordinates (y down), verified against the
/// bitmap.
fn fit_bitmap(bitmap: &BitImage) -> Option<Vec<Vec<FitPoint>>> {
    let loops = trace_pixel_loops(bitmap);
    let contours: Vec<Vec<FitPoint>> = loops
        .iter()
        .map(|lp| {
            let pts: Vec<(f64, f64)> = lp.iter().map(|&(x, y)| (x as f64, y as f64)).collect();
            if pts.len() < MIN_LOOP_CORNERS {
                staircase(&pts)
            } else {
                fit_loop(&pts)
            }
        })
        .filter(|c| c.len() >= 3)
        .map(|c| c.into_iter().map(quantize).collect())
        .collect();
    faithful(bitmap, &contours).then_some(contours)
}

/// Snap a point to the grid the font will carry: on-curve points to half
/// pixels (pixel corners and edge midpoints already are), control points to
/// whole pixels. Coarser coordinates deflate far better in the PDF, and a
/// control point moved by half a pixel bends its curve by at most a quarter.
fn quantize((x, y, on_curve): FitPoint) -> FitPoint {
    let step = if on_curve { 0.5 } else { 1.0 };
    (
        (x / step).round() * step,
        (y / step).round() * step,
        on_curve,
    )
}

/// The polygon itself, collinear vertices removed, every point on-curve.
fn staircase(pts: &[(f64, f64)]) -> Vec<FitPoint> {
    let n = pts.len();
    (0..n)
        .filter(|&i| {
            let prev = pts[(i + n - 1) % n];
            let p = pts[i];
            let next = pts[(i + 1) % n];
            (p.0 - prev.0, p.1 - prev.1) != (next.0 - p.0, next.1 - p.1)
        })
        .map(|i| (pts[i].0, pts[i].1, true))
        .collect()
}

/// Corner detection plus per-arc fitting over one closed polygon.
fn fit_loop(pts: &[(f64, f64)]) -> Vec<FitPoint> {
    let n = pts.len();
    let mut corners = detect_corners(pts);
    if corners.len() < 2 {
        // A smooth loop (an `o`) is fitted as two arcs; a single corner gets
        // its opposite point as the second split.
        let first = corners.first().copied().unwrap_or(0);
        corners = vec![first, (first + n / 2) % n];
        corners.sort_unstable();
    }
    let start = pts[corners[0]];
    let mut out: Vec<FitPoint> = vec![(start.0, start.1, true)];
    for (k, &c0) in corners.iter().enumerate() {
        let c1 = corners[(k + 1) % corners.len()];
        // Samples between two corners are the midpoints of the pixel edges,
        // not the pixel corners: on a slanted edge the corners zigzag half a
        // pixel either side of the true boundary while the midpoints sit on
        // it, and the corners at the ends are kept exact.
        let steps = if c1 > c0 { c1 - c0 } else { c1 + n - c0 };
        let mut arc: Vec<(f64, f64)> = Vec::with_capacity(steps + 1);
        arc.push(pts[c0]);
        for j in 0..steps {
            let a = pts[(c0 + j) % n];
            let b = pts[(c0 + j + 1) % n];
            arc.push(((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0));
        }
        arc.push(pts[c1]);
        fit_arc(&arc, &mut out);
    }
    // The last arc ends where the contour started; TrueType closes it.
    out.pop();
    out
}

/// Indices of vertices where the boundary turns sharply, non-maximum
/// suppressed within the window.
fn detect_corners(pts: &[(f64, f64)]) -> Vec<usize> {
    let n = pts.len();
    let k = CORNER_WINDOW.min(n / 4).max(1);
    let angle = |i: usize| -> f64 {
        let a = pts[(i + n - k) % n];
        let p = pts[i];
        let b = pts[(i + k) % n];
        let v1 = (p.0 - a.0, p.1 - a.1);
        let v2 = (b.0 - p.0, b.1 - p.1);
        let l1 = (v1.0 * v1.0 + v1.1 * v1.1).sqrt();
        let l2 = (v2.0 * v2.0 + v2.1 * v2.1).sqrt();
        if l1 == 0.0 || l2 == 0.0 {
            return 0.0;
        }
        let cos = ((v1.0 * v2.0 + v1.1 * v2.1) / (l1 * l2)).clamp(-1.0, 1.0);
        cos.acos().to_degrees()
    };
    // The turning angle says whether a corner is near; on a staircase the
    // vertex next to a true corner can turn even more sharply over the
    // window than the corner itself, so the corner's exact vertex is the
    // one farthest from the chord across the window.
    let strength = |i: usize| -> f64 {
        let a = pts[(i + n - k) % n];
        let b = pts[(i + k) % n];
        max_distance_to_segment(&[pts[i]], a, b)
    };
    let angles: Vec<f64> = (0..n).map(angle).collect();
    let strengths: Vec<f64> = (0..n).map(strength).collect();
    (0..n)
        .filter(|&i| {
            if angles[i] < CORNER_ANGLE_DEG {
                return false;
            }
            (1..=k).all(|d| {
                let before = (i + n - d) % n;
                let after = (i + d) % n;
                let beats = |j: usize, tie_wins: bool| {
                    angles[j] < CORNER_ANGLE_DEG
                        || strengths[i] > strengths[j]
                        || (tie_wins && strengths[i] == strengths[j])
                };
                beats(before, false) && beats(after, true)
            })
        })
        .collect()
}

/// Append the fit of `arc` (its first point already emitted) to `out`.
fn fit_arc(arc: &[(f64, f64)], out: &mut Vec<FitPoint>) {
    let n = arc.len();
    let p0 = arc[0];
    let p2 = arc[n - 1];
    if n <= 2 || max_distance_to_segment(arc, p0, p2) <= FIT_TOLERANCE {
        out.push((p2.0, p2.1, true));
        return;
    }

    // Chord-length parametrization.
    let mut t = vec![0.0f64; n];
    for i in 1..n {
        let d = ((arc[i].0 - arc[i - 1].0).powi(2) + (arc[i].1 - arc[i - 1].1).powi(2)).sqrt();
        t[i] = t[i - 1] + d;
    }
    let total = t[n - 1];
    if total > 0.0 {
        for v in t.iter_mut() {
            *v /= total;
        }
    }

    // Least-squares control point with the ends fixed.
    let (mut sa2, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64);
    for i in 1..n - 1 {
        let ti = t[i];
        let a = 2.0 * ti * (1.0 - ti);
        let rx = arc[i].0 - (1.0 - ti).powi(2) * p0.0 - ti * ti * p2.0;
        let ry = arc[i].1 - (1.0 - ti).powi(2) * p0.1 - ti * ti * p2.1;
        sa2 += a * a;
        sx += a * rx;
        sy += a * ry;
    }
    let p1 = if sa2 > 0.0 {
        (sx / sa2, sy / sa2)
    } else {
        ((p0.0 + p2.0) / 2.0, (p0.1 + p2.1) / 2.0)
    };

    let (mut worst, mut worst_i) = (0.0f64, 1usize);
    for i in 1..n - 1 {
        let b = bezier(p0, p1, p2, t[i]);
        let d = ((b.0 - arc[i].0).powi(2) + (b.1 - arc[i].1).powi(2)).sqrt();
        if d > worst {
            worst = d;
            worst_i = i;
        }
    }
    if worst <= FIT_TOLERANCE {
        // A control point this close to the chord bends the curve by at
        // most half that: call it a line and save the point.
        if max_distance_to_segment(&[p1], p0, p2) > FIT_TOLERANCE / 2.0 {
            out.push((p1.0, p1.1, false));
        }
        out.push((p2.0, p2.1, true));
        return;
    }
    let split = worst_i.clamp(1, n - 2);
    fit_arc(&arc[..=split], out);
    fit_arc(&arc[split..], out);
}

fn bezier(p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), t: f64) -> (f64, f64) {
    let u = 1.0 - t;
    (
        u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0,
        u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1,
    )
}

fn max_distance_to_segment(pts: &[(f64, f64)], a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    pts.iter()
        .map(|&p| {
            if len2 == 0.0 {
                return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
            }
            let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0);
            let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
            ((p.0 - qx).powi(2) + (p.1 - qy).powi(2)).sqrt()
        })
        .fold(0.0, f64::max)
}

/// Flatten contours into closed polylines.
fn flatten(contours: &[Vec<FitPoint>]) -> Vec<Vec<(f64, f64)>> {
    contours
        .iter()
        .map(|c| {
            let n = c.len();
            // Rotate so the polyline starts on-curve (an implied midpoint if
            // no point is on-curve).
            let start = c.iter().position(|p| p.2);
            let first: (f64, f64) = match start {
                Some(i) => (c[i].0, c[i].1),
                None => ((c[0].0 + c[n - 1].0) / 2.0, (c[0].1 + c[n - 1].1) / 2.0),
            };
            let s = start.unwrap_or(0);
            let mut poly = vec![first];
            let mut cur = first;
            let mut pending_off: Option<(f64, f64)> = None;
            for k in 1..=n {
                let p = c[(s + k) % n];
                let pt = (p.0, p.1);
                let on = p.2 || (k == n && start.is_none());
                if on {
                    match pending_off.take() {
                        Some(ctrl) => {
                            for step in 1..=FLATTEN_STEPS {
                                let t = step as f64 / FLATTEN_STEPS as f64;
                                poly.push(bezier(cur, ctrl, pt, t));
                            }
                        }
                        None => poly.push(pt),
                    }
                    cur = pt;
                } else if let Some(prev) = pending_off {
                    // Two off-curve points imply an on-curve midpoint.
                    let mid = ((prev.0 + pt.0) / 2.0, (prev.1 + pt.1) / 2.0);
                    for step in 1..=FLATTEN_STEPS {
                        let t = step as f64 / FLATTEN_STEPS as f64;
                        poly.push(bezier(cur, prev, mid, t));
                    }
                    cur = mid;
                    pending_off = Some(pt);
                } else {
                    pending_off = Some(pt);
                }
            }
            poly
        })
        .collect()
}

/// Nonzero-winding fill of the contours sampled at pixel centres.
pub(crate) fn rasterize(contours: &[Vec<FitPoint>], width: usize, height: usize) -> Vec<bool> {
    let polys = flatten(contours);
    let mut out = vec![false; width * height];
    let mut crossings: Vec<(f64, i32)> = Vec::new();
    for row in 0..height {
        let py = row as f64 + 0.5;
        crossings.clear();
        for poly in &polys {
            let n = poly.len();
            for i in 0..n {
                let (x1, y1) = poly[i];
                let (x2, y2) = poly[(i + 1) % n];
                if (y1 <= py) != (y2 <= py) {
                    let x = x1 + (py - y1) / (y2 - y1) * (x2 - x1);
                    crossings.push((x, if y2 > y1 { 1 } else { -1 }));
                }
            }
        }
        crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        // Winding at a pixel centre counts crossings to its right.
        let mut idx = 0;
        let mut winding: i32 = crossings.iter().map(|c| c.1).sum();
        for col in 0..width {
            let px = col as f64 + 0.5;
            while idx < crossings.len() && crossings[idx].0 <= px {
                winding -= crossings[idx].1;
                idx += 1;
            }
            out[row * width + col] = winding != 0;
        }
    }
    out
}

/// Does the outline reproduce the bitmap? Edge pixels may flip; anything
/// two pixels thick, or more than a third of the ink, means the fit went
/// wrong.
fn faithful(bitmap: &BitImage, contours: &[Vec<FitPoint>]) -> bool {
    let (w, h) = (bitmap.width, bitmap.height);
    let filled = rasterize(contours, w, h);
    let mut xor = vec![false; w * h];
    let mut total = 0u32;
    let mut ink = 0u32;
    for y in 0..h {
        for x in 0..w {
            let b = bitmap.get_usize(x, y);
            ink += b as u32;
            if b != filled[y * w + x] {
                xor[y * w + x] = true;
                total += 1;
            }
        }
    }
    let mut thick = 0u32;
    for y in 0..h.saturating_sub(1) {
        for x in 0..w.saturating_sub(1) {
            if xor[y * w + x]
                && xor[y * w + x + 1]
                && xor[(y + 1) * w + x]
                && xor[(y + 1) * w + x + 1]
            {
                thick += 1;
            }
        }
    }
    thick <= (ink / 32).max(2) && total * 3 <= ink.max(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap_from<F: Fn(f64, f64) -> bool>(w: usize, h: usize, ink: F) -> BitImage {
        let mut img = BitImage::new(w as u32, h as u32).unwrap();
        for y in 0..h {
            for x in 0..w {
                if ink(x as f64 + 0.5, y as f64 + 0.5) {
                    img.set_usize(x, y, true);
                }
            }
        }
        img
    }

    fn point_count(c: &[Vec<FitPoint>]) -> usize {
        c.iter().map(Vec::len).sum()
    }

    fn off_curve_count(c: &[Vec<FitPoint>]) -> usize {
        c.iter().flatten().filter(|p| !p.2).count()
    }

    #[test]
    fn a_rectangle_is_four_corners() {
        let img = bitmap_from(14, 24, |x, y| {
            (2.0..12.0).contains(&x) && (2.0..22.0).contains(&y)
        });
        let c = fit_bitmap(&img).expect("faithful");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].len(), 4, "{:?}", c[0]);
        assert!(c[0].iter().all(|p| p.2));
    }

    #[test]
    fn a_ring_becomes_a_few_curves_that_fill_back_to_the_ring() {
        let img = bitmap_from(30, 30, |x, y| {
            let r = ((x - 15.0).powi(2) + (y - 15.0).powi(2)).sqrt();
            (8.0..12.0).contains(&r)
        });
        let loops = trace_pixel_loops(&img);
        let stair_points: usize = loops
            .iter()
            .map(|l| {
                staircase(
                    &l.iter()
                        .map(|&(x, y)| (x as f64, y as f64))
                        .collect::<Vec<_>>(),
                )
                .len()
            })
            .sum();
        let c = fit_bitmap(&img).expect("faithful");
        assert_eq!(c.len(), 2, "outer and hole");
        assert!(off_curve_count(&c) >= 4, "{c:?}");
        assert!(
            point_count(&c) * 3 <= stair_points,
            "fit {} points vs staircase {}",
            point_count(&c),
            stair_points
        );
        // Bit-for-bit away from the edge: only boundary pixels may differ.
        let filled = rasterize(&c, 30, 30);
        for y in 1..29 {
            for x in 1..29 {
                let b = img.get_usize(x, y);
                if b != filled[y * 30 + x] {
                    let interior = [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)]
                        .iter()
                        .all(|&(nx, ny)| img.get_usize(nx, ny) == b);
                    assert!(!interior, "interior pixel ({x},{y}) flipped");
                }
            }
        }
    }

    #[test]
    fn a_diagonal_bar_is_straight_lines() {
        // A parallelogram with 45-degree sides: its staircase has dozens of
        // steps, its fit has four corners.
        let img = bitmap_from(30, 24, |x, y| {
            let left = 2.0 + y;
            (2.0..22.0).contains(&y) && (left..left + 6.0).contains(&x)
        });
        let c = fit_bitmap(&img).expect("faithful");
        assert_eq!(c.len(), 1);
        assert!(c[0].len() <= 6, "{:?}", c[0]);
        assert_eq!(off_curve_count(&c), 0, "{:?}", c[0]);
    }

    #[test]
    fn a_dot_stays_a_staircase() {
        let img = bitmap_from(6, 6, |x, y| {
            (2.0..4.0).contains(&x) && (2.0..4.0).contains(&y)
        });
        let c = fit_bitmap(&img).expect("faithful");
        assert_eq!(c[0].len(), 4);
        assert!(c[0].iter().all(|p| p.2));
    }

    #[test]
    fn font_units_follow_the_origin_and_baseline() {
        let img = bitmap_from(6, 8, |x, y| {
            (1.0..5.0).contains(&x) && (1.0..7.0).contains(&y)
        });
        let out = vectorize(&img, -1, 7).unwrap();
        let xs: Vec<i16> = out[0].iter().map(|p| p.x).collect();
        let ys: Vec<i16> = out[0].iter().map(|p| p.y).collect();
        assert_eq!(*xs.iter().min().unwrap(), 0);
        assert_eq!(*xs.iter().max().unwrap(), 4 * UNITS_PER_PIXEL as i16);
        assert_eq!(*ys.iter().min().unwrap(), 0, "bottom on the baseline");
        assert_eq!(*ys.iter().max().unwrap(), 6 * UNITS_PER_PIXEL as i16);
    }

    #[test]
    fn a_broken_fit_is_rejected() {
        let img = bitmap_from(10, 10, |x, y| {
            (2.0..8.0).contains(&x) && (2.0..8.0).contains(&y)
        });
        // A contour that fills nothing like the bitmap.
        let bogus = vec![vec![
            (0.0, 0.0, true),
            (10.0, 0.0, true),
            (10.0, 2.0, true),
            (0.0, 2.0, true),
        ]];
        assert!(!faithful(&img, &bogus));
        assert!(faithful(&img, &fit_bitmap(&img).unwrap()));
    }
}
