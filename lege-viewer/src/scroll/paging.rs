use crate::PageIndex;
use crate::geometry::RectF;
use crate::text::LineBox;

pub const NOTIONAL_ROWS_PER_PAGE: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagingDirection {
    Up,
    Down,
}

pub fn paging_target(
    direction: PagingDirection,
    viewport: RectF,
    lines: impl IntoIterator<Item = LineBox>,
    content_height: f64,
) -> f64 {
    let tolerance = 1.0;
    let mut fully_visible: Vec<LineBox> = lines
        .into_iter()
        .filter(|line| {
            line.bounds.y + tolerance >= viewport.y
                && line.bounds.bottom() - tolerance <= viewport.bottom()
        })
        .collect();
    fully_visible.sort_by(|left, right| {
        left.bounds
            .y
            .partial_cmp(&right.bounds.y)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let max_scroll = (content_height - viewport.height).max(0.0);
    let target = match direction {
        PagingDirection::Down => fully_visible.last().map_or_else(
            || geometric_target(direction, viewport),
            |line| line.bounds.y,
        ),
        PagingDirection::Up => fully_visible.first().map_or_else(
            || geometric_target(direction, viewport),
            |line| line.bounds.bottom() - viewport.height,
        ),
    };
    target.clamp(0.0, max_scroll)
}

/// Supply stable paging anchors for pages that have no usable text lines.
/// Ten equal-height rows preserve the same "last visible line becomes first"
/// contract as text paging without pretending the page contains selectable
/// text.
pub fn notional_page_lines(page: PageIndex, page_bounds: RectF) -> Vec<LineBox> {
    let row_height = page_bounds.height / f64::from(NOTIONAL_ROWS_PER_PAGE);
    (0..NOTIONAL_ROWS_PER_PAGE)
        .map(|row| {
            let y = page_bounds.y + f64::from(row) * row_height;
            LineBox {
                page,
                bounds: RectF {
                    x: page_bounds.x,
                    y,
                    width: page_bounds.width,
                    height: row_height,
                },
                baseline_y: y + row_height,
                char_range: (0, 0),
            }
        })
        .collect()
}

fn geometric_target(direction: PagingDirection, viewport: RectF) -> f64 {
    let overlap = (viewport.height * 0.08).clamp(24.0, 96.0);
    let delta = (viewport.height - overlap).max(1.0);
    match direction {
        PagingDirection::Down => viewport.y + delta,
        PagingDirection::Up => viewport.y - delta,
    }
}
