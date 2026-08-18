//! Acceptance test for `@audit.milestone.m1-factual-audit#schema-versioned`.
//!
//! Guards the JSON spelling of the report, which is the contract with any
//! downstream consumer, and the two invariants `AuditReport::validate` exists to
//! hold.
#![allow(clippy::unwrap_used, reason = "a failed unwrap is a failed test")]

use pdf_forensics::{
    AnalysisMode, AuditReport, Availability, CAVEAT, Disposition, DocumentFacts, EvidenceStrength,
    FileIdentity, Finding, FindingCode, OpenState, ParseAssessment, SCHEMA_VERSION,
    UnavailableReason,
};
use serde_json::{Value, json};

const RASTER_ONLY: FindingCode = FindingCode::new("page.raster_only");

/// A complete, unremarkable set of facts, so these tests exercise the schema
/// rules rather than the intake path. `pending` is empty on purpose: a report
/// with sections outstanding cannot legitimately reach every disposition.
fn facts() -> DocumentFacts {
    DocumentFacts {
        identity: FileIdentity {
            sha256: "00".repeat(32),
            size_bytes: 1024,
        },
        parse: ParseAssessment {
            state: OpenState::Clean,
            recovery_events: Vec::new(),
            xref_rebuilt: false,
            xref_section_count: 1,
        },
        header: Availability::available(pdf_forensics::HeaderFacts {
            major: 1,
            minor: 7,
            header_offset: 0,
        }),
        lineage: pdf_forensics::LineageFacts::default(),
        signatures: Availability::available(pdf_forensics::SignatureInventoryFacts::default()),
        encryption: Availability::available(None),
        features: Availability::unavailable(UnavailableReason::NotImplemented, "test fixture"),
        annotations: Availability::unavailable(UnavailableReason::NotImplemented, "test fixture"),
        pages: Availability::unavailable(UnavailableReason::NotImplemented, "test fixture"),
        fonts: Availability::unavailable(UnavailableReason::NotImplemented, "test fixture"),
        images: Availability::unavailable(UnavailableReason::NotImplemented, "test fixture"),
        pending: Vec::new(),
    }
}

fn report_over(
    mode: AnalysisMode,
    disposition: Disposition,
    findings: Vec<Finding>,
) -> AuditReport {
    AuditReport::new_with_facts(mode, disposition, findings, facts())
}

fn hard_fact() -> Finding {
    Finding {
        code: RASTER_ONLY,
        strength: EvidenceStrength::StructuralFact,
        explanation: "Every page draws a single full-page image and no text objects.".to_owned(),
        limitation: None,
        requires_mode: AnalysisMode::Full,
        rests_on_recovered_structure: false,
    }
}

#[test]
fn report_carries_schema_version_and_caveat() {
    let report = report_over(
        AnalysisMode::Full,
        Disposition::Inconclusive,
        vec![hard_fact()],
    );
    let json: Value = serde_json::to_value(&report).unwrap();

    assert_eq!(json["schema_version"], json!(SCHEMA_VERSION));
    assert_eq!(json["caveat"], json!(CAVEAT));
    report.validate().unwrap();
}

#[test]
fn json_spelling_is_the_contract() {
    let report = report_over(
        AnalysisMode::StructureOnly,
        Disposition::NoReviewIndicatorFound,
        vec![],
    );
    let json: Value = serde_json::to_value(&report).unwrap();

    // Dispositions are the four named outcomes, screaming-snake in JSON.
    assert_eq!(json["disposition"], json!("NO_REVIEW_INDICATOR_FOUND"));
    assert_eq!(json["analysis_mode"], json!("structure_only"));
    // No score, ever.
    assert!(json.get("score").is_none());

    let round_tripped: AuditReport = serde_json::from_value(json).unwrap();
    assert_eq!(round_tripped, report);
}

#[test]
fn unavailable_is_distinct_from_absent() {
    let determined_none: Availability<Vec<String>> = Availability::available(vec![]);
    let blocked: Availability<Vec<String>> = Availability::unavailable(
        UnavailableReason::Encrypted,
        "/AcroForm is an encrypted stream",
    );

    let determined_json = serde_json::to_value(&determined_none).unwrap();
    let blocked_json = serde_json::to_value(&blocked).unwrap();

    assert_eq!(determined_json["status"], json!("available"));
    assert_eq!(determined_json["value"], json!([]));
    assert_eq!(blocked_json["status"], json!("unavailable"));
    assert_eq!(blocked_json["reason"], json!("encrypted"));
    assert!(blocked.value().is_none());
}

#[test]
fn structure_only_run_may_not_emit_a_full_mode_finding() {
    let report = report_over(
        AnalysisMode::StructureOnly,
        Disposition::Inconclusive,
        vec![hard_fact()],
    );

    let violations = report.validate().unwrap_err();
    assert_eq!(violations.len(), 1);
    assert!(violations[0].to_string().contains("page.raster_only"));
}

#[test]
fn soft_findings_must_state_a_limitation() {
    let mut finding = hard_fact();
    finding.strength = EvidenceStrength::WeakHeuristic;
    finding.limitation = None;
    let report = report_over(
        AnalysisMode::Full,
        Disposition::ReviewRecommended,
        vec![finding.clone()],
    );
    assert!(report.validate().is_err());

    finding.limitation = Some("Ordinary scanners produce the same output.".to_owned());
    let report = report_over(
        AnalysisMode::Full,
        Disposition::ReviewRecommended,
        vec![finding],
    );
    report.validate().unwrap();
}

#[test]
fn a_problem_verdict_needs_findings_behind_it() {
    let report = report_over(AnalysisMode::Full, Disposition::IntegrityFailure, vec![]);
    assert!(report.validate().is_err());

    // Finding nothing is always a legitimate outcome.
    let report = report_over(
        AnalysisMode::Full,
        Disposition::NoReviewIndicatorFound,
        vec![],
    );
    report.validate().unwrap();
}
