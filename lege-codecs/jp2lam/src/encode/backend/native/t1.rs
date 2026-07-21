use crate::dwt::norms::reversible_exponent;
#[cfg(feature = "counters")]
use crate::encode::counters;
use crate::encode::profile_enter;
use crate::mq::{MqCoder, T1_CTXNO_AGG, T1_CTXNO_UNI, T1_CTXNO_ZC};
use crate::plan::BandOrientation;
use crate::profile::{BlockClass, classify_from_nonzero_fraction};
use crate::simd::PRIMITIVES;
use crate::tier1::flags::FlagGrid;
use crate::tier1::helpers::{
    magnitude_context, sign_context, sign_prediction_bit, zero_coding_context,
};

use rayon::prelude::*;

/// Maximum bitplane count signalled for a subband in the codestream.
/// Per ISO 15444-1 Annex E: Mb(b) = guard_bits - 1 + expn(b). For reversible
/// 5/3 with precision P, expn(b) = P + band_gain(b).
pub(crate) fn band_max_bitplanes(precision: u32, guard_bits: u8, band: BandOrientation) -> u8 {
    let expn = reversible_exponent(precision, band);
    guard_bits.saturating_sub(1).saturating_add(expn)
}

use super::NativeComponentCoefficients;
use super::layout::{NativeCodeBlock, NativeComponentLayout};
use crate::plan::CodeBlockSize;
use crate::tiling::{CodeBlockRect, TileComponentRect, codeblock_rects_for_subband, subband_rects};

// ---------------------------------------------------------------------------
// Public data types (unchanged)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeTier1CodeBlock {
    pub tile_index: u16,
    pub resolution: u8,
    pub band: BandOrientation,
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
    pub width: usize,
    pub height: usize,
    pub coefficient_count: usize,
    pub max_magnitude: u32,
    pub magnitude_bitplanes: u8,
    pub zero_bitplanes: u8,
    /// Fraction of quantized coefficients that are non-zero (0.0–1.0).
    pub nonzero_fraction: f32,
    pub coding_passes: Vec<NativeTier1Pass>,
    pub coefficients: Vec<i32>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeTier1Band {
    pub resolution: u8,
    pub band: BandOrientation,
    pub blocks: Vec<NativeTier1CodeBlock>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeTier1Layout {
    pub bands: Vec<NativeTier1Band>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeEncodedTier1CodeBlock {
    pub tile_index: u16,
    pub resolution: u8,
    pub band: BandOrientation,
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
    pub magnitude_bitplanes: u8,
    pub zero_bitplanes: u8,
    pub block_class: BlockClass,
    pub payload: Vec<u8>,
    pub passes: Vec<NativeEncodedTier1Pass>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeEncodedTier1Band {
    pub resolution: u8,
    pub band: BandOrientation,
    pub blocks: Vec<NativeEncodedTier1CodeBlock>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeEncodedTier1Layout {
    pub bands: Vec<NativeEncodedTier1Band>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeTier1SelectionBand {
    pub resolution: u8,
    pub band: BandOrientation,
    pub selected_passes: Vec<u16>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeTier1SelectionLayout {
    pub bands: Vec<NativeTier1SelectionBand>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeTier1PassKind {
    SignificancePropagation,
    MagnitudeRefinement,
    Cleanup,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeTier1PassTermination {
    TermAll,
    ErTerm,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeTier1PassCodingMode {
    Mq,
    Raw,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeTier1Pass {
    pub kind: NativeTier1PassKind,
    pub bitplane: u8,
    pub pass_index: u16,
    pub coding_mode: NativeTier1PassCodingMode,
    pub termination: NativeTier1PassTermination,
    pub segmark: bool,
    pub significant_before: usize,
    pub newly_significant: usize,
    pub significant_after: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeEncodedTier1Pass {
    pub kind: NativeTier1PassKind,
    pub bitplane: u8,
    pub pass_index: u16,
    pub coding_mode: NativeTier1PassCodingMode,
    pub termination: NativeTier1PassTermination,
    pub segmark: bool,
    /// True when this pass is followed by an MQ termination, starting a new
    /// length-signaling segment in the packet header.
    pub is_terminated: bool,
    pub newly_significant: usize,
    /// Number of already-significant coefficients present when this pass ran.
    /// Non-zero only for MagnitudeRefinement: these are the samples the MR pass
    /// actually refines, giving them non-zero distortion credit in PCRD.
    pub significant_before: usize,
    pub length: usize,
    pub cumulative_length: usize,
    pub distortion_hint: u32,
    /// Annex J ΔMSE numerator in quantized-coefficient units (pre-scaling by
    /// quant_step² × subband_weight). Computed from actual coefficient values;
    /// rate.rs scales this to image-domain MSE for PCRD slope calculation.
    pub mse_numerator: f64,
}

// ---------------------------------------------------------------------------
// Analysis phase (entry point)
// ---------------------------------------------------------------------------

pub(crate) fn analyze_component_layout(layout: &NativeComponentLayout) -> NativeTier1Layout {
    // Default: 8-bit precision with 2 guard bits (the current Gray-lossless lane).
    analyze_component_layout_with(layout, 8, 2)
}

pub(crate) fn analyze_component_layout_with_max_bitplanes<F>(
    layout: &NativeComponentLayout,
    mut max_bitplanes_for_subband: F,
) -> NativeTier1Layout
where
    F: FnMut(u8, BandOrientation) -> u8,
{
    let mut bands = Vec::with_capacity(layout.subbands.len());
    let mut jobs = Vec::new();

    for (band_index, subband) in layout.subbands.iter().enumerate() {
        let mb = max_bitplanes_for_subband(subband.resolution, subband.band);
        bands.push(NativeTier1Band {
            resolution: subband.resolution,
            band: subband.band,
            blocks: Vec::with_capacity(subband.codeblocks.len()),
        });
        jobs.extend(
            subband
                .codeblocks
                .iter()
                .map(|block| (band_index, subband.resolution, subband.band, mb, block)),
        );
    }

    let analyzed: Vec<_> = jobs
        .par_iter()
        .map(|&(band_index, resolution, band, mb, block)| {
            (band_index, analyze_codeblock(resolution, band, mb, block))
        })
        .collect();

    for (band_index, block) in analyzed {
        bands[band_index].blocks.push(block);
    }

    NativeTier1Layout { bands }
}

pub(crate) fn analyze_component_layout_with(
    layout: &NativeComponentLayout,
    precision: u32,
    guard_bits: u8,
) -> NativeTier1Layout {
    analyze_component_layout_with_max_bitplanes(layout, |_, band| {
        band_max_bitplanes(precision, guard_bits, band)
    })
}

#[allow(dead_code)]
pub(crate) fn encode_component_coefficients_with_max_bitplanes<F>(
    coefficients: &NativeComponentCoefficients,
    component_index: u16,
    code_block_size: CodeBlockSize,
    max_bitplanes_for_subband: F,
) -> NativeEncodedTier1Layout
where
    F: FnMut(u8, BandOrientation) -> u8,
{
    encode_component_coefficients_for_tile_with_max_bitplanes(
        coefficients,
        0,
        component_index,
        code_block_size,
        max_bitplanes_for_subband,
    )
}

pub(crate) fn encode_component_coefficients_for_tile_with_max_bitplanes<F>(
    coefficients: &NativeComponentCoefficients,
    tile_index: u16,
    component_index: u16,
    code_block_size: CodeBlockSize,
    mut max_bitplanes_for_subband: F,
) -> NativeEncodedTier1Layout
where
    F: FnMut(u8, BandOrientation) -> u8,
{
    let _p = profile_enter("t1::encode_component_coefficients_with_max_bitplanes");

    let tc = TileComponentRect {
        tile_index,
        component: component_index,
        x0: coefficients.x0 as u32,
        y0: coefficients.y0 as u32,
        x1: (coefficients.x0 + coefficients.width) as u32,
        y1: (coefficients.y0 + coefficients.height) as u32,
    };
    // `subband_rects` applies the reference-grid phase, then translate its
    // rectangles into the packed tile-local coefficient plane used by Tier-1.
    let mut subbands = subband_rects(&tc, coefficients.levels);
    for subband in &mut subbands {
        subband.x0 -= tc.x0;
        subband.x1 -= tc.x0;
        subband.y0 -= tc.y0;
        subband.y1 -= tc.y0;
    }
    let policy = NativeTier1CodingPolicy::default();
    let mut next_block_id = 0usize;

    let bands = subbands
        .into_iter()
        .map(|subband| {
            let max_bitplanes = max_bitplanes_for_subband(subband.resolution, subband.band);
            let blocks = codeblock_rects_for_subband(&subband, code_block_size, &mut next_block_id);

            #[cfg(feature = "counters")]
            counters::TOTAL_BLOCKS
                .fetch_add(blocks.len() as u64, std::sync::atomic::Ordering::Relaxed);

            let encoded_blocks = blocks
                .par_iter()
                .map_init(Vec::<i32>::new, |worker_scratch, block| {
                    pack_codeblock_coefficients_into(coefficients, block, worker_scratch);
                    let scratch = std::mem::take(worker_scratch);
                    let analyzed = analyze_codeblock_coefficients(
                        block.tile_index,
                        block.resolution,
                        block.band,
                        max_bitplanes,
                        block.x0 as usize,
                        block.y0 as usize,
                        block.x1 as usize,
                        block.y1 as usize,
                        scratch,
                    );
                    let encoded = encode_codeblock_with_policy(&analyzed, &policy);
                    *worker_scratch = analyzed.coefficients;
                    worker_scratch.clear();
                    encoded
                })
                .collect();

            NativeEncodedTier1Band {
                resolution: subband.resolution,
                band: subband.band,
                blocks: encoded_blocks,
            }
        })
        .collect();

    NativeEncodedTier1Layout { bands }
}

#[allow(dead_code)]
pub(crate) fn encode_component_coefficients_with(
    coefficients: &NativeComponentCoefficients,
    component_index: u16,
    code_block_size: CodeBlockSize,
    precision: u32,
    guard_bits: u8,
) -> NativeEncodedTier1Layout {
    encode_component_coefficients_with_max_bitplanes(
        coefficients,
        component_index,
        code_block_size,
        |_, band| band_max_bitplanes(precision, guard_bits, band),
    )
}

fn pack_codeblock_coefficients_into(
    coefficients: &NativeComponentCoefficients,
    block: &CodeBlockRect,
    scratch: &mut Vec<i32>,
) {
    let x0 = block.x0 as usize;
    let y0 = block.y0 as usize;
    let x1 = block.x1 as usize;
    let y1 = block.y1 as usize;
    scratch.clear();
    scratch.reserve((x1 - x0) * (y1 - y0));
    for y in y0..y1 {
        let row = y * coefficients.width;
        scratch.extend_from_slice(&coefficients.data[row + x0..row + x1]);
    }
}

fn analyze_codeblock(
    resolution: u8,
    band: BandOrientation,
    mb: u8,
    block: &NativeCodeBlock,
) -> NativeTier1CodeBlock {
    analyze_codeblock_coefficients(
        0,
        resolution,
        band,
        mb,
        block.x0,
        block.y0,
        block.x1,
        block.y1,
        block.coefficients.clone(),
    )
}

#[allow(clippy::too_many_arguments)]
fn analyze_codeblock_coefficients(
    tile_index: u16,
    resolution: u8,
    band: BandOrientation,
    mb: u8,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    coefficients: Vec<i32>,
) -> NativeTier1CodeBlock {
    let width = x1 - x0;
    let height = y1 - y0;

    // Fuse max_magnitude and nonzero_count into a single primitive scan.
    let (max_magnitude, nonzero_count) =
        (PRIMITIVES.analyze.i32_max_magnitude_and_nnz)(&coefficients);

    let magnitude_bitplanes = if max_magnitude == 0 {
        0
    } else {
        (u32::BITS - max_magnitude.leading_zeros()) as u8
    };
    let zero_bitplanes = mb.saturating_sub(magnitude_bitplanes);
    let coding_passes = plan_coding_passes(&coefficients, magnitude_bitplanes);
    let nonzero_fraction = if coefficients.is_empty() {
        0.0f32
    } else {
        nonzero_count as f32 / coefficients.len() as f32
    };

    NativeTier1CodeBlock {
        tile_index,
        resolution,
        band,
        x0,
        y0,
        x1,
        y1,
        width,
        height,
        coefficient_count: coefficients.len(),
        max_magnitude,
        magnitude_bitplanes,
        zero_bitplanes,
        nonzero_fraction,
        coding_passes,
        coefficients,
    }
}

// ---------------------------------------------------------------------------
// Encoding phase (entry point)
// ---------------------------------------------------------------------------

pub(crate) fn encode_placeholder_tier1(layout: &NativeTier1Layout) -> NativeEncodedTier1Layout {
    let _p = profile_enter("t1::encode_placeholder_tier1");

    // Count bands first for statistics
    #[cfg(feature = "counters")]
    {
        let band_count: usize = layout.bands.iter().map(|b| b.blocks.len()).sum();
        counters::TOTAL_BLOCKS.fetch_add(band_count as u64, std::sync::atomic::Ordering::Relaxed);
    }

    let policy = NativeTier1CodingPolicy::default();
    let mut bands = Vec::with_capacity(layout.bands.len());
    let mut jobs = Vec::new();

    for (band_index, band) in layout.bands.iter().enumerate() {
        bands.push(NativeEncodedTier1Band {
            resolution: band.resolution,
            band: band.band,
            blocks: Vec::with_capacity(band.blocks.len()),
        });
        jobs.extend(band.blocks.iter().map(|block| (band_index, block)));
    }

    let encoded: Vec<_> = jobs
        .par_iter()
        .map(|&(band_index, block)| (band_index, encode_codeblock_with_policy(block, &policy)))
        .collect();

    for (band_index, block) in encoded {
        bands[band_index].blocks.push(block);
    }

    NativeEncodedTier1Layout { bands }
}

fn encode_band_with_policy(
    band: &NativeTier1Band,
    policy: &NativeTier1CodingPolicy,
) -> NativeEncodedTier1Band {
    let blocks = band
        .blocks
        .par_iter()
        .map(|block| encode_codeblock_with_policy(block, policy))
        .collect();

    NativeEncodedTier1Band {
        resolution: band.resolution,
        band: band.band,
        blocks,
    }
}

/// Encode one codeblock using TERMALL mode: every pass is individually
/// flushed and produces its own byte stream. Context states persist across
/// passes within the block (only a/c/ct restart at each pass boundary).
///
/// Pass structure follows ISO 15444-1 §C.4 / OpenJPEG convention:
///   • Cleanup only for the most-significant bitplane.
///   • SP → MR → Cleanup for all lower bitplanes.
fn encode_codeblock_with_policy(
    block: &NativeTier1CodeBlock,
    policy: &NativeTier1CodingPolicy,
) -> NativeEncodedTier1CodeBlock {
    let sample_count = block.width.saturating_mul(block.height);
    crate::encode::counters::record_codeblock_worker(
        sample_count
            .saturating_mul(std::mem::size_of::<i32>() + std::mem::size_of::<u16>())
            .saturating_add(sample_count / 2 + 64)
            .saturating_add(
                block.coding_passes.len() * std::mem::size_of::<NativeEncodedTier1Pass>(),
            ),
    );
    if block.magnitude_bitplanes == 0 {
        #[cfg(feature = "counters")]
        counters::EMPTY_BLOCKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NativeEncodedTier1CodeBlock {
            tile_index: block.tile_index,
            resolution: block.resolution,
            band: block.band,
            x0: block.x0,
            y0: block.y0,
            x1: block.x1,
            y1: block.y1,
            magnitude_bitplanes: 0,
            zero_bitplanes: block.zero_bitplanes,
            block_class: BlockClass::Flat,
            payload: Vec::new(),
            passes: Vec::new(),
        };
    }

    if policy.single_termination {
        return encode_codeblock_single_term(block);
    }

    // Pre-allocate MQ output buffer based on codeblock size
    // Rough estimate: half the pixels plus some overhead
    let estimated_capacity = (block.width * block.height) / 2 + 64;
    let mut coder = MqCoder::with_capacity(estimated_capacity);
    let mut flags = FlagGrid::new(block.width, block.height);
    let mut passes: Vec<NativeEncodedTier1Pass> = Vec::with_capacity(block.coding_passes.len());
    let mut payload = Vec::new();
    let mut cumulative_length = 0usize;
    let mut pass_index = 0u16;
    let mut significant_so_far = 0usize;

    let top = block.magnitude_bitplanes - 1;

    // Most-significant bitplane: Cleanup only.
    {
        let coding_style = pass_coding_style(policy, NativeTier1PassKind::Cleanup, top, block);
        let (newly_significant, cl_mse) = cleanup_encode(&mut coder, block, top, &mut flags);
        let cl_bytes = finalize_pass_bytes(
            &mut coder,
            coding_style.mode,
            coding_style.termination,
            coding_style.segmark,
        );
        append_pass(
            &mut passes,
            &mut payload,
            &mut cumulative_length,
            pass_index,
            NativeTier1PassKind::Cleanup,
            top,
            coding_style.mode,
            coding_style.termination,
            coding_style.segmark,
            0,
            newly_significant,
            cl_mse,
            cl_bytes,
            block,
        );
        significant_so_far += newly_significant;
        pass_index += 1;

        // Clear the per-bitplane visited flags before moving to the next bitplane.
        clear_visited_all(&mut flags, block.width, block.height);
    }

    // Lower bitplanes: SP → MR → Cleanup.
    for bitplane in (0..top).rev() {
        // MR refines the coefficients that were significant before this bitplane's SP
        // pass ran. Save the count now so we can credit MR correctly in PCRD.
        let significant_before_sp = significant_so_far;

        let sp_style = pass_coding_style(
            policy,
            NativeTier1PassKind::SignificancePropagation,
            bitplane,
            block,
        );
        initialize_pass_coder(&mut coder, sp_style.mode);
        let (sp_ns, sp_mse) =
            sigpass_encode(&mut coder, block, bitplane, &mut flags, sp_style.mode);
        let sp_bytes = finalize_pass_bytes(
            &mut coder,
            sp_style.mode,
            sp_style.termination,
            sp_style.segmark,
        );
        append_pass(
            &mut passes,
            &mut payload,
            &mut cumulative_length,
            pass_index,
            NativeTier1PassKind::SignificancePropagation,
            bitplane,
            sp_style.mode,
            sp_style.termination,
            sp_style.segmark,
            significant_before_sp,
            sp_ns,
            sp_mse,
            sp_bytes,
            block,
        );
        significant_so_far += sp_ns;
        pass_index += 1;

        let mr_style = pass_coding_style(
            policy,
            NativeTier1PassKind::MagnitudeRefinement,
            bitplane,
            block,
        );
        initialize_pass_coder(&mut coder, mr_style.mode);
        let mr_mse = refpass_encode_mut(&mut coder, block, bitplane, &mut flags, mr_style.mode);
        let mr_bytes = finalize_pass_bytes(
            &mut coder,
            mr_style.mode,
            mr_style.termination,
            mr_style.segmark,
        );
        append_pass(
            &mut passes,
            &mut payload,
            &mut cumulative_length,
            pass_index,
            NativeTier1PassKind::MagnitudeRefinement,
            bitplane,
            mr_style.mode,
            mr_style.termination,
            mr_style.segmark,
            significant_before_sp,
            0,
            mr_mse,
            mr_bytes,
            block,
        );
        pass_index += 1;

        let cl_style = pass_coding_style(policy, NativeTier1PassKind::Cleanup, bitplane, block);
        let (cl_ns, cl_mse) = cleanup_encode(&mut coder, block, bitplane, &mut flags);
        let cl_bytes = finalize_pass_bytes(
            &mut coder,
            cl_style.mode,
            cl_style.termination,
            cl_style.segmark,
        );
        append_pass(
            &mut passes,
            &mut payload,
            &mut cumulative_length,
            pass_index,
            NativeTier1PassKind::Cleanup,
            bitplane,
            cl_style.mode,
            cl_style.termination,
            cl_style.segmark,
            significant_so_far,
            cl_ns,
            cl_mse,
            cl_bytes,
            block,
        );
        significant_so_far += cl_ns;
        pass_index += 1;

        clear_visited_all(&mut flags, block.width, block.height);
    }

    NativeEncodedTier1CodeBlock {
        tile_index: block.tile_index,
        resolution: block.resolution,
        band: block.band,
        x0: block.x0,
        y0: block.y0,
        x1: block.x1,
        y1: block.y1,
        magnitude_bitplanes: block.magnitude_bitplanes,
        zero_bitplanes: block.zero_bitplanes,
        block_class: classify_from_nonzero_fraction(block.nonzero_fraction),
        payload,
        passes,
    }
}

/// Default JPEG 2000 Part-1 codeblock coding: a single MQ-terminated segment
/// per codeblock, with per-pass length snapshots recorded mid-stream (no
/// per-pass restart). Matches OpenJPEG's default (non-TERMALL) behavior.
fn encode_codeblock_single_term(block: &NativeTier1CodeBlock) -> NativeEncodedTier1CodeBlock {
    // Pre-allocate MQ output buffer based on codeblock size
    let estimated_capacity = (block.width * block.height) / 2 + 64;
    let mut coder = MqCoder::with_capacity(estimated_capacity);
    let mut flags = FlagGrid::new(block.width, block.height);
    // (kind, bitplane, newly_significant, snapshot_after, significant_before, mse_numerator)
    let mut metas: Vec<(NativeTier1PassKind, u8, usize, usize, usize, f64)> =
        Vec::with_capacity(block.coding_passes.len());
    let mut significant_so_far = 0usize;

    let top = block.magnitude_bitplanes - 1;

    // Most-significant bitplane: cleanup only, no SP/MR.
    let (ns, cl_mse) = cleanup_encode(&mut coder, block, top, &mut flags);
    metas.push((
        NativeTier1PassKind::Cleanup,
        top,
        ns,
        coder.numbytes(),
        0,
        cl_mse,
    ));
    significant_so_far += ns;
    clear_visited_all(&mut flags, block.width, block.height);

    for bitplane in (0..top).rev() {
        let significant_before_sp = significant_so_far;

        let (sp_ns, sp_mse) = sigpass_encode(
            &mut coder,
            block,
            bitplane,
            &mut flags,
            NativeTier1PassCodingMode::Mq,
        );
        metas.push((
            NativeTier1PassKind::SignificancePropagation,
            bitplane,
            sp_ns,
            coder.numbytes(),
            significant_before_sp,
            sp_mse,
        ));
        significant_so_far += sp_ns;

        let mr_mse = refpass_encode_mut(
            &mut coder,
            block,
            bitplane,
            &mut flags,
            NativeTier1PassCodingMode::Mq,
        );
        metas.push((
            NativeTier1PassKind::MagnitudeRefinement,
            bitplane,
            0,
            coder.numbytes(),
            significant_before_sp,
            mr_mse,
        ));

        let (cl_ns, cl_mse) = cleanup_encode(&mut coder, block, bitplane, &mut flags);
        metas.push((
            NativeTier1PassKind::Cleanup,
            bitplane,
            cl_ns,
            coder.numbytes(),
            significant_so_far,
            cl_mse,
        ));
        significant_so_far += cl_ns;

        clear_visited_all(&mut flags, block.width, block.height);
    }

    // Single termination at end of codeblock.
    let terminated = coder.flush_and_restart();
    let total = terminated.len();

    let mut passes: Vec<NativeEncodedTier1Pass> = Vec::with_capacity(metas.len());
    let mut cumulative = 0usize;
    let mut prev_snap = 0usize;
    let mut sum_lengths = 0usize;
    let last_idx = metas.len() - 1;

    for (i, (kind, bitplane, ns, snap, sb, mse)) in metas.iter().copied().enumerate() {
        let mut length = snap.saturating_sub(prev_snap);
        prev_snap = snap;
        if i == last_idx {
            // Absorb termination overhead / trimming discrepancy.
            length = total.saturating_sub(sum_lengths);
        }
        sum_lengths += length;
        cumulative += length;

        passes.push(NativeEncodedTier1Pass {
            kind,
            bitplane,
            pass_index: i as u16,
            coding_mode: NativeTier1PassCodingMode::Mq,
            termination: NativeTier1PassTermination::TermAll,
            segmark: false,
            is_terminated: i == last_idx,
            newly_significant: ns,
            significant_before: sb,
            length,
            cumulative_length: cumulative,
            distortion_hint: distortion_hint(bitplane, ns, block.max_magnitude),
            mse_numerator: mse,
        });

        #[cfg(feature = "counters")]
        {
            #[allow(unreachable_patterns)]
            match kind {
                NativeTier1PassKind::Cleanup => {
                    counters::CLEANUP_PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                NativeTier1PassKind::SignificancePropagation => {
                    counters::SP_PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                NativeTier1PassKind::MagnitudeRefinement => {
                    counters::MR_PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                _ => {}
            }
            counters::TOTAL_PASS_BYTES
                .fetch_add(length as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }

    NativeEncodedTier1CodeBlock {
        tile_index: block.tile_index,
        resolution: block.resolution,
        band: block.band,
        x0: block.x0,
        y0: block.y0,
        x1: block.x1,
        y1: block.y1,
        magnitude_bitplanes: block.magnitude_bitplanes,
        zero_bitplanes: block.zero_bitplanes,
        block_class: classify_from_nonzero_fraction(block.nonzero_fraction),
        payload: terminated,
        passes,
    }
}

fn append_pass(
    passes: &mut Vec<NativeEncodedTier1Pass>,
    payload: &mut Vec<u8>,
    cumulative_length: &mut usize,
    pass_index: u16,
    kind: NativeTier1PassKind,
    bitplane: u8,
    coding_mode: NativeTier1PassCodingMode,
    termination: NativeTier1PassTermination,
    segmark: bool,
    significant_before: usize,
    newly_significant: usize,
    mse_numerator: f64,
    bytes: Vec<u8>,
    block: &NativeTier1CodeBlock,
) {
    let length = bytes.len();
    payload.extend_from_slice(&bytes);
    *cumulative_length += length;

    // Count passes
    #[cfg(feature = "counters")]
    {
        #[allow(unreachable_patterns)]
        match kind {
            NativeTier1PassKind::Cleanup => {
                counters::CLEANUP_PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            NativeTier1PassKind::SignificancePropagation => {
                counters::SP_PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            NativeTier1PassKind::MagnitudeRefinement => {
                counters::MR_PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {}
        }
        counters::TOTAL_PASS_BYTES.fetch_add(length as u64, std::sync::atomic::Ordering::Relaxed);
    }

    passes.push(NativeEncodedTier1Pass {
        kind,
        bitplane,
        pass_index,
        coding_mode,
        termination,
        segmark,
        is_terminated: true,
        newly_significant,
        significant_before,
        length,
        cumulative_length: *cumulative_length,
        distortion_hint: distortion_hint(bitplane, newly_significant, block.max_magnitude),
        mse_numerator,
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeTier1CodingStyle {
    mode: NativeTier1PassCodingMode,
    termination: NativeTier1PassTermination,
    segmark: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeTier1CodingPolicy {
    lower_mode: NativeTier1PassCodingMode,
    cleanup_termination: NativeTier1PassTermination,
    cleanup_segmark: bool,
    /// When true (default), emit a single MQ-terminated segment per codeblock
    /// with per-pass length snapshots (standard Part-1 behavior). When false,
    /// use the legacy TERMALL/restart path configured by `cleanup_termination`.
    single_termination: bool,
}

impl Default for NativeTier1CodingPolicy {
    fn default() -> Self {
        Self {
            lower_mode: NativeTier1PassCodingMode::Mq,
            cleanup_termination: NativeTier1PassTermination::TermAll,
            cleanup_segmark: false,
            single_termination: true,
        }
    }
}

fn pass_coding_style(
    policy: &NativeTier1CodingPolicy,
    _kind: NativeTier1PassKind,
    _bitplane: u8,
    _block: &NativeTier1CodeBlock,
) -> NativeTier1CodingStyle {
    match _kind {
        NativeTier1PassKind::Cleanup => NativeTier1CodingStyle {
            mode: NativeTier1PassCodingMode::Mq,
            termination: policy.cleanup_termination,
            segmark: policy.cleanup_segmark,
        },
        NativeTier1PassKind::SignificancePropagation | NativeTier1PassKind::MagnitudeRefinement => {
            NativeTier1CodingStyle {
                mode: policy.lower_mode,
                termination: NativeTier1PassTermination::TermAll,
                segmark: false,
            }
        }
    }
}

fn initialize_pass_coder(coder: &mut MqCoder, mode: NativeTier1PassCodingMode) {
    if matches!(mode, NativeTier1PassCodingMode::Raw) {
        coder.bypass_init();
    }
}

fn finalize_pass_bytes(
    coder: &mut MqCoder,
    mode: NativeTier1PassCodingMode,
    termination: NativeTier1PassTermination,
    segmark: bool,
) -> Vec<u8> {
    match mode {
        NativeTier1PassCodingMode::Mq => {
            if segmark {
                coder.segmark_encode();
            }
            match termination {
                NativeTier1PassTermination::TermAll => coder.flush_and_restart(),
                NativeTier1PassTermination::ErTerm => {
                    let bytes = coder.erterm_flush();
                    coder.restart_init();
                    bytes
                }
            }
        }
        NativeTier1PassCodingMode::Raw => coder
            .raw_term_flush_and_restart(matches!(termination, NativeTier1PassTermination::ErTerm)),
    }
}

// ---------------------------------------------------------------------------
// Significance Propagation pass
//
// Traversal: column-major within 4-row stripes (x outer, ci=0..4 inner).
// Processes samples that are NOT yet significant AND have at least one
// significant neighbour.  Marks processed samples as visited (PI).
// ---------------------------------------------------------------------------

fn sigpass_encode(
    coder: &mut MqCoder,
    block: &NativeTier1CodeBlock,
    bitplane: u8,
    flags: &mut FlagGrid,
    mode: NativeTier1PassCodingMode,
) -> (usize, f64) {
    let w = block.width;
    let h = block.height;
    let one = 1u32 << bitplane;
    let mut newly_significant = 0usize;
    let mut mse_num = 0.0f64;

    let full_stripes = h / 4;
    let rem = h % 4;

    for k in (0..full_stripes * 4).step_by(4) {
        for x in 0..w {
            for ci in 0..4usize {
                let y = k + ci;
                sigpass_step(
                    coder,
                    block,
                    x,
                    y,
                    w,
                    one,
                    flags,
                    &mut newly_significant,
                    &mut mse_num,
                    mode,
                );
            }
        }
    }
    if rem > 0 {
        let k = full_stripes * 4;
        for x in 0..w {
            for ci in 0..rem {
                let y = k + ci;
                sigpass_step(
                    coder,
                    block,
                    x,
                    y,
                    w,
                    one,
                    flags,
                    &mut newly_significant,
                    &mut mse_num,
                    mode,
                );
            }
        }
    }

    (newly_significant, mse_num)
}

#[inline]
fn sigpass_step(
    coder: &mut MqCoder,
    block: &NativeTier1CodeBlock,
    x: usize,
    y: usize,
    w: usize,
    one: u32,
    flags: &mut FlagGrid,
    newly_significant: &mut usize,
    mse_num: &mut f64,
    mode: NativeTier1PassCodingMode,
) {
    if flags.is_significant(x, y) {
        return;
    }
    let neighbour_mask = flags.neighbour_mask(x, y);
    if neighbour_mask == 0 {
        return;
    }
    // Sample is not significant but has a significant neighbour — encode ZC.
    let ctx = zero_coding_context(block.band, neighbour_mask);
    // SAFETY: x < w and y < height, so y*w+x is always in bounds
    let coeff = unsafe { *block.coefficients.get_unchecked(y * w + x) };
    let mag = coeff.unsigned_abs();
    let plane_bit = ((mag & one) != 0) as u8;
    encode_symbol(coder, mode, ctx, plane_bit);
    flags.mark_visited(x, y);
    if plane_bit == 1 {
        let sign_bit = (coeff < 0) as u8;
        let (sign_lut_index, _) = flags.cardinal_sign_context(x, y);
        let sign_ctx = sign_context(sign_lut_index);
        let prediction = sign_prediction_bit(sign_lut_index);
        encode_symbol(coder, mode, sign_ctx, sign_bit ^ prediction);
        flags.mark_significant(x, y, sign_bit);
        *newly_significant += 1;
        *mse_num += sig_mse_delta(mag, one);
    }
}

// ---------------------------------------------------------------------------
// Magnitude Refinement pass
//
// Processes samples that were ALREADY significant before this bitplane (i.e.,
// significant AND NOT visited by the current SP pass).
// ---------------------------------------------------------------------------

fn refpass_encode_mut(
    coder: &mut MqCoder,
    block: &NativeTier1CodeBlock,
    bitplane: u8,
    flags: &mut FlagGrid,
    mode: NativeTier1PassCodingMode,
) -> f64 {
    let w = block.width;
    let h = block.height;
    let one = 1u32 << bitplane;
    let mut mse_num = 0.0f64;

    let full_stripes = h / 4;
    let rem = h % 4;

    let mut refine = |coder: &mut MqCoder, flags: &mut FlagGrid, x: usize, y: usize| {
        if !flags.is_significant(x, y) || flags.is_visited(x, y) {
            return;
        }
        let has_sig_neighbor = flags.neighbour_mask(x, y) != 0;
        let ctx = magnitude_context(has_sig_neighbor, flags.has_refinement_history(x, y));
        // SAFETY: x < w and y < height, so y*w+x is always in bounds
        let mag = unsafe { block.coefficients.get_unchecked(y * w + x).unsigned_abs() };
        let plane_bit = ((mag & one) != 0) as u8;
        encode_symbol(coder, mode, ctx, plane_bit);
        flags.mark_refined(x, y);
        mse_num += mr_mse_delta(mag, one);
    };

    for k in (0..full_stripes * 4).step_by(4) {
        for x in 0..w {
            for ci in 0..4usize {
                refine(coder, flags, x, k + ci);
            }
        }
    }
    if rem > 0 {
        let k = full_stripes * 4;
        for x in 0..w {
            for ci in 0..rem {
                refine(coder, flags, x, k + ci);
            }
        }
    }

    mse_num
}

// ---------------------------------------------------------------------------
// Cleanup pass
//
// Traversal: column-major within 4-row stripes.
// Uses AGG run-length coding when an entire column-stripe is "clean" (no
// significant neighbours, not significant, not visited) — equivalent to the
// `*f == 0` fast path in OpenJPEG.
// Clears visited (PI) flags for every sample it visits.
// ---------------------------------------------------------------------------

fn cleanup_encode(
    coder: &mut MqCoder,
    block: &NativeTier1CodeBlock,
    bitplane: u8,
    flags: &mut FlagGrid,
) -> (usize, f64) {
    let w = block.width;
    let h = block.height;
    let one = 1u32 << bitplane;
    let mut newly_significant = 0usize;
    let mut mse_num = 0.0f64;

    let full_stripes = h / 4;
    let rem = h % 4;

    for k in (0..full_stripes * 4).step_by(4) {
        for x in 0..w {
            cleanup_stripe(
                coder,
                block,
                x,
                k,
                4,
                one,
                flags,
                &mut newly_significant,
                &mut mse_num,
            );
        }
    }
    if rem > 0 {
        let k = full_stripes * 4;
        for x in 0..w {
            // Partial stripes never use AGG (spec §C.3.2); use regular per-sample coding.
            cleanup_stripe_partial(
                coder,
                block,
                x,
                k,
                rem,
                one,
                flags,
                &mut newly_significant,
                &mut mse_num,
            );
        }
    }

    (newly_significant, mse_num)
}

/// Full 4-row stripe — may use AGG coding when the stripe is clean.
fn cleanup_stripe(
    coder: &mut MqCoder,
    block: &NativeTier1CodeBlock,
    x: usize,
    k: usize,
    lim: usize, // always 4 here
    one: u32,
    flags: &mut FlagGrid,
    newly_significant: &mut usize,
    mse_num: &mut f64,
) {
    debug_assert_eq!(lim, 4);
    let w = block.width;

    if flags.stripe_is_clean(x, k, lim) {
        // AGG path: encode whether any of the 4 samples becomes significant.
        let runlen = (0..lim).find(|&ci| {
            let y = k + ci;
            // SAFETY: x < w and y < height, so y*w+x is always in bounds
            let mag = unsafe { block.coefficients.get_unchecked(y * w + x).unsigned_abs() };
            (mag & one) != 0
        });

        if let Some(rl) = runlen {
            // At least one sample is significant: encode run-length position.
            coder.encode_with_ctx(T1_CTXNO_AGG, 1);
            coder.encode_with_ctx(T1_CTXNO_UNI, ((rl >> 1) & 1) as u8);
            coder.encode_with_ctx(T1_CTXNO_UNI, (rl & 1) as u8);

            // The sample at `rl` is known significant — encode its sign (partial step).
            {
                let y = k + rl;
                // SAFETY: x < w and y < height, so y*w+x is always in bounds
                let coeff = unsafe { *block.coefficients.get_unchecked(y * w + x) };
                let mag = coeff.unsigned_abs();
                let sign_bit = (coeff < 0) as u8;
                let (sign_lut_index, _) = flags.cardinal_sign_context(x, y);
                let sign_ctx = sign_context(sign_lut_index);
                let prediction = sign_prediction_bit(sign_lut_index);
                coder.encode_with_ctx(sign_ctx, sign_bit ^ prediction);
                flags.mark_significant(x, y, sign_bit);
                *newly_significant += 1;
                flags.clear_visited(x, y);
                *mse_num += sig_mse_delta(mag, one);
            }

            // Regular ZC coding for samples after the run-length position.
            for ci in (rl + 1)..lim {
                let y = k + ci;
                cleanup_sample_regular(
                    coder,
                    block,
                    x,
                    y,
                    one,
                    flags,
                    newly_significant,
                    mse_num,
                    w,
                );
            }
        } else {
            // All 4 samples are insignificant at this bitplane: encode runlen=4.
            coder.encode_with_ctx(T1_CTXNO_AGG, 0);
            // Clear visited flags (should already be false in a clean stripe, but
            // clearing unconditionally keeps the invariant correct).
            for ci in 0..lim {
                flags.clear_visited(x, k + ci);
            }
        }
    } else {
        // Regular per-sample coding.
        for ci in 0..lim {
            let y = k + ci;
            cleanup_sample_regular(
                coder,
                block,
                x,
                y,
                one,
                flags,
                newly_significant,
                mse_num,
                w,
            );
        }
    }
}

/// Partial stripe at the bottom of the codeblock — always uses regular coding.
fn cleanup_stripe_partial(
    coder: &mut MqCoder,
    block: &NativeTier1CodeBlock,
    x: usize,
    k: usize,
    lim: usize,
    one: u32,
    flags: &mut FlagGrid,
    newly_significant: &mut usize,
    mse_num: &mut f64,
) {
    let w = block.width;
    for ci in 0..lim {
        let y = k + ci;
        cleanup_sample_regular(
            coder,
            block,
            x,
            y,
            one,
            flags,
            newly_significant,
            mse_num,
            w,
        );
    }
}

/// Regular (non-AGG) cleanup step for a single sample.
/// Skips samples that are already significant OR visited (PI) by the current SP.
/// Always clears the visited flag.
#[inline]
fn cleanup_sample_regular(
    coder: &mut MqCoder,
    block: &NativeTier1CodeBlock,
    x: usize,
    y: usize,
    one: u32,
    flags: &mut FlagGrid,
    newly_significant: &mut usize,
    mse_num: &mut f64,
    w: usize,
) {
    let visited = flags.is_visited(x, y);
    let significant = flags.is_significant(x, y);

    if significant || visited {
        // Skip — already handled by MR or SP — but always clear PI.
        flags.clear_visited(x, y);
        return;
    }

    let neighbour_mask = flags.neighbour_mask(x, y);
    let ctx = if neighbour_mask != 0 {
        zero_coding_context(block.band, neighbour_mask)
    } else {
        T1_CTXNO_ZC
    };

    // SAFETY: x < w and y < height, so y*w+x is always in bounds
    let coeff = unsafe { *block.coefficients.get_unchecked(y * w + x) };
    let mag = coeff.unsigned_abs();
    let plane_bit = ((mag & one) != 0) as u8;
    coder.encode_with_ctx(ctx, plane_bit);
    flags.clear_visited(x, y);

    if plane_bit == 1 {
        let sign_bit = (coeff < 0) as u8;
        let (sign_lut_index, _) = flags.cardinal_sign_context(x, y);
        let sign_ctx = sign_context(sign_lut_index);
        let prediction = sign_prediction_bit(sign_lut_index);
        coder.encode_with_ctx(sign_ctx, sign_bit ^ prediction);
        flags.mark_significant(x, y, sign_bit);
        *newly_significant += 1;
        *mse_num += sig_mse_delta(mag, one);
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn clear_visited_all(flags: &mut FlagGrid, _width: usize, _height: usize) {
    flags.clear_visited_all();
}

/// Annex-J ΔMSE (quantized units) for a coefficient turning significant at the
/// plane with weight `one = 2^b`, assuming the decoder reconstructs truncated
/// coefficients at the midpoint of their uncertainty interval (see decode/t1.rs)
/// and that quantization truncates (residual uniform in [0,1)):
///   err_before = mag + 0.5 (reconstructed as 0)
///   err_after  = lo + 0.5 - one/2, lo = mag & (one-1)
#[inline]
fn sig_mse_delta(mag: u32, one: u32) -> f64 {
    let half = (one >> 1) as f64;
    let lo = (mag & (one - 1)) as f64;
    (mag as f64 + 0.5).powi(2) - (lo + 0.5 - half).powi(2)
}

/// Annex-J ΔMSE (quantized units) for a magnitude-refinement bit at plane
/// `one = 2^b` under midpoint reconstruction:
///   err_before = lo_hi + 0.5 - one   (entering offset is 2^b)
///   err_after  = lo_lo + 0.5 - one/2 (offset shrinks to 2^(b-1))
#[inline]
fn mr_mse_delta(mag: u32, one: u32) -> f64 {
    let half = (one >> 1) as f64;
    let lo_lo = (mag & (one - 1)) as f64;
    let lo_hi = (mag & ((one << 1) - 1)) as f64;
    (lo_hi + 0.5 - one as f64).powi(2) - (lo_lo + 0.5 - half).powi(2)
}

#[inline]
fn distortion_hint(bitplane: u8, newly_significant: usize, max_magnitude: u32) -> u32 {
    if newly_significant == 0 {
        return 0;
    }
    let plane_weight = 1u32 << bitplane.min(30);
    plane_weight
        .saturating_mul(newly_significant as u32 + 1)
        .saturating_mul((max_magnitude > 0) as u32 + 1)
}

#[inline]
fn encode_symbol(coder: &mut MqCoder, mode: NativeTier1PassCodingMode, ctx: u8, bit: u8) {
    match mode {
        NativeTier1PassCodingMode::Mq => coder.encode_with_ctx(ctx, bit),
        NativeTier1PassCodingMode::Raw => coder.bypass_encode(bit),
    }
}

fn plan_coding_passes(coefficients: &[i32], magnitude_bitplanes: u8) -> Vec<NativeTier1Pass> {
    if magnitude_bitplanes == 0 {
        return Vec::new();
    }

    let mut passes = Vec::with_capacity(1 + magnitude_bitplanes.saturating_sub(1) as usize * 3);
    let mut significant = vec![false; coefficients.len()];
    let mut significant_count = 0usize;
    let mut pass_index = 0u16;
    let top = magnitude_bitplanes - 1;

    // Optimized: direct loop instead of iterator chain
    let mut top_newly_significant = 0;
    for i in 0..coefficients.len() {
        if !significant[i] {
            let magnitude = coefficients[i].unsigned_abs();
            if ((magnitude >> top) & 1) != 0 {
                significant[i] = true;
                top_newly_significant += 1;
            }
        }
    }

    let significant_before = significant_count;
    significant_count += top_newly_significant;
    let significant_after = significant_count;
    passes.push(NativeTier1Pass {
        kind: NativeTier1PassKind::Cleanup,
        bitplane: top,
        pass_index,
        coding_mode: NativeTier1PassCodingMode::Mq,
        termination: NativeTier1PassTermination::TermAll,
        segmark: false,
        significant_before,
        newly_significant: top_newly_significant,
        significant_after,
    });
    pass_index += 1;

    for bitplane in (0..top).rev() {
        // Optimized: direct loop instead of iterator chain
        let mut newly_significant = 0;
        for i in 0..coefficients.len() {
            if !significant[i] {
                let magnitude = coefficients[i].unsigned_abs();
                if ((magnitude >> bitplane) & 1) != 0 {
                    significant[i] = true;
                    newly_significant += 1;
                }
            }
        }

        let significant_before = significant_count;
        significant_count += newly_significant;
        let significant_after = significant_count;
        passes.push(NativeTier1Pass {
            kind: NativeTier1PassKind::SignificancePropagation,
            bitplane,
            pass_index,
            coding_mode: NativeTier1PassCodingMode::Mq,
            termination: NativeTier1PassTermination::TermAll,
            segmark: false,
            significant_before,
            newly_significant,
            significant_after,
        });
        pass_index += 1;

        passes.push(NativeTier1Pass {
            kind: NativeTier1PassKind::MagnitudeRefinement,
            bitplane,
            pass_index,
            coding_mode: NativeTier1PassCodingMode::Mq,
            termination: NativeTier1PassTermination::TermAll,
            segmark: false,
            significant_before: significant_after,
            newly_significant: 0,
            significant_after,
        });
        pass_index += 1;

        passes.push(NativeTier1Pass {
            kind: NativeTier1PassKind::Cleanup,
            bitplane,
            pass_index,
            coding_mode: NativeTier1PassCodingMode::Mq,
            termination: NativeTier1PassTermination::TermAll,
            segmark: false,
            significant_before: significant_after,
            newly_significant: 0,
            significant_after,
        });
        pass_index += 1;
    }

    passes
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        NativeTier1CodingPolicy, NativeTier1Layout, NativeTier1PassCodingMode, NativeTier1PassKind,
        NativeTier1PassTermination, analyze_component_layout, band_max_bitplanes,
        encode_band_with_policy, encode_component_coefficients_for_tile_with_max_bitplanes,
        encode_component_coefficients_with, encode_placeholder_tier1,
    };
    use crate::encode::backend::native::NativeComponentCoefficients;
    use crate::encode::backend::native::layout::build_component_layout;
    use crate::plan::{BandOrientation, CodeBlockSize};

    #[test]
    fn tier1_analysis_computes_bitplane_metadata() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 4,
            height: 4,
            levels: 2,
            data: vec![-38, 36, 0, 16, 144, 0, 0, 16, 0, 0, 0, 0, 64, 64, 0, 0],
        };
        let layout = build_component_layout(
            &coefficients,
            CodeBlockSize {
                width: 64,
                height: 64,
            },
        )
        .expect("build layout");

        let analyzed = analyze_component_layout(&layout);
        let ll = analyzed
            .bands
            .iter()
            .find(|band| band.resolution == 0 && band.band == BandOrientation::Ll)
            .expect("ll band");
        assert_eq!(ll.blocks.len(), 1);
        assert_eq!(ll.blocks[0].max_magnitude, 38);
        assert_eq!(ll.blocks[0].magnitude_bitplanes, 6);
        // Mb(LL) = guard_bits - 1 + expn = 2 - 1 + 8 = 9; zero_bitplanes = 9 - 6 = 3.
        assert_eq!(ll.blocks[0].zero_bitplanes, 3);
        assert_eq!(ll.blocks[0].coding_passes.len(), 16);
        assert_eq!(
            ll.blocks[0].coding_passes[0].kind,
            NativeTier1PassKind::Cleanup
        );
        assert_eq!(
            ll.blocks[0].coding_passes[0].coding_mode,
            NativeTier1PassCodingMode::Mq
        );
        assert_eq!(
            ll.blocks[0].coding_passes[0].termination,
            NativeTier1PassTermination::TermAll
        );
        assert!(!ll.blocks[0].coding_passes[0].segmark);
        assert_eq!(ll.blocks[0].coding_passes[0].bitplane, 5);
        assert_eq!(ll.blocks[0].coding_passes[0].newly_significant, 1);

        let hh2 = analyzed
            .bands
            .iter()
            .find(|band| band.resolution == 2 && band.band == BandOrientation::Hh)
            .expect("resolution 2 hh band");
        assert_eq!(hh2.blocks[0].max_magnitude, 0);
        assert_eq!(hh2.blocks[0].magnitude_bitplanes, 0);
        // Empty HH band: Mb(HH) = guard_bits - 1 + expn = 2 - 1 + 10 = 11.
        assert_eq!(hh2.blocks[0].zero_bitplanes, 11);
        assert!(hh2.blocks[0].coding_passes.is_empty());
    }

    #[test]
    fn direct_coefficient_plane_encoding_matches_legacy_layout_path() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 6,
            height: 6,
            levels: 1,
            data: (0..36).map(|v| v - 18).collect(),
        };
        let code_block_size = CodeBlockSize {
            width: 2,
            height: 2,
        };

        let legacy_layout =
            build_component_layout(&coefficients, code_block_size).expect("build layout");
        let legacy_analyzed = analyze_component_layout(&legacy_layout);
        let legacy_encoded = encode_placeholder_tier1(&legacy_analyzed);

        let direct_encoded =
            encode_component_coefficients_with(&coefficients, 0, code_block_size, 8, 2);

        assert_eq!(direct_encoded, legacy_encoded);
    }

    #[test]
    fn direct_coefficient_plane_encoding_preserves_tile_index() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 6,
            height: 6,
            levels: 1,
            data: (0..36).map(|v| v - 18).collect(),
        };
        let code_block_size = CodeBlockSize {
            width: 2,
            height: 2,
        };

        let encoded = encode_component_coefficients_for_tile_with_max_bitplanes(
            &coefficients,
            7,
            2,
            code_block_size,
            |_, band| band_max_bitplanes(8, 2, band),
        );

        assert!(
            encoded
                .bands
                .iter()
                .flat_map(|band| &band.blocks)
                .all(|block| block.tile_index == 7)
        );
    }

    #[test]
    fn tier1_analysis_preserves_band_and_block_counts() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 6,
            height: 6,
            levels: 1,
            data: (0..36).collect(),
        };
        let layout = build_component_layout(
            &coefficients,
            CodeBlockSize {
                width: 2,
                height: 2,
            },
        )
        .expect("build layout");

        let analyzed = analyze_component_layout(&layout);
        assert_eq!(
            analyzed,
            NativeTier1Layout {
                bands: analyzed.bands.clone(),
            }
        );
        let hh = analyzed
            .bands
            .iter()
            .find(|band| band.band == BandOrientation::Hh)
            .expect("hh band");
        assert_eq!(hh.blocks.len(), 4);
        assert_eq!(hh.blocks[0].coefficient_count, 4);
        assert_eq!(hh.blocks[0].max_magnitude, 28);
        assert_eq!(hh.blocks[0].magnitude_bitplanes, 5);
        assert_eq!(hh.blocks[0].coding_passes.len(), 13);
        assert_eq!(
            hh.blocks[0]
                .coding_passes
                .iter()
                .filter(|pass| pass.kind == NativeTier1PassKind::SignificancePropagation)
                .count(),
            4
        );
    }

    #[test]
    fn placeholder_tier1_encoding_produces_cumulative_pass_lengths() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 4,
            height: 4,
            levels: 2,
            data: vec![-38, 36, 0, 16, 144, 0, 0, 16, 0, 0, 0, 0, 64, 64, 0, 0],
        };
        let layout = build_component_layout(
            &coefficients,
            CodeBlockSize {
                width: 64,
                height: 64,
            },
        )
        .expect("build layout");
        let analyzed = analyze_component_layout(&layout);
        let encoded = encode_placeholder_tier1(&analyzed);

        let ll = encoded
            .bands
            .iter()
            .find(|band| band.resolution == 0 && band.band == BandOrientation::Ll)
            .expect("ll band");
        assert_eq!(ll.blocks[0].passes.len(), 16);
        // In single-termination mode, all bytes live in the code-block payload.
        let total: usize = ll.blocks[0].passes.iter().map(|p| p.length).sum();
        assert!(total > 0);
        assert_eq!(ll.blocks[0].payload.len(), total);
        assert_eq!(
            ll.blocks[0].passes[0].coding_mode,
            NativeTier1PassCodingMode::Mq
        );
        assert_eq!(
            ll.blocks[0].passes[0].termination,
            NativeTier1PassTermination::TermAll
        );
        assert!(!ll.blocks[0].passes[0].segmark);
        assert_eq!(
            ll.blocks[0].passes[0].cumulative_length,
            ll.blocks[0].passes[0].length
        );
        assert_eq!(
            ll.blocks[0].passes[1].cumulative_length,
            ll.blocks[0].passes[0].length + ll.blocks[0].passes[1].length
        );
        assert!(!ll.blocks[0].payload.is_empty());

        let hh2 = encoded
            .bands
            .iter()
            .find(|band| band.resolution == 2 && band.band == BandOrientation::Hh)
            .expect("resolution 2 hh band");
        assert!(hh2.blocks[0].passes.is_empty());
    }

    #[test]
    fn tier1_decoder_roundtrips_native_encoded_codeblock() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 4,
            height: 4,
            levels: 2,
            data: vec![-38, 36, 0, 16, 144, 0, 0, 16, 0, 0, 0, 0, 64, 64, 0, 0],
        };
        let layout = build_component_layout(
            &coefficients,
            CodeBlockSize {
                width: 64,
                height: 64,
            },
        )
        .expect("build layout");
        let analyzed = analyze_component_layout(&layout);
        let encoded = encode_placeholder_tier1(&analyzed);

        let analyzed_ll = analyzed
            .bands
            .iter()
            .find(|band| band.resolution == 0 && band.band == BandOrientation::Ll)
            .expect("analyzed ll");
        let encoded_ll = encoded
            .bands
            .iter()
            .find(|band| band.resolution == 0 && band.band == BandOrientation::Ll)
            .expect("encoded ll");
        let source = &analyzed_ll.blocks[0];
        let encoded_block = &encoded_ll.blocks[0];
        let data = encoded_block.payload.as_slice();

        let decoded = crate::decode::t1::decode_codeblock(
            source.width,
            source.height,
            source.band,
            source.zero_bitplanes + source.magnitude_bitplanes,
            source.zero_bitplanes as u32,
            encoded_block.passes.len() as u32,
            false,
            data,
        )
        .expect("decode codeblock");

        assert_eq!(decoded.coefficients, source.coefficients);
    }

    #[test]
    fn encoding_deterministic_across_calls() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 4,
            height: 4,
            levels: 1,
            data: vec![7, -3, 0, 5, 2, -8, 1, 0, 4, 0, 6, -2, 0, 1, 0, 3],
        };
        let layout = build_component_layout(
            &coefficients,
            CodeBlockSize {
                width: 64,
                height: 64,
            },
        )
        .expect("build layout");
        let analyzed = analyze_component_layout(&layout);
        let enc1 = encode_placeholder_tier1(&analyzed);
        let enc2 = encode_placeholder_tier1(&analyzed);

        for (b1, b2) in enc1.bands.iter().zip(enc2.bands.iter()) {
            for (p1, p2) in b1.blocks.iter().zip(b2.blocks.iter()) {
                assert_eq!(p1.passes, p2.passes, "encoding must be deterministic");
            }
        }
    }

    #[test]
    fn cleanup_policy_can_select_erterm() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 4,
            height: 4,
            levels: 1,
            data: vec![7, -3, 0, 5, 2, -8, 1, 0, 4, 0, 6, -2, 0, 1, 0, 3],
        };
        let layout = build_component_layout(
            &coefficients,
            CodeBlockSize {
                width: 64,
                height: 64,
            },
        )
        .expect("build layout");
        let analyzed = analyze_component_layout(&layout);
        let band = analyzed
            .bands
            .iter()
            .find(|band| band.band == BandOrientation::Ll)
            .expect("ll band");

        let termall = encode_band_with_policy(
            band,
            &NativeTier1CodingPolicy {
                lower_mode: NativeTier1PassCodingMode::Mq,
                cleanup_termination: NativeTier1PassTermination::TermAll,
                cleanup_segmark: false,
                single_termination: false,
            },
        );
        let erterm = encode_band_with_policy(
            band,
            &NativeTier1CodingPolicy {
                lower_mode: NativeTier1PassCodingMode::Mq,
                cleanup_termination: NativeTier1PassTermination::ErTerm,
                cleanup_segmark: false,
                single_termination: false,
            },
        );

        assert_eq!(termall.blocks.len(), 1);
        assert_eq!(erterm.blocks.len(), 1);
        let cleanup_termall: Vec<_> = termall.blocks[0]
            .passes
            .iter()
            .filter(|pass| pass.kind == NativeTier1PassKind::Cleanup)
            .collect();
        let cleanup_erterm: Vec<_> = erterm.blocks[0]
            .passes
            .iter()
            .filter(|pass| pass.kind == NativeTier1PassKind::Cleanup)
            .collect();

        assert!(!cleanup_termall.is_empty());
        assert_eq!(cleanup_termall.len(), cleanup_erterm.len());
        assert!(
            cleanup_erterm
                .iter()
                .all(|pass| pass.termination == NativeTier1PassTermination::ErTerm)
        );
        assert!(
            termall.blocks[0].payload != erterm.blocks[0].payload,
            "erterm policy should change at least one cleanup pass payload"
        );
    }

    #[test]
    fn cleanup_policy_can_enable_segmark() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 4,
            height: 4,
            levels: 1,
            data: vec![7, -3, 0, 5, 2, -8, 1, 0, 4, 0, 6, -2, 0, 1, 0, 3],
        };
        let layout = build_component_layout(
            &coefficients,
            CodeBlockSize {
                width: 64,
                height: 64,
            },
        )
        .expect("build layout");
        let analyzed = analyze_component_layout(&layout);
        let band = analyzed
            .bands
            .iter()
            .find(|band| band.band == BandOrientation::Ll)
            .expect("ll band");

        let default_encoded = encode_band_with_policy(
            band,
            &NativeTier1CodingPolicy {
                lower_mode: NativeTier1PassCodingMode::Mq,
                cleanup_termination: NativeTier1PassTermination::TermAll,
                cleanup_segmark: false,
                single_termination: false,
            },
        );
        let segmark_encoded = encode_band_with_policy(
            band,
            &NativeTier1CodingPolicy {
                lower_mode: NativeTier1PassCodingMode::Mq,
                cleanup_termination: NativeTier1PassTermination::TermAll,
                cleanup_segmark: true,
                single_termination: false,
            },
        );

        let cleanup_segmark: Vec<_> = segmark_encoded.blocks[0]
            .passes
            .iter()
            .filter(|pass| pass.kind == NativeTier1PassKind::Cleanup)
            .collect();

        assert!(!cleanup_segmark.is_empty());
        assert!(cleanup_segmark.iter().all(|pass| pass.segmark));
        assert!(
            default_encoded.blocks[0].payload != segmark_encoded.blocks[0].payload,
            "segmark policy should change at least one cleanup pass payload"
        );
    }

    #[test]
    fn lower_pass_policy_can_select_raw_mode() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 4,
            height: 4,
            levels: 1,
            data: vec![7, -3, 0, 5, 2, -8, 1, 0, 4, 0, 6, -2, 0, 1, 0, 3],
        };
        let layout = build_component_layout(
            &coefficients,
            CodeBlockSize {
                width: 64,
                height: 64,
            },
        )
        .expect("build layout");
        let analyzed = analyze_component_layout(&layout);
        let band = analyzed
            .bands
            .iter()
            .find(|band| band.band == BandOrientation::Ll)
            .expect("ll band");

        let mq_encoded = encode_band_with_policy(
            band,
            &NativeTier1CodingPolicy {
                lower_mode: NativeTier1PassCodingMode::Mq,
                cleanup_termination: NativeTier1PassTermination::TermAll,
                cleanup_segmark: false,
                single_termination: false,
            },
        );
        let raw_encoded = encode_band_with_policy(
            band,
            &NativeTier1CodingPolicy {
                lower_mode: NativeTier1PassCodingMode::Raw,
                cleanup_termination: NativeTier1PassTermination::TermAll,
                cleanup_segmark: false,
                single_termination: false,
            },
        );

        let lower_raw: Vec<_> = raw_encoded.blocks[0]
            .passes
            .iter()
            .filter(|pass| {
                matches!(
                    pass.kind,
                    NativeTier1PassKind::SignificancePropagation
                        | NativeTier1PassKind::MagnitudeRefinement
                )
            })
            .collect();

        assert!(!lower_raw.is_empty());
        assert!(
            lower_raw
                .iter()
                .all(|pass| pass.coding_mode == NativeTier1PassCodingMode::Raw)
        );
        assert!(
            mq_encoded.blocks[0].payload != raw_encoded.blocks[0].payload,
            "raw lower-pass policy should change at least one lower-pass payload"
        );
    }
}
