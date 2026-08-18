//! End-to-end tests for [`pdf_forensics::audit_bytes`] over real PDF bytes.
//!
//! These cover the seam that matters most: that a document which does not open
//! still produces a truthful report rather than an empty-looking clean one.
#![allow(clippy::unwrap_used, reason = "a failed unwrap is a failed test")]

use pdf_forensics::{
    AnalysisMode, Availability, Disposition, OpenState, UnavailableReason, audit_bytes,
};
use pdf_test_support::builder;

/// SHA-256 of the empty input, from the published test vectors.
const SHA256_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[test]
fn file_identity_is_computed_over_the_raw_bytes() {
    // Not a PDF at all: identity must still be reported, because it is a fact
    // about the file rather than about the document.
    let report = audit_bytes(b"", None);
    assert_eq!(report.document.identity.sha256, SHA256_EMPTY);
    assert_eq!(report.document.identity.size_bytes, 0);

    let pdf = builder::multipage_classic(3);
    let report = audit_bytes(&pdf, None);
    assert_eq!(report.document.identity.size_bytes, pdf.len() as u64);
    assert_eq!(report.document.identity.sha256.len(), 64);
    assert!(
        report
            .document
            .identity
            .sha256
            .chars()
            .all(|c| c.is_ascii_hexdigit())
    );
}

#[test]
fn a_clean_document_audits_in_full() {
    let report = audit_bytes(&builder::multipage_classic(3), None);
    report.validate().unwrap();

    assert_eq!(report.document.parse.state, OpenState::Clean);
    assert!(!report.document.parse.rests_on_recovered_structure());
    assert!(!report.document.parse.xref_rebuilt);
    assert_eq!(report.analysis_mode, AnalysisMode::Full);

    let pages = report.document.pages.value().unwrap();
    assert_eq!(pages.len(), 3);
    assert!(
        pages
            .iter()
            .all(|p| p.representation == pdf_forensics::PageRepresentation::Vector),
        "{pages:?}"
    );
    assert!(report.document.features.is_available());
    assert!(report.document.annotations.is_available());
    let fonts = report.document.fonts.value().unwrap();
    assert_eq!(fonts.len(), 1);
    assert_eq!(fonts[0].base_font, "Helvetica");
    assert!(report.document.images.value().unwrap().is_empty());
    // Unencrypted is a determined answer, not a gap.
    assert_eq!(report.document.encryption.value(), Some(&None));
    assert!(
        !report.document.pending.iter().any(|p| {
            matches!(
                p.section.as_str(),
                "page_representation" | "fonts" | "images"
            )
        }),
        "visual sections should no longer be pending: {:?}",
        report.document.pending
    );
}

#[test]
fn recovery_is_recorded_rather_than_absorbed() {
    let broken = builder::without_xref(builder::multipage_classic(2));
    let report = audit_bytes(&broken, None);
    report.validate().unwrap();

    assert_eq!(report.document.parse.state, OpenState::Recovered);
    assert!(report.document.parse.rests_on_recovered_structure());
    assert!(!report.document.parse.recovery_events.is_empty());
    // It opened, so the audit is full — but every later claim can see that the
    // structure under it was repaired.
    assert_eq!(report.analysis_mode, AnalysisMode::Full);
}

#[test]
fn an_unopenable_file_reports_gaps_not_zeroes() {
    let report = audit_bytes(b"this is not a PDF at all", None);
    report.validate().unwrap();

    assert!(matches!(
        report.document.parse.state,
        OpenState::Failed { .. }
    ));
    assert_eq!(report.analysis_mode, AnalysisMode::StructureOnly);

    // The crux: nothing readable must look like nothing present.
    for section in [
        &report.document.features.is_available(),
        &report.document.annotations.is_available(),
        &report.document.pages.is_available(),
        &report.document.fonts.is_available(),
        &report.document.images.is_available(),
    ] {
        assert!(
            !section,
            "an unreadable section must not report as available"
        );
    }
    assert!(matches!(
        report.document.encryption,
        Availability::Unavailable {
            reason: UnavailableReason::Unparseable,
            ..
        }
    ));
}

#[test]
fn the_header_survives_a_document_that_will_not_open() {
    // AES-256, declined by the handler: the document does not open, but the
    // header and the trailer /ID need no decryption.
    let report = audit_bytes(&pdf_test_support::builder::aes256_encrypted_fixture(), None);
    report.validate().unwrap();
    assert_eq!(report.analysis_mode, AnalysisMode::StructureOnly);

    let header = report.document.header.value().unwrap();
    assert_eq!((header.major, header.minor), (1, 7));
    assert_eq!(report.document.lineage.id_permanent.as_deref(), Some("01"));
    assert_eq!(report.document.lineage.id_changing.as_deref(), Some("02"));

    // /Info strings are encrypted, so that half is an explicit gap.
    assert!(matches!(
        report.document.lineage.metadata,
        Availability::Unavailable {
            reason: UnavailableReason::Encrypted,
            ..
        }
    ));
}

#[test]
fn a_file_with_no_header_says_so_rather_than_guessing() {
    let report = audit_bytes(b"this is not a PDF at all", None);
    assert!(matches!(
        report.document.header,
        Availability::Unavailable {
            reason: UnavailableReason::Unparseable,
            ..
        }
    ));
    assert!(report.document.lineage.id_permanent.is_none());
}

#[test]
fn leading_garbage_is_reported_as_an_offset() {
    let junk = b"%!PS-Adobe-3.0\n";
    let report = audit_bytes(
        &pdf_test_support::builder::with_leading_garbage(builder::multipage_classic(1), junk),
        None,
    );
    let header = report.document.header.value().unwrap();
    assert_eq!(header.header_offset, junk.len() as u64);
}

#[test]
fn a_document_without_signatures_reports_an_empty_inventory_not_a_gap() {
    let report = audit_bytes(&builder::multipage_classic(1), None);
    // Determined and empty — distinct from unavailable.
    let signatures = report.document.signatures.value().unwrap();
    assert!(signatures.signatures.is_empty());
    assert_eq!(signatures.unsigned_fields, 0);
}

/// A document whose XMP records a save-and-edit cycle: the DocumentID carried
/// over from an ancestor while the InstanceID was minted fresh.
fn xmp_lineage_pdf() -> Vec<u8> {
    let packet = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF
 xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
 <rdf:Description rdf:about="" xmlns:xmpMM="http://ns.adobe.com/xap/1.0/mm/"
   xmpMM:DocumentID="xmp.did:ANCESTOR" xmpMM:InstanceID="xmp.iid:RESAVED">
  <xmpMM:DerivedFrom rdf:parseType="Resource">
   <stRef:instanceID>xmp.iid:ORIGINAL</stRef:instanceID>
  </xmpMM:DerivedFrom>
  <xmpMM:History><rdf:Seq>
   <rdf:li stEvt:action="saved" stEvt:when="2026-07-13T09:30:00Z"
     stEvt:softwareAgent="Adobe Acrobat 23.0"/>
  </rdf:Seq></xmpMM:History>
 </rdf:Description></rdf:RDF></x:xmpmeta>"#;

    let mut b = pdf_test_support::builder::PdfBuilder::new();
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
fn xmp_lineage_reaches_the_report() {
    let report = audit_bytes(&xmp_lineage_pdf(), None);
    report.validate().unwrap();

    let metadata = report.document.lineage.metadata.value().unwrap();
    assert!(metadata.has_xmp_metadata);
    let xmp = metadata.xmp.as_ref().unwrap();

    assert_eq!(xmp.document_id.as_deref(), Some("xmp.did:ANCESTOR"));
    assert_eq!(xmp.instance_id.as_deref(), Some("xmp.iid:RESAVED"));
    assert_eq!(
        xmp.derived_from_instance_id.as_deref(),
        Some("xmp.iid:ORIGINAL")
    );
    assert_eq!(xmp.history.len(), 1);
    assert_eq!(
        xmp.history[0].software_agent.as_deref(),
        Some("Adobe Acrobat 23.0")
    );

    // The whole point of surfacing it: the packet is no longer a pending gap.
    assert!(
        !report
            .document
            .pending
            .iter()
            .any(|p| p.section == "xmp_contents"),
        "xmp_contents should no longer be pending"
    );
}

#[test]
fn a_document_with_no_xmp_reports_absence_not_a_gap() {
    let report = audit_bytes(&builder::multipage_classic(1), None);
    let metadata = report.document.lineage.metadata.value().unwrap();
    assert!(!metadata.has_xmp_metadata);
    assert!(metadata.xmp.is_none());
}

#[test]
fn a_partial_inventory_cannot_declare_an_all_clear() {
    let report = audit_bytes(&builder::multipage_classic(1), None);

    // This build does not reach the whole M1 inventory, and says so.
    assert!(!report.document.pending.is_empty());
    assert_eq!(report.disposition, Disposition::Inconclusive);

    // And the schema refuses to let it claim otherwise.
    let mut overclaiming = report.clone();
    overclaiming.disposition = Disposition::NoReviewIndicatorFound;
    assert!(overclaiming.validate().is_err());
}

#[test]
fn a_full_page_image_classifies_as_raster_only() {
    let report = audit_bytes(&full_page_image_pdf(false), None);
    report.validate().unwrap();
    let pages = report.document.pages.value().unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(
        pages[0].representation,
        pdf_forensics::PageRepresentation::RasterOnly
    );
    assert_eq!(pages[0].image_coverage_bps, 10_000);
    assert_eq!(pages[0].visible_text_runs, 0);
    assert_eq!(report.document.images.value().unwrap().len(), 1);
    assert!(report.document.fonts.value().unwrap().is_empty());
}

#[test]
fn a_full_page_image_with_invisible_text_classifies_as_ocr_layer() {
    let report = audit_bytes(&full_page_image_pdf(true), None);
    report.validate().unwrap();
    let pages = report.document.pages.value().unwrap();
    assert_eq!(
        pages[0].representation,
        pdf_forensics::PageRepresentation::RasterWithTextLayer
    );
}

#[test]
fn a_blank_page_classifies_as_blank() {
    let report = audit_bytes(&blank_page_pdf(), None);
    report.validate().unwrap();
    let pages = report.document.pages.value().unwrap();
    assert_eq!(
        pages[0].representation,
        pdf_forensics::PageRepresentation::Blank
    );
}

#[test]
fn every_pending_section_says_what_blocks_it() {
    let report = audit_bytes(&builder::multipage_classic(1), None);
    for pending in &report.document.pending {
        assert!(!pending.section.is_empty());
        assert!(
            !pending.blocked_on.is_empty(),
            "pending section {} states no blocker",
            pending.section
        );
    }
}

/// One 72×72 pt page painted with a 2×2 DeviceRGB image covering the crop.
fn full_page_image_pdf(ocr_layer: bool) -> Vec<u8> {
    let mut b = pdf_test_support::builder::PdfBuilder::new();
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

fn blank_page_pdf() -> Vec<u8> {
    let mut b = pdf_test_support::builder::PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 72 72]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R>>");
    b.add_stream(4, "", b"");
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}
