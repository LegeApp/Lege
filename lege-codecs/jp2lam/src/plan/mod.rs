mod derive;
mod validate;

use crate::error::{Jp2LamError, Result};
use crate::model::{
    ColorEncoding, ColorSpace, ComponentView, EncodeOptions, Image, ImageView, OutputFormat,
    RateControl, ResourceLimits, TilePolicy,
};
use derive::{
    apply_document_step_scaling, apply_quality_step_scaling, derive_code_block_size, derive_lane,
    derive_subband_quants, derive_view_component_plans, max_decompositions,
    max_target_decompositions, tcp_rate_from_quality, use_mct,
};
use validate::{validate_image, validate_image_view};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressionOrder {
    Lrcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaveletTransform {
    Reversible53,
    Irreversible97,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuantizationStyle {
    NoQuantization,
    ScalarExpounded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BandOrientation {
    Ll,
    Hl,
    Lh,
    Hh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncodeLane {
    GrayLossless,
    GrayLossy,
    RgbLossless,
    RgbLossy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodeBlockSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct QualityLayer {
    pub target_rate: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputRateTarget {
    Bytes(u64),
}

/// How PCRD pass selection is driven for lossy encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateMode {
    /// Quality maps to a lambda threshold; the heuristic distortion model
    /// ranks passes. Original behavior.
    QualityLambda,
    /// Quantization step size does the rate control; PCRD only trims passes
    /// with negligible measured-ΔMSE payoff. Used by `ContentProfile::Document`.
    DocumentTrim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TilePlan {
    pub index: u16,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ComponentPlan {
    pub precision: u32,
    pub signed: bool,
    pub dx: u32,
    pub dy: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubbandQuant {
    pub resolution: u8,
    pub band: BandOrientation,
    pub exponent: u8,
    pub mantissa: u16,
}

impl From<&crate::model::Component> for ComponentPlan {
    fn from(component: &crate::model::Component) -> Self {
        Self {
            precision: component.precision,
            signed: component.signed,
            dx: component.dx,
            dy: component.dy,
        }
    }
}

impl From<&ComponentView<'_>> for ComponentPlan {
    fn from(component: &ComponentView<'_>) -> Self {
        Self {
            precision: component.precision,
            signed: component.signed,
            dx: component.dx,
            dy: component.dy,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct EncodingPlan {
    pub width: u32,
    pub height: u32,
    pub component_count: u16,
    pub colorspace: ColorSpace,
    pub color_encoding: ColorEncoding,
    pub output_format: OutputFormat,
    pub quality: u8,
    pub rate_control: RateControl,
    pub output_rate_target: Option<OutputRateTarget>,
    pub lane: EncodeLane,
    pub rate_mode: RateMode,
    pub progression_order: ProgressionOrder,
    pub transform: WaveletTransform,
    pub quantization_style: QuantizationStyle,
    pub use_mct: bool,
    pub decomposition_levels: u8,
    pub num_resolutions: u8,
    pub code_block_size: CodeBlockSize,
    pub guard_bits: u8,
    pub layers: Vec<QualityLayer>,
    pub tile_policy: TilePolicy,
    pub resource_limits: ResourceLimits,
    pub tile: TilePlan,
    pub components: Vec<ComponentPlan>,
    pub subband_quants: Vec<SubbandQuant>,
}

impl EncodingPlan {
    #[allow(dead_code)]
    pub(crate) fn build(image: &Image, options: &EncodeOptions) -> Result<Self> {
        validate_image(image)?;
        let view = image.as_view()?;
        Self::build_view(&view, options)
    }

    pub(crate) fn build_view(image: &ImageView<'_>, options: &EncodeOptions) -> Result<Self> {
        validate_image_view(image)?;
        validate_resource_limits(&options.resource_limits)?;

        let color_encoding = match (&options.color_encoding, image.colorspace) {
            (Some(encoding), colorspace) => {
                encoding.validate_for(colorspace)?;
                encoding.clone()
            }
            (None, ColorSpace::Gray) => ColorEncoding::Gray,
            (None, ColorSpace::Srgb) => ColorEncoding::Srgb,
            (None, ColorSpace::Rgb) => {
                return Err(Jp2LamError::InvalidInput(
                    "ambiguous RGB input requires an explicit ColorEncoding".into(),
                ));
            }
            (None, ColorSpace::Yuv | ColorSpace::YCbCr) => {
                return Err(Jp2LamError::InvalidInput(
                    "implicit YUV/YCbCr conversion is not an advertised photographic input".into(),
                ));
            }
            (None, ColorSpace::Cmyk) => {
                // CMYK is a decode-only color space in this crate; the encoder
                // targets the grayscale/sRGB photographic pipeline.
                return Err(Jp2LamError::InvalidInput(
                    "CMYK encoding is not supported (decode-only color space)".into(),
                ));
            }
        };

        let encoding_colorspace = image.colorspace.encoding_domain();
        let rate_control = resolve_rate_control(image, options)?;
        let (quality, output_rate_target) = match rate_control {
            RateControl::Lossless => (100, None),
            RateControl::Quality(quality) => (quality, None),
            RateControl::TargetBytes(bytes) => (99, Some(OutputRateTarget::Bytes(bytes))),
            RateControl::TargetBitsPerPixel(bpp) => (
                99,
                Some(OutputRateTarget::Bytes(target_bytes_from_bpp(image, bpp)?)),
            ),
            RateControl::CompressionRatio(ratio) => (
                99,
                Some(OutputRateTarget::Bytes(target_bytes_from_ratio(
                    image, ratio,
                )?)),
            ),
        };
        let is_lossless = matches!(rate_control, RateControl::Lossless);
        let transform = if is_lossless {
            WaveletTransform::Reversible53
        } else {
            WaveletTransform::Irreversible97
        };
        let tile = derive_tile_plan(image, options, 0)?;
        let decomposition_cap = max_target_decompositions(encoding_colorspace);
        let decomposition_levels = max_decompositions(image.width, image.height)
            .min(decomposition_cap)
            .min(max_tile_grid_decompositions(
                image.width,
                image.height,
                tile.width,
                tile.height,
            )) as u8;
        let use_mct = use_mct(encoding_colorspace);
        let target_rate = if is_lossless {
            None
        } else {
            Some(tcp_rate_from_quality(quality))
        };
        let lane = derive_lane(encoding_colorspace, target_rate.is_none() && is_lossless);
        let components = derive_view_component_plans(image);
        let (quantization_style, mut subband_quants) = derive_subband_quants(
            image.components[0].precision,
            decomposition_levels,
            transform,
        );
        // Scale step sizes at the plan level so quantizer, tier-1 bitplane
        // analysis, PCRD distortion estimates, and the QCD header all agree.
        let rate_mode = if matches!(options.profile, crate::model::ContentProfile::Document)
            && matches!(transform, WaveletTransform::Irreversible97)
        {
            RateMode::DocumentTrim
        } else {
            RateMode::QualityLambda
        };
        if matches!(transform, WaveletTransform::Irreversible97) {
            match rate_mode {
                RateMode::DocumentTrim => {
                    apply_document_step_scaling(&mut subband_quants, quality);
                }
                RateMode::QualityLambda => {
                    apply_quality_step_scaling(&mut subband_quants, quality);
                }
            }
        }

        Ok(Self {
            width: image.width,
            height: image.height,
            component_count: image.components.len() as u16,
            colorspace: encoding_colorspace,
            color_encoding,
            output_format: options.format,
            quality,
            rate_control,
            output_rate_target,
            lane,
            rate_mode,
            progression_order: ProgressionOrder::Lrcp,
            transform,
            quantization_style,
            use_mct,
            decomposition_levels,
            num_resolutions: decomposition_levels.saturating_add(1),
            code_block_size: derive_code_block_size(quality),
            guard_bits: 2,
            layers: vec![QualityLayer { target_rate }],
            tile_policy: options.tile_policy,
            resource_limits: options.resource_limits.clone(),
            tile,
            components,
            subband_quants,
        })
    }

    pub(crate) fn is_lossless(&self) -> bool {
        matches!(self.transform, WaveletTransform::Reversible53)
            && self.layers[0].target_rate.is_none()
    }
}

fn resolve_rate_control(image: &ImageView<'_>, options: &EncodeOptions) -> Result<RateControl> {
    let rate_control = options.rate_control.unwrap_or_else(|| {
        if options.quality >= 100 {
            RateControl::Lossless
        } else {
            RateControl::Quality(options.quality)
        }
    });
    match rate_control {
        RateControl::Quality(0..=99) | RateControl::Lossless => Ok(rate_control),
        RateControl::Quality(quality) => Err(Jp2LamError::InvalidInput(format!(
            "quality rate control must be in 0..=99, got {quality}"
        ))),
        RateControl::TargetBytes(0) => Err(Jp2LamError::InvalidInput(
            "target output bytes must be non-zero".into(),
        )),
        RateControl::TargetBytes(_) => Ok(rate_control),
        RateControl::TargetBitsPerPixel(value) if value.is_finite() && value > 0.0 => {
            let _ = target_bytes_from_bpp(image, value)?;
            Ok(rate_control)
        }
        RateControl::CompressionRatio(value) if value.is_finite() && value > 0.0 => {
            let _ = target_bytes_from_ratio(image, value)?;
            Ok(rate_control)
        }
        RateControl::TargetBitsPerPixel(value) => Err(Jp2LamError::InvalidInput(format!(
            "target bits per pixel must be finite and positive, got {value}"
        ))),
        RateControl::CompressionRatio(value) => Err(Jp2LamError::InvalidInput(format!(
            "compression ratio must be finite and positive, got {value}"
        ))),
    }
}

fn target_bytes_from_bpp(image: &ImageView<'_>, bpp: f32) -> Result<u64> {
    let pixels = u64::from(image.width)
        .checked_mul(u64::from(image.height))
        .ok_or_else(|| Jp2LamError::InvalidInput("image pixel count overflows u64".into()))?;
    rounded_positive_bytes(pixels as f64 * f64::from(bpp) / 8.0, "target bpp")
}

fn target_bytes_from_ratio(image: &ImageView<'_>, ratio: f32) -> Result<u64> {
    let source_bits = image
        .components
        .iter()
        .try_fold(0u128, |total, component| {
            let sampled_width = u128::from(image.width).div_ceil(u128::from(component.dx));
            let sampled_height = u128::from(image.height).div_ceil(u128::from(component.dy));
            total
                .checked_add(
                    sampled_width
                        .checked_mul(sampled_height)
                        .and_then(|samples| samples.checked_mul(u128::from(component.precision)))
                        .ok_or_else(|| {
                            Jp2LamError::InvalidInput(
                                "meaningful source bit count overflows".into(),
                            )
                        })?,
                )
                .ok_or_else(|| Jp2LamError::InvalidInput("source bit count overflows".into()))
        })?;
    rounded_positive_bytes(
        source_bits as f64 / f64::from(ratio) / 8.0,
        "compression ratio",
    )
}

fn rounded_positive_bytes(value: f64, label: &str) -> Result<u64> {
    if !value.is_finite() || value > u64::MAX as f64 {
        return Err(Jp2LamError::InvalidInput(format!(
            "{label} produces an unrepresentable output size"
        )));
    }
    Ok(value.round().max(1.0) as u64)
}

fn derive_tile_plan(
    image: &ImageView<'_>,
    options: &EncodeOptions,
    _decomposition_levels: u8,
) -> Result<TilePlan> {
    match options.tile_policy {
        TilePolicy::Single => Ok(TilePlan {
            index: 0,
            width: image.width,
            height: image.height,
        }),
        TilePolicy::Fixed { width: 0, .. } | TilePolicy::Fixed { height: 0, .. } => Err(
            Jp2LamError::InvalidInput("fixed tile dimensions must be non-zero".to_string()),
        ),
        TilePolicy::Fixed { width, height } if width >= image.width && height >= image.height => {
            Ok(TilePlan {
                index: 0,
                width,
                height,
            })
        }
        TilePolicy::Fixed { width, height } => Ok(TilePlan {
            index: 0,
            width,
            height,
        }),
        TilePolicy::Auto => {
            let (width, height) = derive_auto_tile_dimensions(image, &options.resource_limits)?;
            Ok(TilePlan {
                index: 0,
                width: width.min(image.width),
                height: height.min(image.height),
            })
        }
    }
}

/// Maximum common decomposition count that keeps every actual tile's LL
/// interval non-empty. Arbitrary tile origins are allowed; bounds follow the
/// Annex B ceil-division recurrence rather than nominal tile dimensions.
fn max_tile_grid_decompositions(
    image_width: u32,
    image_height: u32,
    tile_width: u32,
    tile_height: u32,
) -> u32 {
    let mut common = u32::MAX;
    let tiles_x = image_width.div_ceil(tile_width);
    let tiles_y = image_height.div_ceil(tile_height);
    for tile_y in 0..tiles_y {
        for tile_x in 0..tiles_x {
            let mut ax = tile_x * tile_width;
            let mut ay = tile_y * tile_height;
            let mut bx = ax.saturating_add(tile_width).min(image_width);
            let mut by = ay.saturating_add(tile_height).min(image_height);
            let mut levels = 0u32;
            while levels < 31 {
                let next_ax = ax.div_ceil(2);
                let next_ay = ay.div_ceil(2);
                let next_bx = bx.div_ceil(2);
                let next_by = by.div_ceil(2);
                if next_ax >= next_bx || next_ay >= next_by {
                    break;
                }
                levels += 1;
                (ax, ay, bx, by) = (next_ax, next_ay, next_bx, next_by);
            }
            common = common.min(levels);
        }
    }
    if common == u32::MAX { 0 } else { common }
}

fn derive_auto_tile_dimensions(
    image: &ImageView<'_>,
    limits: &ResourceLimits,
) -> Result<(u32, u32)> {
    const DEFAULT_EDGE: u32 = 2048;
    const LARGE_EDGE: u32 = 4096;
    const MIN_EDGE: u32 = 64;

    let Some(max_working_memory) = limits.max_working_memory else {
        return Ok((
            DEFAULT_EDGE.min(image.width),
            DEFAULT_EDGE.min(image.height),
        ));
    };

    let mut edge = LARGE_EDGE;
    while edge >= MIN_EDGE {
        let tile_width = edge.min(image.width);
        let tile_height = edge.min(image.height);
        let edge_bytes = estimate_auto_tile_working_bytes(image, tile_width, tile_height)?;
        if edge_bytes <= max_working_memory {
            return Ok((tile_width, tile_height));
        }
        edge /= 2;
    }

    let minimum_width = MIN_EDGE.min(image.width);
    let minimum_height = MIN_EDGE.min(image.height);
    let minimum_bytes = estimate_auto_tile_working_bytes(image, minimum_width, minimum_height)?;
    Err(Jp2LamError::InvalidInput(format!(
        "max_working_memory {max_working_memory} bytes is below the estimated \
         {minimum_bytes} bytes needed for a {minimum_width}x{minimum_height} active tile"
    )))
}

fn estimate_auto_tile_working_bytes(
    _image: &ImageView<'_>,
    tile_width: u32,
    tile_height: u32,
) -> Result<usize> {
    let pixels = usize::try_from(tile_width)
        .ok()
        .and_then(|width| {
            usize::try_from(tile_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| Jp2LamError::InvalidInput("tile pixel count overflow".to_string()))?;
    // The public memory limit governs encoder-owned transient storage. Borrowed
    // source samples remain caller-owned and are deliberately excluded.
    let active_component_work = pixels
        .checked_mul(std::mem::size_of::<i32>())
        .and_then(|plane| plane.checked_mul(2))
        .ok_or_else(|| Jp2LamError::InvalidInput("tile working-memory overflow".to_string()))?;

    Ok(active_component_work)
}

fn validate_resource_limits(limits: &ResourceLimits) -> Result<()> {
    if limits.max_working_memory == Some(0) {
        return Err(Jp2LamError::InvalidInput(
            "max_working_memory must be non-zero when set".to_string(),
        ));
    }
    if limits.max_threads == Some(0) {
        return Err(Jp2LamError::InvalidInput(
            "max_threads must be non-zero when set".to_string(),
        ));
    }
    if limits.encoded_store_memory_limit == Some(0) {
        return Err(Jp2LamError::InvalidInput(
            "encoded_store_memory_limit must be non-zero when set".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Component, EncodeOptions, Image, OutputFormat};

    #[test]
    fn rate_is_monotonic_across_quality_range() {
        let mut prev = f32::INFINITY;
        for q in (0u8..100).step_by(5) {
            let rate = tcp_rate_from_quality(q);
            assert!(rate <= prev, "rate not monotonic at q={q}: {rate} > {prev}");
            prev = rate;
        }
    }

    #[test]
    fn validate_rejects_wrong_component_count() {
        let image = Image {
            width: 1,
            height: 1,
            components: vec![],
            colorspace: ColorSpace::Gray,
        };
        assert!(EncodingPlan::build(&image, &EncodeOptions::default()).is_err());
    }

    #[test]
    fn explicit_rate_semantics_use_complete_output_and_declared_source_bits() {
        let width = 100;
        let height = 80;
        let component = Component {
            data: vec![0; (width * height) as usize],
            width,
            height,
            precision: 12,
            signed: false,
            dx: 1,
            dy: 1,
        };
        let image = Image {
            width,
            height,
            components: vec![component.clone(), component.clone(), component],
            colorspace: ColorSpace::Srgb,
        };

        for rate_control in [
            RateControl::TargetBytes(2_000),
            RateControl::TargetBitsPerPixel(2.0),
            RateControl::CompressionRatio(18.0),
        ] {
            let plan = EncodingPlan::build(
                &image,
                &EncodeOptions {
                    rate_control: Some(rate_control),
                    ..Default::default()
                },
            )
            .expect("explicit rate plan");
            assert_eq!(
                plan.output_rate_target,
                Some(OutputRateTarget::Bytes(2_000))
            );
            assert_eq!(plan.rate_control, rate_control);
            assert!(!plan.is_lossless());
        }
    }

    #[test]
    fn invalid_explicit_rate_values_are_rejected() {
        let image = gray_image(13, 11);
        for rate_control in [
            RateControl::Quality(100),
            RateControl::TargetBytes(0),
            RateControl::TargetBitsPerPixel(f32::NAN),
            RateControl::CompressionRatio(0.0),
        ] {
            let result = EncodingPlan::build(
                &image,
                &EncodeOptions {
                    rate_control: Some(rate_control),
                    ..Default::default()
                },
            );
            assert!(result.is_err(), "accepted {rate_control:?}");
        }
    }

    #[test]
    fn plan_caps_resolution_count() {
        let image = Image {
            width: 8,
            height: 8,
            components: vec![Component {
                data: vec![0; 64],
                width: 8,
                height: 8,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }],
            colorspace: ColorSpace::Gray,
        };
        let plan = EncodingPlan::build(
            &image,
            &EncodeOptions {
                quality: 85,
                format: OutputFormat::Jp2,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("build plan");
        assert!(plan.num_resolutions <= 4);
        assert_eq!(plan.progression_order, ProgressionOrder::Lrcp);
        assert_eq!(plan.code_block_size.width, 64);
        assert_eq!(plan.lane, EncodeLane::GrayLossy);
        assert_eq!(plan.quantization_style, QuantizationStyle::ScalarExpounded);
        assert_eq!(plan.tile.width, 8);
        assert_eq!(plan.components.len(), 1);
    }

    #[test]
    fn single_tile_policy_is_recorded_in_plan() {
        let image = gray_image(13, 11);
        let options = EncodeOptions {
            quality: 100,
            rate_control: None,
            format: OutputFormat::J2k,
            profile: Default::default(),
            tile_policy: TilePolicy::Single,
            resource_limits: ResourceLimits {
                max_working_memory: Some(512 * 1024 * 1024),
                max_threads: Some(2),
                encoded_store_memory_limit: Some(64 * 1024 * 1024),
                spill_directory: None,
            },
            color_encoding: None,
        };

        let plan = EncodingPlan::build(&image, &options).expect("build plan");

        assert_eq!(plan.tile_policy, TilePolicy::Single);
        assert_eq!(plan.tile.width, image.width);
        assert_eq!(plan.tile.height, image.height);
        assert_eq!(plan.resource_limits.max_threads, Some(2));
    }

    #[test]
    fn fixed_multi_tile_policy_is_recorded_in_plan() {
        let image = gray_image(13, 11);
        let options = EncodeOptions {
            quality: 100,
            tile_policy: TilePolicy::Fixed {
                width: 8,
                height: 8,
            },
            ..Default::default()
        };

        let plan = EncodingPlan::build(&image, &options).expect("fixed tile plan");

        assert_eq!(plan.tile.width, 8);
        assert_eq!(plan.tile.height, 8);
        assert_eq!(crate::tiling::tile_grid(&plan).num_tiles(), 4);
    }

    #[test]
    fn fixed_policy_covering_image_is_safe_single_tile() {
        let image = gray_image(13, 11);
        let options = EncodeOptions {
            tile_policy: TilePolicy::Fixed {
                width: 16,
                height: 16,
            },
            ..Default::default()
        };

        let plan = EncodingPlan::build(&image, &options).expect("fixed covering tile");

        assert_eq!(plan.tile_policy, options.tile_policy);
        assert_eq!(plan.tile.width, 16);
        assert_eq!(plan.tile.height, 16);
        assert_eq!(crate::tiling::tile_grid(&plan).num_tiles(), 1);
    }

    #[test]
    fn auto_policy_covering_image_is_safe_single_tile() {
        let image = gray_image(640, 480);
        let options = EncodeOptions {
            tile_policy: TilePolicy::Auto,
            resource_limits: ResourceLimits {
                max_working_memory: Some(32 * 1024 * 1024),
                ..Default::default()
            },
            ..Default::default()
        };

        let plan = EncodingPlan::build(&image, &options).expect("auto full-image tile");

        assert_eq!(plan.tile_policy, TilePolicy::Auto);
        assert_eq!(plan.tile.width, image.width);
        assert_eq!(plan.tile.height, image.height);
        assert_eq!(crate::tiling::tile_grid(&plan).num_tiles(), 1);
    }

    #[test]
    fn auto_policy_rejects_budget_too_small_for_one_active_tile() {
        let image = gray_image(640, 480);
        let options = EncodeOptions {
            tile_policy: TilePolicy::Auto,
            resource_limits: ResourceLimits {
                max_working_memory: Some(1024),
                ..Default::default()
            },
            ..Default::default()
        };

        let err = EncodingPlan::build(&image, &options).expect_err("budget is too small");

        assert!(err.to_string().contains("max_working_memory"));
    }

    #[test]
    fn invalid_resource_limit_is_rejected_explicitly() {
        let image = gray_image(13, 11);
        let options = EncodeOptions {
            resource_limits: ResourceLimits {
                max_threads: Some(0),
                ..Default::default()
            },
            ..Default::default()
        };

        let err = EncodingPlan::build(&image, &options).expect_err("zero threads should fail");

        assert!(err.to_string().contains("max_threads"));
    }

    #[test]
    fn tiny_gray_plan_is_bounded() {
        for (width, height, expected_resolutions) in
            [(2, 2, 2), (3, 2, 2), (3, 3, 2), (5, 3, 2), (17, 19, 5)]
        {
            let image = Image {
                width,
                height,
                components: vec![Component {
                    data: vec![0; (width * height) as usize],
                    width,
                    height,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                }],
                colorspace: ColorSpace::Gray,
            };
            let plan = EncodingPlan::build(
                &image,
                &EncodeOptions {
                    quality: 85,
                    format: OutputFormat::J2k,
                    profile: Default::default(),
                    ..Default::default()
                },
            )
            .expect("build plan");

            assert_eq!(plan.lane, EncodeLane::GrayLossy, "{width}x{height}");
            assert_eq!(
                plan.transform,
                WaveletTransform::Irreversible97,
                "{width}x{height}"
            );
            assert_eq!(
                plan.quantization_style,
                QuantizationStyle::ScalarExpounded,
                "{width}x{height}"
            );
            assert!(!plan.use_mct, "{width}x{height}");
            assert_eq!(
                plan.num_resolutions, expected_resolutions,
                "{width}x{height}"
            );
            assert_eq!(
                plan.decomposition_levels,
                expected_resolutions - 1,
                "{width}x{height}"
            );
            assert!(plan.layers[0].target_rate.is_some(), "{width}x{height}");
        }
    }

    #[test]
    fn lossy_rgb_plan_enables_scalar_expounded_quantization() {
        let image = Image {
            width: 32,
            height: 32,
            components: vec![
                Component {
                    data: vec![0; 1024],
                    width: 32,
                    height: 32,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                },
                Component {
                    data: vec![0; 1024],
                    width: 32,
                    height: 32,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                },
                Component {
                    data: vec![0; 1024],
                    width: 32,
                    height: 32,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                },
            ],
            colorspace: ColorSpace::Srgb,
        };
        let plan = EncodingPlan::build(
            &image,
            &EncodeOptions {
                quality: 42,
                format: OutputFormat::J2k,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("build plan");
        assert_eq!(plan.lane, EncodeLane::RgbLossy);
        assert_eq!(plan.quantization_style, QuantizationStyle::ScalarExpounded);
        assert!(plan.use_mct);
        assert_eq!(plan.components.len(), 3);
        assert_eq!(plan.tile.height, 32);
        assert!(plan.subband_quants[0].mantissa > 0);
    }

    fn gray_image(width: u32, height: u32) -> Image {
        Image {
            width,
            height,
            components: vec![Component {
                data: vec![0; (width * height) as usize],
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }],
            colorspace: ColorSpace::Gray,
        }
    }
}
