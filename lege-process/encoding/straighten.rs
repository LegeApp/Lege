//! Text orientation and skew for the glyph font, from the page's connected
//! components alone.
//!
//! Printed text lines share a baseline, so the bottoms of a page's
//! components pile up into sharp rows once the page is turned the right way
//! and its skew is rotated out. [`detect_frame`] scores that pile-up (the
//! sum of squared histogram counts, the projection-profile criterion) for
//! every quarter turn over a range of skew angles and keeps the best. Which
//! way is up follows from the same score: upright Latin text has one sharp
//! bottom edge (the baseline) but two top edges (x-height and ascender), so
//! the bottom histogram is decisively sharper than the top one.
//!
//! Nothing is resampled. A quarter turn of a bilevel component is lossless,
//! and the skew is removed from the component *positions* only: each
//! glyph's bottom is put on its straight baseline while its shape stays as
//! scanned, which at the couple of degrees a scan is off is invisible and
//! is all the line grouping needs.

use jbig2enc_rust::jbig2cc::BBox;
use jbig2enc_rust::jbig2sym::BitImage;

/// Fewer text-sized components than this and the page is taken as upright
/// and straight: there is not enough structure to measure.
const MIN_LAYOUT_COMPONENTS: usize = 40;
/// Skew search range and steps, degrees.
const SKEW_RANGE_DEG: f64 = 6.0;
const SKEW_COARSE_STEP_DEG: f64 = 0.25;
const SKEW_FINE_STEP_DEG: f64 = 0.02;
/// A page is only turned upside down when the bottom edges of the flipped
/// page are this much sharper than those of the page as it is; below it
/// (all-capital or CJK pages, whose tops and bottoms are alike) the page is
/// taken as upright.
const FLIP_MARGIN: f64 = 1.3;
/// A page is only taken as sideways when the bottoms of the turned page are
/// this much sharper than those of the page as it is. Upright pages are the
/// common case, and typewritten text piles its glyphs into columns nearly
/// as sharp as its lines.
const SIDEWAYS_MARGIN: f64 = 1.5;
/// A skew is only rotated out when it sharpens the baselines by this factor
/// over no correction, so a straight page is not chased around by noise.
const SKEW_GAIN_MIN: f64 = 1.05;
/// Components whose larger dimension is outside this range around the
/// median are not text (specks, rules, pictures) and do not vote.
const TEXT_SIZE_MIN_RATIO: f64 = 0.4;
const TEXT_SIZE_MAX_RATIO: f64 = 4.0;

/// How a page's text was straightened: the quarter turns applied to the
/// page so its text reads upright, and the skew rotated out of the turned
/// page's component positions. Every glyph placement the dictionary emits
/// is in the *upright* frame this describes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PageFrame {
    /// The page's size in pixels before turning.
    pub width: u32,
    pub height: u32,
    /// Quarter turns clockwise that make the text upright (0–3).
    pub turns: u8,
    /// Skew of the turned page's lines, radians; positive when lines
    /// descend to the right (y down). Rotated out about the page centre.
    pub skew: f64,
}

impl PageFrame {
    /// The page's size after turning.
    pub fn turned_size(&self) -> (u32, u32) {
        if self.turns % 2 == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    /// A page point after the quarter turns (continuous coordinates).
    pub fn turn_point(&self, x: f64, y: f64) -> (f64, f64) {
        let (w, h) = (self.width as f64, self.height as f64);
        match self.turns % 4 {
            0 => (x, y),
            1 => (h - y, x),
            2 => (w - x, h - y),
            _ => (y, w - x),
        }
    }

    /// A turned-page point back on the page as scanned.
    pub fn unturn_point(&self, x: f64, y: f64) -> (f64, f64) {
        let (w, h) = (self.width as f64, self.height as f64);
        match self.turns % 4 {
            0 => (x, y),
            1 => (y, h - x),
            2 => (w - x, h - y),
            _ => (w - y, x),
        }
    }

    /// A direction in the turned page expressed on the page as scanned.
    pub fn unturn_direction(&self, dx: f64, dy: f64) -> (f64, f64) {
        match self.turns % 4 {
            0 => (dx, dy),
            1 => (dy, -dx),
            2 => (-dx, -dy),
            _ => (-dy, dx),
        }
    }

    /// A turned-page point with the skew rotated out.
    pub fn deskew_point(&self, x: f64, y: f64) -> (f64, f64) {
        if self.skew == 0.0 {
            return (x, y);
        }
        let (w, h) = self.turned_size();
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let (s, c) = self.skew.sin_cos();
        let (rx, ry) = (x - cx, y - cy);
        (cx + rx * c + ry * s, cy - rx * s + ry * c)
    }

    /// A page point (as scanned) in the upright, straightened frame.
    pub fn to_upright(&self, x: f64, y: f64) -> (f64, f64) {
        let (tx, ty) = self.turn_point(x, y);
        self.deskew_point(tx, ty)
    }

    /// A component's box after the quarter turns.
    pub fn turn_box(&self, b: &BBox) -> BBox {
        let (w, h) = (self.width as i32, self.height as i32);
        match self.turns % 4 {
            0 => *b,
            1 => BBox {
                xmin: h - b.ymax,
                ymin: b.xmin,
                xmax: h - b.ymin,
                ymax: b.xmax,
            },
            2 => BBox {
                xmin: w - b.xmax,
                ymin: h - b.ymax,
                xmax: w - b.xmin,
                ymax: h - b.ymin,
            },
            _ => BBox {
                xmin: b.ymin,
                ymin: w - b.xmax,
                xmax: b.ymax,
                ymax: w - b.xmin,
            },
        }
    }

    /// A component's bitmap after the quarter turns (lossless).
    pub fn turn_bitmap(&self, bitmap: BitImage) -> BitImage {
        let turns = self.turns % 4;
        if turns == 0 {
            return bitmap;
        }
        let (w, h) = (bitmap.width, bitmap.height);
        let (tw, th) = if turns % 2 == 1 { (h, w) } else { (w, h) };
        let mut out = BitImage::new(tw as u32, th as u32).expect("turned component fits");
        for y in 0..h {
            for x in 0..w {
                if !bitmap.get_usize(x, y) {
                    continue;
                }
                let (tx, ty) = match turns {
                    1 => (h - 1 - y, x),
                    2 => (w - 1 - x, h - 1 - y),
                    _ => (y, w - 1 - x),
                };
                out.set_usize(tx, ty, true);
            }
        }
        out
    }
}

/// Decide a page's orientation and skew from its components.
pub fn detect_frame(shapes: &[(BitImage, BBox)], width: u32, height: u32) -> PageFrame {
    let mut frame = PageFrame {
        width,
        height,
        turns: 0,
        skew: 0.0,
    };
    let size = |b: &BBox| (b.xmax - b.xmin).max(b.ymax - b.ymin);
    let mut sizes: Vec<i32> = shapes.iter().map(|(_, b)| size(b)).collect();
    if sizes.len() < MIN_LAYOUT_COMPONENTS {
        return frame;
    }
    sizes.sort_unstable();
    let median = sizes[sizes.len() / 2] as f64;
    let boxes: Vec<BBox> = shapes
        .iter()
        .map(|(_, b)| *b)
        .filter(|b| {
            let s = size(b) as f64;
            s >= median * TEXT_SIZE_MIN_RATIO && s <= median * TEXT_SIZE_MAX_RATIO
        })
        .collect();
    if boxes.len() < MIN_LAYOUT_COMPONENTS {
        return frame;
    }

    // Best baseline sharpness over the coarse skew grid, per quarter turn.
    let coarse = angle_grid(0.0, SKEW_RANGE_DEG, SKEW_COARSE_STEP_DEG);
    let mut best = [(0.0f64, 0.0f64); 4]; // (score, angle)
    for turns in 0..4u8 {
        let turned = PageFrame { turns, ..frame };
        let bottoms = bottom_points(&turned, &boxes);
        for &angle in &coarse {
            let score = baseline_sharpness(&bottoms, angle);
            if score > best[turns as usize].0 {
                best[turns as usize] = (score, angle);
            }
        }
    }

    let horizontal = best[0].0.max(best[2].0) * SIDEWAYS_MARGIN >= best[1].0.max(best[3].0);
    frame.turns = if horizontal {
        if best[2].0 > best[0].0 * FLIP_MARGIN {
            2
        } else {
            0
        }
    } else if best[3].0 > best[1].0 {
        3
    } else {
        1
    };

    // Refine the skew for the chosen turn.
    let bottoms = bottom_points(&frame, &boxes);
    let (_, coarse_angle) = best[frame.turns as usize];
    let mut fine_best = (baseline_sharpness(&bottoms, coarse_angle), coarse_angle);
    for angle in angle_grid(coarse_angle, SKEW_COARSE_STEP_DEG, SKEW_FINE_STEP_DEG) {
        let score = baseline_sharpness(&bottoms, angle);
        if score > fine_best.0 {
            fine_best = (score, angle);
        }
    }
    let straight = baseline_sharpness(&bottoms, 0.0);
    if fine_best.0 >= straight * SKEW_GAIN_MIN {
        frame.skew = fine_best.1;
    }
    frame
}

/// Angles (radians) from `centre − range` to `centre + range` in `step`
/// degrees.
fn angle_grid(centre_rad: f64, range_deg: f64, step_deg: f64) -> Vec<f64> {
    let n = (range_deg / step_deg).round() as i32;
    (-n..=n)
        .map(|i| centre_rad + (i as f64 * step_deg).to_radians())
        .collect()
}

/// `(x centre, bottom)` of every box after the frame's quarter turns.
fn bottom_points(frame: &PageFrame, boxes: &[BBox]) -> Vec<(f64, f64)> {
    boxes
        .iter()
        .map(|b| {
            let t = frame.turn_box(b);
            ((t.xmin + t.xmax) as f64 / 2.0, t.ymax as f64)
        })
        .collect()
}

/// How sharply the points' bottoms pile into rows once lines at `skew`
/// are made level: the sum of squared counts over two-pixel windows (each
/// pair of adjacent one-pixel bins), so a pile that straddles a bin edge
/// scores the same as one that does not.
fn baseline_sharpness(points: &[(f64, f64)], skew: f64) -> f64 {
    let tan = skew.tan();
    let projected: Vec<i64> = points
        .iter()
        .map(|&(x, bottom)| (bottom - x * tan).round() as i64)
        .collect();
    let Some(&min) = projected.iter().min() else {
        return 0.0;
    };
    let max = *projected.iter().max().unwrap();
    let mut bins = vec![0u32; (max - min + 2) as usize];
    for p in projected {
        bins[(p - min) as usize] += 1;
    }
    bins.windows(2)
        .map(|w| {
            let pair = (w[0] + w[1]) as f64;
            pair * pair
        })
        .sum()
}

/// Lines with at least this many glyphs get their own baseline fit; shorter
/// ones use the median bottom.
const LINE_FIT_MIN_GLYPHS: usize = 12;
/// A fitted line slope beyond this is not a baseline (about 1.7 degrees);
/// the fit is discarded.
const LINE_FIT_MAX_SLOPE: f64 = 0.03;

/// The baseline through a line's glyph bottoms as `(slope, intercept)`, so
/// that the baseline at `x` is `intercept + slope·x`: a Theil–Sen fit
/// (median of pairwise slopes, median of intercepts), which descenders and
/// punctuation cannot drag off. `points` are `(x centre, bottom)`.
/// Returns `None` when the line is too short to fit or the fit is not a
/// baseline.
pub fn fit_baseline(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = points.len();
    if n < LINE_FIT_MIN_GLYPHS {
        return None;
    }
    // All pairs on short lines; a strided subset on long ones keeps it
    // linear-ish.
    let stride = (n * (n - 1) / 2).div_ceil(400).max(1);
    let mut slopes = Vec::new();
    let mut k = 0usize;
    for i in 0..n {
        for j in i + 1..n {
            if k % stride == 0 {
                let dx = points[j].0 - points[i].0;
                if dx.abs() >= 1.0 {
                    slopes.push((points[j].1 - points[i].1) / dx);
                }
            }
            k += 1;
        }
    }
    if slopes.is_empty() {
        return None;
    }
    let slope = median(&mut slopes);
    if slope.abs() > LINE_FIT_MAX_SLOPE {
        return None;
    }
    let mut intercepts: Vec<f64> = points.iter().map(|&(x, y)| y - slope * x).collect();
    Some((slope, median(&mut intercepts)))
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(w: usize, h: usize, on: &[(usize, usize)]) -> BitImage {
        let mut b = BitImage::new(w as u32, h as u32).unwrap();
        for &(x, y) in on {
            b.set_usize(x, y, true);
        }
        b
    }

    /// Boxes of a page of text lines with `skew` (radians), optionally turned.
    fn text_page(turns: u8, skew: f64) -> (Vec<(BitImage, BBox)>, u32, u32) {
        let (w, h) = (1600u32, 2200u32);
        let mut shapes = Vec::new();
        let upright = PageFrame {
            width: w,
            height: h,
            turns: 0,
            skew: 0.0,
        };
        // Lines every 40 px of proportional type (widths cycling, each
        // line starting at its own offset so glyphs do not line up in
        // columns), x-height 18, ascenders on every third glyph, descenders
        // on every ninth.
        let widths = [8, 12, 14, 16, 20, 10, 15];
        for line in 0..40 {
            let mut x = 100 + (line * 7) % 13;
            for col in 0..60 {
                let width = widths[(col + line) % widths.len()];
                let baseline = 200.0 + line as f64 * 40.0 + (x as f64 - 800.0) * skew.tan();
                let bottom = baseline.round() as i32 + if col % 9 == 3 { 6 } else { 0 };
                let top =
                    bottom - if col % 3 == 0 { 26 } else { 18 } - if col % 9 == 3 { 6 } else { 0 };
                let b = BBox {
                    xmin: x as i32,
                    ymin: top,
                    xmax: (x + width) as i32,
                    ymax: bottom,
                };
                shapes.push((BitImage::new(width as u32, 18).unwrap(), b));
                x += width + 3;
            }
        }
        // Express the page as scanned: turn the upright page by `turns`
        // clockwise, i.e. the scanned page turned by `turns` is upright.
        // Turning the scanned page by `turns` must give the upright box, so
        // the scanned box is the upright box turned by `4 − turns`.
        let inverse = PageFrame {
            turns: (4 - turns) % 4,
            ..upright
        };
        let shapes = shapes
            .into_iter()
            .map(|(bm, b)| (bm, inverse.turn_box(&b)))
            .collect();
        let (sw, sh) = inverse.turned_size();
        (shapes, sw, sh)
    }

    #[test]
    fn quarter_turns_round_trip_points_boxes_and_bitmaps() {
        for turns in 0..4u8 {
            let f = PageFrame {
                width: 30,
                height: 20,
                turns,
                skew: 0.0,
            };
            let (x, y) = f.turn_point(3.0, 7.0);
            assert_eq!(f.unturn_point(x, y), (3.0, 7.0), "turns {turns}");
            let b = BBox {
                xmin: 2,
                ymin: 5,
                xmax: 6,
                ymax: 9,
            };
            let t = f.turn_box(&b);
            let (tw, th) = f.turned_size();
            assert!(t.xmin >= 0 && t.ymin >= 0 && t.xmax <= tw as i32 && t.ymax <= th as i32);
            assert_eq!(t.xmax - t.xmin + t.ymax - t.ymin, 8);
            // The box's corners map onto the turned box.
            let (cx, cy) = f.turn_point(b.xmin as f64, b.ymin as f64);
            assert!(cx >= t.xmin as f64 && cx <= t.xmax as f64);
            assert!(cy >= t.ymin as f64 && cy <= t.ymax as f64);
        }
        // An L shape turned clockwise: the foot goes to the left.
        let f = PageFrame {
            width: 10,
            height: 10,
            turns: 1,
            skew: 0.0,
        };
        let l = bitmap(3, 4, &[(0, 0), (0, 1), (0, 2), (0, 3), (1, 3), (2, 3)]);
        let t = f.turn_bitmap(l);
        assert_eq!((t.width, t.height), (4, 3));
        // Original top-left (0,0) → (h−1−0, 0) = (3, 0); foot (2,3) → (0, 2).
        assert!(t.get_usize(3, 0) && t.get_usize(0, 2) && t.get_usize(0, 0));
        assert!(!t.get_usize(3, 2));
    }

    #[test]
    fn upright_straight_text_is_left_alone() {
        let (shapes, w, h) = text_page(0, 0.0);
        let f = detect_frame(&shapes, w, h);
        assert_eq!(f.turns, 0);
        assert_eq!(f.skew, 0.0);
    }

    #[test]
    fn skew_is_measured_to_a_few_hundredths_of_a_degree() {
        for deg in [-2.5, 0.7, 1.3] {
            let (shapes, w, h) = text_page(0, (deg as f64).to_radians());
            let f = detect_frame(&shapes, w, h);
            assert_eq!(f.turns, 0, "{deg}° page turned");
            assert!(
                (f.skew.to_degrees() - deg).abs() <= 0.05,
                "{deg}° measured as {}°",
                f.skew.to_degrees()
            );
            // Rotating the skew out levels the first line's bottoms.
            let mut bottoms: Vec<f64> = shapes[..60]
                .iter()
                .enumerate()
                .filter(|(col, _)| col % 9 != 3)
                .map(|(_, (_, b))| {
                    f.deskew_point((b.xmin + b.xmax) as f64 / 2.0, b.ymax as f64)
                        .1
                })
                .collect();
            bottoms.sort_by(|a, b| a.total_cmp(b));
            let spread = bottoms[bottoms.len() * 9 / 10] - bottoms[bottoms.len() / 10];
            assert!(spread <= 2.0, "levelled bottoms spread {spread}");
        }
    }

    #[test]
    fn turned_pages_are_recognized() {
        for turns in 1..4u8 {
            let (shapes, w, h) = text_page(turns, 0.0);
            let f = detect_frame(&shapes, w, h);
            assert_eq!(f.turns, turns, "page needing {turns} turns");
        }
    }

    #[test]
    fn an_all_capitals_page_is_not_flipped() {
        // Tops and bottoms equally sharp: no ascender/descender asymmetry.
        let (w, h) = (1600u32, 2200u32);
        let mut shapes = Vec::new();
        for line in 0..40 {
            for col in 0..60 {
                let x = 100 + col * 20;
                let bottom = 200 + line * 40;
                shapes.push((
                    BitImage::new(14, 24).unwrap(),
                    BBox {
                        xmin: x,
                        ymin: bottom - 24,
                        xmax: x + 14,
                        ymax: bottom,
                    },
                ));
            }
        }
        let f = detect_frame(&shapes, w, h);
        assert_eq!(f.turns, 0);
    }

    #[test]
    fn baseline_fit_ignores_descenders() {
        let points: Vec<(f64, f64)> = (0..30)
            .map(|i| {
                let x = i as f64 * 20.0;
                let y = 100.0 + x * 0.01 + if i % 5 == 2 { 7.0 } else { 0.0 };
                (x, y)
            })
            .collect();
        let (slope, intercept) = fit_baseline(&points).unwrap();
        assert!((slope - 0.01).abs() < 1e-9);
        assert!((intercept - 100.0).abs() < 1e-9);
        assert!(fit_baseline(&points[..5]).is_none());
    }
}
