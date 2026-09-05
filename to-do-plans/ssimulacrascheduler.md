Yes. A substantial part of the JPXL work transfers, but **not the calibration that maps “how hard should I compress?” into codec parameters**.

After inspecting both codebases, I would estimate the split like this:

| JPXL component                        | Transfer to JP2LAM | Recommendation                                   |
| ------------------------------------- | -----------------: | ------------------------------------------------ |
| SSIMULACRA2 implementation            |              ~100% | Share the exact implementation                   |
| Precomputed-reference/evaluator model |           ~90–100% | Reuse directly with JP2 adapter                  |
| “target = minimum score” semantics    |               100% | Same API philosophy                              |
| Probe budgets/status/trace machinery  |            ~80–90% | Port                                             |
| Log-loss crossing/bracketing math     |            ~80–90% | Port                                             |
| Exact-finalist verification           |               ~90% | Port                                             |
| Source feature definitions            |            ~50–70% | Reuse generic features, add DWT-specific ones    |
| Search constants                      |            ~30–60% | Start with JPXL values, then measure             |
| Initial-rung predictor                |            ~10–20% | Retrain completely                               |
| Predictor coefficients/tables         |                ~0% | Do not copy                                      |
| JPXL quantizer ladder                 |                ~0% | Replace with JPEG 2000 quantization/PCRD control |
| Structure-reuse rules                 |                ~0% | JXL-specific                                     |
| Surrogate renderer                    |             ~0–20% | Defer; redesign for DWT if needed                |

So **you do not need another ground-up SSIMULACRA2 scheduler project**. The difficult architectural work has already been solved. But the part that says *“for this source and target SSIMULACRA2 score, start around this JPEG 2000 quantization and truncation point”* must be trained again.

More importantly, JPEG 2000 gives JP2LAM an advantage that should make this iteration easier.

# 1. Don't transplant the JPXL scheduler literally

JPXL currently has a very clean quality-controller contract:

> Find the smallest exact stream, within a bounded search budget, whose reconstructed pixels meet a requested minimum SSIMULACRA2 score.

That should become JP2LAM's contract too.

But JPXL navigates roughly through a **quantizer/effective-scale ladder**, rebuilding candidates as needed. Its controller does:

```text
source features
    ↓
predict initial rung
    ↓
render candidate pixels
    ↓
SSIMULACRA2
    ↓
geometrically bracket target
    ↓
log-loss crossing estimate
    ↓
correct
    ↓
exact-price viable finalists
    ↓
smallest stream ≥ target
```

That search design transfers.

The **candidate engine underneath it does not**.

JPEG 2000 gives you two independent-ish knobs:

```text
                JPEG 2000 quality control

                     SSIMULACRA2 target
                            │
                    OUTER CONTROLLER
                            │
                 global quantization scale
                            │
                 DWT → quantize → Tier 1
                            │
                    stored coding passes
                            │
                     INNER CONTROLLER
                            │
                      PCRD truncation
                            │
                    reconstructed pixels
                            │
                      SSIMULACRA2
```

That distinction should be central to the implementation.

JPEG's own current Qfactor guidance makes essentially the same architectural distinction: JPEG 2000 can use quantization parameters for direct quality control, while PCRD can independently enforce/trade rate, and quantization-driven quality can avoid generating many passes only to discard them later. ([JPEG][1])

JP2LAM is already well-positioned for that architecture.

---

# 2. JP2LAM is not starting from zero

The current code has three pieces that substantially reduce this project.

### A. The perceptual measurement harness already exists

`examples/perceptual_curve.rs` already:

* encodes through JP2LAM;
* decodes through JP2LAM;
* computes PSNR;
* computes Butteraugli;
* computes SSIMULACRA2;
* reports bytes/bpp.

So the calibration pipeline has already been prototyped.

It currently uses the external `ssimulacra2` crate as a measurement tool. Replace that, for production, with the same pinned implementation used by JPXL.

### B. JP2LAM already does perceptually biased PCRD

`src/encode/backend/native/rate.rs` already modifies the measured Annex-J ΔMSE using:

* synthesis/subband weighting;
* band bias;
* block classification;
* contrast visibility;
* optional masking structure.

And your August qualification results show this isn't useless:

* q50 small-photo mean: **+1.82 SSIMULACRA2** at essentially identical size;
* q75: **+1.11** on the small set;
* higher-quality effects are smaller/mixed.

So JP2LAM already has a perceptual **local allocator**.

What it lacks is a perceptual **global quality controller**.

Those are different jobs.

I would keep the local allocator rather than replacing it.

### C. JP2LAM retains Tier-1 coding passes

This is the most useful difference from JPXL.

`select_stored_tile_passes()` and the exact-rate machinery can take already encoded Tier-1 passes and choose different truncation points.

That's exactly how JPEG 2000 PCRD is supposed to work. OpenJPEG similarly retains/selects code-block passes according to their distortion-reduction/rate slope rather than re-running the whole transform for each desired rate. ([GitHub][2])

Therefore:

> **One expensive JPEG 2000 quantizer candidate can yield many relatively cheap SSIMULACRA2 rate probes.**

That should make JP2LAM's eventual controller cheaper than a naive port of JPXL's controller.

---

# 3. First extract the SSIMULACRA2 implementation from JPXL

I would make this the first commit.

Right now JPXL has the right architecture in `jpxl-perceptual`:

```text
PrecomputedReference
Ssimulacra2
LinearRgbView
deterministic execution
metric version
source-only work cached once
candidate scratch reused
```

But it still has some JPXL dependencies and its evaluator specifically knows about `ValidatedPixelPlan`.

Split it:

```text
perceptual-core/
    color.rs
    blur.rs
    pyramid.rs
    pool.rs
    reference.rs
    ssimulacra2.rs
    lib.rs

jpxl-perceptual/
    evaluator.rs        # JXL adapter

jp2lam/
    perceptual/
        evaluator.rs    # JPEG2000 adapter
```

Naming is up to you; I'd probably use something neutral such as `lege-perceptual` if it will ultimately serve the broader codec ecosystem.

The important invariant is:

```text
JPXL SSIMULACRA2 target 90.0
        ==
JP2LAM SSIMULACRA2 target 90.0
```

Not “approximately the same metric through two implementations.”

The metric version should be pinned and surfaced in diagnostics.

The external Rust `ssimulacra2` crate should remain a **parity oracle in tests**, as JPXL already does, rather than becoming JP2LAM's production dependency.

---

# 4. Add a real perceptual target to JP2LAM

Do **not** immediately redefine `RateControl::Quality(u8)`.

Currently:

```rust
pub enum RateControl {
    Lossless,
    Quality(u8),
    TargetBytes(u64),
    TargetBitsPerPixel(f32),
    CompressionRatio(f32),
}
```

Add something explicit:

```rust
pub enum RateControl {
    Lossless,
    Quality(u8),

    Perceptual(PerceptualTarget),

    TargetBytes(u64),
    TargetBitsPerPixel(f32),
    CompressionRatio(f32),
}

pub struct PerceptualTarget {
    pub score: f64,
    pub effort: PerceptualEffort,
}

pub enum PerceptualEffort {
    Fast,
    Balanced,
    Quality,
}
```

You don't technically need a metric enum if SSIMULACRA2 is deliberately the sole canonical quality metric. I'd avoid abstraction for an imaginary second metric.

Later, once the relationship is well calibrated, `Quality(u8)` can itself map onto SSIMULACRA2 targets.

But don't change its semantics while developing the controller. You need the old path as your baseline.

---

# 5. Port JPXL's controller *contract*

I would bring across almost verbatim:

### `PerceptualObservation`

Something equivalent to:

```rust
struct PerceptualObservation {
    score: f64,
    reconstruct_millis: Option<u64>,
    metric_millis: Option<u64>,
}
```

### `QualityStatus`

Keep distinctions such as:

```text
Met
SaturatedTop
UnderTargetWorkCap
```

A perceptual encoder should never quietly return a result below the requested floor as if it succeeded.

### Probe tracing

Every run should be able to report something like:

```text
target_score = 90
metric = ssimulacra2-<version>

probe 0:
    quant_scale = ...
    pcrd_bytes = ...
    score = 86.4

probe 1:
    quant_scale = ...
    pcrd_bytes = ...
    score = 92.1

probe 2:
    ...
    score = 90.35

winner:
    score = 90.35
    bytes = 481221
```

This proved useful in JPXL and will be even more important while fitting JPEG 2000.

### Hard budgets

JPXL currently uses approximately:

| Effort   | Pixel probes | Exact prices |
| -------- | -----------: | -----------: |
| Fast     |            3 |            2 |
| Balanced |            5 |            3 |
| Quality  |           10 |            4 |

Use those as **starting controller budgets**, not sacred JP2 constants.

---

# 6. Port the most valuable piece of scheduler math

JPXL's fundamental interpolation choice is excellent and codec-independent:

```text
loss = max(100 - SSIMULACRA2, ε)
```

Then model:

```text
log(loss)
    versus
log(codec fineness)
```

JPXL's initial prior is roughly:

```text
loss ∝ effective_scale^-0.9
```

and it fits the actual local slope once it has two observations.

This is exactly the kind of work you do **not** need to rediscover.

Port:

* `LOSS_EPSILON`;
* the log-loss transformation;
* crossing interpolation;
* geometric bracket expansion;
* target reserve;
* bounded correction;
* feasible/infeasible bracket tracking;
* finalist selection.

Do **not** initially assume JP2's exponent is 0.9.

Use 0.9 as the prior merely because it gives the new controller somewhere reasonable to start.

Then measure the JP2 corpus.

There is a good chance the actual fitted exponent will differ materially.

---

# 7. Define a JPEG 2000-native “rung”

This is where the fork from JPXL should occur.

Don't make public `quality` the search coordinate.

Define a continuous/logarithmic quantizer parameter:

```rust
struct QuantRung {
    log_scale: f64,
}
```

Conceptually:

```text
smaller Δ  → finer quantization → higher score
larger Δ   → coarser quantization → lower score
```

It should ultimately scale the existing subband quantization derivation globally:

```text
Δb,c' = global_scale × Δb,c
```

while preserving JP2LAM's existing relative subband/component weighting.

In other words:

```text
current JP2LAM perceptual model:
    determines relative allocation

SSIM controller:
    determines global severity
```

Don't have SSIMULACRA2 directly manufacture every JPEG 2000 subband quantizer.

That would throw out useful JPEG 2000 structure and make calibration vastly harder.

JPEG's Qfactor guidance similarly treats global quality as a mechanism for deriving quantization steps while preserving subband weighting. ([JPEG][1])

---

# 8. Use a two-dimensional controller, but hide that complexity internally

At a particular quantizer:

```text
Q₁
 ↓
DWT
 ↓
quantize
 ↓
Tier 1
 ↓
all coding passes retained
```

you have an entire family of candidates:

```text
Q₁ + PCRD truncation A → 65 KB
Q₁ + PCRD truncation B → 84 KB
Q₁ + PCRD truncation C → 110 KB
Q₁ + all useful passes → 147 KB
```

You can score several of them without redoing the forward transform or Tier-1 encoding.

Therefore the search should be:

```text
OUTER:
    choose quantization scale

INNER:
    choose smallest PCRD truncation at that quantizer
    whose SSIMULACRA2 score ≥ target
```

That's much better than pretending JPEG 2000 has a single quality dial.

---

# 9. Implement the correctness-first version before optimizing it

There is one place I would deliberately accept inefficiency temporarily.

For the first implementation:

```text
stored Tier-1 passes
       ↓
select passes
       ↓
construct candidate J2K in memory
       ↓
JP2LAM decoder
       ↓
viewer-visible RGB
       ↓
SSIMULACRA2
```

This gives you an extremely important invariant:

> The pixels being scored are precisely the pixels JP2LAM itself decodes from the prospective output.

Do that first.

It removes an entire category of possible controller bugs.

Your current `perceptual_curve.rs` already proves this route works.

Once the scheduler passes its corpus tests, optimize away:

* Tier-2 serialization;
* codestream parsing;
* repeated allocation.

---

# 10. Then make candidate reconstruction cheap

The optimized pipeline should become approximately:

```text
StoredTier1Candidate
    │
    ├─ selected pass counts
    │
    ↓
reuse JP2LAM Tier-1 decoder
    ↓
quantized coefficient reconstruction
    ↓
inverse 9/7 DWT
    ↓
inverse colour transform
    ↓
sample quantization/clamping
    ↓
viewer-visible candidate pixels
```

JP2LAM already has the reconstruction machinery in:

```text
src/decode/t1.rs
src/decode/reconstruct.rs
src/dwt/irrev97.rs
```

So I would expose an internal candidate-reconstruction path rather than implement a second approximate encoder-side decoder.

Something like:

```rust
fn reconstruct_stored_selection(
    state: &StoredPerceptualState,
    selections: &[NativeTier1SelectionLayout],
    scratch: &mut ReconstructionScratch,
) -> Result<PerceptualRaster>;
```

The same decoding/reconstruction primitives used for real files should be used.

### Critical test

For a candidate:

```text
internal candidate reconstruction
```

must be pixel-identical to:

```text
serialize candidate
→ Jp2Decoder
→ reconstructed raster
```

for every supported 8-bit RGB/gray case.

Do not move the controller onto the fast internal path until this passes.

---

# 11. The inner PCRD search is where JP2 can beat the JPXL scheduler computationally

At fixed quantization, PCRD's control coordinate can simply be:

```text
target body bytes
```

or a lambda threshold.

I slightly prefer **body bytes as the navigation abstraction**, because:

* it has obvious monotonic ordering;
* the exact-rate machinery already understands it;
* it makes search traces intuitive;
* final filesize can later be exact-priced separately.

So:

```rust
fn probe_at_body_bytes(
    quant_state: &QuantizerState,
    body_bytes: u32,
) -> ScoreObservation;
```

You can then search:

```text
all passes        score 94.7       520 KB
350 KB            score 93.2
250 KB            score 91.4
190 KB            score 89.1
                         ↑
                       target 90

→ interpolate/probe around ~210–225 KB
```

Only after finding the smallest satisfactory truncation do you construct/exact-price the JP2 output.

OpenJPEG's own PCRD path is based on selecting coding passes according to rate-distortion thresholds, so this remains completely natural JPEG 2000 behavior; SSIMULACRA2 is simply evaluating the global result rather than pretending to be an additive code-block distortion term. ([GitHub][2])

---

# 12. Do not try to calculate “ΔSSIMULACRA2 per Tier-1 pass”

This is probably the biggest architectural trap.

It is tempting to replace:

```text
ΔMSE / Δbytes
```

with:

```text
ΔSSIMULACRA2 / Δbytes
```

inside PCRD.

Don't.

SSIMULACRA2 is:

* spatial;
* multi-scale;
* nonlinear;
* globally pooled;
* affected by interactions among distortions.

There is no useful independent scalar `ΔSSIMULACRA2` intrinsic to one isolated coding pass.

You would either need absurd numbers of full-image metric evaluations or end up inventing an approximation that essentially becomes another perceptual distortion model.

Instead:

### Inner local optimization

Keep:

```text
measured ΔMSE
× synthesis norms
× perceptual band/block/masking weights
```

for PCRD.

### Outer global truth

Use:

```text
actual reconstructed image
→ SSIMULACRA2
```

to decide whether the resulting stream meets its requested quality.

That gives you both advantages.

---

# 13. Outer quantizer search

Suppose the target is `90`.

Start from a quantizer rung.

For each quantization state:

1. Produce all useful Tier-1 passes.
2. Score its highest-quality reconstruction.
3. If even that score is below 90, quantization is too coarse.
4. Move to a finer quantizer.
5. Once the quantizer can exceed 90, run its inner PCRD search.
6. Record the smallest feasible stream from that quantizer.
7. Probe a neighboring coarser/finer quantizer if budget permits.
8. Return the smallest exact stream among the feasible quantizer states examined.

Pseudo-code:

```rust
reference = metric.precompute(source);

rung = initial_rung(target, features);

loop {
    state = prepare_quantizer_state(rung);

    ceiling = score(state.all_passes());

    if ceiling < target {
        observe_infeasible(rung, ceiling);
        rung = next_finer_rung();
        continue;
    }

    candidate = find_minimum_pcrd_candidate(
        &state,
        target,
        &reference,
    );

    record_feasible(candidate);

    if should_probe_neighboring_quantizer() {
        rung = next_candidate_rung();
        continue;
    }

    return exact_smallest_verified_candidate();
}
```

---

# 14. Don't assume SSIMULACRA2 is perfectly monotonic

This matters.

Increasing:

* bitrate;
* retained coding passes; or
* quantizer fineness

will generally improve distortion.

But a perceptual metric can wobble slightly:

```text
89.82
90.06
89.99
90.14
```

because the reconstructed artifact pattern changes.

So don't write the search as a conventional mathematically strict binary search on SSIMULACRA2.

Use JPXL's philosophy:

```text
observed feasible points
observed infeasible points
local crossing estimate
bounded correction
```

and always select from **actual scored candidates**.

For the very last selection, inspect neighboring PCRD truncation points when practical.

Correctness should mean:

```text
winner.score >= requested_score
```

not:

```text
model predicted winner.score >= requested_score
```

---

# 15. Quantization should remain the primary quality tool

There is an easy but wrong first implementation:

```text
always quantize at q99
→ Tier 1 creates a massive amount of information
→ PCRD throws most of it away until SSIM target reached
```

That will prove the concept, and it might be useful as an intermediate milestone.

But don't ship it.

It wastes:

* Tier-1 work;
* memory;
* coding passes;
* potentially compression efficiency.

The JPEG committee's Qfactor guidance specifically calls out this issue: quality-driven quantization can reduce the work spent generating passes that would later be discarded by PCRD. ([JPEG][1])

So the final controller should seek a quantizer that is already near the perceptual operating point, then use PCRD for the last trimming.

That's also already conceptually present in your `DocumentTrim` profile.

---

# 16. This suggests a better JP2LAM rate architecture generally

Today you effectively have:

```text
RateMode::QualityLambda
RateMode::DocumentTrim
```

I would eventually move toward:

```rust
enum RateMode {
    LegacyQualityLambda,
    QuantizationDriven,
    PerceptualTarget,
    ExactRate,
}
```

with the long-term general photographic path becoming:

```text
perceptual target
    ↓
quantization-driven primary control
    ↓
perceptually weighted PCRD secondary trim
```

Once this proves itself, `QualityLambda` probably becomes legacy machinery rather than the central interpretation of photographic quality.

That's a larger architectural improvement than merely adding SSIMULACRA2.

---

# 17. First controller version: no predictor

This is important.

**Do not immediately port JPXL's predictor.**

For v1:

```text
target
↓
fixed neutral starting rung
↓
geometric search
↓
SSIM observations
↓
bracket
↓
crossing fit
```

Let it spend 5–8 evaluations if necessary.

The goal is to establish the actual JPEG 2000 response curve.

The predictor is an optimization, not part of quality correctness.

This is one of the lessons I'd carry over from the amount of work the JPXL scheduler required: **do not tune prediction and navigation simultaneously.**

---

# 18. Then train a JP2-specific predictor

This is the main work that genuinely needs to be done again.

JPXL's predictor uses things such as:

* dimensions;
* grayscale flag;
* luma variance quantiles;
* chroma variance;
* flat fraction;
* edge proxy;
* transform-domain statistics.

The generic pixel features are reusable.

The JPEG XL transform features are not.

For JP2LAM I would produce:

```rust
struct Jp2PerceptualFeatures {
    width: u32,
    height: u32,
    grayscale: bool,

    // generic source
    luma_variance_q10: f32,
    luma_variance_q50: f32,
    luma_variance_q90: f32,
    chroma_variance_q50: f32,
    flat_fraction: f32,
    edge_proxy: f32,

    // JPEG2000-native
    ll_energy: f64,
    highpass_energy: f64,
    high_low_ratio: f64,
    hh_fraction: f64,
    directional_asymmetry: f64,
    chroma_wavelet_ratio: f64,

    coefficient_near_zero_fraction: f64,
    coefficient_tail_q90: f64,
    coefficient_tail_q99: f64,
}
```

The DWT statistics are particularly attractive because the encoder is computing the transform anyway.

Don't do an extra analysis transform just for prediction.

---

# 19. Calibration tooling

Extend `examples/perceptual_curve.rs` into a proper oracle-generation tool.

For every source and target score, have an exhaustive/offline mode search enough quantizer/PCRD candidates to establish approximately:

```text
source
target_score
best_quantizer
best_pcrd_bytes
achieved_score
output_bytes
features...
```

For example:

```text
photo001.png,80,...
photo001.png,85,...
photo001.png,90,...
photo001.png,92.5,...
photo001.png,95,...
photo001.png,97.5,...
```

The production scheduler isn't allowed to exhaustively search.

The **offline trainer is**.

Then train:

```text
(source features, target score)
        ↓
predicted initial log_quant_scale
```

The desired predictor is not:

> predict filesize.

Nor:

> predict SSIMULACRA2.

It is:

> predict the codec control value likely to place the first candidate near the requested crossing.

That's exactly the valuable part of the JPXL predictor architecture.

---

# 20. Reuse JPXL's loss-domain modeling

I would explicitly reproduce this design.

Instead of fitting raw score:

```text
90 → 92 → 94 → 96 → 98
```

fit:

```text
100 - score

10
8
6
4
2
```

and operate in log space:

```text
ln(max(100 - score, ε))
```

This captures the fact that moving from:

```text
90 → 95
```

is very different from:

```text
95 → 100
```

even though both appear to be five score points.

This is one of the hard-won JPXL pieces I would consider already solved.

---

# 21. Which JPXL constants should you copy?

### Copy initially

These are sensible starting points:

```text
BRACKET_RATIO = 1.8
LOSS_EPSILON = 1e-3
DEFAULT_SCORE_GUARD = 0
```

### Copy provisionally and measure

```text
PRIOR_LOSS_EXPONENT = 0.9
EXPANSION_MARGIN = 1.25
MAX_EXPANSION_JUMP = 16
```

### Don't copy conceptually

Anything involving:

* VarDCT structural rebuilding;
* cover/CfL;
* effective-scale/JXL quantizer definitions;
* JXL case-predictor tables;
* JXL one-shot coefficients;
* JXL surrogate rendering assumptions.

Those solve JXL problems, not perceptual-controller problems.

---

# 22. The existing perceptual PCRD should remain below the SSIM controller

The hierarchy I'd aim for is:

```text
SSIMULACRA2
global objective / acceptance test
        │
        ▼
quantizer controller
global loss severity
        │
        ▼
existing perceptual PCRD
decides where the available bytes matter most
        │
        ▼
Tier-1 truncation
```

Your existing perceptual weighting was already shown to improve SSIMULACRA2 at equal size around important operating points.

So don't throw that work away.

In fact the new SSIM controller makes it more valuable, because:

```text
old:
same nominal q → perceptually better allocation

new:
same actual perceptual quality → potentially fewer bytes
```

That second measurement is what ultimately matters.

---

# 23. New qualification should be score-matched, not q-matched

The August work measured:

```text
same bytes
→ compare SSIMULACRA2
```

That's useful for PCRD.

The controller's test is different:

```text
same SSIMULACRA2
→ compare bytes
```

For each image and target:

| Encoder/path                      | Target | Achieved |  Bytes |
| --------------------------------- | -----: | -------: | -----: |
| legacy JP2 q                      |     90 |     90.1 | 800 KB |
| SSIM controller + baseline PCRD   |     90 |     90.2 | 730 KB |
| SSIM controller + perceptual PCRD |     90 |     90.1 | 690 KB |

That will tell you whether each part is actually helping the thing you care about.

---

# 24. Resolution stability should be a first-class acceptance criterion

The existing JP2LAM notes already identify resolution instability as unfinished work.

SSIMULACRA2 targeting gives you a much cleaner way out.

The desired test is:

```text
same image/content at:
    0.8 MP
    4 MP
    12 MP
    50 MP

target = 90
```

and every one should come out approximately:

```text
SSIMULACRA2 >= 90
```

without having to invent resolution-specific `quality_to_lambda()` corrections.

The amount of data will still differ.

That's fine.

The point is that:

```text
quality = 90
```

should describe **appearance**, not some internal encoder parameter that happens to work on one resolution.

---

# 25. Scope v1 to formats where the metric has clear semantics

I'd initially support SSIM-target mode for:

* 8-bit grayscale;
* 8-bit sRGB;
* 16-bit grayscale/sRGB if your shared metric adapter is exact there.

For grayscale, replicate into the metric's RGB domain consistently.

I would initially reject or fall back for ambiguous:

* raw CMYK without a well-defined display conversion;
* unusual component models;
* auxiliary channel combinations.

SSIMULACRA2 needs a meaningful visual reference.

Don't quietly score CMYK channel numbers as though they're RGB.

For the PDF use case this isn't much of a limitation; Lege's rendered page path can feed normalized gray/RGB JP2 images.

PDF itself standardizes JPEG 2000 through the `JPXDecode` image filter, including constraints on the JPX feature set used in image XObjects. ([PDF Association][3])

---

# 26. PDF/MRC gives you one additional opportunity

There are actually **two levels** at which you can use this work.

### Inside JP2LAM

JP2LAM receives a background image:

```text
background pixels → target SSIM → JP2
```

It optimizes faithfully against its own input.

That's the generic codec behavior.

### Inside `lege-process`

For MRC you eventually know:

```text
candidate JP2 background
+
preserved foreground
+
preserved JBIG2 mask
=
final PDF page
```

So a later Lege-level controller could score the **composited final page**, not merely the naked background.

That is more intelligent because it avoids spending JP2 bits reproducing pixels hidden beneath opaque foreground/mask content.

For example:

```text
original page
        ↕ SSIMULACRA2
candidate background
 + existing foreground
 + unchanged mask
```

This is not required for JP2LAM itself.

But your preservation architecture makes it a particularly interesting second-stage optimization.

---

# 27. Recommended implementation sequence

I would hand the coding agent these milestones, in this order:

1. **Extract/shared SSIMULACRA2 core.** Make JPXL and JP2LAM use precisely the same production metric implementation and metric version. Establish parity against the external SSIMULACRA2 crate.

2. **Add `RateControl::Perceptual`.** No changes yet to legacy `Quality(u8)`. Add target, effort, status, observations and diagnostic trace types.

3. **Implement the exact slow evaluator.** Build candidate codestream → decode with `Jp2Decoder` → score. Precompute source-side SSIM data once per encode.

4. **Implement PCRD-only proof of concept.** Use a fixed fine quantizer and vary only stored-pass truncation. This proves target semantics, score bracketing and exact-finalist behavior without solving quantizer prediction yet.

5. **Port JPXL navigation math.** Bring across log-loss crossing, geometric expansion, bounded corrections and work budgets. Remove all JXL-specific candidate/structure assumptions.

6. **Add global quantization rung.** Make existing subband quants accept a continuous/global multiplier. Outer-loop it around the inner PCRD search.

7. **Optimize candidate reconstruction.** Reuse decoder Tier-1 + inverse DWT machinery directly over stored selections; prove pixel identity against serialize+decode before enabling it.

8. **Cache DWT/source analysis where practical.** A new global quantizer should not require recomputing source analysis. If memory permits, retain unquantized DWT coefficients and requantize them for outer probes.

9. **Build exhaustive calibration mode.** Generate the JP2-specific oracle dataset across source type, resolution, score and quantizer/PCRD combinations.

10. **Train initial-rung predictor.** Reuse generic JPXL source features, add cheap DWT features, but fit entirely new JP2 coefficients.

11. **Retune navigation constants only after predictor works.** Measure probe counts, undershoots, overshoots and byte regret. Don't optimize blindly.

12. **Map legacy qualities/presets onto score targets.** Once stable, decide what `PhotoCompact`, `PhotoHigh`, etc. should mean perceptually and whether `Quality(u8)` should become a compatibility projection onto target scores.

13. **Integrate with Lege Preservation.** Let strict JP2 PDF output request actual perceptual targets rather than arbitrary JP2 quality numbers.

---

# 28. Acceptance criteria

I would not call the work complete until all of these hold.

**Correctness**

```text
Every successful perceptual encode:
    achieved canonical SSIMULACRA2 >= requested target.
```

**Metric equivalence**

```text
in-loop candidate score
≈
score of emitted JP2 decoded through canonical decoder
```

Ideally identical within deterministic floating-point expectations.

**Search boundedness**

```text
Fast/Balanced/Quality never exceed published probe budgets
except explicitly documented rescue behavior.
```

**Resolution stability**

Same target retains approximately the same perceptual score across major resolution tiers.

**Prediction is optional**

Disabling the predictor still reaches correct outputs; it merely consumes more probes.

**No quality regression from optimization**

Fast internal candidate reconstruction selects the same winner as the slow serialize/decode reference path.

**Rate efficiency**

The new controller beats or at least matches the smallest legacy `Quality(q)` output that reaches the same SSIMULACRA2 target.

**Existing exact-rate behavior remains unchanged**

```text
TargetBytes
TargetBitsPerPixel
CompressionRatio
```

should not silently acquire a new perceptual policy as part of this change.

---

# 29. How much of the painful JPXL tuning must actually be repeated?

This is the useful distinction:

### You do **not** need to rediscover

* what quality means;
* how to score it;
* source-reference precomputation;
* how to structure a score floor;
* how to bracket a target;
* why `100-score` is the useful error domain;
* how to interpolate in log space;
* how to bound probes;
* how to distinguish work-cap failure from saturation;
* how to exact-verify finalists;
* how to instrument/calibrate the controller;
* how to separate prediction from correctness.

That was a large part of the conceptual difficulty of JPXL.

### You **do** have to rediscover

```text
source + SSIM target
        ↓
best JPEG2000 quantizer neighborhood
```

and, to a lesser extent:

```text
quantizer + SSIM target
        ↓
optimal PCRD truncation neighborhood
```

The second should be relatively tractable because JP2LAM can cheaply explore stored coding passes.

So I would characterize this as **a new calibration project, not a new controller research project**.

And there is a plausible outcome where JP2LAM ends up with a *simpler* production scheduler than JPXL: JPEG 2000 was explicitly designed around quantization plus post-compression rate-distortion truncation, and its own current quality-control guidance describes using those two mechanisms together. ([JPEG][1])

The architectural target I would use is therefore:

```text
                   SHARED ACROSS CODECS
                 ┌─────────────────────┐
                 │    SSIMULACRA2      │
                 │  reference + score  │
                 └──────────┬──────────┘
                            │
                 ┌──────────▼──────────┐
                 │ generic controller │
                 │ target / bracket / │
                 │ crossing / budgets │
                 └──────────┬──────────┘
                            │
        ┌───────────────────┴───────────────────┐
        │                                       │
        ▼                                       ▼
 JPEG XL adapter                         JPEG 2000 adapter
 VarDCT quantizer                       global DWT quantizer
 structure search                             +
 entropy price                          perceptual PCRD
        │                                truncation search
        ▼                                       │
       JXL                                      ▼
                                                JP2
```

I would **share the metric outright, share the controller mathematics aggressively, but deliberately keep the codec-control adapters separate**. Trying to share the final scheduler wholesale would force JPEG 2000 into JPEG XL's optimization model and squander one of JPEG 2000's strongest architectural features—its post-compression truncation machinery.

[1]: https://ds.jpeg.org/documents/jpeg2000/wg1n100430-098-COM-Guideline_on_controlling_JPEG_2000_image_quality_using_a_single_parameter.pdf?utm_source=chatgpt.com "ISO-IEC JTC 1-SC 29-WG 1_wg1n100430-098-COM-Guideline on controlling JPEG 2000 image quality using a single parameter (Qfactor)"
[2]: https://github.com/uclouvain/openjpeg/blob/master/src/lib/openjp2/tcd.c?utm_source=chatgpt.com "openjpeg/src/lib/openjp2/tcd.c at master · uclouvain/openjpeg · GitHub"
[3]: https://pdfa.org/iso-32000-22020-clause-7-syntax/?utm_source=chatgpt.com "ISO 32000-2:2020 Clause 7: Syntax – PDF Association"
