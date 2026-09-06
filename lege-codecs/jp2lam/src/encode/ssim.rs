//! Slow exact perceptual evaluator.
//!
//! A candidate is serialized (or already a JP2/J2K buffer), decoded with
//! jp2lam's own decoder, converted to viewer-visible linear RGB, and scored
//! with the pinned SSIMULACRA2 implementation. Source-side metric work is
//! precomputed once.

use std::time::Instant;

use jpxl_perceptual::{
    LinearRgbView, METRIC_VERSION, PrecomputedReference, ReferenceRetention, SerialExecutor,
    Ssimulacra2,
};

use crate::decode::{DecodeLimits, DecodeRequest, DecodeResult, Jp2Decoder};
use crate::error::{Jp2LamError, Result};
use crate::model::{
    ColorSpace, DisplayColor, DisplayProfile, Image, ImageView, PerceptualEffort,
    PerceptualObservation, PerceptualTarget,
};

/// Pixel probes (reconstruct + metric evaluations) the perceptual search has spent,
/// process-global. Reset it before an encode to attribute probes to that encode; the
/// controller only writes it, nothing reads it in production.
pub static PERCEPTUAL_PROBES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    static ACHIEVED_SCORE: std::cell::Cell<f64> = const { std::cell::Cell::new(f64::NAN) };
}

/// SSIMULACRA2 score of the stream the last perceptual encode *on this thread*
/// shipped, `NaN` before the first one. Same contract as [`PERCEPTUAL_PROBES`]:
/// the controller only writes it, and a test or bench reads the encoder's own
/// number back to check it against an independent measurement. Thread-local
/// rather than a global so parallel tests do not read each other's encodes.
#[must_use]
pub fn last_achieved_score() -> f64 {
    ACHIEVED_SCORE.with(std::cell::Cell::get)
}

pub(crate) fn record_achieved_score(score: f64) {
    ACHIEVED_SCORE.with(|cell| cell.set(score));
}

pub(crate) const LOSS_EPSILON: f64 = 1e-3;
/// Shape of the measured crossing curve in `ln(loss)`, shared by the first-probe
/// predictor and by every correction that moves a probe to another score
/// (see [`prior_body_fraction`] and [`crossing_ratio`]).
pub(crate) const LOSS_EXPONENT: f64 = -0.168_855;
pub(crate) const LOSS_CURVATURE: f64 = -0.124_717;
pub(crate) const MAX_EXPANSION_JUMP: f64 = 16.0;
/// Smallest relative climb from the largest infeasible probe while bracketed.
pub(crate) const MIN_BRACKET_STEP: f64 = 1.05;

pub(crate) struct QualityBudget {
    pub pixel_probes: u32,
}

impl QualityBudget {
    pub(crate) fn for_effort(effort: PerceptualEffort) -> Self {
        match effort {
            PerceptualEffort::Fast => Self { pixel_probes: 3 },
            PerceptualEffort::Balanced => Self { pixel_probes: 5 },
            PerceptualEffort::Quality => Self { pixel_probes: 10 },
        }
    }

    /// Budget for one target. A display-conditioned target is capped at one encode plus
    /// two corrections whatever `effort` says: the metric is cheap and permissive at
    /// display size, so a long search buys nothing. (A profile that conditions nothing
    /// for the source never reaches here: the plan drops it, see
    /// [`DisplayProfile::conditions`].)
    ///
    /// The second correction costs nothing on a well-predicted encode -- the search stops
    /// the moment a probe lands inside the accept band -- and is what stops a badly
    /// predicted one shipping its first probe. With one correction, a first probe that
    /// overshoots by 20 points has to be fixed by a single extrapolated shrink; when that
    /// shrink lands under the floor there is no budget to interpolate the bracket it just
    /// created, so the fat first probe ships. Measured on the 2026-09-05 page corpus
    /// (472x800 crops, the shape a real reader emits), that case was 1.5x to 2.5x the
    /// bytes of the same floor at source resolution.
    pub(crate) fn for_target(target: &PerceptualTarget) -> Self {
        if target.display.is_some() {
            Self { pixel_probes: 2 }
        } else {
            Self::for_effort(target.effort)
        }
    }

    /// Navigation cap plus one documented rescue probe.
    pub(crate) fn max_evaluations(self) -> u32 {
        self.pixel_probes.saturating_add(1)
    }
}

/// Outer quantizer rungs (`Quality(u8)` steps) tried in order for a target score.
///
/// Tuned on the 2026-09-05 Session 8c oracle sweep (12 full-grid sources, 0.8-12 MP,
/// `ssimulacra2-jpxl-1`): the first rung is the byte-cheapest quantizer that meets the
/// band on that corpus, the rest are finer fallbacks for sources whose all-pass ceiling
/// misses. The old fixed `[75, 90, 99]` cost ~24% extra bytes at 80 and ~6% at 95, and
/// q99 was never the cheapest rung at any target (q95 already scores 100 all-pass).
pub(crate) fn outer_rungs_for_target(score: f64) -> [u8; 3] {
    // ponytail: five hand-read bands from the sweep; replace with the Session 9 predictor
    // if per-source features ever move the best rung more than the ~2% band spread.
    if score < 87.5 {
        [60, 75, 90]
    } else if score < 94.0 {
        [75, 90, 95]
    } else if score < 96.0 {
        [50, 75, 90]
    } else if score < 98.0 {
        [70, 90, 95]
    } else {
        [85, 92, 95]
    }
}

/// Lowest all-pass SSIMULACRA2 score a rung reached on the Session 8c corpus, minus a
/// safety margin. A target at or below it makes the ceiling probe a foregone conclusion,
/// so the inner search may seed its first aim from this estimate instead of spending an
/// evaluation. `None` for rungs the sweep did not cover: probe as before.
pub(crate) fn safe_ceiling_for_rung(quality: u8) -> Option<f64> {
    const MARGIN: f64 = 1.5;
    let floor = match quality {
        50 => 95.3,
        60 => 96.5,
        70 => 97.4,
        75 => 97.7,
        80 => 97.9,
        85 => 98.1,
        90 => 98.3,
        92 => 98.9,
        95..=99 => 99.9,
        _ => return None,
    };
    Some(floor - MARGIN)
}

/// Smallest margin above the target the crossing aims at, whatever the effort's
/// loss-relative reserve works out to.
pub(crate) const MIN_AIM_MARGIN: f64 = 0.25;

/// Fraction of the remaining loss the crossing aims above the target. Ported from
/// JPXL's quality navigator (`reserve` per effort): a correction aimed exactly at the
/// target lands infeasible half the time and costs a bracket probe, so aim slightly
/// past it and accept anything inside the band below. A display-conditioned target
/// gets exactly one correction, which therefore has to land feasible, so it never
/// reserves less than the widest preset does.
///
/// Balanced was re-measured on the 2026-09-05 probe corpus (seven photos x floors
/// 70-90) once the first-probe predictor got its per-source term: at 0.03 the accept
/// band was narrower than the predictor's own spread, so a first probe that was already
/// good enough still bought a correction. Widening it to 0.045 costs 0.5% bytes and
/// saves 0.63 probes per encode (2.54 -> 1.91). Fast stays at 0.06 and buys its probes
/// with a wider band instead -- see [`accept_band`].
pub(crate) fn reserve_for_target(target: &PerceptualTarget) -> f64 {
    const DISPLAY_RESERVE: f64 = 0.06;
    let reserve: f64 = match target.effort {
        PerceptualEffort::Fast => 0.06,
        PerceptualEffort::Balanced => 0.045,
        PerceptualEffort::Quality => 0.02,
    };
    if target.display.is_some() {
        reserve.max(DISPLAY_RESERVE)
    } else {
        reserve
    }
}

/// Score margin above the floor the search steers at, loss-relative so it is a
/// constant width in the controller's log-loss coordinate rather than a fixed number
/// of points (1.2 at target 80 for Fast, 0.9 for Balanced).
pub(crate) fn aim_margin(target: &PerceptualTarget) -> f64 {
    (reserve_for_target(target) * loss(target.score)).max(MIN_AIM_MARGIN)
}

/// Score the search aims a correction at: the floor plus the effort's margin.
pub(crate) fn aim_score(target: &PerceptualTarget) -> f64 {
    target.score + aim_margin(target)
}

/// Overshoot a feasible stream may carry and still end the search: JPXL's
/// reserve-coupled accept band, whose fixed one-point band excluded its own aim at low
/// targets and never stopped there. Twice the aim margin centres the aim in the band, so
/// a probe landing where a correction pointed always stops.
///
/// Fast takes three times the margin instead. It is the preset that trades bytes for
/// wall clock, and a first probe that overshoots by less than a correction's own reserve
/// is already good enough to ship: on the 2026-09-05 probe corpus the wider band takes
/// Fast from 1.69 to 1.43 probes per encode for 2.0% more bytes. Buying the same probes
/// by raising Fast's reserve to 0.09 instead was measured on the same corpus and cost
/// 4.4% -- a fatter aim pays on every encode, a wider band only on the ones it stops.
/// The cost is bounded by the band: nothing outside it is ever accepted.
pub(crate) fn accept_band(target: &PerceptualTarget) -> f64 {
    let width = if matches!(target.effort, PerceptualEffort::Fast) {
        3.0
    } else {
        2.0
    };
    width * aim_margin(target)
}

/// First-probe scale for a display-conditioned target: the same body scores far
/// higher when both sides are downscaled to the panel, so the source-resolution
/// crossing prior starts several times too large.
///
/// Measured on the 2026-09-05 display corpus (eleven photos, 0.8/4.3/6.0 MP, into
/// 400x300, 600x450 and 800x600, floors 60-80, 112 converged crossings read from a
/// raised-budget build), against the source-resolution prior above:
/// `ln scale = c + 0.559*ln(display px) + 0.298*ln(E_hp/px) + 0.889*ln(E_ll/px)`.
///
/// The two band energies are the whole point, and they are the same ones the
/// source-resolution prior reads out of the same DWT pass. Fitted on the pixel counts
/// alone the residual is 0.87 in ln, because a soft source loses almost nothing to the
/// downscale and crosses ten times lower than a detailed one at the same panel -- no
/// function of the two pixel counts can see that. The band energies see it directly:
/// leave-one-source-out over all eleven sources, 0.871 -> 0.247, with the fitted
/// coefficients stable across every fold (ln display px 0.54..0.58, ln E_hp 0.26..0.34,
/// ln E_ll 0.82..0.97) and no residual worse than 0.50. Per-panel bias is inside 0.03.
///
/// The source pixel count then earns nothing (its coefficient falls to -0.12 and LOSO
/// does not improve): the energies are already per source pixel, and the scale is a
/// fraction of the rung's own stored bytes, which grow with the source. Adding the
/// near-zero fraction or the rung's bytes per pixel on top buys 0.01 and is not worth
/// two more constants.
///
/// `FEASIBLE_FIRST_BIAS` keeps a deliberate fat bias: a display target gets one
/// correction, so a first probe that is already shippable is worth more than an unbiased
/// one. At 0.6 of the residual sd, roughly one first probe in four still lands under the
/// floor and spends that correction.
///
/// A reversible transform has no 9/7 planes to read, and falls back to the pixel-count
/// form (`c + 0.577*ln(display px) - 0.652*ln(source px)`, residual 0.87) refitted
/// against the same prior.
///
/// The fit is only measured for ratios of 0.02 to 0.6; the `ratio..=1.0` clamp keeps the
/// extrapolation sane outside that (a barely-downscaled small source must not be told to
/// spend a fraction of the bytes the same image needs at full size).
pub(crate) fn display_prior_scale(
    target: &PerceptualTarget,
    width: u32,
    height: u32,
    bands: Option<SourceBands>,
) -> f64 {
    const FEASIBLE_FIRST_BIAS: f64 = 0.15;
    let Some(profile) = target.display else {
        return 1.0;
    };
    let (metric_width, metric_height) = profile.metric_size(width, height);
    let source = f64::from(width) * f64::from(height);
    let displayed = f64::from(metric_width) * f64::from(metric_height);
    if source <= 0.0 || displayed <= 0.0 || displayed >= source {
        return 1.0;
    }
    let log_scale = match bands {
        Some(bands) => {
            // Fitted intercept -10.140_4 in per-pixel units, restated per coefficient.
            const INTERCEPT: f64 = -8.836_6;
            const PER_LOG_DISPLAY_PIXELS: f64 = 0.559_2;
            const PER_LOG_HIGHPASS_ENERGY: f64 = 0.298_2;
            const PER_LOG_LL_ENERGY: f64 = 0.888_6;
            INTERCEPT
                + PER_LOG_DISPLAY_PIXELS * displayed.ln()
                + PER_LOG_HIGHPASS_ENERGY * bands.highpass_per_sample.max(1e-6).ln()
                + PER_LOG_LL_ENERGY * bands.ll_per_sample.max(1e-6).ln()
        }
        None => {
            const INTERCEPT: f64 = 0.987_4;
            const PER_LOG_DISPLAY_PIXELS: f64 = 0.577_1;
            const PER_LOG_SOURCE_PIXELS: f64 = -0.651_9;
            INTERCEPT
                + PER_LOG_DISPLAY_PIXELS * displayed.ln()
                + PER_LOG_SOURCE_PIXELS * source.ln()
        }
    };
    (log_scale + FEASIBLE_FIRST_BIAS)
        .exp()
        .clamp(displayed / source, 1.0)
}

/// Per-source statistics of the unquantized 9/7 planes, all read in the one pass the
/// search already makes over the DWT cache (`ssim_oracle::source_band_stats`).
///
/// Everything here is per *coefficient*, not per pixel. The corpus the constants are
/// fitted on is entirely three-component 4:4:4, so the two normalizations differ only by
/// a constant there -- but a grayscale source has a third of the samples, and reading its
/// stored bytes per pixel as "a third as many, so an easy image" made the first probe
/// two and a half times too fat. That is invisible at source resolution, where the
/// budget corrects it, and ships as-is on a display target, which gets one correction.
#[derive(Clone, Copy)]
pub(crate) struct SourceBands {
    /// Fraction of coefficients below the near-zero threshold.
    pub near_zero: f64,
    /// LL band energy per coefficient.
    pub ll_per_sample: f64,
    /// HL+LH+HH band energy per coefficient.
    pub highpass_per_sample: f64,
    /// Unquantized coefficients in the image.
    pub samples: f64,
}

/// Per-source inputs to the first-probe predictor, all free at the point of use.
#[derive(Clone, Copy)]
pub(crate) struct PriorFeatures {
    /// `None` for a transform that has no unquantized 9/7 planes.
    pub bands: Option<SourceBands>,
    /// This rung's total stored Tier-1 body bytes.
    pub body_bytes: f64,
}

/// Crossing body bytes as a fraction of the rung's total stored pass bytes.
///
/// Fitted on the Session 8c oracle sweep extended down to body fraction 0.05 on all
/// twelve sources (2026-09-05; 0.8-12 MP, rungs 50-97, targets 60-95, 704 measured
/// crossings): `ln fraction = a + b*quality + c*ln(loss) + d*ln(loss)^2`, plus the three
/// per-source terms below. Expressing the prior relative to the rung's own stored bytes
/// rather than as an absolute bits-per-pixel cuts the first-probe log error -- the
/// stored Tier-1 layout already measures how hard this image is, for free.
///
/// The quadratic loss term keeps the per-target in-sample bias inside 0.02 in ln
/// fraction everywhere from 60 to 95. A cubic term buys nothing (LOSO unchanged at
/// 0.146).
///
/// The three per-source terms all come out of the one pass the search already makes
/// over the unquantized DWT cache. Leave-one-source-out over all twelve sources, on the
/// union sweep:
///
/// | model                              | in-sample sd | leave-one-source-out sd |
/// |------------------------------------|--------------|-------------------------|
/// | quality + loss only                | 0.197        | 0.212                   |
/// | + near-zero fraction               | 0.157        | 0.168                   |
/// | + near-zero + ln bytes/px          | 0.156        | 0.173                   |
/// | + near-zero + ln bytes/px + ln E_hp| 0.131        | 0.146                   |
///
/// The near-zero fraction and the rung's stored bytes per pixel are collinear
/// (r = -0.67) and on this data the pair alone is no better than near-zero alone; the
/// highpass band energy per pixel is what makes all three earn their place, and the
/// fitted coefficients are then stable across every fold (near-zero -1.97..-1.62,
/// ln bytes/px -0.93..-0.79, ln E_hp 0.15..0.19), which the previous two-term fit was
/// not -- its ln bytes/px coefficient collapsed from -0.70 to -0.09 when the
/// large-source low-fraction data arrived. The only alternative that fits as well
/// (`ln edge_proxy`, 0.146) needs a pass over source pixels; the band energies are
/// already in the cache.
///
/// What the 2026-09-05 large-source re-sweep bought is not visible in the pooled sd: it
/// is the extrapolation below fraction 0.25 on the 4-12 MP sources, which the previous
/// grid never measured. On the 98 large-source crossings at target <= 80 and rung <= 75
/// -- exactly where the production ladder puts its first probe -- the old constants were
/// biased -0.138 in ln with sd 0.307; these are biased -0.027 with LOSO sd 0.223.
///
/// A reversible transform has no 9/7 coefficients to count, so that path keeps the
/// pooled quality-and-loss constants (LOSO 0.212). Substituting corpus means for the
/// per-source terms instead scores far worse than the pooled fit, so it is not done.
/// The statistics are taken from the unquantized DWT cache when there is one and
/// recomputed per tile when there is not, because the cache is a memory optimisation and
/// `ssim_dwt::cached_search_matches_recompute_winner` requires it not to move the winner.
pub(crate) fn prior_body_fraction(quality: u8, target: f64, features: PriorFeatures) -> f64 {
    let log_loss = loss(target).ln();
    let log_fraction = match features.bands {
        Some(bands) => {
            // Fitted intercept 0.699_834 in per-pixel units, restated per coefficient.
            const INTERCEPT: f64 = -0.046_161;
            const PER_QUALITY: f64 = -0.002_484;
            const PER_NEAR_ZERO: f64 = -1.711_927;
            const PER_LOG_BYTES_PER_SAMPLE: f64 = -0.846_510;
            const PER_LOG_HIGHPASS_ENERGY: f64 = 0.167_452;
            INTERCEPT
                + PER_QUALITY * f64::from(quality)
                + LOSS_EXPONENT * log_loss
                + LOSS_CURVATURE * log_loss * log_loss
                + PER_NEAR_ZERO * bands.near_zero
                + PER_LOG_BYTES_PER_SAMPLE
                    * (features.body_bytes / bands.samples.max(1.0))
                        .max(1e-6)
                        .ln()
                + PER_LOG_HIGHPASS_ENERGY * bands.highpass_per_sample.max(1e-6).ln()
        }
        None => {
            const INTERCEPT: f64 = 1.282_624;
            const PER_QUALITY: f64 = -0.018_937;
            const POOLED_LOSS_EXPONENT: f64 = -0.046_504;
            const POOLED_LOSS_CURVATURE: f64 = -0.143_456;
            INTERCEPT
                + PER_QUALITY * f64::from(quality)
                + POOLED_LOSS_EXPONENT * log_loss
                + POOLED_LOSS_CURVATURE * log_loss * log_loss
        }
    };
    log_fraction.exp().clamp(0.02, 1.0)
}

/// Body bytes to try next after a feasible probe with no infeasible bracket yet: the
/// loss-model shrink without the upward expansion margin.
pub(crate) fn shrink_body_bytes(body: u32, score: f64, target: f64) -> u32 {
    let ratio = crossing_ratio(score, target);
    (f64::from(body) * ratio)
        .round()
        .clamp(1.0, f64::from(body.saturating_sub(1).max(1))) as u32
}

pub(crate) fn loss(score: f64) -> f64 {
    (100.0 - score).max(LOSS_EPSILON)
}

/// Body-byte multiplier that moves a probe scoring `score` to `target`, read off the
/// same crossing curve the first probe is predicted from: the per-source terms cancel,
/// leaving only the measured loss shape.
///
/// This replaces a fixed `loss^(1/0.9)` power law. That law is accurate around target
/// 70, where it was read off, and progressively too fat above it -- on the union oracle
/// sweep (704 crossings, twelve sources) its ratio was biased -0.135 in ln from 85 to 90
/// and -0.415 from 85 to 95, so a correction from just under a high floor overshot by
/// half and cost two more probes coming back down. The curve form is unbiased everywhere
/// the sweep measures (|bias| <= 0.03 in ln, 60 through 95) at the same spread.
pub(crate) fn crossing_ratio(score: f64, target: f64) -> f64 {
    let from = loss(score).ln();
    let to = loss(target).ln();
    (LOSS_EXPONENT * (to - from) + LOSS_CURVATURE * (to * to - from * from)).exp()
}

/// Body bytes to try after an infeasible probe with no feasible bracket yet.
///
/// Aims straight at the target: the old `* 1.25` safety margin on top existed because
/// the power-law ratio it multiplied was already too fat, and on the 2026-09-05 probe
/// corpus removing it takes the 4-6 MP sources from 3.29 probes per encode to 2.62 and
/// the 0.8 MP set from 1.60 to 1.56, for 0.7% fewer bytes. An expansion that still lands
/// short is bracketed by `MIN_BRACKET_STEP` from below, which is the cheaper guard.
pub(crate) fn aim_body_bytes(body: u32, score: f64, target: f64) -> u32 {
    let ratio = crossing_ratio(score, target);
    let aimed = f64::from(body) * ratio;
    let capped = aimed.min(f64::from(body) * MAX_EXPANSION_JUMP);
    capped.round().clamp(1.0, f64::from(u32::MAX)) as u32
}

pub(crate) fn interpolate_body_bytes(lo: (u32, f64), hi: (u32, f64), target: f64) -> Option<u32> {
    let (lo_body, lo_score) = lo;
    let (hi_body, hi_score) = hi;
    if lo_body == hi_body {
        return None;
    }
    let x0 = f64::from(lo_body).ln();
    let x1 = f64::from(hi_body).ln();
    let y0 = -loss(lo_score).ln();
    let y1 = -loss(hi_score).ln();
    let yt = -loss(target).ln();
    let span = y1 - y0;
    if !span.is_finite() || span.abs() < 1e-12 {
        return None;
    }
    let fraction = ((yt - y0) / span).clamp(0.0, 1.0);
    let x = x0 + fraction * (x1 - x0);
    Some(x.exp().round().clamp(1.0, f64::from(u32::MAX)) as u32)
}

/// Scores reconstructed streams against one precomputed source.
pub struct StreamEvaluator {
    decoder: Jp2Decoder,
    reference: PrecomputedReference,
    metric: Ssimulacra2,
    linear: [Vec<f32>; 3],
    conditioner: Option<DisplayConditioner>,
    width: u32,
    height: u32,
    /// Size the metric actually runs at, after display conditioning.
    metric_width: u32,
    metric_height: u32,
    /// Candidate scratch for a stream that already decodes at the metric size
    /// (a display-resampled encode), allocated on first such score.
    candidate: Option<[Vec<f32>; 3]>,
    max_pixels: u64,
    evaluations: u32,
}

impl StreamEvaluator {
    /// Precomputes source-side SSIMULACRA2 data. Gray is replicated into RGB.
    pub fn from_view(image: ImageView<'_>) -> Result<Self> {
        Self::from_view_ref(&image)
    }

    pub fn from_view_ref(image: &ImageView<'_>) -> Result<Self> {
        Self::new(image, None)
    }

    /// Precomputes source-side data conditioned to what the reader will see:
    /// the source is downscaled into `display` (fit inside, never upscaled) and,
    /// for e-ink, folded to display luminance. Scores are then SSIMULACRA2 at
    /// display size. An image that already fits a color display is scored
    /// exactly as [`Self::from_view`] would.
    pub fn for_display(image: ImageView<'_>, display: DisplayProfile) -> Result<Self> {
        Self::new(&image, Some(display))
    }

    pub(crate) fn for_target(image: &ImageView<'_>, target: &PerceptualTarget) -> Result<Self> {
        Self::new(image, target.display)
    }

    fn new(image: &ImageView<'_>, display: Option<DisplayProfile>) -> Result<Self> {
        let _p = crate::encode::profile_enter("ssim::evaluator_new");
        let width = image.width;
        let height = image.height;
        let pixels = usize::try_from(u64::from(width) * u64::from(height))
            .map_err(|_| Jp2LamError::InvalidInput("image area exceeds usize".into()))?;
        let mut linear = [vec![0.0; pixels], vec![0.0; pixels], vec![0.0; pixels]];
        source_to_linear_ref(image, &mut linear)?;
        let mut conditioner =
            display.and_then(|profile| DisplayConditioner::new(width, height, profile));
        let (metric_width, metric_height) = match &mut conditioner {
            Some(conditioner) => {
                conditioner.apply(&linear, width, height);
                (conditioner.width, conditioner.height)
            }
            None => (width, height),
        };
        let planes = conditioner.as_ref().map_or(&linear, |c| &c.out);
        let view = LinearRgbView::new(
            metric_width,
            metric_height,
            &planes[0],
            &planes[1],
            &planes[2],
        )
        .map_err(|err| Jp2LamError::InvalidInput(err.to_string()))?;
        let reference =
            PrecomputedReference::new(view, ReferenceRetention::Moments, &SerialExecutor)
                .map_err(|err| Jp2LamError::InvalidInput(err.to_string()))?;
        Ok(Self {
            decoder: Jp2Decoder::new(),
            reference,
            metric: Ssimulacra2::new(),
            linear,
            conditioner,
            width,
            height,
            metric_width,
            metric_height,
            candidate: None,
            max_pixels: u64::from(width) * u64::from(height),
            evaluations: 0,
        })
    }

    /// Number of stream scores performed (pixel probes).
    #[must_use]
    pub fn evaluations(&self) -> u32 {
        self.evaluations
    }

    /// Pinned metric identity the scores come from.
    #[must_use]
    pub fn metric_version(&self) -> &'static str {
        METRIC_VERSION
    }

    /// Decode `bytes` with jp2lam and score against the precomputed source.
    pub fn score_stream(&mut self, bytes: &[u8]) -> Result<PerceptualObservation> {
        self.evaluations = self.evaluations.saturating_add(1);
        let reconstruct_started = Instant::now();
        let decoded = self.decode_native(bytes)?;
        let reconstruct_millis = millis_since(reconstruct_started);
        self.score_decoded(&decoded, reconstruct_millis)
    }

    /// Score an already reconstructed raster (internal candidate path).
    pub(crate) fn score_image(
        &mut self,
        image: &Image,
        reconstruct_millis: Option<u64>,
    ) -> Result<PerceptualObservation> {
        self.evaluations = self.evaluations.saturating_add(1);
        // Either the source size or, for a display-resampled encode, the metric
        // size; `score_decoded` routes on that and rejects anything else.
        self.score_decoded(image, reconstruct_millis.unwrap_or(0))
    }

    fn score_decoded(
        &mut self,
        decoded: &Image,
        reconstruct_millis: u64,
    ) -> Result<PerceptualObservation> {
        let _p = crate::encode::profile_enter("ssim::score_decoded");
        if (decoded.width, decoded.height) != (self.width, self.height) {
            return self.score_display_sized_candidate(decoded, reconstruct_millis);
        }
        decoded_to_linear(decoded, &mut self.linear)?;
        let (width, height) = match self.conditioner.as_mut() {
            Some(conditioner) => {
                conditioner.apply(&self.linear, self.width, self.height);
                (conditioner.width, conditioner.height)
            }
            None => (self.width, self.height),
        };
        let planes = self.conditioner.as_ref().map_or(&self.linear, |c| &c.out);
        let view = LinearRgbView::new(width, height, &planes[0], &planes[1], &planes[2])
            .map_err(|err| Jp2LamError::DecodeFailed(err.to_string()))?;
        // Timed from here so `metric_millis` is the SSIMULACRA2 pass itself, at
        // whatever size it actually runs at.
        let metric_started = Instant::now();
        let result = self
            .metric
            .score(&self.reference, view, &SerialExecutor)
            .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
        Ok(PerceptualObservation {
            score: result.score,
            reconstruct_millis: Some(reconstruct_millis),
            metric_millis: Some(millis_since(metric_started)),
        })
    }

    /// Score a stream that already decodes at the metric size: a display encode
    /// whose source was resampled to the panel. The reader sees exactly these
    /// pixels, so the only conditioning left is the e-ink luminance fold.
    fn score_display_sized_candidate(
        &mut self,
        decoded: &Image,
        reconstruct_millis: u64,
    ) -> Result<PerceptualObservation> {
        if (decoded.width, decoded.height) != (self.metric_width, self.metric_height) {
            return Err(Jp2LamError::DecodeFailed(format!(
                "decoded {}x{} is neither the source {}x{} nor the metric size {}x{}",
                decoded.width,
                decoded.height,
                self.width,
                self.height,
                self.metric_width,
                self.metric_height
            )));
        }
        let mut planes = self.candidate.take().unwrap_or_else(|| {
            let pixels = self.metric_width as usize * self.metric_height as usize;
            [vec![0.0; pixels], vec![0.0; pixels], vec![0.0; pixels]]
        });
        let filled = decoded_to_linear(decoded, &mut planes);
        if filled.is_ok() && self.conditioner.as_ref().is_some_and(|c| c.gray) {
            for index in 0..planes[0].len() {
                let luma = display_luma(planes[0][index], planes[1][index], planes[2][index]);
                planes[0][index] = luma;
                planes[1][index] = luma;
                planes[2][index] = luma;
            }
        }
        let scored = filled.and_then(|()| {
            let view = LinearRgbView::new(
                self.metric_width,
                self.metric_height,
                &planes[0],
                &planes[1],
                &planes[2],
            )
            .map_err(|err| Jp2LamError::DecodeFailed(err.to_string()))?;
            let metric_started = Instant::now();
            let result = self
                .metric
                .score(&self.reference, view, &SerialExecutor)
                .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
            Ok(PerceptualObservation {
                score: result.score,
                reconstruct_millis: Some(reconstruct_millis),
                metric_millis: Some(millis_since(metric_started)),
            })
        });
        self.candidate = Some(planes);
        scored
    }

    fn decode_native(&mut self, bytes: &[u8]) -> Result<Image> {
        let decoded = self.decoder.decode(
            bytes,
            &DecodeRequest {
                limits: DecodeLimits {
                    max_input_bytes: bytes.len().saturating_add(1024 * 1024),
                    max_pixels: self.max_pixels.saturating_add(1),
                    max_working_bytes: usize::MAX,
                    ..DecodeLimits::default()
                },
                ..DecodeRequest::default()
            },
        )?;
        let DecodeResult::Native(image) = decoded else {
            return Err(Jp2LamError::DecodeFailed(
                "perceptual evaluator expected native planar decode".into(),
            ));
        };
        if (image.width, image.height) != (self.width, self.height)
            && (image.width, image.height) != (self.metric_width, self.metric_height)
        {
            return Err(Jp2LamError::DecodeFailed(format!(
                "decoded {}x{} is neither the source {}x{} nor the metric size {}x{}",
                image.width,
                image.height,
                self.width,
                self.height,
                self.metric_width,
                self.metric_height
            )));
        }
        Ok(image)
    }
}

/// Rec.709 luminance weights: linear RGB to display gray.
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// Display luminance of one linear-RGB pixel, written so equal planes (an
/// already-gray source) pass through bit-exactly rather than drifting by the
/// f32 rounding of the weight sum.
fn display_luma(r: f32, g: f32, b: f32) -> f32 {
    r + (g - r) * LUMA[1] + (b - r) * LUMA[2]
}

/// Resamples linear-light planes down to the display box and, for e-ink, folds
/// them to a single luminance plane replicated across RGB (the metric takes
/// RGB; equal planes are how gray sources are already scored). Built once per
/// evaluator, `out` reused for every candidate.
struct DisplayConditioner {
    width: u32,
    height: u32,
    gray: bool,
    /// Per-output-column source start plus normalized box weights; empty when
    /// the source already fits the box.
    xmap: Vec<(usize, Vec<f32>)>,
    ymap: Vec<(usize, Vec<f32>)>,
    /// Horizontal pass scratch, `width` x source height.
    temp: Vec<f32>,
    /// Source-sized luminance fold, only for e-ink.
    luma: Vec<f32>,
    out: [Vec<f32>; 3],
}

impl DisplayConditioner {
    /// `None` when the profile asks for nothing: the source already fits and
    /// the panel is color, so scoring is the unconditioned path.
    fn new(src_width: u32, src_height: u32, display: DisplayProfile) -> Option<Self> {
        let (width, height) = display.metric_size(src_width, src_height);
        let gray = matches!(display.color, DisplayColor::Eink);
        let resample = (width, height) != (src_width, src_height);
        if !resample && !gray {
            return None;
        }
        let pixels = width as usize * height as usize;
        Some(Self {
            width,
            height,
            gray,
            xmap: if resample {
                axis_map(src_width, width)
            } else {
                Vec::new()
            },
            ymap: if resample {
                axis_map(src_height, height)
            } else {
                Vec::new()
            },
            temp: if resample {
                vec![0.0; width as usize * src_height as usize]
            } else {
                Vec::new()
            },
            luma: if gray {
                vec![0.0; src_width as usize * src_height as usize]
            } else {
                Vec::new()
            },
            out: [vec![0.0; pixels], vec![0.0; pixels], vec![0.0; pixels]],
        })
    }

    fn apply(&mut self, src: &[Vec<f32>; 3], src_width: u32, src_height: u32) {
        if self.gray {
            // Luminance is a linear fold and the box filter is linear, so
            // folding first and resampling one plane is identical to resampling
            // three and folding after -- for a third of the resample work.
            let mut luma = std::mem::take(&mut self.luma);
            for (index, value) in luma.iter_mut().enumerate() {
                *value = display_luma(src[0][index], src[1][index], src[2][index]);
            }
            self.resample(&luma, 0, src_width, src_height);
            self.luma = luma;
            let (first, rest) = self.out.split_at_mut(1);
            for plane in rest {
                plane.copy_from_slice(&first[0]);
            }
            return;
        }
        for plane in 0..3 {
            self.resample(&src[plane], plane, src_width, src_height);
        }
    }

    fn resample(&mut self, src: &[f32], plane: usize, src_width: u32, src_height: u32) {
        if self.xmap.is_empty() {
            self.out[plane].copy_from_slice(src);
            return;
        }
        let dst_width = self.width as usize;
        for y in 0..src_height as usize {
            let row = &src[y * src_width as usize..(y + 1) * src_width as usize];
            for (x, (start, weights)) in self.xmap.iter().enumerate() {
                let mut acc = 0.0;
                for (k, weight) in weights.iter().enumerate() {
                    acc += row[start + k] * weight;
                }
                self.temp[y * dst_width + x] = acc;
            }
        }
        for (y, (start, weights)) in self.ymap.iter().enumerate() {
            for x in 0..dst_width {
                let mut acc = 0.0;
                for (k, weight) in weights.iter().enumerate() {
                    acc += self.temp[(start + k) * dst_width + x] * weight;
                }
                self.out[plane][y * dst_width + x] = acc;
            }
        }
    }
}

/// Box-filter (area-average) weights for one axis: each output sample covers a
/// fractional source span, so non-integer ratios stay unbiased.
pub(crate) fn axis_map(src: u32, dst: u32) -> Vec<(usize, Vec<f32>)> {
    let scale = f64::from(src) / f64::from(dst);
    (0..dst as usize)
        .map(|i| {
            let lo = i as f64 * scale;
            let hi = (lo + scale).min(f64::from(src));
            let first = (lo.floor() as usize).min(src as usize - 1);
            let last = (hi.ceil() as usize).clamp(first + 1, src as usize);
            let mut weights: Vec<f32> = (first..last)
                .map(|s| ((((s + 1) as f64).min(hi) - (s as f64).max(lo)).max(0.0)) as f32)
                .collect();
            let sum: f32 = weights.iter().sum();
            if sum > 0.0 {
                for weight in &mut weights {
                    *weight /= sum;
                }
            }
            (first, weights)
        })
        .collect()
}

fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn srgb8_to_linear(sample: u8) -> f32 {
    let v = f32::from(sample) / 255.0;
    if v <= 12.92 * 0.003_130_8 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// The sRGB transfer function tabulated over the 256 8-bit codes. Evaluating it
/// per sample was `powf`, and at ~22 ns a sample that was the entire cost of
/// building an evaluator and most of the cost of scoring a candidate.
pub(crate) fn srgb8_lut() -> &'static [f32; 256] {
    static LUT: std::sync::LazyLock<[f32; 256]> =
        std::sync::LazyLock::new(|| std::array::from_fn(|code| srgb8_to_linear(code as u8)));
    &LUT
}

/// Nearest 8-bit sRGB code to a linear value, by binary search in [`srgb8_lut`]:
/// the exact inverse of the forward table, without a `powf` per sample.
pub(crate) fn linear_to_srgb8(value: f32) -> u8 {
    let lut = srgb8_lut();
    let above = lut.partition_point(|&entry| entry < value);
    if above == 0 {
        return 0;
    }
    if above >= 256 {
        return 255;
    }
    if value - lut[above - 1] <= lut[above] - value {
        (above - 1) as u8
    } else {
        above as u8
    }
}

fn source_to_linear_ref(image: &ImageView<'_>, out: &mut [Vec<f32>; 3]) -> Result<()> {
    fill_linear_from_view(image, out)
}

fn decoded_to_linear(image: &Image, out: &mut [Vec<f32>; 3]) -> Result<()> {
    fill_linear_from_view(&image.as_view()?, out)
}

/// One row of an 8-bit component, linearized through `lut` into `out`.
///
/// The row is taken as a single bounds-checked slice and then walked by the
/// component's sample stride, so planar and interleaved 8-bit inputs both cost
/// an indexed read plus a table lookup per sample instead of a
/// `ComponentView::sample_at` with its per-sample checked arithmetic. That call
/// was ~2.9 ns/sample and this path runs over every source sample twice per
/// resampled display encode (once to downscale, once to build the reference).
pub(crate) fn linear_row(
    component: &crate::model::ComponentView<'_>,
    y: u32,
    lut: &[f32; 256],
    out: &mut [f32],
) -> Result<()> {
    use crate::model::ComponentSampleData;
    if out.len() > component.width as usize || y >= component.height {
        return Err(Jp2LamError::InvalidInput("sample out of range".into()));
    }
    let Some(base) = component
        .offset
        .checked_add((y as usize).saturating_mul(component.row_stride))
    else {
        return Err(Jp2LamError::InvalidInput("sample out of range".into()));
    };
    let stride = component.sample_stride;
    // `ComponentView`'s fields are public, so treat the geometry as untrusted:
    // an unvalidated stride must fail the read, not overflow into a panic.
    let Some(last) = out.len().checked_sub(1) else {
        return Ok(());
    };
    let end = last
        .checked_mul(stride)
        .and_then(|span| span.checked_add(1))
        .and_then(|span| base.checked_add(span));
    macro_rules! fill {
        ($samples:expr, $code:expr) => {{
            let row = end
                .and_then(|end| $samples.get(base..end))
                .ok_or_else(|| Jp2LamError::InvalidInput("sample out of range".into()))?;
            for (dst, sample) in out.iter_mut().zip(row.iter().step_by(stride)) {
                *dst = lut[$code(*sample)];
            }
        }};
    }
    match component.samples {
        ComponentSampleData::U8(samples) => fill!(samples, |sample: u8| usize::from(sample)),
        ComponentSampleData::U16(samples) => {
            fill!(samples, |sample: u16| usize::from(sample.min(255)))
        }
        ComponentSampleData::I32(samples) => {
            fill!(samples, |sample: i32| sample.clamp(0, 255) as usize)
        }
    }
    Ok(())
}

fn fill_linear_from_view(image: &ImageView<'_>, out: &mut [Vec<f32>; 3]) -> Result<()> {
    let lut = srgb8_lut();
    let planes: &[usize] = match image.colorspace.encoding_domain() {
        ColorSpace::Gray => &[0],
        ColorSpace::Srgb => &[0, 1, 2],
        other => {
            return Err(Jp2LamError::InvalidInput(format!(
                "perceptual scoring supports gray and sRGB, got {other:?}"
            )));
        }
    };
    if image.components.len() < planes.len() {
        return Err(Jp2LamError::InvalidInput(
            "image has fewer components than its color model".into(),
        ));
    }
    for &plane in planes {
        let component = &image.components[plane];
        if component.precision != 8 || component.signed {
            return Err(Jp2LamError::InvalidInput(
                "perceptual scoring supports 8-bit unsigned gray and sRGB".into(),
            ));
        }
        let width = image.width as usize;
        for y in 0..image.height {
            let start = y as usize * width;
            linear_row(component, y, lut, &mut out[plane][start..start + width])?;
        }
    }
    if planes.len() == 1 {
        let (gray, rest) = out.split_at_mut(1);
        for target in rest {
            target.copy_from_slice(&gray[0]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DisplayConditioner, StreamEvaluator};
    use crate::model::{
        DisplayProfile, EncodeOptions, Image, OutputFormat, PerceptualEffort, PerceptualTarget,
        RateControl,
    };
    use crate::{decode_jp2, encode};
    use jpxl_perceptual::{LinearRgbView, score_pair};

    fn gray_ramp(width: u32, height: u32) -> Image {
        let n = (width * height) as usize;
        let mut data = Vec::with_capacity(n);
        for y in 0..height {
            for x in 0..width {
                data.push(((x * 13 + y * 7) % 256) as u8);
            }
        }
        Image::from_gray_bytes(width, height, &data).expect("gray")
    }

    fn rgb_ramp(width: u32, height: u32) -> Image {
        let n = (width * height * 3) as usize;
        let mut data = Vec::with_capacity(n);
        for y in 0..height {
            for x in 0..width {
                data.push((x % 256) as u8);
                data.push((y % 256) as u8);
                data.push(((x + y) % 256) as u8);
            }
        }
        Image::from_rgb_bytes(width, height, &data).expect("rgb")
    }

    fn score_decoded_independently(source: &Image, decoded: &Image) -> f64 {
        let mut src_planes = [
            vec![0.0; (source.width * source.height) as usize],
            vec![0.0; (source.width * source.height) as usize],
            vec![0.0; (source.width * source.height) as usize],
        ];
        let mut cand_planes = src_planes.clone();
        super::source_to_linear_ref(&source.as_view().expect("src view"), &mut src_planes)
            .expect("src linear");
        super::decoded_to_linear(decoded, &mut cand_planes).expect("cand linear");
        let src = LinearRgbView::new(
            source.width,
            source.height,
            &src_planes[0],
            &src_planes[1],
            &src_planes[2],
        )
        .expect("src view");
        let cand = LinearRgbView::new(
            decoded.width,
            decoded.height,
            &cand_planes[0],
            &cand_planes[1],
            &cand_planes[2],
        )
        .expect("cand view");
        score_pair(src, cand).expect("score").score
    }

    fn assert_evaluator_matches_independent_decode(source: &Image) {
        let bytes = encode(
            source,
            &EncodeOptions {
                rate_control: Some(RateControl::ApproxQuality(50)),
                format: OutputFormat::Jp2,
                ..Default::default()
            },
        )
        .expect("encode");
        let mut evaluator =
            StreamEvaluator::from_view(source.as_view().expect("view")).expect("eval");
        assert_eq!(evaluator.metric_version(), jpxl_perceptual::METRIC_VERSION);
        let in_loop = evaluator.score_stream(&bytes).expect("in-loop");
        let decoded = decode_jp2(&bytes).expect("independent decode");
        let independent = score_decoded_independently(source, &decoded);
        assert_eq!(
            in_loop.score.to_bits(),
            independent.to_bits(),
            "in-loop {} vs independent {}",
            in_loop.score,
            independent
        );
    }

    #[test]
    fn gray_stream_score_matches_independent_decode() {
        assert_evaluator_matches_independent_decode(&gray_ramp(32, 24));
    }

    #[test]
    fn rgb_stream_score_matches_independent_decode() {
        assert_evaluator_matches_independent_decode(&rgb_ramp(24, 20));
    }

    #[test]
    fn prior_fraction_is_monotone_and_shrink_moves_down() {
        // No DWT cache: the pooled quality-and-loss constants.
        let pooled = super::PriorFeatures {
            bands: None,
            body_bytes: 2_359_296.0,
        };
        let mut last = 0.0;
        for target in [60.0, 70.0, 77.0, 80.0, 90.0, 93.0, 97.5, 99.0] {
            let fraction = super::prior_body_fraction(60, target, pooled);
            assert!(fraction >= last, "{target}: {fraction} < {last}");
            assert!((0.02..=1.0).contains(&fraction), "{target}: {fraction}");
            last = fraction;
        }
        // A finer rung stores more passes, so the same target sits at a smaller
        // fraction of them.
        assert!(
            super::prior_body_fraction(90, 90.0, pooled)
                < super::prior_body_fraction(50, 90.0, pooled)
        );
        // Corpus geomean over the twelve sources at the rungs the sweep resolved,
        // within the pooled fit's residual (measured 0.482 and 0.353).
        assert!((super::prior_body_fraction(60, 90.0, pooled) - 0.482).abs() < 0.03);
        assert!((super::prior_body_fraction(75, 90.0, pooled) - 0.353).abs() < 0.03);
        // Measured, not extrapolated, since the fractions-to-0.05 re-sweep
        // (corpus geomeans 0.132 and 0.184).
        assert!((super::prior_body_fraction(60, 60.0, pooled) - 0.132).abs() < 0.02);
        assert!((super::prior_body_fraction(60, 70.0, pooled) - 0.184).abs() < 0.02);
        // With the per-source terms, one corpus source measured end to end (1024x768
        // RGB, 2_359_296 coefficients; near-zero 0.641, band energies 0.564 and 69.86
        // per coefficient; 781_278 and 1_012_082 stored body bytes at q60/q75, crossings
        // 0.143/0.446 and 0.113/0.330).
        let bands = super::SourceBands {
            near_zero: 0.641,
            ll_per_sample: 0.564,
            highpass_per_sample: 69.86,
            samples: 2_359_296.0,
        };
        let q60 = super::PriorFeatures {
            bands: Some(bands),
            body_bytes: 781_278.0,
        };
        let q75 = super::PriorFeatures {
            bands: Some(bands),
            body_bytes: 1_012_082.0,
        };
        assert!((super::prior_body_fraction(60, 60.0, q60) - 0.143).abs() < 0.02);
        assert!((super::prior_body_fraction(60, 90.0, q60) - 0.446).abs() < 0.06);
        assert!((super::prior_body_fraction(75, 60.0, q75) - 0.113).abs() < 0.02);
        assert!((super::prior_body_fraction(75, 90.0, q75) - 0.330).abs() < 0.06);
        // A flatter image (more near-zero coefficients) crosses lower.
        let flat = super::PriorFeatures {
            bands: Some(super::SourceBands {
                near_zero: 0.8,
                ..bands
            }),
            ..q60
        };
        assert!(
            super::prior_body_fraction(60, 80.0, flat) < super::prior_body_fraction(60, 80.0, q60)
        );
        let shrunk = super::shrink_body_bytes(100_000, 85.0, 80.0);
        assert!(shrunk < 100_000 && shrunk > 50_000, "{shrunk}");
        assert_eq!(super::shrink_body_bytes(1, 85.0, 80.0), 1);
    }

    /// The accept band always contains the aim, at every effort and target: a probe
    /// landing exactly where a correction pointed ends the search. A display target
    /// aims at least as far above the floor as the widest preset, because it only
    /// gets one correction.
    #[test]
    fn the_accept_band_contains_the_aim() {
        for effort in [
            PerceptualEffort::Fast,
            PerceptualEffort::Balanced,
            PerceptualEffort::Quality,
        ] {
            for score in [30.0, 50.0, 70.0, 80.0, 90.0, 95.0, 99.0] {
                let target = PerceptualTarget::new(score, effort).expect("target");
                let margin = super::aim_score(&target) - score;
                assert!(margin >= super::MIN_AIM_MARGIN);
                assert!(super::accept_band(&target) >= 2.0 * margin - 1e-12);
                let displayed =
                    PerceptualTarget::for_display(score, DisplayProfile::eink(400, 300), effort)
                        .expect("display target");
                assert!(super::aim_margin(&displayed) >= super::aim_margin(&target));
                // Both sides shrink to the panel, so the first probe starts smaller.
                // One measured corpus source, and the reversible fallback.
                let bands = super::SourceBands {
                    near_zero: 0.641,
                    ll_per_sample: 0.564,
                    highpass_per_sample: 69.86,
                    samples: 2_359_296.0,
                };
                for read in [Some(bands), None] {
                    assert!(super::display_prior_scale(&displayed, 1024, 768, read) < 0.5);
                    assert!(
                        (super::display_prior_scale(&target, 1024, 768, read) - 1.0).abs() < 1e-12
                    );
                    // Smaller panel, smaller first probe, and never past the rung ceiling.
                    let big = PerceptualTarget::for_display(
                        score,
                        DisplayProfile::eink(800, 600),
                        effort,
                    )
                    .expect("display target");
                    let small = super::display_prior_scale(&displayed, 1024, 768, read);
                    let large = super::display_prior_scale(&big, 1024, 768, read);
                    assert!(small < large && large <= 1.0, "{small} {large}");
                }
                // A softer source (less band energy) needs a smaller fraction at the
                // same panel -- the spread no function of the pixel counts can see.
                let soft = super::SourceBands {
                    ll_per_sample: 0.057,
                    highpass_per_sample: 6.97,
                    ..bands
                };
                assert!(
                    super::display_prior_scale(&displayed, 1024, 768, Some(soft))
                        < super::display_prior_scale(&displayed, 1024, 768, Some(bands))
                );
            }
        }
    }

    #[test]
    fn outer_rungs_are_coarse_first_and_strictly_finer_after() {
        for score in [0.0, 80.0, 87.5, 90.0, 94.0, 95.9, 96.0, 97.9, 98.0, 99.99] {
            let rungs = super::outer_rungs_for_target(score);
            assert!(
                rungs[0] < rungs[1] && rungs[1] < rungs[2],
                "{score}: {rungs:?}"
            );
            assert!(
                rungs[2] >= 90,
                "{score}: top fallback must be able to reach the target"
            );
        }
        assert_eq!(super::outer_rungs_for_target(80.0)[0], 60);
        assert_eq!(super::outer_rungs_for_target(95.0)[0], 50);
        assert_eq!(super::outer_rungs_for_target(99.0), [85, 92, 95]);
        for score in [80.0, 90.0, 95.0, 99.0] {
            // The top fallback must always be probed for real, never assumed.
            let top = super::outer_rungs_for_target(score)[2];
            assert!(super::safe_ceiling_for_rung(top).is_none_or(|c| c < 100.0));
            assert!(super::safe_ceiling_for_rung(50) < super::safe_ceiling_for_rung(90));
        }
    }

    fn linear_to_srgb8(linear: f32) -> u8 {
        let v = if linear <= 0.003_130_8 {
            linear * 12.92
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        };
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    }

    fn linear_planes(image: &Image) -> [Vec<f32>; 3] {
        let n = (image.width * image.height) as usize;
        let mut planes = [vec![0.0; n], vec![0.0; n], vec![0.0; n]];
        super::source_to_linear_ref(&image.as_view().expect("view"), &mut planes).expect("linear");
        planes
    }

    fn quantized_stream(image: &Image) -> Vec<u8> {
        encode(
            image,
            &EncodeOptions {
                rate_control: Some(RateControl::ApproxQuality(15)),
                format: OutputFormat::Jp2,
                ..Default::default()
            },
        )
        .expect("encode")
    }

    #[test]
    fn conditioning_an_image_that_already_fits_is_the_unconditioned_path() {
        let image = rgb_ramp(32, 24);
        let bytes = quantized_stream(&image);
        let view = image.as_view().expect("view");
        let plain = StreamEvaluator::from_view_ref(&view)
            .expect("plain")
            .score_stream(&bytes)
            .expect("plain score");
        let fitted = StreamEvaluator::for_display(view.clone(), DisplayProfile::tablet(64, 64))
            .expect("fitted")
            .score_stream(&bytes)
            .expect("fitted score");
        assert_eq!(plain.score.to_bits(), fitted.score.to_bits());
        assert!(DisplayConditioner::new(32, 24, DisplayProfile::tablet(64, 64)).is_none());

        // Gray survives the e-ink fold too: luminance of equal planes is the plane.
        let gray = gray_ramp(32, 24);
        let gray_bytes = quantized_stream(&gray);
        let gray_view = gray.as_view().expect("view");
        let gray_plain = StreamEvaluator::from_view_ref(&gray_view)
            .expect("plain")
            .score_stream(&gray_bytes)
            .expect("plain score");
        let gray_eink = StreamEvaluator::for_display(gray_view, DisplayProfile::eink(64, 64))
            .expect("eink")
            .score_stream(&gray_bytes)
            .expect("eink score");
        assert!(
            gray_plain.score.to_bits() == gray_eink.score.to_bits(),
            "{} vs {}",
            gray_plain.score,
            gray_eink.score
        );
    }

    #[test]
    fn display_conditioning_is_more_permissive_than_source_resolution() {
        let image = rgb_ramp(64, 48);
        let bytes = quantized_stream(&image);
        let view = image.as_view().expect("view");
        let source = StreamEvaluator::from_view_ref(&view)
            .expect("source")
            .score_stream(&bytes)
            .expect("source score");
        let display = StreamEvaluator::for_display(view, DisplayProfile::tablet(32, 24))
            .expect("display")
            .score_stream(&bytes)
            .expect("display score");
        assert!(
            display.score > source.score,
            "display {} must beat source {}",
            display.score,
            source.score
        );
    }

    #[test]
    fn eink_conditioning_of_rgb_matches_its_gray_conversion() {
        let rgb = rgb_ramp(40, 32);
        let rgb_linear = linear_planes(&rgb);
        let gray_bytes: Vec<u8> = (0..rgb_linear[0].len())
            .map(|i| {
                linear_to_srgb8(super::display_luma(
                    rgb_linear[0][i],
                    rgb_linear[1][i],
                    rgb_linear[2][i],
                ))
            })
            .collect();
        let gray = Image::from_gray_bytes(40, 32, &gray_bytes).expect("gray");
        let gray_linear = linear_planes(&gray);

        let profile = DisplayProfile::eink(20, 16);
        let mut from_rgb = DisplayConditioner::new(40, 32, profile).expect("conditioner");
        from_rgb.apply(&rgb_linear, 40, 32);
        let mut from_gray = DisplayConditioner::new(40, 32, profile).expect("conditioner");
        from_gray.apply(&gray_linear, 40, 32);

        assert_eq!((from_rgb.width, from_rgb.height), (20, 16));
        for plane in 0..3 {
            for (i, (a, b)) in from_rgb.out[plane]
                .iter()
                .zip(&from_gray.out[plane])
                .enumerate()
            {
                assert!((a - b).abs() < 0.01, "plane {plane} pixel {i}: {a} vs {b}");
                assert_eq!(a.to_bits(), from_rgb.out[0][i].to_bits());
            }
        }
    }

    #[test]
    fn a_display_target_gets_one_encode_and_two_corrections() {
        let display = PerceptualTarget::for_display(
            75.0,
            DisplayProfile::eink(400, 300),
            PerceptualEffort::Quality,
        )
        .expect("target");
        assert_eq!(
            super::QualityBudget::for_target(&display).max_evaluations(),
            3
        );
        for effort in [
            PerceptualEffort::Fast,
            PerceptualEffort::Balanced,
            PerceptualEffort::Quality,
        ] {
            let plain = PerceptualTarget::new(75.0, effort).expect("target");
            assert_eq!(
                super::QualityBudget::for_target(&plain).max_evaluations(),
                super::QualityBudget::for_effort(effort).max_evaluations()
            );
            assert!(super::QualityBudget::for_target(&plain).max_evaluations() > 3);
        }
        assert!(
            PerceptualTarget::for_display(
                75.0,
                DisplayProfile::eink(0, 300),
                PerceptualEffort::Fast
            )
            .is_err()
        );
    }
}
