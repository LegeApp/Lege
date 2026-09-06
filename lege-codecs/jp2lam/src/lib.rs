mod batch;
pub mod content;
pub mod decode;
mod dwt;
mod encode;
mod error;
mod j2k;
mod jp2;
mod model;
mod mq;
mod perceptual;
mod plan;
mod profile;
mod simd;
mod t2;
mod tier1;
mod tiling;

pub use batch::{
    BatchComponentProfile, BatchDecoder, BatchEncoder, BatchProfile, decode_batch, encode_batch,
};
pub use content::{ContentClass, ContentStats, analyze as analyze_content, auto_quality};
pub use decode::{
    DecodeConcurrency, DecodeIntoInfo, DecodeLimits, DecodeMetadata, DecodeOutputFormat,
    DecodeRegion, DecodeRequest, DecodeResolution, DecodeResult, DecodeTarget, DecodedRaster,
    Jp2DecodeStats, Jp2Decoder, decode_from_reader, decode_from_reader_request, decode_jp2,
    decode_jp2_request, decode_jp2_with_stats, inspect_jp2, inspect_jp2_with_limits,
};
#[cfg(feature = "counters")]
pub use encode::counters::{
    CLEANUP_PASSES, EMPTY_BLOCKS, MQ_SYMBOLS, MR_PASSES, MemoryCounterSnapshot, SP_PASSES,
    TOTAL_BLOCKS, TOTAL_PASS_BYTES, memory_snapshot, print, reset,
};
pub use encode::ssim::{PERCEPTUAL_PROBES, StreamEvaluator, last_achieved_score};
pub use encode::ssim_oracle::{
    OracleConfig, OracleFeatures, OracleLabel, OracleProbe, OracleStatus, OracleSweepResult,
    default_oracle_body_fractions, default_oracle_quant_qualities, default_oracle_targets,
    reduce_labels, sweep_source,
};
pub use encode::{
    EncodeMetrics, clear_timing_data, encode, encode_to_writer, encode_view, encode_view_to_writer,
    encode_with_psnr, print_timing_data,
};
pub use error::{Jp2LamError, Result};
/// Pinned production SSIMULACRA2 identity, shared with JPXL.
///
/// A perceptual encode is only comparable across codecs when both score with
/// this string. Bumping it is an encoder-behaviour change.
pub use jpxl_perceptual::METRIC_VERSION;
pub use model::{
    ColorEncoding, ColorSpace, Component, ComponentSampleData, ComponentView, ContentProfile,
    DisplayColor, DisplayProfile, EncodeOptions, IccComponentModel, Image, ImageView, OutputFormat,
    PerceptualEffort, PerceptualObservation, PerceptualProbe, PerceptualTarget, PerceptualTrace,
    Preset, QualityStatus, RateControl, ResourceLimits, SamplePrecision, TilePolicy,
};
