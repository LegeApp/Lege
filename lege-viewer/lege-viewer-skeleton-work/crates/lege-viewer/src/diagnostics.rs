use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct FrameMetrics {
    pub input_received: Option<Instant>,
    pub redraw_requested: Option<Instant>,
    pub frame_started: Option<Instant>,
    pub compose_finished: Option<Instant>,
    pub present_finished: Option<Instant>,
    pub compose_time: Duration,
    pub present_time: Duration,
    pub damaged_pixels: u64,
    pub copied_pixels: u64,
    pub page_blits: u32,
    pub allocations: u32,
    pub compile_pending: usize,
    pub raster_pending: usize,
    pub in_flight: usize,
}

impl Default for FrameMetrics {
    fn default() -> Self {
        Self {
            input_received: None,
            redraw_requested: None,
            frame_started: None,
            compose_finished: None,
            present_finished: None,
            compose_time: Duration::ZERO,
            present_time: Duration::ZERO,
            damaged_pixels: 0,
            copied_pixels: 0,
            page_blits: 0,
            allocations: 0,
            compile_pending: 0,
            raster_pending: 0,
            in_flight: 0,
        }
    }
}

impl FrameMetrics {
    pub fn begin_frame(&mut self) {
        self.frame_started = Some(Instant::now());
        self.damaged_pixels = 0;
        self.copied_pixels = 0;
        self.page_blits = 0;
        self.allocations = 0;
    }

    pub fn finish_compose(&mut self) {
        let now = Instant::now();
        self.compose_finished = Some(now);
        self.compose_time = self.frame_started.map_or(Duration::ZERO, |start| now - start);
    }

    pub fn finish_present(&mut self) {
        let now = Instant::now();
        self.present_finished = Some(now);
        self.present_time = self
            .compose_finished
            .map_or(Duration::ZERO, |compose| now - compose);
    }
}
