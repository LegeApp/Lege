use crate::pagerender::prelude::{PdfRenderer, RasterConfig as PdfRasterConfig};
use crate::{debug_println, error_println, info_println};
use anyhow::{Context, Result, anyhow};
use image::RgbImage;
use std::sync::Arc;

/// Render PDF pages to PNG files at specified height
pub fn run_pdf_to_png_mode(
    pdf_path: std::path::PathBuf,
    page_range: Option<String>,
    target_height: u32,
    _config: crate::types::AppConfig,
    png_quantize: bool,
    png_colors: u16,
    output_dir: Option<std::path::PathBuf>,
) -> Result<()> {
    // Simple prints for progress in this utility mode
    use std::fs;

    // Honor an explicit --output directory; otherwise default to `<pdf>_png_output/`
    // next to the input PDF.
    let output_dir = output_dir.unwrap_or_else(|| {
        pdf_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!(
                "{}_png_output",
                pdf_path.file_stem().unwrap().to_string_lossy()
            ))
    });

    fs::create_dir_all(&output_dir)?;

    info_println!("PDF to PNG Mode");
    info_println!("Input PDF: {}", pdf_path.display());
    info_println!("Output folder: {}", output_dir.display());
    info_println!("Target height: {}px", target_height);

    // Read PDF bytes and initialize renderer
    let pdf_bytes_vec = fs::read(&pdf_path)
        .with_context(|| format!("Failed to read PDF: {}", pdf_path.display()))?;
    let pdf_bytes: Arc<[u8]> = Arc::from(pdf_bytes_vec.into_boxed_slice());
    let mut raster_cfg = PdfRasterConfig::default();
    raster_cfg.render_forms = false;
    let renderer = PdfRenderer::new_from_bytes(pdf_bytes, raster_cfg)?;
    let total_pages = renderer.page_count() as usize;

    // Parse page range or use all pages
    let pages_to_render = if let Some(range_str) = page_range {
        parse_page_range(&range_str, total_pages as usize)?
    } else {
        (1..=(total_pages as usize)).collect()
    };

    info_println!(
        "Rendering {} pages from PDF with {} total pages",
        pages_to_render.len(),
        total_pages
    );
    let overall_start = std::time::Instant::now();
    println!("Rendering {} pages...", pages_to_render.len());

    // Render each page
    for page_num in pages_to_render.iter() {
        crate::progress::cancellation_checkpoint("before PDF-to-PNG page")?;
        let page_start = std::time::Instant::now();

        match render_pdf_page_to_png(
            &renderer,
            (*page_num - 1) as u32,
            target_height,
            &output_dir,
            png_quantize,
            png_colors,
        ) {
            Ok(output_path) => {
                let elapsed = page_start.elapsed().as_secs_f64() * 1000.0;
                info_println!(
                    "Page {} rendered in {:.2} ms -> {}",
                    page_num,
                    elapsed,
                    output_path.display()
                );
            }
            Err(e) => {
                error_println!("Failed to render page {}: {}", page_num, e);
            }
        }
        crate::progress::cancellation_checkpoint("after PDF-to-PNG page")?;
    }

    println!("PDF rendering complete");
    let total_elapsed = overall_start.elapsed().as_secs_f64();
    let throughput = if total_elapsed > 0.0 {
        pages_to_render.len() as f64 / total_elapsed
    } else {
        0.0
    };
    info_println!(
        "PDF rendering completed in {:.2} s ({:.2} pages/s). {} PNG files saved to {}",
        total_elapsed,
        throughput,
        pages_to_render.len(),
        output_dir.display()
    );

    Ok(())
}

/// Render a single PDF page to PNG at the specified height.
fn render_pdf_page_to_png(
    renderer: &PdfRenderer,
    page_index: u32,
    target_height: u32,
    output_dir: &std::path::Path,
    png_quantize: bool,
    png_colors: u16,
) -> Result<std::path::PathBuf> {
    let rgb = if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| {
            handle.block_on(renderer.render_page_rgb(page_index, target_height, None))
        })?
    } else {
        let rt = crate::runtime_stats::build_control_runtime()?;
        rt.block_on(renderer.render_page_rgb(page_index, target_height, None))?
    };
    let img = RgbImage::from_raw(rgb.width, rgb.height, rgb.data).ok_or_else(|| {
        anyhow!(
            "Failed to construct image buffer for page {}",
            page_index + 1
        )
    })?;

    debug_println!(
        "Page {}: rendered to {}x{} px",
        page_index as usize + 1,
        img.width(),
        img.height()
    );

    // Save as PNG
    let output_filename = format!("page_{:04}.png", page_index as usize + 1);
    let output_path = output_dir.join(output_filename);

    if png_quantize {
        crate::colorquant::write_quantized_rgb_png(
            &img,
            &output_path,
            crate::colorquant::PngQuantizationOptions { colors: png_colors },
        )
        .with_context(|| format!("Failed to save quantized PNG: {}", output_path.display()))?;
    } else {
        img.save(&output_path)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("Failed to save PNG: {}", output_path.display()))?;
    }

    Ok(output_path)
}

/// Parse page range string into vector of page numbers
fn parse_page_range(range_str: &str, total_pages: usize) -> Result<Vec<usize>> {
    let mut pages = Vec::new();

    for part in range_str.split(',') {
        let part = part.trim();
        if part.contains('-') {
            // Range like "1-5"
            let range_parts: Vec<&str> = part.split('-').collect();
            if range_parts.len() != 2 {
                return Err(anyhow!("Invalid page range format: {}", part));
            }

            let start: usize = range_parts[0]
                .parse()
                .map_err(|_| anyhow!("Invalid page number: {}", range_parts[0]))?;
            let end: usize = range_parts[1]
                .parse()
                .map_err(|_| anyhow!("Invalid page number: {}", range_parts[1]))?;

            if start == 0 || end == 0 {
                return Err(anyhow!("Page numbers must start from 1"));
            }

            if start > end {
                return Err(anyhow!(
                    "Invalid range: start page {} is greater than end page {}",
                    start,
                    end
                ));
            }

            if end > total_pages {
                return Err(anyhow!(
                    "Page {} exceeds total pages ({})",
                    end,
                    total_pages
                ));
            }

            for page_num in start..=end {
                pages.push(page_num);
            }
        } else {
            // Single page like "3"
            let page_num: usize = part
                .parse()
                .map_err(|_| anyhow!("Invalid page number: {}", part))?;

            if page_num == 0 {
                return Err(anyhow!("Page numbers must start from 1"));
            }

            if page_num > total_pages {
                return Err(anyhow!(
                    "Page {} exceeds total pages ({})",
                    page_num,
                    total_pages
                ));
            }

            pages.push(page_num);
        }
    }

    // Remove duplicates and sort
    pages.sort_unstable();
    pages.dedup();

    Ok(pages)
}

/// Render PDF pages and encode each as JP2 twice — the legacy open-loop preset
/// and the verified display floor it maps to — logging bytes, encode time and
/// the reader-visible SSIMULACRA2 score of both.
///
/// Usage: `lege <file.pdf> [page-range] --jp2-debug HEIGHT`
///
/// `HEIGHT` is the *source* render height. The display box is the saved
/// resolution preset, or 1200px tall when none is saved, so rendering above it
/// exercises the pre-resize (the emitted JP2 then carries the box dimensions).
///
/// Outputs per page, for tiers (preset 80 → floor 75), (62 → 70), (42 → 65):
/// - `page_NNNN_q{80,62,42}.jp2`      — legacy RGB preset
/// - `page_NNNN_f{75,70,65}.jp2`      — verified RGB display floor
/// - `page_NNNN_gray_q80.jp2` / `_gray_f75.jp2`
pub fn run_pdf_to_jp2_debug_mode(
    pdf_path: std::path::PathBuf,
    page_range: Option<String>,
    target_height: u32,
    _config: crate::types::AppConfig,
) -> Result<()> {
    use crate::encoding::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
        Jp2DisplaySettings,
    };
    use std::fs;

    let output_dir = pdf_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!(
            "{}_jp2_debug",
            pdf_path.file_stem().unwrap().to_string_lossy()
        ));
    fs::create_dir_all(&output_dir)?;

    info_println!("JP2 Debug Mode");
    info_println!("Input PDF: {}", pdf_path.display());
    info_println!("Output folder: {}", output_dir.display());
    info_println!("Target height: {}px", target_height);

    let pdf_bytes_vec = fs::read(&pdf_path)
        .with_context(|| format!("Failed to read PDF: {}", pdf_path.display()))?;
    let pdf_bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(pdf_bytes_vec.into_boxed_slice());
    let mut raster_cfg = crate::pagerender::prelude::RasterConfig::default();
    raster_cfg.render_forms = false;
    let renderer = crate::pagerender::prelude::PdfRenderer::new_from_bytes(pdf_bytes, raster_cfg)?;
    let total_pages = renderer.page_count() as usize;

    let pages_to_render = if let Some(range_str) = page_range {
        parse_page_range(&range_str, total_pages)?
    } else {
        (1..=(total_pages)).collect()
    };

    // Where the reader actually sees the page. The saved device preset when
    // there is one, else a 1200px-tall e-reader panel.
    let (box_h, box_w_fixed) = match crate::resolution_preset::load().ok().flatten() {
        Some(preset) => (preset.height.max(1), preset.width),
        None => (1200, None),
    };

    println!(
        "Encoding {} pages as JP2: legacy preset vs verified display floor (box height {}px)…",
        pages_to_render.len(),
        box_h
    );
    println!(
        "{:<7} {:<11} {:>4} {:>8} {:>7} {:>6}   {:>5} {:>8} {:>7} {:>6}  {}",
        "page", "source", "q", "bytes", "ms", "s2", "floor", "bytes", "ms", "s2", "emitted"
    );
    println!("{}", "-".repeat(96));

    let overall_start = std::time::Instant::now();
    for &page_num in &pages_to_render {
        crate::progress::cancellation_checkpoint("before PDF-to-JP2 debug page")?;

        let rgb = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                handle.block_on(renderer.render_page_rgb(
                    (page_num - 1) as u32,
                    target_height,
                    None,
                ))
            })?
        } else {
            let rt = crate::runtime_stats::build_control_runtime()?;
            rt.block_on(renderer.render_page_rgb((page_num - 1) as u32, target_height, None))?
        };

        let w = rgb.width;
        let h = rgb.height;
        let rgb_data = rgb.data;
        let box_w = box_w_fixed
            .unwrap_or_else(|| ((w as f64 * box_h as f64 / h as f64).round() as u32).max(1));

        // Build grayscale
        let gray_data: Vec<u8> = rgb_data
            .chunks_exact(3)
            .map(|px| {
                let r = px[0] as f32;
                let g = px[1] as f32;
                let b = px[2] as f32;
                (0.299 * r + 0.587 * g + 0.114 * b).round() as u8
            })
            .collect();

        let encode_timed = |settings: &EncodingSettings,
                            data: &[u8],
                            channels: u8|
         -> anyhow::Result<(Vec<u8>, f64)> {
            let buf = LegeImageBuffer {
                data,
                width: w,
                height: h,
                channels,
            };
            let started = std::time::Instant::now();
            let result =
                EncodingManager::encode(&buf, settings).map_err(|e| anyhow::anyhow!("{}", e))?;
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            match result {
                EncodingResult::Standard(d) => Ok((d, ms)),
                _ => Err(anyhow::anyhow!("unexpected encoding result type")),
            }
        };

        let stem = format!("page_{:04}", page_num);
        // (channels, source, legacy preset, display floor)
        let tiers: [(u8, &[u8], u8, u8); 4] = [
            (3, &rgb_data, 80, 75),
            (3, &rgb_data, 62, 70),
            (3, &rgb_data, 42, 65),
            (1, &gray_data, 80, 75),
        ];

        for (channels, source, quality, floor) in tiers {
            crate::progress::cancellation_checkpoint("during PDF-to-JP2 debug tier")?;
            let (legacy, legacy_ms) =
                encode_timed(&EncodingSettings::Jp2Lam { quality }, source, channels)?;
            let display_settings = EncodingSettings::Jp2Display(Jp2DisplaySettings {
                max_width: box_w,
                max_height: box_h,
                floor,
                fallback_quality: quality,
            });
            let (display, display_ms) = encode_timed(&display_settings, source, channels)?;
            let (emitted_w, emitted_h) = display_dimensions(&display, w, h);

            let tag = if channels == 1 { "gray_" } else { "" };
            fs::write(
                output_dir.join(format!("{stem}_{tag}q{quality}.jp2")),
                &legacy,
            )?;
            fs::write(
                output_dir.join(format!("{stem}_{tag}f{floor}.jp2")),
                &display,
            )?;

            let (legacy_s2, display_s2) =
                display_scores(source, w, h, channels, box_w, box_h, &legacy, &display);

            println!(
                "{:<7} {:<11} {:>4} {:>8} {:>7.0} {:>6} {:>7} {:>8} {:>7.0} {:>6}  {}x{}",
                format!("p{}{}", page_num, if channels == 1 { "g" } else { "" }),
                format!("{}x{}", w, h),
                format!("q{}", quality),
                fmt_bytes(legacy.len()),
                legacy_ms,
                legacy_s2,
                format!("f{}", floor),
                fmt_bytes(display.len()),
                display_ms,
                display_s2,
                emitted_w,
                emitted_h,
            );
        }
        crate::progress::cancellation_checkpoint("after PDF-to-JP2 debug page")?;
    }

    let total_s = overall_start.elapsed().as_secs_f64();
    println!(
        "\nDone — {} pages in {:.2}s → {}",
        pages_to_render.len(),
        total_s,
        output_dir.display()
    );
    Ok(())
}

/// Pixel dimensions of an emitted JP2, falling back to the source size.
fn display_dimensions(data: &[u8], width: u32, height: u32) -> (u32, u32) {
    crate::encoding::jp2::jp2_dimensions(data).unwrap_or((width, height))
}

/// SSIMULACRA2 of both streams measured where the reader sees them: source and
/// candidate downscaled into the display box (grayscale sources folded to e-ink
/// luminance). Returns `("n/a", ..)` if the metric cannot run.
#[cfg(feature = "jp2-lam")]
fn display_scores(
    source: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    box_w: u32,
    box_h: u32,
    legacy: &[u8],
    display: &[u8],
) -> (String, String) {
    let profile = if channels == 1 {
        jp2lam::DisplayProfile::eink(box_w, box_h)
    } else {
        jp2lam::DisplayProfile::tablet(box_w, box_h)
    };
    let view = if channels == 1 {
        jp2lam::ImageView::from_gray8(width, height, source)
    } else {
        jp2lam::ImageView::from_rgb8_interleaved(width, height, source)
    };
    let Ok(view) = view else {
        return ("n/a".into(), "n/a".into());
    };
    let Ok(mut evaluator) = jp2lam::StreamEvaluator::for_display(view, profile) else {
        return ("n/a".into(), "n/a".into());
    };
    let score = |eval: &mut jp2lam::StreamEvaluator, bytes: &[u8]| {
        eval.score_stream(bytes)
            .map(|o| format!("{:.1}", o.score))
            .unwrap_or_else(|_| "err".to_string())
    };
    let a = score(&mut evaluator, legacy);
    let b = score(&mut evaluator, display);
    (a, b)
}

#[cfg(not(feature = "jp2-lam"))]
fn display_scores(
    _source: &[u8],
    _width: u32,
    _height: u32,
    _channels: u8,
    _box_w: u32,
    _box_h: u32,
    _legacy: &[u8],
    _display: &[u8],
) -> (String, String) {
    ("n/a".into(), "n/a".into())
}

fn fmt_bytes(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1}M", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.0}K", n as f64 / 1024.0)
    } else {
        format!("{}B", n)
    }
}
