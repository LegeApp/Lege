//! Lowering a [`SemanticPage`] to the backend-neutral
//! [`pdf_page_ir::CompiledPage`] (roadmap §7 Phase 3).
//!
//! This is where PDF's *implicit* graphics state becomes *explicit* display
//! operations: the semantic layer's `SetFillColor`/`SetFillAlpha`/… state
//! mutations disappear, and their effect is baked directly onto each paint
//! operation (`FillPath { paint, alpha, blend, … }`). Resources are interned
//! into per-page tables and referenced by typed handle. The clip stack becomes
//! explicit `PushClip`/`PopClip` pairs bracketed to the `q`/`Q` scopes.
//!
//! Painter order is preserved exactly; nothing is reordered.

use std::collections::HashMap;
use std::sync::Arc;

use pdf_page_ir::{
    BlendMode, Color, CompiledPage, DisplayOp, FontId, FontResource, GlyphRun, ImageId, ImageIr,
    InterpolationMode, LineCap, LineJoin, Matrix, PageComplexity, PageFeatures, Paint, PaintId,
    PathData, PathId, PlacedGlyph, Point, Rect, ResourceKey, ShadingId, ShadingKind,
    ShadingResource, StrokeStyle, StrokeStyleId, TilingId, TilingPattern, TransparencyGroup,
    TransparencyGroupId,
};

use crate::semantic::{
    SemColor, SemShading, SemShadingKind, SemTiling, SemanticOp, SemanticPage, TextElement, TextRun,
};

/// Lowering-time graphics state — the subset the IR bakes onto paint ops.
/// Cloned on `Save`, restored on `Restore`, exactly mirroring the semantic
/// `q`/`Q` scopes so state established inside a scope does not leak past it.
#[derive(Debug, Clone)]
struct LowerGs {
    ctm: Matrix,
    fill_color: SemColor,
    stroke_color: SemColor,
    fill_alpha: f32,
    stroke_alpha: f32,
    blend: BlendMode,
    line_width: f64,
    line_cap: LineCap,
    line_join: LineJoin,
    miter_limit: f64,
    dash_pattern: Vec<f64>,
    dash_phase: f64,
}

impl Default for LowerGs {
    fn default() -> Self {
        Self {
            ctm: Matrix::IDENTITY,
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
        }
    }
}

/// Dedup key for interning [`Paint`]s. Floats are keyed by bit pattern so the
/// map is exact and deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PaintKey {
    Solid([u32; 4]),
    /// Tiling-pattern handle (the pattern `/Matrix` is fixed per handle).
    Pattern(u32),
    /// Shading-pattern handle.
    Shading(u32),
}

pub(crate) struct Lowerer<'a> {
    page: &'a SemanticPage,
    ops: Vec<DisplayOp>,

    paths: Vec<PathData>,
    path_map: HashMap<Vec<u8>, PathId>,
    paints: Vec<Paint>,
    paint_map: HashMap<PaintKey, PaintId>,
    stroke_styles: Vec<StrokeStyle>,
    stroke_map: HashMap<Vec<u8>, StrokeStyleId>,
    glyph_runs: Vec<GlyphRun>,
    fonts: Vec<FontResource>,
    /// Maps a semantic font handle to the compiled font table index.
    font_map: HashMap<u32, FontId>,
    images: Vec<ImageIr>,
    groups: Vec<TransparencyGroup>,

    gs: LowerGs,
    gs_stack: Vec<LowerGs>,
    /// Number of clips pushed and not yet popped.
    clip_count: u32,
    /// Clip count captured at each `Save`, to unwind on the matching `Restore`.
    clip_scope: Vec<u32>,
    /// Conservative page-space union of visible paint operations. Soft-mask
    /// construction is deliberately excluded: it modulates later paint but
    /// does not itself put marks on the page.
    content_bounds: Option<Rect>,
    soft_mask_depth: u32,
}

impl<'a> Lowerer<'a> {
    pub(crate) fn new(page: &'a SemanticPage) -> Self {
        Self {
            page,
            ops: Vec::with_capacity(page.ops.len()),
            paths: Vec::new(),
            path_map: HashMap::new(),
            paints: Vec::new(),
            paint_map: HashMap::new(),
            stroke_styles: Vec::new(),
            stroke_map: HashMap::new(),
            glyph_runs: Vec::new(),
            fonts: Vec::new(),
            font_map: HashMap::new(),
            images: Vec::new(),
            groups: Vec::new(),
            gs: LowerGs::default(),
            gs_stack: Vec::new(),
            clip_count: 0,
            clip_scope: Vec::new(),
            content_bounds: None,
            soft_mask_depth: 0,
        }
    }

    pub(crate) fn lower(mut self) -> CompiledPage {
        // Clone the op handle first so `self` is free to be mutated while we
        // walk it (the ops borrow the page's Arc, not `self`).
        let ops = self.page.ops.clone();
        for op in ops.iter() {
            self.lower_op(op);
        }

        // Shadings and tilings map 1:1 (index-preserving) from the semantic
        // tables, so the handles carried in resolved paints stay valid.
        let shadings: Vec<ShadingResource> =
            self.page.shadings.iter().map(convert_shading).collect();
        let tilings: Vec<TilingPattern> = self.page.tilings.iter().map(convert_tiling).collect();

        let features = self.compute_features(&shadings, &tilings);
        let complexity = self.compute_complexity();

        CompiledPage {
            schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
            bounds: self.page.bounds,
            content_bounds: self
                .content_bounds
                .and_then(|bounds| bounds.intersect(self.page.bounds.crop.normalized())),
            operations: self.ops.into(),
            paths: self.paths.into(),
            paints: self.paints.into(),
            stroke_styles: self.stroke_styles.into(),
            glyph_runs: self.glyph_runs.into(),
            fonts: self.fonts.into(),
            images: self.images.into(),
            // Soft masks are represented in the IR (DisplayOp::ApplySoftMask +
            // MaskResource) but populated in Phase 10.
            masks: Vec::new().into(),
            groups: self.groups.into(),
            shadings: shadings.into(),
            tilings: tilings.into(),
            features,
            complexity,
        }
    }

    fn lower_op(&mut self, op: &SemanticOp) {
        match op {
            SemanticOp::Save => {
                self.ops.push(DisplayOp::Save);
                self.gs_stack.push(self.gs.clone());
                self.clip_scope.push(self.clip_count);
            }
            SemanticOp::Restore => {
                let target = self.clip_scope.pop().unwrap_or(0);
                while self.clip_count > target {
                    self.ops.push(DisplayOp::PopClip);
                    self.clip_count -= 1;
                }
                self.ops.push(DisplayOp::Restore);
                self.gs = self.gs_stack.pop().unwrap_or_default();
            }
            SemanticOp::Concat(m) => {
                self.gs.ctm = m.then(self.gs.ctm);
                self.ops.push(DisplayOp::ConcatTransform(*m));
            }

            // State changes: absorbed into the paint ops that follow.
            SemanticOp::SetLineWidth(w) => self.gs.line_width = *w,
            SemanticOp::SetLineCap(c) => self.gs.line_cap = *c,
            SemanticOp::SetLineJoin(j) => self.gs.line_join = *j,
            SemanticOp::SetMiterLimit(m) => self.gs.miter_limit = *m,
            SemanticOp::SetDash { pattern, phase } => {
                self.gs.dash_pattern = pattern.clone();
                self.gs.dash_phase = *phase;
            }
            SemanticOp::SetFillColor(c) => self.gs.fill_color = c.clone(),
            SemanticOp::SetStrokeColor(c) => self.gs.stroke_color = c.clone(),
            SemanticOp::SetFillAlpha(a) => self.gs.fill_alpha = *a,
            SemanticOp::SetStrokeAlpha(a) => self.gs.stroke_alpha = *a,
            SemanticOp::SetBlendMode(m) => self.gs.blend = *m,

            SemanticOp::Fill { path, rule } => {
                self.include_path(path.index(), 0.0);
                let path = self.intern_path(path.index());
                let paint = self.intern_paint(&self.gs.fill_color.clone());
                self.ops.push(DisplayOp::FillPath {
                    path,
                    paint,
                    rule: *rule,
                    alpha: self.gs.fill_alpha,
                    blend: self.gs.blend,
                });
            }
            SemanticOp::Stroke { path } => {
                self.include_path(path.index(), self.gs.line_width * 0.5);
                let path = self.intern_path(path.index());
                let paint = self.intern_paint(&self.gs.stroke_color.clone());
                let style = self.intern_stroke();
                self.ops.push(DisplayOp::StrokePath {
                    path,
                    paint,
                    style,
                    alpha: self.gs.stroke_alpha,
                    blend: self.gs.blend,
                });
            }
            SemanticOp::FillStroke { path, rule } => {
                self.include_path(path.index(), self.gs.line_width * 0.5);
                let path = self.intern_path(path.index());
                let fill_paint = self.intern_paint(&self.gs.fill_color.clone());
                self.ops.push(DisplayOp::FillPath {
                    path,
                    paint: fill_paint,
                    rule: *rule,
                    alpha: self.gs.fill_alpha,
                    blend: self.gs.blend,
                });
                let stroke_paint = self.intern_paint(&self.gs.stroke_color.clone());
                let style = self.intern_stroke();
                self.ops.push(DisplayOp::StrokePath {
                    path,
                    paint: stroke_paint,
                    style,
                    alpha: self.gs.stroke_alpha,
                    blend: self.gs.blend,
                });
            }
            SemanticOp::Clip { path, rule } => {
                let path = self.intern_path(path.index());
                self.ops.push(DisplayOp::PushClip { path, rule: *rule });
                self.clip_count += 1;
            }
            SemanticOp::ClipText { runs } => {
                // Intern each clip-contributing run to a glyph run; the backend
                // unions their outlines into one text clip. Balanced by the
                // `PopClip` the enclosing `Restore` emits, like a path clip.
                let run_ids: Vec<pdf_page_ir::GlyphRunId> = runs
                    .iter()
                    .map(|r| self.intern_glyph_run(r.index()))
                    .collect();
                self.ops.push(DisplayOp::PushClipText {
                    runs: run_ids.into(),
                });
                self.clip_count += 1;
            }

            SemanticOp::ShowText(run) => {
                let semantic_run = &self.page.text_runs[run.index()];
                if !semantic_run.visible
                    || self.page.fonts[semantic_run.font.index()].subtype == b"Type3"
                {
                    return;
                }
                let mode = semantic_run.render_mode;
                let run_id = self.intern_glyph_run(run.index());
                self.include_glyph_run(run_id);
                let paint = self.intern_paint(&self.gs.fill_color.clone());
                // Stroking render modes (Tr 1/2/5/6) also stroke the glyph
                // outlines with the current stroke colour, alpha, and line
                // style (§9.3.1). The backend selects fill vs stroke from the
                // run's render_mode; the fill paint above is harmless when the
                // mode does not fill.
                let stroke = if matches!(mode, 1 | 2 | 5 | 6) {
                    Some(pdf_page_ir::GlyphStroke {
                        paint: self.intern_paint(&self.gs.stroke_color.clone()),
                        style: self.intern_stroke(),
                        alpha: self.gs.stroke_alpha,
                    })
                } else {
                    None
                };
                self.ops.push(DisplayOp::DrawGlyphRun {
                    run: run_id,
                    paint,
                    alpha: self.gs.fill_alpha,
                    blend: self.gs.blend,
                    stroke,
                });
            }
            SemanticOp::DrawImage(image) => {
                self.include_unit_square();
                let image_id = self.intern_image(image.index());
                // The current fill paint colors an /ImageMask stencil.
                let paint = self.intern_paint(&self.gs.fill_color.clone());
                // The image occupies the unit square in current user space; the
                // CTM is carried by the surrounding ConcatTransform scope, so
                // the op's own transform is identity (consistent with paths).
                self.ops.push(DisplayOp::DrawImage {
                    image: image_id,
                    paint,
                    transform: Matrix::IDENTITY,
                    alpha: self.gs.fill_alpha,
                    blend: self.gs.blend,
                });
            }
            SemanticOp::BeginGroup {
                isolated,
                knockout,
                bounds,
                opacity,
                blend,
            } => {
                let id = TransparencyGroupId(self.groups.len() as u32);
                self.groups.push(TransparencyGroup {
                    isolated: *isolated,
                    knockout: *knockout,
                    bounds: *bounds,
                    opacity: *opacity,
                    blend: *blend,
                });
                self.ops
                    .push(DisplayOp::BeginTransparencyGroup { group: id });
                // ISO 32000-1 §11.6.6: inside a transparency group the
                // constant alpha and blend mode reset to their defaults — the
                // outer values become the GROUP's composite parameters
                // (carried on `TransparencyGroup` above), and must not also
                // apply to the ops inside, or a 0.5-alpha group draws its
                // content at 0.25. The interpreter resets its own state after
                // emitting BeginGroup but emits no op for it, so this replayed
                // state must reset here too. The group is always bracketed by
                // Save/Restore, which restores the outer values on exit.
                self.gs.fill_alpha = 1.0;
                self.gs.stroke_alpha = 1.0;
                self.gs.blend = BlendMode::Normal;
            }
            SemanticOp::EndGroup => self.ops.push(DisplayOp::EndTransparencyGroup),
            SemanticOp::BeginPaintOrigin(o) => self.ops.push(DisplayOp::BeginPaintOrigin(*o)),
            SemanticOp::EndPaintOrigin => self.ops.push(DisplayOp::EndPaintOrigin),
            SemanticOp::BeginSoftMask { kind, transfer } => {
                self.soft_mask_depth += 1;
                self.ops.push(DisplayOp::BeginSoftMask {
                    kind: *kind,
                    transfer: transfer.clone(),
                });
            }
            SemanticOp::EndSoftMask => {
                self.ops.push(DisplayOp::EndSoftMask);
                self.soft_mask_depth = self.soft_mask_depth.saturating_sub(1);
            }
            SemanticOp::ClearSoftMask => self.ops.push(DisplayOp::ClearSoftMask),
            SemanticOp::PaintShading(id) => {
                // A shading without a finite BBox is clipped by the page.
                // Using the page box remains conservative and avoids decoding
                // any image/font resource merely to determine an extent.
                self.include_rect(self.page.bounds.crop);
                // `sh` paints in the current user space; the CTM is carried by
                // the enclosing ConcatTransform scope, so the op transform is
                // identity (consistent with DrawImage).
                self.ops.push(DisplayOp::DrawShading {
                    shading: ShadingId(id.0),
                    transform: Matrix::IDENTITY,
                });
            }
        }
    }

    fn include_rect(&mut self, rect: Rect) {
        if self.soft_mask_depth != 0 {
            return;
        }
        let rect = rect.normalized();
        if ![rect.x0, rect.y0, rect.x1, rect.y1]
            .iter()
            .all(|value| value.is_finite())
            || rect.width() <= 0.0
            || rect.height() <= 0.0
        {
            return;
        }
        self.content_bounds = Some(match self.content_bounds {
            Some(bounds) => bounds.union(rect),
            None => rect,
        });
    }

    fn include_transformed_rect(&mut self, rect: Rect, transform: Matrix) {
        let corners = [
            Point {
                x: rect.x0,
                y: rect.y0,
            },
            Point {
                x: rect.x1,
                y: rect.y0,
            },
            Point {
                x: rect.x1,
                y: rect.y1,
            },
            Point {
                x: rect.x0,
                y: rect.y1,
            },
        ];
        let points = corners.map(|point| transform.apply(point));
        let rect = Rect {
            x0: points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min),
            y0: points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min),
            x1: points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max),
            y1: points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max),
        };
        self.include_rect(rect);
    }

    fn include_path(&mut self, semantic_index: usize, stroke_half_width: f64) {
        if self.soft_mask_depth != 0 {
            return;
        }
        let path = &self.page.paths[semantic_index];
        if path.points.is_empty() {
            return;
        }
        let points: Vec<Point> = path
            .points
            .iter()
            .map(|point| self.gs.ctm.apply(*point))
            .collect();
        let scale = self
            .gs
            .ctm
            .a
            .hypot(self.gs.ctm.b)
            .max(self.gs.ctm.c.hypot(self.gs.ctm.d));
        let pad = stroke_half_width.abs() * scale;
        self.include_rect(Rect {
            x0: points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min) - pad,
            y0: points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min) - pad,
            x1: points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max) + pad,
            y1: points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max) + pad,
        });
    }

    fn include_unit_square(&mut self) {
        self.include_transformed_rect(
            Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            self.gs.ctm,
        );
    }

    fn include_glyph_run(&mut self, run_id: pdf_page_ir::GlyphRunId) {
        if self.soft_mask_depth != 0 {
            return;
        }
        let run = &self.glyph_runs[run_id.index()];
        let transform = run.transform.then(self.gs.ctm);
        let size = run.font_size.abs().max(0.01);
        let boxes: Vec<Rect> = run
            .glyphs
            .iter()
            .map(|glyph| Rect {
                x0: glyph.x,
                y0: glyph.y - size * 0.25,
                x1: glyph.x + size,
                y1: glyph.y + size,
            })
            .collect();
        for bounds in boxes {
            self.include_transformed_rect(bounds, transform);
        }
    }

    fn intern_path(&mut self, semantic_index: usize) -> PathId {
        let data = &self.page.paths[semantic_index];
        let key = path_key(data);
        if let Some(id) = self.path_map.get(&key) {
            return *id;
        }
        let id = PathId(self.paths.len() as u32);
        self.paths.push(data.clone());
        self.path_map.insert(key, id);
        id
    }

    fn intern_paint(&mut self, color: &SemColor) -> PaintId {
        let paint = resolve_paint(color);
        let key = paint_key(&paint);
        if let Some(id) = self.paint_map.get(&key) {
            return *id;
        }
        let id = PaintId(self.paints.len() as u32);
        self.paints.push(paint);
        self.paint_map.insert(key, id);
        id
    }

    fn intern_stroke(&mut self) -> StrokeStyleId {
        let style = StrokeStyle {
            width: self.gs.line_width,
            cap: self.gs.line_cap,
            join: self.gs.line_join,
            miter_limit: self.gs.miter_limit,
            dash_pattern: self.gs.dash_pattern.clone().into(),
            dash_phase: self.gs.dash_phase,
        };
        let key = stroke_key(&style);
        if let Some(id) = self.stroke_map.get(&key) {
            return *id;
        }
        let id = StrokeStyleId(self.stroke_styles.len() as u32);
        self.stroke_styles.push(style);
        self.stroke_map.insert(key, id);
        id
    }

    fn intern_font(&mut self, semantic_index: usize) -> FontId {
        if let Some(id) = self.font_map.get(&(semantic_index as u32)) {
            return *id;
        }
        let sem = &self.page.fonts[semantic_index];
        let key = match sem.object {
            Some(obj) => ResourceKey {
                object_number: obj.number,
                generation: obj.generation,
                variant: 0,
            },
            None => ResourceKey {
                object_number: 0,
                generation: 0,
                variant: semantic_index as u32,
            },
        };
        let id = FontId(self.fonts.len() as u32);
        // Carry the embedded outline program (empty → the backend falls back to
        // placement boxes).
        let program: Arc<[u8]> = match &sem.program {
            Some(bytes) => bytes.clone(),
            None => Vec::new().into(),
        };
        self.fonts.push(FontResource {
            key,
            program,
            face_index: sem.face_index,
            synthetic_shear: sem.synthesis.oblique_shear,
            // PDFium's weight-700 embolden level: kWeightPow[(700-400)/10]
            // = 70 font units per 1000 upem (cfx_face.cpp).
            synthetic_embolden_em: if sem.synthesis.embolden { 0.07 } else { 0.0 },
        });
        self.font_map.insert(semantic_index as u32, id);
        id
    }

    fn intern_glyph_run(&mut self, semantic_index: usize) -> pdf_page_ir::GlyphRunId {
        let run = self.page.text_runs[semantic_index].clone();
        let sem = &self.page.fonts[run.font.index()];
        let metrics = sem.metrics.clone();
        let glyph_map = sem.glyph_map.clone();
        let font = self.intern_font(run.font.index());
        let glyphs = placed_glyphs(&run, &metrics, &glyph_map);
        let compiled = GlyphRun {
            font,
            font_size: run.font_size,
            transform: run.text_matrix,
            glyphs: glyphs.into(),
            render_mode: run.render_mode,
        };
        let id = pdf_page_ir::GlyphRunId(self.glyph_runs.len() as u32);
        self.glyph_runs.push(compiled);
        id
    }

    fn intern_image(&mut self, semantic_index: usize) -> ImageId {
        let sem = &self.page.images[semantic_index];
        let key = match sem.object {
            Some(obj) => ResourceKey {
                object_number: obj.number,
                generation: obj.generation,
                variant: 0,
            },
            // Inline images have no object identity; key them by position so
            // two distinct inline images never collide in a cache.
            None => ResourceKey {
                object_number: 0,
                generation: 0,
                variant: semantic_index as u32,
            },
        };
        let ir = ImageIr {
            key,
            width: sem.width,
            height: sem.height,
            is_stencil: sem.is_mask,
            interpolation: if sem.interpolate {
                InterpolationMode::Bilinear
            } else {
                InterpolationMode::Nearest
            },
            soft_mask: None,
            bits_per_component: sem.bits_per_component,
            color_space: sem
                .color_space
                .clone()
                .unwrap_or(pdf_page_ir::ImageColorSpace::Gray),
            decode: sem.decode.clone().map(Arc::from),
            samples: sem.samples.clone(),
            codec: sem.codec,
            codec_data: sem.codec_data.clone(),
            codec_parms: sem.codec_parms.clone(),
            smask: sem.smask.clone(),
            mask: sem.mask.clone(),
            smask_in_data: sem.smask_in_data,
            lowering_degraded: sem.lowering_degraded,
        };
        let id = ImageId(self.images.len() as u32);
        self.images.push(ir);
        id
    }

    fn compute_features(
        &self,
        shadings: &[ShadingResource],
        tilings: &[TilingPattern],
    ) -> PageFeatures {
        let mut f = PageFeatures::empty();
        for op in &self.ops {
            match op {
                DisplayOp::FillPath { alpha, blend, .. }
                | DisplayOp::StrokePath { alpha, blend, .. } => {
                    f |= PageFeatures::BASIC_PATHS;
                    apply_transparency(&mut f, *alpha, *blend);
                }
                DisplayOp::DrawGlyphRun { alpha, blend, .. } => {
                    f |= PageFeatures::TEXT;
                    apply_transparency(&mut f, *alpha, *blend);
                }
                DisplayOp::DrawImage { alpha, blend, .. } => {
                    f |= PageFeatures::IMAGES;
                    apply_transparency(&mut f, *alpha, *blend);
                }
                DisplayOp::PushClip { .. } => f |= PageFeatures::CLIPPING,
                DisplayOp::PushClipText { .. } => f |= PageFeatures::CLIPPING | PageFeatures::TEXT,
                DisplayOp::BeginTransparencyGroup { .. } => f |= PageFeatures::TRANSPARENCY,
                DisplayOp::DrawShading { .. } => f |= PageFeatures::SHADINGS,
                _ => {}
            }
        }
        if !shadings.is_empty()
            || self
                .paints
                .iter()
                .any(|p| matches!(p, Paint::Shading { .. }))
        {
            f |= PageFeatures::SHADINGS;
        }
        if !tilings.is_empty()
            || self
                .paints
                .iter()
                .any(|p| matches!(p, Paint::Pattern { .. }))
        {
            f |= PageFeatures::PATTERNS;
        }
        if self
            .stroke_styles
            .iter()
            .any(|s| !s.dash_pattern.is_empty())
        {
            f |= PageFeatures::DASHED_STROKES;
        }
        if self.images.iter().any(|i| i.is_stencil) {
            f |= PageFeatures::STENCIL_MASKS;
        }
        if self.page.fonts.iter().any(|font| font.subtype == b"Type3") {
            f |= PageFeatures::TYPE3_FONTS;
        }
        // Codec requirements from each image's filter chain, and from its
        // soft mask's (an MRC page's JPX foreground carries a JBIG2 /SMask —
        // both codecs must be advertised or preflight routes it wrong).
        for img in self.page.images.iter() {
            for filter in &img.filters {
                f |= codec_feature(filter);
            }
            if let Some(smask) = &img.smask {
                f |= codec_kind_feature(smask.codec);
            }
            // A stencil `/Mask` can itself be JBIG2/CCITT-encoded — advertise
            // its codec so preflight routes the page to a backend that has it.
            if let Some(pdf_page_ir::ImageMask::Stencil(sm)) = &img.mask {
                f |= codec_kind_feature(sm.codec);
            }
        }
        // Color-management facts observed by the interpreter: flags only —
        // ICC rendering stays the arity approximation and overprint does not
        // change compositing; preflight/policy can now see them (§2.4).
        if self.page.uses_icc_color {
            f |= PageFeatures::ICC_COLOR;
        }
        if self.page.uses_overprint {
            f |= PageFeatures::OVERPRINT;
        }
        f
    }

    fn compute_complexity(&self) -> PageComplexity {
        let path_segment_count = self.paths.iter().map(|p| p.verbs.len() as u32).sum::<u32>();
        let glyph_count = self.glyph_runs.iter().map(|r| r.glyphs.len() as u32).sum();
        let image_pixels = self
            .images
            .iter()
            .map(|i| u64::from(i.width).saturating_mul(u64::from(i.height)))
            .fold(0u64, u64::saturating_add);
        // Rendering prepares images one at a time. Budget the largest cold
        // working set, not the sum of every image on the page: a four-byte
        // converted raster remains live while codec-backed inputs may hold one
        // four-byte coefficient plane per source component. An attached mask
        // can be decoded while its base image remains live, so it is nested in
        // that image's estimate.
        let estimated_image_decode_peak_bytes = self
            .images
            .iter()
            .map(estimate_cold_image_decode_bytes)
            .max()
            .unwrap_or(0);
        // Transparency surface requirement: each group needs an offscreen
        // buffer sized to its bounds (4 bytes/pixel at 1× scale).
        let estimated_peak_bytes = self
            .groups
            .iter()
            .map(|g| {
                let w = (g.bounds.x1 - g.bounds.x0).abs().ceil().max(0.0) as u64;
                let h = (g.bounds.y1 - g.bounds.y0).abs().ceil().max(0.0) as u64;
                w.saturating_mul(h).saturating_mul(4)
            })
            .fold(0u64, u64::saturating_add);
        PageComplexity {
            operation_count: self.ops.len() as u32,
            path_segment_count,
            glyph_count,
            image_pixels,
            transparency_group_count: self.groups.len() as u32,
            estimated_peak_bytes,
            estimated_image_decode_peak_bytes,
        }
    }
}

fn estimate_cold_image_decode_bytes(image: &ImageIr) -> u64 {
    let pixels = u64::from(image.width).saturating_mul(u64::from(image.height));
    let converted = pixels.saturating_mul(4);
    let codec_planes = if image.codec.is_some() {
        let source_components = (image.color_space.components() as u64)
            .saturating_add(u64::from(image.smask_in_data != 0));
        pixels.saturating_mul(source_components).saturating_mul(4)
    } else {
        0
    };
    let soft_mask = image
        .smask
        .as_deref()
        .map(estimate_cold_mask_decode_bytes)
        .unwrap_or(0);
    let hard_mask = match image.mask.as_ref() {
        Some(pdf_page_ir::ImageMask::Stencil(mask)) => estimate_cold_mask_decode_bytes(mask),
        _ => 0,
    };
    converted
        .saturating_add(codec_planes)
        .saturating_add(soft_mask)
        .saturating_add(hard_mask)
}

fn estimate_cold_mask_decode_bytes(mask: &pdf_page_ir::ImageSMask) -> u64 {
    let pixels = u64::from(mask.width).saturating_mul(u64::from(mask.height));
    // One retained byte of coverage plus a four-byte coefficient/sample plane
    // for codec-backed masks.
    pixels.saturating_mul(if mask.codec.is_some() { 5 } else { 1 })
}

/// Set the transparency and non-separable-blend flags from one op's alpha and
/// blend mode.
fn apply_transparency(f: &mut PageFeatures, alpha: f32, blend: BlendMode) {
    if alpha < 1.0 || blend != BlendMode::Normal {
        f.insert(PageFeatures::TRANSPARENCY);
    }
    if !blend.is_separable() {
        f.insert(PageFeatures::NONSEPARABLE_BLENDS);
    }
}

/// Map a canonical filter name to its codec requirement flag.
fn codec_feature(filter: &[u8]) -> PageFeatures {
    match filter {
        b"DCTDecode" => PageFeatures::NEEDS_DCT,
        b"JPXDecode" => PageFeatures::NEEDS_JPX,
        b"JBIG2Decode" => PageFeatures::NEEDS_JBIG2,
        b"CCITTFaxDecode" => PageFeatures::NEEDS_CCITT,
        _ => PageFeatures::empty(),
    }
}

/// The `NEEDS_*` feature for a decoded [`ImageCodecKind`] (soft/hard masks
/// carry a codec enum rather than a filter name).
fn codec_kind_feature(codec: Option<pdf_page_ir::ImageCodecKind>) -> PageFeatures {
    match codec {
        Some(pdf_page_ir::ImageCodecKind::Dct) => PageFeatures::NEEDS_DCT,
        Some(pdf_page_ir::ImageCodecKind::Jpx) => PageFeatures::NEEDS_JPX,
        Some(pdf_page_ir::ImageCodecKind::Jbig2) => PageFeatures::NEEDS_JBIG2,
        Some(pdf_page_ir::ImageCodecKind::CcittFax) => PageFeatures::NEEDS_CCITT,
        None => PageFeatures::empty(),
    }
}

/// Place a run's character codes using the PDF advance widths (fonts.md §3).
/// **The `glyph` field still carries the raw character code, not a font glyph
/// index** — encoding→glyph-id resolution and outlines are later font phases —
/// but horizontal placement is now metric-correct: advance = width·Tfs/1000 +
/// Tc (+ Tw for code 32), scaled by Th, with `TJ` adjustments applied.
fn placed_glyphs(
    run: &TextRun,
    metrics: &pdf_font::FontMetrics,
    glyph_map: &pdf_font::GlyphMap,
) -> Vec<PlacedGlyph> {
    let th = run.horizontal_scale / 100.0;
    let fs = run.font_size;
    // Vertical writing (wmode 1, §9.7.4.3): the pen runs down the y axis by
    // the `/W2` vertical displacement (`w1y`, typically negative), and each
    // glyph is drawn displaced by −v — the vector from its horizontal origin
    // to its vertical origin (default `vx = w0/2`, `vy = /DW2[0]`), so the
    // glyph hangs centered below the pen point. Neither the advance nor `TJ`
    // adjustments are scaled by Th, which is horizontal-only; the `v` x
    // displacement is horizontal, so it is.
    let vertical = metrics.is_vertical();
    let mut cursor = 0.0;
    let mut out = Vec::new();
    for el in run.elements.iter() {
        match el {
            TextElement::Show(bytes) => {
                // The font decodes the byte string into codes/CIDs with PDF
                // advances (1 byte for simple fonts, 2 for Identity CID); the
                // CID→GID map turns a composite CID into its glyph id.
                for dc in metrics.decode(bytes) {
                    let word = if dc.word_space { run.word_spacing } else { 0.0 };
                    if let Some(vp) = vertical.then(|| metrics.vertical(dc.cid)).flatten() {
                        let (vx, vy) = vp.origin;
                        out.push(PlacedGlyph {
                            glyph: glyph_map.gid(dc.cid),
                            x: -(vx as f64) / 1000.0 * fs * th,
                            y: cursor + run.rise - (vy as f64) / 1000.0 * fs,
                        });
                        cursor += vp.advance as f64 / 1000.0 * fs + run.char_spacing + word;
                    } else {
                        out.push(PlacedGlyph {
                            glyph: glyph_map.gid(dc.cid),
                            x: cursor,
                            y: run.rise,
                        });
                        let w = dc.advance as f64 / 1000.0 * fs;
                        cursor += (w + run.char_spacing + word) * th;
                    }
                }
            }
            TextElement::Adjust(n) => {
                cursor += if vertical {
                    -n / 1000.0 * fs
                } else {
                    -n / 1000.0 * fs * th
                };
            }
        }
    }
    out
}

// Resolve a semantic color to a device-space IR paint. Device families convert
// exactly; parameterized spaces are approximated by component arity until the
// color phase resolves their tint transforms (documented deferral).
fn resolve_paint(color: &SemColor) -> Paint {
    let clamp = |v: f64| v.clamp(0.0, 1.0) as f32;
    match color {
        SemColor::DeviceGray(g) => {
            let [r, gg, b] = pdf_color::gray_to_rgb(clamp(*g));
            Paint::Solid(rgba(r, gg, b))
        }
        SemColor::DeviceRgb(r, g, b) => Paint::Solid(rgba(clamp(*r), clamp(*g), clamp(*b))),
        SemColor::DeviceCmyk(c, m, y, k) => {
            let [r, g, b] = pdf_color::cmyk_to_rgb(clamp(*c), clamp(*m), clamp(*y), clamp(*k));
            Paint::Solid(rgba(r, g, b))
        }
        SemColor::Components { values, .. } => {
            let v: Vec<f32> = values.iter().map(|x| clamp(*x)).collect();
            let [r, g, b] = match v.len() {
                1 => pdf_color::gray_to_rgb(v[0]),
                3 => [v[0], v[1], v[2]],
                4 => pdf_color::cmyk_to_rgb(v[0], v[1], v[2], v[3]),
                _ => [0.0, 0.0, 0.0],
            };
            Paint::Solid(rgba(r, g, b))
        }
        // An unresolved pattern paints nothing (transparent) rather than a
        // spurious solid — the resolved forms below carry the real paint.
        SemColor::Pattern { .. } => Paint::Solid(Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        }),
        SemColor::ShadingPattern { shading, matrix } => Paint::Shading {
            shading: ShadingId(shading.0),
            matrix: *matrix,
        },
        SemColor::TilingPattern { tiling, matrix } => Paint::Pattern {
            tiling: TilingId(tiling.0),
            matrix: *matrix,
        },
    }
}

fn rgba(r: f32, g: f32, b: f32) -> Color {
    Color { r, g, b, a: 1.0 }
}

fn color4(c: [f32; 4]) -> Color {
    Color {
        r: c[0],
        g: c[1],
        b: c[2],
        a: c[3],
    }
}

fn resource_key(object: Option<pdf_object::ObjectId>, fallback: u32) -> ResourceKey {
    match object {
        Some(o) => ResourceKey {
            object_number: o.number,
            generation: o.generation,
            variant: 0,
        },
        None => ResourceKey {
            object_number: 0,
            generation: 0,
            variant: fallback,
        },
    }
}

/// Convert a semantic shading (already ramp-sampled) into the IR resource.
fn convert_shading(s: &SemShading) -> ShadingResource {
    let key = resource_key(s.object, 0);
    let ramp =
        |r: &[[f32; 4]]| -> std::sync::Arc<[Color]> { r.iter().map(|c| color4(*c)).collect() };
    let (shading_type, kind) = match &s.kind {
        SemShadingKind::Axial {
            coords,
            domain,
            extend,
            ramp: r,
            background,
        } => (
            2u8,
            ShadingKind::Axial {
                coords: [
                    coords[0] as f32,
                    coords[1] as f32,
                    coords[2] as f32,
                    coords[3] as f32,
                ],
                domain: [domain[0] as f32, domain[1] as f32],
                extend: *extend,
                ramp: ramp(r),
                background: background.map(color4),
            },
        ),
        SemShadingKind::Radial {
            coords,
            domain,
            extend,
            ramp: r,
            background,
        } => (
            3u8,
            ShadingKind::Radial {
                coords: [
                    coords[0] as f32,
                    coords[1] as f32,
                    coords[2] as f32,
                    coords[3] as f32,
                    coords[4] as f32,
                    coords[5] as f32,
                ],
                domain: [domain[0] as f32, domain[1] as f32],
                extend: *extend,
                ramp: ramp(r),
                background: background.map(color4),
            },
        ),
        SemShadingKind::MeshTriangles {
            shading_type,
            triangles,
            background,
        } => (
            *shading_type,
            ShadingKind::MeshTriangles {
                triangles: triangles
                    .iter()
                    .map(|t| {
                        [
                            mesh_vertex_ir(&t[0]),
                            mesh_vertex_ir(&t[1]),
                            mesh_vertex_ir(&t[2]),
                        ]
                    })
                    .collect(),
                background: background.map(color4),
            },
        ),
        SemShadingKind::MeshPatches {
            shading_type,
            patches,
            background,
        } => (
            *shading_type,
            ShadingKind::MeshPatches {
                patches: patches
                    .iter()
                    .map(|p| pdf_page_ir::MeshPatch {
                        points: p.points.map(|q| [q[0] as f32, q[1] as f32]),
                        colors: p.colors.map(color4),
                    })
                    .collect(),
                background: background.map(color4),
            },
        ),
        SemShadingKind::FunctionGrid {
            domain,
            matrix,
            grid_w,
            grid_h,
            colors,
            background,
        } => (
            1u8,
            ShadingKind::FunctionGrid {
                domain: [
                    domain[0] as f32,
                    domain[1] as f32,
                    domain[2] as f32,
                    domain[3] as f32,
                ],
                matrix: *matrix,
                grid_w: *grid_w,
                grid_h: *grid_h,
                colors: colors.iter().map(|c| color4(*c)).collect(),
                background: background.map(color4),
            },
        ),
        SemShadingKind::Unsupported { background } => (
            0u8,
            ShadingKind::Unsupported {
                background: background.map(color4),
            },
        ),
    };
    ShadingResource {
        key,
        shading_type,
        kind,
        bbox: s.bbox,
    }
}

fn mesh_vertex_ir(v: &crate::semantic::SemMeshVertex) -> pdf_page_ir::MeshVertex {
    pdf_page_ir::MeshVertex {
        x: v.x as f32,
        y: v.y as f32,
        color: color4(v.color),
    }
}

/// Convert a semantic tiling pattern into the IR resource, lowering its cell
/// content into a nested [`CompiledPage`].
fn convert_tiling(t: &SemTiling) -> TilingPattern {
    let key = resource_key(t.object, 0);
    let cell = lower(&t.cell);
    TilingPattern {
        key,
        uncolored: t.uncolored,
        under_color: color4(t.under_color),
        bbox: [
            t.bbox[0] as f32,
            t.bbox[1] as f32,
            t.bbox[2] as f32,
            t.bbox[3] as f32,
        ],
        x_step: t.x_step as f32,
        y_step: t.y_step as f32,
        cell: std::sync::Arc::new(cell),
    }
}

fn paint_key(paint: &Paint) -> PaintKey {
    match paint {
        Paint::Solid(c) => {
            PaintKey::Solid([c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits()])
        }
        Paint::Pattern { tiling, .. } => PaintKey::Pattern(tiling.0),
        Paint::Shading { shading, .. } => PaintKey::Shading(shading.0),
    }
}

fn path_key(data: &PathData) -> Vec<u8> {
    let mut key = Vec::with_capacity(data.verbs.len() + data.points.len() * 16 + 4);
    key.extend_from_slice(&(data.verbs.len() as u32).to_le_bytes());
    for v in data.verbs.iter() {
        key.push(*v as u8);
    }
    for p in data.points.iter() {
        key.extend_from_slice(&p.x.to_bits().to_le_bytes());
        key.extend_from_slice(&p.y.to_bits().to_le_bytes());
    }
    key
}

fn stroke_key(s: &StrokeStyle) -> Vec<u8> {
    let mut key = Vec::new();
    key.extend_from_slice(&s.width.to_bits().to_le_bytes());
    key.push(s.cap as u8);
    key.push(s.join as u8);
    key.extend_from_slice(&s.miter_limit.to_bits().to_le_bytes());
    key.extend_from_slice(&s.dash_phase.to_bits().to_le_bytes());
    for d in s.dash_pattern.iter() {
        key.extend_from_slice(&d.to_bits().to_le_bytes());
    }
    key
}

/// The public entry point.
pub(crate) fn lower(page: &SemanticPage) -> CompiledPage {
    Lowerer::new(page).lower()
}
