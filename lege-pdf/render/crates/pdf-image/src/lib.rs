//! Image XObject semantics: descriptors and decode parameters.
//!
//! Deliberately residency-free (roadmap §8.4): an image here is *what the
//! PDF says*, not decoded pixels. CPU rasters and GPU textures live in
//! backend caches keyed by [`pdf_object::ObjectId`]-derived resource keys.
//! Decoders (Phase 2) each get explicit limits: pixel count, decompressed
//! bytes, checked arithmetic, cancellation.

use pdf_color::ColorSpaceDesc;
use pdf_object::ObjectId;

pub mod ccitt;
pub mod codec;
pub mod jbig2;
pub mod jpeg;
pub mod jpx;

pub use ccitt::{CcittCodec, CcittParams};
pub use codec::{
    CodecRegistry, DecodeLimits, DecodedFormat, DecodedImage, ImageCodec, codec_feature,
    is_image_codec_filter, registry_features,
};
pub use jbig2::Jbig2Codec;
pub use jpeg::JpegCodec;
pub use jpx::JpxCodec;

/// Stream filters applicable to image data (and general streams).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFilter {
    FlateDecode,
    LzwDecode,
    RunLengthDecode,
    AsciiHexDecode,
    Ascii85Decode,
    DctDecode, // JPEG
    Jpx,       // JPEG 2000
    Jbig2,
    CcittFax,
    Crypt,
}

/// Semantic descriptor of an image XObject or inline image.
#[derive(Debug, Clone)]
pub struct ImageDescriptor {
    pub width: u32,
    pub height: u32,
    pub bits_per_component: u8,
    pub color_space: Option<ColorSpaceDesc>,
    /// `/ImageMask true` — 1-bit stencil painted with the current fill.
    pub is_mask: bool,
    /// `/Interpolate`.
    pub interpolate: bool,
    /// Filter chain in application order.
    pub filters: Vec<StreamFilter>,
    /// Source object; `None` for inline images.
    pub object: Option<ObjectId>,
}

/// `/Decode` array and related sampling parameters.
#[derive(Debug, Clone, Default)]
pub struct DecodeParameters {
    /// Per-component [min, max] remap pairs, if non-default.
    pub decode: Option<Vec<[f64; 2]>>,
    /// `/DecodeParms /JBIG2Globals` — the shared segment stream (symbol
    /// dictionaries) referenced by an embedded JBIG2 image.
    pub jbig2_globals: Option<std::sync::Arc<[u8]>>,
    /// `/DecodeParms` for a CCITT image (`/K`, `/Columns`, `/BlackIs1`, …).
    pub ccitt: Option<ccitt::CcittParams>,
    /// Destination footprint of this draw in device pixels, when known. A hint,
    /// not a limit: a codec that supports resolution scaling (JPX) may decode a
    /// minified draw at a reduced resolution instead of full size. Codecs that
    /// do not scale ignore it. `None` means decode at full resolution.
    pub target_size: Option<(u32, u32)>,
    // Later: remaining filter-specific parms (K, Columns, BlackIs1, ...).
}

/// Soft-mask / stencil relationship attached to an image.
#[derive(Debug, Clone)]
pub enum ImageMask {
    /// `/SMask` — grayscale alpha image.
    Soft(ObjectId),
    /// `/Mask` as stencil image.
    Stencil(ObjectId),
    /// `/Mask` as color-key ranges.
    ColorKey(Vec<[u32; 2]>),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ImageError {
    #[error("image dimensions {width}x{height} exceed pixel limit")]
    TooLarge { width: u32, height: u32 },
    #[error("unsupported filter {0:?}")]
    UnsupportedFilter(StreamFilter),
    #[error("malformed image dictionary: {0}")]
    Malformed(&'static str),
    #[error("decode failure: {0}")]
    Decode(String),
}
