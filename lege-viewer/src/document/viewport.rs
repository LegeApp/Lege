use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use crate::geometry::{RectF, SizeF, Vec2d};

use super::PageIndex;
use super::layout::PageLayoutIndex;
use super::tile::{TILE_SIZE, TileCoord, TileDemand, TileTier, ZoomBucket};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
    Stationary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationMode {
    SequentialForward,
    SequentialBackward,
    JumpLikely,
    Skimming,
    Idle,
}

#[derive(Debug, Clone)]
pub struct ViewportIntent {
    pub generation: u64,
    pub viewport_document: RectF,
    pub viewport_device: SizeF,
    pub zoom: f64,
    pub bucket: ZoomBucket,
    pub velocity: Vec2d,
    pub direction: ScrollDirection,
    pub navigation_mode: NavigationMode,
    pub visible_tiles: Arc<[TileDemand]>,
    pub overscan_tiles: Arc<[TileDemand]>,
    /// Exact-zoom final tiles prepared for the pages most likely to become
    /// visible next. These are deliberately bounded so high zoom levels do
    /// not turn prediction into a full-document render.
    pub final_prefetch_tiles: Arc<[TileDemand]>,
    /// Low-resolution whole-page fallbacks for the wider prediction ring.
    pub preview_pages: Arc<[PageIndex]>,
    pub compile_pages: Arc<[PageIndex]>,
    pub thumbnail_pages: Arc<[PageIndex]>,
    pub hover_page: Option<PageIndex>,
    pub changed_at: Instant,
}

impl ViewportIntent {
    pub fn empty() -> Self {
        Self {
            generation: 0,
            viewport_document: RectF::ZERO,
            viewport_device: SizeF::default(),
            zoom: 1.0,
            bucket: ZoomBucket::ONE,
            velocity: Vec2d::ZERO,
            direction: ScrollDirection::Stationary,
            navigation_mode: NavigationMode::Idle,
            visible_tiles: Arc::from([]),
            overscan_tiles: Arc::from([]),
            final_prefetch_tiles: Arc::from([]),
            preview_pages: Arc::from([]),
            compile_pages: Arc::from([]),
            thumbnail_pages: Arc::from([]),
            hover_page: None,
            changed_at: Instant::now(),
        }
    }

    pub fn page_is_relevant(&self, page: PageIndex) -> bool {
        self.compile_pages.contains(&page)
    }

    pub fn thumbnail_page_is_relevant(&self, page: PageIndex) -> bool {
        self.thumbnail_pages.contains(&page) || self.preview_pages.contains(&page)
    }

    pub fn tile_is_relevant(&self, page: PageIndex, coord: TileCoord) -> bool {
        self.visible_tiles
            .iter()
            .chain(self.overscan_tiles.iter())
            .chain(self.final_prefetch_tiles.iter())
            .any(|demand| demand.page == page && demand.coord == coord)
    }

    pub fn tile_is_visible(&self, page: PageIndex, coord: TileCoord) -> bool {
        self.visible_tiles
            .iter()
            .any(|demand| demand.page == page && demand.coord == coord)
    }

    pub fn tile_is_final_prefetch(&self, page: PageIndex, coord: TileCoord) -> bool {
        self.final_prefetch_tiles
            .iter()
            .any(|demand| demand.page == page && demand.coord == coord)
    }

    pub fn raster_tile_is_relevant(
        &self,
        page: PageIndex,
        bucket: ZoomBucket,
        coord: TileCoord,
        tier: TileTier,
    ) -> bool {
        if tier == TileTier::Thumbnail {
            return self.thumbnail_page_is_relevant(page);
        }
        if bucket != self.bucket {
            return false;
        }
        match tier {
            TileTier::Thumbnail => unreachable!(),
            TileTier::Final => {
                self.tile_is_visible(page, coord) || self.tile_is_final_prefetch(page, coord)
            }
            TileTier::Draft | TileTier::TextFirst => self
                .visible_tiles
                .iter()
                .chain(self.overscan_tiles.iter())
                .any(|demand| demand.page == page && demand.coord == coord),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ViewportPlanner {
    pub overscan_screens_ahead: f64,
    pub overscan_screens_behind: f64,
}

impl Default for ViewportPlanner {
    fn default() -> Self {
        Self {
            overscan_screens_ahead: 2.0,
            overscan_screens_behind: 0.75,
        }
    }
}

impl ViewportPlanner {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &self,
        generation: u64,
        layout: &PageLayoutIndex,
        scroll: Vec2d,
        velocity: Vec2d,
        viewport_device: SizeF,
        zoom: f64,
        hover_page: Option<PageIndex>,
    ) -> ViewportIntent {
        let navigation_mode = if velocity.y > 4.0 {
            NavigationMode::SequentialForward
        } else if velocity.y < -4.0 {
            NavigationMode::SequentialBackward
        } else {
            NavigationMode::Idle
        };
        self.build_with_navigation(
            generation,
            layout,
            scroll,
            velocity,
            viewport_device,
            zoom,
            hover_page,
            navigation_mode,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build_with_navigation(
        &self,
        generation: u64,
        layout: &PageLayoutIndex,
        scroll: Vec2d,
        velocity: Vec2d,
        viewport_device: SizeF,
        zoom: f64,
        hover_page: Option<PageIndex>,
        navigation_mode: NavigationMode,
    ) -> ViewportIntent {
        let zoom = zoom.max(0.01);
        let viewport_document = RectF {
            x: scroll.x / zoom,
            y: scroll.y / zoom,
            width: viewport_device.width / zoom,
            height: viewport_device.height / zoom,
        };
        let direction = match navigation_mode {
            NavigationMode::SequentialForward => ScrollDirection::Down,
            NavigationMode::SequentialBackward => ScrollDirection::Up,
            NavigationMode::JumpLikely | NavigationMode::Skimming | NavigationMode::Idle => {
                ScrollDirection::Stationary
            }
        };
        let ahead = viewport_document.height * self.overscan_screens_ahead;
        let behind = viewport_document.height * self.overscan_screens_behind;
        let overscan = match direction {
            ScrollDirection::Down => RectF {
                y: (viewport_document.y - behind).max(0.0),
                height: viewport_document.height + ahead + behind,
                ..viewport_document
            },
            ScrollDirection::Up => RectF {
                y: (viewport_document.y - ahead).max(0.0),
                height: viewport_document.height + ahead + behind,
                ..viewport_document
            },
            ScrollDirection::Stationary => RectF {
                y: (viewport_document.y - viewport_document.height).max(0.0),
                height: viewport_document.height * 3.0,
                ..viewport_document
            },
        };

        let bucket = ZoomBucket::from_zoom(zoom);
        let raster_scale = bucket.scale();
        let visible_tiles = tile_demands(
            layout,
            viewport_document,
            viewport_document,
            raster_scale,
            true,
        );
        let overscan_tiles = tile_demands(layout, overscan, viewport_document, raster_scale, false)
            .into_iter()
            .filter(|candidate| {
                !visible_tiles.iter().any(|visible| {
                    visible.page == candidate.page && visible.coord == candidate.coord
                })
            })
            .collect::<Vec<_>>();

        let visible_pages = unique_pages(visible_tiles.iter().map(|demand| demand.page));
        let first_visible = visible_pages.first().copied();
        let last_visible = visible_pages.last().copied();
        let predicted_pages =
            predicted_pages(layout, first_visible, last_visible, navigation_mode, 10);

        // A PageDown should normally enter an already-rendered page.  Keep
        // this strictly bounded, but give sequential reading three exact
        // pages of runway before falling back to low-resolution previews.
        const FINAL_PREFETCH_TILE_BUDGET: usize = 160;
        const FINAL_PREFETCH_PAGE_LIMIT: usize = 5;
        let mut final_prefetch_tiles = Vec::new();
        for page in predicted_pages
            .iter()
            .copied()
            .take(FINAL_PREFETCH_PAGE_LIMIT)
        {
            let Some(placement) = layout.placement(page) else {
                continue;
            };
            let page_demands = tile_demands(
                layout,
                placement.bounds,
                viewport_document,
                raster_scale,
                false,
            );
            if page_demands.is_empty()
                || final_prefetch_tiles.len() + page_demands.len() > FINAL_PREFETCH_TILE_BUDGET
            {
                continue;
            }
            final_prefetch_tiles.extend(page_demands);
        }

        let final_prefetch_pages =
            unique_pages(final_prefetch_tiles.iter().map(|demand| demand.page));
        let preview_pages = predicted_pages
            .iter()
            .copied()
            .filter(|page| !final_prefetch_pages.contains(page))
            .take(8)
            .collect::<Vec<_>>();

        // Compilation order is scheduling order: visible pages first, then
        // exact-quality predictions, nearby overscan, and finally previews.
        let mut pages = Vec::new();
        extend_unique(&mut pages, visible_pages.iter().copied());
        extend_unique(&mut pages, final_prefetch_pages.iter().copied());
        let overscan_pages = unique_pages(overscan_tiles.iter().map(|demand| demand.page));
        match direction {
            ScrollDirection::Up => extend_unique(&mut pages, overscan_pages.iter().rev().copied()),
            ScrollDirection::Down | ScrollDirection::Stationary => {
                extend_unique(&mut pages, overscan_pages.iter().copied());
            }
        }
        extend_unique(&mut pages, preview_pages.iter().copied());

        let mut thumbnail_pages = BTreeSet::new();
        if let Some(hover) = hover_page {
            let start = hover.0.saturating_sub(5);
            let end = hover.0.saturating_add(5);
            for number in start..=end {
                let page = PageIndex(number);
                if layout.placement(page).is_some() {
                    thumbnail_pages.insert(page);
                    push_unique(&mut pages, page);
                }
            }
        }

        ViewportIntent {
            generation,
            viewport_document,
            viewport_device,
            zoom,
            bucket,
            velocity,
            direction,
            navigation_mode,
            visible_tiles: visible_tiles.into(),
            overscan_tiles: overscan_tiles.into(),
            final_prefetch_tiles: final_prefetch_tiles.into(),
            preview_pages: preview_pages.into(),
            compile_pages: pages.into(),
            thumbnail_pages: thumbnail_pages.into_iter().collect::<Vec<_>>().into(),
            hover_page,
            changed_at: Instant::now(),
        }
    }
}

fn predicted_pages(
    layout: &PageLayoutIndex,
    first_visible: Option<PageIndex>,
    last_visible: Option<PageIndex>,
    mode: NavigationMode,
    limit: usize,
) -> Vec<PageIndex> {
    let Some(first) = first_visible else {
        return Vec::new();
    };
    let last = last_visible.unwrap_or(first);
    let mut pages = Vec::with_capacity(limit);
    match mode {
        NavigationMode::SequentialForward => {
            for offset in 1..limit as u32 {
                push_if_present(layout, &mut pages, last.0.saturating_add(offset));
            }
            if let Some(number) = first.0.checked_sub(1) {
                push_if_present(layout, &mut pages, number);
            }
        }
        NavigationMode::SequentialBackward => {
            for offset in 1..limit as u32 {
                if let Some(number) = first.0.checked_sub(offset) {
                    push_if_present(layout, &mut pages, number);
                }
            }
            push_if_present(layout, &mut pages, last.0.saturating_add(1));
        }
        NavigationMode::JumpLikely | NavigationMode::Skimming => {
            for offset in 1..=limit.div_ceil(2) as u32 {
                push_if_present(layout, &mut pages, last.0.saturating_add(offset));
                if let Some(number) = first.0.checked_sub(offset) {
                    push_if_present(layout, &mut pages, number);
                }
            }
        }
        NavigationMode::Idle => {
            for offset in 1..=limit.div_ceil(2) as u32 {
                push_if_present(layout, &mut pages, last.0.saturating_add(offset));
                if let Some(number) = first.0.checked_sub(offset) {
                    push_if_present(layout, &mut pages, number);
                }
            }
        }
    }
    pages.truncate(limit);
    pages
}

fn push_if_present(layout: &PageLayoutIndex, pages: &mut Vec<PageIndex>, number: u32) {
    let page = PageIndex(number);
    if layout.placement(page).is_some() {
        push_unique(pages, page);
    }
}

fn unique_pages(pages: impl IntoIterator<Item = PageIndex>) -> Vec<PageIndex> {
    let mut unique = Vec::new();
    extend_unique(&mut unique, pages);
    unique
}

fn extend_unique(pages: &mut Vec<PageIndex>, additions: impl IntoIterator<Item = PageIndex>) {
    for page in additions {
        push_unique(pages, page);
    }
}

fn push_unique(pages: &mut Vec<PageIndex>, page: PageIndex) {
    if !pages.contains(&page) {
        pages.push(page);
    }
}

fn tile_demands(
    layout: &PageLayoutIndex,
    region: RectF,
    viewport: RectF,
    zoom: f64,
    visible: bool,
) -> Vec<TileDemand> {
    let mut demands = Vec::new();
    for placement in &layout.placements()[layout.visible_pages(region)] {
        let Some(exposed) = placement.bounds.intersection(region) else {
            continue;
        };
        let page_local = RectF {
            x: exposed.x - placement.bounds.x,
            y: exposed.y - placement.bounds.y,
            width: exposed.width,
            height: exposed.height,
        };
        let device = RectF {
            x: page_local.x * zoom,
            y: page_local.y * zoom,
            width: page_local.width * zoom,
            height: page_local.height * zoom,
        };
        let tile_x0 = (device.x / f64::from(TILE_SIZE)).floor() as i32;
        let tile_y0 = (device.y / f64::from(TILE_SIZE)).floor() as i32;
        let tile_x1 = (device.right() / f64::from(TILE_SIZE)).ceil() as i32;
        let tile_y1 = (device.bottom() / f64::from(TILE_SIZE)).ceil() as i32;
        for tile_y in tile_y0..tile_y1 {
            for tile_x in tile_x0..tile_x1 {
                let page_device_rect = crate::geometry::RectI {
                    x: tile_x * TILE_SIZE as i32,
                    y: tile_y * TILE_SIZE as i32,
                    width: TILE_SIZE,
                    height: TILE_SIZE,
                };
                let page_document_rect = RectF {
                    x: placement.bounds.x + f64::from(page_device_rect.x) / zoom,
                    y: placement.bounds.y + f64::from(page_device_rect.y) / zoom,
                    width: f64::from(page_device_rect.width) / zoom,
                    height: f64::from(page_device_rect.height) / zoom,
                };
                let distance = vertical_distance(page_document_rect, viewport);
                demands.push(TileDemand {
                    page: placement.page,
                    coord: TileCoord {
                        x: tile_x,
                        y: tile_y,
                    },
                    page_device_rect,
                    page_document_rect,
                    distance_from_viewport: distance,
                    visible,
                    page_view_box: placement.view_box,
                    color_mode: layout.color_mode,
                    variant: layout.render_variant,
                });
            }
        }
    }
    demands
}

pub fn thumbnail_demands(
    layout: &PageLayoutIndex,
    page: PageIndex,
    preferred_width_device_px: u32,
) -> Option<(ZoomBucket, Vec<TileDemand>)> {
    let placement = layout.placement(page)?;
    let desired_scale =
        f64::from(preferred_width_device_px.max(32)) / placement.bounds.width.max(1.0);
    let bucket = ZoomBucket::from_zoom(desired_scale);
    let scale = bucket.scale();
    let width = (placement.bounds.width * scale).ceil().max(1.0) as i32;
    let height = (placement.bounds.height * scale).ceil().max(1.0) as i32;
    let tile_x1 = (f64::from(width) / f64::from(TILE_SIZE)).ceil() as i32;
    let tile_y1 = (f64::from(height) / f64::from(TILE_SIZE)).ceil() as i32;
    let mut demands = Vec::with_capacity((tile_x1 * tile_y1).max(0) as usize);
    for tile_y in 0..tile_y1 {
        for tile_x in 0..tile_x1 {
            let x = tile_x * TILE_SIZE as i32;
            let y = tile_y * TILE_SIZE as i32;
            let tile_width = (width - x).clamp(1, TILE_SIZE as i32) as u32;
            let tile_height = (height - y).clamp(1, TILE_SIZE as i32) as u32;
            let page_device_rect = crate::geometry::RectI {
                x,
                y,
                width: tile_width,
                height: tile_height,
            };
            demands.push(TileDemand {
                page,
                coord: TileCoord {
                    x: tile_x,
                    y: tile_y,
                },
                page_device_rect,
                page_document_rect: RectF {
                    x: placement.bounds.x + f64::from(x) / scale,
                    y: placement.bounds.y + f64::from(y) / scale,
                    width: f64::from(tile_width) / scale,
                    height: f64::from(tile_height) / scale,
                },
                distance_from_viewport: 0.0,
                visible: false,
                page_view_box: placement.view_box,
                color_mode: layout.color_mode,
                variant: layout.render_variant,
            });
        }
    }
    Some((bucket, demands))
}

fn vertical_distance(rect: RectF, viewport: RectF) -> f64 {
    if rect.intersects(viewport) {
        0.0
    } else if rect.bottom() < viewport.y {
        viewport.y - rect.bottom()
    } else {
        rect.y - viewport.bottom()
    }
}
