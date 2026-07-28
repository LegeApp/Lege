pub mod cache;
pub mod conductor;
pub mod empty;
pub mod engine;
pub mod features;
pub mod layout;
#[cfg(feature = "pdf-engine")]
pub mod pdf_engine;
pub mod preview;
pub mod session;
pub mod synthetic;
pub mod tile;
pub mod viewport;

pub use cache::{CacheCategory, MemoryArbiter, MemoryLease, TileCache, TileFrameSnapshot};
pub use conductor::{ConductorCommand, ConductorHandle, WarmHint, WarmReason};
pub use empty::EmptyEngine;
pub use engine::{
    CancellationFlag, CompiledArtifact, CompiledArtifacts, DocumentCompileWorker,
    DocumentDescriptor, DocumentEngine, DocumentEngineError, DocumentRasterWorker, PageGeometry,
    RasterPass, SemanticArtifact, TextArtifact,
};
pub use features::{
    ColorMode, ContentExtent, ContentExtentSource, DocumentLink, LinkPeekRequest, LinkTarget,
    OutlineNode, OutlineSource, PageStructure,
};
pub use layout::{PageLayoutIndex, PagePlacement};
pub use preview::{PagePreviewCache, canonical_preview_demand};
pub use session::{PageArtifactUpdate, SessionUpdate, UpdateQueue, WakeSink};
pub use tile::{TILE_SIZE, TileCoord, TileDemand, TileKey, TileSurface, TileTier, ZoomBucket};
pub use viewport::{
    NavigationMode, ScrollDirection, ViewportIntent, ViewportPlanner, thumbnail_demands,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocumentId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageIndex(pub u32);
