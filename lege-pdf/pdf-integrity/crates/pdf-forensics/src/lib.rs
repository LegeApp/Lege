//! `pdf-forensics` — the interpretation layer of the PDF integrity checker.
//!
//! The renderer's `pdf-read` crate is the neutral facts layer: it reports what
//! a document says. This crate decides what those facts are worth. It holds the
//! report schema, the findings vocabulary, logical-revision reconstruction,
//! object-change classification, and evidence generation. The findings engine
//! never lives in the parser crates.
//!
//! Two rules shape every type here, and both are load-bearing rather than
//! stylistic:
//!
//! * **No score.** The report has no numeric fraud percentage. Findings are
//!   categorical, each carries its own evidence strength and its own stated
//!   limitation, and the document-level verdict is one of four named
//!   [`Disposition`]s. A percentage would be indefensible in the admissions and
//!   employment workflows this tool serves.
//! * **No silent gaps.** A fact the tool could not reach is
//!   [`Availability::Unavailable`] with a reason, never absent and never zero;
//!   a section it cannot yet compute at all is listed in
//!   [`DocumentFacts::pending`]; and a finding may not rest on a fact its own
//!   [`AnalysisMode`] could not have reached. An encrypted document audited
//!   without a password must not look like a clean one, and neither must a
//!   half-built audit.
//!
//! # Status
//!
//! [`audit_bytes`] works today over what the fact layer currently surfaces:
//! file identity, the parse assessment, the header, lineage identifiers,
//! encryption, feature flags, annotation counts, signatures, per-page
//! representation classification, and font and image inventories. The rest of
//! the M1 inventory is enumerated in [`DocumentFacts::pending`] rather than
//! silently missing.

mod document;
mod intake;
mod report;
mod representation;

pub use document::{
    AnnotationFacts, CompileOutcome, DocumentFacts, EncryptionFacts, FeatureFacts, FileIdentity,
    FontFacts, HeaderFacts, ImageFacts, LineageFacts, MetadataFacts, OpenState, PageFacts,
    PageRepresentation, ParseAssessment, PendingSection, SignatureFacts, SignatureInventoryFacts,
    XmpHistoryEventFacts, XmpLineageFacts,
};
pub use intake::audit_bytes;
pub use report::{
    AnalysisMode, AuditReport, Availability, CAVEAT, Disposition, EvidenceStrength, Finding,
    FindingCode, SCHEMA_VERSION, SchemaViolation, UnavailableReason,
};
