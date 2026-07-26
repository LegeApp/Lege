use std::sync::Arc;

use crate::geometry::{Affine, PointF, RectF};
use crate::paint::PixelSurface;
use crate::text::{
    CharacterGeometry, TextSubstrate, cluster_lines, transform_characters_to_document,
};

use super::engine::{
    CancellationFlag, CompiledArtifact, CompiledArtifacts, DocumentCompileWorker,
    DocumentDescriptor, DocumentEngine, DocumentEngineError, DocumentRasterWorker, PageGeometry,
    RasterPass, SemanticArtifact, SyntheticCompiledPage, SyntheticSemanticPage, TextArtifact,
};
use super::features::PageStructure;
use super::tile::{TileDemand, TileKey, TileSurface, TileTier, ZoomBucket};
use super::{DocumentId, PageIndex};

#[derive(Debug, Clone)]
pub struct SyntheticEngine {
    descriptor: DocumentDescriptor,
}

impl SyntheticEngine {
    pub fn new(page_count: u32) -> Self {
        let geometries: Vec<PageGeometry> = (0..page_count)
            .map(|page| PageGeometry {
                crop: RectF {
                    x: 0.0,
                    y: 0.0,
                    width: if page % 9 == 0 { 640.0 } else { 612.0 },
                    height: if page % 13 == 0 { 760.0 } else { 792.0 },
                },
                rotation: 0,
            })
            .collect();
        Self {
            descriptor: DocumentDescriptor {
                id: DocumentId(1),
                display_name: format!("Synthetic {page_count}-page document"),
                page_count,
                page_geometries: geometries.into(),
                outline: Arc::from([]),
                page_links: (0..page_count).map(|_| Arc::from([])).collect(),
            },
        }
    }
}

#[derive(Debug)]
struct SyntheticCompileWorker {
    engine: SyntheticEngine,
}

impl DocumentCompileWorker for SyntheticCompileWorker {
    fn compile_page(
        &mut self,
        page: PageIndex,
        page_to_doc: Affine,
        cancellation: &CancellationFlag,
    ) -> Result<Arc<CompiledArtifacts>, DocumentEngineError> {
        compile_synthetic_page(&self.engine, page, page_to_doc, cancellation)
    }
}

#[derive(Debug)]
struct SyntheticRasterWorker {
    engine: SyntheticEngine,
}

impl DocumentRasterWorker for SyntheticRasterWorker {
    fn raster_tile(
        &mut self,
        artifacts: &CompiledArtifacts,
        bucket: ZoomBucket,
        demand: TileDemand,
        pass: RasterPass,
        generation: u64,
        cancellation: &CancellationFlag,
    ) -> Result<TileSurface, DocumentEngineError> {
        raster_synthetic_tile(
            &self.engine,
            artifacts,
            bucket,
            demand,
            pass,
            generation,
            cancellation,
        )
    }
}

impl DocumentEngine for SyntheticEngine {
    fn descriptor(&self) -> &DocumentDescriptor {
        &self.descriptor
    }

    fn create_compile_worker(&self) -> Box<dyn DocumentCompileWorker> {
        Box::new(SyntheticCompileWorker {
            engine: self.clone(),
        })
    }

    fn create_raster_worker(&self) -> Box<dyn DocumentRasterWorker> {
        Box::new(SyntheticRasterWorker {
            engine: self.clone(),
        })
    }
}

fn compile_synthetic_page(
    engine: &SyntheticEngine,
    page: PageIndex,
    page_to_doc: Affine,
    cancellation: &CancellationFlag,
) -> Result<Arc<CompiledArtifacts>, DocumentEngineError> {
    if cancellation.is_cancelled() {
        return Err(DocumentEngineError::Cancelled);
    }
    let geometry = *engine
        .descriptor
        .page_geometries
        .get(page.0 as usize)
        .ok_or(DocumentEngineError::PageOutOfRange(page))?;
    let mut characters = Vec::new();
    let mut utf16 = Vec::new();
    let line_count = 42 + (page.0 as usize % 11);
    for line in 0..line_count {
        let baseline = geometry.crop.height - 54.0 - line as f64 * 15.5;
        if baseline < 30.0 {
            break;
        }
        let text = format!("Page {} — synthetic line {:02}", page.0 + 1, line + 1);
        for (column, ch) in text.chars().enumerate() {
            let index = characters.len();
            utf16.extend(ch.encode_utf16(&mut [0; 2]).iter().copied());
            characters.push(CharacterGeometry {
                unicode: Some(ch),
                origin: PointF {
                    x: 58.0 + column as f64 * 7.2,
                    y: baseline,
                },
                bounds: RectF {
                    x: 58.0 + column as f64 * 7.2,
                    y: baseline - 3.0,
                    width: 6.2,
                    height: 11.0,
                },
                nominal_height: 11.0,
                font_size: 11.0,
                bold: line == 0,
                object_id: line as u32,
                char_index: index,
            });
        }
        utf16.push(b'\n' as u16);
    }
    let lines = Arc::new(cluster_lines(page, &characters, page_to_doc));
    transform_characters_to_document(&mut characters, page_to_doc);
    let substrate = Arc::new(TextSubstrate {
        utf16: utf16.into(),
        characters: characters.into(),
        lines,
    });
    Ok(Arc::new(CompiledArtifacts {
        page,
        geometry,
        semantic: SemanticArtifact::Synthetic(Arc::new(SyntheticSemanticPage { page, line_count })),
        compiled: CompiledArtifact::Synthetic(Arc::new(SyntheticCompiledPage {
            page,
            geometry,
            operation_count: line_count + 4,
        })),
        text: TextArtifact::Synthetic(substrate),
        structure: PageStructure::page_box(geometry.display_box()),
        lowering_degraded: false,
    }))
}

fn raster_synthetic_tile(
    engine: &SyntheticEngine,
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
    let width = demand.page_device_rect.width;
    let height = demand.page_device_rect.height;
    let stride = width as usize;
    let mut pixels = vec![0x00ff_ffff; stride * height as usize];
    let origin_x = demand.page_device_rect.x;
    let origin_y = demand.page_device_rect.y;

    for y in 0..height as i32 {
        let page_y = origin_y + y;
        for x in 0..width as i32 {
            let page_x = origin_x + x;
            let index = y as usize * stride + x as usize;
            if page_x < 2 || page_y < 2 {
                pixels[index] = 0x00d0_d0d0;
            }
        }
    }

    let line_color = match pass {
        RasterPass::Thumbnail | RasterPass::Draft => 0x0078_7878,
        RasterPass::TextFirst | RasterPass::Final => 0x0028_2828,
    };
    let scale = bucket.scale();
    for line in artifacts.text.lines().lines.iter() {
        let page_y = ((line.bounds.y - demand.page_document_rect.y) * scale).round() as i32;
        let y = page_y.clamp(0, height.saturating_sub(1) as i32);
        let start_x = (48.0 * scale).round() as i32 - origin_x;
        let end_x = ((artifacts.geometry.display_width() - 54.0) * scale).round() as i32 - origin_x;
        if y >= 0 && y < height as i32 {
            for x in start_x.max(0)..end_x.min(width as i32) {
                pixels[y as usize * stride + x as usize] = line_color;
            }
        }
    }
    if pass == RasterPass::Final {
        let block_x0 =
            (artifacts.geometry.display_width() * 0.56 * scale).round() as i32 - origin_x;
        let block_y0 =
            (artifacts.geometry.display_height() * 0.18 * scale).round() as i32 - origin_y;
        let block_x1 = block_x0 + (120.0 * scale).round() as i32;
        let block_y1 = block_y0 + (90.0 * scale).round() as i32;
        for y in block_y0.max(0)..block_y1.min(height as i32) {
            for x in block_x0.max(0)..block_x1.min(width as i32) {
                let shade = 0x0090 + ((x + y + artifacts.page.0 as i32) & 0x3f) as u32;
                pixels[y as usize * stride + x as usize] =
                    (shade << 16) | ((shade - 12) << 8) | (shade - 24);
            }
        }
    }

    let tier = match pass {
        RasterPass::Thumbnail => TileTier::Thumbnail,
        RasterPass::Draft => TileTier::Draft,
        RasterPass::TextFirst => TileTier::TextFirst,
        RasterPass::Final => TileTier::Final,
    };
    Ok(TileSurface {
        key: TileKey {
            document: engine.descriptor.id,
            page: demand.page,
            bucket,
            coord: demand.coord,
            tier,
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
