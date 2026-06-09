use crate::{debug_println, pipeline::config::runtime_asset_path_if_exists};
use anyhow::{Result, anyhow};
use once_cell::sync::Lazy;
use pdfium_render::prelude::*;
use std::cell::RefCell;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct NativeTextWord {
    pub text: String,
    /// Bounding box in the source rendered page pixel space: [x1, y1, x2, y2].
    pub bbox: [f32; 4],
}

struct PdfiumWorkerContext {
    pdf_data: Arc<[u8]>,
    leaked_pdf_ptr: *const [u8],
    document: Option<PdfDocument<'static>>,
    render_forms: bool,
    cached_height: u32,
    cached_width: Option<u32>,
    config: PdfRenderConfig,
}

impl Drop for PdfiumWorkerContext {
    fn drop(&mut self) {
        if let Some(doc) = self.document.take() {
            drop(doc);
        }
        unsafe {
            // Reconstruct the leaked Arc to balance Arc::into_raw
            Arc::from_raw(self.leaked_pdf_ptr);
        }
    }
}

impl PdfiumWorkerContext {
    fn new(
        pdf_data: Arc<[u8]>,
        raster_cfg: &RasterConfig,
        target_height: u32,
        target_width: Option<u32>,
    ) -> Result<Self> {
        let _guard = PDFIUM_GLOBAL_LOCK
            .lock()
            .map_err(|e| anyhow!("Failed to acquire Pdfium lock: {}", e))?;
        let leaked_clone = Arc::clone(&pdf_data);
        let leaked_ptr = Arc::into_raw(leaked_clone);
        let bytes_ref: &'static [u8] = unsafe { &*leaked_ptr };
        let document = PDFIUM.load_pdf_from_byte_slice(bytes_ref, None)?;
        let config = Self::build_config(target_height, target_width, raster_cfg);
        Ok(Self {
            pdf_data,
            leaked_pdf_ptr: leaked_ptr,
            document: Some(document),
            render_forms: raster_cfg.render_forms,
            cached_height: target_height,
            cached_width: target_width.or(raster_cfg.target_width),
            config,
        })
    }

    fn matches(&self, pdf_data: &Arc<[u8]>, raster_cfg: &RasterConfig) -> bool {
        Arc::ptr_eq(&self.pdf_data, pdf_data) && self.render_forms == raster_cfg.render_forms
    }

    fn ensure_config(
        &mut self,
        target_height: u32,
        target_width: Option<u32>,
        raster_cfg: &RasterConfig,
    ) {
        let effective_width = target_width.or(raster_cfg.target_width);
        if self.cached_height == target_height
            && self.cached_width == effective_width
            && self.render_forms == raster_cfg.render_forms
        {
            return;
        }
        self.config = Self::build_config(target_height, target_width, raster_cfg);
        self.cached_height = target_height;
        self.cached_width = effective_width;
        self.render_forms = raster_cfg.render_forms;
    }

    fn render_page(&mut self, page_index: u32) -> Result<RgbPage> {
        let _guard = PDFIUM_GLOBAL_LOCK
            .lock()
            .map_err(|e| anyhow!("Failed to acquire Pdfium lock: {}", e))?;

        let document = self
            .document
            .as_ref()
            .ok_or_else(|| anyhow!("Pdfium document not available"))?;
        let page_total = document.pages().len() as u32;
        if page_index >= page_total {
            return Err(anyhow!(
                "Page index {} out of bounds (0..{})",
                page_index,
                page_total
            ));
        }
        let page = document.pages().get(page_index as u16)?;

        // Capture original PDF dimensions in points
        let page_width_pts = page.width().value;
        let page_height_pts = page.height().value;

        #[cfg(feature = "debug-logging")]
        {
            debug_println!(
                "[PageRender] Page {}: PDF dimensions = {:.2}x{:.2} pts, target = {}x{:?}, config cached = {}x{:?}",
                page_index,
                page_width_pts,
                page_height_pts,
                self.cached_height,
                self.cached_width,
                self.cached_height,
                self.cached_width
            );
        }

        let bitmap = page.render_with_config(&self.config)?;
        let img = bitmap.as_image().to_rgb8();

        #[cfg(feature = "debug-logging")]
        {
            debug_println!(
                "[PageRender] Page {}: Rendered dimensions = {}x{} pixels",
                page_index,
                img.width(),
                img.height()
            );
        }

        Ok(RgbPage {
            width: img.width(),
            height: img.height(),
            data: img.into_raw(),
            original_width_pts: page_width_pts,
            original_height_pts: page_height_pts,
        })
    }

    fn has_text_layer(&mut self, page_index: u32) -> Result<bool> {
        let _guard = PDFIUM_GLOBAL_LOCK
            .lock()
            .map_err(|e| anyhow!("Failed to acquire Pdfium lock: {}", e))?;

        let document = self
            .document
            .as_ref()
            .ok_or_else(|| anyhow!("Pdfium document not available"))?;
        let page_total = document.pages().len() as u32;
        if page_index >= page_total {
            return Err(anyhow!(
                "Page index {} out of bounds (0..{})",
                page_index,
                page_total
            ));
        }

        let page = document.pages().get(page_index as u16)?;

        // Extract page text and check for any non-whitespace characters.
        // If the page's text API returns an empty or whitespace-only string,
        // we consider the page as lacking a text layer.
        let text = page.text()?.all();
        Ok(text.chars().any(|c| !c.is_whitespace()))
    }

    fn extract_positioned_text_words(
        &mut self,
        page_index: u32,
        source_width: u32,
        source_height: u32,
    ) -> Result<Vec<NativeTextWord>> {
        let _guard = PDFIUM_GLOBAL_LOCK
            .lock()
            .map_err(|e| anyhow!("Failed to acquire Pdfium lock: {}", e))?;

        let document = self
            .document
            .as_ref()
            .ok_or_else(|| anyhow!("Pdfium document not available"))?;
        let page_total = document.pages().len() as u32;
        if page_index >= page_total {
            return Err(anyhow!(
                "Page index {} out of bounds (0..{})",
                page_index,
                page_total
            ));
        }

        let page = document.pages().get(page_index as u16)?;
        let page_width_pts = page.width().value.max(1.0);
        let page_height_pts = page.height().value.max(1.0);
        let text = page.text()?;
        let mut words = Vec::new();

        for segment in text.segments().iter() {
            let segment_text = segment.text();
            if segment_text.trim().is_empty() {
                continue;
            }
            let rect = segment.bounds();
            #[allow(deprecated)]
            let bbox = pdf_rect_to_pixels(
                rect.left.value,
                rect.bottom.value,
                rect.right.value,
                rect.top.value,
                page_width_pts,
                page_height_pts,
                source_width,
                source_height,
            );
            push_segment_words(&mut words, &segment_text, bbox);
        }

        Ok(words)
    }

    fn build_config(
        target_height: u32,
        target_width: Option<u32>,
        raster_cfg: &RasterConfig,
    ) -> PdfRenderConfig {
        let mut config = PdfRenderConfig::new().set_target_height(target_height as i32);
        if raster_cfg.render_forms {
            config = config.render_form_data(true).render_annotations(true);
        }
        if let Some(width) = target_width.or(raster_cfg.target_width) {
            config = config.set_target_width(width as i32);
        }
        config
    }
}

fn pdf_rect_to_pixels(
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
    page_width_pts: f32,
    page_height_pts: f32,
    source_width: u32,
    source_height: u32,
) -> [f32; 4] {
    let sx = source_width as f32 / page_width_pts;
    let sy = source_height as f32 / page_height_pts;
    [
        (left * sx).clamp(0.0, source_width as f32),
        ((page_height_pts - top) * sy).clamp(0.0, source_height as f32),
        (right * sx).clamp(0.0, source_width as f32),
        ((page_height_pts - bottom) * sy).clamp(0.0, source_height as f32),
    ]
}

fn push_segment_words(words: &mut Vec<NativeTextWord>, text: &str, bbox: [f32; 4]) {
    let total_chars = text.chars().count();
    if total_chars == 0 {
        return;
    }

    let width = (bbox[2] - bbox[0]).max(1.0);
    let mut current_word = String::new();
    let mut word_start = 0usize;

    for (char_index, ch) in text.chars().enumerate() {
        if ch.is_whitespace() {
            if !current_word.is_empty() {
                let x1 = bbox[0] + width * (word_start as f32 / total_chars as f32);
                let x2 = bbox[0] + width * (char_index as f32 / total_chars as f32);
                words.push(NativeTextWord {
                    text: std::mem::take(&mut current_word),
                    bbox: [x1, bbox[1], x2.max(x1 + 1.0).min(bbox[2]), bbox[3]],
                });
            }
        } else {
            if current_word.is_empty() {
                word_start = char_index;
            }
            current_word.push(ch);
        }
    }

    if !current_word.is_empty() {
        let x1 = bbox[0] + width * (word_start as f32 / total_chars as f32);
        let x2 = bbox[2];
        words.push(NativeTextWord {
            text: current_word,
            bbox: [x1, bbox[1], x2.max(x1 + 1.0).min(bbox[2]), bbox[3]],
        });
    }
}

thread_local! {
    static PDFIUM_THREAD_CONTEXT: RefCell<Option<PdfiumWorkerContext>> = RefCell::new(None);
}

// Global Pdfium instance - MUST be accessed through proper synchronization
pub static PDFIUM: Lazy<Pdfium> =
    Lazy::new(|| bind_pdfium_once().expect("Failed to initialize Pdfium"));

/// CRITICAL: Single global mutex for ALL Pdfium operations
/// Pdfium is not thread-safe for ANY concurrent operations, even on different documents
static PDFIUM_GLOBAL_LOCK: Lazy<std::sync::Mutex<()>> = Lazy::new(|| std::sync::Mutex::new(()));

fn bind_pdfium_from_runtime_dirs() -> Result<Box<dyn PdfiumLibraryBindings>, anyhow::Error> {
    // Only look in the runtime directory
    let lib_name = Pdfium::pdfium_platform_library_name();
    match runtime_asset_path_if_exists(lib_name.to_string_lossy().as_ref()) {
        Some(path) if path.exists() => Pdfium::bind_to_library(path)
            .map_err(|e| anyhow!("Failed to load PDFium library: {:?}", e)),
        _ => Err(anyhow!("PDFium library not found in runtime directory")),
    }
}

fn bind_pdfium_once() -> Result<Pdfium> {
    // Try to bind to PDFium library from runtime directory
    let bindings = bind_pdfium_from_runtime_dirs()?;
    let pdfium = Pdfium::new(bindings);

    // Verify we can create a document
    pdfium
        .create_new_pdf()
        .map_err(|e| anyhow!("Failed to create PDF document: {:?}", e))?;

    Ok(pdfium)
}

/// High-performance PDF renderer with proper synchronization
pub struct PdfiumRenderer {
    pdf_bytes: Arc<[u8]>,
    raster_cfg: RasterConfig,
    // Cache page count to avoid re-opening document
    page_count: u16,
}

impl PdfiumRenderer {
    /// Create new renderer and pre-cache page count
    pub fn new_from_bytes(pdf_bytes: Arc<[u8]>, raster_cfg: RasterConfig) -> Result<Self> {
        let page_count = Self::get_page_count_internal(&pdf_bytes)?;

        Ok(Self {
            pdf_bytes,
            raster_cfg,
            page_count,
        })
    }

    /// Internal method to get page count with proper locking
    fn get_page_count_internal(pdf_bytes: &[u8]) -> Result<u16> {
        let _guard = PDFIUM_GLOBAL_LOCK
            .lock()
            .map_err(|e| anyhow!("Failed to acquire Pdfium lock: {}", e))?;

        let document = PDFIUM.load_pdf_from_byte_slice(pdf_bytes, None)?;
        Ok(document.pages().len())
    }

    /// Extract the bookmark tree from this PDF. Returns an empty vec if there are no bookmarks.
    /// Must be called after all rendering is complete; acquires PDFIUM_GLOBAL_LOCK internally.
    pub fn extract_bookmarks(&self) -> Vec<OwnedBookmarkNode> {
        extract_bookmarks_from_bytes(&self.pdf_bytes)
    }

    /// Get cached page count - no locking required
    pub fn page_count(&self) -> u16 {
        self.page_count
    }

    /// High-performance page rendering with proper synchronization
    pub async fn render_page_rgb(
        &self,
        page_index: u32,
        target_height: u32,
        target_width: Option<u32>,
    ) -> Result<RgbPage> {
        if page_index >= self.page_count as u32 {
            return Err(anyhow!(
                "Page index {} out of bounds (0..{})",
                page_index,
                self.page_count
            ));
        }

        let pdf_bytes = self.pdf_bytes.clone();
        let raster_cfg = self.raster_cfg.clone();

        // Perform rendering in blocking task to avoid blocking async runtime
        tokio::task::spawn_blocking(move || {
            Self::render_page_blocking(
                pdf_bytes,
                page_index,
                target_height,
                target_width,
                &raster_cfg,
            )
        })
        .await?
    }

    /// Synchronous version of render_page_rgb for use in blocking contexts
    pub fn render_page_rgb_sync(
        &self,
        page_index: u32,
        target_height: u32,
        target_width: Option<u32>,
    ) -> Result<RgbPage> {
        if page_index >= self.page_count as u32 {
            return Err(anyhow!(
                "Page index {} out of bounds (0..{})",
                page_index,
                self.page_count
            ));
        }

        let pdf_bytes = self.pdf_bytes.clone();
        let raster_cfg = self.raster_cfg.clone();

        // Direct call to blocking function (no spawn_blocking to avoid task cancellation)
        Self::render_page_blocking(
            pdf_bytes,
            page_index,
            target_height,
            target_width,
            &raster_cfg,
        )
    }

    /// Returns true if the given page has any extractable text (non-whitespace).
    /// Uses the global Pdfium lock and the worker context to safely access Pdfium.
    pub async fn has_text_layer(&self, page_index: u32) -> Result<bool> {
        if page_index >= self.page_count as u32 {
            return Err(anyhow!(
                "Page index {} out of bounds (0..{})",
                page_index,
                self.page_count
            ));
        }

        let pdf_bytes = self.pdf_bytes.clone();
        let raster_cfg = self.raster_cfg.clone();

        tokio::task::spawn_blocking(move || {
            PDFIUM_THREAD_CONTEXT.with(|cell| -> Result<bool> {
                let mut borrowed = cell.borrow_mut();

                let should_rebuild = borrowed
                    .as_ref()
                    .map(|ctx| !ctx.matches(&pdf_bytes, &raster_cfg))
                    .unwrap_or(true);

                if should_rebuild {
                    // Build a minimal context; render settings are irrelevant for text checks.
                    *borrowed = Some(PdfiumWorkerContext::new(
                        pdf_bytes.clone(),
                        &raster_cfg,
                        1,
                        None,
                    )?);
                }

                let ctx = borrowed
                    .as_mut()
                    .expect("Pdfium worker context must be initialized");

                ctx.has_text_layer(page_index)
            })
        })
        .await?
    }

    /// Check if the document has any text layers (OCR) by sampling first pages.
    /// Samples up to 5 pages to balance accuracy and performance.
    /// Returns true if ANY sampled page contains non-whitespace text.
    pub async fn has_any_text_layer(&self) -> Result<bool> {
        let total_pages = self.page_count as u32;
        if total_pages == 0 {
            return Ok(false);
        }

        // Sample up to 5 pages spread across the document
        let sample_size = std::cmp::min(5, total_pages);
        let step = if total_pages <= 5 {
            1
        } else {
            total_pages / sample_size
        };

        for i in 0..sample_size {
            let page_index = i * step;
            if self.has_text_layer(page_index).await? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Extracts full page text for pass-through when OCR is disabled.
    pub async fn extract_page_text(&self, page_index: u32) -> Result<String> {
        if page_index >= self.page_count as u32 {
            return Err(anyhow!(
                "Page index {} out of bounds (0..{})",
                page_index,
                self.page_count
            ));
        }

        let pdf_bytes = self.pdf_bytes.clone();
        let raster_cfg = self.raster_cfg.clone();

        tokio::task::spawn_blocking(move || {
            PDFIUM_THREAD_CONTEXT.with(|cell| -> Result<String> {
                let mut borrowed = cell.borrow_mut();

                let should_rebuild = borrowed
                    .as_ref()
                    .map(|ctx| !ctx.matches(&pdf_bytes, &raster_cfg))
                    .unwrap_or(true);

                if should_rebuild {
                    *borrowed = Some(PdfiumWorkerContext::new(
                        pdf_bytes.clone(),
                        &raster_cfg,
                        1,
                        None,
                    )?);
                }

                let ctx = borrowed
                    .as_mut()
                    .expect("Pdfium worker context must be initialized");

                let _guard = PDFIUM_GLOBAL_LOCK
                    .lock()
                    .map_err(|e| anyhow!("Failed to acquire Pdfium lock: {}", e))?;

                let document = ctx
                    .document
                    .as_ref()
                    .ok_or_else(|| anyhow!("Pdfium document not available"))?;
                let page = document.pages().get(page_index as u16)?;
                let text = page.text()?.all();
                Ok(text)
            })
        })
        .await?
    }

    pub async fn extract_positioned_text_words(
        &self,
        page_index: u32,
        source_width: u32,
        source_height: u32,
    ) -> Result<Vec<NativeTextWord>> {
        if page_index >= self.page_count as u32 {
            return Err(anyhow!(
                "Page index {} out of bounds (0..{})",
                page_index,
                self.page_count
            ));
        }

        let pdf_bytes = self.pdf_bytes.clone();
        let raster_cfg = self.raster_cfg.clone();

        tokio::task::spawn_blocking(move || {
            PDFIUM_THREAD_CONTEXT.with(|cell| -> Result<Vec<NativeTextWord>> {
                let mut borrowed = cell.borrow_mut();

                let should_rebuild = borrowed
                    .as_ref()
                    .map(|ctx| !ctx.matches(&pdf_bytes, &raster_cfg))
                    .unwrap_or(true);

                if should_rebuild {
                    *borrowed = Some(PdfiumWorkerContext::new(
                        pdf_bytes.clone(),
                        &raster_cfg,
                        1,
                        None,
                    )?);
                }

                let ctx = borrowed
                    .as_mut()
                    .expect("Pdfium worker context must be initialized");

                ctx.extract_positioned_text_words(page_index, source_width, source_height)
            })
        })
        .await?
    }

    /// Blocking render implementation with proper Pdfium synchronization
    fn render_page_blocking(
        pdf_bytes: Arc<[u8]>,
        page_index: u32,
        target_height: u32,
        target_width: Option<u32>,
        raster_cfg: &RasterConfig,
    ) -> Result<RgbPage> {
        let start_time = Instant::now();
        let raster_cfg = raster_cfg.clone();

        let page = PDFIUM_THREAD_CONTEXT.with(|cell| -> Result<RgbPage> {
            let mut borrowed = cell.borrow_mut();

            let should_rebuild = borrowed
                .as_ref()
                .map(|ctx| !ctx.matches(&pdf_bytes, &raster_cfg))
                .unwrap_or(true);

            if should_rebuild {
                *borrowed = Some(PdfiumWorkerContext::new(
                    pdf_bytes.clone(),
                    &raster_cfg,
                    target_height,
                    target_width,
                )?);
            }

            let ctx = borrowed
                .as_mut()
                .expect("Pdfium worker context must be initialized");

            ctx.ensure_config(target_height, target_width, &raster_cfg);
            ctx.render_page(page_index)
        })?;

        debug_println!(
            "[PdfiumRenderer] page {}: {}x{} in {:?}",
            page_index,
            page.width,
            page.height,
            start_time.elapsed()
        );

        Ok(page)
    }

    /// Batch render multiple pages sequentially - most efficient for multiple pages
    pub async fn render_pages_rgb(
        &self,
        page_indices: &[u32],
        target_height: u32,
        target_width: Option<u32>,
    ) -> Result<Vec<RgbPage>> {
        let pdf_bytes = self.pdf_bytes.clone();
        let raster_cfg = self.raster_cfg.clone();
        let indices = page_indices.to_vec();

        tokio::task::spawn_blocking(move || {
            let mut results = Vec::with_capacity(indices.len());
            for &page_index in &indices {
                let rendered = Self::render_page_blocking(
                    pdf_bytes.clone(),
                    page_index,
                    target_height,
                    target_width,
                    &raster_cfg,
                )?;
                results.push(rendered);
            }
            Ok(results)
        })
        .await?
    }

    /// Clean shutdown
    pub fn shutdown(&self) -> Result<()> {
        debug_println!("PdfiumRenderer shutdown complete");
        Ok(())
    }
}

/// Configuration for PDF rendering
#[derive(Clone, Debug)]
pub struct RasterConfig {
    pub render_forms: bool,
    pub target_width: Option<u32>,
}

impl Default for RasterConfig {
    fn default() -> Self {
        Self {
            render_forms: true,
            target_width: None,
        }
    }
}

/// Rendered page result
#[derive(Debug, Clone)]
pub struct RgbPage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub original_width_pts: f32,
    pub original_height_pts: f32,
}

/// No-op for compatibility
pub fn maybe_run_pdfium_worker_and_exit() {
    // In-process rendering, no worker needed
}

pub mod prelude {
    pub use super::{PDFIUM, PdfiumRenderer, RasterConfig, RgbPage};
}

// Optional performance metrics
impl PdfiumRenderer {
    /// Get memory usage statistics (if available)
    pub fn memory_usage(&self) -> Result<String> {
        // Pdfium doesn't expose memory stats directly, but we can estimate
        let doc_size = self.pdf_bytes.len();
        let estimated_raster_memory = (self.page_count as usize) * 10 * 1024 * 1024; // ~10MB per page

        Ok(format!(
            "PDF: {}, Estimated raster: {}",
            format_memory_size(doc_size),
            format_memory_size(estimated_raster_memory)
        ))
    }
}

/// An owned representation of a single bookmark node in the outline tree.
#[derive(Debug, Clone)]
pub struct OwnedBookmarkNode {
    /// Bookmark title; empty string when pdfium returns None.
    pub title: String,
    /// 0-based source page index this bookmark points to.
    pub source_page: usize,
    pub children: Vec<OwnedBookmarkNode>,
}

/// Recursively walk pdfium bookmark tree starting from `node`.
/// `depth` guards against cycles or pathologically deep trees.
fn walk_bookmark<'a>(node: pdfium_render::prelude::PdfBookmark<'a>, depth: u32) -> OwnedBookmarkNode {
    let title = node.title().unwrap_or_default();

    let source_page = {
        let via_dest = node
            .destination()
            .and_then(|d| d.page_index().ok())
            .map(|idx| idx as usize);
        let via_action = node.action().and_then(|action| {
            action
                .as_local_destination_action()
                .and_then(|ld| ld.destination().ok())
                .and_then(|d| d.page_index().ok())
                .map(|idx| idx as usize)
        });
        via_dest.or(via_action)
    };

    let mut children = Vec::new();
    if depth < 64 {
        let mut child = node.first_child();
        while let Some(c) = child {
            let sibling = c.next_sibling();
            children.push(walk_bookmark(c, depth + 1));
            child = sibling;
        }
    }

    OwnedBookmarkNode {
        title,
        source_page: source_page.unwrap_or(usize::MAX),
        children,
    }
}

/// Extract the full bookmark tree from raw PDF bytes.
/// Returns an empty vec when there are no bookmarks or pdfium is unavailable.
/// Acquires PDFIUM_GLOBAL_LOCK; call only after all rendering is complete.
pub fn extract_bookmarks_from_bytes(pdf_bytes: &[u8]) -> Vec<OwnedBookmarkNode> {
    let guard = match PDFIUM_GLOBAL_LOCK.lock() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    let document = match PDFIUM.load_pdf_from_byte_slice(pdf_bytes, None) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let bookmarks = document.bookmarks();
    let first = match bookmarks.root() {
        Some(r) => r,
        None => return Vec::new(),
    };
    // Walk all top-level bookmarks (siblings of the first)
    let mut result = Vec::new();
    let mut current = Some(first);
    while let Some(node) = current {
        let sibling = node.next_sibling();
        result.push(walk_bookmark(node, 0));
        current = sibling;
    }
    drop(guard);
    result
}

fn format_memory_size(bytes: usize) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    if unit_index == 0 {
        format!("{} {}", bytes, UNITS[unit_index])
    } else {
        format!("{:.1} {}", size, UNITS[unit_index])
    }
}
