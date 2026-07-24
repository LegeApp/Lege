use crate::engine::Detection;
use crate::pagerender::prelude::{PdfRenderer, RasterConfig as PdfRasterConfig};
use crate::types::{AppConfig, ContentCategory};
use crate::{debug_println, info_println};
use anyhow::{Context, Result, anyhow};
use image::{RgbImage, Rgba, RgbaImage};
use lege_gpu::vision::{LayoutConfig, LayoutDetector};
use std::path::PathBuf;
use std::sync::Arc;

/// Collapse a layout label (YOLO or PP-DocLayout vocabulary) into the four
/// pipeline categories, by name. Used only for the visualization tint; the
/// detector's own `class_name` drives the drawn label.
fn category_for_name(name: &str) -> ContentCategory {
    match name {
        "figure" | "image" | "chart" | "seal" | "header_image" | "footer_image" => {
            ContentCategory::Image
        }
        "table" => ContentCategory::Table,
        "abandon" | "header" | "footer" | "number" | "page_number" => ContentCategory::Abandon,
        _ => ContentCategory::Text,
    }
}

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

fn draw_label(
    canvas: &mut RgbaImage,
    font: &fontdue::Font,
    text: &str,
    x: i32,
    y: i32,
    color: Rgba<u8>,
) {
    let px_size = 16.0;
    let w = canvas.width() as i32;
    let h = canvas.height() as i32;

    let mut cursor_x = x as f32;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, px_size);
        let draw_x = (cursor_x + metrics.xmin as f32).round() as i32;
        let draw_y = (y as f32 + metrics.ymin as f32).round() as i32;

        for by in 0..metrics.height {
            for bx in 0..metrics.width {
                let coverage = bitmap[by * metrics.width + bx];
                if coverage == 0 {
                    continue;
                }
                let px = draw_x + bx as i32;
                let py = draw_y + by as i32;
                if px < 0 || py < 0 || px >= w || py >= h {
                    continue;
                }
                let t = coverage as f32 / 255.0;
                let pixel = canvas.get_pixel_mut(px as u32, py as u32);
                let r = (color.0[0] as f32 * t + pixel.0[0] as f32 * (1.0 - t)) as u8;
                let g = (color.0[1] as f32 * t + pixel.0[1] as f32 * (1.0 - t)) as u8;
                let b = (color.0[2] as f32 * t + pixel.0[2] as f32 * (1.0 - t)) as u8;
                *pixel = Rgba([r, g, b, 255]);
            }
        }
        cursor_x += metrics.advance_width;
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

    let font = load_font();

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

        if let Some(ref f) = font {
            let class_name = crate::types::detection_label(det);
            let label = format!("{} ({:.0}%)", class_name, det.confidence * 100.0);
            let label_y = y1.saturating_sub(22).max(0);
            draw_label(&mut canvas, f, &label, x1, label_y, color);
        }
    }

    canvas
}

fn load_font() -> Option<fontdue::Font> {
    let candidates = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    ];
    for path in &candidates {
        if let Ok(data) = std::fs::read(path) {
            if let Ok(font) = fontdue::Font::from_bytes(data, fontdue::FontSettings::default()) {
                return Some(font);
            }
        }
    }
    None
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
    let layout_config = LayoutConfig {
        model_path: model_path.clone().into(),
        confidence_threshold: pipeline_config.confidence_threshold(),
        iou_threshold: pipeline_config.nms_threshold(),
        max_detections: 300,
    };
    let detector = if model_path.is_empty() {
        LayoutDetector::from_model_bytes(crate::EMBEDDED_LAYOUT_MODEL, layout_config)?
    } else {
        LayoutDetector::new(layout_config)?
    };

    info_println!(
        "Processing {} pages from PDF with {} total pages",
        pages_to_render.len(),
        total_pages
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(crate::runtime_stats::MAX_BLOCKING_THREADS)
        .enable_all()
        .build()?;

    for &page_num in &pages_to_render {
        println!("  Page {}/{}  (page {})", page_num, total_pages, page_num);

        let rgb =
            rt.block_on(renderer.render_page_rgb((page_num - 1) as u32, target_height, None))?;

        let img = RgbImage::from_raw(rgb.width, rgb.height, rgb.data)
            .ok_or_else(|| anyhow!("Failed to construct image buffer for page {}", page_num))?;

        let detections: Vec<Detection> = detector
            .detect_rgb(&img)
            .with_context(|| format!("Layout inference failed on page {}", page_num))?
            .into_iter()
            .map(|d| Detection {
                class_id: d.class_id,
                class_name: Some(d.class_name.to_string()),
                confidence: d.confidence,
                bbox: d.bbox,
                category: category_for_name(d.class_name),
                context: None,
            })
            .collect();

        let output_path = output_dir.join(format!("page_{:04}.png", page_num));

        if detections.is_empty() {
            debug_println!("  Page {} — no detections, saving raw render", page_num);
            img.save(&output_path)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("Failed to save PNG: {}", output_path.display()))?;
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
    }

    info_println!(
        "Layout visualization complete. {} pages saved to {}",
        pages_to_render.len(),
        output_dir.display()
    );
    println!("\nOutput: {}", output_dir.display());

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
