//! The `SemanticPage` — Phase 2's compilation product (roadmap §5.1).
//!
//! A `SemanticPage` holds **validated, resource-resolved PDF-level
//! operations**, but deliberately *retains high-level PDF concepts* rather
//! than flattening them: the graphics-state stack is still explicit
//! (`Save`/`Restore`/`Concat`), colors keep their PDF color-space identity
//! instead of being resolved to device RGBA, and geometry stays in its
//! natural user/text space (the CTM context is carried by the surrounding
//! `Concat` scope, not baked into points).
//!
//! Turning this into the fully-explicit, interned, backend-neutral
//! [`pdf_page_ir::CompiledPage`] — flattening transforms, resolving paints,
//! recording an explicit clip stack, computing feature/complexity summaries —
//! is the job of Phase 3. Keeping the two representations separate is what
//! lets the parser side evolve independently of every raster backend.

use std::sync::Arc;

use pdf_object::{NameId, ObjectId};
use pdf_page_ir::{
    BlendMode, FillRule, ImageColorSpace, ImageSMask, LineCap, LineJoin, MaskKind, Matrix,
    PageBounds, PathData, Rect,
};

macro_rules! handle {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);
        impl $name {
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

handle!(
    /// Index into [`SemanticPage::paths`].
    PathId
);
handle!(
    /// Index into [`SemanticPage::text_runs`].
    TextRunId
);
handle!(
    /// Identity of one active marked-content `/ActualText` scope.
    ActualTextSpanId
);
handle!(
    /// Index into [`SemanticPage::images`].
    ImageId
);
handle!(
    /// Index into [`SemanticPage::fonts`].
    FontId
);
handle!(
    /// Index into [`SemanticPage::shadings`].
    ShadingId
);
handle!(
    /// Index into [`SemanticPage::tilings`].
    TilingId
);

/// A color with its PDF color-space identity retained. Device colors keep
/// their components; parameterized spaces (ICC, Separation, CalRGB, Lab,
/// Indexed, DeviceN) are recorded symbolically by resource name with their
/// raw tint components — conversion to a device space is a Phase 3/6 concern
/// so the semantic layer never bakes in a rendering intent.
#[derive(Debug, Clone, PartialEq)]
pub enum SemColor {
    DeviceGray(f64),
    DeviceRgb(f64, f64, f64),
    DeviceCmyk(f64, f64, f64, f64),
    /// A non-device space referenced by its resource name plus raw tint
    /// components (`/CS0 sc 0.2 0.4`).
    Components {
        space: NameId,
        values: Vec<f64>,
    },
    /// A pattern fill/stroke (`/Pattern cs /P1 scn`); `under` carries the
    /// underlying color for uncolored (`PaintType 2`) tiling patterns.
    Pattern {
        name: NameId,
        under: Option<Box<SemColor>>,
    },
    /// A shading pattern (PatternType 2) already resolved to a table entry,
    /// with its pattern `/Matrix` (pattern space → page default user space).
    ShadingPattern {
        shading: ShadingId,
        matrix: Matrix,
    },
    /// A tiling pattern (PatternType 1) already resolved, with its `/Matrix`.
    TilingPattern {
        tiling: TilingId,
        matrix: Matrix,
    },
}

/// One element of a show-text run. `TJ` interleaves shows and numeric
/// position adjustments; keeping them as elements retains the PDF-level
/// structure (roadmap §5.1) instead of pre-resolving kerning.
#[derive(Debug, Clone, PartialEq)]
pub enum TextElement {
    /// Raw character-code bytes (still to be mapped through the font's
    /// encoding at glyph time — Phase 4+).
    Show(Vec<u8>),
    /// A `TJ` adjustment in thousandths of a text-space unit (negative moves
    /// the text forward).
    Adjust(f64),
}

/// Owned replacement text attached to a show-text operation.
///
/// Identity matters because PDFium emits replacement text only once for
/// consecutive text objects in the same marked-content scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualTextSpan {
    pub id: ActualTextSpanId,
    pub utf16: Arc<[u16]>,
}

/// A show-text operation with its text state captured. The mapping from
/// character codes to positioned glyphs needs font programs (a Phase 4/6
/// concern), so a semantic text run keeps the *codes* plus the text state and
/// text matrix needed to place them later.
#[derive(Debug, Clone, PartialEq)]
pub struct TextRun {
    pub font: FontId,
    pub font_size: f64,
    /// PDF text render mode 0–7 (`Tr`).
    pub render_mode: u8,
    /// `Tc` — extra spacing between characters, in unscaled text units.
    pub char_spacing: f64,
    /// `Tw` — extra spacing applied to single-byte code 32.
    pub word_spacing: f64,
    /// `Tz / 100` — horizontal scaling factor.
    pub horizontal_scale: f64,
    /// `Ts` — text rise.
    pub rise: f64,
    /// Text-space → user-space matrix (`Tm`) at the run's start. Excludes the
    /// CTM, which is carried by the enclosing `Concat` scope.
    pub text_matrix: Matrix,
    /// `Arc` rather than `Vec`: every other heavy field here is `Copy` or
    /// already refcounted, so this was the one field making `TextRun::clone`
    /// a deep copy — and the run is cloned once per show operator during
    /// interpretation and again per glyph run in both lowering and text
    /// extraction. Never mutated after construction.
    pub elements: Arc<[TextElement]>,
    /// Whether this run participates in painting. Hidden optional content and
    /// extraction-only Type 3 runs remain available to `pdf-text`.
    pub visible: bool,
    /// Innermost enclosing marked-content replacement, when present.
    pub actual_text: Option<ActualTextSpan>,
}

/// A resolved font resource as referenced by a page (`/Font /F1`). Carries the
/// PDF advance metrics needed to place text; encoding→glyph-id resolution and
/// outlines attach in later font phases (fonts.md).
#[derive(Debug, Clone, PartialEq)]
pub struct SemFont {
    /// The resource name it was reached through (`F1`).
    pub resource_name: NameId,
    /// The backing font dictionary object, if the resource resolved.
    pub object: Option<ObjectId>,
    /// `/Subtype` (e.g. `Type1`, `TrueType`, `Type0`), empty if absent.
    pub subtype: Vec<u8>,
    /// `/BaseFont` name, empty if absent.
    pub base_font: Vec<u8>,
    /// How this font's strings decode into positioned glyphs with PDF
    /// advances (simple single-byte, or Identity two-byte CID).
    pub metrics: Arc<pdf_font::FontMetrics>,
    /// Embedded outline program bytes (TrueType/CFF/OpenType), when present
    /// AND the code/CID maps reliably to a glyph id. `Some` enables real glyph
    /// rendering; `None` falls back to placement boxes.
    pub program: Option<Arc<[u8]>>,
    /// Face index inside a font collection (.ttc); 0 for a plain font.
    pub face_index: u32,
    /// Resolves a decoded code/CID to a glyph id (composite CID→GID, simple
    /// code→GID, or Identity when there is no embedded program).
    pub glyph_map: Arc<pdf_font::GlyphMap>,
    /// What must be synthesized to present the substituted face in the
    /// requested style (oblique shear / embolden); default = nothing.
    pub synthesis: pdf_font::Synthesis,
    /// Owned `/ToUnicode` and simple-encoding mappings. Keys are original
    /// character codes, never CIDs or GIDs.
    pub unicode_map: Arc<pdf_font::UnicodeMap>,
    /// Composite-font repertoire for the Adobe-*-UCS2 fallback.
    pub charset: pdf_font::Charset,
    /// `/FontBBox` from the font or its descriptor, in 1000-unit PDF glyph
    /// space. Retained for extraction loose-box geometry.
    pub font_bbox: Option<[f64; 4]>,
    /// Type 3 glyph-space matrix. `None` for every program-backed font.
    pub type3_matrix: Option<Matrix>,
}

/// A semantic image: an image XObject or an inline image. Residency-free
/// (roadmap §8.4) — this is *what the PDF says*, not decoded pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct SemImage {
    /// Backing object for an image XObject; `None` for an inline image.
    pub object: Option<ObjectId>,
    pub width: u32,
    pub height: u32,
    pub bits_per_component: u8,
    /// `/ImageMask true` — a 1-bit stencil painted with the current fill.
    pub is_mask: bool,
    /// Filter names in application order (raw, resolved to canonical spelling).
    pub filters: Vec<Vec<u8>>,
    /// Raw (still-encoded) bytes for inline images; empty for XObjects (whose
    /// bytes stay in the document source until a backend decodes them).
    pub inline_data: Vec<u8>,
    /// Source color space (device family / Indexed); `None` for stencil masks.
    pub color_space: Option<ImageColorSpace>,
    pub interpolate: bool,
    /// `/Decode` per-component `[min, max]` remap, if non-default.
    pub decode: Option<Vec<[f32; 2]>>,
    /// Filter-decoded, row-byte-aligned packed samples. `None` when the
    /// image is codec-encoded (see `codec`/`codec_data`).
    pub samples: Option<Arc<[u8]>>,
    /// Which image codec `codec_data` requires (DCT/JPX/JBIG2/CCITT).
    pub codec: Option<pdf_page_ir::ImageCodecKind>,
    /// The codec-encoded payload, general stream filters already applied.
    pub codec_data: Option<Arc<[u8]>>,
    /// `/DecodeParms` the codec needs.
    pub codec_parms: Option<pdf_page_ir::CodecParms>,
    /// Grayscale `/SMask` soft mask.
    pub smask: Option<Arc<ImageSMask>>,
    /// Explicit hard `/Mask` (color-key or stencil). `None` when `/SMask` is
    /// present (which overrides `/Mask`) or absent.
    pub mask: Option<pdf_page_ir::ImageMask>,
    /// PDF `/SMaskInData` (§7.4.7) for a JPXDecode image with an in-codestream
    /// opacity channel: 0 = ignore, 1 = soft mask, 2 = premultiplied.
    pub smask_in_data: u8,
    /// Interpretation had to approximate this image's colour pipeline (e.g. a
    /// degenerate tint-transform ramp was repaired — PLAN-POST-SWEEP3 §R1).
    /// Lowered onto [`pdf_page_ir::ImageIr::lowering_degraded`] so the
    /// degraded-draw tracker counts it.
    pub lowering_degraded: bool,
}

/// A resolved shading resource (`sh` operand or a shading pattern). The color
/// function is already sampled into an RGBA `ramp`, so the semantic/IR layers
/// carry no `Function` machinery.
#[derive(Debug, Clone, PartialEq)]
pub struct SemShading {
    pub object: Option<ObjectId>,
    pub kind: SemShadingKind,
    /// The shading's `/BBox` (ISO 32000-1 §8.7.4.3), a temporary clip in the
    /// shading's target coordinate space, normalized to `[x0, y0, x1, y1]`.
    pub bbox: Option<[f32; 4]>,
}

/// Axial/radial shading parameterization, mesh shadings (types 4–7) decoded
/// to flat triangle lists / tensor patches, and the function-based type 1
/// pre-sampled on a grid; anything unrecognized degrades to a background.
#[derive(Debug, Clone, PartialEq)]
pub enum SemShadingKind {
    Axial {
        coords: [f64; 4],
        domain: [f64; 2],
        extend: [bool; 2],
        ramp: Vec<[f32; 4]>,
        /// `/Background` (RGBA), painted outside the axis when `/Extend` is off
        /// (pattern fills only).
        background: Option<[f32; 4]>,
    },
    Radial {
        coords: [f64; 6],
        domain: [f64; 2],
        extend: [bool; 2],
        ramp: Vec<[f32; 4]>,
        background: Option<[f32; 4]>,
    },
    /// Types 4/5: Gouraud triangles in shading space, colors resolved to RGBA.
    /// `shading_type` preserves 4 vs 5 for diagnostics.
    MeshTriangles {
        shading_type: u8,
        triangles: Vec<[SemMeshVertex; 3]>,
        background: Option<[f32; 4]>,
    },
    /// Types 6/7 as tensor patches (a Coons patch is upgraded to tensor form
    /// via the §8.7.4.5.8 interior-point formulas at parse time). `points`
    /// uses the row-major grid order `p[i][j] = points[i * 4 + j]` and
    /// `colors` are `[C(0,0), C(0,1), C(1,1), C(1,0)]` — the IR convention.
    MeshPatches {
        shading_type: u8,
        patches: Vec<SemMeshPatch>,
        background: Option<[f32; 4]>,
    },
    /// Type 1 (function-based), sampled on a `grid_w × grid_h` grid over
    /// `/Domain` (`[x0, x1, y0, y1]`); `colors[t * grid_w + s]` per the IR
    /// convention, `matrix` is the shading's own `/Matrix`.
    FunctionGrid {
        domain: [f64; 4],
        matrix: Matrix,
        grid_w: u32,
        grid_h: u32,
        colors: Vec<[f32; 4]>,
        background: Option<[f32; 4]>,
    },
    Unsupported {
        background: Option<[f32; 4]>,
    },
}

/// One Gouraud mesh vertex in shading space (types 4/5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SemMeshVertex {
    pub x: f64,
    pub y: f64,
    pub color: [f32; 4],
}

/// One tensor patch (types 6/7) in shading space: 16 control points in the
/// row-major `p[i][j] = points[i * 4 + j]` order and the 4 corner colors
/// `[C(0,0), C(0,1), C(1,1), C(1,0)]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SemMeshPatch {
    pub points: [[f64; 2]; 16],
    pub colors: [[f32; 4]; 4],
}

/// A resolved tiling pattern (PatternType 1). The cell content is compiled to
/// its own [`SemanticPage`], lowered independently at Phase 3.
#[derive(Debug, Clone, PartialEq)]
pub struct SemTiling {
    pub object: Option<ObjectId>,
    pub uncolored: bool,
    pub under_color: [f32; 4],
    pub bbox: [f64; 4],
    pub x_step: f64,
    pub y_step: f64,
    pub cell: Arc<SemanticPage>,
}

/// One semantic operation. Painting operations reference interned resources
/// by handle; state operations mutate the replayed graphics state.
#[derive(Debug, Clone, PartialEq)]
pub enum SemanticOp {
    Save,
    Restore,
    Concat(Matrix),

    SetLineWidth(f64),
    SetLineCap(LineCap),
    SetLineJoin(LineJoin),
    SetMiterLimit(f64),
    SetDash {
        pattern: Vec<f64>,
        phase: f64,
    },
    SetFillColor(SemColor),
    SetStrokeColor(SemColor),
    SetFillAlpha(f32),
    SetStrokeAlpha(f32),
    SetBlendMode(BlendMode),

    Fill {
        path: PathId,
        rule: FillRule,
    },
    Stroke {
        path: PathId,
    },
    FillStroke {
        path: PathId,
        rule: FillRule,
    },
    /// Intersect the clip region with `path`. In PDF a `W`/`W*` takes effect
    /// after the path-painting operator that follows it; the interpreter emits
    /// this immediately after the corresponding paint (or on its own for the
    /// `W n` idiom).
    Clip {
        path: PathId,
        rule: FillRule,
    },
    /// Intersect the clip region with the union of the glyph outlines of these
    /// text runs — the text-clipping path accumulated across one text object
    /// whose runs used a clip render mode (Tr 4–7, ISO 32000-1 §9.4.3). Emitted
    /// at the object's `ET`; the runs are also `ShowText`-emitted for their
    /// paint (modes 4–6) or skipped as invisible (mode 7). An empty run set
    /// clips everything out (spec empty-clip-path rule).
    ClipText {
        runs: Vec<TextRunId>,
    },

    ShowText(TextRunId),
    DrawImage(ImageId),

    /// Diagnostic-only marker: the operations up to the matching
    /// [`SemanticOp::EndPaintOrigin`] came from `origin` (a Form XObject, an
    /// annotation appearance, a tiling cell, a Type 3 glyph, a soft mask).
    /// Nesting is a stack. Painting ignores these; they exist so a rendered
    /// page can be attributed back to the kind of object that produced each
    /// pixel. See `pdf_render_cpu::attribution`.
    BeginPaintOrigin(pdf_page_ir::PaintOrigin),
    /// Close the innermost [`SemanticOp::BeginPaintOrigin`].
    EndPaintOrigin,
    /// The `sh` operator: paint `shading` across the current clip region in
    /// the current user space (ISO 32000-1 §8.7.4.2).
    PaintShading(ShadingId),

    /// Begin a transparency group (a Form XObject with a `/Group`
    /// `/Transparency` subtype). Its operations follow until the matching
    /// [`SemanticOp::EndGroup`]; `bounds` is the form's `/BBox` mapped into the
    /// current user space (a conservative extent for offscreen allocation).
    /// `opacity` and `blend` are the composite parameters (the `ca`/blend in
    /// effect at the group's invocation) applied when the group is composited
    /// back onto its backdrop.
    BeginGroup {
        isolated: bool,
        knockout: bool,
        bounds: Rect,
        opacity: f32,
        blend: BlendMode,
    },
    EndGroup,

    /// Begin a soft-mask group (ExtGState `/SMask` with a `/G` group). The ops
    /// until [`SemanticOp::EndSoftMask`] are the mask's content; after it the
    /// derived mask modulates subsequent painting until scope end or
    /// [`SemanticOp::ClearSoftMask`]. `transfer` is the `/TR` function
    /// pre-sampled to a 256-entry LUT (`None` = identity).
    BeginSoftMask {
        kind: MaskKind,
        transfer: Option<pdf_page_ir::TransferLut>,
    },
    EndSoftMask,
    ClearSoftMask,
}

/// The compiled semantic page: an ordered op list plus interned resource
/// tables. Immutable and shareable; `PartialEq` so determinism tests can
/// compare compilations across worker counts.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticPage {
    pub bounds: PageBounds,
    pub ops: Arc<[SemanticOp]>,
    pub paths: Arc<[PathData]>,
    pub text_runs: Arc<[TextRun]>,
    pub images: Arc<[SemImage]>,
    pub fonts: Arc<[SemFont]>,
    pub shadings: Arc<[SemShading]>,
    pub tilings: Arc<[SemTiling]>,
    /// The page referenced an `/ICCBased` colour space (feature flag only;
    /// rendering keeps the arity approximation).
    pub uses_icc_color: bool,
    /// An ExtGState turned overprint on (`/OP` or `/op` true).
    pub uses_overprint: bool,
}

impl SemanticPage {
    /// An empty page carrying only its bounds.
    pub fn empty(bounds: PageBounds) -> Self {
        Self {
            bounds,
            ops: Arc::from([]),
            paths: Arc::from([]),
            text_runs: Arc::from([]),
            images: Arc::from([]),
            fonts: Arc::from([]),
            shadings: Arc::from([]),
            tilings: Arc::from([]),
            uses_icc_color: false,
            uses_overprint: false,
        }
    }
}

// PageBounds is Copy-of-f64s; PathData is Arc slices of plain data. Both are
// Send + Sync, so a SemanticPage crosses threads freely.
const fn _assert_send_sync<T: Send + Sync>() {}
const _: () = _assert_send_sync::<SemanticPage>();
