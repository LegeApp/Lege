use std::{
    ops::Range,
    sync::{Mutex, OnceLock},
};

use cosmic_text::{
    Align, Attrs, Buffer, Color as CosmicColor, Family, FontSystem, Metrics, Shaping, SwashCache,
    SwashContent, Weight,
};
use freya_render_api::{
    Color, GlyphBitmapRef, GlyphMaskRef, ParagraphLayout, Point, Rect, Size, TextEngine,
    TextPosition, TextRasterTarget,
};

static GLOBAL_FONT_SYSTEM: OnceLock<Mutex<FontSystem>> = OnceLock::new();
static GLOBAL_SWASH_CACHE: OnceLock<Mutex<SwashCache>> = OnceLock::new();

fn global_font_system() -> &'static Mutex<FontSystem> {
    GLOBAL_FONT_SYSTEM.get_or_init(|| Mutex::new(FontSystem::new()))
}

fn global_swash_cache() -> &'static Mutex<SwashCache> {
    GLOBAL_SWASH_CACHE.get_or_init(|| Mutex::new(SwashCache::new()))
}

pub struct CosmicTextEngine {
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl CosmicTextEngine {
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
        }
    }

    pub fn font_system(&mut self) -> &mut FontSystem {
        &mut self.font_system
    }

    pub fn swash_cache(&mut self) -> &mut SwashCache {
        &mut self.swash_cache
    }
}

impl Default for CosmicTextEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine for CosmicTextEngine {}

pub struct CosmicParagraph {
    inner: Mutex<CosmicParagraphInner>,
    size: Size,
}

struct CosmicParagraphInner {
    buffer: Buffer,
    color: Color,
}

fn to_cosmic_align(text_align: freya_render_api::TextAlign) -> Align {
    match text_align {
        freya_render_api::TextAlign::Left => Align::Left,
        freya_render_api::TextAlign::Right => Align::Right,
        freya_render_api::TextAlign::Center => Align::Center,
        freya_render_api::TextAlign::Justify => Align::Justified,
    }
}

fn family_from_families(families: &[String]) -> Family<'_> {
    families
        .first()
        .map(|f| Family::Name(f.as_str()))
        .unwrap_or(Family::SansSerif)
}

impl CosmicParagraph {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        text: &str,
        width: f32,
        font_size: f32,
        line_height: f32,
        color: Color,
        font_weight: f32,
        font_families: &[String],
        max_lines: Option<usize>,
        text_align: freya_render_api::TextAlign,
    ) -> Self {
        let mut font_system = global_font_system().lock().expect("font system lock");
        let metrics = Metrics::new(font_size, line_height);
        let mut buffer = Buffer::new(&mut font_system, metrics);
        {
            let mut borrowed = buffer.borrow_with(&mut font_system);
            let max_h = max_lines.map(|ml| ml as f32 * line_height);
            borrowed.set_size(Some(width), max_h);

            let family = family_from_families(font_families);
            let attrs = Attrs::new()
                .weight(Weight(font_weight as u16))
                .family(family);

            let align = to_cosmic_align(text_align);
            borrowed.set_text(text, &attrs, Shaping::Advanced, Some(align));
            borrowed.shape_until_scroll(true);
        }

        let size = paragraph_size(&buffer, width);

        drop(font_system);

        Self {
            inner: Mutex::new(CosmicParagraphInner { buffer, color }),
            size,
        }
    }

    pub fn new_with_spans(
        spans: &[(String, Color, f32, f32, Vec<String>)],
        width: f32,
        base_font_size: f32,
        base_line_height: f32,
        base_color: Color,
        max_lines: Option<usize>,
        text_align: freya_render_api::TextAlign,
    ) -> Self {
        let mut font_system = global_font_system().lock().expect("font system lock");
        let metrics = Metrics::new(base_font_size, base_line_height);
        let mut buffer = Buffer::new(&mut font_system, metrics);
        {
            let mut borrowed = buffer.borrow_with(&mut font_system);
            let max_h = max_lines.map(|ml| ml as f32 * base_line_height);
            borrowed.set_size(Some(width), max_h);

            let align = to_cosmic_align(text_align);

            let cosmic_spans: Vec<(&str, Attrs)> = spans
                .iter()
                .map(|(text, color, _font_size, font_weight, font_families)| {
                    let family = family_from_families(font_families);
                    let attrs = Attrs::new()
                        .weight(Weight(*font_weight as u16))
                        .family(family)
                        .color(cosmic_text::Color::rgba(color.r, color.g, color.b, color.a));
                    (text.as_str(), attrs)
                })
                .collect();

            borrowed.set_rich_text(cosmic_spans, &Attrs::new(), Shaping::Advanced, Some(align));
            borrowed.shape_until_scroll(true);
        }

        let size = paragraph_size(&buffer, width);

        drop(font_system);

        Self {
            inner: Mutex::new(CosmicParagraphInner {
                buffer,
                color: base_color,
            }),
            size,
        }
    }
}

impl ParagraphLayout for CosmicParagraph {
    fn size(&self) -> Size {
        self.size
    }

    fn hit_test_point(&self, point: Point) -> TextPosition {
        let inner = self.inner.lock().expect("cosmic paragraph lock poisoned");
        let byte_index = inner
            .buffer
            .hit(point.x, point.y)
            .map(|cursor| cursor.index)
            .unwrap_or_default();

        TextPosition { byte_index }
    }

    fn rects_for_range(&self, range: Range<usize>) -> Vec<Rect> {
        let inner = self.inner.lock().expect("cosmic paragraph lock poisoned");
        let mut rects = Vec::new();

        for run in inner.buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                let start = glyph.start;
                let end = glyph.end;

                if start < range.end && end > range.start {
                    rects.push(Rect::new(
                        glyph.x,
                        run.line_top,
                        glyph.x + glyph.w,
                        run.line_top + run.line_height,
                    ));
                }
            }
        }

        rects
    }

    fn cursor_rect(&self, position: TextPosition) -> Rect {
        let inner = self.inner.lock().expect("cosmic paragraph lock poisoned");
        let buffer = &inner.buffer;

        // Find cursor position using cosmic-text's hit-testing for cursor position
        if let Some(cursor) = buffer.hit(position.byte_index as f32, 0.0)
            && let Some((x, _y)) = buffer.cursor_position(&cursor)
        {
            let line = cursor.line;
            let y = buffer
                .layout_runs()
                .nth(line)
                .map(|run| run.line_top)
                .unwrap_or(0.0);
            let line_height = buffer
                .layout_runs()
                .nth(line)
                .map(|run| run.line_height)
                .unwrap_or(buffer.metrics().font_size);

            return Rect::new(x, y, x + 1.0, y + line_height);
        }

        let x = position.byte_index as f32 * buffer.metrics().font_size * 0.5;
        let height = self.size.height.max(buffer.metrics().font_size);
        Rect::new(x, 0.0, x + 1.0, height)
    }

    fn draw(&self, renderer: &mut dyn TextRasterTarget, origin: Point) {
        let mut inner = self.inner.lock().expect("cosmic paragraph lock poisoned");
        let mut font_system = global_font_system().lock().expect("font system lock");
        let mut swash_cache = global_swash_cache().lock().expect("swash cache lock");
        let color = CosmicColor::rgba(inner.color.r, inner.color.g, inner.color.b, inner.color.a);

        inner.buffer.shape_until_scroll(&mut font_system, false);

        for run in inner.buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                let physical_glyph = glyph.physical((0., run.line_y), 1.0);
                let glyph_color = glyph.color_opt.map_or(color, |c| c);

                if let Some(image) =
                    swash_cache.get_image(&mut font_system, physical_glyph.cache_key)
                {
                    let x = physical_glyph.x as f32 + image.placement.left as f32;
                    let y = physical_glyph.y as f32 - image.placement.top as f32;

                    match image.content {
                        SwashContent::Mask => {
                            renderer.draw_alpha_mask(
                                GlyphMaskRef {
                                    width: image.placement.width,
                                    height: image.placement.height,
                                    data: &image.data,
                                },
                                Point::new(origin.x + x, origin.y + y),
                                Color::rgba(
                                    glyph_color.r(),
                                    glyph_color.g(),
                                    glyph_color.b(),
                                    glyph_color.a(),
                                ),
                            );
                        }
                        SwashContent::Color | SwashContent::SubpixelMask => {
                            renderer.draw_color_bitmap(
                                GlyphBitmapRef {
                                    width: image.placement.width,
                                    height: image.placement.height,
                                    data: &image.data,
                                },
                                Point::new(origin.x + x, origin.y + y),
                            );
                        }
                    }
                }
            }

            cosmic_text::render_decoration(
                &mut CosmicDecorationRenderer {
                    target: &mut *renderer,
                    origin,
                },
                &run,
                color,
            );
        }
    }
}

fn paragraph_size(buffer: &Buffer, fallback_width: f32) -> Size {
    let mut width: f32 = 0.0;
    let mut height: f32 = 0.0;

    for run in buffer.layout_runs() {
        height = height.max(run.line_top + run.line_height);
        for glyph in run.glyphs.iter() {
            width = width.max(glyph.x + glyph.w);
        }
    }

    Size::new(width.min(fallback_width), height)
}

pub mod prelude {
    pub use crate::{CosmicParagraph, CosmicTextEngine};
}

struct CosmicDecorationRenderer<'a> {
    target: &'a mut dyn TextRasterTarget,
    origin: Point,
}

impl cosmic_text::Renderer for CosmicDecorationRenderer<'_> {
    fn rectangle(&mut self, x: i32, y: i32, w: u32, h: u32, color: CosmicColor) {
        self.target.fill_rect(
            Rect::new(
                self.origin.x + x as f32,
                self.origin.y + y as f32,
                self.origin.x + x as f32 + w as f32,
                self.origin.y + y as f32 + h as f32,
            ),
            Color::rgba(color.r(), color.g(), color.b(), color.a()),
        );
    }

    fn glyph(&mut self, _physical_glyph: cosmic_text::PhysicalGlyph, _color: CosmicColor) {}
}
