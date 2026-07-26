# The right target

Use the renderer’s speed aggressively, but **brute-force coverage rather than final fidelity**.

The viewer should not define success as “the final page at every possible zoom was pre-rendered.” That expands into page × zoom × rotation × annotation state × presentation mode, and it wastes enormous memory and CPU.

Define success as:

1. **The correct page is visible immediately.**
2. **The visible reading area becomes readable almost immediately.**
3. **Exact pixels replace it without flicker or blank regions.**

That implies three representations:

| Layer                                | Scope                                                    | Purpose                                                  |
| ------------------------------------ | -------------------------------------------------------- | -------------------------------------------------------- |
| **L0: universal page preview**       | Every page                                               | Guarantees that a random seek never lands on blank paper |
| **L1: readable warm representation** | Sequential-ahead pages, navigation targets, likely jumps | Makes likely seeks readable before exact work finishes   |
| **L2: exact current-bucket raster**  | Visible pages and a cost-bounded hot ring                | Final display quality                                    |

For small documents, Lege can additionally brute-force L2 for the whole document. For larger documents, L0 should be universal, L1 probabilistic, and L2 demand-driven.

That is the architecture I would build.

---

# What is currently delaying random seeks

The uploaded viewer is structurally sound, but several implementation details are working against the intended priority model.

## 1. The sought page can be compiled behind nearby pages

In `src/document/viewport.rs:156–199`, compile candidates are gathered into a `BTreeSet`. This means they come out in numeric page order.

On a seek to page 500, the compile list can become:

```text
498, 499, 500, 501, 502...
```

Then `src/document/conductor.rs:304–307` queues that list before visible tile scheduling.

So the page the user actually requested may be the third compile job, even before background work is considered.

## 2. A queued compile cannot be promoted

`ensure_compiled()` in `conductor.rs:355–381` returns immediately when the page is already queued or in flight.

That means this sequence is possible:

1. Page 500 is queued as compile-ahead at priority 798.
2. Visible tile planning discovers page 500 needs to be shown.
3. `ensure_compiled(page, 1000)` returns because the page is already queued.
4. It remains at priority 798.

This is a direct priority-promotion bug.

## 3. Background indexing can occupy the complete compile pool

The code creates:

```rust
let compile_workers = (workers / 3).max(1);
```

and permits two background index jobs. On an eight-thread machine, there will generally be two compile workers, meaning background indexing can occupy both.

Furthermore, the compile channel is buffered to `compile_workers * 2`. Low-priority work can leave the conductor’s priority heap and become trapped in a FIFO worker channel. A new high-priority jump cannot overtake it.

## 4. Indexing and interactive compilation are separate jobs

The current keys are:

```rust
enum WorkKey {
    Compile(PageIndex),
    Index(PageIndex),
    Raster(TileKey),
}
```

Consequently, a background `Index(page)` and a foreground `Compile(page)` may compile the same page concurrently.

More importantly, when a text-index compilation completes, `conductor.rs:556–565` publishes the text and then discards the complete `CompiledArtifacts`. That compilation already produced precisely the IR needed to:

* generate a whole-page preview;
* satisfy a subsequent interactive compile;
* retain a stripped raster artifact;
* estimate page rendering cost.

The viewer is paying for the most expensive preparation and throwing away most of its result.

## 5. Draft and Final are currently duplicate work

The viewer sends:

```rust
quality: if matches!(pass, RasterPass::Thumbnail | RasterPass::Draft) {
    RenderQuality::Draft
} else {
    RenderQuality::Normal
}
```

But the CPU renderer never reads `request.quality`. `RenderQuality::Draft` is documented, but it is not implemented by `pdf-render-cpu`.

Therefore, at the same zoom bucket, Draft and Final currently perform effectively the same lowering and raster work and produce the same pixels.

Any visible “low-resolution then high-resolution” effect is coming from a stale bucket or thumbnail fallback, not the Draft flag itself.

Until Draft becomes genuinely cheaper, scheduling both is wasted work.

## 6. A deliberate jump is treated as high-velocity scrolling

`navigate_to()` calls:

```rust
self.scroll.apply(ScrollCommand::SetAbsolute(...));
self.bump_generation();
```

`ScrollModel::apply()` derives velocity from the distance moved. An outline or search jump from page 10 to page 500 therefore produces an enormous velocity.

Final tiles are only requested when:

```rust
intent.speed_pages_per_second_hint() < 3.0
```

So a deliberate navigation target can be classified as “rapidly skimming” and initially receive Draft-only scheduling. There is no mouse-release event associated with an outline/search/history jump to settle that velocity.

This should be fixed immediately.

## 7. Every tile repeats request-specific lowering

Each 256×256 tile calls `CpuBackend::render_with()`. That function performs:

```rust
prepared::lower_with_font_cache(...)
```

for every request. Its own comment describes the operation as:

```text
transform, flatten, cull, classify, once
```

It is once per tile, rather than once per visible page region.

A page needing 12 visible tiles can therefore repeat page traversal, transformation, culling, image preparation, and classification 12 times.

## 8. The tile cache will not scale to universal previews

`TileFrameSnapshot::best_covering_into()` scans the complete tile map for every demand. `TileCache::frame_snapshot()` clones the complete tile map whenever tiles change.

The app then responds to any `TileReady` with:

```rust
self.tile_snapshot = self.tiles.frame_snapshot();
self.damage.mark_full();
```

A background sweep producing thousands of page thumbnails would therefore:

* grow the global scan cost;
* clone a larger map repeatedly;
* wake the UI continually;
* cause full-window redraws for offscreen work.

Universal previews need their own direct page-indexed cache.

---

# The core architecture

## L0: one canonical whole-page preview for every page

Render every page once at approximately 128–192 pixels wide, with a pixel-count cap for unusually long pages.

This representation should be:

* independent of the current zoom;
* always available as an underlay;
* generated during the existing text-index sweep;
* stored in a dedicated page preview cache;
* persisted in a file-backed cache where useful;
* silently inserted when offscreen.

At 160 pixels wide, a portrait A-series page is roughly 160×226:

| Storage format     | Per page | 1,000 pages |
| ------------------ | -------: | ----------: |
| XRGB/RGBA, 4 bytes |  141 KiB |     138 MiB |
| RGB565, 2 bytes    |   71 KiB |      69 MiB |
| Gray8, 1 byte      |   35 KiB |    34.5 MiB |

For ordinary documents, 160-pixel previews are practical. For ten-thousand-page documents, they should be file-backed rather than all decoded in heap memory.

The best first implementation is:

* store all preview pixels in a raw or lightly compressed page pack on disk;
* keep a 32–128 MiB decoded hot preview cache;
* load one preview into the hot cache on a seek;
* use an `mmap`-backed pack so the operating system handles cold-page eviction.

A 100–200 KiB page copy is trivial compared with recompiling and rasterizing a PDF page.

## L1: readable proxy for pages with meaningful probability

L1 should be approximately 512–1024 pixels wide, or a half-resolution band at the current zoom. It is not universal.

Candidates include:

* the next several pages in reading direction;
* previous pages likely to be revisited;
* visible outline destinations;
* the active and adjacent search results;
* history back/forward targets;
* link-hover destinations;
* the current scrollbar prediction;
* page numbers being typed before Enter is pressed.

For text pages, L1 may be a filtered text/basic-vector pass. For scanned pages, it should be an image proxy. A universal “text first” policy is wrong for scans.

## L2: exact current-bucket pixels

These remain tile- or band-based and belong to:

* currently visible regions;
* a directional hot ring;
* selected high-confidence navigation targets;
* every page only when the document-specific cost model says that is cheap.

---

# First implementation change: unify compile, index, and preview work

There should be one compile state per page, not separate `Compile` and `Index` jobs.

A page compilation can satisfy several consumers:

```rust
#[derive(Debug, Default, Clone, Copy)]
struct CompileNeeds {
    interactive_generation: Option<u64>,
    text_index: bool,
    preview: bool,
}

impl CompileNeeds {
    fn merge(&mut self, other: Self) {
        if let Some(generation) = other.interactive_generation {
            // The conductor is serial, so this is the newest request observed.
            self.interactive_generation = Some(generation);
        }
        self.text_index |= other.text_index;
        self.preview |= other.preview;
    }

    fn interactive(generation: u64) -> Self {
        Self {
            interactive_generation: Some(generation),
            text_index: true,
            preview: true,
        }
    }

    fn background() -> Self {
        Self {
            interactive_generation: None,
            text_index: true,
            preview: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompilePhase {
    Pending,
    InFlight,
}

#[derive(Debug)]
struct CompileState {
    needs: CompileNeeds,
    priority: i32,
    revision: u64,
    phase: CompilePhase,
    cancellation: CancellationFlag,
}

#[derive(Debug)]
struct CompileTicket {
    page: PageIndex,
    revision: u64,
}
```

Use lazy heap invalidation to support promotion:

```rust
fn request_compile(
    &mut self,
    page: PageIndex,
    needs: CompileNeeds,
    priority: i32,
) {
    if self.quarantined_pages.contains(&page) {
        return;
    }

    if let Some(resident) = self.compiled.get(&page) {
        self.satisfy_compile_needs_from_resident(page, resident, needs);
        return;
    }

    let mut push_ticket = None;

    match self.compile_states.entry(page) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            let revision = 1;
            entry.insert(CompileState {
                needs,
                priority,
                revision,
                phase: CompilePhase::Pending,
                cancellation: CancellationFlag::default(),
            });
            push_ticket = Some(revision);
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            let state = entry.get_mut();
            state.needs.merge(needs);

            // If already running as a background index job, do not compile it
            // again. Its completion will satisfy the new interactive request.
            if state.phase == CompilePhase::Pending && priority > state.priority {
                state.priority = priority;
                state.revision = state.revision.wrapping_add(1);
                push_ticket = Some(state.revision);
            }
        }
    }

    if let Some(revision) = push_ticket {
        self.compile_pending.push(Prioritized {
            priority,
            sequence: self.next_sequence(),
            value: CompileTicket { page, revision },
        });
    }
}
```

When popping:

```rust
fn pop_next_compile(&mut self) -> Option<CompileJob> {
    while let Some(ticket) = self.compile_pending.pop() {
        let page = ticket.value.page;
        let Some(state) = self.compile_states.get_mut(&page) else {
            continue;
        };

        // A newer promoted ticket superseded this heap entry.
        if state.revision != ticket.value.revision
            || state.phase != CompilePhase::Pending
        {
            continue;
        }

        let placement = self.layout.placement(page)?;
        state.phase = CompilePhase::InFlight;

        return Some(CompileJob::Page {
            page,
            page_to_doc: placement.page_to_doc,
            cancellation: state.cancellation.clone(),
        });
    }

    None
}
```

At completion, consult the **latest** merged needs:

```rust
fn handle_compile_success(
    &mut self,
    page: PageIndex,
    artifacts: Arc<CompiledArtifacts>,
) {
    let needs = self
        .compile_states
        .remove(&page)
        .map_or_else(CompileNeeds::default, |state| state.needs);

    if needs.text_index && self.indexed_pages.insert(page) {
        self.updates.push(SessionUpdate::PageIndexed {
            page,
            text: Arc::clone(artifacts.text.substrate()),
        });
    }

    if needs.preview && !self.previews.contains(page) {
        // The raster job keeps the Arc alive. Do not discard the compile
        // result until the preview has consumed it.
        self.schedule_page_preview(Arc::clone(&artifacts), page);
    }

    let currently_relevant = self.intent.load().page_is_relevant(page);
    let interactive = needs.interactive_generation.is_some();

    if currently_relevant || interactive || self.cold_ir.should_admit(page, &artifacts) {
        self.retain_raster_artifact(page, artifacts);
    }
}
```

This does four useful things:

1. The same page is never compiled simultaneously for indexing and display.
2. A random seek can attach to an already-running background compile.
3. A queued background compile can be promoted.
4. Every whole-document indexing compile can generate a preview before its IR disappears.

---

# Separate raster IR from semantic and text artifacts

`CompiledArtifacts` currently bundles:

* semantic page;
* native text page;
* viewer text substrate;
* structure;
* compiled raster IR.

The raster worker does not need all of that.

Introduce a stripped artifact:

```rust
#[derive(Debug, Clone)]
pub struct RasterArtifacts {
    pub page: PageIndex,
    pub geometry: PageGeometry,
    pub compiled: CompiledArtifact,
    pub lowering_degraded: bool,
}

impl From<&CompiledArtifacts> for RasterArtifacts {
    fn from(value: &CompiledArtifacts) -> Self {
        Self {
            page: value.page,
            geometry: value.geometry,
            compiled: value.compiled.clone(),
            lowering_degraded: value.lowering_degraded,
        }
    }
}
```

Then change raster workers to accept `RasterArtifacts`.

After indexing:

* compact text remains in the search store;
* visible-page geometry remains in the UI artifact cache;
* stripped raster IR may remain in a separate cold IR cache;
* heavy semantic structures can be dropped.

This should substantially increase how many pages can remain raster-ready.

There is also a memory-accounting mistake to correct. `CompiledArtifact::estimated_peak_bytes()` returns `PageComplexity::estimated_peak_bytes`, which is documented as estimated **render-time intermediate memory at 1×**. It is not the retained byte size of `CompiledPage`.

Add a separate method:

```rust
impl CompiledPage {
    pub fn retained_bytes(&self) -> u64 {
        use std::mem::size_of_val;

        let mut bytes = size_of_val(self) as u64;
        bytes += size_of_val(self.operations.as_ref()) as u64;
        bytes += size_of_val(self.paths.as_ref()) as u64;
        bytes += size_of_val(self.paints.as_ref()) as u64;
        bytes += size_of_val(self.stroke_styles.as_ref()) as u64;
        bytes += size_of_val(self.glyph_runs.as_ref()) as u64;
        bytes += size_of_val(self.fonts.as_ref()) as u64;
        bytes += size_of_val(self.images.as_ref()) as u64;
        bytes += size_of_val(self.masks.as_ref()) as u64;
        bytes += size_of_val(self.groups.as_ref()) as u64;
        bytes += size_of_val(self.shadings.as_ref()) as u64;
        bytes += size_of_val(self.tilings.as_ref()) as u64;

        // Each resource type should include owned payloads here:
        // encoded image bytes, path coordinates, glyph arrays, font data, etc.
        bytes
    }
}
```

Keep `estimated_peak_bytes` for scheduling and transient-memory admission. Use `retained_bytes` for cache residency.

---

# Fix foreground priority and worker queue inversion

## Replan visible pages first

The first stage of `replan()` should derive distinct visible pages directly from visible tile demands and request those before the compile ring:

```rust
fn replan(&mut self) {
    let intent = self.intent.load_full();
    self.cancel_irrelevant(&intent);

    let mut visible_pages = Vec::new();
    let mut seen = HashSet::new();

    for demand in intent.visible_tiles.iter() {
        if seen.insert(demand.page) {
            visible_pages.push(demand.page);
        }
    }

    // Current visible pages always receive the highest compile priority.
    for (order, page) in visible_pages.iter().copied().enumerate() {
        self.request_compile(
            page,
            CompileNeeds::interactive(intent.generation),
            1_400 - order as i32,
        );
    }

    for demand in intent.visible_tiles.iter().copied() {
        self.schedule_visible_tile(&intent, demand);
    }

    // Compile-ahead candidates come only after visible requests.
    for (order, page) in self
        .ordered_compile_ahead_pages(&intent)
        .into_iter()
        .enumerate()
    {
        if !seen.contains(&page) {
            self.request_compile(
                page,
                CompileNeeds {
                    interactive_generation: None,
                    text_index: true,
                    preview: true,
                },
                850 - order.min(500) as i32,
            );
        }
    }

    // Overscan, hints, sweep...
    self.schedule_background_work(&intent);
    self.dispatch();
}
```

The planner should ideally stop returning one numerically sorted `compile_pages` set. Return separate collections:

```rust
pub struct ViewportIntent {
    pub visible_pages: Arc<[PageIndex]>,
    pub ahead_pages: Arc<[PageIndex]>,
    pub behind_pages: Arc<[PageIndex]>,
    // ...
}
```

This preserves semantic order.

## Do not buffer several jobs per worker

As an immediate improvement, change:

```rust
let (compile_tx, compile_rx) = bounded(compile_workers * 2);
let (raster_tx, raster_rx) = bounded(raster_workers * 3);
```

to very shallow queues:

```rust
let (compile_tx, compile_rx) = bounded(1);
let (raster_tx, raster_rx) = bounded(1);
```

Most queued work then remains in the priority heaps where it can be overtaken or cancelled.

The stronger design is a pull-based priority broker: workers take the current highest-priority job directly from a shared heap. That removes the second FIFO queue entirely.

## Reserve foreground capacity

Background indexing should never be allowed to consume all compile workers:

```rust
fn background_compile_limit(compile_workers: usize) -> usize {
    compile_workers.saturating_sub(1)
}
```

On machines with at least two compile workers, always leave one for interactive work.

On a one-compile-worker machine:

* start background compilation only after an idle interval;
* cancel it on interaction;
* add cancellation checkpoints inside `PageCompiler`, because current cancellation only runs before and after `compile_artifacts()`.

Useful compiler checkpoints include:

* every N content-stream operators;
* after resolving each Form XObject;
* after each major resource set;
* between annotation appearance streams;
* during long text and path construction loops.

The renderer already checks cancellation between raster operations. Compilation needs equivalent cooperation.

SumatraPDF contains two relevant policies: an identical request already in its queue is moved to the top, and predictive pages are chained one at a time rather than flooding the queue. It also re-requests visible pages so they overtake predictions.  PDF.js follows a similarly strict ordering: visible pages, detail regions of visible pages, then the directionally adjacent page, with thumbnails below page rendering. 

Lege can speculate much more aggressively than those viewers, but it should preserve their central rule: **prediction may consume spare capacity; it may never delay a confirmed visible target.**

---

# Remove the fake Draft pass

Until `pdf-render-cpu` actually implements `RenderQuality::Draft`, schedule only one current-bucket exact render.

```rust
fn schedule_visible_tile(
    &mut self,
    intent: &ViewportIntent,
    demand: TileDemand,
) {
    let Some(artifacts) = self.compiled.get(&demand.page) else {
        self.request_compile(
            demand.page,
            CompileNeeds::interactive(intent.generation),
            1_400,
        );
        return;
    };

    if intent.is_fast_skimming() {
        // The canonical preview remains underneath. Do not spend exact work
        // on every page crossed during a scrollbar or fast wheel gesture.
        return;
    }

    self.schedule_raster(
        intent,
        demand,
        RasterPass::Final,
        1_250,
    );
}
```

The low-to-high progression should come from genuinely different representations:

```text
L0 160 px page preview
→ L1 half-scale page or readable pass
→ L2 exact current-bucket raster
```

That progression saves work. Draft and Final at the same bucket currently do not.

Later, a real Draft mode could implement:

* reduced edge antialiasing;
* simpler image filtering;
* lower-resolution image decode;
* omission of expensive shadings or transparency;
* lower-scale raster rather than merely lower-quality sampling.

But given the renderer’s speed, I would first implement resolution-tiered previews rather than invest heavily in reduced-AA exact-size Draft rendering.

---

# Fix deliberate navigation and scroll settling

Programmatic jumps should be immediately stationary:

```rust
fn navigate_to(&mut self, location: DocumentLocation, record_jump: bool) {
    if record_jump {
        if let Some(current) = self.current_location() {
            self.history.push_jump(current);
        }
        self.history.push_jump(location);
    }

    let Some(placement) = self.layout.placement(location.page) else {
        return;
    };

    let target_y = location
        .target_region
        .map_or(placement.bounds.y, |region| placement.bounds.y + region.y);

    self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
        x: self.scroll.position.x,
        y: target_y * self.zoom,
    }));

    // This was an intentional jump, not physical scrolling.
    self.scroll.settle();
    self.bump_generation();
}
```

Wheel input needs a short idle-settle timer so a final stationary intent is always published:

```rust
const SCROLL_SETTLE_DELAY: Duration = Duration::from_millis(90);

// ViewerApp field:
scroll_settle_deadline: Option<Instant>,
```

In `finish_direct_scroll()`:

```rust
self.scroll_settle_deadline =
    Some(Instant::now() + SCROLL_SETTLE_DELAY);
```

In `about_to_wait()`:

```rust
fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    let now = Instant::now();

    if self
        .scroll_settle_deadline
        .is_some_and(|deadline| deadline <= now)
    {
        self.scroll_settle_deadline = None;
        self.scroll.settle();
        self.intent_dirty = true;
        self.request_redraw();
    }

    if self.scrollbar.reveal_preview_if_due(now) {
        self.damage.mark_full();
        self.request_redraw();
    }

    let next_deadline = [
        self.scroll_settle_deadline,
        self.scrollbar.preview_deadline(),
    ]
    .into_iter()
    .flatten()
    .min();

    event_loop.set_control_flow(match next_deadline {
        Some(deadline) => ControlFlow::WaitUntil(deadline),
        None => ControlFlow::Wait,
    });
}
```

This is a small change with a substantial effect on final-quality convergence.

---

# Implement the universal preview as a separate cache

Do not insert every page preview into the current global tile `HashMap`.

Use direct page slots:

```rust
use arc_swap::ArcSwapOption;

#[derive(Debug)]
pub struct PreviewEntry {
    pub surface: Arc<TileSurface>,
    _lease: MemoryLease,
}

#[derive(Debug)]
pub struct PagePreviewCache {
    slots: Box<[ArcSwapOption<PreviewEntry>]>,
    memory: MemoryArbiter,
}

impl PagePreviewCache {
    pub fn new(page_count: u32, memory: MemoryArbiter) -> Self {
        let slots = (0..page_count)
            .map(|_| ArcSwapOption::empty())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self { slots, memory }
    }

    pub fn get(&self, page: PageIndex) -> Option<Arc<PreviewEntry>> {
        self.slots.get(page.0 as usize)?.load_full()
    }

    pub fn contains(&self, page: PageIndex) -> bool {
        self.get(page).is_some()
    }

    pub fn insert(&self, page: PageIndex, surface: Arc<TileSurface>) {
        let Some(slot) = self.slots.get(page.0 as usize) else {
            return;
        };

        let lease = self
            .memory
            .reserve(CacheCategory::Thumbnails, surface.byte_len());

        slot.store(Some(Arc::new(PreviewEntry {
            surface,
            _lease: lease,
        })));
    }
}
```

Render it exactly once underneath exact tiles:

```rust
painter.fill_rect(page_rect, theme.colors.paper);
painter.stroke_rect(page_rect, 1, theme.colors.page_border);

painter.push_clip(page_rect);

if let Some(preview) = previews.get(placement.page) {
    painter.draw_tile(
        Arc::clone(&preview.surface),
        page_screen,
        ImageSampling::Linear,
    );
}

// Exact or stale exact tiles overlay the preview.
for demand in intent
    .visible_tiles
    .iter()
    .filter(|demand| demand.page == placement.page)
{
    // Existing tile lookup and paint.
}
```

This is important: the preview is a **page underlay**, not one more fallback candidate searched separately for every tile.

## A full-page preview demand

The existing raster method accepts arbitrary demand dimensions; it is not technically restricted to 256×256. Create one synthetic full-page demand:

```rust
fn page_preview_demand(
    layout: &PageLayoutIndex,
    page: PageIndex,
    preferred_width: u32,
    max_pixels: u64,
) -> Option<(ZoomBucket, TileDemand)> {
    let placement = layout.placement(page)?;
    let page_width = placement.bounds.width.max(1.0);
    let page_height = placement.bounds.height.max(1.0);

    let mut scale = f64::from(preferred_width.max(64)) / page_width;
    let predicted_pixels = page_width * page_height * scale * scale;

    if predicted_pixels > max_pixels as f64 {
        scale *= (max_pixels as f64 / predicted_pixels).sqrt();
    }

    let bucket = ZoomBucket::from_zoom(scale);
    let scale = bucket.scale();

    let width = (page_width * scale).ceil().max(1.0) as u32;
    let height = (page_height * scale).ceil().max(1.0) as u32;

    Some((
        bucket,
        TileDemand {
            page,
            coord: TileCoord { x: 0, y: 0 },
            page_device_rect: RectI {
                x: 0,
                y: 0,
                width,
                height,
            },
            page_document_rect: placement.bounds,
            distance_from_viewport: f64::INFINITY,
            visible: false,
        },
    ))
}
```

A reasonable initial call would be approximately:

```rust
page_preview_demand(layout, page, 160, 256 * 384)
```

The pixel cap handles posters, receipts, and extremely long pages.

## Fuse preview generation into the index pipeline

The background pipeline should become:

```text
compile page once
    ├── publish compact text index
    ├── schedule canonical preview using the same Arc<CompiledPage>
    ├── optionally admit stripped IR into cold IR cache
    └── release semantic/compiler artifacts
```

Cap the number of compiled-but-not-yet-previewed pages. Otherwise a fast compiler could outrun raster workers and hold many large IRs simultaneously.

A memory or count semaphore of two to four page artifacts is sufficient.

---

# Persist previews as a page pack

A raw file-backed pack is preferable to retaining every decoded preview in the heap.

A simple index format is enough:

```rust
#[repr(C)]
#[derive(Clone, Copy)]
struct PreviewRecord {
    offset: u64,
    byte_len: u32,
    width: u16,
    height: u16,
    stride: u16,
    format: u8,
    complete: u8,
}
```

The cache key should include:

```rust
struct PreviewCacheKey {
    document_fingerprint: [u8; 32],
    renderer_schema: u32,
    preview_width: u16,
    annotation_mode: u8,
}
```

The fingerprint can include:

* file length;
* modification time;
* hash of the first and last portions;
* PDF trailer ID where available.

For the first version:

1. Append raw XRGB previews to a temporary pack.
2. Maintain a page-to-offset index.
3. Memory-map completed segments.
4. Copy the requested page into a hot `PixelSurface`.
5. Persist the pack between sessions once stable.

Raw storage is acceptable initially. Compression can be added after measuring I/O and cache sizes. For ordinary text documents, Gray8 previews are also attractive, but the current presenter assumes XRGB, so format-aware preview storage should be a later optimization.

Persistent previews change reopening behavior dramatically: after the first open, every seek can have page-specific pixels immediately.

---

# Replace per-tile raster with tile-run or band rendering

This is probably the largest renderer/viewer integration win after universal previews.

The current renderer can already render a translated rectangular region. You do not initially need a new renderer API. Group adjacent demands and render their union once.

## Add shared pixel views

```rust
#[derive(Debug)]
pub struct PixelBacking {
    pub pixels: Arc<[u32]>,
    _lease: MemoryLease,
}

#[derive(Debug, Clone)]
pub struct PixelSurface {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub offset: usize,
    pub backing: Arc<PixelBacking>,
}

impl PixelSurface {
    pub fn row(&self, y: usize) -> &[u32] {
        let start = self.offset + y * self.stride;
        &self.backing.pixels[start..start + self.width as usize]
    }
}
```

The blitter then indexes through `offset`:

```rust
let src_start =
    source.offset + src_y * source.stride + src_x;
```

The backing owns one memory lease. Tile views do not each reserve the same band memory.

## Render a run once

```rust
fn raster_pdf_run(
    backend: &CpuBackend,
    context: &mut CpuWorkerContext,
    artifacts: &RasterArtifacts,
    bucket: ZoomBucket,
    demands: &[TileDemand],
    generation: u64,
    cancellation: &CancellationFlag,
) -> Result<Vec<TileSurface>, DocumentEngineError> {
    let CompiledArtifact::Pdf(compiled) = &artifacts.compiled else {
        return Err(DocumentEngineError::Engine(
            "non-PDF artifact in PDF raster worker".to_owned(),
        ));
    };

    let run_rect = union_device_rects(
        demands.iter().map(|demand| demand.page_device_rect),
    )
    .ok_or_else(|| DocumentEngineError::Engine("empty tile run".to_owned()))?;

    let scale = bucket.scale();
    let full_matrix =
        device_matrix(compiled.bounds.crop, compiled.bounds.rotate, scale);

    let run_matrix = full_matrix.then(Matrix::translate(
        -f64::from(run_rect.x),
        -f64::from(run_rect.y),
    ));

    let request = RenderRequest {
        page: compiled.clone(),
        transform: PageTransform { matrix: run_matrix },
        crop: None,
        output_size: DeviceSize {
            width: run_rect.width,
            height: run_rect.height,
        },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations: AnnotationMode::StaticAppearances,
        quality: RenderQuality::Normal,
        limits: RenderLimits {
            cancellation: Some(CancellationToken::from_shared(
                cancellation.shared_flag(),
            )),
            ..RenderLimits::default()
        },
        residency: OutputResidency::HostRequired,
    };

    // One lower/flatten/cull/classify and one raster execution for the run.
    let (host, stats) = backend
        .render_with(&request, context)
        .map_err(map_render_error)?;

    let backing = ingest_rgba_to_shared_xrgb(&host)?;

    let mut output = Vec::with_capacity(demands.len());

    for demand in demands {
        let local_x =
            (demand.page_device_rect.x - run_rect.x) as usize;
        let local_y =
            (demand.page_device_rect.y - run_rect.y) as usize;

        output.push(TileSurface {
            key: demand.key(
                document_id,
                bucket,
                TileTier::Final,
            ),
            generation,
            page_device_rect: demand.page_device_rect,
            page_document_rect: demand.page_document_rect,
            pixels: PixelSurface {
                width: demand.page_device_rect.width,
                height: demand.page_device_rect.height,
                stride: host.width as usize,
                offset: local_y * host.width as usize + local_x,
                backing: Arc::clone(&backing),
            },
            degraded: stats.degraded_draws > 0
                || artifacts.lowering_degraded,
        });
    }

    Ok(output)
}
```

Group demands when:

```text
union area <= about 1.25–1.5 × total requested tile area
and
union surface <= approximately 8–16 MiB
```

Useful grouping policy:

* fit-width page smaller than the cap: render the complete page;
* ordinary zoom: render one or two horizontal bands;
* high zoom: render connected visible tile runs;
* target-region jump: render the band containing the destination first.

This avoids repeating `lower_with_font_cache()` for every tile.

MuPDF’s display-list API explicitly supports reusing compiled drawing commands for repeated renders and replaying only a scissored region, with cancellation/progress control.  Lege already has the equivalent immutable `CompiledPage`; tile-run rendering is how the viewer should fully exploit it.

## Remove the RGBA-to-XRGB copy afterward

`ingest_rgba_to_xrgb()` currently allocates another complete pixel vector and loops over every pixel.

Add an output format that matches the viewer’s native surface, for example:

```rust
pub enum OutputFormat {
    Rgba8PremultipliedSrgb,
    Gray8,
    Xrgb8888Native,
}
```

Because the viewer renders over opaque white, XRGB is a natural final host format.

Do this after tile-run rendering. The repeated lowering is likely a much larger cost than the pixel swizzle, but eliminating both is appropriate.

---

# Use page-specific first-paint strategies, not only text-first

The plan’s text-first concept is valuable for digitally generated books, but not as a universal policy.

You already have:

```rust
PageFeatures
PageComplexity {
    operation_count,
    path_segment_count,
    glyph_count,
    image_pixels,
    transparency_group_count,
    estimated_peak_bytes,
}
```

You can classify pages as scheduling hints:

```rust
#[derive(Debug, Clone, Copy)]
enum PagePhenotype {
    TextVector,
    ScanLike,
    SlideLike,
    Mixed,
    ComplexTransparency,
}

fn largest_image_coverage(page: &pdf_page_ir::CompiledPage) -> f64 {
    let crop = page.bounds.crop;
    let page_area =
        ((crop.x1 - crop.x0) * (crop.y1 - crop.y0)).abs().max(1.0);

    page.operations
        .iter()
        .filter_map(|op| match op {
            pdf_page_ir::DisplayOp::DrawImage { transform, .. } => {
                // Image XObjects are normally mapped from a unit square.
                let area =
                    (transform.a * transform.d - transform.b * transform.c)
                        .abs();
                Some((area / page_area).clamp(0.0, 1.0))
            }
            _ => None,
        })
        .fold(0.0, f64::max)
}

fn classify_page(page: &pdf_page_ir::CompiledPage) -> PagePhenotype {
    let coverage = largest_image_coverage(page);
    let complexity = page.complexity;
    let features = page.features;

    if features.intersects(
        pdf_page_ir::PageFeatures::TRANSPARENCY
            | pdf_page_ir::PageFeatures::SOFT_MASKS,
    ) && complexity.transparency_group_count > 4
    {
        PagePhenotype::ComplexTransparency
    } else if coverage >= 0.70 && complexity.glyph_count < 200 {
        PagePhenotype::ScanLike
    } else if complexity.glyph_count >= 150
        && coverage < 0.35
    {
        PagePhenotype::TextVector
    } else if coverage >= 0.45
        && complexity.glyph_count >= 20
        && complexity.glyph_count < 500
    {
        PagePhenotype::SlideLike
    } else {
        PagePhenotype::Mixed
    }
}
```

The thresholds should be tuned from traces rather than treated as PDF truths.

Then choose:

| Page type            | Immediate representation            |
| -------------------- | ----------------------------------- |
| Text/vector          | Text + simple fills/strokes         |
| Scan-like            | Dominant image at reduced decode    |
| Slide-like           | Background/image + text             |
| Complex transparency | Canonical preview, then full raster |
| Mixed                | L1 coarse whole-page raster         |

Your renderer already passes device-footprint target sizes into JPEG and JPX decoding, so a small whole-page preview can use reduced-resolution image decoding instead of decoding a camera-sized scan at full resolution. That makes universal previews especially attractive for scanned PDFs.

---

# Navigation prediction: use explicit intent before heuristics

A viewer has more information than “current page and velocity.” Add a general warm-hint API:

```rust
#[derive(Debug, Clone, Copy)]
pub enum WarmReason {
    Sequential,
    OutlineHover,
    OutlineTarget,
    SearchActive,
    SearchAdjacent,
    LinkHover,
    History,
    ScrollbarPrediction,
    PageNumberInput,
    BackgroundSweep,
}

#[derive(Debug, Clone)]
pub struct WarmHint {
    pub page: PageIndex,
    pub region: Option<RectF>,
    pub reason: WarmReason,
    pub probability: f32,
    pub expires_at: Instant,
}

pub enum ConductorCommand {
    IntentChanged,
    LayoutChanged(Arc<PageLayoutIndex>),
    Warm(WarmHint),
    Shutdown,
}
```

The representation requested should depend on confidence:

```text
Confirmed navigation target:
    compile/retain IR + L1 + exact focal region

Outline/link hover:
    compile/retain IR + L1 target region

Search next/previous:
    L1 and retain IR

Sequential prediction:
    exact current bucket within work/memory horizon

Weak outline or inferred TOC target:
    L0 or L1 only

Background sweep:
    L0 only
```

## Signals worth exploiting

### Outline pointer movement

The viewer already extracts embedded outline destinations and preserves `target_region` in `pdf_engine.rs:78–116`.

When the pointer enters an outline row, immediately warm that destination. Even a short hover provides useful lead time.

On mouse-down, promote it again. Navigation can still occur on the normal click action.

### Page-number typeahead

When the user starts typing a page number, every additional digit narrows the target before Enter is pressed.

For example:

```text
typed "4"   → possible pages 4, 40–49, 400–499...
typed "42"  → page 42 or 420–429...
typed "427" → page 427
```

Once the candidate set is small, warm it. This is nearly free prediction because the user explicitly supplies the future target.

### Search

When search results arrive, warm:

* active hit;
* next hit;
* previous hit.

Do not warm all 5,000 hits.

### History

Back and forward targets should normally retain L1 and stripped IR because the probability of reuse is high.

### Internal links

When link annotations are exposed by the renderer, link hover should request the destination region. The existing `LinkPeek` command is already the right architectural seam, but it currently only requests compilation.

### Scrollbar scrubbing

During active thumb dragging:

* show L0 previews only;
* suppress exact work for every transient page;
* after the pointer remains near one page for roughly 25–50 ms, begin L1;
* on release, settle and request exact focal tiles.

This avoids turning scrollbar scrubbing into a random-page exact-render benchmark.

---

# Is OCR of the table of contents a good idea?

**Rendering navigation destinations early is a good idea. OCR should be the last source used to discover those destinations.**

The order should be:

1. Embedded outline destinations.
2. Internal link annotations from a clickable contents page.
3. Native text extraction from a printed table of contents.
4. PDF page-label mapping or inferred front-matter offset.
5. OCR only for image-only contents pages.

Your code already implements the first and strongest source.

For a native-text contents page, recognize patterns such as:

```text
Chapter title ........ 127
Chapter title          127
```

But printed page 127 may not be PDF page index 126 because of covers, Roman-numeral front matter, inserted pages, or custom page labels. The mapping must be verified through:

* `/PageLabels` when implemented;
* internal links;
* matching chapter headings on candidate pages;
* a consistently inferred offset.

For prefetching, uncertainty is less dangerous than for navigation. A false-positive hint merely spends some background work. Therefore, an inferred TOC destination can be admitted as a low-confidence `WarmHint` without being exposed as an actual clickable destination.

I would not implement OCR-based TOC prediction before:

* the universal preview cache;
* compile-job unification;
* foreground worker reservation;
* outline-hover warming;
* internal-link parsing.

Those deliver more benefit with less complexity.

---

# Use a navigation-mode state machine

Velocity alone is not enough. Add a small behavioral classifier:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavigationMode {
    Reading,
    Skimming,
    Jumping,
    Idle,
}
```

Suggested interpretation:

| Mode     | Evidence                                                   | Scheduling                                      |
| -------- | ---------------------------------------------------------- | ----------------------------------------------- |
| Reading  | Small repeated scrolls/page-downs, dwell, stable direction | Exact-render several pages ahead                |
| Skimming | Rapid wheel or thumb drag, many pages crossed              | L0 only, L1 for stable predicted target         |
| Jumping  | Outline, search, history, link, typed page                 | Target preview and focal exact work first       |
| Idle     | No recent interaction                                      | Universal sweep, cold IR, persistent-cache work |

Sequential reading should use a **work horizon**, not a fixed page count.

For example, keep enough exact work ahead to represent roughly 200–500 ms of measured raster CPU, subject to the tile budget. With a very fast renderer, that may mean dozens of pages. With image-heavy pages, it may mean only a few.

After a reader dwells for several seconds, there is little reason not to have the next chapter of pages prepared.

---

# Sweep order should reflect the real usage model

A strict page-0-to-page-N sweep is ideal for beginning-to-end reading but poor for early random skimming.

Use interleaved lanes:

```text
Sequential lane:
    pages from current position in reading direction

Outline lane:
    top-level outline targets, then visible subtree targets

Coverage lane:
    midpoint, quarter points, eighth points, etc.

Fill lane:
    remaining unrendered pages
```

A simple cadence could be:

```text
Sequential
Sequential
Sequential
Outline
Sequential
Coverage
repeat
```

This means:

* beginning readers get a long warm runway;
* chapter jumps are likely ready;
* random skimming quickly gains document-wide visual coverage;
* every page is still eventually rendered.

For enormous documents, use a two-stage sweep:

```text
Stage A: 96–128 px preview for broad coverage
Stage B: upgrade all pages to 160–192 px
```

For ordinary documents, go directly to 160–192 px.

---

# Decide when to render the entire document exactly

Do not use a fixed “fewer than 100 pages” rule. Use measured cost.

```rust
#[derive(Debug, Clone, Copy)]
enum DocumentWarmPolicy {
    ExactAllAtCurrentBucket,
    PreviewAllAndExactAnchors,
    CoveragePreviewThenFullPreview,
}

fn choose_warm_policy(
    predicted_exact_ms: f64,
    predicted_exact_bytes: u64,
    predicted_preview_ms: f64,
    memory_budget: u64,
    config: &WarmConfig,
) -> DocumentWarmPolicy {
    if predicted_exact_ms <= config.exact_all_cpu_budget_ms
        && predicted_exact_bytes <= memory_budget / 4
    {
        DocumentWarmPolicy::ExactAllAtCurrentBucket
    } else if predicted_preview_ms <= config.direct_full_preview_budget_ms {
        DocumentWarmPolicy::PreviewAllAndExactAnchors
    } else {
        DocumentWarmPolicy::CoveragePreviewThenFullPreview
    }
}
```

A reasonable initial policy is:

```text
Render all exact at the current fit-width bucket when:
    predicted total CPU work is below roughly 500–750 ms
    and
    decoded surfaces fit within 20–25% of the viewer memory budget

Otherwise:
    render every page at L0
    render L1 for anchors and predictions
    keep exact L2 demand-driven
```

The exact threshold should come from real traces.

Also consider file-backed exact pages for medium documents. A full fit-width page pack can be used as:

* exact display at the original bucket;
* high-quality L1 fallback after zoom changes;
* persistent warm state on reopen.

The faster renderer shifts the boundary toward more eager rendering, but it does not remove the need for representation tiers.

---

# Add a feedback controller for background concurrency

Do not permanently choose “two background workers.”

Use foreground latency to regulate speculation:

```rust
fn adjust_background_limit(
    current: usize,
    maximum: usize,
    foreground_p95_ms: f64,
    idle_for: Duration,
) -> usize {
    if foreground_p95_ms > 35.0 {
        // Multiplicative decrease under user-visible pressure.
        current / 2
    } else if idle_for >= Duration::from_millis(500) {
        // Additive increase while the UI is healthy.
        (current + 1).min(maximum)
    } else {
        current
    }
}
```

This is an AIMD-style policy:

* interaction latency rises: sharply reduce background concurrency;
* viewer stays idle and responsive: gradually use more cores;
* a confirmed seek always retains a reserved foreground lane.

That lets Lege use its parallel renderer fully during idle periods without turning eager rendering into input latency.

Background preview renders should also have a low cache-admission class so a whole-document sweep does not evict hot fonts, glyphs, decoded images, or exact tiles:

```rust
pub enum CacheAdmission {
    Foreground,
    Warm,
    ReadOnly,
    Bypass,
}
```

L0 background sweep jobs should normally be `ReadOnly` or use a small separate decode cache. Visible and sequential warm jobs may use normal foreground admission.

---

# Keep useful completions even after the viewport changes

`handle_result()` currently discards a raster completion unless it remains relevant to the exact current intent.

That is too aggressive for inexpensive, reusable output.

Use two decisions:

```text
Should it enter the cache?
Should it wake/redraw the UI?
```

These are not the same.

A completed tile that:

* matches a common zoom bucket;
* belongs to recent history;
* is one page away;
* was nearly finished when the user moved;

may still be worth caching.

But it should not wake the UI when offscreen.

```rust
let cache_worthy = self.cache_admission.should_keep(&tile, &current);
let visible_now = current.tile_is_relevant(tile.key.page, tile.key.coord);

if cache_worthy {
    self.tiles.insert(tile.clone(), demand.distance_from_viewport);
}

if visible_now {
    self.updates.push(SessionUpdate::TileReady {
        key: tile.key,
        generation: tile.generation,
        document_rect: tile.page_document_rect,
    });
}
```

Cancellation should also consider work stage:

* queued: cancel freely;
* just started: cancel;
* expensive decode almost complete: finish if result/cache is reusable;
* final compositing nearly complete: finish silently.

This requires lightweight progress reporting from the renderer.

PDFium’s progressive API uses explicit start/continue calls and keeps the in-progress bitmap in a reusable state between pauses, avoiding unnecessary conversion work.  Lege does not need to copy PDFium’s API, but operation- or band-level yielding would make preemption and first-pixel delivery more predictable.

---

# Redesign the tile cache before populating thousands of previews

A better tile layout is page-indexed:

```rust
#[derive(Default, Clone)]
struct TierSlots {
    draft: Option<Arc<TileSurface>>,
    text_first: Option<Arc<TileSurface>>,
    final_surface: Option<Arc<TileSurface>>,
}

#[derive(Default, Clone)]
struct BucketTiles {
    cells: HashMap<TileCoord, TierSlots>,
}

#[derive(Default, Clone)]
struct PageTileSnapshot {
    buckets: BTreeMap<ZoomBucket, BucketTiles>,
}
```

Store one `ArcSwap<PageTileSnapshot>` per page:

```rust
struct TileCache {
    pages: Box<[ArcSwap<PageTileSnapshot>]>,
    // eviction metadata separately
}
```

On insertion:

* clone only one page’s small snapshot;
* update one bucket/cell;
* atomically swap it.

During painting:

* load snapshots only for visible pages;
* exact lookup is O(1);
* fallback bucket lookup scans only that page’s buckets;
* no full-document tile map clone is required.

The dedicated `PagePreviewCache` remains separate.

---

# Stop repainting the whole UI for background work

The current app treats every `PageIndexed`, progress update, and tile completion as a full visible change.

That should be narrowed.

For a tile:

```rust
SessionUpdate::TileReady {
    key,
    document_rect,
    ..
} => {
    if self.intent.page_is_relevant(key.page) {
        self.refresh_tile_snapshot_for_page(key.page);

        if let Some(screen_rect) =
            self.document_rect_to_screen(document_rect)
        {
            self.damage.add(screen_rect);
            self.request_redraw();
        }
    }
}
```

For background indexing:

* update the search index without canvas damage;
* redraw only the search/sidebar/status region when that UI is visible;
* coalesce progress to perhaps 10–20 Hz;
* do not redraw once per indexed page.

For offscreen preview completion:

* insert silently;
* no `ViewerEvent::Wake`;
* wake only if the page is currently visible or used by the scrollbar popup.

Otherwise, a successful background sweep can consume a surprising amount of UI-thread time.

---

# Measure user-facing milestones, not pages per second alone

Add one trace record per navigation generation:

```rust
#[derive(Debug, Default)]
struct SeekTrace {
    input_received: Option<Instant>,
    intent_published: Option<Instant>,

    compile_queued: Option<Instant>,
    compile_started: Option<Instant>,
    compile_finished: Option<Instant>,

    first_page_pixels_ready: Option<Instant>,
    first_page_pixels_presented: Option<Instant>,

    viewport_covered: Option<Instant>,
    readable_representation_presented: Option<Instant>,
    exact_viewport_presented: Option<Instant>,

    cancelled_cpu_ms: f64,
    discarded_cpu_ms: f64,
}
```

Record separate timings for:

```text
compile queue wait
page compilation
raster queue wait
request lowering
image decode
raster execution
RGBA conversion
cache insertion
UI wake
snapshot update
paint
present
```

The primary product metrics should be:

| Metric                             | Meaning                                |
| ---------------------------------- | -------------------------------------- |
| Input → first page-specific pixels | Did the seek feel instant?             |
| Input → viewport fully covered     | Were there blank holes?                |
| Input → readable                   | Could the user begin reading?          |
| Input → final exact                | How quickly did it settle?             |
| Wasted work after navigation       | Is prediction harming foreground work? |
| Cache hit by navigation reason     | Which predictions are useful?          |

Measure p50, p95, and p99 across realistic traces:

```text
open and page-down
slow wheel reading
fast wheel skimming
outline jump
search next/previous
history back
link jump
scrollbar scrub and release
typed page number
reopen document
random stress seek
```

Random stress seeking should remain a regression ceiling, but it should not dictate the entire scheduling strategy.

---

# Recommended implementation order

| Priority | Change                                                       | Main result                                       |
| -------- | ------------------------------------------------------------ | ------------------------------------------------- |
| **P0**   | Settle programmatic jumps and wheel idle                     | Final-quality work starts reliably                |
| **P0**   | Visible compile first, queued-job promotion                  | Removes avoidable seek delay                      |
| **P0**   | Unify interactive/index compile jobs                         | Stops duplicate compilation                       |
| **P0**   | Reserve foreground worker capacity and shallow worker queues | Removes priority inversion                        |
| **P0**   | Stop scheduling same-bucket fake Draft + Final               | Removes duplicate raster work                     |
| **P0**   | Add seek-stage instrumentation                               | Reveals actual latency distribution               |
| **P1**   | Dedicated universal page preview cache                       | Blank random seeks disappear                      |
| **P1**   | Generate previews from background indexing artifacts         | Reuses work already being performed               |
| **P1**   | Draw preview once as page underlay                           | Stable progressive replacement                    |
| **P1**   | Visibility-aware updates and damage                          | Background work stops disturbing UI               |
| **P2**   | Persistent/mmap preview pack                                 | Instant seeks after reopening                     |
| **P2**   | Page-indexed tile cache snapshots                            | Cache cost remains bounded                        |
| **P2**   | Warm-hint API for outline/search/history/scrollbar/typeahead | High-probability jumps become pre-rendered        |
| **P3**   | Tile-run/band raster and shared backing views                | Amortizes lowering and raster setup               |
| **P3**   | Native XRGB output                                           | Removes full-surface swizzle/copy                 |
| **P3**   | Adaptive background concurrency                              | Uses all spare renderer throughput safely         |
| **P4**   | Page-phenotype first-paint passes                            | Faster readable display across text and scan PDFs |
| **P4**   | Cold stripped-IR pack                                        | Removes compilation from more random seeks        |
| **P4**   | Native-text and OCR TOC inference                            | Adds weak hints for poorly authored documents     |

# Bottom line

Do not choose between brute force and viewer tricks. Use both at different levels:

* **Brute-force every page into a small canonical preview.**
* **Brute-force exact current-bucket pages only when measured document cost says it is cheap.**
* **Use outlines, links, search, history, typed page numbers, and scrollbar intent to decide which pages receive readable and exact warmth.**
* **Reserve foreground capacity so background work never blocks a confirmed seek.**
* **Exploit renderer integration through compile-result reuse, stripped IR retention, band rendering, reduced image decoding, direct pixel output, and cooperative cancellation.**

The most consequential immediate changes are not OCR or a more elaborate heuristic. They are fixing compile promotion, merging index/interactive compilation, settling deliberate jumps, removing duplicate Draft work, and turning the existing whole-document index pass into a universal preview generator. Those changes directly convert Lege’s renderer speed into user-facing seek performance.
