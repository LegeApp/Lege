//! Turning a file into a report.
//!
//! This is the seam between the renderer's neutral fact layer and this crate's
//! interpretation. `pdf-read` is asked what the document says; everything here
//! is translation, plus the one judgement that cannot be deferred: how much of
//! the document this run could actually see.

use std::sync::Arc;

use pdf_document::DocumentLimits;
use pdf_read::{CompileStatus, DocumentReport, OpenOutcome};
use pdf_source::OwnedBytesSource;
use sha2::{Digest, Sha256};

use crate::document::{
    AnnotationFacts, CompileOutcome, DocumentFacts, EncryptionFacts, FeatureFacts, FileIdentity,
    FontFacts, HeaderFacts, ImageFacts, LineageFacts, MetadataFacts, OpenState, PageFacts,
    PageRepresentation, ParseAssessment, PendingSection, SignatureFacts, SignatureInventoryFacts,
    XmpHistoryEventFacts, XmpLineageFacts,
};
use crate::report::{AnalysisMode, AuditReport, Availability, Disposition, UnavailableReason};
use crate::representation;

/// Audit the bytes of a PDF.
///
/// Never fails and never panics: a file that is not a PDF at all still yields a
/// report, one whose parse assessment says so. That is a requirement rather
/// than politeness — a forensic tool that refuses its most damaged inputs is
/// useless on exactly the documents that warrant a look.
///
/// `password` is tried against an encrypted document. Without one, an encrypted
/// document is still audited, structurally, and the report says which happened.
#[must_use]
pub fn audit_bytes(bytes: &[u8], password: Option<&str>) -> AuditReport {
    let identity = FileIdentity {
        sha256: hex_sha256(bytes),
        size_bytes: bytes.len() as u64,
    };

    let source = Arc::new(OwnedBytesSource::new(bytes.to_vec()));
    let report = pdf_read::examine_with_password(source, DocumentLimits::default(), password);

    let facts = translate(&report, identity);
    let mode = analysis_mode(&facts);

    // No findings engine yet, and the inventory is still partial. INCONCLUSIVE
    // is the only honest verdict: NO_REVIEW_INDICATOR_FOUND would assert that
    // nothing was found by a run that did not finish looking.
    AuditReport::new_with_facts(mode, Disposition::Inconclusive, Vec::new(), facts)
}

/// How much of the document this run could see.
///
/// A document that opened was fully readable. One that did not is
/// structure-only — whether because it is encrypted and no usable password was
/// supplied, or because it is too damaged to open. The distinction between
/// those two causes is carried per-section by [`UnavailableReason`]; the mode
/// records only how far the run got.
fn analysis_mode(facts: &DocumentFacts) -> AnalysisMode {
    match facts.parse.state {
        OpenState::Clean | OpenState::Recovered => AnalysisMode::Full,
        OpenState::Failed { .. } => AnalysisMode::StructureOnly,
    }
}

fn translate(report: &DocumentReport, identity: FileIdentity) -> DocumentFacts {
    let (state, recovery_events) = match &report.open {
        OpenOutcome::Ok => (OpenState::Clean, Vec::new()),
        OpenOutcome::Recovered { how } => (OpenState::Recovered, how.clone()),
        OpenOutcome::Failed { error } => (
            OpenState::Failed {
                error: error.clone(),
            },
            Vec::new(),
        ),
    };

    let parse = ParseAssessment {
        state,
        recovery_events,
        xref_rebuilt: report.xref.rebuilt,
        xref_section_count: report.xref.revision_count,
    };

    let opened = !matches!(parse.state, OpenState::Failed { .. });

    let encryption = report.encryption.as_ref().map(|e| EncryptionFacts {
        version: e.version,
        revision: e.revision,
        method: e.method.clone(),
        user_password_empty: e.user_password_empty,
    });

    // Why this run could not read the rest. An encrypted document that failed
    // to open failed *because* of the encryption; anything else that failed to
    // open is simply unparseable.
    let blocked_by = if encryption.is_some() {
        UnavailableReason::Encrypted
    } else {
        UnavailableReason::Unparseable
    };
    let blocked_detail = match &parse.state {
        OpenState::Failed { error } => error.clone(),
        _ => String::new(),
    };

    let encryption = if opened || encryption.is_some() {
        // Either the document opened, or the structural retry read /Encrypt —
        // both are determined answers.
        Availability::available(encryption)
    } else {
        // The document did not open and no /Encrypt was read, which means the
        // structural layer itself was unreadable. "Not encrypted" would be a
        // claim this run cannot make.
        Availability::unavailable(UnavailableReason::Unparseable, blocked_detail.clone())
    };

    // The remaining sections need an open document. Note that the fact layer
    // does fill in `uses_object_streams` on the failed path, but a FeatureFacts
    // with one real flag and five defaulted ones would read as five determined
    // negatives, so the section is reported unavailable as a whole.
    let (features, annotations, pages, fonts, images) = if opened {
        (
            Availability::available(FeatureFacts {
                has_acroform: report.features.has_acroform,
                has_xfa: report.features.has_xfa,
                has_javascript: report.features.has_javascript,
                has_outlines: report.features.has_outlines,
                has_optional_content: report.features.has_optional_content,
                uses_object_streams: report.features.uses_object_streams,
                has_embedded_files: report.features.has_embedded_files,
            }),
            Availability::available(AnnotationFacts {
                count_per_subtype: report.annotations.count_per_subtype.clone(),
            }),
            Availability::available(report.pages.iter().map(translate_page).collect()),
            Availability::available(report.fonts.iter().map(translate_font).collect()),
            Availability::available(report.images.iter().map(translate_image).collect()),
        )
    } else {
        (
            Availability::unavailable(blocked_by.clone(), blocked_detail.clone()),
            Availability::unavailable(blocked_by.clone(), blocked_detail.clone()),
            Availability::unavailable(blocked_by.clone(), blocked_detail.clone()),
            Availability::unavailable(blocked_by.clone(), blocked_detail.clone()),
            Availability::unavailable(blocked_by.clone(), blocked_detail.clone()),
        )
    };

    // The header needs no decryption, so it survives a failed open. Its
    // absence is not a gap in the audit — it is why the document failed.
    let header = match &report.header {
        Some(h) => Availability::available(HeaderFacts {
            major: h.major,
            minor: h.minor,
            header_offset: h.header_offset,
        }),
        None => Availability::unavailable(
            UnavailableReason::Unparseable,
            "no %PDF header found in the file",
        ),
    };

    // The trailer /ID pair is never encrypted; /Info is.
    let lineage = LineageFacts {
        id_permanent: report.identity.id_permanent.clone(),
        id_changing: report.identity.id_changing.clone(),
        metadata: if opened {
            Availability::available(MetadataFacts {
                info: report.identity.info.clone(),
                has_xmp_metadata: report.identity.has_xmp_metadata,
                xmp: report.identity.xmp.as_ref().map(translate_xmp),
            })
        } else {
            Availability::unavailable(blocked_by.clone(), blocked_detail.clone())
        },
    };

    let signatures = if opened {
        Availability::available(SignatureInventoryFacts {
            signatures: report
                .signatures
                .signatures
                .iter()
                .map(translate_signature)
                .collect(),
            unsigned_fields: report.signatures.unsigned_fields,
        })
    } else {
        Availability::unavailable(blocked_by, blocked_detail)
    };

    DocumentFacts {
        identity,
        parse,
        header,
        lineage,
        encryption,
        features,
        annotations,
        pages,
        fonts,
        images,
        signatures,
        pending: pending_sections(),
    }
}

fn translate_xmp(xmp: &pdf_read::XmpLineage) -> XmpLineageFacts {
    XmpLineageFacts {
        document_id: xmp.document_id.clone(),
        instance_id: xmp.instance_id.clone(),
        original_document_id: xmp.original_document_id.clone(),
        derived_from_document_id: xmp.derived_from_document_id.clone(),
        derived_from_instance_id: xmp.derived_from_instance_id.clone(),
        history: xmp
            .history
            .iter()
            .map(|e| XmpHistoryEventFacts {
                action: e.action.clone(),
                when: e.when.clone(),
                software_agent: e.software_agent.clone(),
                changed: e.changed.clone(),
                instance_id: e.instance_id.clone(),
            })
            .collect(),
    }
}

fn translate_signature(sig: &pdf_read::SignatureInfo) -> SignatureFacts {
    SignatureFacts {
        field_name: sig.field_name.clone(),
        filter: sig.filter.clone(),
        sub_filter: sig.sub_filter.clone(),
        signed_at: sig.signed_at.clone(),
        byte_range: sig.byte_range.clone(),
        covers_whole_file: sig.covers_whole_file,
    }
}

fn translate_page(page: &pdf_read::PageStatus) -> PageFacts {
    let compile = match &page.compile {
        CompileStatus::Ok { op_count } => CompileOutcome::Ok {
            op_count: *op_count,
        },
        CompileStatus::Degraded { op_count, detail } => CompileOutcome::Degraded {
            op_count: *op_count,
            detail: detail.clone(),
        },
        CompileStatus::Failed { error } => CompileOutcome::Failed {
            error: error.clone(),
        },
    };
    match &page.metrics {
        Some(metrics) => PageFacts {
            index: page.index,
            media_box_present: page.media_box_present,
            compile,
            representation: representation::classify(metrics),
            text_runs: metrics.text_runs,
            visible_text_runs: metrics.visible_text_runs,
            fonts: metrics.fonts,
            images: metrics.images,
            path_paints: metrics.path_paints,
            image_coverage_bps: metrics.image_coverage_bps,
            effective_dpi: metrics.effective_dpi,
        },
        None => PageFacts {
            index: page.index,
            media_box_present: page.media_box_present,
            compile,
            representation: PageRepresentation::Unknown,
            text_runs: 0,
            visible_text_runs: 0,
            fonts: 0,
            images: 0,
            path_paints: 0,
            image_coverage_bps: 0,
            effective_dpi: None,
        },
    }
}

fn translate_font(font: &pdf_read::FontRecord) -> FontFacts {
    FontFacts {
        resource_name: font.resource_name.clone(),
        object: font.object.map(|id| id.to_string()),
        subtype: font.subtype.clone(),
        base_font: font.base_font.clone(),
        embedded: font.embedded,
    }
}

fn translate_image(image: &pdf_read::ImageRecord) -> ImageFacts {
    ImageFacts {
        object: image.object.map(|id| id.to_string()),
        width: image.width,
        height: image.height,
        bits_per_component: image.bits_per_component,
        is_mask: image.is_mask,
        filters: image.filters.clone(),
    }
}

/// The M1 inventory this build does not reach yet.
///
/// Every entry is a section named in the M1 milestone that the fact layer does
/// not surface. Keeping the list in the report — rather than in a comment —
/// means a consumer can tell a partial audit from a complete one, and means the
/// list has to be shortened rather than quietly forgotten.
fn pending_sections() -> Vec<PendingSection> {
    vec![
        PendingSection::new(
            "xref_sections",
            "per-section offsets, types and entry counts; needs the XrefSection type from \
             @audit.decision.revision-model",
        ),
        PendingSection::new(
            "linearization",
            "pdf-read must surface this as a neutral fact (M1 plan step 2)",
        ),
        PendingSection::new(
            "annotation_identity",
            "per-annotation object identity and forensic metadata; \
             @lege.work.annotation-forensic-metadata",
        ),
        PendingSection::new(
            "signature_cryptography",
            "structural ByteRange coverage is reported, but nothing verifies the signature \
             itself; that is M4",
        ),
        PendingSection::new(
            "edit_markers",
            "/TouchUp_TextEdit and related marked-content tags; \
             @audit.work.edit-provenance-facts",
        ),
    ]
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Hand-rolled rather than pulling in a hex crate for sixteen nibbles.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
