//! Pure-Rust DBNet post-processing for PP-OCR text detection.
//!
//! DBNet emits a per-pixel text-probability map `[1,1,H,W]`. Turning that into
//! text-line boxes is: threshold to a bitmap, group connected foreground pixels
//! into regions, keep regions whose mean probability clears `box_thresh`, and
//! "unclip" each region's box outward (the shrunk-region training makes the raw
//! box tighter than the true text). Pages reaching OCR are already deskewed
//! upstream, so axis-aligned boxes are sufficient — we skip the rotated
//! min-area-rectangle fit that opencv-based pipelines use.

/// DB post-processing thresholds (PP-OCR defaults).
#[derive(Debug, Clone)]
pub(crate) struct DbConfig {
    /// Probability above which a pixel is text (bitmap threshold).
    pub(crate) thresh: f32,
    /// Minimum mean region probability to keep a box.
    pub(crate) box_thresh: f32,
    /// Outward expansion ratio (area·ratio / perimeter pixels each side).
    pub(crate) unclip_ratio: f32,
    /// Discard regions smaller than this pixel area.
    pub(crate) min_area: usize,
    /// Discard boxes whose shorter side is below this many pixels.
    pub(crate) min_side: f32,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            thresh: 0.3,
            box_thresh: 0.6,
            unclip_ratio: 1.5,
            min_area: 16,
            min_side: 3.0,
        }
    }
}

/// An axis-aligned text box in probability-map pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DbBox {
    pub(crate) x0: f32,
    pub(crate) y0: f32,
    pub(crate) x1: f32,
    pub(crate) y1: f32,
    pub(crate) score: f32,
}

/// Extract text boxes from a probability map (`prob.len() == w*h`, row-major).
///
/// Boxes are in prob-map coordinates; the caller scales them to the original
/// image. Regions are found by 4-connected flood fill over the thresholded
/// bitmap — a single pass with an explicit stack, no recursion.
pub(crate) fn boxes_from_prob(prob: &[f32], w: usize, h: usize, cfg: &DbConfig) -> Vec<DbBox> {
    let Some(pixel_count) = w.checked_mul(h) else {
        return Vec::new();
    };
    if pixel_count == 0 || prob.len() != pixel_count {
        return Vec::new();
    }
    // Only visited/unvisited state is needed; retaining a u32 component label
    // consumed an avoidable ~4 MiB for a 960x960 detector map.
    let mut visited = vec![false; pixel_count];
    let mut stack: Vec<usize> = Vec::new();
    let mut boxes = Vec::new();

    for start in 0..pixel_count {
        // Written this way so NaN probabilities are treated as background.
        if !(prob[start] > cfg.thresh) || visited[start] {
            continue;
        }
        visited[start] = true;
        stack.push(start);
        let (mut minx, mut miny) = (usize::MAX, usize::MAX);
        let (mut maxx, mut maxy) = (0usize, 0usize);
        let mut psum = 0.0f64;
        let mut pcnt = 0usize;

        while let Some(idx) = stack.pop() {
            let x = idx % w;
            let y = idx / w;
            psum += prob[idx] as f64;
            pcnt += 1;
            minx = minx.min(x);
            maxx = maxx.max(x);
            miny = miny.min(y);
            maxy = maxy.max(y);
            // 4-neighbourhood
            let visit = |nx: usize, ny: usize, stack: &mut Vec<usize>, visited: &mut [bool]| {
                let nidx = ny * w + nx;
                if prob[nidx] > cfg.thresh && !visited[nidx] {
                    visited[nidx] = true;
                    stack.push(nidx);
                }
            };
            if x > 0 {
                visit(x - 1, y, &mut stack, &mut visited);
            }
            if x + 1 < w {
                visit(x + 1, y, &mut stack, &mut visited);
            }
            if y > 0 {
                visit(x, y - 1, &mut stack, &mut visited);
            }
            if y + 1 < h {
                visit(x, y + 1, &mut stack, &mut visited);
            }
        }

        let bw = maxx - minx + 1;
        let bh = maxy - miny + 1;
        if bw * bh < cfg.min_area {
            continue;
        }
        let score = (psum / pcnt.max(1) as f64) as f32;
        if score < cfg.box_thresh {
            continue;
        }
        // Unclip: offset every side outward by area·ratio / perimeter.
        let peri = 2 * (bw + bh);
        let d = pcnt as f32 * cfg.unclip_ratio / peri.max(1) as f32;
        let x0 = minx as f32 - d;
        let y0 = miny as f32 - d;
        let x1 = maxx as f32 + 1.0 + d;
        let y1 = maxy as f32 + 1.0 + d;
        if (x1 - x0).min(y1 - y0) < cfg.min_side {
            continue;
        }
        boxes.push(DbBox {
            x0,
            y0,
            x1,
            y1,
            score,
        });
    }
    boxes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connected_component_produces_an_unclipped_box() {
        let mut prob = vec![0.0; 8 * 6];
        for y in 2..4 {
            for x in 2..6 {
                prob[y * 8 + x] = 0.9;
            }
        }
        let cfg = DbConfig {
            min_area: 1,
            min_side: 1.0,
            ..DbConfig::default()
        };
        let boxes = boxes_from_prob(&prob, 8, 6, &cfg);
        assert_eq!(boxes.len(), 1);
        let bbox = boxes[0];
        assert!(bbox.x0 < 2.0 && bbox.y0 < 2.0);
        assert!(bbox.x1 > 6.0 && bbox.y1 > 4.0);
        assert!((bbox.score - 0.9).abs() < 1e-6);
    }

    #[test]
    fn malformed_and_nan_probability_maps_are_safe() {
        let cfg = DbConfig::default();
        assert!(boxes_from_prob(&[0.9], 2, 2, &cfg).is_empty());
        assert!(boxes_from_prob(&[f32::NAN; 4], 2, 2, &cfg).is_empty());
        assert!(boxes_from_prob(&[], usize::MAX, 2, &cfg).is_empty());
    }
}
