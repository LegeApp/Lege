# Seek and scan optimization pass

This pass sits between viewer Stages 3 and 4. Its target is not merely lower
raster time; it is to make the likely destination complete before it becomes
visible and to make cold destinations structurally legible while final pixels
arrive.

## Implemented policy

`NavigationMode` records five useful states:

- `SequentialForward`
- `SequentialBackward`
- `JumpLikely`
- `Skimming`
- `Idle`

Wheel, fine-scroll, page-step, outline/search navigation, and Home/End publish
the appropriate mode. A held scrollbar thumb uses `Skimming`: the viewer
shows canonical previews but deliberately does not raster transient exact
destinations. Direct movement settles to `Idle`
after 140 ms without input. That transition publishes a new intent, so
quality work cannot remain suppressed after the final wheel event.

Every viewport intent now carries work in scheduling order:

1. visible pages and tiles;
2. exact-bucket `Final` tiles for the most likely next one or two pages;
3. directional overscan structural tiles;
4. canonical whole-page previews from the document-wide L0 cache;
5. scrollbar-hover thumbnails;
6. a combined whole-document preview/index sweep, only while navigation is
   idle.

Exact off-screen work is capped at 32 tiles per intent. At high zoom, a whole
page that exceeds the cap is skipped instead of consuming the raster pool.
The preview layer remains available as a page-sized underlay at every zoom.

Compilation order is semantic, not numeric: the visible page is always first,
followed by exact-quality predictions, overscan pages, and previews. One
promotable state owns compilation, text indexing, and preview needs for a
page. If background work becomes visible, its priority and interactive
generation merge in place; the page is not compiled again.

Compile and raster handoff channels are deliberately one item deep. Nearly
all priority therefore remains in the conductor heap instead of becoming
trapped in a lower-priority FIFO, while a newly spawned worker can never miss
the only dispatch attempt. Background compilation uses at most the pool minus
one worker when possible, and the preview backlog is bounded.

## Canonical preview layer

`PagePreviewCache` is a page-indexed L0 store separate from the exact tile
cache. The idle sweep reuses each page's one compilation result for text
indexing and a small full-page bitmap. The preview is painted under exact
tiles in the main canvas and reused by the scrollbar popup.

The layer has a 128 MiB document-wide target. Its per-page pixel allowance is
computed from the page count, so very large documents receive smaller
canonical previews instead of turning a universal fallback into unbounded
memory.

## Structural PDF tier

Text PDFs now support a cheap `TextFirst` tile generated from the compiled
character geometry. It paints layout-correct grayscale character silhouettes
without running images, shadings, or the full CPU display list. This is an
honest structural fallback, not the final renderer result.

The longer-term renderer integration remains a true vector/text display-list
pass followed by image/shading completion. Image-only pages now use the
canonical preview followed by a single `Final` pass. The old `Draft` then
`Final` sequence was removed because those renderer passes currently perform
the same work. Text pages retain the honest `TextFirst` structural pass
before final pixels.

## Update and timing behavior

Off-screen preview completions are installed without requesting a UI redraw.
Text-index progress is ingested without repainting the canvas unless search
or outline chrome can visibly change. `SeekTrace` records intent publication,
first pixels ready, first pixels presented, and exact viewport presentation
for each navigation generation.

## Cache behavior

When an exact tile is promoted, lower tiers at the same
`(document, page, bucket, coordinate)` are removed. A late lower-quality
completion cannot replace an existing higher tier. Navigation replans also
refresh each cached tile's distance so eviction decisions do not retain stale
viewport classifications.

The exact tile cache now publishes one immutable snapshot per page. Inserting
or promoting a tile clones only that page's tile map, and a frame captures only
the visible, overscan, exact-prefetch, thumbnail, and hover pages. Painting and
fallback selection therefore never clone or scan the complete document tile
map. The canonical preview cache remains separate and directly page-indexed.

## Explicit warm destinations

`WarmHint` supplies a page, navigation reason, probability, optional region,
and expiry. The conductor merges repeated hints by page, promotes pending
compilation in place, and keeps warm compilation/preview work alive only while
the hint remains current. Warm work prepares semantic text and the canonical
preview; it does not masquerade as visible work or start an exact-tile storm.

The app currently emits bounded, deduplicated hints for:

- outline hover and activation;
- active and adjacent search results;
- back/forward history;
- scrollbar prediction.

The existing link-peek conductor entry point now uses the same warm-hint path;
page-number typeahead and viewer-side link hit testing can submit hints when
those UI features are implemented.

Visible and confirmed target work still outranks speculative hints. Background
sweep work remains below them. This establishes the P2 API seam for page-number
typeahead and stronger L1 representations without coupling those features to
the viewport planner.

Persistent preview packs are deliberately deferred until the document source
has a stable fingerprint and cache-version seam. Reusing previews across
reopens without reliable invalidation would risk displaying pixels from a
different revision of the document.

## Textless page paging

Page Up and Page Down retain their line-continuity contract on pages without
usable text. Such pages expose ten geometric rows solely as paging anchors, so
the last fully visible tenth becomes the first after Page Down and the first
becomes the last after Page Up. These anchors never enter selection or search
data.

## Certification

The architecture tests assert:

- visible-first compile order;
- forward-only predicted pages during forward navigation;
- the 32-tile exact prefetch bound;
- completion of a predicted `Final` tile while it is still off screen;
- replacement and late-arrival rejection across tile tiers;
- a nonblank structural tile from a real PDF;
- rapid random replanning reaches the new viewport and drains obsolete work.
- a held scrollbar skim produces a canonical preview without exact tiles;
- the idle sweep reaches the final page of the document-wide preview layer;
- compile needs promote in place while retaining index/preview obligations;
- canonical preview resolution stays inside its per-page memory allowance;
- seek timing stages are monotonic and recorded once.
- textless pages retain Page Up/Page Down continuity through notional rows;
- frame snapshots can be scoped to relevant pages and remain immutable;
- an explicit far-off warm hint produces a preview even during skimming.

The remaining empirical work is a real-document interactive timing capture:
sequential PageDown, wheel scanning, a cold scrollbar seek, and an outline
jump under both presenters. This should tune the 140 ms settle delay and the
32-tile/8-preview bounds; it should not change the ownership model.
