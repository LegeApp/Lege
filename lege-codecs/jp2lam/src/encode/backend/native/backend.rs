use super::{layout, rate, t1, t2};
use crate::dwt::norms::{band_gain, reversible_exponent};
use crate::dwt::pcrd::select_for_quality;
use crate::encode::backend::CodestreamBackend;
use crate::encode::block_store::{
    EncodedBlockStore, SharedEncodedBlockStore, StoredTier1Layout, store_tier1_layout,
    stored_layout_metadata_bytes,
};
use crate::encode::context::EncodeContext;
use crate::encode::profile_enter;
use crate::error::{Jp2LamError, Result};
use crate::j2k::{CodestreamParts, TilePart, TilePartHeader, build_main_header_segments};
use crate::model::{OutputFormat, SamplePrecision};
use crate::perceptual::{
    ContrastMaskMap, ContrastMaskParams, build_contrast_mask_map_from_luma_u8,
};
use crate::plan::{
    EncodeLane, EncodingPlan, OutputRateTarget, QuantizationStyle, SubbandQuant, WaveletTransform,
};
use crate::simd::PRIMITIVES;
use crate::tiling::{TileRect, tile_component_rect, tile_grid};

pub(crate) struct NativeBackend;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeComponentCoefficients {
    pub x0: usize,
    pub y0: usize,
    pub width: usize,
    pub height: usize,
    pub levels: u8,
    pub data: Vec<i32>,
}

impl CodestreamBackend for NativeBackend {
    fn supports(&self, context: &EncodeContext<'_>) -> bool {
        // Enable for testing - GrayLossless lane is under development
        self.supports_lane(context)
    }

    fn encode_codestream(&self, context: &EncodeContext<'_>) -> Result<Vec<u8>> {
        let _p = crate::encode::profile_enter("encode_codestream");
        if !self.supports_lane(context) {
            return Err(Jp2LamError::EncodeFailed(
                "native backend only supports GrayLossless".to_string(),
            ));
        }
        self.prepare_codestream_bytes(context)
    }
}

impl NativeBackend {
    pub(crate) fn emit_plan(&self, plan: &EncodingPlan) -> EncodingPlan {
        native_emit_plan(plan)
    }

    /// Prepare 9/7 irreversible coefficients for a component.
    ///
    /// Pipeline:
    /// 1. DC level-shift (unsigned -> signed-centered).
    /// 2. Optional irreversible MCT for RGB.
    /// 3. Forward 9/7 2-D transform in `f32`.
    /// 4. Per-band scalar-expounded quantization from the plan's QCD metadata.
    /// 4. Return `i32` sign-magnitude coefficients consumable by Tier-1.
    pub(crate) fn prepare_component_coefficients_97(
        &self,
        context: &EncodeContext<'_>,
        component_index: usize,
    ) -> Result<NativeComponentCoefficients> {
        self.prepare_component_coefficients_97_rect(
            context,
            component_index,
            0,
            0,
            context.plan.width,
            context.plan.height,
        )
    }

    /// Prepare 9/7 irreversible coefficients for a tile-component rectangle.
    ///
    /// The returned coefficient plane is tile-local: `(0, 0)` in the plane
    /// corresponds to `(x0, y0)` in the source component.
    pub(crate) fn prepare_component_coefficients_97_rect(
        &self,
        context: &EncodeContext<'_>,
        component_index: usize,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
    ) -> Result<NativeComponentCoefficients> {
        let _p = crate::encode::profile_enter("prepare_component_coefficients_97");
        let data =
            irreversible_input_component_rect(context, component_index, x0, y0, width, height)?;
        self.prepare_component_coefficients_97_from_input(
            context,
            component_index,
            x0,
            y0,
            width,
            height,
            data,
        )
    }

    /// Finish irreversible coefficient preparation from one already-built
    /// level-shifted (and, where applicable, ICT-transformed) tile plane.
    /// Keeping this boundary explicit lets the RGB path retain its three source
    /// planes once while each ICT output is transformed and Tier-1 encoded.
    fn prepare_component_coefficients_97_from_input(
        &self,
        context: &EncodeContext<'_>,
        component_index: usize,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
        mut data: Vec<f32>,
    ) -> Result<NativeComponentCoefficients> {
        crate::encode::counters::record_tile_samples(data.len() * std::mem::size_of::<f32>());
        crate::encode::counters::record_dwt_coefficients(data.len() * std::mem::size_of::<f32>());

        let width = width as usize;
        let height = height as usize;
        let levels = context.plan.decomposition_levels;

        crate::dwt::forward_97_2d_in_place_at(
            &mut data,
            width,
            height,
            levels,
            x0 as usize,
            y0 as usize,
        )?;

        let precision = context
            .plan
            .components
            .get(component_index)
            .map(|component| component.precision)
            .unwrap_or(8);
        let quantized = quantize_97_coefficients(
            &data,
            width,
            height,
            levels,
            precision,
            &context.plan.subband_quants,
            x0 as usize,
            y0 as usize,
        )?;

        Ok(NativeComponentCoefficients {
            x0: x0 as usize,
            y0: y0 as usize,
            width,
            height,
            levels,
            data: quantized,
        })
    }

    pub(super) fn supports_lane(&self, context: &EncodeContext<'_>) -> bool {
        matches!(
            context.plan.lane,
            EncodeLane::GrayLossless
                | EncodeLane::RgbLossless
                | EncodeLane::GrayLossy
                | EncodeLane::RgbLossy
        )
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_component_coefficients(
        &self,
        context: &EncodeContext<'_>,
        component_index: usize,
    ) -> Result<NativeComponentCoefficients> {
        if !self.supports_lane(context) {
            return Err(Jp2LamError::EncodeFailed(
                "native coefficient preparation is not implemented for this lane".to_string(),
            ));
        }

        if matches!(context.plan.transform, WaveletTransform::Irreversible97) {
            return self.prepare_component_coefficients_97(context, component_index);
        }

        self.prepare_component_coefficients_rect(
            context,
            component_index,
            0,
            0,
            context.plan.width,
            context.plan.height,
        )
    }

    /// Prepare reversible or irreversible coefficients for a tile-component
    /// rectangle. This is a Phase 4 staging API for tile-by-tile encoding; the
    /// active path still calls it with the full image dimensions.
    #[allow(dead_code, clippy::too_many_arguments)]
    pub(crate) fn prepare_component_coefficients_rect(
        &self,
        context: &EncodeContext<'_>,
        component_index: usize,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
    ) -> Result<NativeComponentCoefficients> {
        if !self.supports_lane(context) {
            return Err(Jp2LamError::EncodeFailed(
                "native coefficient preparation is not implemented for this lane".to_string(),
            ));
        }

        if matches!(context.plan.transform, WaveletTransform::Irreversible97) {
            return self.prepare_component_coefficients_97_rect(
                context,
                component_index,
                x0,
                y0,
                width,
                height,
            );
        }

        let mut data =
            reversible_input_component_rect(context, component_index, x0, y0, width, height)?;
        crate::encode::counters::record_tile_samples(data.len() * std::mem::size_of::<i32>());
        crate::encode::counters::record_dwt_coefficients(data.len() * std::mem::size_of::<i32>());

        crate::dwt::forward_53_2d_in_place_at(
            &mut data,
            width as usize,
            height as usize,
            context.plan.decomposition_levels,
            x0 as usize,
            y0 as usize,
        )?;

        Ok(NativeComponentCoefficients {
            x0: x0 as usize,
            y0: y0 as usize,
            width: width as usize,
            height: height as usize,
            levels: context.plan.decomposition_levels,
            data,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_component_layout(
        &self,
        context: &EncodeContext<'_>,
        component_index: usize,
    ) -> Result<layout::NativeComponentLayout> {
        let coefficients = self.prepare_component_coefficients(context, component_index)?;
        layout::build_component_layout(&coefficients, context.plan.code_block_size)
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_tier1_layout(
        &self,
        context: &EncodeContext<'_>,
        component_index: usize,
    ) -> Result<t1::NativeTier1Layout> {
        let _p = crate::encode::profile_enter("prepare_tier1_layout");
        let layout = self.prepare_component_layout(context, component_index)?;
        let precision = context
            .plan
            .components
            .get(component_index)
            .map(|c| c.precision)
            .unwrap_or(8);
        let guard_bits = context.plan.guard_bits;
        // For reversible MCT (RCT), Cb and Cr expand to ±255 (9-bit), so components 1 and 2
        // need one extra bitplane of precision.
        // For irreversible MCT (ICT), the channel ranges are different after ICT: Y has larger
        // magnitude range than Cb/Cr, so we use the component's actual precision.
        let effective_precision = if native_use_mct(&context.plan) && component_index > 0 {
            if matches!(context.plan.transform, WaveletTransform::Reversible53) {
                precision + 1
            } else {
                // ICT: after forward transform, components are Y (0), Cb (1), Cr (2).
                // Y has wider range than Cb/Cr, but we use base precision for all.
                precision
            }
        } else {
            precision
        };
        let analyzed = match context.plan.quantization_style {
            QuantizationStyle::NoQuantization => {
                t1::analyze_component_layout_with(&layout, effective_precision, guard_bits)
            }
            QuantizationStyle::ScalarExpounded => {
                t1::analyze_component_layout_with_max_bitplanes(&layout, |resolution, band| {
                    context
                        .plan
                        .subband_quants
                        .iter()
                        .find(|quant| quant.resolution == resolution && quant.band == band)
                        .map(|quant| guard_bits.saturating_sub(1).saturating_add(quant.exponent))
                        .unwrap_or_else(|| reversible_exponent(precision, band))
                })
            }
        };
        Ok(analyzed)
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_tier1_encoded_layout(
        &self,
        context: &EncodeContext<'_>,
    ) -> Result<t1::NativeEncodedTier1Layout> {
        self.prepare_tier1_encoded_component(context, 0)
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_tier1_encoded_component(
        &self,
        context: &EncodeContext<'_>,
        component_index: usize,
    ) -> Result<t1::NativeEncodedTier1Layout> {
        self.prepare_tier1_encoded_component_rect(
            context,
            0,
            component_index,
            0,
            0,
            context.plan.width,
            context.plan.height,
        )
    }

    /// Prepare and Tier-1 encode one tile-component rectangle.
    ///
    /// This wires Phase 4's tile-local sample/DWT preparation into the active
    /// Tier-1 code-block encoder. Current production code calls this with
    /// tile 0 and the full image extent; future multi-tile orchestration can
    /// call it once per tile/component and drop each tile coefficient plane
    /// immediately after Tier-1 encoding.
    #[allow(dead_code, clippy::too_many_arguments)]
    pub(crate) fn prepare_tier1_encoded_component_rect(
        &self,
        context: &EncodeContext<'_>,
        tile_index: u16,
        component_index: usize,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
    ) -> Result<t1::NativeEncodedTier1Layout> {
        let _p = crate::encode::profile_enter("prepare_tier1_encoded_component");
        let coefficients = self.prepare_component_coefficients_rect(
            context,
            component_index,
            x0,
            y0,
            width,
            height,
        )?;
        self.encode_tier1_coefficients(context, tile_index, component_index, &coefficients)
    }

    fn encode_tier1_coefficients(
        &self,
        context: &EncodeContext<'_>,
        tile_index: u16,
        component_index: usize,
        coefficients: &NativeComponentCoefficients,
    ) -> Result<t1::NativeEncodedTier1Layout> {
        let precision = context
            .plan
            .components
            .get(component_index)
            .map(|c| c.precision)
            .unwrap_or(8);
        let guard_bits = context.plan.guard_bits;
        let effective_precision = if native_use_mct(&context.plan) && component_index > 0 {
            if matches!(context.plan.transform, WaveletTransform::Reversible53) {
                precision + 1
            } else {
                precision
            }
        } else {
            precision
        };

        let encoded = match context.plan.quantization_style {
            QuantizationStyle::NoQuantization => {
                t1::encode_component_coefficients_for_tile_with_max_bitplanes(
                    &coefficients,
                    tile_index,
                    component_index as u16,
                    context.plan.code_block_size,
                    |_, band| t1::band_max_bitplanes(effective_precision, guard_bits, band),
                )
            }
            QuantizationStyle::ScalarExpounded => {
                t1::encode_component_coefficients_for_tile_with_max_bitplanes(
                    &coefficients,
                    tile_index,
                    component_index as u16,
                    context.plan.code_block_size,
                    |resolution, band| {
                        context
                            .plan
                            .subband_quants
                            .iter()
                            .find(|quant| quant.resolution == resolution && quant.band == band)
                            .map(|quant| {
                                guard_bits.saturating_sub(1).saturating_add(quant.exponent)
                            })
                            .unwrap_or_else(|| reversible_exponent(precision, band))
                    },
                )
            }
        };
        Ok(encoded)
    }

    /// Prepare all irreversible-MCT tile components from one retained RGB input
    /// triple. The live preparation set is R/G/B plus one ICT output plane,
    /// exactly the four-plane budget used by the resource planner; it avoids
    /// rebuilding the same R/G/B planes for every output component.
    fn prepare_tier1_encoded_mct_components_rect(
        &self,
        context: &EncodeContext<'_>,
        tile_index: u16,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
    ) -> Result<Vec<t1::NativeEncodedTier1Layout>> {
        let mut encoded = Vec::with_capacity(3);
        self.visit_tier1_encoded_mct_components_rect(
            context,
            tile_index,
            x0,
            y0,
            width,
            height,
            |_, layout| {
                encoded.push(layout);
                Ok(())
            },
        )?;
        Ok(encoded)
    }

    /// Visit each Tier-1 layout while retaining the RGB source triple. The
    /// store-backed path consumes a layout immediately through this method, so
    /// sharing MCT inputs does not make it retain all three compressed layouts.
    fn visit_tier1_encoded_mct_components_rect(
        &self,
        context: &EncodeContext<'_>,
        tile_index: u16,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
        mut visit: impl FnMut(usize, t1::NativeEncodedTier1Layout) -> Result<()>,
    ) -> Result<()> {
        let inputs = IctTileInputs::load(context, x0, y0, width, height)?;
        for component_index in 0..3 {
            let data = inputs.output_component(component_index)?;
            let coefficients = self.prepare_component_coefficients_97_from_input(
                context,
                component_index,
                x0,
                y0,
                width,
                height,
                data,
            )?;
            let layout = self.encode_tier1_coefficients(
                context,
                tile_index,
                component_index,
                &coefficients,
            )?;
            visit(component_index, layout)?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_tier1_encoded_layouts(
        &self,
        context: &EncodeContext<'_>,
    ) -> Result<Vec<t1::NativeEncodedTier1Layout>> {
        let _p = crate::encode::profile_enter("prepare_tier1_encoded_layouts");

        let component_count = context.plan.component_count as usize;

        if native_use_mct(&context.plan) {
            return self.prepare_tier1_encoded_mct_components_rect(
                context,
                0,
                0,
                0,
                context.plan.width,
                context.plan.height,
            );
        }

        // Phase 4 §8.3/§8.4: the default bounded-memory path encodes transformed
        // components sequentially so only one component coefficient plane is live
        // at a time. Each `prepare_tier1_encoded_component` call transforms one
        // component, encodes its code-blocks (still parallel *within* the
        // component via Tier-1), and drops that component's coefficient plane
        // before the next component begins. Component-level parallelism is opt-in
        // and only taken when the declared resource budget can hold every
        // component coefficient plane at once (§3.3 "memory limits control
        // parallelism").
        if allow_component_parallelism(context) {
            use rayon::prelude::*;
            (0..component_count)
                .into_par_iter()
                .map(|component_index| {
                    let _cp = crate::encode::profile_enter("per_component_encode");
                    self.prepare_tier1_encoded_component(context, component_index)
                })
                .collect()
        } else {
            let mut encoded_layouts = Vec::with_capacity(component_count);
            for component_index in 0..component_count {
                let _cp = crate::encode::profile_enter("per_component_encode");
                encoded_layouts
                    .push(self.prepare_tier1_encoded_component(context, component_index)?);
            }
            Ok(encoded_layouts)
        }
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_tier1_encoded_layouts_and_selections(
        &self,
        context: &EncodeContext<'_>,
    ) -> Result<(
        Vec<t1::NativeEncodedTier1Layout>,
        Vec<t1::NativeTier1SelectionLayout>,
    )> {
        let encoded_layouts = self.prepare_tier1_encoded_layouts(context)?;
        let selections = if native_pcrd_enabled() {
            select_layout_passes(&encoded_layouts, context)?
        } else {
            all_pass_selections(&encoded_layouts)
        };
        Ok((encoded_layouts, selections))
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_packet_sequence(
        &self,
        context: &EncodeContext<'_>,
    ) -> Result<t2::NativePacketSequence> {
        let (encoded, selections) = self.prepare_tier1_encoded_layouts_and_selections(context)?;
        t2::build_packet_sequence_for_components_with_selections(&encoded, Some(&selections))
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_tile_part_payload(
        &self,
        context: &EncodeContext<'_>,
    ) -> Result<crate::t2::TilePartPayload> {
        let _p = crate::encode::profile_enter("prepare_tile_part_payload");
        let (encoded, selections) = self.prepare_tier1_encoded_layouts_and_selections(context)?;
        t2::build_tile_part_payload_for_tile_components_owned(0, encoded, Some(&selections))
    }

    /// Prepare one tile-part payload from a tile rectangle.
    ///
    /// This is Phase 4 staging for true tile-by-tile encoding: each component is
    /// loaded, transformed, Tier-1 encoded, and released for the requested tile
    /// before Tier-2 packetization. It intentionally selects all passes for now;
    /// Phase 5 will add an encoded block store plus global PCRD across all
    /// tiles/components.
    #[allow(dead_code)]
    pub(crate) fn prepare_tile_part_payload_for_tile_rect(
        &self,
        context: &EncodeContext<'_>,
        tile: TileRect,
    ) -> Result<crate::t2::TilePartPayload> {
        let _p = crate::encode::profile_enter("prepare_tile_part_payload_for_tile_rect");
        if !self.supports_lane(context) {
            return Err(Jp2LamError::EncodeFailed(
                "native tile-part assembly is not implemented for this lane".to_string(),
            ));
        }

        let encoded = if native_use_mct(&context.plan) {
            self.prepare_tier1_encoded_mct_components_rect(
                context,
                tile.tile_index,
                tile.x0,
                tile.y0,
                tile.width(),
                tile.height(),
            )?
        } else {
            let mut encoded = Vec::with_capacity(context.plan.component_count as usize);
            for (component_index, component) in context.plan.components.iter().enumerate() {
                let tc = tile_component_rect(&tile, component_index as u16, component);
                if tc.width() == 0 || tc.height() == 0 {
                    return Err(Jp2LamError::EncodeFailed(format!(
                        "tile {} component {component_index} has empty tile-component extent",
                        tile.tile_index
                    )));
                }
                encoded.push(self.prepare_tier1_encoded_component_rect(
                    context,
                    tile.tile_index,
                    component_index,
                    tc.x0,
                    tc.y0,
                    tc.width(),
                    tc.height(),
                )?);
            }
            encoded
        };

        let selections = all_pass_selections(&encoded);
        t2::build_tile_part_payload_for_tile_components_owned(
            tile.tile_index,
            encoded,
            Some(&selections),
        )
    }

    /// Encode one tile into the bounded Phase 5 block store.
    ///
    /// Each component layout is consumed by the store before the next
    /// component is prepared, so neither coefficient planes nor Tier-1 payload
    /// vectors accumulate across components or tiles. Only compact pass and
    /// geometry metadata remains resident after this boundary.
    pub(crate) fn prepare_stored_tier1_for_tile_rect(
        &self,
        context: &EncodeContext<'_>,
        tile: TileRect,
        store: &mut EncodedBlockStore,
    ) -> Result<Vec<StoredTier1Layout>> {
        if !self.supports_lane(context) {
            return Err(Jp2LamError::EncodeFailed(
                "native stored tile encoding is not implemented for this lane".to_string(),
            ));
        }

        if native_use_mct(&context.plan) {
            let mut stored = Vec::with_capacity(3);
            self.visit_tier1_encoded_mct_components_rect(
                context,
                tile.tile_index,
                tile.x0,
                tile.y0,
                tile.width(),
                tile.height(),
                |component_index, layout| {
                    stored.push(store_tier1_layout(store, component_index as u16, layout)?);
                    Ok(())
                },
            )?;
            return Ok(stored);
        }

        let mut stored = Vec::with_capacity(context.plan.component_count as usize);
        for (component_index, component) in context.plan.components.iter().enumerate() {
            let tc = tile_component_rect(&tile, component_index as u16, component);
            if tc.width() == 0 || tc.height() == 0 {
                return Err(Jp2LamError::EncodeFailed(format!(
                    "tile {} component {component_index} has empty tile-component extent",
                    tile.tile_index
                )));
            }
            let encoded = self.prepare_tier1_encoded_component_rect(
                context,
                tile.tile_index,
                component_index,
                tc.x0,
                tc.y0,
                tc.width(),
                tc.height(),
            )?;
            stored.push(store_tier1_layout(store, component_index as u16, encoded)?);
        }
        Ok(stored)
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_codestream_parts(
        &self,
        context: &EncodeContext<'_>,
    ) -> Result<CodestreamParts> {
        let _p = crate::encode::profile_enter("prepare_codestream_parts");
        if !self.supports_lane(context) {
            return Err(Jp2LamError::EncodeFailed(
                "native codestream assembly is not implemented for this lane".to_string(),
            ));
        }
        let emit_plan = native_emit_plan(&context.plan);
        let main_header_segments = build_main_header_segments(&emit_plan)?;
        let grid = tile_grid(&context.plan);
        // ISO/IEC 15444-1 Annex A.4.2: emit one SOT/SOD tile-part for each
        // raster-order tile. Each tile has exactly one tile-part, so TPsot=0
        // and TNsot=1. Multi-tile payloads are retained once in the bounded
        // Phase 5 store and referenced by packet plans rather than copied into
        // packet-owned body vectors.
        let tile_rects = grid.tile_rects();
        let tile_parts = if tile_rects.len() > 1 || !context.plan.is_lossless() {
            let mut store = EncodedBlockStore::from_resource_limits(&context.plan.resource_limits);
            let mut stored_tiles = Vec::with_capacity(tile_rects.len());
            for tile in &tile_rects {
                stored_tiles.push((
                    *tile,
                    self.prepare_stored_tier1_for_tile_rect(context, *tile, &mut store)?,
                ));
                let metadata_bytes = stored_tiles
                    .iter()
                    .flat_map(|(_, layouts)| layouts)
                    .map(stored_layout_metadata_bytes)
                    .sum();
                crate::encode::counters::record_rd_metadata(metadata_bytes);
            }
            let shared_store = store.into_shared();
            if let Some(OutputRateTarget::Bytes(target_bytes)) = context.plan.output_rate_target {
                select_exact_rate_tile_parts(
                    &stored_tiles,
                    context,
                    &emit_plan,
                    &main_header_segments,
                    shared_store,
                    target_bytes,
                )?
            } else {
                let stored_selections = select_stored_tile_passes(&stored_tiles, context, None)?;
                build_stored_tile_parts(&stored_tiles, stored_selections, shared_store)?
            }
        } else {
            let tile = tile_rects[0];
            vec![TilePart {
                header: TilePartHeader {
                    tile_index: tile.tile_index,
                    part_index: 0,
                    total_parts: 1,
                },
                header_segments: Vec::new(),
                payload: self.prepare_tile_part_payload_for_tile_rect(context, tile)?,
            }]
        };
        Ok(CodestreamParts {
            main_header_segments,
            tile_parts,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_codestream_bytes(&self, context: &EncodeContext<'_>) -> Result<Vec<u8>> {
        let _p = crate::encode::profile_enter("prepare_codestream_bytes");
        let parts = self.prepare_codestream_parts(context)?;
        parts.encode(&native_emit_plan(&context.plan))
    }

    /// Measure the encoded result using the crate's decoder.
    ///
    /// Decoding the exact returned JP2/J2K bytes makes this valid for every tile
    /// layout and rate-selection path, including tile-local transform boundaries.
    /// This remains entirely in-process; it does not depend on an external tool.
    pub(crate) fn compute_quality_metrics(
        &self,
        context: &EncodeContext<'_>,
        encoded: &[u8],
    ) -> Result<crate::encode::EncodeMetrics> {
        if context.plan.quality >= 100 {
            return Ok(crate::encode::EncodeMetrics {
                psnr_db: f64::INFINITY,
                ssim: 1.0,
            });
        }
        if !matches!(context.plan.transform, WaveletTransform::Irreversible97) {
            return Err(Jp2LamError::EncodeFailed(
                "quality metrics only supported for irreversible 9/7 encodes".to_string(),
            ));
        }
        if matches!(
            context.plan.color_encoding,
            crate::model::ColorEncoding::IccProfile { .. }
        ) {
            return Err(Jp2LamError::EncodeFailed(
                "quality metrics require an enumerated Gray or sRGB color model".into(),
            ));
        }
        let decoded = crate::decode::decode_jp2(encoded)?;
        metrics_from_decoded_image(context, &decoded)
    }
}

fn metrics_from_decoded_image(
    context: &EncodeContext<'_>,
    decoded: &crate::model::Image,
) -> Result<crate::encode::EncodeMetrics> {
    if decoded.width != context.image.width || decoded.height != context.image.height {
        return Err(Jp2LamError::EncodeFailed(format!(
            "quality-metric decode dimensions {}x{} do not match source {}x{}",
            decoded.width, decoded.height, context.image.width, context.image.height
        )));
    }

    let width = usize::try_from(context.image.width)
        .map_err(|_| Jp2LamError::EncodeFailed("metric width exceeds usize".into()))?;
    let height = usize::try_from(context.image.height)
        .map_err(|_| Jp2LamError::EncodeFailed("metric height exceeds usize".into()))?;
    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| Jp2LamError::EncodeFailed("metric image area overflows usize".into()))?;
    let precision = context
        .plan
        .components
        .first()
        .map(|component| component.precision)
        .unwrap_or(8);
    let max_val = ((1u32 << precision) - 1) as f64;
    let mut original_luma = vec![0.0f32; pixel_count];
    let mut decoded_luma = vec![0.0f32; pixel_count];
    let mut total_sse = 0.0f64;
    let sample_count;

    match context.image.colorspace {
        crate::model::ColorSpace::Gray => {
            let original = context.load_component_i32(0)?;
            let reconstructed = decoded.components.first().ok_or_else(|| {
                Jp2LamError::EncodeFailed("quality-metric decode is missing grayscale data".into())
            })?;
            if original.len() != pixel_count || reconstructed.data.len() != pixel_count {
                return Err(Jp2LamError::EncodeFailed(
                    "quality-metric grayscale sample count does not match image area".into(),
                ));
            }
            sample_count = pixel_count;
            for index in 0..pixel_count {
                let actual = reconstructed.data[index] as f32;
                let expected = original[index] as f32;
                let difference = actual - expected;
                total_sse += f64::from(difference * difference);
                original_luma[index] = expected;
                decoded_luma[index] = actual;
            }
        }
        crate::model::ColorSpace::Srgb if decoded.components.len() == 3 => {
            let original_r = context.load_component_i32(0)?;
            let original_g = context.load_component_i32(1)?;
            let original_b = context.load_component_i32(2)?;
            let reconstructed_r = &decoded.components[0].data;
            let reconstructed_g = &decoded.components[1].data;
            let reconstructed_b = &decoded.components[2].data;
            if [
                original_r.len(),
                original_g.len(),
                original_b.len(),
                reconstructed_r.len(),
                reconstructed_g.len(),
                reconstructed_b.len(),
            ]
            .iter()
            .any(|&len| len != pixel_count)
            {
                return Err(Jp2LamError::EncodeFailed(
                    "quality-metric RGB sample count does not match image area".into(),
                ));
            }
            sample_count = pixel_count * 3;
            for index in 0..pixel_count {
                let actual_r = reconstructed_r[index] as f32;
                let actual_g = reconstructed_g[index] as f32;
                let actual_b = reconstructed_b[index] as f32;
                let expected_r = original_r[index] as f32;
                let expected_g = original_g[index] as f32;
                let expected_b = original_b[index] as f32;
                total_sse += f64::from((actual_r - expected_r).powi(2))
                    + f64::from((actual_g - expected_g).powi(2))
                    + f64::from((actual_b - expected_b).powi(2));
                original_luma[index] = 0.299 * expected_r + 0.587 * expected_g + 0.114 * expected_b;
                decoded_luma[index] = 0.299 * actual_r + 0.587 * actual_g + 0.114 * actual_b;
            }
        }
        _ => {
            return Err(Jp2LamError::EncodeFailed(
                "quality metrics not implemented for this colorspace configuration".into(),
            ));
        }
    }

    let mse = total_sse / sample_count as f64;
    let psnr_db = if mse < 1e-10 {
        100.0
    } else {
        20.0 * (max_val / mse.sqrt()).log10()
    };
    let ssim = mssim_8x8(&original_luma, &decoded_luma, width, height, max_val);
    Ok(crate::encode::EncodeMetrics { psnr_db, ssim })
}

/// Level-shifted (and optionally ICT-transformed) irreversible input samples for
/// a tile-component rectangle.
///
/// Phase 4 §8.3: the active single-tile path passes the full-image extent, but
/// this rectangle form lets a future tile encoder prepare one tile-component at
/// a time. Each JPEG 2000 tile transforms independently, so preparing a
/// rectangle here is self-contained.
fn irreversible_input_component_rect(
    context: &EncodeContext<'_>,
    component_index: usize,
    x0: u32,
    y0: u32,
    width: u32,
    height: u32,
) -> Result<Vec<f32>> {
    if !native_use_mct(&context.plan) {
        let source = context.load_component_rect_i32(component_index, x0, y0, width, height)?;
        let mut out = vec![0.0f32; source.len()];
        let precision = SamplePrecision::new(context.plan.components[component_index].precision)?;
        (PRIMITIVES.color.level_shift_f32)(&source, precision.unsigned_level_shift(), &mut out);
        return Ok(out);
    }

    IctTileInputs::load(context, x0, y0, width, height)?.output_component(component_index)
}

/// Dense RGB source planes retained while all three irreversible-MCT outputs
/// for a tile are derived. Keeping this state tile-scoped removes repeated
/// deinterleave/load work without raising the four-plane peak already required
/// for one ICT output.
struct IctTileInputs {
    r: Vec<i32>,
    g: Vec<i32>,
    b: Vec<i32>,
    level_shift: i32,
}

impl IctTileInputs {
    fn load(
        context: &EncodeContext<'_>,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        if context.plan.component_count != 3 {
            return Err(Jp2LamError::EncodeFailed(
                "irreversible MCT requires exactly 3 components".to_string(),
            ));
        }
        let r = context.load_component_rect_i32(0, x0, y0, width, height)?;
        let g = context.load_component_rect_i32(1, x0, y0, width, height)?;
        let b = context.load_component_rect_i32(2, x0, y0, width, height)?;
        if r.len() != g.len() || r.len() != b.len() {
            return Err(Jp2LamError::EncodeFailed(
                "component sample lengths differ for irreversible MCT".to_string(),
            ));
        }
        let precision = SamplePrecision::new(context.plan.components[0].precision)?;
        Ok(Self {
            r,
            g,
            b,
            level_shift: precision.unsigned_level_shift(),
        })
    }

    fn output_component(&self, component_index: usize) -> Result<Vec<f32>> {
        if component_index > 2 {
            return Err(Jp2LamError::EncodeFailed(format!(
                "irreversible MCT only supports component index 0..2, got {component_index}"
            )));
        }
        let mut out = vec![0.0f32; self.r.len()];
        (PRIMITIVES.color.forward_ict_component)(
            &self.r,
            &self.g,
            &self.b,
            component_index,
            self.level_shift,
            &mut out,
        );
        Ok(out)
    }
}

/// Level-shifted (and optionally RCT-transformed) reversible input samples for a
/// tile-component rectangle. See `irreversible_input_component_rect`.
fn reversible_input_component_rect(
    context: &EncodeContext<'_>,
    component_index: usize,
    x0: u32,
    y0: u32,
    width: u32,
    height: u32,
) -> Result<Vec<i32>> {
    if !native_use_mct(&context.plan) {
        let source = context.load_component_rect_i32(component_index, x0, y0, width, height)?;
        let mut out = vec![0i32; source.len()];
        let precision = SamplePrecision::new(context.plan.components[component_index].precision)?;
        (PRIMITIVES.color.level_shift_i32)(&source, precision.unsigned_level_shift(), &mut out);
        return Ok(out);
    }
    if context.plan.component_count != 3 {
        return Err(Jp2LamError::EncodeFailed(
            "reversible MCT requires exactly 3 components".to_string(),
        ));
    }
    let r = context.load_component_rect_i32(0, x0, y0, width, height)?;
    let g = context.load_component_rect_i32(1, x0, y0, width, height)?;
    let b = context.load_component_rect_i32(2, x0, y0, width, height)?;
    if r.len() != g.len() || r.len() != b.len() {
        return Err(Jp2LamError::EncodeFailed(
            "component sample lengths differ for reversible MCT".to_string(),
        ));
    }
    if component_index > 2 {
        return Err(Jp2LamError::EncodeFailed(format!(
            "reversible MCT only supports component index 0..2, got {component_index}"
        )));
    }
    let mut out = vec![0i32; r.len()];
    let precision = SamplePrecision::new(context.plan.components[component_index].precision)?;
    (PRIMITIVES.color.forward_rct_component)(
        &r,
        &g,
        &b,
        component_index,
        precision.unsigned_level_shift(),
        &mut out,
    );
    Ok(out)
}

fn quantize_97_coefficients(
    data: &[f32],
    width: usize,
    height: usize,
    levels: u8,
    precision: u32,
    subband_quants: &[SubbandQuant],
    x0: usize,
    y0: usize,
) -> Result<Vec<i32>> {
    let _p = profile_enter("quantize_97_coefficients");
    if data.len() != width.saturating_mul(height) {
        return Err(Jp2LamError::EncodeFailed(
            "irreversible quantization received mismatched coefficient geometry".to_string(),
        ));
    }
    // subband_quants already carry quality-scaled step sizes from the plan.
    let mut out = vec![0i32; data.len()];
    if width == 0 || height == 0 {
        return Ok(out);
    }

    let resolutions = crate::tiling::phase_resolution_sizes(x0, y0, width, height, levels);

    let ll = resolutions[0];
    let ll_step = subband_quant_step(
        precision,
        0,
        crate::plan::BandOrientation::Ll,
        subband_quants,
    )?;
    quantize_subband_rect(data, &mut out, width, 0, 0, ll.0, ll.1, ll_step);

    for (index, w) in resolutions.windows(2).enumerate() {
        let (low, full) = (w[0], w[1]);
        let resolution = (index + 1) as u8;
        let hl_step = subband_quant_step(
            precision,
            resolution,
            crate::plan::BandOrientation::Hl,
            subband_quants,
        )?;
        let lh_step = subband_quant_step(
            precision,
            resolution,
            crate::plan::BandOrientation::Lh,
            subband_quants,
        )?;
        let hh_step = subband_quant_step(
            precision,
            resolution,
            crate::plan::BandOrientation::Hh,
            subband_quants,
        )?;

        quantize_subband_rect(data, &mut out, width, low.0, 0, full.0, low.1, hl_step);
        quantize_subband_rect(data, &mut out, width, 0, low.1, low.0, full.1, lh_step);
        quantize_subband_rect(data, &mut out, width, low.0, low.1, full.0, full.1, hh_step);
    }

    Ok(out)
}

fn quantize_subband_rect(
    data: &[f32],
    out: &mut [i32],
    stride: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    step: f32,
) {
    (PRIMITIVES.quant.quantize_f32_rect)(data, out, stride, x0, y0, x1, y1, step);
}

fn subband_quant_step(
    precision: u32,
    resolution: u8,
    band: crate::plan::BandOrientation,
    subband_quants: &[SubbandQuant],
) -> Result<f32> {
    let quant = subband_quants
        .iter()
        .find(|quant| quant.resolution == resolution && quant.band == band)
        .ok_or_else(|| {
            Jp2LamError::EncodeFailed(format!(
                "missing quantization parameters for resolution={resolution} band={band:?}"
            ))
        })?;
    let numbps = (precision + u32::from(band_gain(band))) as i32;
    let exponent = i32::from(quant.exponent);
    let base = 1.0 + f32::from(quant.mantissa) / 2048.0;
    Ok((base * 2f32.powi(numbps - exponent)).max(1e-6))
}

fn native_pcrd_enabled() -> bool {
    true
}

fn all_pass_selections(
    layouts: &[t1::NativeEncodedTier1Layout],
) -> Vec<t1::NativeTier1SelectionLayout> {
    layouts
        .iter()
        .map(|layout| t1::NativeTier1SelectionLayout {
            bands: layout
                .bands
                .iter()
                .map(|band| t1::NativeTier1SelectionBand {
                    resolution: band.resolution,
                    band: band.band,
                    selected_passes: band
                        .blocks
                        .iter()
                        .map(|block| sanitized_selected_passes(block, block.passes.len()))
                        .collect(),
                })
                .collect(),
        })
        .collect()
}

fn select_layout_passes(
    layouts: &[t1::NativeEncodedTier1Layout],
    context: &EncodeContext<'_>,
) -> Result<Vec<t1::NativeTier1SelectionLayout>> {
    let quality = context.plan.quality;

    if quality >= 100 {
        return Ok(all_pass_selections(layouts));
    }

    let pixel_count = u64::from(context.image.width) * u64::from(context.image.height);
    let contrast_mask = build_luma_contrast_mask(context);
    let document_trim = matches!(context.plan.rate_mode, crate::plan::RateMode::DocumentTrim);

    let mut all_selections = Vec::with_capacity(layouts.len());
    for (component_index, layout) in layouts.iter().enumerate() {
        let taubman_weights: Option<Vec<f64>> = None;
        let component_weight = pcrd_component_weight(context, component_index);

        let precision = context
            .plan
            .components
            .first()
            .map(|c| c.precision)
            .unwrap_or(8);
        let curves = rate::curves_from_tier1_layout(
            layout,
            context.plan.num_resolutions,
            &context.plan.subband_quants,
            precision,
            quality,
            component_weight,
            contrast_mask.as_ref(),
            taubman_weights.as_deref(),
        )
        .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;

        let selection = if document_trim {
            crate::dwt::pcrd::select_for_document_trim(&curves)
        } else {
            select_for_quality(&curves, quality, pixel_count)
        }
        .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;

        let mut flat_selected_passes = vec![0u16; curves.len()];
        for block in selection.selections {
            if let Some(slot) = flat_selected_passes.get_mut(block.block_id) {
                *slot = block.passes;
            } else {
                return Err(Jp2LamError::EncodeFailed(
                    "PCRD block selection index out of range".to_string(),
                ));
            }
        }
        all_selections.push(selection_layout_from_flat(layout, &flat_selected_passes)?);
    }
    Ok(all_selections)
}

fn select_stored_tile_passes(
    stored_tiles: &[(TileRect, Vec<StoredTier1Layout>)],
    context: &EncodeContext<'_>,
    target_body_bytes: Option<u32>,
) -> Result<Vec<Vec<t1::NativeTier1SelectionLayout>>> {
    if context.plan.quality >= 100 {
        return stored_tiles
            .iter()
            .map(|(_, layouts)| {
                layouts
                    .iter()
                    .map(stored_all_pass_selection)
                    .collect::<Result<Vec<_>>>()
            })
            .collect();
    }

    let mut curves = Vec::new();
    for (_, layouts) in stored_tiles {
        for layout in layouts {
            let component_index = usize::from(layout.component);
            let precision = context
                .plan
                .components
                .get(component_index)
                .map(|component| component.precision)
                .unwrap_or(8);
            let mut component_curves = rate::curves_from_stored_layout(
                layout,
                curves.len(),
                context.plan.num_resolutions,
                &context.plan.subband_quants,
                precision,
                pcrd_component_weight(context, component_index),
            )
            .map_err(|error| Jp2LamError::EncodeFailed(error.to_string()))?;
            curves.append(&mut component_curves);
        }
    }

    let pixel_count = u64::from(context.image.width) * u64::from(context.image.height);
    let selection = if let Some(target_body_bytes) = target_body_bytes {
        crate::dwt::pcrd::select_for_target_bytes(&curves, target_body_bytes)
    } else if matches!(context.plan.rate_mode, crate::plan::RateMode::DocumentTrim) {
        crate::dwt::pcrd::select_for_document_trim(&curves)
    } else {
        select_for_quality(&curves, context.plan.quality, pixel_count)
    }
    .map_err(|error| Jp2LamError::EncodeFailed(error.to_string()))?;

    let mut flat_selected_passes = vec![0u16; curves.len()];
    for block in selection.selections {
        let slot = flat_selected_passes
            .get_mut(block.block_id)
            .ok_or_else(|| {
                Jp2LamError::EncodeFailed("global PCRD block selection index out of range".into())
            })?;
        *slot = block.passes;
    }

    let mut next_block = 0usize;
    stored_tiles
        .iter()
        .map(|(_, layouts)| {
            layouts
                .iter()
                .map(|layout| {
                    stored_selection_layout_from_flat(
                        layout,
                        &flat_selected_passes,
                        &mut next_block,
                    )
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect()
}

fn build_stored_tile_parts(
    stored_tiles: &[(TileRect, Vec<StoredTier1Layout>)],
    stored_selections: Vec<Vec<t1::NativeTier1SelectionLayout>>,
    shared_store: SharedEncodedBlockStore,
) -> Result<Vec<TilePart>> {
    stored_tiles
        .iter()
        .zip(stored_selections)
        .map(|((tile, layouts), selections)| {
            Ok(TilePart {
                header: TilePartHeader {
                    tile_index: tile.tile_index,
                    part_index: 0,
                    total_parts: 1,
                },
                header_segments: Vec::new(),
                payload: t2::build_stored_tile_part_payload(
                    tile.tile_index,
                    shared_store.clone(),
                    layouts,
                    Some(&selections),
                )?,
            })
        })
        .collect()
}

fn select_exact_rate_tile_parts(
    stored_tiles: &[(TileRect, Vec<StoredTier1Layout>)],
    context: &EncodeContext<'_>,
    emit_plan: &EncodingPlan,
    main_header_segments: &[Vec<u8>],
    shared_store: SharedEncodedBlockStore,
    target_output_bytes: u64,
) -> Result<Vec<TilePart>> {
    let max_body_bytes = stored_tiles
        .iter()
        .flat_map(|(_, layouts)| layouts)
        .flat_map(|layout| &layout.bands)
        .flat_map(|band| &band.blocks)
        .filter_map(|block| block.passes.last())
        .fold(0u64, |total, pass| {
            total.saturating_add(pass.cumulative_length as u64)
        })
        .min(u64::from(u32::MAX)) as u32;

    let mut low = 0u32;
    let mut high = max_body_bytes;
    let mut best: Option<(u64, Vec<TilePart>)> = None;

    while low <= high {
        let body_target = low + (high - low) / 2;
        let selections = select_stored_tile_passes(stored_tiles, context, Some(body_target))?;
        let tile_parts = build_stored_tile_parts(stored_tiles, selections, shared_store.clone())?;
        let candidate = CodestreamParts {
            main_header_segments: main_header_segments.to_vec(),
            tile_parts: tile_parts.clone(),
        };
        let output_bytes = complete_output_len(context, emit_plan, &candidate)?;

        if output_bytes <= target_output_bytes {
            if best
                .as_ref()
                .is_none_or(|(best_bytes, _)| output_bytes > *best_bytes)
            {
                best = Some((output_bytes, tile_parts));
            }
            if body_target == u32::MAX {
                break;
            }
            low = body_target + 1;
        } else if body_target == 0 {
            break;
        } else {
            high = body_target - 1;
        }
    }

    best.map(|(_, tile_parts)| tile_parts).ok_or_else(|| {
        Jp2LamError::InvalidInput(format!(
            "target output size {target_output_bytes} bytes is smaller than mandatory codestream/container overhead"
        ))
    })
}

fn complete_output_len(
    context: &EncodeContext<'_>,
    emit_plan: &EncodingPlan,
    parts: &CodestreamParts,
) -> Result<u64> {
    let codestream_len = parts.byte_len(emit_plan)?;
    match context.plan.output_format {
        OutputFormat::J2k => Ok(codestream_len as u64),
        OutputFormat::Jp2 => {
            let mut header = Vec::new();
            crate::jp2::write_jp2_header_for_view(
                &context.image,
                &context.plan.color_encoding,
                codestream_len,
                &mut header,
            )?;
            Ok(header.len() as u64 + codestream_len as u64)
        }
    }
}

fn stored_all_pass_selection(layout: &StoredTier1Layout) -> Result<t1::NativeTier1SelectionLayout> {
    let mut next_block = 0usize;
    let selected = layout
        .bands
        .iter()
        .flat_map(|band| &band.blocks)
        .map(|block| u16::try_from(block.passes.len()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Jp2LamError::EncodeFailed("Tier-1 pass count exceeds u16".into()))?;
    stored_selection_layout_from_flat(layout, &selected, &mut next_block)
}

fn stored_selection_layout_from_flat(
    layout: &StoredTier1Layout,
    flat_selected_passes: &[u16],
    next_block: &mut usize,
) -> Result<t1::NativeTier1SelectionLayout> {
    let mut bands = Vec::with_capacity(layout.bands.len());
    for band in &layout.bands {
        let mut selected_passes = Vec::with_capacity(band.blocks.len());
        for block in &band.blocks {
            let desired = flat_selected_passes
                .get(*next_block)
                .copied()
                .ok_or_else(|| {
                    Jp2LamError::EncodeFailed("global PCRD block count mismatch".into())
                })?;
            *next_block += 1;
            let mut retain = usize::from(desired).min(block.passes.len());
            while retain > 0 && block.passes[retain - 1].cumulative_length == 0 {
                retain -= 1;
            }
            selected_passes.push(retain as u16);
        }
        bands.push(t1::NativeTier1SelectionBand {
            resolution: band.resolution,
            band: band.band,
            selected_passes,
        });
    }
    Ok(t1::NativeTier1SelectionLayout { bands })
}

fn selection_layout_from_flat(
    layout: &t1::NativeEncodedTier1Layout,
    flat_selected_passes: &[u16],
) -> Result<t1::NativeTier1SelectionLayout> {
    let expected_block_count = layout
        .bands
        .iter()
        .map(|band| band.blocks.len())
        .sum::<usize>();
    if flat_selected_passes.len() != expected_block_count {
        return Err(Jp2LamError::EncodeFailed(format!(
            "PCRD block count mismatch: expected {expected_block_count}, got {}",
            flat_selected_passes.len()
        )));
    }

    let mut block_id = 0usize;
    let mut bands = Vec::with_capacity(layout.bands.len());
    for band in &layout.bands {
        let mut selected_passes = Vec::with_capacity(band.blocks.len());
        for block in &band.blocks {
            selected_passes.push(sanitized_selected_passes(
                block,
                usize::from(flat_selected_passes[block_id]),
            ));
            block_id += 1;
        }
        bands.push(t1::NativeTier1SelectionBand {
            resolution: band.resolution,
            band: band.band,
            selected_passes,
        });
    }
    Ok(t1::NativeTier1SelectionLayout { bands })
}

fn sanitized_selected_passes(block: &t1::NativeEncodedTier1CodeBlock, desired: usize) -> u16 {
    let mut retain = desired.min(block.passes.len());
    while retain > 0 && block.passes[retain - 1].cumulative_length == 0 {
        retain -= 1;
    }
    retain as u16
}

fn pcrd_component_weight(context: &EncodeContext<'_>, component_index: usize) -> f64 {
    use crate::model::ColorSpace;

    if !context.plan.use_mct
        || !matches!(context.image.colorspace.encoding_domain(), ColorSpace::Srgb)
        || !matches!(
            context.plan.color_encoding,
            crate::model::ColorEncoding::Srgb
        )
    {
        return 1.0;
    }

    match component_index {
        0 => 1.0,
        // Annex G inverse ICT maps component errors into RGB as:
        //   eR = eY + 1.402 eCr
        //   eG = eY - 0.34413 eCb - 0.71414 eCr
        //   eB = eY + 1.772 eCb
        // Under the additive/uncorrelated-error approximation in Annex J.14.3,
        // RGB squared-error weights are the squared column norms of that matrix.
        // Normalize by Y's norm²=3 so the quality-lambda scale remains stable.
        1 => (0.344_13f64.powi(2) + 1.772f64.powi(2)) / 3.0,
        2 => (1.402f64.powi(2) + 0.714_14f64.powi(2)) / 3.0,
        _ => 1.0,
    }
}

/// Build contrast mask map from the luma component.
///
/// For grayscale images, uses the first component directly.
/// For RGB images, computes luma as Y = 0.299*R + 0.587*G + 0.114*B.
fn build_luma_contrast_mask(context: &EncodeContext<'_>) -> Option<ContrastMaskMap> {
    use crate::model::ColorSpace;

    let width = context.image.width as usize;
    let height = context.image.height as usize;
    if matches!(
        context.plan.color_encoding,
        crate::model::ColorEncoding::IccProfile {
            component_model: crate::model::IccComponentModel::Rgb,
            ..
        }
    ) {
        return None;
    }
    let precision = context.plan.components.first()?.precision;
    let sample_max = ((1u32 << precision) - 1) as f64;
    let to_mask_u8 = |sample: f64| (sample * 255.0 / sample_max).clamp(0.0, 255.0) as u8;

    // Extract luma component
    let luma = match context.image.colorspace {
        ColorSpace::Gray => {
            // Grayscale: use first component directly
            context
                .load_component_i32(0)
                .ok()?
                .into_iter()
                .map(|value| to_mask_u8(f64::from(value)))
                .collect::<Vec<u8>>()
        }
        ColorSpace::Srgb => {
            // RGB: compute luma as Y = 0.299*R + 0.587*G + 0.114*B
            if context.image.components.len() < 3 {
                return None;
            }
            let r = context.load_component_i32(0).ok()?;
            let g = context.load_component_i32(1).ok()?;
            let b = context.load_component_i32(2).ok()?;

            r.iter()
                .zip(g.iter())
                .zip(b.iter())
                .map(|((&r, &g), &b)| {
                    let y = 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64;
                    to_mask_u8(y)
                })
                .collect::<Vec<u8>>()
        }
        _ => return None, // Unsupported colorspace
    };

    if luma.len() != width * height {
        return None;
    }

    let params = ContrastMaskParams::default();
    Some(build_contrast_mask_map_from_luma_u8(
        &luma, width, height, params,
    ))
}

fn native_emit_plan(plan: &EncodingPlan) -> EncodingPlan {
    let mut adjusted = plan.clone();
    adjusted.use_mct = native_use_mct(plan);
    adjusted
}

fn native_use_mct(plan: &EncodingPlan) -> bool {
    // The current marker path emits a single QCD shared by all components and the
    // decoder intentionally rejects QCC. Reversible RCT can expand RGB component
    // dynamic ranges differently (notably chroma components), so enabling it for
    // lossless RGB before per-component quantization signaling is available can
    // produce codestreams whose code-block pass counts are inconsistent with the
    // advertised bit-plane counts. Keep lossless RGB independent for the Phase 0
    // correctness baseline; re-enable RCT with QCC/range-aware signaling.
    plan.use_mct && matches!(plan.transform, WaveletTransform::Irreversible97)
}

fn allow_component_parallelism(context: &EncodeContext<'_>) -> bool {
    let component_count = usize::from(context.plan.component_count);
    if component_count <= 1 || context.plan.resource_limits.max_threads == Some(1) {
        return false;
    }

    let Some(limit) = context.plan.resource_limits.max_working_memory else {
        return false;
    };

    component_parallel_working_memory_floor(&context.plan)
        .map(|required| limit >= required)
        .unwrap_or(false)
}

/// Conservative lower bound for enabling component-level parallelism.
///
/// The active native component encoder still materializes a full transformed
/// component plane. During sample preparation it also overlaps a source-load
/// `i32` plane with the transformed output. For irreversible RGB ICT, producing
/// any one output component currently loads R, G, and B at once. Only enable
/// component-level Rayon jobs when the declared working-memory budget can hold
/// every component's full-component preparation floor at the same time.
fn component_parallel_working_memory_floor(plan: &EncodingPlan) -> Option<usize> {
    let pixels = usize::try_from(plan.width)
        .ok()?
        .checked_mul(usize::try_from(plan.height).ok()?)?;
    let components = usize::from(plan.component_count);
    let bytes_per_sample_plane = std::mem::size_of::<i32>();
    let preparation_planes_per_component = if native_use_mct(plan) {
        // R + G + B i32 inputs and one f32 ICT output. f32 and i32 are both 4
        // bytes, so this is four full planes per parallel component job.
        4usize
    } else {
        // One loaded i32 source plane plus one transformed coefficient plane.
        2usize
    };

    pixels
        .checked_mul(bytes_per_sample_plane)?
        .checked_mul(preparation_planes_per_component)?
        .checked_mul(components)
}

/// Mean SSIM over non-overlapping 8×8 luma blocks.
/// Wang et al. (2004), constants from the standard formulation.
fn mssim_8x8(orig: &[f32], recon: &[f32], width: usize, height: usize, sample_peak: f64) -> f64 {
    const BLOCK: usize = 8;
    let c1 = (0.01 * sample_peak).powi(2);
    let c2 = (0.03 * sample_peak).powi(2);

    let block_rows = height / BLOCK;
    let block_cols = width / BLOCK;
    if block_rows == 0 || block_cols == 0 {
        return 1.0;
    }

    let mut ssim_sum = 0.0f64;
    let n = (BLOCK * BLOCK) as f64;

    for br in 0..block_rows {
        for bc in 0..block_cols {
            let mut sum_x = 0.0f64;
            let mut sum_y = 0.0f64;
            let mut sum_xx = 0.0f64;
            let mut sum_yy = 0.0f64;
            let mut sum_xy = 0.0f64;

            for dy in 0..BLOCK {
                for dx in 0..BLOCK {
                    let idx = (br * BLOCK + dy) * width + (bc * BLOCK + dx);
                    let x = orig[idx] as f64;
                    let y = recon[idx] as f64;
                    sum_x += x;
                    sum_y += y;
                    sum_xx += x * x;
                    sum_yy += y * y;
                    sum_xy += x * y;
                }
            }

            let ux = sum_x / n;
            let uy = sum_y / n;
            let sx2 = (sum_xx / n - ux * ux).max(0.0);
            let sy2 = (sum_yy / n - uy * uy).max(0.0);
            let sxy = sum_xy / n - ux * uy;

            let num = (2.0 * ux * uy + c1) * (2.0 * sxy + c2);
            let den = (ux * ux + uy * uy + c1) * (sx2 + sy2 + c2);
            ssim_sum += num / den;
        }
    }

    ssim_sum / (block_rows * block_cols) as f64
}

#[cfg(test)]
mod tests {
    use super::{
        IctTileInputs, NativeBackend, allow_component_parallelism,
        component_parallel_working_memory_floor, irreversible_input_component_rect,
    };
    use crate::encode::block_store::EncodedBlockStore;
    use crate::encode::context::EncodeContext;
    use crate::model::{
        ColorSpace, Component, EncodeOptions, Image, OutputFormat, Preset, ResourceLimits,
    };
    use crate::tiling::TileRect;

    fn rgb_context(
        width: u32,
        height: u32,
        quality: u8,
        limits: ResourceLimits,
    ) -> EncodeContext<'static> {
        let samples = usize::try_from(width)
            .unwrap()
            .saturating_mul(usize::try_from(height).unwrap());
        let image = Box::leak(Box::new(Image {
            width,
            height,
            components: (0..3)
                .map(|_| Component {
                    data: vec![0; samples],
                    width,
                    height,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                })
                .collect(),
            colorspace: ColorSpace::Srgb,
        }));
        EncodeContext::new(
            image,
            &EncodeOptions {
                quality,
                format: OutputFormat::J2k,
                profile: Default::default(),
                resource_limits: limits,
                ..Default::default()
            },
        )
        .expect("build context")
    }

    fn gray_context(width: u32, height: u32, quality: u8) -> EncodeContext<'static> {
        let samples = usize::try_from(width)
            .unwrap()
            .saturating_mul(usize::try_from(height).unwrap());
        let image = Box::leak(Box::new(Image {
            width,
            height,
            components: vec![Component {
                data: (0..samples).map(|i| (i % 251) as i32).collect(),
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }],
            colorspace: ColorSpace::Gray,
        }));
        EncodeContext::new(
            image,
            &EncodeOptions {
                quality,
                format: OutputFormat::J2k,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("build context")
    }

    #[test]
    fn component_parallelism_is_disabled_by_default_for_bounded_memory() {
        let context = rgb_context(8, 8, 100, ResourceLimits::default());

        assert!(!allow_component_parallelism(&context));
    }

    #[test]
    fn component_parallelism_requires_declared_memory_budget() {
        let mut context = rgb_context(
            8,
            8,
            Preset::DocumentHigh.quality(),
            ResourceLimits {
                max_threads: Some(3),
                ..Default::default()
            },
        );
        let required =
            component_parallel_working_memory_floor(&context.plan).expect("memory estimate");

        assert!(!allow_component_parallelism(&context));

        context.plan.resource_limits.max_working_memory = Some(required - 1);
        assert!(!allow_component_parallelism(&context));

        context.plan.resource_limits.max_working_memory = Some(required);
        assert!(allow_component_parallelism(&context));
    }

    #[test]
    fn component_parallelism_honors_single_thread_limit() {
        let mut context = rgb_context(
            8,
            8,
            100,
            ResourceLimits {
                max_threads: Some(1),
                ..Default::default()
            },
        );
        let required =
            component_parallel_working_memory_floor(&context.plan).expect("memory estimate");
        context.plan.resource_limits.max_working_memory = Some(required);

        assert!(!allow_component_parallelism(&context));
    }

    #[test]
    fn full_component_coefficients_delegate_to_rect_path() {
        let context = gray_context(16, 16, 100);
        let backend = NativeBackend;

        let full = backend
            .prepare_component_coefficients(&context, 0)
            .expect("full coefficients");
        let rect = backend
            .prepare_component_coefficients_rect(&context, 0, 0, 0, 16, 16)
            .expect("full-rect coefficients");

        assert_eq!(full, rect);
    }

    #[test]
    fn reversible_tile_rect_coefficients_are_tile_local() {
        let context = gray_context(16, 16, 100);
        let backend = NativeBackend;

        let rect = backend
            .prepare_component_coefficients_rect(&context, 0, 4, 4, 8, 8)
            .expect("tile-rect coefficients");

        assert_eq!(rect.width, 8);
        assert_eq!(rect.height, 8);
        assert_eq!(rect.data.len(), 64);
    }

    #[test]
    fn irreversible_ict_tile_rect_coefficients_are_tile_local() {
        let context = rgb_context(
            16,
            16,
            Preset::DocumentHigh.quality(),
            ResourceLimits::default(),
        );
        let backend = NativeBackend;

        let rect = backend
            .prepare_component_coefficients_97_rect(&context, 1, 4, 4, 8, 8)
            .expect("ICT tile-rect coefficients");

        assert_eq!(rect.width, 8);
        assert_eq!(rect.height, 8);
        assert_eq!(rect.data.len(), 64);
    }

    #[test]
    fn shared_ict_inputs_match_per_component_preparation() {
        let context = rgb_context(
            16,
            16,
            Preset::DocumentHigh.quality(),
            ResourceLimits::default(),
        );
        let inputs = IctTileInputs::load(&context, 4, 4, 8, 8).expect("shared RGB inputs");
        for component in 0..3 {
            assert_eq!(
                inputs
                    .output_component(component)
                    .expect("shared ICT output"),
                irreversible_input_component_rect(&context, component, 4, 4, 8, 8)
                    .expect("per-component ICT output"),
            );
        }
    }

    #[test]
    fn shared_ict_tile_encoding_matches_component_by_component_encoding() {
        let context = rgb_context(
            16,
            16,
            Preset::DocumentHigh.quality(),
            ResourceLimits::default(),
        );
        let backend = NativeBackend;
        let shared = backend
            .prepare_tier1_encoded_mct_components_rect(&context, 2, 4, 4, 8, 8)
            .expect("shared ICT tile encoding");
        let individual = (0..3)
            .map(|component| {
                backend
                    .prepare_tier1_encoded_component_rect(&context, 2, component, 4, 4, 8, 8)
                    .expect("individual ICT tile encoding")
            })
            .collect::<Vec<_>>();
        assert_eq!(shared, individual);
    }

    #[test]
    fn full_tier1_component_encoding_delegates_to_tile_rect_path() {
        let context = gray_context(16, 16, 100);
        let backend = NativeBackend;

        let full = backend
            .prepare_tier1_encoded_component(&context, 0)
            .expect("full Tier-1");
        let rect = backend
            .prepare_tier1_encoded_component_rect(&context, 0, 0, 0, 0, 16, 16)
            .expect("full-rect Tier-1");

        assert_eq!(full, rect);
    }

    #[test]
    fn tile_rect_tier1_component_encoding_preserves_tile_index() {
        let context = gray_context(16, 16, 100);
        let backend = NativeBackend;

        let encoded = backend
            .prepare_tier1_encoded_component_rect(&context, 5, 0, 4, 4, 8, 8)
            .expect("tile-rect Tier-1");

        assert!(
            encoded
                .bands
                .iter()
                .flat_map(|band| &band.blocks)
                .all(|block| { block.tile_index == 5 && block.x1 <= 8 && block.y1 <= 8 })
        );
    }

    #[test]
    fn full_tile_part_payload_delegates_to_tile_rect_payload_for_lossless() {
        let context = gray_context(16, 16, 100);
        let backend = NativeBackend;

        let full = backend
            .prepare_tile_part_payload(&context)
            .expect("full tile payload");
        let tile_rect = backend
            .prepare_tile_part_payload_for_tile_rect(
                &context,
                TileRect {
                    tile_index: 0,
                    x0: 0,
                    y0: 0,
                    x1: 16,
                    y1: 16,
                },
            )
            .expect("full tile-rect payload");

        let mut full_bytes = Vec::new();
        full.write_to(&mut full_bytes).expect("write full payload");
        let mut rect_bytes = Vec::new();
        tile_rect
            .write_to(&mut rect_bytes)
            .expect("write tile-rect payload");
        assert_eq!(full_bytes, rect_bytes);
    }

    #[test]
    fn tile_rect_payload_encodes_tile_local_component_extent() {
        let context = gray_context(16, 16, 100);
        let backend = NativeBackend;

        let payload = backend
            .prepare_tile_part_payload_for_tile_rect(
                &context,
                TileRect {
                    tile_index: 4,
                    x0: 4,
                    y0: 4,
                    x1: 12,
                    y1: 12,
                },
            )
            .expect("tile-rect payload");

        assert!(payload.packet_count() > 0);
    }

    #[test]
    fn stored_tile_boundary_spills_payloads_and_preserves_tile_identity() {
        let context = gray_context(16, 16, 100);
        let backend = NativeBackend;
        let limits = ResourceLimits {
            encoded_store_memory_limit: Some(1),
            ..Default::default()
        };
        let mut store = EncodedBlockStore::from_resource_limits(&limits);

        let layouts = backend
            .prepare_stored_tier1_for_tile_rect(
                &context,
                TileRect {
                    tile_index: 7,
                    x0: 0,
                    y0: 0,
                    x1: 16,
                    y1: 16,
                },
                &mut store,
            )
            .expect("store tile");

        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].tile_index, 7);
        assert!(
            layouts[0]
                .bands
                .iter()
                .flat_map(|band| &band.blocks)
                .all(|block| block.id.tile_index == 7 && block.id.component == 0)
        );
        let stats = store.stats();
        assert!(stats.payload_count > 0);
        assert!(stats.spilled_payload_count > 0);
        assert!(stats.memory_bytes <= 1);
    }
}
