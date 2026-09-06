#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use lege_viewer::PageIndex;
use lege_viewer::cli::{CliError, LaunchMode, parse_launch_config, parse_launch_mode};
use lege_viewer::document::engine::{CancellationFlag, DocumentEngine};
use lege_viewer::document::layout::PageLayoutIndex;
use lege_viewer::document::synthetic::SyntheticEngine;
use lege_viewer::document::tile::ZoomBucket;
use lege_viewer::geometry::{RectF, SizeF};
use lege_viewer::scroll::{
    DocumentLocation, NavigationHistory, PagingDirection, ReadingAnchor, notional_page_lines,
    paging_target,
};
use lege_viewer::text::{LineBox, SearchIndex};
use lege_viewer::theme::Theme;

#[derive(Debug, Default)]
struct TestWake {
    count: AtomicUsize,
}

impl lege_viewer::document::WakeSink for TestWake {
    fn wake(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(feature = "pdf-engine")]
fn viewer_pdf_fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lege-pdf/render/crates/pdf-chaos-tests/tests/fixtures/hello_world.pdf")
}

#[cfg(feature = "pdf-engine")]
#[test]
fn pdf_links_reach_viewer_space_with_destination_geometry() {
    use std::io::Write;

    use lege_viewer::document::LinkTarget;
    use lege_viewer::document::pdf_engine::PdfEngine;
    use pdf_test_support::builder::PdfBuilder;

    let mut builder = PdfBuilder::new();
    builder.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    builder.add_object(2, "<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>");
    builder.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Annots[10 0 R 11 0 R]>>",
    );
    builder.add_object(4, "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>");
    builder.add_object(
        10,
        "<</Type/Annot/Subtype/Link/Rect[10 20 110 40]/Dest[4 0 R/XYZ 15 700 1]>>",
    );
    builder.add_object(
        11,
        "<</Type/Annot/Subtype/Link/Rect[10 50 110 70]\
         /A<</S/URI/URI(https://example.com/)>>>>",
    );
    builder.finish_classic_xref("/Root 1 0 R");
    let mut file = tempfile::NamedTempFile::new().expect("temporary link PDF");
    file.write_all(&builder.into_bytes())
        .expect("write link PDF");

    let engine = PdfEngine::open(file.path(), None).expect("open link PDF in viewer engine");
    let links = &engine.descriptor().page_links[0];
    assert_eq!(links.len(), 2);
    assert_eq!(
        links[0].source_region,
        RectF {
            x: 10.0,
            y: 752.0,
            width: 100.0,
            height: 20.0,
        }
    );
    assert_eq!(
        links[0].target,
        LinkTarget::Internal {
            page: PageIndex(1),
            target_region: Some(RectF {
                x: 15.0,
                y: 92.0,
                width: 1.0,
                height: 1.0,
            }),
        }
    );
    assert_eq!(
        links[1].target,
        LinkTarget::External(Arc::from("https://example.com/"))
    );
}

#[test]
fn launch_mode_falls_back_to_the_empty_document_state() {
    assert_eq!(parse_launch_mode::<_, &str>([]).unwrap(), LaunchMode::Empty);
    assert_eq!(
        parse_launch_config(["--presenter", "software"])
            .unwrap()
            .mode,
        LaunchMode::Empty
    );
    assert_eq!(
        parse_launch_mode(["--synthetic"]).unwrap(),
        LaunchMode::Synthetic { page_count: 10_000 }
    );
    assert_eq!(
        parse_launch_mode(["--synthetic", "25"]).unwrap(),
        LaunchMode::Synthetic { page_count: 25 }
    );
    assert_eq!(
        parse_launch_mode(["example.pdf"]).unwrap(),
        LaunchMode::Pdf("example.pdf".into())
    );
    let gpu = parse_launch_config(["--presenter", "gpu", "example.pdf"]).unwrap();
    assert_eq!(gpu.mode, LaunchMode::Pdf("example.pdf".into()));
    assert_eq!(
        gpu.presenter,
        lege_viewer::present::PresenterPreference::Gpu
    );
    let software = parse_launch_config(["--synthetic", "25", "--presenter", "software"]).unwrap();
    assert_eq!(
        software.presenter,
        lege_viewer::present::PresenterPreference::Software
    );
    assert_eq!(software.mode, LaunchMode::Synthetic { page_count: 25 });
    assert_eq!(
        parse_launch_config(["--presenter", "unknown", "example.pdf"]),
        Err(CliError::InvalidPresenter("unknown".to_owned()))
    );
}

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
fn textless_page_paging_preserves_a_notional_ten_percent_row() {
    let page = RectF {
        x: 20.0,
        y: 100.0,
        width: 600.0,
        height: 1_000.0,
    };
    let viewport = RectF {
        x: 0.0,
        y: 100.0,
        width: 700.0,
        height: 500.0,
    };
    let rows = notional_page_lines(PageIndex(3), page);
    assert_eq!(rows.len(), 10);
    assert_eq!(
        paging_target(PagingDirection::Down, viewport, rows.clone(), 2_000.0),
        500.0
    );
    let next_viewport = RectF {
        y: 500.0,
        ..viewport
    };
    assert_eq!(
        paging_target(PagingDirection::Up, next_viewport, rows, 2_000.0),
        100.0
    );
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
    assert_eq!(
        ZoomBucket::from_zoom(std::f64::consts::SQRT_2),
        ZoomBucket(1)
    );
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
    assert_eq!(
        artifacts.structure.content_extent.rect,
        artifacts.geometry.crop
    );
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
        SizeF {
            width: 800.0,
            height: 5_000.0,
        },
        SizeF {
            width: 800.0,
            height: 600.0,
        },
    );
    let mut second = first.clone();
    assert_eq!(
        trace.replay_positions(&mut first),
        trace.replay_positions(&mut second)
    );
}

#[test]
fn memory_leases_account_shared_ownership_once() {
    use lege_viewer::document::{CacheCategory, MemoryArbiter};

    let arbiter = MemoryArbiter::new(1_024);
    let first = arbiter.reserve(CacheCategory::Text, 256);
    let second = first.clone();
    assert_eq!(arbiter.total_bytes(), 256);
    let gpu = arbiter.reserve(CacheCategory::GpuTiles, 512);
    assert_eq!(arbiter.category_bytes(CacheCategory::GpuTiles), 512);
    assert_eq!(arbiter.total_bytes(), 768);
    drop(gpu);
    drop(first);
    assert_eq!(arbiter.total_bytes(), 256);
    drop(second);
    assert_eq!(arbiter.total_bytes(), 0);
}

#[test]
fn cross_bucket_fallback_matches_page_space_not_tile_number() {
    use lege_viewer::document::{
        DocumentId, MemoryArbiter, TileCache, TileCoord, TileDemand, TileKey, TileSurface, TileTier,
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
                variant: 0,
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
        page_view_box: lege_viewer::geometry::RectF {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        color_mode: lege_viewer::document::ColorMode::Original,
        variant: 0,
    };
    let fallback = cache.best_covering(demand, ZoomBucket(3));
    assert_eq!(fallback.len(), 1);
    assert_eq!(fallback[0].key.tier, TileTier::Thumbnail);
}

#[test]
fn older_tile_generation_never_replaces_newer_pixels() {
    use lege_viewer::document::{
        DocumentId, MemoryArbiter, TileCache, TileCoord, TileDemand, TileKey, TileSurface, TileTier,
    };
    use lege_viewer::geometry::RectI;
    use lege_viewer::paint::PixelSurface;

    let document = DocumentId(8);
    let page = PageIndex(0);
    let cache = TileCache::new(document, MemoryArbiter::new(1 << 20));
    let demand = TileDemand {
        page,
        coord: TileCoord { x: 0, y: 0 },
        page_device_rect: RectI {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        page_document_rect: RectF {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        distance_from_viewport: 0.0,
        visible: true,
        page_view_box: lege_viewer::geometry::RectF {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        color_mode: lege_viewer::document::ColorMode::Original,
        variant: 0,
    };
    let surface = |generation, pixel| {
        Arc::new(TileSurface {
            key: TileKey {
                document,
                page,
                bucket: ZoomBucket::ONE,
                coord: demand.coord,
                tier: TileTier::Final,
                variant: 0,
            },
            generation,
            page_device_rect: demand.page_device_rect,
            page_document_rect: demand.page_document_rect,
            pixels: PixelSurface {
                width: 1,
                height: 1,
                stride: 1,
                pixels: vec![pixel].into(),
            },
            degraded: false,
        })
    };
    cache.insert(surface(5, 0x0012_3456), 0.0);
    cache.insert(surface(4, 0x0065_4321), 0.0);
    let selected = cache.best_covering(demand, ZoomBucket::ONE);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].generation, 5);
    assert_eq!(selected[0].pixels.pixels[0], 0x0012_3456);
}

#[test]
fn frame_tile_snapshot_is_immutable_across_cache_updates() {
    use lege_viewer::document::{
        DocumentId, MemoryArbiter, TileCache, TileCoord, TileDemand, TileKey, TileSurface, TileTier,
    };
    use lege_viewer::geometry::RectI;
    use lege_viewer::paint::PixelSurface;

    let document = DocumentId(9);
    let page = PageIndex(0);
    let cache = TileCache::new(document, MemoryArbiter::new(1 << 20));
    let demand = TileDemand {
        page,
        coord: TileCoord { x: 0, y: 0 },
        page_device_rect: RectI {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        page_document_rect: RectF {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        distance_from_viewport: 0.0,
        visible: true,
        page_view_box: lege_viewer::geometry::RectF {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        color_mode: lege_viewer::document::ColorMode::Original,
        variant: 0,
    };
    let surface = |generation, pixel| {
        Arc::new(TileSurface {
            key: TileKey {
                document,
                page,
                bucket: ZoomBucket::ONE,
                coord: demand.coord,
                tier: TileTier::Final,
                variant: 0,
            },
            generation,
            page_device_rect: demand.page_device_rect,
            page_document_rect: demand.page_document_rect,
            pixels: PixelSurface {
                width: 1,
                height: 1,
                stride: 1,
                pixels: vec![pixel].into(),
            },
            degraded: false,
        })
    };

    cache.insert(surface(1, 0x0011_1111), 0.0);
    let first_frame = cache.frame_snapshot();
    cache.insert(surface(2, 0x0022_2222), 0.0);
    let second_frame = cache.frame_snapshot();

    let first = first_frame.best_covering(demand, ZoomBucket::ONE);
    let second = second_frame.best_covering(demand, ZoomBucket::ONE);
    assert_eq!(
        (first[0].generation, first[0].pixels.pixels[0]),
        (1, 0x0011_1111)
    );
    assert_eq!(
        (second[0].generation, second[0].pixels.pixels[0]),
        (2, 0x0022_2222)
    );
}

#[test]
fn frame_tile_snapshot_can_be_scoped_to_relevant_pages() {
    use lege_viewer::document::{
        DocumentId, MemoryArbiter, TileCache, TileCoord, TileDemand, TileKey, TileSurface, TileTier,
    };
    use lege_viewer::geometry::RectI;
    use lege_viewer::paint::PixelSurface;

    let document = DocumentId(10);
    let cache = TileCache::new(document, MemoryArbiter::new(1 << 20));
    let demand_for = |page| TileDemand {
        page,
        coord: TileCoord { x: 0, y: 0 },
        page_device_rect: RectI {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        page_document_rect: RectF {
            x: 0.0,
            y: f64::from(page.0),
            width: 1.0,
            height: 1.0,
        },
        distance_from_viewport: 0.0,
        visible: true,
        page_view_box: lege_viewer::geometry::RectF {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        color_mode: lege_viewer::document::ColorMode::Original,
        variant: 0,
    };
    for page in [PageIndex(0), PageIndex(1)] {
        let demand = demand_for(page);
        cache.insert(
            Arc::new(TileSurface {
                key: TileKey {
                    document,
                    page,
                    bucket: ZoomBucket::ONE,
                    coord: demand.coord,
                    tier: TileTier::Final,
                    variant: 0,
                },
                generation: 1,
                page_device_rect: demand.page_device_rect,
                page_document_rect: demand.page_document_rect,
                pixels: PixelSurface {
                    width: 1,
                    height: 1,
                    stride: 1,
                    pixels: vec![page.0].into(),
                },
                degraded: false,
            }),
            0.0,
        );
    }

    let snapshot = cache.frame_snapshot_for_pages([PageIndex(0)]);
    assert_eq!(
        snapshot
            .best_covering(demand_for(PageIndex(0)), ZoomBucket::ONE)
            .len(),
        1
    );
    assert!(
        snapshot
            .best_covering(demand_for(PageIndex(1)), ZoomBucket::ONE)
            .is_empty(),
        "a frame should not clone or expose unrelated page maps"
    );
}

#[test]
fn synthetic_conductor_delivers_compiled_artifacts_and_tiles() {
    use lege_viewer::document::{
        ConductorHandle, MemoryArbiter, SessionUpdate, TileCache, UpdateQueue, ViewportPlanner,
    };
    use lege_viewer::geometry::Vec2d;

    let engine: Arc<dyn DocumentEngine> = Arc::new(SyntheticEngine::new(20));
    let layout = Arc::new(PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    ));
    let memory = MemoryArbiter::new(64 * 1024 * 1024);
    let tiles = Arc::new(TileCache::new(engine.descriptor().id, memory.clone()));
    let wake = Arc::new(TestWake::default());
    let updates = UpdateQueue::new(128, wake.clone());
    let conductor = ConductorHandle::spawn(
        engine,
        layout.clone(),
        updates.clone(),
        memory,
        tiles.clone(),
    )
    .expect("spawn synthetic conductor");
    let intent = ViewportPlanner::default().build(
        1,
        &layout,
        Vec2d::ZERO,
        Vec2d::ZERO,
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
    );
    conductor.publish_intent(intent);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut compiled = false;
    let mut tile_ready = false;
    while Instant::now() < deadline && !(compiled && tile_ready) {
        for update in updates.drain() {
            match update {
                SessionUpdate::PageCompiled(_) => compiled = true,
                SessionUpdate::TileReady { .. } => tile_ready = true,
                SessionUpdate::PageError { message, .. } => {
                    panic!("synthetic conductor reported an error: {message}")
                }
                SessionUpdate::QueueDepths { .. }
                | SessionUpdate::PreviewReady { .. }
                | SessionUpdate::PageIndexed { .. }
                | SessionUpdate::TextIndexProgress { .. }
                | SessionUpdate::SearchCompleted { .. } => {}
            }
        }
        if !(compiled && tile_ready) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    drop(conductor);
    assert!(compiled);
    assert!(tile_ready);
    assert!(!tiles.is_empty());
    assert!(wake.count.load(Ordering::Relaxed) > 0);
}

#[test]
fn explicit_warm_hint_is_deferred_until_skimming_settles() {
    use lege_viewer::document::{
        ConductorHandle, MemoryArbiter, NavigationMode, TileCache, UpdateQueue, ViewportPlanner,
        WarmHint, WarmReason,
    };
    use lege_viewer::geometry::Vec2d;

    let engine: Arc<dyn DocumentEngine> = Arc::new(SyntheticEngine::new(40));
    let layout = Arc::new(PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    ));
    let memory = MemoryArbiter::new(128 * 1024 * 1024);
    let tiles = Arc::new(TileCache::new(engine.descriptor().id, memory.clone()));
    let updates = UpdateQueue::new(256, Arc::new(TestWake::default()));
    let conductor = ConductorHandle::spawn(engine, layout.clone(), updates, memory, tiles)
        .expect("spawn warm-hint conductor");
    conductor.publish_intent(ViewportPlanner::default().build_with_navigation(
        1,
        &layout,
        Vec2d::ZERO,
        Vec2d::ZERO,
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
        NavigationMode::Skimming,
    ));

    let destination = PageIndex(35);
    conductor.warm(WarmHint::for_duration(
        destination,
        WarmReason::OutlineHover,
        0.9,
        Duration::from_secs(2),
    ));
    let previews = conductor.previews();
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !previews.contains(destination),
        "active skimming must not spend raster workers on predicted previews"
    );
    conductor.publish_intent(ViewportPlanner::default().build(
        2,
        &layout,
        Vec2d::ZERO,
        Vec2d::ZERO,
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !previews.contains(destination) {
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(conductor);
    assert!(
        previews.contains(destination),
        "an explicit likely destination should be ready without becoming visible"
    );
}

#[test]
fn idle_conductor_builds_the_document_wide_canonical_preview_layer() {
    use lege_viewer::document::{
        ConductorHandle, MemoryArbiter, TileCache, UpdateQueue, ViewportPlanner,
    };
    use lege_viewer::geometry::Vec2d;

    let engine: Arc<dyn DocumentEngine> = Arc::new(SyntheticEngine::new(24));
    let layout = Arc::new(PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    ));
    let memory = MemoryArbiter::new(128 * 1024 * 1024);
    let tiles = Arc::new(TileCache::new(engine.descriptor().id, memory.clone()));
    let updates = UpdateQueue::new(256, Arc::new(TestWake::default()));
    let conductor = ConductorHandle::spawn(engine, layout.clone(), updates, memory, tiles)
        .expect("spawn preview conductor");
    let previews = conductor.previews();
    conductor.publish_intent(ViewportPlanner::default().build(
        1,
        &layout,
        Vec2d::ZERO,
        Vec2d::ZERO,
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
    ));

    let last_page = PageIndex(23);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !previews.contains(last_page) {
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(conductor);
    assert!(
        previews.contains(last_page),
        "the idle sweep should eventually prepare even a far-off page"
    );
}

#[test]
fn skim_intent_renders_exact_tiles_without_building_a_new_preview() {
    use lege_viewer::document::{
        ConductorHandle, MemoryArbiter, NavigationMode, TileCache, TileTier, UpdateQueue,
        ViewportPlanner,
    };
    use lege_viewer::geometry::Vec2d;

    let engine: Arc<dyn DocumentEngine> = Arc::new(SyntheticEngine::new(20));
    let layout = Arc::new(PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    ));
    let memory = MemoryArbiter::new(128 * 1024 * 1024);
    let tiles = Arc::new(TileCache::new(engine.descriptor().id, memory.clone()));
    let updates = UpdateQueue::new(256, Arc::new(TestWake::default()));
    let conductor = ConductorHandle::spawn(
        engine.clone(),
        layout.clone(),
        updates,
        memory,
        tiles.clone(),
    )
    .expect("spawn skim conductor");
    let previews = conductor.previews();
    let destination = PageIndex(10);
    let destination_y = layout
        .placement(destination)
        .expect("skim destination")
        .bounds
        .y;
    let intent = ViewportPlanner::default().build_with_navigation(
        2,
        &layout,
        Vec2d {
            x: 0.0,
            y: destination_y,
        },
        Vec2d::ZERO,
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
        NavigationMode::Skimming,
    );
    let visible = *intent.visible_tiles.first().expect("visible skim tile");
    let exact = visible.key(engine.descriptor().id, intent.bucket, TileTier::Final);
    conductor.publish_intent(intent);

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !tiles.contains(exact) {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        tiles.contains(exact),
        "thumb dragging should render the latest exact viewport without waiting for release"
    );
    assert!(
        !previews.contains(destination),
        "active skimming should use only previews that were already cached"
    );
    drop(conductor);
}

#[test]
fn conductor_completes_predicted_final_tiles_before_they_are_visible() {
    use lege_viewer::document::{
        ConductorHandle, MemoryArbiter, NavigationMode, TileCache, TileTier, UpdateQueue,
        ViewportPlanner,
    };
    use lege_viewer::geometry::Vec2d;

    let engine: Arc<dyn DocumentEngine> = Arc::new(SyntheticEngine::new(20));
    let layout = Arc::new(PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    ));
    let memory = MemoryArbiter::new(64 * 1024 * 1024);
    let tiles = Arc::new(TileCache::new(engine.descriptor().id, memory.clone()));
    let updates = UpdateQueue::new(256, Arc::new(TestWake::default()));
    let conductor = ConductorHandle::spawn(
        engine.clone(),
        layout.clone(),
        updates,
        memory,
        tiles.clone(),
    )
    .expect("spawn predictive conductor");
    let intent = ViewportPlanner::default().build_with_navigation(
        1,
        &layout,
        Vec2d::ZERO,
        Vec2d { x: 0.0, y: 2_000.0 },
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
        NavigationMode::SequentialForward,
    );
    let predicted = *intent
        .final_prefetch_tiles
        .first()
        .expect("an exact-quality off-screen prediction");
    assert!(!predicted.visible);
    let expected = predicted.key(engine.descriptor().id, intent.bucket, TileTier::Final);
    conductor.publish_intent(intent);

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !tiles.contains(expected) {
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(conductor);
    assert!(
        tiles.contains(expected),
        "the next page should be final-quality before it enters the viewport"
    );
}

#[test]
fn rapid_replan_reaches_the_new_viewport_and_drains_old_work() {
    use lege_viewer::document::{
        ConductorHandle, MemoryArbiter, SessionUpdate, TileCache, UpdateQueue, ViewportPlanner,
    };
    use lege_viewer::geometry::Vec2d;

    let engine: Arc<dyn DocumentEngine> = Arc::new(SyntheticEngine::new(10_000));
    let layout = Arc::new(PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    ));
    let memory = MemoryArbiter::new(64 * 1024 * 1024);
    let tiles = Arc::new(TileCache::new(engine.descriptor().id, memory.clone()));
    let updates = UpdateQueue::new(256, Arc::new(TestWake::default()));
    let conductor = ConductorHandle::spawn(engine, layout.clone(), updates.clone(), memory, tiles)
        .expect("spawn replan conductor");
    let planner = ViewportPlanner::default();
    conductor.publish_intent(planner.build(
        1,
        &layout,
        Vec2d::ZERO,
        Vec2d::ZERO,
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
    ));
    let destination_page = PageIndex(5_000);
    let destination_y = layout
        .placement(destination_page)
        .expect("destination placement")
        .bounds
        .y;
    conductor.publish_intent(planner.build(
        2,
        &layout,
        Vec2d {
            x: 0.0,
            y: destination_y,
        },
        Vec2d {
            x: 0.0,
            y: 50_000.0,
        },
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
    ));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut reached_destination = false;
    let mut idle = false;
    while Instant::now() < deadline && !(reached_destination && idle) {
        for update in updates.drain() {
            match update {
                SessionUpdate::TileReady { key, .. } if key.page == destination_page => {
                    reached_destination = true;
                }
                SessionUpdate::QueueDepths {
                    compile_pending,
                    raster_pending,
                    in_flight,
                } => {
                    idle = compile_pending == 0 && raster_pending == 0 && in_flight == 0;
                }
                SessionUpdate::PageError { message, .. } => {
                    panic!("replan conductor reported an error: {message}")
                }
                SessionUpdate::PageCompiled(_)
                | SessionUpdate::TileReady { .. }
                | SessionUpdate::PreviewReady { .. }
                | SessionUpdate::PageIndexed { .. }
                | SessionUpdate::TextIndexProgress { .. }
                | SessionUpdate::SearchCompleted { .. } => {}
            }
        }
        if !(reached_destination && idle) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    drop(conductor);
    assert!(reached_destination);
    assert!(idle);
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
    assert!(intent.compile_pages.contains(&PageIndex(10)));
}

#[test]
fn directional_planning_prioritizes_visible_work_and_prefetches_ahead() {
    use lege_viewer::document::{NavigationMode, ViewportPlanner};
    use lege_viewer::geometry::Vec2d;

    let engine = SyntheticEngine::new(200);
    let layout = PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    );
    let destination = PageIndex(100);
    let destination_y = layout.placement(destination).unwrap().bounds.y;
    let intent = ViewportPlanner::default().build_with_navigation(
        9,
        &layout,
        Vec2d {
            x: 0.0,
            y: destination_y,
        },
        Vec2d {
            x: 0.0,
            y: 50_000.0,
        },
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
        NavigationMode::SequentialForward,
    );

    let visible_page = intent.visible_tiles.first().unwrap().page;
    assert_eq!(intent.compile_pages.first(), Some(&visible_page));
    assert!(!intent.final_prefetch_tiles.is_empty());
    assert!(
        intent
            .final_prefetch_tiles
            .iter()
            .all(|demand| demand.page > visible_page)
    );
    // The planner admits up to five exact-quality pages within its explicit
    // 160-tile runway budget.
    assert!(intent.final_prefetch_tiles.len() <= 160);
    let forward_previews = intent
        .preview_pages
        .iter()
        .filter(|page| **page > visible_page)
        .count();
    let backward_previews = intent
        .preview_pages
        .iter()
        .filter(|page| **page < visible_page)
        .count();
    assert!(forward_previews > backward_previews);
}

#[test]
fn promoted_tiles_replace_lower_quality_exact_entries() {
    use lege_viewer::document::{
        DocumentId, MemoryArbiter, TileCache, TileCoord, TileKey, TileSurface, TileTier,
    };
    use lege_viewer::geometry::RectI;
    use lege_viewer::paint::PixelSurface;

    let document = DocumentId(12);
    let cache = TileCache::new(document, MemoryArbiter::new(1 << 20));
    let surface = |tier, generation| {
        Arc::new(TileSurface {
            key: TileKey {
                document,
                page: PageIndex(3),
                bucket: ZoomBucket::ONE,
                coord: TileCoord { x: 1, y: 2 },
                tier,
                variant: 0,
            },
            generation,
            page_device_rect: RectI {
                x: 256,
                y: 512,
                width: 1,
                height: 1,
            },
            page_document_rect: RectF {
                x: 256.0,
                y: 512.0,
                width: 1.0,
                height: 1.0,
            },
            pixels: PixelSurface {
                width: 1,
                height: 1,
                stride: 1,
                pixels: vec![0].into(),
            },
            degraded: false,
        })
    };

    cache.insert(surface(TileTier::Draft, 1), 0.0);
    cache.insert(surface(TileTier::TextFirst, 1), 0.0);
    assert_eq!(cache.len(), 1);
    cache.insert(surface(TileTier::Final, 1), 0.0);
    cache.insert(surface(TileTier::TextFirst, 2), 0.0);
    assert_eq!(cache.len(), 1);
    assert!(cache.contains(TileKey {
        document,
        page: PageIndex(3),
        bucket: ZoomBucket::ONE,
        coord: TileCoord { x: 1, y: 2 },
        tier: TileTier::Final,
        variant: 0,
    }));
}

#[test]
fn visibility_below_document_is_empty() {
    let engine = SyntheticEngine::new(2);
    let layout = PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    );
    assert!(
        layout
            .visible_pages(RectF {
                x: 0.0,
                y: layout.total_height + 10.0,
                width: 800.0,
                height: 600.0,
            })
            .is_empty()
    );
}

#[cfg(feature = "pdf-engine")]
#[test]
fn real_pdf_compiles_text_and_renders_a_nonblank_tile() {
    use lege_viewer::document::engine::RasterPass;
    use lege_viewer::document::pdf_engine::PdfEngine;
    use lege_viewer::document::{ViewportPlanner, ZoomBucket};
    use lege_viewer::geometry::Vec2d;

    let started = Instant::now();
    let engine = PdfEngine::open(viewer_pdf_fixture(), None).expect("open viewer PDF fixture");
    let layout = PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    );
    let placement = layout
        .placement(PageIndex(0))
        .expect("first page placement");
    let cancellation = CancellationFlag::default();
    let artifacts = engine
        .compile_page(PageIndex(0), placement.page_to_doc, &cancellation)
        .expect("compile first PDF page");
    assert!(artifacts.compiled.operation_count() > 0);
    assert!(!artifacts.text.substrate().characters.is_empty());

    let intent = ViewportPlanner::default().build(
        1,
        &layout,
        Vec2d::ZERO,
        Vec2d::ZERO,
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
    );
    let demand = *intent.visible_tiles.first().expect("visible PDF tile");
    let structure = engine
        .raster_tile(
            &artifacts,
            ZoomBucket::ONE,
            demand,
            RasterPass::TextFirst,
            1,
            &cancellation,
        )
        .expect("render structural PDF tile");
    assert_eq!(
        structure.key.tier,
        lege_viewer::document::TileTier::TextFirst
    );
    assert!(
        structure
            .pixels
            .pixels
            .iter()
            .any(|pixel| *pixel != 0x00ff_ffff),
        "structural tile must preserve visible text geometry"
    );
    assert!(
        !engine.image_renderer_initialized(),
        "text-first work must not initialize the image renderer"
    );

    let tile = engine
        .raster_tile(
            &artifacts,
            ZoomBucket::ONE,
            demand,
            RasterPass::Final,
            1,
            &cancellation,
        )
        .expect("render first PDF tile");
    assert_eq!(tile.pixels.width, demand.page_device_rect.width);
    assert_eq!(tile.pixels.height, demand.page_device_rect.height);
    assert!(
        tile.pixels.pixels.iter().any(|pixel| *pixel != 0x00ff_ffff),
        "rendered tile must contain document ink"
    );
    assert!(
        engine.image_renderer_initialized(),
        "final raster work must initialize the renderer policy"
    );
    eprintln!(
        "viewer fixture open + first compiled/rastered tile: {:?}",
        started.elapsed()
    );
}

#[cfg(feature = "pdf-engine")]
#[test]
fn cancelled_pdf_raster_stops_before_producing_a_tile() {
    use lege_viewer::document::ViewportPlanner;
    use lege_viewer::document::engine::{DocumentEngineError, RasterPass};
    use lege_viewer::document::pdf_engine::PdfEngine;
    use lege_viewer::geometry::Vec2d;

    let engine = PdfEngine::open(viewer_pdf_fixture(), None).expect("open cancellation fixture");
    let layout = PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    );
    let placement = layout
        .placement(PageIndex(0))
        .expect("first page placement");
    let compile_cancel = CancellationFlag::default();
    let artifacts = engine
        .compile_page(PageIndex(0), placement.page_to_doc, &compile_cancel)
        .expect("compile cancellation fixture");
    let intent = ViewportPlanner::default().build(
        1,
        &layout,
        Vec2d::ZERO,
        Vec2d::ZERO,
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
    );
    let demand = *intent.visible_tiles.first().expect("visible tile");
    let raster_cancel = CancellationFlag::default();
    raster_cancel.cancel();
    let error = engine
        .raster_tile(
            &artifacts,
            intent.bucket,
            demand,
            RasterPass::Final,
            1,
            &raster_cancel,
        )
        .expect_err("cancelled raster must not produce a tile");
    assert!(matches!(error, DocumentEngineError::Cancelled));
}

#[cfg(feature = "pdf-engine")]
#[test]
fn real_pdf_conductor_reaches_the_tile_cache_without_blocking_the_caller() {
    use lege_viewer::document::pdf_engine::PdfEngine;
    use lege_viewer::document::{
        ConductorHandle, MemoryArbiter, SessionUpdate, TileCache, UpdateQueue, ViewportPlanner,
    };
    use lege_viewer::geometry::Vec2d;

    let started = Instant::now();
    let engine: Arc<dyn DocumentEngine> = Arc::new(
        PdfEngine::open(viewer_pdf_fixture(), None).expect("open asynchronous viewer fixture"),
    );
    let layout = Arc::new(PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    ));
    let memory = MemoryArbiter::new(64 * 1024 * 1024);
    let tiles = Arc::new(TileCache::new(engine.descriptor().id, memory.clone()));
    let updates = UpdateQueue::new(128, Arc::new(TestWake::default()));
    let conductor = ConductorHandle::spawn(
        engine,
        layout.clone(),
        updates.clone(),
        memory,
        tiles.clone(),
    )
    .expect("spawn PDF conductor");
    conductor.publish_intent(ViewportPlanner::default().build(
        1,
        &layout,
        Vec2d::ZERO,
        Vec2d::ZERO,
        SizeF {
            width: 800.0,
            height: 600.0,
        },
        1.0,
        None,
    ));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut compiled = false;
    let mut tile_ready = false;
    while Instant::now() < deadline && !(compiled && tile_ready) {
        for update in updates.drain() {
            match update {
                SessionUpdate::PageCompiled(_) => compiled = true,
                SessionUpdate::TileReady { .. } => tile_ready = true,
                SessionUpdate::PageError { message, .. } => {
                    panic!("PDF conductor reported an error: {message}")
                }
                SessionUpdate::QueueDepths { .. }
                | SessionUpdate::PreviewReady { .. }
                | SessionUpdate::PageIndexed { .. }
                | SessionUpdate::TextIndexProgress { .. }
                | SessionUpdate::SearchCompleted { .. } => {}
            }
        }
        if !(compiled && tile_ready) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    drop(conductor);
    assert!(compiled);
    assert!(tile_ready);
    assert!(!tiles.is_empty());
    eprintln!(
        "viewer asynchronous open-to-first-tile: {:?}",
        started.elapsed()
    );
}

/// Stage 7: a page the renderer cannot compile must not stop the document,
/// and must not be retried forever either.
#[derive(Debug)]
struct BrokenPageEngine {
    inner: SyntheticEngine,
    broken: PageIndex,
}

struct BrokenCompileWorker {
    inner: Box<dyn lege_viewer::document::engine::DocumentCompileWorker>,
    broken: PageIndex,
}

impl lege_viewer::document::engine::DocumentCompileWorker for BrokenCompileWorker {
    fn compile_page(
        &mut self,
        page: PageIndex,
        page_to_doc: lege_viewer::geometry::Affine,
        cancellation: &CancellationFlag,
    ) -> Result<
        Arc<lege_viewer::document::engine::CompiledArtifacts>,
        lege_viewer::document::engine::DocumentEngineError,
    > {
        if page == self.broken {
            return Err(lege_viewer::document::engine::DocumentEngineError::Engine(
                "synthetic page defect".to_owned(),
            ));
        }
        self.inner.compile_page(page, page_to_doc, cancellation)
    }
}

impl DocumentEngine for BrokenPageEngine {
    fn descriptor(&self) -> &lege_viewer::document::engine::DocumentDescriptor {
        self.inner.descriptor()
    }

    fn create_compile_worker(
        &self,
    ) -> Box<dyn lege_viewer::document::engine::DocumentCompileWorker> {
        Box::new(BrokenCompileWorker {
            inner: self.inner.create_compile_worker(),
            broken: self.broken,
        })
    }

    fn create_raster_worker(&self) -> Box<dyn lege_viewer::document::engine::DocumentRasterWorker> {
        self.inner.create_raster_worker()
    }
}

#[test]
fn a_broken_page_is_reported_set_aside_and_does_not_stop_the_document() {
    use lege_viewer::document::{
        ConductorHandle, MemoryArbiter, SessionUpdate, TileCache, UpdateQueue, ViewportPlanner,
    };
    use lege_viewer::geometry::Vec2d;

    let engine: Arc<dyn DocumentEngine> = Arc::new(BrokenPageEngine {
        inner: SyntheticEngine::new(8),
        broken: PageIndex(0),
    });
    let layout = Arc::new(PageLayoutIndex::build(
        &engine.descriptor().page_geometries,
        &Theme::default().metrics,
    ));
    let memory = MemoryArbiter::new(64 * 1024 * 1024);
    let tiles = Arc::new(TileCache::new(engine.descriptor().id, memory.clone()));
    let updates = UpdateQueue::new(256, Arc::new(TestWake::default()));
    let conductor = ConductorHandle::spawn(
        engine,
        layout.clone(),
        updates.clone(),
        memory,
        tiles.clone(),
    )
    .expect("spawn conductor over a document with a broken page");
    // Tall enough that the broken first page and a healthy second page are
    // visible together: a page-local failure must stay page-local.
    let viewport = SizeF {
        width: 800.0,
        height: 1_800.0,
    };
    let intent = |generation: u64| {
        ViewportPlanner::default().build(
            generation,
            &layout,
            Vec2d::ZERO,
            Vec2d::ZERO,
            viewport,
            1.0,
            None,
        )
    };
    conductor.publish_intent(intent(1));

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut set_aside = false;
    let mut healthy_tile = false;
    let mut generation = 1;
    while Instant::now() < deadline && !(set_aside && healthy_tile) {
        for update in updates.drain() {
            match update {
                SessionUpdate::PageError {
                    page, quarantined, ..
                } => {
                    assert_eq!(page, PageIndex(0), "only the broken page may fail");
                    set_aside |= quarantined;
                }
                SessionUpdate::TileReady { key, .. } => {
                    healthy_tile |= key.page != PageIndex(0);
                }
                _ => {}
            }
        }
        generation += 1;
        conductor.publish_intent(intent(generation));
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        set_aside,
        "the broken page was never set aside; it would be retried forever"
    );
    assert!(
        healthy_tile,
        "a page-local failure stopped the rest of the document from rendering"
    );

    // Once set aside it stays set aside: replanning must not resurrect it.
    let settle = Instant::now() + Duration::from_millis(400);
    let mut retries = 0;
    while Instant::now() < settle {
        for update in updates.drain() {
            if matches!(update, SessionUpdate::PageError { .. }) {
                retries += 1;
            }
        }
        generation += 1;
        conductor.publish_intent(intent(generation));
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(conductor);
    assert_eq!(
        retries, 0,
        "a quarantined page was compiled again on the next replan"
    );
}

#[cfg(feature = "pdf-engine")]
fn encrypted_pdf_fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lege-pdf/render/crates/pdf-document/tests/fixtures")
        .join(name)
}

/// Stage 7: an encrypted document is a question, not a failure. The viewer has
/// to be able to tell the two apart before it can ask it.
#[cfg(feature = "pdf-engine")]
#[test]
fn an_encrypted_document_asks_for_a_password_rather_than_failing_to_open() {
    use lege_viewer::document::engine::DocumentEngineError;
    use lege_viewer::document::pdf_engine::PdfEngine;

    // PDFium's own fixture; user password "hôtel".
    let path = encrypted_pdf_fixture("encrypted_hello_world_r3.pdf");
    let error = PdfEngine::open(&path, None).expect_err("locked document");
    assert!(
        matches!(error, DocumentEngineError::PasswordRequired),
        "an encrypted document reported as {error} would send the reader to a file dialog"
    );

    let error =
        PdfEngine::open(&path, Some("not the password")).expect_err("wrong password rejected");
    assert!(
        matches!(error, DocumentEngineError::IncorrectPassword),
        "a mistyped password must be distinguishable from a broken file, not {error}"
    );

    let engine = PdfEngine::open(&path, Some("h\u{f4}tel")).expect("correct password opens");
    assert_eq!(engine.descriptor().page_count, 1);
}

/// A corrupt file must not be mistaken for a locked one: offering a password
/// field for a document no password can open is worse than saying it failed.
#[cfg(feature = "pdf-engine")]
#[test]
fn a_malformed_document_is_not_reported_as_needing_a_password() {
    use lege_viewer::document::engine::DocumentEngineError;
    use lege_viewer::document::pdf_engine::PdfEngine;
    use std::io::Write;

    let mut file = tempfile::NamedTempFile::new().expect("temporary file");
    file.write_all(b"%PDF-1.7\nthis is not a PDF body\n")
        .expect("write malformed document");
    let error = PdfEngine::open(file.path(), None).expect_err("malformed document rejected");
    assert!(
        matches!(error, DocumentEngineError::Engine(_)),
        "a malformed document reported as {error} would prompt for a password that cannot exist"
    );
}
