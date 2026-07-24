use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use crate::geometry::{RectF, SizeF, Vec2d};

use super::layout::PageLayoutIndex;
use super::tile::{TILE_SIZE, TileCoord, TileDemand, ZoomBucket};
use super::PageIndex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
    Stationary,
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
    pub visible_tiles: Arc<[TileDemand]>,
    pub overscan_tiles: Arc<[TileDemand]>,
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
            visible_tiles: Arc::from([]),
            overscan_tiles: Arc::from([]),
            compile_pages: Arc::from([]),
            thumbnail_pages: Arc::from([]),
            hover_page: None,
            changed_at: Instant::now(),
        }
    }

    pub fn page_is_relevant(&self, page: PageIndex) -> bool {
        self.compile_pages.binary_search(&page).is_ok()
    }

    pub fn thumbnail_page_is_relevant(&self, page: PageIndex) -> bool {
        self.thumbnail_pages.binary_search(&page).is_ok()
    }

    pub fn tile_is_relevant(&self, page: PageIndex, coord: TileCoord) -> bool {
        self.visible_tiles
            .iter()
            .chain(self.overscan_tiles.iter())
            .any(|demand| demand.page == page && demand.coord == coord)
    }

    pub fn speed_pages_per_second_hint(&self) -> f64 {
        let page_height = self.viewport_document.height.max(1.0);
        self.velocity.y.abs() / page_height
    }
}

#[derive(Debug, Clone)]
pub struct ViewportPlanner {
    pub overscan_screens_ahead: f64,
    pub overscan_screens_behind: f64,
    pub compile_extra_pages: usize,
}

impl Default for ViewportPlanner {
    fn default() -> Self {
        Self {
            overscan_screens_ahead: 2.0,
            overscan_screens_behind: 0.75,
            compile_extra_pages: 2,
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
        let zoom = zoom.max(0.01);
        let viewport_document = RectF {
            x: scroll.x / zoom,
            y: scroll.y / zoom,
            width: viewport_device.width / zoom,
            height: viewport_device.height / zoom,
        };
        let direction = if velocity.y > 4.0 {
            ScrollDirection::Down
        } else if velocity.y < -4.0 {
            ScrollDirection::Up
        } else {
            ScrollDirection::Stationary
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
                !visible_tiles
                    .iter()
                    .any(|visible| visible.page == candidate.page && visible.coord == candidate.coord)
            })
            .collect::<Vec<_>>();

        let mut pages = BTreeSet::new();
        for demand in visible_tiles.iter().chain(overscan_tiles.iter()) {
            pages.insert(demand.page);
        }
        if let Some(first) = pages.first().copied() {
            for offset in 1..=self.compile_extra_pages {
                pages.insert(PageIndex(first.0.saturating_sub(offset as u32)));
            }
        }
        if let Some(last) = pages.last().copied() {
            for offset in 1..=self.compile_extra_pages {
                let candidate = PageIndex(last.0.saturating_add(offset as u32));
                if layout.placement(candidate).is_some() {
                    pages.insert(candidate);
                }
            }
        }
        pages.retain(|page| layout.placement(*page).is_some());

        let mut thumbnail_pages = BTreeSet::new();
        if let Some(hover) = hover_page {
            let start = hover.0.saturating_sub(5);
            let end = hover.0.saturating_add(5);
            for number in start..=end {
                let page = PageIndex(number);
                if layout.placement(page).is_some() {
                    thumbnail_pages.insert(page);
                    pages.insert(page);
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
            visible_tiles: visible_tiles.into(),
            overscan_tiles: overscan_tiles.into(),
            compile_pages: pages.into_iter().collect::<Vec<_>>().into(),
            thumbnail_pages: thumbnail_pages.into_iter().collect::<Vec<_>>().into(),
            hover_page,
            changed_at: Instant::now(),
        }
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
                    x: placement.bounds.x
                        + f64::from(page_device_rect.x) / zoom,
                    y: placement.bounds.y
                        + f64::from(page_device_rect.y) / zoom,
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
    let desired_scale = f64::from(preferred_width_device_px.max(32))
        / placement.bounds.width.max(1.0);
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
                coord: TileCoord { x: tile_x, y: tile_y },
                page_device_rect,
                page_document_rect: RectF {
                    x: placement.bounds.x + f64::from(x) / scale,
                    y: placement.bounds.y + f64::from(y) / scale,
                    width: f64::from(tile_width) / scale,
                    height: f64::from(tile_height) / scale,
                },
                distance_from_viewport: 0.0,
                visible: false,
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
