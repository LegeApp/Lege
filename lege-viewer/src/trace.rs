//! Deterministic input traces for scroll-feel regression tests.
//!
//! Winit events are normalized into these commands at the application
//! boundary. Replaying a trace compares document positions rather than relying
//! on subjective visual judgement — and because momentum and autoscroll are
//! time-driven, replay steps the clock itself rather than reading it, so a
//! trace recorded on a fast machine reproduces exactly on a slow one.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::geometry::Vec2d;
use crate::scroll::{ScrollCommand, ScrollModel};

/// Simulated frame cadence used between trace events. Matches the event
/// loop's own animation deadline.
const REPLAY_STEP: Duration = Duration::from_millis(8);

/// Frames of animation replayed after the last event, so a trace that ends
/// mid-fling still settles. At 8 ms this is four seconds — longer than any
/// kinetic glide can survive.
const REPLAY_TAIL_FRAMES: u32 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TraceCommand {
    WheelPixels(Vec2d),
    WheelLines(Vec2d),
    TouchpadPan(Vec2d),
    DragPan(Vec2d),
    SetAbsolute(Vec2d),
    FineStep(Vec2d),
    Fling { from_touch: bool },
    BeginAutoScroll,
    SteerAutoScroll(Vec2d),
    EndAutoScroll,
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
            Self::Fling { from_touch } => ScrollCommand::Fling { from_touch },
            Self::BeginAutoScroll => ScrollCommand::BeginAutoScroll,
            Self::SteerAutoScroll(offset) => ScrollCommand::SteerAutoScroll(offset),
            Self::EndAutoScroll => ScrollCommand::EndAutoScroll,
            Self::Stop => ScrollCommand::Stop,
        }
    }

    /// The trace form of a live scroll command, or `None` for commands whose
    /// meaning is resolved outside the scroll model (paging needs line sets,
    /// so it is not reproducible from the model alone).
    pub fn from_scroll(command: ScrollCommand) -> Option<Self> {
        Some(match command {
            ScrollCommand::WheelPixels(delta) => Self::WheelPixels(delta),
            ScrollCommand::WheelLines(delta) => Self::WheelLines(delta),
            ScrollCommand::TouchpadPan(delta) => Self::TouchpadPan(delta),
            ScrollCommand::DragPan(delta) => Self::DragPan(delta),
            ScrollCommand::SetAbsolute(position) => Self::SetAbsolute(position),
            ScrollCommand::FineStep(delta) => Self::FineStep(delta),
            ScrollCommand::Fling { from_touch } => Self::Fling { from_touch },
            ScrollCommand::BeginAutoScroll => Self::BeginAutoScroll,
            ScrollCommand::SteerAutoScroll(offset) => Self::SteerAutoScroll(offset),
            ScrollCommand::EndAutoScroll => Self::EndAutoScroll,
            ScrollCommand::Stop => Self::Stop,
            ScrollCommand::PageStep(_) => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub elapsed_micros: u64,
    pub command: TraceCommand,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

    /// Replay onto `scroll`, returning the position after each event.
    ///
    /// Animation frames between events are stepped too, so a trace containing
    /// a fling reproduces the same glide every time. The returned vector has
    /// one entry per recorded event; the final settled position is
    /// `scroll.position` once this returns.
    pub fn replay_positions(&self, scroll: &mut ScrollModel) -> Vec<Vec2d> {
        let origin = Instant::now();
        scroll.last_update = origin;
        let mut clock = origin;
        let mut positions = Vec::with_capacity(self.events.len());
        for event in &self.events {
            let event_at = origin + Duration::from_micros(event.elapsed_micros);
            // Step the animation up to the event, then apply it. A wheel
            // notch arriving mid-fling must see the position the fling had
            // actually reached.
            while clock + REPLAY_STEP <= event_at {
                clock += REPLAY_STEP;
                scroll.advance(clock);
            }
            if event_at > clock {
                clock = event_at;
                scroll.advance(clock);
            }
            scroll.apply_at(event.command.into_scroll(), clock);
            positions.push(scroll.position);
        }
        for _ in 0..REPLAY_TAIL_FRAMES {
            if !scroll.wants_animation() {
                break;
            }
            clock += REPLAY_STEP;
            scroll.advance(clock);
        }
        positions
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// Records a live session's scroll commands against a monotonic origin.
///
/// Enabled by `LEGE_INPUT_TRACE=<path>`; the trace is written when the
/// recorder is dropped, so an ordinary quit produces a usable file.
#[derive(Debug)]
pub struct TraceRecorder {
    origin: Instant,
    trace: InputTrace,
    path: std::path::PathBuf,
}

impl TraceRecorder {
    /// A recorder for `LEGE_INPUT_TRACE`, or `None` when it is unset.
    #[must_use]
    pub fn from_environment() -> Option<Self> {
        let path = std::env::var_os("LEGE_INPUT_TRACE")?;
        // `LEGE_INPUT_TRACE=1` predates trace files and only asked for
        // pointer logging; it is not a path, so it records nothing.
        let path = std::path::PathBuf::from(path);
        if path.as_os_str() == "1" {
            return None;
        }
        Some(Self {
            origin: Instant::now(),
            trace: InputTrace::default(),
            path,
        })
    }

    pub fn record(&mut self, command: ScrollCommand) {
        let Some(command) = TraceCommand::from_scroll(command) else {
            return;
        };
        let elapsed = self.origin.elapsed().as_micros();
        self.trace
            .push(u64::try_from(elapsed).unwrap_or(u64::MAX), command);
    }

    #[must_use]
    pub fn trace(&self) -> &InputTrace {
        &self.trace
    }

    /// Write the trace out. Called on drop; safe to call early as well.
    pub fn flush(&self) {
        match self.trace.to_json() {
            Ok(json) => {
                if let Err(error) = std::fs::write(&self.path, json) {
                    eprintln!(
                        "Lege Viewer could not write the input trace to {}: {error}",
                        self.path.display()
                    );
                }
            }
            Err(error) => eprintln!("Lege Viewer could not encode the input trace: {error}"),
        }
    }
}

impl Drop for TraceRecorder {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::geometry::SizeF;
    use crate::scroll::MovementTuning;

    fn model() -> ScrollModel {
        let mut model = ScrollModel::new();
        model.set_extents(
            SizeF {
                width: 1_000.0,
                height: 100_000.0,
            },
            SizeF {
                width: 800.0,
                height: 600.0,
            },
        );
        model.tuning = MovementTuning {
            kinetic_enabled: true,
            ..MovementTuning::default()
        };
        model
    }

    fn flick_trace() -> InputTrace {
        let mut trace = InputTrace::default();
        for step in 1..=4_u64 {
            trace.push(step * 16_000, TraceCommand::DragPan(Vec2d { x: 0.0, y: 45.0 }));
        }
        trace.push(72_000, TraceCommand::Fling { from_touch: false });
        trace
    }

    #[test]
    fn a_trace_replays_to_the_same_final_position_every_time() {
        let trace = flick_trace();
        let mut first = model();
        let mut second = model();
        let a = trace.replay_positions(&mut first);
        let b = trace.replay_positions(&mut second);
        assert_eq!(a, b);
        assert_eq!(first.position, second.position);
        assert!(
            first.position.y > 180.0,
            "the fling carried past the drag itself, landing at {:.1}",
            first.position.y
        );
    }

    #[test]
    fn replay_settles_rather_than_leaving_motion_running() {
        let mut scroll = model();
        flick_trace().replay_positions(&mut scroll);
        assert!(!scroll.wants_animation());
    }

    #[test]
    fn a_trace_round_trips_through_json() {
        let trace = flick_trace();
        let json = trace.to_json().expect("trace encodes");
        let decoded = InputTrace::from_json(&json).expect("trace decodes");
        assert_eq!(trace, decoded);
    }

    #[test]
    fn autoscroll_replays_deterministically() {
        let mut trace = InputTrace::default();
        trace.push(0, TraceCommand::BeginAutoScroll);
        trace.push(1_000, TraceCommand::SteerAutoScroll(Vec2d { x: 0.0, y: 80.0 }));
        trace.push(500_000, TraceCommand::EndAutoScroll);
        let mut first = model();
        let mut second = model();
        trace.replay_positions(&mut first);
        trace.replay_positions(&mut second);
        assert_eq!(first.position, second.position);
        assert!(first.position.y > 0.0, "autoscroll travelled");
    }

    #[test]
    fn paging_is_not_recordable_because_the_model_cannot_reproduce_it() {
        assert!(
            TraceCommand::from_scroll(ScrollCommand::PageStep(
                crate::scroll::AxisDirection::Forward
            ))
            .is_none()
        );
    }
}
