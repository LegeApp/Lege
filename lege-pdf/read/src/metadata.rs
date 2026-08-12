use std::sync::Arc;

use pdf_document::ParseContext;
use pdf_object::{ObjectId, PdfObject};

use crate::RenderSession;
use crate::outline::decode_pdf_text_string;

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
        title: read_string(
            session,
            dict.get(session.snapshot.names().intern(b"Title")),
            &mut context,
        )
        .and_then(|value| credible(value, true)),
        author: read_string(
            session,
            dict.get(session.snapshot.names().intern(b"Author")),
            &mut context,
        )
        .and_then(|value| credible(value, false)),
    }
}

fn read_string(
    session: &RenderSession,
    value: Option<&PdfObject>,
    context: &mut ParseContext,
) -> Option<String> {
    let value: Arc<PdfObject> = match value? {
        PdfObject::Reference(id) => resolve(session, *id, context)?,
        other => Arc::new(other.clone()),
    };
    match &*value {
        PdfObject::String(text) => Some(decode_pdf_text_string(text.as_bytes())),
        _ => None,
    }
}

fn resolve(
    session: &RenderSession,
    id: ObjectId,
    context: &mut ParseContext,
) -> Option<Arc<PdfObject>> {
    session
        .snapshot
        .objects()
        .resolve(&session.snapshot, id, context)
        .ok()
}

fn credible(value: String, title: bool) -> Option<String> {
    let value = value
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if value.is_empty() {
        return None;
    }
    let lower = value.to_lowercase();
    let placeholder = if title {
        matches!(lower.as_str(), "document" | "untitled" | "unknown")
    } else {
        matches!(lower.as_str(), "author" | "unknown")
    };
    (!placeholder).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::credible;

    #[test]
    fn rejects_only_empty_and_generic_identity_fields() {
        assert_eq!(credible(" Document ".into(), true), None);
        assert_eq!(credible("Author".into(), false), None);
        assert_eq!(
            credible("  Ursula   Le Guin ".into(), false).as_deref(),
            Some("Ursula Le Guin")
        );
    }
}
