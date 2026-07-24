use std::sync::Arc;

use lege_viewer::document::engine::{CancellationFlag, DocumentEngine};
use lege_viewer::document::layout::PageLayoutIndex;
use lege_viewer::document::synthetic::SyntheticEngine;
use lege_viewer::document::tile::ZoomBucket;
use lege_viewer::geometry::{RectF, SizeF};
use lege_viewer::scroll::{
    DocumentLocation, NavigationHistory, PagingDirection, ReadingAnchor, paging_target,
};
use lege_viewer::text::{LineBox, SearchIndex};
use lege_viewer::theme::Theme;
use lege_viewer::PageIndex;

#[test]
fn line_paging_keeps_one_visual_line() {
    let viewport = RectF {
        x: 0.0,
        y: 100.0,
        width: 600.0,
        height: 300.0,
    };
    let lines = (0..8)
        .map(|index| LineBox {
            page: PageIndex(0),
            bounds: RectF {
                x: 20.0,
                y: 110.0 + f64::from(index) * 35.0,
                width: 400.0,
                height: 20.0,
            },
            baseline_y: 125.0 + f64::from(index) * 35.0,
            char_range: (index as usize * 10, index as usize * 10 + 10),
        })
        .collect::<Vec<_>>();

    let target = paging_target(PagingDirection::Down, viewport, lines, 2_000.0);
    assert_eq!(target, 355.0);
}

#[test]
fn geometric_paging_is_always_available() {
    let viewport = RectF {
        x: 0.0,
        y: 100.0,
        width: 600.0,
        height: 500.0,
    };
    let target = paging_target(PagingDirection::Down, viewport, Vec::new(), 2_000.0);
    assert_eq!(target, 560.0); // 500 - clamp(8%, 24..96) = 460
}

#[test]
fn reading_anchor_round_trips_layout_position() {
    let engine = SyntheticEngine::new(4);
    let layout = PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    );
    let viewport = RectF {
        x: 0.0,
        y: 925.0,
        width: 700.0,
        height: 500.0,
    };
    let anchor = ReadingAnchor::capture(&layout, viewport).expect("anchor");
    let restored = anchor.restore(&layout, viewport.height).expect("restore");
    assert!((restored - viewport.y).abs() < 0.001);
}

#[test]
fn zoom_buckets_are_sqrt_two_steps() {
    assert_eq!(ZoomBucket::from_zoom(1.0), ZoomBucket::ONE);
    assert_eq!(ZoomBucket::from_zoom(std::f64::consts::SQRT_2), ZoomBucket(1));
    assert!((ZoomBucket(2).scale() - 2.0).abs() < 0.000_001);
}

#[test]
fn compilation_returns_semantic_text_ir_and_structure_together() {
    let engine = SyntheticEngine::new(2);
    let layout = PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    );
    let artifacts = engine
        .compile_page(
            PageIndex(0),
            layout.placement(PageIndex(0)).unwrap().page_to_doc,
            &CancellationFlag::default(),
        )
        .expect("compile synthetic page");
    assert!(!artifacts.text.substrate().characters.is_empty());
    assert!(artifacts.compiled.operation_count() > 0);
    assert_eq!(artifacts.structure.content_extent.rect, artifacts.geometry.crop);
}

#[test]
fn search_offsets_stay_in_native_utf16_space() {
    let engine = SyntheticEngine::new(1);
    let layout = PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    );
    let artifacts = engine
        .compile_page(
            PageIndex(0),
            layout.placement(PageIndex(0)).unwrap().page_to_doc,
            &CancellationFlag::default(),
        )
        .unwrap();
    let mut index = SearchIndex::default();
    index.insert(PageIndex(0), Arc::clone(artifacts.text.substrate()));
    let hits = index.search_exact("synthetic line 01", 8);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].page, PageIndex(0));
}

#[test]
fn navigation_history_records_jumps_not_scroll_positions() {
    let mut history = NavigationHistory::default();
    history.push_jump(DocumentLocation {
        page: PageIndex(3),
        target_region: None,
    });
    history.push_jump(DocumentLocation {
        page: PageIndex(40),
        target_region: None,
    });
    assert_eq!(history.back().unwrap().page, PageIndex(3));
    assert_eq!(history.forward().unwrap().page, PageIndex(40));
}

#[test]
fn layout_cost_is_independent_of_window_size() {
    let engine = SyntheticEngine::new(10_000);
    let layout = PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    );
    let visible = layout.visible_pages(RectF {
        x: 0.0,
        y: 3_000_000.0,
        width: 1_200.0,
        height: 900.0,
    });
    assert!(visible.len() < 8);
    assert_eq!(layout.extent().width, layout.total_width);
    let _unused = SizeF::default();
}

#[test]
fn normalized_input_trace_replays_the_same_position_curve() {
    use lege_viewer::geometry::Vec2d;
    use lege_viewer::scroll::ScrollModel;
    use lege_viewer::trace::{InputTrace, TraceCommand};

    let mut trace = InputTrace::default();
    trace.push(0, TraceCommand::WheelPixels(Vec2d { x: 0.0, y: 15.25 }));
    trace.push(1_000, TraceCommand::WheelPixels(Vec2d { x: 0.0, y: 8.75 }));
    trace.push(2_000, TraceCommand::FineStep(Vec2d { x: 0.0, y: 42.0 }));

    let mut first = ScrollModel::new();
    first.set_extents(
        SizeF { width: 800.0, height: 5_000.0 },
        SizeF { width: 800.0, height: 600.0 },
    );
    let mut second = first.clone();
    assert_eq!(trace.replay_positions(&mut first), trace.replay_positions(&mut second));
}

#[test]
fn memory_leases_account_shared_ownership_once() {
    use lege_viewer::document::{CacheCategory, MemoryArbiter};

    let arbiter = MemoryArbiter::new(1_024);
    let first = arbiter.reserve(CacheCategory::Text, 256);
    let second = first.clone();
    assert_eq!(arbiter.total_bytes(), 256);
    drop(first);
    assert_eq!(arbiter.total_bytes(), 256);
    drop(second);
    assert_eq!(arbiter.total_bytes(), 0);
}

#[test]
fn cross_bucket_fallback_matches_page_space_not_tile_number() {
    use lege_viewer::document::{
        DocumentId, MemoryArbiter, TileCache, TileCoord, TileDemand, TileKey, TileSurface,
        TileTier,
    };
    use lege_viewer::geometry::RectI;
    use lege_viewer::paint::PixelSurface;

    let document = DocumentId(7);
    let page = PageIndex(2);
    let cache = TileCache::new(document, MemoryArbiter::new(1 << 20));
    cache.insert(
        Arc::new(TileSurface {
            key: TileKey {
                document,
                page,
                bucket: ZoomBucket(-2),
                coord: TileCoord { x: 0, y: 0 },
                tier: TileTier::Thumbnail,
            },
            generation: 1,
            page_device_rect: RectI {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
            page_document_rect: RectF {
                x: 10.0,
                y: 20.0,
                width: 400.0,
                height: 500.0,
            },
            pixels: PixelSurface {
                width: 1,
                height: 1,
                stride: 1,
                pixels: vec![0x00ff_ffff].into(),
            },
            degraded: false,
        }),
        0.0,
    );

    // Coordinate 9,12 cannot match the thumbnail's coordinate. Page-space
    // coverage still can, which is the required stale/thumbnail behavior.
    let demand = TileDemand {
        page,
        coord: TileCoord { x: 9, y: 12 },
        page_device_rect: RectI {
            x: 2_304,
            y: 3_072,
            width: 256,
            height: 256,
        },
        page_document_rect: RectF {
            x: 100.0,
            y: 120.0,
            width: 50.0,
            height: 50.0,
        },
        distance_from_viewport: 0.0,
        visible: true,
    };
    let fallback = cache.best_covering(demand, ZoomBucket(3));
    assert_eq!(fallback.len(), 1);
    assert_eq!(fallback[0].key.tier, TileTier::Thumbnail);
}

#[test]
fn scrollbar_hover_plans_a_bounded_thumbnail_ring() {
    use lege_viewer::document::ViewportPlanner;
    use lege_viewer::geometry::Vec2d;

    let engine = SyntheticEngine::new(30);
    let layout = PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    );
    let intent = ViewportPlanner::default().build(
        4,
        &layout,
        Vec2d::ZERO,
        Vec2d::ZERO,
        SizeF {
            width: 1_000.0,
            height: 800.0,
        },
        1.0,
        Some(PageIndex(10)),
    );
    assert_eq!(intent.thumbnail_pages.first(), Some(&PageIndex(5)));
    assert_eq!(intent.thumbnail_pages.last(), Some(&PageIndex(15)));
    assert_eq!(intent.thumbnail_pages.len(), 11);
    assert!(intent.compile_pages.binary_search(&PageIndex(10)).is_ok());
}

#[test]
fn visibility_below_document_is_empty() {
    let engine = SyntheticEngine::new(2);
    let layout = PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    );
    assert!(layout
        .visible_pages(RectF {
            x: 0.0,
            y: layout.total_height + 10.0,
            width: 800.0,
            height: 600.0,
        })
        .is_empty());
}
