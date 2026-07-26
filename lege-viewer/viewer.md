> **Roadmap note (2026-07-25):** This is the original platform and viewer
> design reference. Its phase numbering is historical. `STAGES.md` is the
> authority for current stage order and for the evidence-gated placement of
> GPU work.

This is feasible, with one necessary qualification: **Winit alone cannot present pixels**. It creates windows and supplies events and raw platform handles, but deliberately provides no drawing API. The minimal practical foundation is therefore **Winit plus Softbuffer**, with every layout, control, scrollbar, compositor, and viewer behavior written specifically for Lege. Winit’s current event model already exposes wheel, pan, pinch, touch, IME, DPI, keyboard, file-drop, and redraw events, so it is sufficient as the platform boundary. ([Docs.rs][1])

# Lege Viewer: From-Scratch Cross-Platform GUI Plan

## 1. Project objective

Create a Windows, Linux, and macOS PDF/DjVu viewer with:

* No general GUI framework.
* No widget template system.
* No HTML/CSS or reactive component model.
* One identical custom-drawn client interface across all platforms.
* Native outer window decorations.
* Direct integration with `lege-render`.
* A permanent software presentation path.
* An optional GPU presentation path later.
* Smooth, precise scrolling designed for document viewing.
* Minimal dependencies and no C or C++ application code.
* Incremental development from a basic viewer into the semantic document application previously described.

The project should create **only the GUI machinery this application requires**. It should not become a reusable GUI library.

---

# 2. Initial dependency policy

## Required external dependency

```toml
winit = { version = "...", features = ["rwh_06"] }
```

Winit owns:

* Window creation.
* Native event loop integration.
* Mouse, keyboard, touch, gesture, and IME events.
* DPI and monitor changes.
* Drag-and-drop events.
* Cursor management.
* Native window decorations.
* Raw window/display handles.

Winit does not draw into windows. It exposes raw handles so another presentation implementation can attach to the native window. ([Docs.rs][1])

## Recommended second dependency

```toml
softbuffer = "..."
```

Softbuffer is not a GUI library. It supplies writable software window buffers through the raw-window-handle interface. It also supports presenting explicit damage rectangles. ([Docs.rs][2])

The initial dependency boundary should therefore be:

```text
std
winit
softbuffer
existing Lege workspace crates
```

No geometry crate, layout crate, widget crate, theme crate, image-view crate, scene-graph crate, or scroll crate should be introduced.

## Why not Winit alone

A Winit-only application would need three or four native presentation implementations:

```text
Windows:
    Win32 GDI, Direct2D, DirectComposition, or DXGI

macOS:
    CoreGraphics, IOSurface, CoreAnimation, or Metal

Linux X11:
    XImage or MIT-SHM

Linux Wayland:
    wl_shm buffers, buffer lifecycle, and surface commits
```

That is possible, but it would turn the first viewer milestone into a platform-presentation project. Softbuffer already handles this narrow problem while leaving all GUI behavior under Lege’s control.

Place it behind an internal trait so it can later be:

* Retained permanently.
* Forked or vendored.
* Replaced by custom platform presenters.
* Supplemented by WGPU.

---

# 3. Keep the first implementation in one crate

Do not begin by designing a family of reusable GUI crates.

Use one application crate:

```text
lege-viewer/
├── src/
│   ├── main.rs
│   ├── app.rs
│   ├── event.rs
│   ├── geometry.rs
│   ├── theme.rs
│   ├── layout.rs
│   ├── input.rs
│   ├── frame.rs
│   ├── damage.rs
│   ├── paint.rs
│   ├── present/
│   │   ├── mod.rs
│   │   └── softbuffer.rs
│   ├── document/
│   │   ├── session.rs
│   │   ├── canvas.rs
│   │   ├── viewport.rs
│   │   ├── page_layout.rs
│   │   ├── surface_cache.rs
│   │   └── renderer_bridge.rs
│   ├── scroll/
│   │   ├── mod.rs
│   │   ├── model.rs
│   │   ├── input.rs
│   │   └── physics.rs
│   ├── chrome/
│   │   ├── mod.rs
│   │   ├── toolbar.rs
│   │   ├── sidebar.rs
│   │   ├── scrollbar.rs
│   │   ├── status.rs
│   │   └── popup.rs
│   ├── text/
│   │   ├── mod.rs
│   │   ├── layout.rs
│   │   ├── edit.rs
│   │   └── cache.rs
│   └── diagnostics/
│       ├── mod.rs
│       ├── frame_metrics.rs
│       └── debug_overlay.rs
```

Split modules into separate crates only after stable ownership boundaries become evident.

This minimizes:

* Public API boilerplate.
* Cross-crate type conversions.
* Feature propagation.
* Duplicate error types.
* Abstraction introduced before actual requirements are known.

---

# 4. Do not create a generic widget system

Avoid beginning with:

```rust
trait Widget {
    fn layout(...);
    fn event(...);
    fn paint(...);
}
```

That immediately creates questions about:

* Widget identity.
* Parent-child lifetimes.
* Dynamic dispatch.
* Event propagation.
* Focus traversal.
* Invalidation.
* State ownership.
* Reusable layout.
* Styling.
* Accessibility adapters.

Instead, make the first interface concrete:

```rust
pub struct AppUi {
    pub toolbar: ToolbarState,
    pub sidebar: SidebarState,
    pub canvas: DocumentCanvas,
    pub vertical_scrollbar: ScrollbarState,
    pub horizontal_scrollbar: ScrollbarState,
    pub status_bar: StatusBarState,
    pub popup: Option<PopupState>,
}
```

Use one explicit layout calculation:

```rust
pub struct AppLayout {
    pub toolbar: Rect,
    pub sidebar: Rect,
    pub canvas: Rect,
    pub vertical_scrollbar: Rect,
    pub horizontal_scrollbar: Rect,
    pub status_bar: Rect,
}
```

```rust
impl AppLayout {
    pub fn calculate(
        window: PhysicalSize,
        scale_factor: f64,
        sidebar_width: f64,
        theme: &ThemeMetrics,
    ) -> Self;
}
```

Use an application-specific hit target:

```rust
pub enum HitTarget {
    None,
    Canvas,
    ToolbarButton(Command),
    SidebarRow(NavigationNodeId),
    VerticalScrollbar(ScrollbarPart),
    HorizontalScrollbar(ScrollbarPart),
    SearchField,
    StatusItem(StatusItemId),
    PopupItem(PopupItemId),
}
```

The event flow can initially be fixed:

```text
popup
    ↓
scrollbars
    ↓
toolbar/sidebar
    ↓
document canvas
```

This is not a GUI toolkit. It is the concrete interaction model of one application.

Generalization should occur only when two real controls require the same mechanism.

---

# 5. Thread architecture

## Main thread

The Winit event loop and all UI state live on the main thread.

Winit’s event loop is intentionally not `Send` or `Sync`; cross-thread work should wake it through an `EventLoopProxy`. ([Docs.rs][3])

The main thread owns:

* Winit window.
* Presenter.
* Retained window backbuffer.
* UI state.
* Focus and pointer capture.
* Viewport.
* Page-surface references.
* Frame scheduler.
* Damage tracker.

It must never perform:

* PDF parsing of substantial objects.
* Page compilation.
* Rasterization.
* Image decoding.
* JBIG2 decoding.
* OCR.
* PAL analysis.
* Whole-document indexing.

## Renderer and compute threads

The Lege compute scheduler owns:

* Page compilation.
* CPU rendering.
* Decoders.
* Font worker state.
* OCR.
* Layout detection.
* Thumbnail creation.

## Tokio control runtime

Tokio remains separate and handles:

* Document-session orchestration.
* Cancellation coordination.
* File watching.
* Library services.
* IPC or networking.
* Async result plumbing where useful.

Do not run the Winit loop inside Tokio, and do not run rendering work on Tokio worker threads.

---

# 6. Cross-thread update path

Define a small Winit user-event type:

```rust
pub enum ViewerEvent {
    Wake,
    DocumentOpened(DocumentId),
    PageSurfaceReady(PageSurfaceUpdate),
    PageStructureReady(PageStructureUpdate),
    OutlineUpdated(OutlineUpdate),
    OcrUpdated(OcrUpdate),
    FatalBackgroundError(BackgroundError),
}
```

Workers should not send every tile or small update directly through `EventLoopProxy`.

Use:

```text
worker
    ↓ pushes update
bounded/coalescing update queue
    ↓ first producer changes wake flag false → true
EventLoopProxy::send_event(ViewerEvent::Wake)
    ↓
UI drains all pending updates
    ↓ resets wake flag
```

This prevents an event storm when many pages or tiles complete together.

Winit’s `EventLoopProxy` is explicitly designed to send user events from another thread into the event loop. ([Docs.rs][4])

---

# 7. Main event and frame pipeline

The central flow should be:

```text
Winit window/input event
        ↓
normalize input
        ↓
update concrete application state
        ↓
mark damaged regions
        ↓
request one redraw
        ↓
WindowEvent::RedrawRequested
        ↓
compose damaged regions
        ↓
present
```

Winit aggregates duplicate redraw requests and recommends drawing when `RedrawRequested` arrives, rather than maintaining a permanent polling loop. ([Docs.rs][5])

## Frame modes

```rust
pub enum FrameMode {
    Idle,
    Interactive,
    Animating,
}
```

### Idle

* No continuous redraw.
* Event loop waits.
* CPU use should be effectively zero.
* A renderer update requests one new frame.

### Interactive

Used while:

* Scrolling.
* Resizing.
* Dragging.
* Selecting.
* Zooming.
* Moving a scrollbar thumb.

Redraw requests continue while input or visible motion remains active.

### Animating

Used for:

* Kinetic scrolling.
* Animated page jumps.
* Scrollbar fading.
* Sidebar transitions, if later desired.

The frame scheduler calculates the next wake time. It should never use unrestricted polling merely to keep the application responsive.

---

# 8. Presentation abstraction

Define the boundary early:

```rust
pub trait Presenter {
    fn resize(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<(), PresentError>;

    fn present(
        &mut self,
        pixels: &[u32],
        stride: usize,
        damage: &[DamageRect],
    ) -> Result<(), PresentError>;

    fn format(&self) -> PixelFormat;
}
```

Initial implementation:

```text
SoftbufferPresenter
```

Later implementations may include:

```text
NativeWin32Presenter
NativeX11Presenter
NativeWaylandPresenter
NativeMacPresenter
WgpuPresenter
```

No application code should know which presenter is active.

## Retained backing buffer

Maintain one application-owned window framebuffer:

```rust
pub struct WindowBuffer {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub pixels: Vec<u32>,
}
```

Do not rely on a presentation buffer preserving previous contents.

The application-owned buffer enables:

* Partial redraw.
* Scroll blitting.
* Screenshot testing.
* Damage diagnostics.
* Presenter replacement.
* Deterministic client-area output.

---

# 9. Minimal custom software compositor

Implement only the primitives the viewer needs.

## Phase-one painter operations

```rust
pub struct Painter<'a> {
    target: &'a mut WindowBuffer,
    clip: RectI,
}
```

Required operations:

```rust
fn clear(&mut self, color: Color);
fn fill_rect(&mut self, rect: RectI, color: Color);
fn stroke_rect(&mut self, rect: RectI, width: u32, color: Color);
fn draw_horizontal_line(...);
fn draw_vertical_line(...);

fn blit_opaque(
    &mut self,
    source: &PageSurface,
    source_rect: RectI,
    destination: PointI,
);

fn blend_alpha_mask(
    &mut self,
    mask: &AlphaMask,
    destination: PointI,
    color: Color,
);

fn push_clip(&mut self, rect: RectI);
fn pop_clip(&mut self);
```

Later additions:

```text
rounded rectangles
scaled page blits
selection overlays
vector icons
drop shadows
subpixel text
```

Do not begin with arbitrary transformed layers or a scene graph.

## Kernel implementation

Use:

* Contiguous row loops.
* Explicit stride.
* Clipped source and destination rectangles.
* No per-pixel virtual calls.
* No allocations during paint.
* Narrowly audited `unsafe` only after safe implementations are measured.
* Shared blending primitives with `lege-render` where appropriate.

The compositor and PDF renderer may share:

* Color types.
* Alpha-mask representation.
* Rectangle clipping.
* Span blending.
* Affine and geometry types.
* Font outline and glyph-mask infrastructure.

They should not share mutable renderer state.

---

# 10. Document viewport architecture

The document canvas is not a scrollable collection of page widgets.

Use one viewport over a virtual page layout:

```rust
pub struct DocumentViewport {
    pub scroll: ScrollModel,
    pub zoom: f64,
    pub rotation: PageRotation,
    pub viewport: RectF,
    pub layout: PageLayoutIndex,
    pub generation: u64,
}
```

## Page layout index

```rust
pub struct PagePlacement {
    pub page: PageIndex,
    pub bounds: RectF,
}
```

For continuous vertical layout, maintain:

```rust
pub struct PageLayoutIndex {
    pub placements: Box<[PagePlacement]>,
    pub page_starts_y: Box<[f64]>,
    pub total_width: f64,
    pub total_height: f64,
}
```

Visible-page lookup becomes:

```text
binary search first page intersecting viewport top
linear scan until page top exceeds viewport bottom
```

This remains cheap for documents containing tens of thousands of pages.

No page object should exist merely because the document contains that page.

## Active page range

Maintain:

```text
visible pages
+ small directional overscan
+ optional thumbnail references
```

The overscan should grow in the current scroll direction and shrink behind it.

---

# 11. Design the scroll core before designing the scrollbar

The scrollbar is only one controller of the scroll model.

Build a general input-independent model:

```rust
pub struct ScrollModel {
    pub position: Vec2d,
    pub target: Vec2d,
    pub velocity: Vec2d,

    pub content_extent: Vec2d,
    pub viewport_extent: Vec2d,

    pub mode: ScrollMode,
    pub generation: u64,
    pub last_update: Instant,
}
```

```rust
pub enum ScrollMode {
    Stationary,
    Direct,
    Animated,
    Kinetic,
    ThumbDrag,
}
```

Inputs become explicit commands:

```rust
pub enum ScrollCommand {
    WheelPixels(Vec2d),
    WheelLines(Vec2d),
    TouchpadPan(Vec2d),
    DragPan(Vec2d),
    SetAbsolute(Vec2d),
    PageStep(AxisDirection),
    FineStep(Vec2d),
    JumpTo(DocumentLocation),
    Stop,
}
```

This provides the foundation for later:

* High-quality custom scrollbar dragging.
* Click-and-drag document panning.
* Middle-mouse autoscroll.
* Fine keyboard movement.
* Adjustable line movement.
* Page-edge snapping.
* Controlled kinetic scrolling.
* Cursor-anchored zoom.
* Reading-mode movement.
* Custom navigation hardware.

## Precision rules

From the beginning:

* Store scroll positions as `f64`.
* Never quantize accumulated wheel or touchpad input.
* Keep physical-pixel and logical-page coordinates distinct.
* Convert line-wheel events through a configurable line distance.
* Preserve direct pixel deltas from precision devices.
* Separate direct manipulation from animated movement.
* Clamp only at the final content boundary.
* Base animation on elapsed time, not frame count.
* Clamp abnormally large frame-time deltas after stalls.
* Keep horizontal and vertical movement independent.

## Software-presentation placement

At settled positions:

* Snap page raster placement to physical pixels.
* Avoid blurred text caused by fractional bitmap placement.

During direct movement:

* Preserve the fractional scroll remainder internally.
* Move the visual surface at integer pixel boundaries.
* Carry the remainder into subsequent input.
* Use exact fractional placement later in the GPU presenter.

This allows precise control without resampling every page surface during each software frame.

---

# 12. Scroll-blit path

When all of these remain unchanged:

* Zoom.
* Rotation.
* Page layout.
* Window dimensions.
* Sidebar geometry.
* Page surface versions.

and scrolling moves the canvas by an integer pixel amount:

1. Move the existing canvas pixels within the retained framebuffer.
2. Mark the newly exposed horizontal or vertical strip as damaged.
3. Redraw only that strip.
4. Redraw fixed-position overlays such as scrollbars.
5. Present the accumulated damage.

```rust
pub fn scroll_blit(
    buffer: &mut WindowBuffer,
    canvas: RectI,
    delta_x: i32,
    delta_y: i32,
) -> ExposedRegions;
```

This path can make software scrolling largely independent of window size when the scroll delta is small.

Fall back to a normal canvas repaint when:

* A page surface changes.
* A popup overlaps the canvas.
* Zoom changes.
* Fractional scaling is active.
* The scroll delta exceeds the canvas.
* The application is resized.

---

# 13. Renderer integration

The viewer should consume immutable surface updates:

```rust
pub struct PageSurfaceUpdate {
    pub document: DocumentId,
    pub page: PageIndex,
    pub key: PageSurfaceKey,
    pub generation: u64,
    pub surface: Arc<PageSurface>,
}
```

```rust
pub struct PageSurfaceKey {
    pub scale_bucket: RenderScale,
    pub rotation: PageRotation,
    pub region: Option<PageRegion>,
    pub quality: SurfaceQuality,
}
```

```rust
pub enum SurfaceQuality {
    Thumbnail,
    Preview,
    Exact,
}
```

The UI swaps the new `Arc<PageSurface>` into its cache. It never waits for or locks a surface while the renderer is writing to it.

## Render selection

For each visible page, choose the best available presentation:

```text
exact surface at target scale
    else close-scale exact surface
    else preview surface
    else thumbnail
    else placeholder
```

Render requests should prioritize:

1. Visible missing previews.
2. Visible exact surfaces.
3. Adjacent previews.
4. Directional overscan.
5. Thumbnails.
6. Background work.

Scrolling itself must never wait for page rendering.

## Zoom behavior

During a zoom gesture:

* Reuse the existing page surface.
* Scale it temporarily in the compositor.
* Update page geometry immediately.
* Delay exact-scale rendering until the gesture settles.
* Request exact visible surfaces under a new viewport generation.
* Discard or cache stale completions.

---

# 14. Surface and memory cache

Use explicit budgets rather than page-count limits:

```rust
pub struct SurfaceCache {
    entries: HashMap<PageSurfaceKey, SurfaceEntry>,
    resident_bytes: u64,
    budget_bytes: u64,
}
```

Eviction priority should consider:

* Visible status.
* Distance from viewport.
* Surface quality.
* Scale similarity to current zoom.
* Cost of reproducing the surface.
* Last use.
* Whether a lower-quality replacement exists.

Do not evict the only preview of an adjacent page merely to preserve an exact surface hundreds of pages away.

The viewer, renderer, image decoder, OCR system, and PAL analysis should participate in the same document memory-budget policy.

---

# 15. Concrete application chrome

Begin with fixed, concrete regions:

```text
toolbar
sidebar
document canvas
vertical scrollbar
horizontal scrollbar
status strip
popup layer
```

## Initial toolbar

Only:

* Open file.
* Previous and next page.
* Page number.
* Zoom out/in.
* Fit width.
* Fit page.
* Toggle sidebar.
* Search field placeholder.

## Initial sidebar

Only:

* Embedded outline.
* Page list or thumbnails later.

## Status strip

Only:

* Current page.
* Page count.
* Zoom.
* Render/debug status during development.

Avoid preferences, annotations, OCR, library management, and complex menus until the viewer core is stable.

---

# 16. Theme system

Use one explicit theme structure:

```rust
pub struct Theme {
    pub metrics: ThemeMetrics,
    pub colors: ThemeColors,
}

pub struct ThemeMetrics {
    pub toolbar_height: f64,
    pub status_height: f64,
    pub scrollbar_width: f64,
    pub sidebar_default_width: f64,
    pub border_width: f64,
    pub corner_radius: f64,
    pub control_padding: f64,
}
```

Do not build stylesheets.

Use a bundled UI font and bundled vector or alpha-mask icons so client-area output remains consistent across operating systems.

Allow:

```text
light
slightly dark
time-adjusted
high contrast later
```

but keep them as complete theme records, not cascading style rules.

---

# 17. Text strategy

Text is the largest subsystem that can accidentally turn the project into a GUI toolkit.

## Initial text scope

Support:

* Static toolbar labels.
* Page number.
* Status text.
* Outline titles.
* One simple search field.

Reuse the renderer’s font-program and glyph-outline infrastructure where practical.

Use:

* Bundled font data.
* Worker-independent immutable font program.
* UI-thread glyph-mask cache.
* Alpha8 glyph masks.
* Direct mask compositing.

## Editable text

Implement one concrete `TextFieldState`:

```rust
pub struct TextFieldState {
    pub text: String,
    pub selection: Range<usize>,
    pub composition: Option<ImeComposition>,
    pub caret_visible: bool,
    pub horizontal_scroll: f64,
}
```

Winit supplies IME events, but selection, caret movement, deletion, clipboard operations, and visual composition remain application responsibilities. Winit’s event set includes IME and keyboard events across its window-event model. ([Docs.rs][5])

Do not implement a general multiline editor initially.

## Multilingual shaping

High-quality arbitrary-script UI text may eventually justify one isolated shaping dependency. Do not attempt to implement Arabic, Indic, bidirectional, and complex-script shaping merely to maintain a one-crate ideal.

The dependency policy should be:

> Implement simple application machinery directly; depend on a focused library when the alternative is reimplementing an international standard.

---

# 18. Input routing and pointer capture

Maintain explicit state:

```rust
pub struct InputState {
    pub pointer_position: PointF,
    pub buttons: MouseButtons,
    pub modifiers: Modifiers,
    pub hover: HitTarget,
    pub capture: Option<PointerCapture>,
    pub focus: FocusTarget,
}
```

```rust
pub enum PointerCapture {
    CanvasPan(CanvasPanState),
    VerticalThumb(ScrollbarDragState),
    HorizontalThumb(ScrollbarDragState),
    SidebarResize(SplitterDragState),
    Selection(TextSelectionState),
}
```

Once a drag begins, its target remains captured until release, even when the pointer leaves the original bounds.

This is essential for a non-janky scrollbar and drag-to-pan implementation.

---

# 19. Damage tracking

Use a bounded damage list:

```rust
pub struct DamageRegion {
    rects: Vec<RectI>,
    full: bool,
}
```

Rules:

* Clip every damage rectangle to the window.
* Merge overlapping or adjacent rectangles.
* Promote to full-window redraw when the list becomes too fragmented.
* Track canvas damage separately from fixed chrome damage.
* Expose a debug overlay showing damage.

Damage sources include:

```text
page surface replacement
scrollbar state change
cursor or caret blink
selection change
sidebar update
popup opening/closing
window exposure
resize
theme change
```

Softbuffer’s presentation API can accept explicit damage rectangles, although the application must still preserve and repaint its own retained backing store correctly. ([Docs.rs][2])

---

# 20. Diagnostics from the first prototype

Implement internal metrics before adding features:

```rust
pub struct FrameMetrics {
    pub input_received: Instant,
    pub redraw_requested: Instant,
    pub frame_started: Instant,
    pub compose_finished: Instant,
    pub present_finished: Instant,

    pub damaged_pixels: u64,
    pub copied_pixels: u64,
    pub page_blits: u32,
    pub allocations: u32,
}
```

Track:

* Input-to-frame latency.
* Composition time.
* Presentation time.
* Frame interval.
* Missed frame budget.
* Scroll-blit hit rate.
* Damage area.
* Page-surface cache hit rate.
* Render-request latency.
* Visible page quality.
* Allocations during steady-state scrolling.

Add a toggleable overlay:

```text
FPS / frame interval
compose ms
present ms
damage %
surface cache MB
visible pages
render queue
scroll mode
```

Scrollbar and movement quality cannot be tuned without frame and input traces.

---

# 21. Development phases

## Phase 0: architecture contracts

Implement:

* Geometry types.
* Pixel format.
* Presenter trait.
* Window-buffer ownership.
* Damage-region model.
* Viewer event type.
* Thread ownership rules.
* Error types.

Exit gate:

* No GUI behavior yet.
* The presenter can be replaced without touching application state.

## Phase 1: window and software presentation proof

Implement:

* Winit `ApplicationHandler`.
* Native decorated window.
* Softbuffer presenter.
* Resize.
* DPI changes.
* Retained window buffer.
* Full-buffer clear and presentation.
* Redraw-on-demand.

Exit gate:

* Identical solid and patterned test frames on Windows, Linux, and macOS.
* No continuous idle redraw.
* Resize does not panic, tear, or expose uninitialized client pixels.

## Phase 2: software painter

Implement:

* Rectangles.
* Lines.
* Clipping.
* Opaque bitmap blit.
* Alpha-mask blending.
* Damage visualization.
* Screenshot export for tests.

Exit gate:

* Golden screenshots are pixel-identical across operating systems for the client area.
* No allocations in repeated primitive painting.

## Phase 3: synthetic virtual document

Implement:

* 10,000 synthetic pages.
* Continuous page layout.
* Binary-search visible-page lookup.
* Mouse-wheel movement.
* Page placeholders.
* Vertical custom scrollbar.
* Scroll blitting.
* Frame metrics.

Do not connect `lege-render` yet.

Exit gate:

* Page count does not materially change frame cost.
* Scrolling work depends on visible area, not document length.
* Idle CPU remains near zero.
* Steady scrolling performs no heap allocations.
* Input remains responsive during rapid movement.

## Phase 4: `lege-render` bridge

Implement:

* `DocumentSession`.
* Async page-surface requests.
* `EventLoopProxy` wake path.
* Preview and exact surfaces.
* Surface cache.
* Viewport generation.
* Cancellation.
* Visible-page priorities.

Exit gate:

* Opening a real PDF paints a visible preview as soon as one page completes.
* Scrolling remains independent of render completion.
* Outdated render results never replace newer surfaces.
* Renderer workers never block the UI thread.

## Phase 5: zoom and navigation

Implement:

* Fit width.
* Fit page.
* Cursor-anchored zoom.
* Temporary surface scaling.
* Exact-scale rerender after settling.
* Page number navigation.
* Back and forward history.
* Existing PDF/DjVu outline.

Exit gate:

* Zoom changes viewport geometry immediately.
* Exact rendering occurs asynchronously.
* Navigation to uncached pages remains interactive.

## Phase 6: basic application chrome

Implement:

* Toolbar.
* Sidebar.
* Status strip.
* Buttons.
* Toggle controls.
* Splitter.
* Tooltips.
* Simple popup menu.
* Theme structure.

Exit gate:

* All client-area controls look identical across platforms.
* Chrome repaint does not force unnecessary canvas repaint.
* Pointer capture and keyboard focus are deterministic.

## Phase 7: text and search shell

Implement:

* Bundled UI font.
* Glyph-mask cache.
* Static text.
* One editable search field.
* IME composition.
* Clipboard integration.
* Native PDF text search.

Exit gate:

* Search input works on all platforms.
* Text repaint is damage-bounded.
* Caret blinking does not redraw the document canvas.

## Phase 8: advanced movement controls

Build on the established `ScrollModel`:

* Drag-to-pan.
* Fine movement commands.
* Adjustable wheel scaling.
* Kinetic movement.
* Page snapping.
* Scrollbar track behavior.
* Thumb drag refinement.
* Middle-mouse autoscroll.
* Navigation easing.
* Optional overlay scrollbars.

Exit gate:

* All movement sources feed one scroll model.
* Direct manipulation is never delayed by animation.
* Input traces can be replayed deterministically.
* Scrolling remains smooth while page rendering is active.

## Phase 9: semantic viewer features

Add:

* PAL layout overlay.
* Automatic chapter-title candidates.
* Virtual outline.
* OCR current page.
* Whole-document OCR.
* Search result overlays.
* Text selection.
* User annotations.

These are layered over page coordinates and do not modify raster surfaces.

## Phase 10: optional GPU presenter

Add a `WgpuPresenter` only after the software viewer is complete and profiled.

Use GPU presentation for:

* Page textures.
* Fractional page translation.
* High-refresh scrolling.
* Temporary zoom scaling.
* Overlay composition.
* Future backend-resident render output.

Do not remove the software presenter.

## Phase 11: accessibility and platform integration

Add:

* Accessibility tree.
* Screen-reader roles.
* Keyboard traversal.
* Native clipboard.
* File-open dialog.
* File associations.
* Recent-document integration.
* Window restoration.
* Printing later.

This phase may justify focused platform or accessibility dependencies. Hiding these behind narrow interfaces preserves the rest of the architecture.

---

# 22. Performance gates

The basic viewer should not be considered ready merely because it displays pages.

## Steady-state requirements

* No allocations per normal scroll frame.
* No locks in UI painting.
* No blocking work on the UI thread.
* No per-page GUI objects outside the active viewport.
* No permanent frame loop while idle.
* No page rendering triggered synchronously by scrolling.
* No full document relayout during viewport movement.
* No generic object tree traversal during every frame.

## Frame budget

For a display interval:

```text
60 Hz:  16.67 ms
120 Hz:  8.33 ms
144 Hz:  6.94 ms
```

Composition should consume only part of this budget, leaving time for event handling and presentation.

Measure independently at:

* 1080p.
* 1440p.
* 4K.
* 100%, 150%, and 200% scaling.
* Mouse wheel.
* Precision touchpad.
* Scrollbar dragging.
* Rapid page jumps.
* Active background rendering.

## Stress corpus

Test:

* 10,000 synthetic pages.
* Mixed page sizes.
* Large scanned pages.
* Dense vector pages.
* Image-heavy PDFs.
* Rapid zoom changes.
* Continuous renderer completion while scrolling.
* Sidebar opening and closing during movement.
* Repeated monitor/DPI transitions.

---

# 23. Explicit non-goals

Do not build these unless the viewer later proves they are necessary:

* A public GUI crate.
* CSS.
* Flexbox or grid.
* Virtual DOM.
* Reactive dependency framework.
* General widget inheritance.
* Arbitrary nesting of scroll containers.
* Rich-text editor.
* General animation system.
* Pluggable renderer API for third-party widgets.
* Custom window decorations.
* Cross-platform native control abstraction.
* Full vector scene graph.
* Custom Wayland, X11, Win32, and AppKit presenters during the initial viewer work.

These would convert a controlled application project into a GUI-framework project.

---

# 24. Criteria for replacing Softbuffer

Softbuffer should be replaced or forked only when measurements show one of the following:

* Presentation copies dominate frame time.
* Damage handling is insufficient.
* Frame pacing is visibly inconsistent.
* A platform backend prevents required synchronization.
* WGPU-rendered pages need zero-copy presentation.
* Color-management requirements exceed the abstraction.
* HDR output becomes a real requirement.

At that point, replace the presenter, not the UI.

The app-specific compositor, viewport, controls, scroll model, renderer bridge, and surface cache remain unchanged.

---

# 25. Intended final architecture

```text
┌──────────────────────────────────────────────────────────────┐
│ Native decorated Winit window                               │
├──────────────────────────────────────────────────────────────┤
│ Concrete Lege UI state                                      │
│ toolbar · sidebar · canvas · scrollbars · status · popups   │
├──────────────────────────────────────────────────────────────┤
│ App-specific layout, input routing, text, and scroll model   │
├──────────────────────────────────────────────────────────────┤
│ Retained Lege software compositor                            │
│ rects · masks · text · page blits · clips · damage           │
├──────────────────────────────────────────────────────────────┤
│ Presenter                                                    │
│ Softbuffer initially · WGPU or native presenter later        │
├──────────────────────────────────────────────────────────────┤
│ Winit window and event loop                                  │
└──────────────────────────────────────────────────────────────┘
                 ▲
                 │ immutable incremental updates
                 │
┌──────────────────────────────────────────────────────────────┐
│ DocumentSession                                              │
│ lege-render · decoders · OCR · PAL · search · navigation     │
└──────────────────────────────────────────────────────────────┘
```

The guiding rule is:

> Build a single highly specialized document-viewing application whose internal controls happen to share a few primitives. Do not design those primitives as a GUI library until proven repetition forces a shared abstraction.

The first serious milestone should be a synthetic 10,000-page viewer with a custom scrollbar, retained software framebuffer, damage tracking, and smooth virtual scrolling. Once that works identically across all three operating systems, connect `lege-render`. That sequence isolates GUI and presentation problems from PDF-rendering problems and gives the later movement controls a sound foundation.

The main architectural concession should be Softbuffer as the initial second dependency. Everything above it—including the scrollbar, viewport, frame scheduler, software compositor, controls, and renderer integration—remains Lege-owned and replaceable.

[1]: https://docs.rs/winit/latest/winit/?utm_source=chatgpt.com "winit - Rust"
[2]: https://docs.rs/softbuffer/latest/softbuffer/struct.Buffer.html?utm_source=chatgpt.com "Buffer in softbuffer - Rust"
[3]: https://docs.rs/winit/latest/winit/event_loop/struct.EventLoopProxy.html?utm_source=chatgpt.com "EventLoopProxy in winit::event_loop - Rust"
[4]: https://docs.rs/winit/latest/winit/event_loop/?utm_source=chatgpt.com "winit::event_loop - Rust"
[5]: https://docs.rs/winit/latest/winit/event/enum.WindowEvent.html?utm_source=chatgpt.com "WindowEvent in winit::event - Rust"
