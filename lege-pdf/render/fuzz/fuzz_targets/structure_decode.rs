//! Fuzz the general stream-filter chain: `pdf_structure::decode_stream`
//! with a minimal `/Filter` dictionary. The first input byte selects the
//! filter; the rest is the raw stream body. Everything must end in decoded
//! bytes or a typed `DecodeError` under a small `DecodeBudget`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pdf_object::{Dictionary, NameTable, PdfObject};
use pdf_structure::decode::{DecodeBudget, decode_stream};

fuzz_target!(|data: &[u8]| {
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    let names = NameTable::new();
    let filter: &[u8] = match selector % 6 {
        0 => b"FlateDecode",
        1 => b"LZWDecode",
        2 => b"ASCIIHexDecode",
        3 => b"ASCII85Decode",
        4 => b"RunLengthDecode",
        // No filter at all: exercises the unfiltered materialization path.
        _ => b"",
    };
    let dict = if filter.is_empty() {
        Dictionary::new()
    } else {
        Dictionary::from_pairs([(
            names.known.filter,
            PdfObject::Name(names.intern(filter)),
        )])
    };
    // Small budget: 1 MiB of decompressed output per input.
    let mut budget = DecodeBudget::new(1 << 20);
    let _ = decode_stream(body, &dict, &names, &mut budget);
});
