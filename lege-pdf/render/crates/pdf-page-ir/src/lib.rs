//! The backend-neutral compiled-page IR — **the rendering contract**.
//!
//! `CompiledPage` is what the parser side produces and what every raster
//! backend (CPU now, WGPU later) consumes. Rules (roadmap §5, §8.3):
//!
//! - Explicit painter order. Optimizers may reorder only with proven
//!   equivalence.
//! - No backend data: no bitmaps, tessellation handles, atlas positions,
//!   GPU objects, or thread-local cache references. Backend-prepared forms
//!   (`CpuPreparedPage`, `GpuPreparedPage`) live in backend crates.
//! - Geometry is `f64`; narrowing to `f32` is a backend lowering decision.
//! - Resources are typed `u32` handles into per-page tables (4-byte, cache
//!   friendly, trivially serializable for dumps and schema versioning).

use std::sync::Arc;

/// Geometry now lives in the leaf `pdf-geom` crate (shared with the lege-pdf
/// writer at migration time); re-exported here so every existing
/// `pdf_page_ir::{geom::,}*` path keeps compiling unchanged.
pub use pdf_geom as geom;

pub use pdf_geom::{DeviceRect, DeviceSize, Matrix, Point, Rect};

/// Bumped on any breaking change to this crate's types. Serialized dumps
/// and caches must key on it (roadmap §7 Phase 3).
pub const IR_SCHEMA_VERSION: u32 = 4;

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
    /// Index into [`CompiledPage::paths`].
    PathId
);
handle!(
    /// Index into [`CompiledPage::paints`].
    PaintId
);
handle!(
    /// Index into [`CompiledPage::stroke_styles`].
    StrokeStyleId
);
handle!(
    /// Index into [`CompiledPage::glyph_runs`].
    GlyphRunId
);
handle!(
    /// Index into [`CompiledPage::images`].
    ImageId
);
handle!(
    /// Index into [`CompiledPage::masks`].
    MaskId
);
handle!(
    /// Index into [`CompiledPage::groups`].
    TransparencyGroupId
);
handle!(
    /// Index into [`CompiledPage::shadings`].
    ShadingId
);
handle!(
    /// Index into [`CompiledPage::tilings`].
    TilingId
);
handle!(
    /// Index into [`CompiledPage::fonts`].
    FontId
);

/// Stable identity of a document resource for cross-page and cross-backend
/// caching (roadmap §8.5). Derived from the source object plus the decode
/// variant; the same key indexes independent CPU and GPU caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceKey {
    pub object_number: u32,
    pub generation: u16,
    /// Discriminates decode/transform variants of the same object.
    pub variant: u32,
}

/// Path fill rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

/// PDF blend modes (ISO 32000-1 §11.3.5). Separable first, nonseparable
/// last four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendMode {
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl BlendMode {
    pub fn is_separable(self) -> bool {
        !matches!(
            self,
            BlendMode::Hue | BlendMode::Saturation | BlendMode::Color | BlendMode::Luminosity
        )
    }
}

/// Image sampling policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpolationMode {
    Nearest,
    Bilinear,
}

/// Straight (non-premultiplied) RGBA color in the page's compositing space.
/// Premultiplication policy is part of the frozen surface contract in
/// `pdf-render-api`, not the IR.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const WHITE: Color = Color {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };
    pub const BLACK: Color = Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };

    /// Opaque color from straight RGB components.
    pub const fn from_rgb(r: f32, g: f32, b: f32) -> Color {
        Color { r, g, b, a: 1.0 }
    }
}

/// What to paint with.
#[derive(Debug, Clone)]
pub enum Paint {
    Solid(Color),
    /// Tiling pattern (PatternType 1) — its cell content is compiled into
    /// [`CompiledPage::tilings`], referenced by handle. `matrix` maps pattern
    /// space to the page's default user space.
    Pattern {
        tiling: TilingId,
        matrix: Matrix,
    },
    /// Shading pattern (PatternType 2) used as fill/stroke paint. `matrix`
    /// maps the shading's coordinate space to the page's default user space
    /// (the pattern `/Matrix`).
    Shading {
        shading: ShadingId,
        matrix: Matrix,
    },
}

/// Path geometry, verb/point split for compact storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathVerb {
    /// Uses 1 point.
    MoveTo,
    /// Uses 1 point.
    LineTo,
    /// Cubic Bézier; uses 3 points.
    CurveTo,
    /// Uses 0 points.
    Close,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathData {
    pub verbs: Arc<[PathVerb]>,
    pub points: Arc<[Point]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineJoin {
    Miter,
    Round,
    Bevel,
}

#[derive(Debug, Clone)]
pub struct StrokeStyle {
    pub width: f64,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f64,
    pub dash_pattern: Arc<[f64]>,
    pub dash_phase: f64,
}

/// One positioned glyph within a run. Positions are in text space already
/// combined with the text matrix — device mapping happens via the run's
/// transform at render time.
#[derive(Debug, Clone, Copy)]
pub struct PlacedGlyph {
    /// Glyph index in the run's font program.
    pub glyph: u32,
    pub x: f64,
    pub y: f64,
}

/// A run of glyphs sharing font, size, and render mode.
#[derive(Debug, Clone)]
pub struct GlyphRun {
    pub font: FontId,
    pub font_size: f64,
    pub transform: Matrix,
    pub glyphs: Arc<[PlacedGlyph]>,
    /// PDF text render mode 0–7 (fill/stroke/clip combinations).
    pub render_mode: u8,
}

/// The stroke half of a text-showing operation, for render modes Tr 1/2/5/6
/// (§9.3.1). The glyph outlines are stroked with this paint/alpha and the
/// stroke style (line width, cap, join, dash) captured from the graphics
/// state — the line width being in unscaled text space, transformed to device
/// by the CTM like any other stroke.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphStroke {
    pub paint: PaintId,
    pub style: StrokeStyleId,
    pub alpha: f32,
}

/// Font reference in the IR: identity + program bytes. Backend-neutral —
/// glyph rasterization state is backend business.
#[derive(Debug, Clone)]
pub struct FontResource {
    pub key: ResourceKey,
    /// Embedded/substituted program bytes; shared document-wide.
    pub program: Arc<[u8]>,
    /// Face index inside a font *collection* (`.ttc`/`.otc`); 0 for a plain
    /// font. A system CJK family is typically one collection holding a face
    /// per language, so the bytes alone do not identify the font.
    pub face_index: u32,
    /// Synthetic oblique for a substituted face lacking the requested italic
    /// cut: the shear tangent to apply in glyph space (0 = none; PDFium
    /// slants 12°).
    pub synthetic_shear: f32,
    /// Synthetic bold for a substituted face lacking the requested bold cut:
    /// outline growth as a fraction of the em (0 = none; PDFium's weight-700
    /// embolden level is 70/1000 em — `cfx_face.cpp` `kWeightPow`).
    pub synthetic_embolden_em: f32,
}

/// A backend-neutral image color space (roadmap §8.4). Device families plus
/// Indexed; other spaces are approximated to a device family by arity at
/// compile time (documented deferral).
#[derive(Debug, Clone, PartialEq)]
pub struct IccCmykTransform {
    /// Grid points per input dimension; the CLUT contains `grid.pow(4)` cells.
    pub grid: u8,
    /// Four normalized input curves, one per CMYK channel.
    pub input_tables: [Arc<[f32]>; 4],
    /// `grid.pow(4)` normalized Lab-PCS triples, first input most significant.
    pub clut: Arc<[[f32; 3]]>,
    /// Three normalized output curves for the encoded Lab PCS.
    pub output_tables: [Arc<[f32]>; 3],
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImageColorSpace {
    Gray,
    Rgb,
    Cmyk,
    /// Indexed palette: `base` space, `hival` (max index), and the packed
    /// lookup table (`base` components × `hival + 1` bytes, 8-bit each).
    Indexed {
        base: Box<ImageColorSpace>,
        hival: u32,
        lookup: Arc<[u8]>,
    },
    /// A single-input `/Separation` (or 1-colorant `/DeviceN`) image space,
    /// pre-resolved to a 256-entry sample→sRGB table (`256 × 3` bytes). The one
    /// sample per pixel is a *tint*, not a gray value: it is routed through the
    /// tint transform into the alternate space at compile time and baked here,
    /// so the backend indexes `rgb[normalized_sample]` instead of mis-reading
    /// the tint as DeviceGray (which inverts near-white scans to near-black).
    TintLut {
        rgb: Arc<[u8]>,
    },
    /// A **two**-colorant `/DeviceN` image space, pre-resolved to a
    /// `256 x 256 x 3` sample-pair→sRGB table (192 KiB). The two samples per
    /// pixel are tints of two colorants, not colour channels: the arity
    /// approximation read one of them as DeviceGray, which is badly wrong for a
    /// spot+black duotone. Entry `(a, b)` lives at `rgb[(a * 256 + b) * 3]`.
    /// Two inputs is where a baked table stops being affordable — three would
    /// be 48 MiB — so wider `/DeviceN` images keep the arity approximation.
    TintLut2 {
        rgb: Arc<[u8]>,
    },
    /// An `/ICCBased` RGB image whose embedded profile is **not** sRGB. The
    /// three samples are encoded in that profile, so reading them as sRGB
    /// shifts the whole image (a gamma-1.8 monitor profile renders ~10/255 too
    /// dark per channel). `trc` is three concatenated 256-point tone curves
    /// (encoded → linear) and `matrix` is the row-major profile→XYZ(D65)
    /// matrix; the backend runs them through `pdf_color::icc::to_srgb_with`.
    /// An sRGB profile never produces this variant — it stays `Rgb`, so the
    /// common case is untouched.
    IccRgb {
        trc: Arc<[f32]>,
        matrix: [f32; 9],
    },
    /// An `/ICCBased` CMYK image carrying a parsed 4-input Lab-PCS lookup
    /// transform. This is primarily needed for codec-backed JPEG/JPX images:
    /// their samples do not exist until backend preparation, so compile-time
    /// conversion cannot consume them.
    IccCmyk {
        transform: Arc<IccCmykTransform>,
    },
    /// A CIE `/Lab` image space. The three samples are L*, a*, b* — *not* RGB.
    /// Reading them as RGB is badly wrong: a near-zero b* byte is strongly
    /// blue, but as a blue channel it is black (custom/image_lab rendered
    /// yellow-green where PDFium, hayro and MuPDF all render blue). The
    /// backend maps each normalised sample through `range` and converts with
    /// `pdf_color::lab_to_rgb`.
    Lab {
        white_point: [f32; 3],
        range: [f32; 4],
    },
}

impl ImageColorSpace {
    /// Number of samples per pixel in the *source* data (1 for Indexed/TintLut).
    pub fn components(&self) -> usize {
        match self {
            ImageColorSpace::Gray => 1,
            ImageColorSpace::Rgb => 3,
            ImageColorSpace::Cmyk => 4,
            ImageColorSpace::Lab { .. } => 3,
            ImageColorSpace::Indexed { .. } => 1,
            ImageColorSpace::TintLut { .. } => 1,
            ImageColorSpace::TintLut2 { .. } => 2,
            ImageColorSpace::IccRgb { .. } => 3,
            ImageColorSpace::IccCmyk { .. } => 4,
        }
    }
}

/// Immutable owned bytes whose original `Vec` allocation is shared without a
/// `Vec -> Arc<[u8]>` full-buffer copy. The private field intentionally exposes
/// only slices: codec payloads are immutable after page compilation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharedBytes(Arc<Vec<u8>>);

impl SharedBytes {
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl From<Vec<u8>> for SharedBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(Arc::new(bytes))
    }
}

impl From<&[u8]> for SharedBytes {
    fn from(bytes: &[u8]) -> Self {
        Self::from(bytes.to_vec())
    }
}

impl std::ops::Deref for SharedBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

/// A grayscale soft-mask image attached to an image XObject (`/SMask`). Kept
/// as decoded samples so the backend resamples it into image space.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageSMask {
    pub width: u32,
    pub height: u32,
    pub bits_per_component: u8,
    pub decode: Option<Arc<[[f32; 2]]>>,
    /// Filter-decoded, row-byte-aligned packed samples. Empty when the mask
    /// is codec-encoded — then `codec`/`codec_data` carry the payload and the
    /// backend decodes it through its registry (MRC scans mask with JBIG2).
    pub samples: Arc<[u8]>,
    /// Which image codec `codec_data` requires.
    pub codec: Option<ImageCodecKind>,
    /// The codec-encoded payload (general stream filters already applied).
    pub codec_data: Option<SharedBytes>,
    /// `/DecodeParms` the mask's codec needs (JBIG2 globals, CCITT K/…).
    pub codec_parms: Option<CodecParms>,
}

/// An explicit `/Mask` on an image (ISO 32000-1 §8.9.6.3–8.9.6.4), distinct
/// from the grayscale `/SMask`: a *hard* (fully on/off) mask. When both are
/// present `/SMask` wins and `/Mask` is dropped (§8.9.6.4 note), so this is
/// only ever set on images without a soft mask.
#[derive(Debug, Clone, PartialEq)]
pub enum ImageMask {
    /// Color-key masking (§8.9.6.4). `/Mask` is `[min0 max0 min1 max1 …]`,
    /// one `[min, max]` pair per source component in **raw sample space** —
    /// the integer sample values *before* `/Decode` is applied. A texel is
    /// fully transparent iff every component's raw sample lies within its
    /// range. One pair per component (Indexed counts as one: the index).
    ColorKey(Arc<[[u32; 2]]>),
    /// A stencil mask (§8.9.6.3): a separate 1-bit image-mask XObject
    /// (`/ImageMask true`). A mask sample of 1 (after the mask's own
    /// `/Decode`, default `[0 1]`) marks the base pixel transparent; 0 paints
    /// — the **reverse** polarity of an `/SMask` luminosity. The mask has its
    /// own geometry and is resampled into image space like a soft mask; the
    /// payload reuses [`ImageSMask`] so the codec path (JBIG2/CCITT masks)
    /// comes for free.
    Stencil(Arc<ImageSMask>),
}

/// Image reference in the IR (roadmap §8.4): the decoded *semantics* — packed
/// samples, color space, decode — not a premultiplied RGBA bitmap. Backend
/// caches hold residency (CPU raster / GPU texture) keyed by `key`.
#[derive(Debug, Clone)]
pub struct ImageIr {
    pub key: ResourceKey,
    pub width: u32,
    pub height: u32,
    /// True for `/ImageMask` stencils painted with the current fill paint.
    pub is_stencil: bool,
    pub interpolation: InterpolationMode,
    /// Optional soft-mask image (legacy handle; superseded by `smask`).
    pub soft_mask: Option<MaskId>,
    pub bits_per_component: u8,
    pub color_space: ImageColorSpace,
    /// Per-component `[min, max]` decode remap (`/Decode`), if non-default.
    pub decode: Option<Arc<[[f32; 2]]>>,
    /// Filter-decoded, row-byte-aligned packed samples. `None` when the
    /// image is codec-encoded — then `codec`/`codec_data` carry the encoded
    /// payload, and the page's `NEEDS_*` feature routes it to a backend
    /// whose codec registry covers it (or away entirely).
    pub samples: Option<Arc<[u8]>>,
    /// Which image codec `codec_data` requires.
    pub codec: Option<ImageCodecKind>,
    /// The codec-encoded payload (general stream filters already applied).
    pub codec_data: Option<SharedBytes>,
    /// `/DecodeParms` a codec needs: JBIG2's globals stream, CCITT's K /
    /// Columns / BlackIs1. Backend-neutral because the codec registry that
    /// consumes them is backend-injected.
    pub codec_parms: Option<CodecParms>,
    /// Grayscale soft mask, resampled by the backend into image space.
    pub smask: Option<Arc<ImageSMask>>,
    /// Explicit hard `/Mask` (color-key or stencil), §8.9.6.3–8.9.6.4. Only
    /// set when `smask` is absent (`/SMask` overrides `/Mask`).
    pub mask: Option<ImageMask>,
    /// PDF `/SMaskInData` (§7.4.7): 0 = ignore any in-codestream alpha, 1 = the
    /// JPXDecode opacity channel is an (unassociated) soft mask, 2 =
    /// premultiplied. Only meaningful for a JPX image carrying a `cdef` opacity
    /// channel; the backend splits that alpha into `smask` when this is ≥ 1.
    pub smask_in_data: u8,
    /// Lowering could not vouch for this image's pixel values (e.g. a
    /// Separation/Indexed tint LUT built from a suspect tint transform — the
    /// R1 class). The image still paints, but the executing backend must tick
    /// `degraded_draws` so a whitewashed cover can never be scored clean by
    /// the silent-blank detector. Set by pdf-content, consumed by
    /// pdf-render-cpu; false everywhere else.
    pub lowering_degraded: bool,
}

/// Filter-specific `/DecodeParms` carried to whichever codec decodes the
/// image.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodecParms {
    /// `/JBIG2Globals`, decoded.
    pub jbig2_globals: Option<Arc<[u8]>>,
    /// `/K` — CCITT encoding mode (< 0 Group 4, 0 Group 3 1-D, > 0 mixed).
    pub k: i32,
    pub columns: u32,
    pub rows: u32,
    pub black_is_1: bool,
    pub byte_align: bool,
    pub end_of_line: bool,
    pub end_of_block: bool,
}

/// The image codec an [`ImageIr::codec_data`] payload is encoded with.
/// Backends map this onto their injected codec registry; the parallel
/// `PageFeatures::NEEDS_*` flags drive preflight routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageCodecKind {
    /// `/DCTDecode` (JPEG).
    Dct,
    /// `/JPXDecode` (JPEG 2000).
    Jpx,
    /// `/JBIG2Decode`.
    Jbig2,
    /// `/CCITTFaxDecode`.
    CcittFax,
}

/// Soft mask description.
#[derive(Debug, Clone)]
pub struct MaskResource {
    pub key: ResourceKey,
    pub kind: MaskKind,
}

/// A `/TR` soft-mask transfer function pre-sampled to a 256-entry LUT
/// (ISO 32000-1 §11.6.5.2): the backend applies `mask = lut[mask]` after
/// deriving each mask value. Shared cheaply; `None` in the carrying op means
/// identity.
pub type TransferLut = Arc<[u8; 256]>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskKind {
    Alpha,
    Luminosity,
    /// Luminosity with an explicit `/BC` backdrop color (ISO 32000-1
    /// §11.6.5.2), already converted by lowering into the mask group's
    /// equivalent RGB bytes: the mask content is composited against a
    /// backdrop of this color (instead of black), and areas outside the
    /// mask's `/BBox` take the backdrop's luminosity (instead of 0).
    ///
    /// Append-only addition (A6); pdf-content emits this variant when the
    /// `/SMask` dictionary carries `/BC`. A `/TR` transfer function rides
    /// separately as the sampled [`TransferLut`] on
    /// [`DisplayOp::BeginSoftMask`] (it is non-`Copy`, which `MaskKind`
    /// cannot carry) — see pdf-render-cpu `derive_soft_mask` for where it
    /// applies.
    LuminosityBc {
        backdrop: [u8; 3],
    },
}

/// Transparency group parameters (ISO 32000-1 §11.4.7, §11.6.6).
#[derive(Debug, Clone)]
pub struct TransparencyGroup {
    pub isolated: bool,
    pub knockout: bool,
    /// Conservative bounds (page user space), for bounded offscreen allocation.
    pub bounds: Rect,
    /// Constant alpha applied to the whole group at composite-back time (the
    /// `ca` in effect at the group's invocation).
    pub opacity: f32,
    /// Blend mode used to composite the group onto its backdrop.
    pub blend: BlendMode,
}

/// Shading (gradient) description (ISO 32000-1 §8.7.4.5). The color function
/// is pre-sampled into a `ramp` at compile time — a backend-neutral lookup
/// table a CPU or GPU backend consumes identically — so no PDF `Function`
/// machinery leaks into the render backends.
#[derive(Debug, Clone)]
pub struct ShadingResource {
    pub key: ResourceKey,
    /// Shading types 1–7 per spec.
    pub shading_type: u8,
    pub kind: ShadingKind,
    /// The shading's `/BBox` (ISO 32000-1 §8.7.4.3), a temporary clip in the
    /// shading's target coordinate space, normalized to `[x0, y0, x1, y1]`.
    pub bbox: Option<[f32; 4]>,
}

/// The rasterizable parameterization of a shading. Axial (type 2) and radial
/// (type 3) are modeled; other types are carried as a background-only hook.
#[derive(Debug, Clone)]
pub enum ShadingKind {
    /// Type 2: a gradient along the axis from `(x0,y0)` to `(x1,y1)`.
    Axial {
        coords: [f32; 4],
        domain: [f32; 2],
        /// Extend past the axis start/end with the endpoint colors.
        extend: [bool; 2],
        /// Pre-sampled color ramp across the domain (index 0 = domain start).
        ramp: Arc<[Color]>,
        /// `/Background`, painted outside the axis when `/Extend` is off
        /// (pattern fills only; ignored for `sh`).
        background: Option<Color>,
    },
    /// Type 3: a gradient between two circles `(x0,y0,r0)` and `(x1,y1,r1)`.
    Radial {
        coords: [f32; 6],
        domain: [f32; 2],
        extend: [bool; 2],
        ramp: Arc<[Color]>,
        background: Option<Color>,
    },
    /// Types 1 and 4–7: not yet rasterized. A `/Background` color, if present,
    /// is the only thing a backend can paint (documented deferral).
    Unsupported { background: Option<Color> },
    /// Types 4/5 decoded to a flat Gouraud triangle list, in shading space
    /// (append-only, A9). All stream decoding — `/BitsPerCoordinate`/
    /// `/BitsPerComponent`/`/BitsPerFlag` unpacking, `/Decode` mapping,
    /// free-form edge flags (type 4) and lattice rows (type 5) — is the
    /// lowering side's job; backends receive resolved triangles.
    MeshTriangles {
        triangles: Arc<[[MeshVertex; 3]]>,
        background: Option<Color>,
    },
    /// Types 6/7 as tensor patches in shading space (append-only, A9). A
    /// Coons patch (type 6) is upgraded to tensor form by lowering via the
    /// §8.7.4.5.7 interior-point formula; `points` uses the tensor row-major
    /// grid order `p[i][j] = points[i * 4 + j]` (i along the u/D1 direction),
    /// and `colors` are the four corner colors `[C(0,0), C(0,1), C(1,1),
    /// C(1,0)]` matching the spec's corner numbering.
    MeshPatches {
        patches: Arc<[MeshPatch]>,
        background: Option<Color>,
    },
    /// Type 1 (function-based), pre-sampled by lowering on a `grid_w ×
    /// grid_h` grid over `/Domain` (append-only, A9): `colors[t * grid_w + s]`
    /// is the color at domain coordinates
    /// `(x0 + (s + 0.5)/grid_w · (x1 − x0), y0 + (t + 0.5)/grid_h · (y1 − y0))`.
    /// `matrix` is the shading's own `/Matrix` (domain → shading target
    /// space); backends compose it with the pattern/CTM transform.
    FunctionGrid {
        /// `/Domain` as `[x0, x1, y0, y1]`.
        domain: [f32; 4],
        matrix: Matrix,
        grid_w: u32,
        grid_h: u32,
        colors: Arc<[Color]>,
        background: Option<Color>,
    },
}

/// One Gouraud mesh vertex in shading space (A9, mesh shadings).
#[derive(Debug, Clone, Copy)]
pub struct MeshVertex {
    pub x: f32,
    pub y: f32,
    pub color: Color,
}

/// One tensor patch (types 6/7) in shading space (A9): 16 control points in
/// row-major `p[i][j] = points[i * 4 + j]` order and 4 corner colors — see
/// [`ShadingKind::MeshPatches`].
#[derive(Debug, Clone, Copy)]
pub struct MeshPatch {
    pub points: [[f32; 2]; 16],
    pub colors: [Color; 4],
}

/// A tiling pattern (PatternType 1, ISO 32000-1 §8.7.3.1): a cell of content
/// replicated on a lattice. The cell content is compiled into its own op
/// stream + resource tables (a self-contained mini [`CompiledPage`]).
#[derive(Debug, Clone)]
pub struct TilingPattern {
    pub key: ResourceKey,
    /// `true` for an uncolored pattern (PaintType 2): the cell is a stencil
    /// painted with `under_color`; `false` for a colored pattern.
    pub uncolored: bool,
    /// Underlying fill color for an uncolored pattern.
    pub under_color: Color,
    /// Cell bounding box in pattern space `[x0, y0, x1, y1]`.
    pub bbox: [f32; 4],
    /// Horizontal / vertical replication step in pattern space.
    pub x_step: f32,
    pub y_step: f32,
    /// The compiled cell: a nested page whose operations paint one cell.
    pub cell: Arc<CompiledPage>,
}

bitflags::bitflags! {
    /// Feature summary for backend preflight (roadmap §2.4): a backend
    /// declares which features it implements; the scheduler routes pages.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PageFeatures: u64 {
        const BASIC_PATHS             = 1 << 0;
        const TEXT                    = 1 << 1;
        const IMAGES                  = 1 << 2;
        const CLIPPING                = 1 << 3;
        const TRANSPARENCY            = 1 << 4;
        const SOFT_MASKS              = 1 << 5;
        const PATTERNS                = 1 << 6;
        const SHADINGS                = 1 << 7;
        const TYPE3_FONTS             = 1 << 8;
        const ICC_COLOR               = 1 << 9;
        const NONSEPARABLE_BLENDS     = 1 << 10;
        const OVERPRINT               = 1 << 11;
        const STENCIL_MASKS           = 1 << 12;
        const DASHED_STROKES          = 1 << 13;

        // Image codec requirements, set by the page compiler from each
        // image's filter chain. A renderer whose codec registry lacks the
        // decoder declines (or degrades) the page *before* work starts, so
        // a missing codec is an observable routing decision, never a
        // silent blank page.
        const NEEDS_DCT               = 1 << 14;
        const NEEDS_JPX               = 1 << 15;
        const NEEDS_JBIG2             = 1 << 16;
        const NEEDS_CCITT             = 1 << 17;
    }
}

impl PageFeatures {
    /// All codec-requirement flags.
    pub const CODEC_MASK: PageFeatures = PageFeatures::NEEDS_DCT
        .union(PageFeatures::NEEDS_JPX)
        .union(PageFeatures::NEEDS_JBIG2)
        .union(PageFeatures::NEEDS_CCITT);

    /// The codec requirements this page carries.
    pub fn required_codecs(self) -> PageFeatures {
        self.intersection(Self::CODEC_MASK)
    }
}

/// Cost estimates for scheduling and memory budgeting (roadmap §7 Phase 3).
#[derive(Debug, Clone, Copy, Default)]
pub struct PageComplexity {
    pub operation_count: u32,
    pub path_segment_count: u32,
    pub glyph_count: u32,
    pub image_pixels: u64,
    pub transparency_group_count: u32,
    /// Estimated peak intermediate bytes at 1x scale.
    pub estimated_peak_bytes: u64,
    /// Largest cold image working set on the page at source resolution.
    ///
    /// Unrelated images are decoded sequentially by a render job, so this is a
    /// maximum rather than a page-wide sum. It includes the retained converted
    /// image and codec coefficient planes, plus an attached mask when present.
    /// The scheduler owns one outer permit covering this estimate; codecs must
    /// not reacquire the same page budget while that permit is held.
    pub estimated_image_decode_peak_bytes: u64,
}

/// Page geometry needed to map user space to device space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageBounds {
    /// CropBox-derived visible rect in PDF user-space points.
    pub crop: Rect,
    /// Normalized rotation in {0, 90, 180, 270}.
    pub rotate: u16,
}

/// One explicit painter-order operation (roadmap §5.2).
/// Where a painting operation came from, kept purely for **diagnostic
/// attribution** (`pdf_render_cpu::attribution`). It is deliberately about the
/// *containing construct*, not what the operation paints: a Form XObject holds
/// nested text, images and paths, and both facts are worth knowing when a
/// differential harness asks "what kind of object do these disagreeing pixels
/// belong to?".
///
/// Never consulted while painting. Adding a variant cannot change a rendered
/// pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum PaintOrigin {
    /// The page's own content stream.
    #[default]
    PageContent = 0,
    /// Inside a Form XObject invoked by `Do` (any nesting depth).
    FormXObject = 1,
    /// An annotation's `/AP` appearance stream.
    AnnotationAppearance = 2,
    /// Inside a tiling pattern's cell content.
    TilingPatternCell = 3,
    /// Inside a Type 3 font's glyph procedure.
    Type3Glyph = 4,
    /// Inside a soft mask's content stream. Such operations render offscreen
    /// and never reach the page, so this never appears in a page attribution
    /// map; it exists so the mask pass can be attributed too.
    SoftMaskContent = 5,
}

impl PaintOrigin {
    /// Stable name for report output.
    pub fn name(self) -> &'static str {
        match self {
            Self::PageContent => "page-content",
            Self::FormXObject => "form-xobject",
            Self::AnnotationAppearance => "annotation-appearance",
            Self::TilingPatternCell => "tiling-pattern-cell",
            Self::Type3Glyph => "type3-glyph",
            Self::SoftMaskContent => "soft-mask-content",
        }
    }
}

/// What kind of construct actually painted a pixel — the *leaf* of the
/// attribution, as opposed to [`PaintOrigin`]'s container. Derived from the
/// prepared operation kind, so it needs no plumbing through the IR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum PaintLeaf {
    /// No operation covered this pixel.
    #[default]
    Unpainted = 0,
    /// A filled or stroked path with a solid colour.
    Path = 1,
    /// A path painted from a shading (a shading pattern, or the `sh` operator).
    Shading = 2,
    /// A path filled with a tiling pattern.
    TilingPattern = 3,
    /// Glyphs.
    Text = 4,
    /// An image XObject or an inline image.
    Image = 5,
}

impl PaintLeaf {
    /// Stable name for report output.
    pub fn name(self) -> &'static str {
        match self {
            Self::Unpainted => "unpainted",
            Self::Path => "path",
            Self::Shading => "shading",
            Self::TilingPattern => "tiling-pattern",
            Self::Text => "text",
            Self::Image => "image",
        }
    }
}

#[derive(Debug, Clone)]
pub enum DisplayOp {
    Save,
    Restore,
    ConcatTransform(Matrix),
    PushClip {
        path: PathId,
        rule: FillRule,
    },
    /// Push a clip formed by the union of the given glyph runs' outlines: the
    /// accumulated text-clipping path of a single text object whose runs used a
    /// clip render mode (Tr 4–7, ISO 32000-1 §9.4.3 / §9.3.3). Emitted at the
    /// text object's `ET`; balanced by a [`DisplayOp::PopClip`] at the enclosing
    /// `Restore`, exactly like [`DisplayOp::PushClip`]. An empty run set (a
    /// clip-mode text object that placed no glyph outlines) clips everything
    /// out, per the spec's empty-clip-path rule.
    PushClipText {
        runs: Box<[GlyphRunId]>,
    },
    PopClip,
    FillPath {
        path: PathId,
        paint: PaintId,
        rule: FillRule,
        alpha: f32,
        blend: BlendMode,
    },
    StrokePath {
        path: PathId,
        paint: PaintId,
        style: StrokeStyleId,
        alpha: f32,
        blend: BlendMode,
    },
    /// Show a text run. `paint`/`alpha` are the fill; `stroke` is present for
    /// the stroking render modes (Tr 1/2/5/6). The run's own `render_mode`
    /// selects which of fill/stroke actually paint (fill for 0/2/4/6, stroke
    /// for 1/2/5/6).
    DrawGlyphRun {
        run: GlyphRunId,
        paint: PaintId,
        alpha: f32,
        blend: BlendMode,
        stroke: Option<GlyphStroke>,
    },
    DrawImage {
        image: ImageId,
        paint: PaintId,
        transform: Matrix,
        alpha: f32,
        blend: BlendMode,
    },
    BeginTransparencyGroup {
        group: TransparencyGroupId,
    },
    EndTransparencyGroup,
    ApplySoftMask {
        mask: Option<MaskId>,
    },
    DrawShading {
        shading: ShadingId,
        transform: Matrix,
    },

    /// Begin a soft-mask group: the operations up to the matching
    /// [`DisplayOp::EndSoftMask`] are the mask's content (rendered offscreen,
    /// never to the page). After `EndSoftMask` the derived per-pixel mask (the
    /// content's luminosity or alpha) modulates subsequent painting until the
    /// enclosing scope restores or [`DisplayOp::ClearSoftMask`].
    ///
    /// `transfer` is the `/TR` transfer function pre-sampled to a 256-entry
    /// LUT at compile time (`mask = lut[mask]`, applied after the per-pixel
    /// derivation and to the outside-extent value); `None` is identity.
    /// Function evaluation stays compile-side; render stays data-only.
    BeginSoftMask {
        kind: MaskKind,
        transfer: Option<TransferLut>,
    },
    EndSoftMask,
    /// `/SMask /None`: disable soft masking in the current scope.
    ClearSoftMask,

    /// Diagnostic-only marker: the operations up to the matching
    /// [`DisplayOp::EndPaintOrigin`] came from `origin`. Nesting is a stack.
    /// **Painting ignores these entirely** — they exist so
    /// `pdf_render_cpu::attribution` can label pixels by containing construct.
    BeginPaintOrigin(PaintOrigin),
    /// Close the innermost [`DisplayOp::BeginPaintOrigin`].
    EndPaintOrigin,
}

/// The compiled page (roadmap §5.3). Immutable, `Send + Sync`, shareable
/// across any number of render jobs at different scales.
#[derive(Debug, Clone)]
pub struct CompiledPage {
    pub schema_version: u32,
    pub bounds: PageBounds,
    /// Conservative union of visible painting operations in page user space.
    ///
    /// `None` means that the page paints nothing measurable (or that an older
    /// producer could not determine an extent). Consumers must then fall back
    /// to the page box.
    pub content_bounds: Option<Rect>,
    pub operations: Arc<[DisplayOp]>,
    pub paths: Arc<[PathData]>,
    pub paints: Arc<[Paint]>,
    pub stroke_styles: Arc<[StrokeStyle]>,
    pub glyph_runs: Arc<[GlyphRun]>,
    pub fonts: Arc<[FontResource]>,
    pub images: Arc<[ImageIr]>,
    pub masks: Arc<[MaskResource]>,
    pub groups: Arc<[TransparencyGroup]>,
    pub shadings: Arc<[ShadingResource]>,
    pub tilings: Arc<[TilingPattern]>,
    pub features: PageFeatures,
    pub complexity: PageComplexity,
}

impl CompiledPage {
    /// A deterministic, human-readable serialization of the whole IR (roadmap
    /// §7 Phase 3, task 8). Keyed by [`IR_SCHEMA_VERSION`] so a dump names the
    /// schema it was produced under. Intended for golden tests and debugging,
    /// not as a wire format.
    pub fn debug_dump(&self) -> String {
        use std::fmt::Write as _;
        let mut o = String::new();
        let b = &self.bounds.crop;
        let _ = writeln!(o, "CompiledPage schema={}", self.schema_version);
        let _ = writeln!(
            o,
            "bounds [{} {} {} {}] rotate {}",
            g(b.x0),
            g(b.y0),
            g(b.x1),
            g(b.y1),
            self.bounds.rotate
        );

        let flags: Vec<&str> = self.features.iter_names().map(|(n, _)| n).collect();
        let _ = writeln!(
            o,
            "features: {}",
            if flags.is_empty() {
                "none".into()
            } else {
                flags.join("|")
            }
        );
        let c = &self.complexity;
        let _ = writeln!(
            o,
            "complexity: ops={} segments={} glyphs={} image_pixels={} groups={} peak_bytes={} image_decode_peak_bytes={}",
            c.operation_count,
            c.path_segment_count,
            c.glyph_count,
            c.image_pixels,
            c.transparency_group_count,
            c.estimated_peak_bytes,
            c.estimated_image_decode_peak_bytes
        );

        let _ = writeln!(o, "operations:");
        for op in self.operations.iter() {
            let _ = writeln!(o, "  {}", fmt_op(op));
        }
        dump_table(&mut o, "paths", &self.paths, |p| {
            format!("verbs={} points={}", p.verbs.len(), p.points.len())
        });
        dump_table(&mut o, "paints", &self.paints, fmt_paint);
        dump_table(&mut o, "stroke_styles", &self.stroke_styles, |s| {
            format!(
                "width={} cap={:?} join={:?} miter={} dash=[{}] phase={}",
                g(s.width),
                s.cap,
                s.join,
                g(s.miter_limit),
                s.dash_pattern
                    .iter()
                    .map(|d| g(*d))
                    .collect::<Vec<_>>()
                    .join(" "),
                g(s.dash_phase)
            )
        });
        dump_table(&mut o, "glyph_runs", &self.glyph_runs, |r| {
            format!(
                "font#{} size {} mode {} glyphs {}",
                r.font.0,
                g(r.font_size),
                r.render_mode,
                r.glyphs.len()
            )
        });
        dump_table(&mut o, "fonts", &self.fonts, |f| {
            format!(
                "key {}:{}:{} program_bytes={}",
                f.key.object_number,
                f.key.generation,
                f.key.variant,
                f.program.len()
            )
        });
        dump_table(&mut o, "images", &self.images, |i| {
            format!(
                "{}x{} bpc={} stencil={} interp={:?} decoded={} smask={}",
                i.width,
                i.height,
                i.bits_per_component,
                i.is_stencil,
                i.interpolation,
                i.samples.is_some(),
                i.smask.is_some()
            )
        });
        dump_table(&mut o, "groups", &self.groups, |gr| {
            format!(
                "isolated={} knockout={} opacity={} blend={:?} bounds [{} {} {} {}]",
                gr.isolated,
                gr.knockout,
                g(gr.opacity as f64),
                gr.blend,
                g(gr.bounds.x0),
                g(gr.bounds.y0),
                g(gr.bounds.x1),
                g(gr.bounds.y1)
            )
        });
        dump_table(&mut o, "shadings", &self.shadings, |s| match &s.kind {
            ShadingKind::Axial { extend, ramp, .. } => {
                format!(
                    "type{} axial extend=[{} {}] ramp={}",
                    s.shading_type,
                    extend[0],
                    extend[1],
                    ramp.len()
                )
            }
            ShadingKind::Radial { extend, ramp, .. } => {
                format!(
                    "type{} radial extend=[{} {}] ramp={}",
                    s.shading_type,
                    extend[0],
                    extend[1],
                    ramp.len()
                )
            }
            ShadingKind::Unsupported { .. } => format!("type{} unsupported", s.shading_type),
            ShadingKind::MeshTriangles { triangles, .. } => {
                format!("type{} mesh triangles={}", s.shading_type, triangles.len())
            }
            ShadingKind::MeshPatches { patches, .. } => {
                format!("type{} mesh patches={}", s.shading_type, patches.len())
            }
            ShadingKind::FunctionGrid { grid_w, grid_h, .. } => {
                format!("type{} function grid {}x{}", s.shading_type, grid_w, grid_h)
            }
        });
        dump_table(&mut o, "tilings", &self.tilings, |t| {
            format!(
                "uncolored={} step=[{} {}] ops={}",
                t.uncolored,
                g(t.x_step as f64),
                g(t.y_step as f64),
                t.cell.operations.len()
            )
        });
        o
    }

    /// An empty page with the given bounds — the minimal valid value; also
    /// what stub compilers return today.
    pub fn empty(bounds: PageBounds) -> Self {
        Self {
            schema_version: IR_SCHEMA_VERSION,
            bounds,
            content_bounds: None,
            operations: Arc::from([]),
            paths: Arc::from([]),
            paints: Arc::from([]),
            stroke_styles: Arc::from([]),
            glyph_runs: Arc::from([]),
            fonts: Arc::from([]),
            images: Arc::from([]),
            masks: Arc::from([]),
            groups: Arc::from([]),
            shadings: Arc::from([]),
            tilings: Arc::from([]),
            features: PageFeatures::empty(),
            complexity: PageComplexity::default(),
        }
    }
}

/// Deterministic number formatting for the debug dump: integers without a
/// decimal point, otherwise the shortest round-tripping form; `-0.0` → `0`.
fn g(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    format!("{v}")
}

fn dump_table<T>(o: &mut String, name: &str, items: &[T], fmt: impl Fn(&T) -> String) {
    use std::fmt::Write as _;
    if items.is_empty() {
        return;
    }
    let _ = writeln!(o, "{name}:");
    for (i, item) in items.iter().enumerate() {
        let _ = writeln!(o, "  #{i} {}", fmt(item));
    }
}

fn fmt_matrix(m: &Matrix) -> String {
    format!(
        "[{} {} {} {} {} {}]",
        g(m.a),
        g(m.b),
        g(m.c),
        g(m.d),
        g(m.e),
        g(m.f)
    )
}

fn fmt_paint(p: &Paint) -> String {
    match p {
        Paint::Solid(c) => format!(
            "solid rgba({}, {}, {}, {})",
            g(c.r as f64),
            g(c.g as f64),
            g(c.b as f64),
            g(c.a as f64)
        ),
        Paint::Pattern { tiling, .. } => format!("tiling#{}", tiling.0),
        Paint::Shading { shading, .. } => format!("shading#{}", shading.0),
    }
}

fn fmt_op(op: &DisplayOp) -> String {
    match op {
        DisplayOp::Save => "save".into(),
        DisplayOp::Restore => "restore".into(),
        DisplayOp::ConcatTransform(m) => format!("concat {}", fmt_matrix(m)),
        DisplayOp::PushClip { path, rule } => format!("push-clip path#{} {:?}", path.0, rule),
        DisplayOp::PushClipText { runs } => {
            let ids: Vec<String> = runs.iter().map(|r| format!("run#{}", r.0)).collect();
            format!("push-clip-text [{}]", ids.join(" "))
        }
        DisplayOp::PopClip => "pop-clip".into(),
        DisplayOp::FillPath {
            path,
            paint,
            rule,
            alpha,
            blend,
        } => format!(
            "fill path#{} paint#{} {:?} alpha {} {:?}",
            path.0,
            paint.0,
            rule,
            g(*alpha as f64),
            blend
        ),
        DisplayOp::StrokePath {
            path,
            paint,
            style,
            alpha,
            blend,
        } => format!(
            "stroke path#{} paint#{} style#{} alpha {} {:?}",
            path.0,
            paint.0,
            style.0,
            g(*alpha as f64),
            blend
        ),
        DisplayOp::DrawGlyphRun {
            run,
            paint,
            alpha,
            blend,
            stroke,
        } => {
            let s = match stroke {
                Some(gs) => format!(
                    " stroke(paint#{} style#{} alpha {})",
                    gs.paint.0,
                    gs.style.0,
                    g(gs.alpha as f64)
                ),
                None => String::new(),
            };
            format!(
                "draw-glyph-run run#{} paint#{} alpha {} {:?}{s}",
                run.0,
                paint.0,
                g(*alpha as f64),
                blend
            )
        }
        DisplayOp::DrawImage {
            image,
            paint,
            transform,
            alpha,
            blend,
        } => format!(
            "draw-image image#{} paint#{} {} alpha {} {:?}",
            image.0,
            paint.0,
            fmt_matrix(transform),
            g(*alpha as f64),
            blend
        ),
        DisplayOp::BeginTransparencyGroup { group } => format!("begin-group group#{}", group.0),
        DisplayOp::EndTransparencyGroup => "end-group".into(),
        DisplayOp::ApplySoftMask { mask } => match mask {
            Some(m) => format!("apply-soft-mask mask#{}", m.0),
            None => "apply-soft-mask none".into(),
        },
        DisplayOp::DrawShading { shading, transform } => {
            format!(
                "draw-shading shading#{} {}",
                shading.0,
                fmt_matrix(transform)
            )
        }
        DisplayOp::BeginPaintOrigin(o) => format!("begin-paint-origin {}", o.name()),
        DisplayOp::EndPaintOrigin => "end-paint-origin".into(),
        DisplayOp::BeginSoftMask { kind, transfer } => {
            if transfer.is_some() {
                format!("begin-soft-mask {kind:?} transfer=lut256")
            } else {
                format!("begin-soft-mask {kind:?}")
            }
        }
        DisplayOp::EndSoftMask => "end-soft-mask".into(),
        DisplayOp::ClearSoftMask => "clear-soft-mask".into(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const fn assert_send_sync<T: Send + Sync>() {}
    const _: () = assert_send_sync::<CompiledPage>();

    #[test]
    fn empty_page_is_valid() {
        let page = CompiledPage::empty(PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 612.0,
                y1: 792.0,
            },
            rotate: 0,
        });
        assert_eq!(page.schema_version, IR_SCHEMA_VERSION);
        assert!(page.operations.is_empty());
        assert!(page.features.is_empty());
    }

    #[test]
    fn shared_bytes_preserves_the_vec_data_allocation() {
        let bytes = vec![1, 2, 3, 4, 5];
        let allocation = bytes.as_ptr();

        let shared = SharedBytes::from(bytes);

        assert_eq!(shared.as_ptr(), allocation);
        assert_eq!(shared.as_slice(), &[1, 2, 3, 4, 5]);
        assert_eq!(shared.clone().as_ptr(), allocation);
    }
}
