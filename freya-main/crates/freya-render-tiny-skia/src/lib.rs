use std::sync::Arc;

use freya_render_api::{
    Affine2D, Brush, ClipShape, Color, FillRule, GlyphBitmapRef, GlyphMaskRef, ImageDrawOptions,
    ImageId, PaintStyle, ParagraphLayout, PathData, PathVerb, Point, RRect, Rect, RenderCommands,
    StrokeStyle, SvgDrawOptions, SvgId, TextRasterTarget,
};
use tiny_skia::{
    FillRule as TinyFillRule, LineCap as TinyLineCap, LineJoin as TinyLineJoin, Mask, Paint, Path,
    PathBuilder, Pixmap, PremultipliedColorU8, Rect as TinyRect, Stroke, Transform,
};

#[derive(Debug)]
pub enum RendererError {
    InvalidSize,
}

/// The clip mask is behind an `Arc` because the render pipeline clones the
/// state on every `save()` and draw call; a plain `Mask` clone would memcpy a
/// window-sized buffer each time.
#[derive(Clone)]
struct RenderState {
    transform: Transform,
    clip: Option<Arc<Mask>>,
}

/// An offscreen group-opacity layer (the CPU analogue of Skia's
/// `save_layer_alpha`). Window-sized so clip masks and coordinates carry over
/// unchanged; composited onto the target below at `opacity` when the save
/// stack shrinks past `depth`.
struct OpacityLayer {
    pixmap: Pixmap,
    opacity: f32,
    /// `states.len()` at push time; composite once `states.len()` drops below it.
    depth: usize,
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
    layers: Vec<OpacityLayer>,
}

impl TinySkiaRenderer {
    pub fn new(width: u32, height: u32) -> Result<Self, RendererError> {
        let pixmap = Pixmap::new(width, height).ok_or(RendererError::InvalidSize)?;

        Ok(Self {
            pixmap,
            states: vec![RenderState::default()],
            layers: Vec::new(),
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), RendererError> {
        self.pixmap = Pixmap::new(width, height).ok_or(RendererError::InvalidSize)?;
        self.states.clear();
        self.states.push(RenderState::default());
        self.layers.clear();
        Ok(())
    }

    /// Prepares the renderer for a new frame: reallocates the pixmap only when
    /// the size changed and resets the save/clip stack, so the same renderer
    /// can be reused across frames without a per-frame allocation.
    pub fn begin_frame(&mut self, width: u32, height: u32) -> Result<(), RendererError> {
        if self.pixmap.width() != width || self.pixmap.height() != height {
            self.pixmap = Pixmap::new(width, height).ok_or(RendererError::InvalidSize)?;
        }
        self.states.clear();
        self.states.push(RenderState::default());
        self.layers.clear();
        Ok(())
    }

    /// The pixmap draws currently land in: the top opacity layer if one is
    /// open, else the base pixmap. Paired with the current state because the
    /// two are almost always needed together and this keeps the borrows
    /// disjoint.
    fn target_and_state(&mut self) -> (&mut Pixmap, &RenderState) {
        let Self {
            pixmap,
            states,
            layers,
        } = self;
        let target = layers
            .last_mut()
            .map(|layer| &mut layer.pixmap)
            .unwrap_or(pixmap);
        let state = states.last().expect("renderer always has one state");
        (target, state)
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
            // The frame is cleared with an opaque background, so nearly every
            // pixel takes this division-free path.
            *dst = if alpha == 255 {
                ((src.red() as u32) << 16) | ((src.green() as u32) << 8) | src.blue() as u32
            } else {
                let red = unpremultiply(src.red() as u32, alpha);
                let green = unpremultiply(src.green() as u32, alpha);
                let blue = unpremultiply(src.blue() as u32, alpha);
                (red << 16) | (green << 8) | blue
            };
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
        // Clamp against both half-extents so wide-flat (or tall-narrow) rects
        // with a large radius never produce a self-intersecting path.
        let max_radius = (rect.width() / 2.0).min(rect.height() / 2.0);
        let tl = rrect.tl.x.clamp(0.0, max_radius);
        let tr = rrect.tr.x.clamp(0.0, max_radius);
        let br = rrect.br.x.clamp(0.0, max_radius);
        let bl = rrect.bl.x.clamp(0.0, max_radius);

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

        // Work in device space: transform the path up front so the crop box,
        // the fill and the composite all agree with the current transform.
        let transform = self.state().transform;
        let transformed;
        let device_path = if transform.is_identity() {
            path
        } else {
            match path.clone().transform(transform) {
                Some(p) => {
                    transformed = p;
                    &transformed
                }
                None => return,
            }
        };

        // Determine the shadow bounding box
        let path_bounds = device_path.bounds();
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

        // Fill the shadow path on the temp pixmap. The path is already in
        // device space, so only the crop offset and the shadow offset apply.
        let offset_transform = Transform::from_translate(
            -(shadow_left as f32) + offset_x,
            -(shadow_top as f32) + offset_y,
        );

        shadow_pixmap.fill_path(
            device_path,
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
            offset_transform,
            None,
        );

        // Apply box blur
        if blur_int > 0 {
            Self::box_blur(&mut shadow_pixmap, blur_int);
        }

        // Composite shadow onto the target (identity: already device space)
        let (target, state) = self.target_and_state();
        target.draw_pixmap(
            shadow_left as i32,
            shadow_top as i32,
            shadow_pixmap.as_ref(),
            &tiny_skia::PixmapPaint {
                opacity: 1.0,
                blend_mode: tiny_skia::BlendMode::SourceOver,
                quality: tiny_skia::FilterQuality::Nearest,
            },
            Transform::identity(),
            state.clip.as_deref(),
        );
    }

    /// Fast separable box blur (one horizontal + one vertical pass)
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

impl TinySkiaRenderer {
    /// Direct SourceOver blit of premultiplied source pixels at an
    /// untransformed origin, honoring the active clip mask. This is the glyph
    /// hot path: it avoids the per-glyph `Pixmap` allocation and full
    /// compositor pass that `draw_pixmap` would incur.
    ///
    /// `src` returns premultiplied RGBA for local coordinates inside `w`×`h`.
    fn blit_premul(
        &mut self,
        origin: Point,
        w: u32,
        h: u32,
        src: impl Fn(u32, u32) -> (u8, u8, u8, u8),
    ) {
        let px = origin.x.floor() as i32;
        let py = origin.y.floor() as i32;
        let dst_w = self.pixmap.width() as i32;
        let dst_h = self.pixmap.height() as i32;
        let Self {
            pixmap,
            states,
            layers,
        } = self;
        let clip = states.last().and_then(|state| state.clip.as_deref());
        let mask_data = clip.map(|mask| mask.data());
        let target = layers
            .last_mut()
            .map(|layer| &mut layer.pixmap)
            .unwrap_or(pixmap);
        let pixels = target.pixels_mut();

        for y in 0..h as i32 {
            let dy = py + y;
            if dy < 0 || dy >= dst_h {
                continue;
            }
            for x in 0..w as i32 {
                let dx = px + x;
                if dx < 0 || dx >= dst_w {
                    continue;
                }
                let (mut r, mut g, mut b, mut a) = src(x as u32, y as u32);
                if a == 0 {
                    continue;
                }
                let di = (dy * dst_w + dx) as usize;
                if let Some(mask) = mask_data {
                    let m = mask[di] as u32;
                    if m == 0 {
                        continue;
                    }
                    if m < 255 {
                        r = ((r as u32 * m) / 255) as u8;
                        g = ((g as u32 * m) / 255) as u8;
                        b = ((b as u32 * m) / 255) as u8;
                        a = ((a as u32 * m) / 255) as u8;
                        if a == 0 {
                            continue;
                        }
                    }
                }
                let dst = pixels[di];
                let inv = 255 - a as u32;
                let nr = (r as u32 + (dst.red() as u32 * inv) / 255).min(255) as u8;
                let ng = (g as u32 + (dst.green() as u32 * inv) / 255).min(255) as u8;
                let nb = (b as u32 + (dst.blue() as u32 * inv) / 255).min(255) as u8;
                let na = (a as u32 + (dst.alpha() as u32 * inv) / 255).min(255) as u8;
                if let Some(pixel) = PremultipliedColorU8::from_rgba(nr, ng, nb, na) {
                    pixels[di] = pixel;
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
        // Composite (LIFO) any opacity layers opened inside the restored
        // scope. Compared against `token`, not `states.len()`, so a full
        // unwind (token 0, where the stack is refilled to one entry above)
        // still closes base-depth layers.
        while self.layers.last().is_some_and(|layer| layer.depth > token) {
            let layer = self.layers.pop().expect("checked non-empty");
            let target = self
                .layers
                .last_mut()
                .map(|below| &mut below.pixmap)
                .unwrap_or(&mut self.pixmap);
            target.draw_pixmap(
                0,
                0,
                layer.pixmap.as_ref(),
                &tiny_skia::PixmapPaint {
                    opacity: layer.opacity,
                    blend_mode: tiny_skia::BlendMode::SourceOver,
                    quality: tiny_skia::FilterQuality::Nearest,
                },
                Transform::identity(),
                None,
            );
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

        let transform = self.state().transform;
        // Nested clips must compose: intersect the new shape with the
        // inherited mask instead of replacing it.
        let next_mask = match self.state().clip.as_deref() {
            Some(prev) => {
                let mut next = prev.clone();
                next.intersect_path(&path, TinyFillRule::Winding, anti_alias, transform);
                next
            }
            None => {
                let Some(mut next) = Mask::new(self.pixmap.width(), self.pixmap.height()) else {
                    return;
                };
                next.fill_path(&path, TinyFillRule::Winding, anti_alias, transform);
                next
            }
        };

        self.state_mut().clip = Some(Arc::new(next_mask));
    }

    fn push_opacity_layer(&mut self, _bounds: Rect, opacity: f32) {
        // Fully opaque groups need no offscreen pass. The layer is
        // window-sized (not bounds-sized) so coordinates and the window-sized
        // clip masks apply unchanged; composited in `restore_to`.
        if opacity >= 1.0 {
            return;
        }
        let Some(pixmap) = Pixmap::new(self.pixmap.width(), self.pixmap.height()) else {
            return;
        };
        self.layers.push(OpacityLayer {
            pixmap,
            opacity: opacity.max(0.0),
            depth: self.states.len(),
        });
    }

    fn fill_path(&mut self, path: &PathData, fill_rule: FillRule, paint: &PaintStyle) {
        let Some(path) = Self::tiny_path(path) else {
            return;
        };
        let tiny_rule = match fill_rule {
            FillRule::NonZero => TinyFillRule::Winding,
            FillRule::EvenOdd => TinyFillRule::EvenOdd,
        };
        let (target, state) = self.target_and_state();

        target.fill_path(
            &path,
            &Self::tiny_paint(paint),
            tiny_rule,
            state.transform,
            state.clip.as_deref(),
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
        let (target, state) = self.target_and_state();

        target.stroke_path(
            &path,
            &Self::tiny_paint(paint),
            &tiny_stroke,
            state.transform,
            state.clip.as_deref(),
        );
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) {
        let Some(rect) = tiny_rect(rect) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.r, color.g, color.b, color.a);
        let (target, state) = self.target_and_state();

        target.fill_rect(rect, &paint, state.transform, state.clip.as_deref());
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
        let (target, state) = self.target_and_state();

        target.fill_rect(rect, &paint, state.transform, state.clip.as_deref());
    }

    fn draw_alpha_mask(&mut self, mask: GlyphMaskRef<'_>, origin: Point, color: Color) {
        let w = mask.width;
        let h = mask.height;
        if w == 0 || h == 0 {
            return;
        }
        // Skip glyphs fully outside the pixmap.
        let px = origin.x.floor() as i32;
        let py = origin.y.floor() as i32;
        if px + w as i32 <= 0
            || py + h as i32 <= 0
            || px >= self.pixmap.width() as i32
            || py >= self.pixmap.height() as i32
        {
            return;
        }

        if self.state().transform.is_identity() {
            let (cr, cg, cb, ca) = (
                color.r as u32,
                color.g as u32,
                color.b as u32,
                color.a as u32,
            );
            self.blit_premul(origin, w, h, |x, y| {
                let alpha = mask.data[(y * w + x) as usize] as u32;
                let out_a = (ca * alpha) / 255;
                (
                    ((cr * out_a) / 255) as u8,
                    ((cg * out_a) / 255) as u8,
                    ((cb * out_a) / 255) as u8,
                    out_a as u8,
                )
            });
            return;
        }

        // Transformed fallback: rasterize into a temp pixmap and composite.
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
                if let Some(pixel) = PremultipliedColorU8::from_rgba(
                    (color.r as u32 * out_a / 255) as u8,
                    (color.g as u32 * out_a / 255) as u8,
                    (color.b as u32 * out_a / 255) as u8,
                    out_a as u8,
                ) {
                    pixels[(y * w + x) as usize] = pixel;
                }
            }
        }
        let (target, state) = self.target_and_state();
        target.draw_pixmap(
            px,
            py,
            temp.as_ref(),
            &tiny_skia::PixmapPaint {
                opacity: 1.0,
                blend_mode: tiny_skia::BlendMode::SourceOver,
                quality: tiny_skia::FilterQuality::Nearest,
            },
            state.transform,
            state.clip.as_deref(),
        );
    }

    fn draw_color_bitmap(&mut self, bitmap: GlyphBitmapRef<'_>, origin: Point) {
        let w = bitmap.width;
        let h = bitmap.height;
        if w == 0 || h == 0 {
            return;
        }
        // Skip glyphs fully outside the pixmap.
        let px = origin.x.floor() as i32;
        let py = origin.y.floor() as i32;
        if px + w as i32 <= 0
            || py + h as i32 <= 0
            || px >= self.pixmap.width() as i32
            || py >= self.pixmap.height() as i32
        {
            return;
        }

        if self.state().transform.is_identity() {
            self.blit_premul(origin, w, h, |x, y| {
                let src_idx = ((y * w + x) * 4) as usize;
                let r = bitmap.data[src_idx] as u32;
                let g = bitmap.data[src_idx + 1] as u32;
                let b = bitmap.data[src_idx + 2] as u32;
                let a = bitmap.data[src_idx + 3] as u32;
                (
                    ((r * a) / 255) as u8,
                    ((g * a) / 255) as u8,
                    ((b * a) / 255) as u8,
                    a as u8,
                )
            });
            return;
        }

        // Transformed fallback: rasterize into a temp pixmap and composite.
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
                if let Some(pixel) = PremultipliedColorU8::from_rgba(
                    (r as u32 * a as u32 / 255) as u8,
                    (g as u32 * a as u32 / 255) as u8,
                    (b as u32 * a as u32 / 255) as u8,
                    a,
                ) {
                    pixels[(y * w + x) as usize] = pixel;
                }
            }
        }
        let (target, state) = self.target_and_state();
        target.draw_pixmap(
            px,
            py,
            temp.as_ref(),
            &tiny_skia::PixmapPaint {
                opacity: 1.0,
                blend_mode: tiny_skia::BlendMode::SourceOver,
                quality: tiny_skia::FilterQuality::Nearest,
            },
            state.transform,
            state.clip.as_deref(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn red_at(renderer: &TinySkiaRenderer, x: u32, y: u32) -> u8 {
        let idx = (y * renderer.pixmap.width() + x) as usize;
        renderer.pixmap.pixels()[idx].red()
    }

    #[test]
    fn nested_clips_intersect() {
        let mut renderer = TinySkiaRenderer::new(100, 100).unwrap();
        renderer.clear(Color::WHITE);

        let token = renderer.save();
        renderer.clip(&ClipShape::Rect(Rect::new(0.0, 0.0, 50.0, 100.0)), false);
        renderer.clip(&ClipShape::Rect(Rect::new(25.0, 0.0, 100.0, 100.0)), false);
        RenderCommands::fill_rect(
            &mut renderer,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            Color::rgb(255, 0, 0),
        );
        renderer.restore_to(token);

        // Only the intersection (x in 25..50) may be painted. The green
        // channel distinguishes white (255) from the red fill (0).
        let green_at = |r: &TinySkiaRenderer, x: u32, y: u32| {
            let idx = (y * r.pixmap.width() + x) as usize;
            r.pixmap.pixels()[idx].green()
        };
        assert_eq!(green_at(&renderer, 10, 50), 255, "outside inner clip");
        assert_eq!(green_at(&renderer, 60, 50), 255, "outside outer clip");
        assert_eq!(green_at(&renderer, 35, 50), 0, "inside both clips");
    }

    #[test]
    fn restore_drops_clip() {
        let mut renderer = TinySkiaRenderer::new(50, 50).unwrap();
        renderer.clear(Color::WHITE);

        let token = renderer.save();
        renderer.clip(&ClipShape::Rect(Rect::new(0.0, 0.0, 10.0, 10.0)), false);
        renderer.restore_to(token);

        RenderCommands::fill_rect(
            &mut renderer,
            Rect::new(0.0, 0.0, 50.0, 50.0),
            Color::rgb(255, 0, 0),
        );
        assert_eq!(red_at(&renderer, 40, 40), 255, "clip must not survive restore");
    }

    #[test]
    fn glyph_blit_blends_and_respects_clip() {
        let mut renderer = TinySkiaRenderer::new(20, 20).unwrap();
        renderer.clear(Color::WHITE);
        renderer.clip(&ClipShape::Rect(Rect::new(0.0, 0.0, 10.0, 20.0)), false);

        // A fully opaque 4x4 glyph mask straddling the clip edge at x=8.
        let data = [255u8; 16];
        renderer.draw_alpha_mask(
            GlyphMaskRef {
                width: 4,
                height: 4,
                data: &data,
            },
            Point::new(8.0, 8.0),
            Color::rgb(0, 0, 255),
        );

        let blue_at = |r: &TinySkiaRenderer, x: u32, y: u32| {
            let idx = (y * r.pixmap.width() + x) as usize;
            r.pixmap.pixels()[idx].blue()
        };
        let green_at = |r: &TinySkiaRenderer, x: u32, y: u32| {
            let idx = (y * r.pixmap.width() + x) as usize;
            r.pixmap.pixels()[idx].green()
        };
        assert_eq!(green_at(&renderer, 9, 9), 0, "glyph drawn inside clip");
        assert_eq!(blue_at(&renderer, 9, 9), 255, "glyph drawn inside clip");
        assert_eq!(green_at(&renderer, 11, 9), 255, "glyph clipped outside");
    }

    #[test]
    fn glyph_blit_partially_offscreen() {
        let mut renderer = TinySkiaRenderer::new(10, 10).unwrap();
        renderer.clear(Color::WHITE);

        let data = [255u8; 16];
        renderer.draw_alpha_mask(
            GlyphMaskRef {
                width: 4,
                height: 4,
                data: &data,
            },
            Point::new(-2.0, -2.0),
            Color::rgb(0, 0, 255),
        );

        let blue_at = |r: &TinySkiaRenderer, x: u32, y: u32| {
            let idx = (y * r.pixmap.width() + x) as usize;
            r.pixmap.pixels()[idx].blue()
        };
        let green_at = |r: &TinySkiaRenderer, x: u32, y: u32| {
            let idx = (y * r.pixmap.width() + x) as usize;
            r.pixmap.pixels()[idx].green()
        };
        assert_eq!(green_at(&renderer, 0, 0), 0);
        assert_eq!(blue_at(&renderer, 0, 0), 255);
        assert_eq!(green_at(&renderer, 5, 5), 255, "beyond glyph extent");
    }

    #[test]
    fn opacity_layer_composites_group_alpha() {
        let mut renderer = TinySkiaRenderer::new(10, 10).unwrap();
        renderer.clear(Color::WHITE);

        let token = renderer.save();
        renderer.push_opacity_layer(Rect::new(0.0, 0.0, 10.0, 10.0), 0.5);
        RenderCommands::fill_rect(
            &mut renderer,
            Rect::new(0.0, 0.0, 10.0, 10.0),
            Color::rgb(255, 0, 0),
        );
        // Nothing lands on the base pixmap until the layer is composited.
        assert_eq!(renderer.pixmap.pixels()[0].green(), 255);
        renderer.restore_to(token);

        let px = renderer.pixmap.pixels()[0];
        assert_eq!(px.red(), 255, "red stays saturated");
        assert!(
            (120..=135).contains(&px.green()),
            "white blended with red at 50%, got green {}",
            px.green()
        );
        assert!(renderer.layers.is_empty(), "layer must be popped");
    }

    #[test]
    fn opaque_opacity_layer_is_skipped() {
        let mut renderer = TinySkiaRenderer::new(10, 10).unwrap();
        renderer.clear(Color::WHITE);

        let token = renderer.save();
        renderer.push_opacity_layer(Rect::new(0.0, 0.0, 10.0, 10.0), 1.0);
        assert!(renderer.layers.is_empty(), "opacity 1.0 needs no layer");
        RenderCommands::fill_rect(
            &mut renderer,
            Rect::new(0.0, 0.0, 10.0, 10.0),
            Color::rgb(255, 0, 0),
        );
        renderer.restore_to(token);
        assert_eq!(renderer.pixmap.pixels()[0].green(), 0);
    }

    #[test]
    fn begin_frame_reuses_pixmap_and_resets_state() {
        let mut renderer = TinySkiaRenderer::new(30, 30).unwrap();
        renderer.save();
        renderer.clip(&ClipShape::Rect(Rect::new(0.0, 0.0, 5.0, 5.0)), false);

        renderer.begin_frame(30, 30).unwrap();
        assert_eq!(renderer.states.len(), 1);
        assert!(renderer.state().clip.is_none());
        assert_eq!(renderer.pixmap.width(), 30);

        renderer.begin_frame(40, 20).unwrap();
        assert_eq!(renderer.pixmap.width(), 40);
        assert_eq!(renderer.pixmap.height(), 20);
    }
}
