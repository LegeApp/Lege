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

/// Register one of pdf-font's embedded base-14 programs and report the family
/// name it actually landed under.
///
/// The name is discovered rather than assumed. This used to be hardcoded as
/// `"FoxitSans"`, matching the *file* name of the program pdf-font ships
/// (`fonts/FoxitSans.otf`), but the face inside it declares its family as
/// `"Chrom Sans OTF"`. `fontdb` matches on the declared family, so the query
/// never resolved, and every label in the viewer was silently drawn with
/// whatever system font the fallback chain happened to reach — a different
/// typeface on every machine, and nothing at all on a host with no fonts
/// installed. Reading the name back from the database keeps that from
/// recurring the next time the bundled program is swapped.
///
/// `fontdb::Database::load_font_data` returns no id, so the new face is found
/// by diffing the id set around the load.
fn register_embedded_family(font_system: &mut FontSystem, font: StandardFont) -> Option<String> {
    let before: std::collections::HashSet<_> =
        font_system.db().faces().map(|face| face.id).collect();
    font_system
        .db_mut()
        .load_font_data(font.program_data().to_vec());
    font_system
        .db()
        .faces()
        .find(|face| !before.contains(&face.id))
        .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
}

#[allow(missing_debug_implementations)]
pub struct UiTextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    /// Family name of the embedded regular face, when it registered.
    regular_family: Option<String>,
    /// Family name of the embedded bold face. The bundled bold program is a
    /// *separate family* (`"Chrom Sans OTF Bold"`, subfamily `Regular`) rather
    /// than a heavy weight within the regular family, so asking for
    /// `Weight::BOLD` on the regular family would never reach it.
    bold_family: Option<String>,
    revision: u64,
    surfaces: HashMap<[u64; 4], (u64, Arc<SceneSurface>)>,
}

impl UiTextRenderer {
    pub fn new() -> Self {
        // `FontSystem::new()` scans the system's installed fonts, which costs
        // roughly 60ms once at startup. That scan is load-bearing and must not
        // be traded away for an embedded-only database: the fallback lists
        // `PlatformFallback` supplies are *family names* ("Noto Sans CJK JP",
        // "DejaVu Sans", "Noto Sans Symbols"…) that are resolved against this
        // same database. The viewer draws PDF outline titles (`app.rs`, from
        // the document's own bookmarks) and the search field's IME preedit
        // through this renderer, so it routinely receives CJK, Arabic,
        // Cyrillic and other scripts the embedded Latin face has no glyphs
        // for. With no system faces to fall back to, every one of those
        // renders as tofu.
        let mut font_system = FontSystem::new();
        let regular_family = register_embedded_family(&mut font_system, StandardFont::Helvetica);
        let bold_family = register_embedded_family(&mut font_system, StandardFont::HelveticaBold);
        Self {
            font_system,
            swash_cache: SwashCache::new(),
            regular_family,
            bold_family,
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
        // Pick the embedded face for the requested weight, and fall back to
        // the generic sans-serif family only when a program failed to
        // register. The weight is still requested either way: if the bold
        // program is missing, it lets the fallback chain pick a heavier face
        // instead of silently rendering bold labels at regular weight.
        let family = if paint.bold {
            self.bold_family
                .as_deref()
                .or(self.regular_family.as_deref())
        } else {
            self.regular_family.as_deref()
        };
        let mut buffer = Buffer::new(&mut self.font_system, Metrics::new(paint.size, line_height));
        {
            let mut borrowed = buffer.borrow_with(&mut self.font_system);
            borrowed.set_size(Some(paint.max_width as f32), Some(line_height));
            let attrs = Attrs::new()
                .family(match family {
                    Some(name) => Family::Name(name),
                    None => Family::SansSerif,
                })
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
    #![allow(clippy::expect_used)]

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

    /// The embedded programs must actually resolve. This is the regression
    /// that hid for as long as the family was hardcoded to the file name:
    /// `Family::Name("FoxitSans")` matched nothing, so the bundled face was
    /// loaded and then never used.
    #[test]
    fn embedded_faces_register_and_are_resolvable() {
        let renderer = UiTextRenderer::new();
        let regular = renderer
            .regular_family
            .clone()
            .expect("the embedded regular program must register");
        let bold = renderer
            .bold_family
            .clone()
            .expect("the embedded bold program must register");
        assert_ne!(regular, bold, "bold must be its own family, not a weight");
        for family in [&regular, &bold] {
            let query = cosmic_text::fontdb::Query {
                families: &[cosmic_text::fontdb::Family::Name(family)],
                ..Default::default()
            };
            assert!(
                renderer.font_system.db().query(&query).is_some(),
                "family {family:?} does not resolve in the database"
            );
        }
    }

    /// System fonts are the fallback source for everything the embedded Latin
    /// face cannot cover. PDF outline titles and the search field's IME
    /// preedit both arrive here as arbitrary Unicode, so the database must
    /// carry more than the two bundled faces.
    #[test]
    fn system_faces_remain_available_for_fallback() {
        let renderer = UiTextRenderer::new();
        assert!(
            renderer.font_system.db().len() > 2,
            "only the embedded faces are present: non-Latin text would render \
             as tofu. FontSystem::new()'s system scan must not be removed."
        );
    }

    /// Non-Latin labels must still put ink on the canvas. On a host with no
    /// fonts installed at all there is nothing to fall back to, so this
    /// asserts only when the database actually has system faces.
    #[test]
    fn non_latin_labels_render_through_fallback() {
        let mut renderer = UiTextRenderer::new();
        if renderer.font_system.db().len() <= 2 {
            return;
        }
        for (index, sample) in [
            "\u{7b2c}\u{4e00}\u{7ae0}",
            "\u{413}\u{43b}\u{430}\u{432}\u{430}",
        ]
        .into_iter()
        .enumerate()
        {
            let surface = renderer.render(
                [9, index as u64, 0, 0],
                240,
                40,
                0x00ff_ffff,
                &[],
                &[TextPaint {
                    text: sample.to_owned(),
                    x: 4,
                    y: 4,
                    max_width: 220,
                    size: 16.0,
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
                    .any(|pixel| *pixel != 0x00ff_ffff),
                "{sample:?} drew nothing: script fallback is broken"
            );
        }
    }
}
