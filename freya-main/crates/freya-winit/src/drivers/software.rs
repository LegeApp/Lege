use std::num::NonZeroU32;

use freya_render_tiny_skia::prelude::TinySkiaRenderer;
use winit::{dpi::PhysicalSize, window::Window};

pub struct SoftwareDriver {
    context: Option<softbuffer::Context<&'static Window>>,
    surface: Option<softbuffer::Surface<&'static Window, &'static Window>>,
    /// Reused across frames; `begin_frame` only reallocates the pixmap when
    /// the window size changes.
    renderer: Option<TinySkiaRenderer>,
    last_size: PhysicalSize<u32>,
}

impl SoftwareDriver {
    pub fn new() -> Self {
        Self {
            context: None,
            surface: None,
            renderer: None,
            last_size: PhysicalSize::new(0, 0),
        }
    }

    pub fn present(
        &mut self,
        size: PhysicalSize<u32>,
        window: &Window,
        render: impl FnOnce(&mut TinySkiaRenderer),
    ) -> Result<(), SoftwarePresentError> {
        let width = NonZeroU32::new(size.width).ok_or(SoftwarePresentError::EmptySize)?;
        let height = NonZeroU32::new(size.height).ok_or(SoftwarePresentError::EmptySize)?;

        let ctx_ref = self.context.get_or_insert_with(|| {
            softbuffer::Context::new(unsafe {
                std::mem::transmute::<&Window, &'static Window>(window)
            })
            .expect("Failed to create softbuffer context")
        });

        let is_new_surface = self.surface.is_none();
        let surf_ref = self.surface.get_or_insert_with(|| {
            softbuffer::Surface::new(ctx_ref, unsafe {
                std::mem::transmute::<&Window, &'static Window>(window)
            })
            .expect("Failed to create softbuffer surface")
        });

        if is_new_surface || size != self.last_size {
            surf_ref
                .resize(width, height)
                .map_err(SoftwarePresentError::Softbuffer)?;
            self.last_size = size;
        }

        let renderer = match self.renderer.as_mut() {
            Some(renderer) => {
                renderer
                    .begin_frame(size.width, size.height)
                    .map_err(SoftwarePresentError::Renderer)?;
                renderer
            }
            None => {
                let renderer = TinySkiaRenderer::new(size.width, size.height)
                    .map_err(SoftwarePresentError::Renderer)?;
                self.renderer.insert(renderer)
            }
        };

        render(renderer);

        let mut buffer = surf_ref
            .buffer_mut()
            .map_err(SoftwarePresentError::Softbuffer)?;
        renderer.write_softbuffer_rgb(&mut buffer);
        window.pre_present_notify();
        buffer.present().map_err(SoftwarePresentError::Softbuffer)
    }

    pub fn resize(&mut self, _size: PhysicalSize<u32>) {
        // The softbuffer surface is resized during `present`, when we have
        // access to the surface and can render into a buffer with matching
        // dimensions. Mark the cached size stale here so the next present does
        // not skip `Surface::resize` after a window resize event.
        self.last_size = PhysicalSize::new(0, 0);
    }
}

impl Default for SoftwareDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum SoftwarePresentError {
    EmptySize,
    Renderer(freya_render_tiny_skia::RendererError),
    Softbuffer(softbuffer::SoftBufferError),
}
