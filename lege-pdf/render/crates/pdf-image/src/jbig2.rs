//! `/JBIG2Decode` — adapter over our in-house JBIG2 decoder (`jbig2enc-rust`,
//! decode half).
//!
//! JBIG2 is what archive.org's MRC scans use for the foreground mask, so a
//! decoder is needed before those documents render at all. The decoder is the
//! decode half of the same crate that holds our JBIG2 *encoder*; the renderer
//! depends on it `default-features = false, features = ["decode"]`, so the
//! encoder and its heavier dependencies are gated out of this build.
//!
//! The dependency is confined to this file and one registry entry, so the
//! codec seam stays swappable: replacing the decoder means changing
//! [`Jbig2Codec`]'s body, with no change anywhere else in the engine.
//!
//! # Polarity
//! JBIG2 codes **1 = black**; PDF image samples are DeviceGray, where
//! **0 = black**. The filter therefore inverts, matching every PDF
//! implementation (ISO 32000-1 §7.4.7). This is what makes an MRC mask work:
//! archive.org marks foreground text white in the source bitmap, so it codes
//! as JBIG2 0, inverts to sample 1, and yields alpha 1 (opaque text) when the
//! mask is used as an `/SMask`.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use jbig2enc_rust::decode::{
    DecodeError, DecodeOptions, DecodeStrictness, DecodedGlobals, DecoderContext,
    decode_embedded_with_globals, decode_globals,
};
use jbig2enc_rust::shared::limits::DecodeLimits as Jbig2Limits;

use crate::codec::{DecodeLimits, DecodedFormat, DecodedImage, ImageCodec};
use crate::{DecodeParameters, ImageDescriptor, ImageError, StreamFilter};

// Decoder scratch is internal implementation state, not a process-global
// document cache.
thread_local! {
    /// Decoder scratch belongs to one render worker and is reused across page
    /// streams. This mirrors the JPX adapter's worker-local session and avoids
    /// reallocating arithmetic contexts and region buffers for every page.
    static JBIG2_CONTEXT: RefCell<DecoderContext> = RefCell::new(DecoderContext::new());
}

const MAX_CACHED_GLOBALS_BYTES: usize = 32 * 1024 * 1024;

struct CachedGlobals {
    encoded: Arc<[u8]>,
    limits: Jbig2Limits,
    decoded: Arc<DecodedGlobals>,
}

/// The `/JBIG2Decode` codec for a [`crate::CodecRegistry`].
///
/// One immutable decoded `/JBIG2Globals` entry is shared by all workers using
/// this registry. A one-entry cache is deliberate: a renderer normally serves
/// one active document, it bounds retained memory, and replacing the entry is
/// cheaper than an unbounded document-keyed map.
#[derive(Default)]
pub struct Jbig2Codec {
    globals_cache: Mutex<Option<CachedGlobals>>,
}

impl std::fmt::Debug for Jbig2Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jbig2Codec")
            .field(
                "globals_cached",
                &lock_unpoisoned(&self.globals_cache).is_some(),
            )
            .finish()
    }
}

impl ImageCodec for Jbig2Codec {
    fn filter(&self) -> StreamFilter {
        StreamFilter::Jbig2
    }

    fn decode(
        &self,
        data: &[u8],
        _descriptor: &ImageDescriptor,
        params: &DecodeParameters,
        limits: &DecodeLimits,
    ) -> Result<DecodedImage, ImageError> {
        // PDF embeds a bare JBIG2 segment stream (no file header); the
        // optional `/JBIG2Globals` stream carries shared symbol dictionaries.
        let globals = params
            .jbig2_globals
            .as_ref()
            .filter(|globals| !globals.is_empty());
        let input_bytes = data
            .len()
            .checked_add(globals.map_or(0, |g| g.len()))
            .ok_or_else(|| ImageError::Decode("JBIG2: input size overflow".into()))?;
        limits.check_input(input_bytes)?;

        // Push our codec-level pixel budget down into the decoder so a
        // malformed stream fails before it allocates, rather than after.
        // Compatible strictness: wild PDFs carry malformed-but-recoverable
        // streams; the decoder's documented recoveries apply instead of
        // strict rejection (its recovery log is internal to the decode).
        // JBIG2 output is bilevel, so the pixel budget scales with the 1 bpp
        // output density (see `max_pixels_at_bpp`).
        let px_budget = limits.max_pixels_at_bpp(1);
        let options = DecodeOptions {
            limits: decoder_limits(limits, px_budget),
            strictness: DecodeStrictness::Compatible,
        };

        let decoded_globals = globals
            .map(|bytes| self.decode_globals_cached(bytes, &options))
            .transpose()?;
        let bitmap = JBIG2_CONTEXT
            .with(|ctx| {
                decode_embedded_with_globals(
                    decoded_globals.as_deref(),
                    data,
                    &options,
                    &mut ctx.borrow_mut(),
                )
            })
            .map_err(map_err)?;
        if limits.is_cancelled() {
            return Err(ImageError::Cancelled);
        }

        let (width, height) = (bitmap.width(), bitmap.height());
        if width == 0 || height == 0 {
            return Err(ImageError::Decode("JBIG2: zero dimension".into()));
        }
        if u64::from(width) * u64::from(height) > px_budget {
            return Err(ImageError::TooLarge { width, height });
        }

        // The decoder's MSB-first byte view is exactly the packed Mono1 layout
        // the engine wants: `ceil(width / 8)` bytes per row, MSB = leftmost
        // pixel. Its `set bit = black` polarity is the opposite of PDF's, so
        // every byte is inverted here (black -> sample 0, white -> sample 1).
        let packed = bitmap.as_msb_bytes();
        let stride = packed.bytes_per_row();
        let bytes = stride
            .checked_mul(height as usize)
            .ok_or_else(|| ImageError::Decode("JBIG2: size overflow".into()))?;
        if bytes as u64 > limits.max_output_bytes {
            return Err(ImageError::TooLarge { width, height });
        }

        let mut data = packed.to_vec();
        for (index, b) in data.iter_mut().enumerate() {
            if index & 0xffff == 0 && limits.is_cancelled() {
                return Err(ImageError::Cancelled);
            }
            *b = !*b;
        }

        Ok(DecodedImage {
            width,
            height,
            format: DecodedFormat::Mono1,
            stride,
            data: Arc::from(data),
        })
    }
}

impl Jbig2Codec {
    /// Construct a codec with an empty bounded globals cache.
    pub fn new() -> Self {
        Self::default()
    }

    fn decode_globals_cached(
        &self,
        encoded: &Arc<[u8]>,
        options: &DecodeOptions,
    ) -> Result<Arc<DecodedGlobals>, ImageError> {
        {
            let cache = lock_unpoisoned(&self.globals_cache);
            if let Some(cached) = cache.as_ref()
                && cached.limits == options.limits
                && (Arc::ptr_eq(&cached.encoded, encoded)
                    || cached.encoded.as_ref() == encoded.as_ref())
            {
                return Ok(Arc::clone(&cached.decoded));
            }
        }

        // Decode outside the lock. Concurrent first-page misses may duplicate
        // this work once, but page decodes never serialize behind the cache.
        let decoded = Arc::new(decode_globals(encoded, options).map_err(map_err)?);
        let retained = decoded
            .retained_bytes()
            .checked_add(encoded.len())
            .unwrap_or(usize::MAX);
        let caller_budget = options.limits.max_retained_bytes;
        if retained <= caller_budget.min(MAX_CACHED_GLOBALS_BYTES) {
            *lock_unpoisoned(&self.globals_cache) = Some(CachedGlobals {
                encoded: Arc::clone(encoded),
                limits: options.limits,
                decoded: Arc::clone(&decoded),
            });
        }
        Ok(decoded)
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn decoder_limits(limits: &DecodeLimits, px_budget: u64) -> Jbig2Limits {
    let defaults = Jbig2Limits::default();
    let retained_byte_budget = usize::try_from(limits.max_working_bytes)
        .unwrap_or(usize::MAX)
        .min(defaults.max_retained_bytes);
    Jbig2Limits {
        max_page_pixels: px_budget,
        max_region_pixels: px_budget,
        // Real scans encode a page-sized generic region as one symbol
        // (600 dpi A4 is ~31 Mpx), which the decoder's own 16 Mpx
        // symbol default rejects as malformed. PDFium has no such
        // symbol-specific cap, so our budget is the only one that
        // should apply here — otherwise the page renders blank.
        max_symbol_pixels: px_budget,
        max_total_dictionary_pixels: px_budget,
        // Retained dictionaries, regions and arithmetic contexts are part of
        // the renderer's decode working set, not just the final page.
        max_retained_bytes: retained_byte_budget,
        ..defaults
    }
}

/// Map a decoder error into the codec's error, keeping resource-limit failures
/// reported distinctly from malformed input.
fn map_err(e: DecodeError) -> ImageError {
    match &e {
        DecodeError::Limit(_) => ImageError::Decode(format!("JBIG2: limit exceeded: {e}")),
        _ => ImageError::Decode(format!("JBIG2: {e}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// A 12×5 embedded JBIG2 generic-region stream (no file header, no globals)
    /// encoding an 'L' of black pixels: column 0 for all five rows, plus the
    /// bottom row's first six pixels. Produced by `encode_document_pdf_split`
    /// (generic config) from our own encoder — the exact bytes a PDF would
    /// carry in a `/JBIG2Decode` stream.
    const JBIG2_L_12X5: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x30, 0x00, 0x01, 0x00, 0x00, 0x00, 0x13, 0x00, 0x00, 0x00, 0x0c,
        0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x01, 0x2c, 0x00, 0x00, 0x01, 0x2c, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x02, 0x26, 0x00, 0x01, 0x00, 0x00, 0x00, 0x22, 0x00, 0x00, 0x00, 0x0c,
        0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03,
        0xff, 0xfd, 0xff, 0x02, 0xfe, 0xfe, 0xfe, 0xd2, 0x47, 0xc5, 0xc3, 0x1b, 0xff, 0xff, 0xac,
    ];

    /// Expected packed Mono1 output (stride 2, MSB-first) after the codec's
    /// polarity inversion: black -> sample bit 0, white -> bit 1. Column-0
    /// black clears bit 7 (`0x7f`); the bottom row's six black pixels clear the
    /// top six bits (`0x03`). Trailing 4 padding bits are 1.
    const EXPECT_L: &[u8] = &[0x7f, 0xff, 0x7f, 0xff, 0x7f, 0xff, 0x7f, 0xff, 0x03, 0xff];

    fn descriptor() -> ImageDescriptor {
        ImageDescriptor {
            width: 12,
            height: 5,
            bits_per_component: 1,
            color_space: None,
            is_mask: false,
            interpolate: false,
            filters: vec![StreamFilter::Jbig2],
            object: None,
        }
    }

    #[test]
    fn decodes_embedded_generic_region_with_pdf_polarity() {
        let codec = Jbig2Codec::new();
        let img = codec
            .decode(
                JBIG2_L_12X5,
                &descriptor(),
                &DecodeParameters::default(),
                &DecodeLimits::default(),
            )
            .expect("decode L fixture");

        assert_eq!((img.width, img.height), (12, 5));
        assert_eq!(img.format, DecodedFormat::Mono1);
        assert_eq!(img.stride, 2);
        assert_eq!(&*img.data, EXPECT_L);

        // A worker-local DecoderContext is reset and safely reusable.
        let again = codec
            .decode(
                JBIG2_L_12X5,
                &descriptor(),
                &DecodeParameters::default(),
                &DecodeLimits::default(),
            )
            .expect("repeat decode L fixture");
        assert_eq!(again.data, img.data);
    }

    #[test]
    fn cancellation_wins_over_malformed_input() {
        let limits = DecodeLimits {
            should_cancel: Some(Arc::new(|| true)),
            ..DecodeLimits::default()
        };
        let result = Jbig2Codec::new().decode(
            b"not a jbig2 stream",
            &descriptor(),
            &DecodeParameters::default(),
            &limits,
        );
        assert!(matches!(result, Err(ImageError::Cancelled)));
    }

    #[test]
    fn garbage_is_a_typed_error_not_a_panic() {
        let err = Jbig2Codec::default().decode(
            b"not a jbig2 stream at all",
            &descriptor(),
            &DecodeParameters::default(),
            &DecodeLimits::default(),
        );
        assert!(matches!(err, Err(ImageError::Decode(_))));
    }

    #[test]
    fn pixel_limit_is_enforced() {
        // JBIG2 is bilevel, so the budget is `max_pixels * 8` (one bit per
        // pixel — see `DecodeLimits::max_pixels_at_bpp`). 4 * 8 = 32 px still
        // rejects this 12×5 = 60 px page.
        let tight = DecodeLimits {
            max_pixels: 4,
            ..DecodeLimits::default()
        };
        let err = Jbig2Codec::default().decode(
            JBIG2_L_12X5,
            &descriptor(),
            &DecodeParameters::default(),
            &tight,
        );
        assert!(matches!(
            err,
            Err(ImageError::Decode(_)) | Err(ImageError::TooLarge { .. })
        ));
    }

    /// The bilevel budget is what lets 600 dpi scans through: a page over
    /// `max_pixels` but inside `max_pixels * 8` must decode, not blank out.
    #[test]
    fn bilevel_budget_admits_a_page_over_the_raw_pixel_cap() {
        let limits = DecodeLimits {
            max_pixels: 8,
            ..DecodeLimits::default()
        };
        let out = Jbig2Codec::default()
            .decode(
                JBIG2_L_12X5,
                &descriptor(),
                &DecodeParameters::default(),
                &limits,
            )
            .expect("60 px is over max_pixels=8 but inside the 64 px bilevel budget");
        assert_eq!((out.width, out.height), (12, 5));
    }

    #[test]
    fn renderer_byte_ceiling_limits_retained_decoder_state() {
        let limits = DecodeLimits {
            max_output_bytes: 8192,
            max_working_bytes: 4096,
            ..DecodeLimits::default()
        };
        let mapped = decoder_limits(&limits, limits.max_pixels_at_bpp(1));
        assert_eq!(mapped.max_retained_bytes, 4096);

        let loose = decoder_limits(&DecodeLimits::default(), u64::MAX);
        assert_eq!(
            loose.max_retained_bytes,
            Jbig2Limits::default().max_retained_bytes
        );
    }
}
