//! Composition: reading-order-constrained greedy flow.
//!
//! Consumes the [`FlowItem`] stream and produces an ordered list of
//! [`StripBlock`]s — a device-width "master strip" of text lines, spacers, and
//! atomic figure blocks. Words are packed onto a line until the next would
//! overflow the content width, then the line flushes; vertical gaps are
//! normalized into spacer blocks; figures are scaled to fit and never split.
//!
//! The cardinal rule is **reading order beats packing efficiency** — items are
//! emitted strictly in stream order. (This is exactly why a treemap/squarify
//! layout is unsuitable here; see `RASTER_REFLOW_PLAN.md`.)
//!
//! Coordinates in a block are relative to the block's own top-left, with `x`
//! already inside the content area (margins are applied later by pagination).

#![allow(dead_code)]

use super::config::RasterReflowConfig;
use super::types::{FlowItem, PlacedItem, PlacedKind, PxRect, RegionKind, SourceRef};

/// An item placed within a [`StripBlock`], in block-relative coordinates.
#[derive(Debug, Clone)]
pub struct BlockItem {
    /// Rectangle relative to the block's top-left.
    pub rect: PxRect,
    pub src: SourceRef,
    pub scale: f32,
    pub kind: PlacedKind,
}

impl BlockItem {
    /// Translate this block-relative item into an output-page-absolute
    /// [`PlacedItem`] by offsetting `rect` with the page margin and the
    /// block's vertical cursor position `y`.
    pub fn place(&self, margin: u32, y: u32) -> PlacedItem {
        PlacedItem {
            out_rect: PxRect::new(
                margin + self.rect.x,
                margin + y + self.rect.y,
                self.rect.w,
                self.rect.h,
            ),
            src: self.src,
            scale: self.scale,
            kind: self.kind,
        }
    }
}

/// One stackable unit of the master strip: a text line, a spacer, or a figure.
#[derive(Debug, Clone)]
pub struct StripBlock {
    /// Total vertical space this block occupies (includes leading for lines).
    pub height: u32,
    /// Safe to start an output page immediately before this block.
    pub breakable_before: bool,
    /// A page break here is preferred (e.g. before a heading / after a figure).
    pub page_break_hint: bool,
    /// Placed items (empty for pure spacer blocks).
    pub items: Vec<BlockItem>,
}

impl StripBlock {
    fn spacer(height: u32) -> Self {
        Self {
            height,
            breakable_before: true,
            page_break_hint: false,
            items: Vec::new(),
        }
    }
}

/// Mutable line-accumulation state threaded through `compose`'s flow loop.
/// Bundles the fields that previously had to be passed as six `&mut` arguments
/// to a `flush` closure at every wrap/break/figure site.
struct LineState {
    line: Vec<BlockItem>,
    cursor_x: u32,
    line_height: u32,
    pending_break_before: bool,
    pending_page_hint: bool,
}

impl LineState {
    fn new() -> Self {
        Self {
            line: Vec::new(),
            cursor_x: 0,
            line_height: 0,
            pending_break_before: false,
            pending_page_hint: false,
        }
    }

    /// Flush the current line into a block (no-op if empty), then reset.
    fn flush(&mut self, blocks: &mut Vec<StripBlock>, cfg: &RasterReflowConfig) {
        if self.line.is_empty() {
            return;
        }
        blocks.push(StripBlock {
            height: cfg.line_pitch_px(self.line_height),
            breakable_before: self.pending_break_before,
            page_break_hint: self.pending_page_hint,
            items: std::mem::take(&mut self.line),
        });
        self.cursor_x = 0;
        self.line_height = 0;
        self.pending_break_before = false;
        self.pending_page_hint = false;
    }
}

/// Greedily compose a flow stream into master-strip blocks.
pub fn compose(flow: &[FlowItem], cfg: &RasterReflowConfig) -> Vec<StripBlock> {
    let content_w = cfg.content_width();
    let content_h = cfg.content_height();
    let target_h = cfg.target_text_height;
    let word_spacing = cfg.word_spacing_px(target_h);

    let mut blocks: Vec<StripBlock> = Vec::new();
    let mut st = LineState::new();

    for item in flow {
        match *item {
            FlowItem::WordBitmap { src, height } | FlowItem::RunBitmap { src, height } => {
                let kind = match item {
                    FlowItem::RunBitmap { .. } => PlacedKind::Run,
                    _ => PlacedKind::Word,
                };
                // `height` is the page's uniform body-text height, so `scale`
                // is the same for every word on the page. Both the width and the
                // height are scaled by it (aspect-preserving): pinning the height
                // to `target_h` instead would stretch each word's source crop by
                // a different vertical factor (target_h / its own ink height),
                // making otherwise-equal text vary in size word by word.
                let src_h = height.max(1);
                let scale = cfg.clamp_scale(target_h as f32 / src_h as f32);
                let w_out = ((src.rect.w as f32) * scale).round().max(1.0) as u32;
                let h_out = ((src.rect.h as f32) * scale).round().max(1.0) as u32;

                // Wrap if the word would overflow the content width.
                if !st.line.is_empty() && st.cursor_x + w_out > content_w {
                    st.flush(&mut blocks, cfg);
                }

                // A single word wider than the content area is clamped to fit.
                let w_clamped = w_out.min(content_w.max(1));
                st.line.push(BlockItem {
                    rect: PxRect::new(st.cursor_x, 0, w_clamped, h_out),
                    src,
                    scale,
                    kind,
                });
                st.cursor_x += w_clamped + word_spacing;
                st.line_height = st.line_height.max(h_out);
            }

            FlowItem::LineBreak => {
                // Soft: words continue to flow; only overflow / gaps wrap lines.
                // (MVP keeps prose reflowing across source rows.)
            }

            FlowItem::Gap { px } => {
                st.flush(&mut blocks, cfg);
                if px > 0 {
                    blocks.push(StripBlock::spacer(px));
                }
                st.pending_break_before = true;
            }

            FlowItem::RegionBreak => {
                st.flush(&mut blocks, cfg);
                blocks.push(StripBlock::spacer(cfg.line_pitch_px(target_h)));
                st.pending_break_before = true;
            }

            FlowItem::PageBreakHint => {
                st.flush(&mut blocks, cfg);
                st.pending_page_hint = true;
                st.pending_break_before = true;
            }

            FlowItem::Figure { src, kind } => {
                st.flush(&mut blocks, cfg);
                let (out_w, out_h, scale) = fit_figure(src.rect, content_w, content_h);
                let x = (content_w.saturating_sub(out_w)) / 2;
                let placed_kind = match kind {
                    RegionKind::Table => PlacedKind::Table,
                    _ => PlacedKind::Figure,
                };
                blocks.push(StripBlock {
                    height: out_h,
                    breakable_before: true,
                    page_break_hint: false,
                    items: vec![BlockItem {
                        rect: PxRect::new(x, 0, out_w, out_h),
                        src,
                        scale,
                        kind: placed_kind,
                    }],
                });
                st.pending_break_before = true;
            }
        }
    }

    // Flush any trailing line.
    st.flush(&mut blocks, cfg);

    blocks
}

/// Scale a figure rectangle to fit within `(max_w, max_h)`, preserving aspect
/// ratio. Shrinks as needed; never upscales beyond the box. Returns the scaled
/// `(width, height, scale)` so callers don't have to re-derive the ratio.
pub fn fit_figure(src: PxRect, max_w: u32, max_h: u32) -> (u32, u32, f32) {
    if src.w == 0 || src.h == 0 || max_w == 0 || max_h == 0 {
        return (max_w.min(src.w.max(1)), max_h.min(src.h.max(1)), 1.0);
    }
    let sw = max_w as f32 / src.w as f32;
    let sh = max_h as f32 / src.h as f32;
    let s = sw.min(sh).min(1.0); // fit, do not enlarge
    let w = ((src.w as f32) * s).round().max(1.0) as u32;
    let h = ((src.h as f32) * s).round().max(1.0) as u32;
    (w.min(max_w), h.min(max_h), s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RasterReflowConfig {
        RasterReflowConfig {
            page_width: 220,
            page_height: 300,
            margin: 10, // content width = 200
            target_text_height: 20,
            word_spacing_frac: 0.5, // 10px spacing
            line_spacing_frac: 1.5, // line pitch = 30 for height 20
            scale_min: 0.1,
            scale_max: 10.0,
            ..Default::default()
        }
    }

    fn word(x: u32, w: u32, h: u32) -> FlowItem {
        FlowItem::WordBitmap {
            src: SourceRef {
                page_index: 0,
                rect: PxRect::new(x, 0, w, h),
            },
            height: h,
        }
    }

    #[test]
    fn words_pack_then_wrap_on_overflow() {
        let c = cfg(); // content width 200, spacing 10, scale = 20/20 = 1
        // each word 80 wide → 80,(+10)90 .. third would be 180+80=260 > 200 → wrap
        let flow = vec![word(0, 80, 20), word(0, 80, 20), word(0, 80, 20)];
        let blocks = compose(&flow, &c);
        // two text lines
        let lines: Vec<&StripBlock> = blocks.iter().filter(|b| !b.items.is_empty()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].items.len(), 2);
        assert_eq!(lines[1].items.len(), 1);
        // first word at x=0, second at x=90
        assert_eq!(lines[0].items[0].rect.x, 0);
        assert_eq!(lines[0].items[1].rect.x, 90);
        // line pitch
        assert_eq!(lines[0].height, 30);
    }

    #[test]
    fn scale_enlarges_small_text_to_target() {
        let c = cfg(); // target 20
        // source height 10 → scale 2.0, width 30 → out width 60
        let flow = vec![word(0, 30, 10)];
        let blocks = compose(&flow, &c);
        let item = &blocks[0].items[0];
        assert!((item.scale - 2.0).abs() < 1e-6);
        assert_eq!(item.rect.w, 60);
        assert_eq!(item.rect.h, 20);
    }

    #[test]
    fn gap_creates_spacer_and_break_point() {
        let c = cfg();
        let flow = vec![word(0, 40, 20), FlowItem::Gap { px: 25 }, word(0, 40, 20)];
        let blocks = compose(&flow, &c);
        // line, spacer(25), line
        assert_eq!(blocks.len(), 3);
        assert!(blocks[1].items.is_empty());
        assert_eq!(blocks[1].height, 25);
        assert!(blocks[1].breakable_before);
        // line after the gap is also breakable
        assert!(blocks[2].breakable_before);
    }

    #[test]
    fn figure_is_atomic_and_centered() {
        let c = cfg(); // content 200 x 280
        let flow = vec![FlowItem::Figure {
            src: SourceRef {
                page_index: 0,
                rect: PxRect::new(0, 0, 400, 200), // 2:1, wider than content
            },
            kind: RegionKind::Figure,
        }];
        let blocks = compose(&flow, &c);
        assert_eq!(blocks.len(), 1);
        let it = &blocks[0].items[0];
        assert_eq!(it.kind, PlacedKind::Figure);
        // fit to width 200 → 200x100, centered → x = 0
        assert_eq!(it.rect.w, 200);
        assert_eq!(it.rect.h, 100);
        assert_eq!(it.rect.x, 0);
        assert_eq!(blocks[0].height, 100);
    }

    #[test]
    fn fit_figure_never_upscales() {
        assert_eq!(
            fit_figure(PxRect::new(0, 0, 50, 50), 200, 200),
            (50, 50, 1.0)
        );
        assert_eq!(
            fit_figure(PxRect::new(0, 0, 400, 200), 200, 300),
            (200, 100, 0.5)
        );
    }

    #[test]
    fn oversized_word_clamped_to_content_width() {
        let c = cfg(); // content 200
        let flow = vec![word(0, 500, 20)]; // 500 > 200
        let blocks = compose(&flow, &c);
        assert_eq!(blocks[0].items[0].rect.w, 200);
    }
}
