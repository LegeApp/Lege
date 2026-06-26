use std::{ops::Range, sync::Arc};

pub type ImageId = u64;
pub type SvgId = u64;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Rect {
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn width(self) -> f32 {
        self.right - self.left
    }

    pub fn height(self) -> f32 {
        self.bottom - self.top
    }

    pub fn is_empty(self) -> bool {
        self.width() <= 0.0 || self.height() <= 0.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Radius {
    pub x: f32,
    pub y: f32,
}

impl Radius {
    pub const fn circular(value: f32) -> Self {
        Self { x: value, y: value }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RRect {
    pub rect: Rect,
    pub tl: Radius,
    pub tr: Radius,
    pub br: Radius,
    pub bl: Radius,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine2D {
    pub sx: f32,
    pub kx: f32,
    pub ky: f32,
    pub sy: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Affine2D {
    pub const IDENTITY: Self = Self {
        sx: 1.0,
        kx: 0.0,
        ky: 0.0,
        sy: 1.0,
        tx: 0.0,
        ty: 0.0,
    };
}

impl Default for Affine2D {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Affine2D {
    pub fn from_translate(tx: f32, ty: f32) -> Self {
        Self {
            sx: 1.0,
            kx: 0.0,
            ky: 0.0,
            sy: 1.0,
            tx,
            ty,
        }
    }

    pub fn from_scale(sx: f32, sy: f32) -> Self {
        Self {
            sx,
            kx: 0.0,
            ky: 0.0,
            sy,
            tx: 0.0,
            ty: 0.0,
        }
    }

    pub fn from_rotate(angle_rad: f32) -> Self {
        let cos = angle_rad.cos();
        let sin = angle_rad.sin();
        Self {
            sx: cos,
            kx: -sin,
            ky: sin,
            sy: cos,
            tx: 0.0,
            ty: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    pub const WHITE: Self = Self::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Brush {
    Solid(Color),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineJoin {
    Miter,
    Round,
    Bevel,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StrokeStyle {
    pub width: f32,
    pub miter_limit: f32,
    pub line_cap: LineCap,
    pub line_join: LineJoin,
    pub dash: Option<Vec<f32>>,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        Self {
            width: 1.0,
            miter_limit: 4.0,
            line_cap: LineCap::Butt,
            line_join: LineJoin::Miter,
            dash: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClipShape {
    Rect(Rect),
    RRect(RRect),
    Path(PathData),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaintStyle {
    pub brush: Brush,
    pub anti_alias: bool,
}

impl PaintStyle {
    pub const fn solid(color: Color) -> Self {
        Self {
            brush: Brush::Solid(color),
            anti_alias: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageDrawOptions {
    pub src: Option<Rect>,
    pub opacity: f32,
    pub sampling: ImageSampling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageSampling {
    Nearest,
    Bilinear,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SvgDrawOptions {
    pub opacity: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathData {
    pub verbs: Arc<[PathVerb]>,
}

impl PathData {
    pub fn new(verbs: impl Into<Arc<[PathVerb]>>) -> Self {
        Self {
            verbs: verbs.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PathVerb {
    MoveTo(Point),
    LineTo(Point),
    QuadTo(Point, Point),
    CubicTo(Point, Point, Point),
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextPosition {
    pub byte_index: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum TextAlign {
    #[default]
    Left,
    Right,
    Center,
    Justify,
}

pub trait ParagraphLayout: Send + Sync {
    fn size(&self) -> Size;
    fn hit_test_point(&self, point: Point) -> TextPosition;
    fn rects_for_range(&self, range: Range<usize>) -> Vec<Rect>;
    fn cursor_rect(&self, position: TextPosition) -> Rect;
    fn draw(&self, renderer: &mut dyn TextRasterTarget, origin: Point);
}

pub struct GlyphMaskRef<'a> {
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
}

pub struct GlyphBitmapRef<'a> {
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
}

pub trait TextRasterTarget {
    fn fill_rect(&mut self, rect: Rect, color: Color);
    fn draw_alpha_mask(&mut self, mask: GlyphMaskRef<'_>, origin: Point, color: Color);
    fn draw_color_bitmap(&mut self, bitmap: GlyphBitmapRef<'_>, origin: Point);
}

pub trait ImageStore {}

pub trait SvgStore {}

pub trait TextEngine {}

pub struct RenderResources<'a> {
    pub images: &'a mut dyn ImageStore,
    pub svgs: &'a mut dyn SvgStore,
    pub text: &'a mut dyn TextEngine,
}

pub trait RenderCommands {
    fn save(&mut self) -> usize;
    fn restore_to(&mut self, token: usize);
    fn clear(&mut self, color: Color);
    fn transform(&mut self, affine: Affine2D);
    fn clip(&mut self, shape: &ClipShape, anti_alias: bool);
    fn push_opacity_layer(&mut self, bounds: Rect, opacity: f32);
    fn fill_path(&mut self, path: &PathData, fill_rule: FillRule, paint: &PaintStyle);
    fn stroke_path(&mut self, path: &PathData, stroke: &StrokeStyle, paint: &PaintStyle);
    fn fill_rect(&mut self, rect: Rect, color: Color);

    /// Render a drop shadow for the given path
    fn fill_drop_shadow(
        &mut self,
        path: &PathData,
        color: Color,
        blur_radius: f32,
        offset_x: f32,
        offset_y: f32,
        spread: f32,
    );

    fn draw_image(&mut self, image: ImageId, dest: Rect, options: &ImageDrawOptions);
    fn draw_svg(&mut self, svg: SvgId, dest: Rect, options: &SvgDrawOptions);
    fn draw_paragraph(&mut self, paragraph: &dyn ParagraphLayout, origin: Point);
}

pub mod prelude {
    pub use crate::{
        Affine2D, Brush, ClipShape, Color, FillRule, GlyphBitmapRef, GlyphMaskRef,
        ImageDrawOptions, ImageId, ImageSampling, ImageStore, LineCap, LineJoin, PaintStyle,
        ParagraphLayout, PathData, PathVerb, Point, RRect, Radius, Rect, RenderCommands,
        RenderResources, Size, StrokeStyle, SvgDrawOptions, SvgId, SvgStore, TextAlign, TextEngine,
        TextPosition, TextRasterTarget,
    };
}
