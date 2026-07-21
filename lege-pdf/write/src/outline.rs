//! Outline (bookmark) tree: First/Last/Prev/Next/Parent/Count linking,
//! GoTo destinations resolved from WrittenPageSlots.
//!
//! Input items carry an *output* page index (Lege resolves any reflow
//! source→output mapping before handing them over). An item whose page has not
//! been written is skipped along with its subtree, matching the current
//! writer's behavior. Titles are emitted as PDF text strings (UTF-16BE with a
//! BOM when non-ASCII).

use std::io::Write;

use crate::pages::WrittenPageSlots;
use crate::serialize::{write_i64, write_literal_string, write_name, write_ref};
use crate::sink::PdfSink;
use crate::types::{ObjectId, Result};

/// A bookmark node from Lege. `page_index` is the 0-based output page index.
pub struct OutlineItem {
    pub title: String,
    pub page_index: u32,
    pub children: Vec<OutlineItem>,
}

/// A resolved node with its allocated object id.
struct Node {
    id: ObjectId,
    title: String,
    page: ObjectId,
    children: Vec<Node>,
}

/// Write the whole outline tree and return the `/Outlines` root id, or `None`
/// if nothing resolved (no `/Outlines` entry should then be added to the
/// catalog).
pub fn write_outline<W: Write>(
    sink: &mut PdfSink<W>,
    slots: &WrittenPageSlots,
    items: &[OutlineItem],
) -> Result<Option<ObjectId>> {
    let root_id = sink.alloc_id();
    let nodes = build_nodes(sink, slots, items);
    if nodes.is_empty() {
        // Nothing to link; do not emit an empty, dangling Outlines object.
        return Ok(None);
    }

    write_nodes(sink, &nodes, root_id)?;

    let total: i64 = nodes.iter().map(|n| 1 + descendants(n)).sum();
    let mut d = Vec::new();
    d.extend_from_slice(b"<<");
    key(&mut d, b"Type");
    write_name(&mut d, b"Outlines");
    key(&mut d, b"First");
    write_ref(&mut d, nodes.first().unwrap().id);
    key(&mut d, b"Last");
    write_ref(&mut d, nodes.last().unwrap().id);
    key(&mut d, b"Count");
    write_i64(&mut d, total);
    d.extend_from_slice(b">>");
    sink.write_indirect(root_id, &d)?;

    Ok(Some(root_id))
}

/// Resolve pages and allocate ids DFS. Unresolvable nodes (and their subtrees)
/// are dropped.
fn build_nodes<W: Write>(
    sink: &mut PdfSink<W>,
    slots: &WrittenPageSlots,
    items: &[OutlineItem],
) -> Vec<Node> {
    let mut out = Vec::new();
    for item in items {
        let Some(page) = slots.page_id(item.page_index) else {
            continue;
        };
        let id = sink.alloc_id();
        let children = build_nodes(sink, slots, &item.children);
        out.push(Node {
            id,
            title: item.title.clone(),
            page,
            children,
        });
    }
    out
}

fn write_nodes<W: Write>(sink: &mut PdfSink<W>, nodes: &[Node], parent: ObjectId) -> Result<()> {
    for (i, node) in nodes.iter().enumerate() {
        let prev = if i > 0 { Some(nodes[i - 1].id) } else { None };
        let next = nodes.get(i + 1).map(|n| n.id);

        let mut d = Vec::new();
        d.extend_from_slice(b"<<");
        key(&mut d, b"Title");
        write_text_string(&mut d, &node.title);
        key(&mut d, b"Parent");
        write_ref(&mut d, parent);
        if let Some(p) = prev {
            key(&mut d, b"Prev");
            write_ref(&mut d, p);
        }
        if let Some(n) = next {
            key(&mut d, b"Next");
            write_ref(&mut d, n);
        }
        if !node.children.is_empty() {
            key(&mut d, b"First");
            write_ref(&mut d, node.children.first().unwrap().id);
            key(&mut d, b"Last");
            write_ref(&mut d, node.children.last().unwrap().id);
            key(&mut d, b"Count");
            write_i64(&mut d, descendants(node));
        }
        // Destination: [page /Fit].
        key(&mut d, b"Dest");
        d.push(b'[');
        write_ref(&mut d, node.page);
        d.extend_from_slice(b" /Fit]");
        d.extend_from_slice(b">>");
        sink.write_indirect(node.id, &d)?;

        write_nodes(sink, &node.children, node.id)?;
    }
    Ok(())
}

/// Total number of descendants (open tree), used for `/Count`.
fn descendants(node: &Node) -> i64 {
    node.children.iter().map(|c| 1 + descendants(c)).sum()
}

/// Write a PDF text string: a literal for ASCII, else a UTF-16BE hex string
/// with a byte-order mark.
fn write_text_string(out: &mut Vec<u8>, s: &str) {
    if s.is_ascii() {
        write_literal_string(out, s.as_bytes());
    } else {
        out.push(b'<');
        push_hex(out, 0xFE);
        push_hex(out, 0xFF);
        for u in s.encode_utf16() {
            let [hi, lo] = u.to_be_bytes();
            push_hex(out, hi);
            push_hex(out, lo);
        }
        out.push(b'>');
    }
}

fn push_hex(out: &mut Vec<u8>, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push(HEX[(b >> 4) as usize]);
    out.push(HEX[(b & 0x0f) as usize]);
}

fn key(d: &mut Vec<u8>, k: &[u8]) {
    write_name(d, k);
    d.push(b' ');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots_with(n: usize) -> WrittenPageSlots {
        let mut s = WrittenPageSlots::new(n);
        for i in 0..n {
            // Page object ids 100.. so they are distinguishable in output.
            s.set(i as u32, ObjectId::new(100 + i as u32)).unwrap();
        }
        s
    }

    #[test]
    fn empty_or_unresolved_yields_none() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let slots = slots_with(1);
        assert!(write_outline(&mut sink, &slots, &[]).unwrap().is_none());

        // page_index out of range -> dropped -> None
        let items = vec![OutlineItem {
            title: "x".into(),
            page_index: 9,
            children: vec![],
        }];
        assert!(write_outline(&mut sink, &slots, &items).unwrap().is_none());
    }

    #[test]
    fn nested_tree_links_correctly() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let slots = slots_with(3);
        let items = vec![
            OutlineItem {
                title: "Chapter 1".into(),
                page_index: 0,
                children: vec![OutlineItem {
                    title: "Section 1.1".into(),
                    page_index: 1,
                    children: vec![],
                }],
            },
            OutlineItem {
                title: "Chapter 2".into(),
                page_index: 2,
                children: vec![],
            },
        ];
        let root = write_outline(&mut sink, &slots, &items).unwrap().unwrap();
        let t = String::from_utf8_lossy(&sink.finish().unwrap()).into_owned();

        assert!(t.contains("/Type /Outlines"));
        // Root count = 3 items total (2 chapters + 1 section).
        assert!(t.contains("/Count 3"), "{t}");
        assert!(t.contains("(Chapter 1)"));
        assert!(t.contains("(Section 1.1)"));
        assert!(t.contains("/Dest [100 0 R /Fit]"));
        assert!(t.contains("/Dest [101 0 R /Fit]"));
        assert!(t.contains("/Dest [102 0 R /Fit]"));
        // Chapter 1 has a child count of 1.
        assert!(t.contains("/Count 1"), "{t}");
        assert!(root.num >= 1);
    }

    #[test]
    fn unicode_title_is_utf16_bom_hex() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let slots = slots_with(1);
        let items = vec![OutlineItem {
            title: "Café".into(),
            page_index: 0,
            children: vec![],
        }];
        write_outline(&mut sink, &slots, &items).unwrap();
        let t = String::from_utf8_lossy(&sink.finish().unwrap()).into_owned();
        // "Café" => FEFF 0043 0061 0066 00E9
        assert!(t.contains("<FEFF00430061006600E9>"), "{t}");
    }
}
