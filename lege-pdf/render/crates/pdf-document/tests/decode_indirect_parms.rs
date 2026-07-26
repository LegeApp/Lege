#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! An indirect `/DecodeParms` (or `/Filter`) reference must be resolved before
//! the pure-`pdf-structure` decoder runs. A shared predictor dict written as
//! `N 0 R` is extremely common; when it was silently dropped, the PNG predictor
//! was skipped and the image decoded to its raw filtered deltas — a near-black
//! smear. Regression for that fix (`Resolver::resolve_filter_params`).

use std::io::Write;
use std::sync::Arc;

use pdf_document::{DocumentLimits, DocumentSnapshot, ParseContext};
use pdf_object::{ObjectId, PdfObject};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// Two 3-wide rows `[1,2,3]` and `[4,5,6]` PNG **Up**-filtered (filter byte 2):
/// row 1 deltas from the zero row = `[1,2,3]`; row 2 deltas from row 1 =
/// `[3,3,3]`. De-predicting must recover `[1,2,3,4,5,6]`.
fn up_filtered_rows() -> Vec<u8> {
    vec![2, 1, 2, 3, 2, 3, 3, 3]
}

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open")
}

fn decode_stream_obj(snap: &DocumentSnapshot, number: u32) -> Vec<u8> {
    let mut ctx = ParseContext::new();
    let obj = snap
        .objects()
        .resolve(snap, ObjectId::new(number, 0), &mut ctx)
        .expect("resolve stream");
    let PdfObject::Stream(stream) = obj.as_ref() else {
        panic!("object {number} is not a stream");
    };
    snap.decode_stream_data(stream, &mut ctx).expect("decode")
}

#[test]
fn indirect_decode_parms_predictor_is_applied() {
    let compressed = zlib(&up_filtered_rows());
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    // The image stream references its predictor dict indirectly (`4 0 R`).
    b.add_stream(3, "/Filter/FlateDecode/DecodeParms 4 0 R", &compressed);
    b.add_object(4, "<</Predictor 12/Colors 1/Columns 3>>");
    b.finish_classic_xref("/Root 1 0 R");

    let snap = open(b.into_bytes());
    assert_eq!(
        decode_stream_obj(&snap, 3),
        vec![1, 2, 3, 4, 5, 6],
        "indirect /DecodeParms predictor must be applied (not skipped → filtered deltas)"
    );
}

#[test]
fn indirect_filter_reference_is_resolved() {
    // `/Filter` itself as an indirect reference to a name.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    b.add_stream(3, "/Filter 4 0 R", b"48 65 6C 6C 6F>");
    b.add_object(4, "/ASCIIHexDecode");
    b.finish_classic_xref("/Root 1 0 R");

    let snap = open(b.into_bytes());
    assert_eq!(decode_stream_obj(&snap, 3), b"Hello");
}

#[test]
fn indirect_filter_reference_to_array_of_references_is_resolved() {
    // Seen in the wild on Flate-then-DCT covers (Spence *Search for Modern
    // China*, *The Shock Doctrine*): `/Filter` is a single indirect reference
    // that resolves to an *array of indirect references*, each a filter name —
    // `/Filter 4 0 R` → `[5 0 R 6 0 R]` → `[/FlateDecode /ASCIIHexDecode]`.
    // Resolving only the outer reference left the elements as references, which
    // the decoder rejected as "/Filter entry is not a name", dropping the whole
    // image draw (a blank page). Both nesting levels must be resolved.
    let compressed = zlib(b"48 65 6C 6C 6F>");
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    b.add_stream(3, "/Filter 4 0 R", &compressed);
    b.add_object(4, "[5 0 R 6 0 R]");
    b.add_object(5, "/FlateDecode");
    b.add_object(6, "/ASCIIHexDecode");
    b.finish_classic_xref("/Root 1 0 R");

    let snap = open(b.into_bytes());
    assert_eq!(
        decode_stream_obj(&snap, 3),
        b"Hello",
        "indirect /Filter resolving to an array of indirect name references must decode"
    );
}
