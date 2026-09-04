pub mod content;
pub mod images;
pub mod inspect;
pub mod mcp;
pub mod print;
pub mod render;
pub mod search;
pub mod serve;
pub mod text;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use pdf_content::PageCompiler;
use pdf_document::{DocumentSnapshot, ParseContext};
use pdf_render_api::AnnotationMode;

use crate::open::{self, DocumentIdentity};
use crate::schema::{Envelope, OutputMode};

/// Shared compile helper: annotations on by default (viewer parity).
pub fn page_compiler(system_fonts: bool, annotations: bool) -> PageCompiler {
    let c = PageCompiler::new().with_annotations(annotations);
    if system_fonts {
        c.with_system_fonts(Arc::new(pdf_font::FolderFontProvider::system()))
    } else {
        c
    }
}

pub fn default_annotations() -> bool {
    matches!(AnnotationMode::default(), AnnotationMode::StaticAppearances)
}

/// Fresh parse context for a page job.
pub fn parse_context() -> ParseContext {
    ParseContext::new()
}

pub fn snapshot_arc(snapshot: DocumentSnapshot) -> Arc<DocumentSnapshot> {
    Arc::new(snapshot)
}

/// Take the caller-supplied snapshot when there is one (the `serve`/`mcp`
/// cache hands one over), else open `path` from disk.
///
/// A cached snapshot arrives without a stat, so the placeholder identity
/// carries only the path: `len`/`modified` exist to invalidate the cache, and
/// the cache has already made that decision by the time we are called.
pub fn resolve_snapshot(
    path: &Path,
    password: Option<&str>,
    snapshot: Option<Arc<DocumentSnapshot>>,
    identity: Option<DocumentIdentity>,
) -> Result<(DocumentIdentity, Arc<DocumentSnapshot>)> {
    if let Some(snapshot) = snapshot {
        let identity = identity.unwrap_or_else(|| DocumentIdentity {
            path: path.to_path_buf(),
            len: 0,
            modified: None,
        });
        return Ok((identity, snapshot));
    }
    let (identity, snap) = open::open_document(path, password)?;
    Ok((identity, Arc::new(snap)))
}

/// Emit one page-local failure record.
///
/// `multi` selects the pretty single-envelope form over JSONL, matching
/// `emit`: a run that will produce more than one record must stay
/// newline-delimited so a partial stream is still parseable.
pub fn emit_failed(env: &Envelope, mode: OutputMode, multi: bool) -> Result<()> {
    match mode {
        OutputMode::Human => {
            eprintln!(
                "error: page {}: {}",
                env.page.unwrap_or(0),
                env.data
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("failed")
            );
            Ok(())
        }
        OutputMode::Json if !multi => env.write_json(),
        OutputMode::Json | OutputMode::Jsonl => env.write_jsonl(),
    }
}
