#![no_main]

use jp2lam::{
    DecodeLimits, DecodeRequest, decode_jp2_request, inspect_jp2_with_limits,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let limits = DecodeLimits {
        max_input_bytes: 1024 * 1024,
        max_pixels: 1_000_000,
        max_precincts: 100_000,
        max_packets: 200_000,
        max_code_blocks: 200_000,
        max_working_bytes: 64 * 1024 * 1024,
    };

    // Exercise the container and Annex-A marker parsers independently, then
    // drive accepted headers through precinct planning, packet headers, Tier-1,
    // and reconstruction under strict allocation/work ceilings.
    let _ = inspect_jp2_with_limits(data, &limits);
    let request = DecodeRequest {
        limits,
        ..DecodeRequest::default()
    };
    let _ = decode_jp2_request(data, &request);
});
