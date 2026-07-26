use std::sync::Arc;

use pdf_object::{PdfObject, decode_text_string_lossy};

use crate::outline::{collect_named_destinations, resolve_destination, resolve_id, resolve_value};
use crate::{DocumentDestination, DocumentSnapshot, PageIndex, ParseContext};

#[derive(Debug, Clone, PartialEq)]
pub enum DocumentLinkTarget {
    Internal(DocumentDestination),
    Uri(Arc<str>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentLink {
    /// Link rectangle in default PDF user space.
    pub rect: [f64; 4],
    pub target: DocumentLinkTarget,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocumentLinks {
    /// Directly page-indexed link arrays.
    pub pages: Arc<[Arc<[DocumentLink]>]>,
}

pub(crate) fn extract(snapshot: &DocumentSnapshot, ctx: &mut ParseContext) -> DocumentLinks {
    ctx.begin_job();
    let names = snapshot.names();
    let named = resolve_id(snapshot, snapshot.structure().trailer.root, ctx)
        .and_then(|catalog| {
            catalog
                .as_dict()
                .map(|catalog| collect_named_destinations(snapshot, catalog, ctx))
        })
        .unwrap_or_default();
    let link_name = names.intern(b"Link");
    let mut pages = Vec::with_capacity(snapshot.page_count() as usize);

    for page_number in 0..snapshot.page_count() {
        let Some(page) = snapshot.page(PageIndex(page_number)).ok() else {
            pages.push(Arc::from([]));
            continue;
        };
        let mut links = Vec::new();
        for annotation in page.annotations.iter() {
            if annotation.hidden_for_display()
                || annotation.subtype != Some(link_name)
                || annotation.rect[2] <= annotation.rect[0]
                || annotation.rect[3] <= annotation.rect[1]
            {
                continue;
            }
            let target = annotation
                .destination
                .clone()
                .and_then(|raw| resolve_destination(snapshot, ctx, &named, raw))
                .map(DocumentLinkTarget::Internal)
                .or_else(|| action_target(snapshot, ctx, &named, annotation.action.as_ref()));
            if let Some(target) = target {
                links.push(DocumentLink {
                    rect: annotation.rect,
                    target,
                });
            }
        }
        pages.push(links.into());
    }

    DocumentLinks {
        pages: pages.into(),
    }
}

fn action_target(
    snapshot: &DocumentSnapshot,
    ctx: &mut ParseContext,
    named: &std::collections::HashMap<Vec<u8>, PdfObject>,
    raw: Option<&PdfObject>,
) -> Option<DocumentLinkTarget> {
    let action = resolve_value(snapshot, raw?, ctx)?;
    let action = action.as_dict()?;
    let names = snapshot.names();
    match action
        .get(names.intern(b"S"))
        .and_then(PdfObject::as_name)
        .map(|name| names.resolve(name))
        .as_deref()
    {
        Some(b"GoTo") => resolve_destination(
            snapshot,
            ctx,
            named,
            action.get(names.intern(b"D"))?.clone(),
        )
        .map(DocumentLinkTarget::Internal),
        Some(b"URI") => {
            let uri = match action.get(names.intern(b"URI"))? {
                PdfObject::String(value) => decode_text_string_lossy(value.as_bytes()),
                PdfObject::Name(value) => String::from_utf8_lossy(&names.resolve(*value)).into(),
                _ => return None,
            };
            let uri = uri.trim();
            (!uri.is_empty()).then(|| DocumentLinkTarget::Uri(Arc::from(uri)))
        }
        _ => None,
    }
}
