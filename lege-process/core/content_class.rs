//! Line-art vs continuous-tone classification for layout image boxes.
//!
//! Per-32×32-cell features are ported from `bpg-rs` still265 preanalysis
//! (variance, Sobel, orientation entropy, chroma). Tuned on Structures.pdf
//! (paintings vs line-only maps) and a line-art-only engraving book:
//! colour / filled plates keep their overlay; paper-ground maps and
//! bimodal ink (diagrams, engravings, sketches) are skipped so the region
//! is binarized or carried by the MRC JBIG2 mask.

const CELL: u32 = 32;

const FLAT_VAR: u32 = 12;
const TEXTURE_VAR: u32 = 64;
const FLAT_EDGE_Q8: u16 = 13;
const GRAD_EDGE_Q8: u16 = 26;
const EDGE_DENSE_Q8: u16 = 38;
const TEXT_EDGE_Q8: u16 = 64;
const DIR_DOMINANT_Q8: u16 = 128;
const AXIS_ALIGNED_Q8: u16 = 160;
const LOW_ENTROPY_Q8: u16 = 110;
const HIGH_ENTROPY_Q8: u16 = 150;
const NOISE_HI: u16 = 7;
const CHROMA_HI: u16 = 96;

const EDGE_GRAD: i32 = 48;
const FLAT_GRAD: i32 = 12;
const WEAK_GRAD: i32 = 24;

/// Paper-ground crops (line maps, diagrams) above this flat share are line art.
const PAPER_FLAT_MIN: f32 = 0.38;
/// Need some structured ink, not an empty / smooth wash.
const PAPER_INK_MIN: f32 = 0.06;
/// Filled continuous-tone plates: little paper, lots of texture.
const FILLED_FLAT_MAX: f32 = 0.24;
const FILLED_TEXTURE_MIN: f32 = 0.45;
/// Mean per-cell chroma activity that marks a colour plate.
/// Cream scan paper plus black ink lands around 15–60; paintings/covers
/// are hundreds.
const COLOR_CHROMA_MIN: f32 = 80.0;
/// Mid-band luma share below this is ink+paper (engravings, sketches).
const BIMODAL_MID_MAX: f32 = 0.22;
const BIMODAL_SPREAD_MIN: u32 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegionClass {
    Flat,
    Gradient,
    ChromaCritical,
    Noisy,
    Texture,
    DirectionalEdge,
    TextLike,
}

#[derive(Clone, Copy, Debug)]
struct CellAnalysis {
    class: RegionClass,
    flat_ratio_q8: u16,
    chroma_activity: u16,
}

/// Outcome of the per-crop line-art vs photo test.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineArtVerdict {
    pub is_line_art: bool,
    pub cells: u32,
    pub ink_share: f32,
    pub texture_share: f32,
    pub chroma_share: f32,
    pub structured: f32,
    pub avg_flat: f32,
    pub mean_chroma: f32,
    pub mid_share: f32,
}

impl LineArtVerdict {
    fn empty() -> Self {
        Self {
            is_line_art: true,
            cells: 0,
            ink_share: 0.0,
            texture_share: 0.0,
            chroma_share: 0.0,
            structured: 0.0,
            avg_flat: 0.0,
            mean_chroma: 0.0,
            mid_share: 0.0,
        }
    }
}

/// True when `bbox` is line art (diagrams, line maps, engravings, sketches)
/// rather than a photograph or colour plate.
pub fn region_is_line_art(rgb: &[u8], width: usize, height: usize, bbox: [f32; 4]) -> bool {
    classify_image_region(rgb, width, height, bbox).is_line_art
}

/// Classify a crop and return the shares used by the keep/skip rule.
pub fn classify_image_region(
    rgb: &[u8],
    width: usize,
    height: usize,
    bbox: [f32; 4],
) -> LineArtVerdict {
    classify_region(rgb, width, height, bbox)
}

fn classify_region(rgb: &[u8], width: usize, height: usize, bbox: [f32; 4]) -> LineArtVerdict {
    let x0 = (bbox[0].max(0.0) as u32).min(width as u32);
    let y0 = (bbox[1].max(0.0) as u32).min(height as u32);
    let x1 = (bbox[2].max(0.0) as u32).min(width as u32);
    let y1 = (bbox[3].max(0.0) as u32).min(height as u32);
    if x1 <= x0 || y1 <= y0 {
        return LineArtVerdict::empty();
    }

    let mut textlike = 0u32;
    let mut texture = 0u32;
    let mut flat = 0u32;
    let mut chroma = 0u32;
    let mut directional = 0u32;
    let mut flat_ratio_sum = 0u32;
    let mut chroma_sum = 0u32;
    let mut n = 0u32;

    let mut y = y0;
    while y < y1 {
        let cy1 = (y + CELL).min(y1);
        let mut x = x0;
        while x < x1 {
            let cx1 = (x + CELL).min(x1);
            let cell = analyze_cell(rgb, width as u32, x, y, cx1, cy1, x0, y0, x1, y1);
            match cell.class {
                RegionClass::TextLike => textlike += 1,
                RegionClass::Texture | RegionClass::Noisy => texture += 1,
                RegionClass::Flat => flat += 1,
                RegionClass::ChromaCritical => chroma += 1,
                RegionClass::DirectionalEdge => directional += 1,
                RegionClass::Gradient => {}
            }
            flat_ratio_sum += u32::from(cell.flat_ratio_q8);
            chroma_sum += u32::from(cell.chroma_activity);
            n += 1;
            x = cx1;
        }
        y = cy1;
    }

    if n == 0 {
        return LineArtVerdict::empty();
    }
    let n_f = n as f32;
    let ink_share = (textlike + directional) as f32 / n_f;
    let texture_share = texture as f32 / n_f;
    let chroma_share = chroma as f32 / n_f;
    let structured = (textlike + directional + flat) as f32 / n_f;
    let avg_flat = (flat_ratio_sum as f32 / n_f) / 256.0;
    let mean_chroma = chroma_sum as f32 / n_f;
    let (mid_share, spread) = luma_mid_share(rgb, width as u32, x0, y0, x1, y1);

    // Colour / filled plates keep the overlay. Paper-ground maps and
    // bimodal ink (engravings, sketches, diagrams) do not.
    let color_plate = mean_chroma >= COLOR_CHROMA_MIN;
    let filled_photo = avg_flat < FILLED_FLAT_MAX && texture_share >= FILLED_TEXTURE_MIN;
    let paper_line = avg_flat >= PAPER_FLAT_MIN && ink_share >= PAPER_INK_MIN;
    let bimodal_ink = mid_share < BIMODAL_MID_MAX && spread >= BIMODAL_SPREAD_MIN;
    let is_line_art = !color_plate && !filled_photo && (paper_line || bimodal_ink);

    LineArtVerdict {
        is_line_art,
        cells: n,
        ink_share,
        texture_share,
        chroma_share,
        structured,
        avg_flat,
        mean_chroma,
        mid_share,
    }
}

fn luma_mid_share(rgb: &[u8], stride: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> (f32, u32) {
    let rw = (x1 - x0).max(1);
    let rh = (y1 - y0).max(1);
    let step = ((((rw * rh) as f64) / 65536.0).sqrt().ceil() as u32).max(1);
    let mut hist = [0u32; 256];
    let mut total = 0u32;
    let mut y = y0;
    while y < y1 {
        let mut x = x0;
        while x < x1 {
            let i = ((y * stride + x) * 3) as usize;
            let l = luma601(rgb[i], rgb[i + 1], rgb[i + 2]) as usize;
            hist[l] += 1;
            total += 1;
            x += step;
        }
        y += step;
    }
    if total == 0 {
        return (1.0, 0);
    }
    let percentile = |p: f64| -> usize {
        let target = (total as f64 * p) as u32;
        let mut acc = 0u32;
        for (v, &c) in hist.iter().enumerate() {
            acc += c;
            if acc >= target {
                return v;
            }
        }
        255
    };
    let lo = percentile(0.05);
    let hi = percentile(0.95);
    let spread = hi.saturating_sub(lo) as u32;
    if spread < BIMODAL_SPREAD_MIN {
        return (1.0, spread);
    }
    let band_lo = lo + (spread as usize) / 4;
    let band_hi = (lo + (spread as usize) * 3 / 4).min(255);
    let mid: u32 = hist[band_lo..=band_hi].iter().sum();
    (mid as f32 / total as f32, spread)
}

fn analyze_cell(
    rgb: &[u8],
    stride: u32,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    bx0: u32,
    by0: u32,
    bx1: u32,
    by1: u32,
) -> CellAnalysis {
    let lum = |x: i64, y: i64| -> i32 {
        let sx = x.clamp(bx0 as i64, bx1 as i64 - 1) as u32;
        let sy = y.clamp(by0 as i64, by1 as i64 - 1) as u32;
        let i = ((sy * stride + sx) * 3) as usize;
        luma601(rgb[i], rgb[i + 1], rgb[i + 2])
    };

    let n = ((x1 - x0) * (y1 - y0)).max(1) as i64;
    let mut sum = 0i64;
    let mut sum_sq = 0i64;
    let mut edge_count = 0i64;
    let mut flat_count = 0i64;
    let mut noise_sum = 0i64;
    let mut weak_count = 0i64;
    let mut dir = [0i64; 4];
    let mut cb_sum = 0i64;
    let mut cb_sq = 0i64;
    let mut cr_sum = 0i64;
    let mut cr_sq = 0i64;

    for y in y0..y1 {
        for x in x0..x1 {
            let c = lum(x as i64, y as i64);
            sum += c as i64;
            sum_sq += (c as i64) * (c as i64);

            let i = ((y * stride + x) * 3) as usize;
            let r = rgb[i] as i32;
            let b = rgb[i + 2] as i32;
            let cb = b - c;
            let cr = r - c;
            cb_sum += cb as i64;
            cb_sq += (cb as i64) * (cb as i64);
            cr_sum += cr as i64;
            cr_sq += (cr as i64) * (cr as i64);

            let tl = lum(x as i64 - 1, y as i64 - 1);
            let tc = lum(x as i64, y as i64 - 1);
            let tr = lum(x as i64 + 1, y as i64 - 1);
            let ml = lum(x as i64 - 1, y as i64);
            let mr = lum(x as i64 + 1, y as i64);
            let bl = lum(x as i64 - 1, y as i64 + 1);
            let bc = lum(x as i64, y as i64 + 1);
            let br = lum(x as i64 + 1, y as i64 + 1);
            let gx = (tr + 2 * mr + br) - (tl + 2 * ml + bl);
            let gy = (bl + 2 * bc + br) - (tl + 2 * tc + tr);
            let grad = gx.abs() + gy.abs();

            if grad > EDGE_GRAD {
                edge_count += 1;
                let ax = gx.abs();
                let ay = gy.abs();
                let bin = if ax >= 2 * ay {
                    0
                } else if ay >= 2 * ax {
                    1
                } else if (gx > 0) == (gy > 0) {
                    2
                } else {
                    3
                };
                dir[bin] += 1;
            }
            if grad < FLAT_GRAD {
                flat_count += 1;
            }
            if grad < WEAK_GRAD {
                let box_mean = (tl + tc + tr + ml + c + mr + bl + bc + br) / 9;
                noise_sum += (c - box_mean).abs() as i64;
                weak_count += 1;
            }
        }
    }

    let mean = sum / n;
    let variance = ((sum_sq / n) - mean * mean).max(0) as u32;
    let edge_density_q8 = ((edge_count * 256) / n) as u16;
    let flat_ratio_q8 = ((flat_count * 256) / n) as u16;
    let noise = if weak_count > 0 {
        (noise_sum / weak_count) as u16
    } else {
        0
    };
    let edge_total = dir.iter().sum::<i64>().max(1);
    let dir_max = *dir.iter().max().unwrap();
    let dir_dominance_q8 = ((dir_max * 256) / edge_total) as u16;
    let axis_aligned_q8 = (((dir[0] + dir[1]) * 256) / edge_total) as u16;
    let orient_entropy_q8 = entropy4_q8(&dir, edge_total);

    let cb_mean = cb_sum / n;
    let cr_mean = cr_sum / n;
    let v_cb = ((cb_sq / n) - cb_mean * cb_mean).max(0);
    let v_cr = ((cr_sq / n) - cr_mean * cr_mean).max(0);
    let chroma_activity = (v_cb + v_cr).min(u16::MAX as i64) as u16;

    CellAnalysis {
        class: classify(
            variance,
            edge_density_q8,
            orient_entropy_q8,
            dir_dominance_q8,
            axis_aligned_q8,
            noise,
            chroma_activity,
        ),
        flat_ratio_q8,
        chroma_activity,
    }
}

fn classify(
    variance: u32,
    edge_density_q8: u16,
    orient_entropy_q8: u16,
    dir_dominance_q8: u16,
    axis_aligned_q8: u16,
    noise: u16,
    chroma_activity: u16,
) -> RegionClass {
    if edge_density_q8 >= TEXT_EDGE_Q8
        && orient_entropy_q8 < LOW_ENTROPY_Q8
        && axis_aligned_q8 >= AXIS_ALIGNED_Q8
        && noise < NOISE_HI
    {
        return RegionClass::TextLike;
    }
    if edge_density_q8 >= EDGE_DENSE_Q8 && dir_dominance_q8 >= DIR_DOMINANT_Q8 {
        return RegionClass::DirectionalEdge;
    }
    if noise >= NOISE_HI && variance < TEXTURE_VAR {
        return RegionClass::Noisy;
    }
    if variance >= TEXTURE_VAR && orient_entropy_q8 >= HIGH_ENTROPY_Q8 {
        return RegionClass::Texture;
    }
    if variance < FLAT_VAR && edge_density_q8 < FLAT_EDGE_Q8 {
        return RegionClass::Flat;
    }
    if edge_density_q8 < GRAD_EDGE_Q8 {
        return RegionClass::Gradient;
    }
    if chroma_activity >= CHROMA_HI {
        return RegionClass::ChromaCritical;
    }
    RegionClass::Texture
}

fn entropy4_q8(dir: &[i64; 4], total: i64) -> u16 {
    if total <= 0 {
        return 0;
    }
    let t = total as f64;
    let mut h = 0.0f64;
    for &c in dir {
        if c > 0 {
            let p = c as f64 / t;
            h -= p * p.log2();
        }
    }
    ((h / 2.0) * 256.0).round().clamp(0.0, 256.0) as u16
}

#[inline]
fn luma601(r: u8, g: u8, b: u8) -> i32 {
    ((77 * r as i32 + 150 * g as i32 + 29 * b as i32 + 128) >> 8).clamp(0, 255)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_from_luma(luma: &[u8], width: usize, height: usize) -> Vec<u8> {
        let mut rgb = vec![0u8; width * height * 3];
        for (i, &y) in luma.iter().enumerate() {
            rgb[i * 3] = y;
            rgb[i * 3 + 1] = y;
            rgb[i * 3 + 2] = y;
        }
        rgb
    }

    fn full_box(width: usize, height: usize) -> [f32; 4] {
        [0.0, 0.0, width as f32, height as f32]
    }

    #[test]
    fn staff_lines_are_line_art() {
        let (w, h) = (256usize, 256usize);
        let mut luma = vec![245u8; w * h];
        for y in (8..h).step_by(8) {
            for x in 8..(w - 8) {
                luma[y * w + x] = 20;
            }
        }
        let rgb = rgb_from_luma(&luma, w, h);
        assert!(region_is_line_art(&rgb, w, h, full_box(w, h)));
    }

    #[test]
    fn checkerboard_hatching_is_line_art() {
        // Fine black/white hatching is how engravings look at cell scale.
        let (w, h) = (256usize, 256usize);
        let mut luma = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                luma[y * w + x] = if (x + y) % 2 == 0 { 0 } else { 255 };
            }
        }
        let rgb = rgb_from_luma(&luma, w, h);
        assert!(region_is_line_art(&rgb, w, h, full_box(w, h)));
    }

    #[test]
    fn smooth_gradient_photo_is_not_line_art() {
        let (w, h) = (256usize, 128usize);
        let mut luma = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                luma[y * w + x] = x as u8;
            }
        }
        let rgb = rgb_from_luma(&luma, w, h);
        assert!(!region_is_line_art(&rgb, w, h, full_box(w, h)));
    }

    #[test]
    fn colourful_map_is_not_line_art() {
        let (w, h) = (256usize, 256usize);
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                rgb[i] = (x as u8).wrapping_mul(3);
                rgb[i + 1] = (y as u8).wrapping_mul(5);
                rgb[i + 2] = ((x + y) as u8).wrapping_mul(7);
            }
        }
        assert!(!region_is_line_art(&rgb, w, h, full_box(w, h)));
    }

    #[test]
    fn empty_bbox_is_treated_as_line_art() {
        let rgb = vec![128u8; 12];
        assert!(region_is_line_art(&rgb, 2, 2, [0.0, 0.0, 0.0, 0.0]));
    }
}
