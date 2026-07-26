//! Fuzz the in-house CCITT G3/G4 fax decoder (`/CCITTFaxDecode`).

#![no_main]

use libfuzzer_sys::fuzz_target;
use pdf_image::codec::{DecodeLimits, ImageCodec};
use pdf_image::{CcittCodec, CcittParams, DecodeParameters, ImageDescriptor, StreamFilter};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, body)) = data.split_first() else {
        return;
    };
    // Vary the coding scheme and framing knobs from the selector byte.
    let k: i32 = match sel & 0b11 {
        0 => -1, // Group 4
        1 => 0,  // Group 3 1-D
        _ => 4,  // Group 3 mixed 2-D
    };
    let columns = 1 + body.first().copied().unwrap_or(0) as u32 * 8;
    let rows = 1 + body.get(1).copied().unwrap_or(0) as u32;
    let params = DecodeParameters {
        ccitt: Some(CcittParams {
            k,
            columns,
            rows,
            black_is_1: sel & 0b100 != 0,
            byte_align: sel & 0b1000 != 0,
            ..CcittParams::default()
        }),
        ..DecodeParameters::default()
    };
    let descriptor = ImageDescriptor {
        width: columns,
        height: rows,
        bits_per_component: 1,
        color_space: None,
        is_mask: false,
        interpolate: false,
        filters: vec![StreamFilter::CcittFax],
        object: None,
    };
    let limits = DecodeLimits {
        max_pixels: 1 << 20,
        max_output_bytes: 1 << 24,
        should_cancel: None,
    };
    let _ = CcittCodec.decode(body, &descriptor, &params, &limits);
});
