use std::time::{Duration, Instant};

use crate::geometry::RectF;

const HOVER_PREVIEW_DELAY: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Default)]
pub struct ScrollbarState {
    pub hover_document_fraction: Option<f64>,
    pub dragging: bool,
    hover_started: Option<Instant>,
    preview_visible: bool,
}

impl ScrollbarState {
    pub fn enter_or_move(&mut self, document_fraction: f64, now: Instant) {
        if self.hover_document_fraction.is_none() {
            self.hover_started = Some(now);
            self.preview_visible = false;
        }
        self.hover_document_fraction = Some(document_fraction.clamp(0.0, 1.0));
    }

    pub fn leave(&mut self) {
        self.hover_document_fraction = None;
        self.hover_started = None;
        self.preview_visible = false;
    }

    pub fn begin_drag(&mut self) {
        self.dragging = true;
        self.preview_visible = true;
    }

    pub fn end_drag(&mut self) {
        self.dragging = false;
    }

    pub fn preview_visible(&self) -> bool {
        self.preview_visible || self.dragging
    }

    pub fn preview_deadline(&self) -> Option<Instant> {
        (!self.preview_visible && !self.dragging)
            .then(|| {
                self.hover_started
                    .map(|started| started + HOVER_PREVIEW_DELAY)
            })
            .flatten()
    }

    pub fn reveal_preview_if_due(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.preview_deadline() else {
            return false;
        };
        if now < deadline {
            return false;
        }
        self.preview_visible = true;
        true
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ScrollbarGeometry {
    pub track: RectF,
    pub thumb: RectF,
    pub document_per_track_pixel: f64,
}

impl ScrollbarGeometry {
    pub fn calculate(
        track: RectF,
        content_height: f64,
        viewport_height: f64,
        scroll_y: f64,
    ) -> Self {
        if content_height <= 0.0 || viewport_height >= content_height {
            return Self {
                track,
                thumb: track,
                document_per_track_pixel: 0.0,
            };
        }
        let minimum_thumb = 24.0;
        let raw_thumb = track.height * viewport_height / content_height;
        let thumb_height = raw_thumb.max(minimum_thumb).min(track.height);
        let available = (track.height - thumb_height).max(0.0);
        let max_scroll = (content_height - viewport_height).max(1.0);
        let top = track.y + available * (scroll_y / max_scroll).clamp(0.0, 1.0);
        Self {
            track,
            thumb: RectF {
                x: track.x,
                y: top,
                width: track.width,
                height: thumb_height,
            },
            document_per_track_pixel: max_scroll / available.max(1.0),
        }
    }

    pub fn scroll_for_thumb_top(
        self,
        thumb_top: f64,
        content_height: f64,
        viewport_height: f64,
    ) -> f64 {
        let available = (self.track.height - self.thumb.height).max(1.0);
        let fraction = ((thumb_top - self.track.y) / available).clamp(0.0, 1.0);
        fraction * (content_height - viewport_height).max(0.0)
    }

    /// The scroll position that centres the thumb under the pointer.
    ///
    /// This is what "scroll here" has to mean: the reader points at a place on
    /// the track and the thumb arrives there, rather than at a fraction of the
    /// track that the thumb's own height can never reach.
    pub fn scroll_for_pointer_centered(
        self,
        pointer_y: f64,
        content_height: f64,
        viewport_height: f64,
    ) -> f64 {
        self.scroll_for_thumb_top(
            pointer_y - self.thumb.height * 0.5,
            content_height,
            viewport_height,
        )
    }

    pub fn document_fraction_at(self, pointer_y: f64) -> f64 {
        ((pointer_y - self.track.y) / self.track.height.max(1.0)).clamp(0.0, 1.0)
    }
}

/// Delay before a held track click starts repeating, and the interval after.
///
/// The first step is immediate on press; the pause before the second is what
/// separates a single deliberate page from a held-down run through the
/// document.
const TRACK_REPEAT_DELAY: Duration = Duration::from_millis(400);
const TRACK_REPEAT_INTERVAL: Duration = Duration::from_millis(110);

/// A held click on the scrollbar track, paging while the button is down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackRepeat {
    /// True when the press landed below the thumb, i.e. paging forward.
    pub forward: bool,
    next_at: Instant,
}

impl TrackRepeat {
    /// Begin repeating after the initial delay. The caller has already taken
    /// the first step, which is why this starts paused.
    #[must_use]
    pub fn begin(forward: bool, now: Instant) -> Self {
        Self {
            forward,
            next_at: now + TRACK_REPEAT_DELAY,
        }
    }

    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.next_at
    }

    /// Whether another step is due, advancing the schedule if so.
    pub fn step_due(&mut self, now: Instant) -> bool {
        if now < self.next_at {
            return false;
        }
        // Schedule from `now` rather than from the missed deadline: a stalled
        // frame must not release a burst of catch-up pages.
        self.next_at = now + TRACK_REPEAT_INTERVAL;
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;

    fn geometry() -> ScrollbarGeometry {
        ScrollbarGeometry::calculate(
            RectF {
                x: 0.0,
                y: 100.0,
                width: 14.0,
                height: 600.0,
            },
            10_000.0,
            1_000.0,
            0.0,
        )
    }

    #[test]
    fn scroll_here_puts_the_thumb_under_the_pointer() {
        let geometry = geometry();
        let pointer = 400.0;
        let scroll = geometry.scroll_for_pointer_centered(pointer, 10_000.0, 1_000.0);
        let moved = ScrollbarGeometry::calculate(geometry.track, 10_000.0, 1_000.0, scroll);
        assert!(
            (moved.thumb.center().y - pointer).abs() < 1.0,
            "thumb centre landed at {:.1} for a click at {pointer:.1}",
            moved.thumb.center().y
        );
    }

    #[test]
    fn scroll_here_at_the_ends_of_the_track_reaches_the_ends_of_the_document() {
        let geometry = geometry();
        assert_eq!(
            geometry.scroll_for_pointer_centered(geometry.track.y, 10_000.0, 1_000.0),
            0.0
        );
        assert_eq!(
            geometry.scroll_for_pointer_centered(geometry.track.bottom(), 10_000.0, 1_000.0),
            9_000.0
        );
    }

    #[test]
    fn a_held_track_click_pauses_before_it_repeats() {
        let start = Instant::now();
        let mut repeat = TrackRepeat::begin(true, start);
        assert!(repeat.forward);
        assert!(
            !repeat.step_due(start + Duration::from_millis(399)),
            "a click released quickly pages exactly once"
        );
        assert!(repeat.step_due(start + TRACK_REPEAT_DELAY));
        assert!(!repeat.step_due(start + TRACK_REPEAT_DELAY));
        assert!(repeat.step_due(start + TRACK_REPEAT_DELAY + TRACK_REPEAT_INTERVAL));
    }

    #[test]
    fn a_stalled_frame_does_not_release_a_burst_of_pages() {
        let start = Instant::now();
        let mut repeat = TrackRepeat::begin(false, start);
        assert!(repeat.step_due(start + Duration::from_secs(5)));
        assert!(
            !repeat.step_due(start + Duration::from_secs(5)),
            "the next step is scheduled from now, not from the missed deadline"
        );
        assert_eq!(
            repeat.deadline(),
            start + Duration::from_secs(5) + TRACK_REPEAT_INTERVAL
        );
    }
}
