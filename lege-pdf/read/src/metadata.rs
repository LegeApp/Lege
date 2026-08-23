use std::sync::Arc;

use pdf_document::ParseContext;
use pdf_object::{Dictionary, PdfObject};

use crate::RenderSession;
use crate::outline::decode_pdf_text_string;

/// Values so generic that they say nothing about the document. Producers emit
/// them as defaults, so treating them as identity would be worse than nothing.
const TITLE_PLACEHOLDERS: &[&str] = &["document", "untitled", "unknown"];
const AUTHOR_PLACEHOLDERS: &[&str] = &["author", "unknown"];

/// Credible document identity carried by the source PDF's Info dictionary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
}

/// Extract `/Title` and `/Author` without rendering a page.
pub fn extract_metadata(session: &RenderSession) -> DocumentMetadata {
    let Some(info_id) = session.snapshot.structure().trailer.info else {
        return DocumentMetadata::default();
    };
    let mut context = ParseContext::new();
    let Ok(info) = session
        .snapshot
        .objects()
        .resolve(&session.snapshot, info_id, &mut context)
    else {
        return DocumentMetadata::default();
    };
    let Some(dict) = info.as_dict() else {
        return DocumentMetadata::default();
    };

    DocumentMetadata {
        title: info_string(session, dict, b"Title", &mut context)
            .and_then(|value| credible(value, TITLE_PLACEHOLDERS)),
        author: info_string(session, dict, b"Author", &mut context)
            .and_then(|value| credible(value, AUTHOR_PLACEHOLDERS)),
    }
}

/// Read one Info-dictionary entry as text, following a single indirect
/// reference. Anything that is not a PDF string is not identity.
fn info_string(
    session: &RenderSession,
    info: &Dictionary,
    key: &[u8],
    context: &mut ParseContext,
) -> Option<String> {
    let resolved: Arc<PdfObject> = match info.get(session.snapshot.names().intern(key))? {
        PdfObject::Reference(id) => session
            .snapshot
            .objects()
            .resolve(&session.snapshot, *id, context)
            .ok()?,
        other => Arc::new(other.clone()),
    };
    match &*resolved {
        PdfObject::String(text) => Some(decode_pdf_text_string(text.as_bytes())),
        _ => None,
    }
}

/// Collapse whitespace and reject anything that carries no identity.
///
/// Control characters are removed rather than treated as separators, so an
/// embedded newline joins its neighbours instead of splitting them.
fn credible(value: String, placeholders: &[&str]) -> Option<String> {
    let printable = value
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    let value = printable.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        return None;
    }
    let lower = value.to_lowercase();
    (!placeholders.contains(&lower.as_str())).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{AUTHOR_PLACEHOLDERS, TITLE_PLACEHOLDERS, credible};

    #[test]
    fn rejects_only_empty_and_generic_identity_fields() {
        assert_eq!(credible(" Document ".into(), TITLE_PLACEHOLDERS), None);
        assert_eq!(credible("Author".into(), AUTHOR_PLACEHOLDERS), None);
        assert_eq!(
            credible("  Ursula   Le Guin ".into(), AUTHOR_PLACEHOLDERS).as_deref(),
            Some("Ursula Le Guin")
        );
    }
}
