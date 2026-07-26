use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{CompiledPage, DeviceRect, DeviceSize, Matrix, PageFeatures};
use pdf_render_api::{
    AnnotationMode, Background, OutputFormat, OutputResidency, PageTransform, RenderColorPolicy,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::{CpuBackend, CpuBackendOptions, CpuWorkerContext};

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
}

/// Visible page geometry in PDF user space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageGeometry {
    pub crop_box: [f64; 4],
    pub rotate: u16,
}

impl PageGeometry {
    pub fn width(self) -> f64 {
        (self.crop_box[2] - self.crop_box[0]).max(0.0)
    }

    pub fn height(self) -> f64 {
        (self.crop_box[3] - self.crop_box[1]).max(0.0)
    }

    pub fn display_width(self) -> f64 {
        if self.rotate % 180 == 0 {
            self.width()
        } else {
            self.height()
        }
    }

    pub fn display_height(self) -> f64 {
        if self.rotate % 180 == 0 {
            self.height()
        } else {
            self.width()
        }
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
            limits: RenderLimits {
                cancellation: cancellation.map(|token| token.0.clone()),
                ..RenderLimits::default()
            },
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
            RasterFormat::Rgb8 => {
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
                let mut rgb = Vec::with_capacity(capacity);
                for row in 0..rendered.height as usize {
                    let start = row.checked_mul(rendered.stride).ok_or(ReadError::Render {
                        page: page.page_index,
                        message: "renderer row offset overflow".to_string(),
                    })?;
                    let end = start.checked_add(row_bytes).ok_or(ReadError::Render {
                        page: page.page_index,
                        message: "renderer row end overflow".to_string(),
                    })?;
                    let pixels =
                        rendered
                            .pixels
                            .get(start..end)
                            .ok_or_else(|| ReadError::Render {
                                page: page.page_index,
                                message: "renderer returned a truncated RGBA surface".to_string(),
                            })?;
                    rgb.extend(pixels.chunks_exact(4).flat_map(|pixel| &pixel[..3]));
                }
                Ok(RasterPlane::Rgb8(RgbSurface {
                    width: rendered.width,
                    height: rendered.height,
                    stride: rgb_stride,
                    pixels: Arc::from(rgb),
                }))
            }
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

    fn page_out_of_range(&self, page: u32) -> ReadError {
        ReadError::PageOutOfRange {
            page,
            page_count: self.page_count(),
        }
    }
}

fn page_to_device_matrix(page: &CompiledPage, output_width: u32, output_height: u32) -> Matrix {
    let crop = page.bounds.crop;
    let rotation = page.bounds.rotate % 360;
    let (display_width, display_height) = if rotation % 180 == 0 {
        (crop.width(), crop.height())
    } else {
        (crop.height(), crop.width())
    };
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
    let full_width = crop.x.max(0) as u32 + crop.width;
    let full_height = crop.y.max(0) as u32 + crop.height;
    let mut matrix = page_to_device_matrix(page, full_width.max(1), full_height.max(1));
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
