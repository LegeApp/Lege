//! Fuzz structural load: header scan, xref chains (classic, stream,
//! hybrid), trailers, incremental revisions, and every recovery path in
//! `pdf_structure::load_structure` — over an in-memory `PdfSource`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pdf_object::NameTable;
use pdf_source::OwnedBytesSource;
use pdf_structure::{StructureLimits, load_structure};

fuzz_target!(|data: &[u8]| {
    let source = OwnedBytesSource::new(data.to_vec());
    let names = NameTable::new();
    let limits = StructureLimits {
        max_revisions: 32,
        max_objects: 1 << 16,
        max_decoded_bytes: 1 << 22, // 4 MiB
        ..StructureLimits::default()
    };
    let _ = load_structure(&source, &names, &limits);
});
