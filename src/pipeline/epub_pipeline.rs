//! EPUB pipeline: render → layout-detect → slow-OCR → reflowable EPUB.
//!
//! Unlike the PDF/DJVU pipelines this produces a text-only, reflowable document.
//! There is no binarization, image encoding, or page-image assembly — the bulk of
//! the work is OCR and turning layout-detected regions into paragraphs.
//!
//! Each detected text region becomes one block (≈ paragraph); blocks are emitted
//! in reading order (see `lege_ocr::reading_order`). Chapters are split only for
//! title-like detections whose page geometry looks like a real chapter opening:
//! a heading followed by a paragraph after a large vertical blank gap.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use anyhow::{Result, anyhow};
use epub_builder::{EpubBuilder, EpubContent, ReferenceType, ZipLibrary};
use tokio::sync::mpsc;

use lege_ocr::{
    OcrPipeline, SlowOcrConfig, SlowOcrOutputMode,
    coordinate::CoordinateMap,
    document::{BlockKind, PageText, TextBlock},
    normalize,
    types::TextRegion,
};

use crate::pipeline::config::PipelineConfig;
use crate::pipeline::pdf_tokio_pipeline::{CachedDetections, inference_stage_parallel};
use crate::pipeline::policies::maybe_remap_bbox_from_infer;
use crate::pipeline::runtime_limits::PipelineRuntimeLimits;
use crate::pipeline::source::{PageSource, PdfiumPageSource, source_stage};
use crate::progress::{ProcessingStatus, ProgressTracker};
use crate::types::ContentCategory;
use crate::{info_log, success_log, warn_log};

/// Run the full EPUB pipeline over a PDF byte buffer.
#[allow(clippy::too_many_arguments)]
pub async fn create_and_run_epub_pipeline(
    pdf_bytes: Arc<[u8]>,
    config: Arc<PipelineConfig>,
    output_path: &Path,
    page_range: Option<std::ops::Range<usize>>,
    progress_tracker: &ProgressTracker,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
    _progress_callback: impl Fn(usize, usize) + Send + Sync + 'static,
) -> Result<()> {
    info_log!("[EPUB] Starting EPUB pipeline");
    crate::pipeline::reset_standard_dimensions();

    let mut config = config;
    let source: Arc<dyn PageSource> = Arc::new(PdfiumPageSource::new(pdf_bytes, config.clone())?);

    let inference_handle = if config.enable_layout_detection() {
        match crate::pipeline::inference::InferenceHandle::new(&config) {
            Ok(handle) => Some(Arc::new(handle)),
            Err(e) if crate::pipeline::inference::is_layout_software_adapter_error(e.as_ref()) => {
                let msg = format!(
                    "No usable hardware GPU found — wgpu fell back to a CPU/software adapter. \
                     Layout detection has been disabled for this run. \
                     Install or update your GPU driver to enable hardware acceleration."
                );
                warn_log!("[EPUB] {msg}");
                progress_tracker.update(crate::progress::ProcessingStatus::PipelineMessage {
                    stage: "GPU Warning".to_string(),
                    message: msg,
                });
                let mut fallback = (*config).clone();
                fallback.set_enable_layout_detection(false);
                config = Arc::new(fallback);
                None
            }
            Err(e) if crate::pipeline::inference::is_gpu_device_error(e.as_ref()) => {
                let msg = format!(
                    "GPU initialization failed ({}). Layout detection disabled; \
                     processing will continue without it. \
                     Check that your GPU driver supports DX12 (Windows) or Vulkan (Linux/macOS).",
                    e
                );
                warn_log!("[EPUB] {msg}");
                progress_tracker.update(crate::progress::ProcessingStatus::PipelineMessage {
                    stage: "GPU Warning".to_string(),
                    message: msg,
                });
                let mut fallback = (*config).clone();
                fallback.set_enable_layout_detection(false);
                config = Arc::new(fallback);
                None
            }
            Err(e) => return Err(anyhow!("[EPUB] Failed to create InferenceHandle: {}", e)),
        }
    } else {
        None
    };

    let layout_enabled = config.enable_layout_detection();
    let limits = PipelineRuntimeLimits::from_config(&config);

    let document_pages = source.page_count();
    let page_start = page_range.as_ref().map(|r| r.start).unwrap_or(0);
    let page_end = page_range.as_ref().map(|r| r.end).unwrap_or(document_pages);
    let total_pages = page_end.saturating_sub(page_start);
    if total_pages == 0 {
        return Err(anyhow!("[EPUB] No pages to process"));
    }
    progress_tracker.update(ProcessingStatus::PipelineMessage {
        stage: "EPUB".to_string(),
        // Rendering, layout detection and OCR are pipelined and run concurrently;
        // a single up-front message sets that expectation. The live "OCR'd X/N"
        // heartbeat emitted from the OCR stage is the real start-to-finish signal.
        message: format!(
            "Processing {} pages (render + layout + OCR run concurrently)...",
            total_pages
        ),
    });

    // GPU resize only pays off past a page threshold (matches the PDF pipeline).
    const MIN_PAGES_FOR_GPU_RESIZE: usize = 10;
    crate::resize::set_gpu_resize_enabled(total_pages >= MIN_PAGES_FOR_GPU_RESIZE);

    let infer_concurrency = limits.page_workers.max(1);
    let ocr_concurrency = limits.page_workers.clamp(1, 4);

    let deskew_engine = crate::pipeline::prepare_shared_deskew_engine(&config)?;

    // Shared OCR pipeline (engine initialized once, reused across pages).
    let ocr_pipeline = Arc::new(OcrPipeline::new(SlowOcrConfig {
        language: config.ocr_language().to_string(),
        output_mode: SlowOcrOutputMode::Structured,
        binary_is_analysis_mask: true,
        ..Default::default()
    }));

    let render_count = Arc::new(AtomicUsize::new(0));
    let detect_count = Arc::new(AtomicUsize::new(0));
    let encode_count = Arc::new(AtomicUsize::new(0));

    let (render_tx, render_rx) = mpsc::channel(limits.render_buffer);
    let (infer_tx, infer_rx) = mpsc::channel(limits.inference_buffer);

    // Stage 1: render
    let mut render_task = tokio::spawn(source_stage(
        source.clone(),
        config.clone(),
        deskew_engine,
        page_start..page_end,
        render_tx,
        render_count.clone(),
        detect_count.clone(),
        encode_count.clone(),
        progress_tracker.clone(),
        total_pages,
        layout_enabled,
    ));

    // Stage 2: layout detection
    let detection_cache = Arc::new(Vec::<CachedDetections>::new());
    let mut infer_task = tokio::spawn(inference_stage_parallel(
        inference_handle,
        render_rx,
        infer_tx,
        detect_count.clone(),
        infer_concurrency,
        render_count.clone(),
        encode_count.clone(),
        progress_tracker.clone(),
        total_pages,
        layout_enabled,
        detection_cache,
        0,
    ));

    // Stage 3: OCR each page into structured PageText, collected in page order.
    let mut ocr_task = {
        let config = config.clone();
        let ocr_pipeline = ocr_pipeline.clone();
        let progress = progress_tracker.clone();
        tokio::spawn(async move {
            ocr_collect_stage(
                infer_rx,
                config,
                ocr_pipeline,
                ocr_concurrency,
                progress,
                total_pages,
            )
            .await
        })
    };

    // Drive stages, aborting downstream on cancellation.
    use crate::pipeline::helper_functions::await_stage_or_cancel;
    let h_infer = infer_task.abort_handle();
    let h_ocr = ocr_task.abort_handle();

    await_stage_or_cancel(
        &mut render_task,
        &mut shutdown_rx,
        "render",
        &[h_infer.clone(), h_ocr.clone()],
    )
    .await?;
    // Note: render/inference/OCR are pipelined, so these stage joins complete in
    // quick succession once the (sequential) render stage drains its backlog.
    // Emitting "Rendering complete → inference complete" as user-facing progress
    // here is misleading because OCR has been running the whole time; keep them as
    // debug logs and let the live OCR heartbeat report real progress instead.
    info_log!("[EPUB] Render stage complete");

    await_stage_or_cancel(
        &mut infer_task,
        &mut shutdown_rx,
        "inference",
        std::slice::from_ref(&h_ocr),
    )
    .await?;
    info_log!("[EPUB] Inference stage complete");

    // The OCR stage returns a value, so it cannot use the `Result<()>`-typed
    // `await_stage_or_cancel`; drive it with an inline shutdown select instead.
    let pages_map = tokio::select! {
        joined = &mut ocr_task => {
            joined.map_err(|e| anyhow!("[EPUB] OCR stage panicked: {e}"))??
        }
        signal = shutdown_rx.recv() => {
            ocr_task.abort();
            let msg = signal
                .ok()
                .and_then(|s| s.message)
                .unwrap_or_else(|| "User requested cancellation".to_string());
            return Err(anyhow!("Processing cancelled during ocr: {}", msg));
        }
    };
    info_log!("[EPUB] OCR stage complete ({} pages)", pages_map.len());
    let ocr_pages = pages_map.len();
    progress_tracker.publish_layout_progress(
        render_count.load(std::sync::atomic::Ordering::Relaxed),
        detect_count.load(std::sync::atomic::Ordering::Relaxed),
        ocr_pages,
        0,
        total_pages,
    );
    progress_tracker.update(ProcessingStatus::PipelineMessage {
        stage: "EPUB".to_string(),
        message: format!("OCR complete for {} pages. Assembling EPUB...", ocr_pages),
    });

    let pages: Vec<PageText> = pages_map.into_values().collect();

    // Assemble + package the EPUB (CPU/IO; off the async runtime).
    let title = output_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Document")
        .to_string();
    let output_path_buf = output_path.to_path_buf();
    progress_tracker.update(ProcessingStatus::AssemblingOutput);
    tokio::task::spawn_blocking(move || build_epub(&pages, &title, &output_path_buf))
        .await
        .map_err(|e| anyhow!("[EPUB] packaging task panicked: {e}"))??;

    success_log!("EPUB pipeline complete: {}", output_path.display());
    Ok(())
}

/// Consume inferred pages, OCR each into a `PageText`, and collect them ordered
/// by page index. OCR runs on the blocking pool with bounded concurrency.
async fn ocr_collect_stage(
    mut infer_rx: mpsc::Receiver<crate::pipeline::pdf_tokio_pipeline::PdfInferenceData>,
    config: Arc<PipelineConfig>,
    ocr_pipeline: Arc<OcrPipeline>,
    concurrency: usize,
    progress: ProgressTracker,
    total_pages: usize,
) -> Result<BTreeMap<usize, PageText>> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
    let mut in_flight: FuturesUnordered<tokio::task::JoinHandle<Result<PageText>>> =
        FuturesUnordered::new();
    let mut results: BTreeMap<usize, PageText> = BTreeMap::new();

    // Coarse, release-visible heartbeat. OCR is the final (and on Linux the
    // slowest) stage, so its completed-page count is the single best indicator of
    // overall progress. Report in ~5% steps to avoid per-page log spam.
    let report_step = (total_pages / 20).max(1);
    let mut last_reported = 0usize;
    let mut report_ocr_progress = |done: usize| {
        if done > last_reported && (done - last_reported >= report_step || done == total_pages) {
            last_reported = done;
            progress.update(ProcessingStatus::PipelineMessage {
                stage: "EPUB".to_string(),
                message: format!("OCR'd {done}/{total_pages} pages..."),
            });
        }
    };

    loop {
        tokio::select! {
            biased;
            Some(joined) = in_flight.next(), if !in_flight.is_empty() => {
                let page: PageText = joined
                    .map_err(|e| anyhow!("[EPUB] OCR task panicked: {e}"))??;
                results.insert(page.page_index, page);
                report_ocr_progress(results.len());
            }
            maybe = infer_rx.recv() => {
                match maybe {
                    Some(data) => {
                        let permit = semaphore.clone().acquire_owned().await
                            .map_err(|_| anyhow!("[EPUB] OCR semaphore closed"))?;
                        let config = config.clone();
                        let ocr_pipeline = ocr_pipeline.clone();
                        in_flight.push(tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            ocr_page(&config, &ocr_pipeline, data)
                        }));
                    }
                    None => break,
                }
            }
        }
    }

    // Drain remaining in-flight OCR tasks.
    while let Some(joined) = in_flight.next().await {
        let page: PageText = joined.map_err(|e| anyhow!("[EPUB] OCR task panicked: {e}"))??;
        results.insert(page.page_index, page);
        report_ocr_progress(results.len());
    }

    Ok(results)
}

/// OCR a single inferred page into structured text blocks.
fn ocr_page(
    config: &PipelineConfig,
    ocr_pipeline: &OcrPipeline,
    data: crate::pipeline::pdf_tokio_pipeline::PdfInferenceData,
) -> Result<PageText> {
    let page_index = data.inference_result.index;
    let high_res = data.inference_result.high_res_image.clone();
    let page_w = high_res.width();
    let page_h = high_res.height();

    // Map layout detections from inference space into page-pixel space and keep
    // only OCR-worthy EPUB content regions.
    let spec = config.inference_resize_spec();
    let regions = text_regions_from_detections(
        &data.inference_result.detections,
        page_index,
        page_w,
        page_h,
        &spec,
    );

    let binary = normalize::binarize_page(&high_res);
    let coord_map = CoordinateMap::identity(page_w, page_h, page_w as f32, page_h as f32);
    let slow_page =
        ocr_pipeline.process_page(&high_res, &binary, &regions, &coord_map, page_index)?;

    Ok(PageText {
        page_index,
        blocks: slow_page.blocks,
        width_px: page_w,
        height_px: page_h,
    })
}

fn text_regions_from_detections(
    detections: &[crate::engine::Detection],
    page_index: usize,
    page_w: u32,
    page_h: u32,
    spec: &crate::pipeline::policies::InferenceResizeSpec,
) -> Vec<TextRegion> {
    let classifier = crate::types::LabelClassifier::default();
    let mut regions: Vec<TextRegion> = Vec::new();
    for det in detections {
        if !classifier.should_process_with_ocr(det)
            || matches!(det.category, ContentCategory::Abandon)
            || is_epub_skippable_class(det.class_name.as_deref())
        {
            continue;
        }
        let mapped = maybe_remap_bbox_from_infer(det.bbox, page_w, page_h, spec);
        let bbox = [
            mapped[0].floor().clamp(0.0, page_w as f32) as u32,
            mapped[1].floor().clamp(0.0, page_h as f32) as u32,
            mapped[2].ceil().clamp(0.0, page_w as f32) as u32,
            mapped[3].ceil().clamp(0.0, page_h as f32) as u32,
        ];
        if bbox[2] > bbox[0] && bbox[3] > bbox[1] {
            let region_id = regions.len();
            regions.push(TextRegion {
                page_index,
                region_id,
                class_name: det.class_name.clone(),
                bbox_highres: bbox,
                confidence: det.confidence,
            });
        }
    }

    regions
}

// ── Chapter assembly + XHTML ────────────────────────────────────────────────

struct Chapter {
    title: Option<String>,
    /// XHTML body fragment (already escaped).
    body: String,
}

const CHAPTER_BLANK_GAP_RATIO: f32 = 0.20;
const MAX_VERTICAL_SPACING_EM: f32 = 14.0;
const MAX_HORIZONTAL_MARGIN_PCT: f32 = 45.0;

#[derive(Debug, Clone, Copy)]
struct TextFrame {
    left: u32,
    right: u32,
}

/// Block kinds that are page furniture rather than reading content.
fn is_skippable(kind: &BlockKind) -> bool {
    matches!(kind, BlockKind::Header | BlockKind::Footer)
        || matches!(kind, BlockKind::Other(name)
            if matches!(name.as_str(), "page" | "page_number" | "number" | "seal" | "stamp"))
}

fn is_epub_skippable_class(class_name: Option<&str>) -> bool {
    is_skippable(&BlockKind::from_class_name(class_name.unwrap_or("text")))
}

fn is_heading(kind: &BlockKind) -> bool {
    matches!(kind, BlockKind::Title | BlockKind::SectionHeading)
}

fn is_paragraph_text(kind: &BlockKind) -> bool {
    matches!(kind, BlockKind::Paragraph)
}

fn vertical_gap_px(upper: &TextBlock, lower: &TextBlock) -> u32 {
    lower.bbox[1].saturating_sub(upper.bbox[3])
}

fn gap_ratio(gap_px: u32, page_height_px: u32) -> f32 {
    if page_height_px == 0 {
        0.0
    } else {
        gap_px as f32 / page_height_px as f32
    }
}

fn is_geometric_chapter_start(page: &PageText, block_index: usize) -> bool {
    let Some(heading) = page.blocks.get(block_index) else {
        return false;
    };
    if !is_heading(&heading.kind) || heading.is_empty() {
        return false;
    }

    page.blocks
        .iter()
        .skip(block_index + 1)
        .filter(|block| !is_skippable(&block.kind) && !block.is_empty())
        .find(|block| is_paragraph_text(&block.kind))
        .is_some_and(|paragraph| {
            gap_ratio(vertical_gap_px(heading, paragraph), page.height_px)
                >= CHAPTER_BLANK_GAP_RATIO
        })
}

fn blocks_with_layout_geometry(page: &PageText) -> Vec<(usize, TextBlock)> {
    let visible_indices: Vec<usize> = page
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            (!is_skippable(&block.kind) && !block.is_empty()).then_some(index)
        })
        .collect();
    let mut blocks: Vec<(usize, TextBlock)> = visible_indices
        .iter()
        .map(|&index| (index, page.blocks[index].clone()))
        .collect();
    let frame = text_frame(&blocks, page.width_px);

    for i in 0..blocks.len() {
        let source_index = visible_indices[i];
        let spacing = if i == 0 {
            page.blocks[source_index].bbox[1]
        } else if i > 0 {
            vertical_gap_px(&blocks[i - 1].1, &blocks[i].1)
        } else {
            0
        };
        let first_line_left = blocks[i]
            .1
            .lines
            .iter()
            .find(|line| !line.text.trim().is_empty())
            .map(|line| line.bbox[0])
            .unwrap_or(blocks[i].1.bbox[0]);
        blocks[i].1.spacing_before_px = spacing;
        blocks[i].1.indent_px = first_line_left.saturating_sub(frame.left);
        blocks[i].1.spacing_after_px = frame.right.saturating_sub(blocks[i].1.bbox[2]);
    }

    blocks
}

fn text_frame(blocks: &[(usize, TextBlock)], page_width_px: u32) -> TextFrame {
    let mut left = page_width_px;
    let mut right = 0;
    for (_, block) in blocks {
        if block.bbox[2] <= block.bbox[0] {
            continue;
        }
        left = left.min(block.bbox[0]);
        right = right.max(block.bbox[2]);
    }

    if left < right {
        TextFrame { left, right }
    } else {
        TextFrame {
            left: 0,
            right: page_width_px,
        }
    }
}

fn layout_style(block: &TextBlock, page: &PageText) -> String {
    let mut declarations = Vec::new();

    if block.spacing_before_px > 0 {
        let em = (block.spacing_before_px as f32 / 48.0).clamp(0.0, MAX_VERTICAL_SPACING_EM);
        if em > 0.0 {
            declarations.push(format!("margin-top:{em:.2}em"));
        }
    }

    if page.width_px > 0 && block.bbox[2] > block.bbox[0] {
        let block_w = block.bbox[2].saturating_sub(block.bbox[0]);
        let frame_w = block
            .indent_px
            .saturating_add(block_w)
            .saturating_add(block.spacing_after_px);
        let frame_w = frame_w.max(1) as f32;
        let left = (block.indent_px as f32 / frame_w * 100.0).clamp(0.0, MAX_HORIZONTAL_MARGIN_PCT);
        let right =
            (block.spacing_after_px as f32 / frame_w * 100.0).clamp(0.0, MAX_HORIZONTAL_MARGIN_PCT);
        if left > 0.0 {
            declarations.push(format!("margin-left:{left:.2}%"));
        }
        if right > 0.0 {
            declarations.push(format!("margin-right:{right:.2}%"));
        }
    }

    if declarations.is_empty() {
        String::new()
    } else {
        format!(" style=\"{}\"", declarations.join(";"))
    }
}

/// Render one block's text into an XHTML element string.
fn block_to_xhtml(block: &TextBlock, page: &PageText) -> String {
    let text = xml_escape(&block.paragraph_text());
    if text.trim().is_empty() {
        return String::new();
    }
    let style = layout_style(block, page);
    match &block.kind {
        BlockKind::Title => format!("<h1{style}>{text}</h1>\n"),
        BlockKind::SectionHeading => format!("<h2{style}>{text}</h2>\n"),
        BlockKind::Caption => format!("<p class=\"caption\"{style}>{text}</p>\n"),
        BlockKind::Footnote => format!("<p class=\"footnote\"{style}>{text}</p>\n"),
        BlockKind::Formula => format!("<p class=\"formula\"{style}>{text}</p>\n"),
        BlockKind::Table => format!("<p class=\"table\"{style}>{text}</p>\n"),
        _ => format!("<p{style}>{text}</p>\n"),
    }
}

/// Heading text used as a chapter/TOC title.
fn heading_title(block: &TextBlock) -> String {
    let t = block.paragraph_text();
    let t = t.trim();
    if t.is_empty() {
        "Untitled".to_string()
    } else {
        t.to_string()
    }
}

/// Split the document's blocks into chapters only at heading pages with enough
/// blank vertical space before body text to look like an intentional chapter
/// opening. Other detected headings remain headings inside the running flow.
fn assemble_chapters(pages: &[PageText]) -> Vec<Chapter> {
    let mut chapters: Vec<Chapter> = Vec::new();
    let mut current: Option<Chapter> = None;

    for page in pages {
        let blocks = blocks_with_layout_geometry(page);
        for (block_index, block) in blocks.iter() {
            if is_geometric_chapter_start(page, *block_index) {
                // Close the running chapter and open a new one titled by this heading.
                if let Some(ch) = current.take()
                    && !ch.body.trim().is_empty()
                {
                    chapters.push(ch);
                }
                let mut body = String::new();
                body.push_str(&block_to_xhtml(block, page));
                current = Some(Chapter {
                    title: Some(heading_title(block)),
                    body,
                });
            } else {
                let chapter = current.get_or_insert_with(|| Chapter {
                    title: None,
                    body: String::new(),
                });
                chapter.body.push_str(&block_to_xhtml(block, page));
            }
        }
    }
    if let Some(ch) = current.take()
        && !ch.body.trim().is_empty()
    {
        chapters.push(ch);
    }

    chapters
}

/// Wrap a chapter body fragment in a complete XHTML document.
fn render_xhtml(chapter: &Chapter) -> String {
    let title = xml_escape(chapter.title.as_deref().unwrap_or("Document"));
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE html>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\">\n\
         <head><meta charset=\"utf-8\"/><title>{title}</title></head>\n\
         <body>\n{}</body>\n</html>\n",
        chapter.body
    )
}

/// Package chapters into an EPUB file at `output_path`.
fn build_epub(pages: &[PageText], title: &str, output_path: &Path) -> Result<()> {
    let chapters = assemble_chapters(pages);
    if chapters.is_empty() {
        return Err(anyhow!(
            "[EPUB] No text content recognized — refusing to write an empty EPUB"
        ));
    }

    let zip = ZipLibrary::new().map_err(|e| anyhow!("[EPUB] zip init failed: {e}"))?;
    let mut builder =
        EpubBuilder::new(zip).map_err(|e| anyhow!("[EPUB] builder init failed: {e}"))?;
    builder
        .metadata("title", title)
        .map_err(|e| anyhow!("[EPUB] metadata title failed: {e}"))?;
    builder
        .metadata("author", "Lege")
        .map_err(|e| anyhow!("[EPUB] metadata author failed: {e}"))?;
    let _ = builder.metadata("lang", "en");

    for (i, chapter) in chapters.iter().enumerate() {
        let filename = format!("chapter_{:04}.xhtml", i + 1);
        let content = render_xhtml(chapter);
        let mut item = EpubContent::new(filename, content.as_bytes()).reftype(ReferenceType::Text);
        if let Some(t) = &chapter.title {
            item = item.title(t.clone());
        }
        builder
            .add_content(item)
            .map_err(|e| anyhow!("[EPUB] add_content failed: {e}"))?;
    }

    // Inline a navigation TOC at the start of the book.
    builder.inline_toc();

    let mut file = std::fs::File::create(output_path)
        .map_err(|e| anyhow!("[EPUB] cannot create {}: {e}", output_path.display()))?;
    builder
        .generate(&mut file)
        .map_err(|e| anyhow!("[EPUB] generate failed: {e}"))?;
    Ok(())
}

/// Minimal XML/XHTML text escaping.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Detection;
    use crate::pipeline::policies::InferenceResizeSpec;
    use crate::types::ContentCategory;
    use lege_ocr::document::TextLine;

    fn page(blocks: Vec<TextBlock>) -> PageText {
        PageText {
            page_index: 0,
            blocks,
            width_px: 1000,
            height_px: 1400,
        }
    }

    fn blk(kind: BlockKind, text: &str) -> TextBlock {
        blk_at(kind, text, [0, 0, 0, 0])
    }

    fn blk_at(kind: BlockKind, text: &str, bbox: [u32; 4]) -> TextBlock {
        TextBlock {
            kind,
            bbox,
            lines: vec![TextLine {
                text: text.to_string(),
                bbox,
                words: Vec::new(),
            }],
            indent_px: 0,
            spacing_before_px: 0,
            spacing_after_px: 0,
        }
    }

    fn det(class_name: &str, category: ContentCategory) -> Detection {
        Detection {
            class_id: 2,
            class_name: Some(class_name.to_string()),
            confidence: 0.9,
            bbox: [10.0, 20.0, 80.0, 90.0],
            category,
            context: None,
        }
    }

    #[test]
    fn headings_without_large_blank_gap_do_not_split_chapters() {
        let pages = vec![page(vec![
            blk_at(BlockKind::Title, "Chapter One", [100, 100, 900, 160]),
            blk_at(BlockKind::Paragraph, "Body of one.", [100, 190, 900, 500]),
            blk_at(
                BlockKind::SectionHeading,
                "Section Two",
                [100, 560, 900, 610],
            ),
            blk_at(BlockKind::Paragraph, "Body of two.", [100, 635, 900, 900]),
        ])];
        let chapters = assemble_chapters(&pages);
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].title.as_deref(), None);
        assert!(chapters[0].body.contains("Chapter One</h1>"));
        assert!(chapters[0].body.contains("Body of one.</p>"));
        assert!(chapters[0].body.contains("Section Two</h2>"));
        assert!(chapters[0].body.contains("Body of two.</p>"));
    }

    #[test]
    fn splits_chapters_on_heading_with_large_blank_gap_before_paragraph() {
        let pages = vec![page(vec![
            blk_at(BlockKind::Title, "Chapter One", [100, 120, 900, 180]),
            blk_at(BlockKind::Paragraph, "Body of one.", [100, 500, 900, 900]),
        ])];
        let chapters = assemble_chapters(&pages);
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].title.as_deref(), Some("Chapter One"));
        assert!(
            chapters[0]
                .body
                .contains("<h1 style=\"margin-top:2.50em\">Chapter One</h1>")
        );
        assert!(
            chapters[0]
                .body
                .contains("<p style=\"margin-top:6.67em\">Body of one.</p>")
        );
    }

    #[test]
    fn page_outer_margins_do_not_become_epub_margins() {
        let pages = vec![
            page(vec![blk_at(
                BlockKind::Paragraph,
                "Centered crop page.",
                [100, 100, 900, 300],
            )]),
            PageText {
                page_index: 1,
                blocks: vec![blk_at(
                    BlockKind::Paragraph,
                    "Offset crop page.",
                    [240, 100, 840, 300],
                )],
                width_px: 1200,
                height_px: 1400,
            },
        ];

        let chapters = assemble_chapters(&pages);
        assert_eq!(chapters.len(), 1);
        assert!(
            chapters[0]
                .body
                .contains("<p style=\"margin-top:2.08em\">Centered crop page.</p>")
        );
        assert!(
            chapters[0]
                .body
                .contains("<p style=\"margin-top:2.08em\">Offset crop page.</p>")
        );
        assert!(!chapters[0].body.contains("margin-left"));
        assert!(!chapters[0].body.contains("margin-right"));
    }

    #[test]
    fn falls_back_to_single_flow_without_geometric_chapters() {
        let pages = vec![
            page(vec![blk(BlockKind::Paragraph, "Page one text.")]),
            PageText {
                page_index: 1,
                blocks: vec![blk(BlockKind::Paragraph, "Page two text.")],
                width_px: 1000,
                height_px: 1400,
            },
        ];
        let chapters = assemble_chapters(&pages);
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].title.as_deref(), None);
        assert!(chapters[0].body.contains("Page one text."));
        assert!(chapters[0].body.contains("Page two text."));
    }

    #[test]
    fn skips_headers_and_footers() {
        let pages = vec![page(vec![
            blk(BlockKind::Header, "running head"),
            blk(BlockKind::Paragraph, "real content"),
            blk(BlockKind::Footer, "page 3"),
        ])];
        let chapters = assemble_chapters(&pages);
        assert_eq!(chapters.len(), 1);
        assert!(chapters[0].body.contains("real content"));
        assert!(!chapters[0].body.contains("running head"));
        assert!(!chapters[0].body.contains("page 3"));
    }

    #[test]
    fn no_layout_text_regions_stays_empty() {
        let regions =
            text_regions_from_detections(&[], 0, 100, 100, &InferenceResizeSpec::default());
        assert!(
            regions.is_empty(),
            "EPUB should not synthesize a whole-page OCR region when layout finds no text"
        );
    }

    #[test]
    fn epub_region_filter_skips_page_furniture_before_ocr() {
        let detections = vec![
            det("header", ContentCategory::Text),
            det("footer", ContentCategory::Text),
            det("number", ContentCategory::Text),
            det("seal", ContentCategory::Text),
            det("image", ContentCategory::Image),
            det("abandon", ContentCategory::Abandon),
            det("text", ContentCategory::Text),
            det("table", ContentCategory::Table),
        ];

        let regions =
            text_regions_from_detections(&detections, 0, 100, 100, &InferenceResizeSpec::default());
        let classes: Vec<&str> = regions
            .iter()
            .filter_map(|r| r.class_name.as_deref())
            .collect();

        assert_eq!(classes, vec!["text", "table"]);
    }

    #[test]
    fn build_epub_rejects_pages_without_content() {
        let pages = vec![page(vec![])];
        let mut path = std::env::temp_dir();
        path.push(format!("lege_epub_empty_test_{}.epub", std::process::id()));

        let err = build_epub(&pages, "Empty Book", &path)
            .expect_err("empty EPUB content should be rejected");
        assert!(err.to_string().contains("No text content recognized"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn escapes_xml_special_chars() {
        assert_eq!(xml_escape("a<b>&\"c"), "a&lt;b&gt;&amp;&quot;c");
    }

    #[test]
    fn build_epub_writes_valid_zip() {
        let pages = vec![page(vec![
            blk(BlockKind::Title, "Hello"),
            blk(BlockKind::Paragraph, "World & <friends>."),
        ])];
        let mut path = std::env::temp_dir();
        path.push(format!("lege_epub_test_{}.epub", std::process::id()));

        build_epub(&pages, "Test Book", &path).expect("epub generation should succeed");

        let bytes = std::fs::read(&path).expect("epub file should exist");
        // EPUB is a ZIP archive: first bytes are the local-file-header magic "PK\x03\x04".
        assert!(bytes.len() > 4, "epub should not be empty");
        assert_eq!(&bytes[0..2], b"PK", "epub must be a zip archive");

        let _ = std::fs::remove_file(&path);
    }
}
