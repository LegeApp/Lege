use std::sync::Arc;

use lege_gpu::presentation::{
    GpuCompositor, ImageKey, ImageQuad, ImageSource, PresentationConfig, Rect, Sampling,
};
use winit::window::Window;

use crate::geometry::{RectF, RectI};
use crate::scene::{FrameScene, ImageSampling, SceneCommand, tile_image_words};

use super::{PixelFormat, PresentError, Presenter, PresenterBackend, PresenterStats, ScrollReuse};

#[allow(missing_debug_implementations)]
pub struct WgpuPresenter {
    compositor: GpuCompositor,
}

impl WgpuPresenter {
    pub fn new(window: Arc<Window>, width: u32, height: u32) -> Result<Self, PresentError> {
        let compositor = GpuCompositor::new(
            window,
            width.max(1),
            height.max(1),
            PresentationConfig::default(),
        )
        .map_err(|error| PresentError::Backend(error.to_string()))?;
        Ok(Self { compositor })
    }
}

impl Presenter for WgpuPresenter {
    fn resize(&mut self, width: u32, height: u32) -> Result<(), PresentError> {
        self.compositor
            .resize(width, height)
            .map_err(|error| PresentError::Backend(error.to_string()))
    }

    fn present(
        &mut self,
        scene: &FrameScene,
        _damage: &[RectI],
        _scroll_reuse: Option<ScrollReuse>,
    ) -> Result<(), PresentError> {
        self.compositor.begin_frame(scene.clear_color);
        for command in &scene.commands {
            match command {
                SceneCommand::Solid { rect, clip, color } => {
                    self.compositor
                        .push_solid(gpu_rect_i(*rect), gpu_rect_i(*clip), *color);
                }
                SceneCommand::AlphaSolid { rect, clip, color } => {
                    self.compositor
                        .push_solid_argb(gpu_rect_i(*rect), gpu_rect_i(*clip), *color);
                }
                SceneCommand::Image {
                    tile,
                    destination,
                    clip,
                    sampling,
                } => {
                    self.compositor
                        .push_image(ImageQuad {
                            source: ImageSource {
                                key: ImageKey(tile_image_words(tile)),
                                revision: tile.generation,
                                width: tile.pixels.width,
                                height: tile.pixels.height,
                                stride_pixels: tile.pixels.stride,
                                pixels: &tile.pixels.pixels,
                            },
                            destination: gpu_rect(*destination),
                            clip: gpu_rect_i(*clip),
                            sampling: match sampling {
                                ImageSampling::Nearest => Sampling::Nearest,
                                ImageSampling::Linear => Sampling::Linear,
                            },
                        })
                        .map_err(|error| PresentError::Backend(error.to_string()))?;
                }
                SceneCommand::Surface {
                    surface,
                    destination,
                    clip,
                    sampling,
                } => {
                    self.compositor
                        .push_image(ImageQuad {
                            source: ImageSource {
                                key: ImageKey(surface.key),
                                revision: surface.revision,
                                width: surface.pixels.width,
                                height: surface.pixels.height,
                                stride_pixels: surface.pixels.stride,
                                pixels: &surface.pixels.pixels,
                            },
                            destination: gpu_rect(*destination),
                            clip: gpu_rect_i(*clip),
                            sampling: match sampling {
                                ImageSampling::Nearest => Sampling::Nearest,
                                ImageSampling::Linear => Sampling::Linear,
                            },
                        })
                        .map_err(|error| PresentError::Backend(error.to_string()))?;
                }
            }
        }
        self.compositor
            .present()
            .map(|_| ())
            .map_err(|error| PresentError::Backend(error.to_string()))
    }

    fn format(&self) -> PixelFormat {
        PixelFormat::Xrgb8888
    }

    fn backend(&self) -> PresenterBackend {
        PresenterBackend::Gpu
    }

    fn stats(&self) -> PresenterStats {
        let stats = self.compositor.stats();
        PresenterStats {
            adapter: Some(self.compositor.adapter_name().to_owned()),
            backend: Some(self.compositor.backend_name().to_owned()),
            frames_presented: stats.frames_presented,
            frames_skipped: stats.frames_skipped,
            atlas_uploads: stats.atlas_uploads,
            atlas_upload_bytes: stats.atlas_upload_bytes,
            atlas_resident_images: stats.atlas_resident_images,
            atlas_bytes: stats.atlas_bytes,
            draw_calls: stats.draw_calls,
            vertices: stats.vertices,
        }
    }
}

fn gpu_rect(rect: RectF) -> Rect {
    Rect {
        x: rect.x as f32,
        y: rect.y as f32,
        width: rect.width as f32,
        height: rect.height as f32,
    }
}

fn gpu_rect_i(rect: RectI) -> Rect {
    Rect {
        x: rect.x as f32,
        y: rect.y as f32,
        width: rect.width as f32,
        height: rect.height as f32,
    }
}
