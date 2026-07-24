use std::num::NonZeroU32;
use std::sync::Arc;

use softbuffer::{Context, Surface};
use winit::window::Window;

use crate::geometry::RectI;
use crate::paint::WindowBuffer;

use super::{PixelFormat, PresentError, Presenter};

pub struct SoftbufferPresenter {
    _context: Context<Arc<Window>>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    width: u32,
    height: u32,
}

impl SoftbufferPresenter {
    pub fn new(window: Arc<Window>) -> Result<Self, PresentError> {
        let context = Context::new(window.clone())
            .map_err(|error| PresentError::Backend(error.to_string()))?;
        let surface = Surface::new(&context, window)
            .map_err(|error| PresentError::Backend(error.to_string()))?;
        Ok(Self {
            _context: context,
            surface,
            width: 0,
            height: 0,
        })
    }
}

impl Presenter for SoftbufferPresenter {
    fn resize(&mut self, width: u32, height: u32) -> Result<(), PresentError> {
        let width_nonzero = NonZeroU32::new(width).ok_or(PresentError::InvalidSize)?;
        let height_nonzero = NonZeroU32::new(height).ok_or(PresentError::InvalidSize)?;
        self.surface
            .resize(width_nonzero, height_nonzero)
            .map_err(|error| PresentError::Backend(error.to_string()))?;
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn present(&mut self, buffer: &WindowBuffer, damage: &[RectI]) -> Result<(), PresentError> {
        if buffer.width != self.width || buffer.height != self.height {
            self.resize(buffer.width, buffer.height)?;
        }
        let mut target = self
            .surface
            .buffer_mut()
            .map_err(|error| PresentError::Backend(error.to_string()))?;
        target.copy_from_slice(&buffer.pixels);

        let damage: Vec<softbuffer::Rect> = damage
            .iter()
            .filter_map(|rect| {
                Some(softbuffer::Rect {
                    x: rect.x.max(0) as u32,
                    y: rect.y.max(0) as u32,
                    width: NonZeroU32::new(rect.width)?,
                    height: NonZeroU32::new(rect.height)?,
                })
            })
            .collect();
        if damage.is_empty() {
            target
                .present()
                .map_err(|error| PresentError::Backend(error.to_string()))
        } else {
            target
                .present_with_damage(&damage)
                .map_err(|error| PresentError::Backend(error.to_string()))
        }
    }

    fn format(&self) -> PixelFormat {
        PixelFormat::Xrgb8888
    }
}
