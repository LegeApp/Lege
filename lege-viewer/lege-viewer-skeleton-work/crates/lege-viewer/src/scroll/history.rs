use crate::document::PageIndex;
use crate::geometry::RectF;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DocumentLocation {
    pub page: PageIndex,
    pub target_region: Option<RectF>,
}

/// Browser-style history for semantic jumps only. Ordinary wheel/pan motion
/// must not call `push_jump`, which preserves the reader's useful history.
#[derive(Debug, Clone)]
pub struct NavigationHistory {
    entries: Vec<DocumentLocation>,
    cursor: usize,
    capacity: usize,
}

impl Default for NavigationHistory {
    fn default() -> Self {
        Self::new(256)
    }
}

impl NavigationHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            cursor: 0,
            capacity: capacity.max(2),
        }
    }

    pub fn push_jump(&mut self, location: DocumentLocation) {
        if self.entries.get(self.cursor).copied() == Some(location) {
            return;
        }
        if !self.entries.is_empty() {
            self.entries.truncate(self.cursor + 1);
        }
        self.entries.push(location);
        if self.entries.len() > self.capacity {
            self.entries.remove(0);
        }
        self.cursor = self.entries.len().saturating_sub(1);
    }

    pub fn back(&mut self) -> Option<DocumentLocation> {
        if self.cursor == 0 || self.entries.is_empty() {
            return None;
        }
        self.cursor -= 1;
        self.entries.get(self.cursor).copied()
    }

    pub fn forward(&mut self) -> Option<DocumentLocation> {
        if self.cursor + 1 >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        self.entries.get(self.cursor).copied()
    }
}
