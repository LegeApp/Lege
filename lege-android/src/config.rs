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
//!     public String  textFormat;              // "jbig2" | "ccitt4" | …; null = default
//!     public boolean enableLayoutDetection;
//!     public boolean enableOcr;
//!     public String  ocrLanguage;             // null = default
//!     public boolean highQualityOutput;
//!     public boolean enableCoverPage;
//!     public boolean invertInput;
//!     public String  pageRange;               // "5" or "1-20"; null = whole document
//!     public int     maxParallelPages;        // 0 = size from device automatically
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
use lege::pipeline::PageRange;

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

    if let Some(format) = string_field(env, params, "textFormat")? {
        config
            .set_text_format(&format)
            .with_context(|| format!("unsupported textFormat `{format}`"))?;
    }

    let target_height = int_field(env, params, "targetHeight")?;
    if target_height > 0 {
        config
            .set_target_height(target_height as u32)
            .context("invalid targetHeight")?;
    }

    config.set_enable_layout_detection(bool_field(env, params, "enableLayoutDetection")?);
    config.set_enable_ocr(bool_field(env, params, "enableOcr")?);
    config.set_high_quality_output(bool_field(env, params, "highQualityOutput")?);
    config.set_enable_cover_page(bool_field(env, params, "enableCoverPage")?);
    config.set_invert_input(bool_field(env, params, "invertInput")?);

    if let Some(language) = string_field(env, params, "ocrLanguage")? {
        config
            .set_ocr_language(&language)
            .with_context(|| format!("unsupported ocrLanguage `{language}`"))?;
    }

    if let Some(range) = string_field(env, params, "pageRange")? {
        let parsed =
            PageRange::parse(&range).with_context(|| format!("invalid pageRange `{range}`"))?;
        config.set_page_range(Some(parsed));
    }

    // 0 means "decide from the device". Leaving it unset lets
    // `PipelineRuntimeLimits` derive the count from cores and the memory
    // budget reported by `android::available_ram_gb`, which is the right
    // answer far more often than a hardcoded UI value.
    let max_parallel = int_field(env, params, "maxParallelPages")?;
    if max_parallel > 0 {
        config
            .set_max_parallel_pages(Some(max_parallel as usize))
            .context("invalid maxParallelPages")?;
    }

    config.validate().context("pipeline config is not valid")?;
    Ok(config)
}
