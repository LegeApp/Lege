//! Classic xref table + trailer + startxref. Later (M4, optional): xref streams.
//!
//! Consumes the sink's offset table and writes a PDF 32000-1 §7.5.4 classic
//! cross-reference table, the trailer dictionary, `startxref`, and `%%EOF`.
//! Every xref entry is exactly 20 bytes (`\r\n`-terminated) as the spec
//! requires.

use std::io::Write;

use crate::serialize::{PdfValue, write_u64, write_value};
use crate::sink::PdfSink;
use crate::types::{ObjectId, Result};

/// The largest byte offset a classic 10-digit xref field can hold. Beyond this
/// a file needs xref streams (deferred, M4).
const MAX_CLASSIC_OFFSET: u64 = 9_999_999_999;

/// Write the xref table, trailer, `startxref`, and `%%EOF`. Must be the last
/// thing written to the sink. `root` is the catalog; `info` the document
/// information dictionary if present.
pub fn write_xref_and_trailer<W: Write>(
    sink: &mut PdfSink<W>,
    root: ObjectId,
    info: Option<ObjectId>,
) -> Result<()> {
    let xref_offset = sink.position();
    let size = sink.offsets().len() as u64;

    let mut buf = Vec::with_capacity(sink.offsets().len() * 20 + 128);

    // Table header.
    buf.extend_from_slice(b"xref\n");
    write_u64(&mut buf, 0);
    buf.push(b' ');
    write_u64(&mut buf, size);
    buf.push(b'\n');

    // Free head: object 0, generation 65535, free.
    buf.extend_from_slice(b"0000000000 65535 f\r\n");

    // In-use entries for objects 1..size.
    for &offset in &sink.offsets()[1..] {
        debug_assert!(
            offset <= MAX_CLASSIC_OFFSET,
            "offset {offset} exceeds classic xref range; xref streams needed"
        );
        write_offset_field(&mut buf, offset);
        buf.extend_from_slice(b" 00000 n\r\n");
    }

    // Trailer dictionary.
    buf.extend_from_slice(b"trailer\n");
    let size_val = PdfValue::Integer(size as i64);
    if let Some(info_id) = info {
        write_value(
            &mut buf,
            &PdfValue::Dict(&[
                (b"Size", size_val),
                (b"Root", PdfValue::Reference(root)),
                (b"Info", PdfValue::Reference(info_id)),
            ]),
        );
    } else {
        write_value(
            &mut buf,
            &PdfValue::Dict(&[(b"Size", size_val), (b"Root", PdfValue::Reference(root))]),
        );
    }
    buf.push(b'\n');

    // startxref + EOF.
    buf.extend_from_slice(b"startxref\n");
    write_u64(&mut buf, xref_offset);
    buf.extend_from_slice(b"\n%%EOF\n");

    sink.write_raw(&buf)
}

/// Write a 10-digit zero-padded byte offset.
fn write_offset_field(out: &mut Vec<u8>, offset: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    let mut v = offset;
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let digits = &tmp[i..];
    for _ in digits.len()..10 {
        out.push(b'0');
    }
    out.extend_from_slice(digits);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::StreamBody;

    #[test]
    fn offset_field_is_ten_digits() {
        let mut out = Vec::new();
        write_offset_field(&mut out, 0);
        assert_eq!(out, b"0000000000");
        out.clear();
        write_offset_field(&mut out, 12345);
        assert_eq!(out, b"0000012345");
    }

    #[test]
    fn full_table_structure() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let cat = sink.alloc_id();
        sink.write_indirect(cat, b"<</Type /Catalog /Pages 2 0 R>>")
            .unwrap();
        let pages = sink.alloc_id();
        sink.write_stream(pages, b"<</Length 0>>", &StreamBody::Empty)
            .unwrap();

        write_xref_and_trailer(&mut sink, cat, None).unwrap();
        let bytes = sink.finish().unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();

        // Header line: "0 3" (free + 2 objects).
        assert!(text.contains("xref\n0 3\n"), "{text}");
        assert!(text.contains("0000000000 65535 f\r\n"), "{text}");
        // Two 20-byte in-use entries.
        assert_eq!(text.matches(" 00000 n\r\n").count(), 2);
        // Trailer references the catalog, no Info.
        assert!(text.contains("/Size 3"), "{text}");
        assert!(text.contains("/Root 1 0 R"), "{text}");
        assert!(!text.contains("/Info"), "{text}");
        // Ends correctly.
        assert!(text.contains("startxref\n"));
        assert!(text.trim_end().ends_with("%%EOF"), "{text}");
    }

    #[test]
    fn each_xref_entry_is_twenty_bytes() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let id = sink.alloc_id();
        sink.write_indirect(id, b"<<>>").unwrap();
        write_xref_and_trailer(&mut sink, id, None).unwrap();
        let bytes = sink.finish().unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // Slice out the region between the subsection header and "trailer".
        let start = text.find("0 2\n").unwrap() + "0 2\n".len();
        let end = text.find("trailer").unwrap();
        let entries = &text[start..end];
        // Two entries (free + one object), 20 bytes each.
        assert_eq!(entries.len(), 40, "entries region: {entries:?}");
    }
}
