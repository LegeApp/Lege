//! Lege-owned document intake and reading seam.
//!
//! Renderer types stay private to this crate. The processing pipeline can
//! adopt this API without coupling itself to the temporary renderer path.

mod intake;
mod outline;
mod session;
mod text;

pub use intake::{CompileStatus, DocumentIntake, examine_document};
pub use outline::{OwnedBookmarkNode, extract_outline};
pub use session::{
    AnalysisTarget, BaseTarget, CancellationToken, CompiledDocumentPage, DeviceCrop,
    GraySuitability, GraySurface, OcrTarget, PageGeometry, PageOutputPlan, PageRasterProducts,
    RasterFormat, RasterPlane, RasterProduct, ReadError, RegionTarget, RenderSession,
    RenderedRegion, RgbSurface,
};
pub use text::{NativeTextWord, has_text_layer, page_text, positioned_words};
