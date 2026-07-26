use std::time::Instant;

use crate::geometry::{SizeF, Vec2d};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollMode {
    Stationary,
    Direct,
    Animated,
    Kinetic,
    ThumbDrag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisDirection {
    Backward,
    Forward,
}

#[derive(Debug, Clone, Copy)]
pub enum ScrollCommand {
    WheelPixels(Vec2d),
    WheelLines(Vec2d),
    TouchpadPan(Vec2d),
    DragPan(Vec2d),
    SetAbsolute(Vec2d),
    PageStep(AxisDirection),
    FineStep(Vec2d),
    Stop,
}

#[derive(Debug, Clone)]
pub struct ScrollModel {
    pub position: Vec2d,
    pub target: Vec2d,
    pub velocity: Vec2d,
    pub content_extent: SizeF,
    pub viewport_extent: SizeF,
    pub mode: ScrollMode,
    pub generation: u64,
    pub last_update: Instant,
    pub line_wheel_distance: f64,
}

impl ScrollModel {
    pub fn new() -> Self {
        Self {
            position: Vec2d::ZERO,
            target: Vec2d::ZERO,
            velocity: Vec2d::ZERO,
            content_extent: SizeF::default(),
            viewport_extent: SizeF::default(),
            mode: ScrollMode::Stationary,
            generation: 0,
            last_update: Instant::now(),
            line_wheel_distance: 42.0,
        }
    }

    pub fn set_extents(&mut self, content: SizeF, viewport: SizeF) {
        self.content_extent = content;
        self.viewport_extent = viewport;
        self.position = self.clamp(self.position);
        self.target = self.clamp(self.target);
    }

    pub fn apply(&mut self, command: ScrollCommand) {
        let previous = self.position;
        match command {
            ScrollCommand::WheelPixels(delta)
            | ScrollCommand::TouchpadPan(delta)
            | ScrollCommand::DragPan(delta) => {
                self.position = self.clamp(self.position + delta);
                self.target = self.position;
                self.mode = ScrollMode::Direct;
            }
            ScrollCommand::WheelLines(delta) => {
                self.position = self.clamp(
                    self.position
                        + Vec2d {
                            x: delta.x * self.line_wheel_distance,
                            y: delta.y * self.line_wheel_distance,
                        },
                );
                self.target = self.position;
                self.mode = ScrollMode::Direct;
            }
            ScrollCommand::SetAbsolute(position) => {
                self.position = self.clamp(position);
                self.target = self.position;
                self.mode = ScrollMode::ThumbDrag;
            }
            ScrollCommand::FineStep(delta) => {
                self.position = self.clamp(self.position + delta);
                self.target = self.position;
                self.mode = ScrollMode::Direct;
            }
            ScrollCommand::Stop => {
                self.velocity = Vec2d::ZERO;
                self.target = self.position;
                self.mode = ScrollMode::Stationary;
            }
            ScrollCommand::PageStep(_) => {
                // Reader semantics need line sets and are resolved by paging.rs.
            }
        }
        let now = Instant::now();
        let elapsed = now
            .duration_since(self.last_update)
            .as_secs_f64()
            .max(1.0 / 1000.0);
        self.velocity = Vec2d {
            x: (self.position.x - previous.x) / elapsed,
            y: (self.position.y - previous.y) / elapsed,
        };
        self.last_update = now;
        if self.position != previous {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    pub fn settle(&mut self) {
        self.mode = ScrollMode::Stationary;
        self.velocity = Vec2d::ZERO;
        self.target = self.position;
    }

    pub fn max_position(&self) -> Vec2d {
        Vec2d {
            x: (self.content_extent.width - self.viewport_extent.width).max(0.0),
            y: (self.content_extent.height - self.viewport_extent.height).max(0.0),
        }
    }

    fn clamp(&self, position: Vec2d) -> Vec2d {
        let max = self.max_position();
        Vec2d {
            x: position.x.clamp(0.0, max.x),
            y: position.y.clamp(0.0, max.y),
        }
    }
}

impl Default for ScrollModel {
    fn default() -> Self {
        Self::new()
    }
}
