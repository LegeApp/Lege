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
        r"C:\Windows\Fonts\arial.ttf",
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\calibri.ttf",
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
    Ok(image::imageops::resize(
        &rgb,
        width,
        target_h,
        image::imageops::FilterType::Triangle,
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
