use crate::document::layout::PageLayoutIndex;
use crate::document::PageIndex;
use crate::geometry::RectF;

#[derive(Debug, Clone, Copy, Default)]
pub struct StatusState {
    pub current_page: Option<PageIndex>,
    pub page_count: u32,
    pub visible_fraction: f64,
    pub zoom: f64,
}

impl StatusState {
    pub fn update(&mut self, layout: &PageLayoutIndex, viewport: RectF, zoom: f64) {
        self.zoom = zoom;
        self.page_count = layout.placements().len() as u32;
        let midpoint = viewport.y + viewport.height * 0.5;
        self.current_page = layout.page_at_y(midpoint).map(|placement| placement.page);
        self.visible_fraction = self.current_page.map_or(0.0, |page| {
            let Some(placement) = layout.placement(page) else {
                return 0.0;
            };
            placement.bounds.intersection(viewport).map_or(0.0, |visible| {
                (visible.height / placement.bounds.height.max(1.0)).clamp(0.0, 1.0)
            })
        });
    }
}
