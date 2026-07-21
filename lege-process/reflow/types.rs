//! Core data types for the RasterReflow pipeline.
//!
//! RasterReflow is a raster-first reflow mode for scanned / image-only books.
//! It detects readable visual structure (columns, rows, graphical words) purely
//! from pixels, enlarges the effective text size, removes wasted whitespace, and
//! re-composes new output pages while preserving reading order.
//!
//! These types are intentionally decoupled from any single pipeline location.
//! Geometry uses integer pixel rectangles ([`PxRect`]); scale / transform math
//! uses floats. Source rectangles and a [`SourceMap`] are kept all the way to
//! final composition so a native-text / hOCR / OCR layer can be re-projected
//! onto the reflowed output later.
//!
//! See `RASTER_REFLOW_PLAN.md` for the full design rationale.

#![allow(dead_code)] // Scaffolding: not yet wired into the live pipeline.

use crate::types::ContentCategory;

/// An axis-aligned rectangle in integer pixel coordinates.
///
/// Origin is top-left, `x`/`y` are the inclusive top-left corner, `w`/`h` the
/// extents. An empty rect has `w == 0 || h == 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PxRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl PxRect {
    #[inline]
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Build from inclusive-exclusive corner coordinates `[x1, y1, x2, y2)`.
    /// Returns an empty rect if the corners are inverted.
    #[inline]
    pub fn from_xyxy(x1: u32, y1: u32, x2: u32, y2: u32) -> Self {
        Self {
            x: x1.min(x2),
            y: y1.min(y2),
            w: x2.saturating_sub(x1),
            h: y2.saturating_sub(y1),
        }
    }

    /// Convert a YOLO-style float bbox `[x1, y1, x2, y2]` into a clamped pixel
    /// rect inside `(width, height)`. Used to ingest [`crate::engine::Detection`]
    /// boxes as coarse region hints.
    pub fn from_f32_bbox(bbox: [f32; 4], width: u32, height: u32) -> Self {
        let clamp_x = |v: f32| v.max(0.0).min(width as f32).round() as u32;
        let clamp_y = |v: f32| v.max(0.0).min(height as f32).round() as u32;
        let x1 = clamp_x(bbox[0]);
        let y1 = clamp_y(bbox[1]);
        let x2 = clamp_x(bbox[2]);
        let y2 = clamp_y(bbox[3]);
        Self::from_xyxy(x1, y1, x2, y2)
    }

    #[inline]
    pub const fn right(&self) -> u32 {
        self.x + self.w
    }

    #[inline]
    pub const fn bottom(&self) -> u32 {
        self.y + self.h
    }

    #[inline]
    pub const fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    #[inline]
    pub fn center_x(&self) -> u32 {
        self.x + self.w / 2
    }

    #[inline]
    pub fn center_y(&self) -> u32 {
        self.y + self.h / 2
    }

    /// True if `other` overlaps this rect (positive-area intersection).
    pub fn intersects(&self, other: &PxRect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// True if `p` lies inside the rect.
    pub fn contains_point(&self, px: u32, py: u32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    /// Intersection rectangle, or `None` if disjoint.
    pub fn intersection(&self, other: &PxRect) -> Option<PxRect> {
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = self.right().min(other.right());
        let y2 = self.bottom().min(other.bottom());
        if x1 < x2 && y1 < y2 {
            Some(PxRect::from_xyxy(x1, y1, x2, y2))
        } else {
            None
        }
    }

    /// Smallest rectangle containing both inputs.
    pub fn union(&self, other: &PxRect) -> PxRect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x1 = self.x.min(other.x);
        let y1 = self.y.min(other.y);
        let x2 = self.right().max(other.right());
        let y2 = self.bottom().max(other.bottom());
        PxRect::from_xyxy(x1, y1, x2, y2)
    }

    /// Fraction of `self`'s area covered by the intersection with `other`.
    pub fn overlap_fraction(&self, other: &PxRect) -> f32 {
        if self.is_empty() {
            return 0.0;
        }
        match self.intersection(other) {
            Some(i) => i.area() as f32 / self.area() as f32,
            None => 0.0,
        }
    }
}

/// A rendered source page plus the context needed to map regions back to the
/// original document coordinate space.
#[derive(Clone)]
pub struct SourcePageImage {
    /// Zero-based page index in the source document.
    pub page_index: usize,
    /// Grayscale render used for all geometric analysis.
    pub gray: image::GrayImage,
    /// Optional full-color / full-tone render kept for final visual extraction.
    pub rgb: Option<image::RgbImage>,
    /// DPI the page was rasterized at.
    pub render_dpi: f32,
    /// Original PDF page size in points (1/72 inch). Used to map pixel rects
    /// back to PDF user space for text-layer preservation.
    pub page_pts: (f32, f32),
}

impl SourcePageImage {
    pub fn dimensions(&self) -> (u32, u32) {
        self.gray.dimensions()
    }

    pub fn width(&self) -> u32 {
        self.gray.width()
    }

    pub fn height(&self) -> u32 {
        self.gray.height()
    }

    /// Full-page rectangle in source pixel coordinates.
    pub fn page_rect(&self) -> PxRect {
        let (w, h) = self.dimensions();
        PxRect::new(0, 0, w, h)
    }

    /// Pixels-per-point on each axis, for mapping source pixel rects back to PDF
    /// user-space points.
    pub fn px_per_pt(&self) -> (f32, f32) {
        let (w, h) = self.dimensions();
        let (pw, ph) = self.page_pts;
        let sx = if pw > 0.0 { w as f32 / pw } else { 1.0 };
        let sy = if ph > 0.0 { h as f32 / ph } else { 1.0 };
        (sx, sy)
    }
}

/// A 1-bit foreground (ink) mask over a page or sub-region.
///
/// `true` means "ink" (foreground / dark pixel). Stored row-major as a dense
/// `Vec<bool>`; for MVP simplicity this favours clarity over bit-packing. The
/// `origin` records where this mask sits within the full source page so derived
/// rectangles can be reported in page coordinates.
#[derive(Clone)]
pub struct InkMask {
    pub width: u32,
    pub height: u32,
    /// Top-left of this mask within the source page (page coordinates).
    pub origin: (u32, u32),
    pub bits: Vec<bool>,
}

impl InkMask {
    pub fn new(width: u32, height: u32, origin: (u32, u32)) -> Self {
        Self {
            width,
            height,
            origin,
            bits: vec![false; (width as usize) * (height as usize)],
        }
    }

    #[inline]
    pub fn idx(&self, x: u32, y: u32) -> usize {
        (y as usize) * (self.width as usize) + (x as usize)
    }

    #[inline]
    pub fn get(&self, x: u32, y: u32) -> bool {
        self.bits[self.idx(x, y)]
    }

    #[inline]
    pub fn set(&mut self, x: u32, y: u32, v: bool) {
        let i = self.idx(x, y);
        self.bits[i] = v;
    }

    /// Total number of ink pixels.
    pub fn ink_count(&self) -> u64 {
        self.bits.iter().filter(|&&b| b).count() as u64
    }

    /// Fraction of pixels that are ink (0.0..=1.0).
    pub fn ink_ratio(&self) -> f32 {
        let total = self.bits.len();
        if total == 0 {
            0.0
        } else {
            self.ink_count() as f32 / total as f32
        }
    }

    /// Translate a local rect (relative to this mask) into page coordinates.
    pub fn to_page_rect(&self, local: PxRect) -> PxRect {
        PxRect::new(
            local.x + self.origin.0,
            local.y + self.origin.1,
            local.w,
            local.h,
        )
    }
}

/// Which axis a [`ProjectionProfile`] runs along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionAxis {
    /// One bucket per row (ink summed across X); used for row detection.
    Horizontal,
    /// One bucket per column (ink summed across Y); used for column / word gaps.
    Vertical,
}

/// An ink histogram along one axis of a mask.
#[derive(Debug, Clone)]
pub struct ProjectionProfile {
    pub axis: ProjectionAxis,
    /// Ink count per bucket. Length is page-region height (Horizontal) or width
    /// (Vertical).
    pub values: Vec<u32>,
    /// Offset of bucket 0 within the source page along this axis.
    pub offset: u32,
}

impl ProjectionProfile {
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn max(&self) -> u32 {
        self.values.iter().copied().max().unwrap_or(0)
    }
}

/// Coarse classification of a detected source region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Ordinary prose / body text — eligible for row+word reflow.
    Text,
    /// Photographic, illustrative, or scanned-image block. Preserved atomically
    /// and composited from the RGB source without binarization.
    Figure,
    /// Tabular block — preserved atomically (no word reflow in MVP).
    Table,
    /// Could not be confidently classified.
    Unknown,
}

impl RegionKind {
    /// Map a layout-model [`ContentCategory`] hint to a region kind.
    pub fn from_content_category(cat: ContentCategory) -> Self {
        match cat {
            ContentCategory::Text => RegionKind::Text,
            ContentCategory::Image => RegionKind::Figure,
            ContentCategory::Table => RegionKind::Table,
            ContentCategory::Abandon => RegionKind::Unknown,
        }
    }

    /// True if this region should be reflowed into rows + words rather than
    /// preserved as an atomic block.
    ///
    /// Only `Text` is reflowed. `Figure` is preserved atomically so photographs
    /// and illustrations are composited from the RGB source without binarization.
    /// `Table` stays atomic to preserve grid structure.
    pub fn is_reflowable(&self) -> bool {
        matches!(self, RegionKind::Text)
    }
}

/// A candidate content region in source-page pixel coordinates.
#[derive(Debug, Clone)]
pub struct ReflowRegion {
    pub id: u32,
    /// Source-page rectangle.
    pub rect: PxRect,
    /// Explicit reading-order index (lower = earlier). Must be assigned and is
    /// asserted monotonic by tests.
    pub reading_order: usize,
    pub kind: RegionKind,
    /// Detection confidence (0.0..=1.0).
    pub confidence: f32,
    /// Optional originating layout-model label, when a hint drove this region.
    pub onnx_label: Option<ContentCategory>,
}

/// A detected text row within a region.
#[derive(Debug, Clone)]
pub struct TextRow {
    /// Source-page rectangle of the row band.
    pub rect: PxRect,
    /// Estimated text height (ink extent) in pixels — drives output scale.
    pub text_height: u32,
    /// Whitespace gap (px) between the previous row's bottom and this row's top
    /// within the same region. `None` for the first row in a region.
    pub gap_above: Option<u32>,
    /// ID of the region this row belongs to.
    pub region_id: u32,
    /// Reading-order index across the whole page.
    pub reading_order: usize,
}

/// A graphical "word": a horizontal slice of a row separated from neighbours by
/// a blank-column gap. Carries no recognized text — it is a bitmap span.
#[derive(Debug, Clone)]
pub struct WordSpan {
    /// Source-page rectangle of the word.
    pub rect: PxRect,
    /// Index of the owning row within its region's row list.
    pub row_index: usize,
    /// Order of this word within its row (left-to-right).
    pub order_in_row: usize,
}

/// A reference back to a source-page rectangle, preserved through composition so
/// text / hOCR / OCR layers can be re-projected onto the reflowed output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRef {
    pub page_index: usize,
    pub rect: PxRect,
}

/// One element of the reading-order flow stream produced before composition.
#[derive(Debug, Clone)]
pub enum FlowItem {
    /// A graphical word bitmap to be packed onto an output line.
    WordBitmap { src: SourceRef, height: u32 },
    /// A horizontal run (e.g. a whole row kept intact) placed as one unit.
    RunBitmap { src: SourceRef, height: u32 },
    /// A figure / table / atomic block scaled to fit but never split.
    Figure { src: SourceRef, kind: RegionKind },
    /// Explicit inter-row / inter-paragraph vertical gap (already normalized).
    Gap { px: u32 },
    /// Forces the current output line to flush (end of a source row).
    LineBreak,
    /// Boundary between regions; a safe pagination point.
    RegionBreak,
    /// Soft hint that a page break here is preferable (e.g. before a heading).
    PageBreakHint,
}

/// What kind of content a [`PlacedItem`] holds, for downstream rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacedKind {
    Word,
    Run,
    Figure,
    Table,
}

/// A source rectangle placed at an output rectangle on a specific output page.
#[derive(Debug, Clone)]
pub struct PlacedItem {
    /// Output rectangle (output-page pixel coordinates).
    pub out_rect: PxRect,
    /// Source rectangle this was lifted from.
    pub src: SourceRef,
    /// Scale applied from source to output (out_rect.w / src.rect.w, approx).
    pub scale: f32,
    pub kind: PlacedKind,
}

/// A planned output page: a set of placements over a fixed canvas. The actual
/// raster is rendered later by the output integration stage.
#[derive(Debug, Clone)]
pub struct ReflowPage {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub items: Vec<PlacedItem>,
}

impl ReflowPage {
    pub fn new(index: usize, width: u32, height: u32) -> Self {
        Self {
            index,
            width,
            height,
            items: Vec::new(),
        }
    }

    pub fn bounds(&self) -> PxRect {
        PxRect::new(0, 0, self.width, self.height)
    }
}

/// Aggregated source→output mapping for the whole reflowed document. Lets a
/// later stage re-project native text / hOCR / OCR boxes onto the new pages.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    pub placements: Vec<Placement>,
}

/// A single source→output placement record.
#[derive(Debug, Clone)]
pub struct Placement {
    pub out_page: usize,
    pub out_rect: PxRect,
    pub src: SourceRef,
}

impl SourceMap {
    pub fn push(&mut self, out_page: usize, out_rect: PxRect, src: SourceRef) {
        self.placements.push(Placement {
            out_page,
            out_rect,
            src,
        });
    }

    /// All placements originating from a given source page.
    pub fn for_source_page(&self, page_index: usize) -> impl Iterator<Item = &Placement> {
        self.placements
            .iter()
            .filter(move |p| p.src.page_index == page_index)
    }
}

/// Confidence / quality signals for a reflowed page, surfaced to logs and used
/// to decide whether to fall back to conservative crop/resize.
#[derive(Debug, Clone, Copy)]
pub struct ReflowConfidence {
    /// Number of text rows detected on the source page.
    pub row_count: usize,
    /// Number of reflowable text regions.
    pub text_region_count: usize,
    /// Coefficient of variation of row heights (lower = more regular prose).
    pub row_height_cv: f32,
    /// Overall score in 0.0..=1.0 combining the above.
    pub score: f32,
}

impl ReflowConfidence {
    /// True when the page looks like ordinary reflowable prose.
    pub fn is_confident(&self, threshold: f32) -> bool {
        self.score >= threshold
    }
}

/// Final plan for a reflowed document: output pages + the source map + an
/// overall confidence summary.
#[derive(Debug, Clone, Default)]
pub struct ReflowDocument {
    pub pages: Vec<ReflowPage>,
    pub source_map: SourceMap,
    /// Per-source-page confidence, indexed by source page order of submission.
    pub confidence: Vec<ReflowConfidence>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_from_xyxy_and_extents() {
        let r = PxRect::from_xyxy(10, 20, 40, 60);
        assert_eq!(r, PxRect::new(10, 20, 30, 40));
        assert_eq!(r.right(), 40);
        assert_eq!(r.bottom(), 60);
        assert_eq!(r.area(), 30 * 40);
    }

    #[test]
    fn rect_from_xyxy_inverted_is_empty() {
        let r = PxRect::from_xyxy(40, 60, 10, 20);
        assert!(r.is_empty());
    }

    #[test]
    fn rect_intersection_and_union() {
        let a = PxRect::new(0, 0, 10, 10);
        let b = PxRect::new(5, 5, 10, 10);
        assert!(a.intersects(&b));
        assert_eq!(a.intersection(&b), Some(PxRect::new(5, 5, 5, 5)));
        assert_eq!(a.union(&b), PxRect::new(0, 0, 15, 15));

        let c = PxRect::new(100, 100, 5, 5);
        assert!(!a.intersects(&c));
        assert_eq!(a.intersection(&c), None);
    }

    #[test]
    fn rect_overlap_fraction() {
        let a = PxRect::new(0, 0, 10, 10);
        let b = PxRect::new(0, 0, 5, 10); // half of a
        assert!((a.overlap_fraction(&b) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn rect_from_f32_bbox_clamps() {
        let r = PxRect::from_f32_bbox([-5.0, -5.0, 50.0, 80.0], 40, 60);
        assert_eq!(r, PxRect::from_xyxy(0, 0, 40, 60));
    }

    #[test]
    fn ink_mask_get_set_and_counts() {
        let mut m = InkMask::new(4, 4, (2, 3));
        assert_eq!(m.ink_count(), 0);
        m.set(1, 1, true);
        m.set(2, 2, true);
        assert!(m.get(1, 1));
        assert_eq!(m.ink_count(), 2);
        assert!((m.ink_ratio() - 2.0 / 16.0).abs() < 1e-6);
        // local -> page coordinate translation uses origin
        assert_eq!(
            m.to_page_rect(PxRect::new(1, 1, 2, 2)),
            PxRect::new(3, 4, 2, 2)
        );
    }

    #[test]
    fn region_kind_mapping() {
        assert_eq!(
            RegionKind::from_content_category(ContentCategory::Text),
            RegionKind::Text
        );
        assert_eq!(
            RegionKind::from_content_category(ContentCategory::Image),
            RegionKind::Figure
        );
        assert!(RegionKind::Text.is_reflowable());
        assert!(!RegionKind::Figure.is_reflowable());
        assert!(!RegionKind::Table.is_reflowable());
    }

    #[test]
    fn source_map_filters_by_page() {
        let mut sm = SourceMap::default();
        sm.push(
            0,
            PxRect::new(0, 0, 10, 10),
            SourceRef {
                page_index: 0,
                rect: PxRect::new(0, 0, 5, 5),
            },
        );
        sm.push(
            1,
            PxRect::new(0, 0, 10, 10),
            SourceRef {
                page_index: 2,
                rect: PxRect::new(0, 0, 5, 5),
            },
        );
        assert_eq!(sm.for_source_page(0).count(), 1);
        assert_eq!(sm.for_source_page(2).count(), 1);
        assert_eq!(sm.for_source_page(5).count(), 0);
    }
}
