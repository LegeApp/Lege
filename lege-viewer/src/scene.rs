use std::sync::Arc;

use crate::document::{TileKey, TileSurface};
use crate::geometry::{RectF, RectI};
use crate::paint::PixelSurface;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSampling {
    Nearest,
    Linear,
}

#[derive(Debug, Clone)]
pub enum SceneCommand {
    Solid {
        rect: RectI,
        clip: RectI,
        color: u32,
    },
    AlphaSolid {
        rect: RectI,
        clip: RectI,
        color: u32,
    },
    Image {
        tile: Arc<TileSurface>,
        destination: RectF,
        clip: RectI,
        sampling: ImageSampling,
    },
    Surface {
        surface: Arc<SceneSurface>,
        destination: RectF,
        clip: RectI,
        sampling: ImageSampling,
    },
}

#[derive(Debug, Clone)]
pub struct SceneSurface {
    pub key: [u64; 4],
    pub revision: u64,
    pub pixels: PixelSurface,
}

#[derive(Debug, Clone)]
pub struct FrameScene {
    pub width: u32,
    pub height: u32,
    pub clear_color: u32,
    pub commands: Vec<SceneCommand>,
    clips: Vec<RectI>,
}

impl FrameScene {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            clear_color: 0,
            commands: Vec::with_capacity(512),
            clips: Vec::with_capacity(8),
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    pub fn begin(&mut self, clear_color: u32) -> SceneBuilder<'_> {
        self.clear_color = clear_color;
        self.commands.clear();
        let bounds = RectI {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        };
        self.clips.clear();
        self.clips.push(bounds);
        SceneBuilder { scene: self }
    }

    pub fn bounds(&self) -> RectI {
        RectI {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }
}

#[derive(Debug)]
pub struct SceneBuilder<'a> {
    scene: &'a mut FrameScene,
}

impl SceneBuilder<'_> {
    pub fn push_clip(&mut self, rect: RectI) {
        let clip = self.clip().intersection(rect).unwrap_or_default();
        self.scene.clips.push(clip);
    }

    pub fn pop_clip(&mut self) {
        if self.scene.clips.len() > 1 {
            self.scene.clips.pop();
        }
    }

    pub fn fill_rect(&mut self, rect: RectI, color: u32) {
        let clip = self.clip();
        if rect.intersection(clip).is_none() {
            return;
        }
        self.scene
            .commands
            .push(SceneCommand::Solid { rect, clip, color });
    }

    pub fn blend_rect(&mut self, rect: RectI, argb: u32) {
        let clip = self.clip();
        if rect.intersection(clip).is_none() {
            return;
        }
        self.scene.commands.push(SceneCommand::AlphaSolid {
            rect,
            clip,
            color: argb,
        });
    }

    pub fn stroke_rect(&mut self, rect: RectI, width: u32, color: u32) {
        if width == 0 || rect.is_empty() {
            return;
        }
        self.fill_rect(
            RectI {
                height: width.min(rect.height),
                ..rect
            },
            color,
        );
        self.fill_rect(
            RectI {
                y: rect.bottom().saturating_sub(width as i32),
                height: width.min(rect.height),
                ..rect
            },
            color,
        );
        self.fill_rect(
            RectI {
                width: width.min(rect.width),
                ..rect
            },
            color,
        );
        self.fill_rect(
            RectI {
                x: rect.right().saturating_sub(width as i32),
                width: width.min(rect.width),
                ..rect
            },
            color,
        );
    }

    pub fn draw_tile(
        &mut self,
        tile: Arc<TileSurface>,
        destination: RectF,
        sampling: ImageSampling,
    ) {
        let clip = self.clip();
        if RectI::from(destination).intersection(clip).is_none() {
            return;
        }
        self.scene.commands.push(SceneCommand::Image {
            tile,
            destination,
            clip,
            sampling,
        });
    }

    pub fn draw_surface(
        &mut self,
        surface: Arc<SceneSurface>,
        destination: RectF,
        sampling: ImageSampling,
    ) {
        let clip = self.clip();
        if RectI::from(destination).intersection(clip).is_none() {
            return;
        }
        self.scene.commands.push(SceneCommand::Surface {
            surface,
            destination,
            clip,
            sampling,
        });
    }

    fn clip(&self) -> RectI {
        self.scene.clips.last().copied().unwrap_or_default()
    }
}

pub fn tile_image_words(tile: &TileSurface) -> [u64; 4] {
    tile_key_image_words(tile.key)
}

fn tile_key_image_words(key: TileKey) -> [u64; 4] {
    let bucket = u16::from_ne_bytes(key.bucket.0.to_ne_bytes()) as u64;
    let x = u32::from_ne_bytes(key.coord.x.to_ne_bytes()) as u64;
    let y = u32::from_ne_bytes(key.coord.y.to_ne_bytes()) as u64;
    [
        key.document.0,
        u64::from(key.page.0) | (bucket << 32) | (u64::from(key.tier.rank()) << 48),
        x | (y << 32),
        key.variant,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{DocumentId, PageIndex, TileCoord, TileKey, TileTier, ZoomBucket};

    #[test]
    fn scene_builder_records_effective_clip_without_a_widget_tree() {
        let mut scene = FrameScene::new(100, 80);
        {
            let mut builder = scene.begin(0);
            builder.push_clip(RectI {
                x: 10,
                y: 10,
                width: 30,
                height: 20,
            });
            builder.fill_rect(
                RectI {
                    x: 0,
                    y: 0,
                    width: 50,
                    height: 50,
                },
                0x12_34_56,
            );
            builder.pop_clip();
        }
        assert!(matches!(
            scene.commands.as_slice(),
            [SceneCommand::Solid {
                clip: RectI {
                    x: 10,
                    y: 10,
                    width: 30,
                    height: 20
                },
                ..
            }]
        ));
    }

    #[test]
    fn tile_image_identity_includes_render_variant() {
        let key = TileKey {
            document: DocumentId(7),
            page: PageIndex(3),
            bucket: ZoomBucket::ONE,
            coord: TileCoord { x: -2, y: 5 },
            tier: TileTier::Thumbnail,
            variant: 11,
        };
        let mut changed = key;
        changed.variant = 12;

        assert_ne!(tile_key_image_words(key), tile_key_image_words(changed));
    }
}
