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

#[derive(Debug, Clone, Copy)]
pub enum PointerCapture {
    CanvasPan { origin: PointF },
    VerticalThumb(ScrollbarDragState),
    HorizontalThumb(ScrollbarDragState),
    SidebarResize { initial_width: f64 },
    Selection { anchor: PointF },
}

#[derive(Debug, Clone)]
pub struct InputState {
    pub pointer_position: PointF,
    pub modifiers: ModifiersState,
    pub hover: HitTarget,
    pub capture: Option<PointerCapture>,
    pub left_button_down: bool,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            pointer_position: PointF::default(),
            modifiers: ModifiersState::empty(),
            hover: HitTarget::None,
            capture: None,
            left_button_down: false,
        }
    }
}
