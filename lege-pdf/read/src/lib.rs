//! Lege-owned document intake and reading seam.
//!
//! Renderer types stay private to this crate. The processing pipeline can
//! adopt this API without coupling itself to the temporary renderer path.

mod intake;
mod metadata;
mod outline;
mod session;
mod text;

pub use intake::{CompileStatus, DocumentIntake, examine_document};
pub use metadata::{DocumentMetadata, extract_metadata};
pub use outline::{OwnedBookmarkNode, extract_outline};
pub use session::{
    AnalysisTarget, BaseTarget, CancellationToken, CompiledDocumentPage, DeviceCrop,
    GraySuitability, GraySurface, Jbig2StencilPolarity, OcrTarget, PageGeometry, PageOutputPlan,
    PageRasterProducts, PreservedJbig2Smask, RasterFormat, RasterPlane, RasterProduct, ReadError,
    RegionTarget, RenderSession, RenderedRegion, RgbSurface,
};
pub use text::{
    DirectScanImage, NativeTextWord, PageContentKind, PageTextEvidence, has_text_layer, page_text,
    page_text_evidence, positioned_words,
};
