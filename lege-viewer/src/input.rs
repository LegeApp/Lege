use std::time::Instant;

use winit::keyboard::ModifiersState;

use crate::geometry::PointF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Open,
    PreviousPage,
    NextPage,
    ZoomOut,
    ZoomIn,
    FitWidth,
    FitPage,
    ToggleSidebar,
    ToggleFullscreen,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarPart {
    DecrementTrack,
    Thumb,
    IncrementTrack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    None,
    Canvas,
    ToolbarButton(Command),
    VerticalScrollbar(ScrollbarPart),
    HorizontalScrollbar(ScrollbarPart),
    Sidebar,
    Status,
    Popup,
}

#[derive(Debug, Clone, Copy)]
pub struct ScrollbarDragState {
    pub pointer_offset_in_thumb: f64,
}

/// A middle-click autoscroll in progress.
///
/// The anchor is where the reader pressed. Pointer displacement from it sets
/// the speed, so the gesture is "point where you want to go" rather than a
/// drag the arm has to sustain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoScrollState {
    pub anchor: PointF,
    /// True once the pointer has left the dead zone. Until then a release is
    /// read as a click that cancels, not as a deliberate stop.
    pub engaged: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum PointerCapture {
    /// Grab-and-drag panning of the canvas. `last` is the previous pointer
    /// position, so each move applies a delta rather than re-deriving one
    /// from the origin and accumulating clamp error at the document edges.
    CanvasPan {
        origin: PointF,
        last: PointF,
        pressed_at: Instant,
        moved: bool,
    },
    VerticalThumb(ScrollbarDragState),
    HorizontalThumb(ScrollbarDragState),
    SidebarResize {
        initial_width: f64,
    },
    ProcessingPanelResize {
        origin: PointF,
        initial_width: f64,
        initial_height: f64,
    },
    Selection {
        anchor: PointF,
    },
}

#[derive(Debug, Clone)]
pub struct InputState {
    pub pointer_position: PointF,
    pub modifiers: ModifiersState,
    pub hover: HitTarget,
    pub capture: Option<PointerCapture>,
    pub left_button_down: bool,
    pub autoscroll: Option<AutoScrollState>,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            pointer_position: PointF::default(),
            modifiers: ModifiersState::empty(),
            hover: HitTarget::None,
            capture: None,
            left_button_down: false,
            autoscroll: None,
        }
    }
}
