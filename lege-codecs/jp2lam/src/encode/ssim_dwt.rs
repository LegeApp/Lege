//! Unquantized 9/7 DWT cache for perceptual outer rungs.
//!
//! Production wiring lives on `NativeBackend`. These tests lock the Session 7
//! gates: requantize matches a fresh transform, the forward DWT runs once per
//! tile-component when the cache is kept, winners match the recompute path,
//! and a tight working-memory budget falls back instead of failing.

use crate::encode::backend::native::{
    NativeBackend, UnquantizedTileDwt, forward_dwt_call_count, reset_forward_dwt_calls,
    unquantized_dwt_cache_fits, unquantized_dwt_retention_bytes,
};
use crate::encode::block_store::EncodedBlockStore;
use crate::encode::context::EncodeContext;
use crate::error::Result;
use crate::j2k::{CodestreamParts, build_main_header_segments};
use crate::model::{
    EncodeOptions, Image, OutputFormat, PerceptualEffort, PerceptualTarget, RateControl,
    ResourceLimits, TilePolicy,
};
use crate::plan::{EncodingPlan, apply_perceptual_quant_scale, perceptual_quant_scale_for_quality};
use crate::tiling::{TileRect, tile_grid};

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

fn quality_options(quality: u8) -> EncodeOptions {
    EncodeOptions {
        rate_control: Some(RateControl::ApproxQuality(quality)),
        format: OutputFormat::J2k,
        tile_policy: TilePolicy::Single,
        ..Default::default()
    }
}

fn perceptual_options(score: f64, limits: ResourceLimits) -> EncodeOptions {
    EncodeOptions {
        rate_control: Some(RateControl::Perceptual(
            PerceptualTarget::new(score, PerceptualEffort::Fast).expect("target"),
        )),
        format: OutputFormat::J2k,
        tile_policy: TilePolicy::Single,
        resource_limits: limits,
        ..Default::default()
    }
}

fn apply_rung(plan: &EncodingPlan, quality: u8) -> EncodingPlan {
    let scale = perceptual_quant_scale_for_quality(quality);
    let mut plan = plan.clone();
    plan.quant_scale = scale;
    plan.quality = quality;
    plan.subband_quants = plan.base_subband_quants.clone();
    apply_perceptual_quant_scale(&mut plan.subband_quants, scale);
    plan
}

fn encode_search(
    backend: &NativeBackend,
    context: &EncodeContext<'_>,
    tile_rects: &[TileRect],
    cache: Option<&[UnquantizedTileDwt]>,
) -> Result<Vec<u8>> {
    let (tile_parts, winner_plan) =
        backend.search_perceptual_tile_parts_with_optional_cache(context, tile_rects, cache)?;
    let emit_plan = backend.emit_plan(&winner_plan);
    let main_header_segments = build_main_header_segments(&emit_plan)?;
    CodestreamParts {
        main_header_segments,
        tile_parts,
    }
    .encode(&emit_plan)
}

fn assert_cached_quantize_matches_direct(image: &Image, quality: u8) {
    let options = quality_options(quality);
    let context = EncodeContext::new(image, &options).expect("context");
    let backend = NativeBackend;
    let tile = tile_grid(&context.plan).tile_rects()[0];
    let cached = backend
        .prepare_unquantized_dwt_for_tile_rect(&context, tile)
        .expect("dwt");
    assert_eq!(
        cached.components.len(),
        usize::from(context.plan.component_count)
    );
    for dwt in &cached.components {
        let direct = backend
            .prepare_component_coefficients_97_rect(
                &context,
                dwt.component_index,
                dwt.x0 as u32,
                dwt.y0 as u32,
                dwt.width as u32,
                dwt.height as u32,
            )
            .expect("direct");
        let requant = backend
            .quantize_unquantized_component(&context, dwt)
            .expect("requant");
        assert_eq!(
            direct.data, requant.data,
            "cached requantize must match a fresh 9/7+quant path"
        );
        assert_eq!(direct.x0, requant.x0);
        assert_eq!(direct.y0, requant.y0);
        assert_eq!(direct.width, requant.width);
        assert_eq!(direct.height, requant.height);
        assert_eq!(direct.levels, requant.levels);
    }
}

#[test]
fn gray_cached_requantize_matches_direct_prepare() {
    let image = gray_ramp(32, 24);
    for quality in [75_u8, 90, 99] {
        assert_cached_quantize_matches_direct(&image, quality);
    }
}

#[test]
fn rgb_cached_requantize_matches_direct_prepare() {
    let image = rgb_ramp(24, 20);
    for quality in [75_u8, 90, 99] {
        assert_cached_quantize_matches_direct(&image, quality);
    }
}

#[test]
fn cached_outer_rungs_do_not_rerun_forward_dwt() {
    let image = rgb_ramp(24, 20);
    let options = perceptual_options(40.0, ResourceLimits::default());
    let context = EncodeContext::new(&image, &options).expect("context");
    let backend = NativeBackend;
    let tile_rects = tile_grid(&context.plan).tile_rects();
    reset_forward_dwt_calls();
    let cache = backend
        .try_cache_unquantized_dwt(&context, &tile_rects)
        .expect("cache")
        .expect("memory permits default cache");
    let after_cache = forward_dwt_call_count();
    assert_eq!(
        after_cache,
        u32::from(context.plan.component_count) * tile_rects.len() as u32,
        "cache build runs the forward DWT once per tile-component"
    );

    for quality in [75_u8, 90, 99] {
        let rung_plan = apply_rung(&context.plan, quality);
        let rung_context = context.with_plan(rung_plan);
        let mut store = EncodedBlockStore::from_resource_limits(&rung_context.plan.resource_limits);
        for cached in &cache {
            let _ = backend
                .prepare_stored_tier1_from_unquantized_dwt(&rung_context, cached, &mut store)
                .expect("requant+t1");
        }
    }
    assert_eq!(
        forward_dwt_call_count(),
        after_cache,
        "requantizing outer rungs must not rerun the forward DWT"
    );
}

#[test]
fn cached_search_matches_recompute_winner() {
    let image = gray_ramp(32, 24);
    let options = perceptual_options(50.0, ResourceLimits::default());
    let context = EncodeContext::new(&image, &options).expect("context");
    let backend = NativeBackend;
    let tile_rects = tile_grid(&context.plan).tile_rects();
    let cache = backend
        .try_cache_unquantized_dwt(&context, &tile_rects)
        .expect("cache")
        .expect("cache");
    let cached_bytes =
        encode_search(&backend, &context, &tile_rects, Some(&cache)).expect("cached");
    let fresh_bytes = encode_search(&backend, &context, &tile_rects, None).expect("fresh");
    assert_eq!(
        cached_bytes, fresh_bytes,
        "DWT cache must not change the perceptual winner"
    );
}

#[test]
fn rgb_cached_search_matches_recompute_winner() {
    let image = rgb_ramp(24, 20);
    let options = perceptual_options(40.0, ResourceLimits::default());
    let context = EncodeContext::new(&image, &options).expect("context");
    let backend = NativeBackend;
    let tile_rects = tile_grid(&context.plan).tile_rects();
    let cache = backend
        .try_cache_unquantized_dwt(&context, &tile_rects)
        .expect("cache")
        .expect("cache");
    let cached_bytes =
        encode_search(&backend, &context, &tile_rects, Some(&cache)).expect("cached");
    let fresh_bytes = encode_search(&backend, &context, &tile_rects, None).expect("fresh");
    assert_eq!(cached_bytes, fresh_bytes);
}

#[test]
fn tight_working_memory_falls_back_without_changing_the_winner() {
    let image = gray_ramp(32, 24);
    let default_options = perceptual_options(50.0, ResourceLimits::default());
    let default_context = EncodeContext::new(&image, &default_options).expect("ctx");
    let tile_rects = tile_grid(&default_context.plan).tile_rects();
    let required =
        unquantized_dwt_retention_bytes(&default_context.plan, &tile_rects).expect("retention");
    assert!(required > 1);
    let tight = ResourceLimits {
        max_working_memory: Some(required - 1),
        ..Default::default()
    };
    let tight_options = perceptual_options(50.0, tight);
    let tight_context = EncodeContext::new(&image, &tight_options).expect("tight ctx");
    assert!(
        !unquantized_dwt_cache_fits(&tight_context.plan, &tile_rects),
        "declared budget below retention must refuse the cache"
    );
    let backend = NativeBackend;
    assert!(
        backend
            .try_cache_unquantized_dwt(&tight_context, &tile_rects)
            .expect("try")
            .is_none()
    );

    let cached = encode_search(
        &backend,
        &default_context,
        &tile_rects,
        Some(
            &backend
                .try_cache_unquantized_dwt(&default_context, &tile_rects)
                .expect("cache")
                .expect("default caches"),
        ),
    )
    .expect("cached");
    let fallback = encode_search(&backend, &tight_context, &tile_rects, None).expect("fallback");
    assert_eq!(
        cached, fallback,
        "memory fallback must emit the same winner as the cached path"
    );
}
