use crate::pagerender::prelude::{PdfiumRenderer, RasterConfig as PdfRasterConfig};
use crate::pipeline::config::{PipelineConfig, RenderedPageData};
use crate::pipeline::helper_functions::wait_for_memory_relief;
use crate::pipeline::policies::build_inference_image;
use crate::progress::ProgressTracker;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use image::RgbImage;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

const SUPPORTED_IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "ppm", "pbm", "pgm", "pnm", "tiff", "tif", "bmp", "jp2",
];

pub struct SourcePage {
    pub image: RgbImage,
    pub original_width_pts: f32,
    pub original_height_pts: f32,
}

#[async_trait]
pub trait PageSource: Send + Sync {
    fn page_count(&self) -> usize;
    fn source_concurrency(&self) -> usize;
    fn pdf_renderer(&self) -> Option<Arc<PdfiumRenderer>> {
        None
    }
    async fn load_page(&self, page_index: usize) -> Result<SourcePage>;
}

pub struct PdfiumPageSource {
    renderer: Arc<PdfiumRenderer>,
    config: Arc<PipelineConfig>,
}

impl PdfiumPageSource {
    pub fn new(pdf_bytes: Arc<[u8]>, config: Arc<PipelineConfig>) -> Result<Self> {
        let mut raster_cfg = PdfRasterConfig::default();
        raster_cfg.render_forms = false;
        raster_cfg.target_width = config.target_width();
        let renderer = Arc::new(PdfiumRenderer::new_from_bytes(pdf_bytes, raster_cfg)?);
        Ok(Self { renderer, config })
    }
}

#[async_trait]
impl PageSource for PdfiumPageSource {
    fn page_count(&self) -> usize {
        self.renderer.page_count() as usize
    }

    fn source_concurrency(&self) -> usize {
        1
    }

    fn pdf_renderer(&self) -> Option<Arc<PdfiumRenderer>> {
        Some(self.renderer.clone())
    }

    async fn load_page(&self, page_index: usize) -> Result<SourcePage> {
        let rgb_page = self
            .renderer
            .render_page_rgb(
                page_index as u32,
                self.config.target_height(),
                self.config.target_width(),
            )
            .await
            .map_err(|e| anyhow!("Failed to render page {}: {}", page_index, e))?;

        let image = RgbImage::from_raw(rgb_page.width, rgb_page.height, rgb_page.data).ok_or_else(
            || {
                anyhow!(
                    "Failed to create ImageBuffer for page {} ({}x{})",
                    page_index,
                    rgb_page.width,
                    rgb_page.height
                )
            },
        )?;

        Ok(SourcePage {
            image,
            original_width_pts: rgb_page.original_width_pts,
            original_height_pts: rgb_page.original_height_pts,
        })
    }
}

pub struct ImageFolderPageSource {
    files: Arc<Vec<PathBuf>>,
    concurrency: usize,
    target_height: u32,
    target_width: Option<u32>,
}

impl ImageFolderPageSource {
    pub fn new(
        files: Vec<PathBuf>,
        concurrency: usize,
        target_height: u32,
        target_width: Option<u32>,
    ) -> Self {
        Self {
            files: Arc::new(files),
            concurrency: concurrency.max(1),
            target_height,
            target_width,
        }
    }
}

#[async_trait]
impl PageSource for ImageFolderPageSource {
    fn page_count(&self) -> usize {
        self.files.len()
    }

    fn source_concurrency(&self) -> usize {
        self.concurrency
    }

    async fn load_page(&self, page_index: usize) -> Result<SourcePage> {
        let path = self
            .files
            .get(page_index)
            .cloned()
            .ok_or_else(|| anyhow!("Image page index {} out of bounds", page_index))?;

        let decode_path = path.clone();
        let mut image = tokio::task::spawn_blocking(move || decode_folder_image(&decode_path))
            .await
            .map_err(|e| anyhow!("Image decode task panicked: {}", e))??;
        let (width, height) = image.dimensions();
        maybe_dump_folder_source_image("decoded", page_index, &image)?;
        image = normalize_folder_image_to_target(
            image,
            self.target_height,
            self.target_width,
            page_index,
        )?;
        maybe_dump_folder_source_image("normalized", page_index, &image)?;

        Ok(SourcePage {
            image,
            original_width_pts: width as f32,
            original_height_pts: height as f32,
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn source_stage(
    source: Arc<dyn PageSource>,
    config: Arc<PipelineConfig>,
    deskew_engine: Option<Arc<crate::deskew::DeskewEngine>>,
    page_range: std::ops::Range<usize>,
    tx: mpsc::Sender<RenderedPageData>,
    render_count: Arc<AtomicUsize>,
    detect_count: Arc<AtomicUsize>,
    encode_count: Arc<AtomicUsize>,
    progress: ProgressTracker,
    total_pages: usize,
    layout_enabled: bool,
) -> Result<()> {
    crate::info_log!(
        "[SourceStage] Starting source stage for {} pages with concurrency={}",
        total_pages,
        source.source_concurrency()
    );

    let mut in_flight = FuturesUnordered::new();
    let mut next_page = page_range.start;
    let page_end = page_range.end;
    let concurrency = source.source_concurrency().max(1);

    loop {
        while next_page < page_end && in_flight.len() < concurrency {
            let source = source.clone();
            let config = config.clone();
            let deskew_engine = deskew_engine.clone();
            let page_index = next_page;
            next_page += 1;

            in_flight.push(tokio::spawn(async move {
                if page_index % 10 == 0 {
                    wait_for_memory_relief().await;
                }

                #[cfg(feature = "debug-logging")]
                crate::info_log!("[SourceStage] Loading page {}", page_index);
                let mut source_page = source.load_page(page_index).await.map_err(|e| {
                    #[cfg(feature = "debug-logging")]
                    crate::error_println!("[SourceStage] Page {} load failed: {:#}", page_index, e);
                    e
                })?;

                if let Some(engine) = deskew_engine {
                    let image_for_deskew = source_page.image.clone();
                    match tokio::task::spawn_blocking(move || {
                        engine.process_image(&image_for_deskew)
                    })
                    .await
                    {
                        Ok(Ok(corrected)) => source_page.image = corrected,
                        Ok(Err(e)) => {
                            crate::warn_log!("Page {}: deskew failed: {}", page_index, e);
                        }
                        Err(e) => {
                            crate::warn_log!("Page {}: deskew task panicked: {}", page_index, e);
                        }
                    }
                }

                crate::pipeline::set_standard_dimensions_once(
                    source_page.image.width(),
                    source_page.image.height(),
                );

                let high_res_arc = Arc::new(source_page.image);
                // In no-layout mode the inference image is never used; share the
                // high-res Arc instead of deep-cloning a full RGB page per file.
                let inference_image = if config.enable_layout_detection() {
                    let spec = config.inference_resize_spec();
                    let img = build_inference_image(high_res_arc.as_ref(), &spec)
                        .unwrap_or_else(|_| (*high_res_arc).clone());
                    Arc::new(img)
                } else {
                    high_res_arc.clone()
                };

                Ok::<_, anyhow::Error>(RenderedPageData {
                    index: page_index,
                    high_res_image: high_res_arc,
                    inference_image,
                    original_width_pts: source_page.original_width_pts,
                    original_height_pts: source_page.original_height_pts,
                })
            }));
        }

        let Some(result) = in_flight.next().await else {
            break;
        };
        let rendered = result.map_err(|e| anyhow!("Source stage task panicked: {}", e))??;
        if tx.send(rendered).await.is_err() {
            break;
        }

        let rendered_val = render_count.fetch_add(1, Ordering::Relaxed) + 1;
        let deskewed_val = if config.enable_deskew() {
            rendered_val
        } else {
            0
        };
        if layout_enabled {
            let detected_val = detect_count.load(Ordering::Relaxed);
            let encoded_val = encode_count.load(Ordering::Relaxed);
            progress.publish_layout_progress(
                rendered_val,
                detected_val,
                encoded_val,
                deskewed_val,
                total_pages,
            );
        } else {
            progress.publish_no_layout_progress(rendered_val, deskewed_val, total_pages);
        }
    }

    crate::info_log!("[SourceStage] Source stage complete");
    Ok(())
}

pub fn collect_largest_sequential_image_run(folder: &Path) -> Result<Vec<PathBuf>> {
    let mut all_files: Vec<PathBuf> = Vec::new();
    collect_supported_images_recursive(folder, &mut all_files)?;

    if all_files.is_empty() {
        return Ok(all_files);
    }

    let mut by_ext: std::collections::HashMap<String, Vec<PathBuf>> =
        std::collections::HashMap::new();
    for file in all_files {
        let ext = file
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        by_ext.entry(ext).or_default().push(file);
    }

    let mut best_run = Vec::new();
    for (_ext, group) in by_ext {
        let mut numbered = Vec::new();
        let mut unnumbered = Vec::new();

        for path in group {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            match trailing_number(stem) {
                Some(n) => numbered.push((n, path)),
                None => unnumbered.push(path),
            }
        }

        if numbered.is_empty() {
            unnumbered.sort();
            if unnumbered.len() > best_run.len() {
                best_run = unnumbered;
            }
            continue;
        }

        numbered.sort_by_key(|(n, _)| *n);
        let run = longest_consecutive_run(&numbered);
        if run.len() > best_run.len() {
            best_run = run;
        }
    }

    if folder_source_debug_enabled() {
        crate::info_log!(
            "[ImageFolderPageSource] Selected {} files from {}",
            best_run.len(),
            folder.display()
        );
        for (idx, path) in best_run.iter().take(5).enumerate() {
            crate::info_log!(
                "[ImageFolderPageSource] selected[{}]={}",
                idx,
                path.display()
            );
        }
        if best_run.len() > 5 {
            for (idx, path) in best_run
                .iter()
                .enumerate()
                .skip(best_run.len().saturating_sub(5))
            {
                crate::info_log!(
                    "[ImageFolderPageSource] selected[{}]={}",
                    idx,
                    path.display()
                );
            }
        }
    }

    Ok(best_run)
}

fn folder_source_debug_enabled() -> bool {
    std::env::var("LEGE_FOLDER_SOURCE_DEBUG")
        .map(|value| value != "0")
        .unwrap_or(false)
}

fn maybe_dump_folder_source_image(stage: &str, page_index: usize, image: &RgbImage) -> Result<()> {
    if !folder_source_debug_enabled() {
        return Ok(());
    }

    let out_dir = crate::app_dirs::data_dir().join("folder_source_debug");
    std::fs::create_dir_all(&out_dir).with_context(|| {
        format!(
            "Failed to create folder source debug directory: {:?}",
            out_dir
        )
    })?;
    let out_path = out_dir.join(format!("page_{:04}_{}.png", page_index + 1, stage));
    image
        .save(&out_path)
        .with_context(|| format!("Failed to write folder source debug image: {:?}", out_path))?;
    Ok(())
}

fn normalize_folder_image_to_target(
    image: RgbImage,
    target_height: u32,
    target_width: Option<u32>,
    page_index: usize,
) -> Result<RgbImage> {
    if target_height == 0 || image.height() == target_height {
        return Ok(image);
    }

    let current_w = image.width();
    let current_h = image.height();
    let aspect_ratio = current_w as f32 / current_h as f32;
    let target_w =
        target_width.unwrap_or_else(|| (target_height as f32 * aspect_ratio).round() as u32);
    if target_w == 0 {
        return Ok(image);
    }

    let params = crate::resize::ResizeParams {
        target_width: target_w,
        target_height,
        method: crate::resize::ResizeMethod::Lanczos3,
        letterbox: false,
        border_value: 0.0,
        swap_rb: false,
    };

    match crate::resize::resize_bytes(image.as_raw(), current_w, current_h, &params, 3) {
        Ok(bytes) => RgbImage::from_raw(target_w, target_height, bytes).ok_or_else(|| {
            anyhow!(
                "Failed to create normalized image buffer for page {} ({}x{})",
                page_index,
                target_w,
                target_height
            )
        }),
        Err(e) => {
            crate::warn_log!(
                "Page {}: folder source resize failed: {}; using CPU image fallback",
                page_index,
                e
            );
            Ok(image::imageops::resize(
                &image,
                target_w,
                target_height,
                image::imageops::FilterType::Lanczos3,
            ))
        }
    }
}

fn collect_supported_images_recursive(folder: &Path, all_files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_supported_images_recursive(&path, all_files)?;
        } else if path.is_file() && is_supported_image(&path) {
            all_files.push(path);
        }
    }
    Ok(())
}

fn decode_folder_image(image_path: &Path) -> Result<RgbImage> {
    let is_jp2 = image_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("jp2"))
        .unwrap_or(false);

    if is_jp2 {
        #[cfg(feature = "jp2-lam")]
        {
            let bytes = std::fs::read(image_path)
                .with_context(|| format!("Failed to read JP2: {}", image_path.display()))?;
            let (width, height, rgb_data) = Legencode::jp2_encoder::decode_jp2_bytes(&bytes)
                .map_err(|e| {
                    anyhow!(
                        "JP2 cannot be decoded, job aborted: {} - {}",
                        image_path.display(),
                        e
                    )
                })?;
            return RgbImage::from_raw(width, height, rgb_data).ok_or_else(|| {
                anyhow!(
                    "JP2 cannot be decoded, job aborted: {} - invalid image dimensions or buffer",
                    image_path.display()
                )
            });
        }
        #[cfg(not(feature = "jp2-lam"))]
        return Err(anyhow!(
            "JP2 support not compiled in (enable jp2-lam feature): {}",
            image_path.display()
        ));
    }

    let dynamic = image::open(image_path)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("Failed to open image: {}", image_path.display()))?;
    Ok(dynamic.to_rgb8())
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| SUPPORTED_IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

fn trailing_number(s: &str) -> Option<u64> {
    let rev_digits: String = s.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    if rev_digits.is_empty() {
        return None;
    }
    let digits: String = rev_digits.chars().rev().collect();
    digits.parse().ok()
}

fn longest_consecutive_run(sorted: &[(u64, PathBuf)]) -> Vec<PathBuf> {
    if sorted.is_empty() {
        return Vec::new();
    }

    let mut best_start = 0usize;
    let mut best_len = 1usize;
    let mut cur_start = 0usize;
    let mut cur_len = 1usize;

    for i in 1..sorted.len() {
        if sorted[i].0 == sorted[i - 1].0 + 1 {
            cur_len += 1;
        } else {
            if cur_len > best_len {
                best_len = cur_len;
                best_start = cur_start;
            }
            cur_start = i;
            cur_len = 1;
        }
    }
    if cur_len > best_len {
        best_len = cur_len;
        best_start = cur_start;
    }

    sorted[best_start..best_start + best_len]
        .iter()
        .map(|(_, path)| path.clone())
        .collect()
}
