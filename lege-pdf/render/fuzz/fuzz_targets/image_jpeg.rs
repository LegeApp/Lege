//! Fuzz the in-house baseline/progressive JPEG decoder (`/DCTDecode`).

#![no_main]

use libfuzzer_sys::fuzz_target;
use pdf_image::codec::{DecodeLimits, ImageCodec};
use pdf_image::{DecodeParameters, ImageDescriptor, JpegCodec, StreamFilter};

fuzz_target!(|data: &[u8]| {
    // The descriptor's width/height come from the PDF dictionary in real
    // documents and may disagree with the bit-stream; vary them too.
    let w = 1 + data.first().copied().unwrap_or(0) as u32;
    let h = 1 + data.get(1).copied().unwrap_or(0) as u32;
    let descriptor = ImageDescriptor {
        width: w,
        height: h,
        bits_per_component: 8,
        color_space: None,
        is_mask: false,
        interpolate: false,
        filters: vec![StreamFilter::DctDecode],
        object: None,
    };
    let limits = DecodeLimits {
        max_pixels: 1 << 20,       // 1 Mpx
        max_output_bytes: 1 << 24, // 16 MiB
        ..DecodeLimits::default()
    };
    let _ = JpegCodec.decode(data, &descriptor, &DecodeParameters::default(), &limits);
});
