use std::sync::Arc;

use crate::geometry::RectF;

use super::PageIndex;

/// A renderer-side color policy applied while interpreting the compiled page
/// IR. This is intentionally not a post-raster inversion flag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorPolicy {
    Original,
    Night {
        paper_rgb: [u8; 3],
        text_rgb: [u8; 3],
        image_luminance_scale: f32,
    },
    WarmPaper {
        paper_rgb: [u8; 3],
    },
}

impl Default for ColorPolicy {
    fn default() -> Self {
        Self::Original
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentExtentSource {
    /// Exact bounds accumulated from display-list operations.
    DisplayList,
    /// Temporary fallback until the IR extent pass is implemented.
    PageBox,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContentExtent {
    pub rect: RectF,
    pub source: ContentExtentSource,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    Internal {
        page: PageIndex,
        target_region: Option<RectF>,
    },
    External(Arc<str>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentLink {
    pub source_region: RectF,
    pub target: LinkTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkPeekRequest {
    pub source_page: PageIndex,
    pub target_page: PageIndex,
    pub target_region: RectF,
    pub preferred_width_device_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineSource {
    Embedded,
    Synthesized,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutlineNode {
    pub title: Arc<str>,
    pub page: PageIndex,
    pub target_region: Option<RectF>,
    pub depth: u16,
    pub source: OutlineSource,
}

/// Structure emitted alongside semantic/text/IR artifacts. Placeholder values
/// are explicit, so an implementing agent can replace them with exact IR and
/// document-tree extraction without changing viewer ownership or messaging.
#[derive(Debug, Clone)]
pub struct PageStructure {
    pub content_extent: ContentExtent,
    pub links: Arc<[DocumentLink]>,
}

impl PageStructure {
    pub fn page_box(rect: RectF) -> Self {
        Self {
            content_extent: ContentExtent {
                rect,
                source: ContentExtentSource::PageBox,
            },
            links: Arc::from([]),
        }
    }
}
