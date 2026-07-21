//! Tier-1 code-block decoding for the default Part 1 MQ path.
//!
//! This mirrors the encoder pass scan in `encode/backend/native/t1.rs`:
//! top bit-plane cleanup only, then SP -> MR -> cleanup for lower bit-planes.

use crate::error::{Jp2LamError, Result};
use crate::j2k::decode_markers::{CodeBlockStyle, CodestreamHeader, WaveletTransform};
use crate::mq::{MqDecoder, T1_CTXNO_AGG, T1_CTXNO_UNI, T1_CTXNO_ZC};
use crate::plan::BandOrientation;
use crate::tier1::consts::{T1_SIGMA_S, T1_SIGMA_SE, T1_SIGMA_SW};
use crate::tier1::flags::FlagGrid;
use crate::tier1::helpers::{
    magnitude_context, sign_context, sign_prediction_bit, zero_coding_context,
};

use super::stats::StatsSink;
use super::t2::{DecodedCodewordSegment, DecodedTilePackets};
use rayon::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedCodeBlockCoefficients {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) coefficients: Vec<i32>,
}

/// A fully decoded tile-component coefficient plane, already in the exact
/// numeric domain the inverse DWT consumes.
///
/// Reversible 5/3 coding carries no quantization, so Tier-1 writes the raw
/// integer magnitudes the inverse 5/3 lifting steps expect. Irreversible 9/7
/// coding is dequantized as each code block is emitted, so Tier-1 writes the
/// final `f32` subband samples directly and no separate full-image `i32`
/// coefficient plane or dequantization sweep is materialized.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CoefficientPlane {
    Integer(Vec<i32>),
    Real(Vec<f32>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DecodedTileCoefficients {
    pub(crate) component: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) plane: CoefficientPlane,
}

impl DecodedTileCoefficients {
    /// Take the reversible integer plane, erroring if this tile was decoded as
    /// an irreversible (dequantized `f32`) plane.
    pub(crate) fn into_integer(self) -> Result<Vec<i32>> {
        match self.plane {
            CoefficientPlane::Integer(values) => Ok(values),
            CoefficientPlane::Real(_) => Err(invalid(
                "reversible reconstruction requested for an irreversible coefficient plane",
            )),
        }
    }

    /// Take the irreversible, already-dequantized `f32` plane, erroring if this
    /// tile was decoded as a reversible integer plane.
    pub(crate) fn into_real(self) -> Result<Vec<f32>> {
        match self.plane {
            CoefficientPlane::Real(values) => Ok(values),
            CoefficientPlane::Integer(_) => Err(invalid(
                "irreversible reconstruction requested for a reversible coefficient plane",
            )),
        }
    }
}

pub(crate) struct Tier1Scratch {
    coefficients: Vec<i32>,
    flags: FlagGrid,
}

impl Tier1Scratch {
    pub(crate) fn new() -> Self {
        Self {
            coefficients: Vec::new(),
            flags: FlagGrid::new(0, 0),
        }
    }

    fn reset(&mut self, width: usize, height: usize) -> Result<()> {
        let coefficient_count = width
            .checked_mul(height)
            .ok_or_else(|| invalid("code-block coefficient count overflow"))?;
        self.coefficients.resize(coefficient_count, 0);
        self.coefficients.fill(0);
        self.flags.reset(width, height);
        Ok(())
    }
}

pub(crate) fn decode_tile_components(
    header: &CodestreamHeader,
    packets: &DecodedTilePackets<'_>,
) -> Result<Vec<DecodedTileCoefficients>> {
    let mut stats = StatsSink::disabled();
    decode_tile_components_profiled(header, packets, &mut stats)
}

pub(crate) fn decode_tile_components_profiled(
    header: &CodestreamHeader,
    packets: &DecodedTilePackets<'_>,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<DecodedTileCoefficients>> {
    let mut scratch = Tier1Scratch::new();
    decode_tile_components_with_scratch(header, packets, stats, &mut scratch)
}

pub(crate) fn decode_tile_components_with_scratch(
    header: &CodestreamHeader,
    packets: &DecodedTilePackets<'_>,
    stats: &mut StatsSink<'_>,
    scratch: &mut Tier1Scratch,
) -> Result<Vec<DecodedTileCoefficients>> {
    let width =
        usize::try_from(header.siz.width).map_err(|_| invalid("tile width exceeds usize"))?;
    let height =
        usize::try_from(header.siz.height).map_err(|_| invalid("tile height exceeds usize"))?;
    let component_count = header.siz.components.len();
    // Irreversible 9/7 code blocks are dequantized as they are written, so the
    // tile plane is the final `f32` subband image. Reversible 5/3 has no
    // quantization and keeps integer magnitudes for the inverse 5/3 lifting.
    let irreversible = matches!(header.cod.transform, WaveletTransform::Irreversible97);
    let mut components = (0..component_count)
        .map(|component| DecodedTileCoefficients {
            component,
            width,
            height,
            plane: if irreversible {
                CoefficientPlane::Real(vec![0.0f32; width * height])
            } else {
                CoefficientPlane::Integer(vec![0i32; width * height])
            },
        })
        .collect::<Vec<_>>();
    // Serial when profiling (per-pass attribution needs one timeline) or when
    // the active worker budget is one (`DecodeConcurrency::Serial` installs a
    // one-thread pool). Otherwise fan the code blocks across the request's
    // worker budget; each block writes into its own disjoint plane rectangle.
    if stats.is_enabled() || rayon::current_num_threads() <= 1 {
        for block in &packets.codeblocks {
            let params = block_params(header, block)?;
            decode_codeblock_segments_into(
                params.block_width,
                params.block_height,
                block.band,
                params.max_bitplanes,
                block.zero_bitplanes,
                block.passes,
                header.cod.code_block_style,
                &block.segments,
                scratch,
                stats,
            )?;
            let component = components
                .get_mut(block.component)
                .ok_or_else(|| invalid("decoded code-block references invalid component"))?;
            let output_start = stats.start();
            write_block_into_plane(
                &mut component.plane,
                width,
                block.x0,
                block.y0,
                params.block_width,
                params.block_height,
                &scratch.coefficients,
                params.step,
            )?;
            stats.finish(output_start, |stats, elapsed| {
                stats.tier1_block_output_ns = stats.tier1_block_output_ns.saturating_add(elapsed);
            });
        }
    } else {
        decode_codeblocks_parallel(header, packets, width, height, &mut components)?;
    }

    Ok(components)
}

struct BlockParams {
    block_width: usize,
    block_height: usize,
    max_bitplanes: u8,
    /// Scalar dequantization step for the irreversible 9/7 path; `None` for the
    /// reversible 5/3 path, which carries no quantization.
    step: Option<f32>,
}

/// Resolve a code block's decode geometry, magnitude bit-plane count, and (for
/// the irreversible path) its fused dequantization step from the codestream
/// headers. Shared by the serial and parallel Tier-1 loops.
fn block_params(header: &CodestreamHeader, block: &super::t2::DecodedCodeBlock<'_>) -> Result<BlockParams> {
    // Per-component quantization: a QCC override for this component, else the
    // default QCD (ISO/IEC 15444-1 Annex A.6.5).
    let comp_quant = header.quant_for(block.component);
    let quant = *comp_quant
        .steps
        .get(block.band_index % comp_quant.steps.len())
        .ok_or_else(|| invalid("missing quantization step for decoded subband"))?;
    let max_bitplanes = comp_quant.guard_bits.saturating_sub(1) + quant.exponent;
    let step = if matches!(header.cod.transform, WaveletTransform::Irreversible97) {
        let precision = header
            .siz
            .components
            .get(block.component)
            .ok_or_else(|| invalid("decoded code-block references invalid component"))?
            .precision;
        Some(super::reconstruct::quant_step(
            u32::from(precision),
            block.band,
            quant,
        ))
    } else {
        None
    };
    Ok(BlockParams {
        block_width: (block.x1 - block.x0) as usize,
        block_height: (block.y1 - block.y0) as usize,
        max_bitplanes,
        step,
    })
}

/// Write one decoded code block's magnitudes into its tile-component plane,
/// applying fused dequantization for the irreversible (`Real`) plane.
#[allow(clippy::too_many_arguments)]
fn write_block_into_plane(
    plane: &mut CoefficientPlane,
    tile_width: usize,
    x0: u32,
    y0: u32,
    block_width: usize,
    block_height: usize,
    coefficients: &[i32],
    step: Option<f32>,
) -> Result<()> {
    match plane {
        CoefficientPlane::Integer(values) => copy_block_to_tile(
            tile_width,
            values,
            x0,
            y0,
            block_width,
            block_height,
            coefficients,
        ),
        CoefficientPlane::Real(values) => {
            let step =
                step.ok_or_else(|| invalid("irreversible plane write missing quantization step"))?;
            dequantize_block_to_tile(
                tile_width,
                values,
                x0,
                y0,
                block_width,
                block_height,
                coefficients,
                step,
            )
        }
    }
}

/// A `Send`/`Sync` view of every tile-component plane as raw base pointers, so
/// the parallel Tier-1 loop can write each code block into its own disjoint
/// rectangle without a per-block result allocation.
enum PlanePtr {
    Integer(*mut i32),
    Real(*mut f32),
}

struct BlockPlanes {
    planes: Vec<PlanePtr>,
    tile_width: usize,
    tile_len: usize,
}

// SAFETY: `BlockPlanes` only ever hands out `&mut` to pairwise-disjoint plane
// rectangles (one code block each). The raw pointers alias the `components`
// planes, which outlive every parallel task, and no task reads another task's
// region, so sharing the base pointers across threads is sound.
unsafe impl Send for BlockPlanes {}
unsafe impl Sync for BlockPlanes {}

impl BlockPlanes {
    fn new(components: &mut [DecodedTileCoefficients], width: usize, height: usize) -> Self {
        let planes = components
            .iter_mut()
            .map(|component| match &mut component.plane {
                CoefficientPlane::Integer(values) => PlanePtr::Integer(values.as_mut_ptr()),
                CoefficientPlane::Real(values) => PlanePtr::Real(values.as_mut_ptr()),
            })
            .collect();
        Self {
            planes,
            tile_width: width,
            tile_len: width.saturating_mul(height),
        }
    }

    /// # Safety
    /// The caller must guarantee that the `(component, rectangle)` written here
    /// does not overlap any other rectangle written concurrently. Tier-1 code
    /// blocks tile disjoint subband rectangles of each component plane, so this
    /// invariant holds across the parallel loop.
    #[allow(clippy::too_many_arguments)]
    unsafe fn write_block(
        &self,
        component: usize,
        x0: usize,
        y0: usize,
        block_width: usize,
        block_height: usize,
        coefficients: &[i32],
        step: Option<f32>,
    ) -> Result<()> {
        let plane = self
            .planes
            .get(component)
            .ok_or_else(|| invalid("decoded code-block references invalid component"))?;
        for y in 0..block_height {
            let dst = (y0 + y)
                .checked_mul(self.tile_width)
                .and_then(|row| row.checked_add(x0))
                .ok_or_else(|| invalid("block copy offset overflow"))?;
            if dst.checked_add(block_width).is_none_or(|end| end > self.tile_len) {
                return Err(invalid("decoded block extends past tile"));
            }
            let src_row = &coefficients[y * block_width..y * block_width + block_width];
            match *plane {
                PlanePtr::Integer(ptr) => {
                    // SAFETY: `dst..dst + block_width` is in-bounds (checked
                    // above) and disjoint from every other task's rectangle.
                    let dst_slice =
                        unsafe { std::slice::from_raw_parts_mut(ptr.add(dst), block_width) };
                    dst_slice.copy_from_slice(src_row);
                }
                PlanePtr::Real(ptr) => {
                    let step = step.ok_or_else(|| {
                        invalid("irreversible plane write missing quantization step")
                    })?;
                    // SAFETY: as above.
                    let dst_slice =
                        unsafe { std::slice::from_raw_parts_mut(ptr.add(dst), block_width) };
                    for (out, &q) in dst_slice.iter_mut().zip(src_row) {
                        *out = crate::simd::scalar::dequantize_i32_to_f32(q, step);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Decode every code block across the active worker budget, writing each into
/// its disjoint plane rectangle with a per-worker reusable [`Tier1Scratch`].
fn decode_codeblocks_parallel(
    header: &CodestreamHeader,
    packets: &DecodedTilePackets<'_>,
    width: usize,
    height: usize,
    components: &mut [DecodedTileCoefficients],
) -> Result<()> {
    let planes = BlockPlanes::new(components, width, height);
    let style = header.cod.code_block_style;
    packets
        .codeblocks
        .par_iter()
        .try_for_each_init(Tier1Scratch::new, |scratch, block| -> Result<()> {
            let params = block_params(header, block)?;
            let mut sink = StatsSink::disabled();
            decode_codeblock_segments_into(
                params.block_width,
                params.block_height,
                block.band,
                params.max_bitplanes,
                block.zero_bitplanes,
                block.passes,
                style,
                &block.segments,
                scratch,
                &mut sink,
            )?;
            // SAFETY: code blocks tile pairwise-disjoint rectangles of each
            // component plane, so no two parallel writes overlap.
            unsafe {
                planes.write_block(
                    block.component,
                    block.x0 as usize,
                    block.y0 as usize,
                    params.block_width,
                    params.block_height,
                    &scratch.coefficients,
                    params.step,
                )
            }
        })
}

#[allow(dead_code)]
pub(crate) fn decode_tile_coefficients(
    header: &CodestreamHeader,
    packets: &DecodedTilePackets<'_>,
) -> Result<DecodedTileCoefficients> {
    let mut components = decode_tile_components(header, packets)?;
    if components.len() != 1 {
        return Err(invalid(
            "single-component decode requested for multi-component tile",
        ));
    }
    Ok(components.remove(0))
}

/// Consume the segmentation symbol that closes a cleanup pass: four symbols
/// coded with the UNIFORM context spelling `0b1010` (ISO/IEC 15444-1 D.5).
/// A different value means the codeword segment has desynchronized.
fn read_segmentation_symbol(decoder: &mut MqDecoder<'_>) -> Result<()> {
    let mut value = 0u8;
    for _ in 0..4 {
        value = (value << 1) | decoder.decode_with_ctx(T1_CTXNO_UNI);
    }
    if value != 0xA {
        return Err(invalid(format!(
            "segmentation symbol mismatch: expected 0xA, found {value:#x}"
        )));
    }
    Ok(())
}

/// Decode one code-block's coding passes.
///
/// `segmentation_symbols` mirrors the COD code-block style bit (ISO/IEC
/// 15444-1 D.5): when set, each cleanup pass is followed by the four-symbol
/// `0xA` marker coded with the UNIFORM context, which must be consumed to
/// keep the MQ decoder aligned.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_codeblock(
    width: usize,
    height: usize,
    band: BandOrientation,
    max_bitplanes: u8,
    zero_bitplanes: u32,
    pass_count: u32,
    segmentation_symbols: bool,
    data: &[u8],
) -> Result<DecodedCodeBlockCoefficients> {
    let segment = DecodedCodewordSegment {
        passes: pass_count,
        data: std::borrow::Cow::Borrowed(data),
    };
    decode_codeblock_segments(
        width,
        height,
        band,
        max_bitplanes,
        zero_bitplanes,
        pass_count,
        CodeBlockStyle {
            segmentation_symbols,
            ..CodeBlockStyle::default()
        },
        std::slice::from_ref(&segment),
    )
}

#[allow(clippy::too_many_arguments)]
fn decode_codeblock_segments(
    width: usize,
    height: usize,
    band: BandOrientation,
    max_bitplanes: u8,
    zero_bitplanes: u32,
    pass_count: u32,
    style: CodeBlockStyle,
    segments: &[DecodedCodewordSegment<'_>],
) -> Result<DecodedCodeBlockCoefficients> {
    let mut scratch = Tier1Scratch::new();
    let mut stats = StatsSink::disabled();
    decode_codeblock_segments_into(
        width,
        height,
        band,
        max_bitplanes,
        zero_bitplanes,
        pass_count,
        style,
        segments,
        &mut scratch,
        &mut stats,
    )?;
    Ok(DecodedCodeBlockCoefficients {
        width,
        height,
        coefficients: scratch.coefficients,
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_codeblock_segments_into(
    width: usize,
    height: usize,
    band: BandOrientation,
    max_bitplanes: u8,
    zero_bitplanes: u32,
    pass_count: u32,
    style: CodeBlockStyle,
    segments: &[DecodedCodewordSegment<'_>],
    scratch: &mut Tier1Scratch,
    stats: &mut StatsSink<'_>,
) -> Result<()> {
    scratch.reset(width, height)?;
    if pass_count == 0 {
        return Ok(());
    }
    let segment_passes = segments.iter().try_fold(0u32, |total, segment| {
        total
            .checked_add(segment.passes)
            .ok_or_else(|| invalid("codeword-segment pass count overflow"))
    })?;
    if segment_passes != pass_count {
        return Err(invalid(format!(
            "codeword segments carry {segment_passes} passes, expected {pass_count}"
        )));
    }

    let zero_bitplanes = u8::try_from(zero_bitplanes)
        .map_err(|_| invalid("zero-bitplane count exceeds supported range"))?;
    let magnitude_bitplanes = max_bitplanes
        .checked_sub(zero_bitplanes)
        .ok_or_else(|| invalid("zero-bitplane count exceeds subband bit-plane count"))?;
    if magnitude_bitplanes == 0 {
        return Err(invalid("non-empty code-block has no magnitude bit-planes"));
    }
    let max_passes = 1u32 + 3u32 * u32::from(magnitude_bitplanes.saturating_sub(1));
    if pass_count > max_passes {
        return Err(invalid(format!(
            "code-block pass count {pass_count} exceeds maximum {max_passes} for {magnitude_bitplanes} magnitude bit-planes"
        )));
    }

    let top = magnitude_bitplanes - 1;
    let mut pass_index = 0u32;
    let mut contexts = None;

    for segment in segments {
        if segment.passes == 0 {
            return Err(invalid("codeword segment carries zero coding passes"));
        }
        let mut decoder = match contexts {
            Some(saved) if !style.reset_contexts => {
                MqDecoder::new_with_contexts(&segment.data, saved)
            }
            _ => MqDecoder::new(&segment.data),
        };

        for _ in 0..segment.passes {
            let (pass_kind, bitplane) = pass_kind_and_bitplane(pass_index, top)?;
            let pass_start = stats.start();
            match pass_kind {
                PassKind::Significance => sigpass_decode(
                    &mut decoder,
                    band,
                    bitplane,
                    &mut scratch.flags,
                    &mut scratch.coefficients,
                    width,
                    height,
                    style.vertical_causal,
                ),
                PassKind::Refinement => refpass_decode(
                    &mut decoder,
                    bitplane,
                    &mut scratch.flags,
                    &mut scratch.coefficients,
                    width,
                    height,
                    style.vertical_causal,
                ),
                PassKind::Cleanup => {
                    cleanup_decode(
                        &mut decoder,
                        band,
                        bitplane,
                        &mut scratch.flags,
                        &mut scratch.coefficients,
                        width,
                        height,
                        style.vertical_causal,
                    );
                    if style.segmentation_symbols {
                        read_segmentation_symbol(&mut decoder)?;
                    }
                    debug_assert!(
                        !scratch.flags.has_visited(),
                        "cleanup pass must clear every visited flag"
                    );
                }
            }
            stats.finish(pass_start, |stats, elapsed| match pass_kind {
                PassKind::Significance => {
                    stats.tier1_significance_ns =
                        stats.tier1_significance_ns.saturating_add(elapsed);
                }
                PassKind::Refinement => {
                    stats.tier1_refinement_ns = stats.tier1_refinement_ns.saturating_add(elapsed);
                }
                PassKind::Cleanup => {
                    stats.tier1_cleanup_ns = stats.tier1_cleanup_ns.saturating_add(elapsed);
                }
            });
            pass_index += 1;
            if style.reset_contexts {
                decoder.reset_contexts();
            }
        }
        contexts = Some(decoder.contexts());
    }

    for (index, coefficient) in scratch.coefficients.iter_mut().enumerate() {
        if scratch.flags.is_negative_idx(index) {
            *coefficient = -*coefficient;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum PassKind {
    Significance,
    Refinement,
    Cleanup,
}

fn pass_kind_and_bitplane(pass_index: u32, top: u8) -> Result<(PassKind, u8)> {
    if pass_index == 0 {
        return Ok((PassKind::Cleanup, top));
    }
    let after_top = pass_index - 1;
    let plane_offset = after_top / 3 + 1;
    let bitplane = u32::from(top)
        .checked_sub(plane_offset)
        .and_then(|value| u8::try_from(value).ok())
        .ok_or_else(|| invalid("coding-pass index exceeds magnitude bit-planes"))?;
    let kind = match after_top % 3 {
        0 => PassKind::Significance,
        1 => PassKind::Refinement,
        _ => PassKind::Cleanup,
    };
    Ok((kind, bitplane))
}

fn sigpass_decode(
    decoder: &mut MqDecoder<'_>,
    band: BandOrientation,
    bitplane: u8,
    flags: &mut FlagGrid,
    magnitudes: &mut [i32],
    width: usize,
    height: usize,
    vertical_causal: bool,
) {
    let full_stripes = height / 4;
    let rem = height % 4;

    for k in (0..full_stripes * 4).step_by(4) {
        for x in 0..width {
            for ci in 0..4 {
                sigpass_step(
                    decoder,
                    band,
                    bitplane,
                    flags,
                    magnitudes,
                    width,
                    x,
                    k + ci,
                    vertical_causal,
                );
            }
        }
    }
    if rem > 0 {
        let k = full_stripes * 4;
        for x in 0..width {
            for ci in 0..rem {
                sigpass_step(
                    decoder,
                    band,
                    bitplane,
                    flags,
                    magnitudes,
                    width,
                    x,
                    k + ci,
                    vertical_causal,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn sigpass_step(
    decoder: &mut MqDecoder<'_>,
    band: BandOrientation,
    bitplane: u8,
    flags: &mut FlagGrid,
    magnitudes: &mut [i32],
    width: usize,
    x: usize,
    y: usize,
    vertical_causal: bool,
) {
    if flags.is_significant(x, y) {
        return;
    }
    let neighbour_mask = neighbour_mask(flags, x, y, vertical_causal);
    if neighbour_mask == 0 {
        return;
    }
    let ctx = zero_coding_context(band, neighbour_mask);
    let plane_bit = decoder.decode_with_ctx(ctx);
    flags.mark_visited(x, y);
    if plane_bit != 0 {
        decode_new_significant(
            decoder,
            bitplane,
            flags,
            magnitudes,
            width,
            x,
            y,
            vertical_causal,
        );
    }
}

fn refpass_decode(
    decoder: &mut MqDecoder<'_>,
    bitplane: u8,
    flags: &mut FlagGrid,
    magnitudes: &mut [i32],
    width: usize,
    height: usize,
    vertical_causal: bool,
) {
    let full_stripes = height / 4;
    let rem = height % 4;

    for k in (0..full_stripes * 4).step_by(4) {
        for x in 0..width {
            for ci in 0..4 {
                refpass_step(
                    decoder,
                    bitplane,
                    flags,
                    magnitudes,
                    width,
                    x,
                    k + ci,
                    vertical_causal,
                );
            }
        }
    }
    if rem > 0 {
        let k = full_stripes * 4;
        for x in 0..width {
            for ci in 0..rem {
                refpass_step(
                    decoder,
                    bitplane,
                    flags,
                    magnitudes,
                    width,
                    x,
                    k + ci,
                    vertical_causal,
                );
            }
        }
    }
}

fn refpass_step(
    decoder: &mut MqDecoder<'_>,
    bitplane: u8,
    flags: &mut FlagGrid,
    magnitudes: &mut [i32],
    width: usize,
    x: usize,
    y: usize,
    vertical_causal: bool,
) {
    if !flags.is_significant(x, y) || flags.is_visited(x, y) {
        return;
    }
    let has_sig_neighbor = neighbour_mask(flags, x, y, vertical_causal) != 0;
    let ctx = magnitude_context(has_sig_neighbor, flags.has_refinement_history(x, y));
    let plane_bit = decoder.decode_with_ctx(ctx);
    // Midpoint update. Entering MR at plane b the magnitude carries a 2^b
    // offset (midpoint of the previous uncertainty interval [0, 2^(b+1))).
    // Learning bit b halves the interval: offset becomes 2^(b-1).
    //   bit=1: known += 2^b            → mag += 2^(b-1)
    //   bit=0: known unchanged         → mag -= 2^b - 2^(b-1)
    // At b=0 this reduces to bit=1: exact, bit=0: mag -= 1 (exact).
    let one = 1i32 << bitplane;
    let half = one >> 1;
    let idx = y * width + x;
    if plane_bit != 0 {
        magnitudes[idx] += half;
    } else {
        magnitudes[idx] -= one - half;
    }
    flags.mark_refined(x, y);
}

fn cleanup_decode(
    decoder: &mut MqDecoder<'_>,
    band: BandOrientation,
    bitplane: u8,
    flags: &mut FlagGrid,
    magnitudes: &mut [i32],
    width: usize,
    height: usize,
    vertical_causal: bool,
) {
    let full_stripes = height / 4;
    let rem = height % 4;

    for k in (0..full_stripes * 4).step_by(4) {
        for x in 0..width {
            cleanup_stripe(
                decoder,
                band,
                bitplane,
                flags,
                magnitudes,
                width,
                x,
                k,
                4,
                vertical_causal,
            );
        }
    }
    if rem > 0 {
        let k = full_stripes * 4;
        for x in 0..width {
            for ci in 0..rem {
                cleanup_sample_regular(
                    decoder,
                    band,
                    bitplane,
                    flags,
                    magnitudes,
                    width,
                    x,
                    k + ci,
                    vertical_causal,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cleanup_stripe(
    decoder: &mut MqDecoder<'_>,
    band: BandOrientation,
    bitplane: u8,
    flags: &mut FlagGrid,
    magnitudes: &mut [i32],
    width: usize,
    x: usize,
    k: usize,
    lim: usize,
    vertical_causal: bool,
) {
    debug_assert_eq!(lim, 4);
    if flags.stripe_is_clean_with_causal(x, k, lim, vertical_causal) {
        if decoder.decode_with_ctx(T1_CTXNO_AGG) == 0 {
            for ci in 0..lim {
                flags.clear_visited(x, k + ci);
            }
            return;
        }

        let hi = decoder.decode_with_ctx(T1_CTXNO_UNI);
        let lo = decoder.decode_with_ctx(T1_CTXNO_UNI);
        let runlen = ((hi << 1) | lo) as usize;
        let y = k + runlen;
        decode_new_significant(
            decoder,
            bitplane,
            flags,
            magnitudes,
            width,
            x,
            y,
            vertical_causal,
        );
        flags.clear_visited(x, y);

        for ci in (runlen + 1)..lim {
            cleanup_sample_regular(
                decoder,
                band,
                bitplane,
                flags,
                magnitudes,
                width,
                x,
                k + ci,
                vertical_causal,
            );
        }
    } else {
        for ci in 0..lim {
            cleanup_sample_regular(
                decoder,
                band,
                bitplane,
                flags,
                magnitudes,
                width,
                x,
                k + ci,
                vertical_causal,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cleanup_sample_regular(
    decoder: &mut MqDecoder<'_>,
    band: BandOrientation,
    bitplane: u8,
    flags: &mut FlagGrid,
    magnitudes: &mut [i32],
    width: usize,
    x: usize,
    y: usize,
    vertical_causal: bool,
) {
    let visited = flags.is_visited(x, y);
    let significant = flags.is_significant(x, y);
    if significant || visited {
        flags.clear_visited(x, y);
        return;
    }

    let neighbour_mask = neighbour_mask(flags, x, y, vertical_causal);
    let ctx = if neighbour_mask != 0 {
        zero_coding_context(band, neighbour_mask)
    } else {
        T1_CTXNO_ZC
    };
    let plane_bit = decoder.decode_with_ctx(ctx);
    flags.clear_visited(x, y);
    if plane_bit != 0 {
        decode_new_significant(
            decoder,
            bitplane,
            flags,
            magnitudes,
            width,
            x,
            y,
            vertical_causal,
        );
    }
}

fn decode_new_significant(
    decoder: &mut MqDecoder<'_>,
    bitplane: u8,
    flags: &mut FlagGrid,
    magnitudes: &mut [i32],
    width: usize,
    x: usize,
    y: usize,
    vertical_causal: bool,
) {
    let (sign_lut_index, _) = if vertical_causal && y % 4 == 3 {
        flags.cardinal_sign_context_without_south(x, y)
    } else {
        flags.cardinal_sign_context(x, y)
    };
    let sign_ctx = sign_context(sign_lut_index);
    let prediction = sign_prediction_bit(sign_lut_index);
    let sign_bit = decoder.decode_with_ctx(sign_ctx) ^ prediction;
    let idx = y * width + x;
    // Midpoint reconstruction: a newly significant coefficient at plane b is
    // known to lie in [2^b, 2^(b+1)); reconstruct at 1.5·2^b so truncated
    // streams (bit-planes below b never decoded) land mid-interval. Later MR
    // passes shrink the offset; a fully decoded block ends up bit-exact.
    let one = 1i32 << bitplane;
    magnitudes[idx] = one | (one >> 1);
    flags.mark_significant(x, y, sign_bit);
}

#[inline]
fn neighbour_mask(flags: &FlagGrid, x: usize, y: usize, vertical_causal: bool) -> u32 {
    let mask = flags.neighbour_mask(x, y);
    if vertical_causal && y % 4 == 3 {
        mask & !(T1_SIGMA_S | T1_SIGMA_SW | T1_SIGMA_SE)
    } else {
        mask
    }
}

fn copy_block_to_tile(
    tile_width: usize,
    tile: &mut [i32],
    block_x0: u32,
    block_y0: u32,
    block_width: usize,
    block_height: usize,
    coefficients: &[i32],
) -> Result<()> {
    let block_x0 = usize::try_from(block_x0).map_err(|_| invalid("block x0 exceeds usize"))?;
    let block_y0 = usize::try_from(block_y0).map_err(|_| invalid("block y0 exceeds usize"))?;
    for y in 0..block_height {
        let dst = (block_y0 + y)
            .checked_mul(tile_width)
            .and_then(|row| row.checked_add(block_x0))
            .ok_or_else(|| invalid("block copy offset overflow"))?;
        let src = y * block_width;
        let dst_end = dst + block_width;
        let src_end = src + block_width;
        let dst_slice = tile
            .get_mut(dst..dst_end)
            .ok_or_else(|| invalid("decoded block extends past tile"))?;
        dst_slice.copy_from_slice(&coefficients[src..src_end]);
    }
    Ok(())
}

/// Write one code block's decoded magnitudes into the tile's `f32` subband
/// plane, applying scalar dequantization in the same pass (fused Tier-1 output
/// + dequantization for the irreversible 9/7 path).
#[allow(clippy::too_many_arguments)]
fn dequantize_block_to_tile(
    tile_width: usize,
    tile: &mut [f32],
    block_x0: u32,
    block_y0: u32,
    block_width: usize,
    block_height: usize,
    coefficients: &[i32],
    step: f32,
) -> Result<()> {
    let block_x0 = usize::try_from(block_x0).map_err(|_| invalid("block x0 exceeds usize"))?;
    let block_y0 = usize::try_from(block_y0).map_err(|_| invalid("block y0 exceeds usize"))?;
    for y in 0..block_height {
        let dst = (block_y0 + y)
            .checked_mul(tile_width)
            .and_then(|row| row.checked_add(block_x0))
            .ok_or_else(|| invalid("block copy offset overflow"))?;
        let src = y * block_width;
        let dst_slice = tile
            .get_mut(dst..dst + block_width)
            .ok_or_else(|| invalid("decoded block extends past tile"))?;
        let src_slice = &coefficients[src..src + block_width];
        for (out, &q) in dst_slice.iter_mut().zip(src_slice) {
            *out = crate::simd::scalar::dequantize_i32_to_f32(q, step);
        }
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> Jp2LamError {
    Jp2LamError::DecodeFailed(message.into())
}

#[cfg(test)]
mod tests {
    use super::{decode_codeblock, decode_codeblock_segments};
    use crate::decode::t2::DecodedCodewordSegment;
    use crate::j2k::decode_markers::CodeBlockStyle;
    use crate::mq::{MqCoder, T1_CTXNO_MAG, T1_CTXNO_ZC};
    use crate::plan::BandOrientation;
    use crate::tier1::helpers::{sign_context, sign_prediction_bit};
    use std::borrow::Cow;

    #[test]
    fn cleanup_only_single_sample_decodes_positive_one() {
        let mut coder = MqCoder::new();
        coder.encode_with_ctx(T1_CTXNO_ZC, 1);
        let sign_ctx = sign_context(0);
        let prediction = sign_prediction_bit(0);
        coder.encode_with_ctx(sign_ctx, prediction);
        let bytes = coder.finish();

        let decoded =
            decode_codeblock(1, 1, BandOrientation::Ll, 1, 0, 1, false, &bytes).expect("decode");
        assert_eq!(decoded.coefficients, vec![1]);
    }

    #[test]
    fn cleanup_and_refinement_single_sample_decodes_negative_three() {
        let mut coder = MqCoder::new();
        coder.encode_with_ctx(T1_CTXNO_ZC, 1);
        let sign_ctx = sign_context(0);
        let prediction = sign_prediction_bit(0);
        coder.encode_with_ctx(sign_ctx, 1 ^ prediction);
        coder.encode_with_ctx(T1_CTXNO_MAG, 1);
        let bytes = coder.finish();

        let decoded =
            decode_codeblock(1, 1, BandOrientation::Ll, 2, 0, 3, false, &bytes).expect("decode");
        assert_eq!(decoded.coefficients, vec![-3]);
    }

    #[test]
    fn d4_d5_termall_reset_and_segmark_decode_across_pass_segments() {
        let sign_ctx = sign_context(0);
        let prediction = sign_prediction_bit(0);

        let mut cleanup = MqCoder::new();
        cleanup.encode_with_ctx(T1_CTXNO_ZC, 1);
        cleanup.encode_with_ctx(sign_ctx, prediction);
        cleanup.segmark_encode();
        let cleanup = cleanup.finish();

        // The significance pass has no symbols for an already significant
        // one-sample block. A zero-byte terminated segment is valid; Annex
        // D.4.1 supplies synthetic 0xFF bytes if the decoder needs symbols.
        let significance = Vec::new();

        let mut refinement = MqCoder::new();
        refinement.encode_with_ctx(T1_CTXNO_MAG, 1);
        let refinement = refinement.finish();

        let segments = [
            DecodedCodewordSegment {
                passes: 1,
                data: Cow::Borrowed(&cleanup),
            },
            DecodedCodewordSegment {
                passes: 1,
                data: Cow::Borrowed(&significance),
            },
            DecodedCodewordSegment {
                passes: 1,
                data: Cow::Borrowed(&refinement),
            },
        ];
        let decoded = decode_codeblock_segments(
            1,
            1,
            BandOrientation::Ll,
            2,
            0,
            3,
            CodeBlockStyle {
                reset_contexts: true,
                terminate_each_pass: true,
                predictable_termination: true,
                segmentation_symbols: true,
                ..CodeBlockStyle::default()
            },
            &segments,
        )
        .expect("terminated pass segments");
        assert_eq!(decoded.coefficients, vec![3]);
    }

    #[test]
    fn d41_empty_segment_uses_synthesized_end_bytes() {
        let decoded = decode_codeblock(1, 1, BandOrientation::Ll, 1, 0, 1, false, &[])
            .expect("MQ decoder synthesizes end bytes");
        assert_eq!(decoded.coefficients, vec![0]);
    }

    #[test]
    fn impossible_pass_count_fails_fast() {
        let err = decode_codeblock(1, 1, BandOrientation::Ll, 1, 0, 2, false, &[0])
            .expect_err("too many passes for one bit-plane should fail")
            .to_string();

        assert!(err.contains("exceeds maximum"), "{err}");
    }
}
