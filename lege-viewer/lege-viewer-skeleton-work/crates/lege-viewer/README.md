# Lege Viewer skeleton

This crate turns the three viewer plans into a concrete Rust architecture. It
is intentionally a **skeleton**, not a feature-complete viewer and not a generic
GUI toolkit.

## What is already architectural code

- One application-specific `winit` UI loop with retained software framebuffer,
  damage tracking, concrete chrome layout, custom scrollbar, and redraw-on-demand.
- A `Presenter` seam with `softbuffer` reference implementation and a reserved
  `wgpu` destination.
- A document-engine boundary returning semantic page, native text substrate,
  compiled display-list IR, links/content structure, and raster tiles together.
- A conductor thread owning priorities and lifecycle, with separate compile and
  raster pools, continuous cancellation against `ArcSwap<ViewportIntent>`, panic
  quarantine, bounded/coalesced UI updates, and memory leases.
- Tiles as the only raster currency (`256×256`), power-of-√2 zoom buckets, and
  `Final → TextFirst → Draft → neighboring bucket → thumbnail → skeleton`
  selection semantics.
- Line-continuity paging, geometric fallback, reading anchors, jump-only browser
  history, text search offsets, and selection overlay contracts.
- Explicit renderer-owned feature seams for IR night mode, exact content extent,
  link peek, embedded/synthesized outlines, pass-split raster, and tile-run output.

## Renderer changes included

`pdf-content::PageCompiler::compile_artifacts()` now returns the retained
`SemanticPage` and lowered `CompiledPage` from one interpretation pass. This is
the critical anti-black-box seam: the viewer does not re-parse for text or ask a
bitmap service to rediscover structure.

`pdf-render-api::CancellationToken` can now share the viewer conductor's atomic
cancellation flag, so viewport relevance is checked before and during renderer
work.

## Running the synthetic architecture proof

```text
cargo run -p lege-viewer
```

The default binary opens a 10,000-page synthetic document so scrolling, layout,
paging, tile scheduling, cache behavior, and presentation can be developed
without coupling every UI change to PDF correctness.

With the renderer feature enabled:

```text
cargo run -p lege-viewer --features pdf-engine -- document.pdf
```

The supplied archive omitted its original root `Cargo.toml` and the sibling
`jp2lam` / `jbig2enc-rust` repositories. A workspace manifest was reconstructed
using the paths documented inside the renderer archive; adjust those two paths
if the local Lege ecosystem is laid out differently.

## Deliberate implementation gaps

These are represented in types and ownership already, but still need real work:

1. CPU display-list filtering for `RasterPass::TextFirst`, followed by image and
   shading upgrades into the same tile.
2. Tile-run batching in the renderer API to amortize request setup.
3. Exact IR content extents, link extraction/destinations, outline extraction,
   and typography-based outline synthesis.
4. A real `WgpuPresenter` texture atlas and fractional placement.
5. UI glyph shaping/cache, search-field editing/IME, clipboard, and accessibility.
6. Whole-document indexed search, selection hit-testing, scrollbar search marks,
   background thumbnail sweep, and link-peek popup composition. The hover ring
   and tiled preview popup are already wired.
7. A unified budget callback into renderer image/font/glyph caches; the viewer
   arbiter currently owns viewer-side leases only.

## Recommended implementing order

Keep the synthetic document as the behavioral reference. First make the default
crate compile and pass `tests/architecture.rs`, then wire the real PDF engine,
then implement text-first pass splitting and tile runs. Add WGPU only after the
software compositor has golden frames and trace-replay tests; WGPU replaces the
presenter, not the document model or UI.
