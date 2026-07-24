use crate::geometry::RectI;

const MAX_RECTS: usize = 24;

#[derive(Debug, Default, Clone)]
pub struct DamageRegion {
    rects: Vec<RectI>,
    full: bool,
    window: RectI,
}

impl DamageRegion {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            rects: Vec::with_capacity(MAX_RECTS),
            full: true,
            window: RectI {
                x: 0,
                y: 0,
                width,
                height,
            },
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.window.width = width;
        self.window.height = height;
        self.mark_full();
    }

    pub fn mark_full(&mut self) {
        self.full = true;
        self.rects.clear();
    }

    pub fn add(&mut self, rect: RectI) {
        if self.full || rect.is_empty() {
            return;
        }
        let Some(mut clipped) = rect.intersection(self.window) else {
            return;
        };
        let mut index = 0;
        while index < self.rects.len() {
            let existing = self.rects[index];
            let expanded = RectI {
                x: existing.x.saturating_sub(1),
                y: existing.y.saturating_sub(1),
                width: existing.width.saturating_add(2),
                height: existing.height.saturating_add(2),
            };
            if expanded.intersection(clipped).is_some() {
                clipped = clipped.union(existing);
                self.rects.swap_remove(index);
            } else {
                index += 1;
            }
        }
        self.rects.push(clipped);
        if self.rects.len() > MAX_RECTS {
            self.mark_full();
        }
    }

    pub fn clear(&mut self) {
        self.full = false;
        self.rects.clear();
    }

    pub fn rects(&self) -> &[RectI] {
        if self.full {
            std::slice::from_ref(&self.window)
        } else {
            &self.rects
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.full && self.rects.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.full
    }
}
