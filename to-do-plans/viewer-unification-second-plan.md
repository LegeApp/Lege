# Recommendation

The Viewer can replace Freya for the **single-document processing workflow** after one focused usability pass. It should not yet replace Freya as the only processing interface.

The distinction matters:

| Workflow                                                                                                   | Current readiness | Recommendation                                            |
| ---------------------------------------------------------------------------------------------------------- | ----------------: | --------------------------------------------------------- |
| Open one PDF, inspect it, adjust processing, preview a page, then export                                   |         About 90% | Finish the Viewer and make it the default                 |
| Add PDFs, ZIPs, or image folders; queue several books; choose an output directory; inspect queue/log state |     Not at parity | Retain Freya as a separate **Batch Processing** interface |

Freya still accepts PDFs, image ZIPs, and image folders, owns a queue, and has explicit output-directory and queue/log controls. The Viewer’s open dialog currently accepts PDFs only. See `lege-process/GUI/Freya/src/app.rs:1113–1159` and `:2940–3210`, versus `lege-viewer/src/app.rs:2485–2510`.

Do not force the batch workflow into the Viewer’s document-processing drawer. Those are different interaction models:

* **Viewer:** “I am looking at this document; process this document.”
* **Freya/batch workspace:** “Here is a collection of inputs; process this queue.”

The remaining work in the Viewer is mainly interaction coherence, not missing processing technology.

---

# What the final 10% consists of

The current Viewer already has most processing settings, but four structural issues make it feel less complete than Freya:

1. The processing interface is a floating `540 × 370` panel over the document, rather than part of the application layout. It also splits its options among three tabs. See `lege-viewer/src/app.rs:101–137`, `:623–631`, and `:1958–1975`.
2. Several settings are represented by independent booleans even though they are mutually dependent. The current normalization handles only some combinations. See `lege-viewer/src/processing.rs:303–320`.
3. The current Original/New switching opens another complete document through `open_document()`. That is appropriate for a completed export, but it is not a lightweight temporary page preview. See `lege-viewer/src/app.rs:2529–2547`.
4. The toolbar and search field use fixed horizontal positions. Search is always allocated a field even before the user invokes it. See `lege-viewer/src/app.rs:206–218` and `:1945–1955`.

The solution should be a **docked processing workspace**, a **centralized option-state reducer**, and a **single-page ephemeral preview layer**.

---

# 1. Replace the floating panel with a docked top drawer

The Process button should expand a drawer immediately below the main toolbar. It should reserve space and move the document viewport downward rather than covering the page.

Use approximately **26–28% of the available application height** as the default. Store the user’s preferred height as a ratio, not a fixed pixel value.

Recommended behavior:

* Default ratio: `0.28`.
* Normal user-resizable range: approximately `0.20–0.36`.
* Resize from a horizontal splitter along the bottom edge, not the current lower-right corner.
* Persist the chosen ratio.
* At very short window heights, make the drawer internally scrollable rather than restoring the three-tab design.
* Preserve the visible page anchor when opening, closing, or resizing it. The page should not appear to jump to an unrelated location.
* The drawer should span the full application width. Both the sidebar and document canvas begin beneath it.

The current `AppLayout` only calculates toolbar, sidebar, canvas, scrollbars, and status regions. The drawer belongs in this calculation rather than being positioned relative to the canvas afterward. See `lege-viewer/src/chrome/layout.rs:4–69`.

A suitable layout change is:

```rust
#[derive(Debug, Clone, Copy)]
pub struct AppLayout {
    pub toolbar: RectF,
    pub processing_drawer: RectF,
    pub sidebar: RectF,
    pub canvas: RectF,
    pub vertical_scrollbar: RectF,
    pub horizontal_scrollbar: RectF,
    pub status: RectF,
}

impl AppLayout {
    pub fn calculate(
        window: SizeF,
        scale_factor: f64,
        sidebar_visible: bool,
        processing_visible: bool,
        processing_ratio: f64,
        metrics: &ThemeMetrics,
    ) -> Self {
        let toolbar_height = metrics.toolbar_height * scale_factor;
        let status_height = metrics.status_height * scale_factor;
        let scrollbar_width = metrics.scrollbar_width * scale_factor;

        let available_height =
            (window.height - toolbar_height - status_height).max(0.0);

        let requested_drawer_height =
            available_height * processing_ratio.clamp(0.20, 0.36);

        let drawer_height = if processing_visible {
            requested_drawer_height
                .clamp(190.0 * scale_factor, 340.0 * scale_factor)
                .min(available_height)
        } else {
            0.0
        };

        let content_y = toolbar_height + drawer_height;
        let content_height = (available_height - drawer_height).max(0.0);

        let sidebar_width = if sidebar_visible {
            metrics.sidebar_width * scale_factor
        } else {
            0.0
        };

        let canvas_width =
            (window.width - sidebar_width - scrollbar_width).max(0.0);

        Self {
            toolbar: RectF {
                x: 0.0,
                y: 0.0,
                width: window.width,
                height: toolbar_height,
            },
            processing_drawer: RectF {
                x: 0.0,
                y: toolbar_height,
                width: window.width,
                height: drawer_height,
            },
            sidebar: RectF {
                x: 0.0,
                y: content_y,
                width: sidebar_width,
                height: content_height,
            },
            canvas: RectF {
                x: sidebar_width,
                y: content_y,
                width: canvas_width,
                height: content_height,
            },
            vertical_scrollbar: RectF {
                x: sidebar_width + canvas_width,
                y: content_y,
                width: scrollbar_width,
                height: content_height,
            },
            horizontal_scrollbar: RectF {
                x: sidebar_width,
                y: content_y + content_height - scrollbar_width,
                width: canvas_width,
                height: 0.0,
            },
            status: RectF {
                x: 0.0,
                y: toolbar_height + available_height,
                width: window.width,
                height: status_height,
            },
        }
    }
}
```

Opening and closing should preserve the reading position:

```rust
fn set_processing_visible(&mut self, visible: bool) {
    if self.processing_ui.visible == visible {
        return;
    }

    let anchor = self.capture_reading_anchor();

    self.processing_ui.visible = visible;
    self.recalculate_app_layout();

    self.restore_reading_anchor(anchor);
    self.damage.mark_full();
    self.request_redraw();
}
```

The exact anchor method names will depend on the Viewer’s existing navigation code, but the important behavior is that layout changes are anchored to the current page and location.

The old fields can then disappear:

```rust
processing_panel_width: f64,
processing_panel_height: f64,
ProcessingTab,
ProcessingUiState::tab,
```

---

# 2. Use one Freya-derived composition, not three screens

“All options visible at once” should mean **all currently applicable options**, not every irrelevant or disabled setting occupying permanent space.

A good composition is two major columns, preserving Freya’s recognizable layout while fitting the Viewer’s horizontal drawer.

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ Process document   Preset: Custom ▾   Reset                         Close ×  │
├─────────────────────────────────────┬────────────────────────────────────────┤
│ PAGES & TARGET                      │ OUTPUT & PAGE TREATMENT                │
│                                     │                                        │
│ Scope: Document | Current | Range   │ Output: PDF | DjVu | EPUB             │
│ Range: [ 1-20, 25 ]                 │ Rendering: Binarized | Grayscale MRC   │
│ Resolution: [1200x800] [Preset ▾]   │ Layout analysis [✓]  Exclude [ ... ]  │
│                                     │ Image regions: Original | Dithered     │
│ OCR: Off | Fast | Thorough          │ Binarization: Adaptive | Fixed | Heavy │
│ EPUB sidecar [ ]   Invert [ ]       │ Value: [0.05] or [180]                │
│ High quality [ ]  Compatibility [ ] │ Compression / Cover, when applicable  │
│                                     │                                        │
│ Geometry: Original | Center | Crop | Reflow                                 │
│ Crop option: Free aspect [ ]                                                 │
├──────────────────────────────────────────────────────────────────────────────┤
│ Original | Preview    Temporary preview — not saved                         │
│ Help/status text                 [Preview current page] [Process document]   │
└──────────────────────────────────────────────────────────────────────────────┘
```

## Header

The header should contain:

* **Process document** title.
* One preset selector: `Reading`, `Bilevel`, or `Custom`.
* `Reset`.
* Close button.

The current Reading/Bilevel profile buttons silently overwrite several settings. Replace that behavior with an explicit preset selector:

* Choosing `Reading` applies the Reading preset.
* Choosing `Bilevel` applies the Bilevel preset.
* Any manual change after applying a preset changes its label to `Custom`.
* Choosing a preset again deliberately reapplies it.

That is clearer than treating presets as persistent processing modes when they are actually bundles of starting values. The current preset mutation is in `lege-viewer/src/processing.rs:268–286`.

## Left column: Pages and target

Order these according to the user’s workflow:

1. Scope.
2. Page range, when applicable.
3. Target resolution.
4. Geometry.
5. OCR and related options.
6. General processing modifiers.

The scope should be an explicit segmented control:

* Entire document
* Current page
* Page range

The Viewer already has document and selected-page processing through `ProcessingScope`, including CLI range compaction. See `lege-viewer/src/processing.rs:323–377`. What is missing is Freya’s direct editable page-range workflow.

Layout exclusion should also regain a proper range field. The Viewer currently presents an “Exclude current page” style operation, although its model already supports a `BTreeSet<u32>` of excluded pages. Use the same range parser for both processing scope and layout exclusion.

## Right column: Output and page treatment

Order the settings according to cause and effect:

1. Output container.
2. Text-rendering mode.
3. Layout analysis.
4. Image-region treatment.
5. Binarization method and active value.
6. Effective text encoding.
7. Cover handling.

Avoid cycling several possible values every time a row is clicked. The current `next()` methods are compact to implement, but explicit segmented controls or small menus are much easier to understand.

## Action and help rail

The bottom rail should contain:

* `Original | Preview`.
* A persistent status such as `Temporary preview — not saved`.
* Hover/focus help text.
* `Preview current page`.
* `Process document`, changing to `Cancel processing` while running.

The visual hierarchy should make **Process document** the strongest action, Preview secondary, and Reset tertiary.

---

# 3. Make invalid option combinations impossible

The current dependency normalization is incomplete. It handles Reflow/Layout/Invert and clears halftone in two cases, but several invalid or misleading combinations can remain in state. See `lege-viewer/src/processing.rs:303–320`.

The final dependency matrix should be:

| User action           | Enforced result                                              | UI behavior                                                                  |
| --------------------- | ------------------------------------------------------------ | ---------------------------------------------------------------------------- |
| Reflow enabled        | Layout enabled; Invert disabled; Center/Crop disabled        | Reflow becomes the selected geometry mode                                    |
| Invert enabled        | Layout, Reflow, and Halftone disabled                        | Layout-dependent controls disappear or become unavailable                    |
| Layout disabled       | Reflow and Halftone disabled                                 | Image-region choice disappears; direct CCITT4/JBIG2 choice becomes available |
| Grayscale MRC enabled | Halftone and JPEG compatibility disabled                     | Binarization controls disappear                                              |
| OCR disabled          | EPUB sidecar disabled                                        | OCR quality is represented as Off rather than as a separate stale value      |
| EPUB sidecar enabled  | OCR enabled in Thorough/Best mode                            | OCR selector visibly changes to Thorough                                     |
| OCR Fast selected     | EPUB sidecar disabled                                        | Sidecar checkbox visibly clears                                              |
| Halftone enabled      | Valid only for PDF + Layout + Dithered + Binarized           | Do not show the control outside that combination                             |
| Direct EPUB selected  | Sidecar option is irrelevant                                 | Hide sidecar, cover, and encoding controls that do not apply                 |
| Geometry changed      | Exactly one of Original, Center, Crop, Force Crop, or Reflow | Use one segmented control, not multiple booleans                             |
| Binarization changed  | Exactly one of Adaptive, Threshold, or Heavy                 | Show only the numeric field belonging to the selected method                 |

There are also two source-level issues to resolve:

### “Preserve” and “JPEG” currently produce the same CLI argument

`CoverMode::Preserve` and `CoverMode::Jpeg` both emit `--cover-format jpeg`. See `lege-viewer/src/processing.rs:626–634`.

Either:

* implement a real preserve behavior in the processing CLI, or
* remove `Preserve` from the Viewer.

Do not present two choices that perform the same operation.

### Direct text compression is ignored while Layout is enabled

When Layout is active, `effective_text_format()` derives CCITT4 or JBIG2 from the image-region mode rather than the selected `compression` value. See `lege-viewer/src/processing.rs:288–300`.

Therefore:

* With Layout on, show a read-only summary such as `Effective text encoding: CCITT4`.
* With Layout off, show the editable `CCITT4 | JBIG2` choice.
* Do not display a clickable compression control whose value will be ignored.

## Centralize all state changes

Do not let individual button handlers modify unrelated booleans ad hoc. Every change—mouse, keyboard, preset, loaded settings, or future configuration file—should pass through the same setters or reducer.

A minimally disruptive version can retain the current fields:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrSetting {
    Off,
    Fast,
    Thorough,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryMode {
    Original,
    Center,
    Crop { free_aspect: bool },
    ForceCrop,
    Reflow,
}

impl ProcessingOptions {
    pub fn set_ocr(&mut self, setting: OcrSetting) {
        match setting {
            OcrSetting::Off => {
                self.use_ocr = false;
                self.make_epub_sidecar = false;
            }
            OcrSetting::Fast => {
                self.use_ocr = true;
                self.ocr_mode = OcrMode::Fast;
                self.make_epub_sidecar = false;
            }
            OcrSetting::Thorough => {
                self.use_ocr = true;
                self.ocr_mode = OcrMode::Best;
            }
        }

        self.normalize_dependencies();
    }

    pub fn set_epub_sidecar(&mut self, enabled: bool) {
        self.make_epub_sidecar = enabled;

        if enabled {
            self.use_ocr = true;
            self.ocr_mode = OcrMode::Best;
        }

        self.normalize_dependencies();
    }

    pub fn set_geometry(&mut self, mode: GeometryMode) {
        self.margin_mode = MarginMode::None;
        self.crop_free_aspect = false;
        self.crop_footnotes = false;
        self.reflow = false;

        match mode {
            GeometryMode::Original => {}
            GeometryMode::Center => {
                self.margin_mode = MarginMode::Center;
            }
            GeometryMode::Crop { free_aspect } => {
                self.margin_mode = MarginMode::Crop;
                self.crop_free_aspect = free_aspect;
            }
            GeometryMode::ForceCrop => {
                self.crop_footnotes = true;
            }
            GeometryMode::Reflow => {
                self.reflow = true;
            }
        }

        self.normalize_dependencies();
    }

    pub fn normalize_dependencies(&mut self) {
        if self.make_epub_sidecar {
            self.use_ocr = true;
            self.ocr_mode = OcrMode::Best;
        }

        if !self.use_ocr {
            self.make_epub_sidecar = false;
        } else if self.ocr_mode == OcrMode::Fast {
            self.make_epub_sidecar = false;
        }

        if self.reflow {
            self.layout_analysis = true;
            self.invert = false;
            self.margin_mode = MarginMode::None;
            self.crop_free_aspect = false;
            self.crop_footnotes = false;
        }

        if self.invert {
            self.layout_analysis = false;
            self.reflow = false;
        }

        if self.grayscale {
            self.jpeg_compat = false;
            self.use_jbig2_halftone = false;
        }

        let halftone_is_valid =
            self.output_format == OutputFormat::Pdf
                && self.layout_analysis
                && self.image_processing == ImageProcessing::Dithered
                && !self.grayscale;

        if !halftone_is_valid {
            self.use_jbig2_halftone = false;
        }

        if !self.layout_analysis {
            self.reflow = false;
            self.use_jbig2_halftone = false;
        }

        if self.output_format == OutputFormat::Epub {
            self.make_epub_sidecar = false;
        }
    }
}
```

Longer term, the cleaner model is to replace the related boolean clusters with typed fields:

```rust
ocr: OcrSetting,
geometry: GeometryMode,
rendering: TextRendering,
```

That makes invalid states structurally impossible. It is not necessary to block the first UI pass, provided the centralized setters and tests are added first.

One naming problem also needs review: `crop_footnotes` currently emits `--force-crop`. See `lege-viewer/src/processing.rs:691–693`. Before displaying it, decide whether this is actually a Force Crop mode or a footnote-specific behavior. Do not expose a misleadingly named independent checkbox.

---

# 4. Add a true temporary current-page preview

The preview should not replace the Viewer’s document engine and should not behave like opening a second document.

The current processed-result switch calls `open_document()`, which rebuilds the viewer around the selected path. That resets document-level state and is too heavy for rapid experimentation. See `lege-viewer/src/app.rs:2529–2547`.

## Required user experience

The workflow should be:

1. The user opens Process.

2. The user clicks **Preview current page**.

3. The current page is processed using the current visual settings.

4. The Viewer displays the processed page at the same page position and zoom.

5. A visible badge says:

   **Temporary preview — not saved**

6. `Original | Preview` switches instantly between the original rendered page and the temporary result.

7. Once preview mode has been activated, changing a visual option automatically regenerates it after a short debounce.

8. The existing preview remains visible with an `Updating…` badge until its replacement is ready.

9. Changing a nonvisual option does not regenerate the page.

10. Full document processing always reads from the original input, never from the temporary preview.

The user should never have to wonder whether the source PDF was altered.

## Which options should regenerate preview

Regenerate for:

* Layout analysis.
* Layout exclusion when it affects the current page.
* Original/dithered image regions.
* Grayscale/binarized mode.
* Binarization method and value.
* Invert.
* Center/Crop/Reflow geometry.
* Resolution and dimensions.
* Compatibility/high-quality options when they affect visible image encoding.
* Cover mode only when previewing the cover.

Do not regenerate for:

* Output path.
* Document versus page scope.
* OCR text-layer generation.
* OCR quality.
* EPUB sidecar.
* Output container, unless the selected container genuinely changes visual page treatment.

Create a compact `VisualPreviewOptions` value and hash that, rather than hashing the entire processing configuration.

## Memory model

The Viewer’s search and page caches already use explicit memory accounting. Add a separate category:

```rust
pub enum CacheCategory {
    Compiled,
    Tiles,
    GpuTiles,
    Thumbnails,
    Text,
    Images,
    ProcessPreview,
}
```

Update `MemoryState` and `counter()` accordingly in `lege-viewer/src/document/cache.rs`.

Use these limits:

* One displayed preview.
* At most one in-flight replacement.
* Hard preview budget around **32–48 MiB**.
* Render at target output size, but impose a pixel cap for unexpectedly huge dimensions.
* Drop the previous lease when the replacement is installed.
* Keep the original page in the normal document cache; do not duplicate it in preview state.

For a bilevel page, the ideal canonical preview representation is one bit per pixel. For grayscale, use one byte per pixel. For color, use RGB8. Convert or upload only what the display path requires. A single RGBA page is still acceptable for the first implementation, provided the hard one-page cap exists.

The current `MemoryArbiter::reserve()` accounts for memory but does not reject an allocation. The preview code therefore needs its own pixel/byte limit in addition to the arbiter lease. See `lege-viewer/src/document/cache.rs:37–80`.

## Cancellation and stale-result protection

Rapid option changes will produce overlapping work unless every request has a generation identifier.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewGeneration(u64);

struct ProcessPreviewState {
    generation: PreviewGeneration,
    page: Option<PageIndex>,
    stale: bool,
    running: bool,
    showing_preview: bool,

    control: Option<ProcessingControl>,
    surface: Option<Arc<TileSurface>>,
    lease: Option<MemoryLease>,

    // Keep this outside ProcessingUiState because TempDir is not Clone.
    temp_dir: tempfile::TempDir,
}

impl ProcessPreviewState {
    fn next_generation(&mut self) -> PreviewGeneration {
        self.generation.0 = self.generation.0.wrapping_add(1).max(1);
        self.generation
    }

    fn cancel_in_flight(&mut self) {
        if let Some(control) = self.control.take() {
            control.cancel();
        }
        self.running = false;
    }
}
```

The preview event must carry its generation:

```rust
pub enum PreviewUpdate {
    Started {
        generation: PreviewGeneration,
        page: PageIndex,
    },
    Completed {
        generation: PreviewGeneration,
        page: PageIndex,
        output: PathBuf,
    },
    Failed {
        generation: PreviewGeneration,
        message: String,
    },
}
```

Application logic:

```rust
fn request_process_preview(&mut self) {
    let Some(page) = self.status.current_page else {
        return;
    };

    self.process_preview.cancel_in_flight();

    let generation = self.process_preview.next_generation();
    self.process_preview.page = Some(page);
    self.process_preview.running = true;
    self.process_preview.stale = false;

    let request = self.build_current_page_preview_request(page, generation);

    match processing::start_preview(request, self.event_proxy.clone()) {
        Ok(control) => {
            self.process_preview.control = Some(control);
        }
        Err(error) => {
            self.process_preview.running = false;
            self.processing_ui.detail = error;
        }
    }
}

fn install_process_preview(
    &mut self,
    generation: PreviewGeneration,
    surface: Arc<TileSurface>,
) {
    if generation != self.process_preview.generation {
        // A newer option change superseded this result.
        return;
    }

    let bytes = surface_byte_len(&surface) as u64;
    if bytes > MAX_PROCESS_PREVIEW_BYTES {
        self.processing_ui.detail =
            "Preview exceeds the temporary preview memory limit.".to_owned();
        return;
    }

    let lease = self
        .memory
        .reserve(CacheCategory::ProcessPreview, bytes);

    self.process_preview.surface = Some(surface);
    self.process_preview.lease = Some(lease);
    self.process_preview.running = false;
    self.process_preview.stale = false;
}
```

## First implementation: reuse the existing worker

The safest initial version is:

* Use the existing `lege --gui-worker` bridge.
* Request only the current page through `ProcessingScope::Pages`.
* Write a one-page output into a `tempfile::TempDir`.
* Open/render only the first output page into a preview surface.
* Do not call the main application’s `open_document()`.
* Delete everything automatically when the temporary directory is dropped.

The selected-page CLI infrastructure already exists, and the page range is deliberately placed last in the generated arguments. See `lege-viewer/src/processing.rs:323–377` and `:606–728`.

Use a separate event path such as `ViewerEvent::ProcessPreview`. Do not mix temporary preview completion with the current full-export `ViewerEvent::Processing` messages.

For visual preview transport, a temporary one-page PDF is reasonable even when the final selected format is DjVu or EPUB. The UI should describe this as a **page treatment preview**, not a byte-exact preview of every output container.

## Later optimization: in-process preview

After the interaction is proven, expose an in-process function from the processing core:

```rust
pub fn process_page_preview(
    source: &DocumentSource,
    page: PageIndex,
    options: &VisualPreviewOptions,
    cancel: &AtomicBool,
) -> Result<PreviewRaster, PreviewError>;
```

That avoids subprocess startup, temporary PDF encoding, and rerendering the temporary PDF. It can return bilevel, grayscale, or RGB raster data directly.

This is the right eventual architecture, but it should follow the UI implementation rather than block it.

## Reflow warning

The current Reflow pipeline recomposes the whole document rather than processing pages independently. See `lege-process/pipeline/reflow_pipeline.rs:633–642`.

Consequently, a one-page Reflow preview cannot be represented as exact without processing wider document context. The first release should do one of these:

* disable live preview under Reflow and state why, or
* offer an explicitly labelled **Approximate reflow preview**.

Do not show a local page result and imply that it is identical to the final full-document pagination.

---

# 5. Streamline the toolbar

The current toolbar is divided using fixed widths and fixed X positions. That makes it difficult to adapt when search expands or the window becomes narrow. See `lege-viewer/src/app.rs:206–218`.

Use this order:

```text
Open | Contents | – 100% + | Fit ▾ | Process | Search | View ▾
```

Specific changes:

* Keep **Open** first.
* Put **Contents** next because it changes document navigation.
* Make zoom one compact group: minus, current percentage, plus.
* Combine Fit Width and Fit Page into a `Fit ▾` split button or menu.
* Give **Process** a stable visible location.
* Make Search a compact icon/button until invoked.
* Fold Trim into `View ▾` with appearance and other visual-only settings.
* Rename Appearance to View if that menu now includes Trim and other nonprocessing display controls.

Do not calculate the entire row from hardcoded constants. Use a toolbar layout pass with minimum width, preferred width, and overflow priority.

At narrow widths:

1. Preserve Open, Process, and Search.
2. Compact the zoom group.
3. Move Fit choices into View.
4. Move lower-priority appearance controls into the overflow menu.

## Transient controls must be mutually exclusive

The process drawer itself is persistent and may remain open. Transient UI elements should not overlap or simultaneously own keyboard focus.

Use a single state:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransientUi {
    None,
    Search,
    AppearanceMenu,
    ProcessMenu(ProcessControlId),
    ProcessEdit(ProcessFieldId),
}
```

Opening one transient control closes or commits the previous one. This replaces scattered state such as:

* `search_ui.open`
* `options_visible`
* `processing_ui.open_option`
* `resolution_editing`

The exact fields may remain internally, but one method should enforce ownership:

```rust
fn open_transient_ui(&mut self, next: TransientUi) {
    self.commit_or_cancel_current_editor();
    self.close_all_transient_popups();

    match next {
        TransientUi::None => {}
        TransientUi::Search => self.set_search_open(true),
        TransientUi::AppearanceMenu => self.options_visible = true,
        TransientUi::ProcessMenu(id) => {
            self.processing_ui.open_option = Some(id.index());
        }
        TransientUi::ProcessEdit(field) => {
            self.begin_process_field_edit(field);
        }
    }
}
```

Recommended Escape behavior:

1. Close the current menu or cancel the current editor.
2. Collapse Search.
3. Close Appearance.
4. Only then close the Process drawer.

Only one text field should own IME state at a time: Search, target resolution, page range, or layout-exclusion range.

While full processing is running:

* Disable all option controls.
* Disable Preview.
* Change Process to Cancel.
* Allow the existing document to remain readable.

While preview generation is running:

* Controls remain enabled.
* A new visual change cancels and replaces the old preview job.
* Full Process cancels the preview worker before beginning export.

---

# 6. Collapse Search and distinguish “no matches” from “needs OCR”

Ctrl+F is already implemented in the Viewer, along with F3 navigation and Escape handling. Preserve that code and route it through the new compact search state rather than implementing another search system. See `lege-viewer/src/app.rs:2197–2219` and the search key handling immediately afterward.

## Compact interaction

Default toolbar state:

```text
[ Search icon ]
```

After click or Ctrl+F:

```text
[ Search document…                 ] [↑] [↓] [×]
```

Recommended dimensions:

* Collapsed: 36–40 logical pixels.
* Expanded: approximately 280–320 logical pixels.
* Esc collapses the field and removes active visual search focus.
* Preserve the query internally so Ctrl+F can reopen it.
* Explicit × clears the query and results.

The current search index is already memory-conscious:

* approximately 64 MiB resident text budget,
* temporary-file spill when over the resident limit,
* bounded worker queue retaining only the latest request.

See `lege-viewer/src/text/search.rs:16`, `:61–104`, `:138–183`, and `:212–253`. That part does not need to be redesigned.

## OCR-needed detection

The Viewer currently knows how many pages have been indexed, but it does not track whether any indexed page contains meaningful text. Add that distinction.

Use three states:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchAvailability {
    Indexing,
    Searchable,
    NeedsOcr,
}
```

Track whether each indexed page has at least one non-whitespace, noncontrol character. Store the flag in the page’s index entry so replacement of an indexed page cannot corrupt a simple counter.

```rust
fn has_searchable_text(text: &[u16]) -> bool {
    char::decode_utf16(text.iter().copied())
        .filter_map(Result::ok)
        .any(|ch| !ch.is_whitespace() && !ch.is_control())
}

fn search_availability(&self) -> SearchAvailability {
    if self.search_ui.indexed_pages < self.search_ui.total_pages {
        SearchAvailability::Indexing
    } else if self.search.searchable_page_count() == 0 {
        SearchAvailability::NeedsOcr
    } else {
        SearchAvailability::Searchable
    }
}
```

The messages should be:

* While incomplete: `Indexing text…`
* Complete, with no meaningful text: **`Search needs OCR`**
* Expanded explanation: `No searchable text layer — run OCR to search this document.`
* Searchable text but no result: `No matches`

Do not declare that OCR is needed while indexing is still underway.

Clicking `Search needs OCR` should:

1. Expand the Process drawer.
2. Focus or highlight the OCR row.
3. Leave the actual OCR mode selection to the user.

For a mixed document containing some searchable and some image-only pages, normal search should remain available. A later enhancement can report that some pages may need OCR, but that is not necessary for this pass.

---

# 7. Restore the exact Freya tooltips from one shared source

The Viewer should not receive manually rewritten approximations of the Freya tooltips.

The canonical strings are already in:

`lege-process/language_service/en/gui_text.json:46–79`

Move or expose that catalog so both GUIs consume the same source. Good options are:

* a small shared `lege-gui-text` crate, or
* a generated Rust module produced from the same JSON at build time.

Add a test that verifies:

* every processing control has a tooltip key,
* no key used by Viewer is absent from the catalog,
* the legacy values remain byte-for-byte equal.

The current Viewer text renderer only allocates one line of height for each text paint. Long tooltips would be clipped because `draw_text()` sets the Cosmic Text buffer height to exactly one `line_height`. See `lege-viewer/src/ui.rs:98–128`.

Add a multiline text path with:

* 360–440 pixel maximum tooltip width,
* word wrapping,
* approximately 8–10 pixels internal padding,
* a 400–500 ms hover delay,
* window-edge repositioning,
* immediate display when a control receives keyboard focus.

Because several exact tooltips are long, a useful addition is a persistent help strip in the drawer footer. Hovering or focusing a control displays its exact tooltip both there and, after the delay, in the floating tooltip. Disabled controls should still expose their tooltip. Any dependency explanation should appear as a separate muted line rather than modifying the exact legacy text.

These are the exact existing strings, including current capitalization and punctuation:

```text
add_file_or_folder = "Accepts PDF files, image folders, or image ZIPs."
add_file = "Opens PDFs or JP2/JPEG image ZIPs."
add_folder = "Loads a book from a folder of page images."
output_directory = "Choose where processed files are saved. Outputs default to this folder."
base_format = "1bit encoding - JBIG2 or CCITT4"
image_output_type = "Original color with CCITT4 encoding, or dithered with JBIG2 encoding"
layout_detection = "GPU accelerated layout detection preserves image areas from binarization; disable if no images in document."
layout_exclusion_pages = "Pages to process without layout detection, e.g. 1, 3, 7-10."
heavy_model = "Heavy Sauvola AI model (ONNX) for degraded pages. benefits from layout detection."
inverted_colors = "For digitally created documents with dark background and light text. Will convert to white background, black text"
jpeg_compatibility = "Maximum-compatibility encoders: JPEG for image regions, CCITT4 for binarized text (instead of JPEG2000/JBIG2). Binarized mode only."
ocr_text_layer = "Generate a new OCR text layer instead of preserving an existing one."
ocr_fast = "Fast, suitable for search layer."
ocr_thorough = "Slower, best for EPUB or when the text layer needs to be right."
make_epub_also = "Create a sidecar EPUB using thorough OCR."
jbig2_halftone = "Experimental spec standard halftone dithering; lower file sizes and lower resolution."
high_quality_output = "Use higher-quality image encoding. Keep unchecked for outputs intended for e-ink readers."
cover_format_no_cover = "treat first page same as others; image format affects all non-binarized images"
cover_format_dithered = "CCITT4 text with dithered image regions where enabled (global)"
cover_format_original = "keep original color images (global)"
page_range = "Specify page range (e.g., 1-10, 5-20)"
target_height = "Enter height for proportional output, or height x width / height width, e.g. 1200, 1200x800, or 1200 800."
target_width = "Optional fixed output width in pixels."
width_proportional = "Keep output width proportional to the page aspect ratio."
sauvola_window_size = "Local analysis window size - larger: smoother, smaller: more detail-sensitive"
sauvola_k_factor = "Contrast sensitivity (0.0-1.0) - lower: more text preserved, higher: cleaner backgrounds. Benefits from layout detection."
sauvola_r = "Variance scaling - lower: less aggressive, higher: stronger adaptation to noise"
threshold_value = "One cutoff for the whole page: pixels with luma below this value (0-255) become black, the rest white."
margin_centering = "Centers content area within page dimensions"
margin_crop_resize = "Crops margins to content bounds and resizes to specified dimensions"
crop_free_aspect = "Does not preserve aspect ratio; crops to content bounds and resizes to specified dimensions"
reflow = "Re-paginates text into a clean single-column layout for the target device, reflowing words and lines from the original pages"
output_format = "PDF or DjVu"
```

There is one necessary exception: the Viewer supports direct EPUB output, while the old `output_format` tooltip says only `PDF or DjVu`. See `lege-viewer/src/processing.rs:35–65`.

Do not silently alter the legacy string while claiming exact restoration. Retain it under its existing key, and add a Viewer-specific key such as:

```text
output_format_with_epub = "PDF, DjVu, or EPUB"
```

Likewise, do not attach `cover_format_original` until the Viewer’s “Preserve” behavior is actually distinct from JPEG.

---

# 8. File-by-file implementation plan

## `lege-viewer/src/chrome/layout.rs`

* Add `processing_drawer`.
* Accept processing visibility and persisted height ratio.
* Shift sidebar, canvas, and scrollbars below the drawer.
* Add layout tests for:

  * drawer closed,
  * drawer open at 20%, 28%, and 36%,
  * sidebar open and closed,
  * short and narrow windows.

## `lege-viewer/src/app.rs`

* Remove `ProcessingTab`.
* Remove floating panel dimensions and corner resizing.
* Replace the three tab renderers with the two-column drawer renderer.
* Add a bottom-edge splitter.
* Preserve reading anchor when drawer geometry changes.
* Replace scattered transient popup/edit state with one focus/overlay owner.
* Collapse Search by default.
* Add `SearchAvailability`.
* Add `ProcessPreviewState`.
* Keep ephemeral preview separate from `switch_processing_result()`.
* Distinguish `Preview` from a completed `Processed output`.

## `lege-viewer/src/processing.rs`

* Add central setters or `ProcessingAction`.
* Complete dependency normalization.
* Add direct page-range parsing for scope and layout exclusion.
* Resolve `CoverMode::Preserve`.
* Resolve or rename `crop_footnotes`.
* Add `VisualPreviewOptions`.
* Add a preview worker purpose separate from full export.
* Add dependency tests for every row in the matrix above.

## `lege-viewer/src/event.rs`

Add a separate preview event:

```rust
ViewerEvent::ProcessPreview(PreviewUpdate)
```

Do not reuse the full processing completion event.

## `lege-viewer/src/document/cache.rs`

* Add `CacheCategory::ProcessPreview`.
* Add accounting for it.
* Keep a separate hard preview budget because `MemoryArbiter` only tracks usage.

## `lege-viewer/src/text/search.rs`

* Add per-page `searchable` state.
* Maintain `searchable_page_count`.
* Preserve the existing resident-memory spill strategy.

## `lege-viewer/src/ui.rs`

* Add multiline wrapped text.
* Add tooltip background, padding, and edge correction.
* Ensure keyboard-focused controls can display the same help text.

## Shared text catalog

* Make Freya’s `gui_text.json` available to Viewer.
* Add exact-value and tooltip-coverage tests.
* Add separate strings only where Viewer functionality genuinely differs.

---

# 9. Implementation order

## Phase 1: State correctness

Do this before rearranging the controls.

* Centralize processing actions.
* Complete dependency rules.
* Fix Cover semantics.
* Reconcile Force Crop naming.
* Add tests.
* Make presets explicit and expose `Custom`.

This prevents the new UI from reproducing the current hidden-state problems.

## Phase 2: Docked one-screen processing drawer

* Extend `AppLayout`.
* Remove tabs.
* Build the Freya-derived two-column composition.
* Add the action/help rail.
* Add bottom-edge resize.
* Preserve page anchor.

At this point the Viewer should already feel substantially more complete, even before preview exists.

## Phase 3: Toolbar, focus, Search, and tooltips

* Introduce responsive toolbar layout.
* Move Trim into View.
* Collapse Search.
* Add OCR-needed search state.
* Restore exact tooltips.
* Add multiline wrapping and keyboard-focus help.

## Phase 4: Subprocess current-page preview

* Temporary directory.
* Current-page processing request.
* Generation IDs and cancellation.
* One-page cache and memory cap.
* Original/Preview toggle.
* Debounced live updates after preview has been activated.

This is the safest preview implementation and should be shipped before attempting a deeper processing-core refactor.

## Phase 5: In-process preview optimization

* Extract page-level processing from `lege-process`.
* Return compact raster data directly.
* Avoid temp PDF encode/decode.
* Reuse Viewer document/render infrastructure where doing so does not reduce processing fidelity.

## Phase 6: Replacement decision

Make Viewer the normal interface for one-document processing once all acceptance criteria below pass.

Keep Freya available as Batch Processing until Viewer has, or deliberately delegates:

* folder input,
* ZIP input,
* multiple-input queue,
* output-directory selection,
* queue status,
* logs,
* queue cancellation and retry.

---

# Definition of done

The Viewer should not replace Freya for the single-document workflow until all of these are true:

### Layout

* Process opens as a docked top drawer rather than covering the document.
* Default height is near 25–30% and user-resizable.
* Opening and closing preserves the current reading location.
* All applicable controls are visible without switching among tabs.
* At small dimensions, the drawer scrolls rather than falling back to three screens.

### State

* Geometry is exactly one selected mode.
* Binarization is exactly one selected method.
* OCR is Off, Fast, or Thorough rather than a checkbox plus stale quality value.
* Every invalid combination is automatically normalized.
* Preset changes and manual changes use the same state path.
* The generated CLI arguments cannot contain contradictory geometry modes.

### Preview

* The original document engine remains loaded and unchanged.
* `Original | Preview` switches immediately.
* Every preview clearly says `Temporary preview — not saved`.
* Repeated option changes cancel stale jobs.
* A stale completion can never replace a newer preview.
* One hundred repeated preview changes do not produce steadily increasing memory use.
* Full processing always starts from the original source.
* Reflow preview is explicitly unavailable or marked approximate.

### Search

* Search occupies only an icon-sized area until clicked or invoked with Ctrl+F.
* Ctrl+F focuses it.
* Indexing, no matches, and no OCR layer are distinct states.
* A scanned document eventually displays `Search needs OCR`.
* A searchable document with no hit displays `No matches`.
* A mixed document remains searchable.

### Tooltips

* Every applicable Freya tooltip is restored from the same canonical text source.
* Long text wraps and remains inside the application window.
* Tooltips work for mouse hover and keyboard focus.
* Viewer-specific differences use intentionally separate tooltip keys.

### Replacement boundary

* Viewer is the default for processing the currently open PDF.
* Freya remains available and clearly named for batch/folder/ZIP processing until those capabilities have another home.

---

# Agent-ready implementation brief

> Replace the Viewer’s floating, tabbed processing panel with a full-width docked processing drawer directly below the toolbar. The drawer must reserve layout space rather than cover the document, default to approximately 28% of the available height, support bottom-edge resizing, persist its height ratio, and preserve the current reading anchor whenever its geometry changes.
>
> Remove the Output, Recognition, and Page tabs. Present all applicable processing controls simultaneously in a Freya-derived two-column composition. The left side should contain processing scope, page range, target resolution, geometry, OCR, EPUB sidecar, Invert, quality, and compatibility controls. The right side should contain output format, text rendering, layout analysis and exclusion range, image-region treatment, binarization, effective compression, and cover treatment. Use explicit segmented choices rather than click-to-cycle rows.
>
> Centralize all option mutations. Enforce OCR/sidecar, Layout/Reflow/Invert, Grayscale/compatibility, Halftone validity, geometry exclusivity, and binarization exclusivity in one reducer or set of setters. No button handler may leave invalid state behind. Fix the current Cover Preserve/JPEG equivalence and reconcile the `crop_footnotes` field with its emitted `--force-crop` argument.
>
> Add an ephemeral current-page preview. It must process only the current page into a temporary directory, retain the original document engine, and display the temporary result through an Original/Preview segmented control. The UI must always state `Temporary preview — not saved`. Once preview mode has been invoked, visual option changes should regenerate it after a short debounce. Use generation IDs, cancellation, and stale-result rejection. Retain at most the displayed preview and one in-flight replacement, with a 32–48 MiB hard cap and MemoryArbiter accounting. Full export must always use the original source, never the preview. Reflow must be labelled approximate or excluded from single-page live preview.
>
> Reorganize the toolbar as Open, Contents, Zoom, Fit, Process, Search, and View. Move Trim into View. Replace fixed toolbar X positions with responsive item layout. Search must normally be icon-sized, expand on click or Ctrl+F, and distinguish Indexing, No matches, and Search needs OCR. Only declare OCR necessary after all pages are indexed and no meaningful text exists.
>
> Restore Freya’s processing tooltips from the existing `language_service/en/gui_text.json` rather than rewriting them. Add multiline tooltip rendering because the current Viewer text path is effectively single-line. Show the same help on keyboard focus. Preserve exact legacy strings, while adding separate Viewer-specific keys where functionality differs, particularly direct EPUB output.
>
> This work targets single-open-document parity. Do not add Freya’s queue, image-folder, ZIP, or batch-output workflow to the top processing drawer. Keep Freya as Batch Processing until those capabilities are deliberately migrated into a separate batch workspace.

This is primarily a source-level review rather than a pixel-tuned build review. The architecture and state problems are clear from the implementation; the exact default drawer ratio should be adjusted visually after the first docked version is running.
