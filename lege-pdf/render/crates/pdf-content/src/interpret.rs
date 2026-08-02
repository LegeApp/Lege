//! The content-stream interpreter: the PDF graphics-state machine.
//!
//! Drives the [`crate::tokenizer::ContentLexer`] over a page's decoded content
//! bytes, maintaining the graphics-state stack and text/path state, and emits
//! a [`SemanticPage`]. No raster backend is involved and no document state is
//! mutated — every scratch value is owned here, reading only the immutable
//! [`DocumentSnapshot`].

use std::collections::HashMap;
use std::sync::Arc;

use pdf_document::{DocumentSnapshot, ParseContext};
use pdf_object::{Dictionary, NameId, NameTable, ObjectId, PdfObject, PdfStream, StreamData};
use pdf_page_ir::{
    BlendMode, FillRule, ImageColorSpace, ImageSMask, LineCap, LineJoin, MaskKind, Matrix,
    PageBounds, PaintOrigin, PathData, PathVerb, Point, Rect,
};

use crate::semantic::{
    ActualTextSpan, ActualTextSpanId, FontId, ImageId, PathId, SemColor, SemFont, SemImage,
    SemShading, SemShadingKind, SemTiling, SemanticOp, SemanticPage, ShadingId, TextElement,
    TextRun, TextRunId, TilingId,
};
use crate::state::{ColorSpace, GraphicsState, PathBuilder};
use crate::tokenizer::{ContentLexer, Lexeme, Operand};
use crate::{ContentError, ContentLimits};

/// A hard cap on the operand stack to bound a malformed stream that never
/// applies an operator.
const MAX_OPERAND_STACK: usize = 1 << 16;

/// A hard cap on a Type 0 sampled function's total sample count (`size *
/// n_out`), bounding allocation from an absurd `/Size` (malformed-resource
/// guard). Shadings need only modest resolution.
const MAX_SAMPLED_FN: usize = 1 << 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkupKind {
    Highlight,
    Underline,
    Squiggly,
    StrikeOut,
}

fn append_markup_geometry(builder: &mut PathBuilder, quad: [f64; 8], kind: MarkupKind) {
    if !quad.iter().all(|value| value.is_finite()) {
        return;
    }
    let points = [
        [quad[0], quad[1]],
        [quad[2], quad[3]],
        [quad[4], quad[5]],
        [quad[6], quad[7]],
    ];
    if matches!(kind, MarkupKind::Highlight) {
        append_polygon(builder, &[points[0], points[1], points[3], points[2]]);
        return;
    }

    let distance = |a: [f64; 2], b: [f64; 2]| (b[0] - a[0]).hypot(b[1] - a[1]);
    let height = (distance(points[0], points[2]) + distance(points[1], points[3])) * 0.5;
    let length = distance(points[2], points[3]);
    if height <= 1e-6 || length <= 1e-6 {
        return;
    }
    let bottom_mid = [
        (points[2][0] + points[3][0]) * 0.5,
        (points[2][1] + points[3][1]) * 0.5,
    ];
    let top_mid = [
        (points[0][0] + points[1][0]) * 0.5,
        (points[0][1] + points[1][1]) * 0.5,
    ];
    let inward_len = distance(bottom_mid, top_mid);
    if inward_len <= 1e-6 {
        return;
    }
    let inward = [
        (top_mid[0] - bottom_mid[0]) / inward_len,
        (top_mid[1] - bottom_mid[1]) / inward_len,
    ];
    let thickness = (height / 14.0).clamp(0.5, height / 3.0);

    match kind {
        MarkupKind::Underline => {
            let offset = thickness;
            append_strip(
                builder,
                add_scaled(points[2], inward, offset),
                add_scaled(points[3], inward, offset),
                inward,
                thickness,
            );
        }
        MarkupKind::StrikeOut => {
            append_strip(
                builder,
                [
                    (points[0][0] + points[2][0]) * 0.5,
                    (points[0][1] + points[2][1]) * 0.5,
                ],
                [
                    (points[1][0] + points[3][0]) * 0.5,
                    (points[1][1] + points[3][1]) * 0.5,
                ],
                inward,
                thickness,
            );
        }
        MarkupKind::Squiggly => {
            let amplitude = (height * 0.09).max(thickness);
            let baseline = amplitude + thickness;
            let steps = ((length / (amplitude * 1.5)).ceil() as usize).clamp(4, 256);
            let mut centers = Vec::with_capacity(steps + 1);
            for index in 0..=steps {
                let t = index as f64 / steps as f64;
                let mut center = [
                    points[2][0] + (points[3][0] - points[2][0]) * t,
                    points[2][1] + (points[3][1] - points[2][1]) * t,
                ];
                let wave = if index % 2 == 0 { -0.5 } else { 0.5 };
                center = add_scaled(center, inward, baseline + wave * amplitude);
                centers.push(center);
            }
            let mut polygon = Vec::with_capacity(centers.len() * 2);
            polygon.extend(
                centers
                    .iter()
                    .copied()
                    .map(|point| add_scaled(point, inward, thickness * 0.5)),
            );
            polygon.extend(
                centers
                    .iter()
                    .rev()
                    .copied()
                    .map(|point| add_scaled(point, inward, -thickness * 0.5)),
            );
            append_polygon(builder, &polygon);
        }
        MarkupKind::Highlight => {}
    }
}

fn add_scaled(point: [f64; 2], vector: [f64; 2], scale: f64) -> [f64; 2] {
    [point[0] + vector[0] * scale, point[1] + vector[1] * scale]
}

fn append_strip(
    builder: &mut PathBuilder,
    start: [f64; 2],
    end: [f64; 2],
    normal: [f64; 2],
    thickness: f64,
) {
    let half = thickness * 0.5;
    append_polygon(
        builder,
        &[
            add_scaled(start, normal, half),
            add_scaled(end, normal, half),
            add_scaled(end, normal, -half),
            add_scaled(start, normal, -half),
        ],
    );
}

fn append_polygon(builder: &mut PathBuilder, points: &[[f64; 2]]) {
    let Some(first) = points.first() else {
        return;
    };
    builder.move_to(first[0], first[1]);
    for point in &points[1..] {
        builder.line_to(point[0], point[1]);
    }
    builder.close();
}

#[derive(Debug, Clone)]
struct MarkedContentFrame {
    hidden: bool,
    actual_text: Option<ActualTextSpan>,
}

/// One page-compilation job's mutable state. Owned, never shared.
pub(crate) struct Interpreter<'a> {
    snapshot: &'a DocumentSnapshot,
    ctx: &'a mut ParseContext,
    names: &'a NameTable,
    limits: &'a ContentLimits,
    /// The active resource dictionary (page-level, or a form's while nested).
    resources: Option<Arc<PdfObject>>,
    /// Optional installed-font source for non-embedded fonts (Font Phase 7).
    system_fonts: Option<Arc<dyn pdf_font::SystemFontProvider>>,
    /// The page-level `/Resources`, kept for the whole interpretation. A form,
    /// pattern or CharProc whose own `/Resources` omits a whole category falls
    /// back to these — see [`Self::resource_subdict`].
    page_resources: Option<Arc<PdfObject>>,

    // --- output accumulators ---
    ops: Vec<SemanticOp>,
    paths: Vec<PathData>,
    text_runs: Vec<TextRun>,
    images: Vec<SemImage>,
    fonts: Vec<SemFont>,
    font_cache: HashMap<NameId, FontId>,
    shadings: Vec<SemShading>,
    tilings: Vec<SemTiling>,
    /// Resolved pattern resources, keyed by resource name, so a pattern used
    /// on many draws is compiled once.
    pattern_cache: HashMap<NameId, Option<SemColor>>,
    /// Resolved `/Separation` and `/DeviceN` spaces, keyed by resource name.
    /// Their tint transform is a PDF function that must be evaluated per
    /// colour, so it is parsed once and reused (a page typically sets the
    /// same separation thousands of times).
    tint_cache: HashMap<NameId, Option<Arc<TintSpace>>>,
    /// Resolved `/Type3` fonts, keyed by resource name (mirrors `tint_cache`).
    /// A Type 3 font's glyphs are content streams (`/CharProcs`) executed
    /// inline at show time; the FontMatrix, encoding, widths and CharProc
    /// streams are parsed once and reused for every glyph the page draws.
    type3_cache: HashMap<NameId, Option<Arc<Type3Font>>>,
    /// Resolved CIE-based spaces (`/Lab`, `/CalRGB`, `/CalGray`), keyed by
    /// resource name (mirrors `tint_cache`). Their parameters are read once;
    /// conversion itself is pure math in `pdf-color`.
    cie_cache: HashMap<NameId, Option<CieSpace>>,
    /// Marked-content nesting (`BMC`/`BDC`…`EMC`): `true` for levels opened
    /// by a `/OC` `BDC` whose optional-content group is OFF (ISO 32000-1
    /// §8.11.3.2; cpdf_occontext.cpp default-visibility semantics).
    mc_stack: Vec<MarkedContentFrame>,
    next_actual_text_span: u32,
    /// How many enclosing marked-content levels are OC-hidden. While > 0,
    /// painting is suppressed (state, clipping, and text advances still
    /// apply — hiding content must not shift what follows it).
    oc_hidden_count: u32,
    /// Set by [`Self::build_tint_image_lut`] when it had to repair a
    /// degenerate baked tint ramp (PLAN-POST-SWEEP3 §R1); cleared and read by
    /// the image builders so the flag lands on the right [`SemImage`].
    image_cs_degraded: bool,

    // --- interpreter state ---
    gs: GraphicsState,
    gs_stack: Vec<GraphicsState>,
    /// Lowest graphics-state stack depth the current nested content stream may
    /// pop to. Form-like streams are implicitly isolated even when malformed:
    /// an unmatched `Q` inside a CharProc must not consume its caller's save.
    gs_stack_floor: usize,
    path: PathBuilder,
    pending_clip: Option<FillRule>,
    /// Glyph runs shown with a clip render mode (Tr 4–7) since the current text
    /// object's `BT`; their outlines' union becomes the text-clipping path at
    /// `ET` (ISO 32000-1 §9.4.3). Empty between text objects.
    text_clip_runs: Vec<TextRunId>,
    operands: Vec<Operand>,
    ops_executed: u64,
    invoke_depth: u32,
    /// The CTM at entry of the content stream currently being interpreted —
    /// its *default user space* in page-space units. Pattern space anchors
    /// here (ISO 32000-1 §8.7.3.1: a pattern's `/Matrix` maps pattern space to
    /// the default space of the stream in which the pattern is *referenced* —
    /// the page, or a form/mask-group's space when invoked under a CTM).
    /// Identity for page content; saved/set around each form-content
    /// invocation, alongside the resource-cache swap that keeps per-stream
    /// pattern resolutions from leaking across anchor changes.
    pattern_base: Matrix,
    /// Object ids of forms currently on the invocation stack — precise cycle
    /// detection for self- or mutually-recursive XObjects.
    form_stack: Vec<ObjectId>,
    /// Object ids of the soft-mask groups currently being resolved, for
    /// recursion detection: a mask group whose content references itself (via
    /// an ExtGState `/SMask` naming the same group) is rendered *empty* at the
    /// recursive instance (§11.6.5.2 — an empty luminosity group is 0, fully
    /// masked), matching MuPDF, rather than recursing to the depth limit.
    soft_mask_stack: Vec<ObjectId>,
    /// Nesting depth of Type 3 CharProc execution. `d0`/`d1` glyph-metric
    /// operators only carry meaning inside a CharProc, so they are honoured
    /// only while this is non-zero (a stray `d1` on a page must not silently
    /// suppress the page's colours).
    type3_nesting: u32,
    /// A `d1` CharProc is a shape-only (stencil) glyph: colour-setting
    /// operators inside it are ignored so it paints with the fill colour in
    /// effect at show time (ISO 32000-1 §9.6.5.1). Scoped per CharProc.
    type3_shape_only: bool,
    /// The `wx` from the CharProc's leading `d0`/`d1`, captured as an advance
    /// fallback for a Type 3 font that supplies no `/Widths`.
    type3_d_width: Option<f64>,

    // text object matrices (reset by BT; not part of graphics state)
    text_matrix: Matrix,
    text_line_matrix: Matrix,

    /// The page referenced an `/ICCBased` colour space anywhere (fill,
    /// stroke, or image). Feeds `PageFeatures::ICC_COLOR` — a *flag*, not a
    /// CMM: unsupported profile shapes keep the documented arity
    /// approximation, while supported RGB and CMYK image transforms are
    /// carried explicitly.
    uses_icc_color: bool,
    /// Parsed `/ICCBased` profiles by stream object. `None` means "parsed, and
    /// there is nothing to apply" (an sRGB or out-of-scope profile), which is
    /// worth caching too — a document points every image at the same profile.
    icc_cache: HashMap<ObjectId, Option<Arc<pdf_color::icc::IccRgb>>>,
    /// Parsed `/ICCBased` **CMYK** press profiles by stream object (the
    /// device-CMYK counterpart of `icc_cache`). `None` caches a stream that is
    /// not a 4-input Lab-PCS lut. Used to convert DeviceCMYK content — both an
    /// explicit ICCBased colour space and a `/DefaultCMYK` redirect — the way
    /// PDFium routes it through LittleCMS.
    icc_cmyk_cache: HashMap<ObjectId, Option<Arc<pdf_color::icc::IccCmyk>>>,
    /// Backend-neutral copies of parsed CMYK lookup tables for codec-backed
    /// images. Kept separately because fills use the evaluator above while the
    /// page IR must not depend on `pdf-color`.
    icc_cmyk_image_cache: HashMap<ObjectId, Option<Arc<pdf_page_ir::IccCmykTransform>>>,
    /// An ExtGState enabled overprint (`/OP` or `/op` true). Feeds
    /// `PageFeatures::OVERPRINT`; compositing itself is unchanged.
    uses_overprint: bool,
}

/// The name-keyed resolution caches, parked while a nested content stream runs
/// under a different `/Resources` dictionary. See
/// [`Interpreter::take_resource_caches`].
struct ResourceCaches {
    font: HashMap<NameId, FontId>,
    type3: HashMap<NameId, Option<Arc<Type3Font>>>,
    pattern: HashMap<NameId, Option<SemColor>>,
    tint: HashMap<NameId, Option<Arc<TintSpace>>>,
    cie: HashMap<NameId, Option<CieSpace>>,
}

impl<'a> Interpreter<'a> {
    pub(crate) fn new(
        snapshot: &'a DocumentSnapshot,
        ctx: &'a mut ParseContext,
        limits: &'a ContentLimits,
        resources: Option<Arc<PdfObject>>,
        system_fonts: Option<Arc<dyn pdf_font::SystemFontProvider>>,
    ) -> Self {
        let names = snapshot.names();
        Self {
            snapshot,
            ctx,
            names,
            limits,
            page_resources: resources.clone(),
            resources,
            system_fonts,
            ops: Vec::new(),
            paths: Vec::new(),
            text_runs: Vec::new(),
            images: Vec::new(),
            fonts: Vec::new(),
            font_cache: HashMap::new(),
            shadings: Vec::new(),
            tilings: Vec::new(),
            pattern_cache: HashMap::new(),
            tint_cache: HashMap::new(),
            type3_cache: HashMap::new(),
            cie_cache: HashMap::new(),
            mc_stack: Vec::new(),
            next_actual_text_span: 0,
            oc_hidden_count: 0,
            image_cs_degraded: false,
            gs: GraphicsState::default(),
            gs_stack: Vec::new(),
            gs_stack_floor: 0,
            path: PathBuilder::default(),
            pending_clip: None,
            text_clip_runs: Vec::new(),
            operands: Vec::new(),
            ops_executed: 0,
            invoke_depth: 0,
            pattern_base: Matrix::IDENTITY,
            form_stack: Vec::new(),
            soft_mask_stack: Vec::new(),
            type3_nesting: 0,
            type3_shape_only: false,
            type3_d_width: None,
            text_matrix: Matrix::IDENTITY,
            text_line_matrix: Matrix::IDENTITY,
            uses_icc_color: false,
            icc_cache: HashMap::new(),
            icc_cmyk_cache: HashMap::new(),
            icc_cmyk_image_cache: HashMap::new(),
            uses_overprint: false,
        }
    }

    /// Interpret one content byte stream in the current state.
    ///
    /// Content malformation is *recovered*, not fatal (matching PDFium's
    /// interpreter): an unparseable token is skipped and an operator that fails
    /// on bad/insufficient operands is dropped, each with a `note_recovery`, and
    /// interpretation continues — so a single bad byte can never drop an
    /// otherwise-paintable page. Only the DoS guards (operator budget,
    /// recursion depth, operand-stack overflow, operand nesting depth) still
    /// abort; those are the only errors this ever returns `Err` for. See
    /// [`ContentError::is_fatal`].
    pub(crate) fn run(&mut self, content: &[u8]) -> Result<(), ContentError> {
        let mut lexer = ContentLexer::new(content, &self.limits.syntax);
        let mut lexemes_seen = 0_u64;
        loop {
            if lexemes_seen & 0xff == 0 && self.ctx.is_cancelled() {
                return Err(ContentError::Cancelled);
            }
            let pos_before = lexer.pos();
            match lexer.next_lexeme() {
                Ok(None) => break,
                Ok(Some(lex)) => {
                    lexemes_seen = lexemes_seen.saturating_add(1);
                    match lex {
                        Lexeme::Operand(o) => {
                            if self.operands.len() >= MAX_OPERAND_STACK {
                                // DoS guard: unbounded operand accumulation. Fatal.
                                return Err(ContentError::OperandStackOverflow(
                                    self.operands.len(),
                                ));
                            }
                            self.operands.push(o);
                        }
                        Lexeme::Operator(op) => {
                            self.ops_executed += 1;
                            if self.ops_executed > self.limits.max_ops {
                                return Err(ContentError::OperatorBudget(self.ops_executed));
                            }
                            if let Err(e) = self.dispatch(&op) {
                                if e.is_fatal() {
                                    return Err(e);
                                }
                                // Drop the bad operator and reset the operand stack,
                                // then continue at the next lexeme.
                                self.ctx.note_recovery(format!(
                                    "content operator {} dropped ({e})",
                                    String::from_utf8_lossy(&op)
                                ));
                            }
                            self.operands.clear();
                        }
                        Lexeme::InlineImage { dict, data } => {
                            // Suppressed inside an OC-hidden span.
                            if !self.oc_hidden() {
                                self.inline_image(&dict, data);
                            }
                            self.operands.clear();
                        }
                    }
                }
                Err(e) => {
                    if e.is_fatal() {
                        return Err(e);
                    }
                    self.ctx
                        .note_recovery(format!("content token skipped ({e})"));
                    // Guarantee forward progress. Most malformations leave the
                    // cursor past the offending token (e.g. a keyword where an
                    // operand was expected), so we simply continue. But a
                    // delimiter the lexer refuses to consume (a stray `)` or
                    // `>`) leaves the cursor where it started; step over one
                    // byte so the loop cannot spin. `read_inline_image` already
                    // advances the cursor itself before returning a recoverable
                    // framing error, so it lands in the "already advanced" case.
                    if lexer.pos() <= pos_before && !lexer.skip_byte() {
                        break; // at end of input: return the partial page
                    }
                }
            }
        }
        Ok(())
    }

    /// Finalize into an immutable [`SemanticPage`].
    pub(crate) fn finish(self, bounds: PageBounds) -> SemanticPage {
        SemanticPage {
            bounds,
            ops: self.ops.into(),
            paths: self.paths.into(),
            text_runs: self.text_runs.into(),
            images: self.images.into(),
            fonts: self.fonts.into(),
            shadings: self.shadings.into(),
            tilings: self.tilings.into(),
            uses_icc_color: self.uses_icc_color,
            uses_overprint: self.uses_overprint,
        }
    }

    // --- operand helpers ---------------------------------------------------

    /// The last `N` operands as `f64`s, or `None` if there are too few or any
    /// is non-numeric.
    fn nums<const N: usize>(&self) -> Option<[f64; N]> {
        let ops = &self.operands;
        if ops.len() < N {
            return None;
        }
        let base = ops.len() - N;
        let mut out = [0.0; N];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = ops[base + i].as_f64()?;
        }
        Some(out)
    }

    fn last_num(&self) -> Option<f64> {
        self.operands.last().and_then(Operand::as_f64)
    }

    fn intern(&self, bytes: &[u8]) -> NameId {
        self.names.intern(bytes)
    }

    // --- dispatch ----------------------------------------------------------

    fn dispatch(&mut self, op: &[u8]) -> Result<(), ContentError> {
        match op {
            // --- graphics state ---
            b"q" => {
                self.gs_stack.push(self.gs.clone());
                self.ops.push(SemanticOp::Save);
            }
            b"Q" => {
                if self.gs_stack.len() > self.gs_stack_floor
                    && let Some(prev) = self.gs_stack.pop()
                {
                    self.gs = prev;
                    self.ops.push(SemanticOp::Restore);
                }
                // Unbalanced Q with an empty stack is tolerated (ignored).
            }
            b"cm" => {
                if let Some([a, b, c, d, e, f]) = self.nums() {
                    let m = Matrix { a, b, c, d, e, f };
                    self.gs.ctm = m.then(self.gs.ctm);
                    self.ops.push(SemanticOp::Concat(m));
                }
            }
            b"w" => {
                if let Some(v) = self.last_num() {
                    self.gs.line_width = v;
                    self.ops.push(SemanticOp::SetLineWidth(v));
                }
            }
            b"J" => {
                if let Some(v) = self.last_num() {
                    let cap = map_line_cap(v);
                    self.gs.line_cap = cap;
                    self.ops.push(SemanticOp::SetLineCap(cap));
                }
            }
            b"j" => {
                if let Some(v) = self.last_num() {
                    let join = map_line_join(v);
                    self.gs.line_join = join;
                    self.ops.push(SemanticOp::SetLineJoin(join));
                }
            }
            b"M" => {
                if let Some(v) = self.last_num() {
                    self.gs.miter_limit = v;
                    self.ops.push(SemanticOp::SetMiterLimit(v));
                }
            }
            b"d" => self.op_dash(),
            // Rendering intent and flatness carry no semantic geometry.
            b"ri" | b"i" => {}
            b"gs" => self.op_ext_gstate()?,

            // --- color ---
            b"g" => self.set_device_gray(false),
            b"G" => self.set_device_gray(true),
            b"rg" => self.set_device_rgb(false),
            b"RG" => self.set_device_rgb(true),
            b"k" => self.set_device_cmyk(false),
            b"K" => self.set_device_cmyk(true),
            b"cs" => self.set_color_space(false),
            b"CS" => self.set_color_space(true),
            b"sc" | b"scn" => self.set_color(false),
            b"SC" | b"SCN" => self.set_color(true),

            // --- path construction ---
            b"m" => {
                if let Some([x, y]) = self.nums() {
                    self.path.move_to(x, y);
                }
            }
            b"l" => {
                if let Some([x, y]) = self.nums() {
                    self.path.line_to(x, y);
                }
            }
            b"c" => {
                if let Some([x1, y1, x2, y2, x3, y3]) = self.nums() {
                    self.path.curve_to(x1, y1, x2, y2, x3, y3);
                }
            }
            b"v" => {
                if let Some([x2, y2, x3, y3]) = self.nums() {
                    self.path.curve_v(x2, y2, x3, y3);
                }
            }
            b"y" => {
                if let Some([x1, y1, x3, y3]) = self.nums() {
                    self.path.curve_y(x1, y1, x3, y3);
                }
            }
            b"re" => {
                if let Some([x, y, w, h]) = self.nums() {
                    self.path.rect(x, y, w, h);
                }
            }
            b"h" => self.path.close(),

            // --- path painting ---
            b"S" => self.paint(false, None, true),
            b"s" => self.paint(true, None, true),
            b"f" | b"F" => self.paint(false, Some(FillRule::NonZero), false),
            b"f*" => self.paint(false, Some(FillRule::EvenOdd), false),
            b"B" => self.paint(false, Some(FillRule::NonZero), true),
            b"B*" => self.paint(false, Some(FillRule::EvenOdd), true),
            b"b" => self.paint(true, Some(FillRule::NonZero), true),
            b"b*" => self.paint(true, Some(FillRule::EvenOdd), true),
            b"n" => self.paint(false, None, false),

            // --- clipping ---
            b"W" => self.pending_clip = Some(FillRule::NonZero),
            b"W*" => self.pending_clip = Some(FillRule::EvenOdd),

            // --- text ---
            b"BT" => {
                self.text_matrix = Matrix::IDENTITY;
                self.text_line_matrix = Matrix::IDENTITY;
                // A text object's clip accumulation is bounded by BT..ET; a
                // prior ET always flushes, so this is defensive.
                self.text_clip_runs.clear();
            }
            b"ET" => {
                // ISO 32000-1 §9.4.3: at the end of a text object, the outlines
                // of any glyphs shown with a clip render mode (Tr 4–7) are
                // intersected into the clip, constraining subsequent painting
                // until the enclosing `Q`. A clip-mode text object that placed
                // no outlines still emits the clip (an empty clip = clip all
                // out), matching PDFium (cpdf_renderstatus.cpp ProcessClipPath).
                if !self.text_clip_runs.is_empty() {
                    let runs = std::mem::take(&mut self.text_clip_runs);
                    self.ops.push(SemanticOp::ClipText { runs });
                    self.gs.clip_depth += 1;
                }
            }
            b"Tc" => {
                if let Some(v) = self.last_num() {
                    self.gs.char_spacing = v;
                }
            }
            b"Tw" => {
                if let Some(v) = self.last_num() {
                    self.gs.word_spacing = v;
                }
            }
            b"Tz" => {
                if let Some(v) = self.last_num() {
                    self.gs.horizontal_scale = v;
                }
            }
            b"TL" => {
                if let Some(v) = self.last_num() {
                    self.gs.leading = v;
                }
            }
            b"Tf" => self.op_set_font(),
            b"Tr" => {
                if let Some(v) = self.last_num() {
                    self.gs.render_mode = v as u8;
                }
            }
            b"Ts" => {
                if let Some(v) = self.last_num() {
                    self.gs.rise = v;
                }
            }
            b"Td" => {
                if let Some([tx, ty]) = self.nums() {
                    self.text_move(tx, ty);
                }
            }
            b"TD" => {
                if let Some([tx, ty]) = self.nums() {
                    self.gs.leading = -ty;
                    self.text_move(tx, ty);
                }
            }
            b"Tm" => {
                if let Some([a, b, c, d, e, f]) = self.nums() {
                    self.text_line_matrix = Matrix { a, b, c, d, e, f };
                    self.text_matrix = self.text_line_matrix;
                }
            }
            b"T*" => {
                let leading = self.gs.leading;
                self.text_move(0.0, -leading);
            }
            b"Tj" => {
                if let Some(bytes) = self.operands.last().and_then(Operand::as_string) {
                    let elements = vec![TextElement::Show(bytes.to_vec())];
                    self.emit_text_run(elements);
                }
            }
            b"TJ" => self.op_show_array(),
            b"'" => {
                let leading = self.gs.leading;
                self.text_move(0.0, -leading);
                if let Some(bytes) = self.operands.last().and_then(Operand::as_string) {
                    self.emit_text_run(vec![TextElement::Show(bytes.to_vec())]);
                }
            }
            b"\"" => {
                // aw ac string "
                if let Some([aw, ac]) = self.nums::<2>() {
                    self.gs.word_spacing = aw;
                    self.gs.char_spacing = ac;
                }
                let leading = self.gs.leading;
                self.text_move(0.0, -leading);
                if let Some(bytes) = self.operands.last().and_then(Operand::as_string) {
                    self.emit_text_run(vec![TextElement::Show(bytes.to_vec())]);
                }
            }

            // --- XObjects, shadings, marked content, compatibility ---
            b"Do" => self.op_do()?,
            b"sh" => {
                // Suppressed inside an OC-hidden span (paints nothing).
                if !self.oc_hidden() {
                    self.op_shading()?;
                }
            }

            // --- Type 3 glyph metrics (ISO 32000-1 §9.6.5) ---
            // `d0 wx wy` (coloured glyph) and `d1 wx wy llx lly urx ury`
            // (shape-only glyph) are the first operator of a CharProc. Only
            // meaningful while executing one; capture `wx` as an advance
            // fallback, and let `d1` lock colour for the rest of the glyph.
            b"d0" => {
                if self.type3_nesting > 0 && self.type3_d_width.is_none() {
                    self.type3_d_width = self.operands.first().and_then(Operand::as_f64);
                }
            }
            b"d1" => {
                if self.type3_nesting > 0 {
                    if self.type3_d_width.is_none() {
                        self.type3_d_width = self.operands.first().and_then(Operand::as_f64);
                    }
                    self.type3_shape_only = true;
                }
            }

            // --- marked content (ISO 32000-1 §14.6) ---
            // Only `/OC` spans affect rendering: content inside a span whose
            // optional-content group is OFF is suppressed (§8.11.3.2). Other
            // tags carry no geometry we model.
            b"BMC" => self.mc_stack.push(MarkedContentFrame {
                hidden: false,
                actual_text: None,
            }),
            b"BDC" => {
                let hidden = self.bdc_hides();
                let actual_text = self.bdc_actual_text();
                self.mc_stack.push(MarkedContentFrame {
                    hidden,
                    actual_text,
                });
                if hidden {
                    self.oc_hidden_count += 1;
                }
            }
            b"EMC" => {
                // Unbalanced EMC is tolerated (ignored), like unbalanced Q.
                if let Some(frame) = self.mc_stack.pop()
                    && frame.hidden
                {
                    self.oc_hidden_count -= 1;
                }
            }

            // Any unknown operator: ignore, having consumed the pending
            // operands.
            _ => {}
        }
        Ok(())
    }

    // --- color operators ---------------------------------------------------

    fn set_device_gray(&mut self, stroke: bool) {
        let v = self.last_num().unwrap_or(0.0);
        let color = SemColor::DeviceGray(v);
        self.set_color_and_space(stroke, ColorSpace::DeviceGray, color);
    }

    fn set_device_rgb(&mut self, stroke: bool) {
        let [r, g, b] = self.nums().unwrap_or([0.0; 3]);
        self.set_color_and_space(stroke, ColorSpace::DeviceRgb, SemColor::DeviceRgb(r, g, b));
    }

    fn set_device_cmyk(&mut self, stroke: bool) {
        let [c, m, y, k] = self.nums().unwrap_or([0.0; 4]);
        // A resource `/DefaultCMYK` press profile substitutes for the device
        // space (§8.6.5.6): PDFium runs the `k`/`K` operands through it via
        // LittleCMS instead of its frozen table. Convert once, here, so the
        // colour lands as the same sRGB PDFium produces.
        let color = match self.default_cmyk_profile() {
            Some(prof) => {
                let [r, g, b] = prof.to_srgb([c as f32, m as f32, y as f32, k as f32]);
                SemColor::DeviceRgb(r as f64, g as f64, b as f64)
            }
            None => SemColor::DeviceCmyk(c, m, y, k),
        };
        self.set_color_and_space(stroke, ColorSpace::DeviceCmyk, color);
    }

    fn set_color_and_space(&mut self, stroke: bool, space: ColorSpace, color: SemColor) {
        if stroke {
            self.gs.stroke_space = space;
            self.gs.stroke_color = color.clone();
            self.push_color_op(SemanticOp::SetStrokeColor(color));
        } else {
            self.gs.fill_space = space;
            self.gs.fill_color = color.clone();
            self.push_color_op(SemanticOp::SetFillColor(color));
        }
    }

    /// Emit a colour-setting op, unless a `d1` (shape-only) Type 3 glyph is
    /// being executed — such a glyph paints with the fill colour in effect at
    /// show time, so its own colour operators are suppressed (§9.6.5.1). The
    /// graphics state is still updated by the caller so a matching Restore
    /// stays balanced; only the emitted mark is dropped.
    fn push_color_op(&mut self, op: SemanticOp) {
        if !self.type3_shape_only {
            self.ops.push(op);
        }
    }

    fn set_color_space(&mut self, stroke: bool) {
        let Some(name) = self
            .operands
            .last()
            .and_then(Operand::as_name)
            .map(<[u8]>::to_vec)
        else {
            return;
        };
        let space = self.resolve_color_space(&name);
        // Separation/DeviceN start at 1.0 in every component — full colorant,
        // i.e. black-ish, not white (ISO 32000-1 §8.6.8). `eval_tint` applies
        // that default for an empty operand list.
        let initial = match &space {
            ColorSpace::Named(id) => match self.resolve_tint_space(*id) {
                Some(ts) => self.eval_tint(&ts, &[]),
                // CIE spaces initialise to all-zero components (§8.6.3),
                // which the conversion turns into the right device black.
                None => match self.resolve_cie_space(*id) {
                    Some(cie) => eval_cie(cie, &[]),
                    None => initial_color(&space),
                },
            },
            _ => initial_color(&space),
        };
        if let ColorSpace::Named(id) = &space {
            self.note_named_space_family(*id);
        }
        self.set_color_and_space(stroke, space, initial);
    }

    /// Record color-management facts about a named colour space that only
    /// exist as page feature flags: `/ICCBased` sets `uses_icc_color` (the
    /// rendering itself keeps the documented arity approximation).
    fn note_named_space_family(&mut self, id: NameId) {
        if self.uses_icc_color {
            return;
        }
        let Some(cs) = self.resource_subdict(self.names.known.color_space) else {
            return;
        };
        let Some(entry) = cs.as_dict().and_then(|d| d.get(id)).cloned() else {
            return;
        };
        let Some(obj) = self.resolve_obj(&entry) else {
            return;
        };
        if let PdfObject::Array(items) = &*obj
            && let Some(head) = items.first().and_then(PdfObject::as_name)
            && self.names.resolve(head).as_ref() == b"ICCBased"
        {
            self.uses_icc_color = true;
        }
    }

    /// Resolve a resource-named space if it is `/Separation` or `/DeviceN`.
    ///
    /// These are *subtractive*: a tint of 1.0 means **full colorant**, and
    /// the tint transform maps tints into an alternate space. Treating them
    /// by component arity instead (1 tint → DeviceGray) inverts them —
    /// `1.0` becomes white — which silently erases text painted in
    /// `[/Separation /Black ...]`, a very common way to spell "black" in
    /// print-oriented PDFs.
    fn resolve_tint_space(&mut self, id: NameId) -> Option<Arc<TintSpace>> {
        if let Some(hit) = self.tint_cache.get(&id) {
            return hit.clone();
        }
        let resolved = self.build_tint_space(id);
        self.tint_cache.insert(id, resolved.clone());
        resolved
    }

    /// Resolve a resource-named space *only if it is `/Indexed`*, returning the
    /// palette-carrying [`ImageColorSpace`]. Other families (Separation/DeviceN,
    /// CIE, ICCBased) are left to the tint / CIE / arity paths in
    /// [`Self::set_color`]. An `sc`/`scn` in an Indexed space takes an integer
    /// palette index, not colour components (§8.6.6.3); without this the index
    /// was mis-read as raw components and every fill collapsed to black
    /// (pdfbox/2561 page 3 dropped its red/green/blue bars to one black bar).
    fn resolve_named_indexed(&mut self, id: NameId) -> Option<pdf_page_ir::ImageColorSpace> {
        let cs = self.resolve_image_colorspace(&PdfObject::Name(id))?;
        matches!(cs, pdf_page_ir::ImageColorSpace::Indexed { .. }).then_some(cs)
    }

    fn build_tint_space(&mut self, id: NameId) -> Option<Arc<TintSpace>> {
        let cs = self.resource_subdict(self.names.known.color_space)?;
        let entry = cs.as_dict().and_then(|d| d.get(id)).cloned()?;
        let obj = self.resolve_obj(&entry)?;
        let PdfObject::Array(items) = &*obj else {
            return None;
        };
        self.build_tint_space_from_array(items)
    }

    /// Build a [`TintSpace`] from a resolved `[/Separation …]` / `[/DeviceN …]`
    /// array (shared by the resource-named fill path and the image-colorspace
    /// path).
    fn build_tint_space_from_array(&mut self, items: &[PdfObject]) -> Option<Arc<TintSpace>> {
        let head = items.first().and_then(PdfObject::as_name)?;
        let kind = self.names.resolve(head).to_vec();

        // `[/Separation name alt tint]` | `[/DeviceN names alt tint ...]`
        let inputs = match kind.as_slice() {
            b"Separation" => 1,
            b"DeviceN" => {
                let names = self.resolve_obj(items.get(1)?)?;
                match &*names {
                    PdfObject::Array(a) => a.len().max(1),
                    _ => 1,
                }
            }
            _ => return None,
        };
        // `/None` marks nothing at all; `/All` paints every plate.
        let colorant_name = |this: &mut Self, items: &[PdfObject], name: &[u8]| {
            items
                .get(1)
                .and_then(PdfObject::as_name)
                .map(|n| this.names.resolve(n).as_ref() == name)
                .unwrap_or(false)
        };
        let colorant_none = kind == b"Separation" && colorant_name(self, items, b"None");
        // `/All` is *not* special-cased when painting: PDFium's
        // `CPDF_SeparationCS::GetRGB` branches only on `/None`, and runs the
        // tint transform for every other colorant name including `/All`.
        // Short-circuiting it to neutral ink rendered custom/color_separation_3
        // as flat grey 128 where PDFium, hayro and MuPDF all produce the
        // transform's brown (81, 74, 71). Retained only as a page-feature note.
        let colorant_all = kind == b"Separation" && colorant_name(self, items, b"All");

        let alt_arity = items
            .get(2)
            .and_then(|alt| self.resolve_image_colorspace(alt))
            .map(|cs| cs.components())
            .unwrap_or(1);
        // A CIE alternate (Lab/CalRGB/CalGray): the transform outputs land in
        // that space's component ranges, not device [0,1].
        let alt_cie =
            items
                .get(2)
                .and_then(|alt| self.resolve_obj(alt))
                .and_then(|alt| match &*alt {
                    PdfObject::Array(a) => {
                        let a = a.to_vec();
                        self.cie_space_from_items(&a)
                    }
                    _ => None,
                });
        // An ICCBased CMYK alternate (the common press-profile spot colour):
        // the transform's four outputs go through the profile, not the frozen
        // table. Only consulted when `alt_cie` is absent (a profile is not CIE).
        let alt_icc_cmyk = if alt_cie.is_none() {
            items
                .get(2)
                .and_then(|alt| self.resolve_obj(alt))
                .and_then(|alt| match &*alt {
                    PdfObject::Array(a) => {
                        let a = a.to_vec();
                        self.iccbased_cmyk_from_items(&a)
                    }
                    _ => None,
                })
        } else {
            None
        };
        // The tint transform: 1 input for Separation, N for DeviceN. Built for
        // every arity now that the evaluator handles multi-input Type 0 and
        // Type 4; `eval_tint` feeds it all colorants through `eval_n`. A
        // transform that will not build stays `None` and the caller
        // approximates subtractively.
        let func = items
            .get(3)
            .cloned()
            .and_then(|f| self.build_function(&f, 0));
        Some(Arc::new(TintSpace {
            inputs,
            alt_arity,
            func,
            colorant_none,
            colorant_all,
            alt_cie,
            alt_icc_cmyk,
        }))
    }

    /// Convert tint values through a `/Separation` or `/DeviceN` space.
    fn eval_tint(&self, space: &TintSpace, values: &[f64]) -> SemColor {
        if space.colorant_none {
            // A /None colorant marks nothing. Nothing in the semantic colour
            // model expresses "no marks", so paint white — which is also
            // exactly what PDFium composites: `CPDF_SeparationCS::GetRGB`
            // returns nullopt for the None type and the color state falls
            // back to `GetColorRef().value_or(0xFFFFFFFF)` — white
            // (`cpdf_colorstate.cpp:130`). Verified 2026-07-21.
            return SemColor::DeviceRgb(1.0, 1.0, 1.0);
        }
        // Missing operands: the initial colour of these spaces is 1.0 in
        // every component (ISO 32000-1 §8.6.8) — full colorant.
        let tints: Vec<f32> = if values.is_empty() {
            vec![1.0; space.inputs]
        } else {
            values.iter().map(|v| (*v as f32).clamp(0.0, 1.0)).collect()
        };

        // Run all colorants through the tint transform (§8.6.6.5). `eval_n`
        // dispatches every function type — a 1-input Separation reads the sole
        // tint, a multi-input DeviceN feeds all of them into a `SampledN` /
        // Type 4 transform, landing the colour in its real alternate space
        // instead of the subtractive grey approximation.
        if let Some(func) = &space.func {
            let out = func.eval_n(&tints);
            // A CIE alternate: convert through the alternate space's own
            // math (raw components — Lab's L* is 0..100, a*/b* signed; the
            // conversions clamp internally). PDFium: base_cs_->GetRGB.
            if let Some(cie) = space.alt_cie
                && !out.is_empty()
            {
                let vals: Vec<f64> = out.iter().map(|v| *v as f64).collect();
                return eval_cie(cie, &vals);
            }
            // An ICCBased CMYK alternate: run the four tint outputs through the
            // press profile, matching PDFium's LittleCMS path.
            if let Some(prof) = &space.alt_icc_cmyk
                && out.len() == 4
            {
                let c = |i: usize| out[i].clamp(0.0, 1.0);
                let [r, g, b] = prof.to_srgb([c(0), c(1), c(2), c(3)]);
                return SemColor::DeviceRgb(r as f64, g as f64, b as f64);
            }
            if out.len() == space.alt_arity || matches!(out.len(), 1 | 3 | 4) {
                let comps: Vec<f32> = out.iter().map(|v| v.clamp(0.0, 1.0)).collect();
                match comps.len() {
                    1 => return SemColor::DeviceGray(comps[0] as f64),
                    3 => {
                        return SemColor::DeviceRgb(
                            comps[0] as f64,
                            comps[1] as f64,
                            comps[2] as f64,
                        );
                    }
                    4 => {
                        return SemColor::DeviceCmyk(
                            comps[0] as f64,
                            comps[1] as f64,
                            comps[2] as f64,
                            comps[3] as f64,
                        );
                    }
                    // An arity we cannot map to a device colour (e.g. 2, or a
                    // mismatch against the alternate space): fall through to the
                    // subtractive approximation below.
                    _ => {}
                }
            }
        }
        // No usable transform (multi-colorant DeviceN, Type 4 function, a
        // malformed one): approximate subtractively. The exact hue is lost
        // but the *polarity* is right — more tint means darker — which is
        // what makes text readable rather than invisible.
        let ink = tints.iter().copied().fold(0.0f32, f32::max);
        SemColor::DeviceGray((1.0 - ink) as f64)
    }

    /// Resolve a resource-named colour space into a [`CieSpace`] when its
    /// family is `/Lab`, `/CalRGB` or `/CalGray` (ISO 32000-1 §8.6.5), cached
    /// by resource name like [`Self::resolve_tint_space`]. `None` for every
    /// other family (Separation/DeviceN take the tint path; ICCBased stays an
    /// arity approximation by policy).
    fn resolve_cie_space(&mut self, id: NameId) -> Option<CieSpace> {
        if let Some(hit) = self.cie_cache.get(&id) {
            return *hit;
        }
        let resolved = self.build_cie_space(id);
        self.cie_cache.insert(id, resolved);
        resolved
    }

    fn build_cie_space(&mut self, id: NameId) -> Option<CieSpace> {
        let entry = self
            .resource_subdict(self.names.known.color_space)?
            .as_dict()?
            .get(id)?
            .clone();
        let resolved = self.resolve_obj(&entry)?;
        let PdfObject::Array(items) = &*resolved else {
            return None;
        };
        let items = items.to_vec();
        self.cie_space_from_items(&items)
    }

    /// Parse an already-resolved `[/Lab …]` / `[/CalRGB …]` / `[/CalGray …]`
    /// array into a [`CieSpace`] (shared by the resource-named path and the
    /// Separation/DeviceN *alternate*-space path).
    fn cie_space_from_items(&mut self, items: &[PdfObject]) -> Option<CieSpace> {
        let family = items.first().and_then(PdfObject::as_name)?;
        // The parameter dictionary is the (possibly indirect) second element.
        // Its absence is malformed; tolerate with spec defaults.
        let dict = items
            .get(1)
            .and_then(|d| self.resolve_obj(d))
            .and_then(|d| d.as_dict().cloned());
        let f32s = |key: &[u8], out: &mut [f32]| {
            let Some(d) = &dict else { return };
            let Some(nid) = self.names.lookup(key) else {
                return;
            };
            if let Some(PdfObject::Array(a)) = d.get(nid) {
                for (slot, item) in out.iter_mut().zip(a.iter()) {
                    if let Some(v) = item.as_number() {
                        *slot = v as f32;
                    }
                }
            }
        };
        match self.names.resolve(family).as_ref() {
            b"Lab" => {
                // /WhitePoint is accepted-but-unused by the conversion (it
                // mirrors PDFium's baked white); /Range defaults per §8.6.5.4.
                let mut white_point = [1.0f32, 1.0, 1.0];
                let mut range = [-100.0f32, 100.0, -100.0, 100.0];
                f32s(b"WhitePoint", &mut white_point);
                f32s(b"Range", &mut range);
                Some(CieSpace::Lab { white_point, range })
            }
            b"CalRGB" => {
                // Defaults are load-bearing: pdf_color::calrgb_to_rgb always
                // applies gamma and matrix, so absent entries must be the
                // identity (§8.6.5.3), never zeros (a zero matrix is the
                // singular-degrade-to-black path).
                let mut white_point = [1.0f32, 1.0, 1.0];
                let mut gamma = [1.0f32, 1.0, 1.0];
                let mut matrix = [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
                f32s(b"WhitePoint", &mut white_point);
                f32s(b"Gamma", &mut gamma);
                f32s(b"Matrix", &mut matrix);
                Some(CieSpace::CalRgb {
                    white_point,
                    gamma,
                    matrix,
                })
            }
            b"CalGray" => {
                let mut gamma = [1.0f32];
                if let Some(d) = &dict
                    && let Some(nid) = self.names.lookup(b"Gamma")
                    && let Some(v) = d.get(nid).and_then(PdfObject::as_number)
                {
                    gamma[0] = v as f32;
                }
                Some(CieSpace::CalGray { gamma: gamma[0] })
            }
            // An ICC profile is normally approximated by its `/N` component
            // count, which is right for the RGB/Gray/CMYK profiles that make up
            // almost all of them. It is badly wrong when the profile's data
            // space is *Lab*: pdfjs/issue5939's PANTONE 3278 U is a
            // `/Separation` whose alternate is a 3-component Lab profile, so
            // its tint transform emits L*=56.5 a*=-43 b*=2 — a mint green.
            // Read as RGB that renders hot magenta, because a negative a* is a
            // green and a positive one a magenta, and the whole watermark
            // flipped hue. The profile header declares its data space at bytes
            // 16..20 (ICC.1 §7.2.6), so trust it rather than the arity.
            //
            // Only Lab is worth intercepting: for the other data spaces the
            // arity-based approximation already lands in the right family, and
            // without a colour-management engine we cannot do better.
            b"ICCBased" => {
                let stream = items.get(1).and_then(|s| self.resolve_obj(s))?;
                let PdfObject::Stream(stream) = &*stream else {
                    return None;
                };
                let profile = self.snapshot.decode_stream_data(stream, self.ctx).ok()?;
                if profile.get(16..20)? != b"Lab " {
                    return None;
                }
                // ICC Lab encodes a*/b* over -128..127, wider than the PDF
                // `/Lab` default of -100..100; clamping to the narrower range
                // would clip saturated spot colours.
                Some(CieSpace::Lab {
                    white_point: [1.0, 1.0, 1.0],
                    range: [-128.0, 127.0, -128.0, 127.0],
                })
            }
            _ => None,
        }
    }

    fn resolve_color_space(&mut self, name: &[u8]) -> ColorSpace {
        match name {
            b"DeviceGray" | b"G" => ColorSpace::DeviceGray,
            b"DeviceRGB" | b"RGB" => ColorSpace::DeviceRgb,
            b"DeviceCMYK" | b"CMYK" => ColorSpace::DeviceCmyk,
            b"Pattern" => ColorSpace::Pattern(None),
            other => {
                // A resource-named space that resolves to `[/Pattern base]` is
                // a (uncolored) pattern space; anything else stays Named.
                if self.named_space_is_pattern(other) {
                    ColorSpace::Pattern(None)
                } else {
                    ColorSpace::Named(self.intern(other))
                }
            }
        }
    }

    /// True when a `/ColorSpace` resource named `name` is a Pattern space
    /// (`/Pattern` or `[/Pattern base]`).
    fn named_space_is_pattern(&mut self, name: &[u8]) -> bool {
        let nid = self.intern(name);
        let Some(cs) = self.resource_subdict(self.names.known.color_space) else {
            return false;
        };
        let Some(entry) = cs.as_dict().and_then(|d| d.get(nid)).cloned() else {
            return false;
        };
        match self.resolve_obj(&entry).as_deref() {
            Some(PdfObject::Name(n)) => self.names.resolve(*n).as_ref() == b"Pattern",
            Some(PdfObject::Array(items)) => {
                matches!(items.first().and_then(PdfObject::as_name), Some(n) if self.names.resolve(n).as_ref() == b"Pattern")
            }
            _ => false,
        }
    }

    fn set_color(&mut self, stroke: bool) {
        let space = if stroke {
            self.gs.stroke_space.clone()
        } else {
            self.gs.fill_space.clone()
        };
        let nums: Vec<f64> = self.operands.iter().filter_map(Operand::as_f64).collect();
        // A trailing *name* operand means a pattern, whatever the declared
        // space says. PDFium decides this way round too — `Handle_SetColorPS_*`
        // branches on `pLastParam->IsName()` before it ever looks at the colour
        // space — and files rely on it: pdfjs/issue18466 paints a full-page
        // rectangle with `/P12 scn` and never emits a `cs` operator at all, so
        // the space is still the initial DeviceGray. Reading the operands as
        // grey components then yields *black*, and the page rendered as a solid
        // black block over its own text.
        let pattern_name = self
            .operands
            .iter()
            .rev()
            .find_map(Operand::as_name)
            .map(|b| self.intern(b));
        if !matches!(space, ColorSpace::Pattern(_))
            && let Some(name) = pattern_name
        {
            let color = self
                .resolve_pattern(name, &nums)
                .unwrap_or(SemColor::Pattern { name, under: None });
            if stroke {
                self.gs.stroke_color = color.clone();
                self.push_color_op(SemanticOp::SetStrokeColor(color));
            } else {
                self.gs.fill_color = color.clone();
                self.push_color_op(SemanticOp::SetFillColor(color));
            }
            return;
        }
        let color = match &space {
            ColorSpace::DeviceGray => SemColor::DeviceGray(nums.first().copied().unwrap_or(0.0)),
            ColorSpace::DeviceRgb => {
                SemColor::DeviceRgb(get(&nums, 0), get(&nums, 1), get(&nums, 2))
            }
            ColorSpace::DeviceCmyk => {
                let (c, m, y, k) = (get(&nums, 0), get(&nums, 1), get(&nums, 2), get(&nums, 3));
                // A `/DeviceCMYK cs` under a resource `/DefaultCMYK` press
                // profile is redirected through it, exactly like the `k`
                // operator (§8.6.5.6).
                match self.default_cmyk_profile() {
                    Some(prof) => {
                        let [r, g, b] = prof.to_srgb([c as f32, m as f32, y as f32, k as f32]);
                        SemColor::DeviceRgb(r as f64, g as f64, b as f64)
                    }
                    None => SemColor::DeviceCmyk(c, m, y, k),
                }
            }
            // Separation/DeviceN carry a tint transform; CIE-based spaces
            // (Lab/CalRGB/CalGray) carry conversion parameters — both are
            // evaluated here, where the resources live, and emit a device
            // colour. Only ICCBased stays component-approximated (profiles
            // are a documented policy deferral).
            ColorSpace::Named(id) => {
                if let Some(cs) = self.resolve_named_indexed(*id) {
                    // The operand is a palette index; look it up through the
                    // base space to a device colour.
                    let index = nums.first().copied().unwrap_or(0.0) as f32;
                    let rgba = cs_to_rgba(&cs, &[index]);
                    SemColor::DeviceRgb(rgba[0] as f64, rgba[1] as f64, rgba[2] as f64)
                } else {
                    match self.resolve_tint_space(*id) {
                        Some(space) => self.eval_tint(&space, &nums),
                        None => match self.resolve_cie_space(*id) {
                            Some(cie) => eval_cie(cie, &nums),
                            // An `/ICCBased` CMYK space carries a press profile
                            // PDFium runs through LittleCMS; evaluate the four
                            // operands through it rather than the frozen table.
                            None => match self.iccbased_cmyk_space(*id) {
                                Some(prof) if nums.len() >= 4 => {
                                    let [r, g, b] = prof.to_srgb([
                                        nums[0] as f32,
                                        nums[1] as f32,
                                        nums[2] as f32,
                                        nums[3] as f32,
                                    ]);
                                    SemColor::DeviceRgb(r as f64, g as f64, b as f64)
                                }
                                _ => SemColor::Components {
                                    space: *id,
                                    values: nums,
                                },
                            },
                        },
                    }
                }
            }
            ColorSpace::Pattern(_) => {
                let name = self
                    .operands
                    .iter()
                    .rev()
                    .find_map(Operand::as_name)
                    .map(|b| self.intern(b));
                // The numeric operands preceding the pattern name are the
                // underlying color for an uncolored (PaintType 2) pattern.
                match name {
                    Some(name) => self
                        .resolve_pattern(name, &nums)
                        .unwrap_or(SemColor::Pattern { name, under: None }),
                    None => SemColor::DeviceGray(0.0),
                }
            }
        };
        if stroke {
            self.gs.stroke_color = color.clone();
            self.push_color_op(SemanticOp::SetStrokeColor(color));
        } else {
            self.gs.fill_color = color.clone();
            self.push_color_op(SemanticOp::SetFillColor(color));
        }
    }

    fn op_dash(&mut self) {
        // Operands: [array] phase
        let phase = self.last_num().unwrap_or(0.0);
        let pattern = self
            .operands
            .iter()
            .find_map(Operand::as_array)
            .map(|a| a.iter().filter_map(Operand::as_f64).collect::<Vec<_>>())
            .unwrap_or_default();
        self.gs.dash_pattern = pattern.clone();
        self.gs.dash_phase = phase;
        self.ops.push(SemanticOp::SetDash { pattern, phase });
    }

    // --- path painting -----------------------------------------------------

    fn paint(&mut self, close: bool, fill: Option<FillRule>, stroke: bool) {
        if close {
            self.path.close();
        }
        let clip = self.pending_clip.take();
        let path_data = self.path.take();
        let needs_handle = fill.is_some() || stroke || clip.is_some();
        if !needs_handle {
            return; // `n` with no pending clip: discard the path.
        }
        let pid = self.intern_path(path_data);
        // An OC-hidden span paints nothing, but its `W … n` clips still
        // apply — clipping is state, not marks (§8.11.3.2).
        if !self.oc_hidden() {
            match (fill, stroke) {
                (Some(rule), false) => self.ops.push(SemanticOp::Fill { path: pid, rule }),
                (None, true) => self.ops.push(SemanticOp::Stroke { path: pid }),
                (Some(rule), true) => self.ops.push(SemanticOp::FillStroke { path: pid, rule }),
                (None, false) => {}
            }
        }
        if let Some(rule) = clip {
            self.ops.push(SemanticOp::Clip { path: pid, rule });
            self.gs.clip_depth += 1;
        }
    }

    /// Whether painting is currently suppressed by an enclosing OC-hidden
    /// marked-content span.
    fn oc_hidden(&self) -> bool {
        self.oc_hidden_count > 0
    }

    /// Whether the `BDC` whose operands are on the stack opens a *hidden*
    /// span: tag `/OC` and a properties argument naming (via the
    /// `/Properties` resource sub-dictionary) an OCG/OCMD that is OFF. An
    /// inline-dictionary argument cannot carry references, so it can never
    /// name a configured-off group — visible.
    fn bdc_hides(&mut self) -> bool {
        // Operands: … tag properties. The tag is the second-to-last name.
        let n = self.operands.len();
        if n < 2 {
            return false;
        }
        let Some(tag) = self.operands[n - 2].as_name() else {
            return false;
        };
        if tag != b"OC" {
            return false;
        }
        let Some(prop_name) = self.operands[n - 1].as_name().map(<[u8]>::to_vec) else {
            return false;
        };
        let nid = self.intern(&prop_name);
        let Some(entry) = self
            .resource_subdict(self.intern(b"Properties"))
            .and_then(|d| d.as_dict().and_then(|d| d.get(nid)).cloned())
        else {
            return false;
        };
        !self.oc_value_visible(&entry)
    }

    /// Replacement text carried by the properties operand of the current
    /// `BDC`. Both an inline dictionary and a named `/Properties` resource are
    /// valid (§14.6). Each BDC scope gets a distinct identity.
    fn bdc_actual_text(&mut self) -> Option<ActualTextSpan> {
        let properties = self.operands.last()?.clone();
        let bytes = match properties {
            Operand::Dict(entries) => entries
                .iter()
                .rev()
                .find(|(key, _)| key.as_slice() == b"ActualText")
                .and_then(|(_, value)| value.as_string())
                .map(<[u8]>::to_vec),
            Operand::Name(name) => {
                let property_name = self.intern(&name);
                let category = self.intern(b"Properties");
                let entry = self.resource_subdict(category).and_then(|properties| {
                    properties
                        .as_dict()
                        .and_then(|dict| dict.get(property_name))
                        .cloned()
                })?;
                let object = self.resolve_obj(&entry)?;
                let dict = object.as_dict()?;
                match dict.get(self.intern(b"ActualText")) {
                    Some(PdfObject::String(value)) => Some(value.0.to_vec()),
                    _ => None,
                }
            }
            _ => None,
        }?;
        let utf16 = decode_pdf_text_string(&bytes);
        if utf16.is_empty() {
            return None;
        }
        let id = ActualTextSpanId(self.next_actual_text_span);
        self.next_actual_text_span = self.next_actual_text_span.saturating_add(1);
        Some(ActualTextSpan {
            id,
            utf16: utf16.into(),
        })
    }

    fn active_actual_text(&self) -> Option<ActualTextSpan> {
        self.mc_stack
            .iter()
            .rev()
            .find_map(|frame| frame.actual_text.clone())
    }

    /// Visibility of a `/OC` value (an OCG or OCMD, usually by reference)
    /// under the document's default configuration. Anything unresolvable or
    /// unconfigured is visible — default-visibility semantics
    /// (cpdf_occontext.cpp).
    fn oc_value_visible(&mut self, value: &PdfObject) -> bool {
        let id = value.as_reference();
        let Some(resolved) = self.resolve_obj(value) else {
            return true;
        };
        let Some(dict) = resolved.as_dict() else {
            return true;
        };
        let type_is_ocmd = dict
            .get(self.names.known.type_)
            .and_then(PdfObject::as_name)
            .is_some_and(|n| self.names.resolve(n).as_ref() == b"OCMD");
        if type_is_ocmd {
            // Membership dictionary: visible if ANY listed OCG is on (the
            // /P /AnyOn default; the simple /VE Any policy). No /OCGs →
            // visible.
            let Some(ocgs_raw) = dict.get(self.intern(b"OCGs")).cloned() else {
                return true;
            };
            let mut ids: Vec<ObjectId> = Vec::new();
            match &ocgs_raw {
                PdfObject::Reference(_) => {
                    // A single OCG ref, or a ref to the array itself.
                    if let Some(arr) = self.resolve_obj(&ocgs_raw)
                        && let PdfObject::Array(items) = &*arr
                    {
                        ids.extend(items.iter().filter_map(PdfObject::as_reference));
                    } else if let Some(id) = ocgs_raw.as_reference() {
                        ids.push(id);
                    }
                }
                PdfObject::Array(items) => {
                    ids.extend(items.iter().filter_map(PdfObject::as_reference));
                }
                _ => {}
            }
            if ids.is_empty() {
                return true;
            }
            return ids.iter().any(|id| self.snapshot.ocg_visible(*id));
        }
        // A plain OCG: identity is its object id (matching /ON //OFF refs);
        // a direct dictionary has none and cannot be configured off.
        id.is_none_or(|id| self.snapshot.ocg_visible(id))
    }

    fn intern_path(&mut self, path: PathData) -> PathId {
        let id = PathId(self.paths.len() as u32);
        self.paths.push(path);
        id
    }

    // --- text --------------------------------------------------------------

    fn text_move(&mut self, tx: f64, ty: f64) {
        let t = Matrix::translate(tx, ty);
        self.text_line_matrix = t.then(self.text_line_matrix);
        self.text_matrix = self.text_line_matrix;
    }

    fn op_set_font(&mut self) {
        // Operands: /Name size
        let size = self.last_num().unwrap_or(0.0);
        let Some(name) = self
            .operands
            .iter()
            .find_map(Operand::as_name)
            .map(|b| b.to_vec())
        else {
            return;
        };
        let font = self.lookup_font(&name);
        self.gs.font = Some(font);
        self.gs.font_size = size;
    }

    fn op_show_array(&mut self) {
        let Some(array) = self.operands.last().and_then(Operand::as_array) else {
            return;
        };
        let mut elements = Vec::new();
        for item in array {
            match item {
                Operand::String(s) => elements.push(TextElement::Show(s.clone())),
                other => {
                    if let Some(n) = other.as_f64() {
                        elements.push(TextElement::Adjust(n));
                    }
                }
            }
        }
        self.emit_text_run(elements);
    }

    fn emit_text_run(&mut self, elements: Vec<TextElement>) {
        let Some(font) = self.gs.font else {
            // Showing text with no selected font: nothing placeable.
            return;
        };
        let type3 = self.fonts[font.index()].subtype == b"Type3";
        let visible = !self.oc_hidden();
        let run = TextRun {
            font,
            font_size: self.gs.font_size,
            render_mode: self.gs.render_mode,
            char_spacing: self.gs.char_spacing,
            word_spacing: self.gs.word_spacing,
            horizontal_scale: self.gs.horizontal_scale,
            rise: self.gs.rise,
            text_matrix: self.text_matrix,
            elements: elements.clone(),
            visible: visible && !type3,
            actual_text: self.active_actual_text(),
        };
        let id = TextRunId(self.text_runs.len() as u32);
        self.text_runs.push(run);
        self.ops.push(SemanticOp::ShowText(id));

        // Type 3 glyphs paint through CharProc geometry. Keep the ShowText
        // operation above for extraction, then use the established renderer
        // path (which also advances the text matrix).
        if type3 {
            self.show_type3(font, &elements);
            return;
        }
        if visible {
            // Clip render modes (Tr 4 fill+clip, 5 stroke+clip, 6 fill+stroke+clip,
            // 7 clip only) additionally add this run's glyph outlines to the text
            // object's clipping path, flushed at `ET` (ISO 32000-1 §9.4.3). The
            // `ShowText` above still carries the paint for modes 4–6; mode 7 paints
            // nothing (skipped by the backend) but still contributes to the clip.
            if matches!(self.gs.render_mode, 4 | 5 | 6 | 7) {
                self.text_clip_runs.push(id);
            }
        }

        // Advance the text line position by the run's total displacement, so a
        // following show operator starts where this one ended (ISO 32000-1
        // §9.4.4). The displacement is the running sum of glyph advances —
        // width·Tfs/1000 + Tc (+ Tw for a word-spacing code) — plus the explicit
        // `TJ` adjustments (−n/1000·Tfs), all scaled by Th. This mirrors
        // `lower::placed_glyphs`, which lays out glyphs *within* a run from the
        // same quantities; without it, consecutive `Tj`/`TJ` operators that rely
        // on the viewer to advance (rather than repositioning with `Td`/`Tm`)
        // pile every run onto the same origin.
        let th = self.gs.horizontal_scale / 100.0;
        let fs = self.gs.font_size;
        let metrics = self.fonts[font.index()].metrics.clone();
        // Vertical writing (wmode 1): displacement runs along −y using the
        // `/W2`//`/DW2` metrics, and neither the advance nor the `TJ`
        // adjustments are scaled by Th (which only scales horizontally —
        // §9.4.4 "in vertical writing mode, tx = 0, ty = …").
        let vertical = metrics.is_vertical();
        let (mut tx, mut ty) = (0.0f64, 0.0f64);
        for e in &elements {
            match e {
                TextElement::Show(bytes) => {
                    for dc in metrics.decode(bytes) {
                        let word = if dc.word_space {
                            self.gs.word_spacing
                        } else {
                            0.0
                        };
                        if let Some(vp) = vertical.then(|| metrics.vertical(dc.cid)).flatten() {
                            let w1 = vp.advance as f64 / 1000.0 * fs;
                            ty += w1 + self.gs.char_spacing + word;
                        } else {
                            let w = dc.advance as f64 / 1000.0 * fs;
                            tx += (w + self.gs.char_spacing + word) * th;
                        }
                    }
                }
                TextElement::Adjust(n) => {
                    if vertical {
                        ty += -n / 1000.0 * fs;
                    } else {
                        tx += -n / 1000.0 * fs * th;
                    }
                }
            }
        }
        if tx != 0.0 || ty != 0.0 {
            self.text_matrix = Matrix::translate(tx, ty).then(self.text_matrix);
        }
    }

    /// Show a run of Type 3 text: execute each code's CharProc content stream
    /// inline (bracketed by Save/Concat/Restore, like a form), then advance the
    /// text matrix by the glyph's width (ISO 32000-1 §9.6.5).
    ///
    /// The rendering matrix for a glyph is `FontMatrix × Trm`, where `Trm` is
    /// the text-space→user-space matrix `[Tfs·Th 0 0 Tfs 0 Ts] × Tm`; the
    /// ambient CTM is carried by the surrounding `Concat` scope, so the Concat
    /// we emit is only the glyph-space→current-user-space part.
    fn show_type3(&mut self, font: FontId, elements: &[TextElement]) {
        let resource_name = self.fonts[font.index()].resource_name;
        let Some(t3) = self.type3_cache.get(&resource_name).cloned().flatten() else {
            // A Type 3 font whose CharProcs/FontMatrix could not be parsed:
            // there is nothing placeable and no width data to advance by.
            return;
        };
        let tfs = self.gs.font_size;
        let th = self.gs.horizontal_scale / 100.0;
        let trise = self.gs.rise;
        let tc = self.gs.char_spacing;
        let tw = self.gs.word_spacing;
        // Render modes 3 (invisible) and 7 (clip-only) place no marks but still
        // advance the text position; so does an OC-hidden span.
        let invisible = self.gs.render_mode == 3 || self.gs.render_mode == 7 || self.oc_hidden();
        let fm = t3.font_matrix;
        let param = Matrix {
            a: tfs * th,
            b: 0.0,
            c: 0.0,
            d: tfs,
            e: 0.0,
            f: trise,
        };

        for element in elements {
            match element {
                TextElement::Adjust(n) => {
                    // A TJ adjustment moves the next glyph; thousandths of a
                    // text-space unit, negative moves forward.
                    let adj = -n / 1000.0 * tfs * th;
                    self.text_matrix = Matrix::translate(adj, 0.0).then(self.text_matrix);
                }
                TextElement::Show(bytes) => {
                    for &code in bytes.iter() {
                        // Glyph space → current user space (CTM applied by the
                        // enclosing scope): FontMatrix, then the text params,
                        // then the current text matrix.
                        let concat = fm.then(param).then(self.text_matrix);
                        let mut d_width = None;
                        if !invisible {
                            if let Some(stream) = t3.char_procs[code as usize].clone() {
                                d_width =
                                    self.exec_char_proc(&stream, concat, t3.resources.clone());
                            } else {
                                self.ctx.note_recovery(format!(
                                    "Type 3 glyph for code {code} has no CharProc; advancing only"
                                ));
                            }
                        }
                        // Advance: `/Widths` is authoritative (what viewers
                        // use); the `d0`/`d1` `wx` is a fallback when it is
                        // absent. Widths are in glyph space, so the horizontal
                        // text-space advance is `FontMatrix.a · wx`.
                        let wx = if t3.has_widths {
                            t3.widths[code as usize]
                        } else {
                            d_width.unwrap_or(0.0)
                        };
                        let w0 = fm.a * wx;
                        let mut adv = w0 * tfs + tc;
                        // Word spacing applies to the single-byte code 32.
                        if code == 32 {
                            adv += tw;
                        }
                        adv *= th;
                        self.text_matrix = Matrix::translate(adv, 0.0).then(self.text_matrix);
                    }
                }
            }
        }
    }

    /// Execute one Type 3 CharProc content stream inline, bracketed by
    /// Save/Concat/Restore. Returns the `wx` from a leading `d0`/`d1`, if any
    /// (an advance fallback). Recursion-guarded by the shared invoke depth so a
    /// glyph that shows its own font terminates.
    fn exec_char_proc(
        &mut self,
        stream: &PdfStream,
        concat: Matrix,
        resources: Option<Arc<PdfObject>>,
    ) -> Option<f64> {
        if self.invoke_depth >= self.limits.max_invoke_depth {
            self.ctx
                .note_recovery("Type 3 glyph exceeded recursion depth; glyph skipped".to_string());
            return None;
        }
        let content = match self.snapshot.decode_stream_data(stream, self.ctx) {
            Ok(c) => c,
            Err(_) => {
                self.ctx
                    .note_recovery("Type 3 CharProc stream undecodable; glyph skipped".to_string());
                return None;
            }
        };

        // 1. Save + Concat the glyph-space transform. The wrapper save is a
        // hard floor for the nested stream: a malformed leading `Q` cannot
        // consume it, and any trailing unmatched `q` is unwound below.
        let saved_floor = self.gs_stack_floor;
        self.gs_stack.push(self.gs.clone());
        self.ops.push(SemanticOp::Save);
        self.gs_stack_floor = self.gs_stack.len();
        if concat != Matrix::IDENTITY {
            self.gs.ctm = concat.then(self.gs.ctm);
            self.ops.push(SemanticOp::Concat(concat));
        }

        // 2. Scope the Type 3 flags to this CharProc: each glyph decides its
        // own coloured/shape-only status via its own d0/d1, and each captures
        // its own advance width.
        let saved_shape = self.type3_shape_only;
        let saved_d_width = self.type3_d_width.take();
        let saved_res = self.resources.clone();
        let saved_operands = std::mem::take(&mut self.operands);
        // A CharProc may open its own BT/ET text object; isolate the outer
        // run's text matrices so a nested glyph cannot move the caller's pen.
        let saved_tm = self.text_matrix;
        let saved_tlm = self.text_line_matrix;
        let saved_caches = self.take_resource_caches();
        // Patterns referenced by the CharProc anchor to the glyph's default
        // space (the CTM here includes the font matrix and text placement).
        let saved_pattern_base = self.pattern_base;
        self.pattern_base = self.gs.ctm;
        self.type3_shape_only = false;
        self.resources = resources;
        self.invoke_depth += 1;
        self.type3_nesting += 1;

        // 3. Execute (a malformed CharProc is a skipped glyph, never fatal).
        self.ops
            .push(SemanticOp::BeginPaintOrigin(PaintOrigin::Type3Glyph));
        if let Err(e) = self.run(&content) {
            self.ctx
                .note_recovery(format!("Type 3 CharProc aborted: {e}"));
        }
        self.ops.push(SemanticOp::EndPaintOrigin);

        self.type3_nesting -= 1;
        self.invoke_depth -= 1;
        self.resources = saved_res;
        self.pattern_base = saved_pattern_base;
        self.restore_resource_caches(saved_caches);
        self.operands = saved_operands;
        self.text_matrix = saved_tm;
        self.text_line_matrix = saved_tlm;
        let captured = self.type3_d_width;
        self.type3_shape_only = saved_shape;
        self.type3_d_width = saved_d_width;

        // 4. Close any `q` scopes the malformed CharProc left open, then
        // restore the wrapper. This keeps both the interpreter state and the
        // emitted Save/Restore stream balanced; otherwise the glyph transform
        // leaks into every following text run.
        while self.gs_stack.len() > self.gs_stack_floor {
            if let Some(prev) = self.gs_stack.pop() {
                self.gs = prev;
                self.ops.push(SemanticOp::Restore);
            }
        }
        self.gs_stack_floor = saved_floor;
        if let Some(prev) = self.gs_stack.pop() {
            self.gs = prev;
            self.ops.push(SemanticOp::Restore);
        }
        captured
    }

    /// Parse a `/Type3` font dictionary into a reusable [`Type3Font`]: its
    /// FontMatrix, CharProc execution resources, per-code CharProc streams
    /// (resolved through `/Encoding` + `/Differences`), and per-code glyph-space
    /// widths (ISO 32000-1 §9.6.5).
    fn build_type3(&mut self, dict: &Dictionary) -> Option<Arc<Type3Font>> {
        let font_matrix = read_matrix(dict, self.intern(b"FontMatrix"));
        // CharProc execution uses the font's own /Resources, falling back to
        // the resources active where the font is used.
        let resources = match dict.get(self.intern(b"Resources")).cloned() {
            Some(res) => self.resolve_obj(&res),
            None => self.resources.clone(),
        };

        // Per-code glyph names come from /Encoding (/Differences overrides,
        // else the base encoding's names). Type 3 goes code → NAME →
        // /CharProcs, never through Unicode.
        let (base, diffs) = self.read_encoding(dict, false, None);

        // Widths are in glyph space (NOT divided by 1000).
        let first_char = dict
            .get(self.intern(b"FirstChar"))
            .and_then(PdfObject::as_int)
            .unwrap_or(0);
        let width_vals: Vec<f64> = match dict.get(self.intern(b"Widths")).cloned() {
            Some(w) => match self.resolve_obj(&w).as_deref() {
                Some(PdfObject::Array(items)) => {
                    items.iter().map(|it| self.number_or_resolved(it)).collect()
                }
                _ => Vec::new(),
            },
            None => Vec::new(),
        };
        let has_widths = !width_vals.is_empty();
        let mut widths = [0.0f64; 256];
        for (i, w) in width_vals.iter().enumerate() {
            let code = first_char + i as i64;
            if (0..256).contains(&code) {
                widths[code as usize] = *w;
            }
        }

        // Resolve each code's CharProc content stream once.
        let char_procs_dict = dict
            .get(self.intern(b"CharProcs"))
            .cloned()
            .and_then(|c| self.resolve_obj(&c))
            .and_then(|o| o.as_dict().cloned());
        let mut char_procs: [Option<Arc<PdfStream>>; 256] = std::array::from_fn(|_| None);
        if let Some(cp) = char_procs_dict {
            for code in 0u32..256 {
                let name = diffs
                    .get(&(code as u8))
                    .map(|n| n.as_slice())
                    .or_else(|| pdf_font::builtin_glyph_name(base, code as u8));
                let Some(name) = name else { continue };
                let nid = self.intern(name);
                let Some(entry) = cp.get(nid).cloned() else {
                    continue;
                };
                if let Some(obj) = self.resolve_obj(&entry)
                    && let PdfObject::Stream(s) = &*obj
                {
                    char_procs[code as usize] = Some(s.clone());
                }
            }
        }

        Some(Arc::new(Type3Font {
            font_matrix,
            resources,
            char_procs,
            widths,
            has_widths,
        }))
    }

    fn lookup_font(&mut self, name: &[u8]) -> FontId {
        let nid = self.intern(name);
        if let Some(fid) = self.font_cache.get(&nid) {
            return *fid;
        }
        let sem_font = self.resolve_font(nid);
        let fid = FontId(self.fonts.len() as u32);
        self.fonts.push(sem_font);
        self.font_cache.insert(nid, fid);
        fid
    }

    /// Best-effort resolution of a font resource into a [`SemFont`]. Any
    /// failure degrades to an unresolved descriptor rather than aborting the
    /// page (a missing font must not blank the whole page).
    fn resolve_font(&mut self, name: NameId) -> SemFont {
        let mut sem = SemFont {
            resource_name: name,
            object: None,
            subtype: Vec::new(),
            base_font: Vec::new(),
            metrics: Arc::new(pdf_font::FontMetrics::Simple(
                pdf_font::SimpleWidths::empty(),
            )),
            program: None,
            face_index: 0,
            glyph_map: Arc::new(pdf_font::GlyphMap::Identity),
            synthesis: pdf_font::Synthesis::default(),
            unicode_map: Arc::new(pdf_font::UnicodeMap::new()),
            charset: pdf_font::Charset::Ansi,
            font_bbox: None,
            type3_matrix: None,
        };
        let Some(font_dict) = self.resource_subdict(self.names.known.font) else {
            return sem;
        };
        let Some(entry) = font_dict.as_dict().and_then(|d| d.get(name)).cloned() else {
            return sem;
        };
        sem.object = entry.as_reference();
        let Some(font_obj) = self.resolve_obj(&entry) else {
            return sem;
        };
        let Some(dict) = font_obj.as_dict() else {
            return sem;
        };
        if let Some(sub) = dict
            .get(self.names.known.subtype)
            .and_then(PdfObject::as_name)
        {
            sem.subtype = self.names.resolve(sub).to_vec();
        }
        let base_key = self.intern(b"BaseFont");
        if let Some(base) = dict.get(base_key).and_then(PdfObject::as_name) {
            sem.base_font = self.names.resolve(base).to_vec();
        }
        sem.font_bbox = self.resolve_font_bbox(dict, sem.subtype == b"Type0");
        let mut unicode_map = self.to_unicode_map(dict);
        // Type 3: no outline program. Parse the CharProcs/FontMatrix/Encoding/
        // Widths into a side table keyed by resource name; showing text draws
        // each glyph's content stream inline (§9.6.5). The SemFont stays
        // program-less — nothing places it as a glyph run.
        if sem.subtype == b"Type3" {
            self.fill_simple_unicode_map(dict, None, &mut unicode_map);
            sem.unicode_map = Arc::new(unicode_map);
            let t3 = self.build_type3(dict);
            if let Some(type3) = &t3 {
                sem.type3_matrix = Some(type3.font_matrix);
                let widths = type3
                    .widths
                    .iter()
                    .map(|width| (width * type3.font_matrix.a * 1000.0) as f32)
                    .collect();
                sem.metrics = Arc::new(pdf_font::FontMetrics::Simple(pdf_font::SimpleWidths::new(
                    0, widths, 0.0,
                )));
            }
            self.type3_cache.insert(name, t3);
            return sem;
        }
        // Advance metrics: two-byte Identity CID for Type 0, else simple.
        if sem.subtype == b"Type0" {
            // The `/Encoding` CMap decodes byte strings into CIDs. Identity-H/V
            // (the dominant embedded-subset case) keeps the fast two-byte path;
            // a predefined CJK CMap name or an embedded CMap stream routes
            // through the CMap machinery (variable-width codes → CID).
            let widths = self.build_cid_widths(dict);
            let (cmap, vertical) = self.resolve_cid_encoding(dict);
            sem.charset = self
                .descendant_cidfont(dict)
                .and_then(|font| font.as_dict().cloned())
                .and_then(|font| self.cid_ordering(&font))
                .map(|ordering| pdf_font::Charset::from_ordering(&ordering))
                .filter(|charset| charset.is_cjk())
                .unwrap_or_else(|| pdf_font::Charset::from_font_name(&sem.base_font));
            sem.metrics = Arc::new(if vertical {
                // Writing mode 1 (`Identity-V` or a `/WMode 1` CMap): decode
                // as usual, lay out along −y with the `/W2`//`/DW2` metrics.
                pdf_font::FontMetrics::CidVertical {
                    widths,
                    vertical: self.build_cid_vertical(dict),
                    cmap,
                }
            } else {
                match cmap {
                    Some(cmap) => pdf_font::FontMetrics::CidCMap { widths, cmap },
                    None => pdf_font::FontMetrics::CidIdentity(widths),
                }
            });
            let (program, cid_to_gid, face_index) = self.build_cid_program(dict);
            sem.program = program;
            sem.face_index = face_index;
            sem.glyph_map = Arc::new(pdf_font::GlyphMap::Cid(cid_to_gid));
        } else {
            // Simple font: resolve code→GID through the encoding and a font
            // program — the embedded one, or a bundled standard face chosen
            // by substitution (fonts.md Font Phase 3).
            let embedded = self
                .embedded_program(dict)
                .and_then(|bytes| pdf_font::FontProgram::parse(bytes.clone()).map(|p| (bytes, p)));
            let resolved = match embedded {
                Some((bytes, prog)) => Some((bytes, prog, None, 0)),
                None => {
                    let charset = pdf_font::Charset::from_font_name(&sem.base_font);
                    self.substitute_font(dict, &sem.base_font, charset)
                        .map(|s| {
                            // A bundled substitute may lack the requested cut
                            // (Symbol/ZapfDingbats): record what to synthesize.
                            if !s.system {
                                sem.synthesis =
                                    pdf_font::synthesis(s.face, s.want_bold, s.want_italic);
                            }
                            (s.bytes, s.program, Some(s.face), s.face_index)
                        })
                }
            };
            let widths = self.build_simple_widths(dict);
            if let Some((bytes, prog, face, face_index)) = resolved {
                self.fill_simple_unicode_map(dict, face, &mut unicode_map);
                let table = self.build_simple_gids(dict, &prog, face);
                // Last-resort Unicode: reverse the embedded cmap for any
                // code the encoding left unmapped (a symbolic Latin subset
                // with no /Encoding and no /ToUnicode —
                // PLAN-TEXT-EXTRACTION §5.2 step 4).
                self.fill_program_cmap_unicode(&prog, &table, &mut unicode_map);
                // A font with no /Widths uses the standard-14 metrics
                // (§9.6.2.1); the bundled faces are metric-compatible, so
                // their own advances are those metrics.
                if !widths.has_widths() && face.is_some() {
                    sem.metrics = Arc::new(pdf_font::FontMetrics::Simple(standard_widths(
                        &prog, &table,
                    )));
                } else {
                    sem.metrics = Arc::new(pdf_font::FontMetrics::Simple(widths));
                }
                sem.program = Some(bytes);
                sem.face_index = face_index;
                sem.glyph_map = Arc::new(pdf_font::GlyphMap::Simple(table));
            } else {
                self.fill_simple_unicode_map(dict, None, &mut unicode_map);
                sem.metrics = Arc::new(pdf_font::FontMetrics::Simple(widths));
            }
        }
        sem.unicode_map = Arc::new(unicode_map);
        sem
    }

    fn resolve_font_bbox(&mut self, font: &Dictionary, composite: bool) -> Option<[f64; 4]> {
        let target = if composite {
            self.descendant_cidfont(font)?
        } else {
            Arc::new(PdfObject::Dictionary(Arc::new(font.clone())))
        };
        let target = target.as_dict()?;
        if let Some(bounds) = read_rect(target, self.intern(b"FontBBox")) {
            return Some(bounds);
        }
        let descriptor = target.get(self.intern(b"FontDescriptor"))?.clone();
        let descriptor = self.resolve_obj(&descriptor)?;
        read_rect(descriptor.as_dict()?, self.intern(b"FontBBox"))
    }

    /// Resolve and parse the font's `/ToUnicode` stream while the snapshot is
    /// available. The semantic page owns the result and never borrows the
    /// document.
    fn to_unicode_map(&mut self, dict: &Dictionary) -> pdf_font::UnicodeMap {
        let key = self.intern(b"ToUnicode");
        let Some(value) = dict.get(key).cloned() else {
            return pdf_font::UnicodeMap::new();
        };
        let Some(object) = self.resolve_obj(&value) else {
            return pdf_font::UnicodeMap::new();
        };
        let PdfObject::Stream(stream) = &*object else {
            return pdf_font::UnicodeMap::new();
        };
        self.snapshot
            .decode_stream_data(stream, self.ctx)
            .ok()
            .and_then(|bytes| pdf_font::parse_to_unicode(&bytes))
            .unwrap_or_default()
    }

    /// Complete a simple font's Unicode map from `/Encoding` and glyph names.
    /// Existing `/ToUnicode` entries remain authoritative.
    fn fill_simple_unicode_map(
        &mut self,
        dict: &Dictionary,
        face: Option<pdf_font::StandardFont>,
        map: &mut pdf_font::UnicodeMap,
    ) {
        let symbolic = self.font_is_symbolic(dict);
        let builtin = match face {
            Some(pdf_font::StandardFont::Symbol) => Some(pdf_font::BaseEncoding::SymbolFont),
            Some(pdf_font::StandardFont::ZapfDingbats) => {
                Some(pdf_font::BaseEncoding::ZapfDingbats)
            }
            _ => None,
        };
        let (base, differences) = self.read_encoding(dict, symbolic, builtin);
        for code in 0u16..=255 {
            let code = code as u8;
            let named = differences
                .get(&code)
                .map(Vec::as_slice)
                .or_else(|| pdf_font::builtin_glyph_name(base, code));
            let unicode = named
                .and_then(pdf_font::glyph_name_to_char)
                .or_else(|| base.to_char(code));
            if let Some(ch) = unicode {
                let mut encoded = [0u16; 2];
                let units = ch.encode_utf16(&mut encoded);
                map.insert_if_absent(
                    code as u32,
                    Arc::<[u16]>::from(units),
                    pdf_font::UnicodeSource::SimpleEncoding,
                );
            }
        }
    }

    /// Fill any still-unmapped code from the embedded font program's cmap,
    /// reversed (PLAN-TEXT-EXTRACTION §5.2 step 4). `/ToUnicode` and the
    /// simple-encoding pass keep priority (`insert_if_absent`); this only
    /// rescues a font that names no usable encoding — the common Office
    /// "symbolic" Latin TrueType subset, whose `(3,0)` cmap still carries
    /// the glyph identities.
    fn fill_program_cmap_unicode(
        &self,
        prog: &pdf_font::FontProgram,
        table: &[u32; 256],
        map: &mut pdf_font::UnicodeMap,
    ) {
        for code in 0u32..256 {
            if map.get(code).is_some() {
                continue;
            }
            let gid = table[code as usize];
            if gid == 0 {
                continue;
            }
            if let Some(ch) = prog.char_for_gid(gid) {
                let mut buf = [0u16; 2];
                let units = ch.encode_utf16(&mut buf);
                map.insert_if_absent(
                    code,
                    Arc::<[u16]>::from(&units[..]),
                    pdf_font::UnicodeSource::FontProgram,
                );
            }
        }
    }

    /// Build a simple font's 256-entry code→GID table from its `/Encoding`
    /// (base + `/Differences`) resolved through the embedded program's cmap.
    fn build_simple_gids(
        &mut self,
        dict: &Dictionary,
        prog: &pdf_font::FontProgram,
        face: Option<pdf_font::StandardFont>,
    ) -> Box<[u32; 256]> {
        let symbolic = self.font_is_symbolic(dict);
        // A substituted symbolic face brings its own built-in encoding.
        let builtin = match face {
            Some(pdf_font::StandardFont::Symbol) => Some(pdf_font::BaseEncoding::SymbolFont),
            Some(pdf_font::StandardFont::ZapfDingbats) => {
                Some(pdf_font::BaseEncoding::ZapfDingbats)
            }
            _ => None,
        };
        let (base, differences) = self.read_encoding(dict, symbolic, builtin);

        let mut table = Box::new([0u32; 256]);
        for code in 0u32..256 {
            // A glyph name is authoritative: look it up in the font's `post`
            // names first (exact, and the only route for glyphs with no
            // Unicode), then fall back to name→Unicode→cmap. The name comes
            // from /Differences, else from a symbolic face's built-in
            // encoding.
            let named = differences
                .get(&(code as u8))
                .map(|n| n.as_slice())
                .or_else(|| pdf_font::builtin_glyph_name(base, code as u8));
            let by_name = named.and_then(|n| prog.gid_for_name(n));
            let ch = named
                .and_then(pdf_font::glyph_name_to_char)
                .or_else(|| base.to_char(code as u8));
            let gid = by_name.or_else(|| match ch {
                Some(c) => prog
                    .gid_for_char(c)
                    .or_else(|| symbolic.then(|| prog.gid_for_code(code as u8)).flatten()),
                None => symbolic.then(|| prog.gid_for_code(code as u8)).flatten(),
            });
            table[code as usize] = gid.unwrap_or(0);
        }
        table
    }

    /// A font is symbolic if its descriptor `/Flags` has the Symbolic bit (4).
    /// Pick a bundled standard face for a font the document did not embed
    /// (fonts.md Font Phase 3). Returns its bytes and parsed program.
    ///
    /// Substitution never fails, so non-embedded text renders as glyphs
    /// rather than placement boxes; only an unparseable bundled face (a
    /// build error) falls through to `None`.
    fn substitute_font(
        &mut self,
        dict: &Dictionary,
        base_font: &[u8],
        charset: pdf_font::Charset,
    ) -> Option<Substituted> {
        let descriptor = dict
            .get(self.intern(b"FontDescriptor"))
            .cloned()
            .and_then(|d| self.resolve_obj(&d))
            .and_then(|d| d.as_dict().cloned());
        let num = |d: &Dictionary, key: &[u8], this: &mut Self| -> Option<f64> {
            d.get(this.intern(key)).and_then(PdfObject::as_number)
        };
        let (flags, stem_v, italic_angle) = match &descriptor {
            Some(d) => (
                num(d, b"Flags", self).map(|f| f as u32),
                num(d, b"StemV", self),
                num(d, b"ItalicAngle", self),
            ),
            None => (None, None, None),
        };
        let (face, want_bold, want_italic) =
            pdf_font::substitute_with_style(pdf_font::SubstitutionRequest {
                base_font,
                flags,
                stem_v,
                italic_angle,
            });
        // An installed font that actually answers to this name beats a
        // metric-compatible stand-in — and is the only way to cover CJK or a
        // face outside the standard 14. Falls through to the bundled face.
        if let Some(found) = self.system_font_for(base_font, face, flags, italic_angle, charset) {
            return Some(found);
        }
        let bytes = face.program_data();
        let program = pdf_font::FontProgram::parse(bytes.clone())?;
        Some(Substituted {
            bytes,
            program,
            face,
            want_bold,
            want_italic,
            face_index: 0,
            system: false,
        })
    }

    /// Ask the injected system-font provider for `base_font`.
    ///
    /// Returns the bundled `face` alongside, since the caller still needs it
    /// to know whether a symbolic built-in encoding applies.
    fn system_font_for(
        &mut self,
        base_font: &[u8],
        face: pdf_font::StandardFont,
        flags: Option<u32>,
        italic_angle: Option<f64>,
        charset: pdf_font::Charset,
    ) -> Option<Substituted> {
        let provider = self.system_fonts.clone()?;
        let flags = flags.unwrap_or(0);
        let found = provider.lookup(&pdf_font::SystemFontRequest {
            family: pdf_font::strip_subset_tag(base_font),
            bold: face.is_bold(),
            italic: face.is_italic() || italic_angle.is_some_and(|a| a != 0.0),
            serif: flags & 0x2 != 0,
            fixed_pitch: flags & 0x1 != 0,
            charset,
        })?;
        // Face index matters: the CJK families ship as .ttc collections.
        let program = pdf_font::FontProgram::parse_indexed(found.data.clone(), found.index)?;
        // A system-provider face was selected BY the requested style, so
        // nothing is synthesized for it.
        Some(Substituted {
            bytes: found.data,
            program,
            face,
            want_bold: face.is_bold(),
            want_italic: face.is_italic(),
            face_index: found.index,
            system: true,
        })
    }

    fn font_is_symbolic(&mut self, dict: &Dictionary) -> bool {
        dict.get(self.intern(b"FontDescriptor"))
            .cloned()
            .and_then(|d| self.resolve_obj(&d))
            .and_then(|d| {
                d.as_dict()
                    .and_then(|d| d.get(self.intern(b"Flags")))
                    .and_then(PdfObject::as_int)
            })
            .map(|f| f & 0x4 != 0)
            .unwrap_or(false)
    }

    /// Read `/Encoding`: a named base, or a dictionary with `/BaseEncoding` and
    /// a `/Differences` array (code → glyph name overrides).
    /// `(base encoding, /Differences)` for a simple font. `builtin` is the
    /// encoding to assume when the font names none — for a substituted
    /// Symbol/ZapfDingbats face that is the face's own table (Annex D),
    /// which is the only way its codes reach glyphs.
    fn read_encoding(
        &mut self,
        dict: &Dictionary,
        symbolic: bool,
        builtin: Option<pdf_font::BaseEncoding>,
    ) -> (pdf_font::BaseEncoding, HashMap<u8, Vec<u8>>) {
        let default = builtin.unwrap_or(if symbolic {
            pdf_font::BaseEncoding::Symbolic
        } else {
            pdf_font::BaseEncoding::Standard
        });
        let mut diffs: HashMap<u8, Vec<u8>> = HashMap::new();

        let Some(enc) = dict.get(self.intern(b"Encoding")).cloned() else {
            return (default, diffs);
        };
        let enc = match self.resolve_obj(&enc) {
            Some(e) => e,
            None => return (default, diffs),
        };
        match &*enc {
            PdfObject::Name(n) => (
                pdf_font::BaseEncoding::from_name(&self.names.resolve(*n)).unwrap_or(default),
                diffs,
            ),
            PdfObject::Dictionary(ed) => {
                let base = ed
                    .get(self.intern(b"BaseEncoding"))
                    .and_then(PdfObject::as_name)
                    .and_then(|n| pdf_font::BaseEncoding::from_name(&self.names.resolve(n)))
                    .unwrap_or(default);
                if let Some(PdfObject::Array(items)) = ed
                    .get(self.intern(b"Differences"))
                    .cloned()
                    .and_then(|d| self.resolve_obj(&d))
                    .as_deref()
                {
                    let mut code = 0u32;
                    for item in items.iter() {
                        match item {
                            PdfObject::Integer(i) => code = (*i).max(0) as u32,
                            PdfObject::Name(n) => {
                                if code < 256 {
                                    diffs.insert(code as u8, self.names.resolve(*n).to_vec());
                                }
                                code += 1;
                            }
                            _ => {}
                        }
                    }
                }
                (base, diffs)
            }
            _ => (default, diffs),
        }
    }

    /// Load a composite font's embedded outline program (`/FontFile2` TrueType
    /// or `/FontFile3` CFF/OpenType from the descendant CIDFont's descriptor)
    /// and its `/CIDToGIDMap`.
    /// `(program, CID→GID map, face index)` for a Type 0 font's descendant.
    fn build_cid_program(
        &mut self,
        type0: &Dictionary,
    ) -> (Option<Arc<[u8]>>, pdf_font::CidToGid, u32) {
        let Some(cidfont) = self.descendant_cidfont(type0) else {
            return (None, pdf_font::CidToGid::Identity, 0);
        };
        let Some(dict) = cidfont.as_dict() else {
            return (None, pdf_font::CidToGid::Identity, 0);
        };
        let dict = &dict.clone();

        // /CIDToGIDMap: /Identity (or absent) → Identity; a stream → the map.
        // It is only meaningful for a CIDFontType2 (TrueType) descendant.
        let mut cid_to_gid = match dict.get(self.intern(b"CIDToGIDMap")).cloned() {
            Some(PdfObject::Name(_)) | None => pdf_font::CidToGid::Identity,
            Some(other) => match self.resolve_obj(&other).as_deref() {
                Some(PdfObject::Stream(s)) => match self.snapshot.decode_stream_data(s, self.ctx) {
                    Ok(bytes) => pdf_font::CidToGid::from_stream(&bytes),
                    Err(_) => pdf_font::CidToGid::Identity,
                },
                _ => pdf_font::CidToGid::Identity,
            },
        };

        let program = self.embedded_program(dict);
        if let Some(bytes) = &program {
            // A CIDFontType0 descendant embeds a CID-keyed CFF, whose glyphs
            // are addressed by GID while the PDF addresses them by CID. The
            // CFF's charset is the only mapping between the two (/CIDToGIDMap
            // is a CIDFontType2 feature), and it is *not* identity in a real
            // subset font — so without this the text draws wrong glyphs or
            // .notdef boxes.
            let subtype = dict
                .get(self.intern(b"Subtype"))
                .and_then(PdfObject::as_name)
                .map(|n| self.names.resolve(n).to_vec())
                .unwrap_or_default();
            if subtype == b"CIDFontType0"
                && let Some(map) = pdf_font::cid_to_gid_from_cff(bytes)
            {
                cid_to_gid = map;
            }
            return (program, cid_to_gid, 0);
        }
        // Not embedded. A CID font's repertoire comes from /CIDSystemInfo;
        // Adobe-Identity-0 says nothing (its CIDs are the original font's
        // glyph ids), so fall back to any CMap name the producer left in
        // /BaseFont — `DengXian-GBK-EUC-H-Identity-H` and friends.
        let base_font = dict
            .get(self.intern(b"BaseFont"))
            .and_then(PdfObject::as_name)
            .map(|n| self.names.resolve(n).to_vec())
            .unwrap_or_default();
        let charset = match self.cid_ordering(dict) {
            Some(ordering) if pdf_font::Charset::from_ordering(&ordering).is_cjk() => {
                pdf_font::Charset::from_ordering(&ordering)
            }
            _ => pdf_font::Charset::from_font_name(&base_font),
        };
        match self.substitute_font(dict, &base_font, charset) {
            Some(s) => {
                // A substitute face indexes glyphs by its own cmap, not by the
                // document's CIDs — an identity CID→GID into it draws
                // arbitrary glyphs. For a known CJK charset resolved against
                // an *installed* face (fonts.md Font Phase 7), bridge
                // CID → Unicode (transcribed Adobe-*-UCS2 tables) → the
                // face's charmap. Bundled faces are Latin-only, so the
                // provider-off default is untouched.
                if s.system
                    && charset.is_cjk()
                    && let Some(map) = pdf_font::CidToGid::from_unicode_bridge(charset, &s.program)
                {
                    cid_to_gid = map;
                }
                (Some(s.bytes), cid_to_gid, s.face_index)
            }
            None => (None, cid_to_gid, 0),
        }
    }

    /// Resolve a Type 0 font's `/Encoding` into the CMap that decodes its byte
    /// strings into CIDs (ISO 32000-1 §9.7.5). Returns:
    ///
    /// * `None` for `Identity-H`/`Identity-V` (code = CID, two bytes) — the
    ///   caller uses the fast [`FontMetrics::CidIdentity`] path.
    /// * a **predefined** CMap for a known charset name (`GBK-EUC-H`,
    ///   `UniGB-UCS2-H`, `90ms-RKSJ-H`, …), ported from PDFium's tables.
    /// * an **embedded** CMap parsed from an `/Encoding` stream (honoring an
    ///   in-stream `usecmap` of a predefined base).
    ///
    /// An unrecognized name or an unparseable stream returns `None`; the caller
    /// then falls back to Identity. A best-effort `/CIDSystemInfo /Ordering`
    /// note is emitted so the residual is visible (predefined-name coverage is
    /// PDFium-complete, so this fallback is rare).
    ///
    /// The second return is the writing mode: `true` for vertical (wmode 1 —
    /// `Identity-V`, a `-V` predefined CMap, or an embedded `/WMode 1` CMap).
    fn resolve_cid_encoding(&mut self, type0: &Dictionary) -> (Option<Arc<pdf_font::CMap>>, bool) {
        let Some(resolved) = type0
            .get(self.intern(b"Encoding"))
            .cloned()
            .and_then(|enc| self.resolve_obj(&enc))
        else {
            return (None, false);
        };
        match &*resolved {
            PdfObject::Name(n) => {
                let name = self.names.resolve(*n).to_vec();
                if name == b"Identity-H" {
                    return (None, false);
                }
                if name == b"Identity-V" {
                    return (None, true);
                }
                match pdf_font::predefined_cmap(&name) {
                    Some(cmap) => {
                        let vertical = cmap.wmode() == 1;
                        (Some(cmap), vertical)
                    }
                    None => {
                        let ordering = self.descendant_cidfont(type0).and_then(|c| {
                            c.as_dict()
                                .map(|d| d.clone())
                                .and_then(|d| self.cid_ordering(&d))
                        });
                        self.ctx.note_recovery(format!(
                            "unrecognized Type0 /Encoding /{} (Ordering {:?}); using Identity",
                            String::from_utf8_lossy(&name),
                            ordering.as_deref().map(String::from_utf8_lossy),
                        ));
                        (None, false)
                    }
                }
            }
            PdfObject::Stream(stream) => {
                let cmap = self
                    .snapshot
                    .decode_stream_data(stream, self.ctx)
                    .ok()
                    .and_then(|bytes| pdf_font::parse_embedded_cmap(&bytes));
                match cmap {
                    Some(cmap) => {
                        let vertical = cmap.wmode() == 1;
                        (Some(Arc::new(cmap)), vertical)
                    }
                    None => {
                        self.ctx.note_recovery(
                            "embedded Type0 /Encoding CMap did not parse; using Identity".into(),
                        );
                        (None, false)
                    }
                }
            }
            _ => (None, false),
        }
    }

    /// `/CIDSystemInfo /Ordering` of a descendant CIDFont.
    fn cid_ordering(&mut self, cidfont: &Dictionary) -> Option<Vec<u8>> {
        let info = self.resolve_obj(&cidfont.get(self.intern(b"CIDSystemInfo"))?.clone())?;
        let dict = info.as_dict()?;
        match dict.get(self.intern(b"Ordering"))? {
            PdfObject::String(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        }
    }

    /// Resolve a Type 0 font's one descendant CIDFont dictionary.
    fn descendant_cidfont(&mut self, type0: &Dictionary) -> Option<Arc<PdfObject>> {
        let arr = self.resolve_obj(&type0.get(self.intern(b"DescendantFonts"))?.clone())?;
        let first = match &*arr {
            PdfObject::Array(items) => items.first()?.clone(),
            _ => return None,
        };
        self.resolve_obj(&first)
    }

    /// Decode the `/FontFile2` or `/FontFile3` program from a font/CIDFont
    /// dictionary's `/FontDescriptor`.
    fn embedded_program(&mut self, dict: &Dictionary) -> Option<Arc<[u8]>> {
        let descriptor = self.resolve_obj(&dict.get(self.intern(b"FontDescriptor"))?.clone())?;
        let descriptor = descriptor.as_dict()?;
        // /FontFile2 (TrueType) and /FontFile3 (CFF/OpenType) go to Skrifa;
        // /FontFile is bare Type 1, which the native interpreter handles
        // (fonts.md Font Phase 5). `FontProgram::parse` picks the engine.
        let file = descriptor
            .get(self.intern(b"FontFile2"))
            .or_else(|| descriptor.get(self.intern(b"FontFile3")))
            .or_else(|| descriptor.get(self.intern(b"FontFile")))?
            .clone();
        let obj = self.resolve_obj(&file)?;
        let PdfObject::Stream(stream) = &*obj else {
            return None;
        };
        let bytes = self.snapshot.decode_stream_data(stream, self.ctx).ok()?;
        Some(Arc::from(bytes))
    }

    /// Read the descendant CIDFont's `/DW` + `/W` into CID advance widths.
    fn build_cid_widths(&mut self, type0: &Dictionary) -> pdf_font::CidWidths {
        // /DescendantFonts is a one-element array of the CIDFont (usually a ref).
        let Some(cidfont) = type0
            .get(self.intern(b"DescendantFonts"))
            .cloned()
            .and_then(|d| self.resolve_obj(&d))
            .and_then(|arr| match &*arr {
                PdfObject::Array(items) => items.first().cloned(),
                _ => None,
            })
            .and_then(|f| self.resolve_obj(&f))
        else {
            return pdf_font::CidWidths::uniform(1000.0);
        };
        let Some(dict) = cidfont.as_dict() else {
            return pdf_font::CidWidths::uniform(1000.0);
        };

        let dw = dict
            .get(self.intern(b"DW"))
            .and_then(PdfObject::as_number)
            .unwrap_or(1000.0) as f32;
        let ranges = match dict
            .get(self.intern(b"W"))
            .cloned()
            .and_then(|w| self.resolve_obj(&w))
        {
            Some(w) => match &*w {
                PdfObject::Array(items) => parse_cid_w(items),
                _ => Vec::new(),
            },
            None => Vec::new(),
        };
        pdf_font::CidWidths::new(dw, ranges)
    }

    /// Read the descendant CIDFont's `/DW2` + `/W2` into vertical metrics
    /// (ISO 32000-1 §9.7.4.3), mirroring [`Self::build_cid_widths`]. Spec
    /// defaults (`/DW2 [880 −1000]`, `vx = w0/2`) when absent.
    fn build_cid_vertical(&mut self, type0: &Dictionary) -> pdf_font::CidVerticalMetrics {
        let Some(cidfont) = self.descendant_cidfont(type0) else {
            return pdf_font::CidVerticalMetrics::default_metrics();
        };
        let Some(dict) = cidfont.as_dict() else {
            return pdf_font::CidVerticalMetrics::default_metrics();
        };
        let dict = dict.clone();
        let (mut vy, mut w1y) = (880.0f32, -1000.0f32);
        if let Some(dw2) = dict
            .get(self.intern(b"DW2"))
            .cloned()
            .and_then(|d| self.resolve_obj(&d))
            && let PdfObject::Array(a) = &*dw2
        {
            if let Some(v) = a.first().and_then(PdfObject::as_number) {
                vy = v as f32;
            }
            if let Some(v) = a.get(1).and_then(PdfObject::as_number) {
                w1y = v as f32;
            }
        }
        let ranges = match dict
            .get(self.intern(b"W2"))
            .cloned()
            .and_then(|w| self.resolve_obj(&w))
        {
            Some(w) => match &*w {
                PdfObject::Array(items) => parse_cid_w2(items),
                _ => Vec::new(),
            },
            None => Vec::new(),
        };
        pdf_font::CidVerticalMetrics::new(vy, w1y, ranges)
    }

    /// Read `/FirstChar` + `/Widths` (+ descriptor `/MissingWidth`) into a
    /// simple-font advance table.
    fn build_simple_widths(&mut self, dict: &Dictionary) -> pdf_font::SimpleWidths {
        let first_char = dict
            .get(self.intern(b"FirstChar"))
            .and_then(PdfObject::as_int)
            .unwrap_or(0)
            .max(0) as u32;

        let widths = match dict.get(self.intern(b"Widths")).cloned() {
            Some(w) => match self.resolve_obj(&w).as_deref() {
                Some(PdfObject::Array(items)) => items
                    .iter()
                    .map(|it| self.number_or_resolved(it) as f32)
                    .collect(),
                _ => Vec::new(),
            },
            None => Vec::new(),
        };

        // /MissingWidth lives on the font descriptor.
        let missing = dict
            .get(self.intern(b"FontDescriptor"))
            .cloned()
            .and_then(|d| self.resolve_obj(&d))
            .and_then(|d| {
                d.as_dict()
                    .and_then(|d| d.get(self.intern(b"MissingWidth")))
                    .and_then(PdfObject::as_number)
            })
            .unwrap_or(0.0) as f32;

        pdf_font::SimpleWidths::new(first_char, widths, missing)
    }

    /// A number, resolving an indirect reference (some producers store per-glyph
    /// widths as references); non-numbers become 0.
    fn number_or_resolved(&mut self, obj: &PdfObject) -> f64 {
        if let Some(n) = obj.as_number() {
            return n;
        }
        self.resolve_obj(obj)
            .and_then(|o| o.as_number())
            .unwrap_or(0.0)
    }

    // --- XObjects / inline images ------------------------------------------

    /// `Do`: invoke an XObject. Form XObjects execute inline (their operations
    /// flattened into this op stream, wrapped in Save/Concat/BBox-clip/Restore,
    /// recursion-guarded); image XObjects emit a `DrawImage`.
    fn op_do(&mut self) -> Result<(), ContentError> {
        let Some(name) = self
            .operands
            .iter()
            .rev()
            .find_map(Operand::as_name)
            .map(<[u8]>::to_vec)
        else {
            return Ok(());
        };
        let nid = self.intern(&name);
        let Some(xobjects) = self.resource_subdict(self.names.known.x_object) else {
            return Ok(());
        };
        let Some(entry) = xobjects.as_dict().and_then(|d| d.get(nid)).cloned() else {
            return Ok(());
        };
        let obj_id = entry.as_reference();
        let Some(xobj) = self.resolve_obj(&entry) else {
            return Ok(());
        };
        let PdfObject::Stream(stream) = &*xobj else {
            return Ok(());
        };
        let stream = stream.clone();
        // Optional content: skip the XObject inside an OC-hidden span, and
        // honor its own `/OC` membership (§8.11.3.3) — images and forms both.
        if self.oc_hidden() {
            return Ok(());
        }
        if let Some(oc) = stream.dict.get(self.intern(b"OC")).cloned()
            && !self.oc_value_visible(&oc)
        {
            return Ok(());
        }
        let subtype = stream
            .dict
            .get(self.names.known.subtype)
            .and_then(PdfObject::as_name)
            .map(|n| self.names.resolve(n));
        match subtype.as_deref() {
            Some(b"Image") => {
                self.draw_image_xobject(obj_id, &stream);
                Ok(())
            }
            // Treat a missing /Subtype as a form (tolerant); anything else is
            // an XObject kind we do not model (PostScript), ignored.
            Some(b"Form") | None => self.invoke_form(obj_id, &stream),
            _ => Ok(()),
        }
    }

    fn draw_image_xobject(&mut self, obj_id: Option<ObjectId>, stream: &PdfStream) {
        let dict = &stream.dict;
        let w_key = self.intern(b"Width");
        let width = self.dict_int_indirect(dict, w_key).unwrap_or(0).max(0) as u32;
        let h_key = self.intern(b"Height");
        let height = self.dict_int_indirect(dict, h_key).unwrap_or(0).max(0) as u32;
        let is_mask = dict_bool(dict, self.intern(b"ImageMask"));
        let bpc_key = self.intern(b"BitsPerComponent");
        let bpc = self
            .dict_int_indirect(dict, bpc_key)
            .map(|v| v.clamp(0, 255) as u8)
            .unwrap_or(if is_mask { 1 } else { 0 });
        let filters = dict
            .get(self.names.known.filter)
            .map(|f| self.filter_names(f))
            .unwrap_or_default();
        let interpolate = dict_bool(dict, self.intern(b"Interpolate"));

        // Color space: absent for a stencil mask; else resolve the space.
        self.image_cs_degraded = false;
        let color_space = if is_mask {
            None
        } else {
            dict.get(self.intern(b"ColorSpace"))
                .cloned()
                .and_then(|cs| self.resolve_image_colorspace(&cs))
                .or(Some(pdf_page_ir::ImageColorSpace::Gray))
        };
        let lowering_degraded = self.image_cs_degraded;
        let decode = self.read_decode_array(dict);

        // Decode through the general filters; a codec filter (DCT/JPX/
        // JBIG2/CCITT) stops decoding and yields the encoded payload for
        // the backend's codec registry.
        let (samples, codec, codec_data) = match self
            .snapshot
            .decode_stream_data_to_codec(stream, self.ctx)
        {
            Ok((bytes, None)) => (Some(Arc::from(bytes)), None, None),
            Ok((bytes, Some(name))) => (None, codec_kind(name.as_bytes()), Some(Arc::from(bytes))),
            Err(_) => (None, None, None),
        };
        let smask = self.build_image_smask(dict);
        // `/SMaskInData` (§7.4.7): a JPXDecode image's own opacity channel acts
        // as a soft mask (1) or premultiplied opacity (2). Only honoured when
        // there is no separate `/SMask` (which takes precedence).
        let smask_in_data = if smask.is_some() {
            0
        } else {
            dict_int(dict, self.intern(b"SMaskInData"))
                .unwrap_or(0)
                .clamp(0, 2) as u8
        };
        // `/SMask` overrides `/Mask` (§8.9.6.4 note); a stencil base image does
        // its own masking. Otherwise resolve an explicit hard `/Mask`.
        let mask = if smask.is_some() || is_mask {
            None
        } else {
            self.build_image_mask(dict, color_space.as_ref(), bpc)
        };
        let codec_parms = codec.and_then(|_| self.read_codec_parms(dict));

        // Spaces the per-texel sampler cannot express — Lab (needs the CIE
        // conversion per sample, not an arity remap) and multi-input DeviceN
        // (needs the n-input tint transform, no 1-D LUT) — are converted to
        // plain RGB8 samples here, once, at compile time.
        let (color_space, decode, samples, bpc) = if !is_mask
            && mask.is_none()
            && let Some(s) = &samples
            && let Some(cs_raw) = dict.get(self.intern(b"ColorSpace")).cloned()
            && let Some(rgb) = self.convert_special_image_samples(
                &cs_raw,
                s,
                bpc,
                width,
                height,
                decode.as_deref(),
            ) {
            (Some(pdf_page_ir::ImageColorSpace::Rgb), None, Some(rgb), 8)
        } else {
            (color_space, decode, samples, bpc)
        };

        let img = SemImage {
            object: obj_id,
            width,
            height,
            bits_per_component: bpc,
            is_mask,
            filters,
            inline_data: Vec::new(),
            color_space,
            interpolate,
            decode,
            samples,
            codec,
            codec_data,
            codec_parms,
            smask,
            mask,
            smask_in_data,
            lowering_degraded,
        };
        let id = ImageId(self.images.len() as u32);
        self.images.push(img);
        self.ops.push(SemanticOp::DrawImage(id));
    }

    /// Convert 8-bit image samples in a space the sampler cannot express to
    /// packed RGB8, per sample, at compile time:
    ///
    /// - **Lab**: each texel runs through the same `pdf_color::lab_to_rgb`
    ///   the fill path uses (`/Decode` defaults to `[0 100]` for L* and the
    ///   space's `/Range` for a*/b*, §8.9.5.2), instead of the old
    ///   arity remap that read L*a*b* bytes as RGB.
    /// - **Multi-input DeviceN** (n > 1 colorants with a buildable tint
    ///   transform): each texel's n tints run through `eval_tint`
    ///   (memoized on the raw sample tuple — scans carry few distinct
    ///   colors), instead of the `1 − max(tint)` grey fallback.
    ///
    /// `None` (no conversion; caller keeps the arity approximation) for any
    /// other family, non-8-bpc data, short buffers, or oversized images.
    /// Parse an `/ICCBased` stream's profile into an sRGB transform, or `None`
    /// when it is sRGB (nothing to do), not a matrix/TRC RGB profile, or
    /// undecodable. Cached per stream: a document typically references one
    /// profile from every image on every page.
    fn icc_rgb_transform(&mut self, stream_obj: &PdfObject) -> Option<Arc<pdf_color::icc::IccRgb>> {
        let key = match stream_obj {
            PdfObject::Reference(id) => Some(*id),
            _ => None,
        };
        if let Some(id) = key
            && let Some(hit) = self.icc_cache.get(&id)
        {
            return hit.clone();
        }
        let resolved = self.resolve_obj(stream_obj)?;
        let built = (|| {
            let PdfObject::Stream(stream) = &*resolved else {
                return None;
            };
            // Only 3-component profiles are in scope; /N is authoritative for
            // arity per §8.6.5.5.
            if dict_int(&stream.dict, self.intern(b"N")).unwrap_or(3) != 3 {
                return None;
            }
            let bytes = self.snapshot.decode_stream_data(stream, self.ctx).ok()?;
            pdf_color::icc::IccRgb::from_profile(&bytes).map(Arc::new)
        })();
        if let Some(id) = key {
            self.icc_cache.insert(id, built.clone());
        }
        built
    }

    /// Parse an `/ICCBased` **CMYK** stream (`/N 4`, CMYK-data/Lab-PCS lut) into
    /// a CMYK→sRGB evaluator, cached per stream object. `None` when the stream
    /// is not that shape (any other profile keeps the frozen-table arity
    /// approximation).
    fn cmyk_profile_from_stream(
        &mut self,
        stream_obj: &PdfObject,
    ) -> Option<Arc<pdf_color::icc::IccCmyk>> {
        let key = match stream_obj {
            PdfObject::Reference(id) => Some(*id),
            _ => None,
        };
        if let Some(id) = key
            && let Some(hit) = self.icc_cmyk_cache.get(&id)
        {
            return hit.clone();
        }
        let resolved = self.resolve_obj(stream_obj)?;
        let built = (|| {
            let PdfObject::Stream(stream) = &*resolved else {
                return None;
            };
            if dict_int(&stream.dict, self.intern(b"N")).unwrap_or(4) != 4 {
                return None;
            }
            let bytes = self.snapshot.decode_stream_data(stream, self.ctx).ok()?;
            pdf_color::icc::IccCmyk::from_cmyk_profile(&bytes).map(Arc::new)
        })();
        if let Some(id) = key {
            self.icc_cmyk_cache.insert(id, built.clone());
        }
        built
    }

    fn cmyk_image_transform(
        &mut self,
        stream_obj: &PdfObject,
    ) -> Option<Arc<pdf_page_ir::IccCmykTransform>> {
        let key = match stream_obj {
            PdfObject::Reference(id) => Some(*id),
            _ => None,
        };
        if let Some(id) = key
            && let Some(hit) = self.icc_cmyk_image_cache.get(&id)
        {
            return hit.clone();
        }
        let built = self.cmyk_profile_from_stream(stream_obj).map(|profile| {
            let (grid, input_tables, clut, output_tables) = profile.ir_tables();
            Arc::new(pdf_page_ir::IccCmykTransform {
                grid,
                input_tables,
                clut,
                output_tables,
            })
        });
        if let Some(id) = key {
            self.icc_cmyk_image_cache.insert(id, built.clone());
        }
        built
    }

    /// If a colour-space array `[/ICCBased stream]` names a CMYK press profile,
    /// return its evaluator. Marks `uses_icc_color` for the feature flag.
    fn iccbased_cmyk_from_items(
        &mut self,
        items: &[PdfObject],
    ) -> Option<Arc<pdf_color::icc::IccCmyk>> {
        let head = items.first().and_then(PdfObject::as_name)?;
        if self.names.resolve(head).as_ref() != b"ICCBased" {
            return None;
        }
        self.uses_icc_color = true;
        self.cmyk_profile_from_stream(items.get(1)?)
    }

    /// The current resource dictionary's `/ColorSpace /DefaultCMYK` press
    /// profile, if it declares one as an `/ICCBased` CMYK space. PDFium routes
    /// DeviceCMYK content (the `k`/`K` operators and `/DeviceCMYK` images)
    /// through this profile when present (ISO 32000-1 §8.6.5.6;
    /// `CPDF_DocPageData::GetColorSpaceInternal`).
    fn default_cmyk_profile(&mut self) -> Option<Arc<pdf_color::icc::IccCmyk>> {
        let did = self.intern(b"DefaultCMYK");
        let entry = self
            .resource_subdict(self.names.known.color_space)?
            .as_dict()?
            .get(did)?
            .clone();
        let resolved = self.resolve_obj(&entry)?;
        let PdfObject::Array(items) = &*resolved else {
            return None;
        };
        let items = items.to_vec();
        self.iccbased_cmyk_from_items(&items)
    }

    /// The CMYK press profile a resource-named colour space `id` carries, when
    /// it is an `[/ICCBased stream]` with a 4-input Lab lut. Cached parse via
    /// [`Self::cmyk_profile_from_stream`].
    fn iccbased_cmyk_space(&mut self, id: NameId) -> Option<Arc<pdf_color::icc::IccCmyk>> {
        let entry = self
            .resource_subdict(self.names.known.color_space)?
            .as_dict()?
            .get(id)?
            .clone();
        let resolved = self.resolve_obj(&entry)?;
        let PdfObject::Array(items) = &*resolved else {
            return None;
        };
        let items = items.to_vec();
        self.iccbased_cmyk_from_items(&items)
    }

    fn convert_special_image_samples(
        &mut self,
        cs_obj: &PdfObject,
        samples: &Arc<[u8]>,
        bpc: u8,
        width: u32,
        height: u32,
        decode: Option<&[[f32; 2]]>,
    ) -> Option<Arc<[u8]>> {
        const MAX_PIXELS: usize = 64 * 1024 * 1024;
        if bpc != 8 {
            return None;
        }
        let n_px = (width as usize).checked_mul(height as usize)?;
        if n_px == 0 || n_px > MAX_PIXELS {
            return None;
        }
        // Resolve a named resource / indirect reference down to the array.
        let resolved = self.resolve_obj(cs_obj)?;
        let items: Vec<PdfObject> = match &*resolved {
            PdfObject::Array(a) => a.to_vec(),
            PdfObject::Name(n) => {
                let nid = *n;
                let entry = self
                    .resource_subdict(self.names.known.color_space)?
                    .as_dict()?
                    .get(nid)?
                    .clone();
                match &*self.resolve_obj(&entry)? {
                    PdfObject::Array(a) => a.to_vec(),
                    _ => return None,
                }
            }
            _ => return None,
        };
        let head = items.first().and_then(PdfObject::as_name)?;
        match self.names.resolve(head).as_ref() {
            // An `/ICCBased` RGB stream whose profile is not sRGB: the samples
            // are encoded in *that* profile, and reading them as if they were
            // already sRGB shifts the whole image. Convert once, here. The
            // common sRGB profile yields no converter and falls through, so the
            // usual case stays a pass-through.
            b"ICCBased" => {
                let stream = items.get(1)?;
                // A 3-component matrix/TRC RGB profile (the common non-sRGB case).
                if let Some(icc) = self.icc_rgb_transform(stream) {
                    let src = samples.get(..n_px.checked_mul(3)?)?;
                    let mut out = Vec::with_capacity(n_px * 3);
                    for px in src.chunks_exact(3) {
                        let dec = |c: usize, raw: u8| -> f32 {
                            let [lo, hi] =
                                decode.and_then(|d| d.get(c)).copied().unwrap_or([0.0, 1.0]);
                            lo + raw as f32 / 255.0 * (hi - lo)
                        };
                        let rgb = icc.to_srgb([dec(0, px[0]), dec(1, px[1]), dec(2, px[2])]);
                        for c in rgb {
                            out.push((c.clamp(0.0, 1.0) * 255.0).round() as u8);
                        }
                    }
                    return Some(Arc::from(out));
                }
                // A 4-component CMYK press profile: convert each texel through
                // the lut, matching PDFium's LittleCMS image path. Only fires
                // for non-codec (Flate/raw) CMYK samples; a CMYK JPEG decodes in
                // the backend and keeps the frozen-table path. Memoised on the
                // raw tuple — a one-time compile cost bounded by distinct inks.
                let prof = self.cmyk_profile_from_stream(stream)?;
                let src = samples.get(..n_px.checked_mul(4)?)?;
                let mut memo: HashMap<[u8; 4], [u8; 3]> = HashMap::new();
                let mut out = Vec::with_capacity(n_px * 3);
                for px in src.chunks_exact(4) {
                    let key = [px[0], px[1], px[2], px[3]];
                    let rgb8 = *memo.entry(key).or_insert_with(|| {
                        let dec = |c: usize, raw: u8| -> f32 {
                            let [lo, hi] =
                                decode.and_then(|d| d.get(c)).copied().unwrap_or([0.0, 1.0]);
                            lo + raw as f32 / 255.0 * (hi - lo)
                        };
                        let rgb = prof.to_srgb([
                            dec(0, px[0]),
                            dec(1, px[1]),
                            dec(2, px[2]),
                            dec(3, px[3]),
                        ]);
                        [
                            (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u8,
                            (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u8,
                            (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u8,
                        ]
                    });
                    out.extend_from_slice(&rgb8);
                }
                Some(Arc::from(out))
            }
            b"Lab" => {
                let mut white_point = [1.0f32, 1.0, 1.0];
                let mut range = [-100.0f32, 100.0, -100.0, 100.0];
                if let Some(d) = items
                    .get(1)
                    .and_then(|d| self.resolve_obj(d))
                    .and_then(|d| d.as_dict().cloned())
                {
                    for (key, out) in [
                        (&b"WhitePoint"[..], &mut white_point[..]),
                        (b"Range", &mut range[..]),
                    ] {
                        if let Some(nid) = self.names.lookup(key)
                            && let Some(PdfObject::Array(a)) = d.get(nid)
                        {
                            for (slot, item) in out.iter_mut().zip(a.iter()) {
                                if let Some(v) = item.as_number() {
                                    *slot = v as f32;
                                }
                            }
                        }
                    }
                }
                // §8.9.5.2: the Decode default for a Lab image is
                // [0 100 amin amax bmin bmax].
                let default_decode: [[f32; 2]; 3] =
                    [[0.0, 100.0], [range[0], range[1]], [range[2], range[3]]];
                let dec = |c: usize, raw: u8| -> f32 {
                    let [lo, hi] = decode
                        .and_then(|d| d.get(c))
                        .copied()
                        .unwrap_or(default_decode[c]);
                    lo + raw as f32 / 255.0 * (hi - lo)
                };
                let src = samples.get(..n_px.checked_mul(3)?)?;
                let mut out = Vec::with_capacity(n_px * 3);
                for px in src.chunks_exact(3) {
                    let [r, g, b] = pdf_color::lab_to_rgb(
                        dec(0, px[0]),
                        dec(1, px[1]),
                        dec(2, px[2]),
                        white_point,
                        range,
                    );
                    let to = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                    out.extend_from_slice(&[to(r), to(g), to(b)]);
                }
                Some(Arc::from(out))
            }
            b"DeviceN" => {
                let space = self.build_tint_space_from_array(&items)?;
                if space.inputs < 2 || space.func.is_none() {
                    return None; // 1-input takes the TintLut path; no func → arity approx
                }
                let n = space.inputs;
                let src = samples.get(..n_px.checked_mul(n)?)?;
                let dec = |c: usize, raw: u8| -> f64 {
                    match decode.and_then(|d| d.get(c)) {
                        Some([lo, hi]) => (*lo + raw as f32 / 255.0 * (hi - lo)) as f64,
                        None => raw as f64 / 255.0,
                    }
                };
                // Memoize on the raw tuple: real multi-colorant images carry
                // few distinct ink combinations.
                let mut memo: HashMap<Vec<u8>, [u8; 3]> = HashMap::new();
                let mut out = Vec::with_capacity(n_px * 3);
                let mut tints = vec![0.0f64; n];
                for px in src.chunks_exact(n) {
                    let rgb = match memo.get(px) {
                        Some(rgb) => *rgb,
                        None => {
                            for (c, (t, raw)) in tints.iter_mut().zip(px.iter()).enumerate() {
                                *t = dec(c, *raw);
                            }
                            let rgb = sem_color_to_rgb8(&self.eval_tint(&space, &tints));
                            memo.insert(px.to_vec(), rgb);
                            rgb
                        }
                    };
                    out.extend_from_slice(&rgb);
                }
                Some(Arc::from(out))
            }
            _ => None,
        }
    }

    /// Read a `/Decode` array into per-component `[min, max]` pairs.
    fn read_decode_array(&mut self, dict: &Dictionary) -> Option<Vec<[f32; 2]>> {
        // `/Decode` is frequently an *indirect* reference to the array (and,
        // rarely, its elements are). `read_num_array` only matches a direct
        // array, so resolve one level here first — otherwise an indirect
        // `/Decode [1 0 …]` inversion is silently dropped and the image renders
        // inverted (e.g. a dark scan meant to be shown light).
        let raw = dict.get(self.intern(b"Decode"))?.clone();
        let resolved = self.resolve_obj(&raw)?;
        let PdfObject::Array(items) = &*resolved else {
            return None;
        };
        let arr: Vec<f64> = items
            .iter()
            .filter_map(|it| match it {
                PdfObject::Reference(_) => self.resolve_obj(it).and_then(|o| o.as_number()),
                other => other.as_number(),
            })
            .collect();
        Some(pairs_f32(arr))
    }

    /// Resolve an image `/ColorSpace` object into a device-family / Indexed
    /// space; unrecognized spaces fall back to `None` (caller defaults to Gray).
    fn resolve_image_colorspace(
        &mut self,
        obj: &PdfObject,
    ) -> Option<pdf_page_ir::ImageColorSpace> {
        self.resolve_image_colorspace_depth(obj, 0)
    }

    fn resolve_image_colorspace_depth(
        &mut self,
        obj: &PdfObject,
        depth: u32,
    ) -> Option<pdf_page_ir::ImageColorSpace> {
        use pdf_page_ir::ImageColorSpace as Cs;
        if depth > 8 {
            return None;
        }
        let resolved = self.resolve_obj(obj)?;
        match &*resolved {
            PdfObject::Name(n) => {
                let name = self.names.resolve(*n);
                match name.as_ref() {
                    b"DeviceGray" | b"G" | b"CalGray" => Some(Cs::Gray),
                    b"DeviceRGB" | b"RGB" | b"CalRGB" | b"Lab" => Some(Cs::Rgb),
                    b"DeviceCMYK" | b"CMYK" => Some(Cs::Cmyk),
                    // A named resource space (`/CS0`): look it up and recurse.
                    other => {
                        let nid = self.intern(other);
                        let entry = self
                            .resource_subdict(self.names.known.color_space)?
                            .as_dict()?
                            .get(nid)?
                            .clone();
                        self.resolve_image_colorspace_depth(&entry, depth + 1)
                    }
                }
            }
            PdfObject::Array(items) => self.colorspace_from_array(items, depth),
            _ => None,
        }
    }

    fn colorspace_from_array(
        &mut self,
        items: &[PdfObject],
        depth: u32,
    ) -> Option<pdf_page_ir::ImageColorSpace> {
        use pdf_page_ir::ImageColorSpace as Cs;
        let head = items.first()?.as_name()?;
        match self.names.resolve(head).as_ref() {
            b"ICCBased" => {
                self.uses_icc_color = true;
                let stream = self.resolve_obj(items.get(1)?)?;
                let n = stream
                    .as_dict()
                    .and_then(|d| dict_int(d, self.intern(b"N")))
                    .unwrap_or(3);
                Some(match n {
                    1 => Cs::Gray,
                    4 => match items
                        .get(1)
                        .and_then(|stream| self.cmyk_image_transform(stream))
                    {
                        Some(transform) => Cs::IccCmyk { transform },
                        None => Cs::Cmyk,
                    },
                    // A 3-component profile that is not sRGB is carried into the
                    // IR so the sampler can apply it; a *codec* image's samples
                    // cannot be converted at compile time the way directly
                    // decoded ones are. sRGB (the common case) yields no
                    // transform and stays plain Rgb.
                    _ => match items.get(1).and_then(|o| self.icc_rgb_transform(o)) {
                        Some(icc) => Cs::IccRgb {
                            trc: Arc::from(icc.trc_flat()),
                            matrix: icc.matrix(),
                        },
                        None => Cs::Rgb,
                    },
                })
            }
            b"CalGray" => Some(Cs::Gray),
            b"CalRGB" => Some(Cs::Rgb),
            // `/Lab` image samples are L*a*b*, not RGB. The signed a*/b* axes
            // have to reach the sampler with their `/Range`, so this cannot
            // collapse to `Cs::Rgb` the way `/CalRGB` can.
            b"Lab" => {
                let mut white_point = [1.0f32, 1.0, 1.0];
                let mut range = [-100.0f32, 100.0, -100.0, 100.0];
                let dict = items
                    .get(1)
                    .and_then(|d| self.resolve_obj(d))
                    .and_then(|d| d.as_dict().cloned());
                if let Some(d) = &dict {
                    for (key, out) in [
                        (&b"WhitePoint"[..], &mut white_point[..]),
                        (&b"Range"[..], &mut range[..]),
                    ] {
                        if let Some(nid) = self.names.lookup(key)
                            && let Some(PdfObject::Array(a)) = d.get(nid)
                        {
                            for (slot, item) in out.iter_mut().zip(a.iter()) {
                                if let Some(v) = item.as_number() {
                                    *slot = v as f32;
                                }
                            }
                        }
                    }
                }
                Some(Cs::Lab { white_point, range })
            }
            b"ICC" => Some(Cs::Rgb),
            b"Indexed" | b"I" => {
                let base_obj = items.get(1)?.clone();
                let hival = items.get(2).and_then(PdfObject::as_int).unwrap_or(0).max(0) as u32;
                let lookup = self.read_indexed_lookup(items.get(3)?)?;
                // A palette in a space the sampler cannot express (Lab, whose
                // signed a*/b* bytes read as RGB collapse to near-black, or a
                // multi-input DeviceN) is pre-converted to sRGB here — the
                // palette is a `(hival+1)×1` image in the base space. The
                // sampler then does a plain index→RGB fetch.
                let entries = hival.saturating_add(1);
                let palette: Arc<[u8]> = Arc::from(lookup.clone());
                if let Some(rgb) =
                    self.convert_special_image_samples(&base_obj, &palette, 8, entries, 1, None)
                {
                    return Some(Cs::Indexed {
                        base: Box::new(Cs::Rgb),
                        hival,
                        lookup: rgb,
                    });
                }
                let base = self.resolve_image_colorspace_depth(&base_obj, depth + 1)?;
                Some(Cs::Indexed {
                    base: Box::new(base),
                    hival,
                    lookup: Arc::from(lookup),
                })
            }
            // A `/Separation` (or 1-colorant `/DeviceN`) image sample is a
            // *tint*, not a gray value. Bake the tint transform into a 256-entry
            // sample→sRGB LUT so the sampler routes it correctly instead of
            // mis-reading it as DeviceGray (which inverts near-white scans to
            // near-black — the dominant residual over-ink class). Falls back to
            // the arity approximation only if the transform will not build.
            b"Separation" => self
                .build_tint_image_lut(items)
                .map(|rgb| Cs::TintLut { rgb })
                .or(Some(Cs::Gray)),
            b"DeviceN" => {
                let names_len = self
                    .resolve_obj(items.get(1)?)
                    .and_then(|o| match &*o {
                        PdfObject::Array(a) => Some(a.len()),
                        _ => None,
                    })
                    .unwrap_or(1);
                if names_len == 1
                    && let Some(rgb) = self.build_tint_image_lut(items)
                {
                    return Some(Cs::TintLut { rgb });
                }
                // Two colorants (a spot + black duotone, e.g. `/DeviceN
                // [/PANTONE#207482#20C /Black] /DeviceCMYK`) bake to a 2-D
                // table. The arity approximation would read one tint as
                // DeviceGray and drop the other entirely.
                if names_len == 2
                    && let Some(rgb) = self.build_tint_image_lut2(items)
                {
                    return Some(Cs::TintLut2 { rgb });
                }
                Some(if names_len == 4 {
                    Cs::Cmyk
                } else if names_len == 3 {
                    Cs::Rgb
                } else {
                    Cs::Gray
                })
            }
            _ => None,
        }
    }

    /// Bake a 1-input `/Separation`/`/DeviceN` tint transform into a 256-entry
    /// sample→sRGB table (`256 × 3` bytes) for image sampling. Entry `i` is the
    /// alternate-space colour of tint `i/255`, converted to sRGB with the same
    /// `pdf_color` conversions the fill path uses. `None` for a multi-input
    /// space (no 1-D LUT) or a transform that will not build — the caller then
    /// keeps the arity approximation.
    fn build_tint_image_lut(&mut self, items: &[PdfObject]) -> Option<Arc<[u8]>> {
        let space = self.build_tint_space_from_array(items)?;
        if space.inputs != 1 {
            return None;
        }
        let mut rgb = Vec::with_capacity(256 * 3);
        for i in 0..256u32 {
            let sem = self.eval_tint(&space, &[i as f64 / 255.0]);
            rgb.extend_from_slice(&sem_color_to_rgb8(&sem));
        }
        // R1 (PLAN-POST-SWEEP3): a *wrong-but-valid* LUT. A real tint
        // transform must vary with the tint — tint 0 ≈ no colorant, tint 1 =
        // full colorant. When the function evaluated but baked to a (near-)
        // constant ramp (evaluation errored per-sample, clamped flat, or the
        // function itself is degenerate), sampling through it blankets the
        // image in one colour — the Separation/Indexed "blank cover" class.
        // Repair with the polarity-correct subtractive ramp the non-LUT path
        // would have used, and mark the draw degraded so the tracker sees it.
        // `/None` colorants are excluded: their all-white LUT is deliberate
        // policy ("marks nothing"), not degradation.
        if space.func.is_some() && !space.colorant_none && lut_is_degenerate(&rgb) {
            for (i, px) in rgb.chunks_exact_mut(3).enumerate() {
                let g = 255 - i as u8; // tint i/255 → gray 1 − tint
                px.copy_from_slice(&[g, g, g]);
            }
            self.image_cs_degraded = true;
            self.ctx.note_recovery(
                "degenerate tint-transform ramp replaced with subtractive approximation"
                    .to_string(),
            );
        }
        Some(Arc::from(rgb))
    }

    /// Bake a 2-input `/DeviceN` tint transform into a `256 x 256 x 3`
    /// sample-pair→sRGB table for image sampling. Entry `(a, b)` is the
    /// alternate-space colour of tints `(a/255, b/255)`, converted with the same
    /// `pdf_color` conversions the fill path uses.
    ///
    /// 65 536 evaluations of the tint function is the cost, paid once per image
    /// space; three inputs would be 16.7 M evaluations and a 48 MiB table, which
    /// is why this stops at two.
    fn build_tint_image_lut2(&mut self, items: &[PdfObject]) -> Option<Arc<[u8]>> {
        let space = self.build_tint_space_from_array(items)?;
        if space.inputs != 2 || space.func.is_none() {
            return None;
        }
        let mut rgb = Vec::with_capacity(256 * 256 * 3);
        for a in 0..256u32 {
            for b in 0..256u32 {
                let sem = self.eval_tint(&space, &[a as f64 / 255.0, b as f64 / 255.0]);
                rgb.extend_from_slice(&sem_color_to_rgb8(&sem));
            }
        }
        // Same degeneracy guard as the 1-input LUT: a table that does not vary
        // means the transform did not really evaluate, and sampling through it
        // would blanket the image in one colour. Fall back to the arity
        // approximation rather than paint a flat lie.
        if lut_is_degenerate(&rgb) {
            return None;
        }
        Some(Arc::from(rgb))
    }

    /// Read an Indexed color space's lookup table (a string or a stream).
    fn read_indexed_lookup(&mut self, obj: &PdfObject) -> Option<Vec<u8>> {
        let resolved = self.resolve_obj(obj)?;
        match &*resolved {
            PdfObject::String(s) => Some(s.as_bytes().to_vec()),
            PdfObject::Stream(stream) => self.snapshot.decode_stream_data(stream, self.ctx).ok(),
            _ => None,
        }
    }

    /// Build a grayscale `/SMask` soft mask from an image dictionary.
    ///
    /// Codec-encoded masks (JBIG2 for MRC scans, DCT/JPX elsewhere) keep
    /// their encoded payload for the backend's registry, exactly like the
    /// base image: the interpreter never decodes codecs.
    fn build_image_smask(&mut self, dict: &Dictionary) -> Option<Arc<ImageSMask>> {
        let smask = self.resolve_obj(&dict.get(self.intern(b"SMask"))?.clone())?;
        let PdfObject::Stream(stream) = &*smask else {
            return None;
        };
        let sdict = &stream.dict;
        let w_key = self.intern(b"Width");
        let width = self.dict_int_indirect(sdict, w_key).unwrap_or(0).max(0) as u32;
        let h_key = self.intern(b"Height");
        let height = self.dict_int_indirect(sdict, h_key).unwrap_or(0).max(0) as u32;
        let bpc_key = self.intern(b"BitsPerComponent");
        let bpc = self
            .dict_int_indirect(sdict, bpc_key)
            .unwrap_or(8)
            .clamp(1, 16) as u8;
        let decode = self.read_decode_array(sdict).map(Arc::from);
        let codec_parms = self.read_codec_parms(sdict);

        let (samples, codec, codec_data) =
            match self.snapshot.decode_stream_data_to_codec(stream, self.ctx) {
                Ok((bytes, None)) => (Arc::from(bytes), None, None),
                Ok((bytes, Some(name))) => (
                    Arc::from(Vec::new()),
                    codec_kind(name.as_bytes()),
                    Some(Arc::from(bytes)),
                ),
                Err(_) => return None,
            };
        Some(Arc::new(ImageSMask {
            width,
            height,
            bits_per_component: bpc,
            decode,
            samples,
            codec,
            codec_data,
            codec_parms,
        }))
    }

    /// Build an explicit `/Mask` (ISO 32000-1 §8.9.6.3–8.9.6.4) from an image
    /// dictionary. The caller has already ruled out `/SMask` (which overrides)
    /// and stencil base images. Two forms:
    ///
    /// * an **array** → color-key masking: `[min0 max0 …]` in raw-sample space.
    ///   Rejected (→ `None`, paint unmasked) when the length is not `2 ×
    ///   component-count` or any bound exceeds `2^bpc − 1`, matching PDFium's
    ///   tolerance for malformed masks.
    /// * a **stream** → a 1-bit stencil image-mask XObject, carried as an
    ///   [`ImageSMask`] payload (reusing its codec plumbing; the *polarity* is
    ///   handled at sampling time, not here).
    fn build_image_mask(
        &mut self,
        dict: &Dictionary,
        color_space: Option<&ImageColorSpace>,
        bpc: u8,
    ) -> Option<pdf_page_ir::ImageMask> {
        let mask = self.resolve_obj(&dict.get(self.intern(b"Mask"))?.clone())?;
        match &*mask {
            // Color-key masking: integer ranges in raw-sample space.
            PdfObject::Array(items) => {
                let ncomp = color_space.map(ImageColorSpace::components).unwrap_or(1);
                if items.len() != ncomp * 2 {
                    return None;
                }
                let max_raw: i64 = (1i64 << bpc.min(31)) - 1;
                let mut ranges = Vec::with_capacity(ncomp);
                for pair in items.chunks_exact(2) {
                    let lo = pair[0].as_int()?;
                    let hi = pair[1].as_int()?;
                    // Malformed: negative or out of the sample range, or lo > hi.
                    if lo < 0 || hi < 0 || lo > max_raw || hi > max_raw || lo > hi {
                        return None;
                    }
                    ranges.push([lo as u32, hi as u32]);
                }
                Some(pdf_page_ir::ImageMask::ColorKey(Arc::from(ranges)))
            }
            // Stencil mask: a 1-bit image-mask XObject.
            PdfObject::Stream(stream) => {
                let sdict = &stream.dict;
                let w_key = self.intern(b"Width");
                let width = self.dict_int_indirect(sdict, w_key).unwrap_or(0).max(0) as u32;
                let h_key = self.intern(b"Height");
                let height = self.dict_int_indirect(sdict, h_key).unwrap_or(0).max(0) as u32;
                let mut decode: Option<Vec<[f32; 2]>> = self.read_decode_array(sdict);
                // §8.9.6.4's polarity (sample 1 = masked out) is defined for a
                // *stencil*, i.e. a stream with `/ImageMask true`. Producers do
                // point `/Mask` at an ordinary 1-bit `/DeviceGray` image
                // instead (pdfjs issue6621), and there the samples read as
                // coverage the other way up — 1 paints. PDFium, MuPDF and hayro
                // all agree on that; taking the stencil polarity literally
                // masked out everything those three paint and left the image
                // invisible. Flip whatever polarity the stream declares.
                if !dict_bool(sdict, self.intern(b"ImageMask")) {
                    decode = Some(match decode {
                        Some(d) if !d.is_empty() => d.iter().map(|&[lo, hi]| [hi, lo]).collect(),
                        _ => vec![[1.0, 0.0]],
                    });
                }
                let decode = decode.map(Arc::from);
                let codec_parms = self.read_codec_parms(sdict);
                let (samples, codec, codec_data) =
                    match self.snapshot.decode_stream_data_to_codec(stream, self.ctx) {
                        Ok((bytes, None)) => (Arc::from(bytes), None, None),
                        Ok((bytes, Some(name))) => (
                            Arc::from(Vec::new()),
                            codec_kind(name.as_bytes()),
                            Some(Arc::from(bytes)),
                        ),
                        Err(_) => return None,
                    };
                Some(pdf_page_ir::ImageMask::Stencil(Arc::new(ImageSMask {
                    width,
                    height,
                    // An /ImageMask is 1-bit by definition (§8.9.6.2).
                    bits_per_component: 1,
                    decode,
                    samples,
                    codec,
                    codec_data,
                    codec_parms,
                })))
            }
            _ => None,
        }
    }

    /// Read the `/DecodeParms` a codec needs: JBIG2's globals stream and
    /// CCITT's parameters. `/DecodeParms` may be a dict or an array parallel
    /// to `/Filter`, so every entry is inspected.
    fn read_codec_parms(&mut self, dict: &Dictionary) -> Option<pdf_page_ir::CodecParms> {
        let parms = self.resolve_obj(&dict.get(self.names.known.decode_parms)?.clone())?;
        let candidates: Vec<PdfObject> = match &*parms {
            PdfObject::Array(items) => items.to_vec(),
            other => vec![other.clone()],
        };
        let mut out = pdf_page_ir::CodecParms {
            // The spec's CCITT defaults (ISO 32000-1 table 11).
            columns: 1728,
            end_of_block: true,
            ..Default::default()
        };
        let mut found = false;
        for entry in candidates {
            let Some(entry) = self.resolve_obj(&entry) else {
                continue;
            };
            let Some(d) = entry.as_dict() else { continue };
            let d = d.clone();
            found = true;
            if let Some(reference) = d.get(self.intern(b"JBIG2Globals")).cloned()
                && let Some(resolved) = self.resolve_obj(&reference)
                && let PdfObject::Stream(stream) = &*resolved
                && let Ok(bytes) = self.snapshot.decode_stream_data(stream, self.ctx)
            {
                out.jbig2_globals = Some(Arc::from(bytes));
            }
            let int = |this: &mut Self, key: &[u8]| -> Option<i64> {
                let value = d.get(this.intern(key))?.clone();
                this.resolve_obj(&value)?.as_int()
            };
            let boolean = |this: &mut Self, key: &[u8]| -> Option<bool> {
                let value = d.get(this.intern(key))?.clone();
                match &*this.resolve_obj(&value)? {
                    PdfObject::Boolean(b) => Some(*b),
                    _ => None,
                }
            };
            if let Some(k) = int(self, b"K") {
                out.k = k as i32;
            }
            if let Some(c) = int(self, b"Columns") {
                out.columns = c.max(0) as u32;
            }
            if let Some(r) = int(self, b"Rows") {
                out.rows = r.max(0) as u32;
            }
            if let Some(v) = boolean(self, b"BlackIs1") {
                out.black_is_1 = v;
            }
            if let Some(v) = boolean(self, b"EncodedByteAlign") {
                out.byte_align = v;
            }
            if let Some(v) = boolean(self, b"EndOfLine") {
                out.end_of_line = v;
            }
            if let Some(v) = boolean(self, b"EndOfBlock") {
                out.end_of_block = v;
            }
        }
        found.then_some(out)
    }

    /// Execute a Form XObject inline (ISO 32000-1 §8.10.1).
    fn invoke_form(
        &mut self,
        obj_id: Option<ObjectId>,
        stream: &PdfStream,
    ) -> Result<(), ContentError> {
        self.invoke_form_with_origin(obj_id, stream, PaintOrigin::FormXObject)
    }

    /// Execute a form while attributing its direct content to `origin`.
    /// Annotation appearance streams use their own origin; forms invoked from
    /// inside an appearance still enter through `invoke_form`, so the
    /// innermost Form XObject wins.
    fn invoke_form_with_origin(
        &mut self,
        obj_id: Option<ObjectId>,
        stream: &PdfStream,
        origin: PaintOrigin,
    ) -> Result<(), ContentError> {
        if self.invoke_depth >= self.limits.max_invoke_depth {
            return Err(ContentError::RecursionDepth(self.invoke_depth));
        }
        // Precise cycle detection: a form already on the stack is skipped
        // rather than re-entered (tolerant — matches how viewers survive
        // self-referential forms).
        if let Some(id) = obj_id
            && self.form_stack.contains(&id)
        {
            return Ok(());
        }

        let content = self.snapshot.decode_stream_data(stream, self.ctx)?;
        let matrix = read_matrix(&stream.dict, self.intern(b"Matrix"));
        let bbox = read_rect(&stream.dict, self.intern(b"BBox"));
        let group = self.read_group(&stream.dict, bbox, matrix);
        let form_res = match stream.dict.get(self.names.known.resources).cloned() {
            Some(res) => self.resolve_obj(&res),
            None => self.resources.clone(),
        };

        // Clear the leftover `Do` operands so the form's first operator does
        // not see them.
        self.operands.clear();

        // 1. Save.
        self.gs_stack.push(self.gs.clone());
        self.ops.push(SemanticOp::Save);
        // 2. Concat /Matrix.
        if matrix != Matrix::IDENTITY {
            self.gs.ctm = matrix.then(self.gs.ctm);
            self.ops.push(SemanticOp::Concat(matrix));
        }
        // 3. Clip to /BBox (in form space, i.e. after the Matrix concat).
        if let Some([x0, y0, x1, y1]) = bbox {
            let path = rect_path(x0, y0, x1 - x0, y1 - y0);
            let pid = self.intern_path(path);
            self.ops.push(SemanticOp::Clip {
                path: pid,
                rule: FillRule::NonZero,
            });
            self.gs.clip_depth += 1;
        }
        // A transparency group scopes the form's compositing. Capture the
        // group's composite params (ca/blend at the invocation) BEFORE
        // resetting them for the content: the group as a whole is composited
        // with them, while its content paints with alpha/blend reset to
        // 1/Normal (ISO 32000-1 §11.6.6), avoiding a double alpha.
        if let Some((isolated, knockout, bounds)) = group {
            self.ops.push(SemanticOp::BeginGroup {
                isolated,
                knockout,
                bounds,
                opacity: self.gs.fill_alpha,
                blend: self.gs.blend,
            });
            self.gs.fill_alpha = 1.0;
            self.gs.stroke_alpha = 1.0;
            self.gs.blend = BlendMode::Normal;
        }

        // 4. Execute with the form's resources. Marked-content state is
        // scoped to the stream (§14.6): an unbalanced BMC/EMC inside the
        // form must not leak OC suppression into the caller.
        let saved_res = self.resources.clone();
        let saved_mc_depth = self.mc_stack.len();
        let saved_oc_hidden = self.oc_hidden_count;
        // Path construction is per content stream. PDFium gives each form its
        // own `CPDF_StreamContentParser`, so a path the caller left pending
        // cannot reach the form's operators and the form cannot hand one back.
        // We share one builder across the whole interpretation, so scope it by
        // hand — pdfbox/5302 invokes a form while a page-sized `re` is still
        // pending (`0 0 1155 1563 re / q ... Do Q / W`, never painted), and the
        // first `B` inside the form then filled that rectangle black along with
        // its own, flooding the page.
        let saved_path = std::mem::take(&mut self.path);
        let saved_pending_clip = self.pending_clip.take();
        let saved_caches = self.take_resource_caches();
        // Patterns referenced by this stream anchor to the form's default
        // space — the CTM here, after the /Matrix concat (§8.7.3.1).
        let saved_pattern_base = self.pattern_base;
        self.pattern_base = self.gs.ctm;
        self.resources = form_res;
        self.invoke_depth += 1;
        if let Some(id) = obj_id {
            self.form_stack.push(id);
        }
        self.ops.push(SemanticOp::BeginPaintOrigin(origin));
        let result = self.run(&content);
        self.ops.push(SemanticOp::EndPaintOrigin);
        if obj_id.is_some() {
            self.form_stack.pop();
        }
        self.invoke_depth -= 1;
        self.resources = saved_res;
        self.pattern_base = saved_pattern_base;
        self.restore_resource_caches(saved_caches);
        self.path = saved_path;
        self.pending_clip = saved_pending_clip;
        self.mc_stack.truncate(saved_mc_depth);
        self.oc_hidden_count = saved_oc_hidden;

        if group.is_some() {
            self.ops.push(SemanticOp::EndGroup);
        }

        // 5. Restore.
        if let Some(prev) = self.gs_stack.pop() {
            self.gs = prev;
            self.ops.push(SemanticOp::Restore);
        }
        result
    }

    /// Render a page's annotation static appearances after the page content
    /// (ISO 32000-1 §12.5.5; PDFium `FPDF_ANNOT` display pass).
    ///
    /// PDFium two-pass order (`CPDF_AnnotList::DisplayAnnots`): every
    /// non-widget annotation first, then the widgets. `Hidden`/`NoView`
    /// annotations are skipped for screen rendering. Each annotation is
    /// rendered from a *fresh* graphics state — never the state the page
    /// content happened to end in.
    ///
    /// Only DoS-guard errors propagate (same contract as [`Self::run`]);
    /// everything else drops just the offending annotation.
    pub(crate) fn run_annotations(
        &mut self,
        annots: &[pdf_document::PageAnnotation],
    ) -> Result<(), ContentError> {
        // Balance any un-restored `q` the page content left behind so the
        // annotations (and lowering's state stack) start clean; likewise any
        // dangling marked-content (OC) suppression.
        while self.gs_stack.pop().is_some() {
            self.ops.push(SemanticOp::Restore);
        }
        self.mc_stack.clear();
        self.oc_hidden_count = 0;
        for widgets_pass in [false, true] {
            for annot in annots {
                let is_widget = annot
                    .subtype
                    .is_some_and(|s| self.names.resolve(s).as_ref() == b"Widget");
                if is_widget != widgets_pass {
                    continue;
                }
                if annot.hidden_for_display() {
                    continue;
                }
                if let Err(e) = self.render_annotation(annot) {
                    if e.is_fatal() {
                        return Err(e);
                    }
                    self.ctx
                        .note_recovery(format!("annotation appearance dropped ({e})"));
                }
            }
        }
        Ok(())
    }

    /// Render one annotation's `/AP /N` appearance stream (§12.5.5
    /// algorithm): map the form's `/BBox` through its `/Matrix`, fit that
    /// extent onto `/Rect` with matrix `A`, then run the form under `A`
    /// (the form's own `/Matrix` + `/BBox` clip are applied by
    /// [`Self::invoke_form`], yielding the spec's `/Matrix` ∘ `A`).
    fn render_annotation(
        &mut self,
        annot: &pdf_document::PageAnnotation,
    ) -> Result<(), ContentError> {
        let Some((stream_id, stream)) = self.annotation_appearance(annot) else {
            return self.render_markup_annotation(annot);
        };
        let bbox = read_rect(&stream.dict, self.intern(b"BBox"));
        let matrix = read_matrix(&stream.dict, self.intern(b"Matrix"));
        let Some(fit) = appearance_matrix(bbox, matrix, annot.rect) else {
            self.ctx.note_recovery(
                "annotation appearance skipped (degenerate /BBox or /Rect)".to_string(),
            );
            return Ok(());
        };

        // Fresh per-annotation state (PDFium renders each annotation through
        // its own CPDF_RenderContext layer, not the page's ending state).
        self.gs = GraphicsState::default();
        self.text_matrix = Matrix::IDENTITY;
        self.text_line_matrix = Matrix::IDENTITY;
        self.path = PathBuilder::default();
        self.pending_clip = None;
        self.text_clip_runs.clear();
        self.operands.clear();

        self.gs_stack.push(self.gs.clone());
        self.ops.push(SemanticOp::Save);
        if fit != Matrix::IDENTITY {
            self.gs.ctm = fit.then(self.gs.ctm);
            self.ops.push(SemanticOp::Concat(fit));
        }
        let result =
            self.invoke_form_with_origin(stream_id, &stream, PaintOrigin::AnnotationAppearance);
        if let Some(prev) = self.gs_stack.pop() {
            self.gs = prev;
            self.ops.push(SemanticOp::Restore);
        }
        result
    }

    /// Synthesize the conventional static appearance for text-markup
    /// annotations whose author omitted `/AP /N`. A usable appearance stream
    /// is always authoritative; this fallback covers only the four geometric
    /// markup subtypes defined by §12.5.6.10.
    fn render_markup_annotation(
        &mut self,
        annot: &pdf_document::PageAnnotation,
    ) -> Result<(), ContentError> {
        let Some(subtype) = annot.subtype.map(|name| self.names.resolve(name)) else {
            return Ok(());
        };
        let kind = match subtype.as_ref() {
            b"Highlight" => MarkupKind::Highlight,
            b"Underline" => MarkupKind::Underline,
            b"Squiggly" => MarkupKind::Squiggly,
            b"StrikeOut" => MarkupKind::StrikeOut,
            _ => return Ok(()),
        };

        let fallback_quad = [
            annot.rect[0],
            annot.rect[3],
            annot.rect[2],
            annot.rect[3],
            annot.rect[0],
            annot.rect[1],
            annot.rect[2],
            annot.rect[1],
        ];
        let quads: &[[f64; 8]] = if annot.quad_points.is_empty() {
            if annot.rect[0] >= annot.rect[2] || annot.rect[1] >= annot.rect[3] {
                return Ok(());
            }
            std::slice::from_ref(&fallback_quad)
        } else {
            &annot.quad_points
        };

        let mut builder = PathBuilder::default();
        for quad in quads {
            append_markup_geometry(&mut builder, *quad, kind);
        }
        if builder.is_empty() {
            return Ok(());
        }
        let path = self.intern_path(builder.take());
        let default = if matches!(kind, MarkupKind::Highlight) {
            SemColor::DeviceRgb(1.0, 1.0, 0.0)
        } else {
            SemColor::DeviceGray(0.0)
        };
        let color = match annot.color {
            Some(pdf_document::AnnotationColor::Gray(gray)) => {
                SemColor::DeviceGray(f64::from(gray))
            }
            Some(pdf_document::AnnotationColor::Rgb(rgb)) => {
                SemColor::DeviceRgb(f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2]))
            }
            Some(pdf_document::AnnotationColor::Cmyk(cmyk)) => SemColor::DeviceCmyk(
                f64::from(cmyk[0]),
                f64::from(cmyk[1]),
                f64::from(cmyk[2]),
                f64::from(cmyk[3]),
            ),
            None => default,
        };
        let blend = if matches!(kind, MarkupKind::Highlight) {
            BlendMode::Multiply
        } else {
            BlendMode::Normal
        };

        // Each synthesized appearance is an isolated semantic state just like
        // an appearance Form. Attribution remains AnnotationAppearance so
        // sweep diagnostics do not misclassify these pixels as page content.
        self.ops.push(SemanticOp::Save);
        self.ops.push(SemanticOp::BeginPaintOrigin(
            PaintOrigin::AnnotationAppearance,
        ));
        self.ops.push(SemanticOp::SetFillColor(color));
        self.ops
            .push(SemanticOp::SetFillAlpha(annot.opacity.clamp(0.0, 1.0)));
        self.ops.push(SemanticOp::SetBlendMode(blend));
        self.ops.push(SemanticOp::Fill {
            path,
            rule: FillRule::NonZero,
        });
        self.ops.push(SemanticOp::EndPaintOrigin);
        self.ops.push(SemanticOp::Restore);
        Ok(())
    }

    /// Select an annotation's normal appearance stream. Always `/AP /N`
    /// (PDFium renders `AppearanceMode::kNormal` unconditionally — never
    /// `/D` or `/R`). When `/N` is a sub-dictionary of states, `/AS` picks
    /// the entry; with no `/AS`, a single-entry dictionary falls back to
    /// that entry (tolerance PDFium shares), otherwise nothing is drawn.
    fn annotation_appearance(
        &mut self,
        annot: &pdf_document::PageAnnotation,
    ) -> Option<(Option<ObjectId>, Arc<PdfStream>)> {
        let ap = self.resolve_obj(annot.appearance.as_ref()?)?;
        let n_raw = ap.as_dict()?.get(self.intern(b"N"))?.clone();
        let n_id = match &n_raw {
            PdfObject::Reference(id) => Some(*id),
            _ => None,
        };
        let n = self.resolve_obj(&n_raw)?;
        match &*n {
            PdfObject::Stream(s) => Some((n_id, s.clone())),
            PdfObject::Dictionary(states) => {
                let entry = match annot.appearance_state {
                    Some(state) => states.get(state).cloned(),
                    None if states.len() == 1 => states.iter().next().map(|(_, v)| v.clone()),
                    None => None,
                }?;
                let entry_id = match &entry {
                    PdfObject::Reference(id) => Some(*id),
                    _ => None,
                };
                let resolved = self.resolve_obj(&entry)?;
                match &*resolved {
                    PdfObject::Stream(s) => Some((entry_id, s.clone())),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Read a form's `/Group` as a transparency group, if present. Returns
    /// `(isolated, knockout, bounds)`, with `bounds` the `/BBox` mapped through
    /// the form's effective CTM (matrix ∘ current CTM) — a conservative extent
    /// in page space for later offscreen allocation.
    fn read_group(
        &mut self,
        dict: &Dictionary,
        bbox: Option<[f64; 4]>,
        matrix: Matrix,
    ) -> Option<(bool, bool, Rect)> {
        let group_ref = dict.get(self.intern(b"Group"))?.clone();
        let group = self.resolve_obj(&group_ref)?;
        let gdict = group.as_dict()?;
        let s = gdict.get(self.intern(b"S")).and_then(PdfObject::as_name)?;
        if self.names.resolve(s).as_ref() != b"Transparency" {
            return None;
        }
        let isolated = dict_bool(gdict, self.intern(b"I"));
        let knockout = dict_bool(gdict, self.intern(b"K"));
        let bounds = group_bounds(bbox, matrix.then(self.gs.ctm));
        Some((isolated, knockout, bounds))
    }

    fn filter_names(&self, obj: &PdfObject) -> Vec<Vec<u8>> {
        match obj {
            PdfObject::Name(n) => vec![self.names.resolve(*n).to_vec()],
            PdfObject::Array(items) => items
                .iter()
                .filter_map(PdfObject::as_name)
                .map(|n| self.names.resolve(n).to_vec())
                .collect(),
            _ => Vec::new(),
        }
    }

    fn inline_image(&mut self, dict: &[(Vec<u8>, Operand)], data: Vec<u8>) {
        let mut width = 0u32;
        let mut height = 0u32;
        let mut bpc = 0u8;
        let mut is_mask = false;
        let mut filters = Vec::new();
        let mut interpolate = false;
        let mut cs: Option<Operand> = None;
        let mut decode: Option<Vec<[f32; 2]>> = None;
        // Inline images cannot reference a stream, so `/Mask` here is only ever
        // the color-key array form (§8.9.6.4); it has no abbreviated key.
        let mut mask_op: Option<Operand> = None;
        let mut dp_op: Option<Operand> = None;
        for (key, value) in dict {
            match key.as_slice() {
                b"W" | b"Width" => width = value.as_int().unwrap_or(0).max(0) as u32,
                b"H" | b"Height" => height = value.as_int().unwrap_or(0).max(0) as u32,
                b"BPC" | b"BitsPerComponent" => {
                    bpc = value.as_int().unwrap_or(0).clamp(0, 255) as u8
                }
                b"IM" | b"ImageMask" => is_mask = matches!(value, Operand::Bool(true)),
                b"F" | b"Filter" => filters = collect_filters(value),
                b"I" | b"Interpolate" => interpolate = matches!(value, Operand::Bool(true)),
                b"CS" | b"ColorSpace" => cs = Some(value.clone()),
                b"D" | b"Decode" => {
                    if let Operand::Array(a) = value {
                        let nums: Vec<f64> = a.iter().filter_map(Operand::as_f64).collect();
                        decode = Some(pairs_f32(nums));
                    }
                }
                b"Mask" => mask_op = Some(value.clone()),
                b"DP" | b"DecodeParms" => dp_op = Some(value.clone()),
                _ => {}
            }
        }
        if is_mask && bpc == 0 {
            bpc = 1;
        }
        self.image_cs_degraded = false;
        let color_space = if is_mask {
            None
        } else {
            Some(self.inline_colorspace(cs.as_ref()))
        };
        let lowering_degraded = self.image_cs_degraded;
        // Color-key `/Mask` only — a stencil (stream) mask can't appear inline.
        // Suppressed for stencil base images, matching the XObject path.
        let mask = if is_mask {
            None
        } else {
            mask_op.and_then(|op| inline_color_key_mask(&op, color_space.as_ref(), bpc))
        };
        let (samples, codec, codec_data) = self.decode_inline(&filters, &data);
        let img = SemImage {
            object: None,
            width,
            height,
            bits_per_component: bpc,
            is_mask,
            filters,
            inline_data: data,
            color_space,
            interpolate,
            decode,
            samples,
            codec,
            codec_data,
            // Inline images spell /DecodeParms as /DP (Table 93); the keys
            // *inside* the parms dict are not abbreviated, so the standard
            // CCITT parameter names apply. JBIG2 globals need an indirect
            // stream, which inline images cannot reference — CCITT is the
            // only codec that takes parameters here.
            codec_parms: dp_op.as_ref().and_then(inline_codec_parms),
            smask: None,
            mask,
            smask_in_data: 0,
            lowering_degraded,
        };
        let id = ImageId(self.images.len() as u32);
        self.images.push(img);
        self.ops.push(SemanticOp::DrawImage(id));
    }

    /// Map an inline image's `/CS` (abbreviation or named resource) to a color
    /// space, defaulting to Gray.
    fn inline_colorspace(&mut self, cs: Option<&Operand>) -> ImageColorSpace {
        match cs {
            Some(Operand::Name(n)) => match n.as_slice() {
                b"G" | b"DeviceGray" | b"CalGray" => ImageColorSpace::Gray,
                b"RGB" | b"DeviceRGB" | b"CalRGB" => ImageColorSpace::Rgb,
                b"CMYK" | b"DeviceCMYK" => ImageColorSpace::Cmyk,
                other => {
                    // A named resource color space.
                    let name = PdfObject::Name(self.intern(other));
                    self.resolve_image_colorspace(&name)
                        .unwrap_or(ImageColorSpace::Gray)
                }
            },
            // `/CS` may be written out in full (§8.9.5.1 permits any of the
            // colour-space forms an image XObject accepts, abbreviations
            // optional), most often `[/I /RGB 255 <palette>]`. Falling back to
            // DeviceGray here reads palette *indices* as grey levels — on
            // custom/image_inline_5 index 1 is a light blue but reads as near
            // black, and the page rendered at ink 0.53 against PDFium's 0.06.
            Some(other @ (Operand::Array(_) | Operand::Dict(_))) => {
                let obj = self.operand_to_object(other);
                self.resolve_image_colorspace(&obj)
                    .unwrap_or(ImageColorSpace::Gray)
            }
            _ => ImageColorSpace::Gray,
        }
    }

    /// Convert a content-stream operand into a [`PdfObject`], so an inline
    /// image's parameters can go through the same resolution paths as an image
    /// XObject's dictionary. Inline operands are always direct — there are no
    /// indirect references inside a content stream — so this is a pure
    /// structural mapping.
    fn operand_to_object(&mut self, op: &Operand) -> PdfObject {
        match op {
            Operand::Int(v) => PdfObject::Integer(*v),
            Operand::Real(v) => PdfObject::Real(*v),
            Operand::Bool(v) => PdfObject::Boolean(*v),
            Operand::Null => PdfObject::Null,
            Operand::Name(n) => PdfObject::Name(self.intern(n)),
            Operand::String(s) => PdfObject::String(pdf_object::PdfString::new(s.as_slice())),
            Operand::Array(items) => {
                PdfObject::Array(items.iter().map(|i| self.operand_to_object(i)).collect())
            }
            Operand::Dict(pairs) => {
                let entries: Vec<(NameId, PdfObject)> = pairs
                    .iter()
                    .map(|(k, v)| (self.intern(k), self.operand_to_object(v)))
                    .collect();
                PdfObject::Dictionary(Arc::new(Dictionary::from_pairs(entries)))
            }
        }
    }

    /// Filter-decode inline image bytes through the general filters by
    /// wrapping them in a synthetic stream. A codec filter stops decoding:
    /// the triple is `(samples, codec kind, codec-encoded payload)`.
    #[allow(clippy::type_complexity)]
    fn decode_inline(
        &mut self,
        filters: &[Vec<u8>],
        data: &[u8],
    ) -> (
        Option<Arc<[u8]>>,
        Option<pdf_page_ir::ImageCodecKind>,
        Option<Arc<[u8]>>,
    ) {
        if filters.is_empty() {
            return (Some(Arc::from(data)), None, None);
        }
        let filter_obj = if filters.len() == 1 {
            PdfObject::Name(self.intern(&filters[0]))
        } else {
            PdfObject::Array(
                filters
                    .iter()
                    .map(|f| PdfObject::Name(self.intern(f)))
                    .collect(),
            )
        };
        let dict = Dictionary::from_pairs([(self.names.known.filter, filter_obj)]);
        let stream = PdfStream {
            dict,
            data: StreamData::Owned(Arc::from(data)),
        };
        match self.snapshot.decode_stream_data_to_codec(&stream, self.ctx) {
            Ok((bytes, None)) => (Some(Arc::from(bytes)), None, None),
            Ok((bytes, Some(name))) => (None, codec_kind(name.as_bytes()), Some(Arc::from(bytes))),
            Err(_) => (None, None, None),
        }
    }

    /// `gs`: apply a named ExtGState (ISO 32000-1 §8.4.5). Phase 2 applies the
    /// parameters the semantic layer models: fill/stroke alpha, blend mode,
    /// and line width; the rest (soft masks, transfer functions, halftones)
    /// arrive with later phases.
    fn op_ext_gstate(&mut self) -> Result<(), ContentError> {
        let Some(name) = self
            .operands
            .iter()
            .rev()
            .find_map(Operand::as_name)
            .map(<[u8]>::to_vec)
        else {
            return Ok(());
        };
        let nid = self.intern(&name);
        let Some(egs) = self.resource_subdict(self.names.known.ext_g_state) else {
            return Ok(());
        };
        let Some(entry) = egs.as_dict().and_then(|d| d.get(nid)).cloned() else {
            return Ok(());
        };
        let Some(state) = self.resolve_obj(&entry) else {
            return Ok(());
        };
        let dict = match state.as_dict() {
            Some(d) => d.clone(),
            None => return Ok(()),
        };
        let dict = &dict;

        if let Some(ca) = dict_num(dict, self.intern(b"ca")) {
            self.gs.fill_alpha = ca as f32;
            self.ops.push(SemanticOp::SetFillAlpha(ca as f32));
        }
        if let Some(ca) = dict_num(dict, self.intern(b"CA")) {
            self.gs.stroke_alpha = ca as f32;
            self.ops.push(SemanticOp::SetStrokeAlpha(ca as f32));
        }
        if let Some(lw) = dict_num(dict, self.intern(b"LW")) {
            self.gs.line_width = lw;
            self.ops.push(SemanticOp::SetLineWidth(lw));
        }
        // /OP (stroke) and /op (fill) overprint: recorded as a page feature
        // flag (§8.6.7); compositing itself is unchanged.
        for key in [&b"OP"[..], b"op"] {
            if matches!(dict.get(self.intern(key)), Some(PdfObject::Boolean(true))) {
                self.uses_overprint = true;
            }
        }
        if let Some(blend) = dict.get(self.intern(b"BM")) {
            // /BM may be a single name or an array (first recognized wins).
            let name = match blend {
                PdfObject::Name(n) => Some(*n),
                PdfObject::Array(items) => items.iter().find_map(PdfObject::as_name),
                _ => None,
            };
            if let Some(n) = name {
                let mode = map_blend_mode(&self.names.resolve(n));
                self.gs.blend = mode;
                self.ops.push(SemanticOp::SetBlendMode(mode));
            }
        }

        // /SMask: a soft-mask dictionary, or /None to disable.
        if let Some(smask) = dict.get(self.intern(b"SMask")).cloned() {
            if matches!(&smask, PdfObject::Name(n) if self.names.resolve(*n).as_ref() == b"None") {
                self.ops.push(SemanticOp::ClearSoftMask);
            } else if let Some(sm) = self.resolve_obj(&smask)
                && let Some(sm) = sm.as_dict()
            {
                let sm = sm.clone();
                let luminosity = matches!(
                    sm.get(self.intern(b"S")).and_then(PdfObject::as_name),
                    Some(s) if self.names.resolve(s).as_ref() == b"Luminosity"
                );
                // /BC (luminosity only): explicit backdrop color, converted
                // to RGB bytes with the device approximation (§11.6.5.2).
                let kind = if luminosity {
                    match read_num_array(&sm, self.intern(b"BC")) {
                        Some(bc) if !bc.is_empty() => {
                            let comps: Vec<f32> = bc.iter().map(|v| *v as f32).collect();
                            let rgba = comps_to_rgba(&comps);
                            MaskKind::LuminosityBc {
                                backdrop: [
                                    (rgba[0] * 255.0 + 0.5) as u8,
                                    (rgba[1] * 255.0 + 0.5) as u8,
                                    (rgba[2] * 255.0 + 0.5) as u8,
                                ],
                            }
                        }
                        _ => MaskKind::Luminosity,
                    }
                } else {
                    MaskKind::Alpha
                };
                let transfer = self.build_transfer_lut(&sm);
                if let Some(g) = sm.get(self.intern(b"G")).cloned() {
                    let group_id = g.as_reference();
                    if let Some(group) = self.resolve_obj(&g)
                        && let PdfObject::Stream(stream) = &*group
                    {
                        let stream = stream.clone();
                        self.run_soft_mask(kind, transfer, &stream, group_id)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Sample the `/SMask` dictionary's `/TR` transfer function into a
    /// 256-entry LUT (`None` for absent or `/Identity` — and for a sampled
    /// LUT that turns out to *be* the identity, so the common no-op case
    /// costs the render nothing).
    fn build_transfer_lut(&mut self, sm: &Dictionary) -> Option<pdf_page_ir::TransferLut> {
        let tr = sm.get(self.intern(b"TR")).cloned()?;
        if matches!(&tr, PdfObject::Name(n) if self.names.resolve(*n).as_ref() == b"Identity") {
            return None;
        }
        let func = self.build_function(&tr, 0)?;
        let mut lut = [0u8; 256];
        let mut identity = true;
        for (i, slot) in lut.iter_mut().enumerate() {
            let t = i as f32 / 255.0;
            let v = func.eval(t).first().copied().unwrap_or(t).clamp(0.0, 1.0);
            *slot = (v * 255.0 + 0.5) as u8;
            identity &= *slot == i as u8;
        }
        if identity {
            None
        } else {
            Some(std::sync::Arc::new(lut))
        }
    }

    /// Render a soft-mask group's content isolated, bracketed by
    /// Begin/EndSoftMask so a backend can derive the per-pixel mask (its
    /// luminosity or alpha) and apply it to subsequent painting.
    fn run_soft_mask(
        &mut self,
        kind: MaskKind,
        transfer: Option<pdf_page_ir::TransferLut>,
        stream: &PdfStream,
        group_id: Option<ObjectId>,
    ) -> Result<(), ContentError> {
        if self.invoke_depth >= self.limits.max_invoke_depth {
            return Err(ContentError::RecursionDepth(self.invoke_depth));
        }
        // A soft-mask group that references itself (directly or through a
        // cycle) renders *empty* at the recursive instance: a luminosity group
        // with no marks is 0 everywhere, i.e. fully masked, and an alpha group
        // is likewise fully transparent (§11.6.5.2). Emit an empty
        // Begin/EndSoftMask so the backend derives that all-masking mask
        // instead of dropping the mask (over-painting) or recursing to the
        // depth limit. resvg_masking_mask_recursive_on_self is exactly this.
        let recursive = group_id.is_some_and(|id| self.soft_mask_stack.contains(&id));
        if recursive {
            self.ops.push(SemanticOp::BeginSoftMask { kind, transfer });
            self.ops.push(SemanticOp::EndSoftMask);
            return Ok(());
        }
        let content = self.snapshot.decode_stream_data(stream, self.ctx)?;
        let matrix = read_matrix(&stream.dict, self.intern(b"Matrix"));
        let bbox = read_rect(&stream.dict, self.intern(b"BBox"));
        let form_res = match stream.dict.get(self.names.known.resources).cloned() {
            Some(res) => self.resolve_obj(&res),
            None => self.resources.clone(),
        };

        self.ops.push(SemanticOp::BeginSoftMask { kind, transfer });
        // Isolate the mask content's graphics state (Save/Restore) so its
        // transform and paint changes do not leak into the masked painting.
        self.gs_stack.push(self.gs.clone());
        self.ops.push(SemanticOp::Save);
        if matrix != Matrix::IDENTITY {
            self.gs.ctm = matrix.then(self.gs.ctm);
            self.ops.push(SemanticOp::Concat(matrix));
        }
        if let Some([x0, y0, x1, y1]) = bbox {
            let path = rect_path(x0, y0, x1 - x0, y1 - y0);
            let pid = self.intern_path(path);
            self.ops.push(SemanticOp::Clip {
                path: pid,
                rule: FillRule::NonZero,
            });
            self.gs.clip_depth += 1;
        }
        // Mask content paints fresh (no inherited alpha/blend/soft-mask).
        self.gs.fill_alpha = 1.0;
        self.gs.stroke_alpha = 1.0;
        self.gs.blend = BlendMode::Normal;

        let saved_res = self.resources.clone();
        let saved_caches = self.take_resource_caches();
        // Patterns referenced by the mask group anchor to the group's default
        // space — the CTM here, after the group /Matrix concat (§8.7.3.1).
        let saved_pattern_base = self.pattern_base;
        self.pattern_base = self.gs.ctm;
        self.resources = form_res;
        self.invoke_depth += 1;
        if let Some(id) = group_id {
            self.soft_mask_stack.push(id);
        }
        let operands = std::mem::take(&mut self.operands);
        self.ops
            .push(SemanticOp::BeginPaintOrigin(PaintOrigin::SoftMaskContent));
        let result = self.run(&content);
        self.ops.push(SemanticOp::EndPaintOrigin);
        self.operands = operands;
        if group_id.is_some() {
            self.soft_mask_stack.pop();
        }
        self.invoke_depth -= 1;
        self.resources = saved_res;
        self.pattern_base = saved_pattern_base;
        self.restore_resource_caches(saved_caches);

        if let Some(prev) = self.gs_stack.pop() {
            self.gs = prev;
            self.ops.push(SemanticOp::Restore);
        }
        self.ops.push(SemanticOp::EndSoftMask);
        result
    }

    // --- shadings & patterns ----------------------------------------------

    /// `sh`: paint a shading dictionary from the `/Shading` resources across
    /// the current clip in current user space (ISO 32000-1 §8.7.4.2).
    fn op_shading(&mut self) -> Result<(), ContentError> {
        let Some(name) = self
            .operands
            .iter()
            .rev()
            .find_map(Operand::as_name)
            .map(<[u8]>::to_vec)
        else {
            return Ok(());
        };
        let nid = self.intern(&name);
        let Some(shadings) = self.resource_subdict(self.names.known.shading) else {
            return Ok(());
        };
        let Some(entry) = shadings.as_dict().and_then(|d| d.get(nid)).cloned() else {
            return Ok(());
        };
        let object = entry.as_reference();
        let Some(obj) = self.resolve_obj(&entry) else {
            return Ok(());
        };
        let kind = self.build_shading(&obj);
        // A shading type we cannot rasterize is skipped for `sh` (nothing to
        // paint without a region-filling color); patterns keep the hook.
        if matches!(kind, SemShadingKind::Unsupported { .. }) {
            return Ok(());
        }
        let bbox = self.shading_bbox(&obj);
        let id = ShadingId(self.shadings.len() as u32);
        self.shadings.push(SemShading { object, kind, bbox });
        self.ops.push(SemanticOp::PaintShading(id));
        Ok(())
    }

    /// Park the name-keyed resolution caches while a nested content stream
    /// runs under a different `/Resources` dictionary.
    ///
    /// These caches are keyed by resource *name*, and a name means whatever the
    /// currently active `/Resources` says it means. A form XObject, a soft mask
    /// and a Type 3 CharProc each bring their own dictionary, so an entry
    /// filled under one must not answer a lookup under another. pdfjs/issue8565
    /// gives `/Sh1` three different meanings — page, tiling pattern, soft-mask
    /// form — and the soft mask's entry was answering the tiling pattern's
    /// lookup, painting the mask's grey luminosity ramp where an orange shading
    /// belonged.
    ///
    /// All five move together on purpose. `show_type3` reaches `type3_cache`
    /// through `fonts[id].resource_name`, so scoping `font_cache` without
    /// `type3_cache` (or the reverse) can hand back a font whose Type 3 data is
    /// no longer reachable and silently drop glyphs.
    ///
    /// Known cost, measured and accepted: clearing `font_cache` re-resolves an
    /// identical font object into a second `SemFont`, which shifts font
    /// substitution slightly on some text pages (An Investors Guide to Trading
    /// Options: p22 +0.0026, p33 +0.0030, p44 +0.0045, p55 -0.0040 against
    /// PDFium). It is worth it — the same scoping is what fixes
    /// processstudies.46.1.0025's five sampled pages (~0.06 -> ~0.007 each).
    /// Keying these two by resolved object rather than by name would get both;
    /// nothing measured today needs it.
    ///
    /// Called with the `self.resources` swap it belongs to; restored by
    /// [`Self::restore_resource_caches`] alongside `saved_res`.
    fn take_resource_caches(&mut self) -> ResourceCaches {
        ResourceCaches {
            font: std::mem::take(&mut self.font_cache),
            type3: std::mem::take(&mut self.type3_cache),
            pattern: std::mem::take(&mut self.pattern_cache),
            tint: std::mem::take(&mut self.tint_cache),
            cie: std::mem::take(&mut self.cie_cache),
        }
    }

    fn restore_resource_caches(&mut self, saved: ResourceCaches) {
        self.font_cache = saved.font;
        self.type3_cache = saved.type3;
        self.pattern_cache = saved.pattern;
        self.tint_cache = saved.tint;
        self.cie_cache = saved.cie;
    }

    /// Resolve a `/Pattern` resource by name into a resolved [`SemColor`]
    /// (shading pattern → table entry; tiling pattern → compiled cell).
    /// `under` are the numeric operands preceding the name (the underlying
    /// color for an uncolored tiling pattern). Returns `None` when the pattern
    /// cannot be resolved so the caller falls back to the symbolic form.
    fn resolve_pattern(&mut self, name: NameId, under: &[f64]) -> Option<SemColor> {
        if let Some(cached) = self.pattern_cache.get(&name) {
            return cached.clone();
        }
        let resolved = self.resolve_pattern_uncached(name, under);
        self.pattern_cache.insert(name, resolved.clone());
        resolved
    }

    fn resolve_pattern_uncached(&mut self, name: NameId, under: &[f64]) -> Option<SemColor> {
        let patterns = self.resource_subdict(self.names.known.pattern)?;
        let entry = patterns.as_dict()?.get(name)?.clone();
        let object = entry.as_reference();
        let obj = self.resolve_obj(&entry)?;
        let dict = obj.as_dict()?.clone();
        let pattern_type = dict_int(&dict, self.intern(b"PatternType")).unwrap_or(0);
        // `/Matrix` maps pattern space to the *default space of the stream
        // referencing the pattern* (§8.7.3.1) — compose that anchor in, so a
        // pattern used inside a form or soft-mask group invoked under a CTM
        // (skew, scale) lands where the group's content does. Identity for
        // page-level content (`pattern_base` is identity there).
        let matrix = read_matrix(&dict, self.intern(b"Matrix")).then(self.pattern_base);
        match pattern_type {
            2 => {
                // Shading pattern: resolve its `/Shading` dictionary/stream.
                let shading = self.resolve_obj(&dict.get(self.intern(b"Shading"))?.clone())?;
                let kind = self.build_shading(&shading);
                if matches!(kind, SemShadingKind::Unsupported { .. }) {
                    return None;
                }
                let bbox = self.shading_bbox(&shading);
                let id = ShadingId(self.shadings.len() as u32);
                self.shadings.push(SemShading { object, kind, bbox });
                Some(SemColor::ShadingPattern {
                    shading: id,
                    matrix,
                })
            }
            1 => {
                // Tiling pattern: compile the cell content stream.
                let PdfObject::Stream(stream) = &*obj else {
                    return None;
                };
                let tiling = self.compile_tiling(object, &stream.clone(), &dict, under)?;
                let id = TilingId(self.tilings.len() as u32);
                self.tilings.push(tiling);
                Some(SemColor::TilingPattern { tiling: id, matrix })
            }
            _ => None,
        }
    }

    /// Build a shading parameterization: axial/radial (types 2/3) with a
    /// pre-sampled RGBA ramp, function-based (type 1) sampled on a grid,
    /// and mesh shadings (types 4–7) decoded to triangles/patches;
    /// unrecognized or malformed types degrade to a `/Background`-only hook.
    /// The shading's `/BBox` (ISO 32000-1 §8.7.4.3): a temporary clip applied
    /// when the shading is painted, in the shading's target coordinate space.
    /// Normalized to `[x0, y0, x1, y1]` with `x0 ≤ x1`, `y0 ≤ y1`. Works for
    /// both dictionary shadings (types 1–3) and stream shadings (mesh 4–7),
    /// since `as_dict` yields a stream's dictionary too.
    fn shading_bbox(&self, obj: &PdfObject) -> Option<[f32; 4]> {
        let dict = obj.as_dict()?;
        let r = read_rect(dict, self.intern(b"BBox"))?;
        Some([
            r[0].min(r[2]) as f32,
            r[1].min(r[3]) as f32,
            r[0].max(r[2]) as f32,
            r[1].max(r[3]) as f32,
        ])
    }

    fn build_shading(&mut self, obj: &PdfObject) -> SemShadingKind {
        let Some(dict) = obj.as_dict() else {
            return SemShadingKind::Unsupported { background: None };
        };
        let dict = dict.clone();
        let stype = dict_int(&dict, self.intern(b"ShadingType")).unwrap_or(0);
        let domain = read_num_array(&dict, self.intern(b"Domain"))
            .and_then(|a| (a.len() >= 2).then(|| [a[0], a[1]]))
            .unwrap_or([0.0, 1.0]);
        let extend = self.read_extend(&dict);
        match stype {
            1 => self.build_function_grid(&dict),
            2 | 3 => {
                let coords = read_num_array(&dict, self.intern(b"Coords")).unwrap_or_default();
                let funcs = dict
                    .get(self.intern(b"Function"))
                    .cloned()
                    .and_then(|f| self.build_functions(&f))
                    .unwrap_or_default();
                let ramp =
                    self.build_shading_ramp(&dict, &funcs, [domain[0] as f32, domain[1] as f32]);
                let background = self.read_background(&dict);
                if stype == 2 && coords.len() >= 4 {
                    SemShadingKind::Axial {
                        coords: [coords[0], coords[1], coords[2], coords[3]],
                        domain,
                        extend,
                        ramp,
                        background,
                    }
                } else if stype == 3 && coords.len() >= 6 {
                    SemShadingKind::Radial {
                        coords: [
                            coords[0], coords[1], coords[2], coords[3], coords[4], coords[5],
                        ],
                        domain,
                        extend,
                        ramp,
                        background,
                    }
                } else {
                    SemShadingKind::Unsupported {
                        background: self.read_background(&dict),
                    }
                }
            }
            4..=7 => {
                // Mesh shadings live in a stream object (dict + packed data).
                if let PdfObject::Stream(stream) = obj {
                    let stream = stream.clone();
                    self.build_mesh(stype, &stream, &dict)
                } else {
                    SemShadingKind::Unsupported {
                        background: self.read_background(&dict),
                    }
                }
            }
            _ => SemShadingKind::Unsupported {
                background: self.read_background(&dict),
            },
        }
    }

    /// Sample a type 2/3 shading's `/Function` into a 256-entry RGBA ramp,
    /// converting each sample through the shading's `/ColorSpace`. The function
    /// outputs are components *in that space* (§8.7.4.5.3), so a
    /// Separation/DeviceN shading emits colorant tints that must run through the
    /// tint transform, and a CIE shading emits Lab/Cal components — exactly the
    /// conversions the fill path (`eval_tint`/`eval_cie`) and the mesh path
    /// (`cs_to_rgba`) already apply. Skipping this (the old `build_ramp`) fed
    /// raw components to `comps_to_rgba`, which reads them by arity alone — a
    /// 2-colorant DeviceN (e.g. `[/DeviceN [/Cyan /Black] …]`) then collapsed to
    /// pure black, flooding whole-page background shadings (rosiesmenu3).
    fn build_shading_ramp(
        &mut self,
        dict: &Dictionary,
        funcs: &[pdf_function::Function],
        domain: [f32; 2],
    ) -> Vec<[f32; 4]> {
        let cs = dict
            .get(self.intern(b"ColorSpace"))
            .cloned()
            .and_then(|o| self.resolve_shading_cs(&o));
        const RAMP: usize = 256;
        let mut ramp = Vec::with_capacity(RAMP);
        for i in 0..RAMP {
            let t = domain[0] + (i as f32 / (RAMP - 1) as f32) * (domain[1] - domain[0]);
            let comps: Vec<f32> = funcs.iter().flat_map(|f| f.eval(t)).collect();
            ramp.push(match &cs {
                Some(ShadingCs::Tint(space)) => {
                    let vals: Vec<f64> = comps.iter().map(|v| *v as f64).collect();
                    let rgb = sem_color_to_rgb8(&self.eval_tint(space, &vals));
                    [
                        rgb[0] as f32 / 255.0,
                        rgb[1] as f32 / 255.0,
                        rgb[2] as f32 / 255.0,
                        1.0,
                    ]
                }
                Some(ShadingCs::Cie(cie)) => {
                    let vals: Vec<f64> = comps.iter().map(|v| *v as f64).collect();
                    let rgb = sem_color_to_rgb8(&eval_cie(*cie, &vals));
                    [
                        rgb[0] as f32 / 255.0,
                        rgb[1] as f32 / 255.0,
                        rgb[2] as f32 / 255.0,
                        1.0,
                    ]
                }
                Some(ShadingCs::Image(space)) => cs_to_rgba(space, &comps),
                None => comps_to_rgba(&comps),
            });
        }
        ramp
    }

    /// Classify a shading `/ColorSpace` for ramp conversion: Separation/DeviceN
    /// carry a tint transform, CIE families carry conversion parameters, and
    /// everything else (Device/ICCBased/Indexed) reduces to an image color
    /// space that `cs_to_rgba` handles.
    fn resolve_shading_cs(&mut self, obj: &PdfObject) -> Option<ShadingCs> {
        let resolved = self.resolve_obj(obj)?;
        if let PdfObject::Array(items) = &*resolved {
            let items = items.clone();
            if let Some(head) = items.first().and_then(PdfObject::as_name) {
                match self.names.resolve(head).as_ref() {
                    b"Separation" | b"DeviceN" => {
                        if let Some(space) = self.build_tint_space_from_array(&items) {
                            return Some(ShadingCs::Tint(space));
                        }
                    }
                    b"Lab" | b"CalRGB" | b"CalGray" => {
                        if let Some(cie) = self.cie_space_from_items(&items) {
                            return Some(ShadingCs::Cie(cie));
                        }
                    }
                    _ => {}
                }
            }
        }
        self.resolve_image_colorspace(&resolved)
            .map(ShadingCs::Image)
    }

    /// Type 1 (function-based) shading: sample the 2-in color function on a
    /// fixed grid over `/Domain` (the backend interpolates cell-wise).
    fn build_function_grid(&mut self, dict: &Dictionary) -> SemShadingKind {
        let background = self.read_background(dict);
        let domain = read_num_array(dict, self.intern(b"Domain"))
            .and_then(|a| (a.len() >= 4).then(|| [a[0], a[1], a[2], a[3]]))
            .unwrap_or([0.0, 1.0, 0.0, 1.0]);
        let matrix = read_matrix(dict, self.intern(b"Matrix"));
        let Some(funcs) = dict
            .get(self.intern(b"Function"))
            .cloned()
            .and_then(|f| self.build_functions(&f))
            .filter(|f| !f.is_empty())
        else {
            return SemShadingKind::Unsupported { background };
        };
        let cs = dict
            .get(self.intern(b"ColorSpace"))
            .cloned()
            .and_then(|o| self.resolve_image_colorspace(&o));
        // 64×64: fine enough that bilinear-free cell lookup is invisible at
        // typical zoom, cheap enough to sample eagerly (4096 evaluations).
        const GRID: u32 = 64;
        let (gw, gh) = (GRID, GRID);
        let mut colors = Vec::with_capacity((gw * gh) as usize);
        for t in 0..gh {
            for s in 0..gw {
                let x = domain[0] + (s as f64 + 0.5) / gw as f64 * (domain[1] - domain[0]);
                let y = domain[2] + (t as f64 + 0.5) / gh as f64 * (domain[3] - domain[2]);
                let comps: Vec<f32> = funcs
                    .iter()
                    .flat_map(|f| f.eval_n(&[x as f32, y as f32]))
                    .collect();
                colors.push(match &cs {
                    Some(c) => cs_to_rgba(c, &comps),
                    None => comps_to_rgba(&comps),
                });
            }
        }
        SemShadingKind::FunctionGrid {
            domain,
            matrix,
            grid_w: gw,
            grid_h: gh,
            colors,
            background,
        }
    }

    /// Types 4–7: decode the mesh stream into triangles (4/5) or tensor
    /// patches (6/7). Colors run through the optional `/Function` and the
    /// shading's `/ColorSpace`, exactly like the type 2/3 ramp path.
    fn build_mesh(
        &mut self,
        stype: i64,
        stream: &pdf_object::PdfStream,
        dict: &Dictionary,
    ) -> SemShadingKind {
        let background = self.read_background(dict);
        let unsupported = SemShadingKind::Unsupported { background };
        let Ok(data) = self.snapshot.decode_stream_data(stream, self.ctx) else {
            return unsupported;
        };
        let funcs = dict
            .get(self.intern(b"Function"))
            .cloned()
            .and_then(|f| self.build_functions(&f))
            .unwrap_or_default();
        let Some(cs) = dict
            .get(self.intern(b"ColorSpace"))
            .cloned()
            .and_then(|o| self.resolve_image_colorspace(&o))
        else {
            return unsupported;
        };
        let ncomps = if funcs.is_empty() { cs.components() } else { 1 };
        let Some(decode) = read_num_array(dict, self.intern(b"Decode")) else {
            return unsupported;
        };
        if decode.len() < 4 + ncomps * 2 {
            return unsupported;
        }
        let params = crate::mesh::MeshParams {
            coord_bits: dict_int(dict, self.intern(b"BitsPerCoordinate"))
                .unwrap_or(0)
                .max(0) as u32,
            component_bits: dict_int(dict, self.intern(b"BitsPerComponent"))
                .unwrap_or(0)
                .max(0) as u32,
            flag_bits: dict_int(dict, self.intern(b"BitsPerFlag"))
                .unwrap_or(0)
                .max(0) as u32,
            components: ncomps,
            x: [decode[0] as f32, decode[1] as f32],
            y: [decode[2] as f32, decode[3] as f32],
            comp: (0..ncomps)
                .map(|i| [decode[4 + 2 * i] as f32, decode[5 + 2 * i] as f32])
                .collect(),
        };
        // Type 5 carries no flags; everything else requires a valid width.
        if !params.valid(stype != 5) {
            return unsupported;
        }
        let convert = |vals: &[f32]| -> [f32; 4] {
            if funcs.is_empty() {
                cs_to_rgba(&cs, vals)
            } else {
                let comps: Vec<f32> = funcs.iter().flat_map(|f| f.eval(vals[0])).collect();
                cs_to_rgba(&cs, &comps)
            }
        };
        match stype {
            4 => {
                let triangles = crate::mesh::read_free_triangles(&data, &params, &convert);
                if triangles.is_empty() {
                    unsupported
                } else {
                    SemShadingKind::MeshTriangles {
                        shading_type: 4,
                        triangles,
                        background,
                    }
                }
            }
            5 => {
                let row = dict_int(dict, self.intern(b"VerticesPerRow")).unwrap_or(0);
                if row < 2 {
                    return unsupported;
                }
                let triangles =
                    crate::mesh::read_lattice_triangles(&data, &params, row as usize, &convert);
                if triangles.is_empty() {
                    unsupported
                } else {
                    SemShadingKind::MeshTriangles {
                        shading_type: 5,
                        triangles,
                        background,
                    }
                }
            }
            6 | 7 => {
                let patches = crate::mesh::read_patches(&data, &params, stype == 7, &convert);
                if patches.is_empty() {
                    unsupported
                } else {
                    SemShadingKind::MeshPatches {
                        shading_type: stype as u8,
                        patches,
                        background,
                    }
                }
            }
            _ => unsupported,
        }
    }

    fn read_extend(&mut self, dict: &Dictionary) -> [bool; 2] {
        match dict.get(self.intern(b"Extend")) {
            Some(PdfObject::Array(a)) if a.len() >= 2 => [
                matches!(a[0], PdfObject::Boolean(true)),
                matches!(a[1], PdfObject::Boolean(true)),
            ],
            _ => [false, false],
        }
    }

    fn read_background(&mut self, dict: &Dictionary) -> Option<[f32; 4]> {
        let comps = read_num_array(dict, self.intern(b"Background"))?;
        Some(comps_to_rgba(
            &comps.iter().map(|v| *v as f32).collect::<Vec<_>>(),
        ))
    }

    /// Build the color function(s) of a shading: `/Function` may be a single
    /// function or an array of single-output functions (one per component).
    fn build_functions(&mut self, obj: &PdfObject) -> Option<Vec<pdf_function::Function>> {
        let resolved = self.resolve_obj(obj)?;
        if let PdfObject::Array(items) = &*resolved {
            let mut out = Vec::new();
            for it in items.iter() {
                out.push(self.build_function(it, 0)?);
            }
            Some(out)
        } else {
            Some(vec![self.build_function(obj, 0)?])
        }
    }

    /// Build one PDF function. Handles Type 0 (sampled, both the 1-input
    /// shading form → [`pdf_function::Function::Sampled`] and the m-input
    /// DeviceN form → `SampledN`), Type 2 (exponential), Type 3 (stitching)
    /// and Type 4 (PostScript calculator → `PostScript`, falling back to
    /// `Identity` on a malformed program). `depth` bounds Type 3 nesting so a
    /// cyclic `/Functions` reference cannot recurse without limit
    /// (malformed-resource guard).
    fn build_function(&mut self, obj: &PdfObject, depth: u32) -> Option<pdf_function::Function> {
        if depth > 32 {
            return None;
        }
        let resolved = self.resolve_obj(obj)?;
        let dict = resolved.as_dict()?;
        let ftype = dict_int(dict, self.intern(b"FunctionType"))?;
        let domain = read_num_array(dict, self.intern(b"Domain"))
            .and_then(|a| (a.len() >= 2).then(|| [a[0] as f32, a[1] as f32]))
            .unwrap_or([0.0, 1.0]);
        match ftype {
            2 => {
                let c0 = read_num_array(dict, self.intern(b"C0"))
                    .map(|v| v.iter().map(|x| *x as f32).collect())
                    .unwrap_or_else(|| vec![0.0]);
                let c1 = read_num_array(dict, self.intern(b"C1"))
                    .map(|v| v.iter().map(|x| *x as f32).collect())
                    .unwrap_or_else(|| vec![1.0]);
                let n = dict_num(dict, self.intern(b"N")).unwrap_or(1.0) as f32;
                Some(pdf_function::Function::Exponential { domain, c0, c1, n })
            }
            3 => {
                let sub = dict.get(self.intern(b"Functions")).cloned()?;
                let sub = self.resolve_obj(&sub)?;
                let PdfObject::Array(items) = &*sub else {
                    return None;
                };
                let mut functions = Vec::new();
                for it in items.iter() {
                    functions.push(self.build_function(it, depth + 1)?);
                }
                let bounds = read_num_array(dict, self.intern(b"Bounds"))
                    .map(|v| v.iter().map(|x| *x as f32).collect())
                    .unwrap_or_default();
                let encode = read_num_array(dict, self.intern(b"Encode"))
                    .map(pairs_f32)
                    .unwrap_or_default();
                Some(pdf_function::Function::Stitching {
                    domain,
                    functions,
                    bounds,
                    encode,
                })
            }
            0 => {
                let PdfObject::Stream(stream) = &*resolved else {
                    return None;
                };
                let data = self.snapshot.decode_stream_data(stream, self.ctx).ok()?;
                let sizes = read_num_array(dict, self.intern(b"Size"))?;
                if sizes.is_empty() {
                    return None;
                }
                let bps = dict_int(dict, self.intern(b"BitsPerSample"))? as u32;
                let range = read_num_array(dict, self.intern(b"Range")).map(pairs_f32)?;
                let n_out = range.len();
                if n_out == 0 {
                    return None;
                }

                if sizes.len() == 1 {
                    // ---- 1-input Type 0 (the shading hot path, unchanged) ----
                    let size = sizes[0] as usize;
                    // Bound the sample table against an absurd /Size (allocation
                    // guard): shadings only need modest resolution.
                    if size == 0 || size.saturating_mul(n_out) > MAX_SAMPLED_FN {
                        return None;
                    }
                    let encode = read_num_array(dict, self.intern(b"Encode"))
                        .and_then(|a| (a.len() >= 2).then(|| [a[0] as f32, a[1] as f32]))
                        .unwrap_or([0.0, (size.max(1) - 1) as f32]);
                    let decode = read_num_array(dict, self.intern(b"Decode"))
                        .map(pairs_f32)
                        .unwrap_or_else(|| range.clone());
                    let samples = decode_samples(&data, size, n_out, bps);
                    return Some(pdf_function::Function::Sampled {
                        domain,
                        encode,
                        size,
                        n_out,
                        decode,
                        samples,
                    });
                }

                // ---- m-input Type 0 (DeviceN / tint transforms, §7.10.2) ----
                // Samples are row-major with input 0 varying fastest, matching
                // `Function::SampledN`; `decode_samples` reads them in stream
                // order, which is exactly that layout.
                let m = sizes.len();
                let sizes: Vec<usize> = sizes.iter().map(|v| *v as usize).collect();
                // Total grid points = product of the per-axis sizes. Fold with
                // checked_mul so a corrupt /Size cannot overflow or over-allocate;
                // then apply the same MAX_SAMPLED_FN cap as the 1-D path.
                let mut points: usize = 1;
                for &s in &sizes {
                    if s == 0 {
                        return None;
                    }
                    points = points.checked_mul(s)?;
                }
                if points.checked_mul(n_out).is_none_or(|t| t > MAX_SAMPLED_FN) {
                    return None;
                }
                // Per-axis /Domain and /Encode (2*m values each). Encode defaults
                // per axis to `[0, size_i - 1]`, mirroring the 1-D convention.
                let dom_pairs = read_num_array(dict, self.intern(b"Domain"))
                    .map(pairs_f32)
                    .unwrap_or_default();
                let domain_n: Vec<[f32; 2]> = (0..m)
                    .map(|i| dom_pairs.get(i).copied().unwrap_or([0.0, 1.0]))
                    .collect();
                let enc_pairs = read_num_array(dict, self.intern(b"Encode"))
                    .map(pairs_f32)
                    .unwrap_or_default();
                let encode_n: Vec<[f32; 2]> = (0..m)
                    .map(|i| {
                        enc_pairs
                            .get(i)
                            .copied()
                            .unwrap_or([0.0, (sizes[i].max(1) - 1) as f32])
                    })
                    .collect();
                let decode = read_num_array(dict, self.intern(b"Decode"))
                    .map(pairs_f32)
                    .unwrap_or_else(|| range.clone());
                let samples = decode_samples(&data, points, n_out, bps);
                Some(pdf_function::Function::SampledN {
                    domain: domain_n,
                    encode: encode_n,
                    size: sizes,
                    n_out,
                    decode,
                    samples,
                })
            }
            4 => {
                // Type 4 (PostScript calculator, §7.10.5): a *stream* whose
                // decoded bytes are the `{ … }` program. `/Range` is required
                // (it fixes the output arity); without it we cannot build the
                // function and return `None` for the caller to approximate.
                let PdfObject::Stream(stream) = &*resolved else {
                    return None;
                };
                let range = read_num_array(dict, self.intern(b"Range")).map(pairs_f32)?;
                if range.is_empty() {
                    return None;
                }
                let n_out = range.len();
                let dom_pairs = read_num_array(dict, self.intern(b"Domain"))
                    .map(pairs_f32)
                    .unwrap_or_default();
                let domain_n: Vec<[f32; 2]> = if dom_pairs.is_empty() {
                    vec![[0.0, 1.0]]
                } else {
                    dom_pairs
                };
                let bytes = self.snapshot.decode_stream_data(stream, self.ctx).ok()?;
                match pdf_function::parse_postscript(&bytes) {
                    Some(program) => Some(pdf_function::Function::PostScript {
                        domain: domain_n,
                        range,
                        program,
                    }),
                    // Malformed program text: fall back to Identity of the right
                    // arity so the page still compiles (tolerant).
                    None => Some(pdf_function::Function::Identity { n_out }),
                }
            }
            _ => None,
        }
    }

    /// Compile a tiling pattern's (PatternType 1) cell content stream into a
    /// nested [`SemanticPage`]. Reuses the same interpreter machinery under a
    /// fresh graphics state, guarded by the shared recursion depth.
    fn compile_tiling(
        &mut self,
        object: Option<ObjectId>,
        stream: &PdfStream,
        dict: &Dictionary,
        under: &[f64],
    ) -> Option<SemTiling> {
        if self.invoke_depth >= self.limits.max_invoke_depth {
            return None;
        }
        let paint_type = dict_int(dict, self.intern(b"PaintType")).unwrap_or(1);
        let uncolored = paint_type == 2;
        let bbox = read_rect(dict, self.intern(b"BBox")).unwrap_or([0.0, 0.0, 1.0, 1.0]);
        let x_step = dict_num(dict, self.intern(b"XStep")).unwrap_or(bbox[2] - bbox[0]);
        let y_step = dict_num(dict, self.intern(b"YStep")).unwrap_or(bbox[3] - bbox[1]);
        let under_color = comps_to_rgba(&under.iter().map(|v| *v as f32).collect::<Vec<_>>());
        let content = self.snapshot.decode_stream_data(stream, self.ctx).ok()?;
        let pattern_res = match stream.dict.get(self.names.known.resources).cloned() {
            Some(res) => self.resolve_obj(&res),
            None => self.resources.clone(),
        };

        // Compile the cell in a fresh sub-interpreter (its own state + tables);
        // the recursion depth is shared to bound nested patterns/forms.
        let mut sub = Interpreter::new(
            self.snapshot,
            self.ctx,
            self.limits,
            pattern_res,
            self.system_fonts.clone(),
        );
        // The cell is a separate interpretation but the same document scope:
        // it inherits the page's resources for categories its own pattern
        // `/Resources` omits, exactly as a form does.
        sub.page_resources = self.page_resources.clone();
        sub.invoke_depth = self.invoke_depth + 1;
        sub.ops
            .push(SemanticOp::BeginPaintOrigin(PaintOrigin::TilingPatternCell));
        let result = sub.run(&content);
        sub.ops.push(SemanticOp::EndPaintOrigin);
        if result.is_err() {
            return None;
        }
        let cell = sub.finish(PageBounds {
            crop: Rect {
                x0: bbox[0],
                y0: bbox[1],
                x1: bbox[2],
                y1: bbox[3],
            },
            rotate: 0,
        });
        Some(SemTiling {
            object,
            uncolored,
            under_color,
            bbox,
            x_step,
            y_step,
            cell: Arc::new(cell),
        })
    }

    // --- resource resolution ----------------------------------------------

    /// Resolve a category sub-dictionary of the active resources (e.g.
    /// `/Font`, `/XObject`), following an indirect reference if present.
    fn resource_subdict(&mut self, category: NameId) -> Option<Arc<PdfObject>> {
        if let Some(res) = self.resources.clone()
            && let Some(entry) = res.as_dict().and_then(|d| d.get(category)).cloned()
        {
            return self.resolve_obj(&entry);
        }
        // The active `/Resources` has no dictionary for this category at all:
        // inherit the page's (PDFium `CPDF_StreamContentParser::
        // FindResourceHolder`). Forms in the wild routinely carry a partial
        // `/Resources` and expect the page's to fill the gaps — pdfjs/issue5939
        // paints with patterns its form never declares.
        //
        // The fallback is per *category*, not per name: a form that does
        // declare `/Pattern` owns that namespace completely, and a name it
        // omits stays unresolved rather than reaching past it. That is what
        // keeps pdfjs/issue8565 correct, where three scopes each define their
        // own `/Sh1`.
        let page = self.page_resources.clone()?;
        let entry = page.as_dict()?.get(category)?.clone();
        self.resolve_obj(&entry)
    }

    /// Resolve an object to a shared value: references go through the
    /// repository; direct values are wrapped as-is.
    /// Read an integer entry, following an indirect reference.
    ///
    /// Image dictionaries do carry `/Height 5 0 R` in the wild (issue5592.pdf,
    /// and the Edgerton scan). A direct-only read yields 0 there, which reaches
    /// the codec as a zero dimension and drops the image, so the page renders
    /// blank while every other viewer draws it.
    fn dict_int_indirect(&mut self, dict: &Dictionary, key: NameId) -> Option<i64> {
        let obj = dict.get(key)?.clone();
        if let Some(v) = obj.as_int() {
            return Some(v);
        }
        self.resolve_obj(&obj)?.as_int()
    }

    fn resolve_obj(&mut self, obj: &PdfObject) -> Option<Arc<PdfObject>> {
        match obj {
            PdfObject::Reference(id) => self
                .snapshot
                .objects()
                .resolve(self.snapshot, *id, self.ctx)
                .ok(),
            direct => Some(Arc::new(direct.clone())),
        }
    }
}

// --- free helpers ----------------------------------------------------------

fn get(nums: &[f64], i: usize) -> f64 {
    nums.get(i).copied().unwrap_or(0.0)
}

/// Decode a PDF text string into the UTF-16 representation retained by the
/// semantic layer. UTF-16 BOM forms and PDFDocEncoding are decoded here while
/// the marked-content string is still available.
fn decode_pdf_text_string(bytes: &[u8]) -> Vec<u16> {
    pdf_object::decode_text_string(bytes)
}

/// Convert an alternate-space [`SemColor`] (as produced by `eval_tint`) to an
/// 8-bit sRGB triple, using the same `pdf_color` conversions as the fill path.
/// Only the device families `eval_tint` yields are handled; anything else
/// (which it never returns) falls back to black.
fn sem_color_to_rgb8(c: &SemColor) -> [u8; 3] {
    let to = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let rgb = match c {
        SemColor::DeviceGray(g) => pdf_color::gray_to_rgb(*g as f32),
        SemColor::DeviceRgb(r, g, b) => [*r as f32, *g as f32, *b as f32],
        SemColor::DeviceCmyk(c, m, y, k) => {
            pdf_color::cmyk_to_rgb(*c as f32, *m as f32, *y as f32, *k as f32)
        }
        _ => [0.0, 0.0, 0.0],
    };
    [to(rgb[0]), to(rgb[1]), to(rgb[2])]
}

fn dict_int(dict: &Dictionary, key: NameId) -> Option<i64> {
    dict.get(key).and_then(PdfObject::as_int)
}

fn dict_num(dict: &Dictionary, key: NameId) -> Option<f64> {
    dict.get(key).and_then(PdfObject::as_number)
}

fn dict_bool(dict: &Dictionary, key: NameId) -> bool {
    matches!(dict.get(key), Some(PdfObject::Boolean(true)))
}

/// Read a numeric array value (direct only — the arrays this reads, like
/// `/Coords`, `/Domain`, `/C0`, are always direct in practice).
fn read_num_array(dict: &Dictionary, key: NameId) -> Option<Vec<f64>> {
    match dict.get(key) {
        Some(PdfObject::Array(a)) => Some(a.iter().filter_map(PdfObject::as_number).collect()),
        _ => None,
    }
}

/// Group a flat numeric array into `[a, b]` pairs (drops a trailing odd item).
fn pairs_f32(v: Vec<f64>) -> Vec<[f32; 2]> {
    v.chunks_exact(2)
        .map(|c| [c[0] as f32, c[1] as f32])
        .collect()
}

/// Whether a baked 256×RGB tint ramp is (near-)constant — i.e. the tint
/// transform's output does not meaningfully vary across the whole tint range.
/// Threshold 8/255 per channel: generous enough to admit real ramps into a
/// pale spot colour, tight enough to catch flat/errored/clamped bakes.
fn lut_is_degenerate(rgb: &[u8]) -> bool {
    let mut min = [u8::MAX; 3];
    let mut max = [u8::MIN; 3];
    for px in rgb.chunks_exact(3) {
        for c in 0..3 {
            min[c] = min[c].min(px[c]);
            max[c] = max[c].max(px[c]);
        }
    }
    (0..3).all(|c| max[c] - min[c] < 8)
}

/// Read an inline image's `/DP` (`/DecodeParms`) operand into codec
/// parameters. The operand is a dict or an array of dicts parallel to the
/// filter list; keys inside the parms dict are the standard (unabbreviated)
/// names per Table 93. Mirrors `read_codec_parms` for the XObject path,
/// minus `/JBIG2Globals` (an indirect stream an inline image cannot carry).
fn inline_codec_parms(op: &Operand) -> Option<pdf_page_ir::CodecParms> {
    let dicts: Vec<&[(Vec<u8>, Operand)]> = match op {
        Operand::Dict(d) => vec![d.as_slice()],
        Operand::Array(items) => items
            .iter()
            .filter_map(|e| match e {
                Operand::Dict(d) => Some(d.as_slice()),
                _ => None,
            })
            .collect(),
        _ => return None,
    };
    if dicts.is_empty() {
        return None;
    }
    let mut out = pdf_page_ir::CodecParms {
        // The spec's CCITT defaults (ISO 32000-1 table 11).
        columns: 1728,
        end_of_block: true,
        ..Default::default()
    };
    for d in dicts {
        for (key, value) in d {
            match key.as_slice() {
                b"K" => {
                    if let Some(k) = value.as_int() {
                        out.k = k as i32;
                    }
                }
                b"Columns" => {
                    if let Some(c) = value.as_int() {
                        out.columns = c.max(0) as u32;
                    }
                }
                b"Rows" => {
                    if let Some(r) = value.as_int() {
                        out.rows = r.max(0) as u32;
                    }
                }
                b"BlackIs1" => {
                    if let Operand::Bool(v) = value {
                        out.black_is_1 = *v;
                    }
                }
                b"EncodedByteAlign" => {
                    if let Operand::Bool(v) = value {
                        out.byte_align = *v;
                    }
                }
                b"EndOfLine" => {
                    if let Operand::Bool(v) = value {
                        out.end_of_line = *v;
                    }
                }
                b"EndOfBlock" => {
                    if let Operand::Bool(v) = value {
                        out.end_of_block = *v;
                    }
                }
                _ => {}
            }
        }
    }
    Some(out)
}

/// Build a color-key `/Mask` from an inline image's array operand. Same
/// validation as the XObject path (`2 × components` integer bounds within
/// `[0, 2^bpc − 1]`); a malformed array is ignored (→ `None`).
fn inline_color_key_mask(
    op: &Operand,
    color_space: Option<&ImageColorSpace>,
    bpc: u8,
) -> Option<pdf_page_ir::ImageMask> {
    let Operand::Array(items) = op else {
        return None;
    };
    let ncomp = color_space.map(ImageColorSpace::components).unwrap_or(1);
    if items.len() != ncomp * 2 {
        return None;
    }
    let max_raw: i64 = (1i64 << bpc.min(31)) - 1;
    let mut ranges = Vec::with_capacity(ncomp);
    for pair in items.chunks_exact(2) {
        let lo = pair[0].as_int()?;
        let hi = pair[1].as_int()?;
        if lo < 0 || hi < 0 || lo > max_raw || hi > max_raw || lo > hi {
            return None;
        }
        ranges.push([lo as u32, hi as u32]);
    }
    Some(pdf_page_ir::ImageMask::ColorKey(Arc::from(ranges)))
}

/// Convert raw color components to straight RGBA by arity (the device
/// approximation used throughout — 1=gray, 3=rgb, 4=cmyk).
/// Convert color components in a resolved shading `/ColorSpace` to straight
/// RGBA — the mesh-shading counterpart of [`comps_to_rgba`], additionally
/// handling `Indexed` (palette lookup) and single-tint (`TintLut`) spaces.
fn cs_to_rgba(cs: &ImageColorSpace, comps: &[f32]) -> [f32; 4] {
    match cs {
        ImageColorSpace::Gray | ImageColorSpace::Rgb | ImageColorSpace::Cmyk => {
            comps_to_rgba(comps)
        }
        ImageColorSpace::Indexed {
            base,
            hival,
            lookup,
        } => {
            let n = base.components();
            let idx = (comps.first().copied().unwrap_or(0.0).round().max(0.0) as u32).min(*hival)
                as usize;
            let vals: Vec<f32> = (0..n)
                .map(|k| {
                    lookup
                        .get(idx * n + k)
                        .map(|b| *b as f32 / 255.0)
                        .unwrap_or(0.0)
                })
                .collect();
            // Bounded recursion: a resolved Indexed base is never itself
            // Indexed (the resolver flattens), and the tree is finite.
            cs_to_rgba(base, &vals)
        }
        ImageColorSpace::TintLut { rgb } => {
            let idx =
                (comps.first().copied().unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as usize;
            let at = |k: usize| {
                rgb.get(idx * 3 + k)
                    .map(|b| *b as f32 / 255.0)
                    .unwrap_or(0.0)
            };
            [at(0), at(1), at(2), 1.0]
        }
        ImageColorSpace::IccRgb { trc, matrix } => {
            let at = |k: usize| comps.get(k).copied().unwrap_or(0.0).clamp(0.0, 1.0);
            let rgb = pdf_color::icc::to_srgb_with(trc, matrix, [at(0), at(1), at(2)]);
            [rgb[0], rgb[1], rgb[2], 1.0]
        }
        ImageColorSpace::IccCmyk { transform } => {
            let at = |k: usize| comps.get(k).copied().unwrap_or(0.0).clamp(0.0, 1.0);
            let rgb = pdf_color::icc::cmyk_to_srgb_with(
                transform.grid as usize,
                [
                    &transform.input_tables[0],
                    &transform.input_tables[1],
                    &transform.input_tables[2],
                    &transform.input_tables[3],
                ],
                &transform.clut,
                [
                    &transform.output_tables[0],
                    &transform.output_tables[1],
                    &transform.output_tables[2],
                ],
                [at(0), at(1), at(2), at(3)],
            );
            [rgb[0], rgb[1], rgb[2], 1.0]
        }
        ImageColorSpace::TintLut2 { rgb } => {
            let ix = |k: usize| {
                (comps.get(k).copied().unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as usize
            };
            let o = (ix(0).min(255) * 256 + ix(1).min(255)) * 3;
            let at = |k: usize| rgb.get(o + k).map(|b| *b as f32 / 255.0).unwrap_or(0.0);
            [at(0), at(1), at(2), 1.0]
        }
        ImageColorSpace::Lab { white_point, range } => {
            // Mesh-shading components arrive in the space's own units, so L*
            // and the signed a*/b* pass straight through.
            let g = |k: usize| comps.get(k).copied().unwrap_or(0.0);
            let rgb = pdf_color::lab_to_rgb(g(0), g(1), g(2), *white_point, *range);
            [rgb[0], rgb[1], rgb[2], 1.0]
        }
    }
}

fn comps_to_rgba(comps: &[f32]) -> [f32; 4] {
    let c = |v: f32| v.clamp(0.0, 1.0);
    let [r, g, b] = match comps.len() {
        1 => pdf_color::gray_to_rgb(c(comps[0])),
        3 => [c(comps[0]), c(comps[1]), c(comps[2])],
        4 => pdf_color::cmyk_to_rgb(c(comps[0]), c(comps[1]), c(comps[2]), c(comps[3])),
        _ => [0.0, 0.0, 0.0],
    };
    [r, g, b, 1.0]
}

/// A type 2/3 shading `/ColorSpace`, classified for ramp conversion by
/// [`Interpreter::resolve_shading_cs`].
enum ShadingCs {
    /// Separation / DeviceN: run function outputs through the tint transform.
    Tint(Arc<TintSpace>),
    /// CIE-based (Lab/CalRGB/CalGray): convert via the CIE math.
    Cie(CieSpace),
    /// Device / ICCBased / Indexed: `cs_to_rgba` handles it by arity.
    Image(pdf_page_ir::ImageColorSpace),
}

/// Decode a Type 0 sampled function's packed samples: `count = size * n_out`
/// values of `bps` bits each, MSB-first, normalized to `[0, 1]`.
fn decode_samples(data: &[u8], size: usize, n_out: usize, bps: u32) -> Vec<f32> {
    let count = size.saturating_mul(n_out);
    let max = ((1u64 << bps.min(32)) - 1).max(1) as f32;
    let mut out = Vec::with_capacity(count);
    let mut bit = 0usize;
    for _ in 0..count {
        let mut val: u64 = 0;
        for _ in 0..bps {
            let byte = bit / 8;
            let shift = 7 - (bit % 8);
            let b = data.get(byte).map(|v| (v >> shift) & 1).unwrap_or(0);
            val = (val << 1) | b as u64;
            bit += 1;
        }
        out.push(val as f32 / max);
    }
    out
}

/// Read a 6-element numeric array as a [`Matrix`], defaulting to identity.
fn read_matrix(dict: &Dictionary, key: NameId) -> Matrix {
    match dict.get(key) {
        Some(PdfObject::Array(a)) if a.len() >= 6 => {
            let n = |i: usize| a[i].as_number();
            match (n(0), n(1), n(2), n(3), n(4), n(5)) {
                (Some(a0), Some(b), Some(c), Some(d), Some(e), Some(f)) => Matrix {
                    a: a0,
                    b,
                    c,
                    d,
                    e,
                    f,
                },
                _ => Matrix::IDENTITY,
            }
        }
        _ => Matrix::IDENTITY,
    }
}

/// Read a 4-element numeric array as `[x0, y0, x1, y1]`.
fn read_rect(dict: &Dictionary, key: NameId) -> Option<[f64; 4]> {
    match dict.get(key) {
        Some(PdfObject::Array(a)) if a.len() >= 4 => Some([
            a[0].as_number()?,
            a[1].as_number()?,
            a[2].as_number()?,
            a[3].as_number()?,
        ]),
        _ => None,
    }
}

/// Parse a CIDFont `/W` array into `(cid_start, cid_end, width)` ranges. Two
/// entry forms (ISO 32000-1 Table 117): `c [w1 … wn]` (consecutive CIDs from
/// `c`) and `c_first c_last w` (a shared-width range).
fn parse_cid_w(items: &[PdfObject]) -> Vec<(u32, u32, f32)> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let Some(c) = items[i].as_int() else { break };
        i += 1;
        let Some(next) = items.get(i) else { break };
        match next {
            PdfObject::Array(ws) => {
                for (j, w) in ws.iter().enumerate() {
                    if let Some(wn) = w.as_number() {
                        let cid = (c + j as i64).max(0) as u32;
                        ranges.push((cid, cid, wn as f32));
                    }
                }
                i += 1;
            }
            _ => {
                let c_last = next.as_int();
                let w = items.get(i + 1).and_then(PdfObject::as_number);
                i += 2;
                if let (Some(cl), Some(wn)) = (c_last, w) {
                    ranges.push((c.max(0) as u32, cl.max(0) as u32, wn as f32));
                }
            }
        }
    }
    ranges
}

/// Parse a CIDFont `/W2` array into `(cid_start, cid_end, [w1y, vx, vy])`
/// ranges (ISO 32000-1 §9.7.4.3, mirroring [`parse_cid_w`]). Two entry forms:
/// `c [w1y vx vy …]` (triples for consecutive CIDs from `c`) and
/// `c_first c_last w1y vx vy` (a shared-metrics range).
fn parse_cid_w2(items: &[PdfObject]) -> Vec<(u32, u32, [f32; 3])> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let Some(c) = items[i].as_int() else { break };
        i += 1;
        let Some(next) = items.get(i) else { break };
        match next {
            PdfObject::Array(ms) => {
                for (j, triple) in ms.chunks(3).enumerate() {
                    if triple.len() < 3 {
                        break;
                    }
                    let nums: Vec<f32> = triple
                        .iter()
                        .filter_map(|v| v.as_number().map(|n| n as f32))
                        .collect();
                    if nums.len() == 3 {
                        let cid = (c + j as i64).max(0) as u32;
                        ranges.push((cid, cid, [nums[0], nums[1], nums[2]]));
                    }
                }
                i += 1;
            }
            _ => {
                let c_last = next.as_int();
                let w1y = items.get(i + 1).and_then(PdfObject::as_number);
                let vx = items.get(i + 2).and_then(PdfObject::as_number);
                let vy = items.get(i + 3).and_then(PdfObject::as_number);
                i += 4;
                if let (Some(cl), Some(w), Some(x), Some(y)) = (c_last, w1y, vx, vy) {
                    ranges.push((
                        c.max(0) as u32,
                        cl.max(0) as u32,
                        [w as f32, x as f32, y as f32],
                    ));
                }
            }
        }
    }
    ranges
}

/// Axis-aligned bounds of a `/BBox` rectangle mapped through `ctm`. All four
/// corners are transformed (the CTM may rotate/shear), then their extent is
/// taken.
fn group_bounds(bbox: Option<[f64; 4]>, ctm: Matrix) -> Rect {
    let Some([x0, y0, x1, y1]) = bbox else {
        return Rect::default();
    };
    let corners = [
        ctm.apply(Point { x: x0, y: y0 }),
        ctm.apply(Point { x: x1, y: y0 }),
        ctm.apply(Point { x: x1, y: y1 }),
        ctm.apply(Point { x: x0, y: y1 }),
    ];
    let mut r = Rect {
        x0: corners[0].x,
        y0: corners[0].y,
        x1: corners[0].x,
        y1: corners[0].y,
    };
    for p in &corners[1..] {
        r.x0 = r.x0.min(p.x);
        r.y0 = r.y0.min(p.y);
        r.x1 = r.x1.max(p.x);
        r.y1 = r.y1.max(p.y);
    }
    r
}

/// The §12.5.5 appearance-fitting matrix `A`: the form's `/BBox` corners are
/// mapped through its `/Matrix`, their axis-aligned extent is taken, and `A`
/// is the translate+scale carrying that extent onto the annotation's `/Rect`.
/// `None` when either extent is degenerate (nothing sensible to paint —
/// PDFium's `CFX_Matrix::MatchRect` on an empty rect degenerates too).
fn appearance_matrix(bbox: Option<[f64; 4]>, matrix: Matrix, rect: [f64; 4]) -> Option<Matrix> {
    let [rx0, ry0, rx1, ry1] = rect;
    let (rw, rh) = (rx1 - rx0, ry1 - ry0);
    if !(rw > 0.0 && rh > 0.0 && rw.is_finite() && rh.is_finite()) {
        return None;
    }
    let [bx0, by0, bx1, by1] = bbox?;
    let corners = [
        matrix.apply(Point { x: bx0, y: by0 }),
        matrix.apply(Point { x: bx1, y: by0 }),
        matrix.apply(Point { x: bx1, y: by1 }),
        matrix.apply(Point { x: bx0, y: by1 }),
    ];
    let (mut tx0, mut ty0, mut tx1, mut ty1) =
        (corners[0].x, corners[0].y, corners[0].x, corners[0].y);
    for p in &corners[1..] {
        tx0 = tx0.min(p.x);
        ty0 = ty0.min(p.y);
        tx1 = tx1.max(p.x);
        ty1 = ty1.max(p.y);
    }
    let (tw, th) = (tx1 - tx0, ty1 - ty0);
    if !(tw > 0.0 && th > 0.0 && tw.is_finite() && th.is_finite()) {
        return None;
    }
    let sx = rw / tw;
    let sy = rh / th;
    Some(Matrix {
        a: sx,
        b: 0.0,
        c: 0.0,
        d: sy,
        e: rx0 - tx0 * sx,
        f: ry0 - ty0 * sy,
    })
}

/// A closed rectangle path (`re` geometry): MoveTo, 3×LineTo, Close.
fn rect_path(x: f64, y: f64, w: f64, h: f64) -> PathData {
    let verbs = [
        PathVerb::MoveTo,
        PathVerb::LineTo,
        PathVerb::LineTo,
        PathVerb::LineTo,
        PathVerb::Close,
    ];
    let points = [
        Point { x, y },
        Point { x: x + w, y },
        Point { x: x + w, y: y + h },
        Point { x, y: y + h },
    ];
    PathData {
        verbs: verbs.into(),
        points: points.into(),
    }
}

/// Map a `/BM` blend-mode name to the IR blend mode; unknown names fall back
/// to Normal (ISO 32000-1 §11.3.5).
fn map_blend_mode(name: &[u8]) -> BlendMode {
    match name {
        b"Multiply" => BlendMode::Multiply,
        b"Screen" => BlendMode::Screen,
        b"Overlay" => BlendMode::Overlay,
        b"Darken" => BlendMode::Darken,
        b"Lighten" => BlendMode::Lighten,
        b"ColorDodge" => BlendMode::ColorDodge,
        b"ColorBurn" => BlendMode::ColorBurn,
        b"HardLight" => BlendMode::HardLight,
        b"SoftLight" => BlendMode::SoftLight,
        b"Difference" => BlendMode::Difference,
        b"Exclusion" => BlendMode::Exclusion,
        b"Hue" => BlendMode::Hue,
        b"Saturation" => BlendMode::Saturation,
        b"Color" => BlendMode::Color,
        b"Luminosity" => BlendMode::Luminosity,
        // "Normal", "Compatible", or anything unknown.
        _ => BlendMode::Normal,
    }
}

fn map_line_cap(v: f64) -> LineCap {
    match v as i64 {
        1 => LineCap::Round,
        2 => LineCap::Square,
        _ => LineCap::Butt,
    }
}

fn map_line_join(v: f64) -> LineJoin {
    match v as i64 {
        1 => LineJoin::Round,
        2 => LineJoin::Bevel,
        _ => LineJoin::Miter,
    }
}

fn initial_color(space: &ColorSpace) -> SemColor {
    match space {
        ColorSpace::DeviceGray => SemColor::DeviceGray(0.0),
        ColorSpace::DeviceRgb => SemColor::DeviceRgb(0.0, 0.0, 0.0),
        ColorSpace::DeviceCmyk => SemColor::DeviceCmyk(0.0, 0.0, 0.0, 1.0),
        ColorSpace::Named(id) => SemColor::Components {
            space: *id,
            values: Vec::new(),
        },
        ColorSpace::Pattern(_) => SemColor::DeviceGray(0.0),
    }
}

/// True for image-codec filters (DCT/JPX/JBIG2/CCITT) whose bytes this build
/// does not decode; such images carry no samples and are routed away by the
/// page's `NEEDS_*` feature.
/// Advance widths for a substituted simple font that supplies no `/Widths`.
///
/// ISO 32000-1 §9.6.2.1: such a font uses the standard-14 metrics. The
/// bundled faces are metric-compatible with those fonts, so reading each
/// code's glyph advance out of the face *is* the standard metric — no
/// separate AFM table needed. Advances are in 1/1000 text-space units, which
/// is the faces' own units-per-em.
fn standard_widths(prog: &pdf_font::FontProgram, gids: &[u32; 256]) -> pdf_font::SimpleWidths {
    let scale = 1000.0 / prog.units_per_em().max(1) as f32;
    let widths: Vec<f32> = (0..256)
        .map(|code| prog.advance(gids[code]).map(|w| w * scale).unwrap_or(0.0))
        .collect();
    pdf_font::SimpleWidths::new(0, widths, 0.0)
}

/// Map a canonical codec filter name to its IR kind. `None` means the filter
/// is a general (non-codec) one — the stream-decode layer handled it.
fn codec_kind(name: &[u8]) -> Option<pdf_page_ir::ImageCodecKind> {
    use pdf_page_ir::ImageCodecKind as K;
    match name {
        b"DCTDecode" => Some(K::Dct),
        b"JPXDecode" => Some(K::Jpx),
        b"JBIG2Decode" => Some(K::Jbig2),
        b"CCITTFaxDecode" => Some(K::CcittFax),
        _ => None,
    }
}

fn collect_filters(value: &Operand) -> Vec<Vec<u8>> {
    match value {
        Operand::Name(n) => vec![canonical_filter(n)],
        Operand::Array(a) => a
            .iter()
            .filter_map(Operand::as_name)
            .map(canonical_filter)
            .collect(),
        _ => Vec::new(),
    }
}

/// Expand the inline-image filter abbreviations to their canonical names.
fn canonical_filter(name: &[u8]) -> Vec<u8> {
    match name {
        b"AHx" => b"ASCIIHexDecode".to_vec(),
        b"A85" => b"ASCII85Decode".to_vec(),
        b"LZW" => b"LZWDecode".to_vec(),
        b"Fl" => b"FlateDecode".to_vec(),
        b"RL" => b"RunLengthDecode".to_vec(),
        b"CCF" => b"CCITTFaxDecode".to_vec(),
        b"DCT" => b"DCTDecode".to_vec(),
        other => other.to_vec(),
    }
}

/// Gather a page's content bytes: a single content stream, or an array of
/// streams concatenated with a separating newline (ISO 32000-1 §7.8.2 — the
/// streams form one logical stream but a token must not span the boundary).
pub(crate) fn gather_content(
    snapshot: &DocumentSnapshot,
    contents: Option<&PdfObject>,
    ctx: &mut ParseContext,
) -> Result<Vec<u8>, ContentError> {
    let Some(contents) = contents else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    append_content(snapshot, contents, ctx, &mut out, 0)?;
    Ok(out)
}

fn append_content(
    snapshot: &DocumentSnapshot,
    obj: &PdfObject,
    ctx: &mut ParseContext,
    out: &mut Vec<u8>,
    depth: usize,
) -> Result<(), ContentError> {
    if depth > 8 {
        return Ok(());
    }
    match obj {
        PdfObject::Reference(id) => {
            // A content object that fails to resolve is skipped, not fatal:
            // the page renders whatever other content streams it has.
            match snapshot.objects().resolve(snapshot, *id, ctx) {
                Ok(resolved) => append_content(snapshot, &resolved, ctx, out, depth + 1)?,
                Err(e) => ctx.note_recovery(format!("page content object {id} unresolved: {e}")),
            }
        }
        PdfObject::Array(items) => {
            for item in items.iter() {
                append_content(snapshot, item, ctx, out, depth + 1)?;
            }
        }
        PdfObject::Stream(stream) => {
            // A truncated or corrupt content stream is skipped so the page
            // renders what it can — blank if this was its only stream — rather
            // than dropping the whole page. Viewers (Poppler/PDFium) do the
            // same; a real book can carry deliberately-empty/malformed pages.
            match snapshot.decode_stream_data(stream, ctx) {
                Ok(bytes) => {
                    if !out.is_empty() {
                        out.push(b'\n');
                    }
                    out.extend_from_slice(&bytes);
                }
                Err(e) => ctx.note_recovery(format!("content stream decode failed, skipped: {e}")),
            }
        }
        _ => {}
    }
    Ok(())
}

/// Resolve a page's `/Resources` (possibly inherited, possibly indirect) into
/// a shared dictionary object.
pub(crate) fn resolve_resources(
    snapshot: &DocumentSnapshot,
    resources: Option<&PdfObject>,
    ctx: &mut ParseContext,
) -> Option<Arc<PdfObject>> {
    match resources? {
        PdfObject::Reference(id) => snapshot.objects().resolve(snapshot, *id, ctx).ok(),
        direct => Some(Arc::new(direct.clone())),
    }
}

/// A resolved `/Separation` or `/DeviceN` space: how many tints it takes and
/// how they become a device colour.
#[derive(Debug)]
struct TintSpace {
    /// Colorant count: 1 for Separation, N for DeviceN.
    inputs: usize,
    /// Component count of the alternate space the transform outputs into.
    alt_arity: usize,
    /// The tint transform (any function type, single- or multi-input). `None`
    /// only when it would not build at all (malformed / unbuildable) — the
    /// caller then approximates subtractively.
    func: Option<pdf_function::Function>,
    /// The alternate space when it is CIE-based (Lab/CalRGB/CalGray): the
    /// transform's outputs are components of *that* space — e.g. a PANTONE
    /// `/Separation` whose alternate is Lab outputs L*a*b* on a 0..100 /
    /// −128..127 scale, which clamped-as-DeviceRGB renders wildly wrong
    /// (DS82's lavender band read as pure yellow). Mirrors PDFium's
    /// `CPDF_SeparationCS::GetRGB` → `base_cs_->GetRGB(results)`.
    alt_cie: Option<CieSpace>,
    /// The alternate space when it is an `/ICCBased` **CMYK** press profile: the
    /// transform's four outputs are device CMYK, but PDFium runs them through
    /// the profile via LittleCMS rather than its frozen table (a PANTONE spot
    /// whose alternate is the document's `/DefaultCMYK` press profile — the
    /// Huser cover). Mirrors `base_cs_->GetRGB` where the base is ICCBased.
    alt_icc_cmyk: Option<Arc<pdf_color::icc::IccCmyk>>,
    /// The `/None` colorant, which marks nothing.
    colorant_none: bool,
    /// The `/All` colorant (ISO 32000-1 §8.6.6.4): paints on *every*
    /// separation, so a composite preview renders it as neutral ink —
    /// tint 1.0 is black — bypassing the tint transform.
    colorant_all: bool,
}

/// A CIE-based colour space's conversion parameters (ISO 32000-1 §8.6.5),
/// read once from its resource entry and cached. Conversion is pure math in
/// `pdf-color` (ported from PDFium's `cpdf_colorspace.cpp`).
#[derive(Debug, Clone, Copy)]
enum CieSpace {
    Lab {
        white_point: [f32; 3],
        range: [f32; 4],
    },
    CalRgb {
        white_point: [f32; 3],
        gamma: [f32; 3],
        matrix: [f32; 9],
    },
    CalGray {
        gamma: f32,
    },
}

/// Convert `sc`/`scn` operands through a CIE space into a device colour.
/// Operands arrive unclamped (Lab's L* ∈ [0,100], a*/b* ∈ /Range — the
/// operand path deliberately does not clamp to [0,1]); missing operands
/// default to 0.0, the spec's initial colour for these spaces.
fn eval_cie(space: CieSpace, values: &[f64]) -> SemColor {
    let v = |i: usize| values.get(i).copied().unwrap_or(0.0) as f32;
    match space {
        CieSpace::Lab { white_point, range } => {
            let [r, g, b] = pdf_color::lab_to_rgb(v(0), v(1), v(2), white_point, range);
            SemColor::DeviceRgb(r as f64, g as f64, b as f64)
        }
        CieSpace::CalRgb {
            white_point,
            gamma,
            matrix,
        } => {
            let [r, g, b] =
                pdf_color::calrgb_to_rgb([v(0), v(1), v(2)], white_point, gamma, matrix);
            SemColor::DeviceRgb(r as f64, g as f64, b as f64)
        }
        CieSpace::CalGray { gamma } => {
            SemColor::DeviceGray(pdf_color::calgray_to_gray(v(0), gamma) as f64)
        }
    }
}

/// A parsed `/Type3` font: everything needed to draw its glyphs, which are
/// content streams rather than outlines (ISO 32000-1 §9.6.5).
#[derive(Debug)]
struct Type3Font {
    /// Glyph space → text space (typically `[0.001 0 0 0.001 0 0]`).
    font_matrix: Matrix,
    /// Resources for CharProc execution (the font's `/Resources`, else the
    /// resources active where the font is used).
    resources: Option<Arc<PdfObject>>,
    /// Per character-code CharProc content stream, resolved through
    /// `/Encoding` + `/Differences`; `None` where the code names no glyph.
    char_procs: [Option<Arc<PdfStream>>; 256],
    /// Per character-code advance width in **glyph space** (`/Widths`); 0 where
    /// absent. Transform through `font_matrix` for the text-space advance.
    widths: [f64; 256],
    /// Whether the font supplied a `/Widths` array at all; when it did not, the
    /// CharProc's `d0`/`d1` `wx` is used as the advance instead.
    has_widths: bool,
}

/// A font resolved by substitution: the program to draw with, plus which
/// bundled face the choice corresponds to (the caller needs that to know
/// whether a symbolic built-in encoding applies).
struct Substituted {
    bytes: Arc<[u8]>,
    program: pdf_font::FontProgram,
    face: pdf_font::StandardFont,
    /// The *requested* style — may exceed what `face` offers (the symbolic
    /// faces have no bold/italic cuts); `pdf_font::synthesis` derives the
    /// shear/embolden to fake the difference.
    want_bold: bool,
    want_italic: bool,
    /// Face index within a collection (.ttc); 0 for a plain font.
    face_index: u32,
    /// Resolved through the injected system-font provider (as opposed to a
    /// bundled standard face). Only a system face can cover CJK, so only
    /// then is the CID→Unicode→glyph bridge attempted — keeping the
    /// provider-off default byte-for-byte deterministic.
    system: bool,
}
