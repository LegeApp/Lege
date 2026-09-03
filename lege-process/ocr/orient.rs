//! Turning a page's raster upright for the recognizer.
//!
//! The recognizer reads lines left to right and copes with a degree or so
//! of skew; a page scanned sideways, upside down or at a larger angle comes
//! back as noise. A [`PageFrame`] says how the page's text sits (measured
//! from its connected components, see `encoding::straighten`), so the
//! raster is put upright before recognition and every recognized box is
//! mapped back onto the page as scanned, which is where the text layer and
//! the glyph dictionary expect it.

use image::RgbImage;
use lege_ocr::types::OcrLineResult;

pub use crate::encoding::straighten::PageFrame;

/// Skew below this many degrees is left to the recognizer: this much tilt
/// costs it nothing, and resampling the raster costs a little sharpness.
pub const OCR_SKEW_MIN_DEG: f64 = 0.75;

/// How a raster is turned upright: the frame in the raster's own pixels,
/// and the canvas the upright page is drawn on (the turned page, grown to
/// hold its rotated corners).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Upright {
    frame: PageFrame,
    canvas_w: u32,
    canvas_h: u32,
    /// Where the upright frame's origin sits on the canvas.
    ox: f64,
    oy: f64,
}

impl Upright {
    /// The upright view of a `width × height` raster whose text sits as
    /// `frame` says (`frame` may describe the same page at another size).
    /// `None` when the raster is fine as it is.
    pub fn of(frame: &PageFrame, width: u32, height: u32) -> Option<Self> {
        let mut frame = frame.resized(width, height);
        if frame.skew.abs().to_degrees() < OCR_SKEW_MIN_DEG {
            frame.skew = 0.0;
        }
        if frame.is_identity() || width == 0 || height == 0 {
            return None;
        }
        let (x0, y0, x1, y1) = frame.upright_bounds();
        Some(Self {
            frame,
            canvas_w: ((x1 - x0).ceil() as u32).max(1),
            canvas_h: ((y1 - y0).ceil() as u32).max(1),
            ox: -x0,
            oy: -y0,
        })
    }

    pub fn frame(&self) -> &PageFrame {
        &self.frame
    }

    pub fn canvas_size(&self) -> (u32, u32) {
        (self.canvas_w, self.canvas_h)
    }

    /// A raster point on the canvas.
    pub fn to_canvas(&self, x: f64, y: f64) -> (f64, f64) {
        let (ux, uy) = self.frame.to_upright(x, y);
        (ux + self.ox, uy + self.oy)
    }

    /// A canvas point back on the raster.
    pub fn to_page(&self, x: f64, y: f64) -> (f64, f64) {
        self.frame.to_scanned(x - self.ox, y - self.oy)
    }

    /// The canvas box covering a raster box.
    pub fn rect_to_canvas(&self, rect: [u32; 4]) -> [u32; 4] {
        bounds_of(
            rect,
            |x, y| self.to_canvas(x, y),
            self.canvas_w,
            self.canvas_h,
        )
    }

    /// The raster box covering a canvas box.
    pub fn rect_to_page(&self, rect: [u32; 4]) -> [u32; 4] {
        bounds_of(
            rect,
            |x, y| self.to_page(x, y),
            self.frame.width,
            self.frame.height,
        )
    }

    /// The raster drawn upright on the canvas, `channels` bytes per pixel,
    /// white outside the page. A quarter turn copies pixels; a skew samples
    /// bilinearly.
    pub fn resample(&self, pixels: &[u8], width: u32, height: u32, channels: usize) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (self.canvas_w as usize, self.canvas_h as usize);
        let mut out = vec![255u8; cw * ch * channels];
        if pixels.len() < w * h * channels || w == 0 || h == 0 {
            return out;
        }
        let (ax, ay) = self.frame.scanned_direction(1.0, 0.0);
        let exact = self.frame.skew == 0.0;
        for cy in 0..ch {
            // Sample positions in pixel-centre coordinates.
            let (sx0, sy0) = self.to_page(0.5, cy as f64 + 0.5);
            let (sx0, sy0) = (sx0 - 0.5, sy0 - 0.5);
            let row = &mut out[cy * cw * channels..(cy + 1) * cw * channels];
            for cx in 0..cw {
                let sx = sx0 + cx as f64 * ax;
                let sy = sy0 + cy_step(cx, ay);
                let dst = &mut row[cx * channels..(cx + 1) * channels];
                if exact {
                    let (x, y) = (sx.round() as i64, sy.round() as i64);
                    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                        let src = (y as usize * w + x as usize) * channels;
                        dst.copy_from_slice(&pixels[src..src + channels]);
                    }
                    continue;
                }
                let (fx, fy) = (sx.floor(), sy.floor());
                let (tx, ty) = (sx - fx, sy - fy);
                let (x0, y0) = (fx as i64, fy as i64);
                if x0 < -1 || y0 < -1 || x0 >= w as i64 || y0 >= h as i64 {
                    continue;
                }
                let at = |x: i64, y: i64, c: usize| -> f64 {
                    if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
                        255.0
                    } else {
                        pixels[(y as usize * w + x as usize) * channels + c] as f64
                    }
                };
                for (c, d) in dst.iter_mut().enumerate() {
                    let top = at(x0, y0, c) * (1.0 - tx) + at(x0 + 1, y0, c) * tx;
                    let bottom = at(x0, y0 + 1, c) * (1.0 - tx) + at(x0 + 1, y0 + 1, c) * tx;
                    *d = (top * (1.0 - ty) + bottom * ty).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        out
    }

    /// An RGB raster drawn upright.
    pub fn rgb(&self, image: &RgbImage) -> RgbImage {
        let pixels = self.resample(image.as_raw(), image.width(), image.height(), 3);
        RgbImage::from_raw(self.canvas_w, self.canvas_h, pixels).expect("canvas holds its pixels")
    }

    /// Recognized lines from the canvas back onto the raster. Word boxes
    /// stay relative to their line's box, which becomes the box covering
    /// the mapped words.
    pub fn lines_to_page(&self, lines: &mut [OcrLineResult]) {
        for line in lines.iter_mut() {
            let [lx, ly, _, _] = line.bbox_highres;
            let words: Vec<[u32; 4]> = line
                .words
                .iter()
                .map(|word| {
                    let [x1, y1, x2, y2] = word.bbox_crop_local;
                    self.rect_to_page([
                        x1.saturating_add(lx),
                        y1.saturating_add(ly),
                        x2.saturating_add(lx),
                        y2.saturating_add(ly),
                    ])
                })
                .collect();
            let line_box = if words.is_empty() {
                self.rect_to_page(line.bbox_highres)
            } else {
                words.iter().fold(words[0], |u, b| {
                    [
                        u[0].min(b[0]),
                        u[1].min(b[1]),
                        u[2].max(b[2]),
                        u[3].max(b[3]),
                    ]
                })
            };
            line.bbox_highres = line_box;
            for (word, b) in line.words.iter_mut().zip(words) {
                word.bbox_crop_local = [
                    b[0] - line_box[0],
                    b[1] - line_box[1],
                    b[2] - line_box[0],
                    b[3] - line_box[1],
                ];
            }
        }
    }
}

/// `ay` scaled by the column, kept as a function so the row loop reads as
/// the affine step it is.
#[inline]
fn cy_step(cx: usize, ay: f64) -> f64 {
    cx as f64 * ay
}

/// The box (clamped to `max_w × max_h`) covering a box's four corners
/// mapped through `map`.
fn bounds_of(
    rect: [u32; 4],
    map: impl Fn(f64, f64) -> (f64, f64),
    max_w: u32,
    max_h: u32,
) -> [u32; 4] {
    let [x1, y1, x2, y2] = rect.map(|v| v as f64);
    let corners = [(x1, y1), (x2, y1), (x1, y2), (x2, y2)].map(|(x, y)| map(x, y));
    let (mut bx0, mut by0, mut bx1, mut by1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (x, y) in corners {
        bx0 = bx0.min(x);
        by0 = by0.min(y);
        bx1 = bx1.max(x);
        by1 = by1.max(y);
    }
    let clamp = |v: f64, max: u32| v.round().clamp(0.0, max as f64) as u32;
    [
        clamp(bx0, max_w),
        clamp(by0, max_h),
        clamp(bx1, max_w),
        clamp(by1, max_h),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_ocr::types::OcrWord;

    fn frame(w: u32, h: u32, turns: u8, skew_deg: f64) -> PageFrame {
        PageFrame {
            width: w,
            height: h,
            turns,
            skew: skew_deg.to_radians(),
        }
    }

    #[test]
    fn level_pages_need_nothing() {
        assert!(Upright::of(&frame(100, 80, 0, 0.0), 100, 80).is_none());
        assert!(Upright::of(&frame(100, 80, 0, 0.5), 100, 80).is_none());
        assert!(Upright::of(&frame(100, 80, 1, 0.0), 100, 80).is_some());
        assert!(Upright::of(&frame(100, 80, 0, 3.0), 100, 80).is_some());
    }

    #[test]
    fn a_quarter_turn_copies_pixels_exactly() {
        // A 4×2 gray raster turned one quarter clockwise.
        let pixels = [10u8, 20, 30, 40, 50, 60, 70, 80];
        let up = Upright::of(&frame(4, 2, 1, 0.0), 4, 2).unwrap();
        assert_eq!(up.canvas_size(), (2, 4));
        let out = up.resample(&pixels, 4, 2, 1);
        // Row 0 of the turned raster is the left column, bottom to top.
        assert_eq!(out, vec![50, 10, 60, 20, 70, 30, 80, 40]);
        // And it maps back.
        let (x, y) = up.to_page(0.5, 0.5);
        assert!((x - 0.5).abs() < 1e-9 && (y - 1.5).abs() < 1e-9);
    }

    #[test]
    fn a_skewed_raster_is_resampled_onto_a_larger_canvas() {
        let (w, h) = (40u32, 30u32);
        let mut pixels = vec![255u8; (w * h) as usize];
        // A black horizontal bar in the middle.
        for x in 5..35 {
            pixels[(15 * w + x) as usize] = 0;
        }
        let up = Upright::of(&frame(w, h, 0, 10.0), w, h).unwrap();
        let (cw, ch) = up.canvas_size();
        assert!(cw > w && ch > h);
        let out = up.resample(&pixels, w, h, 1);
        assert_eq!(out.len(), (cw * ch) as usize);
        // Ink ends up somewhere, and the canvas corners stay white.
        assert!(out.iter().any(|&v| v < 128));
        assert_eq!(out[0], 255);
        assert_eq!(out[(cw * ch - 1) as usize], 255);
        // A page box round-trips through the canvas within a pixel.
        let c = up.rect_to_canvas([5, 15, 35, 16]);
        let back = up.rect_to_page(c);
        assert!(
            back[0] <= 6 && back[2] >= 34 && back[1] <= 15 && back[3] >= 16,
            "{back:?}"
        );
    }

    #[test]
    fn recognized_lines_come_back_onto_the_page() {
        // A page 100 wide, 60 tall, scanned one quarter turn off; the
        // recognizer sees a 60×100 canvas with a line at its top.
        let up = Upright::of(&frame(100, 60, 1, 0.0), 100, 60).unwrap();
        let mut lines = vec![OcrLineResult {
            text: "ab".into(),
            confidence: None,
            words: vec![
                OcrWord {
                    text: "a".into(),
                    bbox_crop_local: [0, 0, 10, 8],
                    confidence: None,
                },
                OcrWord {
                    text: "b".into(),
                    bbox_crop_local: [12, 0, 22, 8],
                    confidence: None,
                },
            ],
            bbox_highres: [5, 3, 27, 11],
        }];
        up.lines_to_page(&mut lines);
        let line = &lines[0];
        // Turned back: canvas x runs down the page, canvas y runs left
        // from the page's right edge (turn 1 maps (x, y) → (y, h − x)).
        assert_eq!(line.bbox_highres, [3, 33, 11, 55]);
        let a = line.words[0].bbox_crop_local;
        let b = line.words[1].bbox_crop_local;
        assert_eq!(a, [0, 12, 8, 22]);
        assert_eq!(b, [0, 0, 8, 10]);
    }
}
