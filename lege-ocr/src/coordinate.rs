use crate::types::OcrLineResult;

/// Scale OCR line results (and their word boxes) from one pixel space into
/// another by independent x/y factors, clamping to `max_w`/`max_h`.
///
/// Line bboxes are page-global; word boxes (`bbox_crop_local`) are relative to
/// their line origin. Both are scaled by the same factors, so a word's absolute
/// position (line_origin + local) scales consistently.
pub fn scale_lines(lines: &mut [OcrLineResult], sx: f32, sy: f32, max_w: u32, max_h: u32) {
    if !sx.is_finite() || !sy.is_finite() || sx < 0.0 || sy < 0.0 {
        return;
    }
    if (sx - 1.0).abs() < f32::EPSILON && (sy - 1.0).abs() < f32::EPSILON {
        return;
    }
    let scale = |v: u32, s: f32, max: u32| ((v as f32 * s).round() as u32).min(max);
    for line in lines.iter_mut() {
        let [x1, y1, x2, y2] = line.bbox_highres;
        line.bbox_highres = [
            scale(x1, sx, max_w),
            scale(y1, sy, max_h),
            scale(x2, sx, max_w),
            scale(y2, sy, max_h),
        ];
        // Word boxes are line-local; clamp to the scaled line dimensions.
        let lw = line.bbox_highres[2].saturating_sub(line.bbox_highres[0]);
        let lh = line.bbox_highres[3].saturating_sub(line.bbox_highres[1]);
        for word in line.words.iter_mut() {
            let [wx1, wy1, wx2, wy2] = word.bbox_crop_local;
            word.bbox_crop_local = [
                scale(wx1, sx, lw),
                scale(wy1, sy, lh),
                scale(wx2, sx, lw),
                scale(wy2, sy, lh),
            ];
        }
    }
}

/// Coordinate mapping between analysis space, high-res pixel space, and PDF point space.
///
/// In the main Lege pipeline, the analysis image and the high-res image are the same
/// dimensions (binarization is performed at high-res). The map is still provided so the
/// standalone debug tool can handle cases where they differ.
#[derive(Debug, Clone)]
pub struct CoordinateMap {
    pub highres_width: u32,
    pub highres_height: u32,
    pub analysis_width: u32,
    pub analysis_height: u32,
    pub pdf_width_pts: f32,
    pub pdf_height_pts: f32,
}

impl CoordinateMap {
    /// Create a map where analysis and high-res images have the same dimensions.
    pub fn identity(width: u32, height: u32, pdf_w: f32, pdf_h: f32) -> Self {
        Self {
            highres_width: width,
            highres_height: height,
            analysis_width: width,
            analysis_height: height,
            pdf_width_pts: pdf_w,
            pdf_height_pts: pdf_h,
        }
    }

    /// Map [x1, y1, x2, y2] from analysis-image space to high-res-image space.
    pub fn analysis_to_highres(&self, rect: [f32; 4]) -> [u32; 4] {
        if self.analysis_width == 0 || self.analysis_height == 0 {
            return [0; 4];
        }
        let sx = self.highres_width as f64 / self.analysis_width as f64;
        let sy = self.highres_height as f64 / self.analysis_height as f64;
        let x = |value: f32, round_up: bool| {
            if !value.is_finite() {
                return 0;
            }
            let scaled = value as f64 * sx;
            let rounded = if round_up {
                scaled.ceil()
            } else {
                scaled.floor()
            };
            rounded.clamp(0.0, self.highres_width as f64) as u32
        };
        let y = |value: f32, round_up: bool| {
            if !value.is_finite() {
                return 0;
            }
            let scaled = value as f64 * sy;
            let rounded = if round_up {
                scaled.ceil()
            } else {
                scaled.floor()
            };
            rounded.clamp(0.0, self.highres_height as f64) as u32
        };
        [
            x(rect[0], false),
            y(rect[1], false),
            x(rect[2], true),
            y(rect[3], true),
        ]
    }

    /// Map [x1, y1, x2, y2] from high-res pixel space to PDF point space.
    pub fn highres_to_pdf_pts(&self, rect: [u32; 4]) -> [f32; 4] {
        if self.highres_width == 0 || self.highres_height == 0 {
            return [0.0; 4];
        }
        let sx = self.pdf_width_pts / self.highres_width as f32;
        let sy = self.pdf_height_pts / self.highres_height as f32;
        [
            rect[0] as f32 * sx,
            rect[1] as f32 * sy,
            rect[2] as f32 * sx,
            rect[3] as f32 * sy,
        ]
    }

    /// Translate a crop-local rect by the crop's page-global origin.
    pub fn line_local_to_page(&self, rect: [u32; 4], origin: [u32; 2]) -> [u32; 4] {
        [
            rect[0].saturating_add(origin[0]).min(self.highres_width),
            rect[1].saturating_add(origin[1]).min(self.highres_height),
            rect[2].saturating_add(origin[0]).min(self.highres_width),
            rect[3].saturating_add(origin[1]).min(self.highres_height),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trip() {
        let m = CoordinateMap::identity(2000, 2800, 612.0, 792.0);
        let r = m.analysis_to_highres([100.0, 200.0, 400.0, 600.0]);
        assert_eq!(r, [100, 200, 400, 600]);
    }

    #[test]
    fn scale_analysis_to_highres() {
        let m = CoordinateMap {
            highres_width: 1024,
            highres_height: 1024,
            analysis_width: 512,
            analysis_height: 512,
            pdf_width_pts: 612.0,
            pdf_height_pts: 792.0,
        };
        let r = m.analysis_to_highres([10.0, 20.0, 50.0, 80.0]);
        assert_eq!(r, [20, 40, 100, 160]);
    }

    #[test]
    fn scale_lines_downscales_line_and_word_boxes() {
        use crate::types::{OcrLineResult, OcrWord};
        let mut lines = vec![OcrLineResult {
            text: "hi".to_string(),
            confidence: None,
            words: vec![OcrWord {
                text: "hi".to_string(),
                bbox_crop_local: [10, 4, 40, 20],
                confidence: None,
            }],
            bbox_highres: [100, 200, 400, 240],
        }];
        // Downscale by 0.5 (e.g. high-res 2x -> output 1x).
        scale_lines(&mut lines, 0.5, 0.5, 1000, 1000);
        assert_eq!(lines[0].bbox_highres, [50, 100, 200, 120]);
        assert_eq!(lines[0].words[0].bbox_crop_local, [5, 2, 20, 10]);
    }

    #[test]
    fn scale_lines_identity_is_noop() {
        use crate::types::OcrLineResult;
        let mut lines = vec![OcrLineResult {
            text: "x".to_string(),
            confidence: None,
            words: Vec::new(),
            bbox_highres: [1, 2, 3, 4],
        }];
        scale_lines(&mut lines, 1.0, 1.0, 100, 100);
        assert_eq!(lines[0].bbox_highres, [1, 2, 3, 4]);
    }

    #[test]
    fn line_local_to_page_offset() {
        let m = CoordinateMap::identity(2000, 2800, 612.0, 792.0);
        let r = m.line_local_to_page([10, 5, 100, 25], [500, 300]);
        assert_eq!(r, [510, 305, 600, 325]);
    }

    #[test]
    fn clamped_to_highres_bounds() {
        let m = CoordinateMap::identity(500, 500, 612.0, 792.0);
        let r = m.line_local_to_page([0, 0, 100, 100], [450, 450]);
        assert_eq!(r, [450, 450, 500, 500]);
    }

    #[test]
    fn malformed_coordinate_inputs_do_not_overflow_or_escape_bounds() {
        let m = CoordinateMap {
            highres_width: 100,
            highres_height: 200,
            analysis_width: 0,
            analysis_height: 0,
            pdf_width_pts: 612.0,
            pdf_height_pts: 792.0,
        };
        assert_eq!(
            m.analysis_to_highres([f32::NAN, -1.0, f32::INFINITY, 4.0]),
            [0; 4]
        );
        assert_eq!(
            m.line_local_to_page([u32::MAX; 4], [u32::MAX; 2]),
            [100, 200, 100, 200]
        );
    }
}
