//! The headless backend: writes sheets to disk and records the job it was
//! given. It is the CI backend and how every layout test asserts without a
//! printer attached.

use std::path::PathBuf;
use std::sync::Mutex;

use super::{DeviceCapabilities, JobId, JobStatus, PrinterId, PrinterInfo, SpoolJob, Spooler};
use crate::PrintError;

/// What a `FileSpooler` recorded about one submission.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedJob {
    pub id: JobId,
    pub printer: PrinterId,
    pub title: String,
    /// Files written for this job, in sheet order.
    pub files: Vec<PathBuf>,
    /// True when the job took the pass-through path.
    pub pass_through: bool,
}

/// Writes each sheet to `dir` as PNG.
#[derive(Debug)]
pub struct FileSpooler {
    dir: PathBuf,
    jobs: Mutex<Vec<RecordedJob>>,
}

impl FileSpooler {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            jobs: Mutex::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Every job submitted so far, oldest first.
    #[must_use]
    pub fn recorded(&self) -> Vec<RecordedJob> {
        self.jobs.lock().expect("file spooler mutex").clone()
    }
}

impl Spooler for FileSpooler {
    fn printers(&self) -> Result<Vec<PrinterInfo>, PrintError> {
        let _ = &self.dir;
        unimplemented!("phase 3a")
    }

    fn capabilities(&self, printer: &PrinterId) -> Result<DeviceCapabilities, PrintError> {
        let _ = printer;
        unimplemented!("phase 3a")
    }

    fn submit(&self, job: SpoolJob<'_>) -> Result<JobId, PrintError> {
        let _ = job;
        unimplemented!("phase 3a")
    }

    fn status(&self, job: &JobId) -> Result<JobStatus, PrintError> {
        let _ = job;
        unimplemented!("phase 3a")
    }

    fn cancel(&self, job: &JobId) -> Result<(), PrintError> {
        let _ = job;
        unimplemented!("phase 3a")
    }
}
