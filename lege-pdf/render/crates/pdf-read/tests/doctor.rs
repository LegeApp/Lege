#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Doctor (`pdf_read::examine`) integration tests over corpus-style
//! fixtures: clean, recovered, encrypted, and unopenable documents.

use std::sync::Arc;

use pdf_document::DocumentLimits;
use pdf_read::{CompileStatus, OpenOutcome, examine};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::{self, PdfBuilder};

fn examine_bytes(bytes: Vec<u8>) -> pdf_read::DocumentReport {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    examine(source, DocumentLimits::default())
}

#[test]
fn clean_document_reports_ok() {
    let report = examine_bytes(builder::multipage_classic(2));
    assert!(
        matches!(report.open, OpenOutcome::Ok),
        "open: {:?}",
        report.open
    );
    assert!(!report.xref.recovery_used);
    assert!(!report.xref.rebuilt);
    assert_eq!(report.xref.revision_count, 1);
    assert!(report.encryption.is_none());
    assert_eq!(report.pages.len(), 2);
    for page in &report.pages {
        assert!(
            page.media_box_present,
            "page {} inherits /MediaBox",
            page.index
        );
        assert!(
            matches!(page.compile, CompileStatus::Ok { .. }),
            "page {}: {:?}",
            page.index,
            page.compile
        );
    }
    assert!(!report.features.uses_object_streams);
    assert_eq!(report.annotations.total(), 0);
    // Summary renders and mentions the essentials.
    let summary = report.summary();
    assert!(summary.contains("open: ok"), "{summary}");
    assert!(summary.contains("pages: 2"), "{summary}");
}

#[test]
fn object_streams_are_detected() {
    let report = examine_bytes(builder::xref_stream_fixture(1));
    assert!(
        matches!(report.open, OpenOutcome::Ok),
        "open: {:?}",
        report.open
    );
    assert!(report.features.uses_object_streams);
}

#[test]
fn recovered_document_reports_recovery_used() {
    let report = examine_bytes(builder::without_startxref(builder::multipage_classic(2)));
    match &report.open {
        OpenOutcome::Recovered { how } => {
            assert!(!how.is_empty(), "recovery must be described");
        }
        other => panic!("expected Recovered, got {other:?}"),
    }
    assert!(report.xref.recovery_used);
    assert_eq!(report.pages.len(), 2);
    let summary = report.summary();
    assert!(summary.contains("recovered"), "{summary}");
}

#[test]
fn rebuilt_document_reports_rebuild() {
    let report = examine_bytes(builder::without_xref(builder::multipage_classic(1)));
    assert!(report.xref.recovery_used);
    assert!(report.xref.rebuilt, "full scan rebuild must be flagged");
    assert!(
        matches!(report.open, OpenOutcome::Recovered { .. }),
        "open: {:?}",
        report.open
    );
}

#[test]
fn rc4_encrypted_document_reports_scheme() {
    let report = examine_bytes(builder::encrypted_fixture());
    assert!(
        matches!(report.open, OpenOutcome::Ok),
        "open: {:?}",
        report.open
    );
    let enc = report.encryption.expect("encryption info present");
    assert_eq!(enc.version, 2);
    assert_eq!(enc.revision, 3);
    assert_eq!(enc.method, "RC4-128");
    assert!(
        enc.user_password_empty,
        "opened via the empty user password"
    );
}

#[test]
fn aes256_fixture_fails_open_but_still_reports_encryption() {
    // The bare-U aes256 fixture is declined by the handler; open fails, yet
    // the doctor's structural fallback still names the declared scheme.
    let report = examine_bytes(builder::aes256_encrypted_fixture());
    assert!(
        matches!(report.open, OpenOutcome::Failed { .. }),
        "open: {:?}",
        report.open
    );
    let enc = report.encryption.expect("declared scheme still reported");
    assert_eq!(enc.version, 5);
    assert_eq!(enc.revision, 6);
    assert_eq!(enc.method, "AES-256");
    assert!(!enc.user_password_empty);
    // Structural layer itself is healthy.
    assert_eq!(report.xref.revision_count, 1);
    assert!(!report.xref.recovery_used);
}

#[test]
fn garbage_reports_failed_and_nothing_else() {
    let report = examine_bytes(b"this is not a pdf at all".to_vec());
    assert!(matches!(report.open, OpenOutcome::Failed { .. }));
    assert!(report.pages.is_empty());
    assert!(report.encryption.is_none());
    let summary = report.summary();
    assert!(summary.contains("FAILED"), "{summary}");
}

/// Fixture with annotations, an AcroForm (+XFA), outlines, optional
/// content, and document JavaScript.
fn featureful_fixture() -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_object(
        1,
        "<</Type/Catalog/Pages 2 0 R/AcroForm 7 0 R/Outlines 8 0 R\
         /OCProperties<</OCGs[]/D<<>>>>/Names<</JavaScript 9 0 R>>>>",
    );
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Annots[5 0 R 6 0 R 10 0 R]>>",
    );
    b.add_stream(4, "", b"0 0 10 10 re f");
    b.add_object(5, "<</Type/Annot/Subtype/Link/Rect[0 0 10 10]>>");
    b.add_object(6, "<</Type/Annot/Subtype/Widget/Rect[0 0 10 10]>>");
    b.add_object(10, "<</Type/Annot/Subtype/Link/Rect[20 20 30 30]>>");
    b.add_object(7, "<</Fields[]/XFA(stub)>>");
    b.add_object(8, "<</Type/Outlines/Count 0>>");
    b.add_object(9, "<</Names[(doc-open)11 0 R]>>");
    b.add_object(11, "<</S/JavaScript/JS(app.alert\\(1\\))>>");
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}

#[test]
fn features_and_annotations_are_inventoried() {
    let report = examine_bytes(featureful_fixture());
    assert!(
        matches!(report.open, OpenOutcome::Ok),
        "open: {:?}",
        report.open
    );
    let f = &report.features;
    assert!(f.has_acroform);
    assert!(f.has_xfa);
    assert!(f.has_javascript);
    assert!(f.has_outlines);
    assert!(f.has_optional_content);
    assert!(!f.uses_object_streams);

    assert_eq!(report.annotations.total(), 3);
    assert_eq!(report.annotations.count_per_subtype.get("Link"), Some(&2));
    assert_eq!(report.annotations.count_per_subtype.get("Widget"), Some(&1));

    // media box present directly on the pages node (inherited).
    assert!(report.pages[0].media_box_present);
    match &report.pages[0].compile {
        CompileStatus::Ok { op_count } => assert!(*op_count > 0),
        other => panic!("expected Ok compile, got {other:?}"),
    }

    let summary = report.summary();
    assert!(summary.contains("Link: 2"), "{summary}");
    assert!(summary.contains("javascript=true"), "{summary}");
}

#[test]
fn header_version_and_leading_garbage_are_reported() {
    let report = examine_bytes(builder::multipage_classic(1));
    let header = report
        .header
        .as_ref()
        .expect("a clean fixture has a header");
    assert_eq!((header.major, header.minor), (1, 7));
    assert_eq!(header.header_offset, 0);
    assert!(
        report.summary().contains("%PDF-1.7"),
        "{}",
        report.summary()
    );

    // Leading garbage shifts every structural offset; the doctor must say by
    // how much rather than silently absorbing it.
    let junk = b"%!PS-Adobe-3.0\n";
    let report = examine_bytes(builder::with_leading_garbage(
        builder::multipage_classic(1),
        junk,
    ));
    let header = report
        .header
        .as_ref()
        .expect("the header is found past the garbage");
    assert_eq!(header.header_offset, junk.len() as u64);
    let summary = report.summary();
    assert!(summary.contains("leading garbage"), "{summary}");
}

#[test]
fn a_document_that_will_not_open_still_reports_its_lineage() {
    // /Encrypt with a scheme the handler declines: open fails, but the trailer
    // /ID is never encrypted, so lineage survives.
    let report = examine_bytes(builder::aes256_encrypted_fixture());
    assert!(matches!(report.open, OpenOutcome::Failed { .. }));
    assert_eq!(report.identity.id_permanent.as_deref(), Some("01"));
    assert_eq!(report.identity.id_changing.as_deref(), Some("02"));
    assert!(report.header.is_some());
    // /Info strings are encrypted like any others, so they stay out of reach.
    assert!(report.identity.info.is_empty());
}

#[test]
fn garbage_reports_no_header_at_all() {
    let report = examine_bytes(b"this is not a pdf at all".to_vec());
    assert!(report.header.is_none());
    assert!(report.identity.id_permanent.is_none());
}

/// Fixture with an /Info dictionary, an XMP metadata stream, an embedded file
/// and a trailer /ID pair.
fn identity_fixture() -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_object(
        1,
        "<</Type/Catalog/Pages 2 0 R/Metadata 5 0 R\
         /Names<</EmbeddedFiles 6 0 R>>>>",
    );
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(3, "<</Type/Page/Parent 2 0 R>>");
    b.add_object(
        4,
        "<</Producer(Acrobat Distiller 23.0)/Creator(Microsoft Word)\
         /ModDate(D:20260713120000Z)/CustomKey(private)>>",
    );
    b.add_stream(5, "/Type/Metadata/Subtype/XML", b"<x:xmpmeta/>");
    b.add_object(6, "<</Names[]>>");
    b.finish_classic_xref("/Root 1 0 R/Info 4 0 R/ID[<deadbeef><0badf00d>]");
    b.into_bytes()
}

#[test]
fn identity_and_metadata_are_inventoried() {
    let report = examine_bytes(identity_fixture());
    assert!(matches!(report.open, OpenOutcome::Ok), "{:?}", report.open);

    let id = &report.identity;
    assert_eq!(id.id_permanent.as_deref(), Some("deadbeef"));
    assert_eq!(id.id_changing.as_deref(), Some("0badf00d"));
    assert!(id.has_xmp_metadata);
    assert_eq!(
        id.info.get("Producer").map(String::as_str),
        Some("Acrobat Distiller 23.0")
    );
    assert_eq!(
        id.info.get("Creator").map(String::as_str),
        Some("Microsoft Word")
    );
    // Non-standard keys are kept: a writer that invents one identifies itself.
    assert_eq!(
        id.info.get("CustomKey").map(String::as_str),
        Some("private")
    );

    assert!(report.features.has_embedded_files);
    let summary = report.summary();
    assert!(summary.contains("deadbeef"), "{summary}");
    assert!(summary.contains("embedded-files=true"), "{summary}");
}

/// A document carrying a real XMP packet with media-management lineage: same
/// DocumentID as its ancestor, a fresh InstanceID, an explicit DerivedFrom,
/// and a two-event history ending in an Acrobat save.
fn xmp_lineage_fixture() -> Vec<u8> {
    let packet = br#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF
 xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
 <rdf:Description rdf:about="" xmlns:xmpMM="http://ns.adobe.com/xap/1.0/mm/"
   xmpMM:DocumentID="xmp.did:ORIGINAL-1234"
   xmpMM:InstanceID="xmp.iid:RESAVED-9999">
  <xmpMM:DerivedFrom rdf:parseType="Resource">
   <stRef:documentID>xmp.did:ORIGINAL-1234</stRef:documentID>
   <stRef:instanceID>xmp.iid:FIRSTSAVE-0001</stRef:instanceID>
  </xmpMM:DerivedFrom>
  <xmpMM:History><rdf:Seq>
   <rdf:li stEvt:action="created" stEvt:when="2024-12-01T12:00:00Z"
     stEvt:softwareAgent="Microsoft Word"/>
   <rdf:li stEvt:action="saved" stEvt:when="2026-07-13T09:30:00Z"
     stEvt:softwareAgent="Adobe Acrobat 23.0" stEvt:changed="/"/>
  </rdf:Seq></xmpMM:History>
 </rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#;

    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R/Metadata 5 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(3, "<</Type/Page/Parent 2 0 R>>");
    b.add_stream(5, "/Type/Metadata/Subtype/XML", packet);
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}

#[test]
fn xmp_lineage_is_read_from_the_metadata_stream() {
    let report = examine_bytes(xmp_lineage_fixture());
    assert!(matches!(report.open, OpenOutcome::Ok), "{:?}", report.open);
    assert!(report.identity.has_xmp_metadata);

    let xmp = report.identity.xmp.as_ref().expect("lineage present");
    // The signature of a save-and-edit cycle: the document identity carried
    // over while the instance identity was minted fresh.
    assert_eq!(xmp.document_id.as_deref(), Some("xmp.did:ORIGINAL-1234"));
    assert_eq!(xmp.instance_id.as_deref(), Some("xmp.iid:RESAVED-9999"));
    assert_eq!(
        xmp.derived_from_document_id.as_deref(),
        Some("xmp.did:ORIGINAL-1234")
    );
    assert_eq!(
        xmp.derived_from_instance_id.as_deref(),
        Some("xmp.iid:FIRSTSAVE-0001")
    );

    assert_eq!(xmp.history.len(), 2);
    assert_eq!(
        xmp.history[0].software_agent.as_deref(),
        Some("Microsoft Word")
    );
    assert_eq!(xmp.history[1].action.as_deref(), Some("saved"));
    assert_eq!(
        xmp.history[1].software_agent.as_deref(),
        Some("Adobe Acrobat 23.0")
    );

    let summary = report.summary();
    assert!(summary.contains("xmp.did:ORIGINAL-1234"), "{summary}");
    assert!(summary.contains("Adobe Acrobat 23.0"), "{summary}");
}

#[test]
fn an_xmp_packet_without_lineage_reports_presence_only() {
    // identity_fixture's packet is `<x:xmpmeta/>` — present, but carrying no
    // media-management properties. Presence and lineage stay distinguishable.
    let report = examine_bytes(identity_fixture());
    assert!(report.identity.has_xmp_metadata);
    assert!(report.identity.xmp.is_none());
}

/// A signed document whose `/ByteRange` is patched, after the bytes are final,
/// to span the file exactly the way a real signer does. `split` is where the
/// two covered ranges meet; leaving `gap` bytes uncovered at the end models a
/// signature that does not reach the end of the file.
fn signed_fixture(gap: usize) -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R/AcroForm 6 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Annots[4 0 R]>>");
    // The signature field, carrying its signature dictionary.
    b.add_object(
        4,
        "<</Type/Annot/Subtype/Widget/FT/Sig/T(Signature1)/Rect[0 0 10 10]/V 5 0 R>>",
    );
    // Ten-digit zero-padded slots, overwritten in place below so the xref
    // offsets built around them stay correct.
    b.add_object(
        5,
        "<</Type/Sig/Filter/Adobe.PPKLite/SubFilter/ETSI.CAdES.detached\
         /M(D:20241201120000Z)/ByteRange[0 0000000010 0000000010 0000000000]>>",
    );
    // A second, unsigned signature field.
    b.add_object(7, "<</FT/Sig/T(Signature2)>>");
    b.add_object(6, "<</Fields[4 0 R 7 0 R]>>");
    b.finish_classic_xref("/Root 1 0 R");
    let mut bytes = b.into_bytes();

    let total = bytes.len();
    let tail = total - 10 - gap;
    let patched = format!("{tail:010}");
    let needle = b"[0 0000000010 0000000010 0000000000]";
    let at = bytes
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("the ByteRange placeholder is present");
    let last = at + needle.len() - 11;
    bytes[last..last + 10].copy_from_slice(patched.as_bytes());
    bytes
}

#[test]
fn a_signature_covering_the_whole_file_is_reported_as_such() {
    let report = examine_bytes(signed_fixture(0));
    assert!(matches!(report.open, OpenOutcome::Ok), "{:?}", report.open);

    assert_eq!(report.signatures.signatures.len(), 1);
    assert_eq!(report.signatures.unsigned_fields, 1);

    let sig = &report.signatures.signatures[0];
    assert_eq!(sig.field_name.as_deref(), Some("Signature1"));
    assert_eq!(sig.filter.as_deref(), Some("Adobe.PPKLite"));
    assert_eq!(sig.sub_filter.as_deref(), Some("ETSI.CAdES.detached"));
    assert_eq!(sig.signed_at.as_deref(), Some("D:20241201120000Z"));
    assert_eq!(sig.byte_range.len(), 4);
    assert!(
        sig.covers_whole_file,
        "byte_range {:?} should span the file",
        sig.byte_range
    );

    let summary = report.summary();
    assert!(summary.contains("1 signed, 1 unsigned"), "{summary}");
    assert!(summary.contains("covers whole file"), "{summary}");
}

#[test]
fn a_signature_leaving_bytes_uncovered_is_flagged() {
    // Twenty bytes past the signed range: the structural shape of an
    // incremental update appended after signing.
    let report = examine_bytes(signed_fixture(20));
    let sig = &report.signatures.signatures[0];
    assert!(
        !sig.covers_whole_file,
        "byte_range {:?} leaves 20 bytes unsigned",
        sig.byte_range
    );

    let summary = report.summary();
    assert!(summary.contains("DOES NOT cover whole file"), "{summary}");
}

#[test]
fn a_document_without_signatures_reports_none() {
    let report = examine_bytes(builder::multipage_classic(1));
    assert!(report.signatures.signatures.is_empty());
    assert_eq!(report.signatures.unsigned_fields, 0);
    assert!(report.summary().contains("signatures: none"));
}

#[test]
fn wrong_stream_length_surfaces_as_degraded_or_recovered() {
    // The lazy /Length repair happens during content gathering; the doctor
    // must surface it (either as an open-time recovery or per-page
    // degradation) — never silently.
    let report = examine_bytes(builder::wrong_length_fixture());
    let page_degraded = matches!(&report.pages[0].compile, CompileStatus::Degraded { .. });
    let recovered = report.xref.recovery_used;
    assert!(
        page_degraded || recovered,
        "repair must be visible: page={:?} open={:?}",
        report.pages[0].compile,
        report.open
    );
}

#[test]
fn a_vector_page_reports_text_and_a_font_not_an_image() {
    let report = examine_bytes(builder::multipage_classic(1));
    let metrics = report.pages[0].metrics.as_ref().expect("compiled");
    assert!(metrics.visible_text_runs >= 1, "{metrics:?}");
    assert_eq!(metrics.images, 0);
    assert_eq!(metrics.image_coverage_bps, 0);
    assert_eq!(report.fonts.len(), 1);
    assert_eq!(report.fonts[0].base_font, "Helvetica");
    assert!(!report.fonts[0].embedded);
    assert!(report.images.is_empty());
}

#[test]
fn a_full_page_image_reports_complete_coverage() {
    let report = examine_bytes(full_page_image_fixture(false));
    let metrics = report.pages[0].metrics.as_ref().expect("compiled");
    assert_eq!(metrics.images, 1);
    assert_eq!(metrics.visible_text_runs, 0);
    assert_eq!(metrics.image_coverage_bps, 10_000);
    assert_eq!(metrics.max_image_coverage_bps, 10_000);
    assert_eq!(report.images.len(), 1);
    assert_eq!((report.images[0].width, report.images[0].height), (2, 2));
    // 2×2 pixels painted into a 72×72 pt square is 2 dpi.
    assert_eq!(metrics.effective_dpi, Some(2));
}

#[test]
fn an_invisible_text_layer_is_counted_separately() {
    let report = examine_bytes(full_page_image_fixture(true));
    let metrics = report.pages[0].metrics.as_ref().expect("compiled");
    assert_eq!(metrics.images, 1);
    assert_eq!(metrics.image_coverage_bps, 10_000);
    assert_eq!(metrics.visible_text_runs, 0);
    assert!(metrics.invisible_text_runs >= 1, "{metrics:?}");
}

#[test]
fn a_blank_page_reports_zero_metrics_not_a_gap() {
    let report = examine_bytes(blank_page_fixture());
    let metrics = report.pages[0].metrics.as_ref().expect("compiled");
    assert_eq!(metrics.text_runs, 0);
    assert_eq!(metrics.images, 0);
    assert_eq!(metrics.path_paints, 0);
    assert_eq!(metrics.image_coverage_bps, 0);
}

/// One 72×72 pt page painted with a 2×2 DeviceRGB image covering the crop.
/// When `ocr_layer` is set, an invisible (`Tr 3`) Helvetica run is added.
fn full_page_image_fixture(ocr_layer: bool) -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 72 72]>>");
    let resources = if ocr_layer {
        "/XObject<</Im 5 0 R>>/Font<</F1 6 0 R>>"
    } else {
        "/XObject<</Im 5 0 R>>"
    };
    b.add_object(
        3,
        &format!("<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<{resources}>>>>"),
    );
    let content = if ocr_layer {
        b"q 72 0 0 72 0 0 cm /Im Do Q BT 3 Tr /F1 12 Tf 0 0 Td (hidden) Tj ET".as_slice()
    } else {
        b"q 72 0 0 72 0 0 cm /Im Do Q".as_slice()
    };
    b.add_stream(4, "", content);
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 2/BitsPerComponent 8/ColorSpace/DeviceRGB",
        &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
    );
    if ocr_layer {
        b.add_object(6, "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>");
    }
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}

fn blank_page_fixture() -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 72 72]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R>>");
    b.add_stream(4, "", b"");
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}
