use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::geometry::{Affine, RectF};
use crate::text::{PageLineSet, TextSubstrate};

use super::features::{DocumentLink, OutlineNode, PageStructure};
use super::tile::{TileDemand, TileSurface, ZoomBucket};
use super::{DocumentId, PageIndex};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageGeometry {
    pub crop: RectF,
    pub rotation: u16,
}

impl PageGeometry {
    pub fn display_width(self) -> f64 {
        if matches!(self.rotation, 90 | 270) {
            self.crop.height
        } else {
            self.crop.width
        }
    }

    pub fn display_height(self) -> f64 {
        if matches!(self.rotation, 90 | 270) {
            self.crop.width
        } else {
            self.crop.height
        }
    }

    /// Transform one PDF user-space point into page-local display space.
    /// This shares the exact rotation/crop convention used by page layout,
    /// but omits the page's document-space placement.
    pub fn user_point_to_display(self, point: crate::geometry::PointF) -> crate::geometry::PointF {
        match self.rotation {
            90 => crate::geometry::PointF {
                x: point.y - self.crop.y,
                y: point.x - self.crop.x,
            },
            180 => crate::geometry::PointF {
                x: self.crop.right() - point.x,
                y: point.y - self.crop.y,
            },
            270 => crate::geometry::PointF {
                x: self.crop.bottom() - point.y,
                y: self.crop.right() - point.x,
            },
            _ => crate::geometry::PointF {
                x: point.x - self.crop.x,
                y: self.crop.bottom() - point.y,
            },
        }
    }

    pub fn user_rect_to_display(self, rect: RectF) -> Option<RectF> {
        let points = [
            crate::geometry::PointF {
                x: rect.x,
                y: rect.y,
            },
            crate::geometry::PointF {
                x: rect.right(),
                y: rect.y,
            },
            crate::geometry::PointF {
                x: rect.right(),
                y: rect.bottom(),
            },
            crate::geometry::PointF {
                x: rect.x,
                y: rect.bottom(),
            },
        ]
        .map(|point| self.user_point_to_display(point));
        let x0 = points
            .iter()
            .map(|point| point.x)
            .fold(f64::INFINITY, f64::min);
        let y0 = points
            .iter()
            .map(|point| point.y)
            .fold(f64::INFINITY, f64::min);
        let x1 = points
            .iter()
            .map(|point| point.x)
            .fold(f64::NEG_INFINITY, f64::max);
        let y1 = points
            .iter()
            .map(|point| point.y)
            .fold(f64::NEG_INFINITY, f64::max);
        [x0, y0, x1, y1]
            .iter()
            .all(|value| value.is_finite())
            .then_some(RectF {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            })
    }

    pub fn display_box(self) -> RectF {
        RectF {
            x: 0.0,
            y: 0.0,
            width: self.display_width(),
            height: self.display_height(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DocumentDescriptor {
    pub id: DocumentId,
    pub display_name: String,
    pub page_count: u32,
    pub page_geometries: Arc<[PageGeometry]>,
    pub outline: Arc<[OutlineNode]>,
    pub page_links: Arc<[Arc<[DocumentLink]>]>,
}

#[derive(Debug, Clone)]
pub struct SyntheticSemanticPage {
    pub page: PageIndex,
    pub line_count: usize,
}

#[derive(Debug, Clone)]
pub struct SyntheticCompiledPage {
    pub page: PageIndex,
    pub geometry: PageGeometry,
    pub operation_count: usize,
}

#[derive(Debug, Clone)]
pub enum SemanticArtifact {
    Synthetic(Arc<SyntheticSemanticPage>),
    #[cfg(feature = "pdf-engine")]
    Pdf(Arc<pdf_content::SemanticPage>),
}

#[derive(Debug, Clone)]
pub enum CompiledArtifact {
    Synthetic(Arc<SyntheticCompiledPage>),
    #[cfg(feature = "pdf-engine")]
    Pdf(Arc<pdf_page_ir::CompiledPage>),
}

impl CompiledArtifact {
    pub fn operation_count(&self) -> usize {
        match self {
            Self::Synthetic(page) => page.operation_count,
            #[cfg(feature = "pdf-engine")]
            Self::Pdf(page) => page.operations.len(),
        }
    }

    pub fn estimated_peak_bytes(&self) -> u64 {
        match self {
            Self::Synthetic(page) => (page.operation_count * 64) as u64,
            #[cfg(feature = "pdf-engine")]
            Self::Pdf(page) => page.complexity.estimated_peak_bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TextArtifact {
    Synthetic(Arc<TextSubstrate>),
    #[cfg(feature = "pdf-engine")]
    Pdf {
        native: Arc<pdf_text::TextPage>,
        substrate: Arc<TextSubstrate>,
    },
}

impl TextArtifact {
    pub fn substrate(&self) -> &Arc<TextSubstrate> {
        match self {
            Self::Synthetic(substrate) => substrate,
            #[cfg(feature = "pdf-engine")]
            Self::Pdf { substrate, .. } => substrate,
        }
    }

    pub fn lines(&self) -> &Arc<PageLineSet> {
        &self.substrate().lines
    }
}

#[derive(Debug, Clone)]
pub struct CompiledArtifacts {
    pub page: PageIndex,
    pub geometry: PageGeometry,
    pub semantic: SemanticArtifact,
    pub compiled: CompiledArtifact,
    pub text: TextArtifact,
    pub structure: PageStructure,
    pub lowering_degraded: bool,
}

impl CompiledArtifacts {
    pub fn estimated_compiled_bytes(&self) -> u64 {
        self.compiled.estimated_peak_bytes()
    }

    pub fn estimated_text_bytes(&self) -> u64 {
        (self.text.substrate().characters.len() * 96) as u64
            + (self.text.substrate().utf16.len() * 2) as u64
            + (self.text.substrate().lines.lines.len() * 64) as u64
    }

    pub fn estimated_bytes(&self) -> u64 {
        self.estimated_compiled_bytes() + self.estimated_text_bytes()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RasterPass {
    Thumbnail,
    Draft,
    TextFirst,
    Final,
}

#[derive(Debug, Clone, Default)]
pub struct CancellationFlag {
    cancelled: Arc<AtomicBool>,
}

impl CancellationFlag {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    #[cfg(feature = "pdf-engine")]
    pub(crate) fn shared_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }
}

#[derive(Debug, thiserror::Error, Clone)]
pub enum DocumentEngineError {
    #[error("operation cancelled")]
    Cancelled,
    #[error("text-first pass is not implemented by this engine yet")]
    TextFirstUnsupported,
    #[error("page {0:?} is out of range")]
    PageOutOfRange(PageIndex),
    /// The document is encrypted and no password has been supplied yet.
    ///
    /// Distinct from [`DocumentEngineError::Engine`] on purpose: this is the
    /// one open failure that is not a failure at all, only an unanswered
    /// question, and the viewer has to ask it rather than report it.
    #[error("this document is protected by a password")]
    PasswordRequired,
    /// A password was supplied and did not open the document.
    #[error("that password did not open the document")]
    IncorrectPassword,
    #[error("document engine failed: {0}")]
    Engine(String),
    #[error("document worker panicked: {0}")]
    Panic(String),
}

/// Persistent state owned by one compile-pool thread. Renderer parse/context
/// scratch stays hot across pages and is never shared or locked.
pub trait DocumentCompileWorker: Send {
    fn compile_page(
        &mut self,
        page: PageIndex,
        page_to_doc: Affine,
        cancellation: &CancellationFlag,
    ) -> Result<Arc<CompiledArtifacts>, DocumentEngineError>;
}

/// Persistent state owned by one raster-pool thread. The PDF implementation
/// keeps a `CpuWorkerContext` here while document-scoped font/glyph/image
/// caches remain shared by the backend.
pub trait DocumentRasterWorker: Send {
    fn raster_tile(
        &mut self,
        artifacts: &CompiledArtifacts,
        bucket: ZoomBucket,
        demand: TileDemand,
        pass: RasterPass,
        generation: u64,
        cancellation: &CancellationFlag,
    ) -> Result<TileSurface, DocumentEngineError>;
}

/// A document-engine boundary, not a bitmap-server boundary. Compilation
/// returns the semantic page, native text page, viewer line substrate, page
/// structure, and compiled display-list IR together. Raster workers consume
/// that retained IR and preserve worker-local renderer state across jobs.
pub trait DocumentEngine: Send + Sync + std::fmt::Debug {
    fn descriptor(&self) -> &DocumentDescriptor;

    fn create_compile_worker(&self) -> Box<dyn DocumentCompileWorker>;
    fn create_raster_worker(&self) -> Box<dyn DocumentRasterWorker>;

    /// Convenience entry point for tests and one-off tools. The viewer
    /// conductor uses persistent workers instead.
    fn compile_page(
        &self,
        page: PageIndex,
        page_to_doc: Affine,
        cancellation: &CancellationFlag,
    ) -> Result<Arc<CompiledArtifacts>, DocumentEngineError> {
        self.create_compile_worker()
            .compile_page(page, page_to_doc, cancellation)
    }

    /// Convenience entry point for tests and one-off tools. The viewer
    /// conductor uses persistent workers instead.
    fn raster_tile(
        &self,
        artifacts: &CompiledArtifacts,
        bucket: ZoomBucket,
        demand: TileDemand,
        pass: RasterPass,
        generation: u64,
        cancellation: &CancellationFlag,
    ) -> Result<TileSurface, DocumentEngineError> {
        self.create_raster_worker().raster_tile(
            artifacts,
            bucket,
            demand,
            pass,
            generation,
            cancellation,
        )
    }

    fn supports_text_first(&self, artifacts: &CompiledArtifacts) -> bool {
        !artifacts.text.substrate().characters.is_empty()
    }

    fn source_path(&self) -> Option<&Path> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::PointF;

    #[test]
    fn destination_points_follow_page_rotation_and_crop() {
        let geometry = PageGeometry {
            crop: RectF {
                x: 10.0,
                y: 20.0,
                width: 200.0,
                height: 300.0,
            },
            rotation: 0,
        };
        assert_eq!(
            geometry.user_point_to_display(PointF { x: 25.0, y: 290.0 }),
            PointF { x: 15.0, y: 30.0 }
        );
        assert_eq!(
            PageGeometry {
                rotation: 90,
                ..geometry
            }
            .user_point_to_display(PointF { x: 25.0, y: 290.0 }),
            PointF { x: 270.0, y: 15.0 }
        );
    }
}
