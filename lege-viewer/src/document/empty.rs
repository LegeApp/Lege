//! The document engine the viewer runs on before a file is chosen.
//!
//! Launching without a document still needs a live application: a window, the
//! toolbar, and the Open button. A zero-page engine keeps layout, conductor,
//! and planner on their ordinary paths with simply nothing to do, so opening a
//! real document later is the same transition as replacing one.

use std::sync::Arc;

use crate::geometry::Affine;

use super::engine::{
    CancellationFlag, CompiledArtifacts, DocumentCompileWorker, DocumentDescriptor, DocumentEngine,
    DocumentEngineError, DocumentRasterWorker, RasterPass,
};
use super::tile::{TileDemand, TileSurface, ZoomBucket};
use super::{DocumentId, PageIndex};

#[derive(Debug, Clone)]
pub struct EmptyEngine {
    descriptor: DocumentDescriptor,
}

impl EmptyEngine {
    pub fn new() -> Self {
        Self {
            descriptor: DocumentDescriptor {
                id: DocumentId(0),
                display_name: "No document".to_owned(),
                page_count: 0,
                page_geometries: Vec::new().into(),
                outline: Vec::new().into(),
                page_links: Vec::new().into(),
            },
        }
    }
}

impl Default for EmptyEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// A worker that can never be handed work: the document has no pages, so the
/// conductor never plans a compile or a raster for it.
#[derive(Debug)]
struct EmptyWorker;

impl DocumentCompileWorker for EmptyWorker {
    fn compile_page(
        &mut self,
        page: PageIndex,
        _page_to_doc: Affine,
        _cancellation: &CancellationFlag,
    ) -> Result<Arc<CompiledArtifacts>, DocumentEngineError> {
        Err(DocumentEngineError::PageOutOfRange(page))
    }
}

impl DocumentRasterWorker for EmptyWorker {
    fn raster_tile(
        &mut self,
        _artifacts: &CompiledArtifacts,
        _bucket: ZoomBucket,
        demand: TileDemand,
        _pass: RasterPass,
        _generation: u64,
        _cancellation: &CancellationFlag,
    ) -> Result<TileSurface, DocumentEngineError> {
        Err(DocumentEngineError::PageOutOfRange(demand.page))
    }
}

impl DocumentEngine for EmptyEngine {
    fn descriptor(&self) -> &DocumentDescriptor {
        &self.descriptor
    }

    fn create_compile_worker(&self) -> Box<dyn DocumentCompileWorker> {
        Box::new(EmptyWorker)
    }

    fn create_raster_worker(&self) -> Box<dyn DocumentRasterWorker> {
        Box::new(EmptyWorker)
    }
}
