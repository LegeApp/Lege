#![no_main]
//! Fuzz target: the T.88 §7.2 segment-header parser must never panic on
//! arbitrary input — only return a bounded header or a typed error.
//!
//! Run (needs nightly + `cargo install cargo-fuzz`):
//!   cargo +nightly fuzz run parse_segment_header
//! Seed corpus: any encoder stream (see the round-trip tests).

use libfuzzer_sys::fuzz_target;

use jbig2enc_rust::decode::segment::parse_segment_header;
use jbig2enc_rust::shared::reader::Reader;

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::new(data);
    // Parse until exhausted or error; must never panic or loop unboundedly.
    while !r.is_empty() {
        match parse_segment_header(&mut r) {
            Ok(h) => {
                // Skip the (bounded) declared payload if it is a known length.
                if !h.is_unknown_length() {
                    let len = h.data_length as usize;
                    if r.take(len).is_err() {
                        break;
                    }
                } else {
                    break;
                }
            }
            Err(_) => break,
        }
    }
});
