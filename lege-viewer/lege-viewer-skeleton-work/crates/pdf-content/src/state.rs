//! The PDF graphics state (ISO 32000-1 §8.4, §9.3) and path construction.
//!
//! One [`GraphicsState`] is the interpreter's *current* state; `q`/`Q` push
//! and pop clones of it. Text-state parameters live here too (they are part
//! of the graphics state and obey `q`/`Q`); the text matrix and line matrix
//! do **not** — they are reset by `BT` and only exist within `BT`/`ET`, so
//! they live on the interpreter, not here.

use pdf_object::NameId;
use pdf_page_ir::{BlendMode, LineCap, LineJoin, Matrix, PathData, PathVerb, Point};

use crate::semantic::{FontId, SemColor};

/// The current color space selected by `cs`/`CS`, tracked so `sc`/`scn`
/// (and `SC`/`SCN`) know how to interpret their operands.
#[derive(Debug, Clone, PartialEq)]
pub enum ColorSpace {
    DeviceGray,
    DeviceRgb,
    DeviceCmyk,
    /// `/Pattern` (optionally over an underlying space for uncolored patterns).
    Pattern(Option<Box<ColorSpace>>),
    /// A resource-named space (`/CS0`); components resolved lazily.
    Named(NameId),
}

impl ColorSpace {
    /// Component count where fixed by the family (`None` for pattern/named).
    pub fn components(&self) -> Option<usize> {
        match self {
            ColorSpace::DeviceGray => Some(1),
            ColorSpace::DeviceRgb => Some(3),
            ColorSpace::DeviceCmyk => Some(4),
            ColorSpace::Pattern(_) | ColorSpace::Named(_) => None,
        }
    }
}

/// The full graphics state. `Clone` is the `q` operation.
#[derive(Debug, Clone)]
pub struct GraphicsState {
    pub ctm: Matrix,

    pub fill_space: ColorSpace,
    pub stroke_space: ColorSpace,
    pub fill_color: SemColor,
    pub stroke_color: SemColor,

    pub fill_alpha: f32,
    pub stroke_alpha: f32,
    pub blend: BlendMode,

    pub line_width: f64,
    pub line_cap: LineCap,
    pub line_join: LineJoin,
    pub miter_limit: f64,
    pub dash_pattern: Vec<f64>,
    pub dash_phase: f64,

    /// Depth of active clip intersections — geometry stays in the op stream.
    pub clip_depth: u32,

    // --- text state (§9.3) ---
    pub char_spacing: f64,
    pub word_spacing: f64,
    /// Horizontal scaling as a percentage (`Tz`); default 100.
    pub horizontal_scale: f64,
    pub leading: f64,
    pub font: Option<FontId>,
    pub font_size: f64,
    pub render_mode: u8,
    pub rise: f64,
}

impl Default for GraphicsState {
    fn default() -> Self {
        Self {
            ctm: Matrix::IDENTITY,
            // Initial color is black in DeviceGray for both fill and stroke.
            fill_space: ColorSpace::DeviceGray,
            stroke_space: ColorSpace::DeviceGray,
            fill_color: SemColor::DeviceGray(0.0),
            stroke_color: SemColor::DeviceGray(0.0),
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            blend: BlendMode::Normal,
            line_width: 1.0,
            line_cap: LineCap::Butt,
            line_join: LineJoin::Miter,
            miter_limit: 10.0,
            dash_pattern: Vec::new(),
            dash_phase: 0.0,
            clip_depth: 0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scale: 100.0,
            leading: 0.0,
            font: None,
            font_size: 0.0,
            render_mode: 0,
            rise: 0.0,
        }
    }
}

/// Accumulates path-construction operators into a [`PathData`]. Points are in
/// user space (untransformed by the CTM); the CTM context is carried by the
/// surrounding `Concat` scope in the op stream.
#[derive(Debug, Default, Clone)]
pub struct PathBuilder {
    verbs: Vec<PathVerb>,
    points: Vec<Point>,
    /// Start of the current subpath (target of `h`/close and `v`).
    start: Point,
    /// Current point.
    current: Point,
    has_current: bool,
}

impl PathBuilder {
    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        let p = Point { x, y };
        self.verbs.push(PathVerb::MoveTo);
        self.points.push(p);
        self.start = p;
        self.current = p;
        self.has_current = true;
    }

    pub fn line_to(&mut self, x: f64, y: f64) {
        if !self.has_current {
            // A `l` with no current point starts a subpath (tolerant).
            self.move_to(x, y);
            return;
        }
        let p = Point { x, y };
        self.verbs.push(PathVerb::LineTo);
        self.points.push(p);
        self.current = p;
    }

    /// `c`: cubic Bézier with two explicit control points.
    pub fn curve_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64) {
        if !self.has_current {
            self.move_to(x1, y1);
        }
        self.verbs.push(PathVerb::CurveTo);
        self.points.push(Point { x: x1, y: y1 });
        self.points.push(Point { x: x2, y: y2 });
        self.points.push(Point { x: x3, y: y3 });
        self.current = Point { x: x3, y: y3 };
    }

    /// `v`: cubic Bézier using the current point as the first control point.
    pub fn curve_v(&mut self, x2: f64, y2: f64, x3: f64, y3: f64) {
        let c1 = self.current;
        self.curve_to(c1.x, c1.y, x2, y2, x3, y3);
    }

    /// `y`: cubic Bézier whose second control point coincides with the end.
    pub fn curve_y(&mut self, x1: f64, y1: f64, x3: f64, y3: f64) {
        self.curve_to(x1, y1, x3, y3, x3, y3);
    }

    /// `re`: append a closed rectangle subpath.
    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        self.move_to(x, y);
        self.line_to(x + w, y);
        self.line_to(x + w, y + h);
        self.line_to(x, y + h);
        self.close();
        // Per spec the current point after `re` is the rectangle's origin.
        self.current = Point { x, y };
        self.start = Point { x, y };
        self.has_current = true;
    }

    /// `h`: close the current subpath.
    pub fn close(&mut self) {
        self.verbs.push(PathVerb::Close);
        self.current = self.start;
    }

    /// Take the accumulated path and reset for the next one.
    pub fn take(&mut self) -> PathData {
        let verbs = std::mem::take(&mut self.verbs);
        let points = std::mem::take(&mut self.points);
        self.has_current = false;
        self.start = Point::default();
        self.current = Point::default();
        PathData { verbs: verbs.into(), points: points.into() }
    }

    /// Discard the current path (a painting operator consumed it, or `n` with
    /// no pending clip).
    pub fn clear(&mut self) {
        self.verbs.clear();
        self.points.clear();
        self.has_current = false;
        self.start = Point::default();
        self.current = Point::default();
    }
}
