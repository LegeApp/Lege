//! The image-codec boundary: the ONLY place in the engine where filtered
//! image bytes become pixels.
//!
//! Codecs (DCT/JPEG, JPX/JPEG 2000, JBIG2, CCITT) are developed as separate
//! projects and plug in here as trait objects. The registry is **injected at
//! renderer construction** — never global, per the engine-wide rule — so an
//! engine built without a codec still parses, compiles, schedules, and
//! renders everything else; pages needing the missing codec are routed or
//! degraded via `PageFeatures::NEEDS_*` preflight, decided before any work
//! starts.
//!
//! Swapping an interim decoder (e.g. a crates.io JPEG) for the in-house one
//! later is a one-line change in registry construction.

use std::sync::Arc;

use crate::{DecodeParameters, ImageDescriptor, ImageError, StreamFilter};

/// Pixel layout of a codec's output, before PDF color-space mapping.
///
/// Codecs stop here: `/Decode` arrays, color-space conversion (Indexed,
/// Separation, ICC, ...), and mask/SMask compositing are the renderer's
/// job, applied uniformly to all codecs' output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodedFormat {
    /// 1 bit/pixel, MSB-first rows (JBIG2, CCITT, stencil masks).
    Mono1,
    Gray8,
    /// 8-bit grayscale and opacity (a JPX in-data soft mask on a grayscale
    /// image). Split like [`DecodedFormat::Rgba8`].
    GrayA8,
    Gray16,
    /// Two interleaved 8-bit channels that are *not* a colour space on their
    /// own — the PDF `/ColorSpace` says what they mean (a 2-colorant
    /// `/DeviceN` duotone). Distinct from [`DecodedFormat::GrayA8`], whose
    /// second channel is opacity and is split off.
    Multi2,
    Rgb8,
    /// 8-bit red, green, blue, and opacity (a JPX in-data soft mask). The
    /// renderer splits the opacity plane into an [`crate`]-neutral soft mask.
    Rgba8,
    Cmyk8,
}

impl DecodedFormat {
    /// Bits per pixel.
    pub fn bits_per_pixel(self) -> u32 {
        match self {
            DecodedFormat::Mono1 => 1,
            DecodedFormat::Gray8 => 8,
            DecodedFormat::GrayA8 | DecodedFormat::Gray16 | DecodedFormat::Multi2 => 16,
            DecodedFormat::Rgb8 => 24,
            DecodedFormat::Rgba8 | DecodedFormat::Cmyk8 => 32,
        }
    }
}

/// A decoded raster. Rows are contiguous at `stride` bytes; `stride` may
/// exceed the minimal row width for alignment.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub format: DecodedFormat,
    pub stride: usize,
    pub data: Arc<[u8]>,
}

/// Hard limits every decode must respect (concurrency plan §"Image decoder
/// safety"): decoders must use checked arithmetic against these and fail
/// with [`ImageError::TooLarge`]/[`ImageError::Decode`] rather than
/// allocate unbounded memory. Cancellation between images is the caller's
/// job; long-running codecs should additionally check `should_cancel`.
#[derive(Clone)]
pub struct DecodeLimits {
    pub max_pixels: u64,
    pub max_output_bytes: u64,
    /// Cooperative cancellation probe, checked at coding-pass boundaries.
    /// `None` = never cancelled.
    pub should_cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl std::fmt::Debug for DecodeLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodeLimits")
            .field("max_pixels", &self.max_pixels)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("should_cancel", &self.should_cancel.is_some())
            .finish()
    }
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_pixels: 1 << 28,       // 268 Mpx
            max_output_bytes: 1 << 31, // 2 GiB
            should_cancel: None,
        }
    }
}

impl DecodeLimits {
    pub fn is_cancelled(&self) -> bool {
        self.should_cancel.as_ref().is_some_and(|f| f())
    }

    /// The pixel budget for a codec whose output is `bits_per_pixel` dense.
    ///
    /// `max_pixels` is calibrated for byte-per-sample output, so applying it
    /// unchanged to a 1 bpp codec rejects bilevel images costing an eighth of
    /// the memory an 8-bit image of the same cap would — which is how 600 dpi
    /// JBIG2/CCITT scans (a 496 Mpx page is only 62 MiB packed) ended up
    /// rendering blank while PDFium rendered them fine. Scale the budget by
    /// the output density instead. `max_output_bytes` remains the real memory
    /// guard; this never returns *less* than `max_pixels`, so no codec loses
    /// headroom it has today.
    pub fn max_pixels_at_bpp(&self, bits_per_pixel: u32) -> u64 {
        let bpp = bits_per_pixel.max(1);
        let scaled = self.max_pixels.saturating_mul(8) / u64::from(bpp);
        scaled.max(self.max_pixels)
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    #[test]
    fn bilevel_codecs_get_eight_times_the_pixel_budget() {
        let l = DecodeLimits::default();
        assert_eq!(l.max_pixels_at_bpp(1), l.max_pixels * 8);
        // A 496 Mpx JBIG2 page (pdfbox/4598.pdf) is only 62 MiB packed and must
        // fit; under the unscaled cap it did not, and the page rendered blank.
        assert!(496_739_439 < l.max_pixels_at_bpp(1));
        assert!(496_739_439 > l.max_pixels);
    }

    #[test]
    fn dense_codecs_never_lose_headroom() {
        let l = DecodeLimits::default();
        for bpp in [8, 16, 24, 32, 64] {
            assert_eq!(
                l.max_pixels_at_bpp(bpp),
                l.max_pixels,
                "{bpp} bpp must keep exactly today's budget"
            );
        }
        assert_eq!(
            l.max_pixels_at_bpp(0),
            l.max_pixels * 8,
            "0 treated as 1 bpp"
        );
    }
}

/// One image codec: decodes exactly one [`StreamFilter`]'s bit-stream.
///
/// Implementations must be pure with respect to `&self` — safe for
/// unlimited concurrent decode calls (one call per worker). Any mutable
/// decode state lives on the stack of `decode`; codecs needing shared
/// tables (e.g. JBIG2 globals streams) receive them via `params`, not via
/// interior mutability.
pub trait ImageCodec: Send + Sync + std::fmt::Debug {
    /// The filter this codec implements.
    fn filter(&self) -> StreamFilter;

    /// Decode `data` (the raw bytes after any *general* stream filters such
    /// as Flate have already been applied) into a raster.
    fn decode(
        &self,
        data: &[u8],
        descriptor: &ImageDescriptor,
        params: &DecodeParameters,
        limits: &DecodeLimits,
    ) -> Result<DecodedImage, ImageError>;
}

/// True for filters that require an image codec; false for general byte
/// filters (Flate, LZW, RunLength, ASCII*) handled by the stream-decode
/// layer, and for Crypt (handled by security).
pub fn is_image_codec_filter(filter: StreamFilter) -> bool {
    matches!(
        filter,
        StreamFilter::DctDecode | StreamFilter::Jpx | StreamFilter::Jbig2 | StreamFilter::CcittFax
    )
}

/// The `PageFeatures` requirement flag a filter imposes on a page, if any.
/// The page compiler ORs this over every image's filter chain; renderers
/// advertise the flags their registry covers, and preflight does the rest.
pub fn codec_feature(filter: StreamFilter) -> pdf_page_ir::PageFeatures {
    use pdf_page_ir::PageFeatures as F;
    match filter {
        StreamFilter::DctDecode => F::NEEDS_DCT,
        StreamFilter::Jpx => F::NEEDS_JPX,
        StreamFilter::Jbig2 => F::NEEDS_JBIG2,
        StreamFilter::CcittFax => F::NEEDS_CCITT,
        _ => F::empty(),
    }
}

/// The feature flags a registry satisfies — what a backend adds to its
/// advertised `PageFeatures` for codec coverage.
pub fn registry_features(registry: &CodecRegistry) -> pdf_page_ir::PageFeatures {
    registry
        .supported_filters()
        .fold(pdf_page_ir::PageFeatures::empty(), |acc, f| {
            acc | codec_feature(f)
        })
}

/// The injected set of available codecs. Immutable after construction;
/// cheap to clone and share across workers and backends.
#[derive(Debug, Clone, Default)]
pub struct CodecRegistry {
    codecs: Arc<[Arc<dyn ImageCodec>]>,
}

impl CodecRegistry {
    /// Build from the codecs available in this deployment. Later entries
    /// with a duplicate filter are ignored (first registration wins) so a
    /// default set can be prepended and selectively overridden by ordering.
    pub fn new(codecs: impl IntoIterator<Item = Arc<dyn ImageCodec>>) -> Self {
        let mut seen: Vec<StreamFilter> = Vec::new();
        let mut kept: Vec<Arc<dyn ImageCodec>> = Vec::new();
        for c in codecs {
            let f = c.filter();
            if !seen.contains(&f) {
                seen.push(f);
                kept.push(c);
            }
        }
        Self {
            codecs: kept.into(),
        }
    }

    /// An empty registry: every codec-requiring page gets declined at
    /// preflight. This is a fully functional deployment for text/vector
    /// documents.
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn get(&self, filter: StreamFilter) -> Option<&dyn ImageCodec> {
        self.codecs
            .iter()
            .find(|c| c.filter() == filter)
            .map(|c| c.as_ref())
    }

    pub fn supports(&self, filter: StreamFilter) -> bool {
        self.get(filter).is_some()
    }

    pub fn supported_filters(&self) -> impl Iterator<Item = StreamFilter> + '_ {
        self.codecs.iter().map(|c| c.filter())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const fn assert_send_sync<T: Send + Sync>() {}
    const _: () = assert_send_sync::<CodecRegistry>();
    const _: () = assert_send_sync::<DecodeLimits>();

    #[derive(Debug)]
    struct FakeCodec(StreamFilter);

    impl ImageCodec for FakeCodec {
        fn filter(&self) -> StreamFilter {
            self.0
        }
        fn decode(
            &self,
            _data: &[u8],
            descriptor: &ImageDescriptor,
            _params: &DecodeParameters,
            _limits: &DecodeLimits,
        ) -> Result<DecodedImage, ImageError> {
            let stride = descriptor.width as usize;
            Ok(DecodedImage {
                width: descriptor.width,
                height: descriptor.height,
                format: DecodedFormat::Gray8,
                stride,
                data: Arc::from(vec![0u8; stride * descriptor.height as usize]),
            })
        }
    }

    #[test]
    fn registry_lookup_and_duplicate_policy() {
        let reg = CodecRegistry::new([
            Arc::new(FakeCodec(StreamFilter::DctDecode)) as Arc<dyn ImageCodec>,
            Arc::new(FakeCodec(StreamFilter::CcittFax)),
            // Duplicate: ignored, first wins.
            Arc::new(FakeCodec(StreamFilter::DctDecode)),
        ]);
        assert!(reg.supports(StreamFilter::DctDecode));
        assert!(reg.supports(StreamFilter::CcittFax));
        assert!(!reg.supports(StreamFilter::Jbig2));
        assert_eq!(reg.supported_filters().count(), 2);
    }

    #[test]
    fn empty_registry_supports_nothing() {
        let reg = CodecRegistry::empty();
        assert!(!reg.supports(StreamFilter::DctDecode));
        assert_eq!(reg.supported_filters().count(), 0);
    }

    #[test]
    fn codec_filter_classification() {
        assert!(is_image_codec_filter(StreamFilter::Jbig2));
        assert!(is_image_codec_filter(StreamFilter::Jpx));
        assert!(!is_image_codec_filter(StreamFilter::FlateDecode));
        assert!(!is_image_codec_filter(StreamFilter::Crypt));
    }

    #[test]
    fn registry_features_reflect_registered_codecs() {
        use pdf_page_ir::PageFeatures as F;
        let reg = CodecRegistry::new([
            Arc::new(FakeCodec(StreamFilter::DctDecode)) as Arc<dyn ImageCodec>,
            Arc::new(FakeCodec(StreamFilter::Jbig2)),
        ]);
        let features = registry_features(&reg);
        assert_eq!(features, F::NEEDS_DCT | F::NEEDS_JBIG2);
        // A page needing JPX is not covered by this registry.
        let page_needs = F::NEEDS_JPX | F::NEEDS_DCT;
        assert_eq!(page_needs.difference(features), F::NEEDS_JPX);
        // Flate imposes no codec requirement.
        assert_eq!(codec_feature(StreamFilter::FlateDecode), F::empty());
    }
}
