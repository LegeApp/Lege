use crate::pagerender::prelude::{PdfiumRenderer, RasterConfig as PdfRasterConfig};
use crate::{debug_println, error_println, info_println};
use anyhow::{Context, Result, anyhow};
use image::RgbImage;
use std::sync::Arc;
use tokio::runtime::Runtime;

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
    let renderer = PdfiumRenderer::new_from_bytes(pdf_bytes, raster_cfg)?;
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
    for (i, page_num) in pages_to_render.iter().enumerate() {
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

/// Render a single PDF page to PNG at specified height via PdfiumRenderer
fn render_pdf_page_to_png(
    renderer: &PdfiumRenderer,
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
        let rt = Runtime::new()?;
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

/// Render PDF pages and encode each as JP2 (RGB and grayscale), logging file sizes.
///
/// Usage: `lege <file.pdf> [page-range] --jp2-debug HEIGHT`
///
/// Outputs per page:
/// - `page_NNNN_q80.jp2`      — RGB JP2 at quality 80
/// - `page_NNNN_q62.jp2`      — RGB JP2 at quality 62 (WebHigh equivalent)
/// - `page_NNNN_q42.jp2`      — RGB JP2 at quality 42 (WebLow equivalent)
/// - `page_NNNN_gray_q80.jp2` — Grayscale JP2 at quality 80
pub fn run_pdf_to_jp2_debug_mode(
    pdf_path: std::path::PathBuf,
    page_range: Option<String>,
    target_height: u32,
    _config: crate::types::AppConfig,
) -> Result<()> {
    use crate::encoding::{
        EncodingManager, EncodingResult, EncodingSettings, ImageBuffer as LegeImageBuffer,
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
    let renderer =
        crate::pagerender::prelude::PdfiumRenderer::new_from_bytes(pdf_bytes, raster_cfg)?;
    let total_pages = renderer.page_count() as usize;

    let pages_to_render = if let Some(range_str) = page_range {
        parse_page_range(&range_str, total_pages)?
    } else {
        (1..=(total_pages)).collect()
    };

    println!(
        "Encoding {} pages as JP2 (RGB + gray, 3 quality levels each)…",
        pages_to_render.len()
    );
    println!(
        "{:<28} {:>10} {:>10} {:>10} {:>10}",
        "page", "rgb_q80", "rgb_q62", "rgb_q42", "gray_q80"
    );
    println!("{}", "-".repeat(62));

    let overall_start = std::time::Instant::now();
    for &page_num in &pages_to_render {
        let page_start = std::time::Instant::now();

        let rgb = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                handle.block_on(renderer.render_page_rgb(
                    (page_num - 1) as u32,
                    target_height,
                    None,
                ))
            })?
        } else {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(renderer.render_page_rgb((page_num - 1) as u32, target_height, None))?
        };

        let w = rgb.width;
        let h = rgb.height;
        let rgb_data = rgb.data;

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

        let encode_jp2 = |data: &[u8], channels: u8, q: u8| -> anyhow::Result<Vec<u8>> {
            let buf = LegeImageBuffer {
                data,
                width: w,
                height: h,
                channels,
            };
            let result = EncodingManager::encode(&buf, &EncodingSettings::Jp2Lam { quality: q })
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            match result {
                EncodingResult::Standard(d) => Ok(d),
                _ => Err(anyhow::anyhow!("unexpected encoding result type")),
            }
        };

        let rgb80 = encode_jp2(&rgb_data, 3, 80)?;
        let rgb62 = encode_jp2(&rgb_data, 3, 62)?;
        let rgb42 = encode_jp2(&rgb_data, 3, 42)?;
        let gray80 = encode_jp2(&gray_data, 1, 80)?;

        let stem = format!("page_{:04}", page_num);
        fs::write(output_dir.join(format!("{stem}_q80.jp2")), &rgb80)?;
        fs::write(output_dir.join(format!("{stem}_q62.jp2")), &rgb62)?;
        fs::write(output_dir.join(format!("{stem}_q42.jp2")), &rgb42)?;
        fs::write(output_dir.join(format!("{stem}_gray_q80.jp2")), &gray80)?;

        let elapsed_ms = page_start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{:<28} {:>10} {:>10} {:>10} {:>10}   ({:.0}ms)",
            format!("page {}", page_num),
            fmt_bytes(rgb80.len()),
            fmt_bytes(rgb62.len()),
            fmt_bytes(rgb42.len()),
            fmt_bytes(gray80.len()),
            elapsed_ms,
        );
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

fn fmt_bytes(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1}M", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.0}K", n as f64 / 1024.0)
    } else {
        format!("{}B", n)
    }
}
