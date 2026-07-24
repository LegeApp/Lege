#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! End-to-end tests for `load_structure` over hand-assembled fixtures.

use std::io::Write as _;

use pdf_object::{NameTable, ObjectId};
use pdf_source::OwnedBytesSource;
use pdf_structure::{
    ObjectLocation, RecoveryEvent, StructureError, StructureLimits, load_structure,
};

/// Minimal fixture assembler: append objects, remember offsets, then emit a
/// classic xref table or an xref stream.
struct MiniPdf {
    buf: Vec<u8>,
    /// (number, generation, header-relative offset)
    objects: Vec<(u32, u16, u64)>,
}

impl MiniPdf {
    fn new() -> Self {
        Self { buf: b"%PDF-1.7\n".to_vec(), objects: Vec::new() }
    }

    fn add_object(&mut self, number: u32, body: &str) -> u64 {
        let offset = self.buf.len() as u64;
        self.objects.push((number, 0, offset));
        writeln!(self.buf, "{number} 0 obj\n{body}\nendobj").unwrap();
        offset
    }

    fn add_stream_object(&mut self, number: u32, dict: &str, data: &[u8]) -> u64 {
        let offset = self.buf.len() as u64;
        self.objects.push((number, 0, offset));
        write!(self.buf, "{number} 0 obj\n{dict}\nstream\n").unwrap();
        self.buf.extend_from_slice(data);
        self.buf.extend_from_slice(b"\nendstream\nendobj\n");
        offset
    }

    /// Emit `xref` table + trailer + startxref. Entry list = object 0 free
    /// plus every recorded object, in one subsection when contiguous.
    fn finish_classic(mut self, trailer_extra: &str) -> Vec<u8> {
        let xref_off = self.buf.len();
        let max = self.objects.iter().map(|(n, ..)| *n).max().unwrap_or(0);
        writeln!(self.buf, "xref\n0 {}", max + 1).unwrap();
        write!(self.buf, "0000000000 65535 f\r\n").unwrap();
        for n in 1..=max {
            match self.objects.iter().find(|(num, ..)| *num == n) {
                Some((_, g, off)) => write!(self.buf, "{off:010} {g:05} n\r\n").unwrap(),
                None => write!(self.buf, "0000000000 65535 f\r\n").unwrap(),
            }
        }
        write!(
            self.buf,
            "trailer\n<</Size {} {trailer_extra}>>\nstartxref\n{xref_off}\n%%EOF",
            max + 1
        )
        .unwrap();
        self.buf
    }
}

fn load(bytes: Vec<u8>) -> pdf_structure::DocumentStructure {
    let names = NameTable::new();
    let limits = StructureLimits::default();
    let source = OwnedBytesSource::new(bytes);
    load_structure(&source, &names, &limits).expect("load_structure failed")
}

fn basic_two_object_pdf() -> Vec<u8> {
    let mut pdf = MiniPdf::new();
    pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    pdf.finish_classic("/Root 1 0 R")
}

#[test]
fn classic_table_loads() {
    let structure = load(basic_two_object_pdf());
    assert_eq!(structure.version.major, 1);
    assert_eq!(structure.version.minor, 7);
    assert_eq!(structure.header_offset, 0);
    assert_eq!(structure.trailer.root, ObjectId::new(1, 0));
    assert!(structure.recovery.is_empty(), "unexpected recovery: {:?}", structure.recovery);
    assert!(matches!(
        structure.xref.locate(ObjectId::new(1, 0)),
        ObjectLocation::Offset(_)
    ));
    assert_eq!(structure.xref.locate(ObjectId::new(0, 0)), ObjectLocation::Free);
    assert_eq!(structure.revisions.len(), 1);
}

#[test]
fn rebuild_indexes_object_stream_members() {
    // A modern PDF keeps most objects inside /Type /ObjStm containers. When its
    // xref is unusable and the loader must rebuild, the header scan alone can't
    // see compressed members — the rebuild must decompress each container and
    // index them, or they vanish. Here object 8 (a font) lives inside object
    // stream 7; with no xref at all the loader rebuilds and must still locate 8.
    let member = b"8 0 <</Type/Font/Subtype/Type1/BaseFont/Helvetica>>";
    let mut buf: Vec<u8> = b"%PDF-1.7\n".to_vec();
    buf.extend_from_slice(b"1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n");
    buf.extend_from_slice(
        b"2 0 obj\n<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 612 792]>>\nendobj\n",
    );
    buf.extend_from_slice(
        b"3 0 obj\n<</Type/Page/Parent 2 0 R/Resources<</Font<</F1 8 0 R>>>>>>\nendobj\n",
    );
    // Object stream 7: uncompressed body = pair table "8 0 " (First = 4) then
    // member 8's dictionary. N = 1 member.
    buf.extend_from_slice(
        format!("7 0 obj\n<</Type/ObjStm/N 1/First 4/Length {}>>\nstream\n", member.len())
            .as_bytes(),
    );
    buf.extend_from_slice(member);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    // No xref, no startxref: the loader must rebuild (and find /Root via the
    // trailer keyword).
    buf.extend_from_slice(b"trailer\n<</Root 1 0 R/Size 9>>\n%%EOF\n");

    let structure = load(buf);
    assert!(
        structure.recovery.iter().any(|e| matches!(e, RecoveryEvent::XrefRebuilt)),
        "expected a rebuild, got {:?}",
        structure.recovery
    );
    assert_eq!(
        structure.xref.locate(ObjectId::new(8, 0)),
        ObjectLocation::InObjectStream { container: 7, index: 0 },
        "object-stream member 8 must be indexed by the rebuild"
    );
    assert!(matches!(
        structure.xref.locate(ObjectId::new(7, 0)),
        ObjectLocation::Offset(_)
    ));
}

#[test]
fn recovered_stub_page_tree_triggers_rebuild() {
    // A linearized/incremental file whose first revision is a stub — an empty
    // `<</Type/Pages/Kids[]/Count 0>>` that a later revision overrides with the
    // real page tree. With no findable `startxref` (its tail is a stream in the
    // wild), recovery lands on the stub xref and the document reads as zero
    // pages; the loader must escalate to a full rebuild, whose
    // last-occurrence-wins recovers the real page tree.
    let mut buf: Vec<u8> = b"%PDF-1.7\n".to_vec();
    let off1 = buf.len() as u64;
    buf.extend_from_slice(b"1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n");
    let off2_stub = buf.len() as u64;
    buf.extend_from_slice(b"2 0 obj\n<</Type/Pages/Kids[]/Count 0>>\nendobj\n");
    // Classic xref over the stub revision only, then a trailer — but NO
    // startxref, so the reported-chain path finds nothing and recovery scans
    // for the last `xref` keyword (which is this stub table).
    let xref_off = buf.len() as u64;
    buf.extend_from_slice(
        format!(
            "xref\n0 3\n0000000000 65535 f\r\n{off1:010} 00000 n\r\n{off2_stub:010} 00000 n\r\n\
             trailer\n<</Size 3/Root 1 0 R>>\n"
        )
        .as_bytes(),
    );
    // Later revision (after the xref): redefine object 2 with the real page.
    buf.extend_from_slice(b"2 0 obj\n<</Type/Pages/Kids[3 0 R]/Count 1>>\nendobj\n");
    buf.extend_from_slice(b"3 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>\nendobj\n");
    buf.extend_from_slice(b"%%EOF\n");

    let structure = load(buf);
    assert!(
        structure.recovery.iter().any(|e| matches!(e, RecoveryEvent::XrefRebuilt)),
        "expected a rebuild after the stub, got {:?}",
        structure.recovery
    );
    // Object 2 must now resolve to the real (post-xref) definition, not the stub.
    let ObjectLocation::Offset(o2) = structure.xref.locate(ObjectId::new(2, 0)) else {
        panic!("object 2 unexpectedly free");
    };
    assert!(o2 > xref_off, "object 2 should resolve to its real later definition");
    assert_eq!(structure.trailer.root, ObjectId::new(1, 0));
}

#[test]
fn leading_garbage_header_offset_recorded() {
    let mut bytes = b"GARBAGE-".repeat(12); // 96 bytes of junk
    let inner = basic_two_object_pdf();
    bytes.extend_from_slice(&inner);
    let structure = load(bytes);
    assert_eq!(structure.header_offset, 96);
    // Offsets remain header-relative: object 1 sits at the same structural
    // offset as in the garbage-free file.
    let clean = load(basic_two_object_pdf());
    assert_eq!(
        structure.xref.locate(ObjectId::new(1, 0)),
        clean.xref.locate(ObjectId::new(1, 0))
    );
}

#[test]
fn garbage_beyond_1024_bytes_is_no_header() {
    let mut bytes = vec![b'x'; 2000];
    bytes.extend_from_slice(&basic_two_object_pdf());
    let names = NameTable::new();
    let source = OwnedBytesSource::new(bytes);
    assert!(matches!(
        load_structure(&source, &names, &StructureLimits::default()),
        Err(StructureError::NoHeader)
    ));
}

#[test]
fn sloppy_xref_entry_line_endings_tolerated() {
    // Hand-write entries with 19- and 21-byte lines (LF only / space CR LF).
    let mut pdf = MiniPdf::new();
    let off1 = pdf.add_object(1, "<</Type/Catalog>>");
    let off2 = pdf.add_object(2, "<</X 1>>");
    let mut buf = pdf.buf;
    let xref_off = buf.len();
    write!(
        buf,
        "xref\n0 3\n0000000000 65535 f\n{off1:010} 00000 n\n{off2:010} 00000 n \r\n"
    )
    .unwrap();
    write!(buf, "trailer\n<</Size 3/Root 1 0 R>>\nstartxref\n{xref_off}\n%%EOF").unwrap();
    let structure = load(buf);
    assert_eq!(structure.xref.locate(ObjectId::new(1, 0)), ObjectLocation::Offset(off1));
    assert_eq!(structure.xref.locate(ObjectId::new(2, 0)), ObjectLocation::Offset(off2));
}

#[test]
fn incremental_update_shadows_and_frees() {
    // Base revision: objects 1 (catalog), 2, 3.
    let mut pdf = MiniPdf::new();
    pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    let old2 = pdf.add_object(2, "<</Version 1>>");
    pdf.add_object(3, "<</Keep 1>>");
    let mut buf = pdf.finish_classic("/Root 1 0 R");
    let base_xref = buf.windows(4).position(|w| w == b"xref").unwrap();
    // Strip the base %%EOF tail marker's influence by simply appending the
    // update after it (real incremental updates do exactly this).
    let new2_off = buf.len() as u64;
    write!(buf, "2 0 obj\n<</Version 2>>\nendobj\n").unwrap();
    let upd_xref = buf.len();
    // Update: object 2 rewritten, object 3 freed (gen bumped).
    write!(
        buf,
        "xref\n2 2\n{new2_off:010} 00000 n\r\n0000000000 00001 f\r\n"
    )
    .unwrap();
    write!(
        buf,
        "trailer\n<</Size 4/Root 1 0 R/Prev {base_xref}>>\nstartxref\n{upd_xref}\n%%EOF"
    )
    .unwrap();

    let structure = load(buf);
    assert_eq!(structure.revisions.len(), 2);
    // Object 2: the update's offset wins over the base's.
    let loc2 = structure.xref.locate(ObjectId::new(2, 0));
    assert_eq!(loc2, ObjectLocation::Offset(new2_off));
    assert_ne!(loc2, ObjectLocation::Offset(old2));
    // Object 3: freed by the update (later free shadows earlier in-use).
    assert_eq!(structure.xref.locate(ObjectId::new(3, 0)), ObjectLocation::Free);
    // Object 1: inherited from the base revision.
    assert!(matches!(structure.xref.locate(ObjectId::new(1, 0)), ObjectLocation::Offset(_)));
}

/// Build an xref-stream file. `compress` toggles FlateDecode+predictor.
fn xref_stream_pdf(compress: bool) -> (Vec<u8>, u64, u64) {
    let mut pdf = MiniPdf::new();
    let off1 = pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    let off2 = pdf.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    let mut buf = pdf.buf;
    let xref_off = buf.len() as u64;

    // W [1 2 1]; entries for 0..=3 (3 = the xref stream itself).
    let rows: Vec<[u8; 4]> = vec![
        [0, 0, 0, 255],
        [1, (off1 >> 8) as u8, off1 as u8, 0],
        [1, (off2 >> 8) as u8, off2 as u8, 0],
        [1, (xref_off >> 8) as u8, xref_off as u8, 0],
    ];
    let (data, extra) = if compress {
        // PNG Up predictor, columns 4, then flate.
        let mut filtered = Vec::new();
        let mut prev = [0u8; 4];
        for row in &rows {
            filtered.push(2u8);
            for i in 0..4 {
                filtered.push(row[i].wrapping_sub(prev[i]));
            }
            prev = *row;
        }
        let mut enc =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&filtered).unwrap();
        (
            enc.finish().unwrap(),
            "/Filter/FlateDecode/DecodeParms<</Predictor 12/Columns 4>>".to_string(),
        )
    } else {
        (rows.iter().flatten().copied().collect(), String::new())
    };

    write!(
        buf,
        "3 0 obj\n<</Type/XRef/Size 4/W[1 2 1]{extra}/Root 1 0 R/Length {}>>\nstream\n",
        data.len()
    )
    .unwrap();
    buf.extend_from_slice(&data);
    write!(buf, "\nendstream\nendobj\nstartxref\n{xref_off}\n%%EOF").unwrap();
    (buf, off1, off2)
}

#[test]
fn xref_stream_uncompressed() {
    let (buf, off1, off2) = xref_stream_pdf(false);
    let structure = load(buf);
    assert_eq!(structure.trailer.root, ObjectId::new(1, 0));
    assert_eq!(structure.xref.locate(ObjectId::new(1, 0)), ObjectLocation::Offset(off1));
    assert_eq!(structure.xref.locate(ObjectId::new(2, 0)), ObjectLocation::Offset(off2));
    assert!(structure.recovery.is_empty(), "unexpected recovery: {:?}", structure.recovery);
}

#[test]
fn xref_stream_flate_with_predictor() {
    let (buf, off1, _) = xref_stream_pdf(true);
    let structure = load(buf);
    assert_eq!(structure.xref.locate(ObjectId::new(1, 0)), ObjectLocation::Offset(off1));
}

#[test]
fn xref_stream_type2_entries_locate_in_object_stream() {
    // Objects 4 and 5 live in object stream 3 at indices 0 and 1.
    let mut pdf = MiniPdf::new();
    let off1 = pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    let off2 = pdf.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    let objstm_data = b"4 0 5 8 <</A 1>> <</B 2>>";
    let off3 = pdf.add_stream_object(
        3,
        &format!("<</Type/ObjStm/N 2/First 8/Length {}>>", objstm_data.len()),
        objstm_data,
    );
    let mut buf = pdf.buf;
    let xref_off = buf.len() as u64;
    let mut rows: Vec<[u8; 4]> = vec![
        [0, 0, 0, 255],
        [1, (off1 >> 8) as u8, off1 as u8, 0],
        [1, (off2 >> 8) as u8, off2 as u8, 0],
        [1, (off3 >> 8) as u8, off3 as u8, 0],
        [2, 0, 3, 0], // obj 4 → container 3, index 0
        [2, 0, 3, 1], // obj 5 → container 3, index 1
        [1, (xref_off >> 8) as u8, xref_off as u8, 0],
    ];
    // Fix the container field encoding: W[1]=2 bytes → [hi, lo].
    rows[4] = [2, 0, 3, 0];
    rows[5] = [2, 0, 3, 1];
    let data: Vec<u8> = rows.iter().flatten().copied().collect();
    write!(
        buf,
        "6 0 obj\n<</Type/XRef/Size 7/W[1 2 1]/Root 1 0 R/Length {}>>\nstream\n",
        data.len()
    )
    .unwrap();
    buf.extend_from_slice(&data);
    write!(buf, "\nendstream\nendobj\nstartxref\n{xref_off}\n%%EOF").unwrap();

    let structure = load(buf);
    assert_eq!(
        structure.xref.locate(ObjectId::new(4, 0)),
        ObjectLocation::InObjectStream { container: 3, index: 0 }
    );
    assert_eq!(
        structure.xref.locate(ObjectId::new(5, 0)),
        ObjectLocation::InObjectStream { container: 3, index: 1 }
    );
    // Generation for in-stream objects is 0; generation 1 lookups miss.
    assert_eq!(structure.xref.locate(ObjectId::new(4, 1)), ObjectLocation::Free);
}

#[test]
fn hybrid_xrefstm_classic_entries_win() {
    // Classic table says object 2 is at `off2_table`; the /XRefStm claims
    // it is elsewhere. Table entries take precedence (PDFium merge order),
    // while objects only known to the stream (object 3) come through.
    let mut pdf = MiniPdf::new();
    let off1 = pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    let off2_table = pdf.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    let off3 = pdf.add_object(3, "<</OnlyInStream 1>>");
    let mut buf = pdf.buf;

    // Xref stream: claims object 2 at a bogus offset, and knows object 3.
    let stm_off = buf.len() as u64;
    let rows: Vec<[u8; 4]> = vec![
        [0, 0, 0, 255],
        [1, (off1 >> 8) as u8, off1 as u8, 0],
        [1, 0x30, 0x39, 0], // object 2 → bogus offset 12345
        [1, (off3 >> 8) as u8, off3 as u8, 0],
    ];
    let data: Vec<u8> = rows.iter().flatten().copied().collect();
    write!(buf, "4 0 obj\n<</Type/XRef/Size 4/W[1 2 1]/Length {}>>\nstream\n", data.len())
        .unwrap();
    buf.extend_from_slice(&data);
    write!(buf, "\nendstream\nendobj\n").unwrap();

    // Classic section: objects 1 and 2 only, /XRefStm pointing above.
    let xref_off = buf.len();
    write!(
        buf,
        "xref\n0 3\n0000000000 65535 f\r\n{off1:010} 00000 n\r\n{off2_table:010} 00000 n\r\n"
    )
    .unwrap();
    write!(
        buf,
        "trailer\n<</Size 4/Root 1 0 R/XRefStm {stm_off}>>\nstartxref\n{xref_off}\n%%EOF"
    )
    .unwrap();

    let structure = load(buf);
    // Table wins for object 2.
    assert_eq!(structure.xref.locate(ObjectId::new(2, 0)), ObjectLocation::Offset(off2_table));
    // Stream supplies object 3.
    assert_eq!(structure.xref.locate(ObjectId::new(3, 0)), ObjectLocation::Offset(off3));
}

#[test]
fn missing_startxref_triggers_rebuild() {
    let mut pdf = MiniPdf::new();
    let off1 = pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    let off2 = pdf.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    let mut buf = pdf.buf;
    write!(buf, "trailer\n<</Size 3/Root 1 0 R>>\n%%EOF").unwrap();

    let structure = load(buf);
    assert!(structure.recovery.iter().any(|e| matches!(e, RecoveryEvent::XrefRebuilt)));
    assert_eq!(structure.trailer.root, ObjectId::new(1, 0));
    assert_eq!(structure.xref.locate(ObjectId::new(1, 0)), ObjectLocation::Offset(off1));
    assert_eq!(structure.xref.locate(ObjectId::new(2, 0)), ObjectLocation::Offset(off2));
}

#[test]
fn rebuild_without_trailer_finds_catalog() {
    let mut pdf = MiniPdf::new();
    let off1 = pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    let buf = pdf.buf; // no xref, no trailer, no startxref

    let structure = load(buf);
    assert!(structure.recovery.iter().any(|e| matches!(e, RecoveryEvent::XrefRebuilt)));
    assert_eq!(structure.trailer.root, ObjectId::new(1, 0));
    assert_eq!(structure.xref.locate(ObjectId::new(1, 0)), ObjectLocation::Offset(off1));
}

#[test]
fn rebuild_last_object_occurrence_wins() {
    let mut pdf = MiniPdf::new();
    pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.add_object(2, "<</Old 1>>");
    let newer = pdf.add_object(2, "<</New 1>>");
    let structure = load(pdf.buf);
    assert_eq!(structure.xref.locate(ObjectId::new(2, 0)), ObjectLocation::Offset(newer));
}

#[test]
fn bogus_startxref_offset_recovers() {
    let mut pdf = MiniPdf::new();
    pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    let mut buf = pdf.finish_classic("/Root 1 0 R");
    // Corrupt the startxref operand.
    let pos = buf.windows(9).rposition(|w| w == b"startxref").unwrap();
    let end = buf[pos..].iter().position(|&b| b == b'%').unwrap() + pos;
    let patched = b"startxref\n99999999\n".to_vec();
    buf.splice(pos..end, patched);

    let structure = load(buf);
    // Recovered either by the xref-keyword scan or a full rebuild; both
    // must be observable and both must produce the right root.
    assert!(!structure.recovery.is_empty());
    assert_eq!(structure.trailer.root, ObjectId::new(1, 0));
    assert!(matches!(structure.xref.locate(ObjectId::new(1, 0)), ObjectLocation::Offset(_)));
}

#[test]
fn prev_chain_cycle_terminates() {
    // Two xref tables pointing at each other via /Prev.
    let mut pdf = MiniPdf::new();
    let off1 = pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    let mut buf = pdf.buf;
    let xref_a = buf.len();
    // Table A: object 1, /Prev → table B (below).
    // Compute B's offset after writing A — write A with a placeholder,
    // then B, then patch is messy; instead let A point forward to B.
    write!(buf, "xref\n0 2\n0000000000 65535 f\r\n{off1:010} 00000 n\r\n").unwrap();
    let prev_patch = buf.len();
    write!(buf, "trailer\n<</Size 3/Root 1 0 R/Prev PPPPPPPPPP>>\n").unwrap();
    let xref_b = buf.len();
    write!(buf, "xref\n0 2\n0000000000 65535 f\r\n{off1:010} 00000 n\r\n").unwrap();
    write!(buf, "trailer\n<</Size 3/Root 1 0 R/Prev {xref_a}>>\n").unwrap();
    write!(buf, "startxref\n{xref_a}\n%%EOF").unwrap();
    // Patch A's /Prev to point at B, zero-padded to placeholder width.
    let needle = b"PPPPPPPPPP";
    let at = buf[prev_patch..].windows(needle.len()).position(|w| w == needle).unwrap()
        + prev_patch;
    buf.splice(at..at + needle.len(), format!("{xref_b:010}").into_bytes());

    let structure = load(buf);
    // Terminates, keeps the document, and the cycle is observable.
    assert!(structure.recovery.iter().any(
        |e| matches!(e, RecoveryEvent::Other(msg) if msg.contains("cycle"))
    ));
    assert_eq!(structure.trailer.root, ObjectId::new(1, 0));
}

#[test]
fn revision_limit_enforced() {
    // A /Prev chain longer than max_revisions must fail, not hang.
    let mut pdf = MiniPdf::new();
    let off1 = pdf.add_object(1, "<</Type/Catalog>>");
    let mut buf = pdf.buf;
    let mut prev_off: Option<usize> = None;
    let mut last_xref = 0usize;
    for _ in 0..6 {
        last_xref = buf.len();
        write!(buf, "xref\n0 2\n0000000000 65535 f\r\n{off1:010} 00000 n\r\n").unwrap();
        match prev_off {
            Some(p) => write!(buf, "trailer\n<</Size 2/Root 1 0 R/Prev {p}>>\n").unwrap(),
            None => write!(buf, "trailer\n<</Size 2/Root 1 0 R>>\n").unwrap(),
        }
        prev_off = Some(last_xref);
    }
    write!(buf, "startxref\n{last_xref}\n%%EOF").unwrap();

    let names = NameTable::new();
    let limits = StructureLimits { max_revisions: 3, ..Default::default() };
    let source = OwnedBytesSource::new(buf);
    assert!(matches!(
        load_structure(&source, &names, &limits),
        Err(StructureError::LimitExceeded("max_revisions"))
    ));
}

#[test]
fn wrong_stream_length_repaired_in_xref_stream() {
    // An xref stream whose /Length lies: repaired by endstream scan.
    let mut pdf = MiniPdf::new();
    let off1 = pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    let off2 = pdf.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    let mut buf = pdf.buf;
    let xref_off = buf.len() as u64;
    let rows: Vec<[u8; 4]> = vec![
        [0, 0, 0, 255],
        [1, (off1 >> 8) as u8, off1 as u8, 0],
        [1, (off2 >> 8) as u8, off2 as u8, 0],
        [1, (xref_off >> 8) as u8, xref_off as u8, 0],
    ];
    let data: Vec<u8> = rows.iter().flatten().copied().collect();
    // Declared /Length 3 is wrong (real length 16).
    write!(buf, "3 0 obj\n<</Type/XRef/Size 4/W[1 2 1]/Root 1 0 R/Length 3>>\nstream\n")
        .unwrap();
    buf.extend_from_slice(&data);
    write!(buf, "\nendstream\nendobj\nstartxref\n{xref_off}\n%%EOF").unwrap();

    let structure = load(buf);
    assert!(structure.recovery.iter().any(|e| matches!(
        e,
        RecoveryEvent::StreamLengthRepaired { declared: Some(3), actual: 16, .. }
    )));
    assert_eq!(structure.xref.locate(ObjectId::new(1, 0)), ObjectLocation::Offset(off1));
}

#[test]
fn trailer_id_and_info_captured() {
    let mut pdf = MiniPdf::new();
    pdf.add_object(1, "<</Type/Catalog>>");
    pdf.add_object(2, "<</Producer(test)>>");
    let buf = pdf.finish_classic("/Root 1 0 R/Info 2 0 R/ID[<AB12><CD34>]");
    let structure = load(buf);
    assert_eq!(structure.trailer.info, Some(ObjectId::new(2, 0)));
    let id = structure.trailer.file_id.as_ref().expect("missing /ID");
    assert_eq!(id[0].as_bytes(), &[0xAB, 0x12]);
    assert_eq!(id[1].as_bytes(), &[0xCD, 0x34]);
    assert!(structure.trailer.encrypt.is_none());
}

#[test]
fn size_mismatch_recorded() {
    let mut pdf = MiniPdf::new();
    pdf.add_object(1, "<</Type/Catalog>>");
    let mut buf = pdf.buf;
    let xref_off = buf.len();
    // /Size 99 disagrees with the actual 2 entries.
    write!(
        buf,
        "xref\n0 2\n0000000000 65535 f\r\n0000000009 00000 n\r\ntrailer\n<</Size 99/Root 1 0 R>>\nstartxref\n{xref_off}\n%%EOF"
    )
    .unwrap();
    let structure = load(buf);
    assert!(structure.recovery.iter().any(|e| matches!(
        e,
        RecoveryEvent::SizeRepaired { declared: 99, actual: 2 }
    )));
}

#[test]
fn empty_input_is_no_header() {
    let names = NameTable::new();
    let source = OwnedBytesSource::new(Vec::new());
    assert!(matches!(
        load_structure(&source, &names, &StructureLimits::default()),
        Err(StructureError::NoHeader)
    ));
}

#[test]
fn trailer_values_resolve_newest_first() {
    // Incremental update changes /Info; /Root only in the base. The merged
    // trailer view takes each key from the newest revision that has it.
    let mut pdf = MiniPdf::new();
    pdf.add_object(1, "<</Type/Catalog>>");
    pdf.add_object(2, "<</Old/Info>>");
    let mut buf = pdf.finish_classic("/Root 1 0 R/Info 2 0 R");
    let base_xref = buf.windows(4).position(|w| w == b"xref").unwrap();
    let new_info = buf.len() as u64;
    write!(buf, "3 0 obj\n<</New/Info>>\nendobj\n").unwrap();
    let upd_xref = buf.len();
    write!(buf, "xref\n3 1\n{new_info:010} 00000 n\r\n").unwrap();
    write!(
        buf,
        "trailer\n<</Size 4/Info 3 0 R/Prev {base_xref}>>\nstartxref\n{upd_xref}\n%%EOF"
    )
    .unwrap();
    let structure = load(buf);
    assert_eq!(structure.trailer.root, ObjectId::new(1, 0));
    assert_eq!(structure.trailer.info, Some(ObjectId::new(3, 0)));
}

#[test]
fn object_zero_never_resolvable() {
    let structure = load(basic_two_object_pdf());
    assert_eq!(structure.xref.locate(ObjectId::new(0, 65535)), ObjectLocation::Free);
    assert_eq!(structure.xref.locate(ObjectId::new(0, 0)), ObjectLocation::Free);
}

/// Ported from PDFium ParserXRefTest.XrefIndexWithRepeatedObject: /Index
/// [2 2 3 1] declares object 3 twice. Either occurrence is acceptable per
/// PDFium's own comment; our newest-first first-write-wins merge keeps the
/// first one (offset 15).
#[test]
fn pdfium_xref_index_with_repeated_object() {
    let data: &[u8] = b"%PDF1-7\n%\xa0\xf2\xa4\xf4\n7 0 obj <<\n  /Filter /ASCIIHexDecode\n  /Root 1 0 R\n  /Size 4\n  /Index [2 2 3 1]\n  /W [1 1 1]\n>>\nstream\n01 00 00\n01 0F 00\n01 12 00\nendstream\nendobj\nstartxref\n14\n%%EOF\n";
    let structure = load(data.to_vec());
    assert_eq!(structure.xref.locate(ObjectId::new(2, 0)), ObjectLocation::Offset(0));
    assert_eq!(structure.xref.locate(ObjectId::new(3, 0)), ObjectLocation::Offset(15));
}

/// Ported from PDFium ParserXRefTest.XrefIndexWithOutOfOrderObjects:
/// /Index [3 2 2 1] is not ascending; tolerated, same values as PDFium.
#[test]
fn pdfium_xref_index_out_of_order() {
    let data: &[u8] = b"%PDF1-7\n%\xa0\xf2\xa4\xf4\n7 0 obj <<\n  /Filter /ASCIIHexDecode\n  /Root 1 0 R\n  /Size 5\n  /Index [3 2 2 1]\n  /W [1 1 1]\n>>\nstream\n01 00 00\n01 0F 00\n01 12 00\nendstream\nendobj\nstartxref\n14\n%%EOF\n";
    let structure = load(data.to_vec());
    assert_eq!(structure.xref.locate(ObjectId::new(3, 0)), ObjectLocation::Offset(0));
    assert_eq!(structure.xref.locate(ObjectId::new(4, 0)), ObjectLocation::Offset(15));
    assert_eq!(structure.xref.locate(ObjectId::new(2, 0)), ObjectLocation::Offset(18));
    // The malformed "%PDF1-7" version is tolerated (defaults to 1.7).
    assert_eq!(structure.version.major, 1);
    assert_eq!(structure.version.minor, 7);
}

#[test]
fn objstm_layout_parses_from_fixture() {
    // Cross-check the objstm module against the same bytes the xref-stream
    // fixture embeds.
    let names = NameTable::new();
    let _ = &names;
    let data = b"4 0 5 8 <</A 1>> <</B 2>>";
    let limits = pdf_syntax::SyntaxLimits::default();
    let layout = pdf_structure::objstm::parse_object_stream_layout(data, 2, 8, &limits).unwrap();
    assert_eq!(layout.members.len(), 2);
    assert_eq!(layout.members[0].number, 4);
    assert_eq!(layout.members[0].offset, 8);
    assert_eq!(layout.members[1].number, 5);
    assert_eq!(layout.members[1].offset, 16);
}

/// Build a classic-xref fixture whose `startxref` operand is wrong (forcing the
/// keyword-scan recovery) and whose entry for object 3 points 18 bytes past the
/// real header — the flate_predictor_bpc_1 shape, where `/Root` still resolves
/// but the page's content object does not.
fn stale_offset_fixture() -> (Vec<u8>, u64) {
    let mut pdf = MiniPdf::new();
    pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    let off3 = pdf.add_object(3, "<</Type/Page/Parent 2 0 R>>");
    let mut buf = pdf.finish_classic("/Root 1 0 R");

    // Shift only object 3's entry: the catalog keeps resolving, so nothing but
    // an entry-level check can notice the corruption.
    let stale = format!("{:010}", off3 + 18);
    // `rposition` on "xref" would land inside "startxref"; match the table's
    // own line start instead.
    let xref_off = buf.windows(5).rposition(|w| w == b"\nxref").unwrap();
    let needle = format!("{off3:010}").into_bytes();
    let at = buf[xref_off..].windows(needle.len()).position(|w| w == needle).unwrap() + xref_off;
    buf.splice(at..at + needle.len(), stale.into_bytes());
    (buf, off3)
}

#[test]
fn recovered_chain_with_stale_offsets_rebuilds() {
    let (mut buf, off3) = stale_offset_fixture();
    // Break the startxref operand so the chain is only found by scanning.
    let pos = buf.windows(9).rposition(|w| w == b"startxref").unwrap();
    let end = buf[pos..].iter().position(|&b| b == b'%').unwrap() + pos;
    buf.splice(pos..end, b"startxref\n99999999\n".to_vec());

    let structure = load(buf);
    assert!(structure.recovery.iter().any(|e| matches!(e, RecoveryEvent::XrefRebuilt)));
    // The rebuild relocates object 3 to its real header, not the stale offset.
    assert_eq!(structure.xref.locate(ObjectId::new(3, 0)), ObjectLocation::Offset(off3));
    assert_eq!(structure.trailer.root, ObjectId::new(1, 0));
}

#[test]
fn reported_chain_with_stale_offsets_is_kept() {
    // Same corruption, but `startxref` is correct. PDFium keeps such a table
    // (its VerifyCrossRefTable checks only the first entry), and so do we: a
    // reported offset is as authoritative as the file gets.
    let (buf, off3) = stale_offset_fixture();
    let structure = load(buf);
    assert!(!structure.recovery.iter().any(|e| matches!(e, RecoveryEvent::XrefRebuilt)));
    assert_eq!(structure.xref.locate(ObjectId::new(3, 0)), ObjectLocation::Offset(off3 + 18));
}

/// A producer that writes the subsection header `1 N` while still emitting the
/// object-0 free-list head as the first entry shifts every object number by
/// one. Read literally, the real object 1 is recorded free and every reference
/// to it dies — in pdfjs/issue7229 that is the page's only image, so the page
/// rendered blank. The free-list head is unambiguous, so correct the start.
#[test]
fn a_subsection_declared_at_one_but_starting_with_the_free_head_is_corrected() {
    let mut pdf = MiniPdf::new();
    let cat = pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    let pages = pdf.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");

    // Hand-built xref with the off-by-one subsection header.
    let xref_off = pdf.buf.len();
    write!(pdf.buf, "xref\n1 3\n").unwrap();
    write!(pdf.buf, "0000000000 65535 f\r\n").unwrap();
    write!(pdf.buf, "{cat:010} 00000 n\r\n").unwrap();
    write!(pdf.buf, "{pages:010} 00000 n\r\n").unwrap();
    write!(
        pdf.buf,
        "trailer\n<</Size 3 /Root 1 0 R>>\nstartxref\n{xref_off}\n%%EOF"
    )
    .unwrap();

    let doc = load(pdf.buf);
    // Object 1 must resolve to the catalog, not be a free entry.
    let at = |n: u32| match doc.xref.locate(ObjectId::new(n, 0)) {
        ObjectLocation::Offset(o) => Some(o),
        _ => None,
    };
    assert_eq!(at(1), Some(cat), "object 1 is the catalog, not a free entry");
    assert_eq!(at(2), Some(pages), "object 2 is the page tree");
}
