use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use crate::document::PageIndex;
use crate::geometry::{PointF, RectF};

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
        let from = if page == start.page {
            start.utf16_index
        } else {
            0
        };
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
            .filter(|character| {
                selection.range.start < character.char_index.saturating_add(character.utf16_len)
                    && character.char_index < selection.range.end
            })
            .map(|character| character.bounds)
            .collect::<Vec<_>>()
            .into()
    }

    pub fn selected_text(&self, pages: &BTreeMap<PageIndex, Arc<[u16]>>) -> String {
        let (Some(anchor), Some(focus)) = (self.anchor, self.focus) else {
            return String::new();
        };
        let (start, end) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        let mut output = String::new();
        for page_number in start.page.0..=end.page.0 {
            let page = PageIndex(page_number);
            let Some(text) = pages.get(&page) else {
                continue;
            };
            let from = if page == start.page {
                start.utf16_index.min(text.len())
            } else {
                0
            };
            let to = if page == end.page {
                end.utf16_index.min(text.len())
            } else {
                text.len()
            };
            if from < to {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&String::from_utf16_lossy(&text[from..to]));
            }
        }
        output
    }

    pub fn select_word(&mut self, position: TextPosition, substrate: &TextSubstrate) {
        let text = &substrate.utf16;
        if text.is_empty() {
            self.clear();
            return;
        }
        let pivot = position.utf16_index.min(text.len().saturating_sub(1));
        let class = word_class(text[pivot]);
        let mut start = pivot;
        while start > 0 && word_class(text[start - 1]) == class {
            start -= 1;
        }
        let mut end = pivot + 1;
        while end < text.len() && word_class(text[end]) == class {
            end += 1;
        }
        self.anchor = Some(TextPosition {
            page: position.page,
            utf16_index: start,
        });
        self.focus = Some(TextPosition {
            page: position.page,
            utf16_index: end,
        });
    }

    pub fn select_line(&mut self, position: TextPosition, substrate: &TextSubstrate) {
        let Some(line) = substrate.lines.lines.iter().find(|line| {
            line.char_range.0 <= position.utf16_index && position.utf16_index <= line.char_range.1
        }) else {
            self.select_word(position, substrate);
            return;
        };
        self.anchor = Some(TextPosition {
            page: position.page,
            utf16_index: line.char_range.0,
        });
        self.focus = Some(TextPosition {
            page: position.page,
            utf16_index: line.char_range.1,
        });
    }
}

/// Resolve a document-space pointer to the nearest caret position. Exact
/// glyph hits win; gaps use squared distance to the character rectangle.
pub fn hit_test(page: PageIndex, substrate: &TextSubstrate, point: PointF) -> Option<TextPosition> {
    let character = substrate.characters.iter().min_by(|left, right| {
        distance_to_rect(point, left.bounds)
            .partial_cmp(&distance_to_rect(point, right.bounds))
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;
    let after = point.x >= character.bounds.center().x;
    Some(TextPosition {
        page,
        utf16_index: character.char_index + if after { character.utf16_len } else { 0 },
    })
}

fn distance_to_rect(point: PointF, rect: RectF) -> f64 {
    let dx = if point.x < rect.x {
        rect.x - point.x
    } else if point.x > rect.right() {
        point.x - rect.right()
    } else {
        0.0
    };
    let dy = if point.y < rect.y {
        rect.y - point.y
    } else if point.y > rect.bottom() {
        point.y - rect.bottom()
    } else {
        0.0
    };
    dx * dx + dy * dy
}

fn word_class(unit: u16) -> u8 {
    char::from_u32(u32::from(unit)).map_or(2, |character| {
        if character.is_alphanumeric() || character == '_' {
            0
        } else if character.is_whitespace() {
            1
        } else {
            2
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{CharacterGeometry, LineSource, PageLineSet};

    fn substrate() -> TextSubstrate {
        TextSubstrate {
            utf16: "hello world".encode_utf16().collect::<Vec<_>>().into(),
            characters: (0..11)
                .map(|index| CharacterGeometry {
                    unicode: char::from_u32(u32::from(b"hello world"[index])),
                    origin: PointF {
                        x: index as f64 * 10.0,
                        y: 10.0,
                    },
                    bounds: RectF {
                        x: index as f64 * 10.0,
                        y: 0.0,
                        width: 9.0,
                        height: 12.0,
                    },
                    nominal_height: 12.0,
                    font_size: 12.0,
                    bold: false,
                    object_id: 0,
                    char_index: index,
                    utf16_len: 1,
                })
                .collect::<Vec<_>>()
                .into(),
            lines: Arc::new(PageLineSet {
                page: PageIndex(0),
                lines: Arc::from([]),
                source: LineSource::ContentStream,
                median_height: Some(12.0),
            }),
        }
    }

    #[test]
    fn hit_test_uses_nearest_character_and_half_advance() {
        let substrate = substrate();
        let before = hit_test(PageIndex(0), &substrate, PointF { x: 1.0, y: 3.0 });
        let after = hit_test(PageIndex(0), &substrate, PointF { x: 8.0, y: 3.0 });
        assert_eq!(before.map(|p| p.utf16_index), Some(0));
        assert_eq!(after.map(|p| p.utf16_index), Some(1));
    }

    #[test]
    fn word_selection_expands_to_boundaries() {
        let substrate = substrate();
        let mut selection = SelectionModel::default();
        selection.select_word(
            TextPosition {
                page: PageIndex(0),
                utf16_index: 7,
            },
            &substrate,
        );
        assert_eq!(selection.anchor.map(|p| p.utf16_index), Some(6));
        assert_eq!(selection.focus.map(|p| p.utf16_index), Some(11));
    }

    #[test]
    fn surrogate_pair_geometry_advances_over_both_utf16_units() {
        let utf16: Arc<[u16]> = "😀".encode_utf16().collect::<Vec<_>>().into();
        let substrate = TextSubstrate {
            utf16,
            characters: vec![CharacterGeometry {
                unicode: Some('😀'),
                origin: PointF { x: 0.0, y: 10.0 },
                bounds: RectF {
                    x: 0.0,
                    y: 0.0,
                    width: 12.0,
                    height: 12.0,
                },
                nominal_height: 12.0,
                font_size: 12.0,
                bold: false,
                object_id: 0,
                char_index: 0,
                utf16_len: 2,
            }]
            .into(),
            lines: Arc::new(PageLineSet::none(PageIndex(0))),
        };

        let before = hit_test(PageIndex(0), &substrate, PointF { x: 1.0, y: 3.0 });
        let after = hit_test(PageIndex(0), &substrate, PointF { x: 11.0, y: 3.0 });
        assert_eq!(before.map(|p| p.utf16_index), Some(0));
        assert_eq!(after.map(|p| p.utf16_index), Some(2));
    }
}
