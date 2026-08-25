# Taubman perceptual masking is implemented, tested, and never activated

**Status:** open question, needs a decision before the code is either wired or deleted.
**Found:** 2026-08-25, during the no-suppression compiler-warning campaign.
**Intended home:** the AKR ledger. It could not be written — see *Why this is a file* below.

## What is true today

jp2lam contains a complete, unit-tested implementation of Taubman 2000 §VI subband
perceptual masking that no shipped encode ever uses.

- `TaubmanMaskMap` and `block_masking_multiplier` (`src/perceptual/taubman_masking.rs`)
  implement the algorithm.
- `dwt::pcrd` is wired to consume it, via `DistortionModel::Taubman2000` and
  `PassDistortionContext::taubman_masking_weight`.
- The live rate-control call site — `curves_from_stored_layout` in
  `src/encode/backend/native/rate.rs` — passes a **hardcoded `1.0` literal** where the
  per-block weight belongs.

So the masking term is identity for every block of every encode. The consumer is wired;
the producer is not connected to it.

## Why it surfaced now

The unreferenced half of the machinery was hidden behind jp2lam's eight file-level
`#![allow(dead_code)]` attributes. Removing those (commit `4b358828`) exposed it. The dead
half is currently parked under `#[cfg(test)]` rather than deleted, specifically so this
decision is not foreclosed.

## What activating it would actually cost

This is not a code-motion exercise. The algorithm needs the **pre-quantization DWT `f32`
coefficient buffer** at pass-selection time, and the tile-rect encoder does not retain that
buffer after quantization. Wiring it therefore requires either:

- retaining the buffer — a working-memory cost the bounded-memory design deliberately
  avoids (Phase 4 §8.3/§8.4); or
- recomputing it at selection time.

Per the algorithm's own doc comment, enabling masking only **reorders PCRD truncation
points** and does not move the byte budget. So the change should be rate-neutral: the
output size stays put and only the visual distribution of quality shifts. That makes it
assessable by visual/PSNR-per-region comparison rather than by compression ratio.

## The prior evaluation — unverified

Operator recollection, recorded as context and **not** as fact: an earlier evaluation of
perceptual masking in this codebase is believed to have tested poorly. That evaluation
predates the AKR ledger, and no record or measurement of it survives. Whether the poor
result came from *this* implementation, from a different masking model, or from a flawed
harness is unknown.

**Re-deriving that outcome is the first task for whoever picks this up.** Without it there
is no way to tell whether the hardcoded `1.0` is an oversight or a deliberate, undocumented
disablement — and those two readings imply opposite actions.

## Decision needed

1. **Wire it**, if a fresh evaluation shows a visual win worth the memory cost. Then the
   `#[cfg(test)]` gates come off and `curves_from_stored_layout` gets a real weight.
2. **Delete it**, if the prior negative result is reproduced. Then the whole
   `perceptual::taubman_masking` surface and the `Taubman2000` distortion model go, and
   `pcrd` loses its masking hook.

Leaving it as-is is the one option with no upside: it is dead weight that reads as a
working feature.

## Why this is a file and not an AKR record

`knowledge.propose` refuses to write, because the ledger does not validate:

```
write aborted: the resulting ledger did not validate (22 diagnostics); nothing was written
```

The blockers are **pre-existing and unrelated** — measured identically at session start,
before any of this campaign's changes:

- **20 × AKR-T023** — completed evidence records cite artifacts under `.agent/scratch`,
  which is disposable, and those artifacts have since been pruned. `akr scratch keep`
  cannot fix them: the files are already gone. Each needs its evidence record revised to
  cite a durable path, superseded, or its measurement re-run.
- **8 × AKR-R022** — stale evidence on three unrelated completed records.
- 228 × AKR-G012 lineage warnings, left over from the sanitized-history cutover.

AKR compiles the whole ledger and refuses to write into an invalid state, which is working
as designed. Once those are repaired, this file should be re-proposed as an `observation`
under `lege-ecosystem.jp2lam.taubman-masking-unwired`, scoped to
`lege-codecs/jp2lam/src/perceptual/**`, `src/dwt/pcrd.rs` and
`src/encode/backend/native/rate.rs`, and this file deleted in the same change.

## Related findings from the same sweep

Two other things the suppression removal exposed, both since resolved:

- **Component-level parallelism was unreachable.** `allow_component_parallelism` was a
  complete, tested memory-budget heuristic that only the superseded whole-image pipeline
  consulted. Now wired to the live tile-rect path (commit `d6878f93`), with a
  byte-identity test.
- **RAW/bypass coding and explicit ER-termination** (`NativeTier1PassCodingMode::Raw`,
  `NativeTier1PassTermination::ErTerm`) are real, spec-compliant JPEG2000 options the
  encoder's policy never selects. Gated, not deleted. No decision recorded for these
  either.
