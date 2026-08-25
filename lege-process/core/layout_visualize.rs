use crate::engine::{Detection, LayoutEngine, LayoutEngineConfig};
use crate::pagerender::prelude::{PdfRenderer, RasterConfig as PdfRasterConfig};
use crate::types::AppConfig;
use crate::{debug_println, info_println};
use anyhow::{Context, Result, anyhow};
use image::{RgbImage, Rgba, RgbaImage};
use std::path::PathBuf;
use std::sync::Arc;

const CLASS_COLORS: &[Rgba<u8>] = &[
    Rgba([255, 107, 107, 255]), // coral
    Rgba([81, 207, 102, 255]),  // green
    Rgba([77, 171, 247, 255]),  // sky blue
    Rgba([255, 169, 77, 255]),  // orange
    Rgba([177, 151, 252, 255]), // purple
    Rgba([56, 217, 169, 255]),  // teal
    Rgba([240, 101, 149, 255]), // rose
    Rgba([255, 212, 59, 255]),  // yellow
    Rgba([151, 117, 250, 255]), // indigo
    Rgba([105, 219, 124, 255]), // light green
];

/// Nearest-neighbour upscale for overlay labels: 3 gives a 15x21 pixel cell
/// per character, which stays readable over a full-resolution page render.
const LABEL_SCALE: u32 = 3;

fn draw_rect(canvas: &mut RgbaImage, x1: i32, y1: i32, x2: i32, y2: i32, color: Rgba<u8>) {
    let w = canvas.width() as i32;
    let h = canvas.height() as i32;

    for thickness in 0..2 {
        for px in x1 + thickness..=x2 - thickness {
            if px >= 0 && px < w {
                if y1 + thickness >= 0 && y1 + thickness < h {
                    canvas.put_pixel(px as u32, (y1 + thickness) as u32, color);
                }
                if y2 - thickness >= 0 && y2 - thickness < h {
                    canvas.put_pixel(px as u32, (y2 - thickness) as u32, color);
                }
            }
        }
        for py in y1 + thickness..=y2 - thickness {
            if py >= 0 && py < h {
                if x1 + thickness >= 0 && x1 + thickness < w {
                    canvas.put_pixel((x1 + thickness) as u32, py as u32, color);
                }
                if x2 - thickness >= 0 && x2 - thickness < w {
                    canvas.put_pixel((x2 - thickness) as u32, py as u32, color);
                }
            }
        }
    }
}

/// A 5x7 bitmap font covering printable ASCII (`0x20..=0x7F`).
///
/// Debug overlays previously rasterized their labels with `fontdue`, loading a
/// TrueType face by probing nine hard-coded system paths. That made the label
/// depend on the host having a font installed at one of those exact locations:
/// on a container, a minimal server, or a Windows box with a non-standard
/// Fonts directory, `load_font()` returned `None` and every label silently
/// vanished from the visualization. Baking the glyphs in removes both the
/// dependency and the failure mode -- the label always renders.
///
/// One entry per code point from `0x20` (space); each entry is seven rows of
/// five columns, most significant of the low five bits leftmost. `0x7F` is a
/// filled box used as the substitute glyph for anything out of range.
const GLYPH_ROWS: usize = 7;
const GLYPH_COLS: usize = 5;
const FONT_5X7: [[u8; GLYPH_ROWS]; 96] = [
    /* SP  */ [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
    /* !   */ [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100],
    /* "   */ [0b01010, 0b01010, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
    /* #   */ [0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010],
    /* $   */ [0b00100, 0b01111, 0b10100, 0b01110, 0b00101, 0b11110, 0b00100],
    /* %   */ [0b11000, 0b11001, 0b00010, 0b00100, 0b01000, 0b10011, 0b00011],
    /* &   */ [0b01100, 0b10010, 0b10100, 0b01000, 0b10101, 0b10010, 0b01101],
    /* '   */ [0b00100, 0b00100, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
    /* (   */ [0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010],
    /* )   */ [0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000],
    /* *   */ [0b00000, 0b10101, 0b01110, 0b11111, 0b01110, 0b10101, 0b00000],
    /* +   */ [0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000],
    /* ,   */ [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00100],
    /* -   */ [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000],
    /* .   */ [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100],
    /* /   */ [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
    /* 0   */ [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
    /* 1   */ [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    /* 2   */ [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
    /* 3   */ [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
    /* 4   */ [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
    /* 5   */ [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
    /* 6   */ [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
    /* 7   */ [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
    /* 8   */ [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
    /* 9   */ [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
    /* :   */ [0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000],
    /* ;   */ [0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b00100, 0b01000],
    /* <   */ [0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010],
    /* =   */ [0b00000, 0b00000, 0b11111, 0b00000, 0b11111, 0b00000, 0b00000],
    /* >   */ [0b01000, 0b00100, 0b00010, 0b00001, 0b00010, 0b00100, 0b01000],
    /* ?   */ [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b00000, 0b00100],
    /* @   */ [0b01110, 0b10001, 0b10111, 0b10101, 0b10111, 0b10000, 0b01110],
    /* A   */ [0b00100, 0b01010, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001],
    /* B   */ [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
    /* C   */ [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
    /* D   */ [0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100],
    /* E   */ [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
    /* F   */ [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
    /* G   */ [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
    /* H   */ [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
    /* I   */ [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    /* J   */ [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
    /* K   */ [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
    /* L   */ [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
    /* M   */ [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
    /* N   */ [0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001],
    /* O   */ [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
    /* P   */ [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
    /* Q   */ [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
    /* R   */ [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
    /* S   */ [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
    /* T   */ [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
    /* U   */ [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
    /* V   */ [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
    /* W   */ [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
    /* X   */ [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
    /* Y   */ [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
    /* Z   */ [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
    /* [   */ [0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110],
    /* \   */ [0b10000, 0b01000, 0b01000, 0b00100, 0b00010, 0b00010, 0b00001],
    /* ]   */ [0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110],
    /* ^   */ [0b00100, 0b01010, 0b10001, 0b00000, 0b00000, 0b00000, 0b00000],
    /* _   */ [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111],
    /* `   */ [0b01000, 0b00100, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
    /* a   */ [0b00000, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111],
    /* b   */ [0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b11110],
    /* c   */ [0b00000, 0b00000, 0b01110, 0b10000, 0b10000, 0b10001, 0b01110],
    /* d   */ [0b00001, 0b00001, 0b01111, 0b10001, 0b10001, 0b10001, 0b01111],
    /* e   */ [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110],
    /* f   */ [0b00110, 0b01001, 0b01000, 0b11110, 0b01000, 0b01000, 0b01000],
    /* g   */ [0b00000, 0b01111, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110],
    /* h   */ [0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b10001],
    /* i   */ [0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110],
    /* j   */ [0b00010, 0b00000, 0b00110, 0b00010, 0b00010, 0b10010, 0b01100],
    /* k   */ [0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010],
    /* l   */ [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    /* m   */ [0b00000, 0b00000, 0b11010, 0b10101, 0b10101, 0b10001, 0b10001],
    /* n   */ [0b00000, 0b00000, 0b11110, 0b10001, 0b10001, 0b10001, 0b10001],
    /* o   */ [0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110],
    /* p   */ [0b00000, 0b00000, 0b11110, 0b10001, 0b11110, 0b10000, 0b10000],
    /* q   */ [0b00000, 0b00000, 0b01111, 0b10001, 0b01111, 0b00001, 0b00001],
    /* r   */ [0b00000, 0b00000, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000],
    /* s   */ [0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110],
    /* t   */ [0b01000, 0b01000, 0b11110, 0b01000, 0b01000, 0b01001, 0b00110],
    /* u   */ [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10001, 0b01111],
    /* v   */ [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
    /* w   */ [0b00000, 0b00000, 0b10001, 0b10001, 0b10101, 0b10101, 0b01010],
    /* x   */ [0b00000, 0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001],
    /* y   */ [0b00000, 0b00000, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110],
    /* z   */ [0b00000, 0b00000, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111],
    /* {   */ [0b00011, 0b00100, 0b00100, 0b01000, 0b00100, 0b00100, 0b00011],
    /* |   */ [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
    /* }   */ [0b11000, 0b00100, 0b00100, 0b00010, 0b00100, 0b00100, 0b11000],
    /* ~   */ [0b00000, 0b01001, 0b10101, 0b10010, 0b00000, 0b00000, 0b00000],
    /* DEL */ [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111],
];

/// Rows of a character's glyph, substituting the box glyph for anything
/// outside printable ASCII (the overlay labels are ASCII, but detection class
/// names come from model metadata and are not guaranteed to be).
fn glyph_of(ch: char) -> &'static [u8; GLYPH_ROWS] {
    let index = match u32::from(ch) {
        code @ 0x20..=0x7E => (code - 0x20) as usize,
        // 0x7F is the box; it is the last entry.
        _ => 95,
    };
    match FONT_5X7.get(index) {
        Some(glyph) => glyph,
        None => &FONT_5X7[95],
    }
}

/// Width in pixels of `text` rendered at `scale`, including inter-glyph gaps.
fn text_width(text: &str, scale: u32) -> u32 {
    let glyphs = text.chars().count() as u32;
    if glyphs == 0 {
        return 0;
    }
    // Each glyph is GLYPH_COLS wide with a one-column gap after all but the last.
    (glyphs * (GLYPH_COLS as u32 + 1) - 1) * scale
}

/// Blit `text` into `canvas` with its top-left corner at (`x`, `y`).
///
/// Nearest-neighbour upscaling by `scale` keeps the glyphs crisp; at scale 3
/// a label is 15x21 pixels per character, which is legible against a page
/// render without the antialiasing the old rasterizer provided. Pixels that
/// fall outside the canvas are skipped rather than clamped, so a label near an
/// edge is cropped instead of smeared.
fn draw_label(canvas: &mut RgbaImage, text: &str, x: i32, y: i32, scale: u32, color: Rgba<u8>) {
    let scale = scale.max(1) as i32;
    let width = canvas.width() as i32;
    let height = canvas.height() as i32;

    let mut cursor_x = x;
    for ch in text.chars() {
        let glyph = glyph_of(ch);
        for (row_index, row) in glyph.iter().enumerate() {
            for column in 0..GLYPH_COLS {
                // Leftmost column is the most significant of the low five bits.
                if row & (1 << (GLYPH_COLS - 1 - column)) == 0 {
                    continue;
                }
                let px0 = cursor_x + column as i32 * scale;
                let py0 = y + row_index as i32 * scale;
                for dy in 0..scale {
                    let py = py0 + dy;
                    if py < 0 || py >= height {
                        continue;
                    }
                    for dx in 0..scale {
                        let px = px0 + dx;
                        if px < 0 || px >= width {
                            continue;
                        }
                        canvas.put_pixel(px as u32, py as u32, color);
                    }
                }
            }
        }
        cursor_x += (GLYPH_COLS as i32 + 1) * scale;
    }
}

fn draw_annotations(img: &RgbImage, detections: &[Detection]) -> RgbaImage {
    let (width, height) = (img.width(), img.height());
    let rgb_data = img.as_raw();
    let mut rgba_data = Vec::with_capacity(rgb_data.len() / 3 * 4);
    for chunk in rgb_data.chunks(3) {
        rgba_data.push(chunk[0]);
        rgba_data.push(chunk[1]);
        rgba_data.push(chunk[2]);
        rgba_data.push(255);
    }
    let mut canvas = RgbaImage::from_raw(width, height, rgba_data)
        .expect("RgbImage -> RgbaImage conversion failed");

    for det in detections {
        let color = CLASS_COLORS[det.class_id as usize % CLASS_COLORS.len()];
        let [x1f, y1f, x2f, y2f] = det.bbox;
        let x1 = x1f.max(0.0).min(width as f32 - 1.0).round() as i32;
        let y1 = y1f.max(0.0).min(height as f32 - 1.0).round() as i32;
        let x2 = x2f.max(0.0).min(width as f32 - 1.0).round() as i32;
        let y2 = y2f.max(0.0).min(height as f32 - 1.0).round() as i32;
        if x1 >= x2 || y1 >= y2 {
            continue;
        }

        draw_rect(&mut canvas, x1, y1, x2, y2, color);

        let class_name = crate::types::detection_label(det);
        let mut label = format!("{} ({:.0}%)", class_name, det.confidence * 100.0);
        if crate::types::category_for_class(det.class_id).is_image() {
            let verdict = crate::content_class::classify_image_region(
                img.as_raw(),
                width as usize,
                height as usize,
                det.bbox,
            );
            if verdict.is_line_art {
                label.push_str(" line-art");
            } else {
                label.push_str(" photo");
            }
        }
        // Keep the label inside the canvas: the old code clamped only `y`, so
        // a wide label on a box near the right margin ran off the image.
        let label_height = (GLYPH_ROWS as u32 * LABEL_SCALE) as i32;
        let label_y = (y1 - label_height - 2).max(0);
        let label_x = x1
            .min(width as i32 - text_width(&label, LABEL_SCALE) as i32)
            .max(0);
        draw_label(&mut canvas, &label, label_x, label_y, LABEL_SCALE, color);
    }

    canvas
}

pub fn run_layout_visualize_mode(
    pdf_path: PathBuf,
    page_range: Option<String>,
    target_height: u32,
    _config: AppConfig,
) -> Result<()> {
    let pdf_stem = pdf_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Optional model override (e.g. the PP-DocLayout artifact) for A/B checks.
    // When set, write to ./test-outputs so results are easy to find.
    let model_override = std::env::var("LEGE_LAYOUT_MODEL").ok();

    let now = chrono::Local::now();
    let date_str = now.format("%Y%m%d_%H%M%S");
    let output_dir = if model_override.is_some() {
        PathBuf::from("test-outputs")
    } else {
        pdf_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{}_layout_vis_{}", pdf_stem, date_str))
    };

    std::fs::create_dir_all(&output_dir).with_context(|| {
        format!(
            "Failed to create output directory: {}",
            output_dir.display()
        )
    })?;

    info_println!("Layout Visualize Mode");
    info_println!("  Input PDF: {}", pdf_path.display());
    info_println!("  Output: {}", output_dir.display());
    info_println!("  Target height: {}px", target_height);

    let pdf_bytes_vec = std::fs::read(&pdf_path)
        .with_context(|| format!("Failed to read PDF: {}", pdf_path.display()))?;
    let pdf_bytes: Arc<[u8]> = Arc::from(pdf_bytes_vec.into_boxed_slice());
    let mut raster_cfg = PdfRasterConfig::default();
    raster_cfg.render_forms = false;
    let renderer = PdfRenderer::new_from_bytes(pdf_bytes, raster_cfg)?;
    let total_pages = renderer.page_count() as usize;

    let pages_to_render: Vec<usize> = if let Some(range_str) = page_range {
        parse_page_range(&range_str, total_pages)?
    } else {
        (1..=total_pages).collect()
    };

    let mut pipeline_config = crate::PipelineConfig::default();
    pipeline_config
        .set_high_res_render_height(target_height)
        .map_err(|e| anyhow!("{}", e))?;

    let model_path = model_override
        .clone()
        .unwrap_or_else(|| pipeline_config.model_path().to_string());
    info_println!(
        "  Model: {}",
        if model_path.is_empty() {
            "<embedded>"
        } else {
            &model_path
        }
    );
    // Use LayoutEngine so visualize applies the same NMS + underdetection
    // correction as the production pipeline (vertical bands from DocLayout,
    // horizontal extent from OCR-det / ink).
    let mut detector = LayoutEngine::new(
        &model_path,
        LayoutEngineConfig::new(
            pipeline_config.confidence_threshold(),
            pipeline_config.nms_threshold(),
            pipeline_config.nms_threshold(),
            1,
        ),
    )?;
    let csv_path = output_dir.join("image_boxes.csv");
    let mut csv = String::from(
        "page,class,conf,x0,y0,x1,y1,verdict,cells,ink,texture,chroma,structured,flat,mean_chroma,mid\n",
    );

    info_println!(
        "Processing {} pages from PDF with {} total pages",
        pages_to_render.len(),
        total_pages
    );

    let rt = crate::runtime_stats::build_control_runtime()?;

    for &page_num in &pages_to_render {
        crate::progress::cancellation_checkpoint("before layout-visualization page")?;
        println!("  Page {}/{}  (page {})", page_num, total_pages, page_num);

        let rgb =
            rt.block_on(renderer.render_page_rgb((page_num - 1) as u32, target_height, None))?;

        let img = RgbImage::from_raw(rgb.width, rgb.height, rgb.data)
            .ok_or_else(|| anyhow!("Failed to construct image buffer for page {}", page_num))?;

        let detections: Vec<Detection> = detector
            .detect_single_blocking(&img)
            .with_context(|| format!("Layout inference failed on page {}", page_num))?;

        let output_path = output_dir.join(format!("page_{:04}.png", page_num));
        append_image_box_csv(&mut csv, page_num, &img, &detections);

        if detections.is_empty() {
            debug_println!("  Page {} — no detections, saving raw render", page_num);
            img.save(&output_path)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("Failed to save PNG: {}", output_path.display()))?;
            crate::progress::cancellation_checkpoint("after layout-visualization page")?;
            continue;
        }

        let annotated = draw_annotations(&img, &detections);
        annotated
            .save(&output_path)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("Failed to save annotated PNG: {}", output_path.display()))?;

        debug_println!(
            "  Page {} — {} detections -> {}",
            page_num,
            detections.len(),
            output_path.display()
        );
        crate::progress::cancellation_checkpoint("after layout-visualization page")?;
    }

    std::fs::write(&csv_path, csv)
        .with_context(|| format!("Failed to write {}", csv_path.display()))?;
    info_println!(
        "Layout visualization complete. {} pages saved to {}",
        pages_to_render.len(),
        output_dir.display()
    );
    println!("  Image-box CSV: {}", csv_path.display());
    println!("\nOutput: {}", output_dir.display());

    Ok(())
}

fn append_image_box_csv(
    csv: &mut String,
    page_num: usize,
    img: &RgbImage,
    detections: &[Detection],
) {
    let width = img.width() as usize;
    let height = img.height() as usize;
    for det in detections {
        if !crate::types::category_for_class(det.class_id).is_image() {
            continue;
        }
        let v = crate::content_class::classify_image_region(img.as_raw(), width, height, det.bbox);
        let verdict = if v.is_line_art { "line-art" } else { "photo" };
        println!(
            "    p{} {} {} ink={:.2} tex={:.2} flat={:.2} chroma={:.1} mid={:.2} cells={}",
            page_num,
            crate::types::detection_label(det),
            verdict,
            v.ink_share,
            v.texture_share,
            v.avg_flat,
            v.mean_chroma,
            v.mid_share,
            v.cells
        );
        csv.push_str(&format!(
            "{},{},{:.3},{:.1},{:.1},{:.1},{:.1},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.1},{:.3}\n",
            page_num,
            crate::types::detection_label(det),
            det.confidence,
            det.bbox[0],
            det.bbox[1],
            det.bbox[2],
            det.bbox[3],
            verdict,
            v.cells,
            v.ink_share,
            v.texture_share,
            v.chroma_share,
            v.structured,
            v.avg_flat,
            v.mean_chroma,
            v.mid_share
        ));
    }
}

/// Overlay DocLayout boxes on a single JPEG/PNG (or other raster) image.
pub fn run_layout_visualize_image(image_path: PathBuf, target_height: u32) -> Result<()> {
    let img = load_and_scale_raster(&image_path, Some(target_height))?;
    let output_dir = dated_debug_dir(&image_path, "layout_vis");
    std::fs::create_dir_all(&output_dir)?;
    info_println!("Layout Visualize Mode");
    info_println!("  Input image: {}", image_path.display());
    info_println!("  Output: {}", output_dir.display());
    info_println!("  Raster: {}x{}", img.width(), img.height());

    let mut detector = layout_engine_from_defaults()?;
    let output_path = output_dir.join("page_0001.png");
    write_layout_overlay(&mut detector, &img, &output_path)?;
    println!("\nOutput: {}", output_dir.display());
    Ok(())
}

/// Debug a single raster image: layout overlay plus a binarized PNG.
///
/// Height is optional. When omitted the native pixel size is used so a scan
/// can be inspected without an extra resize. `--binarization heavy` / `--heavy`
/// selects the ONNX Sauvola model.
pub fn run_image_debug_mode(
    image_path: PathBuf,
    target_height: Option<u32>,
    binarization: Option<crate::color::BinarizationConfig>,
    invert_input: bool,
    output_dir: Option<PathBuf>,
) -> Result<()> {
    let img = load_and_scale_raster(&image_path, target_height)?;
    let output_dir = output_dir.unwrap_or_else(|| dated_debug_dir(&image_path, "image_debug"));
    std::fs::create_dir_all(&output_dir)?;

    let method = if binarization
        .as_ref()
        .is_some_and(|cfg| cfg.use_heavy_duty && !cfg.use_fixed_threshold)
    {
        "heavy Sauvola (ONNX)"
    } else if binarization
        .as_ref()
        .is_some_and(|cfg| cfg.use_fixed_threshold)
    {
        "fixed threshold"
    } else {
        "adaptive Sauvola/Otsu"
    };

    info_println!("Image Debug Mode");
    info_println!("  Input: {}", image_path.display());
    info_println!("  Output: {}", output_dir.display());
    info_println!("  Raster: {}x{}", img.width(), img.height());
    info_println!("  Binarization: {method}");

    img.save(output_dir.join("original.png"))
        .map_err(anyhow::Error::msg)
        .context("failed to save original raster")?;

    let mut detector = layout_engine_from_defaults()?;
    write_layout_overlay(&mut detector, &img, &output_dir.join("layout.png"))?;

    let mut bin_cfg = binarization.unwrap_or_default();
    if invert_input {
        bin_cfg.invert_input = true;
    }
    let options = crate::color::BinarizationOptions {
        invert: bin_cfg.invert,
        invert_input: bin_cfg.invert_input,
        k_factor: bin_cfg.k_factor,
        use_heavy_duty: bin_cfg.use_heavy_duty && !bin_cfg.use_fixed_threshold,
        patch_percentage: bin_cfg.patch_percentage,
        no_patch: bin_cfg.no_patch,
        use_fixed_threshold: bin_cfg.use_fixed_threshold,
        fixed_threshold: bin_cfg.fixed_threshold,
        disable_gpu: false,
    };
    let width = img.width() as usize;
    let height = img.height() as usize;
    let binary =
        crate::color::binarization::binarize_image_raw(img.as_raw(), width, height, &options);
    let bin_img = image::GrayImage::from_raw(img.width(), img.height(), binary)
        .ok_or_else(|| anyhow!("failed to wrap binarized buffer"))?;
    bin_img
        .save(output_dir.join("binarized.png"))
        .map_err(anyhow::Error::msg)
        .context("failed to save binarized PNG")?;

    println!("\nOutput: {}", output_dir.display());
    Ok(())
}

fn dated_debug_dir(input: &std::path::Path, kind: &str) -> PathBuf {
    let stem = input
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    input
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!("{stem}_{kind}_{stamp}"))
}

fn layout_engine_from_defaults() -> Result<LayoutEngine> {
    let model_override = std::env::var("LEGE_LAYOUT_MODEL").ok();
    let pipeline_config = crate::PipelineConfig::default();
    let model_path = model_override.unwrap_or_else(|| pipeline_config.model_path().to_string());
    info_println!(
        "  Model: {}",
        if model_path.is_empty() {
            "<embedded>"
        } else {
            &model_path
        }
    );
    LayoutEngine::new(
        &model_path,
        LayoutEngineConfig::new(
            pipeline_config.confidence_threshold(),
            pipeline_config.nms_threshold(),
            pipeline_config.nms_threshold(),
            1,
        ),
    )
}

fn load_and_scale_raster(path: &std::path::Path, target_height: Option<u32>) -> Result<RgbImage> {
    let dynamic = image::open(path)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("Failed to open image: {}", path.display()))?;
    let rgb = dynamic.to_rgb8();
    let Some(target_h) = target_height.filter(|h| *h > 0 && *h != rgb.height()) else {
        return Ok(rgb);
    };
    let width = ((u64::from(rgb.width()) * u64::from(target_h)) / u64::from(rgb.height()).max(1))
        .max(1) as u32;
    Ok(crate::resize::resize_rgb_cpu(
        &rgb,
        width,
        target_h,
        crate::resize::ResizeMethod::Bilinear,
    ))
}

fn write_layout_overlay(
    detector: &mut LayoutEngine,
    img: &RgbImage,
    output_path: &std::path::Path,
) -> Result<()> {
    let detections = detector
        .detect_single_blocking(img)
        .with_context(|| format!("Layout inference failed on {}", output_path.display()))?;
    if detections.is_empty() {
        img.save(output_path)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("Failed to save PNG: {}", output_path.display()))?;
    } else {
        draw_annotations(img, &detections)
            .save(output_path)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("Failed to save annotated PNG: {}", output_path.display()))?;
    }
    info_println!(
        "  {} detections -> {}",
        detections.len(),
        output_path.display()
    );
    Ok(())
}

fn parse_page_range(range_str: &str, total_pages: usize) -> Result<Vec<usize>> {
    let mut pages = Vec::new();
    for part in range_str.split(',') {
        let part = part.trim();
        if part.contains('-') {
            let parts: Vec<&str> = part.split('-').collect();
            if parts.len() != 2 {
                return Err(anyhow!("Invalid page range: {}", part));
            }
            let start: usize = parts[0]
                .parse()
                .map_err(|_| anyhow!("Invalid page: {}", parts[0]))?;
            let end: usize = parts[1]
                .parse()
                .map_err(|_| anyhow!("Invalid page: {}", parts[1]))?;
            if start == 0 || end == 0 {
                return Err(anyhow!("Page numbers must start from 1"));
            }
            if start > end {
                return Err(anyhow!("Invalid range: {}", part));
            }
            if end > total_pages {
                return Err(anyhow!("Page {} exceeds total ({})", end, total_pages));
            }
            for p in start..=end {
                pages.push(p);
            }
        } else {
            let p: usize = part
                .parse()
                .map_err(|_| anyhow!("Invalid page: {}", part))?;
            if p == 0 || p > total_pages {
                return Err(anyhow!("Invalid page {} (1..{}).", p, total_pages));
            }
            pages.push(p);
        }
    }
    pages.sort_unstable();
    pages.dedup();
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The old rasterizer probed nine system font paths and drew nothing when
    /// none existed. The baked-in font has no such failure mode: a label always
    /// puts ink on the canvas, on any host.
    #[test]
    fn labels_render_without_any_system_font() {
        let mut canvas = RgbaImage::from_pixel(400, 40, Rgba([255, 255, 255, 255]));
        draw_label(&mut canvas, "Table (93%)", 4, 4, LABEL_SCALE, Rgba([255, 0, 0, 255]));
        let inked = canvas.pixels().filter(|p| p.0[0..3] != [255, 255, 255]).count();
        assert!(inked > 0, "label drew no pixels");
    }

    #[test]
    fn every_printable_ascii_has_a_distinct_glyph_slot() {
        assert_eq!(FONT_5X7.len(), 96);
        // Space is blank; nothing else printable may be, or that character
        // would silently disappear from a label.
        for code in 0x21u32..=0x7E {
            let ch = char::from_u32(code).unwrap_or(' ');
            let glyph = glyph_of(ch);
            assert!(glyph.iter().any(|row| *row != 0), "{ch:?} rasterizes blank");
            // Only the low five bits are meaningful.
            assert!(glyph.iter().all(|row| *row < 0b100000), "{ch:?} overflows");
        }
        assert!(glyph_of(' ').iter().all(|row| *row == 0));
    }

    #[test]
    fn out_of_range_characters_fall_back_to_the_box_glyph() {
        // Class names come from model metadata and are not guaranteed ASCII.
        assert_eq!(glyph_of('\u{4e2d}'), glyph_of('\u{7f}'));
        assert_eq!(glyph_of('\u{1}'), glyph_of('\u{7f}'));
    }

    #[test]
    fn text_width_matches_what_draw_label_advances() {
        assert_eq!(text_width("", 3), 0);
        // One glyph is GLYPH_COLS wide with no trailing gap.
        assert_eq!(text_width("A", 1), GLYPH_COLS as u32);
        // Each further glyph adds a one-column gap plus its own width.
        assert_eq!(text_width("AB", 1), GLYPH_COLS as u32 * 2 + 1);
        assert_eq!(text_width("AB", 3), (GLYPH_COLS as u32 * 2 + 1) * 3);
    }

    #[test]
    fn drawing_outside_the_canvas_is_clipped_not_wrapped() {
        let mut canvas = RgbaImage::from_pixel(8, 8, Rgba([255, 255, 255, 255]));
        // Far off every edge: must not panic and must not touch a pixel.
        draw_label(&mut canvas, "Wide label", -400, -400, 3, Rgba([0, 0, 0, 255]));
        draw_label(&mut canvas, "Wide label", 400, 400, 3, Rgba([0, 0, 0, 255]));
        assert!(canvas.pixels().all(|p| p.0 == [255, 255, 255, 255]));
    }
}
