#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! The `/JPXDecode` codec adapter over jp2lam.
//!
//! jp2lam ships an encoder, so these round-trip through it rather than
//! committing fixtures: what is under test here is the *adapter* — the
//! planar-to-interleaved repack, the precision scaling, and the
//! codestream-wins-over-dictionary geometry rule — not jp2lam's codec, which
//! has its own suite (and is verified bit-exact against openjpeg separately).

use pdf_image::{
    DecodeLimits, DecodeParameters, DecodedFormat, ImageCodec, ImageDescriptor, JpxCodec,
    StreamFilter,
};

fn descriptor(width: u32, height: u32) -> ImageDescriptor {
    ImageDescriptor {
        width,
        height,
        bits_per_component: 8,
        color_space: None,
        is_mask: false,
        interpolate: false,
        filters: vec![StreamFilter::Jpx],
        object: None,
    }
}

/// Lossless so the round-trip is exact and any adapter error shows up as a
/// pixel difference rather than as codec noise.
fn lossless() -> jp2lam::EncodeOptions {
    jp2lam::EncodeOptions { quality: 100, ..Default::default() }
}

#[test]
fn rgb_jp2_decodes_interleaved() {
    let (w, h) = (16u32, 8u32);
    let mut src = Vec::new();
    for y in 0..h {
        for x in 0..w {
            src.extend_from_slice(&[(x * 16) as u8, (y * 32) as u8, 128]);
        }
    }
    let image = jp2lam::Image::from_rgb_bytes(w, h, &src).expect("build image");
    let jp2 = jp2lam::encode(&image, &lossless()).expect("encode");

    let out = JpxCodec
        .decode(&jp2, &descriptor(w, h), &DecodeParameters::default(), &DecodeLimits::default())
        .expect("decode");

    assert_eq!((out.width, out.height), (w, h));
    assert_eq!(out.format, DecodedFormat::Rgb8);
    assert_eq!(out.stride, w as usize * 3);
    assert_eq!(&*out.data, &src[..], "interleaved RGB round-trips exactly");
}

#[test]
fn codestream_geometry_wins_over_the_dictionary() {
    // A PDF dictionary that disagrees with the codestream must not win: the
    // codec reports what it actually decoded.
    let (w, h) = (16u32, 8u32);
    let src = vec![7u8; (w * h * 3) as usize];
    let image = jp2lam::Image::from_rgb_bytes(w, h, &src).expect("build image");
    let jp2 = jp2lam::encode(&image, &lossless()).expect("encode");

    let lying = descriptor(999, 999);
    let out = JpxCodec
        .decode(&jp2, &lying, &DecodeParameters::default(), &DecodeLimits::default())
        .expect("decode");

    assert_eq!((out.width, out.height), (w, h));
}

#[test]
fn pixel_limit_is_enforced() {
    let (w, h) = (16u32, 8u32);
    let src = vec![0u8; (w * h * 3) as usize];
    let image = jp2lam::Image::from_rgb_bytes(w, h, &src).expect("build image");
    let jp2 = jp2lam::encode(&image, &lossless()).expect("encode");

    let limits = DecodeLimits { max_pixels: 4, ..DecodeLimits::default() };
    assert!(
        JpxCodec
            .decode(&jp2, &descriptor(w, h), &DecodeParameters::default(), &limits)
            .is_err(),
        "decode must refuse to exceed the pixel budget"
    );
}

#[test]
fn garbage_is_a_typed_error_not_a_panic() {
    let out = JpxCodec.decode(
        b"definitely not a jp2 codestream",
        &descriptor(4, 4),
        &DecodeParameters::default(),
        &DecodeLimits::default(),
    );
    assert!(matches!(out, Err(pdf_image::ImageError::Decode(_))));
}

#[test]
fn gray_jp2_decodes_packed_gray8() {
    let (w, h) = (16u32, 8u32);
    let src: Vec<u8> = (0..w * h).map(|i| (i * 3 % 256) as u8).collect();
    let image = jp2lam::Image::from_gray_bytes(w, h, &src).expect("build gray image");
    let jp2 = jp2lam::encode(&image, &lossless()).expect("encode");

    let out = JpxCodec
        .decode(&jp2, &descriptor(w, h), &DecodeParameters::default(), &DecodeLimits::default())
        .expect("decode");

    assert_eq!((out.width, out.height), (w, h));
    assert_eq!(out.format, DecodedFormat::Gray8);
    assert_eq!(out.stride, w as usize);
    // 8-bit packed output is bit-exact with the source (the direct fast path).
    assert_eq!(&*out.data, &src[..], "packed gray round-trips exactly");
}

/// A 16-bit stream is not eligible for the direct 8-bit packed path (jp2lam and
/// the legacy adapter scale >8-bit samples differently); it must fall back to
/// the native planar decode and the historical `v >> (precision - 8)`
/// interleave, which is what this asserts bit-for-bit.
#[test]
fn high_bit_depth_falls_back_to_native_interleave() {
    let (w, h) = (16u32, 8u32);
    // Distinct 16-bit values so the >> 8 scaling is observable per pixel.
    let src16: Vec<u16> = (0..w * h).map(|i| (i as u16).wrapping_mul(517) | 0x0100).collect();
    let view = jp2lam::ImageView::from_gray16(w, h, &src16, 16).expect("build gray16 view");
    // Lossless so the 16-bit samples round-trip exactly and the only transform
    // between input and output byte is the adapter's `>> 8`.
    let jp2 = jp2lam::encode_view(view, &lossless()).expect("encode");

    let out = JpxCodec
        .decode(&jp2, &descriptor(w, h), &DecodeParameters::default(), &DecodeLimits::default())
        .expect("decode");

    assert_eq!((out.width, out.height), (w, h));
    assert_eq!(out.format, DecodedFormat::Gray8);
    let expected: Vec<u8> = src16.iter().map(|&v| (v >> 8) as u8).collect();
    assert_eq!(&*out.data, &expected[..], "16-bit native interleave uses v >> 8");
}

/// A minified draw supplies a device-footprint hint; the codec decodes at a
/// reduced wavelet resolution, yielding a smaller raster than the full source.
/// Without the hint the same stream decodes at full resolution.
#[test]
fn target_size_hint_reduces_decode_resolution() {
    let (w, h) = (256u32, 256u32);
    let src: Vec<u8> = (0..w * h).map(|i| (i / 7 % 256) as u8).collect();
    let image = jp2lam::Image::from_gray_bytes(w, h, &src).expect("build gray image");
    let jp2 = jp2lam::encode(&image, &lossless()).expect("encode");

    // No hint: full resolution.
    let full = JpxCodec
        .decode(&jp2, &descriptor(w, h), &DecodeParameters::default(), &DecodeLimits::default())
        .expect("decode full");
    assert_eq!((full.width, full.height), (w, h));

    // A tiny destination footprint: the decoder drops wavelet levels while
    // keeping at least the quality margin, so the raster is strictly smaller.
    let params = DecodeParameters { target_size: Some((32, 32)), ..DecodeParameters::default() };
    let reduced = JpxCodec
        .decode(&jp2, &descriptor(w, h), &params, &DecodeLimits::default())
        .expect("decode reduced");
    assert!(
        reduced.width < w && reduced.height < h,
        "reduced decode should shrink {}x{} below {w}x{h}",
        reduced.width,
        reduced.height,
    );
    assert_eq!(reduced.format, DecodedFormat::Gray8);
    assert_eq!(reduced.stride, reduced.width as usize);
    // The reduced raster still stays at or above the requested footprint.
    assert!(reduced.width >= 32 && reduced.height >= 32);
}
