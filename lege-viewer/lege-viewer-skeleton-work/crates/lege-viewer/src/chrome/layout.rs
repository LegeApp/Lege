use crate::geometry::{RectF, SizeF};
use crate::theme::ThemeMetrics;

#[derive(Debug, Clone, Copy)]
pub struct AppLayout {
    pub toolbar: RectF,
    pub sidebar: RectF,
    pub canvas: RectF,
    pub vertical_scrollbar: RectF,
    pub horizontal_scrollbar: RectF,
    pub status: RectF,
}

impl AppLayout {
    pub fn calculate(
        window: SizeF,
        scale_factor: f64,
        sidebar_visible: bool,
        metrics: &ThemeMetrics,
    ) -> Self {
        let toolbar_height = metrics.toolbar_height * scale_factor;
        let status_height = metrics.status_height * scale_factor;
        let scrollbar_width = metrics.scrollbar_width * scale_factor;
        let sidebar_width = if sidebar_visible {
            metrics.sidebar_width * scale_factor
        } else {
            0.0
        };
        let canvas_height = (window.height - toolbar_height - status_height).max(0.0);
        let canvas_width = (window.width - sidebar_width - scrollbar_width).max(0.0);
        Self {
            toolbar: RectF {
                x: 0.0,
                y: 0.0,
                width: window.width,
                height: toolbar_height,
            },
            sidebar: RectF {
                x: 0.0,
                y: toolbar_height,
                width: sidebar_width,
                height: canvas_height,
            },
            canvas: RectF {
                x: sidebar_width,
                y: toolbar_height,
                width: canvas_width,
                height: canvas_height,
            },
            vertical_scrollbar: RectF {
                x: sidebar_width + canvas_width,
                y: toolbar_height,
                width: scrollbar_width,
                height: canvas_height,
            },
            horizontal_scrollbar: RectF {
                x: sidebar_width,
                y: toolbar_height + canvas_height - scrollbar_width,
                width: canvas_width,
                height: 0.0,
            },
            status: RectF {
                x: 0.0,
                y: toolbar_height + canvas_height,
                width: window.width,
                height: status_height,
            },
        }
    }
}
