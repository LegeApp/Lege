//! Deterministic input traces for scroll-feel regression tests.
//!
//! Winit events should be normalized into these commands at the application
//! boundary. Replaying a trace compares document positions rather than relying
//! on subjective visual judgement.

use crate::geometry::Vec2d;
use crate::scroll::{ScrollCommand, ScrollModel};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TraceCommand {
    WheelPixels(Vec2d),
    WheelLines(Vec2d),
    TouchpadPan(Vec2d),
    DragPan(Vec2d),
    SetAbsolute(Vec2d),
    FineStep(Vec2d),
    Stop,
}

impl TraceCommand {
    fn into_scroll(self) -> ScrollCommand {
        match self {
            Self::WheelPixels(delta) => ScrollCommand::WheelPixels(delta),
            Self::WheelLines(delta) => ScrollCommand::WheelLines(delta),
            Self::TouchpadPan(delta) => ScrollCommand::TouchpadPan(delta),
            Self::DragPan(delta) => ScrollCommand::DragPan(delta),
            Self::SetAbsolute(position) => ScrollCommand::SetAbsolute(position),
            Self::FineStep(delta) => ScrollCommand::FineStep(delta),
            Self::Stop => ScrollCommand::Stop,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceEvent {
    pub elapsed_micros: u64,
    pub command: TraceCommand,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputTrace {
    pub events: Vec<TraceEvent>,
}

impl InputTrace {
    pub fn push(&mut self, elapsed_micros: u64, command: TraceCommand) {
        self.events.push(TraceEvent {
            elapsed_micros,
            command,
        });
    }

    pub fn replay_positions(&self, scroll: &mut ScrollModel) -> Vec<Vec2d> {
        let mut positions = Vec::with_capacity(self.events.len());
        for event in &self.events {
            // Time is retained for future animation/kinetic replay. Direct
            // commands are deterministic independent of frame cadence.
            let _elapsed_micros = event.elapsed_micros;
            scroll.apply(event.command.into_scroll());
            positions.push(scroll.position);
        }
        positions
    }
}
