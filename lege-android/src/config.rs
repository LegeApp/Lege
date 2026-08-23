//! Java parameters to [`PipelineConfig`].
//!
//! The host passes a `LegeParams` object rather than a long argument list, so
//! adding a knob is a field on both sides instead of a signature change that
//! breaks every existing caller.
//!
//! ```java
//! package com.legeapp.lege;
//!
//! public final class LegeParams {
//!     public int     targetHeight;            // px; 0 = leave at default
//!     public boolean enableLayoutDetection;
//!     public boolean enableOcr;
//!     public boolean highQualityOutput;
//!     public boolean invertInput;
//!     public String  pageRange;               // "5" or "1-20"; null = whole document
//!     public String  marginMode;              // "none" | "center" | "crop" | "reflow"
//!     public boolean slowOcr, jpegCompat, ditherImages;
//!     public String  binarizationMode;        // "default" | "adaptive" | "threshold" | "sauvola"
//!     public float   sauvolaK;                // 0.0–1.0
//!     public int     fixedThreshold;          // 0–255
//!     public String  epubSidecarPath;         // null = no EPUB companion
//! }
//! ```
//!
//! Only a deliberate subset of the ~45 available setters is exposed. The rest
//! keep their defaults, which is what a reader UI wants; widening the surface
//! later is additive.

use anyhow::{Context, Result};
use jni::JNIEnv;
use jni::objects::{JObject, JString};

use lege::PipelineConfig;
use lege::color::BinarizationConfig;
use lege::margin::MarginSettings;
use lege::pipeline::PageRange;
use std::path::PathBuf;

/// Read a `String` field, mapping Java `null` to `None`.
fn string_field(env: &mut JNIEnv<'_>, params: &JObject<'_>, name: &str) -> Result<Option<String>> {
    let value = env
        .get_field(params, name, "Ljava/lang/String;")
        .with_context(|| format!("LegeParams.{name} is missing or not a String"))?
        .l()
        .with_context(|| format!("LegeParams.{name} is not an object"))?;

    if value.is_null() {
        return Ok(None);
    }

    let text: String = env
        .get_string(&JString::from(value))
        .with_context(|| format!("LegeParams.{name} is not readable"))?
        .into();

    // A blank string is the same intent as null — "not set" — and is far
    // easier to produce accidentally from a UI text field.
    Ok(Some(text).filter(|text| !text.trim().is_empty()))
}

fn int_field(env: &mut JNIEnv<'_>, params: &JObject<'_>, name: &str) -> Result<i32> {
    env.get_field(params, name, "I")
        .with_context(|| format!("LegeParams.{name} is missing or not an int"))?
        .i()
        .with_context(|| format!("LegeParams.{name} is not an int"))
}

fn float_field(env: &mut JNIEnv<'_>, params: &JObject<'_>, name: &str) -> Result<f32> {
    env.get_field(params, name, "F")
        .with_context(|| format!("LegeParams.{name} is missing or not a float"))?
        .f()
        .with_context(|| format!("LegeParams.{name} is not a float"))
}

fn bool_field(env: &mut JNIEnv<'_>, params: &JObject<'_>, name: &str) -> Result<bool> {
    env.get_field(params, name, "Z")
        .with_context(|| format!("LegeParams.{name} is missing or not a boolean"))?
        .z()
        .with_context(|| format!("LegeParams.{name} is not a boolean"))
}

/// Build a pipeline configuration from the host's parameters.
///
/// Starts from [`PipelineConfig::new`] rather than `Default`, which panics on
/// error instead of reporting it — an unwanted trait at an FFI boundary.
pub(crate) fn from_java(env: &mut JNIEnv<'_>, params: &JObject<'_>) -> Result<PipelineConfig> {
    let mut config = PipelineConfig::new().context("failed to build a default pipeline config")?;

    let target_height = int_field(env, params, "targetHeight")?;
    if target_height > 0 {
        config
            .set_target_height(target_height as u32)
            .context("invalid targetHeight")?;
    }

    config.set_enable_layout_detection(bool_field(env, params, "enableLayoutDetection")?);
    let enable_ocr = bool_field(env, params, "enableOcr")?;
    config.set_enable_ocr(enable_ocr);
    config.set_high_quality_output(bool_field(env, params, "highQualityOutput")?);
    config.set_invert_input(bool_field(env, params, "invertInput")?);
    config.set_slow_ocr(bool_field(env, params, "slowOcr")?);
    // Desktop treats document analysis as part of OCR: OCR produces the
    // searchable overlay plus its inferred outline/metadata.
    config.set_enable_auto_toc(enable_ocr);
    config.set_jpeg_compat(bool_field(env, params, "jpegCompat")?);
    // Android's crop mode follows the desktop's crop choice: it always uses
    // content's natural aspect rather than presenting a second mobile toggle.
    config.set_crop_free_aspect(true);
    let dither_images = bool_field(env, params, "ditherImages")?;
    config.set_dither_images(dither_images);
    config.set_keep_original_images(!dither_images);

    let binarization_mode =
        string_field(env, params, "binarizationMode")?.unwrap_or_else(|| "default".into());
    let mut binarization = BinarizationConfig::default();
    match binarization_mode.as_str() {
        "default" | "adaptive" => {
            binarization.k_factor = float_field(env, params, "sauvolaK")?;
        }
        "threshold" => {
            binarization.use_fixed_threshold = true;
            binarization.fixed_threshold = u8::try_from(int_field(env, params, "fixedThreshold")?)
                .context("fixedThreshold must be between 0 and 255")?;
        }
        "sauvola" => binarization.use_heavy_duty = true,
        _ => anyhow::bail!("unsupported binarizationMode `{binarization_mode}`"),
    }
    if !(0.0..=1.0).contains(&binarization.k_factor) {
        anyhow::bail!("sauvolaK must be between 0.0 and 1.0");
    }
    config.set_binarization(binarization);

    let margin_mode = string_field(env, params, "marginMode")?.unwrap_or_else(|| "none".into());
    let margin = match margin_mode.as_str() {
        "none" => MarginSettings::None,
        "center" => MarginSettings::StandardizeAndCenter,
        "crop" => MarginSettings::CropAndResize,
        "reflow" => MarginSettings::None,
        _ => anyhow::bail!("unsupported marginMode `{margin_mode}`"),
    };
    config.set_margin_settings(margin);
    config.set_enable_reflow(margin_mode == "reflow");

    if let Some(path) = string_field(env, params, "epubSidecarPath")? {
        config.set_epub_sidecar_output(Some(PathBuf::from(path)));
    }

    if let Some(range) = string_field(env, params, "pageRange")? {
        let parsed =
            PageRange::parse(&range).with_context(|| format!("invalid pageRange `{range}`"))?;
        // The desktop workflow lets a range determine whether a source cover
        // is in scope. If processing starts after page one, do not preserve a
        // separate cover outside the requested range.
        config.set_no_cover_page(parsed.start != 1);
        config.set_page_range(Some(parsed));
    }

    config.validate().context("pipeline config is not valid")?;
    Ok(config)
}
