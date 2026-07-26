# Adding hayro to the differential harness

Written 2026-07-22 for the agent building the multi-renderer harness. hayro was
moved to `tools/hayro/` in this session; everything below was verified from that
location.

---

## 1. What it is and why it earns a seat

hayro is a PDF renderer written in Rust (`vello_cpu` rasteriser), dual-licensed
Apache-2.0 / MIT. Version 0.7.0, upstream commit `9daca6c6`.

It matters more than the other controls for one reason: **this corpus is hayro's
own test corpus.** The sweep reads its PDFs from
`renderer-corpus/hayro-source/hayro-tests/downloads/{pdfjs,pdfbox,pdfium,corpus}/`,
which is a second, full checkout of the same project. Those files are hayro's
regression suite, so hayro is tuned against exactly the pages we grade on. Treat
that as a bias to be aware of, not a reason to weight it higher.

> Do not touch `renderer-corpus/hayro-source/`. It is 1.3 GB and every sweep
> path points into it. `tools/hayro/` is a separate checkout with its own
> `.git`; it is the one to build and update.

---

## 2. Build

```bash
cd tools/hayro
cargo build --release --example render -p hayro
# -> tools/hayro/target/release/examples/render      (~22 s from warm, ~8.9 MB)
```

It is its own cargo workspace. The main workspace is `members = ["crates/*"]`,
so nothing under `tools/` is pulled in — `cargo build --release --workspace` at
the repo root was re-verified clean after the move. `tools/*/target/` is already
gitignored; **`tools/hayro/` itself should be added to `.gitignore`** alongside
whatever you do for `mupdf/`, `poppler-*/`, `ghostscript-*/` and `pdf.js/` —
these are third-party checkouts with their own history, not vendored source. I
left that call to you since those four are yours.

---

## 3. CLI contract

```
render <file.pdf> <output-dir> <scale>
```

- Writes `<output-dir>/rendered_<page-index>.png`, zero-based, one file per
  page. It creates the directory.
- `<scale>` matches our sweep's meaning: `2.0` gives 2 device px per PDF point.
  Pixel size is `floor(page_dimension * scale)`, same flooring the sweep uses.
- Background is opaque **white** (`render_pdf` sets `bg_color: WHITE`); the
  library default is transparent, so if you call the library directly rather
  than the example, set it explicitly or the ink metric will be meaningless.
- Exit status is not meaningful — it `unwrap()`s on a bad file. **Run it under a
  timeout and treat "no PNG produced" as the failure signal**, the way the
  controller already treats a dead worker.

### The font resolver is not optional

`examples/render.rs` installs an `InterpreterSettings.font_resolver` that loads
faces from `tools/hayro/hayro-tests/assets/` (17 files, 4.5 MB — Liberation for
the standard 14, Noto CJK for `AdobeGB1`/`CNS1`/`Japan1`/`Korea1` fallbacks).
Without it hayro falls back to built-in data and non-embedded fonts render
differently. If you integrate as a library, port that resolver across verbatim —
it is the difference between comparing renderers and comparing font tables.

Worth knowing: that CID fallback path (`AdobeGB1 -> NotoSansCJKsc`) is the exact
feature *we* are missing — see `issue18466`, whose `FZSSK--GBK1-0` is Adobe-GB1
with no embedded program.

---

## 4. Two ways to wire it in

**(a) Shell out to the example.** Simplest, matches how you will have to drive
mupdf/ghostscript/poppler anyway. One process per file, read back the PNGs.
Cost: process startup and a PNG encode/decode round trip per page.

**(b) Link it as a library.** hayro is Rust, so `tools/pdfium-diff` can depend on
it by path and render in-process to a `Pixmap`, skipping PNG entirely:

```rust
let pdf = hayro::hayro_syntax::Pdf::new(bytes)?;
let cache = hayro::RenderCache::new();
let pixmap = hayro::render(&pdf.pages()[i], &cache, &settings, &render_settings);
```

Faster and gives exact control of pixel dimensions, but it pulls `vello_cpu` and
`skrifa` into the tool's dependency graph. Both are fine — the tool is already
out of workspace and never links into the engine — but it does mean a hayro
version bump can break the harness build, whereas (a) degrades to "that control
is missing for this run".

**Recommendation: (a) first.** Get all five renderers behind one uniform
"produce a raster for (file, page, scale)" interface, then optimise hayro to (b)
only if the process overhead actually shows up in sweep wall-clock. A uniform
interface is worth more than hayro being fast, because four of the five controls
can only ever be shelled out.

---

## 5. Geometry — the thing that will bite

Every control must produce **the same pixel dimensions** or the comparison is
noise. hayro uses `floor(dim * scale)`; check each of the others (ghostscript's
`-r` is DPI, not a scale factor: `-r144` is our 2.0 at 72 dpi nominal). Either
force identical dimensions per renderer or resample once, centrally, and record
that you did.

Do not compare a resampled raster with an unresampled one and report the delta
as a rendering difference. That mistake is cheap to make and expensive to
believe.

---

## 6. The vote system — and its one hard limit

Record, per (file, page): each renderer's raster, and pairwise metrics between
all of them, not just against PDFium. Suggested CSV, one row per (file, page):

```
file,page,renderer,ok,width,height,ink,sha256
```

plus a derived pairwise table, so a disagreement can be clustered ("A and B
agree, C and D agree, the two groups differ") rather than flattened to a
distance from PDFium.

### Counting votes does not establish correctness

This is the part to take seriously, because it already caught me out today.

`pdfjs/issue6707` fills a large band with `/R22 cs 0.300049 scn`. **PDFium,
hayro and poppler all paint it neutral grey 77. pdf.js and our engine paint it
light beige.** Three renderers against two — and the three are wrong.

`/R22` is `[/Separation /PANTONE#20467#20U#201 /PANTONE#20467#20U#201 14 0 R]`:
the alternate-space slot repeats the *colorant name* instead of naming a colour
space, so the file is malformed. Its tint transform (FunctionType 2, `C0 [1 1
1]`, `C1 [0.90699 0.825366 0.677137]`, `N 1`) at `t = 0.300049` evaluates to RGB
`(247.9, 241.9, 230.3)`. Our render measures `(247.9, 241.9, 229.9)` — the
arithmetic settles it. The other three fall back to using the tint as DeviceGray
when the alternate will not resolve, and grey 77 is simply `0.300049 * 255`.

Three independent codebases agreed because they share one obvious shortcut on
one malformed construct. **Majority is a heuristic for where to look; the file
is the authority.** Build the harness so that a cluster disagreement *raises* a
page for adjudication and never auto-labels one side correct.

Two further caveats:

- **Okular and qpdfview are both poppler. They are one vote, not two.** Whatever
  UI you drive, record the *engine*, not the application.
- Weight nothing by "closeness to PDFium". PDFium is the port's target, not an
  oracle of correctness — on `issue6707` it also silently omits the header photo
  and logo that hayro, pdf.js and we all draw.

---

## 7. Adjudicated pages — use them as harness fixtures

These are already settled with evidence; a harness that disagrees with the
verdict below has a bug in it. They make good end-to-end tests of the voting and
reporting logic, because each has a *different* disagreement shape.

| Page | Verdict | Shape |
|---|---|---|
| `pdfjs/issue6707` | we + pdf.js correct | 3-vs-2 split, majority wrong (above) |
| `pdfbox/5260` | we correct | PDFium floods the page black on a damaged file |
| `pdfbox/5657`, `pdfjs/issue16782` | we correct | PDFium renders blank; genuine non-opaque JPX alpha |
| `Daughters of Tunis` | we correct | out-of-range Indexed index; PDFium reads OOB as black |
| `Aksan, Ottomans and Europeans` p0 | PDFium differs, not our bug | bad JBIG2 scan; PDFium cleans it |
| `pdfjs/issue18466` | fixed 2026-07-22 (`53bd6d5`) | was ours: `scn` with a name and no `cs` |

---

## 8. Where the written-up context lives

- Adjudication method and the four controls: memory note
  `adjudicating-pdfium-disagreements`.
- Per-page findings and the settled list: memory notes `post-sweep9-fixes`,
  `sweep9-analysis`, `sweep7-analysis`.
- Sweep mechanics, including **never run the diff tool or rebuild while a sweep
  is in flight** (it execs a fresh worker binary per chunk, and CPU contention
  causes false 180 s timeouts): `sweep9-analysis`.
