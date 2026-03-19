# `sym_unify` Handoff Summary

This note summarizes the recent `sym_unify` work so development can resume without relying on prior chat context.

## Current State

- `sym_unify` replaced legacy `sym_collapse` as the active quality-first lossy symbol path.
- Legacy collapse code has been removed from the active codebase:
  - `src/jbig2collapse.rs`
  - `src/jbig2collapse_context.rs`
  - `src/bin/profile_sym_collapse.rs`
- Shared collapse-era logic was split into:
  - `src/jbig2classify.rs`
  - `src/jbig2context.rs`
- `sym_refine` is no longer an active mode.
- A lessons-learned document was added:
  - `REFINEMENT_LESSONS.md`

## Core Milestones Reached

### 1. `sym_unify` quality baseline

`sym_unify` was rebuilt as:
- non-destructive
- post-harvest
- representative-based
- quality-first

Key ideas:
- keep CC extraction as a firehose
- do not denoise away small marks
- split fragile marks out of unification
- build conservative classes
- choose dense representatives
- unify only safe members

This solved the earlier substitution problems that made `sym_collapse` unusable.

### 2. CC firehose change

Connected-component analysis was changed so the active path no longer drops or grid-splits tiny CCs during extraction.

Impact:
- periods and `i/j` dots reappeared
- this became the stable foundation for later `sym_unify` tuning

### 3. Adaptive live-anchor dictionary

The biggest compression improvement came from adding adaptive live representative reuse during ingestion in `src/jbig2enc.rs`.

Behavior:
- stable non-fragile symbols become live anchors
- new glyphs try live anchors before the broader symbol lookup
- final `apply_symbol_unify()` still runs afterward

This fixed the bad scaling where symbol count kept exploding as page count increased.

## Recent Architecture Work

### Narrow refinement architecture

A paper-style narrow refinement extension was added on top of `sym_unify`:
- accepted classes can now produce `refinement_subclusters`
- recurring stable subcluster members can be marked `needs_refinement=true`
- text-region refinement is allowed under `sym_unify` if instances actually require it

Important:
- this does **not** revive the old broad family-refine behavior
- refinement is driven only by `sym_unify`-identified subclusters

Files touched:
- `src/jbig2unify.rs`
- `src/jbig2enc.rs`
- `src/jbig2structs.rs`

Result:
- architecture is present
- economics were poor on the current `sahib` corpus
- conservative defaults remain in place, effectively keeping refinement dormant unless it truly pays

## Benchmark Story

### Strong recent clean baseline

Best important recent clean baseline:
- `benchmark_1773768370`

Key result on current 20-page text-only-ish `sahib` set:
- `symbol`: `244.2 KB`
- `sym_unify`: `193.7 KB`
- `sym_unify` is about `20.7%` smaller

### Why this matters

At this stage:
- no obvious errant substitutions in reviewed outputs
- no missing punctuation/dot regressions from the earlier collapse path
- local dictionaries are already tiny compared with vanilla `symbol`

### Dictionary/generic breakdown

For the current `sym_unify` baseline on 20 pages:
- globals: about `15.8 KB`
- local dicts: about `2.2 KB`
- text region: about `115.8 KB`
- generic region: about `59.1 KB`

Interpretation:
- dictionary size is no longer the dominant cost
- remaining size pressure is largely the residual generic tail

## What Was Tried Recently

### 1. Narrow refinement pass

Tried again in a controlled way:
- only stable recurring subclusters
- only under `sym_unify`
- strict score/gain gates

Observed outcomes:
- very conservative settings found almost nothing useful
- looser settings admitted some subclusters but slightly worsened size
- “heaviest recurring only” settings ended up admitting nothing on this corpus

Conclusion:
- the refinement architecture is good to keep
- refinement is not the next optimization lever on this corpus

### 2. Planner-side rescue of page-local one-offs

Goal:
- rescue text-like one-offs back into local symbol dictionaries instead of residual generic

Observed outcomes:
- generic bytes dropped sharply
- local dictionaries exploded
- total file size worsened

Example runs:
- `benchmark_1773768762`
- `benchmark_1773768813`

Conclusion:
- the direction is informative
- but broad local rescue is not economically good enough
- that experimental rescue logic was reverted back to baseline behavior

## Diagnostics and Profiling

### Diagnostics

Heavy diagnostics are gated behind:
- `JBIG2_DIAGNOSTICS=1`

Without the flag:
- benchmark speed returns to the clean path

Useful current diagnostics include:
- class build summary
- weak-core vs low-gain rejects
- anchor remap counts
- residual anchor scans
- local/global dictionary byte splits

### Flamegraph

Profiling was successfully used after enabling `perf`.

Main findings:
- `compute_symbol_signature()` and hole counting were hot
- some symbol matching paths were hot

One speed optimization that stayed:
- skip expensive hole counting for tiny/sparse symbols that cannot plausibly contain useful counters

This improved time without changing file size meaningfully.

## Current Design Interpretation

The current code now has:
- class formation
- dense core representatives
- border members
- adaptive live anchors
- subcluster candidate/refinement architecture

It still does **not** have a full paper-style representative hierarchy or economic planner for refinement dictionaries. That remains future work.

## Best Current Understanding Of The Next File-Size Frontier

Because production symbol-mode input will be text-only:
- residual generic regions should be treated as missed text-symbol opportunities
- not as mixed-content/image safety buckets

That means the next likely fruitful areas are:

1. Better live-anchor timing and readiness
- more text-like leftovers should attach to proven anchors earlier

2. Better second-pass attachment of text-like residuals
- but without loosening visual safety rules enough to risk substitutions

3. Better residual diagnostics for text
- identify exactly why a text-like symbol missed:
  - overlap
  - signature
  - compare
  - outside-ink
  - score

4. Better text-region efficiency
- especially by reducing false negatives in symbol attachment

## Areas That Are Probably Not The Next Lever

- reviving standalone `sym_refine`
- broadening refinement aggressively
- broad planner-side rescue of local one-offs
- more work on mixed-content/image residual handling for this corpus

## Current Important Commits Before This Uncommitted State

- `786314d` checkpoint: solid sym_unify baseline
- `d32817f` checkpoint: adaptive sym_unify dictionary
- `68a282c` checkpoint: tune adaptive sym_unify anchors
- `d66ad67` tune: gate encoder diagnostics behind env flag

## Current Uncommitted Findings

Recent work after this handoff added:

- exact JBIG2-style dictionary byte accounting in diagnostics/gain estimation
- a narrow score-slack rescue path for generic-bound symbols
- a planner-side remap to already-kept page-local symbols, using the same conservative anchor gates
- removal of punctuation/fragile-mark special-casing from active `sym_unify` anchor/class logic

Most useful current measurements:

- `benchmark_1773791226`
- `sahib/10p`
- `symbol`: `134.2 KB`
- `sym_unify`: `105.1 KB`
- `sym_unify` is about `21.7%` smaller
- `sym_unify` breakdown:
  - globals `9.5 KB`
  - local dicts `1.4 KB`
  - text `57.6 KB`
  - generic `36.1 KB`

- `benchmark_1773791226`
- `sahib/50p`
- `symbol`: `545.7 KB`
- `sym_unify`: `429.5 KB`
- `sym_unify` is about `21.3%` smaller
- `sym_unify` breakdown:
  - globals `29.2 KB`
  - local dicts `2.1 KB`
  - text `296.9 KB`
  - generic `99.3 KB`

- earlier reference point before punctuation cleanup:
- `benchmark_1773790591`
- `sahib/10p`
- `sym_unify 10p`: `105.2 KB`
- `sym_unify 50p`: `430.2 KB`

Important interpretation from the current decision log:

- planner rescues to global anchors are still productive:
  - `global_anchor_remaps=979` on `10p`
  - `global_anchor_remaps=3544` on `50p`
- planner rescues to already-kept local anchors also help a bit without growing local dictionaries:
  - `local_anchor_remaps=67` on `10p`
  - `local_anchor_remaps=10` on `50p`
- score-slack rescue is **not** the main lever:
  - `global_score_rescues=127` on `10p`
  - `global_score_rescues=502` on `50p`
  - `local_score_rescues=5` on `10p`
  - `local_score_rescues=0` on `50p`
  - residual `score_rescue_extra=1` on `10p`
  - residual `score_rescue_extra=3` on `50p`
- anchor readiness is **not** currently the dominant blocker:
  - residual `attachable_to_any_global=0` on `10p`
  - residual `attachable_to_any_global=0` on `50p`
  - residual `extra_beyond_anchor_ready=0` on both runs

Current residual reject breakdown:

- `10p`: `overlap=1362`, `signature=484`, `outside_ink=334`, `compare=274`, `score=151`, `pixel_delta=32`
- `50p`: `overlap=3124`, `signature=1015`, `outside_ink=894`, `compare=964`, `score=512`, `pixel_delta=77`

Most important scaling interpretation:

- exact dictionary accounting plus anchor remaps keep dictionary growth under control as pages increase
- the remaining compression pressure still concentrates in the residual generic tail, not in dictionary bytes
- overlap and signature false negatives remain the dominant residual buckets at both `10p` and `50p`
- the old punctuation-preservation hacks were no longer helping after the CC firehose fix:
  - removing fragile-mark exclusions slightly improved total size
  - the main visible effect was lower local-dictionary retention, not better residual rescue

One additional experiment was tried and should not drive the next session:

- a planner-only soft overlap slack for exact-dimension, low-black-delta matches
- result: no measurable change on either `10p` or `50p`, and no change in residual reject counts
- conclusion: revert/ignore that idea for now; it is not the active lever on this corpus

One cleanup was validated and should remain:

- punctuation-like / tittle / apostrophe special-casing was removed from active `sym_unify` class formation and live-anchor readiness
- this did **not** regress current `sahib` compression
- it slightly improved total size on both `10p` and `50p`
- there is no longer a reason to preserve that punctuation-era hack, because the original missing-dot problem was at CC extraction and the firehose change already fixed it

Residual instrumentation was also added on the current code state:

- each residual symbol now gets a planner-stage reason code
- residual summaries now include:
  - symbol count
  - instance count
  - black pixels
  - bitmap-byte proxy
  - pages affected
  - rough shape mix (`tiny`, `punct_like`, `glyph_like`)

Most important current residual findings from `benchmark_1773792244`:

- no pages are falling back wholesale to generic:
  - `full_generic_pages=0` on both `10p` and `50p`
- the generic tail is overwhelmingly residual single-use locals that fail attachment
- by bitmap-byte proxy, the largest residual buckets are:
  - `10p`: `UseCountOneGlobalRejectOverlap` (~34%), `UseCountOneGlobalRejectSignature` (~18%), `UseCountOneLocalRejectDim` (~16%), `UseCountOneGlobalRejectCompare` (~10%)
  - `50p`: `UseCountOneGlobalRejectOverlap` (~31%), `UseCountOneLocalRejectDim` (~23%), `UseCountOneGlobalRejectSignature` (~14%), `UseCountOneGlobalRejectCompare` (~12%)
- `UseCountOneNoCandidates` is present but not dominant:
  - about `3.8%` of bitmap-byte proxy on `50p`
- the residual tail is mostly normal glyph-like shapes, not tiny punctuation/junk:
  - for the top buckets, `glyph_like` heavily dominates `tiny` and `punct_like`

Counterfactual probes were then added and measured on `benchmark_1773798366`:

- relaxing local singleton anchor dim tolerance from `1` to `2` recovered nothing measurable:
  - `local_dim_relax2_symbols=0` on both `10p` and `50p`
- bypassing the overlap-only prescreen while still requiring full compare/outside-ink/score would recover a real minority of the top overlap bucket:
  - `10p`: `global_overlap_skip_symbols=435`, bitmap-byte proxy `2843`
  - `50p`: `global_overlap_skip_symbols=1015`, bitmap-byte proxy `6861`

Current interpretation of those probes:

- the large `UseCountOneLocalRejectDim` bucket does **not** look like a simple “raise dim slack by one” fix
- that bucket is more likely fragmentation or bad local candidate selection than a trivial threshold issue
- `UseCountOneGlobalRejectOverlap` does contain some overlap-prescreen false negatives
- but the overlap prescreen is not the whole generic-tail problem by itself; skipping it only recovers a minority of that bucket

Current interpretation:

- the next likely file-size lever is not generic-region tuning first
- it is reducing residual formation, especially:
  - overlap false negatives against global anchors
  - dimension fragmentation before or during local/global attachment
  - signature/compare false negatives on otherwise text-like singletons

That means the next likely frontier has shifted somewhat:

1. Better second-pass attachment to already-existing symbols
- especially page-local kept symbols and proven globals
- this is already helping and still looks low-risk

2. Better understanding of overlap/signature false negatives
- these now dominate the residual tail much more than score or anchor readiness

3. Better text-like residual diagnostics by reject reason and symbol shape
- especially for the medium/large residuals that still land in generic

The earlier handoff suggestion that live-anchor readiness might be the next main lever now looks less likely on the current code state.

Additional diagnostics were then added in `benchmark_1773798953` and `benchmark_1773799244` to answer two narrower questions:

1. Is `UseCountOneLocalRejectDim` mostly latent cross-page matches that never became visible locally?
2. Is `UseCountOneGlobalRejectOverlap` mostly a bad overlap prescreen, or does it only fail first and then die later anyway?

Results:

- cross-page probing of the `UseCountOneLocalRejectDim` bucket found only a small current-rules recovery set:
  - `10p`: `current_symbols=32`, bitmap-byte proxy `463`
  - `50p`: `current_symbols=31`, bitmap-byte proxy `520`
- there were **no** `dim2_only` recoveries:
  - `10p`: `dim2_only_symbols=0`
  - `50p`: `dim2_only_symbols=0`

Interpretation:

- the large local-dim bucket is **not** explained by “just allow dim delta 2”
- it is also only weakly explained by hidden cross-page matches under current rules
- that bucket now looks more like upstream fragmentation / poor local candidate neighborhoods / mixed non-text leftovers than a simple threshold miss

Overlap-bypass breakdown of `UseCountOneGlobalRejectOverlap`:

- `10p` overlap bucket total: `1341` symbols, bitmap-byte proxy `15040`
  - bypass would still fail on full `compare`: `718` symbols, bitmap-byte proxy `10961`
  - bypass would actually `accept`: `435` symbols, bitmap-byte proxy `2843`
  - bypass would fail on `outside_ink`: `118` symbols, bitmap-byte proxy `677`
  - bypass would fail on `score`: `70` symbols, bitmap-byte proxy `559`
- `50p` overlap bucket total: `3121` symbols, bitmap-byte proxy `43272`
  - bypass would still fail on full `compare`: `1798` symbols, bitmap-byte proxy `34356`
  - bypass would actually `accept`: `1015` symbols, bitmap-byte proxy `6861`
  - bypass would fail on `outside_ink`: `176` symbols, bitmap-byte proxy `968`
  - bypass would fail on `score`: `132` symbols, bitmap-byte proxy `1087`

Interpretation:

- overlap is a real false-negative source, but it is only a minority of the top overlap bucket
- most of the overlap bucket still dies on full compare once overlap is removed
- score remains secondary
- outside-ink exists but is not the main story

Current best explanation for the large generic residual output:

- it is **not** whole-page generic fallback
- it is **not** mainly anchor-readiness lag
- it is **not** mainly score cutoff
- it is **not** a simple local dim-threshold miss
- it is a mixture of:
  - real overlap-prescreen false negatives
  - larger full-compare false negatives against global anchors
  - a local-dim bucket that likely reflects fragmentation or poor candidate neighborhoods rather than a one-line threshold bug

This changes the next-step priority again:

1. Investigate why the full compare is rejecting so much of the former overlap bucket
- this now looks like the single largest recoverable decision path

2. Inspect the `UseCountOneLocalRejectDim` samples as a class, not as a threshold
- especially the larger glyph-like samples and obvious non-text/merged leftovers

3. Only after those are understood, consider generic residual packing work
- residual packing may still matter, but it is not the first-order cause of the current tail

Current verified sizes remain unchanged during these instrumentation passes:

- `sym_unify 10p`: `105.1 KB`
- `sym_unify 50p`: `429.5 KB`

Additional scaling measurements were then taken on `sahib` at `10/20/30/40/50` pages to check whether the encoder is moving toward the desired long-book asymptotic behavior.

Measured `sym_unify` totals:

- `10p`: `105.1 KB`
- `20p`: `190.4 KB`
- `30p`: `271.8 KB`
- `40p`: `354.7 KB`
- `50p`: `429.5 KB`

Measured `sym_unify` stream components:

- globals: `9.5`, `15.8`, `21.1`, `25.2`, `29.2 KB`
- local dicts: `1.4`, `1.9`, `1.9`, `2.0`, `2.1 KB`
- text regions: `57.6`, `116.6`, `175.9`, `241.1`, `296.9 KB`
- generic regions: `36.1`, `55.3`, `71.6`, `84.8`, `99.3 KB`

What this means:

- global-dictionary growth is already flattening somewhat:
  - `+6.3`, `+5.3`, `+4.1`, `+4.0 KB` per extra `10` pages
- local dictionaries are close to saturated already:
  - total local-dict bytes only grow from `1.4 KB` at `10p` to `2.1 KB` at `50p`
- total size is **not** plateauing, but average bytes per page are dropping:
  - `10.51`, `9.52`, `9.06`, `8.87`, `8.59 KB/page`
- generic is also getting cheaper per page, but not cheaply enough:
  - `3.61`, `2.76`, `2.39`, `2.12`, `1.99 KB/page`

Interpretation:

- the current encoder is **not** failing in the sense of “dictionary bytes keep growing linearly forever”
- the dictionary side is behaving directionally correctly already
- the current weakness is that the generic tail remains large enough that the overall file still grows with a relatively high steady slope
- in other words:
  - warm-up behavior exists
  - dictionary saturation is starting
  - but text-like residual leakage is still too high for the kind of long-book asymptotic behavior we want

Important conceptual note for future sessions:

- the ideal long-book behavior is **not** that total file size plateaus
- the ideal is:
  - dictionary growth becomes strongly sublinear
  - local dictionaries stay tiny
  - average bytes per page decline after warm-up
  - residual generic bytes per page become small
- even a very successful JBIG2 symbol path still has roughly linear total growth after warm-up, because each text occurrence still costs reference/placement bits

So the current codebase is already showing the healthy part of the asymptotic story on dictionary bytes, but not yet the healthy part on residual suppression.

One more diagnostic pass was then added on `benchmark_1773802818` to split the `UseCountOneGlobalRejectCompare` bucket into finer causes, because the normal `RejectCompare` path collapses several different misses into one label.

Relaxed-compare probe results:

- `10p` compare bucket total: `262` symbols, bitmap-byte proxy `4270`
  - relaxed best match still fails on `total_err`: `137` symbols, bitmap-byte proxy `2594`
  - relaxed best match becomes `total_err+outside_ink`: `62` symbols, bitmap-byte proxy `819`
  - relaxed best match becomes `score`: `38` symbols, bitmap-byte proxy `538`
  - relaxed best match becomes `outside_ink`: `13` symbols, bitmap-byte proxy `203`
  - relaxed best match actually `accepts`: `12` symbols, bitmap-byte proxy `116`
- `50p` compare bucket total: `959` symbols, bitmap-byte proxy `16542`
  - relaxed best match still fails on `total_err`: `438` symbols, bitmap-byte proxy `8801`
  - relaxed best match becomes `score`: `240` symbols, bitmap-byte proxy `3850`
  - relaxed best match becomes `total_err+outside_ink`: `144` symbols, bitmap-byte proxy `2235`
  - relaxed best match becomes `outside_ink`: `57` symbols, bitmap-byte proxy `833`
  - relaxed best match actually `accepts`: `80` symbols, bitmap-byte proxy `823`

Interpretation:

- the compare bucket is **not** mostly a hidden outside-ink problem
- it is also **not** mostly a hidden overlap problem at this point
- the largest slice is still genuine `total_err` excess under full compare
- there is, however, a meaningful secondary slice that would move from `compare` to `score` under a relaxed compare budget, especially at `50p`
- there is also a small but real tail of outright false negatives in the compare bucket itself

This sharpens the overall residual picture:

- top path 1: overlap-prescreen false negatives
- top path 2: full-compare `total_err` false negatives against plausible globals
- secondary path: candidates that are close enough structurally, but the weighted assignment score still rejects them after compare succeeds
- less likely than before: simple dim slack or border-ink-only fixes

If the next session continues the diagnostic-first approach, the best next probe is:

1. sample and classify the large `total_err` slices from:
   - overlap-bypass `compare`
   - relaxed-compare `total_err`
2. determine whether those are:
   - true non-matches
   - scan-noise variants that refinement should absorb
   - family fragmentation that should have been resolved earlier
3. only then decide whether to:
   - loosen any compare rule
   - add a refinement-only recovery path
   - or fix upstream fragmentation instead

Another diagnostic pass on `benchmark_1773803271` and `benchmark_1773803448` then tested whether the large `total_err` slices are actually near-threshold enough to be rescued by a very small compare-budget increase.

Detailed `total_err` findings:

- overlap-bypass `compare -> total_err` is mostly exact-dimension and low-shift, but often far over budget:
  - `10p`: `718` symbols, `629` exact-dim, all `718` with `|dx|<=1 && |dy|<=1`
    - over budget by `<=2`: `76`
    - `<=4`: `138`
    - `<=8`: `178`
    - `>8`: `326`
  - `50p`: `1798` symbols, `1506` exact-dim, all `1798` with `|dx|<=1 && |dy|<=1`
    - over budget by `<=2`: `132`
    - `<=4`: `337`
    - `<=8`: `427`
    - `>8`: `902`

- global `RejectCompare -> total_err` is much narrower and much closer to threshold:
  - `10p`: `137` symbols, `119` exact-dim, all `137` with `|dx|<=1 && |dy|<=1`
    - over budget by `<=2`: `122`
    - `<=4`: `12`
    - `<=8`: `3`
    - `>8`: `0`
  - `50p`: `438` symbols, `374` exact-dim, all `438` with `|dx|<=1 && |dy|<=1`
    - over budget by `<=2`: `398`
    - `<=4`: `33`
    - `<=8`: `7`
    - `>8`: `0`

This initially looked like strong paydirt for a small compare-slack experiment, so a bounded counterfactual was added next.

Small compare-slack counterfactuals, while still enforcing outside-ink and score:

- from global `total_err`:
  - `10p`: `+2` or `+4` would recover only `27` symbols, bitmap-byte proxy `513`
  - `50p`: `+2` or `+4` would recover only `92` symbols, bitmap-byte proxy `1852`
- from overlap-bypass `compare`:
  - `10p`: `+2` would recover `9` symbols, `+4` would recover `19`
  - `50p`: `+2` would recover `13` symbols, `+4` would recover `38`

Interpretation:

- there **is** a near-threshold compare seam, especially in the global compare bucket
- but a naive “just allow a couple more compare errors” change is **not** enough
- most of those candidates still fail once the later score gate is re-applied
- therefore the simple compare-slack idea has hit bedrock

Current practical conclusion:

- `RejectCompare` is not pure bedrock, because many misses are only slightly over the compare budget
- but it is also not easy paydirt, because score rejects most of those near-misses after compare succeeds
- the next meaningful seam, if diagnostics continue, is likely:
  - why score rejects so many near-threshold exact-dimension, low-shift candidates
  - whether a refinement-only acceptance path should score differently from plain symbol-unify assignment
  - or whether those candidates are still genuinely unsafe despite looking close on raw XOR error

## Recommendation For Next Session

Start from the current clean baseline and focus on:
- residual text-symbol false negatives
- live-anchor matching completeness
- text-region/generic tradeoff for text-only inputs

Do **not** start by loosening substitution thresholds or reviving broad refinement.
