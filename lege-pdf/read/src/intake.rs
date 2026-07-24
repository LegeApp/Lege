use std::sync::Arc;

use pdf_document::{DocumentError, DocumentLimits, DocumentSnapshot};
use pdf_read::{DocumentReport, OpenOutcome};
use pdf_security::SecurityError;
use pdf_source::{OwnedBytesSource, PdfSource};

/// Lege-owned form of the renderer's per-page compile health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileStatus {
    Ok {
        operation_count: usize,
    },
    Degraded {
        operation_count: usize,
        detail: String,
    },
    Failed {
        error: String,
    },
}

/// Read-only facts collected before a document enters the processing path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentIntake {
    pub page_count: u32,
    pub encrypted: bool,
    pub needs_password: bool,
    pub recovery_used: bool,
    pub per_page_compile: Vec<CompileStatus>,
}

/// Examine document bytes without exposing renderer report types.
///
/// `pdf_read::examine` deliberately never fails. If it reports an open
/// failure for an encrypted file, retry the cheap typed open path so a real
/// password requirement is not confused with an unsupported or damaged file.
pub fn examine_document(bytes: &Arc<[u8]>) -> DocumentIntake {
    let limits = DocumentLimits::default();
    let report = pdf_read::examine(source(Arc::clone(bytes)), limits.clone());
    let encrypted = report.encryption.is_some();
    let needs_password = encrypted
        && matches!(&report.open, OpenOutcome::Failed { .. })
        && matches!(
            DocumentSnapshot::open(source(Arc::clone(bytes)), limits),
            Err(DocumentError::Security(SecurityError::PasswordRequired))
        );

    from_report(report, encrypted, needs_password)
}

fn from_report(report: DocumentReport, encrypted: bool, needs_password: bool) -> DocumentIntake {
    let per_page_compile = report
        .pages
        .into_iter()
        .map(|page| match page.compile {
            pdf_read::CompileStatus::Ok { op_count } => CompileStatus::Ok {
                operation_count: op_count,
            },
            pdf_read::CompileStatus::Degraded { op_count, detail } => CompileStatus::Degraded {
                operation_count: op_count,
                detail,
            },
            pdf_read::CompileStatus::Failed { error } => CompileStatus::Failed { error },
        })
        .collect::<Vec<_>>();

    DocumentIntake {
        page_count: u32::try_from(per_page_compile.len()).unwrap_or(u32::MAX),
        encrypted,
        needs_password,
        recovery_used: report.xref.recovery_used,
        per_page_compile,
    }
}

pub(crate) fn source(bytes: Arc<[u8]>) -> Arc<dyn PdfSource> {
    Arc::new(OwnedBytesSource::new(bytes))
}
