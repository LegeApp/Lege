//! Internal candidate reconstruction from stored Tier-1 selections.
//!
//! Inverts selected coding-pass prefixes with the decoder's Tier-1, dequantizes
//! into the tile-component plane, then runs inverse 9/7 / ICT / finalize.
//! The pixels must match serialize → [`crate::decode::decode_jp2`].

use crate::decode::reconstruct::quant_step;
use crate::decode::t1::{Tier1Scratch, decode_pass_prefix};
use crate::encode::backend::native::NativeTier1SelectionLayout;
use crate::encode::block_store::{EncodedBlockStore, StoredTier1Layout};
use crate::error::{Jp2LamError, Result};
use crate::j2k::decode_markers::QuantizationStep;
use crate::model::{ColorSpace, Component, Image};
use crate::plan::{EncodingPlan, WaveletTransform};
use crate::simd::PRIMITIVES;
use crate::tiling::TileRect;

pub(crate) fn reconstruct_stored_selection(
    plan: &EncodingPlan,
    colorspace: ColorSpace,
    stored_tiles: &[(TileRect, Vec<StoredTier1Layout>)],
    selections: &[Vec<NativeTier1SelectionLayout>],
    store: &EncodedBlockStore,
) -> Result<Image> {
    let _p = crate::encode::profile_enter("ssim::reconstruct");
    if stored_tiles.len() != selections.len() {
        return Err(Jp2LamError::EncodeFailed(
            "stored tile count does not match selection count".into(),
        ));
    }
    if !matches!(plan.transform, WaveletTransform::Irreversible97) {
        return Err(Jp2LamError::EncodeFailed(
            "internal perceptual reconstruction supports irreversible 9/7 only".into(),
        ));
    }

    let width = plan.width;
    let height = plan.height;
    let pixels = usize::try_from(u64::from(width) * u64::from(height))
        .map_err(|_| Jp2LamError::EncodeFailed("image area exceeds usize".into()))?;
    let component_count = usize::from(plan.component_count);
    let mut output = vec![vec![0i32; pixels]; component_count];
    let mut scratch = Tier1Scratch::new();

    for ((tile, layouts), tile_selections) in stored_tiles.iter().zip(selections) {
        if layouts.len() != tile_selections.len() {
            return Err(Jp2LamError::EncodeFailed(
                "component layout count does not match selections".into(),
            ));
        }
        let mut planes = Vec::with_capacity(layouts.len());
        for (layout, selection) in layouts.iter().zip(tile_selections.iter()) {
            let component = usize::from(layout.component);
            let precision = plan
                .components
                .get(component)
                .map(|c| c.precision)
                .unwrap_or(8);
            planes.push(reconstruct_tile_component(
                plan,
                tile,
                layout,
                selection,
                store,
                precision,
                &mut scratch,
            )?);
        }
        if plan.use_mct && planes.len() == 3 {
            let [y, cb, cr] = planes.as_mut_slice() else {
                return Err(Jp2LamError::EncodeFailed(
                    "ICT requires three planes".into(),
                ));
            };
            (PRIMITIVES.color.inverse_ict)(y, cb, cr);
        }
        for (component, plane) in planes.into_iter().enumerate() {
            let precision = plan
                .components
                .get(component)
                .map(|c| c.precision)
                .unwrap_or(8);
            let finalized = finalize_f32_samples(plane, precision as u8);
            blit_tile(&mut output[component], width, height, tile, &finalized)?;
        }
    }

    Ok(Image {
        width,
        height,
        colorspace,
        components: output
            .into_iter()
            .enumerate()
            .map(|(index, data)| {
                let meta = plan.components.get(index);
                Component {
                    data,
                    width,
                    height,
                    precision: meta.map(|c| c.precision).unwrap_or(8),
                    signed: meta.map(|c| c.signed).unwrap_or(false),
                    dx: meta.map(|c| c.dx).unwrap_or(1),
                    dy: meta.map(|c| c.dy).unwrap_or(1),
                }
            })
            .collect(),
    })
}

fn reconstruct_tile_component(
    plan: &EncodingPlan,
    tile: &TileRect,
    layout: &StoredTier1Layout,
    selection: &NativeTier1SelectionLayout,
    store: &EncodedBlockStore,
    precision: u32,
    scratch: &mut Tier1Scratch,
) -> Result<Vec<f32>> {
    let tile_width = usize::try_from(tile.width())
        .map_err(|_| Jp2LamError::EncodeFailed("tile width exceeds usize".into()))?;
    let tile_height = usize::try_from(tile.height())
        .map_err(|_| Jp2LamError::EncodeFailed("tile height exceeds usize".into()))?;
    let mut plane = vec![0.0f32; tile_width.saturating_mul(tile_height)];

    if layout.bands.len() != selection.bands.len() {
        return Err(Jp2LamError::EncodeFailed(
            "stored band count does not match selection".into(),
        ));
    }

    for (band, selected) in layout.bands.iter().zip(selection.bands.iter()) {
        if band.blocks.len() != selected.selected_passes.len() {
            return Err(Jp2LamError::EncodeFailed(
                "stored block count does not match selected passes".into(),
            ));
        }
        let step = quant_step_for(plan, precision, band.resolution, band.band)?;
        for (block, &pass_count) in band.blocks.iter().zip(selected.selected_passes.iter()) {
            let width = block.x1.saturating_sub(block.x0);
            let height = block.y1.saturating_sub(block.y0);
            if width == 0 || height == 0 {
                continue;
            }
            let max_bitplanes = block
                .zero_bitplanes
                .saturating_add(block.magnitude_bitplanes);
            if pass_count == 0 || max_bitplanes == 0 {
                continue;
            }
            let prefix_len = selected_prefix_len(block, pass_count)?;
            let payload = store.copy_prefix(block.payload, prefix_len)?;
            decode_pass_prefix(
                width,
                height,
                band.band,
                max_bitplanes,
                u32::from(block.zero_bitplanes),
                u32::from(pass_count),
                &payload,
                scratch,
            )?;
            dequantize_block(
                &mut plane,
                tile_width,
                block.x0,
                block.y0,
                width,
                height,
                scratch.coefficients(),
                step,
            )?;
        }
    }

    (PRIMITIVES.dwt.inverse_97_2d)(
        &mut plane,
        tile_width,
        tile_height,
        plan.decomposition_levels,
    )?;
    Ok(plane)
}

fn selected_prefix_len(
    block: &crate::encode::block_store::StoredCodeBlock,
    pass_count: u16,
) -> Result<usize> {
    if pass_count == 0 {
        return Ok(0);
    }
    let index = usize::from(pass_count.saturating_sub(1));
    let pass = block.passes.get(index).ok_or_else(|| {
        Jp2LamError::EncodeFailed("selected pass count exceeds stored passes".into())
    })?;
    Ok(pass.cumulative_length)
}

fn quant_step_for(
    plan: &EncodingPlan,
    precision: u32,
    resolution: u8,
    band: crate::plan::BandOrientation,
) -> Result<f32> {
    let quant = plan
        .subband_quants
        .iter()
        .find(|q| q.resolution == resolution && q.band == band)
        .ok_or_else(|| {
            Jp2LamError::EncodeFailed("missing subband quantizer for reconstruction".into())
        })?;
    Ok(quant_step(
        precision,
        band,
        QuantizationStep {
            exponent: quant.exponent,
            mantissa: quant.mantissa,
        },
    ))
}

fn dequantize_block(
    plane: &mut [f32],
    tile_width: usize,
    x0: usize,
    y0: usize,
    width: usize,
    height: usize,
    coefficients: &[i32],
    step: f32,
) -> Result<()> {
    for y in 0..height {
        let dst = y0
            .checked_add(y)
            .and_then(|row| row.checked_mul(tile_width))
            .and_then(|row| row.checked_add(x0))
            .ok_or_else(|| Jp2LamError::EncodeFailed("block copy offset overflow".into()))?;
        let src = y.saturating_mul(width);
        let dst_slice = plane
            .get_mut(dst..dst.saturating_add(width))
            .ok_or_else(|| Jp2LamError::EncodeFailed("decoded block extends past tile".into()))?;
        let src_slice = coefficients
            .get(src..src.saturating_add(width))
            .ok_or_else(|| {
                Jp2LamError::EncodeFailed("decoded coefficients shorter than block".into())
            })?;
        for (out, &q) in dst_slice.iter_mut().zip(src_slice) {
            *out = crate::simd::scalar::dequantize_i32_to_f32(q, step);
        }
    }
    Ok(())
}

fn finalize_f32_samples(samples: Vec<f32>, precision: u8) -> Vec<i32> {
    let shift = (1u32 << (precision.saturating_sub(1).min(31))) as f32;
    let max_sample = ((1u32 << precision.min(31)) - 1) as f32;
    samples
        .into_iter()
        .map(|sample| (sample + shift + 0.5).clamp(0.0, max_sample) as i32)
        .collect()
}

fn blit_tile(
    dest: &mut [i32],
    image_width: u32,
    image_height: u32,
    tile: &TileRect,
    src: &[i32],
) -> Result<()> {
    let tw = tile.width();
    let th = tile.height();
    let expected = usize::try_from(u64::from(tw) * u64::from(th))
        .map_err(|_| Jp2LamError::EncodeFailed("tile area exceeds usize".into()))?;
    if src.len() != expected {
        return Err(Jp2LamError::EncodeFailed(
            "reconstructed tile size does not match tile rectangle".into(),
        ));
    }
    for y in 0..th {
        let dest_y = tile.y0.saturating_add(y);
        if dest_y >= image_height {
            return Err(Jp2LamError::EncodeFailed("tile row exceeds image".into()));
        }
        let dest_row =
            usize::try_from(u64::from(dest_y) * u64::from(image_width) + u64::from(tile.x0))
                .map_err(|_| Jp2LamError::EncodeFailed("blit offset overflow".into()))?;
        let src_row = usize::try_from(u64::from(y) * u64::from(tw))
            .map_err(|_| Jp2LamError::EncodeFailed("blit source overflow".into()))?;
        let width = usize::try_from(tw)
            .map_err(|_| Jp2LamError::EncodeFailed("tile width exceeds usize".into()))?;
        if tile.x0.saturating_add(tw) > image_width {
            return Err(Jp2LamError::EncodeFailed(
                "tile column exceeds image".into(),
            ));
        }
        dest[dest_row..dest_row + width].copy_from_slice(&src[src_row..src_row + width]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::reconstruct_stored_selection;
    use crate::decode::decode_jp2;
    use crate::encode::backend::native::{
        NativeBackend, build_stored_tile_parts, select_stored_tile_passes,
    };
    use crate::encode::block_store::EncodedBlockStore;
    use crate::encode::context::EncodeContext;
    use crate::j2k::{CodestreamParts, build_main_header_segments};
    use crate::model::{EncodeOptions, Image, OutputFormat, RateControl};
    use crate::tiling::tile_grid;

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

    fn rgb_ramp(width: u32, height: u32) -> Image {
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                data.push((x % 256) as u8);
                data.push((y % 256) as u8);
                data.push(((x + y) % 256) as u8);
            }
        }
        Image::from_rgb_bytes(width, height, &data).expect("rgb")
    }

    fn assert_internal_matches_decode(image: &Image, quality: u8, truncate: bool) {
        let options = EncodeOptions {
            rate_control: Some(RateControl::ApproxQuality(quality)),
            format: OutputFormat::J2k,
            ..Default::default()
        };
        let context = EncodeContext::new(image, &options).expect("context");
        let backend = NativeBackend;
        let grid = tile_grid(&context.plan);
        let tile_rects = grid.tile_rects();
        let mut store = EncodedBlockStore::from_resource_limits(&context.plan.resource_limits);
        let mut stored_tiles = Vec::new();
        for tile in &tile_rects {
            stored_tiles.push((
                *tile,
                backend
                    .prepare_stored_tier1_for_tile_rect(&context, *tile, &mut store)
                    .expect("t1"),
            ));
        }
        let max_body = stored_tiles
            .iter()
            .flat_map(|(_, layouts)| layouts)
            .flat_map(|layout| &layout.bands)
            .flat_map(|band| &band.blocks)
            .filter_map(|block| block.passes.last())
            .fold(0u64, |total, pass| {
                total.saturating_add(pass.cumulative_length as u64)
            })
            .min(u64::from(u32::MAX)) as u32;
        let body = if truncate {
            (max_body / 2).max(1)
        } else {
            max_body.max(1)
        };
        let selections =
            select_stored_tile_passes(&stored_tiles, &context, Some(body)).expect("sel");
        let reconstructed = reconstruct_stored_selection(
            &context.plan,
            image.colorspace,
            &stored_tiles,
            &selections,
            &store,
        )
        .expect("recon");

        let emit_plan = backend.emit_plan(&context.plan);
        let headers = build_main_header_segments(&emit_plan).expect("hdr");
        let shared = store.into_shared();
        let tile_parts =
            build_stored_tile_parts(&stored_tiles, selections.clone(), shared).expect("parts");
        let stream = CodestreamParts {
            main_header_segments: headers,
            tile_parts,
        }
        .encode(&emit_plan)
        .expect("encode");
        let decoded = decode_jp2(&stream).expect("decode");
        assert_eq!(reconstructed.width, decoded.width);
        assert_eq!(reconstructed.height, decoded.height);
        assert_eq!(reconstructed.components.len(), decoded.components.len());
        for (a, b) in reconstructed.components.iter().zip(&decoded.components) {
            assert_eq!(a.data, b.data, "internal recon must match serialize+decode");
        }
    }

    #[test]
    fn gray_all_passes_match_serialize_decode() {
        assert_internal_matches_decode(&gray_ramp(32, 24), 75, false);
    }

    #[test]
    fn gray_truncated_passes_match_serialize_decode() {
        assert_internal_matches_decode(&gray_ramp(32, 24), 75, true);
    }

    #[test]
    fn rgb_all_passes_match_serialize_decode() {
        assert_internal_matches_decode(&rgb_ramp(24, 20), 90, false);
    }

    #[test]
    fn rgb_truncated_passes_match_serialize_decode() {
        assert_internal_matches_decode(&rgb_ramp(24, 20), 90, true);
    }
}
