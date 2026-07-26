use std::num::NonZeroU32;
use std::sync::Arc;

use softbuffer::{Context, Surface};
use winit::window::Window;

use crate::geometry::RectI;
use crate::paint::{Painter, WindowBuffer, scroll_blit};
use crate::scene::{FrameScene, ImageSampling, SceneCommand};

use super::{PixelFormat, PresentError, Presenter, PresenterBackend, PresenterStats, ScrollReuse};

#[derive(Debug)]
pub struct SoftwareCompositor {
    buffer: WindowBuffer,
}

impl SoftwareCompositor {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            buffer: WindowBuffer::new(width, height),
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.buffer.resize(width, height);
    }

    pub fn render(
        &mut self,
        scene: &FrameScene,
        damage: &[RectI],
        scroll_reuse: Option<ScrollReuse>,
    ) {
        if self.buffer.width != scene.width || self.buffer.height != scene.height {
            self.resize(scene.width, scene.height);
        }
        if let Some(scroll) = scroll_reuse {
            let _ = scroll_blit(
                &mut self.buffer,
                scroll.canvas,
                scroll.delta_x,
                scroll.delta_y,
            );
        }
        let mut painter = Painter::new(&mut self.buffer);
        for &damage_rect in damage {
            painter.push_clip(damage_rect);
            painter.fill_rect(damage_rect, scene.clear_color);
            for command in &scene.commands {
                match command {
                    SceneCommand::Solid { rect, clip, color } => {
                        painter.push_clip(*clip);
                        painter.fill_rect(*rect, *color);
                        painter.pop_clip();
                    }
                    SceneCommand::AlphaSolid { rect, clip, color } => {
                        painter.push_clip(*clip);
                        painter.blend_rect(*rect, *color);
                        painter.pop_clip();
                    }
                    SceneCommand::Image {
                        tile,
                        destination,
                        clip,
                        sampling,
                    } => {
                        painter.push_clip(*clip);
                        match sampling {
                            ImageSampling::Nearest => {
                                painter.blit_scaled(&tile.pixels, RectI::from(*destination));
                            }
                            ImageSampling::Linear => {
                                painter.blit_scaled_linear(&tile.pixels, RectI::from(*destination));
                            }
                        }
                        painter.pop_clip();
                    }
                    SceneCommand::Surface {
                        surface,
                        destination,
                        clip,
                        sampling,
                    } => {
                        painter.push_clip(*clip);
                        match sampling {
                            ImageSampling::Nearest => {
                                painter.blit_scaled(&surface.pixels, RectI::from(*destination));
                            }
                            ImageSampling::Linear => {
                                painter
                                    .blit_scaled_linear(&surface.pixels, RectI::from(*destination));
                            }
                        }
                        painter.pop_clip();
                    }
                }
            }
            painter.pop_clip();
        }
    }

    pub fn buffer(&self) -> &WindowBuffer {
        &self.buffer
    }
}

#[allow(missing_debug_implementations)]
pub struct SoftbufferPresenter {
    _context: Context<Arc<Window>>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    width: u32,
    height: u32,
    compositor: SoftwareCompositor,
    damage_rects: Vec<softbuffer::Rect>,
    frames_presented: u64,
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
            compositor: SoftwareCompositor::new(1, 1),
            damage_rects: Vec::with_capacity(8),
            frames_presented: 0,
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
        self.compositor.resize(width, height);
        Ok(())
    }

    fn present(
        &mut self,
        scene: &FrameScene,
        damage: &[RectI],
        scroll_reuse: Option<ScrollReuse>,
    ) -> Result<(), PresentError> {
        if scene.width != self.width || scene.height != self.height {
            self.resize(scene.width, scene.height)?;
        }
        self.compositor.render(scene, damage, scroll_reuse);
        let mut target = self
            .surface
            .buffer_mut()
            .map_err(|error| PresentError::Backend(error.to_string()))?;
        target.copy_from_slice(&self.compositor.buffer().pixels);

        self.damage_rects.clear();
        self.damage_rects.extend(damage.iter().filter_map(|rect| {
            Some(softbuffer::Rect {
                x: rect.x.max(0) as u32,
                y: rect.y.max(0) as u32,
                width: NonZeroU32::new(rect.width)?,
                height: NonZeroU32::new(rect.height)?,
            })
        }));
        let result = if self.damage_rects.is_empty() {
            target
                .present()
                .map_err(|error| PresentError::Backend(error.to_string()))
        } else {
            target
                .present_with_damage(&self.damage_rects)
                .map_err(|error| PresentError::Backend(error.to_string()))
        };
        if result.is_ok() {
            self.frames_presented = self.frames_presented.saturating_add(1);
        }
        result
    }

    fn format(&self) -> PixelFormat {
        PixelFormat::Xrgb8888
    }

    fn backend(&self) -> PresenterBackend {
        PresenterBackend::Software
    }

    fn stats(&self) -> PresenterStats {
        PresenterStats {
            backend: Some("softbuffer".to_owned()),
            frames_presented: self.frames_presented,
            ..PresenterStats::default()
        }
    }
}
