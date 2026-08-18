//! Document open helpers shared by CLI commands and the serve cache.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use pdf_document::{DocumentLimits, DocumentSnapshot};
use pdf_source::{OwnedBytesSource, PdfSource};

/// Identity used to cache snapshots across serve requests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocumentIdentity {
    pub path: PathBuf,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl DocumentIdentity {
    pub fn from_path(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        Ok(Self {
            path: canonical,
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    pub fn display_path(&self) -> String {
        self.path.display().to_string()
    }
}

/// Open a snapshot from an already-loaded source, without touching disk.
///
/// `open_document`, `open_with_identity`, and `load_source` all bottom out
/// here; a caller that already holds a loaded source (e.g. after running it
/// through `pdf_read::examine*`) should call this directly instead of
/// re-reading the file a second time.
pub fn open_from_source(
    identity: &DocumentIdentity,
    source: Arc<dyn PdfSource>,
    password: Option<&str>,
) -> Result<DocumentSnapshot> {
    DocumentSnapshot::open_with_password(source, DocumentLimits::default(), password)
        .with_context(|| format!("opening {}", identity.path.display()))
}

/// Open a PDF from `path` with an optional password.
pub fn open_document(
    path: &Path,
    password: Option<&str>,
) -> Result<(DocumentIdentity, DocumentSnapshot)> {
    let (identity, source) = load_source(path)?;
    let snapshot = open_from_source(&identity, source as Arc<dyn PdfSource>, password)?;
    Ok((identity, snapshot))
}

/// Open from already-known identity (serve cache miss path).
pub fn open_with_identity(
    identity: &DocumentIdentity,
    password: Option<&str>,
) -> Result<DocumentSnapshot> {
    let bytes = std::fs::read(&identity.path)
        .with_context(|| format!("reading {}", identity.path.display()))?;
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    open_from_source(identity, source, password)
}

/// Load source bytes for doctor-style examination without full open.
pub fn load_source(path: &Path) -> Result<(DocumentIdentity, Arc<OwnedBytesSource>)> {
    let identity = DocumentIdentity::from_path(path)?;
    let bytes = std::fs::read(&identity.path)
        .with_context(|| format!("reading {}", identity.path.display()))?;
    Ok((identity, Arc::new(OwnedBytesSource::new(bytes))))
}
