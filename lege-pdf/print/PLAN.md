# lege-pdf-print implementation plan

Plan for a printing submodule under `lege-pdf/`. Nothing in the ecosystem
prints today: a repo-wide search for `cups`, `winspool`, `PrintDlg` and
`StartDoc` returns nothing, and `lege-pdf/render` has no notion of a print
job, a sheet, a margin, or an imposition.

```text
lege-pdf/
├── read/     lege-pdf-read  — document intake, rasters, page export
├── render/   the renderer workspace (24 crates, AGPL, frozen surface contract)
├── write/    lege-pdf-write — typed append-only emitter
└── print/    lege-pdf-print — this plan
```

---

## 1. The distinction that decides the whole design

"Printing" is two unrelated products, and conflating them is the main way
this goes wrong.

**A. Office printing** — put this document on the printer on my desk.
Wants: a spooler, fit-to-page, margins, N-up, duplex, page ranges, copies,
collation, orientation. Colour is a solved problem: hand the driver sRGB and
it does the rest. This is achievable, is almost entirely *not* renderer work,
and is what a GUI and a CLI actually need.

**B. Prepress** — produce a file a commercial printer or RIP will accept.
Wants: TrimBox/BleedBox, CMYK separations, an output ICC profile and
rendering intent, overprint simulation, halftone screening, PDF/X
conformance.

The renderer scoping pass established that B is blocked on several
large, frozen-contract-level changes:

- `OutputFormat` has exactly two variants, `Rgba8PremultipliedSrgb` and
  `Gray8` (`render/crates/pdf-render-api/src/lib.rs:29-44`). No CMYK, no
  16-bit, no separations.
- Colour conversion is one-directional *into* sRGB by frozen policy
  (`render/crates/pdf-render-api/src/contract.rs:70-90`). There is no
  RGB→CMYK path, no output profile, and the ICC CMYK input path is hardcoded
  to `A2B0`/perceptual with no black-point compensation
  (`render/crates/pdf-color/src/icc.rs:191-199`).
- Overprint is *detected* into a `PageFeatures::OVERPRINT` preflight flag and
  explicitly not simulated (`render/crates/pdf-content/src/interpret.rs:311-313`).

**This plan builds A and explicitly defers B.** Not merely on effort grounds:
B is largely a PDF-to-PDF transformation, so when it is wanted, the right
home is `lege-pdf/write` emitting PDF/X — *not* teaching the display
rasterizer to produce CMYK. Section 7 records that so the deferral is a
decision rather than an omission.

### The design rule

`lege-pdf-print` sits **above** `lege-pdf-read` and owns everything
print-specific. The renderer stays a display rasterizer. Exactly one change
is needed inside the renderer for phase A — page-box parsing — and it is
additive.

---

## 2. The shortcut worth taking first

On Linux and macOS, CUPS accepts PDF as a native spool format: `lp -d
printer -o ... file.pdf` hands the file to the printer's own filter chain,
which is usually better at the driver's colour and halftoning than anything
we would write. Most office print jobs need no rasterization from us at all.

So the pipeline branches:

```text
                      ┌─ pass-through: spool the original PDF bytes
  PrintJob ──────────►│  (CUPS, unmodified page geometry, no imposition)
                      │
                      └─ raster: compose sheets ourselves, spool bitmaps
                         (Windows always; anywhere we impose or N-up)
```

Pass-through applies when the job needs no page composition — no N-up, no
booklet, no scaling we cannot express as a CUPS option. Windows has no
native PDF spool path, so it is always raster. This branch is worth building
first: it makes "print this document" work on two of three platforms with a
few hundred lines, and it keeps us honest about when rasterizing is actually
required.

---

## 3. Crate shape

```text
lege-pdf/print/
├── Cargo.toml          lege-pdf-print, workspace member
├── PLAN.md             this file
└── src/
    ├── lib.rs          PrintJob, PrintOptions, the public entry points
    ├── paper.rs        PaperSize, Orientation, Margins, hardware margins
    ├── layout.rs       pure imposition maths — the heart of the crate
    ├── compose.rs      Placement -> sheet raster, via lege-pdf-read
    ├── spool/
    │   ├── mod.rs      trait Spooler, capability discovery
    │   ├── cups.rs     Linux/macOS
    │   ├── windows.rs  winspool
    │   └── file.rs     write sheets to disk; the headless/test backend
    └── preview.rs      sheet -> PNG for a GUI print preview
```

`layout.rs` is pure: geometry in, geometry out, no I/O, no OS, no renderer.
That is deliberate — it is where every bug that produces a wrong-looking
printout will live, and it is the only part that can be exhaustively tested
without a printer.

---

## 4. Phases

### Phase 0 — page boxes in the renderer (prerequisite, small)

`MediaBox` and `CropBox` are parsed, inherited, and clamped
(`render/crates/pdf-document/src/pages.rs:656-726`). **`BleedBox`,
`TrimBox` and `ArtBox` are not parsed anywhere** — a repo-wide search
returns zero hits.

Add them alongside the existing two, with the standard inheritance and
defaulting rules (each defaults to `CropBox`, itself defaulting to
`MediaBox`), carry them on `PageRef`/`PageBounds`, and surface them through
`lege-pdf-read`'s `PageGeometry`. Additive; breaks no contract.

Phase A only needs this so "scale to the trim area" is expressible. Phase B
cannot start without it. Do it now regardless — it is the cheapest item on
the list and unblocks the most.

*Effort: small.* Mirrors code that already exists, plus an IR field.

### Phase 1 — the layout core (medium, and the part that matters)

Pure imposition maths. No rendering.

```rust
pub struct PrintJob {
    pub pages: Vec<PageGeometry>,   // source page sizes, in points
    pub options: PrintOptions,
}

pub struct PrintOptions {
    pub paper: PaperSize,           // named or explicit, in points
    pub orientation: Orientation,   // Portrait | Landscape | Auto
    pub margins: Margins,           // user margins, in points
    pub scaling: Scaling,
    pub n_up: NUp,                  // 1, 2, 4, 6, 9, 16, or Booklet
    pub duplex: Duplex,             // None | LongEdge | ShortEdge
    pub range: PageRange,
    pub copies: u16,
    pub collate: bool,
    pub reverse: bool,
    pub source_box: PageBoxKind,    // Crop | Media | Trim | Bleed | Art
}

pub enum Scaling {
    ActualSize,                     // 1:1; clip what does not fit
    FitToPage,                      // scale up or down, preserve aspect
    ShrinkToFit,                    // scale down only — the sane default
    FillPage,                       // cover the imageable area, crop overflow
    Percent(f64),
}

/// One source page placed on one sheet.
pub struct Placement {
    pub source_page: u32,
    pub transform: Matrix,          // source points -> sheet points
    pub clip: Rect,                 // imageable area on the sheet
}

pub struct Sheet {
    pub index: u32,
    pub paper: PaperSize,
    pub side: Side,                 // Front | Back, for duplex
    pub placements: Vec<Placement>,
}

pub fn impose(job: &PrintJob, device: &DeviceCapabilities) -> Result<Vec<Sheet>, PrintError>;
```

`impose` is the whole phase. Everything downstream just executes what it
returns.

Points to get right, each of which is a test:

- **Hardware margins.** Printers cannot print to the paper edge. The
  imageable area is the paper minus the *hardware* margin, and user margins
  are measured from the paper edge but clamped to it. Query the real values
  from the driver; when unavailable, assume a conservative 6.35 mm (¼ inch)
  and let the caller override. Silently ignoring this is the classic cause
  of clipped printouts.
- **`Orientation::Auto`** picks whichever rotation wastes less paper —
  compare the fit scale both ways, take the larger.
- **Duplex back-side geometry.** Long-edge binding flips the back page about
  the long axis; short-edge about the short one. Getting this wrong prints
  every other page upside down and is invisible in a single-sided test, so
  it needs an explicit test.
- **Booklet imposition.** Sheet *n* of an *N*-page booklet carries pages in
  the order `(N-2n, 2n+1)` on the front and `(2n+2, N-2n-1)` on the back,
  with `N` rounded up to a multiple of 4 and the remainder blank. Worth
  writing as its own function with a table-driven test at N = 4, 8, 12, and
  a non-multiple like 6.
- **Mixed page sizes.** A document may have pages of differing sizes; each
  is fitted independently against the same sheet.

*Effort: medium.* Little code, but the invariants are fiddly and the
failure modes are visible on paper and nowhere else.

### Phase 2 — sheet composition (small–medium)

Turn `Sheet` into pixels using `lege-pdf-read`.

A sheet may carry several source pages, so this composes: allocate one sheet
raster at the device resolution, render each placement, blit it into place.
`lege-pdf-read`'s `DeviceCrop` and `RasterProduct` already express
"render this page at this size into this sub-rectangle", and `export.rs`
already owns the DPI-to-pixels arithmetic.

The memory question decides the shape here. An A4 sheet at 1200 DPI in RGB
is ~390 MB, which is not an allocation to make casually:

- Cap the composition resolution (default 300 DPI, configurable to 600).
  Above that, the driver's own scaling is generally indistinguishable at
  arm's length and costs nothing.
- Compose in horizontal **bands** rather than whole sheets, and hand each
  band to the spooler as it is finished. Both target spooler APIs accept
  banded delivery, and it turns a 390 MB allocation into a bounded one.
- Grayscale when the job is mono: one byte per pixel instead of three, which
  `RasterFormat::Gray8` already provides.

*Effort: small–medium.* The rendering primitives all exist; banding is the
only real work.

### Phase 3 — spooler backends (medium, platform-shaped)

```rust
pub trait Spooler: Send + Sync {
    fn printers(&self) -> Result<Vec<PrinterInfo>, PrintError>;
    fn capabilities(&self, printer: &PrinterId) -> Result<DeviceCapabilities, PrintError>;
    fn submit(&self, job: SpoolJob<'_>) -> Result<JobId, PrintError>;
    fn status(&self, job: JobId) -> Result<JobStatus, PrintError>;
    fn cancel(&self, job: JobId) -> Result<(), PrintError>;
}
```

- **`file.rs` first.** Writes sheets to disk as PNG or as a composed PDF, and
  records the job it was given. It is the headless backend, the CI backend,
  and how every layout test asserts without a printer attached. Build it
  before either real backend.
- **`cups.rs`** — Linux and macOS. Two options: link `libcups` via FFI, or
  drive the `lp`/`lpstat` CLI. Recommend **starting with the CLI**: no
  native dependency, no build-time headers, no AGPL-adjacent linking
  question, and it covers pass-through printing completely. Move to
  `libcups` only when job-status polling or capability discovery
  (`lpoptions -l` parsing is grim) proves insufficient.
- **`windows.rs`** — `winspool.drv` via the `windows` crate:
  `OpenPrinter`/`StartDocPrinter`/`StartPagePrinter`, and `StretchDIBits`
  onto the printer DC for the raster path. Note `CLAUDE.md`'s standing rule
  that `lege`'s `windows` features are kept to what is actually called — a
  print backend needs `Win32_Graphics_Gdi` and
  `Win32_Graphics_Printing` added deliberately, and only in this crate.

*Effort: medium*, most of it Windows. The trait keeps that contained.

### Phase 4 — frontends (small each)

- `lege-pdf` agent CLI: `print <file> [--printer] [--range] [--n-up] [--duplex]`,
  plus `--to-file` for the `file.rs` backend.
- GUI print dialog with a live preview, using `preview.rs` (which is just
  phase 2 aimed at a PNG rather than a spooler).

---

## 5. Testing

The layout core is pure, so it gets property tests rather than golden files:

- No placement ever extends outside its sheet's imageable area.
- N-up placements on one sheet never overlap.
- `FitToPage` and `ShrinkToFit` preserve aspect ratio to within a float
  epsilon; `ShrinkToFit` never scales above 1.0.
- `impose` emits exactly `ceil(selected_pages / per_sheet)` sheets, doubled
  and rounded for duplex.
- Booklet page order round-trips: folding the emitted sheet sequence
  reproduces 1..N in order.

Composition gets pixel tests through the `file.rs` backend against the
existing render corpus. Spooler backends get a dry-run assertion that the
right options reached the right API, which is as far as CI can go without
hardware.

---

## 6. Effort summary

| Phase | Scope | Effort |
|---|---|---|
| 0 | Trim/Bleed/Art parsing in `pdf-document` | small |
| 1 | `layout.rs` imposition core | medium |
| 2 | Sheet composition + banding | small–medium |
| 3a | `file.rs` backend | small |
| 3b | CUPS via `lp`, incl. pass-through | small–medium |
| 3c | Windows `winspool` + GDI | medium |
| 4 | CLI and GUI frontends | small each |

Phases 0–2 plus 3a are a complete, testable, printer-free deliverable.
Phase 3b makes it real on Linux and macOS.

---

## 7. Deferred: prepress, and why

Recorded so the deferral is a decision, not a gap.

| Want | Blocked on | Effort |
|---|---|---|
| CMYK / separations output | New `OutputFormat`, new compositing path, an RGB→CMYK policy. Breaks the frozen surface contract. | large |
| Output ICC profile, rendering intent, soft-proofing | Only input-side ICC exists; needs a full output transform subsystem. | large |
| Overprint simulation | Detection exists; the per-plate knockout maths does not, and is entangled with CMYK output. | medium, gated on the above |
| Halftone screening | Bilevel dithering in `pdf-postprocess` is a model, but per-plate angles and frequencies are new. | medium |
| PDF/X conformance | OutputIntent emission, font embedding rules, box requirements. | medium |

**When this is wanted, do not start with CMYK rasterization.** A commercial
printer wants a *file*, not a bitmap, and the file is a PDF/X — so the work
belongs in `lege-pdf/write`, which already emits OutputIntent dictionaries
and embedded fonts, with phase 0's box parsing supplying Trim and Bleed. The
renderer would only be involved for on-screen soft-proofing, and that is the
last piece to build, not the first.
