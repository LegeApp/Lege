use crate::geometry::RectF;
use crate::text::LineBox;

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
        PagingDirection::Down => fully_visible
            .last()
            .map_or_else(|| geometric_target(direction, viewport), |line| line.bounds.y),
        PagingDirection::Up => fully_visible.first().map_or_else(
            || geometric_target(direction, viewport),
            |line| line.bounds.bottom() - viewport.height,
        ),
    };
    target.clamp(0.0, max_scroll)
}

fn geometric_target(direction: PagingDirection, viewport: RectF) -> f64 {
    let overlap = (viewport.height * 0.08).clamp(24.0, 96.0);
    let delta = (viewport.height - overlap).max(1.0);
    match direction {
        PagingDirection::Down => viewport.y + delta,
        PagingDirection::Up => viewport.y - delta,
    }
}
