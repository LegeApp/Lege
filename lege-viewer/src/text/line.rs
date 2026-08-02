use std::cmp::Ordering;
use std::sync::Arc;

use crate::document::PageIndex;
use crate::geometry::{Affine, PointF, RectF};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineSource {
    ContentStream,
    InkProfile,
    Embedded,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CharacterGeometry {
    pub unicode: Option<char>,
    pub origin: PointF,
    pub bounds: RectF,
    pub nominal_height: f64,
    pub font_size: f64,
    pub bold: bool,
    pub object_id: u32,
    /// Start of this scalar value in the page's UTF-16 substrate.
    pub char_index: usize,
    /// Number of UTF-16 code units represented by this geometry (one for BMP
    /// values, two for a valid surrogate pair).
    pub utf16_len: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LineBox {
    pub page: PageIndex,
    pub bounds: RectF,
    pub baseline_y: f64,
    pub char_range: (usize, usize),
}

#[derive(Debug, Clone)]
pub struct PageLineSet {
    pub page: PageIndex,
    pub lines: Arc<[LineBox]>,
    pub source: LineSource,
    pub median_height: Option<f64>,
}

impl PageLineSet {
    pub fn none(page: PageIndex) -> Self {
        Self {
            page,
            lines: Arc::from([]),
            source: LineSource::None,
            median_height: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextSubstrate {
    pub utf16: Arc<[u16]>,
    pub characters: Arc<[CharacterGeometry]>,
    pub lines: Arc<PageLineSet>,
}

/// Convert per-character geometry to document space after line clustering.
/// Keeping the substrate in one coordinate space makes selection, search,
/// accessibility, and overlays consume the same rectangles as paging.
pub fn transform_characters_to_document(characters: &mut [CharacterGeometry], page_to_doc: Affine) {
    for character in characters {
        character.origin = page_to_doc.apply(character.origin);
        character.bounds = page_to_doc.apply_rect(character.bounds);
    }
}

#[derive(Debug, Clone)]
struct WorkingLine {
    baseline: f64,
    height: f64,
    chars: Vec<CharacterGeometry>,
}

/// Initial content-stream line clustering. It is intentionally isolated from
/// paging: poor clustering degrades only the quality source, while geometric
/// paging remains instant and correct.
pub fn cluster_lines(
    page: PageIndex,
    characters: &[CharacterGeometry],
    page_to_doc: Affine,
) -> PageLineSet {
    let mut visible: Vec<CharacterGeometry> = characters
        .iter()
        .filter(|character| character.unicode.is_some_and(|ch| !ch.is_whitespace()))
        .cloned()
        .collect();
    if visible.is_empty() {
        return PageLineSet::none(page);
    }
    visible.sort_by(|left, right| {
        right
            .origin
            .y
            .partial_cmp(&left.origin.y)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                left.origin
                    .x
                    .partial_cmp(&right.origin.x)
                    .unwrap_or(Ordering::Equal)
            })
    });

    let mut working: Vec<WorkingLine> = Vec::new();
    for character in visible {
        let tolerance = (character.nominal_height * 0.20).max(0.75);
        let best = working
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                let distance = (line.baseline - character.origin.y).abs();
                (distance <= tolerance.max(line.height * 0.20)).then_some((index, distance))
            })
            .min_by(|left, right| left.1.partial_cmp(&right.1).unwrap_or(Ordering::Equal))
            .map(|(index, _)| index);

        if let Some(index) = best {
            let line = &mut working[index];
            let count = line.chars.len() as f64;
            line.baseline = (line.baseline * count + character.origin.y) / (count + 1.0);
            line.height = line.height.max(character.nominal_height);
            line.chars.push(character);
        } else {
            working.push(WorkingLine {
                baseline: character.origin.y,
                height: character.nominal_height,
                chars: vec![character],
            });
        }
    }

    let mut lines = Vec::new();
    for mut line in working {
        line.chars.sort_by(|left, right| {
            left.bounds
                .x
                .partial_cmp(&right.bounds.x)
                .unwrap_or(Ordering::Equal)
        });
        let widths: Vec<f64> = line
            .chars
            .iter()
            .map(|character| character.bounds.width.max(0.1))
            .collect();
        let median_width = median(widths).unwrap_or(4.0);

        let mut segment_start = 0;
        for index in 1..=line.chars.len() {
            let split = index == line.chars.len()
                || line.chars[index].bounds.x - line.chars[index - 1].bounds.right()
                    > median_width * 2.0;
            if !split {
                continue;
            }
            let segment = &line.chars[segment_start..index];
            if let Some(line_box) = build_line_box(page, segment, line.baseline, page_to_doc) {
                lines.push(line_box);
            }
            segment_start = index;
        }
    }

    lines.sort_by(|left, right| {
        left.bounds
            .y
            .partial_cmp(&right.bounds.y)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                left.bounds
                    .x
                    .partial_cmp(&right.bounds.x)
                    .unwrap_or(Ordering::Equal)
            })
    });
    let median_height = median(lines.iter().map(|line| line.bounds.height).collect());
    PageLineSet {
        page,
        lines: lines.into(),
        source: LineSource::ContentStream,
        median_height,
    }
}

fn build_line_box(
    page: PageIndex,
    segment: &[CharacterGeometry],
    baseline: f64,
    page_to_doc: Affine,
) -> Option<LineBox> {
    let first = segment.first()?;
    let mut bounds = first.bounds;
    let mut min_index = first.char_index;
    let mut max_end = first.char_index.saturating_add(first.utf16_len);
    for character in &segment[1..] {
        let x0 = bounds.x.min(character.bounds.x);
        let y0 = bounds.y.min(character.bounds.y);
        let x1 = bounds.right().max(character.bounds.right());
        let y1 = bounds.bottom().max(character.bounds.bottom());
        bounds = RectF {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        };
        min_index = min_index.min(character.char_index);
        max_end = max_end.max(character.char_index.saturating_add(character.utf16_len));
    }
    let doc_bounds = page_to_doc.apply_rect(bounds);
    let doc_baseline = page_to_doc
        .apply(PointF {
            x: 0.0,
            y: baseline,
        })
        .y;
    Some(LineBox {
        page,
        bounds: doc_bounds,
        baseline_y: doc_baseline,
        char_range: (min_index, max_end),
    })
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    Some(values[values.len() / 2])
}
