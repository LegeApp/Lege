# Refinement Lessons Learned

The original refinement path in `jbig2enc-rust` has been retired from active use. It did not produce consistent file savings, and in practice it usually increased dictionary size, text-region overhead, or both.

## What The Old Refinement Pipeline Did

The retired path had four main stages:

1. Build a symbol dictionary layout from the harvested symbol set.
2. Group visually similar symbols into small families.
3. Pick one prototype per family.
4. Export the prototype and encode some family members as refinement instances against it.

In code terms, that work centered around:

- `plan_symbol_dictionary_layout()`
- `build_refinement_family_layout()`
- `family_should_refine()`
- `encode_text_region_with_refinement()`

The idea was straightforward:

- keep fewer dictionary bitmaps
- preserve near-match fidelity through refinement residuals
- win back bytes by replacing standalone symbol exports with prototype + delta

## Why It Did Not Pay Off

In practice, the current implementation lost on the wrong side of the tradeoff:

- many family members were too different to refine cheaply
- refinement added per-instance overhead in text regions
- prototype choice often did not minimize actual page-stream cost
- one-off or weakly recurring variants were poor refinement candidates
- preserving noisy variation through refinement kept too much useless structure alive

The result was usually:

- larger dictionaries than expected
- larger final files than plain `symbol`
- no meaningful compression advantage over `sym_unify`

## Main Lessons

### 1. Refinement should not be a general-purpose fallback

Using refinement as a broad “near match” mechanism was too expensive. Most candidates were not stable enough to justify prototype + residual coding.

### 2. Refinement only makes sense for recurring structured subvariants

If refinement comes back later, it should be limited to:

- symbols that recur enough to matter
- symbols with stable, coherent residual structure
- variants already attached to a strong canonical representative

### 3. Representative choice matters more than the old path admitted

If the prototype is not the true center of a recurring subvariant, refinement cost rises quickly and the family stops paying for itself.

### 4. Page-byte economics matter more than symbol-count reduction alone

The old path gave too much weight to reducing exported symbols and not enough to:

- actual text-region residual cost
- prototype bitmap cost
- repeated refinement overhead across the document

### 5. Refinement should be downstream of `sym_unify`, not parallel to it

The correct future architecture is not a separate `sym_refine` mode. If refinement returns, it should be a narrow extension of `sym_unify`:

- canonical representative
- stable recurring subcluster
- optional refinement only for that subcluster

That is a better fit than reviving a standalone refinement mode.

## What A Future Revisit Should Look Like

If refinement is reintroduced later, it should be attempted only after:

1. `sym_unify` has already formed safe representative classes
2. border subclusters have been identified
3. repeated subclusters are proven to be structurally stable
4. a gain model estimates total page-byte savings, not just symbol-count savings

The likely shape of a future design would be:

- Tier 1: canonical representative
- Tier 2: stable recurring subcluster
- Tier 3: optional refinement from subcluster to representative
- Outliers: direct symbol or generic residual

## Current Policy

For now:

- `sym_refine` is removed from active use
- dictionary planning no longer depends on refinement
- `sym_unify` is the quality-first compression path

If refinement comes back, it should come back as a narrow, evidence-driven extension of `sym_unify`, not as the old standalone process.
