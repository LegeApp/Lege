use crate::geometry::{Affine, RectF, SizeF};
use crate::theme::ThemeMetrics;

use super::{PageGeometry, PageIndex};

#[derive(Debug, Clone)]
pub struct PagePlacement {
    pub page: PageIndex,
    pub bounds: RectF,
    /// Page user space (y-up points) to document space (y-down logical px).
    pub page_to_doc: Affine,
}

#[derive(Debug, Clone, Default)]
pub struct PageLayoutIndex {
    placements: Box<[PagePlacement]>,
    page_starts_y: Box<[f64]>,
    pub total_width: f64,
    pub total_height: f64,
}

impl PageLayoutIndex {
    pub fn build(geometries: &[PageGeometry], metrics: &ThemeMetrics) -> Self {
        let total_width = geometries
            .iter()
            .map(PageGeometry::display_width)
            .fold(0.0, f64::max)
            + metrics.canvas_margin * 2.0;
        let mut y = metrics.canvas_margin;
        let mut placements = Vec::with_capacity(geometries.len());
        let mut starts = Vec::with_capacity(geometries.len());
        for (index, geometry) in geometries.iter().enumerate() {
            let width = geometry.display_width();
            let height = geometry.display_height();
            let x = (total_width - width) * 0.5;
            let page = PageIndex(index as u32);
            starts.push(y);
            placements.push(PagePlacement {
                page,
                bounds: RectF {
                    x,
                    y,
                    width,
                    height,
                },
                page_to_doc: page_transform(geometry, x, y),
            });
            y += height + metrics.page_gap;
        }
        let total_height = if placements.is_empty() {
            0.0
        } else {
            y - metrics.page_gap + metrics.canvas_margin
        };
        Self {
            placements: placements.into_boxed_slice(),
            page_starts_y: starts.into_boxed_slice(),
            total_width,
            total_height,
        }
    }

    pub fn placements(&self) -> &[PagePlacement] {
        &self.placements
    }

    pub fn placement(&self, page: PageIndex) -> Option<&PagePlacement> {
        self.placements.get(page.0 as usize)
    }

    pub fn page_at_y(&self, y: f64) -> Option<&PagePlacement> {
        if self.placements.is_empty() {
            return None;
        }
        let index = self
            .page_starts_y
            .partition_point(|start| *start <= y)
            .saturating_sub(1)
            .min(self.placements.len() - 1);
        self.placements.get(index)
    }

    pub fn visible_pages(&self, viewport: RectF) -> std::ops::Range<usize> {
        if self.placements.is_empty() || viewport.bottom() <= 0.0 || viewport.y >= self.total_height {
            return 0..0;
        }
        let mut start = self
            .page_starts_y
            .partition_point(|page_start| *page_start < viewport.y)
            .saturating_sub(1);
        while start < self.placements.len()
            && self.placements[start].bounds.bottom() <= viewport.y
        {
            start += 1;
        }
        let mut end = start;
        while end < self.placements.len() && self.placements[end].bounds.y < viewport.bottom() {
            end += 1;
        }
        start..end
    }

    pub fn extent(&self) -> SizeF {
        SizeF {
            width: self.total_width,
            height: self.total_height,
        }
    }
}

fn page_transform(geometry: &PageGeometry, x: f64, y: f64) -> Affine {
    let crop = geometry.crop;
    match geometry.rotation {
        90 => Affine {
            a: 0.0,
            b: 1.0,
            c: 1.0,
            d: 0.0,
            e: x - crop.y,
            f: y - crop.x,
        },
        180 => Affine {
            a: -1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: x + crop.right(),
            f: y - crop.y,
        },
        270 => Affine {
            a: 0.0,
            b: -1.0,
            c: -1.0,
            d: 0.0,
            e: x + crop.bottom(),
            f: y + crop.right(),
        },
        _ => Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            e: x - crop.x,
            f: y + crop.bottom(),
        },
    }
}
