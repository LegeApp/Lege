use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, ParseContext};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, CancellationToken, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::{CpuBackend, CpuWorkerContext};
use pdf_source::MmapSource;
use pdf_text::{TextPage, TextPageOptions};

use crate::geometry::{Affine, PointF, RectF};
use crate::paint::PixelSurface;
use crate::text::{
    CharacterGeometry, TextSubstrate, cluster_lines, transform_characters_to_document,
};

use super::features::PageStructure;
use super::engine::{
    CancellationFlag, CompiledArtifact, CompiledArtifacts, DocumentCompileWorker,
    DocumentDescriptor, DocumentEngine, DocumentEngineError, DocumentRasterWorker, PageGeometry,
    RasterPass, SemanticArtifact, TextArtifact,
};
use super::tile::{TileDemand, TileKey, TileSurface, TileTier, ZoomBucket};
use super::{DocumentId, PageIndex};

static NEXT_DOCUMENT_ID: AtomicU64 = AtomicU64::new(100);

#[derive(Debug, Clone)]
pub struct PdfEngine {
    path: PathBuf,
    snapshot: Arc<DocumentSnapshot>,
    descriptor: DocumentDescriptor,
    compiler: Arc<PageCompiler>,
    backend: Arc<CpuBackend>,
}

impl PdfEngine {
    pub fn open(path: impl AsRef<Path>, password: Option<&str>) -> Result<Self, DocumentEngineError> {
        let path = path.as_ref().to_path_buf();
        let source = Arc::new(
            MmapSource::open(&path)
                .map_err(|error| DocumentEngineError::Engine(error.to_string()))?,
        );
        let snapshot = Arc::new(
            DocumentSnapshot::open_with_password(source, DocumentLimits::default(), password)
                .map_err(|error| DocumentEngineError::Engine(error.to_string()))?,
        );
        let mut geometries = Vec::with_capacity(snapshot.page_count() as usize);
        for number in 0..snapshot.page_count() {
            let page = snapshot
                .page(pdf_document::PageIndex(number))
                .map_err(|error| DocumentEngineError::Engine(error.to_string()))?;
            let [x0, y0, x1, y1] = page.crop_box;
            geometries.push(PageGeometry {
                crop: RectF {
                    x: x0.min(x1),
                    y: y0.min(y1),
                    width: (x1 - x0).abs(),
                    height: (y1 - y0).abs(),
                },
                rotation: page.rotate,
            });
        }
        let id = DocumentId(NEXT_DOCUMENT_ID.fetch_add(1, Ordering::Relaxed));
        let display_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("PDF document")
            .to_owned();
        Ok(Self {
            path,
            snapshot,
            descriptor: DocumentDescriptor {
                id,
                display_name,
                page_count: geometries.len() as u32,
                page_geometries: geometries.into(),
            },
            compiler: Arc::new(PageCompiler::new().with_annotations(true)),
            backend: Arc::new(CpuBackend::default()),
        })
    }
}

#[derive(Debug)]
struct PdfCompileWorker {
    snapshot: Arc<DocumentSnapshot>,
    descriptor: DocumentDescriptor,
    compiler: Arc<PageCompiler>,
    context: ParseContext,
}

impl DocumentCompileWorker for PdfCompileWorker {
    fn compile_page(
        &mut self,
        page: PageIndex,
        page_to_doc: Affine,
        cancellation: &CancellationFlag,
    ) -> Result<Arc<CompiledArtifacts>, DocumentEngineError> {
        compile_pdf_page(
            self.snapshot.as_ref(),
            &self.descriptor,
            self.compiler.as_ref(),
            &mut self.context,
            page,
            page_to_doc,
            cancellation,
        )
    }
}

#[derive(Debug)]
struct PdfRasterWorker {
    document: DocumentId,
    backend: Arc<CpuBackend>,
    context: CpuWorkerContext,
}

impl DocumentRasterWorker for PdfRasterWorker {
    fn raster_tile(
        &mut self,
        artifacts: &CompiledArtifacts,
        bucket: ZoomBucket,
        demand: TileDemand,
        pass: RasterPass,
        generation: u64,
        cancellation: &CancellationFlag,
    ) -> Result<TileSurface, DocumentEngineError> {
        raster_pdf_tile(
            self.document,
            self.backend.as_ref(),
            &mut self.context,
            artifacts,
            bucket,
            demand,
            pass,
            generation,
            cancellation,
        )
    }
}

impl DocumentEngine for PdfEngine {
    fn descriptor(&self) -> &DocumentDescriptor {
        &self.descriptor
    }

    fn create_compile_worker(&self) -> Box<dyn DocumentCompileWorker> {
        Box::new(PdfCompileWorker {
            snapshot: self.snapshot.clone(),
            descriptor: self.descriptor.clone(),
            compiler: self.compiler.clone(),
            context: ParseContext::new(),
        })
    }

    fn create_raster_worker(&self) -> Box<dyn DocumentRasterWorker> {
        Box::new(PdfRasterWorker {
            document: self.descriptor.id,
            backend: self.backend.clone(),
            context: CpuWorkerContext::new(),
        })
    }

    fn supports_text_first(&self, _artifacts: &CompiledArtifacts) -> bool {
        false
    }

    fn source_path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

fn compile_pdf_page(
    snapshot: &DocumentSnapshot,
    descriptor: &DocumentDescriptor,
    compiler: &PageCompiler,
    context: &mut ParseContext,
    page: PageIndex,
    page_to_doc: Affine,
    cancellation: &CancellationFlag,
) -> Result<Arc<CompiledArtifacts>, DocumentEngineError> {
    if cancellation.is_cancelled() {
        return Err(DocumentEngineError::Cancelled);
    }
    let geometry = *descriptor
        .page_geometries
        .get(page.0 as usize)
        .ok_or(DocumentEngineError::PageOutOfRange(page))?;
    context.begin_job();
    let compilation = compiler
        .compile_artifacts(snapshot, pdf_document::PageIndex(page.0), context)
        .map_err(|error| DocumentEngineError::Engine(error.to_string()))?;
    if cancellation.is_cancelled() {
        return Err(DocumentEngineError::Cancelled);
    }

    let text_page = Arc::new(TextPage::build(
        compilation.semantic.as_ref(),
        &TextPageOptions {
            include_annotations: true,
            ..TextPageOptions::default()
        },
    ));
    let mut characters: Vec<CharacterGeometry> = text_page
        .chars()
        .iter()
        .enumerate()
        .map(|(index, info)| CharacterGeometry {
            unicode: char::from_u32(info.unicode),
            origin: PointF {
                x: info.origin.x,
                y: info.origin.y,
            },
            bounds: RectF {
                x: info.char_box.x0,
                y: info.char_box.y0,
                width: info.char_box.width(),
                height: info.char_box.height(),
            },
            nominal_height: info.char_box.height().abs().max(1.0),
            object_id: info.text_object.0,
            char_index: index,
        })
        .collect();
    let lines = Arc::new(cluster_lines(page, &characters, page_to_doc));
    transform_characters_to_document(&mut characters, page_to_doc);
    let substrate = Arc::new(TextSubstrate {
        utf16: Arc::<[u16]>::from(text_page.all_text_utf16()),
        characters: characters.into(),
        lines,
    });
    let lowering_degraded = compilation
        .compiled
        .images
        .iter()
        .any(|image| image.lowering_degraded);
    Ok(Arc::new(CompiledArtifacts {
        page,
        geometry,
        semantic: SemanticArtifact::Pdf(compilation.semantic),
        compiled: CompiledArtifact::Pdf(compilation.compiled),
        text: TextArtifact::Pdf {
            native: text_page,
            substrate,
        },
        structure: PageStructure::page_box(geometry.crop),
        lowering_degraded,
    }))
}

#[allow(clippy::too_many_arguments)]
fn raster_pdf_tile(
    document: DocumentId,
    backend: &CpuBackend,
    context: &mut CpuWorkerContext,
    artifacts: &CompiledArtifacts,
    bucket: ZoomBucket,
    demand: TileDemand,
    pass: RasterPass,
    generation: u64,
    cancellation: &CancellationFlag,
) -> Result<TileSurface, DocumentEngineError> {
    if cancellation.is_cancelled() {
        return Err(DocumentEngineError::Cancelled);
    }
    if pass == RasterPass::TextFirst {
        // The viewer architecture already models this pass. The supplied CPU
        // renderer still needs the display-list pass filter and upgrade target
        // described by blank-slate-architecture.md §4.4.
        return Err(DocumentEngineError::TextFirstUnsupported);
    }
    let CompiledArtifact::Pdf(compiled) = &artifacts.compiled else {
        return Err(DocumentEngineError::Engine(
            "PDF engine received a non-PDF compiled artifact".to_owned(),
        ));
    };
    let scale = bucket.scale();
    let full_page_matrix = device_matrix(compiled.bounds.crop, compiled.bounds.rotate, scale);
    let tile_matrix = full_page_matrix.then(Matrix::translate(
        -f64::from(demand.page_device_rect.x),
        -f64::from(demand.page_device_rect.y),
    ));
    let renderer_cancel = CancellationToken::from_shared(cancellation.shared_flag());
    let request = RenderRequest {
        page: compiled.clone(),
        transform: PageTransform {
            matrix: tile_matrix,
        },
        // Translation plus a tile-sized output gives correct tiles today.
        // The planned tile-run API will amortize setup while retaining these
        // exact cache keys and completion semantics.
        crop: None,
        output_size: DeviceSize {
            width: demand.page_device_rect.width,
            height: demand.page_device_rect.height,
        },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations: AnnotationMode::StaticAppearances,
        quality: if matches!(pass, RasterPass::Thumbnail | RasterPass::Draft) {
            RenderQuality::Draft
        } else {
            RenderQuality::Normal
        },
        limits: RenderLimits {
            cancellation: Some(renderer_cancel),
            ..RenderLimits::default()
        },
        residency: OutputResidency::HostRequired,
    };
    let (host, stats) = backend.render_with(&request, context).map_err(|error| match error {
        pdf_render_api::RenderError::Cancelled => DocumentEngineError::Cancelled,
        other => DocumentEngineError::Engine(other.to_string()),
    })?;
    let pixels = ingest_rgba_to_xrgb(&host)?;
    let tier = match pass {
        RasterPass::Thumbnail => TileTier::Thumbnail,
        RasterPass::Draft => TileTier::Draft,
        RasterPass::TextFirst => TileTier::TextFirst,
        RasterPass::Final => TileTier::Final,
    };
    Ok(TileSurface {
        key: TileKey {
            document,
            page: demand.page,
            bucket,
            coord: demand.coord,
            tier,
        },
        generation,
        page_device_rect: demand.page_device_rect,
        page_document_rect: demand.page_document_rect,
        pixels,
        degraded: stats.degraded_draws > 0 || artifacts.lowering_degraded,
    })
}

fn device_matrix(crop: pdf_page_ir::Rect, rotation: u16, scale: f64) -> Matrix {
    match rotation {
        90 => Matrix {
            a: 0.0,
            b: scale,
            c: scale,
            d: 0.0,
            e: -crop.y0 * scale,
            f: -crop.x0 * scale,
        },
        180 => Matrix {
            a: -scale,
            b: 0.0,
            c: 0.0,
            d: scale,
            e: crop.x1 * scale,
            f: -crop.y0 * scale,
        },
        270 => Matrix {
            a: 0.0,
            b: -scale,
            c: -scale,
            d: 0.0,
            e: crop.y1 * scale,
            f: crop.x1 * scale,
        },
        _ => Matrix {
            a: scale,
            b: 0.0,
            c: 0.0,
            d: -scale,
            e: -crop.x0 * scale,
            f: crop.y1 * scale,
        },
    }
}

fn ingest_rgba_to_xrgb(host: &pdf_render_api::HostPage) -> Result<PixelSurface, DocumentEngineError> {
    if host.format != OutputFormat::Rgba8PremultipliedSrgb {
        return Err(DocumentEngineError::Engine(
            "viewer ingestion expected RGBA8 output".to_owned(),
        ));
    }
    let stride = host.width as usize;
    let mut pixels = vec![0_u32; stride * host.height as usize];
    for y in 0..host.height as usize {
        let source_row = &host.pixels[y * host.stride..y * host.stride + host.width as usize * 4];
        let destination = &mut pixels[y * stride..(y + 1) * stride];
        for (rgba, output) in source_row.chunks_exact(4).zip(destination) {
            *output = (u32::from(rgba[0]) << 16)
                | (u32::from(rgba[1]) << 8)
                | u32::from(rgba[2]);
        }
    }
    Ok(PixelSurface {
        width: host.width,
        height: host.height,
        stride,
        pixels: pixels.into(),
    })
}
