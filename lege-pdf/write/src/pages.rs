//! Page dictionary emission, WrittenPageSlots (page ObjectId by logical index),
//! balanced page tree at finalization, catalog.
//!
//! For M1 the page tree is flat: a single `/Pages` root whose id is reserved up
//! front so pages written on arrival can carry a valid `/Parent`. A balanced
//! fan-out for very large documents is a later refinement (the reserved-root
//! approach generalizes to a precomputed skeleton).

use std::io::Write;

use crate::serialize::{write_i64, write_name, write_real, write_ref, write_u64};
use crate::sink::PdfSink;
use crate::types::{ObjectId, PdfRect, Result, WriteError};

/// Records the object id of each logical page as it is written. Replaces the
/// old `Arc<Mutex<BTreeMap>>` reorder buffer.
pub struct WrittenPageSlots {
    slots: Vec<Option<ObjectId>>,
}

impl WrittenPageSlots {
    pub fn new(total_pages: usize) -> Self {
        Self {
            slots: vec![None; total_pages],
        }
    }

    pub fn total(&self) -> usize {
        self.slots.len()
    }

    /// Record a page's object id. Errors on a duplicate index (the old map
    /// silently overwrote) or an out-of-range index.
    pub fn set(&mut self, index: u32, id: ObjectId) -> Result<()> {
        let i = index as usize;
        let total = self.slots.len();
        let slot = self
            .slots
            .get_mut(i)
            .ok_or(WriteError::PageIndexOutOfRange {
                index,
                total: total as u32,
            })?;
        if slot.is_some() {
            return Err(WriteError::DuplicatePage(index));
        }
        *slot = Some(id);
        Ok(())
    }

    /// Logical indices that have not yet been written.
    pub fn missing(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| if s.is_none() { Some(i) } else { None })
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.slots.iter().all(Option::is_some)
    }

    /// The object id of a logical page, if it has been written.
    pub fn page_id(&self, index: u32) -> Option<ObjectId> {
        self.slots.get(index as usize).copied().flatten()
    }

    fn ordered_ids(&self) -> Option<Vec<ObjectId>> {
        self.slots.iter().copied().collect()
    }
}

/// Write one page dictionary. `xobjects` maps the bare resource name (e.g.
/// `Im1`) to its object id; `fonts` does the same for `/Font` (empty in M1).
pub fn write_page_dict<W: Write>(
    sink: &mut PdfSink<W>,
    page_id: ObjectId,
    parent: ObjectId,
    media_box: PdfRect,
    xobjects: &[(crate::types::ResourceName, ObjectId)],
    fonts: &[(crate::types::ResourceName, ObjectId)],
    contents: ObjectId,
) -> Result<()> {
    let mut d = Vec::new();
    d.extend_from_slice(b"<<");
    kname(&mut d, b"Type");
    write_name(&mut d, b"Page");
    kname(&mut d, b"Parent");
    write_ref(&mut d, parent);

    kname(&mut d, b"MediaBox");
    d.push(b'[');
    for (i, v) in [media_box.x0, media_box.y0, media_box.x1, media_box.y1]
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            d.push(b' ');
        }
        write_real(&mut d, v);
    }
    d.push(b']');

    kname(&mut d, b"Resources");
    d.extend_from_slice(b"<<");
    kname(&mut d, b"XObject");
    write_resource_subdict(&mut d, xobjects);
    if !fonts.is_empty() {
        kname(&mut d, b"Font");
        write_resource_subdict(&mut d, fonts);
    }
    d.extend_from_slice(b">>");

    kname(&mut d, b"Contents");
    write_ref(&mut d, contents);
    d.extend_from_slice(b">>");

    sink.write_indirect(page_id, &d)
}

/// Write the flat `/Pages` root (its id was reserved at construction) with the
/// ordered page ids as `/Kids`. Errors if any page is missing.
pub fn write_pages_root<W: Write>(
    sink: &mut PdfSink<W>,
    pages_root: ObjectId,
    slots: &WrittenPageSlots,
) -> Result<()> {
    let ids = slots.ordered_ids().ok_or_else(|| {
        let missing = slots.missing();
        WriteError::IncompletePages {
            missing: missing.len(),
            total: slots.total(),
        }
    })?;

    let mut d = Vec::new();
    d.extend_from_slice(b"<<");
    kname(&mut d, b"Type");
    write_name(&mut d, b"Pages");
    kname(&mut d, b"Kids");
    d.push(b'[');
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            d.push(b' ');
        }
        write_ref(&mut d, *id);
    }
    d.push(b']');
    kname(&mut d, b"Count");
    write_i64(&mut d, ids.len() as i64);
    d.extend_from_slice(b">>");
    sink.write_indirect(pages_root, &d)
}

/// Optional catalog entries beyond `/Pages`.
#[derive(Default, Clone, Copy)]
pub struct CatalogExtras {
    pub outlines: Option<ObjectId>,
    /// PDF/A OutputIntent object; adds `/OutputIntents [id]`.
    pub output_intent: Option<ObjectId>,
    /// Adds `/MarkInfo << /Marked true >>`.
    pub mark_info: bool,
}

/// Write the document catalog and return its id.
pub fn write_catalog<W: Write>(
    sink: &mut PdfSink<W>,
    pages_root: ObjectId,
    extras: CatalogExtras,
) -> Result<ObjectId> {
    let id = sink.alloc_id();
    let mut d = Vec::new();
    d.extend_from_slice(b"<<");
    kname(&mut d, b"Type");
    write_name(&mut d, b"Catalog");
    kname(&mut d, b"Pages");
    write_ref(&mut d, pages_root);
    if let Some(outline_id) = extras.outlines {
        kname(&mut d, b"Outlines");
        write_ref(&mut d, outline_id);
    }
    if let Some(oi) = extras.output_intent {
        kname(&mut d, b"OutputIntents");
        d.push(b'[');
        write_ref(&mut d, oi);
        d.push(b']');
    }
    if extras.mark_info {
        kname(&mut d, b"MarkInfo");
        d.extend_from_slice(b"<</Marked true>>");
    }
    d.extend_from_slice(b">>");
    sink.write_indirect(id, &d)?;
    Ok(id)
}

fn write_resource_subdict(d: &mut Vec<u8>, entries: &[(crate::types::ResourceName, ObjectId)]) {
    d.extend_from_slice(b"<<");
    for (name, id) in entries {
        name.write_name(d);
        d.push(b' ');
        write_ref(d, *id);
    }
    d.extend_from_slice(b">>");
}

fn kname(d: &mut Vec<u8>, k: &[u8]) {
    write_name(d, k);
    d.push(b' ');
}

// Kept public-in-crate for the writer's empty-content-stream length line.
pub(crate) fn write_length_dict(d: &mut Vec<u8>, len: usize) {
    d.extend_from_slice(b"<</Length ");
    write_u64(d, len as u64);
    d.extend_from_slice(b">>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ResourceName;

    #[test]
    fn slots_reject_dup_and_out_of_range() {
        let mut s = WrittenPageSlots::new(3);
        s.set(0, ObjectId::new(10)).unwrap();
        assert!(matches!(
            s.set(0, ObjectId::new(11)),
            Err(WriteError::DuplicatePage(0))
        ));
        assert!(matches!(
            s.set(5, ObjectId::new(12)),
            Err(WriteError::PageIndexOutOfRange { index: 5, total: 3 })
        ));
        assert_eq!(s.missing(), vec![1, 2]);
        assert!(!s.is_complete());
    }

    #[test]
    fn pages_root_requires_all_pages() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let root = sink.alloc_id();
        let mut slots = WrittenPageSlots::new(2);
        slots.set(0, ObjectId::new(5)).unwrap();
        let err = write_pages_root(&mut sink, root, &slots).unwrap_err();
        assert!(matches!(
            err,
            WriteError::IncompletePages {
                missing: 1,
                total: 2
            }
        ));
    }

    #[test]
    fn page_dict_shape() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let page = sink.alloc_id();
        let parent = ObjectId::new(1);
        write_page_dict(
            &mut sink,
            page,
            parent,
            PdfRect::from_size(612.0, 792.0),
            &[(ResourceName::Image(1), ObjectId::new(3))],
            &[],
            ObjectId::new(4),
        )
        .unwrap();
        let t = String::from_utf8_lossy(&sink.finish().unwrap()).into_owned();
        assert!(t.contains("/Type /Page"));
        assert!(t.contains("/Parent 1 0 R"));
        assert!(t.contains("/MediaBox [0 0 612 792]"));
        assert!(t.contains("/XObject <</Im1 3 0 R>>"), "{t}");
        assert!(t.contains("/Contents 4 0 R"));
    }
}
