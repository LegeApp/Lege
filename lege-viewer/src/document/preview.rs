use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::geometry::{RectF, RectI};

use super::PageIndex;
use super::cache::{CacheCategory, MemoryArbiter, MemoryLease};
use super::layout::PageLayoutIndex;
use super::tile::{TileCoord, TileDemand, TileSurface, ZoomBucket};

const PREVIEW_MEMORY_BUDGET: u64 = 128 * 1024 * 1024;
const PREFERRED_PREVIEW_WIDTH: u32 = 160;
const MIN_PREVIEW_PIXELS: u64 = 8 * 8;

#[derive(Debug)]
struct PreviewEntry {
    surface: Arc<TileSurface>,
    _lease: MemoryLease,
}

/// A document-sized, directly indexed cache for one canonical fallback per
/// page. The per-page resolution adapts to the document size so the complete
/// L0 sweep has a fixed memory ceiling instead of competing with exact tiles.
#[derive(Debug)]
pub struct PagePreviewCache {
    pages: Box<[ArcSwapOption<PreviewEntry>]>,
    arbiter: MemoryArbiter,
    max_pixels_per_page: u64,
}

impl PagePreviewCache {
    pub fn new(page_count: u32, arbiter: MemoryArbiter) -> Self {
        let page_count = u64::from(page_count.max(1));
        let max_pixels_per_page = (PREVIEW_MEMORY_BUDGET / 4 / page_count).max(MIN_PREVIEW_PIXELS);
        let pages = (0..page_count)
            .map(|_| ArcSwapOption::empty())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            pages,
            arbiter,
            max_pixels_per_page,
        }
    }

    pub fn contains(&self, page: PageIndex) -> bool {
        self.pages
            .get(page.0 as usize)
            .is_some_and(|slot| slot.load().is_some())
    }

    pub fn contains_variant(&self, page: PageIndex, variant: u64) -> bool {
        self.pages.get(page.0 as usize).is_some_and(|slot| {
            slot.load()
                .as_ref()
                .is_some_and(|entry| entry.surface.key.variant == variant)
        })
    }

    pub fn get(&self, page: PageIndex) -> Option<Arc<TileSurface>> {
        self.pages
            .get(page.0 as usize)?
            .load_full()
            .map(|entry| Arc::clone(&entry.surface))
    }

    pub fn get_variant(&self, page: PageIndex, variant: u64) -> Option<Arc<TileSurface>> {
        let surface = self
            .pages
            .get(page.0 as usize)?
            .load_full()
            .map(|entry| Arc::clone(&entry.surface))?;
        (surface.key.variant == variant).then_some(surface)
    }

    pub fn insert(&self, surface: Arc<TileSurface>) {
        let Some(slot) = self.pages.get(surface.key.page.0 as usize) else {
            return;
        };
        let lease = self
            .arbiter
            .reserve(CacheCategory::Thumbnails, surface.byte_len());
        slot.store(Some(Arc::new(PreviewEntry {
            surface,
            _lease: lease,
        })));
    }

    pub fn max_pixels_per_page(&self) -> u64 {
        self.max_pixels_per_page
    }

    pub fn demand(
        &self,
        layout: &PageLayoutIndex,
        page: PageIndex,
    ) -> Option<(ZoomBucket, TileDemand)> {
        canonical_preview_demand(
            layout,
            page,
            PREFERRED_PREVIEW_WIDTH,
            self.max_pixels_per_page,
        )
    }
}

pub fn canonical_preview_demand(
    layout: &PageLayoutIndex,
    page: PageIndex,
    preferred_width: u32,
    max_pixels: u64,
) -> Option<(ZoomBucket, TileDemand)> {
    let placement = layout.placement(page)?;
    let desired_scale = f64::from(preferred_width.max(32)) / placement.bounds.width.max(1.0);
    let mut bucket = ZoomBucket::from_zoom(desired_scale);
    let max_pixels = max_pixels.max(MIN_PREVIEW_PIXELS);

    let (width, height, scale) = loop {
        let scale = bucket.scale();
        let width = (placement.bounds.width * scale).ceil().max(1.0) as u32;
        let height = (placement.bounds.height * scale).ceil().max(1.0) as u32;
        if u64::from(width) * u64::from(height) <= max_pixels || bucket.0 == i16::MIN {
            break (width, height, scale);
        }
        bucket.0 = bucket.0.saturating_sub(1);
    };

    Some((
        bucket,
        TileDemand {
            page,
            coord: TileCoord { x: 0, y: 0 },
            page_device_rect: RectI {
                x: 0,
                y: 0,
                width,
                height,
            },
            page_document_rect: RectF {
                x: placement.bounds.x,
                y: placement.bounds.y,
                width: f64::from(width) / scale,
                height: f64::from(height) / scale,
            },
            distance_from_viewport: 0.0,
            visible: false,
            page_view_box: placement.view_box,
            color_mode: layout.color_mode,
            variant: layout.render_variant,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::PageGeometry;
    use crate::theme::Theme;

    #[test]
    fn canonical_preview_is_a_single_memory_bounded_surface() {
        let layout = PageLayoutIndex::build(
            &[PageGeometry {
                crop: RectF {
                    x: 0.0,
                    y: 0.0,
                    width: 612.0,
                    height: 792.0,
                },
                rotation: 0,
            }],
            &Theme::light().metrics,
        );
        let preview = canonical_preview_demand(&layout, PageIndex(0), 160, 4_096);
        assert!(preview.is_some());
        if let Some((_, demand)) = preview {
            assert_eq!(demand.coord, TileCoord { x: 0, y: 0 });
            assert!(
                u64::from(demand.page_device_rect.width)
                    * u64::from(demand.page_device_rect.height)
                    <= 4_096
            );
        }
    }

    #[test]
    fn huge_documents_receive_smaller_per_page_previews() {
        let memory = MemoryArbiter::new(u64::MAX);
        let small = PagePreviewCache::new(10, memory.clone());
        let huge = PagePreviewCache::new(100_000, memory);
        assert!(small.max_pixels_per_page() > huge.max_pixels_per_page());
    }
}
