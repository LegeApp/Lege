use crate::{
    PipelineConfig,
    accumulator::{ContentElement, Page, assemble_pdf},
    debug_println, error_println, info_println, prepare_shared_deskew_engine,
    pagerender::prelude::{PdfiumRenderer, RasterConfig as PdfRasterConfig},
    engine::{PaddleXEngine, PaddleXConfig, Detection},
    pipeline::encode_page_data,
};
use anyhow::{Context, Result, anyhow};
use image::RgbImage;
use std::path::{Path, PathBuf};

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
    if !folder.is_dir() {
        return Err(anyhow!("Path is not a directory: {}", folder.display()));
    }

    let output_dir = output.unwrap_or_else(|| folder.join("output"));
    std::fs::create_dir_all(&output_dir)?;

    info_println!(
        "Image Folder Mode\n  Input: {}\n  Output: {}\n  Deskew: {}",
        folder.display(),
        output_dir.display(),
        if enable_deskew { "ENABLED" } else { "disabled" }
    );

    let image_files = collect_supported_images(&folder)?;
    if image_files.is_empty() {
        return Err(anyhow!(
            "No supported image files found in {}",
            folder.display()
        ));
    }

    info_println!("Found {} image files", image_files.len());

    let mut pipeline_config = PipelineConfig::default();
    pipeline_config.set_enable_layout_detection(true);
    pipeline_config.set_enable_deskew(enable_deskew);
    pipeline_config.set_high_res_render_height(pipeline_config.target_height())?;

    let deskew_engine = prepare_shared_deskew_engine(&pipeline_config)?;
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => {
            // We are not inside a runtime; create a new one.
            let rt = tokio::runtime::Runtime::new()?;
            rt.handle().clone()
        }
    };

    for (idx, image_path) in image_files.iter().enumerate() {
        println!(
            "Processing {} of {}: {}",
            idx + 1,
            image_files.len(),
            image_path.display()
        );

        if let Err(err) = process_single_image(
            image_path,
            &output_dir,
            &pipeline_config,
            deskew_engine.clone(),
            &runtime,
        ) {
            error_println!("Failed to process {}: {}", image_path.display(), err);
        } else {
            debug_println!("Processed {}", image_path.display());
        }
    }

    info_println!("Image processing complete");
    Ok(())
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

fn process_single_image(
    image_path: &Path,
    output_dir: &Path,
    pipeline_config: &PipelineConfig,
    deskew_engine: Option<std::sync::Arc<crate::deskew::DeskewEngine>>,
    runtime: &tokio::runtime::Handle,
) -> Result<()> {
    let dynamic = image::open(image_path)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("Failed to open image: {}", image_path.display()))?;
    let mut rgb_image: RgbImage = dynamic.to_rgb8();

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

    let base_layer = runtime.block_on(async {
        encode_page_data(&binarized, width, height, 0, pipeline_config).await
    })?;

    let page = Page {
        width: width as f32,
        height: height as f32,
        elements: vec![ContentElement {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            content: base_layer,
        }],
        hocr_text: None,
        index: 0,
        binarized: Some(binarized.clone()),
    };

    let output_pdf = output_dir.join(
        image_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
            + ".pdf",
    );

    assemble_pdf(&[page], output_pdf.to_str().unwrap(), 25, false)?;
    Ok(())
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
) -> Result<()> {
    let output_dir = output.unwrap_or_else(|| {
        pdf_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{}_areas", pdf_path.file_stem().unwrap().to_string_lossy()))
    });
    std::fs::create_dir_all(&output_dir)?;

    info_println!(
        "PDF Layout Crop Debug\n  Input: {}\n  Output: {}\n  Deskew: {}\n  Mode: {}",
        pdf_path.display(),
        output_dir.display(),
        if enable_deskew { "ENABLED" } else { "disabled" },
        match crop_kind { DebugCropKind::Text => "text", DebugCropKind::Image => "image", DebugCropKind::Both => "both" }
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
                if parts.len() != 2 { return Err(anyhow!("Invalid page range: {}", part)); }
                let start: usize = parts[0].parse().map_err(|_| anyhow!("Invalid page: {}", parts[0]))?;
                let end: usize = parts[1].parse().map_err(|_| anyhow!("Invalid page: {}", parts[1]))?;
                if start == 0 || end == 0 { return Err(anyhow!("Page numbers must start from 1")); }
                if start > end { return Err(anyhow!("Invalid range: {}", part)); }
                if end > total_pages { return Err(anyhow!("Page {} exceeds total ({})", end, total_pages)); }
                for p in start..=end { pages.push(p); }
            } else {
                let p: usize = part.parse().map_err(|_| anyhow!("Invalid page: {}", part))?;
                if p == 0 || p > total_pages { return Err(anyhow!("Invalid page {} (1..{}).", p, total_pages)); }
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
        let rgb = renderer.render_page_rgb((*page_num - 1) as u32, target_height, None).await?;
        let img_buf = RgbImage::from_raw(rgb.width, rgb.height, rgb.data)
            .ok_or_else(|| anyhow!("Failed to construct image buffer for page {}", page_num))?;
        let mut img: RgbImage = img_buf;

        if let Some(engine_dk) = deskew_engine.as_ref() {
            match engine_dk.process_image(&img) {
                Ok(corrected) => { img = corrected; }
                Err(err) => { error_println!("Deskew failed for page {}: {}", page_num, err); }
            }
        }

        let detections = engine.detect_single_async(&img).await?;
        let filtered: Vec<Detection> = match crop_kind {
            DebugCropKind::Text => detections.into_iter().filter(|d| classifier.is_text_label(d)).collect(),
            DebugCropKind::Image => detections.into_iter().filter(|d| classifier.is_image_label(d)).collect(),
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
            if x2 <= x1 || y2 <= y1 { continue; }
            let w = x2 - x1;
            let h = y2 - y1;
            let crop = image::imageops::crop_imm(&img, x1, y1, w, h).to_image();
            let filename = format!("page_{:04}_area_{:03}{}", page_num, area_idx + 1, ext);
            crop
                .save(output_dir.join(filename))
                .map_err(anyhow::Error::msg)?;
            saved += 1;
        }
        info_println!("Saved {} regions from page {}", saved, page_num);
    }

    info_println!("Region cropping complete: {}", output_dir.display());
    Ok(())
}
