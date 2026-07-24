//! Planned steady-state presenter.
//!
//! The shared currency is already 256×256 tiles, so this module can replace
//! software page blits with texture-atlas quads without changing document,
//! scroll, cache, or conductor code. The supplied renderer's wgpu backend is
//! still a capability placeholder; implementation belongs after the bridge
//! and synthetic performance gates are measured.

use crate::geometry::RectI;
use crate::paint::WindowBuffer;

use super::{PixelFormat, PresentError, Presenter};

#[derive(Debug, Default)]
pub struct WgpuPresenterSkeleton;

impl Presenter for WgpuPresenterSkeleton {
    fn resize(&mut self, _width: u32, _height: u32) -> Result<(), PresentError> {
        Ok(())
    }

    fn present(&mut self, _buffer: &WindowBuffer, _damage: &[RectI]) -> Result<(), PresentError> {
        Err(PresentError::Backend(
            "wgpu presenter skeleton is not implemented".to_owned(),
        ))
    }

    fn format(&self) -> PixelFormat {
        PixelFormat::Xrgb8888
    }
}
