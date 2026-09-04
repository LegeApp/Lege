//! Raster text → per-document glyph font (the `truetyping` text format).
//!
//! A printed book sets its text in a handful of typefaces, so the connected
//! components of its binarized pages cluster into a modest set of glyph
//! shapes. This module keeps one document-wide [`GlyphDictionary`] of those
//! shapes, matches every page's components against it, and records where each
//! glyph occurs. At the end of the document the dictionary becomes an embedded
//! TrueType program and each page's text becomes a PDF text object that draws
//! it — instead of a JBIG2 or CCITT raster.
//!
//! Glyph ids are shapes. When OCR ran, each page's recognized words are
//! aligned with its glyph placements ([`GlyphDictionary::record_text`]) and
//! the font gets a ToUnicode CMap from the resulting votes, so the visible
//! text is also the searchable text and no invisible OCR layer is needed.
//!
//! Prototypes are majority votes over every instance matched to them, so
//! scanning noise averages out; matching tolerates edge noise but rejects
//! structural differences (a missing crossbar, serif or dot). Outlines trace
//! the pixel edges of the prototype (a valid TrueType "staircase" outline
//! that renders bit-exact at source resolution); a curve-fitting vectorizer
//! is the next stage and nothing downstream of [`GlyphOutline`] has to change
//! for it.
//!
//! The clustering kernel is jbig2enc-rust's: `analyze_page` for components and
//! the SIMD alignment [`Comparator`] for similarity, the same code that drives
//! JBIG2 symbol mode.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use jbig2enc_rust::jbig2cc::{BBox, analyze_page};
use jbig2enc_rust::jbig2comparator::Comparator;
use jbig2enc_rust::jbig2sym::BitImage;
use lege_pdf_write::font::{EmbeddedFont, ToUnicode, to_unicode_cmap};

use crate::encoding::bitrows::{self, BitRows, Diff};
use crate::encoding::straighten::{PageFrame, detect_frame, fit_baseline, page_bitmap};
use crate::encoding::vectorize::vectorize;
use crate::truetype_writer::{
    GlyphComponent, GlyphOutline, OutlinePoint, TrueTypeSpec, build_truetype,
};

/// Font units per source pixel. With 1000 units per em this makes one em
/// 62.5 source pixels; a line's text matrix scale is therefore
/// `EM_PIXELS × (points per pixel)`.
///
/// The em is about twice the real type size of body text on a 2400-pixel
/// page, which keeps text extractors' font-size-relative heuristics sane:
/// poppler treats a glyph starting within 0.1 em of the previous one as
/// overprinted duplicate text and breaks the word there, and breaks words
/// at gaps over 0.1 em. At 62.5 pixels per em those thresholds are 6 pixels,
/// above any glyph advance and below any word space.
pub const UNITS_PER_PIXEL: i32 = 16;
pub const UNITS_PER_EM: u16 = 1000;
/// Source pixels per em (`UNITS_PER_EM / UNITS_PER_PIXEL`).
pub const EM_PIXELS: f64 = UNITS_PER_EM as f64 / UNITS_PER_PIXEL as f64;
/// Largest glyph dimension (pixels) whose outline coordinates fit `i16`.
pub const MAX_GLYPH_PIXELS: usize = (i16::MAX as usize) / (UNITS_PER_PIXEL as usize);
/// Glyph ids are 16-bit CIDs; id 0 is `.notdef`.
pub const MAX_GLYPHS: usize = u16::MAX as usize;
/// Prototypes one dictionary may hold: what is left of a font's glyph ids
/// once `.notdef` and the word space are taken. A document that needs more
/// starts another dictionary, and its later pages draw from another font.
pub const MAX_PROTOTYPES: usize = MAX_GLYPHS - FIRST_SHAPE_GID as usize;
/// Glyph id of the blank word-space glyph.
pub const SPACE_GID: u16 = 1;
/// Advance of the space glyph, font units (a quarter em).
pub const SPACE_ADVANCE: u16 = 250;
/// Glyph id of dictionary prototype 0; prototype `i` is `i + FIRST_SHAPE_GID`.
pub const FIRST_SHAPE_GID: u32 = 2;

/// PostScript name of the embedded glyph font.
pub const FONT_NAME: &str = "LegeGlyphs";

/// Width/height slack (pixels) when looking for a matching prototype; grows
/// by one per 16 pixels of glyph size. Narrowing it does not make matching
/// safer: the search keeps the best candidate it is offered, so excluding the
/// right prototype only hands the instance to the next best wrong one.
const DIM_LIMIT: u32 = 2;
/// Alignment search radius (pixels) passed to the comparator.
const SHIFT_LIMIT: i32 = 2;
/// An occurrence whose baseline offset differs from its prototype's becomes
/// a *position variant*: a composite glyph of the same shape at that exact
/// offset. Every occurrence then sits where it was scanned while the shared
/// outline remains deduplicated. The variant also avoids text rise, which
/// keeps each line one string and keeps text extractors from breaking lines
/// at rise changes.
///
/// Variants whose offset differs from the root's by more than this many
/// pixels (or half the frame height, whichever is larger) are a different
/// *character position* — the tittle of an `i` rather than a full stop, an
/// apostrophe rather than a comma — and keep their own text votes.
const SEMANTIC_RISE_MIN_PX: i32 = 4;
/// Vote weight for a word whose glyph cells and characters match one to one.
/// Loose alignments mislabel neighbours when OCR and segmentation disagree
/// on where a word's pieces are, and a rare letter's few votes must not be
/// outvoted by them.
const EXACT_ALIGNMENT_WEIGHT: u32 = 4;
/// Vote weight when characters had to be shared out by position.
const LOOSE_ALIGNMENT_WEIGHT: u32 = 1;
/// Gaps beyond twice the page's typical letter gap plus this are word spaces.
const LETTER_GAP_SLACK_PX: i32 = 3;

/// The baseline offset beyond which a variant is a different character
/// position rather than the same character wobbling.
fn semantic_rise_limit(frame_h: usize) -> i32 {
    (frame_h as i32 / 2).max(SEMANTIC_RISE_MIN_PX)
}

/// One recognized word on a page, in the page's pixel space (y down).
#[derive(Clone, Debug, PartialEq)]
pub struct TextWord {
    pub text: String,
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// A placed glyph's pixel box on a page, for text alignment.
#[derive(Clone, Copy, Debug)]
struct GlyphBox {
    glyph: u32,
    x0: i32,
    x1: i32,
    top: i32,
    bottom: i32,
    /// Frame area: the largest glyph of a cell carries the cell's text.
    area: u32,
    claimed: bool,
}

impl GlyphBox {
    fn centre(&self) -> (f32, f32) {
        (
            (self.x0 + self.x1) as f32 / 2.0,
            (self.top + self.bottom) as f32 / 2.0,
        )
    }
}

/// A word's characters as the units glyphs are matched to: whitespace
/// dropped, combining marks folded into the character before them.
fn word_characters(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        let combining = matches!(
            ch as u32,
            0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
        );
        match out.last_mut() {
            Some(prev) if combining => prev.push(ch),
            _ => out.push(ch.to_string()),
        }
    }
    out
}

/// One glyph occurrence on a page, in page pixel coordinates (y down).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphInstance {
    /// Index into the dictionary (glyph id = index + `FIRST_SHAPE_GID`).
    pub glyph: u32,
    /// Top-left of the placed prototype bitmap.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// A glyph placed on a text line: horizontal position plus the vertical
/// correction against the prototype's own baseline offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacedGlyph {
    pub glyph: u32,
    /// Left edge, page pixels.
    pub x: i32,
    /// The glyph's advance, page pixels: its frame width plus the typical
    /// gap to the next glyph.
    pub width: u32,
    /// Pixels to raise this occurrence relative to where the font's baseline
    /// would put it (negative lowers it). Zero for a consistent glyph.
    pub rise_px: i32,
}

/// One text line: its baseline (page pixel row, y down) and the glyphs on it
/// in left-to-right order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GlyphLine {
    pub baseline_y: i32,
    pub glyphs: Vec<PlacedGlyph>,
}

/// A page's components and its frame, computed without the dictionary.
pub struct PageAnalysis {
    shapes: Vec<(BitImage, BBox)>,
    frame: PageFrame,
    components: Duration,
    frame_time: Duration,
    /// Median component size (the larger of width and height) in pixels: how
    /// big this page's type is in the raster the outlines are traced from.
    median_size: u32,
}

impl PageAnalysis {
    /// Components found on the page, an upper bound on the prototypes it can
    /// add to a dictionary.
    pub fn shape_count(&self) -> usize {
        self.shapes.len()
    }

    /// Segment a binarized page (1 = ink) into components and find which way
    /// its text reads and how far its lines are off level.
    pub fn of(page: &BitImage, dpi: i32) -> Self {
        let started = Instant::now();
        // losslevel 1: erase isolated specks of at most `dpi²/20000 − 1`
        // pixels (one pixel at ~220 dpi). Everything else becomes a glyph.
        let cc = analyze_page(page, dpi.max(72), 1);
        let shapes = cc.extract_shapes();
        drop(cc);
        let components = started.elapsed();
        // Components are turned upright (losslessly) and their positions
        // levelled against this frame, so every line is horizontal.
        let started = Instant::now();
        let frame = detect_frame(&shapes, page.width as u32, page.height as u32);
        let frame_time = started.elapsed();
        let mut sizes: Vec<u32> = shapes
            .iter()
            .map(|(_, b)| (b.xmax - b.xmin).max(b.ymax - b.ymin).max(0) as u32)
            .collect();
        sizes.sort_unstable();
        let median_size = sizes.get(sizes.len() / 2).copied().unwrap_or(0);
        Self {
            shapes,
            frame,
            components,
            frame_time,
            median_size,
        }
    }

    /// How tall this page's type is, in the pixels the outlines come from.
    /// Zero on a page with no components.
    pub fn median_size(&self) -> u32 {
        self.median_size
    }
}

/// A page's text as glyph placements, ready for the PDF artifact adapter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageGlyphRuns {
    /// The upright, straightened frame the lines are placed in.
    pub frame: PageFrame,
    pub lines: Vec<GlyphLine>,
    /// Total glyph occurrences on the page.
    pub glyph_count: usize,
    /// Which dictionary these glyph ids index. One TrueType font holds
    /// 65536 ids, so a document with more shapes than that keeps several
    /// dictionaries and a page draws from exactly one.
    pub bank: u16,
}

impl PageGlyphRuns {
    pub fn is_empty(&self) -> bool {
        self.glyph_count == 0
    }
}

/// A dictionary entry.
///
/// Every prototype has a fixed *frame*: the bitmap rectangle of its first
/// instance. Placements on pages, the glyph's advance and its baseline
/// offset are all expressed against that frame, so the shape inside it can
/// keep improving while pages are already written. The shape is a vote map:
/// every matched instance, aligned by the comparator, adds one vote to each
/// of its ink pixels; the prototype's bitmap is the majority of those votes.
/// The map is padded by [`FRAME_PAD`] on each side so instances that are a
/// little larger or shifted still land in it.
#[derive(Clone, Debug)]
struct Prototype {
    frame_w: usize,
    frame_h: usize,
    /// Ink votes over the padded frame, row-major,
    /// `(frame_w + 2·PAD) × (frame_h + 2·PAD)`; frame origin at `(PAD, PAD)`.
    votes: Vec<u16>,
    /// Instances folded into `votes` (saturates with `u16` votes).
    uses: u32,
    /// The bitmap new instances are compared against: the current majority
    /// shape, cropped to its ink, plus its top-left in frame coordinates.
    matcher: BitImage,
    matcher_offset: (i32, i32),
    /// Ink pixels in `matcher`.
    black: u32,
    /// `matcher`'s ink per row and column, the cheap pre-check.
    profile: InkProfile,
    /// Whether `matcher`'s strokes have an interior (tolerant gate eligibility).
    stroked: bool,
    /// Counters `matcher` encloses (see [`holes`]). The tolerant gate only
    /// folds shapes that enclose the same number.
    counters: u32,
    /// Pixels the frame's bottom edge sits below the text baseline
    /// (positive = descender): the lower median over the glyph's instances on
    /// the page that created it, fixed once that page is placed.
    descent: Option<i32>,
    /// Advance in pixels: the frame width plus the median gap to the next
    /// glyph on the same line, fixed with `descent`. Later occurrences then
    /// need no `TJ` adjustment.
    advance: Option<u32>,
    /// `uses` at which the matcher is next rebuilt from the votes.
    next_refresh: u32,
    /// Set when this prototype turned out to be another one's shape: the
    /// target index and where the target's frame origin sits in this frame.
    /// The glyph then renders as a composite of the target; its own frame,
    /// advance and descent stay as pages already reference them.
    alias: Option<(u32, i32, i32)>,
    /// Non-empty for a *compound*: pieces stacked on one another within a
    /// line (the stem and dot of an `i`, a letter and its accent) that pages
    /// place as one glyph. Each entry is a part prototype and where its
    /// frame origin sits in this frame. Renders as a composite of the parts.
    parts: Vec<(u32, i32, i32)>,
}

/// Padding around a prototype frame in its vote map. Must cover the largest
/// size difference plus alignment shift a matching instance can have.
const FRAME_PAD: usize = 6;
impl Prototype {
    fn new(bitmap: BitImage) -> Self {
        let frame_w = bitmap.width;
        let frame_h = bitmap.height;
        let pw = frame_w + 2 * FRAME_PAD;
        let ph = frame_h + 2 * FRAME_PAD;
        let mut votes = vec![0u16; pw * ph];
        for y in 0..frame_h {
            for x in 0..frame_w {
                if bitmap.get_usize(x, y) {
                    votes[(y + FRAME_PAD) * pw + x + FRAME_PAD] = 1;
                }
            }
        }
        let black = bitmap.count_ones() as u32;
        let stroked = has_stroke_interior(&bitmap);
        let counters = holes(&bitmap);
        let profile = InkProfile::of(&bitmap);
        Self {
            frame_w,
            frame_h,
            votes,
            uses: 1,
            matcher: bitmap,
            matcher_offset: (0, 0),
            black,
            profile,
            stroked,
            counters,
            descent: None,
            advance: None,
            next_refresh: 3,
            alias: None,
            parts: Vec::new(),
        }
    }

    /// The same shape as `root` (prototype `root_idx`) sitting `descent`
    /// pixels below the baseline instead of where the root sits. Renders as
    /// a composite of the root and is never matched against.
    fn position_variant(root: &Prototype, root_idx: u32, descent: i32) -> Self {
        Self {
            frame_w: root.frame_w,
            frame_h: root.frame_h,
            votes: Vec::new(),
            uses: 1,
            matcher: BitImage::new(1, 1).expect("a 1×1 BitImage"),
            matcher_offset: (0, 0),
            black: 0,
            profile: InkProfile::default(),
            stroked: false,
            counters: 0,
            descent: Some(descent),
            advance: root.advance,
            next_refresh: u32::MAX,
            alias: Some((root_idx, 0, 0)),
            parts: Vec::new(),
        }
    }

    /// A compound of `parts` (prototype, frame origin in the compound's
    /// frame) with the given frame size. Never matched against.
    fn compound(frame_w: usize, frame_h: usize, parts: Vec<(u32, i32, i32)>) -> Self {
        Self {
            frame_w,
            frame_h,
            votes: Vec::new(),
            uses: 1,
            matcher: BitImage::new(1, 1).expect("a 1×1 BitImage"),
            matcher_offset: (0, 0),
            black: 0,
            profile: InkProfile::default(),
            stroked: false,
            counters: 0,
            descent: None,
            advance: None,
            next_refresh: u32::MAX,
            alias: None,
            parts,
        }
    }

    /// Owns an outline: neither an alias nor a compound.
    fn is_simple(&self) -> bool {
        self.alias.is_none() && self.parts.is_empty()
    }

    fn padded_width(&self) -> usize {
        self.frame_w + 2 * FRAME_PAD
    }

    fn padded_height(&self) -> usize {
        self.frame_h + 2 * FRAME_PAD
    }

    /// Fold an instance in. `origin` is the instance bitmap's top-left in
    /// frame coordinates; pixels outside the padded frame are dropped.
    fn accumulate(&mut self, instance: &BitImage, origin: (i32, i32)) {
        if self.uses >= u16::MAX as u32 {
            return;
        }
        self.uses += 1;
        let pw = self.padded_width() as i32;
        let ph = self.padded_height() as i32;
        let stride = instance.width.div_ceil(32);
        let words = instance.packed_words();
        for y in 0..instance.height {
            let fy = y as i32 + origin.1 + FRAME_PAD as i32;
            if fy < 0 || fy >= ph {
                continue;
            }
            let row = &mut self.votes[(fy * pw) as usize..((fy + 1) * pw) as usize];
            for (i, &word) in words[y * stride..(y + 1) * stride].iter().enumerate() {
                let mut v = word;
                while v != 0 {
                    // Most significant bit first: bit 31 is pixel 0.
                    let k = v.leading_zeros();
                    v &= !(1u32 << (31 - k));
                    let fx = (i * 32) as i32 + k as i32 + origin.0 + FRAME_PAD as i32;
                    if fx >= 0 && fx < pw {
                        row[fx as usize] += 1;
                    }
                }
            }
        }
    }

    /// The majority shape over the padded frame: a pixel is ink when at least
    /// half of the folded instances had ink there.
    fn majority(&self) -> BitImage {
        let pw = self.padded_width();
        let ph = self.padded_height();
        let mut out = BitImage::new(pw as u32, ph as u32).expect("padded frame fits a BitImage");
        let need = self.uses.div_ceil(2) as u16;
        for y in 0..ph {
            for x in 0..pw {
                if self.votes[y * pw + x] >= need {
                    out.set_usize(x, y, true);
                }
            }
        }
        out
    }

    /// Rebuild the matcher from the votes. Returns `true` when its size
    /// changed (so the bucket index must be updated).
    fn refresh_matcher(&mut self) -> bool {
        let shape = self.majority();
        let (Some(bounds), pw) = (ink_bounds(&shape), self.padded_width()) else {
            // A glyph whose votes cancel out entirely cannot happen: its own
            // first instance is always at least half the votes until uses > 2,
            // and refreshes only happen at odd counts ≥ 3 where a majority
            // exists. Keep the current matcher regardless.
            return false;
        };
        let _ = pw;
        let (x0, y0, x1, y1) = bounds;
        let w = x1 - x0 + 1;
        let h = y1 - y0 + 1;
        let mut cropped = BitImage::new(w as u32, h as u32).expect("crop fits a BitImage");
        for y in 0..h {
            for x in 0..w {
                if shape.get_usize(x0 + x, y0 + y) {
                    cropped.set_usize(x, y, true);
                }
            }
        }
        let resized = cropped.width != self.matcher.width || cropped.height != self.matcher.height;
        self.black = cropped.count_ones() as u32;
        self.stroked = has_stroke_interior(&cropped);
        self.counters = holes(&cropped);
        self.profile = InkProfile::of(&cropped);
        self.matcher = cropped;
        self.matcher_offset = (x0 as i32 - FRAME_PAD as i32, y0 as i32 - FRAME_PAD as i32);
        resized
    }
}

/// Sorts and returns the lower median, or `None` when empty.
fn lower_median(values: &mut [i32]) -> Option<i32> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[(values.len() - 1) / 2])
}

/// Bounding box `(x0, y0, x1, y1)` of a bitmap's ink, inclusive.
fn ink_bounds(bitmap: &BitImage) -> Option<(usize, usize, usize, usize)> {
    let mut bounds: Option<(usize, usize, usize, usize)> = None;
    for y in 0..bitmap.height {
        for x in 0..bitmap.width {
            if bitmap.get_usize(x, y) {
                bounds = Some(match bounds {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
    }
    bounds
}

/// Whether a shape's strokes are thick enough for the tolerant gate; see
/// `TOLERANT_INTERIOR_MIN_PERCENT`.
fn has_stroke_interior(bitmap: &BitImage) -> bool {
    let rows = BitRows::padded(bitmap);
    rows.interior_count() * 100 >= rows.count() * TOLERANT_INTERIOR_MIN_PERCENT
}

/// How many counters a shape encloses: background regions (4-connected,
/// so a diagonal pinch closes them) that the border cannot reach. `c` has
/// none, `e` one, `g` two. A letter's counters are its identity as much as
/// its outline is, and unlike its outline they survive a change of weight.
fn holes(bitmap: &BitImage) -> u32 {
    let (w, h) = (bitmap.width + 2, bitmap.height + 2);
    if bitmap.width == 0 || bitmap.height == 0 {
        return 0;
    }
    // 0 = background, not yet reached; 1 = ink; 2 = reached from outside.
    let mut cell = vec![0u8; w * h];
    for y in 0..bitmap.height {
        for x in 0..bitmap.width {
            if bitmap.get_usize(x, y) {
                cell[(y + 1) * w + x + 1] = 1;
            }
        }
    }
    let mut stack = vec![0usize];
    cell[0] = 2;
    while let Some(i) = stack.pop() {
        let (x, y) = (i % w, i / w);
        let visit = |x: usize, y: usize, cell: &mut Vec<u8>, stack: &mut Vec<usize>| {
            let j = y * w + x;
            if cell[j] == 0 {
                cell[j] = 2;
                stack.push(j);
            }
        };
        if x > 0 {
            visit(x - 1, y, &mut cell, &mut stack);
        }
        if x + 1 < w {
            visit(x + 1, y, &mut cell, &mut stack);
        }
        if y > 0 {
            visit(x, y - 1, &mut cell, &mut stack);
        }
        if y + 1 < h {
            visit(x, y + 1, &mut cell, &mut stack);
        }
    }
    let mut counters = 0;
    for start in 0..w * h {
        if cell[start] != 0 {
            continue;
        }
        counters += 1;
        cell[start] = 2;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            let visit = |x: usize, y: usize, cell: &mut Vec<u8>, stack: &mut Vec<usize>| {
                let j = y * w + x;
                if cell[j] == 0 {
                    cell[j] = 2;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(x - 1, y, &mut cell, &mut stack);
            }
            if x + 1 < w {
                visit(x + 1, y, &mut cell, &mut stack);
            }
            if y > 0 {
                visit(x, y - 1, &mut cell, &mut stack);
            }
            if y + 1 < h {
                visit(x, y + 1, &mut cell, &mut stack);
            }
        }
    }
    counters
}

/// Ink per row and per column of a shape: a lower bound on how much two
/// shapes differ that costs a few dozen subtractions, see
/// [`profile_distance`].
#[derive(Clone, Debug, Default)]
struct InkProfile {
    rows: Vec<u16>,
    cols: Vec<u16>,
}

impl InkProfile {
    fn of(bitmap: &BitImage) -> Self {
        let mut rows = vec![0u16; bitmap.height];
        let mut cols = vec![0u16; bitmap.width];
        let stride = bitmap.width.div_ceil(32);
        let words = bitmap.packed_words();
        for (y, row) in words.chunks(stride).enumerate().take(bitmap.height) {
            for (i, &word) in row.iter().enumerate() {
                let mut v = word;
                rows[y] += v.count_ones() as u16;
                while v != 0 {
                    // Most significant bit first: bit 31 is pixel 0.
                    let k = v.leading_zeros() as usize;
                    cols[i * 32 + k] += 1;
                    v &= !(1u32 << (31 - k));
                }
            }
        }
        Self { rows, cols }
    }

    /// The smallest, over the alignments the matcher may choose, of the
    /// row-profile and column-profile distances to `other`: neither can
    /// exceed the pixels the two shapes differ by once aligned, so a
    /// candidate whose bound is over the gate's limit needs no comparison.
    /// Stops early past `limit`.
    fn distance_bound(&self, other: &Self, limit: u32) -> u32 {
        let rows = profile_distance(&self.rows, &other.rows, SHIFT_LIMIT, limit);
        if rows > limit {
            return rows;
        }
        rows.max(profile_distance(
            &self.cols,
            &other.cols,
            SHIFT_LIMIT,
            limit,
        ))
    }
}

/// The smallest L1 distance between two ink profiles with `b` shifted by
/// up to `max_shift` against `a`, entries outside either profile counting
/// in full. Returns as soon as a shift is within `limit` (the bound can
/// then not reject), so the result is only exact above `limit`.
fn profile_distance(a: &[u16], b: &[u16], max_shift: i32, limit: u32) -> u32 {
    let sum = |v: &[u16]| v.iter().map(|&x| x as u32).sum::<u32>();
    let mut best = u32::MAX;
    for shift in -max_shift..=max_shift {
        // `b[i]` sits under `a[i + shift]`; the overlap in `a`'s index space.
        let lo = shift.max(0);
        let hi = (a.len() as i32).min(b.len() as i32 + shift);
        let d = if hi <= lo {
            sum(a) + sum(b)
        } else {
            let (lo, hi) = (lo as usize, hi as usize);
            let bo = (lo as i32 - shift) as usize;
            let n = hi - lo;
            // The tight loop autovectorizes; the ends are short.
            let overlap: u32 = a[lo..hi]
                .iter()
                .zip(&b[bo..bo + n])
                .map(|(&x, &y)| x.abs_diff(y) as u32)
                .sum();
            overlap + sum(&a[..lo]) + sum(&a[hi..]) + sum(&b[..bo]) + sum(&b[bo + n..])
        };
        best = best.min(d);
        if best <= limit {
            return best;
        }
    }
    best
}

/// Matching tolerances, all relative to the ink of the larger of the two
/// shapes (area would let thin glyphs like `l` and `.` absorb anything).
///
/// Total difference: scanning noise perturbs stroke edges, and small glyphs
/// are mostly edge, so the budget is generous. Thick difference: the part of
/// the budget that may be structural, kept near zero. Far difference: ink
/// that the other shape has nothing near, which is what a whole stroke looks
/// like however thin it is.
const TOTAL_DIFF_PERCENT: u32 = 22;
const TOTAL_DIFF_MIN: u32 = 3;
const THICK_DIFF_DIVISOR: u32 = 40;
const THICK_DIFF_MIN: u32 = 1;
/// Far pixels the strict gate allows: ink with none of the other shape's
/// within a pixel of it. A change of impression puts its difference along
/// the strokes it thickens, so it is never far; a stroke one shape has and
/// the other lacks is far along its length. Two shapes that agree everywhere
/// have none, and a percent covers a broken edge on a large glyph.
const STRICT_FAR_PERCENT: u32 = 1;
const STRICT_FAR_MIN: u32 = 2;
/// The largest group of far pixels the strict gate allows. A hairline bar —
/// the crossing of an `e`, the foot of a `u` against an `n` — is a connected
/// run this long even where it is one pixel thick, which no count of 2×2
/// blocks can see.
const STRICT_FAR_BLOB_MAX: u32 = 2;
/// Ink-count pre-filter: shapes whose ink differs by more than this fraction
/// are not compared (bold against regular, mostly).
const BLACK_DIFF_PERCENT: u32 = 20;

/// The tolerant gate. A printing press has one piece of type per letter,
/// so a book's text is a fixed set of shapes inked a little heavier or
/// lighter on every impression; the strict gate above keeps those apart
/// (a one-pixel change of stroke weight is a large fraction of a small
/// glyph's ink). The tolerant gate forgives ink within a pixel of the
/// other shape and judges only what is left, and how it clusters.
///
/// Ink may differ by this much (a heavier impression; bold *type* is
/// heavier still and stays apart).
const TOLERANT_BLACK_DIFF_PERCENT: u32 = 30;
/// Total difference sanity bound.
const TOLERANT_TOTAL_DIFF_PERCENT: u32 = 60;
/// Far pixels allowed in all (specks and breaks), relative to ink.
const TOLERANT_FAR_PERCENT: u32 = 6;
const TOLERANT_FAR_MIN: u32 = 3;
/// Largest group of far pixels allowed: anything bigger is a part of a
/// letter that one shape has and the other lacks.
const TOLERANT_FAR_BLOB_MAX: u32 = 3;
/// The tolerant gate needs strokes to have an interior: the ink pixels
/// whose four neighbours are all ink must be at least this share of the
/// ink. On type so small that strokes are one or two pixels wide, a pixel
/// of forgiveness swallows the letter's structure (an `s` against an `a`,
/// a `c` against an `o`), so such shapes only ever match strictly.
const TOLERANT_INTERIOR_MIN_PERCENT: u32 = 20;
/// Prototypes with at least this many instances are established enough
/// (their majority shape is clean) for the tolerant gate to match against
/// while pages are processed; the rest wait for the end-of-document pass.
const ESTABLISHED_USES: u32 = 4;

/// Which matching tolerance a search uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// Edge noise only: the shape must agree everywhere.
    Strict,
    /// A one-pixel change of stroke weight is forgiven; structure must agree.
    Tolerant,
}

impl Gate {
    /// The differing pixels the gate allows between shapes with `ink`
    /// (the larger count).
    fn total_limit(self, ink: u32) -> u32 {
        match self {
            Gate::Strict => (ink * TOTAL_DIFF_PERCENT / 100).max(TOTAL_DIFF_MIN),
            Gate::Tolerant => ink * TOLERANT_TOTAL_DIFF_PERCENT / 100,
        }
    }

    /// The far pixels the gate allows, or `None` when it does not look at
    /// them (the strict gate).
    fn far_limit(self, ink: u32) -> Option<u32> {
        match self {
            Gate::Strict => Some((ink * STRICT_FAR_PERCENT / 100).max(STRICT_FAR_MIN)),
            Gate::Tolerant => Some((ink * TOLERANT_FAR_PERCENT / 100).max(TOLERANT_FAR_MIN)),
        }
    }

    /// Whether a difference is worth measuring the far pixels of: the tests
    /// that cost nothing beyond the exclusive-or already done. The far
    /// measures need two dilations and a blob search, so they are only paid
    /// for by a candidate that has passed everything else.
    fn accept_coarse(self, diff: Diff, ink: u32) -> bool {
        if diff.total > self.total_limit(ink) {
            return false;
        }
        match self {
            Gate::Strict => diff.thick <= (ink / THICK_DIFF_DIVISOR).max(THICK_DIFF_MIN),
            Gate::Tolerant => true,
        }
    }

    /// Whether `diff` between two shapes with `ink` (the larger count)
    /// passes, and the key that ranks passing matches (smaller is better).
    fn accept(self, diff: Diff, ink: u32) -> Option<(u32, u32, u32)> {
        if diff.total > self.total_limit(ink) {
            return None;
        }
        match self {
            Gate::Strict => {
                let thick_limit = (ink / THICK_DIFF_DIVISOR).max(THICK_DIFF_MIN);
                // Ranked by structure first: a candidate that agrees
                // everywhere beats one that is merely close, so a rejection
                // never hands the instance to a worse match that survived.
                (diff.thick <= thick_limit && diff.far_blob <= STRICT_FAR_BLOB_MAX).then_some((
                    diff.far_blob,
                    diff.thick,
                    diff.total,
                ))
            }
            Gate::Tolerant => {
                let far_limit = self.far_limit(ink).unwrap_or(0);
                (diff.far <= far_limit && diff.far_blob <= TOLERANT_FAR_BLOB_MAX).then_some((
                    diff.far_blob,
                    diff.far,
                    diff.total,
                ))
            }
        }
    }

    fn ink_diff_percent(self) -> u32 {
        match self {
            Gate::Strict => BLACK_DIFF_PERCENT,
            Gate::Tolerant => TOLERANT_BLACK_DIFF_PERCENT,
        }
    }

    /// The comparator's own error budget while it finds the alignment; its
    /// word-wise count is loose, and the gate proper decides.
    fn comparator_budget(self, ink: u32) -> u32 {
        2 * self.total_limit(ink) + 4
    }
}

/// Where the dictionary's time goes, for the diagnostic dump.
#[derive(Clone, Copy, Debug, Default)]
struct Profile {
    /// Component analysis of the pages.
    components: Duration,
    /// Orientation and skew detection.
    frame: Duration,
    /// Strict matching of instances against prototypes.
    strict: Duration,
    /// Tolerant matching (instances that missed strictly).
    tolerant: Duration,
    /// Folding matched instances in: votes, matcher refreshes, aliasing.
    fold: Duration,
    /// Line grouping, stacking, metrics.
    lines: Duration,
    /// The end-of-document pass.
    finalize: Duration,
    /// Outline fitting for the font.
    outlines: Duration,
    /// Prototypes compared (comparator run) per gate.
    strict_compared: u64,
    tolerant_compared: u64,
    /// Bucket candidates looked at per gate.
    strict_candidates: u64,
    tolerant_candidates: u64,
    /// Instances matched per gate, and new prototypes.
    strict_hits: u64,
    tolerant_hits: u64,
    inserted: u64,
}

impl Profile {
    fn report(&self) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        format!(
            "components {:.0} ms, frame {:.0} ms, strict {:.0} ms ({} hits, {} compared of {} candidates), \
             tolerant {:.0} ms ({} hits, {} compared of {} candidates), fold {:.0} ms, lines {:.0} ms, \
             {} inserted; finalize {:.0} ms, outlines {:.0} ms",
            ms(self.components),
            ms(self.frame),
            ms(self.strict),
            self.strict_hits,
            self.strict_compared,
            self.strict_candidates,
            ms(self.tolerant),
            self.tolerant_hits,
            self.tolerant_compared,
            self.tolerant_candidates,
            ms(self.fold),
            ms(self.lines),
            self.inserted,
            ms(self.finalize),
            ms(self.outlines),
        )
    }
}

/// Document-wide glyph shapes. Not thread-safe by itself; see
/// [`GlyphFontSession`] for the shared form.
#[derive(Default)]
pub struct GlyphDictionary {
    profile: Profile,
    protos: Vec<Prototype>,
    /// Matcher (width, height) → `(ink, prototype index)` sorted by ink,
    /// for cheap candidate lookup: a search starts at the first admissible
    /// ink count and stops at the last, touching no other prototype.
    buckets: HashMap<(u32, u32), Vec<(u32, u32)>>,
    comparator: Comparator,
    instances: usize,
    /// Components too large for a glyph (see `MAX_GLYPH_PIXELS`), dropped.
    oversize_dropped: usize,
    /// Outline glyphs whose curve fit passed its check in the last build;
    /// the rest were emitted as staircases.
    fitted: std::cell::Cell<usize>,
    /// Prototype → its position variants (see `SEMANTIC_RISE_MIN_PX`).
    variants: HashMap<u32, Vec<u32>>,
    /// Part prototypes (in reading order) → the compounds made of them.
    compounds: HashMap<Vec<u32>, Vec<u32>>,
    /// Prototype → text it stood for, with vote counts, from `record_text`.
    text_votes: HashMap<u32, HashMap<String, u32>>,
    /// Glyph ids the last build mapped to text.
    mapped: std::cell::Cell<usize>,
    /// Outline fitting time of the last build.
    outlines: std::cell::Cell<Duration>,
    /// `(folded, into)` pairs the tolerant end-of-document pass made, for
    /// the diagnostic dump.
    tolerant_folds: Vec<(u32, u32)>,
}

impl GlyphDictionary {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.protos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.protos.is_empty()
    }

    pub fn instances(&self) -> usize {
        self.instances
    }

    pub fn oversize_dropped(&self) -> usize {
        self.oversize_dropped
    }

    /// Segment a binarized page (1 = ink) into components, match each against
    /// the dictionary (growing it as needed), and return the page's glyph
    /// placements grouped into lines.
    pub fn process_page(&mut self, page: &BitImage, dpi: i32) -> PageGlyphRuns {
        self.process_analysis(PageAnalysis::of(page, dpi))
    }

    /// Match an analysed page's components against the dictionary (growing
    /// it as needed) and return the page's glyph placements grouped into
    /// lines. The analysis needs no dictionary, so pages analyse in parallel
    /// and only this step serializes on it.
    pub fn process_analysis(&mut self, analysis: PageAnalysis) -> PageGlyphRuns {
        let PageAnalysis {
            shapes,
            frame,
            components,
            frame_time,
            median_size: _,
        } = analysis;
        self.profile.components += components;
        self.profile.frame += frame_time;

        let mut instances = Vec::with_capacity(shapes.len());
        for (bitmap, bbox) in shapes {
            let bitmap = frame.turn_bitmap(bitmap);
            let bbox = frame.turn_box(&bbox);
            if bitmap.width > MAX_GLYPH_PIXELS || bitmap.height > MAX_GLYPH_PIXELS {
                self.oversize_dropped += 1;
                continue;
            }
            let (glyph, dx, dy) = self.match_or_insert(bitmap);
            let proto = &self.protos[glyph as usize];
            // The component's bottom-left corner, levelled; its bottom is
            // what sits on the baseline.
            let (lx, ly) = frame.deskew_point(bbox.xmin as f64, bbox.ymax as f64);
            instances.push(GlyphInstance {
                glyph,
                x: lx.round() as i32 + dx,
                y: ly.round() as i32 - (bbox.ymax - bbox.ymin) + dy,
                width: proto.frame_w as u32,
                height: proto.frame_h as u32,
            });
        }
        self.instances += instances.len();

        let started = Instant::now();
        let mut lines = group_lines(instances);
        self.merge_stacked(&mut lines);
        self.fix_new_prototype_metrics(&lines);
        for line in &mut lines {
            for placed in &mut line.glyphs {
                // `group_lines` leaves each glyph's own descent (bottom −
                // baseline) in `rise_px`. The outline may be shared, but its
                // vertical placement may not: use an exact position variant
                // whenever the occurrence differs from the prototype.
                let inst_descent = placed.rise_px;
                let proto_descent = self.protos[placed.glyph as usize].descent.unwrap_or(0);
                let glyph = if proto_descent != inst_descent {
                    self.position_variant(placed.glyph, inst_descent)
                } else {
                    placed.glyph
                };
                let proto = &self.protos[glyph as usize];
                placed.glyph = glyph;
                placed.rise_px = 0;
                placed.width = proto.advance.unwrap_or(proto.frame_w as u32);
            }
        }
        let glyph_count = lines.iter().map(|l| l.glyphs.len()).sum();
        self.profile.lines += started.elapsed();
        PageGlyphRuns {
            frame,
            lines,
            bank: 0,
            glyph_count,
        }
    }

    /// Give every prototype created on this page its descent and advance,
    /// from the lower medians of what its instances on the page show. Lines
    /// arrive from `group_lines` with each glyph's own descent in `rise_px`
    /// and its frame width in `width`.
    fn fix_new_prototype_metrics(&mut self, lines: &[GlyphLine]) {
        let mut descents: HashMap<u32, Vec<i32>> = HashMap::new();
        let mut gaps: HashMap<u32, Vec<i32>> = HashMap::new();
        let mut page_gaps: Vec<i32> = Vec::new();
        for line in lines {
            for (i, g) in line.glyphs.iter().enumerate() {
                let is_new = self.protos[g.glyph as usize].descent.is_none();
                if is_new {
                    descents.entry(g.glyph).or_default().push(g.rise_px);
                }
                if let Some(next) = line.glyphs.get(i + 1) {
                    let gap = next.x - (g.x + g.width as i32);
                    if gap >= 0 {
                        page_gaps.push(gap);
                        if is_new {
                            gaps.entry(g.glyph).or_default().push(gap);
                        }
                    }
                }
            }
        }
        let page_gap = lower_median(&mut page_gaps).unwrap_or(0);
        // Word spaces must not count as letter gaps, or a letter that often
        // ends a word (`y`, `e`, `d`) would advance past the space after it.
        let letter_gap_limit = 2 * page_gap + LETTER_GAP_SLACK_PX;
        for (idx, mut ds) in descents {
            let proto = &mut self.protos[idx as usize];
            proto.descent = lower_median(&mut ds);
            let mut gs: Vec<i32> = gaps
                .remove(&idx)
                .unwrap_or_default()
                .into_iter()
                .filter(|&g| g <= letter_gap_limit)
                .collect();
            let gap = lower_median(&mut gs).unwrap_or(page_gap);
            // Sanity bound: no glyph advances by more than its own height
            // past its frame.
            let gap = gap.min(proto.frame_h as i32);
            proto.advance = Some(proto.frame_w as u32 + gap as u32);
        }
    }

    /// Replace the pieces stacked on one another within each line — glyphs
    /// whose horizontal extents overlap by at least half the narrower one —
    /// with one compound glyph. Pages then draw an `i` as one glyph with one
    /// advance, text extraction sees one character, and the pen never has
    /// to move back to draw a dot over a stem it has already passed.
    ///
    /// Lines arrive from `group_lines` with frame widths in `width` and each
    /// glyph's own descent in `rise_px`; the compound's entry is in the same
    /// convention.
    fn merge_stacked(&mut self, lines: &mut [GlyphLine]) {
        for line in lines {
            let mut merged: Vec<PlacedGlyph> = Vec::with_capacity(line.glyphs.len());
            let mut cell: Vec<PlacedGlyph> = Vec::new();
            let (mut cx0, mut cx1) = (0, 0);
            let flush =
                |cell: &mut Vec<PlacedGlyph>, merged: &mut Vec<PlacedGlyph>, dict: &mut Self| {
                    match cell.len() {
                        0 => {}
                        1 => merged.push(cell[0]),
                        _ => merged.push(dict.compound_of(cell, line.baseline_y)),
                    }
                    cell.clear();
                };
            for g in &line.glyphs {
                let x1 = g.x + g.width as i32;
                let overlap = cx1.min(x1) - cx0.max(g.x);
                if !cell.is_empty() && overlap * 2 >= (cx1 - cx0).min(g.width as i32) {
                    cx0 = cx0.min(g.x);
                    cx1 = cx1.max(x1);
                } else {
                    flush(&mut cell, &mut merged, self);
                    cx0 = g.x;
                    cx1 = x1;
                }
                cell.push(*g);
            }
            flush(&mut cell, &mut merged, self);
            line.glyphs = merged;
        }
    }

    /// The compound glyph for a stacked cell: an existing one whose parts
    /// and offsets match exactly, else a new one. The parts' outlines may be
    /// shared, but reusing a compound with different offsets would move an
    /// accent or tittle away from where it was scanned. Returns the cell's
    /// placement in `group_lines` convention.
    fn compound_of(&mut self, cell: &[PlacedGlyph], baseline_y: i32) -> PlacedGlyph {
        // Parts in reading order (top to bottom, then left to right), each
        // with its box in page pixels.
        let mut parts: Vec<(u32, i32, i32, i32, i32)> = cell
            .iter()
            .map(|g| {
                let bottom = baseline_y + g.rise_px;
                let top = bottom - self.protos[g.glyph as usize].frame_h as i32;
                (g.glyph, g.x, top, g.x + g.width as i32, bottom)
            })
            .collect();
        parts.sort_by_key(|p| (p.2, p.1));
        let x0 = parts.iter().map(|p| p.1).min().unwrap_or(0);
        let top = parts.iter().map(|p| p.2).min().unwrap_or(0);
        let x1 = parts.iter().map(|p| p.3).max().unwrap_or(0);
        let bottom = parts.iter().map(|p| p.4).max().unwrap_or(0);
        let key: Vec<u32> = parts.iter().map(|p| p.0).collect();
        let offsets: Vec<(u32, i32, i32)> =
            parts.iter().map(|p| (p.0, p.1 - x0, p.2 - top)).collect();

        let existing = self.compounds.get(&key).and_then(|list| {
            list.iter().copied().find(|&idx| {
                self.protos[idx as usize]
                    .parts
                    .iter()
                    .zip(&offsets)
                    .all(|(a, b)| a == b)
            })
        });
        let idx = match existing {
            Some(idx) => {
                self.protos[idx as usize].uses += 1;
                idx
            }
            None => {
                let idx = self.protos.len() as u32;
                self.protos.push(Prototype::compound(
                    (x1 - x0) as usize,
                    (bottom - top) as usize,
                    offsets,
                ));
                self.compounds.entry(key).or_default().push(idx);
                idx
            }
        };
        PlacedGlyph {
            glyph: idx,
            x: x0,
            width: (x1 - x0) as u32,
            rise_px: bottom - baseline_y,
        }
    }

    /// The glyph id for prototype `root`'s shape sitting `descent` pixels
    /// below the baseline: the existing variant at that exact offset, else a
    /// new one. The root owns the outline; the variant owns only placement.
    fn position_variant(&mut self, root: u32, descent: i32) -> u32 {
        let existing = self.variants.get(&root).and_then(|list| {
            list.iter()
                .copied()
                .find(|&idx| self.protos[idx as usize].descent == Some(descent))
        });
        if let Some(idx) = existing {
            self.protos[idx as usize].uses += 1;
            return idx;
        }
        let idx = self.protos.len() as u32;
        let variant = Prototype::position_variant(&self.protos[root as usize], root, descent);
        self.protos.push(variant);
        self.variants.entry(root).or_default().push(idx);
        idx
    }

    /// Align a page's recognized words with its glyph placements and record,
    /// per glyph, the text its occurrences stood for. Votes accumulate over
    /// the document; [`to_unicode_entries`](Self::to_unicode_entries) turns
    /// them into the font's ToUnicode mapping.
    ///
    /// Within a word, glyphs stacked on one another (the stem and dot of an
    /// `i`, a letter and its accent) form one *cell*. Cells and characters
    /// pair up one to one when their counts agree; otherwise characters are
    /// shared out along the word by position, so touching letters vote for
    /// both and a broken letter's pieces vote for it and for nothing. In a
    /// cell the largest glyph carries the text; the rest vote for the empty
    /// string, which keeps them out of extracted text.
    pub fn record_text(&mut self, runs: &PageGlyphRuns, words: &[TextWord]) {
        let mut boxes: Vec<GlyphBox> = runs
            .lines
            .iter()
            .flat_map(|line| line.glyphs.iter().map(move |g| (line.baseline_y, g)))
            .map(|(baseline, g)| {
                let p = &self.protos[g.glyph as usize];
                let bottom = baseline + p.descent.unwrap_or(0) - g.rise_px;
                GlyphBox {
                    glyph: g.glyph,
                    x0: g.x,
                    x1: g.x + p.frame_w as i32,
                    top: bottom - p.frame_h as i32,
                    bottom,
                    area: (p.frame_w * p.frame_h) as u32,
                    claimed: false,
                }
            })
            .collect();

        for word in words {
            let mapped = upright_word(&runs.frame, word);
            let word = &mapped;
            let chars = word_characters(&word.text);
            if chars.is_empty() || word.x1 <= word.x0 || word.y1 <= word.y0 {
                continue;
            }
            // Word boxes are tight on the ink of letters without ascenders
            // or descenders, so the vertical reach is generous; the next
            // line's glyphs are still a full line height away.
            let reach_y = (word.y1 - word.y0) * 0.5;
            let mut members: Vec<GlyphBox> = Vec::new();
            for b in boxes.iter_mut().filter(|b| !b.claimed) {
                let (cx, cy) = b.centre();
                if cx >= word.x0 - 2.0
                    && cx <= word.x1 + 2.0
                    && cy >= word.y0 - reach_y
                    && cy <= word.y1 + reach_y
                {
                    b.claimed = true;
                    members.push(*b);
                }
            }
            if members.is_empty() {
                continue;
            }
            members.sort_by_key(|b| (b.x0, b.x1));

            // Cells: runs of glyphs whose horizontal extents overlap by at
            // least half the narrower one.
            let mut cells: Vec<(i32, i32, Vec<GlyphBox>)> = Vec::new();
            for b in members {
                let joins = cells.last().is_some_and(|(cx0, cx1, _)| {
                    let overlap = cx1.min(&b.x1) - cx0.max(&b.x0);
                    overlap * 2 >= (cx1 - cx0).min(b.x1 - b.x0)
                });
                match cells.last_mut() {
                    Some((cx0, cx1, cell)) if joins => {
                        *cx0 = (*cx0).min(b.x0);
                        *cx1 = (*cx1).max(b.x1);
                        cell.push(b);
                    }
                    _ => cells.push((b.x0, b.x1, vec![b])),
                }
            }
            let n = cells.len();
            let m = chars.len();
            if n > 2 * m + 2 {
                continue;
            }

            let mut cell_text: Vec<String> = vec![String::new(); n];
            let weight = if n == m {
                for (slot, c) in cell_text.iter_mut().zip(&chars) {
                    slot.clone_from(c);
                }
                EXACT_ALIGNMENT_WEIGHT
            } else {
                let x0 = cells[0].0 as f32;
                let span = (cells[n - 1].1 as f32 - x0).max(1.0);
                let nominal = |k: usize| x0 + (k as f32 + 0.5) * span / m as f32;
                if n < m {
                    // Each character goes to the cell under it.
                    for (k, c) in chars.iter().enumerate() {
                        let cx = nominal(k);
                        let i = cells
                            .iter()
                            .position(|(cx0, cx1, _)| cx >= *cx0 as f32 && cx <= *cx1 as f32)
                            .unwrap_or_else(|| {
                                (0..n)
                                    .min_by(|&a, &b| {
                                        let da = (cells[a].0 as f32 + cells[a].1 as f32) / 2.0 - cx;
                                        let db = (cells[b].0 as f32 + cells[b].1 as f32) / 2.0 - cx;
                                        da.abs().total_cmp(&db.abs())
                                    })
                                    .unwrap_or(0)
                            });
                        cell_text[i].push_str(c);
                    }
                } else {
                    // Each cell goes to the character under it; where several
                    // cells land on one character the largest carries it.
                    let mut owner: Vec<Option<usize>> = vec![None; m];
                    let area = |cell: &[GlyphBox]| cell.iter().map(|b| b.area).sum::<u32>();
                    for (i, (cx0, cx1, cell)) in cells.iter().enumerate() {
                        let cx = (cx0 + cx1) as f32 / 2.0;
                        let k = (((cx - x0) / span) * m as f32)
                            .floor()
                            .clamp(0.0, (m - 1) as f32) as usize;
                        if owner[k].is_none_or(|j| area(cell) > area(&cells[j].2)) {
                            owner[k] = Some(i);
                        }
                    }
                    for (k, o) in owner.into_iter().enumerate() {
                        if let Some(i) = o {
                            cell_text[i].clone_from(&chars[k]);
                        }
                    }
                }
                LOOSE_ALIGNMENT_WEIGHT
            };

            for ((_, _, cell), text) in cells.iter().zip(cell_text) {
                let carrier = cell
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, b)| b.area)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                for (i, b) in cell.iter().enumerate() {
                    let vote = if i == carrier { text.as_str() } else { "" };
                    *self
                        .text_votes
                        .entry(b.glyph)
                        .or_default()
                        .entry(vote.to_string())
                        .or_insert(0) += weight;
                }
            }
        }
    }

    /// The glyph → text mapping the recorded votes support, as `(glyph id,
    /// text)` pairs in glyph id order, led by the space glyph when there is
    /// anything to map. A shape alias shares its root's tally; a variant far
    /// from the root's position keeps its own, since the same dot is a full
    /// stop on the baseline and part of an `i` above it.
    pub fn to_unicode_entries(&self) -> Vec<(u16, String)> {
        let tally_key = |idx: u32| -> u32 {
            let (root, _, _) = self.resolve_alias(idx);
            let frame_h = self.protos[idx as usize].frame_h;
            if root == idx || self.alias_rise(idx).abs() > semantic_rise_limit(frame_h) {
                idx
            } else {
                root
            }
        };
        let mut tallies: HashMap<u32, HashMap<&str, u32>> = HashMap::new();
        for (&idx, votes) in &self.text_votes {
            let tally = tallies.entry(tally_key(idx)).or_default();
            for (text, &n) in votes {
                *tally.entry(text.as_str()).or_insert(0) += n;
            }
        }
        let mut out = Vec::new();
        if !tallies.is_empty() {
            out.push((SPACE_GID, " ".to_string()));
        }
        for idx in 0..self.protos.len() as u32 {
            let Ok(gid) = u16::try_from(idx + FIRST_SHAPE_GID) else {
                break;
            };
            let Some(tally) = tallies.get(&tally_key(idx)) else {
                continue;
            };
            // Most votes; then the shorter text; then the smaller one.
            let winner = tally.iter().max_by(|a, b| {
                a.1.cmp(b.1)
                    .then_with(|| b.0.len().cmp(&a.0.len()))
                    .then_with(|| b.0.cmp(a.0))
            });
            if let Some((text, _)) = winner {
                out.push((gid, text.to_string()));
            }
        }
        out
    }

    /// How far (pixels, positive = up) the root outline is shifted inside
    /// glyph `idx` relative to the root's own baseline position: zero for a
    /// plain shape alias, the baseline offset for a position variant.
    fn alias_rise(&self, idx: u32) -> i32 {
        let (root, _, oy) = self.resolve_alias(idx);
        if root == idx {
            return 0;
        }
        let p = &self.protos[idx as usize];
        let r = &self.protos[root as usize];
        let root_baseline = r.frame_h as i32 - r.descent.unwrap_or(0);
        let own_baseline = p.frame_h as i32 - p.descent.unwrap_or(0);
        own_baseline - root_baseline - oy
    }

    /// Find the prototype this component belongs to, or add it as a new one.
    /// Returns `(index, dx, dy)`: the prototype frame's offset relative to
    /// the component's top-left.
    fn match_or_insert(&mut self, bitmap: BitImage) -> (u32, i32, i32) {
        let w = bitmap.width as u32;
        let h = bitmap.height as u32;

        // Exact agreement with any prototype first; failing that, the same
        // type inked a little differently, judged only against prototypes
        // established enough to have a clean majority shape.
        let started = Instant::now();
        let profile = InkProfile::of(&bitmap);
        let mut found = self.find_match(&bitmap, &profile, Gate::Strict, |_, _| true);
        self.profile.strict += started.elapsed();
        if found.is_some() {
            self.profile.strict_hits += 1;
        } else if has_stroke_interior(&bitmap) {
            let started = Instant::now();
            found = self.find_match(&bitmap, &profile, Gate::Tolerant, |_, p| {
                p.uses >= ESTABLISHED_USES
            });
            self.profile.tolerant += started.elapsed();
            if found.is_some() {
                self.profile.tolerant_hits += 1;
            }
        }
        if let Some((idx, dx, dy)) = found {
            let started = Instant::now();
            let proto = &mut self.protos[idx as usize];
            // The matcher sits at `matcher_offset` in the frame and at
            // `(dx, dy)` in the instance, so the instance's origin in frame
            // coordinates is `matcher_offset − (dx, dy)`.
            let origin = (proto.matcher_offset.0 - dx, proto.matcher_offset.1 - dy);
            proto.accumulate(&bitmap, origin);
            let frame_dx = dx - proto.matcher_offset.0;
            let frame_dy = dy - proto.matcher_offset.1;
            if proto.uses >= proto.next_refresh {
                proto.next_refresh = proto.next_refresh * 2 - 1;
                self.refresh(idx);
                // With the noise voted out, a young cluster often turns out
                // to be an established glyph whose first instance it missed.
                self.try_alias(idx, Gate::Strict);
            }
            self.profile.fold += started.elapsed();
            return (idx, frame_dx, frame_dy);
        }

        self.profile.inserted += 1;
        let idx = self.protos.len() as u32;
        let proto = Prototype::new(bitmap);
        let black = proto.black;
        self.protos.push(proto);
        Self::index(&mut self.buckets, (w, h), black, idx);
        (idx, 0, 0)
    }

    /// Put prototype `idx` with `black` ink into its bucket, in ink order.
    fn index(
        buckets: &mut HashMap<(u32, u32), Vec<(u32, u32)>>,
        key: (u32, u32),
        black: u32,
        idx: u32,
    ) {
        let bucket = buckets.entry(key).or_default();
        let at = bucket.partition_point(|&(b, i)| (b, i) < (black, idx));
        bucket.insert(at, (black, idx));
    }

    /// Take prototype `idx` out of the bucket for `key`.
    fn unindex(buckets: &mut HashMap<(u32, u32), Vec<(u32, u32)>>, key: (u32, u32), idx: u32) {
        if let Some(bucket) = buckets.get_mut(&key) {
            bucket.retain(|&(_, i)| i != idx);
        }
    }

    /// The best prototype `bitmap` matches under `gate` among those
    /// `accept` admits, as `(index, dx, dy)` with the prototype's matcher
    /// placed at `(dx, dy)` in `bitmap`'s frame.
    fn find_match(
        &mut self,
        bitmap: &BitImage,
        profile: &InkProfile,
        gate: Gate,
        accept: impl Fn(u32, &Prototype) -> bool,
    ) -> Option<(u32, i32, i32)> {
        let w = bitmap.width as u32;
        let h = bitmap.height as u32;
        let black = bitmap.count_ones() as u32;
        // Both gates forgive ink: the strict one a fifth of it, the tolerant
        // one everything within a pixel of the other shape — which at reading
        // sizes is a whole hairline. A `c` and an `e` differ by a bar four
        // pixels long whose ends touch the bowl, leaving two pixels to
        // notice. What no impression can do is open or close a counter, so
        // shapes that enclose a different number of them never fold together.
        //
        // Only where the strokes have an interior, though: a wall one pixel
        // thick is opened or closed by one speck of dirt, and its counters
        // say more about the scan than about the letter.
        let counters = has_stroke_interior(bitmap).then(|| holes(bitmap));
        let dim_limit = DIM_LIMIT + w.max(h) / 16;

        // The ink counts the pre-filter admits: a lighter prototype may be
        // short by the fraction of the instance's ink, a heavier one by the
        // fraction of its own.
        let percent = gate.ink_diff_percent();
        let ink_lo = black.saturating_sub(black * percent / 100 + TOTAL_DIFF_MIN);
        let ink_hi = (black + TOTAL_DIFF_MIN) * 100 / (100 - percent) + 1;

        // (rank, index, dx, dy)
        let mut best: Option<((u32, u32, u32), u32, i32, i32)> = None;
        let (mut candidates, mut compared) = (0u64, 0u64);
        for bw in w.saturating_sub(dim_limit)..=w + dim_limit {
            for bh in h.saturating_sub(dim_limit)..=h + dim_limit {
                let Some(bucket) = self.buckets.get(&(bw, bh)) else {
                    continue;
                };
                let start = bucket.partition_point(|&(b, _)| b < ink_lo);
                for &(proto_black, idx) in bucket[start..].iter().take_while(|&&(b, _)| b <= ink_hi)
                {
                    candidates += 1;
                    let ink = proto_black.max(black);
                    if proto_black.abs_diff(black) > ink * percent / 100 + TOTAL_DIFF_MIN {
                        continue;
                    }
                    let proto = &self.protos[idx as usize];
                    if !accept(idx, proto) || (gate == Gate::Tolerant && !proto.stroked) {
                        continue;
                    }
                    if proto.stroked && counters.is_some_and(|c| c != proto.counters) {
                        continue;
                    }
                    // Shapes whose ink profiles alone differ by more than
                    // the gate allows cannot match at any alignment.
                    let total_limit = gate.total_limit(ink);
                    if profile.distance_bound(&proto.profile, total_limit) > total_limit {
                        continue;
                    }
                    compared += 1;
                    // The comparator only finds the alignment; its word-wise
                    // error is loose, so it gets a loose budget and the
                    // packed diff decides.
                    let Some(r) = self.comparator.compare_for_refine_family(
                        bitmap,
                        &proto.matcher,
                        gate.comparator_budget(ink),
                        SHIFT_LIMIT,
                        SHIFT_LIMIT,
                    ) else {
                        continue;
                    };
                    // Two passes: the second measures the far pixels, and is
                    // only reached by a candidate the cheap tests admit.
                    let Some(coarse) =
                        bitrows::diff(bitmap, &proto.matcher, r.dx, r.dy, total_limit, None)
                    else {
                        continue;
                    };
                    if !gate.accept_coarse(coarse, ink) {
                        continue;
                    }
                    let Some(diff) = bitrows::diff(
                        bitmap,
                        &proto.matcher,
                        r.dx,
                        r.dy,
                        total_limit,
                        gate.far_limit(ink),
                    ) else {
                        continue;
                    };
                    let Some(rank) = gate.accept(diff, ink) else {
                        continue;
                    };
                    if best.is_none_or(|(b, ..)| rank < b) {
                        best = Some((rank, idx, r.dx, r.dy));
                    }
                }
            }
        }
        match gate {
            Gate::Strict => {
                self.profile.strict_candidates += candidates;
                self.profile.strict_compared += compared;
            }
            Gate::Tolerant => {
                self.profile.tolerant_candidates += candidates;
                self.profile.tolerant_compared += compared;
            }
        }
        best.map(|(_, idx, dx, dy)| (idx, dx, dy))
    }

    /// Rebuild prototype `idx`'s matcher from its votes, keeping the bucket
    /// index in step.
    fn refresh(&mut self, idx: u32) {
        let proto = &mut self.protos[idx as usize];
        let old_key = (proto.matcher.width as u32, proto.matcher.height as u32);
        let old_black = proto.black;
        let resized = proto.refresh_matcher();
        let black = proto.black;
        let new_key = (proto.matcher.width as u32, proto.matcher.height as u32);
        if resized || black != old_black {
            Self::unindex(&mut self.buckets, old_key, idx);
            Self::index(&mut self.buckets, new_key, black, idx);
        }
    }

    /// If prototype `idx`'s current shape matches a heavier prototype under
    /// `gate`, make it an alias of that one and retire it from matching.
    /// "Heavier" breaks ties by index, so two shapes with equal counts can
    /// still fold one into the other.
    fn try_alias(&mut self, idx: u32, gate: Gate) -> bool {
        let (matcher, profile, offset, uses) = {
            let p = &self.protos[idx as usize];
            if gate == Gate::Tolerant && !p.stroked {
                return false;
            }
            (
                p.matcher.clone(),
                p.profile.clone(),
                p.matcher_offset,
                p.uses,
            )
        };
        let heavier = |i: u32, p: &Prototype| p.uses > uses || (p.uses == uses && i < idx);
        let Some((target, dx, dy)) = self.find_match(&matcher, &profile, gate, heavier) else {
            return false;
        };
        // Target matcher pixel q is at `offset + (dx, dy) + q` in this frame
        // and at `target.matcher_offset + q` in the target's; the difference
        // is where the target's frame origin lands here.
        let t = &self.protos[target as usize];
        let origin = (
            offset.0 + dx - t.matcher_offset.0,
            offset.1 + dy - t.matcher_offset.1,
        );
        let key = (matcher.width as u32, matcher.height as u32);
        Self::unindex(&mut self.buckets, key, idx);
        self.protos[idx as usize].alias = Some((target, origin.0, origin.1));
        if gate == Gate::Tolerant {
            self.tolerant_folds.push((idx, target));
        }
        true
    }

    /// Follow aliases to the prototype that owns the outline, accumulating
    /// the frame-origin offset.
    fn resolve_alias(&self, idx: u32) -> (u32, i32, i32) {
        let (mut cur, mut ox, mut oy) = (idx, 0, 0);
        while let Some((target, dx, dy)) = self.protos[cur as usize].alias {
            ox += dx;
            oy += dy;
            cur = target;
        }
        (cur, ox, oy)
    }

    /// End-of-document pass: bring every matcher up to its final majority
    /// shape, fold prototypes that now match a heavier one exactly, then
    /// fold the rest, lightest first, into any heavier prototype that is
    /// the same type inked differently. Idempotent.
    pub fn finalize(&mut self) {
        let started = Instant::now();
        let live: Vec<u32> = (0..self.protos.len() as u32)
            .filter(|&i| self.protos[i as usize].is_simple())
            .collect();
        for &idx in &live {
            if self.protos[idx as usize].uses >= 2 {
                self.refresh(idx);
            }
        }
        let mut order = live;
        order.sort_by_key(|&i| std::cmp::Reverse(self.protos[i as usize].uses));
        for &idx in &order {
            self.try_alias(idx, Gate::Strict);
        }
        // Heaviest first, so that whatever a shape folds into has already
        // had its turn and stays a root: a tolerant fold is only ever one
        // hop, and two hops of "within a pixel" could add up to a different
        // letter.
        for idx in order {
            if self.protos[idx as usize].is_simple() {
                self.try_alias(idx, Gate::Tolerant);
            }
        }
        self.profile.finalize += started.elapsed();
    }

    /// Outline glyphs the last [`build_embedded_font`](Self::build_embedded_font)
    /// emitted as fitted curves rather than staircases.
    pub fn fitted(&self) -> usize {
        self.fitted.get()
    }

    /// Glyph ids the last [`build_embedded_font`](Self::build_embedded_font)
    /// mapped to text.
    pub fn mapped(&self) -> usize {
        self.mapped.get()
    }

    /// Prototypes that own an outline (neither aliases nor compounds).
    pub fn distinct(&self) -> usize {
        self.protos.iter().filter(|p| p.is_simple()).count()
    }

    /// The simple prototypes glyph `idx` draws, each with its frame origin
    /// in `idx`'s frame: itself, its alias root, or its compound parts'
    /// roots, alias chains followed.
    fn simple_parts(&self, idx: u32) -> Vec<(u32, i32, i32)> {
        let (target, ox, oy) = self.resolve_alias(idx);
        let t = &self.protos[target as usize];
        if t.parts.is_empty() {
            return vec![(target, ox, oy)];
        }
        t.parts
            .iter()
            .map(|&(part, px, py)| {
                let (root, rx, ry) = self.resolve_alias(part);
                (root, ox + px + rx, oy + py + ry)
            })
            .collect()
    }

    /// Diagnostic: write the outline-owning prototypes as a contact sheet
    /// (`glyphs.png`, sorted by size so near-duplicates sit together, each
    /// with a bar for its instance count) and a listing (`glyphs.txt`).
    pub fn dump(&mut self, dir: &std::path::Path) -> Result<()> {
        use std::io::Write;
        let mut inbound = vec![0u32; self.protos.len()];
        let mut weight = vec![0u32; self.protos.len()];
        for (i, p) in self.protos.iter().enumerate() {
            let (root, _, _) = self.resolve_alias(i as u32);
            if p.alias.is_some() {
                inbound[root as usize] += 1;
            }
            if p.parts.is_empty() {
                weight[root as usize] += p.uses;
            }
        }
        let mut simple: Vec<usize> = (0..self.protos.len())
            .filter(|&i| self.protos[i].is_simple())
            .collect();
        simple.sort_by_key(|&i| {
            let p = &self.protos[i];
            (p.matcher.height, p.matcher.width, p.black)
        });
        std::fs::create_dir_all(dir)?;
        let mut listing = std::fs::File::create(dir.join("glyphs.txt"))?;
        let mut hist = [0usize; 4];
        for &i in &simple {
            let p = &self.protos[i];
            let w = weight[i];
            hist[match w {
                1 => 0,
                2..=3 => 1,
                4..=15 => 2,
                _ => 3,
            }] += 1;
            let text = self.text_votes.get(&(i as u32)).map(|votes| {
                let mut v: Vec<_> = votes.iter().collect();
                v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                v.iter()
                    .take(3)
                    .map(|(t, n)| format!("{t:?}x{n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            writeln!(
                listing,
                "{i}\t{}x{}\tink {}\tuses {}\t+{} aliases\tweight {}\t{}",
                p.matcher.width,
                p.matcher.height,
                p.black,
                p.uses,
                inbound[i],
                w,
                text.unwrap_or_default()
            )?;
        }
        writeln!(
            listing,
            "# {} shapes: {} used once, {} used 2-3 times, {} used 4-15 times, {} used 16+ times",
            simple.len(),
            hist[0],
            hist[1],
            hist[2],
            hist[3]
        )?;
        eprintln!(
            "[glyphfont] {} shapes: {} used once, {} used 2-3 times, {} used 4-15 times, {} used 16+ times",
            simple.len(),
            hist[0],
            hist[1],
            hist[2],
            hist[3]
        );

        let cell_w = simple
            .iter()
            .map(|&i| self.protos[i].matcher.width)
            .max()
            .unwrap_or(1)
            .min(64)
            + 4;
        let cell_h = simple
            .iter()
            .map(|&i| self.protos[i].matcher.height)
            .max()
            .unwrap_or(1)
            .min(64)
            + 6;
        let cols = 40usize;
        let rows = simple.len().div_ceil(cols).max(1);
        let mut sheet = image::RgbImage::from_pixel(
            (cols * cell_w) as u32,
            (rows * cell_h) as u32,
            image::Rgb([255, 255, 255]),
        );
        for (n, &i) in simple.iter().enumerate() {
            let p = &self.protos[i];
            let ox = (n % cols) * cell_w + 2;
            let oy = (n / cols) * cell_h + 1;
            for y in 0..p.matcher.height.min(cell_h - 6) {
                for x in 0..p.matcher.width.min(cell_w - 4) {
                    if p.matcher.get_usize(x, y) {
                        sheet.put_pixel((ox + x) as u32, (oy + y) as u32, image::Rgb([0, 0, 0]));
                    }
                }
            }
            let colour = match weight[i] {
                1 => image::Rgb([220, 0, 0]),
                2..=3 => image::Rgb([240, 140, 0]),
                4..=15 => image::Rgb([0, 90, 220]),
                _ => image::Rgb([0, 160, 0]),
            };
            let len = (weight[i] as usize).min(cell_w - 4).max(1);
            for x in 0..len {
                for y in 0..2 {
                    sheet.put_pixel((ox + x) as u32, (oy + cell_h - 4 + y) as u32, colour);
                }
            }
        }
        sheet.save(dir.join("glyphs.png"))?;

        // The tolerant folds, each as "folded shape | root shape" at 2×,
        // with the pixels the gate had to forgive (within a pixel of the
        // other shape) in blue and the far ones in red.
        let folds: Vec<(u32, u32)> = self.tolerant_folds.iter().copied().take(1200).collect();
        {
            // What the gate saw for each fold, so a bad fold can be told
            // from a good one by its numbers and not only by eye.
            let mut listing = std::fs::File::create(dir.join("folds.txt"))?;
            writeln!(
                listing,
                "# light\troot\tink\ttotal\tthick\tfar\tblob\tholes"
            )?;
            for &(light, heavy) in &folds {
                let (root, _, _) = self.resolve_alias(heavy);
                let a = self.protos[light as usize].matcher.clone();
                let b = self.protos[root as usize].matcher.clone();
                let Some((dx, dy)) = self
                    .comparator
                    .compare_for_symbol_unify(&a, &b, u32::MAX, SHIFT_LIMIT, SHIFT_LIMIT)
                    .map(|r| (r.dx, r.dy))
                else {
                    continue;
                };
                let Some(d) = bitrows::diff(&a, &b, dx, dy, u32::MAX, Some(u32::MAX)) else {
                    continue;
                };
                let ink = a.count_ones().max(b.count_ones());
                writeln!(
                    listing,
                    "{light}\t{root}\t{ink}\t{}\t{}\t{}\t{}\t{} {}",
                    d.total,
                    d.thick,
                    d.far,
                    d.far_blob,
                    holes(&a),
                    holes(&b)
                )?;
            }
        }
        if !folds.is_empty() {
            let zoom = 2usize;
            let pair_w = zoom * (2 * cell_w + 2);
            let row_h = zoom * cell_h;
            let cols = 8usize;
            let rows = folds.len().div_ceil(cols);
            let mut sheet = image::RgbImage::from_pixel(
                (cols * pair_w) as u32,
                (rows * row_h) as u32,
                image::Rgb([255, 255, 255]),
            );
            let mut blot = |x: usize, y: usize, c: [u8; 3]| {
                for yy in 0..zoom {
                    for xx in 0..zoom {
                        sheet.put_pixel(
                            (x * zoom + xx) as u32,
                            (y * zoom + yy) as u32,
                            image::Rgb(c),
                        );
                    }
                }
            };
            for (n, &(light, heavy)) in folds.iter().enumerate() {
                let (root, _, _) = self.resolve_alias(heavy);
                let ox = (n % cols) * (2 * cell_w + 2);
                let oy = (n / cols) * cell_h + 1;
                let a = self.protos[light as usize].matcher.clone();
                let b = self.protos[root as usize].matcher.clone();
                let (dx, dy) = self
                    .comparator
                    .compare_for_symbol_unify(&a, &b, u32::MAX, SHIFT_LIMIT, SHIFT_LIMIT)
                    .map(|r| (r.dx, r.dy))
                    .unwrap_or((0, 0));
                let on = |img: &BitImage, x: i32, y: i32| -> bool {
                    x >= 0
                        && y >= 0
                        && (x as usize) < img.width
                        && (y as usize) < img.height
                        && img.get_usize(x as usize, y as usize)
                };
                let near = |img: &BitImage, x: i32, y: i32| -> bool {
                    (-1..=1).any(|ny| (-1..=1).any(|nx| on(img, x + nx, y + ny)))
                };
                for (k, (img, other, sx, sy)) in [(&a, &b, -dx, -dy), (&b, &a, dx, dy)]
                    .into_iter()
                    .enumerate()
                {
                    let cx = ox + k * (cell_w + 1) + 2;
                    for y in 0..img.height.min(cell_h - 6) {
                        for x in 0..img.width.min(cell_w - 4) {
                            if !img.get_usize(x, y) {
                                continue;
                            }
                            // This pixel in the other shape's frame.
                            let (qx, qy) = (x as i32 + sx, y as i32 + sy);
                            let colour = if on(other, qx, qy) {
                                [0, 0, 0]
                            } else if near(other, qx, qy) {
                                [70, 110, 255]
                            } else {
                                [230, 0, 0]
                            };
                            blot(cx + x, oy + y, colour);
                        }
                    }
                }
                for y in 0..cell_h - 2 {
                    blot(ox + cell_w, oy + y, [200, 200, 200]);
                }
            }
            sheet.save(dir.join("folds.png"))?;
        }
        Ok(())
    }

    /// The final shape of prototype `idx` over its padded frame, and the
    /// frame origin within it.
    #[cfg(test)]
    fn prototype_shape(&self, idx: usize) -> (BitImage, (usize, usize)) {
        (self.protos[idx].majority(), (FRAME_PAD, FRAME_PAD))
    }

    /// Add a prototype as if `uses` identical instances of `bitmap` had been
    /// matched to it, with the given descent, bypassing matching.
    #[cfg(test)]
    fn insert_prototype(&mut self, bitmap: BitImage, uses: u32, descent: i32) -> u32 {
        let key = (bitmap.width as u32, bitmap.height as u32);
        let mut proto = Prototype::new(bitmap);
        for v in proto.votes.iter_mut() {
            *v *= uses as u16;
        }
        proto.uses = uses;
        proto.descent = Some(descent);
        proto.advance = Some(proto.frame_w as u32);
        let idx = self.protos.len() as u32;
        let black = proto.black;
        self.protos.push(proto);
        Self::index(&mut self.buckets, key, black, idx);
        idx
    }

    /// Build the embedded font: glyph 0 is `.notdef`, glyph [`SPACE_GID`] the
    /// blank word space, glyph `i + FIRST_SHAPE_GID` prototype `i` with its
    /// outline from the prototype's majority shape; aliases and compounds
    /// become composites of the simple glyphs they draw. Call
    /// [`finalize`](Self::finalize) first.
    pub fn build_embedded_font(&self) -> Result<EmbeddedFont> {
        if self.protos.len() + FIRST_SHAPE_GID as usize > MAX_GLYPHS {
            return Err(anyhow!(
                "glyph font: {} shapes exceed the {} glyph ids one font can hold",
                self.protos.len(),
                MAX_GLYPHS
            ));
        }
        let mut glyphs = Vec::with_capacity(self.protos.len() + FIRST_SHAPE_GID as usize);
        let mut widths = Vec::with_capacity(self.protos.len() + FIRST_SHAPE_GID as usize);
        glyphs.push(GlyphOutline::empty(0));
        widths.push(0u16);
        glyphs.push(GlyphOutline::empty(SPACE_ADVANCE));
        widths.push(SPACE_ADVANCE);
        let mut fitted_count = 0usize;
        let mut outlines = Duration::ZERO;
        for (idx, proto) in self.protos.iter().enumerate() {
            let descent = proto.descent.unwrap_or(0);
            let advance_px = proto.advance.unwrap_or(proto.frame_w as u32);
            let advance = u16::try_from(advance_px as i32 * UNITS_PER_PIXEL)
                .map_err(|_| anyhow!("glyph font: advance of {advance_px} px overflows"))?;
            widths.push(advance);
            if !proto.is_simple() {
                // Draw each simple part's outline where its frame sits in
                // ours. A part's y axis starts at its own baseline, so shift
                // by the difference between the two frames' baseline rows.
                let own_baseline = proto.frame_h as i32 - descent;
                let mut components = Vec::new();
                for (root, ox, oy) in self.simple_parts(idx as u32) {
                    let r = &self.protos[root as usize];
                    let root_baseline = r.frame_h as i32 - r.descent.unwrap_or(0);
                    let dx = ox * UNITS_PER_PIXEL;
                    let dy = (own_baseline - root_baseline - oy) * UNITS_PER_PIXEL;
                    components.push(GlyphComponent {
                        glyph: u16::try_from(root + FIRST_SHAPE_GID).map_err(|_| {
                            anyhow!("glyph font: glyph id {} overflows", root + FIRST_SHAPE_GID)
                        })?,
                        dx: i16::try_from(dx)
                            .map_err(|_| anyhow!("glyph font: composite offset {dx} overflows"))?,
                        dy: i16::try_from(dy)
                            .map_err(|_| anyhow!("glyph font: composite offset {dy} overflows"))?,
                    });
                }
                glyphs.push(GlyphOutline::composite(advance, components));
                continue;
            }
            let started = Instant::now();
            let shape = proto.majority();
            let baseline_row = (FRAME_PAD + proto.frame_h) as i32 - descent;
            let origin_x = -(FRAME_PAD as i32);
            let contours = match vectorize(&shape, origin_x, baseline_row) {
                Some(fitted) => {
                    fitted_count += 1;
                    fitted
                }
                None => trace_outline_at(&shape, origin_x, baseline_row),
            };
            outlines += started.elapsed();
            glyphs.push(GlyphOutline {
                contours,
                advance,
                components: Vec::new(),
            });
        }

        let bbox = glyphs
            .iter()
            .filter_map(GlyphOutline::bbox)
            .reduce(|u, b| {
                [
                    u[0].min(b[0]),
                    u[1].min(b[1]),
                    u[2].max(b[2]),
                    u[3].max(b[3]),
                ]
            })
            .unwrap_or([0, 0, UNITS_PER_EM as i16, UNITS_PER_EM as i16]);
        let ascent = bbox[3].max(1);
        let descent = bbox[1].min(0);
        let cap_height = ((ascent as i32) * 7 / 10) as i16;

        let built = build_truetype(&TrueTypeSpec {
            name: FONT_NAME,
            units_per_em: UNITS_PER_EM,
            ascent,
            descent,
            cap_height,
            glyphs: &glyphs,
        });

        self.outlines.set(outlines);
        if std::env::var_os("LEGE_GLYPH_DUMP").is_some() {
            eprintln!(
                "[glyphfont] time: {}",
                Profile {
                    outlines,
                    ..self.profile
                }
                .report()
            );
        }

        let entries = self.to_unicode_entries();
        self.mapped.set(entries.len());
        let to_unicode = if entries.is_empty() {
            ToUnicode::None
        } else {
            ToUnicode::Custom(to_unicode_cmap(&entries).into())
        };

        self.fitted.set(fitted_count);
        Ok(EmbeddedFont {
            data: built.data.into(),
            post_script_name: FONT_NAME.to_string(),
            ascent: ascent as i32,
            descent: descent as i32,
            cap_height: cap_height as i32,
            italic_angle: 0.0,
            bbox: [
                built.bbox[0] as i32,
                built.bbox[1] as i32,
                built.bbox[2] as i32,
                built.bbox[3] as i32,
            ],
            symbolic: true,
            to_unicode,
            cid_widths: Some(widths.into()),
            compress_program: true,
        })
    }
}

/// Group instances into text lines and order each line left to right.
///
/// Instances are swept in order of vertical centre; one joins the current
/// line when its centre is within half a glyph height (the larger of its own
/// and the line's running mean) of the line's running mean centre. The
/// baseline is the lower median of member bottom edges — most glyphs sit on
/// the baseline; descenders and raised punctuation are the minority.
///
/// The returned `rise_px` fields temporarily hold each glyph's own descent
/// (bottom − baseline); `process_page` turns them into rises once prototype
/// descents are known.
/// A recognized word's box carried from the page as scanned into the
/// frame its glyphs were placed in.
fn upright_word(frame: &PageFrame, word: &TextWord) -> TextWord {
    let corners = [
        (word.x0, word.y0),
        (word.x1, word.y0),
        (word.x0, word.y1),
        (word.x1, word.y1),
    ]
    .map(|(x, y)| frame.to_upright(x as f64, y as f64));
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (x, y) in corners {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    TextWord {
        text: word.text.clone(),
        x0: x0 as f32,
        y0: y0 as f32,
        x1: x1 as f32,
        y1: y1 as f32,
    }
}

fn group_lines(mut instances: Vec<GlyphInstance>) -> Vec<GlyphLine> {
    if instances.is_empty() {
        return Vec::new();
    }
    instances.sort_by_key(|i| (2 * i.y + i.height as i32, i.x));

    struct Line {
        members: Vec<GlyphInstance>,
        centre_sum: i64,
        height_sum: i64,
    }
    impl Line {
        fn mean_centre(&self) -> f64 {
            self.centre_sum as f64 / (2.0 * self.members.len() as f64)
        }
        fn mean_height(&self) -> f64 {
            self.height_sum as f64 / self.members.len() as f64
        }
    }

    let mut lines: Vec<Line> = Vec::new();
    for inst in instances {
        let centre2 = 2 * inst.y as i64 + inst.height as i64;
        let joins = lines.last().is_some_and(|line| {
            let tolerance = 0.5 * line.mean_height().max(inst.height as f64);
            (centre2 as f64 / 2.0 - line.mean_centre()).abs() <= tolerance
        });
        if joins {
            let line = lines.last_mut().unwrap();
            line.members.push(inst);
            line.centre_sum += centre2;
            line.height_sum += inst.height as i64;
        } else {
            lines.push(Line {
                members: vec![inst],
                centre_sum: centre2,
                height_sum: inst.height as i64,
            });
        }
    }

    lines
        .into_iter()
        .map(|mut line| {
            line.members.sort_by_key(|i| (i.x, i.y));
            // The baseline: a fit through the bottoms where the line is
            // long enough to support one (page curl leaves a line tilted
            // after the page-wide skew is gone), else the median bottom.
            // Each glyph's descent is measured from the baseline under it,
            // and the line is placed level at the baseline's midpoint.
            let points: Vec<(f64, f64)> = line
                .members
                .iter()
                .map(|i| {
                    (
                        (2 * i.x + i.width as i32) as f64 / 2.0,
                        (i.y + i.height as i32) as f64,
                    )
                })
                .collect();
            let (baseline_y, baseline_at): (i32, Box<dyn Fn(f64) -> i32>) =
                match fit_baseline(&points) {
                    Some((slope, intercept)) => {
                        let mid_x = (points[0].0 + points[points.len() - 1].0) / 2.0;
                        (
                            (intercept + slope * mid_x).round() as i32,
                            Box::new(move |x| (intercept + slope * x).round() as i32),
                        )
                    }
                    None => {
                        let mut bottoms: Vec<i32> =
                            line.members.iter().map(|i| i.y + i.height as i32).collect();
                        bottoms.sort_unstable();
                        let median = bottoms[(bottoms.len() - 1) / 2];
                        (median, Box::new(move |_| median))
                    }
                };
            let glyphs = line
                .members
                .iter()
                .zip(&points)
                .map(|(i, &(xc, _))| PlacedGlyph {
                    glyph: i.glyph,
                    x: i.x,
                    width: i.width,
                    rise_px: (i.y + i.height as i32) - baseline_at(xc),
                })
                .collect();
            GlyphLine { baseline_y, glyphs }
        })
        .collect()
}

/// Trace the pixel edges of a bitmap into closed TrueType contours in font
/// units: outer boundaries clockwise, holes counter-clockwise (y up), every
/// point on-curve, collinear points removed. The glyph origin is the bitmap's
/// left edge on the text baseline; `descent_px` is how far the bitmap's
/// bottom edge sits below that baseline.
///
/// Each boundary edge is emitted with ink on its right, so linking edges
/// head-to-tail yields the correct winding. Where two ink pixels touch only
/// at a corner, the walk turns right (hugging the ink), which separates the
/// two into distinct contours; under nonzero winding either choice renders
/// identically.
pub fn trace_pixel_outline(bitmap: &BitImage, descent_px: i32) -> Vec<Vec<OutlinePoint>> {
    trace_outline_at(bitmap, 0, bitmap.height as i32 - descent_px)
}

/// [`trace_pixel_outline`] with an explicit origin: the glyph origin is
/// `origin_x` pixels right of the bitmap's left edge (negative = the bitmap
/// starts left of the origin), and the baseline runs along the top edge of
/// row `baseline_row` (so a bitmap whose bottom row is the last one above the
/// baseline has `baseline_row == height`).
pub fn trace_outline_at(
    bitmap: &BitImage,
    origin_x: i32,
    baseline_row: i32,
) -> Vec<Vec<OutlinePoint>> {
    let to_font = |(c, r): (i32, i32)| -> OutlinePoint {
        OutlinePoint::on(
            ((c + origin_x) * UNITS_PER_PIXEL) as i16,
            ((baseline_row - r) * UNITS_PER_PIXEL) as i16,
        )
    };
    trace_pixel_loops(bitmap)
        .into_iter()
        .map(|pts| {
            let n = pts.len();
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let prev = pts[(i + n - 1) % n];
                let p = pts[i];
                let next = pts[(i + 1) % n];
                let d1 = (p.0 - prev.0, p.1 - prev.1);
                let d2 = (next.0 - p.0, next.1 - p.1);
                if d1 != d2 {
                    out.push(to_font(p));
                }
            }
            out
        })
        .filter(|c| c.len() >= 3)
        .collect()
}

/// The closed pixel-corner polygons bounding a bitmap's ink, in pixel
/// coordinates (y down), one vertex per boundary edge, in the walking order
/// described at [`trace_pixel_outline`].
pub(crate) fn trace_pixel_loops(bitmap: &BitImage) -> Vec<Vec<(i32, i32)>> {
    let w = bitmap.width;
    let h = bitmap.height;
    let ink = |x: isize, y: isize| -> bool {
        x >= 0
            && y >= 0
            && (x as usize) < w
            && (y as usize) < h
            && bitmap.get_usize(x as usize, y as usize)
    };

    // Directed edges between grid corners (c, r), y down.
    let mut edges: Vec<((i32, i32), (i32, i32))> = Vec::new();
    for y in 0..h as isize {
        for x in 0..w as isize {
            if !ink(x, y) {
                continue;
            }
            let (c, r) = (x as i32, y as i32);
            if !ink(x, y - 1) {
                edges.push(((c, r), (c + 1, r))); // top: east
            }
            if !ink(x, y + 1) {
                edges.push(((c + 1, r + 1), (c, r + 1))); // bottom: west
            }
            if !ink(x - 1, y) {
                edges.push(((c, r + 1), (c, r))); // left: north
            }
            if !ink(x + 1, y) {
                edges.push(((c + 1, r), (c + 1, r + 1))); // right: south
            }
        }
    }
    if edges.is_empty() {
        return Vec::new();
    }

    let mut outgoing: HashMap<(i32, i32), Vec<usize>> = HashMap::with_capacity(edges.len());
    for (i, (start, _)) in edges.iter().enumerate() {
        outgoing.entry(*start).or_default().push(i);
    }
    let mut used = vec![false; edges.len()];

    let mut contours = Vec::new();
    for first in 0..edges.len() {
        if used[first] {
            continue;
        }
        let origin = edges[first].0;
        let mut points: Vec<(i32, i32)> = vec![origin];
        let mut cur = first;
        loop {
            used[cur] = true;
            let (start, end) = edges[cur];
            if end == origin {
                break;
            }
            points.push(end);
            let d_in = (end.0 - start.0, end.1 - start.1);
            let candidates = outgoing.get(&end).map(Vec::as_slice).unwrap_or(&[]);
            let mut next = None;
            for &cand in candidates {
                if used[cand] {
                    continue;
                }
                let (cs, ce) = edges[cand];
                let d_out = (ce.0 - cs.0, ce.1 - cs.1);
                // Cross product in y-up space: negative = right turn.
                let cross = d_in.0 * (-d_out.1) - (-d_in.1) * d_out.0;
                if cross < 0 {
                    next = Some(cand);
                    break;
                }
                if next.is_none() {
                    next = Some(cand);
                }
            }
            match next {
                Some(n) => cur = n,
                None => break, // unreachable for a well-formed boundary graph
            }
        }
        if points.len() >= 4 {
            contours.push(points);
        }
    }

    contours
}

/// The document-wide dictionary shared by concurrently processed pages, plus
/// counters for the run summary.
pub struct GlyphFontSession {
    /// One dictionary per font bank, in the order pages first drew from
    /// them. All but the last are full.
    dicts: Mutex<Vec<GlyphDictionary>>,
    pages: AtomicUsize,
    /// Prototypes a dictionary may hold before the next page starts another.
    max_prototypes: usize,
}

impl Default for GlyphFontSession {
    fn default() -> Self {
        Self::new()
    }
}

impl GlyphFontSession {
    pub fn new() -> Self {
        Self::with_prototype_cap(MAX_PROTOTYPES)
    }

    /// A session whose dictionaries hold at most `max_prototypes` shapes.
    /// Only the font's own id space justifies a lower cap in production; the
    /// tests use it to reach a second bank without 65 000 shapes.
    pub fn with_prototype_cap(max_prototypes: usize) -> Self {
        Self {
            dicts: Mutex::new(vec![GlyphDictionary::new()]),
            pages: AtomicUsize::new(0),
            max_prototypes: max_prototypes.max(1),
        }
    }

    /// Process one binarized page in the pipeline's image convention (one
    /// byte per pixel, `<= 128` is ink). `dpi` only tunes component cleanup.
    pub fn process_page_pixels(
        &self,
        pixels: &[u8],
        width: usize,
        height: usize,
        dpi: i32,
    ) -> Result<PageGlyphRuns> {
        let page = page_bitmap(pixels, width, height).map_err(|e| anyhow!("glyph font: {e}"))?;
        // Components and frame need no dictionary: pages analyse in
        // parallel and only the matching below waits for the lock.
        let analysis = PageAnalysis::of(&page, dpi);
        let type_size = analysis.median_size();
        drop(page);

        let runs = {
            let mut dicts = self
                .dicts
                .lock()
                .map_err(|_| anyhow!("glyph font dictionary poisoned"))?;
            // A page's glyph ids all index one font, so a page that would
            // carry the dictionary past a font's id space starts the next
            // one instead. Every component can add a prototype, and a few
            // add a position variant or a compound besides.
            let headroom = 2 * analysis.shape_count() + 64;
            if dicts
                .last()
                .is_none_or(|dict| dict.len() + headroom > self.max_prototypes)
            {
                dicts.push(GlyphDictionary::new());
            }
            let bank = (dicts.len() - 1) as u16;
            let mut runs = dicts
                .last_mut()
                .expect("a dictionary was just ensured")
                .process_analysis(analysis);
            runs.bank = bank;
            runs
        };
        self.pages.fetch_add(1, Ordering::Relaxed);
        if std::env::var_os("LEGE_GLYPH_DUMP").is_some() {
            eprintln!(
                "[glyphfont] page {}x{}: {} quarter turns, skew {:.2}°, type {} px, {} lines, {} glyphs",
                width,
                height,
                runs.frame.turns,
                runs.frame.skew.to_degrees(),
                type_size,
                runs.lines.len(),
                runs.glyph_count
            );
        }
        Ok(runs)
    }

    /// Record a page's recognized words against its glyph placements; see
    /// [`GlyphDictionary::record_text`].
    pub fn record_text(&self, runs: &PageGlyphRuns, words: &[TextWord]) -> Result<()> {
        let mut dicts = self
            .dicts
            .lock()
            .map_err(|_| anyhow!("glyph font dictionary poisoned"))?;
        if let Some(dict) = dicts.get_mut(usize::from(runs.bank)) {
            dict.record_text(runs, words);
        }
        Ok(())
    }

    /// One font program per bank, in bank order.
    pub fn build_embedded_fonts(&self) -> Result<Vec<EmbeddedFont>> {
        let mut dicts = self
            .dicts
            .lock()
            .map_err(|_| anyhow!("glyph font dictionary poisoned"))?;
        let dump = std::env::var_os("LEGE_GLYPH_DUMP");
        let mut fonts = Vec::with_capacity(dicts.len());
        for (bank, dict) in dicts.iter_mut().enumerate() {
            dict.finalize();
            if let Some(dir) = dump.as_ref() {
                let dir = std::path::Path::new(dir);
                let dir = if bank == 0 {
                    dir.to_path_buf()
                } else {
                    dir.join(format!("bank{bank}"))
                };
                dict.dump(&dir)?;
            }
            fonts.push(dict.build_embedded_font()?);
        }
        Ok(fonts)
    }

    /// Glyph ids the last font build mapped to text.
    pub fn mapped_glyphs(&self) -> usize {
        let dicts = self.dicts.lock().unwrap_or_else(|e| e.into_inner());
        dicts.iter().map(|dict| dict.mapped()).sum()
    }

    /// How many fonts the document's shapes needed.
    pub fn banks(&self) -> usize {
        let dicts = self.dicts.lock().unwrap_or_else(|e| e.into_inner());
        dicts.len()
    }

    /// `(pages, distinct glyphs, glyph occurrences, oversize components dropped)`.
    /// Distinct counts outline-owning shapes (not aliases or compounds);
    /// after the font is built that excludes clusters folded into another.
    pub fn stats(&self) -> (usize, usize, usize, usize) {
        let dicts = self.dicts.lock().unwrap_or_else(|e| e.into_inner());
        (
            self.pages.load(Ordering::Relaxed),
            dicts.iter().map(|dict| dict.distinct()).sum(),
            dicts.iter().map(|dict| dict.instances()).sum(),
            dicts.iter().map(|dict| dict.oversize_dropped()).sum(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(rows: &[&str]) -> BitImage {
        let h = rows.len();
        let w = rows[0].len();
        let mut img = BitImage::new(w as u32, h as u32).unwrap();
        for (y, row) in rows.iter().enumerate() {
            for (x, ch) in row.bytes().enumerate() {
                if ch == b'#' {
                    img.set_usize(x, y, true);
                }
            }
        }
        img
    }

    fn xy(c: &[OutlinePoint]) -> Vec<(i16, i16)> {
        c.iter().map(|p| (p.x, p.y)).collect()
    }

    /// Shoelace area in font units; negative = clockwise (y up).
    fn signed_area(c: &[OutlinePoint]) -> i64 {
        let c = xy(c);
        let n = c.len();
        (0..n)
            .map(|i| {
                let (x1, y1) = c[i];
                let (x2, y2) = c[(i + 1) % n];
                x1 as i64 * y2 as i64 - x2 as i64 * y1 as i64
            })
            .sum()
    }

    /// Nonzero-winding scan at pixel centres, the way a rasterizer fills the
    /// outline; must reproduce the source bitmap exactly.
    fn rasterize(contours: &[Vec<OutlinePoint>], w: usize, h: usize, descent: i32) -> Vec<bool> {
        let contours: Vec<Vec<(i16, i16)>> = contours.iter().map(|c| xy(c)).collect();
        let mut out = vec![false; w * h];
        for r in 0..h {
            for c in 0..w {
                let px = (c as f64 + 0.5) * UNITS_PER_PIXEL as f64;
                let py = (h as f64 - r as f64 - 0.5 - descent as f64) * UNITS_PER_PIXEL as f64;
                let mut winding = 0i32;
                for contour in &contours {
                    let n = contour.len();
                    for i in 0..n {
                        let (x1, y1) = (contour[i].0 as f64, contour[i].1 as f64);
                        let (x2, y2) =
                            (contour[(i + 1) % n].0 as f64, contour[(i + 1) % n].1 as f64);
                        if (y1 <= py) != (y2 <= py) {
                            let x_at = x1 + (py - y1) / (y2 - y1) * (x2 - x1);
                            if x_at > px {
                                winding += if y2 > y1 { 1 } else { -1 };
                            }
                        }
                    }
                }
                out[r * w + c] = winding != 0;
            }
        }
        out
    }

    fn assert_round_trip(rows: &[&str], descent: i32) {
        let img = bitmap(rows);
        let contours = trace_pixel_outline(&img, descent);
        let got = rasterize(&contours, img.width, img.height, descent);
        for y in 0..img.height {
            for x in 0..img.width {
                assert_eq!(
                    got[y * img.width + x],
                    img.get_usize(x, y),
                    "pixel ({x},{y}) differs for {rows:?}"
                );
            }
        }
    }

    #[test]
    fn single_pixel_is_a_clockwise_square() {
        let contours = trace_pixel_outline(&bitmap(&["#"]), 0);
        assert_eq!(contours.len(), 1);
        let c = &contours[0];
        assert_eq!(c.len(), 4);
        assert!(signed_area(c) < 0, "outer contour must be clockwise: {c:?}");
        let u = UNITS_PER_PIXEL as i16;
        let c = xy(c);
        assert!(c.contains(&(0, 0)) && c.contains(&(u, u)));
    }

    #[test]
    fn ring_has_an_outer_and_a_counter_clockwise_hole() {
        let contours = trace_pixel_outline(&bitmap(&["###", "#.#", "###"]), 0);
        assert_eq!(contours.len(), 2);
        let mut areas: Vec<i64> = contours.iter().map(|c| signed_area(c)).collect();
        areas.sort();
        assert!(areas[0] < 0 && areas[1] > 0, "{areas:?}");
    }

    #[test]
    fn outlines_rasterize_back_to_the_bitmap() {
        assert_round_trip(&["###", "#.#", "###"], 0);
        assert_round_trip(&["#.", ".#"], 0);
        assert_round_trip(&["#..", "#..", "###"], 1);
        assert_round_trip(&[".##.", "#..#", "#..#", ".##.", "...#", "..#."], 2);
        assert_round_trip(&["#.#.#", ".#.#.", "#.#.#"], 0);
        // Pseudo-random speckle: the general case.
        let mut seed = 0x1234_5678u32;
        let mut rows = Vec::new();
        for _ in 0..9 {
            let mut row = String::new();
            for _ in 0..12 {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                row.push(if (seed >> 16) & 1 == 1 { '#' } else { '.' });
            }
            rows.push(row);
        }
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        assert_round_trip(&refs, 0);
    }

    #[test]
    fn descent_shifts_the_outline_below_the_baseline() {
        let c = trace_pixel_outline(&bitmap(&["#", "#"]), 1);
        let ys: Vec<i16> = c[0].iter().map(|p| p.y).collect();
        let u = UNITS_PER_PIXEL as i16;
        assert_eq!(*ys.iter().min().unwrap(), -u);
        assert_eq!(*ys.iter().max().unwrap(), u);
    }

    fn page_with(shapes: &[(&[&str], usize, usize)], w: usize, h: usize) -> BitImage {
        let mut page = BitImage::new(w as u32, h as u32).unwrap();
        for (rows, x0, y0) in shapes {
            for (dy, row) in rows.iter().enumerate() {
                for (dx, ch) in row.bytes().enumerate() {
                    if ch == b'#' {
                        page.set_usize(x0 + dx, y0 + dy, true);
                    }
                }
            }
        }
        page
    }

    const GLYPH_H: &[&str] = &["#...#", "#...#", "#####", "#...#", "#...#"];
    const GLYPH_O: &[&str] = &[".###.", "#...#", "#...#", "#...#", ".###."];
    const GLYPH_P: &[&str] = &[
        "####.", "#...#", "####.", "#....", "#....", "#....", "#....",
    ];

    #[test]
    fn identical_shapes_share_a_prototype_and_lines_are_grouped() {
        // Two lines: "H O H" then "O P" (P descends two pixels below the line).
        let page = page_with(
            &[
                (GLYPH_H, 2, 2),
                (GLYPH_O, 10, 2),
                (GLYPH_H, 18, 2),
                (GLYPH_O, 2, 14),
                (GLYPH_P, 10, 14),
            ],
            40,
            30,
        );
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&page, 300);
        assert_eq!(dict.len(), 3, "H, O, P");
        assert_eq!(runs.glyph_count, 5);
        assert_eq!(runs.lines.len(), 2);

        let first = &runs.lines[0];
        assert_eq!(first.baseline_y, 7);
        let ids: Vec<u32> = first.glyphs.iter().map(|g| g.glyph).collect();
        assert_eq!(ids, vec![0, 1, 0]);
        assert!(first.glyphs.iter().all(|g| g.rise_px == 0));
        assert_eq!(first.glyphs[1].x, 10);

        let second = &runs.lines[1];
        assert_eq!(second.baseline_y, 19, "median of bottoms 19 and 21");
        assert_eq!(second.glyphs[0].glyph, 1, "O reused from line one");
        assert_eq!(second.glyphs[0].rise_px, 0);
        assert_eq!(second.glyphs[1].glyph, 2);
        assert_eq!(second.glyphs[1].rise_px, 0, "P defines its own descent");

        // A second page: the same P set two pixels higher, so its bottom sits
        // on the baseline instead of two below it, must be raised by two.
        let page2 = page_with(
            &[(GLYPH_P, 2, 0), (GLYPH_H, 10, 2), (GLYPH_H, 18, 2)],
            40,
            30,
        );
        let runs2 = dict.process_page(&page2, 300);
        assert_eq!(dict.distinct(), 3, "no new shapes");
        assert_eq!(dict.len(), 4, "the raised P is a position variant");
        assert_eq!(runs2.lines.len(), 1);
        let line = &runs2.lines[0];
        assert_eq!(line.baseline_y, 7);
        let p = line.glyphs.iter().find(|g| g.x == 2).unwrap();
        assert_eq!(p.rise_px, 0, "the variant absorbs the rise");
        assert_eq!(dict.resolve_alias(p.glyph), (2, 0, 0));
        assert_eq!(
            dict.alias_rise(p.glyph),
            2,
            "prototype descent 2, this occurrence 0"
        );
    }

    #[test]
    fn near_identical_shapes_match_and_alignment_shifts_the_placement() {
        let mut noisy: Vec<String> = GLYPH_O.iter().map(|s| s.to_string()).collect();
        noisy[2].replace_range(0..1, "."); // one pixel missing
        let noisy_refs: Vec<&str> = noisy.iter().map(String::as_str).collect();
        let page = page_with(&[(GLYPH_O, 2, 2), (&noisy_refs, 20, 2)], 40, 12);
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&page, 300);
        assert_eq!(dict.len(), 1, "one missing pixel is within tolerance");
        let xs: Vec<i32> = runs.lines[0].glyphs.iter().map(|g| g.x).collect();
        assert_eq!(xs, vec![2, 20]);
    }

    #[test]
    fn a_missing_crossbar_is_a_different_glyph() {
        // An "e"-like ring with a two-pixel crossbar against the same ring
        // without it: the total difference is small but two pixels thick.
        const RING: &[&str] = &[
            "..#####..",
            ".#.....#.",
            "#.......#",
            "#.......#",
            "#.......#",
            "#.......#",
            "#.......#",
            ".#.....#.",
            "..#####..",
        ];
        let mut barred: Vec<String> = RING.iter().map(|s| s.to_string()).collect();
        barred[4] = "#########".into();
        barred[5] = "#########".into();
        let barred_refs: Vec<&str> = barred.iter().map(String::as_str).collect();
        let page = page_with(&[(RING, 2, 2), (&barred_refs, 20, 2)], 40, 14);
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&page, 300);
        assert_eq!(dict.len(), 2, "crossbar must not be absorbed");
        assert_eq!(runs.glyph_count, 2);

        // Edge noise of the same total size, one pixel thick, is absorbed.
        let mut ragged: Vec<String> = RING.iter().map(|s| s.to_string()).collect();
        ragged[2] = "##......#".into();
        ragged[3] = "#.......##".into();
        ragged[6] = ".#......#".into();
        let ragged_refs: Vec<&str> = ragged.iter().map(String::as_str).collect();
        let page = page_with(&[(RING, 2, 2), (&ragged_refs, 20, 2)], 40, 14);
        let mut dict = GlyphDictionary::new();
        dict.process_page(&page, 300);
        assert_eq!(dict.len(), 1, "ragged edges are the same glyph");
    }

    #[test]
    fn prototypes_are_the_majority_of_their_instances() {
        // Three H's, one with a stray pixel: the prototype drops it.
        let mut noisy: Vec<String> = GLYPH_H.iter().map(|s| s.to_string()).collect();
        noisy[0] = "##..#".into();
        let noisy_refs: Vec<&str> = noisy.iter().map(String::as_str).collect();
        let page = page_with(
            &[(GLYPH_H, 2, 2), (&noisy_refs, 10, 2), (GLYPH_H, 18, 2)],
            30,
            10,
        );
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&page, 300);
        assert_eq!(dict.len(), 1);
        assert_eq!(runs.glyph_count, 3);
        let (shape, (ox, oy)) = dict.prototype_shape(0);
        assert!(!shape.get_usize(ox + 1, oy), "stray pixel outvoted");
        assert!(shape.get_usize(ox, oy) && shape.get_usize(ox + 4, oy + 4));
        assert_eq!(ink_bounds(&shape), Some((ox, oy, ox + 4, oy + 4)));

        // The refreshed matcher still matches, and the font renders the H.
        let runs = dict.process_page(&page, 300);
        assert_eq!(dict.len(), 1);
        assert_eq!(runs.glyph_count, 3);
        let font = dict.build_embedded_font().unwrap();
        assert_eq!(font.bbox, [0, 0, 5 * UNITS_PER_PIXEL, 5 * UNITS_PER_PIXEL]);
    }

    #[test]
    fn instances_that_grow_the_shape_keep_the_frame_origin() {
        // Two O's whose right column reaches one pixel further, against one
        // plain O: the majority grows the shape by a column right of the
        // frame, and placements still report the frame's origin.
        let mut wide: Vec<String> = GLYPH_O.iter().map(|s| s.to_string()).collect();
        for row in wide.iter_mut() {
            row.push(if row.ends_with('#') { '#' } else { '.' });
        }
        let wide_refs: Vec<&str> = wide.iter().map(String::as_str).collect();
        let page = page_with(
            &[(GLYPH_O, 2, 2), (&wide_refs, 10, 2), (&wide_refs, 20, 2)],
            30,
            10,
        );
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&page, 300);
        assert_eq!(dict.len(), 1, "one column of growth is edge noise");
        let xs: Vec<i32> = runs.lines[0].glyphs.iter().map(|g| g.x).collect();
        assert_eq!(xs, vec![2, 10, 20]);
        let (shape, (ox, oy)) = dict.prototype_shape(0);
        assert_eq!(ink_bounds(&shape), Some((ox, oy, ox + 5, oy + 4)));
        let font = dict.build_embedded_font().unwrap();
        assert_eq!(
            font.bbox[2],
            6 * UNITS_PER_PIXEL,
            "outline covers the grown column"
        );
    }

    #[test]
    fn one_pixel_baseline_difference_keeps_its_scanned_position() {
        let page = page_with(
            &[
                (GLYPH_H, 2, 2),
                (GLYPH_H, 10, 3),
                (GLYPH_H, 18, 2),
                (GLYPH_H, 26, 2),
            ],
            40,
            12,
        );
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&page, 300);
        assert_eq!(runs.lines.len(), 1);
        assert_eq!(dict.distinct(), 1, "the H outline remains deduplicated");
        assert_eq!(dict.len(), 2, "the lower H gets an exact position variant");
        let line = &runs.lines[0];
        let root = line.glyphs.iter().find(|g| g.x == 2).unwrap().glyph;
        let lower = line.glyphs.iter().find(|g| g.x == 10).unwrap().glyph;
        assert_ne!(lower, root);
        assert_eq!(dict.resolve_alias(lower), (root, 0, 0));
        assert_eq!(
            dict.alias_rise(lower),
            -1,
            "the shared outline is placed one pixel lower"
        );
        dict.record_text(&runs, &[word("HHHH", 2.0, 2.0, 31.0, 8.0)]);
        let entries = dict.to_unicode_entries();
        for glyph in [root, lower] {
            assert!(
                entries.iter().any(|(gid, text)| {
                    u32::from(*gid) == glyph + FIRST_SHAPE_GID && text == "H"
                }),
                "the position variant must share its root's OCR text: {entries:?}"
            );
        }
        assert!(
            line.glyphs.iter().all(|g| g.rise_px == 0),
            "position variants keep the line in one text run: {line:?}"
        );
    }

    #[test]
    fn compound_deduplication_requires_exact_part_offsets() {
        let mut dict = GlyphDictionary::new();
        let stem = dict.insert_prototype(bitmap(GLYPH_STEM), 1, 0);
        let dot = dict.insert_prototype(bitmap(GLYPH_DOT), 1, 0);
        let stem_at_baseline = PlacedGlyph {
            glyph: stem,
            x: 8,
            width: 2,
            rise_px: 0,
        };
        let first = dict.compound_of(
            &[
                stem_at_baseline,
                PlacedGlyph {
                    glyph: dot,
                    x: 8,
                    width: 2,
                    rise_px: -10,
                },
            ],
            20,
        );
        let second = dict.compound_of(
            &[
                stem_at_baseline,
                PlacedGlyph {
                    glyph: dot,
                    x: 8,
                    width: 2,
                    rise_px: -9,
                },
            ],
            20,
        );

        assert_ne!(
            first.glyph, second.glyph,
            "a one-pixel tittle shift is placement, not deduplicated shape"
        );
        assert_eq!(dict.distinct(), 2, "the dot and stem outlines stay shared");
    }

    #[test]
    fn a_new_glyph_takes_the_median_descent_of_its_first_page() {
        // Four P's: the first sits on the baseline (a scan wobble), the other
        // three descend by two. The prototype must adopt two, so the
        // majority renders without a rise and the odd one out is snapped.
        let page = page_with(
            &[
                (GLYPH_H, 2, 2),
                (GLYPH_P, 10, 0),
                (GLYPH_P, 18, 2),
                (GLYPH_P, 26, 2),
                (GLYPH_P, 34, 2),
                (GLYPH_H, 42, 2),
                (GLYPH_H, 50, 2),
                (GLYPH_H, 58, 2),
            ],
            70,
            12,
        );
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&page, 300);
        assert_eq!(dict.distinct(), 2);
        assert_eq!(runs.lines.len(), 1);
        let line = &runs.lines[0];
        assert_eq!(line.baseline_y, 7);
        let ps: Vec<&PlacedGlyph> = line
            .glyphs
            .iter()
            .filter(|g| (10..=34).contains(&g.x))
            .collect();
        assert!(ps.iter().all(|g| g.rise_px == 0), "{ps:?}");
        assert_eq!(
            dict.alias_rise(ps[0].glyph),
            2,
            "the odd one out is a variant two up"
        );
        assert!(
            ps[1..]
                .iter()
                .all(|g| g.glyph == ps[1].glyph && dict.protos[g.glyph as usize].alias.is_none()),
            "the majority use the prototype itself"
        );
        let font = dict.build_embedded_font().unwrap();
        assert_eq!(font.bbox[1], -2 * UNITS_PER_PIXEL, "P descends two pixels");
    }

    #[test]
    fn finalize_folds_matching_clusters_into_composites() {
        const RING: &[&str] = &[
            "..#####..",
            ".#.....#.",
            "#.......#",
            "#.......#",
            "#.......#",
            "#.......#",
            "#.......#",
            ".#.....#.",
            "..#####..",
        ];
        // The same ring inside a frame one pixel larger on the left and top,
        // as a cluster whose first instance carried a speck there.
        let mut padded: Vec<String> = RING.iter().map(|r| format!(".{r}")).collect();
        padded.insert(0, ".".repeat(10));
        let padded_refs: Vec<&str> = padded.iter().map(String::as_str).collect();
        let mut barred: Vec<String> = RING.iter().map(|s| s.to_string()).collect();
        barred[4] = "#########".into();
        barred[5] = "#########".into();
        let barred_refs: Vec<&str> = barred.iter().map(String::as_str).collect();

        let mut dict = GlyphDictionary::new();
        let heavy = dict.insert_prototype(bitmap(RING), 10, 0);
        let light = dict.insert_prototype(bitmap(&padded_refs), 3, 1);
        let other = dict.insert_prototype(bitmap(&barred_refs), 5, 0);
        dict.finalize();
        assert_eq!(dict.distinct(), 2);
        assert_eq!(
            dict.resolve_alias(light),
            (heavy, 1, 1),
            "root frame origin at (1,1)"
        );
        assert!(
            dict.protos[other as usize].alias.is_none(),
            "crossbar stays distinct"
        );
        assert!(dict.protos[heavy as usize].alias.is_none());

        let font = dict.build_embedded_font().unwrap();
        assert_eq!(font.cid_widths.as_ref().unwrap().len(), 5);
        let face = lege_pdf_read::read_face_metrics(&font.data, 0).expect("font parses");
        assert_eq!(face.num_glyphs, 5);
        // The composite must put the ring where the light frame had it:
        // one pixel right of its origin, and with descent 1 against the
        // root's 0 the ring's bottom is one pixel below the baseline. The
        // font bbox is therefore the ring's own bounds (fitted curves may
        // bulge a pixel past the ink) grown by one pixel right and down.
        let mut alone = GlyphDictionary::new();
        alone.insert_prototype(bitmap(RING), 10, 0);
        let ring = alone.build_embedded_font().unwrap().bbox;
        assert_eq!(
            font.bbox,
            [
                ring[0],
                ring[1] - UNITS_PER_PIXEL,
                ring[2] + UNITS_PER_PIXEL,
                ring[3]
            ]
        );
    }

    #[test]
    fn embedded_font_has_one_glyph_per_prototype_plus_notdef() {
        let page = page_with(&[(GLYPH_H, 2, 2), (GLYPH_O, 10, 2)], 20, 10);
        let mut dict = GlyphDictionary::new();
        dict.process_page(&page, 300);
        let font = dict.build_embedded_font().unwrap();
        assert!(font.symbolic);
        assert_eq!(font.to_unicode, ToUnicode::None);
        let widths = font.cid_widths.as_ref().unwrap();
        assert_eq!(widths.len(), 4);
        assert_eq!(widths[0], 0);
        assert_eq!(widths[SPACE_GID as usize], SPACE_ADVANCE);
        // H is 5 wide and O follows it 3 pixels on: advance 8.
        assert_eq!(widths[2], 8 * UNITS_PER_PIXEL as u16);
        // O ends the line; it takes the page's typical gap.
        assert_eq!(widths[3], 8 * UNITS_PER_PIXEL as u16);
        let face = lege_pdf_read::read_face_metrics(&font.data, 0).expect("font parses");
        assert_eq!(face.num_glyphs, 4);
        assert_eq!(face.units_per_em, UNITS_PER_EM);
        assert_eq!(font.bbox[3], 5 * UNITS_PER_PIXEL);
    }

    const GLYPH_BAR: &[&str] = &["##"; 20];
    const GLYPH_STEM: &[&str] = &["##"; 8];
    const GLYPH_DOT: &[&str] = &["##", "##"];
    const GLYPH_BLOCK: &[&str] = &["######"; 10];

    /// "l l i ." on one baseline (row 20): two bars, a stem with a tittle
    /// two pixels above it, and a full stop.
    fn word_page() -> BitImage {
        page_with(
            &[
                (GLYPH_BAR, 2, 0),
                (GLYPH_BAR, 5, 0),
                (GLYPH_STEM, 8, 12),
                (GLYPH_DOT, 8, 8),
                (GLYPH_DOT, 14, 18),
            ],
            24,
            24,
        )
    }

    fn word(text: &str, x0: f32, y0: f32, x1: f32, y1: f32) -> TextWord {
        TextWord {
            text: text.to_string(),
            x0,
            y0,
            x1,
            y1,
        }
    }

    /// The glyph id placed at pixel column `x` on the page.
    fn glyph_at(runs: &PageGlyphRuns, x: i32) -> Vec<u32> {
        let mut ids: Vec<u32> = runs
            .lines
            .iter()
            .flat_map(|l| l.glyphs.iter())
            .filter(|g| g.x == x)
            .map(|g| g.glyph)
            .collect();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn stacked_pieces_become_one_compound_glyph() {
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&word_page(), 72);
        let i = glyph_at(&runs, 8);
        assert_eq!(i.len(), 1, "stem and tittle placed as one glyph: {i:?}");
        let compound = &dict.protos[i[0] as usize];
        assert_eq!(compound.parts.len(), 2);
        assert_eq!((compound.frame_w, compound.frame_h), (2, 12));
        assert_eq!(compound.descent, Some(0));
        assert_eq!(dict.distinct(), 3, "bar, dot and stem own outlines");
        assert_eq!(dict.len(), 4);
        assert!(runs.lines[0].glyphs.iter().all(|g| g.rise_px == 0));
        // The same page again reuses the compound.
        dict.process_page(&word_page(), 72);
        assert_eq!(dict.len(), 4);
        let font = dict.build_embedded_font().unwrap();
        let face = lege_pdf_read::read_face_metrics(&font.data, 0).unwrap();
        assert_eq!(face.num_glyphs, 6);
    }

    #[test]
    fn a_shape_far_off_its_baseline_position_is_a_separate_glyph() {
        // Three bars, a dot floating ten pixels up with nothing under it,
        // and a full stop: the two dots share a shape but not a position.
        let page = page_with(
            &[
                (GLYPH_BAR, 2, 0),
                (GLYPH_BAR, 5, 0),
                (GLYPH_BAR, 8, 0),
                (GLYPH_DOT, 14, 8),
                (GLYPH_DOT, 20, 18),
            ],
            24,
            24,
        );
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&page, 72);
        let dots: Vec<u32> = [14, 20].iter().flat_map(|&x| glyph_at(&runs, x)).collect();
        assert_eq!(dots.len(), 2);
        assert_ne!(dots[0], dots[1], "the two dots get different glyph ids");
        // One is the root, the other its position variant ten pixels away.
        let (a, b) = (
            &dict.protos[dots[0] as usize],
            &dict.protos[dots[1] as usize],
        );
        assert!(a.alias.is_some() != b.alias.is_some());
        assert_eq!((a.descent.unwrap() - b.descent.unwrap()).abs(), 10);
        assert!(
            runs.lines
                .iter()
                .flat_map(|l| l.glyphs.iter())
                .all(|g| g.rise_px == 0),
            "variants absorb the rise"
        );
        let variant = if a.alias.is_some() { dots[0] } else { dots[1] };
        assert_eq!(dict.alias_rise(variant).abs(), 10);
        assert_eq!(dict.distinct(), 2, "bar and dot own outlines");
        let font = dict.build_embedded_font().unwrap();
        let face = lege_pdf_read::read_face_metrics(&font.data, 0).unwrap();
        assert_eq!(face.num_glyphs, 5);
    }

    #[test]
    fn recognized_words_vote_text_onto_glyphs() {
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&word_page(), 72);
        dict.record_text(
            &runs,
            &[
                word("lli", 2.0, 0.0, 10.0, 20.0),
                word(".", 14.0, 18.0, 16.0, 20.0),
            ],
        );
        let entries = dict.to_unicode_entries();
        let text_of = |x: i32, frame_h: usize| -> String {
            let g = glyph_at(&runs, x)
                .into_iter()
                .find(|&g| dict.protos[g as usize].frame_h == frame_h)
                .unwrap();
            entries
                .iter()
                .find(|(gid, _)| *gid == g as u16 + FIRST_SHAPE_GID as u16)
                .map(|(_, t)| t.clone())
                .unwrap_or_else(|| panic!("glyph at x = {x} unmapped: {entries:?}"))
        };
        assert_eq!(text_of(2, 20), "l");
        assert_eq!(text_of(8, 12), "i", "the compound carries the i");
        assert_eq!(text_of(14, 2), ".");
        let font = dict.build_embedded_font().unwrap();
        assert!(matches!(font.to_unicode, ToUnicode::Custom(_)));
        assert_eq!(dict.mapped(), 4, "bar, compound, dot and the space");
        assert_eq!(entries[0], (SPACE_GID, " ".to_string()));
    }

    #[test]
    fn a_touching_pair_votes_for_both_letters() {
        let page = page_with(&[(GLYPH_BLOCK, 2, 2)], 12, 14);
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&page, 72);
        dict.record_text(&runs, &[word("fi", 2.0, 2.0, 8.0, 12.0)]);
        assert_eq!(
            dict.to_unicode_entries(),
            vec![(SPACE_GID, " ".to_string()), (2, "fi".to_string())]
        );
    }

    #[test]
    fn a_broken_letter_votes_for_it_once() {
        // Two pieces under one recognized character: the larger carries it.
        let page = page_with(&[(GLYPH_BLOCK, 2, 2), (GLYPH_DOT, 10, 10)], 16, 14);
        let mut dict = GlyphDictionary::new();
        let runs = dict.process_page(&page, 72);
        dict.record_text(&runs, &[word("m", 2.0, 2.0, 12.0, 12.0)]);
        let entries = dict.to_unicode_entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1], (2, "m".to_string()));
        assert_eq!(entries[2], (3, String::new()));
    }

    #[test]
    fn word_characters_fold_marks_and_drop_spaces() {
        assert_eq!(word_characters("e\u{301}a b"), vec!["e\u{301}", "a", "b"]);
        assert!(word_characters(" ").is_empty());
    }

    /// `rows` smeared one pixel to the right: the same type inked heavier
    /// (a sixth more ink on a ring of three-pixel strokes).
    fn heavier(rows: &[&str]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                let b = row.as_bytes();
                (0..b.len() + 1)
                    .map(|x| {
                        let ink = (x < b.len() && b[x] == b'#') || (x > 0 && b[x - 1] == b'#');
                        if ink { '#' } else { '.' }
                    })
                    .collect()
            })
            .collect()
    }

    /// A 24-pixel `o` with three-pixel strokes.
    const BIG_O: [&str; 24] = [
        "........########........",
        "......############......",
        "....################....",
        "...######......######...",
        "..#####..........#####..",
        "..####............####..",
        ".####..............####.",
        ".###................###.",
        ".###................###.",
        "###..................###",
        "###..................###",
        "###..................###",
        "###..................###",
        "###..................###",
        "###..................###",
        ".###................###.",
        ".###................###.",
        ".####..............####.",
        "..####............####..",
        "..#####..........#####..",
        "...######......######...",
        "....################....",
        "......############......",
        "........########........",
    ];

    /// `BIG_O` with an opening on the right: a `c`.
    fn glyph_c() -> Vec<String> {
        BIG_O
            .iter()
            .enumerate()
            .map(|(y, row)| {
                if (7..17).contains(&y) {
                    format!("{}....", &row[..20])
                } else {
                    row.to_string()
                }
            })
            .collect()
    }

    fn strs(rows: &[String]) -> Vec<&str> {
        rows.iter().map(String::as_str).collect()
    }

    #[test]
    fn counters_are_counted_not_guessed() {
        // A ring encloses one counter; the same ring cut open encloses none;
        // two rings stacked enclose two.
        assert_eq!(holes(&bitmap(&BIG_O)), 1);
        assert_eq!(holes(&bitmap(&strs(&glyph_c()))), 0);
        let mut two = BIG_O.iter().map(|r| r.to_string()).collect::<Vec<_>>();
        two.extend(BIG_O.iter().map(|r| r.to_string()));
        assert_eq!(holes(&bitmap(&strs(&two))), 2);
        assert_eq!(holes(&bitmap(&["####", "####"])), 0);
    }

    #[test]
    fn a_tolerant_fold_never_opens_or_closes_a_counter() {
        // A ring three pixels thick — strokes with an interior, so the
        // tolerant gate is allowed to look at it — and the same ring with a
        // two-row opening cut in its side. The gate forgives the opening,
        // being everywhere within a pixel of the ring's own ink, but one
        // shape encloses a counter and the other does not, and no change of
        // weight opens a counter.
        let ring: Vec<String> = (0..12)
            .map(|y| {
                (0..12)
                    .map(|x| {
                        let edge = x < 3 || y < 3 || x > 8 || y > 8;
                        if edge { '#' } else { '.' }
                    })
                    .collect()
            })
            .collect();
        let mut open = ring.clone();
        for row in open.iter_mut().take(7).skip(5) {
            row.replace_range(9..12, "...");
        }
        assert!(has_stroke_interior(&bitmap(&strs(&ring))));
        assert_eq!(holes(&bitmap(&strs(&ring))), 1);
        assert_eq!(holes(&bitmap(&strs(&open))), 0);
        let diff = bitrows::diff(
            &bitmap(&strs(&open)),
            &bitmap(&strs(&ring)),
            0,
            0,
            u32::MAX,
            Some(u32::MAX),
        )
        .unwrap();
        let ink = bitmap(&strs(&ring)).count_ones() as u32;
        assert!(
            Gate::Tolerant.accept(diff, ink).is_some(),
            "the gate itself forgives the opening"
        );

        let mut dict = GlyphDictionary::new();
        let closed = dict.insert_prototype(bitmap(&strs(&ring)), 20, 0);
        let broken = dict.insert_prototype(bitmap(&strs(&open)), 20, 0);
        dict.finalize();
        assert_ne!(
            dict.resolve_alias(broken).0,
            closed,
            "the counters keep them apart"
        );
    }

    #[test]
    fn a_heavier_impression_folds_into_its_type_but_a_c_stays_off_the_o() {
        let mut dict = GlyphDictionary::new();
        let o = dict.insert_prototype(bitmap(&BIG_O), 20, 0);
        let heavy = heavier(&BIG_O);
        let bold = dict.insert_prototype(bitmap(&strs(&heavy)), 1, 0);
        let c = glyph_c();
        let c_idx = dict.insert_prototype(bitmap(&strs(&c)), 1, 0);
        // The strict gate keeps all three apart: a pixel of weight all
        // round is a large share of the ink.
        dict.finalize();
        assert_eq!(
            dict.resolve_alias(bold).0,
            o,
            "heavier impression folds into its type"
        );
        assert_eq!(dict.resolve_alias(c_idx).0, c_idx, "a c is not an o");
        assert_eq!(dict.distinct(), 2);
    }

    #[test]
    fn one_pixel_strokes_never_match_tolerantly() {
        // A ring two pixels thick and a `c` cut from it with a three-row
        // opening. The strict gate rejects the pair (the opening is two
        // pixels thick); the tolerant gate would forgive all but the
        // opening's middle row, and must not get to try.
        let ring: Vec<String> = (0..10)
            .map(|y| {
                (0..10)
                    .map(|x| {
                        let edge = x < 2 || y < 2 || x > 7 || y > 7;
                        if edge { '#' } else { '.' }
                    })
                    .collect()
            })
            .collect();
        let mut open = ring.clone();
        for row in open.iter_mut().take(6).skip(3) {
            row.replace_range(8..10, "..");
        }
        assert!(!has_stroke_interior(&bitmap(&strs(&ring))));
        let diff = bitrows::diff(
            &bitmap(&strs(&open)),
            &bitmap(&strs(&ring)),
            0,
            0,
            u32::MAX,
            Some(u32::MAX),
        )
        .unwrap();
        assert!(Gate::Strict.accept(diff, 64).is_none());
        assert!(Gate::Tolerant.accept(diff, 64).is_some());
        let mut dict = GlyphDictionary::new();
        let o = dict.insert_prototype(bitmap(&strs(&ring)), 20, 0);
        let c = dict.insert_prototype(bitmap(&strs(&open)), 1, 0);
        dict.finalize();
        assert_ne!(dict.resolve_alias(c).0, o);
        assert!(has_stroke_interior(&bitmap(&BIG_O)));
    }

    #[test]
    fn session_accepts_image_convention_pixels() {
        let session = GlyphFontSession::new();
        let (w, h) = (12usize, 8usize);
        let mut pixels = vec![255u8; w * h];
        for (dy, row) in GLYPH_H.iter().enumerate() {
            for (dx, ch) in row.bytes().enumerate() {
                if ch == b'#' {
                    pixels[(dy + 1) * w + dx + 1] = 0;
                }
            }
        }
        let runs = session.process_page_pixels(&pixels, w, h, 300).unwrap();
        assert_eq!(runs.glyph_count, 1);
        assert_eq!(runs.bank, 0);
        assert_eq!(session.stats(), (1, 1, 1, 0));
        session.build_embedded_fonts().unwrap();
    }

    #[test]
    fn a_full_dictionary_sends_the_next_page_to_the_next_font() {
        // A page of one component reserves that component plus the fixed
        // headroom, so a dictionary of exactly that size takes the first page
        // and has no room for the second once the first has left a shape in
        // it.
        let session = GlyphFontSession::with_prototype_cap(2 + 64);
        let (w, h) = (12usize, 8usize);
        let mut pixels = vec![255u8; w * h];
        for (dy, row) in GLYPH_H.iter().enumerate() {
            for (dx, ch) in row.bytes().enumerate() {
                if ch == b'#' {
                    pixels[(dy + 1) * w + dx + 1] = 0;
                }
            }
        }
        let first = session.process_page_pixels(&pixels, w, h, 300).unwrap();
        let second = session.process_page_pixels(&pixels, w, h, 300).unwrap();
        assert_eq!(first.bank, 0);
        assert_eq!(second.bank, 1);
        assert_eq!(session.banks(), 2);
        // Each bank is a font of its own, and each holds the shape its pages
        // drew, so the glyph ids on both pages resolve.
        let fonts = session.build_embedded_fonts().unwrap();
        assert_eq!(fonts.len(), 2);
    }
}
