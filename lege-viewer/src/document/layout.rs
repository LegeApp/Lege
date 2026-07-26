use crate::geometry::{Affine, RectF, SizeF};
use crate::theme::ThemeMetrics;

use super::{ColorMode, PageGeometry, PageIndex};

#[derive(Debug, Clone)]
pub struct PagePlacement {
    pub page: PageIndex,
    pub bounds: RectF,
    /// Visible page-local display-space rectangle. The ordinary page box is
    /// `(0, 0, display_width, display_height)`; trim mode selects a subset.
    pub view_box: RectF,
    /// Page user space (y-up points) to document space (y-down logical px).
    pub page_to_doc: Affine,
}

#[derive(Debug, Clone, Default)]
pub struct PageLayoutIndex {
    placements: Box<[PagePlacement]>,
    page_starts_y: Box<[f64]>,
    pub total_width: f64,
    pub total_height: f64,
    pub render_variant: u64,
    pub color_mode: ColorMode,
}

impl PageLayoutIndex {
    pub fn build(geometries: &[PageGeometry], metrics: &ThemeMetrics) -> Self {
        Self::build_with_options(geometries, &[], false, ColorMode::Original, 0, metrics)
    }

    pub fn build_with_options(
        geometries: &[PageGeometry],
        content_extents: &[Option<RectF>],
        trim_enabled: bool,
        color_mode: ColorMode,
        render_variant: u64,
        metrics: &ThemeMetrics,
    ) -> Self {
        let view_boxes = geometries
            .iter()
            .enumerate()
            .map(|(index, geometry)| {
                let page_box = RectF {
                    x: 0.0,
                    y: 0.0,
                    width: geometry.display_width(),
                    height: geometry.display_height(),
                };
                if trim_enabled {
                    content_extents
                        .get(index)
                        .copied()
                        .flatten()
                        .map(|extent| trim_view_box(page_box, extent))
                        .unwrap_or(page_box)
                } else {
                    page_box
                }
            })
            .collect::<Vec<_>>();
        let total_width = geometries
            .iter()
            .zip(&view_boxes)
            .map(|(_, view)| view.width)
            .fold(0.0, f64::max)
            + metrics.canvas_margin * 2.0;
        let mut y = metrics.canvas_margin;
        let mut placements = Vec::with_capacity(geometries.len());
        let mut starts = Vec::with_capacity(geometries.len());
        for (index, (geometry, view_box)) in geometries.iter().zip(view_boxes).enumerate() {
            let width = view_box.width;
            let height = view_box.height;
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
                view_box,
                page_to_doc: page_transform(geometry, x - view_box.x, y - view_box.y),
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
            render_variant,
            color_mode,
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
        if self.placements.is_empty() || viewport.bottom() <= 0.0 || viewport.y >= self.total_height
        {
            return 0..0;
        }
        let mut start = self
            .page_starts_y
            .partition_point(|page_start| *page_start < viewport.y)
            .saturating_sub(1);
        while start < self.placements.len() && self.placements[start].bounds.bottom() <= viewport.y
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

/// Content-aware crop with restrained padding. Each axis is trimmed only when
/// it saves at least 5% of the page, preventing tiny extent noise from making
/// pages jitter during progressive discovery.
fn trim_view_box(page: RectF, content: RectF) -> RectF {
    let Some(content) = content.intersection(page) else {
        return page;
    };
    if content.width <= 0.0 || content.height <= 0.0 {
        return page;
    }
    let pad_x = (page.width * 0.02).clamp(6.0, 18.0);
    let pad_y = (page.height * 0.02).clamp(6.0, 18.0);
    let padded = RectF {
        x: (content.x - pad_x).max(page.x),
        y: (content.y - pad_y).max(page.y),
        width: (content.right() + pad_x).min(page.right()) - (content.x - pad_x).max(page.x),
        height: (content.bottom() + pad_y).min(page.bottom()) - (content.y - pad_y).max(page.y),
    };
    let trim_x = page.width - padded.width >= page.width * 0.05;
    let trim_y = page.height - padded.height >= page.height * 0.05;
    RectF {
        x: if trim_x { padded.x } else { page.x },
        y: if trim_y { padded.y } else { page.y },
        width: if trim_x { padded.width } else { page.width },
        height: if trim_y { padded.height } else { page.height },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_padding_is_clamped_and_small_gains_are_ignored_per_axis() {
        let page = RectF {
            x: 0.0,
            y: 0.0,
            width: 600.0,
            height: 800.0,
        };
        let trimmed = trim_view_box(
            page,
            RectF {
                x: 100.0,
                y: 10.0,
                width: 400.0,
                height: 780.0,
            },
        );
        assert_eq!(trimmed.x, 88.0, "2% horizontal padding is 12pt");
        assert_eq!(trimmed.width, 424.0);
        assert_eq!(trimmed.y, 0.0, "vertical gain below 5% keeps page box");
        assert_eq!(trimmed.height, 800.0);
    }

    #[test]
    fn trim_layout_keeps_user_to_document_transform_aligned_with_view_box() {
        let geometry = PageGeometry {
            crop: RectF {
                x: 0.0,
                y: 0.0,
                width: 600.0,
                height: 800.0,
            },
            rotation: 0,
        };
        let extent = RectF {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 600.0,
        };
        let layout = PageLayoutIndex::build_with_options(
            &[geometry],
            &[Some(extent)],
            true,
            ColorMode::Night,
            9,
            &crate::theme::Theme::light().metrics,
        );
        let placement = layout.placement(PageIndex(0)).unwrap();
        assert!(placement.view_box.x > 0.0 && placement.view_box.y > 0.0);
        assert_eq!(layout.render_variant, 9);
        assert_eq!(layout.color_mode, ColorMode::Night);
        let top_left_user = crate::geometry::PointF {
            x: placement.view_box.x,
            y: geometry.crop.bottom() - placement.view_box.y,
        };
        let mapped = placement.page_to_doc.apply(top_left_user);
        assert!((mapped.x - placement.bounds.x).abs() < 1e-9);
        assert!((mapped.y - placement.bounds.y).abs() < 1e-9);
    }
}
