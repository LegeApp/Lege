#![no_main]
//! Fuzz target: the custom Huffman-table parser (segment type 53, T.88 B.2) and
//! table decoding (Phase 5c). Building a table from arbitrary bytes and decoding
//! values from arbitrary bytes must never panic, hang, or over-allocate.
//!
//! Run: cargo +nightly fuzz run huffman_table

use libfuzzer_sys::fuzz_target;

use jbig2enc_rust::decode::huffman::{parse_custom_table, standard_table, BitReader};
use jbig2enc_rust::decode::DecodeLimits;

fuzz_target!(|data: &[u8]| {
    let limits = DecodeLimits::default();

    // Half the input builds a custom table; the other half is decoded with it.
    let split = data.len() / 2;
    let (table_bytes, value_bytes) = data.split_at(split);

    if let Ok(table) = parse_custom_table(table_bytes, &limits) {
        let mut r = BitReader::new(value_bytes);
        for _ in 0..64 {
            if table.decode(&mut r).is_err() {
                break;
            }
        }
    }

    // Every standard table must decode arbitrary bits without panicking.
    for n in 1u8..=15 {
        if let Ok(t) = standard_table(n) {
            let mut r = BitReader::new(data);
            for _ in 0..64 {
                if t.decode(&mut r).is_err() {
                    break;
                }
            }
        }
    }
});
