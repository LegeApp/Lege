use crate::pagerender::prelude::{PdfRenderer, RasterConfig as PdfRasterConfig};
use crate::pipeline::config::{PipelineConfig, RenderedPageData};
use crate::pipeline::policies::build_inference_image;
use crate::progress::ProgressTracker;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use image::RgbImage;
use lege_pdf_read::RenderSession;
use std::cmp::Ordering as CmpOrdering;
use std::io::Read;
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
    fn document_session(&self) -> Option<Arc<RenderSession>> {
        None
    }
    async fn load_page(&self, page_index: usize) -> Result<SourcePage>;

    async fn load_page_cancellable(
        &self,
        page_index: usize,
        _cancellation: lege_pdf_read::CancellationToken,
    ) -> Result<SourcePage> {
        self.load_page(page_index).await
    }
}

pub struct PdfPageSource {
    renderer: Arc<PdfRenderer>,
    config: Arc<PipelineConfig>,
}

impl PdfPageSource {
    pub fn new(pdf_bytes: Arc<[u8]>, config: Arc<PipelineConfig>) -> Result<Self> {
        let mut raster_cfg = PdfRasterConfig::default();
        raster_cfg.render_forms = false;
        raster_cfg.target_width = config.target_width();
        let renderer = Arc::new(PdfRenderer::new_from_bytes(pdf_bytes, raster_cfg)?);
        Ok(Self { renderer, config })
    }
}

#[async_trait]
impl PageSource for PdfPageSource {
    fn page_count(&self) -> usize {
        self.renderer.page_count() as usize
    }

    fn source_concurrency(&self) -> usize {
        std::thread::available_parallelism()
            .map(|threads| threads.get().saturating_sub(1).max(1))
            .unwrap_or(1)
    }

    fn document_session(&self) -> Option<Arc<RenderSession>> {
        Some(self.renderer.document_session())
    }

    async fn load_page(&self, page_index: usize) -> Result<SourcePage> {
        self.load_pdf_page(page_index, None).await
    }

    async fn load_page_cancellable(
        &self,
        page_index: usize,
        cancellation: lege_pdf_read::CancellationToken,
    ) -> Result<SourcePage> {
        self.load_pdf_page(page_index, Some(cancellation)).await
    }
}

impl PdfPageSource {
    async fn load_pdf_page(
        &self,
        page_index: usize,
        cancellation: Option<lege_pdf_read::CancellationToken>,
    ) -> Result<SourcePage> {
        // "Render high, resize low": render at the highest resolution any stage
        // needs (OCR when slow-OCR is enabled), then downstream stages resize
        // down to target_height for encoding. Equals target_height otherwise.
        let target_height = self.config.source_render_height();
        let target_width = self.config.source_render_width();
        let rgb_page = self
            .renderer
            .render_page_rgb_cancellable(
                page_index as u32,
                target_height,
                target_width,
                cancellation,
            )
            .await
            .map_err(|error| anyhow!("Failed to render page {page_index}: {error}"))?;

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
            // The renderer already read and returned this geometry while
            // calculating raster dimensions. Avoid a second page-tree lookup
            // for every source page.
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

#[derive(Clone, Debug)]
struct ZipImageEntry {
    archive_index: usize,
    name: String,
    uncompressed_size: u64,
}

/// Streams image entries directly from a ZIP into the image pipeline.
///
/// Each in-flight source task opens its own archive reader, extracts one entry
/// into memory, and decodes it on that same blocking worker. This lets JP2
/// decompression overlap with later pipeline stages without first materializing
/// an Archive.org `_jp2.zip` tree on disk.
pub struct ZipImagePageSource {
    zip_path: PathBuf,
    entries: Arc<Vec<ZipImageEntry>>,
    concurrency: usize,
    target_height: u32,
    target_width: Option<u32>,
}

impl ZipImagePageSource {
    pub fn open(
        zip_path: PathBuf,
        concurrency: usize,
        target_height: u32,
        target_width: Option<u32>,
    ) -> Result<Self> {
        let file = std::fs::File::open(&zip_path)
            .with_context(|| format!("Failed to open ZIP: {}", zip_path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("Failed to read ZIP: {}", zip_path.display()))?;
        let mut entries = Vec::new();
        for archive_index in 0..archive.len() {
            let entry = archive.by_index_raw(archive_index).with_context(|| {
                format!(
                    "Failed to inspect ZIP entry {} in {}",
                    archive_index,
                    zip_path.display()
                )
            })?;
            if !entry.is_dir() && is_supported_image(Path::new(entry.name())) {
                entries.push(ZipImageEntry {
                    archive_index,
                    name: entry.name().to_string(),
                    uncompressed_size: entry.size(),
                });
            }
        }
        let entries = select_primary_zip_image_set(entries);
        if entries.is_empty() {
            return Err(anyhow!(
                "No supported image files found in ZIP {}",
                zip_path.display()
            ));
        }
        Ok(Self {
            zip_path,
            entries: Arc::new(entries),
            concurrency: concurrency.max(1),
            target_height,
            target_width,
        })
    }
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
        let mut image =
            crate::runtime_stats::spawn_blocking(move || decode_folder_image(&decode_path))
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

#[async_trait]
impl PageSource for ZipImagePageSource {
    fn page_count(&self) -> usize {
        self.entries.len()
    }

    fn source_concurrency(&self) -> usize {
        self.concurrency
    }

    async fn load_page(&self, page_index: usize) -> Result<SourcePage> {
        let entry = self
            .entries
            .get(page_index)
            .cloned()
            .ok_or_else(|| anyhow!("ZIP image page index {} out of bounds", page_index))?;
        let zip_path = self.zip_path.clone();
        let mut image =
            crate::runtime_stats::spawn_blocking(move || decode_zip_image(&zip_path, &entry))
                .await
                .map_err(|e| anyhow!("ZIP image decode task panicked: {}", e))??;
        let (width, height) = image.dimensions();
        image = normalize_folder_image_to_target(
            image,
            self.target_height,
            self.target_width,
            page_index,
        )?;

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
    page_range: std::ops::Range<usize>,
    cancellation: lege_pdf_read::CancellationToken,
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
            let cancellation = cancellation.clone();
            let page_index = next_page;
            next_page += 1;

            in_flight.push(tokio::spawn(crate::runtime_stats::track_future(
                crate::runtime_stats::Stage::Render,
                async move {
                    #[cfg(feature = "debug-logging")]
                    crate::info_log!("[SourceStage] Loading page {}", page_index);
                    if cancellation.is_cancelled() {
                        return Err(anyhow!("Source stage cancelled before page {page_index}"));
                    }
                    let source_page = source
                        .load_page_cancellable(page_index, cancellation.clone())
                        .await
                        .map_err(|e| {
                            #[cfg(feature = "debug-logging")]
                            crate::error_println!(
                                "[SourceStage] Page {} load failed: {:#}",
                                page_index,
                                e
                            );
                            e
                        })?;
                    if cancellation.is_cancelled() {
                        return Err(anyhow!(
                            "Source stage cancelled after page {page_index} render"
                        ));
                    }
                    let SourcePage {
                        image,
                        original_width_pts,
                        original_height_pts,
                    } = source_page;

                    crate::pipeline::set_standard_dimensions_once(image.width(), image.height());

                    let high_res_arc = Arc::new(image);
                    let page_layout_enabled = config.layout_detection_enabled_for_page(page_index);
                    // In no-layout mode the inference image is never used; share the
                    // high-res Arc instead of deep-cloning a full RGB page per file.
                    let inference_image = if page_layout_enabled {
                        let spec = config.inference_resize_spec();
                        let img = build_inference_image(high_res_arc.as_ref(), &spec)
                            .unwrap_or_else(|_| (*high_res_arc).clone());
                        Arc::new(img)
                    } else {
                        high_res_arc.clone()
                    };
                    if cancellation.is_cancelled() {
                        return Err(anyhow!(
                            "Source stage cancelled after page {page_index} preparation"
                        ));
                    }

                    Ok::<_, anyhow::Error>(RenderedPageData {
                        index: page_index,
                        high_res_image: high_res_arc,
                        inference_image,
                        layout_detection_enabled: page_layout_enabled,
                        original_width_pts,
                        original_height_pts,
                    })
                },
            )));
        }

        let Some(result) = in_flight.next().await else {
            break;
        };
        let rendered = result.map_err(|e| anyhow!("Source stage task panicked: {}", e))??;
        if tx.send(rendered).await.is_err() {
            break;
        }

        let rendered_val = render_count.fetch_add(1, Ordering::Relaxed) + 1;
        if layout_enabled {
            let detected_val = detect_count.load(Ordering::Relaxed);
            let encoded_val = encode_count.load(Ordering::Relaxed);
            progress.publish_layout_progress(rendered_val, detected_val, encoded_val, total_pages);
        } else {
            progress.publish_no_layout_render_progress(rendered_val, total_pages);
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

    let total_found = all_files.len();

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

    let mut best_run = by_ext
        .into_iter()
        .max_by(|(ext_a, a), (ext_b, b)| {
            a.len()
                .cmp(&b.len())
                .then_with(|| (ext_a == "jp2").cmp(&(ext_b == "jp2")))
        })
        .map(|(_, files)| files)
        .unwrap_or_default();
    best_run.sort_by(|a, b| compare_image_names(a, b));

    if best_run.len() < total_found {
        // Prefer the dominant format so thumbnails/covers in another format do
        // not enter the page stream. Within that format, retain every page even
        // when Archive.org numbering has gaps.
        crate::error_println!(
            "Warning: {} of {} image files in {} were skipped because another image format \
formed the dominant page set. Set LEGE_FOLDER_SOURCE_DEBUG=1 to list the selected files.",
            total_found - best_run.len(),
            total_found,
            folder.display()
        );
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
            let (width, height, rgb_data) = crate::encoding::jp2::decode_jp2_bytes(&bytes)
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

const MAX_ZIP_IMAGE_BYTES: u64 = 512 * 1024 * 1024;

fn decode_zip_image(zip_path: &Path, selected: &ZipImageEntry) -> Result<RgbImage> {
    if selected.uncompressed_size > MAX_ZIP_IMAGE_BYTES {
        return Err(anyhow!(
            "ZIP image is too large to extract safely ({} bytes, limit {}): {}",
            selected.uncompressed_size,
            MAX_ZIP_IMAGE_BYTES,
            selected.name
        ));
    }
    let file = std::fs::File::open(zip_path)
        .with_context(|| format!("Failed to reopen ZIP: {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("Failed to read ZIP: {}", zip_path.display()))?;
    let entry = archive.by_index(selected.archive_index).with_context(|| {
        format!(
            "Failed to extract ZIP entry {} from {}",
            selected.name,
            zip_path.display()
        )
    })?;
    let mut bytes = Vec::with_capacity(selected.uncompressed_size.min(64 * 1024 * 1024) as usize);
    entry
        .take(MAX_ZIP_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to extract ZIP image: {}", selected.name))?;
    if bytes.len() as u64 > MAX_ZIP_IMAGE_BYTES {
        return Err(anyhow!(
            "ZIP image exceeded the {} byte safety limit while extracting: {}",
            MAX_ZIP_IMAGE_BYTES,
            selected.name
        ));
    }
    decode_image_bytes(Path::new(&selected.name), &bytes)
        .with_context(|| format!("Failed to decode ZIP image: {}", selected.name))
}

fn decode_image_bytes(name: &Path, bytes: &[u8]) -> Result<RgbImage> {
    let is_jp2 = name
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jp2"));
    if is_jp2 {
        #[cfg(feature = "jp2-lam")]
        {
            let (width, height, rgb_data) = crate::encoding::jp2::decode_jp2_bytes(bytes)
                .map_err(|e| anyhow!("JP2 cannot be decoded: {e}"))?;
            return RgbImage::from_raw(width, height, rgb_data)
                .ok_or_else(|| anyhow!("JP2 decoder returned an invalid RGB buffer"));
        }
        #[cfg(not(feature = "jp2-lam"))]
        return Err(anyhow!(
            "JP2 support not compiled in (enable jp2-lam feature)"
        ));
    }
    image::load_from_memory(bytes)
        .map_err(anyhow::Error::msg)
        .map(|image| image.to_rgb8())
}

fn select_primary_zip_image_set(entries: Vec<ZipImageEntry>) -> Vec<ZipImageEntry> {
    let mut by_ext: std::collections::HashMap<String, Vec<ZipImageEntry>> =
        std::collections::HashMap::new();
    for entry in entries {
        let ext = Path::new(&entry.name)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        by_ext.entry(ext).or_default().push(entry);
    }
    let mut selected = by_ext
        .into_iter()
        .max_by(|(ext_a, a), (ext_b, b)| {
            a.len()
                .cmp(&b.len())
                .then_with(|| (ext_a == "jp2").cmp(&(ext_b == "jp2")))
        })
        .map(|(_, entries)| entries)
        .unwrap_or_default();
    selected.sort_by(|a, b| compare_image_names(Path::new(&a.name), Path::new(&b.name)));
    selected
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

fn compare_image_names(a: &Path, b: &Path) -> CmpOrdering {
    let a_number = a
        .file_stem()
        .and_then(|value| value.to_str())
        .and_then(trailing_number);
    let b_number = b
        .file_stem()
        .and_then(|value| value.to_str())
        .and_then(trailing_number);
    match (a_number, b_number) {
        (Some(a_number), Some(b_number)) => a_number.cmp(&b_number).then_with(|| a.cmp(b)),
        (Some(_), None) => CmpOrdering::Less,
        (None, Some(_)) => CmpOrdering::Greater,
        (None, None) => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat};
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    fn temp_dir_with(names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lege_source_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for name in names {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        dir
    }

    fn stems(paths: &[PathBuf]) -> Vec<String> {
        let mut out: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    fn ordered_names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn write_image_zip(names: &[&str]) -> PathBuf {
        let dir = temp_dir_with(&[]);
        let zip_path = dir.join("archive_jp2.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        for (index, name) in names.iter().enumerate() {
            let image = RgbImage::from_pixel(2, 2, image::Rgb([index as u8, 20, 30]));
            let mut png = Cursor::new(Vec::new());
            DynamicImage::ImageRgb8(image)
                .write_to(&mut png, ImageFormat::Png)
                .unwrap();
            archive
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            archive.write_all(png.get_ref()).unwrap();
        }
        archive.finish().unwrap();
        zip_path
    }

    #[test]
    fn non_consecutive_numbering_keeps_every_page() {
        let dir = temp_dir_with(&["page_28.png", "page_50.png", "page_100.png"]);
        let run = collect_largest_sequential_image_run(&dir).unwrap();
        assert_eq!(
            ordered_names(&run),
            vec!["page_28.png", "page_50.png", "page_100.png"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn consecutive_numbering_keeps_all_pages() {
        let dir = temp_dir_with(&["page_1.png", "page_2.png", "page_3.png"]);
        let run = collect_largest_sequential_image_run(&dir).unwrap();
        assert_eq!(stems(&run), vec!["page_1.png", "page_2.png", "page_3.png"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn same_format_cover_is_retained_without_silent_data_loss() {
        let dir = temp_dir_with(&["cover.png", "page_10.png", "page_11.png", "page_12.png"]);
        let run = collect_largest_sequential_image_run(&dir).unwrap();
        assert_eq!(
            stems(&run),
            vec!["cover.png", "page_10.png", "page_11.png", "page_12.png"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn zip_source_streams_nested_images_in_numeric_order() {
        let zip_path = write_image_zip(&[
            "book_jp2/page_100.png",
            "book_jp2/page_28.png",
            "book_jp2/page_50.png",
            "cover.jpg",
        ]);
        let source = ZipImagePageSource::open(zip_path.clone(), 3, 2, None).unwrap();

        assert_eq!(source.page_count(), 3);
        assert_eq!(
            source
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "book_jp2/page_28.png",
                "book_jp2/page_50.png",
                "book_jp2/page_100.png"
            ]
        );
        let page = source.load_page(0).await.unwrap();
        assert_eq!(page.image.dimensions(), (2, 2));
        assert_eq!(page.image.get_pixel(0, 0).0, [1, 20, 30]);

        std::fs::remove_dir_all(zip_path.parent().unwrap()).ok();
    }
}
