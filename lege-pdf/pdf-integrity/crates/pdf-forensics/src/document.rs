//! Neutral document facts, as they appear in a report.
//!
//! Everything here is descriptive: what the file is and what it says. Nothing
//! here decides what any of it means — that is [`crate::report`]'s job. The
//! separation is deliberate, because facts that have already been coloured by
//! suspicion cannot be re-weighed later.
//!
//! These types mirror what the fact layer currently reaches rather than what M1
//! ultimately needs. The difference is not left implicit: sections that M1
//! requires and this build cannot yet compute are listed in
//! [`DocumentFacts::pending`], so an incomplete audit reports itself as one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::report::Availability;

/// The file as bytes, independent of anything it claims about itself.
///
/// Computed over the whole file rather than through the parser, so it holds
/// even for a document that will not open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    /// Lowercase hex SHA-256 of the entire file.
    pub sha256: String,
    /// Size in bytes.
    pub size_bytes: u64,
}

/// Whether the document opened, and at what cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenState {
    /// Opened with no repair of any kind.
    Clean,
    /// Opened only after structural repair.
    Recovered,
    /// Did not open. For an encrypted document without a usable password this
    /// is expected, and the structural layer is still readable.
    Failed {
        /// The error, rendered.
        error: String,
    },
}

/// The parse assessment: how much of what follows can be trusted.
///
/// A rebuilt xref is the important case. Recovery is the right behaviour for a
/// renderer and the wrong basis for a claim about revision membership, so the
/// fact that repair happened travels with every report rather than being
/// discarded once the document is open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseAssessment {
    /// Whether the document opened, and how.
    pub state: OpenState,
    /// Every repair performed, in the order performed.
    pub recovery_events: Vec<String>,
    /// The xref was rebuilt by a full object scan — the heaviest repair, and
    /// the one that invalidates revision-history claims.
    pub xref_rebuilt: bool,
    /// Number of xref sections found. Not yet the same as the count of logical
    /// revisions; see the `logical-revision` term and M2.
    pub xref_section_count: usize,
}

impl ParseAssessment {
    /// Whether any claim resting on this parse is resting on repaired
    /// structure.
    #[must_use]
    pub fn rests_on_recovered_structure(&self) -> bool {
        !self.recovery_events.is_empty()
    }
}

/// What the `/Encrypt` dictionary declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionFacts {
    /// `/V` — algorithm version.
    pub version: i64,
    /// `/R` — standard-handler revision.
    pub revision: i64,
    /// Human name of the method, e.g. "AES-256".
    pub method: String,
    /// Whether the empty user password genuinely validates against `/U`. True
    /// here means permissions-only encryption, which audits in full.
    pub user_password_empty: bool,
}

/// Document-level feature flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureFacts {
    /// An `/AcroForm` is present.
    pub has_acroform: bool,
    /// XFA form data is present.
    pub has_xfa: bool,
    /// Document-level JavaScript is present.
    pub has_javascript: bool,
    /// An outline tree is present.
    pub has_outlines: bool,
    /// Optional content (layers) is present.
    pub has_optional_content: bool,
    /// Objects are stored in object streams.
    pub uses_object_streams: bool,
    /// The name tree carries `/EmbeddedFiles` — there are file attachments.
    pub has_embedded_files: bool,
}

/// Annotation counts by `/Subtype`.
///
/// Counts only. Per-annotation identity and forensic metadata — the fields that
/// distinguish an annotation added later from one present from the start — are
/// tracked by `@audit.work.edit-provenance-facts` and
/// `@lege.work.annotation-forensic-metadata`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AnnotationFacts {
    /// How many of each `/Subtype`.
    pub count_per_subtype: BTreeMap<String, u32>,
}

impl AnnotationFacts {
    /// Total annotations across all subtypes.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.count_per_subtype.values().sum()
    }
}

/// The `%PDF` header and where it was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderFacts {
    /// Major version, e.g. 1 in `%PDF-1.7`.
    pub major: u8,
    /// Minor version, e.g. 7 in `%PDF-1.7`.
    pub minor: u8,
    /// Bytes of leading garbage before `%PDF`. Non-zero means every
    /// structural offset in the file is shifted relative to the source.
    pub header_offset: u64,
}

/// One `xmpMM:History` entry: a save, print or conversion the writer recorded.
///
/// Every field is the writer's claim, not an observation. A tool that does not
/// record history leaves none, and a tool that does can write whatever it
/// likes — so an absent or thin history is not evidence of anything.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct XmpHistoryEventFacts {
    /// `stEvt:action`, e.g. `created`, `saved`, `converted`.
    pub action: Option<String>,
    /// `stEvt:when` — the claimed timestamp, verbatim and unparsed.
    pub when: Option<String>,
    /// `stEvt:softwareAgent` — the tool, as it named itself.
    pub software_agent: Option<String>,
    /// `stEvt:changed` — which parts the writer says changed.
    pub changed: Option<String>,
    /// `stEvt:instanceID` — the instance this event produced.
    pub instance_id: Option<String>,
}

/// The `xmpMM` media-management properties, when the packet carries any.
///
/// This is the lineage the trailer `/ID` pair only hints at. `document_id` is
/// meant to persist across a document's entire edit history while
/// `instance_id` is minted on every save, so two files sharing a `document_id`
/// with differing `instance_id`s are two saves of one lineage by the format's
/// own design — where a document regenerated from its source application would
/// mint both fresh. `derived_from_instance_id`, when present, names the
/// ancestor outright.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct XmpLineageFacts {
    /// `xmpMM:DocumentID` — stable across the edit history.
    pub document_id: Option<String>,
    /// `xmpMM:InstanceID` — changes on every save.
    pub instance_id: Option<String>,
    /// `xmpMM:OriginalDocumentID`.
    pub original_document_id: Option<String>,
    /// `stRef:documentID` inside `xmpMM:DerivedFrom`.
    pub derived_from_document_id: Option<String>,
    /// `stRef:instanceID` inside `xmpMM:DerivedFrom`.
    pub derived_from_instance_id: Option<String>,
    /// `xmpMM:History`, in document order.
    pub history: Vec<XmpHistoryEventFacts>,
}

/// `/Info` and XMP. Needs an open document: `/Info` strings and the XMP stream
/// body are encrypted like anything else.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MetadataFacts {
    /// Every `/Info` entry with a string value, including non-standard keys.
    pub info: BTreeMap<String, String>,
    /// Whether the catalog carries an XMP `/Metadata` stream.
    pub has_xmp_metadata: bool,
    /// The media-management properties inside that stream. `None` means the
    /// packet was absent, undecodable, or carried none — which is why
    /// `has_xmp_metadata` is reported separately rather than inferred from
    /// this being present.
    pub xmp: Option<XmpLineageFacts>,
}

/// What the document says about its own identity and descent.
///
/// The trailer `/ID` pair is the durable half and needs no decryption, so it
/// survives even for a document that will not open. Its first element is meant
/// to persist across a document's entire edit history while the second changes
/// on every save — which makes the pair evidence of lineage independent of any
/// metadata a writer chose to refresh.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LineageFacts {
    /// Lowercase hex of the permanent trailer `/ID[0]`, if present.
    pub id_permanent: Option<String>,
    /// Lowercase hex of the changing trailer `/ID[1]`, if present.
    pub id_changing: Option<String>,
    /// `/Info` and XMP presence, when the document opened.
    pub metadata: Availability<MetadataFacts>,
}

/// One signature field and the bytes it covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureFacts {
    /// The field's `/T` name.
    pub field_name: Option<String>,
    /// `/Filter` — the handler that produced the signature.
    pub filter: Option<String>,
    /// `/SubFilter` — the signature encoding.
    pub sub_filter: Option<String>,
    /// `/M` — the claimed signing time, verbatim and unparsed. A claim by the
    /// signer, not an observation.
    pub signed_at: Option<String>,
    /// `/ByteRange`, verbatim.
    pub byte_range: Vec<i64>,
    /// Whether `/ByteRange` spans the entire file.
    ///
    /// False is the structural half of the incremental-update attack: bytes
    /// outside the signed range are unsigned however well the cryptography
    /// over that range verifies. Whether it verifies is a separate question
    /// this build does not yet ask.
    pub covers_whole_file: bool,
}

/// Signature fields found in the `/AcroForm`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SignatureInventoryFacts {
    /// Every signed `/FT /Sig` field.
    pub signatures: Vec<SignatureFacts>,
    /// Signature fields with no `/V` — ordinary in a blank form.
    pub unsigned_fields: u32,
}

/// How a page is painted, derived from the fact layer's metrics.
///
/// This is interpretation, not a fact: the underlying counts and coverage
/// travel alongside it so a reader can disagree with the label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageRepresentation {
    /// Visible text and/or vector paths; images, if any, are decorative.
    Vector,
    /// One or more images dominate the page and nothing visible is drawn
    /// on top — the PDF-Verse / scan-without-OCR case.
    RasterOnly,
    /// A dominating image plus an invisible text layer (typically `Tr 3`).
    RasterWithTextLayer,
    /// Substantial image coverage and visible vector content together.
    Mixed,
    /// The content stream compiled and painted nothing.
    Blank,
    /// The content stream did not compile, so no label is honest.
    Unknown,
}

/// One font resource the document used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontFacts {
    /// Resource name it was reached through (`F1`).
    pub resource_name: String,
    /// Backing font dictionary as `N G R`, when the resource resolved.
    pub object: Option<String>,
    /// `/Subtype`.
    pub subtype: String,
    /// `/BaseFont`.
    pub base_font: String,
    /// An embedded outline program was present and usable.
    pub embedded: bool,
}

/// One image XObject or inline image the document used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageFacts {
    /// Backing object as `N G R`; `None` for an inline image.
    pub object: Option<String>,
    /// Sample width in pixels.
    pub width: u32,
    /// Sample height in pixels.
    pub height: u32,
    /// `/BitsPerComponent`.
    pub bits_per_component: u8,
    /// `/ImageMask true`.
    pub is_mask: bool,
    /// Filter names in application order.
    pub filters: Vec<String>,
}

/// Whether a page's content stream compiled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompileOutcome {
    /// Compiled cleanly.
    Ok {
        /// Number of content-stream operators.
        op_count: usize,
    },
    /// Compiled, but only with repairs along the way.
    Degraded {
        /// Number of content-stream operators.
        op_count: usize,
        /// What had to be repaired.
        detail: String,
    },
    /// Did not compile.
    Failed {
        /// The error, rendered.
        error: String,
    },
}

/// One page's facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageFacts {
    /// Zero-based page index.
    pub index: u32,
    /// Whether a `/MediaBox` is genuinely present rather than defaulted.
    pub media_box_present: bool,
    /// Whether the content stream compiled.
    pub compile: CompileOutcome,
    /// How the page is painted. [`PageRepresentation::Unknown`] when compile
    /// failed, so a failed page cannot be mistaken for a blank one.
    pub representation: PageRepresentation,
    /// Show-text runs interned on the page, visible or not.
    pub text_runs: u32,
    /// Runs that paint.
    pub visible_text_runs: u32,
    /// Distinct font resources referenced by those runs.
    pub fonts: u32,
    /// Image XObjects and inline images interned on the page.
    pub images: u32,
    /// `Fill` / `Stroke` / `FillStroke` operators.
    pub path_paints: u32,
    /// Sum of non-mask image placement areas, in basis points of the crop
    /// box (10_000 = painted area equals the page).
    pub image_coverage_bps: u16,
    /// Effective DPI of the largest-coverage non-mask placement.
    pub effective_dpi: Option<u32>,
}

/// A section M1 requires that this build cannot yet compute.
///
/// Listed in the report rather than omitted. A consumer that sees an audit with
/// a non-empty pending list knows it is reading a partial inventory, which is
/// the same discipline [`Availability`] applies to individual facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSection {
    /// Stable name of the missing section.
    pub section: String,
    /// What has to happen before it can be computed.
    pub blocked_on: String,
}

impl PendingSection {
    /// Note a missing section.
    pub fn new(section: impl Into<String>, blocked_on: impl Into<String>) -> Self {
        Self {
            section: section.into(),
            blocked_on: blocked_on.into(),
        }
    }
}

/// Everything the audit determined about the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentFacts {
    /// The file as bytes.
    pub identity: FileIdentity,
    /// How the document parsed.
    pub parse: ParseAssessment,
    /// The `%PDF` header. Readable without decryption, so it survives a failed
    /// open; unavailable only when no header exists at all.
    pub header: Availability<HeaderFacts>,
    /// What the document declares about its own identity and descent.
    pub lineage: LineageFacts,
    /// Signature fields and the byte ranges they cover.
    pub signatures: Availability<SignatureInventoryFacts>,
    /// The `/Encrypt` declaration, or `None` for an unencrypted document.
    /// Unavailable only when the structural layer itself was unreadable.
    pub encryption: Availability<Option<EncryptionFacts>>,
    /// Document-level feature flags.
    pub features: Availability<FeatureFacts>,
    /// Annotation counts by subtype.
    pub annotations: Availability<AnnotationFacts>,
    /// Per-page facts.
    pub pages: Availability<Vec<PageFacts>>,
    /// Distinct font resources referenced by any compiled page.
    pub fonts: Availability<Vec<FontFacts>>,
    /// Distinct image XObjects (plus every inline image) referenced by any
    /// compiled page.
    pub images: Availability<Vec<ImageFacts>>,
    /// M1 sections this build cannot yet compute.
    pub pending: Vec<PendingSection>,
}
