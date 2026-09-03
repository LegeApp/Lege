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
//! The text may sit at any angle. Neighbouring glyphs lie along their line,
//! so the directions from each component to its nearest neighbours pile up
//! at the angle of the text axis; folded into a quarter circle that angle
//! is the skew of every quarter-turn candidate at once, and the bottom
//! histogram only has to decide among the four and refine the angle. At a
//! large angle a component's lowest point is no longer its box's bottom
//! edge, so bottoms are taken from the ink itself, rotated.
//!
//! Nothing is resampled. A quarter turn of a bilevel component is lossless,
//! and the skew is removed from the component *positions* only: each
//! glyph's bottom is put on its straight baseline while its shape stays as
//! scanned. Output puts every glyph back where it was scanned (see
//! [`PageFrame::to_scanned`]), so the page is not straightened; the
//! straight frame is where lines are found and shapes compared.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use jbig2enc_rust::jbig2cc::{BBox, analyze_page};
use jbig2enc_rust::jbig2sym::{BitImage, binary_pixels_to_bitimage};

/// Fewer text-sized components than this and the page is taken as upright
/// and straight: there is not enough structure to measure.
const MIN_LAYOUT_COMPONENTS: usize = 40;
/// Skew search range around the text axis when the axis is known, and
/// around level when it is not, degrees; and the search steps.
const AXIS_SEARCH_DEG: f64 = 1.5;
const SKEW_RANGE_DEG: f64 = 6.0;
const SKEW_COARSE_STEP_DEG: f64 = 0.25;
const SKEW_FINE_STEP_DEG: f64 = 0.02;
/// A page is only turned upside down when the bottom edges of the flipped
/// page are this much sharper than those of the page as it is; below it
/// (all-capital or CJK pages, whose tops and bottoms are alike, and score
/// within a few percent of each other) the page is taken as upright.
/// Latin text scores its bottoms a third sharper than its tops, less once
/// rasterization at an angle blurs the piles, so the margin is kept small.
const FLIP_MARGIN: f64 = 1.15;
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
/// Bottoms pile into rows this many pixels wide: two for the sampling
/// wobble of level text, one more once the text is at an angle and the
/// lowest point of each shape is a rasterized corner. Orientation is
/// judged with the wider window; the skew is refined with the narrower,
/// which resolves a few hundredths of a degree.
const ORIENTATION_WINDOW_PX: usize = 3;
const SKEW_WINDOW_PX: usize = 2;
/// The text axis histogram: bins of this many degrees over a quarter
/// circle.
const AXIS_BIN_DEG: f64 = 0.5;
/// Neighbours farther than this many median glyph sizes away are on some
/// other line or in the margin and do not vote for the axis.
const AXIS_NEIGHBOUR_REACH: f64 = 3.0;
/// Nearest neighbours per component that vote.
const AXIS_NEIGHBOURS: usize = 2;
/// The axis histogram's peak must hold this many times the mean bin count
/// to be believed; a flat histogram (a picture, a table of rules) is not
/// text along an axis.
const AXIS_PEAK_MIN_RATIO: f64 = 4.0;

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
    /// The frame that leaves a page as it is.
    pub fn identity(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            turns: 0,
            skew: 0.0,
        }
    }

    /// Whether the frame changes nothing.
    pub fn is_identity(&self) -> bool {
        self.turns % 4 == 0 && self.skew == 0.0
    }

    /// The same orientation for a raster of another size (the OCR raster,
    /// rendered at a different resolution).
    pub fn resized(&self, width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            ..*self
        }
    }

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

    /// An upright-frame point with the skew put back: the turned-page
    /// point it came from.
    pub fn reskew_point(&self, x: f64, y: f64) -> (f64, f64) {
        if self.skew == 0.0 {
            return (x, y);
        }
        let (w, h) = self.turned_size();
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let (s, c) = self.skew.sin_cos();
        let (rx, ry) = (x - cx, y - cy);
        (cx + rx * c - ry * s, cy + rx * s + ry * c)
    }

    /// A page point (as scanned) in the upright, straightened frame.
    pub fn to_upright(&self, x: f64, y: f64) -> (f64, f64) {
        let (tx, ty) = self.turn_point(x, y);
        self.deskew_point(tx, ty)
    }

    /// An upright-frame point back on the page as scanned.
    pub fn to_scanned(&self, x: f64, y: f64) -> (f64, f64) {
        let (tx, ty) = self.reskew_point(x, y);
        self.unturn_point(tx, ty)
    }

    /// A direction in the upright frame expressed on the page as scanned.
    pub fn scanned_direction(&self, dx: f64, dy: f64) -> (f64, f64) {
        let (s, c) = self.skew.sin_cos();
        self.unturn_direction(dx * c - dy * s, dx * s + dy * c)
    }

    /// The box `(x0, y0, x1, y1)` the turned page covers in the upright
    /// frame: the page itself when there is no skew, and its rotated
    /// corners' bounds otherwise.
    pub fn upright_bounds(&self) -> (f64, f64, f64, f64) {
        let (w, h) = self.turned_size();
        let (w, h) = (w as f64, h as f64);
        let corners =
            [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)].map(|(x, y)| self.deskew_point(x, y));
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (x, y) in corners {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        (x0, y0, x1, y1)
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
        for (y, x0, x1) in row_spans(&bitmap) {
            for x in x0..x1 {
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

/// Resolution hint for component cleanup. The output raster carries no
/// physical size, so assume a roughly ten-inch page; this only tunes the
/// speck-removal threshold, never geometry.
pub fn analysis_dpi(height_px: usize) -> i32 {
    ((height_px as f32 / 10.0).round() as i32).clamp(72, 1200)
}

/// A binarized page in the pipeline's image convention (one byte per
/// pixel, `<= 128` is ink) as a bitmap.
pub fn page_bitmap(pixels: &[u8], width: usize, height: usize) -> Result<BitImage> {
    let expected = width
        .checked_mul(height)
        .ok_or_else(|| anyhow!("page dimensions overflow"))?;
    if pixels.len() < expected {
        return Err(anyhow!(
            "page buffer holds {} bytes, {}x{} needs {}",
            pixels.len(),
            width,
            height,
            expected
        ));
    }
    let logical: Vec<u8> = pixels[..expected]
        .iter()
        .map(|&v| u8::from(v <= 128))
        .collect();
    binary_pixels_to_bitimage(&logical, width, height).map_err(|e| anyhow!("{e}"))
}

/// A binarized page's orientation and skew, from its components (the
/// same analysis the glyph dictionary runs; `dpi` only tunes speck
/// removal). A page that cannot be read is taken as it is.
pub fn detect_frame_of_pixels(pixels: &[u8], width: usize, height: usize, dpi: i32) -> PageFrame {
    let Ok(page) = page_bitmap(pixels, width, height) else {
        return PageFrame::identity(width as u32, height as u32);
    };
    let shapes = analyze_page(&page, dpi.max(72), 1).extract_shapes();
    detect_frame(&shapes, width as u32, height as u32)
}

/// Decide a page's orientation and skew from its components.
pub fn detect_frame(shapes: &[(BitImage, BBox)], width: u32, height: u32) -> PageFrame {
    let mut frame = PageFrame::identity(width, height);
    let size = |b: &BBox| (b.xmax - b.xmin).max(b.ymax - b.ymin);
    let mut sizes: Vec<i32> = shapes.iter().map(|(_, b)| size(b)).collect();
    if sizes.len() < MIN_LAYOUT_COMPONENTS {
        return frame;
    }
    sizes.sort_unstable();
    let median = sizes[sizes.len() / 2] as f64;
    let text: Vec<&(BitImage, BBox)> = shapes
        .iter()
        .filter(|(_, b)| {
            let s = size(b) as f64;
            s >= median * TEXT_SIZE_MIN_RATIO && s <= median * TEXT_SIZE_MAX_RATIO
        })
        .collect();
    if text.len() < MIN_LAYOUT_COMPONENTS {
        return frame;
    }

    // The angle of the text axis, whichever way the text runs, and how far
    // around it to look; without one, look around level.
    let (axis, range) = match text_axis(&text, median) {
        Some(axis) => (axis, AXIS_SEARCH_DEG),
        None => (0.0, SKEW_RANGE_DEG),
    };

    // Best baseline sharpness over the coarse grid, per quarter turn.
    let coarse = angle_grid(0.0, range, SKEW_COARSE_STEP_DEG);
    let mut best = [(0.0f64, 0.0f64); 4]; // (score, angle off the axis)
    for turns in 0..4u8 {
        let candidate = PageFrame {
            turns,
            skew: axis,
            ..frame
        };
        let bottoms = glyph_bottoms(&candidate, &text);
        for &angle in &coarse {
            let score = baseline_sharpness(&bottoms, angle, ORIENTATION_WINDOW_PX);
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
    let candidate = PageFrame {
        skew: axis,
        ..frame
    };
    let bottoms = glyph_bottoms(&candidate, &text);
    let (_, coarse_angle) = best[frame.turns as usize];
    let mut fine_best = (
        baseline_sharpness(&bottoms, coarse_angle, SKEW_WINDOW_PX),
        coarse_angle,
    );
    for angle in angle_grid(coarse_angle, SKEW_COARSE_STEP_DEG, SKEW_FINE_STEP_DEG) {
        let score = baseline_sharpness(&bottoms, angle, SKEW_WINDOW_PX);
        if score > fine_best.0 {
            fine_best = (score, angle);
        }
    }
    // Only when it beats no correction at all.
    let straight = baseline_sharpness(&glyph_bottoms(&frame, &text), 0.0, SKEW_WINDOW_PX);
    if fine_best.0 >= straight * SKEW_GAIN_MIN {
        frame.skew = axis + fine_best.1;
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

/// The angle of the text axis, radians in `(−45°, 45°]`, from the
/// directions between each component and its nearest neighbours, or `None`
/// when they do not agree on one.
fn text_axis(shapes: &[&(BitImage, BBox)], median_size: f64) -> Option<f64> {
    let centres: Vec<(f64, f64)> = shapes
        .iter()
        .map(|(_, b)| {
            (
                (b.xmin + b.xmax) as f64 / 2.0,
                (b.ymin + b.ymax) as f64 / 2.0,
            )
        })
        .collect();
    let reach = (median_size * AXIS_NEIGHBOUR_REACH).max(1.0);
    let cell_of = |x: f64, y: f64| ((x / reach).floor() as i32, (y / reach).floor() as i32);
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, &(x, y)) in centres.iter().enumerate() {
        grid.entry(cell_of(x, y)).or_default().push(i);
    }

    let bins = (90.0 / AXIS_BIN_DEG).round() as usize;
    let mut hist = vec![0u32; bins];
    let mut votes = 0u32;
    let reach2 = reach * reach;
    for (i, &(x, y)) in centres.iter().enumerate() {
        let (gx, gy) = cell_of(x, y);
        let mut nearest = [(f64::MAX, usize::MAX); AXIS_NEIGHBOURS];
        for ny in gy - 1..=gy + 1 {
            for nx in gx - 1..=gx + 1 {
                let Some(list) = grid.get(&(nx, ny)) else {
                    continue;
                };
                for &j in list {
                    if j == i {
                        continue;
                    }
                    let (dx, dy) = (centres[j].0 - x, centres[j].1 - y);
                    let d2 = dx * dx + dy * dy;
                    if d2 > reach2 || d2 >= nearest[AXIS_NEIGHBOURS - 1].0 {
                        continue;
                    }
                    let mut k = AXIS_NEIGHBOURS - 1;
                    while k > 0 && nearest[k - 1].0 > d2 {
                        nearest[k] = nearest[k - 1];
                        k -= 1;
                    }
                    nearest[k] = (d2, j);
                }
            }
        }
        for (_, j) in nearest {
            if j == usize::MAX {
                continue;
            }
            let (dx, dy) = (centres[j].0 - x, centres[j].1 - y);
            // Folded into a quarter circle: lines and columns, either way
            // along, all vote for the same residual angle.
            let deg = (dy.atan2(dx).to_degrees() + 45.0).rem_euclid(90.0) - 45.0;
            let bin = (((deg + 45.0) / AXIS_BIN_DEG).floor() as usize).min(bins - 1);
            hist[bin] += 1;
            votes += 1;
        }
    }
    if (votes as usize) < MIN_LAYOUT_COMPONENTS {
        return None;
    }
    // Circular three-bin smoothing, the peak, and its parabolic refinement.
    let smooth: Vec<f64> = (0..bins)
        .map(|b| (hist[(b + bins - 1) % bins] + hist[b] + hist[(b + 1) % bins]) as f64)
        .collect();
    let (peak, &peak_value) = smooth
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))?;
    let mean = smooth.iter().sum::<f64>() / bins as f64;
    if peak_value < mean * AXIS_PEAK_MIN_RATIO {
        return None;
    }
    let (left, right) = (smooth[(peak + bins - 1) % bins], smooth[(peak + 1) % bins]);
    let curvature = left - 2.0 * peak_value + right;
    let offset = if curvature.abs() > f64::EPSILON {
        (0.5 * (left - right) / curvature).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    let deg = -45.0 + (peak as f64 + 0.5 + offset) * AXIS_BIN_DEG;
    Some(deg.to_radians())
}

/// `(x centre, bottom)` of every component in the frame's upright
/// coordinates. Without skew the bottom is the turned box's bottom edge;
/// with it, the lowest corner of the component's ink once rotated (a box
/// edge is no longer the lowest point of a rotated shape). A component
/// without ink uses its box.
fn glyph_bottoms(frame: &PageFrame, shapes: &[&(BitImage, BBox)]) -> Vec<(f64, f64)> {
    shapes
        .iter()
        .map(|(bitmap, b)| {
            if frame.skew == 0.0 {
                let t = frame.turn_box(b);
                return ((t.xmin + t.xmax) as f64 / 2.0, t.ymax as f64);
            }
            let (cx, _) = frame.to_upright(
                (b.xmin + b.xmax) as f64 / 2.0,
                (b.ymin + b.ymax) as f64 / 2.0,
            );
            let mut bottom = f64::MIN;
            let mut lowest = |x: f64, y: f64| bottom = bottom.max(frame.to_upright(x, y).1);
            let mut inked = false;
            for (y, x0, x1) in row_spans(bitmap) {
                inked = true;
                let y0 = (b.ymin + y as i32) as f64;
                let x0 = (b.xmin + x0 as i32) as f64;
                let x1 = (b.xmin + x1 as i32) as f64;
                lowest(x0, y0);
                lowest(x1, y0);
                lowest(x0, y0 + 1.0);
                lowest(x1, y0 + 1.0);
            }
            if !inked {
                for (x, y) in [
                    (b.xmin, b.ymin),
                    (b.xmax, b.ymin),
                    (b.xmin, b.ymax),
                    (b.xmax, b.ymax),
                ] {
                    lowest(x as f64, y as f64);
                }
            }
            (cx, bottom)
        })
        .collect()
}

/// Each inked row of a bitmap as `(y, first x, one past the last x)`.
pub fn row_spans(bitmap: &BitImage) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
    let stride = bitmap.width.div_ceil(32);
    let words = bitmap.packed_words();
    (0..bitmap.height).filter_map(move |y| {
        let row = &words[y * stride..(y + 1) * stride];
        let first = row.iter().position(|&w| w != 0)?;
        let last = row.iter().rposition(|&w| w != 0)?;
        let x0 = first * 32 + row[first].leading_zeros() as usize;
        let x1 = last * 32 + 32 - row[last].trailing_zeros() as usize;
        Some((y, x0, x1))
    })
}

/// How sharply the points' bottoms pile into rows once lines at `skew`
/// are made level: the sum of squared counts over `window`-pixel windows
/// (runs of adjacent one-pixel bins), so a pile that straddles a bin edge
/// scores the same as one that does not.
fn baseline_sharpness(points: &[(f64, f64)], skew: f64, window: usize) -> f64 {
    let tan = skew.tan();
    let projected: Vec<i64> = points
        .iter()
        .map(|&(x, bottom)| (bottom - x * tan).round() as i64)
        .collect();
    let Some(&min) = projected.iter().min() else {
        return 0.0;
    };
    let max = *projected.iter().max().unwrap();
    let mut bins = vec![0u32; (max - min) as usize + window];
    for p in projected {
        bins[(p - min) as usize] += 1;
    }
    bins.windows(window)
        .map(|w| {
            let run = w.iter().sum::<u32>() as f64;
            run * run
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

    /// The upright page of `text_page`, filled rectangles rotated `deg`
    /// clockwise about the page centre and rasterized, as a scan of it.
    fn rotated_page(deg: f64) -> (Vec<(BitImage, BBox)>, u32, u32) {
        let (upright, w, h) = text_page(0, 0.0);
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let (s, c) = deg.to_radians().sin_cos();
        let rot = |x: f64, y: f64| {
            let (rx, ry) = (x - cx, y - cy);
            (cx + rx * c - ry * s, cy + rx * s + ry * c)
        };
        let unrot = |x: f64, y: f64| {
            let (rx, ry) = (x - cx, y - cy);
            (cx + rx * c + ry * s, cy - rx * s + ry * c)
        };
        let shapes = upright
            .iter()
            .map(|(_, b)| {
                let corners = [
                    (b.xmin, b.ymin),
                    (b.xmax, b.ymin),
                    (b.xmin, b.ymax),
                    (b.xmax, b.ymax),
                ]
                .map(|(x, y)| rot(x as f64, y as f64));
                let xmin = corners.iter().map(|c| c.0).fold(f64::MAX, f64::min).floor() as i32;
                let ymin = corners.iter().map(|c| c.1).fold(f64::MAX, f64::min).floor() as i32;
                let xmax = corners.iter().map(|c| c.0).fold(f64::MIN, f64::max).ceil() as i32;
                let ymax = corners.iter().map(|c| c.1).fold(f64::MIN, f64::max).ceil() as i32;
                let mut bm = BitImage::new((xmax - xmin) as u32, (ymax - ymin) as u32).unwrap();
                for py in ymin..ymax {
                    for px in xmin..xmax {
                        let (ux, uy) = unrot(px as f64 + 0.5, py as f64 + 0.5);
                        if ux >= b.xmin as f64
                            && ux < b.xmax as f64
                            && uy >= b.ymin as f64
                            && uy < b.ymax as f64
                        {
                            bm.set_usize((px - xmin) as usize, (py - ymin) as usize, true);
                        }
                    }
                }
                (
                    bm,
                    BBox {
                        xmin,
                        ymin,
                        xmax,
                        ymax,
                    },
                )
            })
            .collect();
        (shapes, w, h)
    }

    #[test]
    fn text_at_any_angle_is_found() {
        // (page rotation clockwise, expected turns, expected skew)
        for (deg, turns, skew) in [
            (20.0, 0u8, 20.0),
            (-35.0, 0, -35.0),
            (110.0, 3, 20.0),
            (200.0, 2, 20.0),
            (-80.0, 1, 10.0),
            (44.0, 0, 44.0),
        ] {
            let (shapes, w, h) = rotated_page(deg);
            let f = detect_frame(&shapes, w, h);
            assert_eq!(
                f.turns, turns,
                "page rotated {deg}° given {} turns",
                f.turns
            );
            assert!(
                (f.skew.to_degrees() - skew).abs() <= 0.05,
                "page rotated {deg}°: skew measured as {}°",
                f.skew.to_degrees()
            );
            // The first line's bottoms (descenders aside) are level upright.
            let mut bottoms: Vec<f64> = shapes[..60]
                .iter()
                .enumerate()
                .filter(|(col, _)| col % 9 != 3)
                .map(|(_, (bm, b))| {
                    row_spans(bm)
                        .flat_map(|(y, x0, x1)| {
                            let y1 = (b.ymin + y as i32 + 1) as f64;
                            [
                                f.to_upright((b.xmin + x0 as i32) as f64, y1).1,
                                f.to_upright((b.xmin + x1 as i32) as f64, y1).1,
                            ]
                        })
                        .fold(f64::MIN, f64::max)
                })
                .collect();
            bottoms.sort_by(|a, b| a.total_cmp(b));
            let spread = bottoms[bottoms.len() * 9 / 10] - bottoms[bottoms.len() / 10];
            assert!(spread <= 2.0, "{deg}°: levelled bottoms spread {spread}");
        }
    }

    #[test]
    fn scanned_and_upright_are_inverses() {
        for turns in 0..4u8 {
            for skew in [-0.4, 0.0, 0.01, 0.7] {
                let f = PageFrame {
                    width: 300,
                    height: 500,
                    turns,
                    skew,
                };
                for (x, y) in [(0.0, 0.0), (17.5, 400.25), (299.0, 3.0)] {
                    let (ux, uy) = f.to_upright(x, y);
                    let (sx, sy) = f.to_scanned(ux, uy);
                    assert!(
                        (sx - x).abs() < 1e-9 && (sy - y).abs() < 1e-9,
                        "t{turns} s{skew}"
                    );
                    // Directions follow points.
                    let (ux2, uy2) = (ux + 3.0, uy - 2.0);
                    let (sx2, sy2) = f.to_scanned(ux2, uy2);
                    let (dx, dy) = f.scanned_direction(3.0, -2.0);
                    assert!((sx2 - sx - dx).abs() < 1e-9 && (sy2 - sy - dy).abs() < 1e-9);
                }
                let (x0, y0, x1, y1) = f.upright_bounds();
                let (tw, th) = f.turned_size();
                assert!(x1 - x0 >= tw as f64 - 1e-9 && y1 - y0 >= th as f64 - 1e-9);
            }
        }
    }
}
