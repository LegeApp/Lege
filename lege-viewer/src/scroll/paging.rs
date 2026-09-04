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

/// The scroll target that puts the nearest page boundary at the top of the
/// viewport, or `None` when the document has no pages or is already there.
///
/// This is an explicit command, not a layout mode: continuous scrolling is
/// untouched, and nothing snaps on its own. `boundaries` are the document-space
/// tops of the pages, in ascending order.
pub fn nearest_page_boundary(
    boundaries: impl IntoIterator<Item = f64>,
    viewport_y: f64,
    max_scroll: f64,
) -> Option<f64> {
    let mut best: Option<f64> = None;
    for boundary in boundaries {
        if !boundary.is_finite() {
            continue;
        }
        let candidate = boundary.clamp(0.0, max_scroll.max(0.0));
        let better = best.is_none_or(|current| {
            (candidate - viewport_y).abs() < (current - viewport_y).abs()
        });
        if better {
            best = Some(candidate);
        }
    }
    // Landing where we already are is not a movement; reporting it as one
    // would restart the settle timer and re-plan for nothing.
    best.filter(|target| (target - viewport_y).abs() >= BOUNDARY_SNAP_EPSILON)
}

/// Document points below which a snap target counts as "already there".
const BOUNDARY_SNAP_EPSILON: f64 = 0.5;

fn geometric_target(direction: PagingDirection, viewport: RectF) -> f64 {
    let overlap = (viewport.height * 0.08).clamp(24.0, 96.0);
    let delta = (viewport.height - overlap).max(1.0);
    match direction {
        PagingDirection::Down => viewport.y + delta,
        PagingDirection::Up => viewport.y - delta,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;

    /// Three 800pt pages separated by an 8pt gap, starting 12pt down.
    const BOUNDARIES: [f64; 3] = [12.0, 820.0, 1_628.0];
    const MAX_SCROLL: f64 = 2_000.0;

    #[test]
    fn snapping_moves_to_the_closer_of_the_two_neighbouring_boundaries() {
        assert_eq!(
            nearest_page_boundary(BOUNDARIES, 900.0, MAX_SCROLL),
            Some(820.0),
            "just past a page top snaps back to it"
        );
        assert_eq!(
            nearest_page_boundary(BOUNDARIES, 1_500.0, MAX_SCROLL),
            Some(1_628.0),
            "most of the way down a page snaps forward to the next"
        );
    }

    #[test]
    fn snapping_at_a_boundary_reports_no_movement() {
        assert_eq!(nearest_page_boundary(BOUNDARIES, 820.0, MAX_SCROLL), None);
        assert_eq!(
            nearest_page_boundary(BOUNDARIES, 820.2, MAX_SCROLL),
            None,
            "sub-point drift is already snapped"
        );
    }

    #[test]
    fn a_boundary_past_the_end_of_the_scroll_range_is_clamped() {
        // A short document cannot scroll its last page to the top.
        assert_eq!(
            nearest_page_boundary(BOUNDARIES, 700.0, 750.0),
            Some(750.0),
            "the snap stops at the end of the document rather than overscrolling"
        );
    }

    #[test]
    fn an_empty_document_has_nothing_to_snap_to() {
        assert_eq!(nearest_page_boundary([], 100.0, MAX_SCROLL), None);
    }
}
