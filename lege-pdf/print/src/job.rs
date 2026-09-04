//! The one-call entry point: open a document, decide pass-through versus
//! composition, impose, and hand the result to a spooler.

use crate::{PrintError, PrintJob, PrintOptions};
use crate::spool::{JobId, PrinterId, Spooler};

/// What `print_document` decided to do, so a caller can report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrintRoute {
    /// The original PDF bytes went to the spooler unmodified.
    PassThrough,
    /// Sheets were composed here.
    Composed { sheets: u32 },
}

/// The outcome of a submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedJob {
    pub id: JobId,
    pub route: PrintRoute,
}

/// Print `pdf_bytes` to `printer` under `options`.
pub fn print_document(
    spooler: &dyn Spooler,
    printer: &PrinterId,
    title: &str,
    pdf_bytes: std::sync::Arc<[u8]>,
    options: &PrintOptions,
) -> Result<SubmittedJob, PrintError> {
    let _ = (spooler, printer, title, pdf_bytes, options);
    unimplemented!("phase 4")
}

/// Impose without submitting: what `print_document` would compose. This is
/// what a GUI preview iterates over.
pub fn plan_sheets(
    job: &PrintJob,
    capabilities: &crate::spool::DeviceCapabilities,
) -> Result<Vec<crate::Sheet>, PrintError> {
    crate::layout::impose(job, capabilities.hardware_margins)
}
