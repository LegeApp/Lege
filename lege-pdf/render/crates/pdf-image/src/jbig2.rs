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

use std::sync::Arc;

use jbig2enc_rust::decode::{DecodeError, DecodeOptions, DecodeStrictness, decode_embedded};
use jbig2enc_rust::shared::limits::DecodeLimits as Jbig2Limits;

use crate::codec::{DecodeLimits, DecodedFormat, DecodedImage, ImageCodec};
use crate::{DecodeParameters, ImageDescriptor, ImageError, StreamFilter};

/// The `/JBIG2Decode` codec for a [`crate::CodecRegistry`].
#[derive(Debug, Default)]
pub struct Jbig2Codec;

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
        let globals = params.jbig2_globals.as_deref();

        // Push our codec-level pixel budget down into the decoder so a
        // malformed stream fails before it allocates, rather than after.
        // Compatible strictness: wild PDFs carry malformed-but-recoverable
        // streams; the decoder's documented recoveries apply instead of
        // strict rejection (its recovery log is internal to the decode).
        // JBIG2 output is bilevel, so the pixel budget scales with the 1 bpp
        // output density (see `max_pixels_at_bpp`).
        let px_budget = limits.max_pixels_at_bpp(1);
        let options = DecodeOptions {
            limits: Jbig2Limits {
                max_page_pixels: px_budget,
                max_region_pixels: px_budget,
                // Real scans encode a page-sized generic region as one symbol
                // (600 dpi A4 is ~31 Mpx), which the decoder's own 16 Mpx
                // symbol default rejects as malformed. PDFium has no such
                // symbol-specific cap, so our budget is the only one that
                // should apply here — otherwise the page renders blank.
                max_symbol_pixels: px_budget,
                max_total_dictionary_pixels: px_budget,
                ..Jbig2Limits::default()
            },
            strictness: DecodeStrictness::Compatible,
        };

        let bitmap = decode_embedded(globals, data, &options).map_err(map_err)?;

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
        for b in &mut data {
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
        let img = Jbig2Codec
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
    }

    #[test]
    fn garbage_is_a_typed_error_not_a_panic() {
        let err = Jbig2Codec.decode(
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
        let err = Jbig2Codec.decode(
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
        let out = Jbig2Codec
            .decode(
                JBIG2_L_12X5,
                &descriptor(),
                &DecodeParameters::default(),
                &limits,
            )
            .expect("60 px is over max_pixels=8 but inside the 64 px bilevel budget");
        assert_eq!((out.width, out.height), (12, 5));
    }
}
