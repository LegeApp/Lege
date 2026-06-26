use freya_render_api::{
    Affine2D, Brush, ClipShape, Color, FillRule, GlyphBitmapRef, GlyphMaskRef, ImageDrawOptions,
    ImageId, PaintStyle, ParagraphLayout, PathData, PathVerb, Point, RRect, Rect, RenderCommands,
    StrokeStyle, SvgDrawOptions, SvgId, TextRasterTarget,
};
use tiny_skia::{
    FillRule as TinyFillRule, LineCap as TinyLineCap, LineJoin as TinyLineJoin, Mask, Paint, Path,
    PathBuilder, Pixmap, Rect as TinyRect, Stroke, Transform,
};

#[derive(Debug)]
pub enum RendererError {
    InvalidSize,
}

#[derive(Clone)]
struct RenderState {
    transform: Transform,
    clip: Option<Mask>,
}

impl Default for RenderState {
    fn default() -> Self {
        Self {
            transform: Transform::identity(),
            clip: None,
        }
    }
}

pub struct TinySkiaRenderer {
    pixmap: Pixmap,
    states: Vec<RenderState>,
}

impl TinySkiaRenderer {
    pub fn new(width: u32, height: u32) -> Result<Self, RendererError> {
        let pixmap = Pixmap::new(width, height).ok_or(RendererError::InvalidSize)?;

        Ok(Self {
            pixmap,
            states: vec![RenderState::default()],
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), RendererError> {
        self.pixmap = Pixmap::new(width, height).ok_or(RendererError::InvalidSize)?;
        self.states.clear();
        self.states.push(RenderState::default());
        Ok(())
    }

    pub fn pixmap(&self) -> &Pixmap {
        &self.pixmap
    }

    pub fn pixmap_mut(&mut self) -> &mut Pixmap {
        &mut self.pixmap
    }

    pub fn write_softbuffer_rgb(&self, target: &mut [u32]) {
        for (dst, src) in target.iter_mut().zip(self.pixmap.pixels()) {
            let alpha = src.alpha() as u32;
            let red = unpremultiply(src.red() as u32, alpha);
            let green = unpremultiply(src.green() as u32, alpha);
            let blue = unpremultiply(src.blue() as u32, alpha);

            *dst = (red << 16) | (green << 8) | blue;
        }
    }

    fn state(&self) -> &RenderState {
        self.states.last().expect("renderer always has one state")
    }

    fn state_mut(&mut self) -> &mut RenderState {
        self.states
            .last_mut()
            .expect("renderer always has one state")
    }

    fn tiny_path(path: &PathData) -> Option<Path> {
        let mut builder = PathBuilder::new();

        for verb in path.verbs.iter() {
            match *verb {
                PathVerb::MoveTo(point) => builder.move_to(point.x, point.y),
                PathVerb::LineTo(point) => builder.line_to(point.x, point.y),
                PathVerb::QuadTo(control, point) => {
                    builder.quad_to(control.x, control.y, point.x, point.y)
                }
                PathVerb::CubicTo(c1, c2, point) => {
                    builder.cubic_to(c1.x, c1.y, c2.x, c2.y, point.x, point.y)
                }
                PathVerb::Close => builder.close(),
            }
        }

        builder.finish()
    }

    fn tiny_rrect(rrect: RRect) -> Option<Path> {
        let mut builder = PathBuilder::new();
        let rect = rrect.rect;
        if rect.is_empty() {
            return None;
        }

        let left = rect.left;
        let top = rect.top;
        let right = rect.right;
        let bottom = rect.bottom;
        let tl = rrect.tl.x.min(rect.width() / 2.0).max(0.0);
        let tr = rrect.tr.x.min(rect.width() / 2.0).max(0.0);
        let br = rrect.br.x.min(rect.width() / 2.0).max(0.0);
        let bl = rrect.bl.x.min(rect.width() / 2.0).max(0.0);

        builder.move_to(left + tl, top);
        builder.line_to(right - tr, top);
        builder.quad_to(right, top, right, top + tr);
        builder.line_to(right, bottom - br);
        builder.quad_to(right, bottom, right - br, bottom);
        builder.line_to(left + bl, bottom);
        builder.quad_to(left, bottom, left, bottom - bl);
        builder.line_to(left, top + tl);
        builder.quad_to(left, top, left + tl, top);
        builder.close();
        builder.finish()
    }

    fn tiny_paint(paint: &PaintStyle) -> Paint<'static> {
        let mut tiny_paint = Paint {
            anti_alias: paint.anti_alias,
            ..Paint::default()
        };

        match paint.brush {
            Brush::Solid(color) => {
                tiny_paint.set_color_rgba8(color.r, color.g, color.b, color.a);
            }
        }

        tiny_paint
    }

    /// Render a drop shadow by:
    /// 1. Creating a temporary pixmap
    /// 2. Filling the shape path with the shadow color
    /// 3. Applying a box blur
    /// 4. Compositing onto the main pixmap at the shadow offset
    pub fn fill_shadow(
        &mut self,
        path: &Path,
        shadow_color: Color,
        blur_radius: f32,
        offset_x: f32,
        offset_y: f32,
        spread: f32,
    ) {
        let blur_radius = blur_radius.max(0.0);
        if blur_radius == 0.0 && spread == 0.0 && offset_x == 0.0 && offset_y == 0.0 {
            return;
        }

        let blur_int = blur_radius.ceil() as u32;

        // Determine the shadow bounding box
        let path_bounds = path.bounds();
        let shadow_left = (path_bounds.left() + offset_x - spread - blur_radius)
            .floor()
            .max(0.0) as u32;
        let shadow_top = (path_bounds.top() + offset_y - spread - blur_radius)
            .floor()
            .max(0.0) as u32;
        let shadow_right = (path_bounds.right() + offset_x + spread + blur_radius)
            .ceil()
            .min(self.pixmap.width() as f32) as u32;
        let shadow_bottom = (path_bounds.bottom() + offset_y + spread + blur_radius)
            .ceil()
            .min(self.pixmap.height() as f32) as u32;

        if shadow_right <= shadow_left || shadow_bottom <= shadow_top {
            return;
        }

        let temp_w = shadow_right - shadow_left;
        let temp_h = shadow_bottom - shadow_top;

        let Some(mut shadow_pixmap) = Pixmap::new(temp_w, temp_h) else {
            return;
        };

        // Fill the shadow path on the temp pixmap
        let offset_transform = Transform::from_translate(
            -(shadow_left as f32) + offset_x,
            -(shadow_top as f32) + offset_y,
        );
        let shadow_transform = self.state().transform.pre_concat(offset_transform);

        shadow_pixmap.fill_path(
            path,
            &{
                let mut p = Paint::default();
                p.set_color_rgba8(
                    shadow_color.r,
                    shadow_color.g,
                    shadow_color.b,
                    shadow_color.a,
                );
                p.anti_alias = true;
                p
            },
            TinyFillRule::Winding,
            shadow_transform,
            None,
        );

        // Apply box blur
        if blur_int > 0 {
            Self::box_blur(&mut shadow_pixmap, blur_int);
        }

        // Composite shadow onto main pixmap
        let state = self.state().clone();
        self.pixmap.draw_pixmap(
            shadow_left as i32,
            shadow_top as i32,
            shadow_pixmap.as_ref(),
            &tiny_skia::PixmapPaint {
                opacity: 1.0,
                blend_mode: tiny_skia::BlendMode::SourceOver,
                quality: tiny_skia::FilterQuality::Nearest,
            },
            state.transform,
            state.clip.as_ref(),
        );
    }

    /// Fast separable box blur (3-pass approximation of Gaussian)
    fn box_blur(pixmap: &mut Pixmap, radius: u32) {
        if radius == 0 {
            return;
        }
        let w = pixmap.width();
        let h = pixmap.height();
        if w == 0 || h == 0 {
            return;
        }

        let r = radius as usize;
        let size = (w * h) as usize;
        let mut temp_alpha: Vec<u16> = vec![0; size];

        // Horizontal pass
        for y in 0..h as usize {
            let row_start = y * w as usize;
            let mut sum: u32 = 0;
            let mut count: u32 = 0;

            for x in 0..(w as usize + r) {
                let idx = if x < w as usize {
                    row_start + x
                } else {
                    row_start + w as usize - 1
                };
                if idx < size {
                    let src = pixmap.pixels()[idx];
                    sum += src.alpha() as u32;
                    count += 1;
                }

                if x > r * 2 {
                    let remove_idx = row_start + (x - r * 2 - 1).min(w as usize - 1);
                    if remove_idx < size {
                        let src = pixmap.pixels()[remove_idx];
                        sum = sum.saturating_sub(src.alpha() as u32);
                        count = count.saturating_sub(1);
                    }
                }

                if x >= r {
                    let write_idx = row_start + (x - r).min(w as usize - 1);
                    if write_idx < size && count > 0 {
                        temp_alpha[write_idx] = (sum / count).min(255) as u16;
                    }
                }
            }
        }

        // Vertical pass + write back
        for x in 0..w as usize {
            let mut sum: u32 = 0;
            let mut count: u32 = 0;

            for y in 0..(h as usize + r) {
                let idx = if y < h as usize {
                    y * w as usize + x
                } else {
                    (h as usize - 1) * w as usize + x
                };
                if idx < size {
                    sum += temp_alpha[idx] as u32;
                    count += 1;
                }

                if y > r * 2 {
                    let remove_idx = (y - r * 2 - 1).min(h as usize - 1) * w as usize + x;
                    if remove_idx < size {
                        sum = sum.saturating_sub(temp_alpha[remove_idx] as u32);
                        count = count.saturating_sub(1);
                    }
                }

                if y >= r {
                    let write_idx = (y - r).min(h as usize - 1) * w as usize + x;
                    if write_idx < size && count > 0 {
                        let alpha = (sum / count).min(255) as u8;
                        let pixel = pixmap.pixels()[write_idx];
                        let old_r = pixel.red();
                        let old_g = pixel.green();
                        let old_b = pixel.blue();
                        // Re-premultiply RGB with the new (blurred) alpha
                        let new_r = (old_r as u32 * alpha as u32 / 255).min(255) as u8;
                        let new_g = (old_g as u32 * alpha as u32 / 255).min(255) as u8;
                        let new_b = (old_b as u32 * alpha as u32 / 255).min(255) as u8;
                        if let Some(color) =
                            tiny_skia::PremultipliedColorU8::from_rgba(new_r, new_g, new_b, alpha)
                        {
                            pixmap.pixels_mut()[write_idx] = color;
                        }
                    }
                }
            }
        }
    }
}

impl RenderCommands for TinySkiaRenderer {
    fn save(&mut self) -> usize {
        let token = self.states.len();
        self.states.push(self.state().clone());
        token
    }

    fn restore_to(&mut self, token: usize) {
        while self.states.len() > token {
            self.states.pop();
        }
        if self.states.is_empty() {
            self.states.push(RenderState::default());
        }
    }

    fn clear(&mut self, color: Color) {
        self.pixmap.fill(tiny_skia::Color::from_rgba8(
            color.r, color.g, color.b, color.a,
        ));
    }

    fn transform(&mut self, affine: Affine2D) {
        self.state_mut().transform = self.state().transform.pre_concat(Transform::from_row(
            affine.sx, affine.ky, affine.kx, affine.sy, affine.tx, affine.ty,
        ));
    }

    fn clip(&mut self, shape: &ClipShape, anti_alias: bool) {
        let Some(path) = clip_to_path(shape) else {
            return;
        };

        let Some(mut next_mask) = Mask::new(self.pixmap.width(), self.pixmap.height()) else {
            return;
        };
        next_mask.fill_path(
            &path,
            TinyFillRule::Winding,
            anti_alias,
            self.state().transform,
        );

        self.state_mut().clip = Some(next_mask);
    }

    fn push_opacity_layer(&mut self, _bounds: Rect, _opacity: f32) {}

    fn fill_path(&mut self, path: &PathData, fill_rule: FillRule, paint: &PaintStyle) {
        let Some(path) = Self::tiny_path(path) else {
            return;
        };
        let tiny_rule = match fill_rule {
            FillRule::NonZero => TinyFillRule::Winding,
            FillRule::EvenOdd => TinyFillRule::EvenOdd,
        };
        let state = self.state().clone();

        self.pixmap.fill_path(
            &path,
            &Self::tiny_paint(paint),
            tiny_rule,
            state.transform,
            state.clip.as_ref(),
        );
    }

    fn stroke_path(&mut self, path: &PathData, stroke: &StrokeStyle, paint: &PaintStyle) {
        let Some(path) = Self::tiny_path(path) else {
            return;
        };
        let tiny_stroke = Stroke {
            width: stroke.width,
            miter_limit: stroke.miter_limit,
            line_cap: match stroke.line_cap {
                freya_render_api::LineCap::Butt => TinyLineCap::Butt,
                freya_render_api::LineCap::Round => TinyLineCap::Round,
                freya_render_api::LineCap::Square => TinyLineCap::Square,
            },
            line_join: match stroke.line_join {
                freya_render_api::LineJoin::Miter => TinyLineJoin::Miter,
                freya_render_api::LineJoin::Round => TinyLineJoin::Round,
                freya_render_api::LineJoin::Bevel => TinyLineJoin::Bevel,
            },
            dash: stroke
                .dash
                .as_ref()
                .map(|dash| tiny_skia::StrokeDash::new(dash.clone(), 0.0))
                .unwrap_or(None),
        };
        let state = self.state().clone();

        self.pixmap.stroke_path(
            &path,
            &Self::tiny_paint(paint),
            &tiny_stroke,
            state.transform,
            state.clip.as_ref(),
        );
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) {
        let Some(rect) = tiny_rect(rect) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.r, color.g, color.b, color.a);
        let state = self.state().clone();

        self.pixmap
            .fill_rect(rect, &paint, state.transform, state.clip.as_ref());
    }

    fn fill_drop_shadow(
        &mut self,
        path: &PathData,
        color: Color,
        blur_radius: f32,
        offset_x: f32,
        offset_y: f32,
        spread: f32,
    ) {
        let Some(tiny_path) = Self::tiny_path(path) else {
            return;
        };
        self.fill_shadow(&tiny_path, color, blur_radius, offset_x, offset_y, spread);
    }

    fn draw_image(&mut self, _image: ImageId, _dest: Rect, _options: &ImageDrawOptions) {}

    fn draw_svg(&mut self, _svg: SvgId, _dest: Rect, _options: &SvgDrawOptions) {}

    fn draw_paragraph(&mut self, paragraph: &dyn ParagraphLayout, origin: Point) {
        paragraph.draw(self, origin);
    }
}

impl TextRasterTarget for TinySkiaRenderer {
    fn fill_rect(&mut self, rect: Rect, color: Color) {
        let Some(rect) = tiny_rect(rect) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.r, color.g, color.b, color.a);
        let state = self.state().clone();

        self.pixmap
            .fill_rect(rect, &paint, state.transform, state.clip.as_ref());
    }

    fn draw_alpha_mask(&mut self, mask: GlyphMaskRef<'_>, origin: Point, color: Color) {
        let w = mask.width;
        let h = mask.height;
        if w == 0 || h == 0 {
            return;
        }
        // Skip glyphs fully outside the pixmap to avoid a per-glyph heap allocation.
        let px = origin.x as i32;
        let py = origin.y as i32;
        if px + w as i32 <= 0
            || py + h as i32 <= 0
            || px >= self.pixmap.width() as i32
            || py >= self.pixmap.height() as i32
        {
            return;
        }
        let Some(mut temp) = Pixmap::new(w, h) else {
            return;
        };
        let pixels = temp.pixels_mut();
        for y in 0..h {
            for x in 0..w {
                let alpha = mask.data[(y * w + x) as usize] as u32;
                if alpha == 0 {
                    continue;
                }
                let out_a = (color.a as u32 * alpha) / 255;
                if out_a == 0 {
                    continue;
                }
                if let Some(pixel) = tiny_skia::PremultipliedColorU8::from_rgba(
                    (color.r as u32 * out_a / 255) as u8,
                    (color.g as u32 * out_a / 255) as u8,
                    (color.b as u32 * out_a / 255) as u8,
                    out_a as u8,
                ) {
                    pixels[(y * w + x) as usize] = pixel;
                }
            }
        }
        let state = self.state().clone();
        self.pixmap.draw_pixmap(
            origin.x as i32,
            origin.y as i32,
            temp.as_ref(),
            &tiny_skia::PixmapPaint {
                opacity: 1.0,
                blend_mode: tiny_skia::BlendMode::SourceOver,
                quality: tiny_skia::FilterQuality::Nearest,
            },
            state.transform,
            state.clip.as_ref(),
        );
    }

    fn draw_color_bitmap(&mut self, bitmap: GlyphBitmapRef<'_>, origin: Point) {
        let w = bitmap.width;
        let h = bitmap.height;
        if w == 0 || h == 0 {
            return;
        }
        // Skip glyphs fully outside the pixmap to avoid a per-glyph heap allocation.
        let px = origin.x as i32;
        let py = origin.y as i32;
        if px + w as i32 <= 0
            || py + h as i32 <= 0
            || px >= self.pixmap.width() as i32
            || py >= self.pixmap.height() as i32
        {
            return;
        }
        let Some(mut temp) = Pixmap::new(w, h) else {
            return;
        };
        let pixels = temp.pixels_mut();
        for y in 0..h {
            for x in 0..w {
                let src_idx = ((y * w + x) * 4) as usize;
                let r = bitmap.data[src_idx];
                let g = bitmap.data[src_idx + 1];
                let b = bitmap.data[src_idx + 2];
                let a = bitmap.data[src_idx + 3];
                if a == 0 {
                    continue;
                }
                if let Some(pixel) = tiny_skia::PremultipliedColorU8::from_rgba(
                    (r as u32 * a as u32 / 255) as u8,
                    (g as u32 * a as u32 / 255) as u8,
                    (b as u32 * a as u32 / 255) as u8,
                    a,
                ) {
                    pixels[(y * w + x) as usize] = pixel;
                }
            }
        }
        let state = self.state().clone();
        self.pixmap.draw_pixmap(
            origin.x as i32,
            origin.y as i32,
            temp.as_ref(),
            &tiny_skia::PixmapPaint {
                opacity: 1.0,
                blend_mode: tiny_skia::BlendMode::SourceOver,
                quality: tiny_skia::FilterQuality::Nearest,
            },
            state.transform,
            state.clip.as_ref(),
        );
    }
}

fn clip_to_path(shape: &ClipShape) -> Option<Path> {
    match shape {
        ClipShape::Rect(rect) => tiny_rect(*rect).map(PathBuilder::from_rect),
        ClipShape::RRect(rrect) => TinySkiaRenderer::tiny_rrect(*rrect),
        ClipShape::Path(path) => TinySkiaRenderer::tiny_path(path),
    }
}

fn tiny_rect(rect: Rect) -> Option<TinyRect> {
    TinyRect::from_ltrb(rect.left, rect.top, rect.right, rect.bottom)
}

fn unpremultiply(component: u32, alpha: u32) -> u32 {
    if alpha == 0 {
        0
    } else {
        ((component * 255) / alpha).min(255)
    }
}

pub mod prelude {
    pub use crate::{RendererError, TinySkiaRenderer};
}
