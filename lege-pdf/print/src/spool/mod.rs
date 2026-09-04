//! Spooler backends.
//!
//! The trait is the whole platform seam: everything above it is portable,
//! everything below it is one operating system's print API.

pub mod file;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod cups;

#[cfg(windows)]
pub mod windows;

use crate::paper::Margins;
use crate::{ComposeOptions, PrintError, PrintOptions, Sheet};

/// A printer's opaque system identifier — a CUPS queue name, a Windows
/// printer name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrinterId(pub String);

impl PrinterId {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PrinterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the system says about one queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterInfo {
    pub id: PrinterId,
    /// Human-readable name, when the system offers one distinct from the id.
    pub description: Option<String>,
    pub location: Option<String>,
    pub is_default: bool,
    pub accepting_jobs: bool,
}

/// What a device can do, as far as imposition needs to know.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceCapabilities {
    /// The unprintable border. Falls back to
    /// [`DEFAULT_HARDWARE_MARGIN_PT`](crate::paper::DEFAULT_HARDWARE_MARGIN_PT).
    pub hardware_margins: Margins,
    pub supports_duplex: bool,
    pub supports_color: bool,
    /// Native device resolution in DPI, when known.
    pub resolution_dpi: Option<f64>,
    /// Whether the queue accepts `application/pdf` directly, which is what
    /// makes the pass-through path possible.
    pub accepts_pdf: bool,
}

impl Default for DeviceCapabilities {
    fn default() -> Self {
        Self {
            hardware_margins: Margins::uniform(crate::paper::DEFAULT_HARDWARE_MARGIN_PT),
            supports_duplex: false,
            supports_color: true,
            resolution_dpi: None,
            accepts_pdf: false,
        }
    }
}

/// The payload handed to a spooler: either the original document bytes, or
/// sheets we composed.
pub enum SpoolPayload<'a> {
    /// The original PDF, spooled unmodified.
    PassThroughPdf(&'a [u8]),
    /// Sheets to rasterize and spool. Composition is the spooler's call, so
    /// that a backend able to stream bands never holds a whole sheet.
    Sheets {
        session: &'a lege_pdf_read::RenderSession,
        sheets: &'a [Sheet],
        compose: ComposeOptions,
    },
}

/// One submission.
pub struct SpoolJob<'a> {
    pub printer: PrinterId,
    /// Job title as it appears in the queue.
    pub title: String,
    pub options: &'a PrintOptions,
    pub payload: SpoolPayload<'a>,
}

// `RenderSession` is not `Debug`, so both of these are written out: a job is
// worth printing in a log line, and the session behind it is not.
impl std::fmt::Debug for SpoolPayload<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PassThroughPdf(bytes) => f
                .debug_struct("PassThroughPdf")
                .field("bytes", &bytes.len())
                .finish(),
            Self::Sheets {
                sheets, compose, ..
            } => f
                .debug_struct("Sheets")
                .field("sheets", &sheets.len())
                .field("compose", compose)
                .finish_non_exhaustive(),
        }
    }
}

impl std::fmt::Debug for SpoolJob<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpoolJob")
            .field("printer", &self.printer)
            .field("title", &self.title)
            .field("payload", &self.payload)
            .finish_non_exhaustive()
    }
}

/// A submitted job's system identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobId(pub String);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Pending,
    Processing,
    Completed,
    Cancelled,
    Aborted,
    /// The backend cannot report on this job — a normal answer for the CLI
    /// spooler once the job has left the queue.
    Unknown,
}

/// The platform seam.
pub trait Spooler: Send + Sync {
    fn printers(&self) -> Result<Vec<PrinterInfo>, PrintError>;
    fn default_printer(&self) -> Result<Option<PrinterId>, PrintError> {
        Ok(self
            .printers()?
            .into_iter()
            .find(|p| p.is_default)
            .map(|p| p.id))
    }
    fn capabilities(&self, printer: &PrinterId) -> Result<DeviceCapabilities, PrintError>;
    fn submit(&self, job: SpoolJob<'_>) -> Result<JobId, PrintError>;
    fn status(&self, job: &JobId) -> Result<JobStatus, PrintError>;
    fn cancel(&self, job: &JobId) -> Result<(), PrintError>;
}

/// The spooler for this platform, or the file backend when none exists.
pub fn platform_spooler() -> Box<dyn Spooler> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        Box::new(cups::CupsSpooler::new())
    }
    #[cfg(windows)]
    {
        Box::new(windows::WindowsSpooler::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Box::new(file::FileSpooler::new(std::env::temp_dir()))
    }
}
