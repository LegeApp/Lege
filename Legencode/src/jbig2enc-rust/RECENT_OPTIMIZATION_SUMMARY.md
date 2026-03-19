# Recent Optimization Summary

This file summarizes the main optimizations made after `ec252d1` and flags which ones are likely speed-only versus behavior-changing for `sym_collapse`.

## Safe Or Mostly Safe To Keep

- Comparator hot-loop split into specialized scalar/popcnt kernels in `src/jbig2comparator.rs`
  - Purpose: reduce slice/index overhead in `best_alignment_by_xor`
  - Expected effect: speed only

- Optional AVX2 path in `src/jbig2comparator.rs`
  - Purpose: experimental CPU-specific acceleration
  - Expected effect: speed only
  - Note: not enabled by default

- Bounded shift search for ordinary symbol matching in `src/jbig2enc.rs`
  - Purpose: stop searching shifts that the acceptance rules would later reject anyway
  - Expected effect: speed, plus modest behavior tightening
  - Observation: `symbol` mode remained visually acceptable

- Bounded shift search in planning/local-global aliasing in `src/jbig2enc.rs`
  - Purpose: align planner-time matching with actual acceptance limits
  - Expected effect: speed and consistency

- Recent-cache stack snapshot and delayed hash-key computation in `src/jbig2enc.rs`
  - Purpose: remove small per-symbol overhead in the symbol ingestion path
  - Expected effect: speed only

- Symmetric collapse probe cache in `src/jbig2collapse.rs`
  - Purpose: reuse `(a,b)` and `(b,a)` probe results
  - Expected effect: speed only if the probe result is reversed correctly

- Packed-word `compute_symbol_signature()` in `src/jbig2collapse.rs`
  - Purpose: reduce per-pixel signature extraction overhead
  - Expected effect: speed only if signature values remain equivalent

## Higher-Risk Behavior Changes

- Collapse family probe changed from full-radius detailed comparison plus post-check shift rejection
  to limited detailed comparison inside `max_dx/max_dy`
  - Files: `src/jbig2collapse.rs`, `src/jbig2comparator.rs`
  - Expected effect: more aggressive family acceptance
  - Observation: strongly correlated with visual degeneration
  - Current state: reverted locally

- Detailed collapse metrics were rewritten from per-pixel union-space accounting to a packed-row implementation
  - File: `src/jbig2comparator.rs`
  - Expected effect: speed, but also possible semantic drift in overlap/outside/profile metrics
  - Risk: collapse acceptance can change even if the fast aligner is unchanged

- Prototype candidate ordering and early cutoff in `src/jbig2collapse.rs`
  - Purpose: reduce scoring work
  - Expected effect: can change chosen family prototype
  - Risk: a worse prototype can cause widespread substitutions

## Speed Milestones Observed

- `symbol` on `sahib/50p`
  - Earlier baseline in this round: about `4.33s`
  - Best recent state: about `1.05s - 1.17s`

- `sym_collapse` on `sahib/50p`
  - Earlier baseline in this round: about `13.93s`
  - Aggressive fast state: about `2.12s`, but with visible degeneration
  - Safer rollback state: about `3.78s`, with stricter collapse acceptance

## Current Working Theory

The strongest regression source is collapse-family admission, not the low-level comparator kernel work.

The most suspicious changes are:

1. limited detailed collapse probing in `lossy_family_probe()`
2. packed-row rewrite of `metrics_for_alignment()`
3. prototype-selection changes that alter which archetype a family uses

If further rollback is needed, the safest order is:

1. keep the hot comparator kernels
2. keep bounded ordinary symbol matching
3. keep symmetric probe caching
4. revert or harden detailed collapse metrics
5. only then revisit prototype-selection heuristics
