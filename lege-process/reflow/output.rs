//! Pagination: stack master-strip blocks into output pages.
//!
//! Behaves like a controlled master-bitmap queue rather than independent
//! per-source-page resizing. Blocks are atomic, so a page break never cuts
//! through a text line; breaks occur only at block boundaries. Margins are
//! applied here, and every placed item is recorded into the [`SourceMap`] for
//! later text-layer re-projection.

use super::compose::StripBlock;
use super::config::RasterReflowConfig;
use super::types::{ReflowPage, SourceMap};

/// Paginate composed blocks into output pages starting at `start_index`.
/// Returns the pages plus the aggregated source→output map.
pub fn paginate(
    blocks: &[StripBlock],
    cfg: &RasterReflowConfig,
    start_index: usize,
) -> (Vec<ReflowPage>, SourceMap) {
    let content_h = cfg.content_height();
    let margin = cfg.margin;
    let page_w = cfg.page_width;
    let page_h = cfg.page_height;

    let mut pages: Vec<ReflowPage> = Vec::new();
    let mut source_map = SourceMap::default();
    let mut idx = start_index;
    let mut cur = ReflowPage::new(idx, page_w, page_h);
    let mut y: u32 = 0;

    for block in blocks {
        // Never start a page with a pure spacer — it would waste output space.
        if y == 0 && block.items.is_empty() {
            continue;
        }

        let overflow = y + block.height > content_h;
        let wants_break = block.page_break_hint && y > 0;
        let page_has_content = y > 0 || !cur.items.is_empty();

        if (overflow || wants_break) && page_has_content {
            pages.push(std::mem::replace(&mut cur, {
                idx += 1;
                ReflowPage::new(idx, page_w, page_h)
            }));
            y = 0;
            // Skip a spacer that would now sit at the top of the fresh page.
            if block.items.is_empty() {
                continue;
            }
        }

        for it in &block.items {
            let placed = it.place(margin, y);
            source_map.push(idx, placed.out_rect, placed.src);
            cur.items.push(placed);
        }
        y += block.height;
    }

    if !cur.items.is_empty() {
        pages.push(cur);
    }

    (pages, source_map)
}

#[cfg(test)]
mod tests {
    use super::super::compose::BlockItem;
    use super::super::types::{PlacedKind, PxRect, SourceRef};
    use super::*;

    fn cfg() -> RasterReflowConfig {
        RasterReflowConfig {
            page_width: 120,
            page_height: 120,
            margin: 10, // content area 100 x 100
            ..Default::default()
        }
    }

    fn line_block(height: u32, n_items: usize, src_page: usize) -> StripBlock {
        let items = (0..n_items)
            .map(|i| BlockItem {
                rect: PxRect::new((i as u32) * 20, 0, 18, height),
                src: SourceRef {
                    page_index: src_page,
                    rect: PxRect::new((i as u32) * 20, 0, 18, height),
                },
                scale: 1.0,
                kind: PlacedKind::Word,
            })
            .collect();
        StripBlock {
            height,
            breakable_before: false,
            page_break_hint: false,
            items,
        }
    }

    #[test]
    fn single_page_when_everything_fits() {
        let c = cfg(); // content height 100
        let blocks = vec![line_block(30, 2, 0), line_block(30, 2, 0)];
        let (pages, sm) = paginate(&blocks, &c, 0);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].items.len(), 4);
        assert_eq!(sm.placements.len(), 4);
    }

    #[test]
    fn overflow_starts_new_page_at_block_boundary() {
        let c = cfg(); // content height 100, each block 40 → 3 per page max (120>100 so 2)
        let blocks = vec![
            line_block(40, 1, 0),
            line_block(40, 1, 0),
            line_block(40, 1, 0), // 120 > 100 → page 2
        ];
        let (pages, _) = paginate(&blocks, &c, 0);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].items.len(), 2);
        assert_eq!(pages[1].items.len(), 1);
        assert_eq!(pages[1].index, 1);
    }

    #[test]
    fn all_items_within_page_bounds() {
        let c = cfg();
        let blocks = vec![
            line_block(40, 4, 0),
            line_block(40, 4, 0),
            line_block(40, 4, 0),
        ];
        let (pages, _) = paginate(&blocks, &c, 0);
        for p in &pages {
            let b = p.bounds();
            for it in &p.items {
                assert!(it.out_rect.x >= c.margin);
                assert!(it.out_rect.y >= c.margin);
                assert!(it.out_rect.right() <= b.w, "item exceeds width");
                assert!(it.out_rect.bottom() <= b.h, "item exceeds height");
            }
        }
    }

    #[test]
    fn reading_order_monotonic_across_pages() {
        let c = cfg();
        let blocks = vec![
            line_block(40, 1, 0),
            line_block(40, 1, 0),
            line_block(40, 1, 0),
        ];
        let (pages, _) = paginate(&blocks, &c, 0);
        // Flatten in page then y order; source x should be increasing in stream.
        let mut last_key = (0usize, 0u32);
        for p in &pages {
            for it in &p.items {
                let key = (p.index, it.out_rect.y);
                assert!(key >= last_key, "placement order regressed");
                last_key = key;
            }
        }
    }

    #[test]
    fn leading_spacer_skipped() {
        let c = cfg();
        let spacer = StripBlock {
            height: 30,
            breakable_before: true,
            page_break_hint: false,
            items: Vec::new(),
        };
        let blocks = vec![spacer, line_block(30, 1, 0)];
        let (pages, _) = paginate(&blocks, &c, 0);
        // The text line should sit at the very top (y == margin), spacer skipped.
        assert_eq!(pages[0].items[0].out_rect.y, c.margin);
    }

    #[test]
    fn page_break_hint_forces_new_page() {
        let c = cfg();
        let mut hinted = line_block(30, 1, 0);
        hinted.page_break_hint = true;
        let blocks = vec![line_block(30, 1, 0), hinted];
        let (pages, _) = paginate(&blocks, &c, 0);
        assert_eq!(pages.len(), 2);
    }

    #[test]
    fn source_map_records_every_item() {
        let c = cfg();
        let blocks = vec![line_block(30, 3, 7)];
        let (_pages, sm) = paginate(&blocks, &c, 0);
        assert_eq!(sm.placements.len(), 3);
        assert_eq!(sm.for_source_page(7).count(), 3);
    }
}
