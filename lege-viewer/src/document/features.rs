use std::sync::Arc;

use crate::geometry::RectF;

use super::PageIndex;

/// A renderer-side color policy applied while interpreting the compiled page
/// IR. This is intentionally not a post-raster inversion flag.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ColorMode {
    #[default]
    Original,
    Night,
    WarmPaper,
    /// Curated native UI palette inspired by the retained Sanzo Wada work.
    /// Page pixels stay original so a decorative palette never changes scans.
    SanzoEarth,
    /// A cooler companion palette with the same original-pixel policy.
    SanzoSea,
}

impl ColorMode {
    pub fn next(self) -> Self {
        match self {
            Self::Original => Self::Night,
            Self::Night => Self::WarmPaper,
            Self::WarmPaper => Self::SanzoEarth,
            Self::SanzoEarth => Self::SanzoSea,
            Self::SanzoSea => Self::Original,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Night => "Night",
            Self::WarmPaper => "Warm",
            Self::SanzoEarth => "Earth",
            Self::SanzoSea => "Sea",
        }
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
