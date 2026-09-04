//! CUPS backend for Linux and macOS, driving `lp` / `lpstat` / `lpoptions`.
//!
//! The CLI rather than `libcups`: no native dependency, no build-time
//! headers, no linking question, and it covers pass-through printing
//! completely.

use super::{DeviceCapabilities, JobId, JobStatus, PrinterId, PrinterInfo, SpoolJob, Spooler};
use crate::PrintError;

/// Drives the CUPS command-line tools.
#[derive(Debug, Default)]
pub struct CupsSpooler {
    _private: (),
}

impl CupsSpooler {
    #[must_use]
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Spooler for CupsSpooler {
    fn printers(&self) -> Result<Vec<PrinterInfo>, PrintError> {
        unimplemented!("phase 3b")
    }

    fn capabilities(&self, printer: &PrinterId) -> Result<DeviceCapabilities, PrintError> {
        let _ = printer;
        unimplemented!("phase 3b")
    }

    fn submit(&self, job: SpoolJob<'_>) -> Result<JobId, PrintError> {
        let _ = job;
        unimplemented!("phase 3b")
    }

    fn status(&self, job: &JobId) -> Result<JobStatus, PrintError> {
        let _ = job;
        unimplemented!("phase 3b")
    }

    fn cancel(&self, job: &JobId) -> Result<(), PrintError> {
        let _ = job;
        unimplemented!("phase 3b")
    }
}
