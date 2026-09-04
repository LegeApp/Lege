use std::time::Instant;

use crate::geometry::{SizeF, Vec2d};

/// Kinetic motion loses this fraction of its speed per second. Chosen so a
/// hard flick travels roughly a screen and a half and stops in under a
/// second, which reads as momentum rather than as an animation the reader has
/// to wait out.
const KINETIC_DECAY_PER_SECOND: f64 = 0.0025;

/// Below this speed (document points per second) kinetic motion is over.
/// Anything slower is a pixel every few frames, which looks like a stuck
/// viewport rather than a glide.
const KINETIC_STOP_SPEED: f64 = 24.0;

/// A flick slower than this does not become kinetic at all. Releasing a drag
/// while nearly still should leave the document where the reader put it.
const KINETIC_LAUNCH_SPEED: f64 = 120.0;

/// Kinetic motion never launches faster than this, however hard the flick.
const KINETIC_MAX_SPEED: f64 = 6_000.0;

/// Velocity smoothing time constant. Drag samples arrive irregularly, and a
/// single stuttered frame at release should not decide the throw.
const VELOCITY_SMOOTHING_SECONDS: f64 = 0.06;

/// Autoscroll speed at the edge of its dead zone, and how the pointer offset
/// maps onto speed.
const AUTOSCROLL_DEAD_ZONE: f64 = 12.0;
const AUTOSCROLL_GAIN: f64 = 6.0;
const AUTOSCROLL_MAX_SPEED: f64 = 3_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollMode {
    Stationary,
    Direct,
    Animated,
    Kinetic,
    /// Middle-click autoscroll: a sustained velocity the pointer steers.
    AutoScroll,
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
    /// Release a drag or a touch into kinetic motion at the model's current
    /// smoothed velocity.
    ///
    /// `from_touch` overrides [`MovementTuning::kinetic_enabled`]: momentum on
    /// a mouse drag is a preference, but a touch flick without momentum is
    /// simply broken, so touch always glides.
    Fling { from_touch: bool },
    /// Enter autoscroll. The velocity is set separately by `SteerAutoScroll`
    /// as the pointer moves, so entering is a distinct, zero-speed event.
    BeginAutoScroll,
    /// Point the autoscroll: `offset` is the pointer's displacement from the
    /// anchor, in physical pixels.
    SteerAutoScroll(Vec2d),
    EndAutoScroll,
    Stop,
}

/// Reader-tunable movement constants.
///
/// Every movement source funnels through the one `f64` model, so these are
/// the only knobs: how far a wheel notch travels, and a single scale over
/// every wheel and touchpad delta.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MovementTuning {
    /// Document points one wheel *line* notch travels.
    pub line_wheel_distance: f64,
    /// Multiplier over every wheel and touchpad delta, pixel or line.
    pub wheel_scale: f64,
    /// Whether a released drag or touch continues under momentum.
    pub kinetic_enabled: bool,
}

impl MovementTuning {
    pub const MIN_LINE_DISTANCE: f64 = 8.0;
    pub const MAX_LINE_DISTANCE: f64 = 400.0;
    pub const MIN_WHEEL_SCALE: f64 = 0.1;
    pub const MAX_WHEEL_SCALE: f64 = 10.0;

    /// Clamp to the supported range, replacing non-finite values with the
    /// default. A settings file is user-editable, so it can say anything.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let default = Self::default();
        let clamp = |value: f64, lo: f64, hi: f64, fallback: f64| {
            if value.is_finite() {
                value.clamp(lo, hi)
            } else {
                fallback
            }
        };
        Self {
            line_wheel_distance: clamp(
                self.line_wheel_distance,
                Self::MIN_LINE_DISTANCE,
                Self::MAX_LINE_DISTANCE,
                default.line_wheel_distance,
            ),
            wheel_scale: clamp(
                self.wheel_scale,
                Self::MIN_WHEEL_SCALE,
                Self::MAX_WHEEL_SCALE,
                default.wheel_scale,
            ),
            kinetic_enabled: self.kinetic_enabled,
        }
    }
}

impl Default for MovementTuning {
    fn default() -> Self {
        Self {
            line_wheel_distance: 42.0,
            wheel_scale: 1.0,
            // Off by default: a mouse wheel and a scrollbar thumb are already
            // direct, and momentum on a pointer device reads as lag. Touch
            // and touchpad flings turn it on.
            kinetic_enabled: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScrollModel {
    pub position: Vec2d,
    pub target: Vec2d,
    /// Instantaneous velocity of the last applied command, in document points
    /// per second.
    pub velocity: Vec2d,
    /// Time-smoothed velocity. This is what a fling launches with.
    pub smoothed_velocity: Vec2d,
    pub content_extent: SizeF,
    pub viewport_extent: SizeF,
    pub mode: ScrollMode,
    pub generation: u64,
    pub last_update: Instant,
    pub tuning: MovementTuning,
    /// Live kinetic velocity while `mode` is `Kinetic`.
    kinetic_velocity: Vec2d,
    /// Live autoscroll velocity while `mode` is `AutoScroll`.
    autoscroll_velocity: Vec2d,
}

impl ScrollModel {
    pub fn new() -> Self {
        Self {
            position: Vec2d::ZERO,
            target: Vec2d::ZERO,
            velocity: Vec2d::ZERO,
            smoothed_velocity: Vec2d::ZERO,
            content_extent: SizeF::default(),
            viewport_extent: SizeF::default(),
            mode: ScrollMode::Stationary,
            generation: 0,
            last_update: Instant::now(),
            tuning: MovementTuning::default(),
            kinetic_velocity: Vec2d::ZERO,
            autoscroll_velocity: Vec2d::ZERO,
        }
    }

    /// Kept for callers written before movement tuning existed.
    pub fn line_wheel_distance(&self) -> f64 {
        self.tuning.line_wheel_distance
    }

    pub fn set_extents(&mut self, content: SizeF, viewport: SizeF) {
        self.content_extent = content;
        self.viewport_extent = viewport;
        self.position = self.clamp(self.position);
        self.target = self.clamp(self.target);
    }

    pub fn apply(&mut self, command: ScrollCommand) {
        self.apply_at(command, Instant::now());
    }

    /// [`ScrollModel::apply`] with the clock supplied, so traces replay
    /// identically and tests do not depend on wall time.
    pub fn apply_at(&mut self, command: ScrollCommand, now: Instant) {
        let previous = self.position;
        let scale = self.tuning.wheel_scale;
        match command {
            ScrollCommand::WheelPixels(delta) | ScrollCommand::TouchpadPan(delta) => {
                self.position = self.clamp(self.position + delta * scale);
                self.target = self.position;
                self.mode = ScrollMode::Direct;
                self.stop_animation();
            }
            ScrollCommand::DragPan(delta) => {
                // A drag is 1:1 with the hand: wheel scaling does not apply.
                self.position = self.clamp(self.position + delta);
                self.target = self.position;
                self.mode = ScrollMode::Direct;
                self.kinetic_velocity = Vec2d::ZERO;
            }
            ScrollCommand::WheelLines(delta) => {
                let distance = self.tuning.line_wheel_distance * scale;
                self.position = self.clamp(
                    self.position
                        + Vec2d {
                            x: delta.x * distance,
                            y: delta.y * distance,
                        },
                );
                self.target = self.position;
                self.mode = ScrollMode::Direct;
                self.stop_animation();
            }
            ScrollCommand::SetAbsolute(position) => {
                self.position = self.clamp(position);
                self.target = self.position;
                self.mode = ScrollMode::ThumbDrag;
                self.stop_animation();
            }
            ScrollCommand::FineStep(delta) => {
                self.position = self.clamp(self.position + delta);
                self.target = self.position;
                self.mode = ScrollMode::Direct;
                self.stop_animation();
            }
            ScrollCommand::Fling { from_touch } => {
                let launch = self.smoothed_velocity;
                let speed = launch.length();
                if (from_touch || self.tuning.kinetic_enabled) && speed >= KINETIC_LAUNCH_SPEED {
                    let capped = if speed > KINETIC_MAX_SPEED {
                        launch * (KINETIC_MAX_SPEED / speed)
                    } else {
                        launch
                    };
                    self.kinetic_velocity = capped;
                    self.autoscroll_velocity = Vec2d::ZERO;
                    self.mode = ScrollMode::Kinetic;
                } else {
                    self.stop_animation();
                    self.mode = ScrollMode::Stationary;
                }
                self.last_update = now;
                return;
            }
            ScrollCommand::BeginAutoScroll => {
                self.kinetic_velocity = Vec2d::ZERO;
                self.autoscroll_velocity = Vec2d::ZERO;
                self.mode = ScrollMode::AutoScroll;
                self.last_update = now;
                return;
            }
            ScrollCommand::SteerAutoScroll(offset) => {
                self.autoscroll_velocity = autoscroll_velocity_for(offset);
                self.mode = ScrollMode::AutoScroll;
                self.last_update = now;
                return;
            }
            ScrollCommand::EndAutoScroll => {
                self.autoscroll_velocity = Vec2d::ZERO;
                self.mode = ScrollMode::Stationary;
                self.velocity = Vec2d::ZERO;
                self.smoothed_velocity = Vec2d::ZERO;
                self.last_update = now;
                return;
            }
            ScrollCommand::Stop => {
                self.stop_animation();
                self.velocity = Vec2d::ZERO;
                self.smoothed_velocity = Vec2d::ZERO;
                self.target = self.position;
                self.mode = ScrollMode::Stationary;
                self.last_update = now;
                return;
            }
            ScrollCommand::PageStep(_) => {
                // Reader semantics need line sets and are resolved by paging.rs.
            }
        }
        let elapsed = now
            .saturating_duration_since(self.last_update)
            .as_secs_f64()
            .max(1.0 / 1000.0);
        self.velocity = (self.position - previous) / elapsed;
        self.smoothed_velocity = smooth(self.smoothed_velocity, self.velocity, elapsed);
        self.last_update = now;
        if self.position != previous {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Whether the model is mid-motion and needs to be advanced on a timer.
    pub fn wants_animation(&self) -> bool {
        match self.mode {
            ScrollMode::Kinetic => self.kinetic_velocity != Vec2d::ZERO,
            ScrollMode::AutoScroll => self.autoscroll_velocity != Vec2d::ZERO,
            _ => false,
        }
    }

    /// Integrate kinetic or autoscroll motion up to `now`. Returns true when
    /// the position changed and the caller should redraw.
    pub fn advance(&mut self, now: Instant) -> bool {
        if !self.wants_animation() {
            return false;
        }
        let elapsed = now.saturating_duration_since(self.last_update).as_secs_f64();
        self.last_update = now;
        if elapsed <= 0.0 {
            return false;
        }
        // A long stall — the machine slept, or a modal loop held the thread —
        // must not teleport the document. Integrate at most one slow frame.
        let step = elapsed.min(1.0 / 15.0);
        let previous = self.position;
        match self.mode {
            ScrollMode::Kinetic => {
                self.position = self.clamp(self.position + self.kinetic_velocity * step);
                let decay = KINETIC_DECAY_PER_SECOND.powf(step);
                self.kinetic_velocity = self.kinetic_velocity * decay;
                let stopped_at_edge = self.position == previous;
                if stopped_at_edge || self.kinetic_velocity.length() < KINETIC_STOP_SPEED {
                    self.kinetic_velocity = Vec2d::ZERO;
                    self.mode = ScrollMode::Stationary;
                }
            }
            ScrollMode::AutoScroll => {
                self.position = self.clamp(self.position + self.autoscroll_velocity * step);
            }
            _ => return false,
        }
        self.velocity = if step > 0.0 {
            (self.position - previous) / step
        } else {
            Vec2d::ZERO
        };
        self.smoothed_velocity = self.velocity;
        self.target = self.position;
        if self.position != previous {
            self.generation = self.generation.wrapping_add(1);
            true
        } else {
            false
        }
    }

    pub fn settle(&mut self) {
        self.mode = ScrollMode::Stationary;
        self.stop_animation();
        self.velocity = Vec2d::ZERO;
        self.smoothed_velocity = Vec2d::ZERO;
        self.target = self.position;
    }

    pub fn max_position(&self) -> Vec2d {
        Vec2d {
            x: (self.content_extent.width - self.viewport_extent.width).max(0.0),
            y: (self.content_extent.height - self.viewport_extent.height).max(0.0),
        }
    }

    fn stop_animation(&mut self) {
        self.kinetic_velocity = Vec2d::ZERO;
        self.autoscroll_velocity = Vec2d::ZERO;
    }

    fn clamp(&self, position: Vec2d) -> Vec2d {
        let max = self.max_position();
        Vec2d {
            x: position.x.clamp(0.0, max.x),
            y: position.y.clamp(0.0, max.y),
        }
    }
}

/// Map a pointer offset from the autoscroll anchor onto a velocity.
///
/// Quadratic beyond the dead zone: small offsets creep, large offsets travel,
/// and the transition has no step in it.
fn autoscroll_velocity_for(offset: Vec2d) -> Vec2d {
    let axis = |value: f64| {
        let magnitude = value.abs();
        if magnitude <= AUTOSCROLL_DEAD_ZONE {
            return 0.0;
        }
        let past = (magnitude - AUTOSCROLL_DEAD_ZONE) / AUTOSCROLL_DEAD_ZONE;
        let speed = (past * past * AUTOSCROLL_GAIN * AUTOSCROLL_DEAD_ZONE).min(AUTOSCROLL_MAX_SPEED);
        speed.copysign(value)
    };
    Vec2d {
        x: axis(offset.x),
        y: axis(offset.y),
    }
}

/// Exponential smoothing with a fixed time constant, so an irregular sample
/// cadence does not change how much history is retained.
fn smooth(previous: Vec2d, sample: Vec2d, elapsed: f64) -> Vec2d {
    let alpha = 1.0 - (-elapsed / VELOCITY_SMOOTHING_SECONDS).exp();
    previous + (sample - previous) * alpha
}

impl Default for ScrollModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use std::time::Duration;

    use super::*;

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
        model
    }

    #[test]
    fn wheel_scale_multiplies_every_wheel_source() {
        let mut m = model();
        m.tuning.wheel_scale = 2.0;
        m.apply(ScrollCommand::WheelPixels(Vec2d { x: 0.0, y: 10.0 }));
        assert_eq!(m.position.y, 20.0);
        m.apply(ScrollCommand::Stop);
        m.position = Vec2d::ZERO;
        m.apply(ScrollCommand::WheelLines(Vec2d { x: 0.0, y: 1.0 }));
        assert_eq!(m.position.y, 84.0);
    }

    #[test]
    fn drag_pan_ignores_wheel_scale() {
        let mut m = model();
        m.tuning.wheel_scale = 4.0;
        m.apply(ScrollCommand::DragPan(Vec2d { x: 0.0, y: 25.0 }));
        assert_eq!(m.position.y, 25.0);
    }

    #[test]
    fn a_slow_release_does_not_fling() {
        let mut m = model();
        m.tuning.kinetic_enabled = true;
        let start = Instant::now();
        m.apply_at(
            ScrollCommand::DragPan(Vec2d { x: 0.0, y: 1.0 }),
            start + Duration::from_millis(100),
        );
        m.apply_at(ScrollCommand::Fling { from_touch: false }, start + Duration::from_millis(110));
        assert_eq!(m.mode, ScrollMode::Stationary);
        assert!(!m.wants_animation());
    }

    #[test]
    fn a_fast_release_glides_and_stops_on_its_own() {
        let mut m = model();
        m.tuning.kinetic_enabled = true;
        let start = Instant::now();
        // Three fast samples so the smoothed velocity is genuinely high.
        for step in 1..=3 {
            m.apply_at(
                ScrollCommand::DragPan(Vec2d { x: 0.0, y: 40.0 }),
                start + Duration::from_millis(step * 16),
            );
        }
        m.apply_at(ScrollCommand::Fling { from_touch: false }, start + Duration::from_millis(56));
        assert_eq!(m.mode, ScrollMode::Kinetic);

        let launched_at = m.position.y;
        let mut now = start + Duration::from_millis(56);
        for _ in 0..600 {
            now += Duration::from_millis(16);
            if !m.advance(now) && !m.wants_animation() {
                break;
            }
        }
        assert_eq!(m.mode, ScrollMode::Stationary);
        assert!(m.position.y > launched_at, "kinetic motion moved forward");
    }

    #[test]
    fn a_touch_flick_glides_even_with_kinetic_motion_off() {
        let mut m = model();
        assert!(!m.tuning.kinetic_enabled);
        let start = Instant::now();
        for step in 1..=3 {
            m.apply_at(
                ScrollCommand::DragPan(Vec2d { x: 0.0, y: 40.0 }),
                start + Duration::from_millis(step * 16),
            );
        }
        m.apply_at(
            ScrollCommand::Fling { from_touch: true },
            start + Duration::from_millis(56),
        );
        assert_eq!(m.mode, ScrollMode::Kinetic);
    }

    #[test]
    fn kinetic_motion_is_disabled_by_default() {
        let mut m = model();
        let start = Instant::now();
        for step in 1..=3 {
            m.apply_at(
                ScrollCommand::DragPan(Vec2d { x: 0.0, y: 40.0 }),
                start + Duration::from_millis(step * 16),
            );
        }
        m.apply_at(ScrollCommand::Fling { from_touch: false }, start + Duration::from_millis(56));
        assert_eq!(m.mode, ScrollMode::Stationary);
    }

    #[test]
    fn kinetic_motion_stops_at_the_document_edge() {
        let mut m = model();
        m.tuning.kinetic_enabled = true;
        m.position = Vec2d {
            x: 0.0,
            y: m.max_position().y - 1.0,
        };
        let start = Instant::now();
        for step in 1..=3 {
            m.apply_at(
                ScrollCommand::DragPan(Vec2d { x: 0.0, y: 40.0 }),
                start + Duration::from_millis(step * 16),
            );
        }
        m.apply_at(ScrollCommand::Fling { from_touch: false }, start + Duration::from_millis(56));
        let mut now = start + Duration::from_millis(56);
        for _ in 0..200 {
            now += Duration::from_millis(16);
            m.advance(now);
            if !m.wants_animation() {
                break;
            }
        }
        assert_eq!(m.position.y, m.max_position().y);
        assert_eq!(m.mode, ScrollMode::Stationary);
    }

    #[test]
    fn autoscroll_has_a_dead_zone_and_a_signed_direction() {
        let mut m = model();
        m.apply(ScrollCommand::BeginAutoScroll);
        assert_eq!(m.mode, ScrollMode::AutoScroll);
        m.apply(ScrollCommand::SteerAutoScroll(Vec2d { x: 0.0, y: 5.0 }));
        assert!(!m.wants_animation(), "inside the dead zone nothing moves");

        m.apply(ScrollCommand::SteerAutoScroll(Vec2d { x: 0.0, y: 60.0 }));
        assert!(m.wants_animation());
        let start = Instant::now();
        m.last_update = start;
        assert!(m.advance(start + Duration::from_millis(100)));
        let forward = m.position.y;
        assert!(forward > 0.0);

        m.apply(ScrollCommand::SteerAutoScroll(Vec2d { x: 0.0, y: -60.0 }));
        m.last_update = start;
        m.advance(start + Duration::from_millis(100));
        assert!(m.position.y < forward, "steering back reverses the travel");

        m.apply(ScrollCommand::EndAutoScroll);
        assert_eq!(m.mode, ScrollMode::Stationary);
        assert!(!m.wants_animation());
    }

    #[test]
    fn a_long_stall_integrates_at_most_one_slow_frame() {
        let mut m = model();
        m.apply(ScrollCommand::BeginAutoScroll);
        m.apply(ScrollCommand::SteerAutoScroll(Vec2d { x: 0.0, y: 60.0 }));
        let start = Instant::now();
        m.last_update = start;
        m.advance(start + Duration::from_secs(30));
        let speed = autoscroll_velocity_for(Vec2d { x: 0.0, y: 60.0 }).y;
        assert!(
            m.position.y <= speed / 15.0 + 1.0,
            "a 30 second stall advanced {:.1}pt, not a whole document",
            m.position.y
        );
    }

    #[test]
    fn tuning_sanitizes_a_hand_edited_settings_file() {
        let wild = MovementTuning {
            line_wheel_distance: f64::NAN,
            wheel_scale: 1e9,
            kinetic_enabled: true,
        }
        .sanitized();
        assert_eq!(wild.line_wheel_distance, 42.0);
        assert_eq!(wild.wheel_scale, MovementTuning::MAX_WHEEL_SCALE);
        assert!(wild.kinetic_enabled);
    }

    #[test]
    fn direct_input_cancels_kinetic_motion() {
        let mut m = model();
        m.tuning.kinetic_enabled = true;
        let start = Instant::now();
        for step in 1..=3 {
            m.apply_at(
                ScrollCommand::DragPan(Vec2d { x: 0.0, y: 40.0 }),
                start + Duration::from_millis(step * 16),
            );
        }
        m.apply_at(ScrollCommand::Fling { from_touch: false }, start + Duration::from_millis(56));
        assert!(m.wants_animation());
        m.apply_at(
            ScrollCommand::WheelLines(Vec2d { x: 0.0, y: 1.0 }),
            start + Duration::from_millis(60),
        );
        assert!(!m.wants_animation());
        assert_eq!(m.mode, ScrollMode::Direct);
    }
}
