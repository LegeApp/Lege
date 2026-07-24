use std::ops::Range;
use std::sync::Arc;

use crate::document::PageIndex;
use crate::geometry::RectF;

use super::TextSubstrate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPosition {
    pub page: PageIndex,
    pub utf16_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSelection {
    pub page: PageIndex,
    pub range: Range<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionModel {
    pub anchor: Option<TextPosition>,
    pub focus: Option<TextPosition>,
}

impl SelectionModel {
    pub fn clear(&mut self) {
        self.anchor = None;
        self.focus = None;
    }

    pub fn begin(&mut self, position: TextPosition) {
        self.anchor = Some(position);
        self.focus = Some(position);
    }

    pub fn extend(&mut self, position: TextPosition) {
        if self.anchor.is_some() {
            self.focus = Some(position);
        }
    }

    pub fn page_range(&self, page: PageIndex, page_text_len: usize) -> Option<PageSelection> {
        let (anchor, focus) = (self.anchor?, self.focus?);
        let (start, end) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        if page < start.page || page > end.page {
            return None;
        }
        let from = if page == start.page { start.utf16_index } else { 0 };
        let to = if page == end.page {
            end.utf16_index
        } else {
            page_text_len
        };
        Some(PageSelection {
            page,
            range: from.min(page_text_len)..to.min(page_text_len),
        })
    }

    pub fn overlays(&self, page: PageIndex, substrate: &TextSubstrate) -> Arc<[RectF]> {
        let Some(selection) = self.page_range(page, substrate.utf16.len()) else {
            return Arc::from([]);
        };
        substrate
            .characters
            .iter()
            .filter(|character| selection.range.contains(&character.char_index))
            .map(|character| character.bounds)
            .collect::<Vec<_>>()
            .into()
    }
}

impl PartialOrd for TextPosition {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TextPosition {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.page
            .cmp(&other.page)
            .then_with(|| self.utf16_index.cmp(&other.utf16_index))
    }
}
