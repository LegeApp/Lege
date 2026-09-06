//! Offline perceptual oracle: densified quantizer × PCRD labels.
//!
//! The production scheduler is not allowed to run this search. Session 9 trains
//! on the JSONL this module writes; this module does not fit a predictor.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::encode::backend::native::{
    NativeBackend, UnquantizedComponentDwt, UnquantizedTileDwt, build_stored_tile_parts,
    complete_output_len, max_stored_body_bytes, select_stored_tile_passes,
};
use crate::encode::block_store::EncodedBlockStore;
use crate::encode::context::EncodeContext;
use crate::encode::ssim::StreamEvaluator;
use crate::encode::ssim_recon::reconstruct_stored_selection;
use crate::error::{Jp2LamError, Result};
use crate::j2k::{CodestreamParts, build_main_header_segments};
use crate::model::{
    EncodeOptions, Image, OutputFormat, PerceptualEffort, PerceptualTarget, RateControl,
    ResourceLimits, TilePolicy,
};
use crate::plan::{apply_perceptual_quant_scale, perceptual_quant_scale_for_quality};
use crate::tiling::{phase_resolution_sizes, tile_grid};
use jpxl_perceptual::METRIC_VERSION;

/// 8-bit luma in `0..=1` whose 8×8 variance counts as flat. Matches JPXL's
/// ±1 LSB dither floor, applied here in display-referred luma rather than XYB.
const FLAT_VARIANCE: f32 = 2e-5;
/// Absolute 9/7 coefficient magnitude treated as near-zero for the DWT feature.
const NEAR_ZERO: f32 = 1.0;
/// Reservoir cap for coefficient-magnitude quantiles on large frames.
const MAGNITUDE_CAP: usize = 4_000_000;

/// Default score knots from the Session 8 plan.
#[must_use]
pub fn default_oracle_targets() -> Vec<f64> {
    vec![80.0, 85.0, 90.0, 92.5, 95.0, 97.5]
}

/// Log-ish quality grid covering the live q75/q90/q99 rungs plus denser steps.
#[must_use]
pub fn default_oracle_quant_qualities() -> Vec<u8> {
    vec![50, 60, 70, 75, 80, 85, 90, 92, 95, 97, 99]
}

/// Inner PCRD body fractions of the stored all-pass payload (1.0 = no trim).
#[must_use]
pub fn default_oracle_body_fractions() -> Vec<f64> {
    vec![1.0, 0.7, 0.5, 0.35, 0.25]
}

/// Offline sweep configuration.
#[derive(Debug, Clone)]
pub struct OracleConfig {
    pub targets: Vec<f64>,
    pub quant_qualities: Vec<u8>,
    pub body_fractions: Vec<f64>,
    pub output_dir: PathBuf,
}

impl Default for OracleConfig {
    fn default() -> Self {
        Self {
            targets: default_oracle_targets(),
            quant_qualities: default_oracle_quant_qualities(),
            body_fractions: default_oracle_body_fractions(),
            output_dir: PathBuf::from("."),
        }
    }
}

/// Source + DWT features recorded for Session 9. Not a predictor.
#[derive(Debug, Clone, PartialEq)]
pub struct OracleFeatures {
    pub width: u32,
    pub height: u32,
    pub grayscale: bool,
    pub luma_variance_q10: f32,
    pub luma_variance_q50: f32,
    pub luma_variance_q90: f32,
    pub chroma_variance_q50: f32,
    pub flat_fraction: f32,
    pub edge_proxy: f32,
    pub ll_energy: f64,
    pub highpass_energy: f64,
    pub high_low_ratio: f64,
    pub hh_fraction: f64,
    pub directional_asymmetry: f64,
    pub chroma_wavelet_ratio: f64,
    pub coefficient_near_zero_fraction: f64,
    pub coefficient_tail_q90: f64,
    pub coefficient_tail_q99: f64,
}

/// One densified (quant, PCRD) measurement.
#[derive(Debug, Clone, PartialEq)]
pub struct OracleProbe {
    pub quant_quality: u8,
    pub quant_scale: f64,
    pub pcrd_body: u32,
    pub score: f64,
    pub output_bytes: u64,
}

/// How the crossing sits on the measured grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OracleStatus {
    /// Coarsest measured quantizer already meets the target.
    Floor,
    /// Some finer quantizer meets it; the label is the coarsest that does.
    Met,
    /// Even the finest full-pass probe misses.
    CensoredTop,
}

/// One `(source, target)` label. The control value, not filesize and not SSIM.
#[derive(Debug, Clone, PartialEq)]
pub struct OracleLabel {
    pub target: f64,
    pub status: OracleStatus,
    pub best_quant_quality: Option<u8>,
    pub best_quant_scale: Option<f64>,
    pub best_pcrd_body: Option<u32>,
    pub achieved_score: f64,
    pub output_bytes: Option<u64>,
}

/// Result of sweeping one source.
#[derive(Debug, Clone)]
pub struct OracleSweepResult {
    pub source: String,
    pub skipped: bool,
    pub jsonl_path: PathBuf,
    pub fingerprint: u64,
    pub features: Option<OracleFeatures>,
    pub probes: Vec<OracleProbe>,
    pub labels: Vec<OracleLabel>,
}

/// Exhaustive densified search for one image. Each probe is synced to
/// `raw/<stem>.jsonl`, so an interrupted source resumes from its last durable
/// probe; a trailing `done` record still marks a fully completed source.
pub fn sweep_source(
    image: &Image,
    source: Option<&Path>,
    config: &OracleConfig,
) -> Result<OracleSweepResult> {
    validate_config(config)?;
    let source_name = source
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<memory>".into());
    let fingerprint = source
        .map(fnv1a_file)
        .transpose()?
        .unwrap_or_else(|| fnv1a_image(image));
    let jsonl_path = jsonl_path(&config.output_dir, source, &source_name);
    if jsonl_is_complete(&jsonl_path)? {
        return Ok(OracleSweepResult {
            source: source_name,
            skipped: true,
            jsonl_path,
            fingerprint,
            features: None,
            probes: Vec::new(),
            labels: Vec::new(),
        });
    }

    let options = oracle_encode_options();
    let context = EncodeContext::new(image, &options)?;
    let backend = NativeBackend;
    let tile_rects = tile_grid(&context.plan).tile_rects();
    let cache_tiles = match backend.try_cache_unquantized_dwt(&context, &tile_rects)? {
        Some(cached) => cached,
        None => tile_rects
            .iter()
            .map(|tile| backend.prepare_unquantized_dwt_for_tile_rect(&context, *tile))
            .collect::<Result<Vec<_>>>()?,
    };
    let features = extract_features(image, &cache_tiles);
    let (mut checkpoint, mut probes) =
        open_jsonl_checkpoint(&jsonl_path, &source_name, fingerprint, &features)?;
    let mut evaluator = StreamEvaluator::from_view_ref(&context.image)?;

    let mut qualities = config.quant_qualities.clone();
    qualities.sort_unstable();
    qualities.dedup();
    let mut fractions = config.body_fractions.clone();
    fractions.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    fractions.dedup_by(|a, b| (*a - *b).abs() < 1e-12);

    for quality in qualities {
        let scale = perceptual_quant_scale_for_quality(quality);
        let mut plan = context.plan.clone();
        plan.quant_scale = scale;
        plan.quality = quality;
        plan.subband_quants = plan.base_subband_quants.clone();
        apply_perceptual_quant_scale(&mut plan.subband_quants, scale);
        let emit_plan = backend.emit_plan(&plan);
        let headers = build_main_header_segments(&emit_plan)?;
        let rung_context = context.with_plan(plan);
        let mut store = EncodedBlockStore::from_resource_limits(&rung_context.plan.resource_limits);
        let mut stored_tiles = Vec::with_capacity(cache_tiles.len());
        for cached in &cache_tiles {
            stored_tiles.push((
                cached.tile,
                backend.prepare_stored_tier1_from_unquantized_dwt(
                    &rung_context,
                    cached,
                    &mut store,
                )?,
            ));
        }
        let shared = store.into_shared();
        let max_body = max_stored_body_bytes(&stored_tiles).max(1);
        let mut bodies = Vec::new();
        for fraction in &fractions {
            let body = ((f64::from(max_body) * fraction).round() as u32).clamp(1, max_body);
            if !bodies.contains(&body) {
                bodies.push(body);
            }
        }
        for body in bodies {
            if probes
                .iter()
                .any(|probe| probe.quant_quality == quality && probe.pcrd_body == body)
            {
                continue;
            }
            let selections = select_stored_tile_passes(&stored_tiles, &rung_context, Some(body))?;
            let reconstructed = reconstruct_stored_selection(
                &rung_context.plan,
                image.colorspace,
                &stored_tiles,
                &selections,
                shared.as_ref(),
            )?;
            let observation = evaluator.score_image(&reconstructed, None)?;
            let tile_parts = build_stored_tile_parts(&stored_tiles, selections, shared.clone())?;
            let parts = CodestreamParts {
                main_header_segments: headers.clone(),
                tile_parts,
            };
            let output_bytes = complete_output_len(&rung_context, &emit_plan, &parts)?;
            let probe = OracleProbe {
                quant_quality: quality,
                quant_scale: scale,
                pcrd_body: body,
                score: observation.score,
                output_bytes,
            };
            append_probe(&mut checkpoint, &probe)?;
            probes.push(probe);
        }
    }

    let labels = reduce_labels(&probes, &config.targets);
    finish_jsonl_checkpoint(&mut checkpoint, &labels)?;
    Ok(OracleSweepResult {
        source: source_name,
        skipped: false,
        jsonl_path,
        fingerprint,
        features: Some(features),
        probes,
        labels,
    })
}

/// Reduce measured probes to one label per target. Coarsest feasible quantizer
/// (largest `quant_scale`) that meets the floor, then the smallest PCRD body
/// at that scale. Does not invent a crossing when nothing meets.
#[must_use]
pub fn reduce_labels(probes: &[OracleProbe], targets: &[f64]) -> Vec<OracleLabel> {
    let coarsest_scale = probes
        .iter()
        .map(|probe| probe.quant_scale)
        .fold(f64::NEG_INFINITY, f64::max);
    targets
        .iter()
        .copied()
        .map(|target| {
            let mut meeting: Vec<&OracleProbe> = probes
                .iter()
                .filter(|probe| probe.score >= target)
                .collect();
            if meeting.is_empty() {
                let achieved = probes
                    .iter()
                    .map(|probe| probe.score)
                    .fold(f64::NEG_INFINITY, f64::max);
                return OracleLabel {
                    target,
                    status: OracleStatus::CensoredTop,
                    best_quant_quality: None,
                    best_quant_scale: None,
                    best_pcrd_body: None,
                    achieved_score: achieved,
                    output_bytes: None,
                };
            }
            meeting.sort_by(|a, b| {
                b.quant_scale
                    .partial_cmp(&a.quant_scale)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.pcrd_body.cmp(&b.pcrd_body))
                    .then(a.output_bytes.cmp(&b.output_bytes))
            });
            let best = meeting[0];
            let status = if best.quant_scale == coarsest_scale {
                OracleStatus::Floor
            } else {
                OracleStatus::Met
            };
            OracleLabel {
                target,
                status,
                best_quant_quality: Some(best.quant_quality),
                best_quant_scale: Some(best.quant_scale),
                best_pcrd_body: Some(best.pcrd_body),
                achieved_score: best.score,
                output_bytes: Some(best.output_bytes),
            }
        })
        .collect()
}

pub(crate) fn extract_features(image: &Image, tiles: &[UnquantizedTileDwt]) -> OracleFeatures {
    let source = source_features(image);
    let dwt = dwt_features(tiles);
    OracleFeatures {
        width: image.width,
        height: image.height,
        grayscale: source.grayscale,
        luma_variance_q10: source.luma_variance_q10,
        luma_variance_q50: source.luma_variance_q50,
        luma_variance_q90: source.luma_variance_q90,
        chroma_variance_q50: source.chroma_variance_q50,
        flat_fraction: source.flat_fraction,
        edge_proxy: source.edge_proxy,
        ll_energy: dwt.ll_energy,
        highpass_energy: dwt.highpass_energy,
        high_low_ratio: dwt.high_low_ratio,
        hh_fraction: dwt.hh_fraction,
        directional_asymmetry: dwt.directional_asymmetry,
        chroma_wavelet_ratio: dwt.chroma_wavelet_ratio,
        coefficient_near_zero_fraction: dwt.near_zero,
        coefficient_tail_q90: dwt.tail_q90,
        coefficient_tail_q99: dwt.tail_q99,
    }
}

struct SourceFeat {
    grayscale: bool,
    luma_variance_q10: f32,
    luma_variance_q50: f32,
    luma_variance_q90: f32,
    chroma_variance_q50: f32,
    flat_fraction: f32,
    edge_proxy: f32,
}

struct DwtFeat {
    ll_energy: f64,
    highpass_energy: f64,
    high_low_ratio: f64,
    hh_fraction: f64,
    directional_asymmetry: f64,
    chroma_wavelet_ratio: f64,
    near_zero: f64,
    tail_q90: f64,
    tail_q99: f64,
}

fn source_features(image: &Image) -> SourceFeat {
    let pixels = (image.width as usize).saturating_mul(image.height as usize);
    let gray = image.components.len() == 1
        || (image.components.len() >= 3 && rgb_is_gray(&image.components[0..3], pixels));
    let mut luma_vars = Vec::new();
    let mut chroma_vars = Vec::new();
    let mut flat = 0u32;
    let block = 8u32;
    let rows = image.height / block;
    let cols = image.width / block;
    for by in 0..rows {
        for bx in 0..cols {
            let (luma_var, chroma_var) =
                block_variances(image, bx * block, by * block, block, gray);
            luma_vars.push(luma_var);
            chroma_vars.push(chroma_var);
            if luma_var < FLAT_VARIANCE {
                flat += 1;
            }
        }
    }
    luma_vars.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    chroma_vars.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q10 = quantile_f32(&luma_vars, 0.10);
    let q50 = quantile_f32(&luma_vars, 0.50);
    let q90 = quantile_f32(&luma_vars, 0.90);
    let n_blocks = luma_vars.len().max(1) as f32;
    SourceFeat {
        grayscale: gray,
        luma_variance_q10: q10,
        luma_variance_q50: q50,
        luma_variance_q90: q90,
        chroma_variance_q50: quantile_f32(&chroma_vars, 0.50),
        flat_fraction: flat as f32 / n_blocks,
        edge_proxy: q90 - q50,
    }
}

fn rgb_is_gray(components: &[crate::model::Component], pixels: usize) -> bool {
    if components.len() < 3 {
        return true;
    }
    for i in 0..pixels {
        let r = components[0].data.get(i).copied().unwrap_or(0);
        let g = components[1].data.get(i).copied().unwrap_or(0);
        let b = components[2].data.get(i).copied().unwrap_or(0);
        if r != g || g != b {
            return false;
        }
    }
    true
}

fn block_variances(image: &Image, x0: u32, y0: u32, edge: u32, gray: bool) -> (f32, f32) {
    let n = (edge * edge) as f64;
    let mut sum_y = 0.0;
    let mut sum_yy = 0.0;
    let mut sum_c = 0.0;
    let mut sum_cc = 0.0;
    for dy in 0..edge {
        for dx in 0..edge {
            let (y, c) = luma_chroma(image, x0 + dx, y0 + dy, gray);
            sum_y += f64::from(y);
            sum_yy += f64::from(y) * f64::from(y);
            sum_c += f64::from(c);
            sum_cc += f64::from(c) * f64::from(c);
        }
    }
    let luma_var = ((sum_yy / n) - (sum_y / n).powi(2)).max(0.0) as f32;
    let chroma_var = ((sum_cc / n) - (sum_c / n).powi(2)).max(0.0) as f32;
    (luma_var, chroma_var)
}

fn luma_chroma(image: &Image, x: u32, y: u32, gray: bool) -> (f32, f32) {
    let sample = |index: usize| {
        let component = match image.components.get(index) {
            Some(component) => component,
            None => return 0.0,
        };
        let offset = (y as usize)
            .saturating_mul(component.width as usize)
            .saturating_add(x as usize);
        component.data.get(offset).copied().unwrap_or(0) as f32 / 255.0
    };
    if gray || image.components.len() == 1 {
        (sample(0), 0.0)
    } else {
        let r = sample(0);
        let g = sample(1);
        let b = sample(2);
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let chroma = (r - luma).abs() + (b - luma).abs();
        (luma, chroma)
    }
}

/// One tile's contribution to the per-source terms of the production first-probe
/// predictors: `(near-zero coefficients, coefficients, LL energy, highpass energy)`.
///
/// See `ssim::prior_body_fraction` and `ssim::display_prior_scale`. No sort and no
/// allocation, and the same three statistics the oracle records as
/// `coefficient_near_zero_fraction`, `ll_energy` and `highpass_energy`, so the fitted
/// constants transfer directly.
pub(crate) fn source_band_stats(tile: &UnquantizedTileDwt) -> (u64, u64, f64, f64) {
    let mut near = 0u64;
    let mut total = 0u64;
    let mut ll = 0.0;
    let mut high = 0.0;
    for plane in &tile.components {
        total += plane.data.len() as u64;
        near += plane.data.iter().filter(|c| c.abs() < NEAR_ZERO).count() as u64;
        let bands = plane_band_energy(plane);
        ll += bands.ll;
        high += bands.hl + bands.lh + bands.hh;
    }
    (near, total, ll, high)
}

fn dwt_features(tiles: &[UnquantizedTileDwt]) -> DwtFeat {
    let mut ll = 0.0;
    let mut hl = 0.0;
    let mut lh = 0.0;
    let mut hh = 0.0;
    let mut y_energy = 0.0;
    let mut chroma_energy = 0.0;
    let mut near = 0u64;
    let mut total = 0u64;
    let mut mags = Vec::new();
    for tile in tiles {
        for plane in &tile.components {
            let stats = plane_band_energy(plane);
            ll += stats.ll;
            hl += stats.hl;
            lh += stats.lh;
            hh += stats.hh;
            let plane_e = stats.ll + stats.hl + stats.lh + stats.hh;
            if plane.component_index == 0 {
                y_energy += plane_e;
            } else {
                chroma_energy += plane_e;
            }
            for &coeff in &plane.data {
                total += 1;
                if coeff.abs() < NEAR_ZERO {
                    near += 1;
                }
                if mags.len() < MAGNITUDE_CAP {
                    mags.push(coeff.abs() as f64);
                }
            }
        }
    }
    mags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let high = hl + lh + hh;
    DwtFeat {
        ll_energy: ll,
        highpass_energy: high,
        high_low_ratio: high / ll.max(1e-12),
        hh_fraction: hh / high.max(1e-12),
        directional_asymmetry: (hl - lh).abs() / (hl + lh).max(1e-12),
        chroma_wavelet_ratio: chroma_energy / y_energy.max(1e-12),
        near_zero: if total == 0 {
            0.0
        } else {
            near as f64 / total as f64
        },
        tail_q90: quantile_f64(&mags, 0.90),
        tail_q99: quantile_f64(&mags, 0.99),
    }
}

struct BandEnergy {
    ll: f64,
    hl: f64,
    lh: f64,
    hh: f64,
}

fn plane_band_energy(plane: &UnquantizedComponentDwt) -> BandEnergy {
    let resolutions =
        phase_resolution_sizes(plane.x0, plane.y0, plane.width, plane.height, plane.levels);
    let mut energy = BandEnergy {
        ll: 0.0,
        hl: 0.0,
        lh: 0.0,
        hh: 0.0,
    };
    if resolutions.is_empty() {
        return energy;
    }
    let ll = resolutions[0];
    energy.ll = rect_energy(&plane.data, plane.width, 0, 0, ll.0, ll.1);
    for w in resolutions.windows(2) {
        let (low, full) = (w[0], w[1]);
        energy.hl += rect_energy(&plane.data, plane.width, low.0, 0, full.0, low.1);
        energy.lh += rect_energy(&plane.data, plane.width, 0, low.1, low.0, full.1);
        energy.hh += rect_energy(&plane.data, plane.width, low.0, low.1, full.0, full.1);
    }
    energy
}

fn rect_energy(data: &[f32], stride: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> f64 {
    let mut sum = 0.0;
    for y in y0..y1 {
        let row = y.saturating_mul(stride);
        for x in x0..x1 {
            let v = data.get(row + x).copied().unwrap_or(0.0) as f64;
            sum += v * v;
        }
    }
    sum
}

fn quantile_f32(sorted: &[f32], q: f64) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((q * sorted.len() as f64).floor() as usize).min(sorted.len() - 1);
    sorted[index]
}

fn quantile_f64(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((q * sorted.len() as f64).floor() as usize).min(sorted.len() - 1);
    sorted[index]
}

fn oracle_encode_options() -> EncodeOptions {
    EncodeOptions {
        rate_control: Some(RateControl::Perceptual(
            PerceptualTarget::new(80.0, PerceptualEffort::Quality).expect("80 is in range"),
        )),
        format: OutputFormat::Jp2,
        tile_policy: TilePolicy::Auto,
        resource_limits: ResourceLimits {
            max_working_memory: Some(512 * 1024 * 1024),
            encoded_store_memory_limit: Some(64 * 1024 * 1024),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn validate_config(config: &OracleConfig) -> Result<()> {
    if config.targets.is_empty() {
        return Err(Jp2LamError::InvalidInput(
            "oracle needs at least one target score".into(),
        ));
    }
    for target in &config.targets {
        if !target.is_finite() || !(0.0..100.0).contains(target) {
            return Err(Jp2LamError::InvalidInput(format!(
                "oracle target must be finite and in 0..100, got {target}"
            )));
        }
    }
    if config.quant_qualities.is_empty() {
        return Err(Jp2LamError::InvalidInput(
            "oracle needs at least one quantizer quality".into(),
        ));
    }
    for quality in &config.quant_qualities {
        if *quality > 99 {
            return Err(Jp2LamError::InvalidInput(format!(
                "oracle quantizer quality must be 0..=99, got {quality}"
            )));
        }
    }
    if config.body_fractions.is_empty()
        || config
            .body_fractions
            .iter()
            .any(|fraction| !fraction.is_finite() || *fraction <= 0.0 || *fraction > 1.0)
    {
        return Err(Jp2LamError::InvalidInput(
            "oracle body fractions must be in (0, 1]".into(),
        ));
    }
    Ok(())
}

fn jsonl_path(output_dir: &Path, source: Option<&Path>, fallback: &str) -> PathBuf {
    let raw = output_dir.join("raw");
    let stem = source
        .and_then(|path| path.file_stem())
        .and_then(|stem| stem.to_str())
        .unwrap_or("memory");
    let parent = source
        .and_then(|path| path.parent())
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("src");
    let safe = format!("{parent}_{stem}").replace(['/', '\\', ' '], "_");
    let _ = fallback;
    raw.join(format!("{safe}.jsonl"))
}

fn jsonl_is_complete(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let file = File::open(path).map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
    let last = BufReader::new(file)
        .lines()
        .map(|line| line.unwrap_or_default())
        .filter(|line| !line.is_empty())
        .last();
    Ok(last.is_some_and(|line| line.contains("\"kind\":\"done\"")))
}

fn open_jsonl_checkpoint(
    path: &Path,
    source: &str,
    fingerprint: u64,
    features: &OracleFeatures,
) -> Result<(File, Vec<OracleProbe>)> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
    }
    if path.exists() {
        let probes = read_partial_probes(path, source, fingerprint)?;
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
        return Ok((file, probes));
    }

    let mut file = File::create(path).map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
    let lines = format!(
        "{{\"kind\":\"header\",\"schema\":\"jp2lam.perceptual-oracle-raw/1\",\"metric_version\":{},\"source\":{},\"fingerprint\":{}}}\n{{\"kind\":\"features\",{}}}\n",
        json_str(METRIC_VERSION),
        json_str(source),
        fingerprint,
        features_fields(features),
    );
    sync_jsonl(&mut file, &lines)?;
    Ok((file, Vec::new()))
}

fn append_probe(file: &mut File, probe: &OracleProbe) -> Result<()> {
    sync_jsonl(
        file,
        &format!(
            "{{\"kind\":\"probe\",\"quant_quality\":{},\"quant_scale\":{},\"pcrd_body\":{},\"score\":{},\"output_bytes\":{}}}\n",
            probe.quant_quality,
            json_f64(probe.quant_scale),
            probe.pcrd_body,
            json_f64(probe.score),
            probe.output_bytes
        ),
    )
}

fn finish_jsonl_checkpoint(file: &mut File, labels: &[OracleLabel]) -> Result<()> {
    let mut lines = String::new();
    for label in labels {
        lines.push_str(&label_line(label));
        lines.push('\n');
    }
    lines.push_str("{\"kind\":\"done\"}\n");
    sync_jsonl(file, &lines)
}

fn sync_jsonl(file: &mut File, lines: &str) -> Result<()> {
    file.write_all(lines.as_bytes())
        .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
    file.flush()
        .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
    file.sync_data()
        .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
    Ok(())
}

fn read_partial_probes(path: &Path, source: &str, fingerprint: u64) -> Result<Vec<OracleProbe>> {
    let file = File::open(path).map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
    let mut lines = BufReader::new(file).lines();
    let header = lines
        .find_map(|line| match line {
            Ok(line) if !line.trim().is_empty() => Some(Ok(line)),
            Ok(_) => None,
            Err(err) => Some(Err(Jp2LamError::EncodeFailed(err.to_string()))),
        })
        .transpose()?
        .ok_or_else(|| {
            Jp2LamError::EncodeFailed(format!("partial oracle JSONL is empty: {}", path.display()))
        })?;
    let expected_schema = "\"schema\":\"jp2lam.perceptual-oracle-raw/1\"";
    let expected_metric = format!("\"metric_version\":{}", json_str(METRIC_VERSION));
    let expected_source = format!("\"source\":{}", json_str(source));
    let expected_fingerprint = format!("\"fingerprint\":{fingerprint}");
    if !header.contains("\"kind\":\"header\"")
        || !header.contains(expected_schema)
        || !header.contains(&expected_metric)
        || !header.contains(&expected_source)
        || !header.contains(&expected_fingerprint)
    {
        return Err(Jp2LamError::EncodeFailed(format!(
            "partial oracle JSONL does not match this source or metric: {}",
            path.display()
        )));
    }

    let mut probes = Vec::new();
    for line in lines {
        let line = line.map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
        if line.contains("\"kind\":\"probe\"") {
            probes.push(parse_probe_line(&line)?);
        }
    }
    Ok(probes)
}

fn parse_probe_line(line: &str) -> Result<OracleProbe> {
    Ok(OracleProbe {
        quant_quality: json_number(line, "quant_quality")?,
        quant_scale: json_number(line, "quant_scale")?,
        pcrd_body: json_number(line, "pcrd_body")?,
        score: json_number(line, "score")?,
        output_bytes: json_number(line, "output_bytes")?,
    })
}

fn json_number<T>(line: &str, field: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let prefix = format!("\"{field}\":");
    let (_, rest) = line.split_once(&prefix).ok_or_else(|| {
        Jp2LamError::EncodeFailed(format!("oracle JSONL probe is missing `{field}`"))
    })?;
    let end = rest.find(|ch| ch == ',' || ch == '}').unwrap_or(rest.len());
    rest[..end].parse().map_err(|err: T::Err| {
        Jp2LamError::EncodeFailed(format!("invalid `{field}` in oracle JSONL probe: {err}"))
    })
}

fn features_fields(features: &OracleFeatures) -> String {
    format!(
        "\"width\":{},\"height\":{},\"grayscale\":{},\"luma_variance_q10\":{},\"luma_variance_q50\":{},\"luma_variance_q90\":{},\"chroma_variance_q50\":{},\"flat_fraction\":{},\"edge_proxy\":{},\"ll_energy\":{},\"highpass_energy\":{},\"high_low_ratio\":{},\"hh_fraction\":{},\"directional_asymmetry\":{},\"chroma_wavelet_ratio\":{},\"coefficient_near_zero_fraction\":{},\"coefficient_tail_q90\":{},\"coefficient_tail_q99\":{}",
        features.width,
        features.height,
        features.grayscale,
        json_f32(features.luma_variance_q10),
        json_f32(features.luma_variance_q50),
        json_f32(features.luma_variance_q90),
        json_f32(features.chroma_variance_q50),
        json_f32(features.flat_fraction),
        json_f32(features.edge_proxy),
        json_f64(features.ll_energy),
        json_f64(features.highpass_energy),
        json_f64(features.high_low_ratio),
        json_f64(features.hh_fraction),
        json_f64(features.directional_asymmetry),
        json_f64(features.chroma_wavelet_ratio),
        json_f64(features.coefficient_near_zero_fraction),
        json_f64(features.coefficient_tail_q90),
        json_f64(features.coefficient_tail_q99),
    )
}

fn label_line(label: &OracleLabel) -> String {
    let status = match label.status {
        OracleStatus::Floor => "floor",
        OracleStatus::Met => "met",
        OracleStatus::CensoredTop => "censored_top",
    };
    format!(
        "{{\"kind\":\"label\",\"target\":{},\"status\":{},\"best_quant_quality\":{},\"best_quant_scale\":{},\"best_pcrd_body\":{},\"achieved_score\":{},\"output_bytes\":{}}}",
        json_f64(label.target),
        json_str(status),
        json_opt_u8(label.best_quant_quality),
        json_opt_f64(label.best_quant_scale),
        json_opt_u32(label.best_pcrd_body),
        json_f64(label.achieved_score),
        json_opt_u64(label.output_bytes),
    )
}

fn json_str(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn json_f64(value: f64) -> String {
    if value.is_finite() {
        format!("{value}")
    } else {
        "null".into()
    }
}

fn json_f32(value: f32) -> String {
    json_f64(f64::from(value))
}

fn json_opt_u8(value: Option<u8>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".into())
}

fn json_opt_u32(value: Option<u32>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".into())
}

fn json_opt_u64(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".into())
}

fn json_opt_f64(value: Option<f64>) -> String {
    value.map(json_f64).unwrap_or_else(|| "null".into())
}

fn fnv1a_file(path: &Path) -> Result<u64> {
    let bytes = fs::read(path).map_err(|err| Jp2LamError::InvalidInput(err.to_string()))?;
    Ok(fnv1a_bytes(&bytes))
}

fn fnv1a_image(image: &Image) -> u64 {
    let mut hash = fnv1a_bytes(&image.width.to_le_bytes());
    hash ^= fnv1a_bytes(&image.height.to_le_bytes());
    for component in &image.components {
        for sample in &component.data {
            let bits = sample.to_le_bytes();
            hash ^= fnv1a_bytes(&bits);
        }
    }
    hash
}

fn fnv1a_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{
        OracleConfig, OracleFeatures, OracleProbe, OracleStatus, append_probe,
        default_oracle_targets, finish_jsonl_checkpoint, jsonl_is_complete, open_jsonl_checkpoint,
        reduce_labels, sweep_source,
    };
    use crate::model::Image;
    use std::fs;

    fn gray_ramp(width: u32, height: u32) -> Image {
        let n = (width * height) as usize;
        let mut data = Vec::with_capacity(n);
        for y in 0..height {
            for x in 0..width {
                data.push(((x.wrapping_mul(13) + y.wrapping_mul(7)) % 256) as u8);
            }
        }
        Image::from_gray_bytes(width, height, &data).expect("gray")
    }

    fn probe(quality: u8, scale: f64, body: u32, score: f64, bytes: u64) -> OracleProbe {
        OracleProbe {
            quant_quality: quality,
            quant_scale: scale,
            pcrd_body: body,
            score,
            output_bytes: bytes,
        }
    }

    fn features() -> OracleFeatures {
        OracleFeatures {
            width: 32,
            height: 24,
            grayscale: true,
            luma_variance_q10: 0.1,
            luma_variance_q50: 0.2,
            luma_variance_q90: 0.3,
            chroma_variance_q50: 0.0,
            flat_fraction: 0.25,
            edge_proxy: 0.1,
            ll_energy: 1.0,
            highpass_energy: 2.0,
            high_low_ratio: 2.0,
            hh_fraction: 0.25,
            directional_asymmetry: 0.1,
            chroma_wavelet_ratio: 0.0,
            coefficient_near_zero_fraction: 0.5,
            coefficient_tail_q90: 1.0,
            coefficient_tail_q99: 2.0,
        }
    }

    #[test]
    fn reduce_labels_coarsest_meeting_quantizer() {
        let probes = vec![
            probe(50, 4.0, 100, 70.0, 1000),
            probe(75, 1.0, 80, 88.0, 1800),
            probe(75, 1.0, 40, 84.0, 1200),
            probe(90, 0.25, 200, 94.0, 4000),
        ];
        let labels = reduce_labels(&probes, &[80.0, 90.0, 99.0]);
        assert_eq!(labels[0].status, OracleStatus::Met);
        assert_eq!(labels[0].best_quant_quality, Some(75));
        assert_eq!(labels[0].best_pcrd_body, Some(40));
        assert_eq!(labels[1].status, OracleStatus::Met);
        assert_eq!(labels[1].best_quant_quality, Some(90));
        assert_eq!(labels[2].status, OracleStatus::CensoredTop);
        assert!(labels[2].best_quant_scale.is_none());
    }

    #[test]
    fn reduce_labels_floor_when_coarsest_already_meets() {
        let probes = vec![
            probe(50, 4.0, 50, 91.0, 800),
            probe(90, 0.25, 200, 97.0, 4000),
        ];
        let labels = reduce_labels(&probes, &[80.0]);
        assert_eq!(labels[0].status, OracleStatus::Floor);
        assert_eq!(labels[0].best_quant_quality, Some(50));
        assert_eq!(labels[0].best_pcrd_body, Some(50));
    }

    #[test]
    fn synthetic_sweep_writes_resumable_jsonl_and_meets_a_low_floor() {
        let dir = std::env::temp_dir().join(format!(
            "jp2lam-oracle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("dir");
        let image = gray_ramp(32, 24);
        let config = OracleConfig {
            targets: vec![40.0],
            quant_qualities: vec![75, 90, 99],
            body_fractions: vec![1.0, 0.5],
            output_dir: dir.clone(),
        };
        let first = sweep_source(&image, None, &config).expect("sweep");
        assert!(!first.skipped);
        assert_eq!(first.probes.len(), 6);
        assert_eq!(first.labels.len(), 1);
        assert_ne!(first.labels[0].status, OracleStatus::CensoredTop);
        assert!(first.labels[0].achieved_score >= 40.0);
        let text = fs::read_to_string(&first.jsonl_path).expect("jsonl");
        assert!(text.contains("jp2lam.perceptual-oracle-raw/1"));
        assert!(text.contains(jpxl_perceptual::METRIC_VERSION));
        assert!(text.contains("\"kind\":\"done\""));
        let features = first.features.expect("features");
        assert_eq!(features.width, 32);
        assert_eq!(features.height, 24);
        assert!(features.grayscale);
        assert!(features.ll_energy > 0.0);

        let second = sweep_source(&image, None, &config).expect("resume");
        assert!(second.skipped);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_jsonl_recovers_durable_probes_before_done() {
        let dir = std::env::temp_dir().join(format!(
            "jp2lam-oracle-checkpoint-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let path = dir.join("raw").join("src_memory.jsonl");
        let first = probe(50, 1.0, 100, 91.0, 1000);
        {
            let (mut checkpoint, restored) =
                open_jsonl_checkpoint(&path, "<memory>", 17, &features()).expect("new checkpoint");
            assert!(restored.is_empty());
            append_probe(&mut checkpoint, &first).expect("sync probe");
        }

        let (mut checkpoint, restored) =
            open_jsonl_checkpoint(&path, "<memory>", 17, &features()).expect("resume checkpoint");
        assert_eq!(restored, vec![first.clone()]);
        let labels = reduce_labels(&restored, &[80.0]);
        finish_jsonl_checkpoint(&mut checkpoint, &labels).expect("finish checkpoint");
        assert!(jsonl_is_complete(&path).expect("completion"));
        let text = fs::read_to_string(&path).expect("jsonl");
        assert_eq!(text.matches("\"kind\":\"probe\"").count(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_targets_match_the_session_plan() {
        assert_eq!(
            default_oracle_targets(),
            vec![80.0, 85.0, 90.0, 92.5, 95.0, 97.5]
        );
    }
}
