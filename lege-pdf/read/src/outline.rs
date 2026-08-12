use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pdf_document::{PageIndex, ParseContext};
use pdf_object::{Dictionary, ObjectId, PdfObject};

use crate::RenderSession;

const MAX_OUTLINE_DEPTH: usize = 64;
const MAX_OUTLINE_NODES: usize = 100_000;

/// One resolved source-outline node. Page indexes are zero-based source space.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedBookmarkNode {
    pub title: String,
    pub source_page: usize,
    /// Destination Y in PDF user space, when the destination form carries it.
    pub top: Option<f32>,
    pub children: Vec<OwnedBookmarkNode>,
}

/// Extract the source outline without rasterizing any page.
///
/// Malformed and unresolved nodes are omitted. Their resolvable children are
/// promoted so one bad parent does not discard an otherwise useful subtree.
pub fn extract_outline(session: &RenderSession) -> Vec<OwnedBookmarkNode> {
    OutlineReader::new(session).read()
}

struct OutlineReader<'a> {
    session: &'a RenderSession,
    context: ParseContext,
    page_by_object: HashMap<ObjectId, usize>,
    named_destinations: HashMap<Vec<u8>, PdfObject>,
    visited_nodes: HashSet<ObjectId>,
    visited_name_tree_nodes: HashSet<ObjectId>,
    nodes_seen: usize,
}

impl<'a> OutlineReader<'a> {
    fn new(session: &'a RenderSession) -> Self {
        let page_by_object = (0..session.page_count())
            .filter_map(|index| {
                session
                    .snapshot
                    .page(PageIndex(index))
                    .ok()
                    .and_then(|page| page.object.map(|object| (object, index as usize)))
            })
            .collect();
        Self {
            session,
            context: ParseContext::new(),
            page_by_object,
            named_destinations: HashMap::new(),
            visited_nodes: HashSet::new(),
            visited_name_tree_nodes: HashSet::new(),
            nodes_seen: 0,
        }
    }

    fn read(mut self) -> Vec<OwnedBookmarkNode> {
        let Some(catalog) = self.catalog() else {
            return Vec::new();
        };
        self.collect_named_destinations(&catalog);

        let outlines_key = self.key(b"Outlines");
        let Some(catalog_dict) = catalog.as_dict() else {
            return Vec::new();
        };
        let Some(outlines) = catalog_dict.get(outlines_key).cloned() else {
            return Vec::new();
        };
        let Some(outline_root) = self.resolve_value(&outlines) else {
            return Vec::new();
        };
        let Some(root_dict) = outline_root.as_dict() else {
            return Vec::new();
        };
        let Some(first) = root_dict.get(self.key(b"First")).cloned() else {
            return Vec::new();
        };
        self.walk_siblings(first, 0)
    }

    fn catalog(&mut self) -> Option<Arc<PdfObject>> {
        let root = self.session.snapshot.structure().trailer.root;
        self.resolve_reference(root)
    }

    fn collect_named_destinations(&mut self, catalog_object: &PdfObject) {
        let Some(catalog) = catalog_object.as_dict() else {
            return;
        };

        // The legacy catalog /Dests dictionary is the fallback. Populate it
        // first so the PDF 1.2+ name tree wins on duplicate names.
        if let Some(legacy) = catalog.get(self.key(b"Dests")).cloned()
            && let Some(legacy) = self.resolve_value(&legacy)
            && let Some(dict) = legacy.as_dict()
        {
            let entries = dict
                .iter()
                .map(|(name, value)| {
                    (
                        self.session.snapshot.names().resolve(name).to_vec(),
                        value.clone(),
                    )
                })
                .collect::<Vec<_>>();
            self.named_destinations.extend(entries);
        }

        let Some(names_object) = catalog.get(self.key(b"Names")).cloned() else {
            return;
        };
        let Some(names_object) = self.resolve_value(&names_object) else {
            return;
        };
        let Some(names_dict) = names_object.as_dict() else {
            return;
        };
        let Some(dests_tree) = names_dict.get(self.key(b"Dests")).cloned() else {
            return;
        };
        self.collect_name_tree(dests_tree, 0);
    }

    fn collect_name_tree(&mut self, node: PdfObject, depth: usize) {
        if depth > MAX_OUTLINE_DEPTH {
            return;
        }
        if let Some(id) = node.as_reference()
            && !self.visited_name_tree_nodes.insert(id)
        {
            return;
        }
        let Some(node) = self.resolve_value(&node) else {
            return;
        };
        let Some(dict) = node.as_dict() else {
            return;
        };

        if let Some(PdfObject::Array(entries)) = dict.get(self.key(b"Names")) {
            for pair in entries.chunks_exact(2) {
                if let Some(name) = destination_name(&pair[0], self.session) {
                    self.named_destinations.insert(name, pair[1].clone());
                }
            }
        }

        let kids = match dict.get(self.key(b"Kids")) {
            Some(PdfObject::Array(kids)) => kids.iter().cloned().collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        for kid in kids {
            self.collect_name_tree(kid, depth + 1);
        }
    }

    fn walk_siblings(&mut self, first: PdfObject, depth: usize) -> Vec<OwnedBookmarkNode> {
        if depth > MAX_OUTLINE_DEPTH {
            return Vec::new();
        }

        let mut output = Vec::new();
        let mut current = Some(first);
        while let Some(raw_node) = current.take() {
            if self.nodes_seen >= MAX_OUTLINE_NODES {
                break;
            }
            self.nodes_seen += 1;
            if let Some(id) = raw_node.as_reference()
                && !self.visited_nodes.insert(id)
            {
                break;
            }

            let Some(node) = self.resolve_value(&raw_node) else {
                break;
            };
            let Some(dict) = node.as_dict() else {
                break;
            };

            let title = dict
                .get(self.key(b"Title"))
                .cloned()
                .and_then(|value| self.resolve_value(&value))
                .and_then(|value| match &*value {
                    PdfObject::String(text) => Some(decode_pdf_text_string(text.as_bytes())),
                    _ => None,
                })
                .unwrap_or_default();
            let first_child = dict.get(self.key(b"First")).cloned();
            let next = dict.get(self.key(b"Next")).cloned();
            let destination = dict
                .get(self.key(b"Dest"))
                .cloned()
                .or_else(|| self.action_destination(dict));

            let children = first_child
                .map(|child| self.walk_siblings(child, depth + 1))
                .unwrap_or_default();
            if let Some((source_page, top)) = destination
                .as_ref()
                .and_then(|value| self.resolve_destination(value, &mut HashSet::new()))
            {
                output.push(OwnedBookmarkNode {
                    title,
                    source_page,
                    top,
                    children,
                });
            } else {
                output.extend(children);
            }

            current = next;
        }
        output
    }

    fn action_destination(&mut self, node: &Dictionary) -> Option<PdfObject> {
        let action = node.get(self.key(b"A"))?.clone();
        let action = self.resolve_value(&action)?;
        let action = action.as_dict()?;
        let kind = action.get(self.key(b"S"))?.as_name()?;
        if self.session.snapshot.names().resolve(kind).as_ref() != b"GoTo" {
            return None;
        }
        action.get(self.key(b"D")).cloned()
    }

    fn resolve_destination(
        &mut self,
        value: &PdfObject,
        named_seen: &mut HashSet<Vec<u8>>,
    ) -> Option<(usize, Option<f32>)> {
        let value = self.resolve_value(value)?;
        match &*value {
            PdfObject::Array(items) => self.resolve_destination_array(items),
            PdfObject::Dictionary(dict) => {
                let nested = dict.get(self.key(b"D"))?;
                self.resolve_destination(nested, named_seen)
            }
            PdfObject::Name(_) | PdfObject::String(_) => {
                let name = destination_name(&value, self.session)?;
                if !named_seen.insert(name.clone()) {
                    return None;
                }
                let destination = self.named_destinations.get(&name)?.clone();
                self.resolve_destination(&destination, named_seen)
            }
            _ => None,
        }
    }

    fn resolve_destination_array(&mut self, items: &[PdfObject]) -> Option<(usize, Option<f32>)> {
        let source_page = match items.first()? {
            PdfObject::Reference(id) => *self.page_by_object.get(id)?,
            PdfObject::Integer(index) => usize::try_from(*index)
                .ok()
                .filter(|index| *index < self.session.page_count() as usize)?,
            _ => return None,
        };

        let fit = items
            .get(1)
            .and_then(PdfObject::as_name)
            .map(|name| self.session.snapshot.names().resolve(name));
        let top_index = match fit.as_deref() {
            Some(name) if name == b"XYZ" => Some(3),
            Some(name) if name == b"FitH" || name == b"FitBH" => Some(2),
            Some(name) if name == b"FitR" => Some(5),
            _ => None,
        };
        let top = top_index
            .and_then(|index| items.get(index))
            .and_then(|value| self.resolve_number(value))
            .filter(|value| value.is_finite())
            .map(|value| value as f32);
        Some((source_page, top))
    }

    fn resolve_number(&mut self, value: &PdfObject) -> Option<f64> {
        self.resolve_value(value)?.as_number()
    }

    fn resolve_value(&mut self, value: &PdfObject) -> Option<Arc<PdfObject>> {
        match value {
            PdfObject::Reference(id) => self.resolve_reference(*id),
            other => Some(Arc::new(other.clone())),
        }
    }

    fn resolve_reference(&mut self, id: ObjectId) -> Option<Arc<PdfObject>> {
        self.session
            .snapshot
            .objects()
            .resolve(&self.session.snapshot, id, &mut self.context)
            .ok()
    }

    fn key(&self, name: &[u8]) -> pdf_object::NameId {
        self.session.snapshot.names().intern(name)
    }
}

fn destination_name(value: &PdfObject, session: &RenderSession) -> Option<Vec<u8>> {
    match value {
        PdfObject::Name(name) => Some(session.snapshot.names().resolve(*name).to_vec()),
        PdfObject::String(text) => Some(text.as_bytes().to_vec()),
        _ => None,
    }
}

pub(crate) fn decode_pdf_text_string(bytes: &[u8]) -> String {
    if let Some(body) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units = body
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]));
        return char::decode_utf16(units)
            .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
    }
    if let Some(body) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units = body
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
        return char::decode_utf16(units)
            .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
    }
    if let Some(body) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(body).into_owned();
    }

    bytes
        .iter()
        .filter_map(|byte| pdf_doc_char(*byte))
        .collect()
}

fn pdf_doc_char(byte: u8) -> Option<char> {
    let value = match byte {
        0x18 => 0x02D8,
        0x19 => 0x02C7,
        0x1A => 0x02C6,
        0x1B => 0x02D9,
        0x1C => 0x02DD,
        0x1D => 0x02DB,
        0x1E => 0x02DA,
        0x1F => 0x02DC,
        0x20..=0x7E => byte as u32,
        0x80 => 0x2022,
        0x81 => 0x2020,
        0x82 => 0x2021,
        0x83 => 0x2026,
        0x84 => 0x2014,
        0x85 => 0x2013,
        0x86 => 0x0192,
        0x87 => 0x2044,
        0x88 => 0x2039,
        0x89 => 0x203A,
        0x8A => 0x2212,
        0x8B => 0x2030,
        0x8C => 0x201E,
        0x8D => 0x201C,
        0x8E => 0x201D,
        0x8F => 0x2018,
        0x90 => 0x2019,
        0x91 => 0x201A,
        0x92 => 0x2122,
        0x93 => 0xFB01,
        0x94 => 0xFB02,
        0x95 => 0x0141,
        0x96 => 0x0152,
        0x97 => 0x0160,
        0x98 => 0x0178,
        0x99 => 0x017D,
        0x9A => 0x0131,
        0x9B => 0x0142,
        0x9C => 0x0153,
        0x9D => 0x0161,
        0x9E => 0x017E,
        0xA0 => 0x20AC,
        0xA1..=0xAC | 0xAE..=0xFF => byte as u32,
        _ => return None,
    };
    char::from_u32(value)
}

#[cfg(test)]
mod tests {
    use super::decode_pdf_text_string;

    #[test]
    fn decodes_utf16be_titles() {
        assert_eq!(
            decode_pdf_text_string(&[0xFE, 0xFF, 0x00, 0xDC, 0x00, 0x62, 0x00, 0x65, 0x00, 0x72]),
            "Über"
        );
    }

    #[test]
    fn decodes_pdf_doc_encoding_titles() {
        assert_eq!(
            decode_pdf_text_string(b"Price \xA0 5 \x95uvre"),
            "Price € 5 Łuvre"
        );
    }
}
