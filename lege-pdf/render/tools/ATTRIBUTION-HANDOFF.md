# Per-pixel paint attribution — handoff

Written 2026-07-22. Picks up the "show differences by PDF object category" idea:
render an auxiliary coverage/ID buffer from our display list, then intersect
reference-difference pixels with it to report e.g. *"78% of gross-difference
pixels overlap an Image XObject."*

Handed over mid-implementation because of build contention. **Commit `cfb637f`
landed the vocabulary and the plumbing; it is inert.** Everything below is what
remains, in order.

---

## 0. Do this first

The workspace build and the test suite were **not** run — you were building at
the time. `pdf-page-ir`, `pdf-content` and `pdf-render-cpu` each compile
individually.

```bash
cargo build --release --workspace
cargo test --workspace
```

Two new enum variants exist (`DisplayOp::{BeginPaintOrigin, EndPaintOrigin}` and
the `SemanticOp` twins), so any *exhaustive* match must be extended. In source
that is only `pdf-page-ir/src/lib.rs`, `pdf-content/src/lower.rs`,
`pdf-content/src/dump.rs` and `pdf-render-cpu/src/prepared.rs`, all already
done. **`crates/pdf-content/tests/phase3_exit_gate.rs` also matches `DisplayOp`
exhaustively** and is the likely test-compile failure.

Rendering is bit-identical at `cfb637f`: no marker is emitted, so every
operation carries the default `PaintOrigin::PageContent`.

> Line endings: `pdf-page-ir/src/lib.rs`, `pdf-content/src/lower.rs` and
> `pdf-content/src/semantic.rs` are **CRLF**. Editing them with a tool that
> normalises to LF rewrites the whole file and will collide with your work. I
> did exactly that and had to undo it.

---

## 1. What already exists (`cfb637f`)

```rust
// pdf-page-ir
pub enum PaintOrigin {           // the *containing* construct
    PageContent = 0, FormXObject = 1, AnnotationAppearance = 2,
    TilingPatternCell = 3, Type3Glyph = 4, SoftMaskContent = 5,
}
pub enum PaintLeaf {             // what actually painted
    Unpainted = 0, Path = 1, Shading = 2, TilingPattern = 3, Text = 4, Image = 5,
}
// both have `.name() -> &'static str` for report output
```

- `DisplayOp::BeginPaintOrigin(PaintOrigin)` / `EndPaintOrigin`, mirrored on
  `SemanticOp`, mapped in `lower.rs`, printed by both dump formatters.
- `origin: PaintOrigin` on `PreparedCommand`, `PreparedGlyphRun`,
  `PreparedTiling`, `PreparedImage`.
- `prepared.rs` keeps an `origin_stack` in the lowering loop and stamps it onto
  whatever ops each operation appended (`ops_before` / `out.ops[ops_before..]`).
  This avoids threading a parameter through every `lower_*`.

**`PaintLeaf` is deliberately not plumbed** — it is derivable at attribution
time from the prepared op kind, so nothing in the IR carries it:

| prepared op | leaf |
|---|---|
| `PreparedOp::Draw` with `shading: Some(_)` | `Shading` |
| `PreparedOp::Draw` otherwise | `Path` |
| `PreparedOp::TiledFill` | `TilingPattern` |
| `PreparedOp::GlyphRun` | `Text` |
| `PreparedOp::Image` | `Image` |

Note fill and stroke are both `Path`. Distinguishing them would need a real
field, since `PreparedCommand` uses `FillRule::NonZero` for stroke outlines too.
The agreed category list did not ask for it.

---

## 2. Remaining: emit the markers (`pdf-content/src/interpret.rs`)

Nothing emits `BeginPaintOrigin` yet. Five sites, all of which already have a
clean save/restore or resource scope to hang off — note they are the *same*
functions that scope `take_resource_caches`, so the pairing is easy to keep
balanced:

| function | origin | note |
|---|---|---|
| `invoke_form` | `FormXObject` | wrap the `self.run(&content)` call |
| `render_annotation` | `AnnotationAppearance` | appearance streams |
| `exec_char_proc` | `Type3Glyph` | one per glyph; cheap, they are small |
| `run_soft_mask` | `SoftMaskContent` | never reaches the page, but the mask pass can be attributed |
| `compile_tiling` | `TilingPatternCell` | the cell runs in a **sub-interpreter**, so push the marker as the sub-interpreter's first op rather than around the call |

Shape:

```rust
self.ops.push(SemanticOp::BeginPaintOrigin(PaintOrigin::FormXObject));
let result = self.run(&content);
self.ops.push(SemanticOp::EndPaintOrigin);
```

Emit the `End` on *every* path out, including the error path, or the stack
unbalances. `invoke_form` already has a single join point after `self.run`,
which is the right place.

Nesting is a stack and the innermost wins: a form inside an annotation
attributes to `FormXObject`. If you want the full chain rather than the
innermost, that is a bigger change — record the stack depth or intern the chain.
The category list only asked for one outer label, so innermost-wins is enough,
but say so in the report legend.

---

## 3. Remaining: the attribution pass (`pdf-render-cpu/src/attribution.rs`, new)

```rust
pub struct AttributionMap {
    pub width: u32,
    pub height: u32,
    /// `PaintLeaf as u8`, row-major, one byte per pixel.
    pub leaf: Vec<u8>,
    /// `PaintOrigin as u8`, row-major, one byte per pixel.
    pub origin: Vec<u8>,
}

pub fn render_attribution(page: &CpuPreparedPage) -> AttributionMap;
```

Walk `page.ops` in order and stamp coverage; **last writer wins**, so the map
holds the topmost painter of each pixel. Do not blend.

Reuse the real rasteriser rather than approximating geometry — `RasterKernel`
already exposes exactly the sink needed, and `mask.rs::build_clip_mask` is a
working example of driving it:

```rust
raster.fill(&page.points, subs, w, h, rule, |y, x0, x1, cov| {
    for x in x0..=x1 {
        if cov[x] >= COVERAGE_THRESHOLD { /* stamp leaf/origin at (x, y) */ }
    }
});
```

Per op kind:

- **`Draw`** — `cmd.subpath_range` into `page.subpaths` / `page.points`, with
  `cmd.rule`. Same call as above.
- **`TiledFill`** — it carries the fill shape's subpaths; stamp the shape, not
  the individual tiles.
- **`Image`** — `img.bounds` intersected with the clip, and for each pixel map
  through `img.inv` and keep it if it lands inside the unit square. That honours
  rotation, which stamping the bounding box would not.
- **`GlyphRun`** — stamp each `GlyphPlacement`'s coverage bitmap. Falling back
  to glyph bounding boxes overstates text by a lot on sparse pages; prefer the
  real bitmaps.
- **`BeginGroup` / `EndGroup`** — walk the content inline. Group compositing
  does not matter here; only geometry does.
- **`PushSoftMask`** — **skip to `content_end`.** Mask content never paints to
  the page. (Attributing the mask pass separately is possible later; that is
  what `PaintOrigin::SoftMaskContent` is for.)

Honour clips — `cmd.clip` / `img.clip` index `page.clips`; build lazily with
`build_clip_mask` and cache, exactly as `exec.rs` does. Clipping is cheap to
respect and materially changes the answer.

Suggested `COVERAGE_THRESHOLD` = 8/255: anti-aliased edges otherwise smear a
category one pixel wide around every glyph, which inflates `text` in the report.
Make it a parameter and record its value in the output.

### What this is not

Alpha, blend modes, soft masks and knockout groups are all ignored, so a pixel
that an operation covered but did not visibly change is still attributed to it.
That is **diagnostic attribution, not proof of cause** — state it in the report
header so nobody reads "78% overlaps an Image XObject" as "the image is at
fault".

---

## 4. Remaining: CLI

```
pdfr attribute <file.pdf> <page> <out-prefix> [--scale N]
  -> <out-prefix>.leaf.png     grayscale, pixel value = PaintLeaf as u8
  -> <out-prefix>.origin.png   grayscale, pixel value = PaintOrigin as u8
  -> <out-prefix>.legend.json  { "leaf": {"0":"unpainted", ...}, "origin": {...},
                                 "coverage_threshold": 8, "scale": 2.0 }
```

`pdf-cli/src/main.rs` dispatches on `args[0]`; `render` is the model to copy.
Emit the legend from `PaintLeaf::name()` / `PaintOrigin::name()` so the names
cannot drift from the enums.

Grayscale PNG keeps the buffers directly comparable to the difference mask
without a palette round-trip. If you want them eyeball-friendly, add a separate
`--palette` output rather than changing the canonical one.

---

## 5. Wiring into the harness

Per (file, page) the harness already produces our raster, each control's raster,
and a difference mask. Add:

1. our `leaf` and `origin` planes at the same dimensions;
2. for each control, intersect that control's difference mask with the two
   planes and emit counts per category.

```
file,page,renderer,category_kind,category,diff_pixels,category_pixels,share
```

`diff_pixels` = differing pixels in that category; `category_pixels` = that
category's total area, so a reader can tell "images are 60% of the page and 60%
of the difference" (uninformative) from "images are 5% of the page and 60% of
the difference" (a lead). **Report both numbers, never the share alone.**

Emit one row per `category_kind` in `{leaf, origin}` — do not cross-tabulate
them into one label. A form XObject containing an image should appear as
`origin=form-xobject` *and* `leaf=image`, which is the whole reason both are
kept.

---

## 6. Sanity checks worth having

- A page with no content: `leaf` is entirely `unpainted`.
- `pdfjs/issue968`: 96 petals are stroked with a shading pattern, so a large
  `leaf=shading` area — good check that shading is not mislabelled `path`.
- `pdfbox/1416`: a page-sized CMYK image; expect a dominant `leaf=image`. It is
  also a known open bug (we paint nothing there), so it doubles as a check that
  attribution reflects *our display list*, not our output — the region will be
  attributed `image` while our raster is white. That is correct behaviour and a
  good illustration for the report header.
- `pdfjs/issue18466`: content is a tiling pattern fill, so `leaf=tiling-pattern`
  over the block.
- Any annotated file: `origin=annotation-appearance` must be non-empty.

---

## 7. Context

- Renderer-side findings and the settled PDFium-disagreement list: memory notes
  `post-sweep9-fixes`, `adjudicating-pdfium-disagreements`.
- Controls and why majority agreement does not establish correctness:
  `HAYRO-INTEGRATION.md` §6.
- **Never run the diff tool or rebuild while a sweep is in flight** — the
  controller execs a fresh worker binary per chunk, so a rebuild mixes binaries
  across the run, and CPU contention causes false 180 s timeouts. That cost two
  sweeps in this session.
