use crate::document::PageIndex;
use crate::document::layout::PageLayoutIndex;
use crate::geometry::RectF;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReadingAnchor {
    pub page: PageIndex,
    pub page_y: f64,
    pub viewport_fraction: f64,
}

impl ReadingAnchor {
    pub fn capture(layout: &PageLayoutIndex, viewport: RectF) -> Option<Self> {
        let placement = layout.page_at_y(viewport.y)?;
        let page_y = placement.view_box.y
            + (viewport.y - placement.bounds.y).clamp(0.0, placement.bounds.height);
        Some(Self {
            page: placement.page,
            page_y,
            viewport_fraction: 0.0,
        })
    }

    pub fn restore(self, layout: &PageLayoutIndex, viewport_height: f64) -> Option<f64> {
        let placement = layout.placement(self.page)?;
        Some(
            placement.bounds.y
                + (self.page_y - placement.view_box.y).clamp(0.0, placement.bounds.height)
                - self.viewport_fraction * viewport_height,
        )
    }
}
