use crate::{
    EncodeOptions, ImageFormat, PipelineConfig,
    debug_println, encode_jpeg,
    engine::{Detection, PaddleXConfig, PaddleXEngine},
    error_println, info_println,
    pagerender::prelude::{PdfiumRenderer, RasterConfig as PdfRasterConfig},
    pipeline::encode_page_data,
    pipeline::helper_functions::{init_encode_semaphore, wait_for_memory_relief},
    pipeline::runtime_limits::{AdaptiveConcurrency, PipelineRuntimeLimits},
    prepare_shared_deskew_engine,
    progress::{ProgressMode, ProgressTracker},
};
use anyhow::{Context, Result, anyhow};
use futures::stream::{FuturesUnordered, StreamExt};
use image::RgbImage;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::types::AppConfig;

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "ppm", "pbm", "pgm", "pnm", "tiff", "tif", "bmp",
];

pub fn run_png_mode(
    folder: PathBuf,
    output: Option<PathBuf>,
    _config: AppConfig,
    enable_deskew: bool,
) -> Result<()> {
    let mut pipeline_config = PipelineConfig::default();
    pipeline_config.set_enable_layout_detection(true);
    pipeline_config.set_enable_deskew(enable_deskew);
    pipeline_config.set_high_res_render_height(pipeline_config.target_height())?;

    run_png_mode_with_config(folder, output, pipeline_config, None)
}

pub fn run_png_mode_with_config(
    folder: PathBuf,
    output: Option<PathBuf>,
    mut pipeline_config: PipelineConfig,
    progress_tracker: Option<ProgressTracker>,
) -> Result<()> {
    if !folder.is_dir() {
        return Err(anyhow!("Path is not a directory: {}", folder.display()));
    }

    pipeline_config.set_high_res_render_height(pipeline_config.target_height())?;
    let output_path = output.unwrap_or_else(|| {
        if pipeline_config.text_format() == "djvu" {
            folder.join("output.djvu")
        } else {
            folder.join("output.pdf")
        }
    });
    let parent = output_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;

    info_println!(
        "Image Folder Mode\n  Input: {}\n  Output: {}\n  Layout: {}\n  Deskew: {}",
        folder.display(),
        output_path.display(),
        if pipeline_config.enable_layout_detection() {
            "ENABLED"
        } else {
            "disabled"
        },
        if pipeline_config.enable_deskew() { "ENABLED" } else { "disabled" }
    );

    let image_files = collect_supported_images(&folder)?;
    if image_files.is_empty() {
        return Err(anyhow!(
            "No supported image files found in {}",
            folder.display()
        ));
    }

    info_println!("Found {} image files", image_files.len());

    if let Some(tracker) = progress_tracker.as_ref() {
        tracker.set_total_pages(image_files.len());
        tracker.set_mode_enum(if pipeline_config.enable_layout_detection() {
            ProgressMode::Layout
        } else {
            ProgressMode::NoLayout
        });
        tracker.set_output_is_djvu(pipeline_config.text_format() == "djvu");
        tracker.set_feature_flags(
            pipeline_config.enable_layout_detection(),
            pipeline_config.enable_deskew(),
        );
        if pipeline_config.enable_layout_detection() {
            tracker.publish_layout_progress(0, 0, 0, 0, image_files.len());
        } else {
            tracker.publish_no_layout_progress(0, 0, image_files.len());
        }
    }

    let _limits = PipelineRuntimeLimits::from_config(&pipeline_config);
    // Pick concurrency caps from detected CPU cores and total RAM so the
    // pipeline adapts to the host (a 4-core / 8 GB laptop and a 24-core /
    // 64 GB workstation should both run well, with neither swapping nor
    // leaving cores idle). See AdaptiveConcurrency::from_specs for the rules.
    let adaptive = AdaptiveConcurrency::detect();
    let io_workers = adaptive.io_workers;
    let cpu_workers = adaptive.cpu_workers;
    let workers = io_workers;
    info_println!(
        "Image folder pipeline tuned for this host: io_workers={}, cpu_workers={}",
        io_workers,
        cpu_workers
    );
    init_encode_semaphore(cpu_workers);
    let deskew_engine = prepare_shared_deskew_engine(&pipeline_config)?;
    let owned_runtime;
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => {
            // We are not inside a runtime; create a new one.
            owned_runtime = tokio::runtime::Runtime::new()?;
            owned_runtime.handle().clone()
        }
    };

    if pipeline_config.text_format() == "djvu" {
        let tracker = progress_tracker
            .clone()
            .unwrap_or_else(|| crate::progress::get_progress_manager().create_tracker());
        runtime.block_on(process_image_folder_to_djvu(
            image_files,
            output_path,
            pipeline_config,
            deskew_engine,
            tracker,
            workers,
            cpu_workers,
        ))?;
        info_println!("Image processing complete");
        return Ok(());
    }

    let tracker = progress_tracker
        .clone()
        .unwrap_or_else(|| crate::progress::get_progress_manager().create_tracker());
    runtime.block_on(process_image_folder_to_pdf(
        image_files,
        output_path,
        pipeline_config,
        deskew_engine,
        tracker,
        workers,
        cpu_workers,
    ))?;
    info_println!("Image processing complete");
    Ok(())
}

async fn process_image_folder_to_pdf(
    image_files: Vec<PathBuf>,
    output_path: PathBuf,
    pipeline_config: PipelineConfig,
    deskew_engine: Option<std::sync::Arc<crate::deskew::DeskewEngine>>,
    progress_tracker: ProgressTracker,
    workers: usize,
    cpu_workers: usize,
) -> Result<()> {
    if output_path.is_dir() {
        return Err(anyhow!(
            "PDF output path is an existing directory, not a file: {}",
            output_path.display()
        ));
    }

    let total = image_files.len();
    let config = Arc::new(pipeline_config);
    let cpu_semaphore = Arc::new(tokio::sync::Semaphore::new(cpu_workers.max(1)));
    let (writer, writer_task) = crate::pipeline::helper_functions::spawn_pdf_writer_actor(
        output_path,
        total,
        progress_tracker.clone(),
        false,
        workers,
    );

    let mut in_flight: FuturesUnordered<tokio::task::JoinHandle<Result<(usize, crate::accumulator::Page)>>> =
        FuturesUnordered::new();
    let mut next_input = 0usize;
    let mut processed = 0usize;
    let mut deskewed = 0usize;

    while next_input < total || !in_flight.is_empty() {
        while next_input < total && in_flight.len() < workers {
            let image_path = image_files[next_input].clone();
            let config = config.clone();
            let deskew_engine = deskew_engine.clone();
            let cpu_sem = cpu_semaphore.clone();
            let index = next_input;
            next_input += 1;

            in_flight.push(tokio::spawn(async move {
                wait_for_memory_relief().await;
                process_single_image_to_pdf_page(index, image_path, config, deskew_engine, cpu_sem)
                    .await
                    .map(|page| (index, page))
            }));
        }

        let Some(result) = in_flight.next().await else {
            break;
        };
        let (index, page) = result.map_err(|e| anyhow!("PDF image worker panicked: {}", e))??;
        writer.send_page(page, index).await?;

        processed += 1;
        if config.enable_deskew() {
            deskewed += 1;
        }
        if config.enable_layout_detection() {
            progress_tracker.publish_layout_progress(processed, processed, processed, deskewed, total);
        } else {
            progress_tracker.publish_no_layout_progress(processed, deskewed, total);
        }
    }

    writer.finalize().await?;
    writer_task
        .await
        .map_err(|e| anyhow!("PDF writer task failed: {}", e))??;
    Ok(())
}

async fn process_single_image_to_pdf_page(
    index: usize,
    image_path: PathBuf,
    pipeline_config: Arc<PipelineConfig>,
    deskew_engine: Option<std::sync::Arc<crate::deskew::DeskewEngine>>,
    cpu_sem: Arc<tokio::sync::Semaphore>,
) -> Result<crate::accumulator::Page> {
    // Stage 1: Decode the PNG. Single-threaded per page, so we let many of
    // these run in parallel — this is the only stage we want wide.
    let path_for_decode = image_path.clone();
    let rgb_image = tokio::task::spawn_blocking(move || {
        decode_folder_image(&path_for_decode)
    })
    .await
    .map_err(|e| anyhow!("PNG decode panicked: {}", e))??;

    // Stage 2: Deskew + binarize. Both lean heavily on rayon internally, so we
    // gate this with a CPU semaphore — running too many in parallel just makes
    // them fight over the same global rayon pool.
    let permit = cpu_sem
        .acquire_owned()
        .await
        .map_err(|e| anyhow!("CPU semaphore closed: {}", e))?;
    let config_for_blocking = pipeline_config.clone();
    let deskew_for_blocking = deskew_engine.clone();
    let path_for_log = image_path.clone();
    let (rgb_image, binarized) = tokio::task::spawn_blocking(move || {
        deskew_and_binarize(
            rgb_image,
            &path_for_log,
            config_for_blocking.as_ref(),
            deskew_for_blocking,
        )
    })
    .await
    .map_err(|e| anyhow!("PDF image preparation panicked: {}", e))??;
    drop(permit);

    let (width, height) = (rgb_image.width() as usize, rgb_image.height() as usize);
    let base_layer = encode_page_data(&binarized, width, height, index, pipeline_config.as_ref()).await?;

    Ok(crate::accumulator::Page {
        width: width as f32,
        height: height as f32,
        elements: vec![crate::accumulator::ContentElement {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            content: base_layer,
        }],
        hocr_text: None,
        index,
        binarized: Some(binarized),
    })
}

async fn process_image_folder_to_djvu(
    image_files: Vec<PathBuf>,
    output_path: PathBuf,
    pipeline_config: PipelineConfig,
    deskew_engine: Option<std::sync::Arc<crate::deskew::DeskewEngine>>,
    progress_tracker: ProgressTracker,
    workers: usize,
    cpu_workers: usize,
) -> Result<()> {
    if output_path.is_dir() {
        return Err(anyhow!(
            "DJVU output path is an existing directory, not a file: {}",
            output_path.display()
        ));
    }

    let total = image_files.len();
    let config = Arc::new(pipeline_config);
    let cpu_semaphore = Arc::new(tokio::sync::Semaphore::new(cpu_workers.max(1)));
    let djvu_config = crate::pipeline::djvu_pipeline::create_djvu_pipeline_config(
        &output_path,
        config.as_ref(),
    )?;
    let orchestrator = Arc::new(crate::djvu::DjvuOrchestrator::new(djvu_config.clone())?);
    let (writer, writer_task) = crate::djvu::spawn_djvu_writer_actor(
        output_path,
        total,
        djvu_config.dpi,
        djvu_config.iw44_quality,
        progress_tracker.clone(),
        workers,
    );

    let mut in_flight: FuturesUnordered<tokio::task::JoinHandle<Result<(usize, djvu_encoder::doc::Page)>>> =
        FuturesUnordered::new();
    let mut next_input = 0usize;
    let mut processed = 0usize;
    let mut deskewed = 0usize;

    while next_input < total || !in_flight.is_empty() {
        while next_input < total && in_flight.len() < workers {
            let image_path = image_files[next_input].clone();
            let config = config.clone();
            let orchestrator = orchestrator.clone();
            let deskew_engine = deskew_engine.clone();
            let cpu_sem = cpu_semaphore.clone();
            let index = next_input;
            next_input += 1;

            in_flight.push(tokio::spawn(async move {
                // Stage 1: parallel PNG decode (single-threaded, run wide).
                let path_for_decode = image_path.clone();
                let rgb_image = tokio::task::spawn_blocking(move || {
                    decode_folder_image(&path_for_decode)
                })
                .await
                .map_err(|e| anyhow!("PNG decode panicked: {}", e))??;

                // Stage 2: rayon-heavy deskew + binarize + DJVU encode, gated.
                let _permit = cpu_sem
                    .acquire_owned()
                    .await
                    .map_err(|e| anyhow!("CPU semaphore closed: {}", e))?;
                let config_for_blocking = config.clone();
                let deskew_for_blocking = deskew_engine.clone();
                let orchestrator_for_blocking = orchestrator.clone();
                let path_for_log = image_path.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let (rgb_image, binarized) = deskew_and_binarize(
                        rgb_image,
                        &path_for_log,
                        config_for_blocking.as_ref(),
                        deskew_for_blocking,
                    )?;
                    orchestrator_for_blocking.process_page(crate::djvu::PageData {
                        index,
                        rgb_image,
                        binarized,
                        detections: Vec::new(),
                        hocr: None,
                    })
                })
                .await
                .map_err(|e| anyhow!("DJVU image worker panicked: {}", e))??;
                Ok((index, result))
            }));
        }

        let Some(result) = in_flight.next().await else {
            break;
        };
        let (index, page) = result.map_err(|e| anyhow!("DJVU image worker panicked: {}", e))??;
        writer.append_page(page, index).await?;

        processed += 1;
        if config.enable_deskew() {
            deskewed += 1;
        }
        if config.enable_layout_detection() {
            progress_tracker.publish_layout_progress(processed, processed, processed, deskewed, total);
        } else {
            progress_tracker.publish_no_layout_progress(processed, deskewed, total);
        }
    }

    writer.finalize().await?;
    writer_task
        .await
        .map_err(|e| anyhow!("DJVU writer task failed: {}", e))??;
    Ok(())
}

fn decode_folder_image(image_path: &Path) -> Result<RgbImage> {
    let dynamic = image::open(image_path)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("Failed to open image: {}", image_path.display()))?;
    Ok(dynamic.to_rgb8())
}

fn deskew_and_binarize(
    mut rgb_image: RgbImage,
    image_path: &Path,
    pipeline_config: &PipelineConfig,
    deskew_engine: Option<std::sync::Arc<crate::deskew::DeskewEngine>>,
) -> Result<(RgbImage, Vec<u8>)> {
    if let Some(engine) = deskew_engine.as_ref() {
        match engine.process_image(&rgb_image) {
            Ok(corrected) => {
                rgb_image = corrected;
            }
            Err(err) => {
                error_println!(
                    "Deskew failed for {} (continuing without correction): {}",
                    image_path.display(),
                    err
                );
            }
        }
    }

    let (width, height) = (rgb_image.width() as usize, rgb_image.height() as usize);
    let binarized = Legencode::color::binarization::binarize_image_raw(
        rgb_image.as_raw(),
        width,
        height,
        &Legencode::types::BinarizationOptions {
            invert: pipeline_config.binarization().invert,
            invert_input: pipeline_config.invert_input(),
            k_factor: pipeline_config.binarization().k_factor,
            use_heavy_duty: pipeline_config.binarization().use_heavy_duty
                && !pipeline_config.binarization().use_fixed_threshold,
            patch_percentage: pipeline_config.binarization().patch_percentage,
            no_patch: pipeline_config.binarization().no_patch,
            use_fixed_threshold: pipeline_config.binarization().use_fixed_threshold,
            fixed_threshold: pipeline_config.binarization().fixed_threshold,
        },
    );

    Ok((rgb_image, binarized))
}

fn collect_supported_images(folder: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && is_supported_image(&path) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| SUPPORTED_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

pub enum DebugCropKind {
    Text,
    Image,
    Both,
}

pub async fn run_pdf_layout_crop_debug(
    pdf_path: PathBuf,
    output: Option<PathBuf>,
    crop_kind: DebugCropKind,
    page_range: Option<String>,
    _config: AppConfig,
    enable_deskew: bool,
    save_format: Option<&str>,
    png_quantize: bool,
    png_colors: u16,
) -> Result<()> {
    let output_dir = output.unwrap_or_else(|| {
        pdf_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!(
                "{}_areas",
                pdf_path.file_stem().unwrap().to_string_lossy()
            ))
    });
    std::fs::create_dir_all(&output_dir)?;

    info_println!(
        "PDF Layout Crop Debug\n  Input: {}\n  Output: {}\n  Deskew: {}\n  Mode: {}",
        pdf_path.display(),
        output_dir.display(),
        if enable_deskew { "ENABLED" } else { "disabled" },
        match crop_kind {
            DebugCropKind::Text => "text",
            DebugCropKind::Image => "image",
            DebugCropKind::Both => "both",
        }
    );

    let pdf_bytes_vec = std::fs::read(&pdf_path)
        .with_context(|| format!("Failed to read PDF: {}", pdf_path.display()))?;
    let pdf_bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(pdf_bytes_vec.into_boxed_slice());
    let mut raster_cfg = PdfRasterConfig::default();
    raster_cfg.render_forms = false;
    let renderer = PdfiumRenderer::new_from_bytes(pdf_bytes, raster_cfg)?;
    let total_pages = renderer.page_count() as usize;

    let mut pipeline_config = PipelineConfig::default();
    pipeline_config.set_enable_layout_detection(true);
    pipeline_config.set_enable_deskew(enable_deskew);
    pipeline_config.set_high_res_render_height(pipeline_config.target_height())?;

    let deskew_engine = prepare_shared_deskew_engine(&pipeline_config)?;
    let mut engine = PaddleXEngine::new(
        pipeline_config.model_path(),
        PaddleXConfig::new(
            pipeline_config.confidence_threshold(),
            pipeline_config.nms_threshold(),
            pipeline_config.nms_threshold(),
            1,
        ),
    )?;

    let target_height = pipeline_config.high_res_render_height();
    let classifier = &crate::types::LABEL_CLASSIFIER;
    let ext = match save_format.map(|s| s.to_ascii_lowercase()) {
        Some(ref s) if s == "jpg" || s == "jpeg" => ".jpg",
        _ => ".png",
    };

    let pages_to_render: Vec<usize> = if let Some(range_str) = page_range {
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
        pages
    } else {
        (1..=total_pages).collect()
    };

    for (i, page_num) in pages_to_render.iter().enumerate() {
        println!("Processing page {} of {}", page_num, total_pages);
        let rgb = renderer
            .render_page_rgb((*page_num - 1) as u32, target_height, None)
            .await?;
        let img_buf = RgbImage::from_raw(rgb.width, rgb.height, rgb.data)
            .ok_or_else(|| anyhow!("Failed to construct image buffer for page {}", page_num))?;
        let mut img: RgbImage = img_buf;

        if let Some(engine_dk) = deskew_engine.as_ref() {
            match engine_dk.process_image(&img) {
                Ok(corrected) => {
                    img = corrected;
                }
                Err(err) => {
                    error_println!("Deskew failed for page {}: {}", page_num, err);
                }
            }
        }

        let detections = engine.detect_single_async(&img).await?;
        let filtered: Vec<Detection> = match crop_kind {
            DebugCropKind::Text => detections
                .into_iter()
                .filter(|d| classifier.is_text_label(d))
                .collect(),
            DebugCropKind::Image => detections
                .into_iter()
                .filter(|d| classifier.is_image_label(d))
                .collect(),
            DebugCropKind::Both => detections,
        };

        if filtered.is_empty() {
            debug_println!("No regions on page {}", page_num);
            continue;
        }

        let mut saved = 0usize;
        let pw = img.width() as f32;
        let ph = img.height() as f32;
        for (area_idx, det) in filtered.iter().enumerate() {
            let x1 = det.bbox[0].floor().clamp(0.0, pw) as u32;
            let y1 = det.bbox[1].floor().clamp(0.0, ph) as u32;
            let x2 = det.bbox[2].ceil().clamp(0.0, pw) as u32;
            let y2 = det.bbox[3].ceil().clamp(0.0, ph) as u32;
            if x2 <= x1 || y2 <= y1 {
                continue;
            }
            let w = x2 - x1;
            let h = y2 - y1;
            let crop = image::imageops::crop_imm(&img, x1, y1, w, h).to_image();
            let filename = format!("page_{:04}_area_{:03}{}", page_num, area_idx + 1, ext);
            let output_path = output_dir.join(filename);
            if ext == ".png" && png_quantize {
                crate::colorquant::write_quantized_rgb_png(
                    &crop,
                    &output_path,
                    crate::colorquant::PngQuantizationOptions { colors: png_colors },
                )?;
            } else {
                crop.save(&output_path).map_err(anyhow::Error::msg)?;
            }
            saved += 1;
        }
        info_println!("Saved {} regions from page {}", saved, page_num);
    }

    info_println!("Region cropping complete: {}", output_dir.display());
    Ok(())
}

/// Process PDF to image folder (PNGs if layout detection is on, PBMs if layout detection is off, JPEG if image_only)
pub fn run_pdf_to_images_mode(
    pdf_path: PathBuf,
    page_range: Option<String>,
    output_dir: Option<PathBuf>,
    config: AppConfig,
    enable_layout_detection: bool,
    enable_deskew: bool,
    image_only: bool,
    png_quantize: bool,
    png_colors: u16,
) -> Result<()> {
    let output_dir = output_dir.unwrap_or_else(|| {
        pdf_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!(
                "{}_images_output",
                pdf_path.file_stem().unwrap().to_string_lossy()
            ))
    });
    std::fs::create_dir_all(&output_dir)?;

    info_println!(
        "PDF to Images Mode\n  Input: {}\n  Output: {}\n  Layout: {}\n  Deskew: {}",
        pdf_path.display(),
        output_dir.display(),
        if enable_layout_detection {
            "ENABLED (PNG)"
        } else {
            "DISABLED (PBM)"
        },
        if enable_deskew { "ENABLED" } else { "disabled" }
    );

    // Read PDF and initialize renderer
    let pdf_bytes_vec = std::fs::read(&pdf_path)
        .with_context(|| format!("Failed to read PDF: {}", pdf_path.display()))?;
    let pdf_bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(pdf_bytes_vec.into_boxed_slice());
    let mut raster_cfg = PdfRasterConfig::default();
    raster_cfg.render_forms = false;
    let renderer = PdfiumRenderer::new_from_bytes(pdf_bytes, raster_cfg)?;
    let total_pages = renderer.page_count() as usize;

    // Parse page range
    let pages_to_render: Vec<usize> = if let Some(range_str) = page_range {
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
        pages
    } else {
        (1..=total_pages).collect()
    };

    info_println!(
        "Processing {} pages from PDF with {} total pages",
        pages_to_render.len(),
        total_pages
    );

    // Setup pipeline config
    let mut pipeline_config = PipelineConfig::default();
    pipeline_config.set_enable_layout_detection(enable_layout_detection);
    pipeline_config.set_enable_deskew(enable_deskew);
    pipeline_config.set_high_res_render_height(pipeline_config.target_height())?;

    let deskew_engine = prepare_shared_deskew_engine(&pipeline_config)?;
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.handle().clone()
        }
    };

    let target_height = pipeline_config.high_res_render_height();

    for (i, page_num) in pages_to_render.iter().enumerate() {
        println!("Processing page {} of {}", page_num, total_pages);

        // Render PDF page to RGB (direct call without block_on to avoid task cancellation)
        let rgb = renderer.render_page_rgb_sync((*page_num - 1) as u32, target_height, None)?;
        let img_buf = RgbImage::from_raw(rgb.width, rgb.height, rgb.data)
            .ok_or_else(|| anyhow!("Failed to construct image buffer for page {}", page_num))?;
        let mut img: RgbImage = img_buf;

        // Apply deskew if enabled
        if let Some(engine_dk) = deskew_engine.as_ref() {
            match engine_dk.process_image(&img) {
                Ok(corrected) => {
                    img = corrected;
                }
                Err(err) => {
                    error_println!("Deskew failed for page {}: {}", page_num, err);
                }
            }
        }

        // Determine output format based on layout detection and image_only flag
        if enable_layout_detection {
            // Save as PNG or JPEG based on image_only flag
            let (output_filename, format_name) = if image_only {
                (format!("page_{:04}.jpg", page_num), "JPEG")
            } else {
                (format!("page_{:04}.png", page_num), "PNG")
            };
            let output_path = output_dir.join(output_filename);

            if image_only {
                // Use TooJPEG for JPEG encoding
                let width = img.width() as u32;
                let height = img.height() as u32;

                let options = EncodeOptions {
                    width,
                    height,
                    format: ImageFormat::RGB,
                    quality: 90,
                    baseline: true,
                    optimized: true,
                    downsample: true,
                };

                let mut file = File::create(&output_path).with_context(|| {
                    format!("Failed to create JPEG file: {}", output_path.display())
                })?;

                encode_jpeg(img.as_raw(), options, &mut file)
                    .map_err(|e| anyhow!("TooJPEG encoding failed: {}", e))?;
            } else {
                // Use image crate for PNG encoding
                if png_quantize {
                    crate::colorquant::write_quantized_rgb_png(
                        &img,
                        &output_path,
                        crate::colorquant::PngQuantizationOptions { colors: png_colors },
                    )
                    .with_context(|| {
                        format!("Failed to save quantized PNG: {}", output_path.display())
                    })?;
                } else {
                    img.save(&output_path)
                        .map_err(anyhow::Error::msg)
                        .with_context(|| format!("Failed to save PNG: {}", output_path.display()))?;
                }
            }
            info_println!(
                "Page {} saved as {}: {}",
                page_num,
                format_name,
                output_path.display()
            );
        } else {
            // Binarize and save as PBM
            let (width, height) = (img.width() as usize, img.height() as usize);
            let binarized = Legencode::color::binarization::binarize_image_raw(
                img.as_raw(),
                width,
                height,
                &Legencode::types::BinarizationOptions {
                    invert: pipeline_config.binarization().invert,
                    invert_input: pipeline_config.invert_input(),
                    k_factor: pipeline_config.binarization().k_factor,
                    use_heavy_duty: pipeline_config.binarization().use_heavy_duty
                        && !pipeline_config.binarization().use_fixed_threshold,
                    patch_percentage: pipeline_config.binarization().patch_percentage,
                    no_patch: pipeline_config.binarization().no_patch,
                    use_fixed_threshold: pipeline_config.binarization().use_fixed_threshold,
                    fixed_threshold: pipeline_config.binarization().fixed_threshold,
                },
            );

            let output_filename = format!("page_{:04}.pbm", page_num);
            let output_path = output_dir.join(output_filename);

            // Use the well-tested PBM writer from binarization.rs
            let pbm_data =
                Legencode::color::binarization::pbm::make_pbm_p4(&binarized, width, height);
            std::fs::write(&output_path, pbm_data)
                .with_context(|| format!("Failed to write PBM file: {}", output_path.display()))?;
            info_println!("Page {} saved as PBM: {}", page_num, output_path.display());
        }
    }

    info_println!(
        "PDF to images conversion complete: {} pages processed",
        pages_to_render.len()
    );
    Ok(())
}

/// Process image folder to image folder (PNGs if layout detection is on, PBMs if layout detection is off, JPEG if image_only)
pub fn run_images_to_images_mode(
    input_folder: PathBuf,
    output_dir: Option<PathBuf>,
    config: AppConfig,
    enable_layout_detection: bool,
    enable_deskew: bool,
    image_only: bool,
    png_quantize: bool,
    png_colors: u16,
) -> Result<()> {
    if !input_folder.is_dir() {
        return Err(anyhow!(
            "Path is not a directory: {}",
            input_folder.display()
        ));
    }

    let output_dir = output_dir.unwrap_or_else(|| input_folder.join("processed_images"));
    std::fs::create_dir_all(&output_dir)?;

    info_println!(
        "Images to Images Mode\n  Input: {}\n  Output: {}\n  Layout: {}\n  Deskew: {}",
        input_folder.display(),
        output_dir.display(),
        if enable_layout_detection {
            "ENABLED (PNG)"
        } else {
            "DISABLED (PBM)"
        },
        if enable_deskew { "ENABLED" } else { "disabled" }
    );

    let image_files = collect_supported_images(&input_folder)?;
    if image_files.is_empty() {
        return Err(anyhow!(
            "No supported image files found in {}",
            input_folder.display()
        ));
    }

    info_println!("Found {} image files", image_files.len());

    let mut pipeline_config = PipelineConfig::default();
    pipeline_config.set_enable_layout_detection(enable_layout_detection);
    pipeline_config.set_enable_deskew(enable_deskew);
    pipeline_config.set_high_res_render_height(pipeline_config.target_height())?;

    let deskew_engine = prepare_shared_deskew_engine(&pipeline_config)?;

    for (idx, image_path) in image_files.iter().enumerate() {
        println!(
            "Processing {} of {}: {}",
            idx + 1,
            image_files.len(),
            image_path.display()
        );

        if let Err(err) = process_single_image_to_image(
            image_path,
            &output_dir,
            &pipeline_config,
            deskew_engine.clone(),
            image_only,
            png_quantize,
            png_colors,
        ) {
            error_println!("Failed to process {}: {}", image_path.display(), err);
        } else {
            info_println!("Processed {}", image_path.display());
        }
    }

    info_println!(
        "Image to image conversion complete: {} files processed",
        image_files.len()
    );
    Ok(())
}

/// Process a single image to image (PNG or PBM, JPEG if image_only)
fn process_single_image_to_image(
    image_path: &Path,
    output_dir: &Path,
    pipeline_config: &PipelineConfig,
    deskew_engine: Option<std::sync::Arc<crate::deskew::DeskewEngine>>,
    image_only: bool,
    png_quantize: bool,
    png_colors: u16,
) -> Result<()> {
    let dynamic = image::open(image_path)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("Failed to open image: {}", image_path.display()))?;
    let mut rgb_image: RgbImage = dynamic.to_rgb8();

    // Apply deskew if enabled
    if let Some(engine) = deskew_engine.as_ref() {
        match engine.process_image(&rgb_image) {
            Ok(corrected) => {
                rgb_image = corrected;
            }
            Err(err) => {
                error_println!(
                    "Deskew failed for {} (continuing without correction): {}",
                    image_path.display(),
                    err
                );
            }
        }
    }

    let file_stem = image_path.file_stem().unwrap_or_default().to_string_lossy();

    if pipeline_config.enable_layout_detection() {
        // Save as PNG or JPEG based on image_only flag
        let (output_filename, format_name) = if image_only {
            (format!("{}.jpg", file_stem), "JPEG")
        } else {
            (format!("{}.png", file_stem), "PNG")
        };
        let output_path = output_dir.join(output_filename);

        if image_only {
            // Use TooJPEG for JPEG encoding
            let width = rgb_image.width() as u32;
            let height = rgb_image.height() as u32;

            let options = EncodeOptions {
                width,
                height,
                format: ImageFormat::RGB,
                quality: 90,
                baseline: true,
                optimized: true,
                downsample: true,
            };

            let mut file = File::create(&output_path).with_context(|| {
                format!("Failed to create JPEG file: {}", output_path.display())
            })?;

            encode_jpeg(rgb_image.as_raw(), options, &mut file)
                .map_err(|e| anyhow!("TooJPEG encoding failed: {}", e))?;
        } else {
            // Use image crate for PNG encoding
            if png_quantize {
                crate::colorquant::write_quantized_rgb_png(
                    &rgb_image,
                    &output_path,
                    crate::colorquant::PngQuantizationOptions { colors: png_colors },
                )
                .with_context(|| {
                    format!("Failed to save quantized PNG: {}", output_path.display())
                })?;
            } else {
                rgb_image
                    .save(&output_path)
                    .map_err(anyhow::Error::msg)
                    .with_context(|| format!("Failed to save PNG: {}", output_path.display()))?;
            }
        }
    } else {
        // Binarize and save as PBM
        let (width, height) = (rgb_image.width() as usize, rgb_image.height() as usize);
        let binarized = Legencode::color::binarization::binarize_image_raw(
            rgb_image.as_raw(),
            width,
            height,
            &Legencode::types::BinarizationOptions {
                invert: pipeline_config.binarization().invert,
                invert_input: pipeline_config.invert_input(),
                k_factor: pipeline_config.binarization().k_factor,
                use_heavy_duty: pipeline_config.binarization().use_heavy_duty
                    && !pipeline_config.binarization().use_fixed_threshold,
                patch_percentage: pipeline_config.binarization().patch_percentage,
                no_patch: pipeline_config.binarization().no_patch,
                use_fixed_threshold: pipeline_config.binarization().use_fixed_threshold,
                fixed_threshold: pipeline_config.binarization().fixed_threshold,
            },
        );

        let output_filename = format!("{}.pbm", file_stem);
        let output_path = output_dir.join(output_filename);

        // Use the well-tested PBM writer from binarization.rs
        let pbm_data = Legencode::color::binarization::pbm::make_pbm_p4(&binarized, width, height);
        std::fs::write(&output_path, pbm_data)
            .with_context(|| format!("Failed to write PBM file: {}", output_path.display()))?;
    }

    Ok(())
}
