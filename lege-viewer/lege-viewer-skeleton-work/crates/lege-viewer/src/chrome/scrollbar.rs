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
            .then(|| self.hover_started.map(|started| started + HOVER_PREVIEW_DELAY))
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
    pub fn calculate(track: RectF, content_height: f64, viewport_height: f64, scroll_y: f64) -> Self {
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

    pub fn scroll_for_thumb_top(self, thumb_top: f64, content_height: f64, viewport_height: f64) -> f64 {
        let available = (self.track.height - self.thumb.height).max(1.0);
        let fraction = ((thumb_top - self.track.y) / available).clamp(0.0, 1.0);
        fraction * (content_height - viewport_height).max(0.0)
    }

    pub fn document_fraction_at(self, pointer_y: f64) -> f64 {
        ((pointer_y - self.track.y) / self.track.height.max(1.0)).clamp(0.0, 1.0)
    }
}
