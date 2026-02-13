// Core infrastructure
pub mod djvu_dir;
pub mod page_encoder;
pub mod page_collection;

// Public builder API
pub mod builder;

// Private encoder implementation
pub(crate) mod encoder;

// Re-export public builder API
pub use builder::{DjvuBuilder, DjvuDocument, ImageLayer, LayerData, Page, PageBuilder};

// Re-export types needed by the builder
pub use djvu_dir::{Bookmark, DjVmDir, DjVmNav, File as DjVuFile, FileType};
pub use page_collection::{DocumentStatus, PageCollection};
pub use page_encoder::{EncodedPage, PageComponents, PageEncodeParams, PageLayer, Rect};
