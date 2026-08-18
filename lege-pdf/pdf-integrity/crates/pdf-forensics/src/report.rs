//! The versioned audit report: what the tool emits, and the rules that keep it
//! honest.

use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::document::DocumentFacts;

/// Version of the `AuditReport` JSON schema.
///
/// Bumped whenever a consumer parsing the previous version could misread the
/// new one. Adding an optional field is not a bump; changing the meaning of an
/// existing field, or of any variant name below, is.
pub const SCHEMA_VERSION: u32 = 1;

/// The standing caveat carried by every report.
///
/// Required rather than advisory: a report that found nothing is not a clean
/// bill of health, and saying so is part of the product's framing as triage
/// rather than fraud detection.
pub const CAVEAT: &str = "Absence of findings does not prove authenticity. This tool reports \
                          recoverable indicators of modification; a competent full rewrite, or a \
                          print-edit-rescan cycle, leaves none of them behind.";

/// How much of the document the audit could actually see.
///
/// The distinction exists because encrypted documents are audited rather than
/// refused. Without a usable password the structural skeleton — xref sections,
/// object locations and roles, and the dictionaries that are not themselves
/// encrypted — is still readable, while stream contents are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisMode {
    /// Stream contents were readable; the whole audit ran.
    Full,
    /// Encrypted without a usable password. Structure was audited; anything
    /// requiring decrypted stream contents is [`Availability::Unavailable`].
    StructureOnly,
}

impl AnalysisMode {
    /// Whether an audit in this mode reaches facts that need `required`.
    ///
    /// [`AnalysisMode::Full`] reaches everything; [`AnalysisMode::StructureOnly`]
    /// reaches only what structure alone can support.
    #[must_use]
    pub fn reaches(self, required: Self) -> bool {
        matches!(
            (self, required),
            (Self::Full, _) | (Self::StructureOnly, Self::StructureOnly)
        )
    }
}

/// Why a fact could not be determined.
///
/// Distinct from a fact that was determined to be absent: a document with no
/// `/AcroForm` has an *available* answer of "none", whereas a document whose
/// `/AcroForm` could not be decrypted is unavailable. Collapsing the two would
/// let an encrypted document read as a clean one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// Needed decrypted stream contents that this run did not have.
    Encrypted,
    /// The structure carrying it could not be parsed, even tolerantly.
    Unparseable,
    /// Reconstruction was ambiguous, so no claim is made.
    Ambiguous,
    /// Recognised, but this build does not compute it yet.
    NotImplemented,
}

impl fmt::Display for UnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Encrypted => "encrypted",
            Self::Unparseable => "unparseable",
            Self::Ambiguous => "ambiguous",
            Self::NotImplemented => "not implemented",
        };
        f.write_str(s)
    }
}

/// A fact that may not have been reachable.
///
/// Every section of the report that can be blocked by encryption or by a failed
/// parse is wrapped in this rather than defaulting to empty, so a gap in the
/// report is always visible as a gap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Availability<T> {
    /// The fact was determined.
    Available {
        /// The determined value.
        value: T,
    },
    /// The fact could not be determined, and why.
    Unavailable {
        /// The category of gap.
        reason: UnavailableReason,
        /// Human-readable detail, e.g. which object resisted parsing.
        detail: String,
    },
}

impl<T> Default for Availability<T> {
    /// A gap, never a determined value.
    ///
    /// Defaulting is where silent zeroes creep in, so the default here is the
    /// admission that nothing was computed. A caller that meant "determined,
    /// and empty" has to say so.
    fn default() -> Self {
        Self::Unavailable {
            reason: UnavailableReason::NotImplemented,
            detail: "not computed".to_owned(),
        }
    }
}

impl<T> Availability<T> {
    /// Wrap a determined value.
    pub const fn available(value: T) -> Self {
        Self::Available { value }
    }

    /// Record a gap.
    pub fn unavailable(reason: UnavailableReason, detail: impl Into<String>) -> Self {
        Self::Unavailable {
            reason,
            detail: detail.into(),
        }
    }

    /// The value, if it was determined.
    pub const fn value(&self) -> Option<&T> {
        match self {
            Self::Available { value } => Some(value),
            Self::Unavailable { .. } => None,
        }
    }

    /// Whether the fact was determined.
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

/// How much weight a finding can bear.
///
/// The report never sums these into a score. They exist so a reader can tell a
/// cryptographic failure from a statistical hunch at a glance, and so policy
/// can require corroboration before a weak signal affects the disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStrength {
    /// Directly readable from the file's structure; not a matter of judgement.
    StructuralFact,
    /// A verified cryptographic result.
    CryptographicFact,
    /// Derived, but with no plausible innocent mechanism known.
    StrongDerived,
    /// Meaningful in context, with known innocent explanations.
    ContextualIndicator,
    /// Statistical or stylistic; corroboration required before it counts.
    WeakHeuristic,
}

/// A stable identifier for a kind of finding.
///
/// Stable strings rather than an enum: codes accumulate across milestones and
/// downstream consumers match on them, so the JSON spelling is the contract.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FindingCode(Cow<'static, str>);

impl FindingCode {
    /// Define a code from a static string.
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self(Cow::Borrowed(code))
    }

    /// The code as written in JSON.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FindingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One reported observation, with everything a reader needs to weigh it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable identifier for this kind of finding.
    pub code: FindingCode,
    /// How much weight it can bear.
    pub strength: EvidenceStrength,
    /// Plain-language statement of what was observed.
    pub explanation: String,
    /// What this finding cannot establish, where that needs saying. Every
    /// finding weaker than a structural or cryptographic fact should carry one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limitation: Option<String>,
    /// The weakest [`AnalysisMode`] that reaches the facts this rests on. A
    /// finding whose evidence needs decrypted streams records
    /// [`AnalysisMode::Full`] here and may not be emitted by a structure-only
    /// run.
    pub requires_mode: AnalysisMode,
    /// Whether this rests on repaired or reconstructed structure. Recovery is
    /// never allowed to pass silently into a claim: a rebuilt xref cannot
    /// support statements about original revision membership.
    pub rests_on_recovered_structure: bool,
}

/// The document-level verdict. Deliberately four named outcomes and no score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Disposition {
    /// Nothing warranting review was found. Read with [`CAVEAT`].
    NoReviewIndicatorFound,
    /// Indicators worth a human look.
    ReviewRecommended,
    /// The document fails on its own terms — a signature that does not verify,
    /// or structure that contradicts itself.
    IntegrityFailure,
    /// Not enough survived to say. A flattened raster export lands here: it is
    /// equally compatible with ordinary scanning and with tampering.
    Inconclusive,
}

/// A report that breaks the schema's own rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaViolation {
    /// `schema_version` is not the version this build writes.
    WrongVersion {
        /// The version found.
        found: u32,
    },
    /// A finding rests on facts its run could not have reached.
    FindingExceedsMode {
        /// The offending finding.
        code: FindingCode,
        /// The mode the run actually had.
        run: AnalysisMode,
        /// The mode the finding needs.
        required: AnalysisMode,
    },
    /// A finding weaker than a hard fact carries no stated limitation.
    MissingLimitation {
        /// The offending finding.
        code: FindingCode,
    },
    /// The disposition asserts more than the findings support.
    UnsupportedDisposition {
        /// The asserted disposition.
        disposition: Disposition,
    },
}

impl fmt::Display for SchemaViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongVersion { found } => {
                write!(f, "schema_version {found} is not {SCHEMA_VERSION}")
            }
            Self::FindingExceedsMode {
                code,
                run,
                required,
            } => write!(
                f,
                "finding {code} needs {required:?} evidence but the run was {run:?}"
            ),
            Self::MissingLimitation { code } => {
                write!(
                    f,
                    "finding {code} is not a hard fact but states no limitation"
                )
            }
            Self::UnsupportedDisposition { disposition } => {
                write!(
                    f,
                    "disposition {disposition:?} is not supported by the findings"
                )
            }
        }
    }
}

impl std::error::Error for SchemaViolation {}

/// The versioned audit report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReport {
    /// Schema version this report was written against.
    pub schema_version: u32,
    /// How much of the document the run could see.
    pub analysis_mode: AnalysisMode,
    /// Everything observed, in no particular order.
    pub findings: Vec<Finding>,
    /// The document-level verdict.
    pub disposition: Disposition,
    /// The neutral facts the verdict was drawn from.
    pub document: DocumentFacts,
    /// The standing caveat, carried in-band so it survives into any consumer
    /// that renders the JSON without reading this crate's documentation.
    pub caveat: String,
}

impl AuditReport {
    /// Build a report, stamping the current schema version and caveat.
    #[must_use]
    pub fn new_with_facts(
        analysis_mode: AnalysisMode,
        disposition: Disposition,
        findings: Vec<Finding>,
        document: DocumentFacts,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            analysis_mode,
            findings,
            disposition,
            document,
            caveat: CAVEAT.to_owned(),
        }
    }

    /// Check the report against the schema's own rules.
    ///
    /// This is the mechanical half of the two invariants the crate exists to
    /// hold: no finding may outrun the evidence its mode could reach, and no
    /// soft finding may travel without its limitation. Emitters call it before
    /// serialising; the schema tests call it on fixtures.
    ///
    /// # Errors
    ///
    /// Returns every violation found, rather than stopping at the first.
    pub fn validate(&self) -> Result<(), Vec<SchemaViolation>> {
        let mut violations = Vec::new();

        if self.schema_version != SCHEMA_VERSION {
            violations.push(SchemaViolation::WrongVersion {
                found: self.schema_version,
            });
        }

        for finding in &self.findings {
            if !self.analysis_mode.reaches(finding.requires_mode) {
                violations.push(SchemaViolation::FindingExceedsMode {
                    code: finding.code.clone(),
                    run: self.analysis_mode,
                    required: finding.requires_mode,
                });
            }

            let is_hard_fact = matches!(
                finding.strength,
                EvidenceStrength::StructuralFact | EvidenceStrength::CryptographicFact
            );
            if !is_hard_fact && finding.limitation.is_none() {
                violations.push(SchemaViolation::MissingLimitation {
                    code: finding.code.clone(),
                });
            }
        }

        // Findings are what licenses a verdict: with none, the only honest
        // answers are that nothing was found or that too little was readable.
        let asserts_a_problem = matches!(
            self.disposition,
            Disposition::ReviewRecommended | Disposition::IntegrityFailure
        );
        if asserts_a_problem && self.findings.is_empty() {
            violations.push(SchemaViolation::UnsupportedDisposition {
                disposition: self.disposition,
            });
        }

        // An all-clear requires having finished looking. While any part of the
        // inventory is still uncomputed, the strongest honest answer is that
        // too little is known.
        if self.disposition == Disposition::NoReviewIndicatorFound
            && !self.document.pending.is_empty()
        {
            violations.push(SchemaViolation::UnsupportedDisposition {
                disposition: self.disposition,
            });
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}
