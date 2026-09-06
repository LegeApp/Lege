use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{
    BlendMode, CompiledPage, DeviceRect, DeviceSize, DisplayOp, ImageCodecKind, ImageSMask, Matrix,
    PageFeatures, Point,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderColorPolicy, RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::{CpuBackend, CpuBackendOptions, CpuWorkerContext, DecodedBilevelSmask};

use crate::intake::source;

struct WorkerContexts {
    session_id: Option<u64>,
    parse: ParseContext,
    raster: CpuWorkerContext,
}

thread_local! {
    /// Rayon has fixed worker threads, so these contexts retain parser caches
    /// and raster scratch across page jobs without cross-thread locking.
    static WORKER_CONTEXTS: RefCell<WorkerContexts> = RefCell::new(WorkerContexts {
        session_id: None,
        parse: ParseContext::new(),
        raster: CpuWorkerContext::new(),
    });
}

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

impl WorkerContexts {
    fn select_document(&mut self, session_id: u64) {
        if self.session_id != Some(session_id) {
            self.session_id = Some(session_id);
            self.parse = ParseContext::new();
            self.raster = CpuWorkerContext::new();
        }
    }
}

/// Lege-owned cooperative cancellation token. The renderer token stays private
/// to this crate while page jobs can cheaply clone, cancel, and checkpoint it.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(pdf_render_api::CancellationToken);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// Errors at the Lege document seam. Renderer error types do not escape.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("failed to open PDF document: {0}")]
    Open(String),
    #[error("page index {page} is out of range (document has {page_count} pages)")]
    PageOutOfRange { page: u32, page_count: u32 },
    #[error("failed to compile PDF page {page}: {message}")]
    Compile { page: u32, message: String },
    #[error("invalid raster product: {0}")]
    InvalidRasterProduct(&'static str),
    #[error("failed to render PDF page {page}: {message}")]
    Render { page: u32, message: String },
    #[error("invalid export options: {0}")]
    InvalidExportOptions(&'static str),
    #[error("exporting a {width}x{height} image exceeds the {max_pixels}-pixel budget")]
    ExportTooLarge {
        width: u32,
        height: u32,
        max_pixels: u64,
    },
    #[error("failed to encode image: {0}")]
    Encode(String),
}

/// Visible page geometry in PDF user space.
///
/// `crop_box` is the visible page and drives every method that does not name
/// a box explicitly — that is what a viewer shows and what this crate has
/// always meant by "the page". The other four boxes are carried for callers
/// that need a different extent (printing to the trim area, prepress); each
/// is clamped to the MediaBox and defaults to the CropBox when the document
/// omits it, so they are always populated and never degenerate on their own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageGeometry {
    pub crop_box: [f64; 4],
    /// The physical sheet the page was authored for.
    pub media_box: [f64; 4],
    /// Trim plus bleed allowance; equals `crop_box` when absent.
    pub bleed_box: [f64; 4],
    /// The finished page after trimming; equals `crop_box` when absent.
    pub trim_box: [f64; 4],
    /// The author-declared meaningful content extent; equals `crop_box` when
    /// absent.
    pub art_box: [f64; 4],
    pub rotate: u16,
}

impl PageGeometry {
    /// Width of the *crop* box, before `/Rotate`.
    pub fn width(self) -> f64 {
        (self.crop_box[2] - self.crop_box[0]).max(0.0)
    }

    /// Height of the *crop* box, before `/Rotate`.
    pub fn height(self) -> f64 {
        (self.crop_box[3] - self.crop_box[1]).max(0.0)
    }

    /// Width of the *crop* box as displayed, i.e. after `/Rotate`.
    pub fn display_width(self) -> f64 {
        display_extent(self.width(), self.height(), self.rotate).0
    }

    /// Height of the *crop* box as displayed, i.e. after `/Rotate`.
    pub fn display_height(self) -> f64 {
        display_extent(self.width(), self.height(), self.rotate).1
    }

    /// The displayed `(width, height)` of an arbitrary page box of this page,
    /// i.e. its extent with the page's `/Rotate` applied.
    ///
    /// Callers pick the box — pass one of the fields above — so this crate
    /// needs no opinion about which box a given consumer means.
    /// `display_width`/`display_height` are exactly this applied to
    /// `crop_box`.
    pub fn display_size_of_box(self, b: [f64; 4]) -> (f64, f64) {
        display_extent((b[2] - b[0]).max(0.0), (b[3] - b[1]).max(0.0), self.rotate)
    }
}

/// A page rotated by an odd multiple of 90 degrees is displayed with its axes
/// swapped. Every page-to-device mapping in this crate goes through here so
/// that rule is stated once.
pub(crate) fn display_extent(width: f64, height: f64, rotate: u16) -> (f64, f64) {
    if rotate % 180 == 0 {
        (width, height)
    } else {
        (height, width)
    }
}

/// An immutable compiled page whose renderer representation remains private.
#[derive(Debug, Clone)]
pub struct CompiledDocumentPage {
    page_index: u32,
    raster: Arc<CompiledPage>,
}

impl CompiledDocumentPage {
    pub fn page_index(&self) -> u32 {
        self.page_index
    }

    pub fn operation_count(&self) -> usize {
        self.raster.operations.len()
    }

    /// The render height at which this page's *bilevel* raster comes out
    /// pixel for pixel: the full-page-equivalent pixel height of the
    /// one-bit image covering the page. `None` for every other page — one
    /// with no image over it, or one whose image carries continuous tone.
    ///
    /// A page that is already black and white holds no more detail at any
    /// scale, and rendering it larger only invents edges: the interpolation
    /// lands differently on each impression of a letter, so shapes that
    /// should match no longer do. A greyscale or colour scan is the
    /// opposite — its soft edges resolve *better* the finer they are
    /// sampled — so it has no ceiling here.
    pub fn bilevel_raster_height(&self) -> Option<u32> {
        bilevel_raster_height(&self.raster)
    }

    pub fn gray_suitability(&self) -> GraySuitability {
        let risky = PageFeatures::TRANSPARENCY
            | PageFeatures::SOFT_MASKS
            | PageFeatures::ICC_COLOR
            | PageFeatures::NONSEPARABLE_BLENDS
            | PageFeatures::OVERPRINT;
        if self.raster.features.intersects(risky) {
            GraySuitability::ColorFallback
        } else if self
            .raster
            .features
            .intersects(PageFeatures::IMAGES | PageFeatures::PATTERNS | PageFeatures::SHADINGS)
        {
            GraySuitability::AcceptableForBilevel
        } else {
            GraySuitability::Exact
        }
    }
}

/// Pixel format requested from a compiled document page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RasterFormat {
    Rgb8,
    Gray8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceCrop {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Full-page raster size the crop coordinates are expressed in. The render
    /// matrix must scale the page to THIS size before translating by (x, y);
    /// it cannot be inferred from the crop rect (that only works for a crop
    /// anchored at the page's bottom-right corner).
    pub page_width: u32,
    pub page_height: u32,
}

/// One final-size raster requested by the Lege processing pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterProduct {
    pub width: u32,
    pub height: u32,
    pub format: RasterFormat,
    pub crop: Option<DeviceCrop>,
}

impl RasterProduct {
    pub fn rgb8(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            format: RasterFormat::Rgb8,
            crop: None,
        }
    }

    pub fn gray8(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            format: RasterFormat::Gray8,
            crop: None,
        }
    }

    pub fn with_crop(mut self, crop: DeviceCrop) -> Self {
        self.crop = Some(crop);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraySuitability {
    Exact,
    AcceptableForBilevel,
    ColorFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisTarget {
    pub product: RasterProduct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseTarget {
    pub product: RasterProduct,
    pub gray_suitability: GraySuitability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionTarget {
    pub product: RasterProduct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrTarget {
    pub product: RasterProduct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageOutputPlan {
    pub analysis: Option<AnalysisTarget>,
    pub base: BaseTarget,
    pub regions: Vec<RegionTarget>,
    pub ocr: Option<OcrTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbSurface {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub pixels: Arc<[u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraySurface {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub pixels: Arc<[u8]>,
}

/// Lege-owned host raster. Renderer surface types do not leave this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RasterPlane {
    Rgb8(RgbSurface),
    Gray8(GraySurface),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedRegion {
    pub target: RegionTarget,
    pub plane: RasterPlane,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRasterProducts {
    pub base: RasterPlane,
    pub regions: Vec<RenderedRegion>,
    pub ocr: Option<RasterPlane>,
}

/// Which decoded JBIG2 sample value the output stencil must paint. An image
/// `/SMask` uses 1 as opaque, while `/ImageMask` uses 0 as paint, so the usual
/// source `/Decode [0 1]` becomes an output stencil `/Decode [1 0]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jbig2StencilPolarity {
    PaintZero,
    PaintOne,
}

/// Source-preserving foreground text plane for a conservatively qualified MRC
/// page. Encoded bytes stay authoritative; `working_coverage` is a separate
/// area-averaged opacity plane on the target grid, used only by processing
/// and analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedJbig2Smask {
    pub page_data: Arc<[u8]>,
    pub global_data: Arc<[u8]>,
    pub native_width: u32,
    pub native_height: u32,
    pub stencil_polarity: Jbig2StencilPolarity,
    pub working_coverage: GraySurface,
}

/// Per-document immutable renderer state.
#[derive(Debug)]
pub struct RenderSession {
    id: u64,
    pub(crate) snapshot: Arc<DocumentSnapshot>,
    pub(crate) compiler: Arc<PageCompiler>,
    // Constructed once per document now so Phase 3 does not accidentally
    // create page-local font, glyph, and decoded-image caches.
    backend: Arc<CpuBackend>,
}

impl RenderSession {
    pub fn open(bytes: Arc<[u8]>, password: Option<&str>) -> Result<Self, ReadError> {
        let snapshot = DocumentSnapshot::open_with_password(
            source(bytes),
            DocumentLimits::default(),
            password,
        )
        .map_err(|error| ReadError::Open(error.to_string()))?;

        Ok(Self {
            id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            snapshot: Arc::new(snapshot),
            compiler: Arc::new(PageCompiler::new().with_annotations(true)),
            backend: Arc::new(CpuBackend::new(CpuBackendOptions::default())),
        })
    }

    pub fn page_count(&self) -> u32 {
        self.snapshot.page_count()
    }

    pub fn page_geometry(&self, page: u32) -> Result<PageGeometry, ReadError> {
        let page_ref = self
            .snapshot
            .page(PageIndex(page))
            .map_err(|_| self.page_out_of_range(page))?;
        Ok(PageGeometry {
            crop_box: page_ref.crop_box,
            media_box: page_ref.media_box,
            bleed_box: page_ref.bleed_box,
            trim_box: page_ref.trim_box,
            art_box: page_ref.art_box,
            rotate: page_ref.rotate,
        })
    }

    pub fn compile(&self, page: u32) -> Result<Arc<CompiledDocumentPage>, ReadError> {
        if page >= self.page_count() {
            return Err(self.page_out_of_range(page));
        }

        let compiled = WORKER_CONTEXTS
            .with(|worker| {
                let mut worker = worker.borrow_mut();
                worker.select_document(self.id);
                self.compiler
                    .compile(&self.snapshot, PageIndex(page), &mut worker.parse)
            })
            .map_err(|error| ReadError::Compile {
                page,
                message: error.to_string(),
            })?;
        Ok(Arc::new(CompiledDocumentPage {
            page_index: page,
            raster: Arc::new(compiled),
        }))
    }

    pub fn render(
        &self,
        page: &Arc<CompiledDocumentPage>,
        product: &RasterProduct,
    ) -> Result<RasterPlane, ReadError> {
        self.render_cancellable(page, product, None)
    }

    pub fn render_cancellable(
        &self,
        page: &Arc<CompiledDocumentPage>,
        product: &RasterProduct,
        cancellation: Option<&CancellationToken>,
    ) -> Result<RasterPlane, ReadError> {
        if product.width == 0 || product.height == 0 {
            return Err(ReadError::InvalidRasterProduct(
                "width and height must both be non-zero",
            ));
        }

        let output_format = match product.format {
            RasterFormat::Rgb8 => OutputFormat::Rgba8PremultipliedSrgb,
            RasterFormat::Gray8 => OutputFormat::Gray8,
        };
        let request = RenderRequest {
            page: Arc::clone(&page.raster),
            transform: PageTransform {
                matrix: product_matrix(&page.raster, product),
            },
            crop: product.crop.map(|crop| DeviceRect {
                x: crop.x,
                y: crop.y,
                width: crop.width,
                height: crop.height,
            }),
            output_size: DeviceSize {
                width: product.width,
                height: product.height,
            },
            output_format,
            background: Background::White,
            color_policy: RenderColorPolicy::Original,
            annotations: AnnotationMode::StaticAppearances,
            quality: RenderQuality::Normal,
            limits: render_limits(cancellation),
            residency: OutputResidency::HostRequired,
        };

        let rendered = catch_unwind(AssertUnwindSafe(|| {
            WORKER_CONTEXTS.with(|worker| {
                let mut worker = worker.borrow_mut();
                worker.select_document(self.id);
                self.backend
                    .render_with(&request, &mut worker.raster)
                    .map(|(page, _stats)| page)
            })
        }))
        .map_err(|payload| ReadError::Render {
            page: page.page_index,
            message: format!(
                "renderer panicked: {}",
                pdf_render_api::panic_message(payload)
            ),
        })?
        .map_err(|error| ReadError::Render {
            page: page.page_index,
            message: error.to_string(),
        })?;
        if rendered.format != output_format {
            return Err(ReadError::Render {
                page: page.page_index,
                message: format!(
                    "renderer returned {:?} for a {:?} request",
                    rendered.format, output_format
                ),
            });
        }

        match product.format {
            RasterFormat::Gray8 => Ok(RasterPlane::Gray8(GraySurface {
                width: rendered.width,
                height: rendered.height,
                stride: rendered.stride,
                pixels: rendered.pixels,
            })),
            RasterFormat::Rgb8 => Ok(RasterPlane::Rgb8(rgb_surface_from_rgba(
                &rendered,
                page.page_index,
            )?)),
        }
    }

    pub fn render_output_plan(
        &self,
        page: &Arc<CompiledDocumentPage>,
        plan: &PageOutputPlan,
        cancellation: Option<&CancellationToken>,
    ) -> Result<PageRasterProducts, ReadError> {
        let base = self.render_cancellable(page, &plan.base.product, cancellation)?;
        let regions = plan
            .regions
            .iter()
            .map(|target| {
                Ok(RenderedRegion {
                    target: target.clone(),
                    plane: self.render_cancellable(page, &target.product, cancellation)?,
                })
            })
            .collect::<Result<Vec<_>, ReadError>>()?;
        let ocr = plan
            .ocr
            .as_ref()
            .map(|target| self.render_cancellable(page, &target.product, cancellation))
            .transpose()?;
        Ok(PageRasterProducts { base, regions, ocr })
    }

    /// Return the native JBIG2 `/SMask` payload plus a target-grid working
    /// opacity plane when this compiled page matches Lege's strict MRC shape.
    /// Any structural or decode mismatch is a normal `None` and leaves the
    /// caller free to run the existing raster/binarization path for that page.
    pub fn preserved_jbig2_smask(
        &self,
        page: &Arc<CompiledDocumentPage>,
        target_width: u32,
        target_height: u32,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Option<PreservedJbig2Smask>, ReadError> {
        if target_width == 0 || target_height == 0 {
            return Err(ReadError::InvalidRasterProduct(
                "preserved mask target dimensions must be non-zero",
            ));
        }
        let Some(candidate) = qualifying_jbig2_smask(&page.raster) else {
            return Ok(None);
        };
        let decoded = self
            .backend
            .decode_bilevel_jbig2_smask(&candidate.smask, &render_limits(cancellation))
            .map_err(|error| ReadError::Render {
                page: page.page_index,
                message: format!("JBIG2 SMask decode failed: {error}"),
            })?;
        let Some(decoded) = decoded else {
            return Ok(None);
        };
        let working_coverage = resize_smask_coverage(decoded, target_width, target_height)
            .ok_or_else(|| ReadError::Render {
                page: page.page_index,
                message: "JBIG2 SMask decoder returned a truncated opacity plane".to_string(),
            })?;
        Ok(Some(PreservedJbig2Smask {
            page_data: candidate.page_data,
            global_data: candidate
                .smask
                .codec_parms
                .as_ref()
                .and_then(|parms| parms.jbig2_globals.clone())
                .unwrap_or_else(|| Arc::from([])),
            native_width: candidate.smask.width,
            native_height: candidate.smask.height,
            stencil_polarity: candidate.stencil_polarity,
            working_coverage,
        }))
    }

    fn page_out_of_range(&self, page: u32) -> ReadError {
        ReadError::PageOutOfRange {
            page,
            page_count: self.page_count(),
        }
    }
}

fn render_limits(cancellation: Option<&CancellationToken>) -> RenderLimits {
    RenderLimits {
        cancellation: cancellation.map(|token| token.0.clone()),
        ..RenderLimits::default()
    }
}

/// Drop the alpha channel from a premultiplied-sRGB RGBA surface into a tightly
/// packed RGB one. The renderer's stride may exceed `width * 4`, so rows are
/// copied individually rather than the buffer being reinterpreted wholesale.
fn rgb_surface_from_rgba(rendered: &HostPage, page_index: u32) -> Result<RgbSurface, ReadError> {
    let row_bytes = usize::try_from(rendered.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or(ReadError::InvalidRasterProduct("RGBA row size overflow"))?;
    let rgb_stride = usize::try_from(rendered.width)
        .ok()
        .and_then(|width| width.checked_mul(3))
        .ok_or(ReadError::InvalidRasterProduct("RGB row size overflow"))?;
    let capacity = rgb_stride
        .checked_mul(rendered.height as usize)
        .ok_or(ReadError::InvalidRasterProduct("RGB surface size overflow"))?;

    let malformed = |message: &str| ReadError::Render {
        page: page_index,
        message: message.to_string(),
    };
    let mut pixels = Vec::with_capacity(capacity);
    for row in 0..rendered.height as usize {
        let start = row
            .checked_mul(rendered.stride)
            .ok_or_else(|| malformed("renderer row offset overflow"))?;
        let end = start
            .checked_add(row_bytes)
            .ok_or_else(|| malformed("renderer row end overflow"))?;
        let row = rendered
            .pixels
            .get(start..end)
            .ok_or_else(|| malformed("renderer returned a truncated RGBA surface"))?;
        pixels.extend(row.chunks_exact(4).flat_map(|pixel| &pixel[..3]));
    }
    Ok(RgbSurface {
        width: rendered.width,
        height: rendered.height,
        stride: rgb_stride,
        pixels: Arc::from(pixels),
    })
}

/// Resample a 0/255 opacity plane onto the target grid by exact area
/// averaging: each target pixel is the coverage of the source region it
/// spans, with the fractional edge rows and columns weighted by their
/// overlap. That is what a coverage plane means, so there is no ringing and
/// no overshoot, and the separable integer passes cost about a tenth of the
/// Lanczos3 resize this replaced (which was the whole preserved-mask probe
/// on a 6 Mpx page).
fn resize_smask_coverage(
    decoded: DecodedBilevelSmask,
    target_width: u32,
    target_height: u32,
) -> Option<GraySurface> {
    let (source_width, source_height) = (decoded.width as usize, decoded.height as usize);
    if decoded.stride != source_width
        || source_width == 0
        || source_height == 0
        || target_width == 0
        || target_height == 0
    {
        return None;
    }
    let source = decoded.opacity.get(..source_width * source_height)?;
    if (target_width as usize, target_height as usize) == (source_width, source_height) {
        return Some(GraySurface {
            width: target_width,
            height: target_height,
            stride: source_width,
            pixels: Arc::clone(&decoded.opacity),
        });
    }
    let (tw, th) = (target_width as usize, target_height as usize);
    let columns = coverage_spans(source_width, tw);
    let rows = coverage_spans(source_height, th);

    // Horizontal pass: per source row, the weighted column sums. Weights are
    // in units of 1/tw source pixel and add up to `source_width` per target
    // column, so the sum stays below 255 * source_width.
    let mut horizontal = vec![0u32; source_height * tw];
    for (source_row, sums) in source
        .chunks_exact(source_width)
        .zip(horizontal.chunks_exact_mut(tw))
    {
        for (sum, span) in sums.iter_mut().zip(&columns) {
            *sum = span
                .taps()
                .map(|(i, weight)| weight * u32::from(source_row[i]))
                .sum();
        }
    }
    // Vertical pass: the same over the row sums, then one exact rounded
    // divide by the pixel's full source area.
    let denominator = (source_width * source_height) as u64;
    let mut pixels = vec![0u8; tw * th];
    for (out_row, span) in pixels.chunks_exact_mut(tw).zip(&rows) {
        for (x, out) in out_row.iter_mut().enumerate() {
            let sum: u64 = span
                .taps()
                .map(|(i, weight)| u64::from(weight) * u64::from(horizontal[i * tw + x]))
                .sum();
            *out = ((sum + denominator / 2) / denominator).min(255) as u8;
        }
    }
    Some(GraySurface {
        width: target_width,
        height: target_height,
        stride: tw,
        pixels: Arc::from(pixels),
    })
}

/// The source pixels one target pixel spans along an axis, with each tap's
/// overlap in units of 1/`target` source pixel. Target pixel `j` covers the
/// source interval `[j*source/target, (j+1)*source/target)`; measured in
/// those units it is `[j*source, (j+1)*source)` and source pixel `i` is
/// `[i*target, (i+1)*target)`, so every overlap is an exact integer and the
/// weights of one span add up to `source`.
#[derive(Clone, Copy)]
struct CoverageSpan {
    first: usize,
    last: usize,
    first_weight: u32,
    last_weight: u32,
    interior_weight: u32,
}

impl CoverageSpan {
    fn taps(&self) -> impl Iterator<Item = (usize, u32)> + '_ {
        (self.first..=self.last).map(move |i| {
            let weight = if self.first == self.last {
                self.first_weight
            } else if i == self.first {
                self.first_weight
            } else if i == self.last {
                self.last_weight
            } else {
                self.interior_weight
            };
            (i, weight)
        })
    }
}

fn coverage_spans(source: usize, target: usize) -> Vec<CoverageSpan> {
    (0..target)
        .map(|j| {
            let start = j * source;
            let end = (j + 1) * source;
            let first = start / target;
            let last = ((end - 1) / target).min(source - 1);
            let overlap = |i: usize| -> u32 {
                let lo = start.max(i * target);
                let hi = end.min((i + 1) * target);
                hi.saturating_sub(lo) as u32
            };
            CoverageSpan {
                first,
                last,
                first_weight: overlap(first),
                last_weight: overlap(last),
                interior_weight: target as u32,
            }
        })
        .collect()
}

#[derive(Clone)]
struct Jbig2SmaskCandidate {
    smask: Arc<ImageSMask>,
    /// The non-empty JBIG2 page stream taken from `smask`. Carrying it here
    /// makes "qualified" and "has a usable payload" one fact instead of two.
    page_data: Arc<[u8]>,
    stencil_polarity: Jbig2StencilPolarity,
}

fn qualifying_jbig2_smask(page: &CompiledPage) -> Option<Jbig2SmaskCandidate> {
    if page.bounds.rotate != 0 {
        return None;
    }
    let mut ctm = Matrix::IDENTITY;
    let mut stack = Vec::new();
    let mut candidate = None;
    let mut image_draws = 0usize;

    for op in page.operations.iter() {
        match op {
            DisplayOp::Save => stack.push(ctm),
            DisplayOp::Restore => ctm = stack.pop()?,
            DisplayOp::ConcatTransform(matrix) => ctm = matrix.then(ctm),
            DisplayOp::BeginPaintOrigin(_) | DisplayOp::EndPaintOrigin => {}
            // Render mode 3 is invisible text, the searchable layer a scanner
            // lays over the page image. It leaves no ink, so it cannot disturb
            // the preserved mask; every other glyph run hits the reject arm.
            DisplayOp::DrawGlyphRun { run, .. }
                if page.glyph_runs.get(run.index())?.render_mode == 3 => {}
            DisplayOp::DrawImage {
                image,
                transform,
                alpha,
                blend,
                ..
            } => {
                image_draws += 1;
                let image = page.images.get(image.index())?;
                let placement = transform.then(ctm);
                if *blend != BlendMode::Normal
                    || (*alpha - 1.0).abs() > f32::EPSILON
                    || !is_unflipped_full_page(placement, page, image.width, image.height)
                    || image.is_stencil
                    || image.lowering_degraded
                    || image.mask.is_some()
                    || image.smask_in_data != 0
                {
                    return None;
                }
                if let Some(smask) = image.smask.as_ref() {
                    if candidate.is_some()
                        || smask.width != image.width
                        || smask.height != image.height
                        || smask.bits_per_component != 1
                        || smask.codec != Some(ImageCodecKind::Jbig2)
                        || !smask.samples.is_empty()
                    {
                        return None;
                    }
                    let page_data = smask.codec_data.clone().filter(|data| !data.is_empty())?;
                    let stencil_polarity = match smask.decode.as_deref() {
                        None => Jbig2StencilPolarity::PaintOne,
                        Some([[lo, hi]]) if *lo == 0.0 && *hi == 1.0 => {
                            Jbig2StencilPolarity::PaintOne
                        }
                        Some([[lo, hi]]) if *lo == 1.0 && *hi == 0.0 => {
                            Jbig2StencilPolarity::PaintZero
                        }
                        _ => return None,
                    };
                    candidate = Some(Jbig2SmaskCandidate {
                        smask: Arc::clone(smask),
                        page_data: Arc::from(page_data.as_slice()),
                        stencil_polarity,
                    });
                }
            }
            _ => return None,
        }
    }
    if !stack.is_empty() || image_draws == 0 {
        return None;
    }
    candidate
}

/// See [`CompiledDocumentPage::bilevel_raster_height`]. An image counts when
/// it covers at least this share of the page's shorter side in both axes: a
/// scan covers the page, a logo or a figure does not.
const PAGE_IMAGE_COVERAGE: f64 = 0.6;

fn bilevel_raster_height(page: &CompiledPage) -> Option<u32> {
    let crop = page.bounds.crop;
    let (page_w, page_h) = (crop.width().abs(), crop.height().abs());
    if !(page_w > 0.0 && page_h > 0.0) {
        return None;
    }
    let mut ctm = Matrix::IDENTITY;
    let mut stack = Vec::new();
    let mut ink: Option<u32> = None;
    for op in page.operations.iter() {
        match op {
            DisplayOp::Save => stack.push(ctm),
            DisplayOp::Restore => ctm = stack.pop().unwrap_or(Matrix::IDENTITY),
            DisplayOp::ConcatTransform(matrix) => ctm = matrix.then(ctm),
            DisplayOp::DrawImage {
                image, transform, ..
            } => {
                let Some(image) = page.images.get(image.index()) else {
                    continue;
                };
                let placement = transform.then(ctm);
                if !placement.is_finite() {
                    continue;
                }
                // The unit image square's extent on the page.
                let drawn_w = placement.a.abs() + placement.c.abs();
                let drawn_h = placement.b.abs() + placement.d.abs();
                if drawn_h <= 0.0
                    || drawn_w < page_w * PAGE_IMAGE_COVERAGE
                    || drawn_h < page_h * PAGE_IMAGE_COVERAGE
                {
                    continue;
                }
                // The ink layer: the image itself when it is one bit, else
                // the one-bit mask painted through it. A continuous-tone
                // image with no such mask is not a ceiling — it is a scan,
                // and it resolves better the finer it is sampled.
                let bilevel = if image.is_stencil || image.bits_per_component == 1 {
                    Some((image.width, image.height))
                } else {
                    image
                        .smask
                        .as_ref()
                        .filter(|smask| smask.bits_per_component == 1)
                        .map(|smask| (smask.width, smask.height))
                };
                let Some((width, height)) = bilevel.filter(|(_, height)| *height > 0) else {
                    continue;
                };
                // A quarter-turned placement puts the image's width along the
                // page's height.
                let along_page_height = if placement.b.abs() > placement.d.abs() {
                    width
                } else {
                    height
                };
                let implied = (f64::from(along_page_height) * page_h / drawn_h).round();
                if implied.is_finite() && implied >= 1.0 {
                    let implied = implied as u32;
                    ink = Some(ink.map_or(implied, |n: u32| n.max(implied)));
                }
            }
            _ => {}
        }
    }
    ink
}

fn is_unflipped_full_page(
    matrix: Matrix,
    page: &CompiledPage,
    image_width: u32,
    image_height: u32,
) -> bool {
    if !matrix.is_finite()
        || matrix.a <= 0.0
        || matrix.d <= 0.0
        || matrix.b.abs() > 1e-7
        || matrix.c.abs() > 1e-7
    {
        return false;
    }
    let p0 = matrix.apply(Point { x: 0.0, y: 0.0 });
    let p1 = matrix.apply(Point { x: 1.0, y: 1.0 });
    let crop = page.bounds.crop;
    let numeric_tolerance = (crop.width().abs().max(crop.height().abs()) * 1e-5).max(1e-4);
    // PDF producers commonly place a full-page raster on half-pixel boundaries.
    // That is still the same full-page image, and rejecting it prevents lossless
    // preservation of an otherwise qualifying native JBIG2 soft mask.
    let tolerance_x = numeric_tolerance.max(matrix.a.abs() / f64::from(image_width.max(1)) * 0.5);
    let tolerance_y = numeric_tolerance.max(matrix.d.abs() / f64::from(image_height.max(1)) * 0.5);
    (p0.x - crop.x0).abs() <= tolerance_x
        && (p0.y - crop.y0).abs() <= tolerance_y
        && (p1.x - crop.x1).abs() <= tolerance_x
        && (p1.y - crop.y1).abs() <= tolerance_y
}

fn page_to_device_matrix(page: &CompiledPage, output_width: u32, output_height: u32) -> Matrix {
    let crop = page.bounds.crop;
    let rotation = page.bounds.rotate % 360;
    let (display_width, display_height) = display_extent(crop.width(), crop.height(), rotation);
    let scale_x = output_width as f64 / display_width.max(f64::EPSILON);
    let scale_y = output_height as f64 / display_height.max(f64::EPSILON);

    match rotation {
        90 => Matrix {
            a: 0.0,
            b: scale_y,
            c: scale_x,
            d: 0.0,
            e: -crop.y0 * scale_x,
            f: -crop.x0 * scale_y,
        },
        180 => Matrix {
            a: -scale_x,
            b: 0.0,
            c: 0.0,
            d: scale_y,
            e: crop.x1 * scale_x,
            f: -crop.y0 * scale_y,
        },
        270 => Matrix {
            a: 0.0,
            b: -scale_y,
            c: -scale_x,
            d: 0.0,
            e: crop.y1 * scale_x,
            f: crop.x1 * scale_y,
        },
        _ => Matrix {
            a: scale_x,
            b: 0.0,
            c: 0.0,
            d: -scale_y,
            e: -crop.x0 * scale_x,
            f: crop.y1 * scale_y,
        },
    }
}

fn product_matrix(page: &CompiledPage, product: &RasterProduct) -> Matrix {
    let Some(crop) = product.crop else {
        return page_to_device_matrix(page, product.width, product.height);
    };
    let mut matrix = page_to_device_matrix(page, crop.page_width.max(1), crop.page_height.max(1));
    let sx = product.width as f64 / crop.width.max(1) as f64;
    let sy = product.height as f64 / crop.height.max(1) as f64;
    matrix.a *= sx;
    matrix.c *= sx;
    matrix.e = (matrix.e - crop.x as f64) * sx;
    matrix.b *= sy;
    matrix.d *= sy;
    matrix.f = (matrix.f - crop.y as f64) * sy;
    matrix
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pdf_page_ir::{
        CodecParms, ImageColorSpace, ImageId, ImageIr, InterpolationMode, PageBounds, PaintId,
        Rect, ResourceKey,
    };

    fn eligible_page() -> CompiledPage {
        let smask = Arc::new(ImageSMask {
            width: 12,
            height: 5,
            bits_per_component: 1,
            decode: None,
            samples: Arc::from([]),
            codec: Some(ImageCodecKind::Jbig2),
            codec_data: Some((&b"encoded mask"[..]).into()),
            codec_parms: Some(CodecParms {
                jbig2_globals: Some(Arc::from(&b"encoded globals"[..])),
                ..CodecParms::default()
            }),
        });
        let image = ImageIr {
            key: ResourceKey {
                object_number: 1,
                generation: 0,
                variant: 0,
            },
            width: 12,
            height: 5,
            is_stencil: false,
            interpolation: InterpolationMode::Nearest,
            soft_mask: None,
            bits_per_component: 8,
            color_space: ImageColorSpace::Gray,
            decode: None,
            samples: Some(Arc::from(&b"base"[..])),
            codec: None,
            codec_data: None,
            codec_parms: None,
            smask: Some(smask),
            mask: None,
            smask_in_data: 0,
            lowering_degraded: false,
        };
        let mut page = CompiledPage::empty(PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 120.0,
                y1: 50.0,
            },
            rotate: 0,
        });
        page.images = Arc::from([image]);
        page.operations = Arc::from([
            DisplayOp::Save,
            DisplayOp::ConcatTransform(Matrix::scale(120.0, 50.0)),
            DisplayOp::DrawImage {
                image: ImageId(0),
                paint: PaintId(0),
                transform: Matrix::IDENTITY,
                alpha: 1.0,
                blend: BlendMode::Normal,
            },
            DisplayOp::Restore,
        ]);
        page
    }

    #[test]
    fn qualifies_only_native_full_page_jbig2_smask() {
        let page = eligible_page();
        let candidate = qualifying_jbig2_smask(&page).expect("eligible mask");
        assert_eq!(candidate.page_data.as_ref(), &b"encoded mask"[..]);
        assert_eq!(
            candidate
                .smask
                .codec_parms
                .as_ref()
                .and_then(|parms| parms.jbig2_globals.as_deref()),
            Some(&b"encoded globals"[..])
        );
        assert_eq!(candidate.stencil_polarity, Jbig2StencilPolarity::PaintOne);

        let mut maskless = page.clone();
        let maskless_image = &mut Arc::make_mut(&mut maskless.images)[0];
        maskless_image.smask = None;
        maskless_image.codec = Some(ImageCodecKind::Dct);
        maskless_image.codec_data = Some((&b"jpeg"[..]).into());
        assert!(qualifying_jbig2_smask(&maskless).is_none());

        let mut non_jbig2 = page.clone();
        let smask = Arc::make_mut(&mut non_jbig2.images)[0]
            .smask
            .as_mut()
            .unwrap();
        Arc::make_mut(smask).codec = Some(ImageCodecKind::CcittFax);
        assert!(qualifying_jbig2_smask(&non_jbig2).is_none());

        let mut grayscale = page.clone();
        let smask = Arc::make_mut(&mut grayscale.images)[0]
            .smask
            .as_mut()
            .unwrap();
        let smask = Arc::make_mut(smask);
        smask.bits_per_component = 8;
        smask.codec = None;
        smask.codec_data = None;
        smask.samples = Arc::from(&b"gray"[..]);
        assert!(qualifying_jbig2_smask(&grayscale).is_none());

        let mut image_mask = page.clone();
        Arc::make_mut(&mut image_mask.images)[0].is_stencil = true;
        assert!(qualifying_jbig2_smask(&image_mask).is_none());

        let mut in_data = page.clone();
        Arc::make_mut(&mut in_data.images)[0].smask_in_data = 1;
        assert!(qualifying_jbig2_smask(&in_data).is_none());

        let mut explicit_mask = page;
        Arc::make_mut(&mut explicit_mask.images)[0].mask =
            Some(pdf_page_ir::ImageMask::ColorKey(Arc::from([[0, 0]])));
        assert!(qualifying_jbig2_smask(&explicit_mask).is_none());
    }

    #[test]
    fn source_decode_maps_to_opposite_stencil_polarity() {
        let mut page = eligible_page();
        {
            let smask = Arc::make_mut(&mut page.images)[0].smask.as_mut().unwrap();
            Arc::make_mut(smask).decode = Some(Arc::from([[1.0, 0.0]]));
        }
        assert_eq!(
            qualifying_jbig2_smask(&page)
                .expect("inverted source decode remains representable")
                .stencil_polarity,
            Jbig2StencilPolarity::PaintZero
        );

        let smask = Arc::make_mut(&mut page.images)[0].smask.as_mut().unwrap();
        Arc::make_mut(smask).decode = Some(Arc::from([[0.2, 0.8]]));
        assert!(qualifying_jbig2_smask(&page).is_none());
    }

    #[test]
    fn rejects_ambiguous_or_unsupported_placement() {
        let page = eligible_page();
        let mut duplicate = page.clone();
        let mut operations = duplicate.operations.to_vec();
        operations.splice(3..3, duplicate.operations[0..3].iter().cloned());
        duplicate.operations = Arc::from(operations);
        assert!(qualifying_jbig2_smask(&duplicate).is_none());

        let mut rotated = page.clone();
        rotated.bounds.rotate = 90;
        assert!(qualifying_jbig2_smask(&rotated).is_none());

        let mut offset = page;
        Arc::make_mut(&mut offset.operations)[1] =
            DisplayOp::ConcatTransform(Matrix::scale(109.0, 50.0));
        assert!(qualifying_jbig2_smask(&offset).is_none());
    }

    #[test]
    fn accepts_full_page_placement_within_half_a_source_pixel() {
        let mut page = eligible_page();
        // The 12x5 base raster spans 120x50 points, so half a source pixel is
        // 5 points on either axis. Keep both edges just inside that allowance.
        Arc::make_mut(&mut page.operations)[1] = DisplayOp::ConcatTransform(Matrix {
            a: 112.0,
            b: 0.0,
            c: 0.0,
            d: 42.0,
            e: 4.0,
            f: 4.0,
        });
        assert!(qualifying_jbig2_smask(&page).is_some());

        Arc::make_mut(&mut page.operations)[1] = DisplayOp::ConcatTransform(Matrix {
            a: 108.0,
            b: 0.0,
            c: 0.0,
            d: 38.0,
            e: 6.0,
            f: 6.0,
        });
        assert!(qualifying_jbig2_smask(&page).is_none());
    }

    #[test]
    fn working_plane_preserves_fractional_edge_coverage() {
        let decoded = DecodedBilevelSmask {
            width: 2,
            height: 2,
            stride: 2,
            opacity: Arc::from([0, 255, 255, 0]),
        };
        let working = resize_smask_coverage(decoded, 7, 7).expect("valid coverage");
        assert_eq!((working.width, working.height, working.stride), (7, 7, 7));
        assert!(
            working
                .pixels
                .iter()
                .any(|&sample| sample > 0 && sample < 255),
            "resized analysis mask must retain antialiased edge coverage"
        );
        // Target pixel 3 spans source [6/7, 8/7): 1/7 of the first source
        // pixel and 1/7 of the second, so its coverage is their mean.
        assert_eq!(working.pixels[3], 128);
        assert_eq!(working.pixels[0], 0);
        assert_eq!(working.pixels[6], 255);
    }

    #[test]
    fn working_plane_is_the_exact_area_average() {
        // A 4x4 checkerboard halved: every 2x2 block is half ink.
        let mut checker = [0u8; 16];
        for (i, v) in checker.iter_mut().enumerate() {
            if (i % 4 + i / 4) % 2 == 0 {
                *v = 255;
            }
        }
        let decoded = DecodedBilevelSmask {
            width: 4,
            height: 4,
            stride: 4,
            opacity: Arc::from(checker),
        };
        let working = resize_smask_coverage(decoded, 2, 2).expect("valid coverage");
        assert_eq!(&working.pixels[..], &[128, 128, 128, 128]);

        // 3 -> 2 along one axis: the first target pixel takes one source
        // pixel and half of the next (255/2 over a span of 1.5 = 85), the
        // second the remaining half and one.
        let decoded = DecodedBilevelSmask {
            width: 3,
            height: 1,
            stride: 3,
            opacity: Arc::from([0, 255, 255]),
        };
        let working = resize_smask_coverage(decoded, 2, 1).expect("valid coverage");
        assert_eq!(&working.pixels[..], &[85, 255]);

        // Same size: the plane is passed through untouched.
        let decoded = DecodedBilevelSmask {
            width: 3,
            height: 1,
            stride: 3,
            opacity: Arc::from([0, 255, 7]),
        };
        let working = resize_smask_coverage(decoded, 3, 1).expect("valid coverage");
        assert_eq!(&working.pixels[..], &[0, 255, 7]);
    }
}
