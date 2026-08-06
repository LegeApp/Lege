use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use copypasta::{ClipboardContext, ClipboardProvider};
use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight,
};
use pdf_font::StandardFont;

use crate::geometry::RectI;
use crate::paint::PixelSurface;
use crate::scene::SceneSurface;

#[derive(Debug, Clone)]
pub struct TextPaint {
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub max_width: u32,
    pub size: f32,
    pub color: u32,
    pub bold: bool,
    pub centered: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RectPaint {
    pub rect: RectI,
    pub color: u32,
}

#[allow(missing_debug_implementations)]
pub struct UiTextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    revision: u64,
    surfaces: HashMap<[u64; 4], (u64, Arc<SceneSurface>)>,
}

impl UiTextRenderer {
    pub fn new() -> Self {
        let mut font_system = FontSystem::new();
        font_system
            .db_mut()
            .load_font_data(StandardFont::Helvetica.program_data().to_vec());
        Self {
            font_system,
            swash_cache: SwashCache::new(),
            revision: 0,
            surfaces: HashMap::new(),
        }
    }

    pub fn render(
        &mut self,
        key: [u64; 4],
        width: u32,
        height: u32,
        background: u32,
        rectangles: &[RectPaint],
        text: &[TextPaint],
    ) -> Arc<SceneSurface> {
        let stride = width as usize;
        let mut pixels = vec![background; stride.saturating_mul(height as usize)];
        for paint in rectangles {
            fill_rect(&mut pixels, stride, width, height, paint.rect, paint.color);
        }
        for paint in text {
            self.draw_text(&mut pixels, stride, width, height, paint);
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        width.hash(&mut hasher);
        height.hash(&mut hasher);
        pixels.hash(&mut hasher);
        let fingerprint = hasher.finish();
        if let Some((cached_fingerprint, surface)) = self.surfaces.get(&key)
            && *cached_fingerprint == fingerprint
        {
            return Arc::clone(surface);
        }
        self.revision = self.revision.wrapping_add(1).max(1);
        let surface = Arc::new(SceneSurface {
            key,
            revision: self.revision,
            pixels: PixelSurface {
                width,
                height,
                stride,
                pixels: pixels.into(),
            },
        });
        self.surfaces
            .insert(key, (fingerprint, Arc::clone(&surface)));
        surface
    }

    fn draw_text(
        &mut self,
        pixels: &mut [u32],
        stride: usize,
        width: u32,
        height: u32,
        paint: &TextPaint,
    ) {
        if paint.text.is_empty() || paint.max_width == 0 {
            return;
        }
        let line_height = (paint.size * 1.35).ceil();
        let mut buffer = Buffer::new(&mut self.font_system, Metrics::new(paint.size, line_height));
        {
            let mut borrowed = buffer.borrow_with(&mut self.font_system);
            borrowed.set_size(Some(paint.max_width as f32), Some(line_height));
            let attrs = Attrs::new()
                .family(Family::Name("FoxitSans"))
                .weight(if paint.bold {
                    Weight::BOLD
                } else {
                    Weight::NORMAL
                });
            borrowed.set_text(&paint.text, &attrs, Shaping::Advanced, None);
            if paint.centered
                && let Some(line) = borrowed.lines.first_mut()
            {
                line.set_align(Some(Align::Center));
            }
            borrowed.shape_until_scroll(true);
        }
        let color = Color::rgb(
            ((paint.color >> 16) & 0xff) as u8,
            ((paint.color >> 8) & 0xff) as u8,
            (paint.color & 0xff) as u8,
        );
        buffer.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            color,
            |x, y, glyph_width, glyph_height, glyph_color| {
                let argb = glyph_color.0;
                for row in 0..glyph_height as i32 {
                    let target_y = paint.y + y + row;
                    if target_y < 0 || target_y >= height as i32 {
                        continue;
                    }
                    for column in 0..glyph_width as i32 {
                        let target_x = paint.x + x + column;
                        if target_x < 0 || target_x >= width as i32 {
                            continue;
                        }
                        let index = target_y as usize * stride + target_x as usize;
                        pixels[index] = blend_xrgb(pixels[index], argb);
                    }
                }
            },
        );
    }
}

impl Default for UiTextRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(missing_debug_implementations)]
pub struct SystemClipboard {
    context: Option<ClipboardContext>,
}

impl SystemClipboard {
    pub fn new() -> Self {
        Self {
            context: ClipboardContext::new().ok(),
        }
    }

    pub fn get(&mut self) -> Result<String, &'static str> {
        self.context
            .as_mut()
            .ok_or("clipboard unavailable")?
            .get_contents()
            .map_err(|_| "clipboard read failed")
    }

    pub fn set(&mut self, text: String) -> Result<(), &'static str> {
        self.context
            .as_mut()
            .ok_or("clipboard unavailable")?
            .set_contents(text)
            .map_err(|_| "clipboard write failed")
    }
}

impl Default for SystemClipboard {
    fn default() -> Self {
        Self::new()
    }
}

fn fill_rect(pixels: &mut [u32], stride: usize, width: u32, height: u32, rect: RectI, color: u32) {
    let left = rect.x.max(0).min(width as i32) as usize;
    let top = rect.y.max(0).min(height as i32) as usize;
    let right = rect.right().max(0).min(width as i32) as usize;
    let bottom = rect.bottom().max(0).min(height as i32) as usize;
    for row in top..bottom {
        pixels[row * stride + left..row * stride + right].fill(color);
    }
}

fn blend_xrgb(destination: u32, source_argb: u32) -> u32 {
    let alpha = (source_argb >> 24) & 0xff;
    let inverse = 255 - alpha;
    let channel = |shift: u32| {
        let source = (source_argb >> shift) & 0xff;
        let destination = (destination >> shift) & 0xff;
        (source * alpha + destination * inverse + 127) / 255
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_surface_is_opaque_and_nonempty() {
        let mut renderer = UiTextRenderer::new();
        let surface = renderer.render(
            [1, 2, 3, 4],
            180,
            40,
            0x00ff_ffff,
            &[],
            &[TextPaint {
                text: "Search".to_owned(),
                x: 4,
                y: 4,
                max_width: 160,
                size: 15.0,
                color: 0,
                bold: false,
                centered: false,
            }],
        );
        assert!(
            surface
                .pixels
                .pixels
                .iter()
                .any(|pixel| *pixel != 0x00ff_ffff)
        );
    }
}
