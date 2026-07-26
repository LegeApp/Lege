# Sweep 6 handoff (written 2026-07-21, Windows session)

You're rebooting to Linux to run sweep 6. This note bridges the gap because the
**Windows session's memories will NOT auto-load on the Linux side** (Claude keys
memory to the project path, and the Linux project path differs). Read the Windows
memories first — pointers below — then run sweep 6.

---

## 1. Where the Windows-side memories are (read these first)

Windows path:
`C:\Users\dk\.claude\projects\D--Rust-projects-pdfium-port-plan\memory\`

From Linux, the Windows C: partition is mounted under `/media/dk/<VOLUME-ID>/`.
Last boot the volume id was `141606191605FC8C`, so the memories were at:
`/media/dk/141606191605FC8C/Users/dk/.claude/projects/D--Rust-projects-pdfium-port-plan/memory/`

The volume id can change per boot. To find it reliably:
```
find /media/dk /mnt -path '*D--Rust-projects-pdfium-port-plan/memory/MEMORY.md' 2>/dev/null
```

Files there (most important last):
- `MEMORY.md` — the index.
- `pdf-renderer-state-pointer.md` — repo layout, prior production pass.
- `sweep4-review-shading-fix.md` — sweep-4 triage + the 6 render fixes and their evidence.
- `sweep5-analysis.md` — **sweep-5 triage + the CJK/font fixes + the full residual triage.**

(The Linux-side memories that WILL auto-load are older; they predate all of today's
work. Trust the Windows ones for current state.)

---

## 2. Current code state

All work is committed to the `pdf-renderer` git repo (no remote). Working tree clean.
`HEAD = 8d8ff53`. Today's commits (newest first):

- `8d8ff53` fix(content): Indexed image with a CIE (Lab) base pre-converts its palette
  — Pearson/Gladstone covers went from solid black to correct (inkΔ ~0.8 → ~0.0001).
- `c41201b` feat(pdfium-diff): substitute non-embedded fonts against system faces
  — **the diff tool now enables system fonts on all grading paths** (see §4 baseline note).
- `95b16a8` feat(font): cross-platform system font paths + Windows/macOS CJK families
  — the CJK fix. Chinese pages: blank → matching PDFium (inkΔ ~0.13 → ~0.013, ~50 pages).
- `0ee40d2` feat(render): shading-pattern text fills and axial/radial /Background
- `18fd8ab` feat(content/render): honor a shading's /BBox clip
- `76f69bc` fix(render-cpu): defensive geometry clamps against corrupt input (4778 green flood)
- `5cab73f` docs(pdfium-diff): clarify the vestigial RenderRequest.annotations field
- `7fc70c1` fix(structure): TIFF predictor 2 for sub-byte and 16-bit depths (issue6071 class)
- `079ead0` fix(content): axial/radial shadings honor their /ColorSpace (rosiesmenu gray-flood)

The diff tool binary (`tools/pdfium-diff/target/release/`) was rebuilt on Windows with
all of these. **On Linux, run-sweep.sh rebuilds it (`cargo build --release`) before the
sweep, so a fresh Linux build picks everything up** — no action needed, just make sure the
build succeeds (watch for the `jp2lam` path dep: `../../Lege-ecosystem/lege-codecs/jp2lam`).

---

## 3. The sweep 6 command

Run from the diff-tool dir, pass a FRESH, empty workdir (the tool resumes by file|page key
from `<workdir>/pdfium-diff-out/results.csv`, so a non-empty dir would skip everything):

```
cd /mnt/Samsung980_1TB/Rust-projects/pdfium-port-plan/pdf-renderer/tools/pdfium-diff
./run-sweep.sh '' 2.0 /mnt/Samsung980_1TB/Rust-projects/pdfium-port-plan/sweep6
```

- arg1 `''` = auto-find `libpdfium.so` (it's at the repo root: `.../pdfium-port-plan/libpdfium.so`).
  If auto-find fails, pass it explicitly as arg1.
- arg2 `2.0` = scale (same as every prior sweep — keep it for comparability).
- arg3 = the fresh workdir. Do NOT run without it, or it writes to the CWD and may resume
  a stale run.
- `CORPUS_ROOT` defaults to `/mnt/Samsung980_1TB`; the 3 roots resolve under it
  (`Pol was right again`, `to-sort`, `renderer-corpus`). Note sweep 5 only reached 2 of 3
  roots before you cancelled it — sweep 6 should cover all three.

### PREREQUISITE new this session: CJK fonts must be installed on the Linux host
The diff tool now substitutes non-embedded fonts (including CJK) against installed system
faces. Windows ships SimSun/MS Gothic/etc.; a bare Linux box may not have CJK fonts, in
which case the ~50-page Chinese cluster will render **blank again** and look like a
regression. Check and, if empty, install:
```
fc-list :lang=zh | head        # should list some CJK fonts
sudo apt install fonts-noto-cjk # if empty (GB_FONTS preference list includes "Noto Sans CJK SC")
```

---

## 4. Baseline shifts — sweep 6 is NOT directly comparable to sweeps 4/5

Two intentional baseline changes moved since sweep 5's binary:
1. **System fonts ON** (c41201b): every non-embedded font now resolves a real installed
   face instead of a bundled metric-compatible stand-in — matching PDFium and real viewers.
   Broadly an improvement, but it shifts many pages. DS82 (embedded) nudged 0.005→0.009.
2. The 6 render fixes from `sweep4-review-shading-fix.md` (shading colorspace/BBox/text/bg,
   TIFF predictor, defensive clamps) — sweep 5's binary had only the first three (colorspace,
   predictor, annot); sweep 6 has all.

Consequence: several sweep-5 "residuals" are already-fixed ghosts. Re-grade before assuming
a page is a live bug (I did this for ~10 files — see §5).

---

## 5. Triage conclusions to save you time (from sweep-5 deep-dive)

Do NOT waste time re-investigating these — they're settled:

- **Daughters of Tunis** — PDFium bug, WE ARE CORRECT. `/Decode[0 255]` gives an out-of-range
  Indexed index (undefined per spec); PDFium reads OOB→black and inverts the page, we clamp
  to hival→white. Confirmed: SumatraPDF and Acrobat both show our (correct) black-on-white.
- **Aksan "Ottomans and Europeans" p0** — PDFium difference, not our bug. The JBIG2 image
  genuinely IS bad-scan static; SumatraPDF shows the same static. PDFium cleans/rejects it.
- **Already fixed** (were live in sweep 5, fixed since): Rana Mitter, Bannockburn, Rotberg.
- **Metric artifact**: Micropolitics, Eugenia Lean, From_Central_Asia render only ~3–6%
  darker than PDFium, but the diff tool's THRESHOLD ink metric flips near-white regions from
  "white" to "inked", inflating inkΔ to look like ~0.4 when the luminance gap is ~0.02.

### The one real open lead worth a future pass
A **systematic ~3–6% darker rendering of grayscale (DeviceGray) images** vs PDFium — no
colorspace conversion involved, so it's likely an image **minification / resampling tone**
difference (larger on minified than magnified images). Affects a class of image-heavy pages.
Connects to the deferred workstream-H "minification weight quality" residual. Subtle, not
catastrophic, but a genuine class fix if pursued.

### Still-open genuine bugs (codec, out of the renderer)
- JBIG2 decoder occasionally emits noise on some scans (jbig2enc-rust, separate crate).
- JPX residual classes live in jp2lam (separate crate; see its HANDOFF-remaining-jpx-decode.md).

---

## 6. Suggested first moves on Linux
1. Read `sweep5-analysis.md` (Windows memory) — it has the full residual map.
2. Verify CJK fonts (`fc-list :lang=zh`).
3. Run sweep 6 (command in §3). It takes hours; it's resumable if interrupted (same workdir).
4. When it lands, triage the NEW numbers — they'll be clean (all fixes + system fonts),
   unlike sweep 5. Expect the tail to be much shorter than sweep 5's.
