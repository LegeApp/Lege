# Handoff: remaining `jp2lam` JPEG 2000 decode gaps

**Audience:** an agent continuing the JPX decode work in `jp2lam`.
**Context:** `jp2lam` is the JPEG 2000 codec behind the `pdf-renderer` port of
PDFium. Its decoder feeds `/JPXDecode` images. A differential sweep of
`pdf-renderer` against PDFium found ~610 corpus pages rendering **blank**
because their JPX image failed to decode and the error was swallowed. That has
been root-caused and mostly fixed; this document hands off the residual tail.

As of 2026-07-18 the blank-corpus census stands at **791/822 JPX streams
decoding (96.2%)**. The remaining **31** streams fall into four classes, below,
ranked by frequency. None is a silent blank anymore — `pdf-renderer`'s B3
diagnostics now flag every failed codec draw, so these show up as counted
`silent-blank(codec)` rows in the sweep, not clean passes.

## 2026-07-19 implementation update

- Sections 1 and 2 are implemented. Tier-2 now models per-resolution precinct
  grids, per-precinct tag trees/code-block routing, all five progression orders,
  and SOP/EPH. The degenerate irreversible 9/7 one-sample phase is fixed before
  relaxing the decomposition bound.
- The Armenia obj8 repro was not precinct-partitioned: after SOP/EPH parsing it
  exposed code-block style `0x3e`. Annex B.10.7.2 terminated-segment lengths and
  Annex D RESET, TERMALL, VSC, predictable termination, and segmentation
  symbols are now implemented. The actual 9/7 output differs from OpenJPEG by
  max 1 (mean 0.00053).
- The Section 3 diagnosis was incomplete. Its extracted 150-tile PDF stream is
  structurally complete according to OpenJPEG, and zero-byte contributions are
  valid with Annex D.4.1 synthetic end bytes. That syntax is now accepted, but
  the repro then exposes a corrupt 1,349-byte tile tail. A salvage prototype
  corrupted the entire affected 128×128 tile, so strict failure remains.
- Section 4 remains unresolved. The same strict no-tail policy is retained
  because both measured salvage attempts produced visible corruption.

## 2026-07-22 (later) update — Section 2 RESOLVED, and sub-8-bit precision added

- **Section 2 was a real DWT bug, and this is it.** `forward_97_vertical_in_place` /
  `inverse_97_vertical_in_place` scaled a *one-row* resolution by INV_K / K.
  The pair round-trips through our own encoder, so every test passed, but it
  does not match conformant streams: OpenJPEG returns from both
  `opj_dwt_encode_and_deinterleave_v_real` and `opj_v8dwt_decode` on height 1
  without touching the data, and **our own horizontal 1-D path already treated
  the case as an identity** — the two disagreed. This is exactly the drift §2
  and §5-trap-1 recorded when the decomposition bound was lifted: a codestream
  that signals more levels than its dimensions need passes through several 1x1
  levels, so its DC comes out multiplied by K^n while the AC stays
  pixel-perfect. On the 884x1344 / 17-resolution repro a fit against OpenJPEG
  gave `ours = 1.002 * opj + 194` — a pure DC offset, K^5 = 2.8158.
  With the identity restored the dimension-derived bound has nothing left to
  reject; `max_dwt_decompositions` is gone and the only limit is the spec's own
  `NL <= 32`. Repro (Howard R. Turner, *Science in Medieval Islam* obj7):
  max abs diff 5, mean 0.147, 3062 of 1,188,096 px over 2 — the 9/7 float floor
  at 11 real levels rather than the usual 5, diffuse and edge-concentrated.
  End-to-end 0.2424 silent-blank(codec) -> 0.0014.
- **Sub-8-bit sample precision (1/2/4-bit) is implemented.** The SIZ gate
  accepted only 8..=16 bits; bitonal scans are 1-bit and were the largest
  remaining census class. Nothing in the pipeline needed a special case.
  Verified bit-exact vs OpenJPEG (max abs diff 0) on pdfbox/3246 obj3.
  **NOTE for future verification work:** the CLI's PNG writers used to clamp
  raw samples to 0..255, which wrote a 1-bit page as 0/1 and *clipped* 16-bit
  components instead of scaling them. They now scale by component precision.
- **Renderer-side caveat (not a codec issue):** pdf-renderer widens sub-8-bit
  samples with PDFium's `src << (8 - prec)`, not full-range scaling, because
  that is what PDFium's `CJpx_Decoder::Decode` does and it is what the corpus
  grades against (exactly 0.00000 on pdfbox/3246 and 4326; full-range leaves
  them at 0.81-0.99). Do not "fix" that to match OpenJPEG's writers.

## 2026-07-22 update — Sections 3 and 4 RESOLVED (the §4 diagnosis below was WRONG)

- **Section 4 was NOT a tile-part assembly bug.** That diagnosis is disproven:
  the Prussian obj15 repro has 35 tiles with an identical 6-part/TNsot=5
  structure, yet only tile 16 failed — an assembly/ordering fault would break
  all 35. The real cause is that the Tier-2 packet-header bit reader
  (`PacketBioReader`) was missing the terminating **byte-realignment** (ISO/IEC
  15444-1 Annex B.10.1 = OpenJPEG `opj_bio_inalign`): when a packet header's
  last consumed byte is `0xFF`, the following bit-stuff byte belongs to the
  header and must be consumed before the body. Tile 16's R3C0 header ends
  `0xFF 0x00`; the old reader sliced the body one byte early at the stuff byte
  and desynced every later packet of that tile — which surfaced as the "leftover
  bytes after the packet sequence" error, and (under naive tail-tolerance) as
  the exact documented 256-row corruption stripe. Fix: `PacketBioReader::inalign()`
  called after every packet-header parse (`src/decode/t2.rs`). Verified: the
  corrupt rows 768-1023 go from max 255 / 75134 px >30 to max 3 / 0 px >30;
  overall max 4 (MCT float floor); end-to-end all pages `degraded=0`.
- **Section 3's named repros were the same `inalign` desync, not truncation.**
  Profit-Over-People obj4 now decodes bit-exact (5/3 max 0); the Imre Nagy
  streams decode within the MCT float floor. **Genuine** stream truncation is
  kept strict — a raw-truncated stream makes OpenJPEG itself error ("Tile part
  length inconsistent with stream length"), so a clean failure is correct there.
- **Also landed 2026-07-22:** 4:2:0 sYCC chroma-subsampling (narrowly gated;
  per-component Annex B.2 geometry + `sycc420_to_rgb`-matching upsample), and
  colr METH 3/4 (any-ICC / vendor colour, inferring the space from the ICC data
  space or component count; ambiguous 2-component rejected cleanly) + palette
  channel-count reconciliation. All verified bit-exact vs OpenJPEG.

---

## 0. Orientation & environment

- **Repo:** `D:\Rust-projects\jp2lam` on Windows; the same disk boots Linux
  where the path is `/mnt/Samsung980_1TB/Rust-projects/jp2lam`. Dual-boot, one
  drive: **`D:\` on Windows == `/mnt/Samsung980_1TB` on Linux.** The corpus
  under `D:\Pol was right again\…` is the Linux `/mnt/Samsung980_1TB/Pol was
  right again/…` in the sweep CSV.
- **Build / test:** `cargo test` (247+ lib tests; a few `#[ignore]`d ones need
  archive.org sample files). CLI decoder: `cargo build --features cli` →
  `target/debug/jp2lam.exe decode <in.jp2> <out.png>`.
- **Rust 1.95 pin** (edition 2024). The crate is also a path-dependency of
  `pdf-renderer`; after changing it, `cargo build --workspace` there must stay
  green (it does today).
- **Oracle = OpenJPEG.** `opj_decompress -i x.jp2 -o x.png` and
  `opj_dump -i x.jp2` are on PATH. **Every fix must be verified bit-exact vs
  OpenJPEG**: 5/3 reversible transform → max abs diff **0**; 9/7 irreversible →
  max abs diff **≤ ~2** (float rounding). A larger diff means the fix is wrong —
  see the two traps in §5.

### The census / extraction tool (your main instrument)

`pdf-renderer/crates/pdf-cli/examples/jpxcensus.rs` extracts every `/JPXDecode`
stream from a list of PDFs, decodes each with `jp2lam`, and tallies normalized
error signatures. Set `JPXDUMPDIR` to also dump one representative failing
stream per signature to disk (as `.jp2`) for `opj_dump`/`opj_decompress`.

```bash
# From D:/Rust-projects/pdfium-port-plan/pdf-renderer
# 1. Build the blank-file list (Windows paths) from the sweep CSV:
awk -F, 'NR>1 && $NF=="" && $(NF-2)<0.001 && $(NF-1)>0.3 {print $1}' \
  oracle-sweep-2026-07-18/pdfium-diff-out/results.csv \
  | sed 's/"//g; s|/mnt/Samsung980_1TB|D:|' | sort -u > /tmp/blankwin.txt

# 2. Run the census (2nd arg = max files; omit for all 424). ~250s per ~34 files.
DUMP="<some-writable-dir>"; mkdir -p "$DUMP"
JPXDUMPDIR="$DUMP" cargo run -q -p pdf-cli --example jpxcensus -- /tmp/blankwin.txt 60
# Dumped streams land in $DUMP/failjpx_<sig>.jp2 — opj_dump / opj_decompress them.
```

Extract a *specific* file's streams by putting one path in the list file.
`jpxcensus` pre-filters on a direct `/Filter /JPXDecode` name; a handful of
indirect-filter streams are missed (acceptable for surveying classes).

### End-to-end oracle (whole page vs PDFium)

`pdf-renderer/tools/pdfium-diff` (a separate crate, needs `libpdfium`;
`D:\Rust-projects\pdfium-port-plan\pdfium.dll` is present):
```bash
cd tools/pdfium-diff
cargo run -- D:/Rust-projects/pdfium-port-plan/pdfium.dll 2.0 "<one-file.pdf>"
# writes pdfium-diff-out/results.csv; summary line prints suspect/silent-blanks/degraded.
```
Use this to confirm a decode fix actually clears the page against PDFium. **Do
not run the full corpus sweep** (11k files, ~2 h) — the owner runs that; single
files are seconds.

### What was already fixed (do not redo)
`ftyp` brand (`jpx ` major / `jp2 ` or `jpx ` compat), QCC per-component
quantization, non-LRCP progression, TNsot-advisory multi-tile-part, raw J2K
codestreams (SOC at offset 0), CMYK (EnumCS 12, 4-comp + MCT-on-first-3),
`floor→ceil` decomposition bound, PLT/PLM/TLM length markers ignored, and
`pclr`+`cmap` palette (added in parallel — verify it still holds if you touch
color). See `pdf-renderer/DEFERRED.md` corpus item 1 for the full writeup.

---

## 1. Precincts / SOP / EPH — ~10 files (LARGEST remaining class)

- **Error:** `unsupported packet syntax: precinct partitioning, SOP markers,
  and EPH markers are not implemented`
- **Repro:** `The-Conversion-Of-Armenia-To-The-Christian-Faith.pdf` obj8
  (`D:\Pol was right again\…\Armenia…`). OpenJPEG decodes it.
- **Reject site:** `src/j2k/decode_markers.rs` ~line 475 (`validate_decoder_scope`):
  `if cod.uses_precincts || cod.sop_markers || cod.eph_markers { … }`. The COD
  `Scod` bits are already parsed (`uses_precincts`, `sop_markers`, `eph_markers`
  at lines 335-337; `precinct_sizes: Vec<PrecinctSize>` is carried on
  `CodSegment`).
- **Why it's the big one:** the Tier-2 packet iterator in `src/decode/t2.rs`
  currently assumes **P=1** (one precinct per resolution) — `packet_axis_order`
  iterates only (layer, resolution, component). Precincts add a fourth axis: a
  packet exists per (layer, resolution, component, **precinct**), and the number
  of precincts per resolution is derived from the `PPx/PPy` sizes and the
  resolution's coordinates. This is the load-bearing change.
- **Recommended approach:**
  1. In `t2.rs`, generalize the packet odometer to include the precinct index
     as the innermost spatial axis (Annex B.12 order within a resolution).
     Compute per-resolution precinct counts from `precinct_sizes` and the
     tile-component reference grid (Annex B.6): `numprecincts_x =
     ceil(trx1/2^PPx) - floor(trx0/2^PPx)` etc.
  2. Route each packet's code-block contributions to the code-blocks of its
     precinct only (precincts partition each subband into rectangles; a
     code-block belongs to exactly one precinct). Today `band_lookup` maps
     (component,resolution)→bands; you'll need band→precinct→code-block routing.
  3. **SOP** (`0xFF91`, before a packet) and **EPH** (`0xFF92`, after the packet
     header) are optional delimiters — detect and skip them at the right points
     in `push_tile_part` / the packet header reader. They don't change the data,
     only frame it; but if `sop_markers`/`eph_markers` are set you MUST consume
     them or the byte cursor desyncs.
  4. Port against OpenJPEG's `opj_t2_decode_packets` / `opj_pi_*` for the
     iteration order and precinct math — that's the reference the corpus was
     produced against.
- **Verification:** the Armenia streams; expect 9/7 max diff ≤ 2. Add a focused
  unit test with a precinct-partitioned fixture (`opj_compress -c [64,64]` makes
  one). Re-run the census; this should clear ~10 files.

---

## 2. Degenerate 1-sample DWT split — ~8 files  [RESOLVED 2026-07-22 — root cause was a K/INV_K scaling on a one-row resolution in the *vertical* pass; see the update near the top. The "relaxing the bound drifts" note below is accurate but the cause is now fixed.]

- **Error:** `COD decomposition levels N exceed DWT limit N for image
  dimensions WxH`
- **Repro:** `Neo-Confucianism-in-History.pdf` obj9 — a **176×16** tile
  declaring **5** decomposition levels (numresolutions=6). OpenJPEG decodes it.
- **Reject site:** `src/j2k/decode_markers.rs` `max_dwt_decompositions`
  (~line 577) returns `ceil(log2(min_dim))`; for min_dim=16 that's 4, so 5 is
  rejected at line 462-465.
- **Diagnosis (IMPORTANT — this is a real DWT bug, not just a bound):** the
  extra level splits an axis that is already down to **1 sample** (16 → 8 → 4 →
  2 → 1, then a 5th level on the height axis operates on a 1-sample band). I
  measured that if you merely relax the bound to `ceil(log2(max_dim))` and let
  it decode, `jp2lam`'s **inverse DWT drifts ~16/255 (mean), max 25** vs
  OpenJPEG on that axis — i.e. it silently mis-decodes. So the bound is
  currently kept strict *on purpose* (a clean B3-flagged failure beats a wrong
  image — see §5).
- **Recommended approach:** fix the inverse DWT (`src/dwt/`, `irrev97.rs` /
  `rev53.rs` and the 2D driver `inverse_97_2d_in_place_at`) to correctly handle
  a lifting step on a length-1 (and length-0) axis — it must be a pass-through
  that matches OpenJPEG's `opj_dwt_decode` boundary handling. **Then** relax
  `max_dwt_decompositions` to `ceil(log2(max_dim))` (the larger dimension) and
  re-verify the 176×16 stream is now max diff ≤ 2. Do the DWT fix first; the
  bound relaxation without it re-introduces the drift.
- **Verification:** decode the dumped `failjpx_…decompositionlevels….jp2` and
  diff vs OpenJPEG — must drop from mean ~16 to ≤ ~1.

---

## 3. Zero-length code-block contribution (truncated streams) — ~6 files

- **Error:** `non-empty code-block contribution has zero byte length`
- **Repro:** was seen on `Stalin…` (that hit is an `.epub`, not a PDF — find a
  PDF trigger by re-running the census with `JPXDUMPDIR` set and grabbing the
  dumped `failjpx_…zerobytelength….jp2`).
- **Reject site:** `src/decode/t2.rs` ~line 481-485, in the packet-header
  contribution reader: a code-block signals inclusion but decodes a body length
  of 0, which the code treats as malformed.
- **Diagnosis:** these are **truncated** streams (the file is cut off; the
  packet header promises data the body doesn't contain). PDFium/OpenJPEG salvage
  the partial image (they render what decoded). `jp2lam` errors → blank.
- **Recommended approach (careful — correctness risk):** treat a truncated tail
  as *end of data*, not a hard error: stop Tier-2 at the first packet that runs
  past the available bytes, keep the coefficients decoded so far, and let
  reconstruction produce the partial image (this mirrors how `pdf-structure`
  already salvages truncated Flate content streams). The catch is that the
  reconstruct path currently expects a complete packet sequence (`finish()` at
  t2.rs ~line 225 errors on `ended early`). You'll need reconstruct to tolerate
  missing contributions (zero-fill those code-blocks). **Verify the partial
  output matches OpenJPEG's partial output**, not just "it doesn't error" — a
  partial decode that diverges from OpenJPEG is a silent quality regression.
- **Priority:** lower than §1 — truncation is inherently lossy and rare.

---

## 4. `TPsot==TNsot` non-conformant tile-parts — ~2 files  [RESOLVED 2026-07-22 — the assembly-bug diagnosis in this section is WRONG; real cause was a missing `inalign` byte-realignment. See the 2026-07-22 update near the top.]

- **Error:** `tile-part contains data after the tile's packet sequence`
- **Repro:** `Prussian-Line-Infantry-1792-1815.pdf` obj15 (1208×1638, 3-comp,
  csty=0, no precincts). OpenJPEG decodes it, emitting
  `[WARNING] Non conformant codestream TPsot==TNsot.`
- **Reject site:** `src/decode/t2.rs` `push_tile_part` ~line 171-176 — after the
  full packet sequence is read, leftover bytes trigger the error.
- **Diagnosis & TRAP:** the leftover bytes are **not** padding — they are a
  genuine tile-part chunk that our tile-part assembly misplaces because the
  stream sets `TPsot == TNsot` (an off-by-one tile-part index). I tried the
  naive fix (break out of the loop, ignore the tail); it decodes but **corrupts
  a 256-row code-block band** (rows 768-1023 of 1638; measured 1.4% of pixels
  >30 off, a visible stripe). That is a silent mis-decode — worse than the clean
  blank — so the strict failure is kept deliberately (see the NOTE comment now
  in `push_tile_part`).
- **Recommended approach:** fix the **tile-part boundary / ordering** for the
  non-conformant `TPsot==TNsot` case in `src/decode/mod.rs`
  `tile_part_indices_by_tile` (the TNsot-advisory logic) so every tile-part's
  payload is assigned to the correct tile and concatenated in the right order —
  then there is no "trailing" data. Compare against OpenJPEG's tolerant
  tile-part handling (`opj_j2k_read_sot` / the `TPsot==TNsot` warning path).
  Verify the Prussian stream reaches max diff ≤ 2 with no corrupted band.

---

## 5. Two traps — do NOT "fix" these by loosening a check

Both were tried this session and produce **silent mis-decodes**, which are
worse than the current clean (B3-flagged) failures:

1. **Degenerate DWT (§2):** relaxing `max_dwt_decompositions` without fixing the
   1-sample DWT lifting → ~16/255 drift. [RESOLVED 2026-07-22: the 1-sample bug
   was the vertical pass scaling by K/INV_K where OpenJPEG is an identity. Fixed;
   the bound is now just the spec's `NL <= 32`. See the update at top.]
2. **Tile-part tail (§4):** ignoring bytes after the packet sequence → a
   256-row corruption band. [RESOLVED 2026-07-22: the tail was a symptom of a
   missing `inalign` byte-realignment, not misplaced assembly; fixing `inalign`
   removes the tail entirely, so no tolerance is needed. See the update at top.]

**Rule:** a JPX fix is only correct if the decoded image is bit-close to
OpenJPEG (max diff ≤ ~2 for 9/7, 0 for 5/3) on the *actual failing stream*.
"It stopped erroring" is not sufficient — always diff the pixels. When in doubt,
prefer the clean failure: `pdf-renderer` B3 surfaces it as a visible
`silent-blank(codec)`/`degraded` row, so it is not lost, and a blank beats a
plausibly-wrong image for the archival (Lege) use case.

---

## 6. Quick reference

| Class | ~N | Error signature (substring) | Reject site | Repro obj |
|---|---:|---|---|---|
| Precincts/SOP/EPH | 10 | `precinct partitioning, SOP markers` | `decode_markers.rs:~475` | Armenia obj8 |
| Degenerate DWT | 8 | `decomposition levels … exceed DWT limit` | `decode_markers.rs:~462` + `dwt/` | Neo-Confucianism obj9 |
| Zero-length contribution | 6 | `zero byte length` | `t2.rs:~481` | (dump via census) |
| TPsot==TNsot tail | 2 | `data after the tile's packet sequence` | `t2.rs:~171` + `mod.rs` assembly | Prussian obj15 |

After any change: `cargo test` in `jp2lam`, then `cargo build --workspace` in
`pdf-renderer`, then re-run `jpxcensus` on `/tmp/blankwin.txt` to confirm the
failed count dropped and nothing regressed. Update `pdf-renderer/DEFERRED.md`
corpus item 1 with what moved.

---

## 7. Post-precinct census (2026-07-19, from pdf-renderer) — new residuals

After the precinct/SOP/EPH + degenerate-DWT round landed, a fresh full-corpus
`jpxcensus` (451 blank docs, 1224 JPX streams) decodes **1207 (98.6%)**. The 17
residuals, for this session:

- **Zero-length contribution ×9** (Stalin) and **data-after-packets ×4** (Imre
  Nagy, Prussian) — the deliberate truncation strict-failures in §3/§4 above.
- **tile-part QCC override ×2** — `unsupported JPEG 2000 feature: tile-part QCC
  component quantization override (marker 0xff5d)`. Repro: *Helena K. Szepe -
  Painters and Patrons in Venetian Documents* obj10. Main-header QCC is already
  handled (`CodestreamHeader::quant_for`); this is the tile-part variant —
  route a tile-header QCC into the same per-component quantization override for
  that tile. Likely small.
- **`box extends past input` ×1** — repro *Frederick Thomas Jane - Imperial
  Russian Navy* obj13. NOT a bare SOC codestream (the raw-J2K path added earlier
  doesn't catch it), so probably a genuinely truncated/odd JP2 box header.
  Inspect with `opj_dump`; decide tolerate-vs-reject.
- **`packet header ended before requested bit` ×1** — repro *Profit Over
  People; Neoliberalism, Global Order* obj4. A truncation variant surfacing in
  the packet-header bit reader; same salvage-vs-strict question as §3.

All 17 are B3-visible in pdf-renderer (never silent). Use the §0 census/extract
workflow to dump each repro stream and diff against OpenJPEG.
