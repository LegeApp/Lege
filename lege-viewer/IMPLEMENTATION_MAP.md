# Implementation map

Roadmap authority and stage completion criteria: [`STAGES.md`](STAGES.md).
This file maps concepts to code; it does not define development order.

| Plan concept | Concrete code |
|---|---|
| Renderer is a document engine | `document/engine.rs`, renderer `compile_artifacts()` patch |
| Viewport intent, continuous cancellation | `document/viewport.rs`, `document/conductor.rs` |
| Navigation prediction and settle debounce | `NavigationMode`, `ViewerApp::finish_direct_scroll`, `ViewerApp::about_to_wait` |
| Visible-first compile and off-screen final prefetch | `ViewportIntent::compile_pages`, `final_prefetch_tiles`, `Conductor::replan` |
| Universal seek/scan preview layer | `document/preview.rs`, `PagePreviewCache`, `paint_scene` |
| Preview-only scrollbar skimming | `NavigationMode::Skimming`, `ViewerApp::finish_direct_scroll`, `Conductor::replan` |
| Promotable compile/index/preview work | `CompileNeeds`, `CompileState`, `Conductor::request_compile` |
| Seek-stage instrumentation | `diagnostics::SeekTrace`, `ViewerApp::compose` |
| Coalescing wake queue | `document/session.rs` |
| Compile/raster role separation | `document/conductor.rs` worker pools |
| One CPU/GPU memory arbiter | `document/cache.rs`, `ViewerApp::sync_presenter_stats` |
| Tiles-only surface model | `document/tile.rs`, `document/cache.rs` |
| Progressive tiers | `RasterPass`, `TileTier`, `TileCache::best_covering`, `paint_scene` |
| PDF structural fallback | `pdf_engine::raster_text_structure` |
| Tier promotion and navigation-aware eviction | `TileCache::insert`, `TileCache::refresh_distances` |
| Text/line substrate | `text/line.rs`, `TextArtifact` |
| Progressive whole-document indexing | `document/conductor.rs` (unified preview/index sweep), `SessionUpdate::PageIndexed` |
| Bounded search text store | `text/search.rs` (`SearchIndex`, 64 MiB anonymous spill) |
| Cancellable background search | `text/search.rs` (`SearchService`), `SessionUpdate::SearchCompleted` |
| Search editing/navigation/overlays | `app.rs` (`SearchUiState`, search editor, `paint_scene`) |
| Document selection and copy | `text/selection.rs`, `app.rs` hit testing/autoscroll/clipboard |
| PDF outline/destination extraction | `pdf-document/src/outline.rs`, `document/pdf_engine.rs` |
| PDF links and predictive link peek | `pdf-document/src/links.rs`, `ViewerApp::link_at`, `paint_link_peek` |
| Synthesized/virtual contents | `text/outline.rs`, `ViewerApp::render_outline_surfaces` |
| Glyph-backed chrome | `ui.rs`, `SceneSurface`, renderer `pdf-font` program data |
| Alpha overlay parity | `SceneCommand::AlphaSolid`, `Painter::blend_rect`, `lege-gpu::presentation` |
| Line-continuity paging | `scroll/paging.rs` |
| Reading anchor | `scroll/anchor.rs` |
| Jump-only history | `scroll/history.rs` |
| Custom scrollbar/document map | `chrome/scrollbar.rs`, `app.rs` (ticks, delayed hover popup, thumbnail ring) |
| Backend-neutral retained scene | `scene.rs`, `paint_scene` |
| Immutable frame tile view | `TileFrameSnapshot`, `TileCache::frame_snapshot` |
| Software reference presenter | `present/softbuffer.rs`, `paint.rs`, `damage.rs` |
| GPU steady-state presenter | `present/wgpu.rs`, `lege-gpu::presentation` |
| Bounded texture atlas | `lege-gpu/src/presentation.rs` |
| Automatic GPU fallback | `ViewerApp::create_window`, `ViewerApp::present_scene` |
| Presenter selection/diagnostics | `cli.rs`, `PresenterStats`, `ViewerApp::sync_presenter_stats` |
| Compiled-IR content extent | renderer `pdf-content/lower.rs`, `CompiledPage::content_bounds`, `document/pdf_engine.rs` |
| Anchored content-aware trim | `document/layout.rs`, `scroll/anchor.rs`, `ViewerApp::rebuild_stage5_layout` |
| Renderer-owned Night/Warm Paper | `pdf-render-api::RenderColorPolicy`, `pdf-render-cpu::prepared`, `document/pdf_engine.rs` |
| Crop/palette cache identity | `PageLayoutIndex::render_variant`, `TileKey::variant`, `PagePreviewCache::get_variant` |
| Persistent display settings | `settings.rs`, toolbar and `M`/`N` controls in `app.rs` |
| Page panic quarantine | `document/conductor.rs`, `SessionUpdate::PageError` |
| Synthetic 10k-page proof | `document/synthetic.rs`, explicit `--synthetic` launch mode |
| Production PDF launch | `cli.rs`, `main.rs`, `document/pdf_engine.rs` |
| Stage 1–5 + seek/scan integration gates | `tests/architecture.rs`, headless/unit tests in `app.rs`, `document/layout.rs`, and `text`; renderer content-bound/color-policy tests; outline/link fixtures in `pdf-document`; compositor tests in `lege-gpu` |
