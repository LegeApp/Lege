use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pdf_object::{Dictionary, ObjectId, PdfObject, decode_text_string_lossy};

use crate::{DocumentSnapshot, PageIndex, ParseContext};

const MAX_OUTLINE_NODES: usize = 16_384;
const MAX_OUTLINE_DEPTH: u16 = 64;
const MAX_NAME_TREE_NODES: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationFit {
    Xyz,
    Fit,
    FitHorizontal,
    FitVertical,
    FitRectangle,
    FitBoundingBox,
    FitBoundingBoxHorizontal,
    FitBoundingBoxVertical,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DocumentDestination {
    pub page: PageIndex,
    pub fit: DestinationFit,
    pub left: Option<f64>,
    pub top: Option<f64>,
    pub right: Option<f64>,
    pub bottom: Option<f64>,
    pub zoom: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentOutlineItem {
    pub title: Arc<str>,
    pub destination: Option<DocumentDestination>,
    pub depth: u16,
    pub initially_open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutlineIssue {
    MalformedRoot,
    MalformedNode,
    ReferenceCycle(ObjectId),
    DepthLimit,
    NodeLimit,
    UnsupportedDestination,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocumentOutline {
    pub items: Arc<[DocumentOutlineItem]>,
    pub issues: Arc<[OutlineIssue]>,
}

pub(crate) fn extract(snapshot: &DocumentSnapshot, ctx: &mut ParseContext) -> DocumentOutline {
    ctx.begin_job();
    let Some(catalog) = resolve_id(snapshot, snapshot.structure().trailer.root, ctx) else {
        return DocumentOutline {
            issues: Arc::from([OutlineIssue::MalformedRoot]),
            ..DocumentOutline::default()
        };
    };
    let Some(catalog) = catalog.as_dict() else {
        return DocumentOutline {
            issues: Arc::from([OutlineIssue::MalformedRoot]),
            ..DocumentOutline::default()
        };
    };

    let names = snapshot.names();
    let named = collect_named_destinations(snapshot, catalog, ctx);
    let Some(root) = catalog.get(names.intern(b"Outlines")) else {
        return DocumentOutline::default();
    };
    let Some(root) = resolve_value(snapshot, root, ctx) else {
        return DocumentOutline {
            issues: Arc::from([OutlineIssue::MalformedRoot]),
            ..DocumentOutline::default()
        };
    };
    let Some(root) = root.as_dict() else {
        return DocumentOutline {
            issues: Arc::from([OutlineIssue::MalformedRoot]),
            ..DocumentOutline::default()
        };
    };
    let Some(first) = root.get(names.intern(b"First")).cloned() else {
        return DocumentOutline::default();
    };

    let mut walker = OutlineWalker {
        snapshot,
        ctx,
        named,
        seen: HashSet::new(),
        items: Vec::new(),
        issues: Vec::new(),
    };
    walker.walk_siblings(first, 0);
    DocumentOutline {
        items: walker.items.into(),
        issues: walker.issues.into(),
    }
}

struct OutlineWalker<'a> {
    snapshot: &'a DocumentSnapshot,
    ctx: &'a mut ParseContext,
    named: HashMap<Vec<u8>, PdfObject>,
    seen: HashSet<ObjectId>,
    items: Vec<DocumentOutlineItem>,
    issues: Vec<OutlineIssue>,
}

impl OutlineWalker<'_> {
    fn walk_siblings(&mut self, first: PdfObject, depth: u16) {
        if depth > MAX_OUTLINE_DEPTH {
            self.issues.push(OutlineIssue::DepthLimit);
            return;
        }
        let mut current = Some(first);
        while let Some(raw) = current.take() {
            if self.items.len() >= MAX_OUTLINE_NODES {
                self.issues.push(OutlineIssue::NodeLimit);
                return;
            }
            if let Some(id) = raw.as_reference()
                && !self.seen.insert(id)
            {
                self.issues.push(OutlineIssue::ReferenceCycle(id));
                return;
            }
            let Some(resolved) = resolve_value(self.snapshot, &raw, self.ctx) else {
                self.issues.push(OutlineIssue::MalformedNode);
                return;
            };
            let Some(dict) = resolved.as_dict() else {
                self.issues.push(OutlineIssue::MalformedNode);
                return;
            };
            let names = self.snapshot.names();
            let title = match dict.get(names.intern(b"Title")) {
                Some(PdfObject::String(value)) => decode_text_string_lossy(value.as_bytes()),
                _ => String::new(),
            };
            let destination = self.destination_for_node(dict);
            if !title.trim().is_empty() {
                self.items.push(DocumentOutlineItem {
                    title: Arc::from(title.trim()),
                    destination,
                    depth,
                    initially_open: dict
                        .get(names.intern(b"Count"))
                        .and_then(PdfObject::as_int)
                        .is_some_and(|count| count >= 0),
                });
            }
            let child = dict.get(names.intern(b"First")).cloned();
            current = dict.get(names.intern(b"Next")).cloned();
            if let Some(child) = child {
                self.walk_siblings(child, depth.saturating_add(1));
            }
        }
    }

    fn destination_for_node(&mut self, dict: &Dictionary) -> Option<DocumentDestination> {
        let names = self.snapshot.names();
        if let Some(dest) = dict.get(names.intern(b"Dest")).cloned() {
            return self.resolve_destination(dest);
        }
        let action = dict.get(names.intern(b"A"))?;
        let action = resolve_value(self.snapshot, action, self.ctx)?;
        let action = action.as_dict()?;
        let action_kind = action
            .get(names.intern(b"S"))
            .and_then(PdfObject::as_name)
            .map(|name| names.resolve(name));
        if action_kind.as_deref() != Some(&b"GoTo"[..]) {
            return None;
        }
        self.resolve_destination(action.get(names.intern(b"D"))?.clone())
    }

    fn resolve_destination(&mut self, raw: PdfObject) -> Option<DocumentDestination> {
        resolve_destination(self.snapshot, self.ctx, &self.named, raw).or_else(|| {
            self.issues.push(OutlineIssue::UnsupportedDestination);
            None
        })
    }
}

pub(crate) fn resolve_destination(
    snapshot: &DocumentSnapshot,
    ctx: &mut ParseContext,
    named: &HashMap<Vec<u8>, PdfObject>,
    raw: PdfObject,
) -> Option<DocumentDestination> {
    let raw = resolve_value(snapshot, &raw, ctx)?;
    match raw.as_ref() {
        PdfObject::Array(array) => parse_destination_array(snapshot, array),
        PdfObject::Name(name) => {
            let key = snapshot.names().resolve(*name);
            resolve_destination(snapshot, ctx, named, named.get(key.as_ref())?.clone())
        }
        PdfObject::String(value) => {
            resolve_destination(snapshot, ctx, named, named.get(value.as_bytes())?.clone())
        }
        PdfObject::Dictionary(dict) => resolve_destination(
            snapshot,
            ctx,
            named,
            dict.get(snapshot.names().intern(b"D"))?.clone(),
        ),
        _ => None,
    }
}

fn parse_destination_array(
    snapshot: &DocumentSnapshot,
    array: &[PdfObject],
) -> Option<DocumentDestination> {
    let first = array.first()?;
    let page = match first {
        PdfObject::Reference(id) => snapshot.inner.pages.index_for_object(*id)?,
        PdfObject::Integer(index) if *index >= 0 => {
            let index = u32::try_from(*index).ok()?;
            (index < snapshot.page_count()).then_some(PageIndex(index))?
        }
        _ => return None,
    };
    let kind = array
        .get(1)
        .and_then(PdfObject::as_name)
        .map(|name| snapshot.names().resolve(name));
    let fit = match kind.as_deref() {
        Some(b"XYZ") => DestinationFit::Xyz,
        Some(b"Fit") => DestinationFit::Fit,
        Some(b"FitH") => DestinationFit::FitHorizontal,
        Some(b"FitV") => DestinationFit::FitVertical,
        Some(b"FitR") => DestinationFit::FitRectangle,
        Some(b"FitB") => DestinationFit::FitBoundingBox,
        Some(b"FitBH") => DestinationFit::FitBoundingBoxHorizontal,
        Some(b"FitBV") => DestinationFit::FitBoundingBoxVertical,
        _ => DestinationFit::Unknown,
    };
    let number = |index: usize| array.get(index).and_then(PdfObject::as_number);
    let (left, top, right, bottom, zoom) = match fit {
        DestinationFit::Xyz => (number(2), number(3), None, None, number(4)),
        DestinationFit::FitHorizontal | DestinationFit::FitBoundingBoxHorizontal => {
            (None, number(2), None, None, None)
        }
        DestinationFit::FitVertical | DestinationFit::FitBoundingBoxVertical => {
            (number(2), None, None, None, None)
        }
        DestinationFit::FitRectangle => (number(2), number(5), number(4), number(3), None),
        _ => (None, None, None, None, None),
    };
    Some(DocumentDestination {
        page,
        fit,
        left,
        top,
        right,
        bottom,
        zoom,
    })
}

pub(crate) fn collect_named_destinations(
    snapshot: &DocumentSnapshot,
    catalog: &Dictionary,
    ctx: &mut ParseContext,
) -> HashMap<Vec<u8>, PdfObject> {
    let names = snapshot.names();
    let mut output = HashMap::new();
    if let Some(dests) = catalog.get(names.intern(b"Dests"))
        && let Some(dests) = resolve_value(snapshot, dests, ctx)
        && let Some(dict) = dests.as_dict()
    {
        for (key, value) in dict.iter() {
            output.insert(names.resolve(key).to_vec(), value.clone());
        }
    }
    if let Some(name_dict) = catalog.get(names.intern(b"Names"))
        && let Some(name_dict) = resolve_value(snapshot, name_dict, ctx)
        && let Some(name_dict) = name_dict.as_dict()
        && let Some(tree) = name_dict.get(names.intern(b"Dests"))
    {
        let mut seen = HashSet::new();
        collect_name_tree(snapshot, tree.clone(), ctx, &mut seen, &mut output);
    }
    output
}

fn collect_name_tree(
    snapshot: &DocumentSnapshot,
    raw: PdfObject,
    ctx: &mut ParseContext,
    seen: &mut HashSet<ObjectId>,
    output: &mut HashMap<Vec<u8>, PdfObject>,
) {
    if seen.len() >= MAX_NAME_TREE_NODES {
        return;
    }
    if let Some(id) = raw.as_reference()
        && !seen.insert(id)
    {
        return;
    }
    let Some(node) = resolve_value(snapshot, &raw, ctx) else {
        return;
    };
    let Some(dict) = node.as_dict() else {
        return;
    };
    let names = snapshot.names();
    if let Some(PdfObject::Array(entries)) = dict.get(names.intern(b"Names")) {
        for pair in entries.chunks_exact(2) {
            let key = match &pair[0] {
                PdfObject::String(value) => Some(value.as_bytes().to_vec()),
                PdfObject::Name(value) => Some(names.resolve(*value).to_vec()),
                _ => None,
            };
            if let Some(key) = key {
                output.insert(key, pair[1].clone());
            }
        }
    }
    if let Some(PdfObject::Array(kids)) = dict.get(names.intern(b"Kids")) {
        for child in kids.iter().cloned() {
            collect_name_tree(snapshot, child, ctx, seen, output);
        }
    }
}

pub(crate) fn resolve_id(
    snapshot: &DocumentSnapshot,
    id: ObjectId,
    ctx: &mut ParseContext,
) -> Option<Arc<PdfObject>> {
    snapshot.objects().resolve(snapshot, id, ctx).ok()
}

pub(crate) fn resolve_value(
    snapshot: &DocumentSnapshot,
    value: &PdfObject,
    ctx: &mut ParseContext,
) -> Option<Arc<PdfObject>> {
    match value {
        PdfObject::Reference(id) => resolve_id(snapshot, *id, ctx),
        direct => Some(Arc::new(direct.clone())),
    }
}
