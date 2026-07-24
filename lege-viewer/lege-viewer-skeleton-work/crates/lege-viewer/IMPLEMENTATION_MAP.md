# Implementation map

| Plan concept | Concrete code |
|---|---|
| Renderer is a document engine | `document/engine.rs`, renderer `compile_artifacts()` patch |
| Viewport intent, continuous cancellation | `document/viewport.rs`, `document/conductor.rs` |
| Coalescing wake queue | `document/session.rs` |
| Compile/raster role separation | `document/conductor.rs` worker pools |
| One memory arbiter | `document/cache.rs` |
| Tiles-only surface model | `document/tile.rs`, `document/cache.rs` |
| Progressive tiers | `RasterPass`, `TileTier`, `TileCache::best_covering`, `paint_scene` |
| Text/line substrate | `text/line.rs`, `TextArtifact` |
| Search and selection | `text/search.rs`, `text/selection.rs` |
| Line-continuity paging | `scroll/paging.rs` |
| Reading anchor | `scroll/anchor.rs` |
| Jump-only history | `scroll/history.rs` |
| Custom scrollbar/document map | `chrome/scrollbar.rs`, `app.rs` (ticks, delayed hover popup, thumbnail ring) |
| Software reference presenter | `present/softbuffer.rs`, `paint.rs`, `damage.rs` |
| GPU steady-state seam | `present/wgpu.rs`, `Presenter` |
| IR night mode/margin trim/link peek | `document/features.rs` |
| Page panic quarantine | `document/conductor.rs`, `SessionUpdate::PageError` |
| Synthetic 10k-page proof | `document/synthetic.rs`, default binary |
