use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMode {
    Idle,
    Interactive,
    Animating,
}

#[derive(Debug)]
pub struct FrameScheduler {
    pub mode: FrameMode,
    redraw_pending: bool,
    last_interaction: Instant,
}

impl FrameScheduler {
    pub fn new() -> Self {
        Self {
            mode: FrameMode::Idle,
            redraw_pending: false,
            last_interaction: Instant::now(),
        }
    }

    pub fn request_redraw(&mut self) -> bool {
        if self.redraw_pending {
            false
        } else {
            self.redraw_pending = true;
            true
        }
    }

    pub fn redraw_started(&mut self) {
        self.redraw_pending = false;
    }

    pub fn interactive(&mut self) {
        self.mode = FrameMode::Interactive;
        self.last_interaction = Instant::now();
    }

    pub fn settle_if_quiet(&mut self) {
        if self.mode == FrameMode::Interactive
            && self.last_interaction.elapsed() >= Duration::from_millis(80)
        {
            self.mode = FrameMode::Idle;
        }
    }
}

impl Default for FrameScheduler {
    fn default() -> Self {
        Self::new()
    }
}
