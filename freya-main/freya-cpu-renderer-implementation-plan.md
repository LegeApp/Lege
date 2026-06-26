# Standalone CPU Renderer Feature For Freya Fork

## Summary

Add a new Cargo-selectable CPU renderer path for the fork, optimized first for the Lege GUI rather than full Freya renderer parity. The new renderer will be available as an alternate feature beside the current Skia GPU path, but selection will happen at build time, not through runtime config or `FREYA_RENDERER`.

Default remains the existing Skia GPU renderer. Lege can later build with `default-features = false` and the new CPU renderer feature to avoid linking Skia.

## Public API And Feature Shape

- Add workspace crates:
  - `freya-render-api`: backend-neutral render types and traits.
  - `freya-render-tiny-skia`: CPU raster backend using `tiny-skia`.
  - `freya-text-cosmic`: text layout/raster adapter using `cosmic-text`.
- Add dependencies:
  - `tiny-skia` for CPU raster drawing.
  - `softbuffer` for desktop window presentation.
  - `cosmic-text` for labels, paragraphs, caret geometry, selection rects, and input hit testing.
- Add Cargo features:
  - `freya/cpu-renderer`
  - `freya-winit/cpu-renderer`
  - `freya-core/cpu-renderer`
  - Keep current `skia-engine` feature intact.
  - Do not add runtime renderer selection in v1.
  - Do not make `cpu-renderer` part of default features.
- Build policy:
  - `skia-engine` builds the current Skia GPU backend.
  - `cpu-renderer` builds the new tiny-skia/cosmic-text/softbuffer backend.
  - If both features are enabled, fail compilation with a clear `compile_error!` until dual-backend runtime selection is intentionally designed.

## Implementation Changes

- Replace Skia-shaped core context with backend-neutral traits:
  - Change `RenderContext` from raw `Canvas`, `FontCollection`, and `FontMgr` to a render command sink plus text/resources handles.
  - Change `ClipContext` to use backend-neutral clip commands.
  - Change `TextCache` from `LRUCache<SkParagraph, NodeId>` to cached `Arc<dyn ParagraphLayout>` or equivalent.
  - Keep a Skia implementation of the new traits so the existing GPU path can continue compiling during migration.
- Implement the renderer contract needed by Lege first:
  - Clear/background fill.
  - Solid rect fills.
  - Borders.
  - Rounded corners.
  - Clip rect and rounded clip.
  - Save/restore render state.
  - Translation for scrolled content and positioned layers.
  - Opacity layers only where current Freya components require them.
  - Basic drop shadows if Lege visual components need them.
  - Defer generic blur, SVG, image rendering, gradients, canvas callbacks, and advanced transforms unless later proven necessary.
- Implement text as a first-class subsystem:
  - Use one `cosmic_text::FontSystem` and `SwashCache` per renderer instance.
  - Implement `ParagraphLayout` with:
    - `size`
    - `draw`
    - `hit_test_point`
    - `rects_for_range`
    - `cursor_rect`
  - Support Lege-required text behavior:
    - labels
    - multiline paragraphs
    - wrapping by available width
    - font size
    - font weight
    - color
    - line height
    - clipping
    - input caret and selection geometry
- Add the software presentation path in `freya-winit`:
  - Add a `SoftwareDriver` behind `cpu-renderer`.
  - Use `softbuffer` to create, resize, and present a window buffer.
  - Render each frame into a `tiny-skia::Pixmap`.
  - Composite to an opaque background, then convert premultiplied RGBA into `softbuffer`'s `u32` buffer format before presenting.
  - Preserve the current event loop, layout pass, accessibility pass, and redraw scheduling behavior.
- Port core elements in this order:
  - `rect`: fill, border, rounded corners, clip, simple shadow.
  - `label`: measurement and drawing through cosmic text.
  - `paragraph`: multiline wrapping, range rects, hit testing.
  - Freya input-related rendering through paragraph/caret APIs.
  - Scroll view rendering via clip plus translated child content.
  - Popup/layer rendering via existing tree layer ordering.

## Non-Goals For V1

- No runtime renderer switching.
- No SVG support.
- No image support unless Lege directly needs it.
- No canvas callback compatibility.
- No full Skia effect parity.
- No generic blur/backdrop filter implementation.
- No attempt to preserve Skia public types under the CPU renderer.

## Test Plan

- Compile checks:
  - `just c` with default Skia features.
  - `just c` with `default-features = false` and `cpu-renderer`.
  - Verify `skia-engine + cpu-renderer` fails clearly if mutual exclusion is added.
- Renderer unit/golden tests:
  - solid rects
  - rounded rects
  - borders
  - nested clips
  - opacity layer over solid background
  - scroll viewport clipping
  - popup/layer ordering
- Text tests:
  - label measurement
  - multiline paragraph wrapping
  - line height
  - caret rectangle for input positions
  - range-to-rect selection geometry
  - point-to-text-position hit testing
  - clipped input text
- Lege acceptance scenarios:
  - main shell renders without Skia.
  - buttons, checkboxes, cards, status panels, and progress bars render correctly.
  - all `Input::new(...)` fields remain editable.
  - scrollable queue/log/debug popups clip and scroll correctly.
  - popup overlays render above the main UI.
  - release binary built with `cpu-renderer` does not link Skia.

## Assumptions

- The first target is the Lege GUI, not full Freya renderer parity.
- Renderer selection is Cargo-only for v1.
- Skia GPU remains the default renderer.
- The CPU renderer is allowed to omit unused Freya features until Lege needs them.
- Formatting, linting, and focused tests should be run at implementation end, following the repo instructions.
