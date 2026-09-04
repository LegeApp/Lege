use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use pdf_content::PageCompiler;
use pdf_document::{
    DocumentLimits, DocumentLinkTarget as PdfLinkTarget, DocumentSnapshot, ParseContext,
};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, CancellationToken, OutputFormat, OutputResidency, PageTransform,
    RenderColorPolicy, RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::{CpuBackendOptions, CpuWorkerContext};
use pdf_render_wgpu::{
    ExperimentalImageRenderer, ImageRenderExecution, ImageRendererPreference,
    ImageRendererTelemetry,
};
use pdf_source::MmapSource;
use pdf_text::{CharType, TextPage, TextPageOptions};

use crate::geometry::{Affine, PointF, RectF};
use crate::paint::PixelSurface;
use crate::text::{
    CharacterGeometry, TextSubstrate, cluster_lines, transform_characters_to_document,
};

use super::engine::{
    CancellationFlag, CompiledArtifact, CompiledArtifacts, DocumentCompileWorker,
    DocumentDescriptor, DocumentEngine, DocumentEngineError, DocumentRasterWorker, PageGeometry,
    RasterPass, SemanticArtifact, TextArtifact,
};
use super::features::{
    ContentExtent, ContentExtentSource, DocumentLink, LinkTarget, OutlineNode, OutlineSource,
    PageStructure,
};
use super::tile::{TileDemand, TileKey, TileSurface, TileTier, ZoomBucket};
use super::{DocumentId, PageIndex};

static NEXT_DOCUMENT_ID: AtomicU64 = AtomicU64::new(100);
type RendererInit = Result<Arc<ExperimentalImageRenderer>, Arc<str>>;

#[derive(Debug, Clone)]
pub struct PdfEngine {
    path: PathBuf,
    snapshot: Arc<DocumentSnapshot>,
    descriptor: DocumentDescriptor,
    compiler: Arc<PageCompiler>,
    renderer: Arc<OnceLock<RendererInit>>,
    renderer_preference: ImageRendererPreference,
}

impl PdfEngine {
    pub fn open(
        path: impl AsRef<Path>,
        password: Option<&str>,
    ) -> Result<Self, DocumentEngineError> {
        let path = path.as_ref().to_path_buf();
        let source = Arc::new(
            MmapSource::open(&path)
                .map_err(|error| DocumentEngineError::Engine(error.to_string()))?,
        );
        let snapshot = Arc::new(
            DocumentSnapshot::open_with_password(source, DocumentLimits::default(), password)
                .map_err(classify_open_error)?,
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
        let mut outline_context = ParseContext::new();
        let document_outline = snapshot.outline(&mut outline_context);
        let mut navigable_ancestors = Vec::new();
        let mut outline_nodes = Vec::new();
        for item in document_outline.items.iter() {
            let source_depth = usize::from(item.depth);
            navigable_ancestors.truncate(source_depth);
            navigable_ancestors.resize(source_depth, false);
            let effective_depth = navigable_ancestors.iter().filter(|valid| **valid).count() as u16;
            let Some(destination) = item.destination else {
                navigable_ancestors.push(false);
                continue;
            };
            let Some(geometry) = geometries.get(destination.page.0 as usize).copied() else {
                navigable_ancestors.push(false);
                continue;
            };
            let target_region = destination.top.map(|top| {
                let display = geometry.user_point_to_display(PointF {
                    x: destination.left.unwrap_or(geometry.crop.x),
                    y: top,
                });
                RectF {
                    x: display.x,
                    y: display.y,
                    width: 1.0,
                    height: 1.0,
                }
            });
            outline_nodes.push(OutlineNode {
                title: Arc::clone(&item.title),
                page: PageIndex(destination.page.0),
                target_region,
                depth: effective_depth,
                source: OutlineSource::Embedded,
            });
            navigable_ancestors.push(true);
        }
        let outline: Arc<[OutlineNode]> = outline_nodes.into();
        let mut link_context = ParseContext::new();
        let document_links = snapshot.links(&mut link_context);
        let page_links = document_links
            .pages
            .iter()
            .enumerate()
            .map(|(page_number, links)| {
                let Some(source_geometry) = geometries.get(page_number).copied() else {
                    return Arc::from([]);
                };
                links
                    .iter()
                    .filter_map(|link| {
                        let source_region = user_rect_to_display(source_geometry, link.rect)?;
                        let target = match &link.target {
                            PdfLinkTarget::Internal(destination) => {
                                let target_geometry =
                                    geometries.get(destination.page.0 as usize).copied()?;
                                let target_region = destination.top.map(|top| {
                                    let display = target_geometry.user_point_to_display(PointF {
                                        x: destination.left.unwrap_or(target_geometry.crop.x),
                                        y: top,
                                    });
                                    RectF {
                                        x: display.x,
                                        y: display.y,
                                        width: 1.0,
                                        height: 1.0,
                                    }
                                });
                                LinkTarget::Internal {
                                    page: PageIndex(destination.page.0),
                                    target_region,
                                }
                            }
                            PdfLinkTarget::Uri(uri) => LinkTarget::External(Arc::clone(uri)),
                        };
                        Some(DocumentLink {
                            source_region,
                            target,
                        })
                    })
                    .collect::<Vec<_>>()
                    .into()
            })
            .collect::<Vec<_>>()
            .into();
        let renderer_preference = ImageRendererPreference::from_env()
            .map_err(|error| DocumentEngineError::Engine(error.to_string()))?;
        Ok(Self {
            path,
            snapshot,
            descriptor: DocumentDescriptor {
                id,
                display_name,
                page_count: geometries.len() as u32,
                page_geometries: geometries.into(),
                outline,
                page_links,
            },
            compiler: Arc::new(PageCompiler::new().with_annotations(true)),
            renderer: Arc::new(OnceLock::new()),
            renderer_preference,
        })
    }

    /// Snapshot routing counts for diagnostics and focused viewer benchmarks.
    pub fn image_renderer_telemetry(&self) -> ImageRendererTelemetry {
        self.renderer
            .get()
            .and_then(|renderer| renderer.as_ref().ok())
            .map_or_else(ImageRendererTelemetry::default, |renderer| {
                renderer.telemetry()
            })
    }

    /// Whether a final raster request has initialized the renderer policy.
    pub fn image_renderer_initialized(&self) -> bool {
        self.renderer.get().is_some()
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
    renderer: Arc<OnceLock<RendererInit>>,
    renderer_preference: ImageRendererPreference,
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
            self.renderer.as_ref(),
            self.renderer_preference,
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
            renderer: self.renderer.clone(),
            renderer_preference: self.renderer_preference,
            context: CpuWorkerContext::new(),
        })
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
    context.set_cancellation_flag(Some(cancellation.shared_flag()));
    let compilation =
        compiler.compile_artifacts(snapshot, pdf_document::PageIndex(page.0), context);
    context.set_cancellation_flag(None);
    let compilation = compilation.map_err(|error| match error {
        pdf_content::ContentError::Cancelled => DocumentEngineError::Cancelled,
        other => DocumentEngineError::Engine(other.to_string()),
    })?;
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
    let retained: Vec<_> = text_page
        .chars()
        .iter()
        .filter(|info| !matches!(info.char_type, CharType::NotUnicode | CharType::Hyphen))
        .collect();
    let mut characters = Vec::with_capacity(retained.len());
    let mut cursor = 0usize;
    let mut utf16_index = 0usize;
    while cursor < retained.len() {
        let info = retained[cursor];
        let (unicode, utf16_len) = decode_geometry_scalar(
            info.unicode as u16,
            retained.get(cursor + 1).map(|next| next.unicode as u16),
        );
        let paired = utf16_len == 2;
        let mut bounds = RectF {
            x: info.char_box.x0,
            y: info.char_box.y0,
            width: info.char_box.width(),
            height: info.char_box.height(),
        };
        if paired {
            let continuation = retained[cursor + 1].char_box;
            let x0 = bounds.x.min(continuation.x0);
            let y0 = bounds.y.min(continuation.y0);
            let x1 = bounds.right().max(continuation.x1);
            let y1 = bounds.bottom().max(continuation.y1);
            bounds = RectF {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            };
        }
        let run = compilation
            .semantic
            .text_runs
            .get(info.text_object.0 as usize);
        let font_size = run.map_or(info.char_box.height().abs().max(1.0), |run| {
            run.font_size.abs().max(1.0)
        });
        let bold = run
            .and_then(|run| compilation.semantic.fonts.get(run.font.0 as usize))
            .is_some_and(|font| {
                font.synthesis.embolden
                    || String::from_utf8_lossy(&font.base_font)
                        .to_ascii_lowercase()
                        .contains("bold")
            });
        characters.push(CharacterGeometry {
            unicode: Some(unicode),
            origin: PointF {
                x: info.origin.x,
                y: info.origin.y,
            },
            bounds,
            nominal_height: bounds.height.abs().max(1.0),
            font_size,
            bold,
            object_id: info.text_object.0,
            char_index: utf16_index,
            utf16_len,
        });
        utf16_index += utf16_len;
        cursor += utf16_len;
    }
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
    let content_extent = compilation
        .compiled
        .content_bounds
        .and_then(|bounds| {
            geometry.user_rect_to_display(RectF {
                x: bounds.x0,
                y: bounds.y0,
                width: bounds.width(),
                height: bounds.height(),
            })
        })
        .map(|rect| ContentExtent {
            rect,
            source: ContentExtentSource::DisplayList,
        })
        .unwrap_or(ContentExtent {
            rect: geometry.display_box(),
            source: ContentExtentSource::PageBox,
        });
    Ok(Arc::new(CompiledArtifacts {
        page,
        geometry,
        semantic: SemanticArtifact::Pdf(compilation.semantic),
        compiled: CompiledArtifact::Pdf(compilation.compiled),
        text: TextArtifact::Pdf {
            native: text_page,
            substrate,
        },
        structure: PageStructure {
            content_extent,
            links: descriptor
                .page_links
                .get(page.0 as usize)
                .cloned()
                .unwrap_or_else(|| Arc::from([])),
        },
        lowering_degraded,
    }))
}

fn decode_geometry_scalar(first: u16, second: Option<u16>) -> (char, usize) {
    if (0xd800..=0xdbff).contains(&first)
        && let Some(second) = second.filter(|unit| (0xdc00..=0xdfff).contains(unit))
        && let Some(character) = char::decode_utf16([first, second])
            .next()
            .and_then(Result::ok)
    {
        return (character, 2);
    }
    (
        char::from_u32(u32::from(first)).unwrap_or(char::REPLACEMENT_CHARACTER),
        1,
    )
}

fn user_rect_to_display(geometry: PageGeometry, rect: [f64; 4]) -> Option<RectF> {
    let corners = [
        geometry.user_point_to_display(PointF {
            x: rect[0],
            y: rect[1],
        }),
        geometry.user_point_to_display(PointF {
            x: rect[0],
            y: rect[3],
        }),
        geometry.user_point_to_display(PointF {
            x: rect[2],
            y: rect[1],
        }),
        geometry.user_point_to_display(PointF {
            x: rect[2],
            y: rect[3],
        }),
    ];
    let x0 = corners
        .iter()
        .map(|point| point.x)
        .fold(f64::INFINITY, f64::min);
    let y0 = corners
        .iter()
        .map(|point| point.y)
        .fold(f64::INFINITY, f64::min);
    let x1 = corners
        .iter()
        .map(|point| point.x)
        .fold(f64::NEG_INFINITY, f64::max);
    let y1 = corners
        .iter()
        .map(|point| point.y)
        .fold(f64::NEG_INFINITY, f64::max);
    let region = RectF {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    };
    (region.width > 0.0 && region.height > 0.0).then_some(region)
}

#[allow(clippy::too_many_arguments)]
fn raster_pdf_tile(
    document: DocumentId,
    renderer: &OnceLock<RendererInit>,
    renderer_preference: ImageRendererPreference,
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
        return raster_text_structure(
            document,
            artifacts,
            bucket,
            demand,
            generation,
            cancellation,
        );
    }
    let CompiledArtifact::Pdf(compiled) = &artifacts.compiled else {
        return Err(DocumentEngineError::Engine(
            "PDF engine received a non-PDF compiled artifact".to_owned(),
        ));
    };
    // The conductor creates raster workers inside background threads. Deferring
    // the shared renderer here keeps adapter discovery off document open and
    // lets TextFirst tiles appear without waiting for WGPU initialization.
    let renderer = renderer
        .get_or_init(|| {
            ExperimentalImageRenderer::new(renderer_preference, CpuBackendOptions::default())
                .map(Arc::new)
                .map_err(|error| Arc::from(error.to_string()))
        })
        .as_ref()
        .map_err(|error| DocumentEngineError::Engine(error.to_string()))?;
    let scale = bucket.scale();
    let full_page_matrix = device_matrix(compiled.bounds.crop, compiled.bounds.rotate, scale);
    let tile_matrix = full_page_matrix.then(Matrix::translate(
        -demand.page_view_box.x * scale - f64::from(demand.page_device_rect.x),
        -demand.page_view_box.y * scale - f64::from(demand.page_device_rect.y),
    ));
    let color_policy = match demand.color_mode {
        super::ColorMode::Original => RenderColorPolicy::Original,
        super::ColorMode::Night => RenderColorPolicy::Night {
            paper_rgb: [0x25, 0x25, 0x25],
            text_rgb: [0xd8, 0xd1, 0xc4],
        },
        super::ColorMode::WarmPaper => RenderColorPolicy::WarmPaper {
            paper_rgb: [0xf2, 0xe8, 0xd2],
        },
        super::ColorMode::SanzoEarth | super::ColorMode::SanzoSea => RenderColorPolicy::Original,
    };
    let paper = color_policy.paper_rgb();
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
        background: Background::Solid(pdf_page_ir::Color {
            r: f32::from(paper[0]) / 255.0,
            g: f32::from(paper[1]) / 255.0,
            b: f32::from(paper[2]) / 255.0,
            a: 1.0,
        }),
        color_policy,
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
    let result = renderer
        .render_to_host_with_cpu_context(&request, context)
        .map_err(|error| match error {
            pdf_render_api::RenderError::Cancelled => DocumentEngineError::Cancelled,
            other => DocumentEngineError::Engine(other.to_string()),
        })?;
    let renderer_degraded = match &result.execution {
        ImageRenderExecution::Cpu(stats) => stats.degraded_draws > 0,
        ImageRenderExecution::Gpu(_) => false,
    };
    let pixels = ingest_rgba_to_xrgb(&result.host)?;
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
            variant: demand.variant,
        },
        generation,
        page_device_rect: demand.page_device_rect,
        page_document_rect: demand.page_document_rect,
        pixels,
        degraded: renderer_degraded || artifacts.lowering_degraded,
    })
}

fn raster_text_structure(
    document: DocumentId,
    artifacts: &CompiledArtifacts,
    bucket: ZoomBucket,
    demand: TileDemand,
    generation: u64,
    cancellation: &CancellationFlag,
) -> Result<TileSurface, DocumentEngineError> {
    let substrate = artifacts.text.substrate();
    if substrate.characters.is_empty() {
        return Err(DocumentEngineError::TextFirstUnsupported);
    }
    let width = demand.page_device_rect.width;
    let height = demand.page_device_rect.height;
    let stride = width as usize;
    let paper = match demand.color_mode {
        super::ColorMode::Original => 0x00ff_ffff,
        super::ColorMode::Night => 0x0025_2525,
        super::ColorMode::WarmPaper => 0x00f2_e8d2,
        super::ColorMode::SanzoEarth | super::ColorMode::SanzoSea => 0x00ff_ffff,
    };
    let mut pixels = vec![paper; stride.saturating_mul(height as usize)];
    let scale = bucket.scale();

    for (index, character) in substrate.characters.iter().enumerate() {
        if index % 256 == 0 && cancellation.is_cancelled() {
            return Err(DocumentEngineError::Cancelled);
        }
        if character.unicode.is_none_or(char::is_whitespace) {
            continue;
        }
        let Some(ink) = character.bounds.intersection(demand.page_document_rect) else {
            continue;
        };
        let x0 = ((ink.x - demand.page_document_rect.x) * scale)
            .floor()
            .clamp(0.0, f64::from(width)) as usize;
        let y0 = ((ink.y - demand.page_document_rect.y) * scale)
            .floor()
            .clamp(0.0, f64::from(height)) as usize;
        let x1 = ((ink.right() - demand.page_document_rect.x) * scale)
            .ceil()
            .clamp(0.0, f64::from(width)) as usize;
        let y1 = ((ink.bottom() - demand.page_document_rect.y) * scale)
            .ceil()
            .clamp(0.0, f64::from(height)) as usize;
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        let color = if demand.color_mode == super::ColorMode::Night {
            if character.bold {
                0x00d8_d1c4
            } else {
                0x00b8_b2a8
            }
        } else if character.bold {
            0x0048_4848
        } else {
            0x0068_6868
        };
        for y in y0..y1 {
            pixels[y * stride + x0..y * stride + x1].fill(color);
        }
    }

    Ok(TileSurface {
        key: TileKey {
            document,
            page: demand.page,
            bucket,
            coord: demand.coord,
            tier: TileTier::TextFirst,
            variant: demand.variant,
        },
        generation,
        page_device_rect: demand.page_device_rect,
        page_document_rect: demand.page_document_rect,
        pixels: PixelSurface {
            width,
            height,
            stride,
            pixels: pixels.into(),
        },
        degraded: false,
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

fn ingest_rgba_to_xrgb(
    host: &pdf_render_api::HostPage,
) -> Result<PixelSurface, DocumentEngineError> {
    if host.format != OutputFormat::Rgba8PremultipliedSrgb {
        return Err(DocumentEngineError::Engine(
            "viewer ingestion expected RGBA8 output".to_owned(),
        ));
    }

    let width = host.width as usize;
    let height = host.height as usize;
    let source_row_bytes = width.checked_mul(4).ok_or_else(|| {
        DocumentEngineError::Engine("renderer output row size overflowed".to_owned())
    })?;
    if host.stride < source_row_bytes {
        return Err(DocumentEngineError::Engine(format!(
            "renderer output stride {} is smaller than the {source_row_bytes}-byte RGBA row",
            host.stride
        )));
    }
    let required_source_bytes = host.stride.checked_mul(height).ok_or_else(|| {
        DocumentEngineError::Engine("renderer output buffer size overflowed".to_owned())
    })?;
    if host.pixels.len() < required_source_bytes {
        return Err(DocumentEngineError::Engine(format!(
            "renderer output is truncated: expected at least {required_source_bytes} bytes, got {}",
            host.pixels.len()
        )));
    }
    let output_pixels = width.checked_mul(height).ok_or_else(|| {
        DocumentEngineError::Engine("viewer output surface size overflowed".to_owned())
    })?;

    let stride = host.width as usize;
    let mut pixels = vec![0_u32; output_pixels];
    for y in 0..height {
        let source_start = y * host.stride;
        let source_row = &host.pixels[source_start..source_start + source_row_bytes];
        let destination = &mut pixels[y * stride..(y + 1) * stride];
        for (rgba, output) in source_row.chunks_exact(4).zip(destination) {
            *output = (u32::from(rgba[0]) << 16) | (u32::from(rgba[1]) << 8) | u32::from(rgba[2]);
        }
    }
    Ok(PixelSurface {
        width: host.width,
        height: host.height,
        stride,
        pixels: pixels.into(),
    })
}

/// Separate an unanswered password question from an actually broken file.
///
/// The renderer already reports this as a typed `SecurityError`; flattening it
/// to a message here would leave the viewer with nothing to match on but
/// English prose, and no way to tell a reader whose document is merely locked
/// that it is worth typing a password.
fn classify_open_error(error: pdf_document::DocumentError) -> DocumentEngineError {
    match error {
        pdf_document::DocumentError::Security(pdf_security::SecurityError::PasswordRequired) => {
            DocumentEngineError::PasswordRequired
        }
        pdf_document::DocumentError::Security(pdf_security::SecurityError::IncorrectPassword) => {
            DocumentEngineError::IncorrectPassword
        }
        other => DocumentEngineError::Engine(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::Arc;

    use pdf_render_api::{HostPage, OutputFormat};

    use super::{DocumentEngineError, classify_open_error, decode_geometry_scalar,
        ingest_rgba_to_xrgb};

    fn host_page(width: u32, height: u32, stride: usize, pixels: Vec<u8>) -> HostPage {
        HostPage {
            width,
            height,
            stride,
            format: OutputFormat::Rgba8PremultipliedSrgb,
            pixels: Arc::from(pixels),
        }
    }

    #[test]
    fn ingest_rejects_short_stride_without_panicking() {
        let host = host_page(2, 1, 7, vec![0; 7]);
        let error = ingest_rgba_to_xrgb(&host).expect_err("short stride must be rejected");
        assert!(error.to_string().contains("stride"));
    }

    #[test]
    fn ingest_rejects_truncated_padded_rows_without_panicking() {
        let host = host_page(2, 2, 12, vec![0; 23]);
        let error = ingest_rgba_to_xrgb(&host).expect_err("truncated buffer must be rejected");
        assert!(error.to_string().contains("truncated"));
    }

    #[test]
    fn ingest_accepts_padding_and_ignores_alpha() {
        let host = host_page(
            2,
            1,
            12,
            vec![0x12, 0x34, 0x56, 0x78, 0xaa, 0xbb, 0xcc, 0xdd, 1, 2, 3, 4],
        );
        let surface = ingest_rgba_to_xrgb(&host).expect("valid padded row");
        assert_eq!(&*surface.pixels, &[0x0012_3456, 0x00aa_bbcc]);
    }

    #[test]
    fn a_locked_document_is_reported_as_a_question_not_a_failure() {
        assert!(matches!(
            classify_open_error(pdf_security::SecurityError::PasswordRequired.into()),
            DocumentEngineError::PasswordRequired
        ));
        assert!(matches!(
            classify_open_error(pdf_security::SecurityError::IncorrectPassword.into()),
            DocumentEngineError::IncorrectPassword
        ));
    }

    #[test]
    fn an_unsupported_encryption_scheme_is_a_failure_not_a_password_prompt() {
        // Nothing the reader can type opens a document the renderer cannot
        // decrypt, so it must not be offered a password field.
        let error = classify_open_error(
            pdf_security::SecurityError::UnsupportedScheme(
                pdf_security::EncryptionScheme::None,
            )
            .into(),
        );
        assert!(matches!(error, DocumentEngineError::Engine(_)));
    }

    #[test]
    fn geometry_scalar_coalesces_valid_utf16_surrogate_pair() {
        let units: Vec<u16> = "😀".encode_utf16().collect();
        assert_eq!(decode_geometry_scalar(units[0], Some(units[1])), ('😀', 2));
        assert_eq!(
            decode_geometry_scalar(units[0], Some(b'A' as u16)),
            (char::REPLACEMENT_CHARACTER, 1)
        );
    }
}
