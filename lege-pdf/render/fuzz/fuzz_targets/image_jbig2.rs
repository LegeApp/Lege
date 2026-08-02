//! Fuzz the JBIG2 decoder (`/JBIG2Decode`), embedded-stream flavor.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pdf_image::codec::{DecodeLimits, ImageCodec};
use pdf_image::{DecodeParameters, ImageDescriptor, Jbig2Codec, StreamFilter};

fuzz_target!(|data: &[u8]| {
    // First byte splits the input into a globals stream and the page
    // stream, exercising the /JBIG2Globals path too.
    let (globals, body): (Option<&[u8]>, &[u8]) = match data.split_first() {
        Some((&sel, rest)) if sel % 2 == 1 && rest.len() >= 2 => {
            let mid = rest.len() / 2;
            (Some(&rest[..mid]), &rest[mid..])
        }
        Some((_, rest)) => (None, rest),
        None => return,
    };
    let w = 1 + body.first().copied().unwrap_or(0) as u32;
    let h = 1 + body.get(1).copied().unwrap_or(0) as u32;
    let descriptor = ImageDescriptor {
        width: w,
        height: h,
        bits_per_component: 1,
        color_space: None,
        is_mask: false,
        interpolate: false,
        filters: vec![StreamFilter::Jbig2],
        object: None,
    };
    let params = DecodeParameters {
        jbig2_globals: globals.map(std::sync::Arc::from),
        ..DecodeParameters::default()
    };
    let limits = DecodeLimits {
        max_pixels: 1 << 20,
        max_output_bytes: 1 << 24,
        ..DecodeLimits::default()
    };
    let _ = Jbig2Codec::default().decode(body, &descriptor, &params, &limits);
});
