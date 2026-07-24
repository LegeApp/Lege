use crate::geometry::RectI;
use crate::paint::WindowBuffer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Xrgb8888,
}

#[derive(Debug, thiserror::Error)]
pub enum PresentError {
    #[error("presenter backend failed: {0}")]
    Backend(String),
    #[error("invalid surface size")]
    InvalidSize,
}

pub trait Presenter {
    fn resize(&mut self, width: u32, height: u32) -> Result<(), PresentError>;
    fn present(&mut self, buffer: &WindowBuffer, damage: &[RectI]) -> Result<(), PresentError>;
    fn format(&self) -> PixelFormat;
}

#[cfg(feature = "softbuffer-presenter")]
pub mod softbuffer;
pub mod wgpu;
