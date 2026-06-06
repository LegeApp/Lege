use std::path::Path;

use anyhow::Result;
use image::{GrayImage, Rgb, RgbImage};

use crate::types::{OcrLineResult, SegmentationResult, TextRegion};

/// Write a binary crop to disk as a PNG (0=black ink, 255=white background).
pub fn save_binary_crop(path: &Path, binary: &[u8], width: u32, height: u32) -> Result<()> {
    let img = GrayImage::from_raw(width, height, binary.to_vec())
        .ok_or_else(|| anyhow::anyhow!("failed to create GrayImage from binary crop"))?;
    img.save(path)?;
    Ok(())
}

/// Write a GrayImage line crop to disk.
pub fn save_gray_crop(path: &Path, img: &GrayImage) -> Result<()> {
    img.save(path)?;
    Ok(())
}

/// Write horizontal projection data as CSV (y, raw_count, smoothed_count).
pub fn save_projection_csv(path: &Path, raw: &[u32], smoothed: &[f32]) -> Result<()> {
    use std::fmt::Write as FmtWrite;
    let mut s = String::from("y,raw,smoothed\n");
    for (i, (r, sm)) in raw.iter().zip(smoothed.iter()).enumerate() {
        writeln!(s, "{i},{r},{sm:.2}")?;
    }
    std::fs::write(path, s)?;
    Ok(())
}

/// Write an annotated PNG showing the binary crop with line split boundaries drawn.
pub fn save_lines_annotated(
    path: &Path,
    binary: &[u8],
    width: u32,
    height: u32,
    seg: &SegmentationResult,
    origin_x: u32,
    origin_y: u32,
) -> Result<()> {
    // Convert binary to RGB so we can draw colored lines
    let mut rgb = RgbImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) as usize;
            let v = binary.get(idx).copied().unwrap_or(255);
            rgb.put_pixel(x, y, Rgb([v, v, v]));
        }
    }

    // Draw line boundaries in red
    for bbox in &seg.line_bboxes {
        let [gx1, gy1, gx2, gy2] = *bbox;
        let bx1 = gx1.saturating_sub(origin_x);
        let by1 = gy1.saturating_sub(origin_y);
        let bx2 = gx2.saturating_sub(origin_x);
        let by2 = gy2.saturating_sub(origin_y);
        // Draw top and bottom edges of each line bbox
        for x in bx1..bx2.min(width) {
            let top = by1.min(height - 1);
            let bottom = by2.saturating_sub(1).min(height - 1);
            rgb.put_pixel(x, top, Rgb([255, 0, 0]));
            rgb.put_pixel(x, bottom, Rgb([255, 0, 0]));
        }
    }

    rgb.save(path)?;
    Ok(())
}

/// Write an OCR line result as JSON.
pub fn save_ocr_result_json(path: &Path, result: &OcrLineResult) -> Result<()> {
    let json = serde_json::to_string_pretty(result)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Write the assembled page hOCR to disk.
pub fn save_page_hocr(path: &Path, hocr: &str) -> Result<()> {
    std::fs::write(path, hocr)?;
    Ok(())
}

/// Write a minimal SVG overlay showing regions and line bboxes on a white canvas.
pub fn save_overlay_svg(
    path: &Path,
    page_width: u32,
    page_height: u32,
    regions: &[TextRegion],
    lines: &[OcrLineResult],
) -> Result<()> {
    use std::fmt::Write as FmtWrite;
    let mut svg = String::new();
    writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{page_width}" height="{page_height}" viewBox="0 0 {page_width} {page_height}">"#
    )?;
    writeln!(
        svg,
        r#"  <rect width="{page_width}" height="{page_height}" fill="white"/>"#
    )?;

    // Regions in blue
    for r in regions {
        let [x1, y1, x2, y2] = r.bbox_highres;
        let (w, h) = (x2.saturating_sub(x1), y2.saturating_sub(y1));
        writeln!(
            svg,
            r#"  <rect x="{x1}" y="{y1}" width="{w}" height="{h}" fill="none" stroke="blue" stroke-width="3" opacity="0.6"/>"#
        )?;
    }

    // Lines in green
    for line in lines {
        let [x1, y1, x2, y2] = line.bbox_highres;
        let (w, h) = (x2.saturating_sub(x1), y2.saturating_sub(y1));
        writeln!(
            svg,
            r#"  <rect x="{x1}" y="{y1}" width="{w}" height="{h}" fill="none" stroke="green" stroke-width="2" opacity="0.7"/>"#
        )?;
        // Word bboxes in orange
        for word in &line.words {
            let [wx1, wy1, wx2, wy2] = word.bbox_crop_local;
            let (ww, wh) = (
                (wx2 + x1).saturating_sub(wx1 + x1),
                (wy2 + y1).saturating_sub(wy1 + y1),
            );
            writeln!(
                svg,
                r#"  <rect x="{}" y="{}" width="{ww}" height="{wh}" fill="none" stroke="orange" stroke-width="1" opacity="0.5"/>"#,
                wx1 + x1,
                wy1 + y1
            )?;
        }
    }

    writeln!(svg, "</svg>")?;
    std::fs::write(path, svg)?;
    Ok(())
}

/// Create a numbered debug directory for a page and return its path.
pub fn make_page_debug_dir(out_dir: &Path, page_index: usize) -> Result<std::path::PathBuf> {
    let dir = out_dir.join(format!("page_{:03}", page_index + 1));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
