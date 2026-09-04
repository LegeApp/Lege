//! Windows backend: `winspool.drv` for the queue, GDI for the raster path.
//!
//! Windows has no native PDF spool format, so every job here is composed.

use super::{DeviceCapabilities, JobId, JobStatus, PrinterId, PrinterInfo, SpoolJob, Spooler};
use crate::PrintError;

#[derive(Debug, Default)]
pub struct WindowsSpooler {
    _private: (),
}

impl WindowsSpooler {
    #[must_use]
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Spooler for WindowsSpooler {
    fn printers(&self) -> Result<Vec<PrinterInfo>, PrintError> {
        unimplemented!("phase 3c")
    }

    fn capabilities(&self, printer: &PrinterId) -> Result<DeviceCapabilities, PrintError> {
        let _ = printer;
        unimplemented!("phase 3c")
    }

    fn submit(&self, job: SpoolJob<'_>) -> Result<JobId, PrintError> {
        let _ = job;
        unimplemented!("phase 3c")
    }

    fn status(&self, job: &JobId) -> Result<JobStatus, PrintError> {
        let _ = job;
        unimplemented!("phase 3c")
    }

    fn cancel(&self, job: &JobId) -> Result<(), PrintError> {
        let _ = job;
        unimplemented!("phase 3c")
    }
}
