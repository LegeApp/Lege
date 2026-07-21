//! Debug overlays for RasterReflow.
//!
//! Renders detected geometry (columns/regions, rows, words) and final
//! placements onto RGB images so the segmentation and composition can be
//! eyeballed during development. Colours are deliberately distinct; nothing here
//! affects the production output.

#![allow(dead_code)]

use super::compose::StripBlock;
use super::types::{PxRect, ReflowPage, ReflowRegion, RegionKind, TextRow, WordSpan};
use image::buffer::ConvertBuffer;
use image::{GrayImage, Rgb, RgbImage};

/// Standard overlay colours.
pub const COLOR_REGION: Rgb<u8> = Rgb([220, 30, 30]); // red
pub const COLOR_FIGURE: Rgb<u8> = Rgb([30, 120, 220]); // blue
pub const COLOR_ROW: Rgb<u8> = Rgb([30, 170, 60]); // green
pub const COLOR_WORD: Rgb<u8> = Rgb([230, 150, 20]); // orange
pub const COLOR_PLACEMENT: Rgb<u8> = Rgb([170, 30, 200]); // purple

/// Promote a grayscale page to RGB so coloured overlays can be drawn on it.
pub fn gray_to_rgb(gray: &GrayImage) -> RgbImage {
    gray.convert()
}

/// Draw a hollow rectangle outline of the given `thickness` (clipped to image).
pub fn draw_rect(img: &mut RgbImage, rect: PxRect, color: Rgb<u8>, thickness: u32) {
    if rect.is_empty() {
        return;
    }
    let (iw, ih) = img.dimensions();
    let t = thickness.max(1);
    let x0 = rect.x.min(iw);
    let y0 = rect.y.min(ih);
    let x1 = rect.right().min(iw);
    let y1 = rect.bottom().min(ih);

    for x in x0..x1 {
        for k in 0..t {
            if y0 + k < ih {
                img.put_pixel(x, y0 + k, color);
            }
            if y1 > k && y1 - 1 - k < ih {
                img.put_pixel(x, y1 - 1 - k, color);
            }
        }
    }
    for y in y0..y1 {
        for k in 0..t {
            if x0 + k < iw {
                img.put_pixel(x0 + k, y, color);
            }
            if x1 > k && x1 - 1 - k < iw {
                img.put_pixel(x1 - 1 - k, y, color);
            }
        }
    }
}

/// Overlay regions on a source page (regions in red, figures/tables in blue).
pub fn overlay_regions(gray: &GrayImage, regions: &[ReflowRegion]) -> RgbImage {
    let mut img = gray_to_rgb(gray);
    for r in regions {
        let color = if r.kind == RegionKind::Text {
            COLOR_REGION
        } else {
            COLOR_FIGURE
        };
        draw_rect(&mut img, r.rect, color, 2);
    }
    img
}

/// Overlay rows (green) and words (orange) onto an existing RGB image.
pub fn overlay_rows_words(img: &mut RgbImage, rows: &[TextRow], words: &[WordSpan]) {
    for r in rows {
        draw_rect(img, r.rect, COLOR_ROW, 1);
    }
    for w in words {
        draw_rect(img, w.rect, COLOR_WORD, 1);
    }
}

/// Render a planned output page's placements as outlines on a white canvas.
pub fn render_page_overlay(page: &ReflowPage) -> RgbImage {
    let mut img = RgbImage::from_pixel(page.width, page.height, Rgb([255, 255, 255]));
    for it in &page.items {
        draw_rect(&mut img, it.out_rect, COLOR_PLACEMENT, 1);
    }
    img
}

/// Outline composed master-strip blocks on a tall white canvas (for inspecting
/// composition before pagination). Width is the content width; height is the
/// summed block heights (capped to avoid runaway allocations).
pub fn render_strip_overlay(blocks: &[StripBlock], width: u32) -> RgbImage {
    let total: u32 = blocks.iter().map(|b| b.height).sum::<u32>().min(20_000);
    let h = total.max(1);
    let mut img = RgbImage::from_pixel(width.max(1), h, Rgb([255, 255, 255]));
    let mut y = 0u32;
    for b in blocks {
        if y >= h {
            break;
        }
        for it in &b.items {
            let r = PxRect::new(it.rect.x, y + it.rect.y, it.rect.w, it.rect.h);
            draw_rect(&mut img, r, COLOR_PLACEMENT, 1);
        }
        y += b.height;
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    #[test]
    fn gray_to_rgb_preserves_intensity() {
        let mut g = GrayImage::new(2, 2);
        g.put_pixel(0, 0, Luma([100]));
        let rgb = gray_to_rgb(&g);
        assert_eq!(rgb.get_pixel(0, 0), &Rgb([100, 100, 100]));
    }

    #[test]
    fn draw_rect_marks_borders_not_center() {
        let mut img = RgbImage::from_pixel(10, 10, Rgb([0, 0, 0]));
        draw_rect(&mut img, PxRect::new(2, 2, 6, 6), COLOR_ROW, 1);
        // corner drawn
        assert_eq!(img.get_pixel(2, 2), &COLOR_ROW);
        // center untouched
        assert_eq!(img.get_pixel(5, 5), &Rgb([0, 0, 0]));
    }

    #[test]
    fn draw_rect_clips_to_image() {
        let mut img = RgbImage::from_pixel(5, 5, Rgb([0, 0, 0]));
        // rect extends beyond bounds — must not panic
        draw_rect(&mut img, PxRect::new(3, 3, 10, 10), COLOR_WORD, 2);
    }
}
