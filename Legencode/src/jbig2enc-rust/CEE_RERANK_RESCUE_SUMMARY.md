# CEE-Style Anchor Rescue Summary

This note records the current experimental `sym_unify` anchor-rescue feature so it can be tabled cleanly while work returns to the paper-driven optimization opportunities.

## What Was Added

The current branch adds a narrow, experimental rescue path for symbols that were otherwise going to page residual generic encoding.

The feature has two parts:

1. A small CEE-inspired rerank signal
   - Implemented as a lightweight context-style bitmap cost, not a trained Guo-style CEE model.
   - Used only to choose among a very small set of already-safe rescue candidates.

2. A score-slack rescue path
   - If a residual symbol fails only on `score`, but is still within a small configurable slack above the normal `sym_unify` score limit, it may still be remapped to an existing anchor instead of going to generic.

Important:
- This is not a full CEE matcher.
- It does not replace the current hard safety gates.
- It is currently aimed only at recovering generic-bound text-like leftovers.

## Current Configuration

New config field:

- `sym_unify_score_rescue_slack`

Default:

- `2`

Benchmark env var:

- `BENCH_UNIFY_SCORE_RESCUE_SLACK`

## Intended Goal

The goal is not to improve ordinary ingest matching globally.

The goal is specifically:

- reduce the residual generic tail
- find safe homes for symbols currently falling to generic
- learn whether those residuals are mostly blocked by score or by harder gates like:
  - dimension mismatch
  - signature mismatch
  - overlap failure
  - compare failure
  - outside-ink failure

## What It Currently Does

In the planning-stage rescue pass:

- page-local one-off symbols are checked against anchor-ready globals
- normal strict accepts are still allowed
- near-miss `RejectScore` candidates can also be considered if:
  - all earlier gates passed
  - the score is within `score_limit + sym_unify_score_rescue_slack`
- among those rescue candidates, a lightweight context-style rerank chooses the best anchor

This keeps the experiment tightly scoped to generic-bound symbols rather than perturbing the whole ingest path.

## What Was Tried And Reverted

An earlier version also reranked anchor choice during ordinary ingest.

That version:

- improved file size slightly
- but imposed too much runtime cost in the hot path

It was removed in favor of the narrower residual-rescue-only approach.

## Current Result

On the recent `sahib/10p` benchmark:

- previous `sym_unify` reference from this work session: about `107.1 KB`
- current rescue build: about `106.3 KB`

Observed effect:

- modest size win
- modest reduction in generic-region bytes
- much better speed/benefit tradeoff than the broader ingest reranker

This is useful, but it is not yet a major breakthrough.

## Why This Is Being Tabled

The user-directed priority is to return to the academic recommendation markdowns and continue with the broader optimization roadmap.

This rescue feature is therefore being parked as:

- a valid experimental branch
- a small confirmed improvement
- not the main next lever unless diagnostics later show a large score-limited residual population

## How To Improve It Later

If this feature is revisited, the best next steps are:

1. Quantify the residual population more precisely
   - Measure how many residuals are:
     - strict accepts to current anchors
     - score-only near misses
     - blocked by earlier gates

2. Restrict reranking to tiny candidate sets
   - Keep rescue candidate counts very small.
   - Do not let reranking expand into wide hot-path search.

3. Replace the heuristic rerank with a better cheap predictor
   - A real trained CEE-like model could be tried later.
   - But it should still operate only after hard safety gating.

4. Separate “safe text rescue” from “general anchor matching”
   - The rescue path should remain focused on generic-bound leftovers.
   - It should not silently loosen global substitution behavior.

5. Add corpus-level diagnostics
   - Report:
     - `score_rescues`
     - `score_rescue_extra`
     - residual reject breakdown before and after rescue
   - Use that to decide whether more work here is justified.

6. Consider targeted pre-score relaxation instead of smarter reranking
   - If residuals are mostly blocked by `signature`, `overlap`, or `outside_ink`, more reranking will not help much.
   - In that case the next work should focus on text-safe relaxation of one earlier gate, not on CEE.

## Main Takeaway

This feature is best understood as:

- a targeted residual rescue experiment
- not a full CEE implementation
- useful for learning about generic-tail failures
- worth keeping available
- not the primary optimization frontier unless future diagnostics show that most missed text-symbol opportunities are score-limited
