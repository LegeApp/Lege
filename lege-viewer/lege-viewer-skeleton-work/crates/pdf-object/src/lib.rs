//! The canonical PDF object model.
//!
//! Objects are **immutable after publication** and cheaply shareable across
//! workers (`Arc` containers). Derived/rendering state never lives here —
//! that separation is the core architectural rule of the engine.

use std::sync::Arc;

mod interner;
pub mod parser;
pub use interner::{NameId, NameInterner};

/// Identity of an indirect object: `number generation R`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId {
    pub number: u32,
    pub generation: u16,
}

impl ObjectId {
    pub const fn new(number: u32, generation: u16) -> Self {
        Self { number, generation }
    }
}

impl std::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} R", self.number, self.generation)
    }
}

/// A PDF byte string. PDF strings are byte strings, not UTF-8; text-string
/// interpretation (PDFDocEncoding / UTF-16BE) is a caller-level concern.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PdfString(pub Arc<[u8]>);

impl PdfString {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self(bytes.into())
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A parsed PDF value. Geometry-bearing numbers stay `f64` through all
/// semantic layers (blueprint §4.2); narrowing happens only in backend
/// lowering.
#[derive(Debug, Clone)]
pub enum PdfObject {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Name(NameId),
    String(PdfString),
    Array(Arc<[PdfObject]>),
    Dictionary(Arc<Dictionary>),
    Stream(Arc<PdfStream>),
    /// An unresolved indirect reference. Resolution goes through the
    /// document's `ObjectRepository`, never through the object graph itself.
    Reference(ObjectId),
}

impl PdfObject {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            PdfObject::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Numeric value with PDF's implicit int→real coercion.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            PdfObject::Integer(i) => Some(*i as f64),
            PdfObject::Real(r) => Some(*r),
            _ => None,
        }
    }

    pub fn as_name(&self) -> Option<NameId> {
        match self {
            PdfObject::Name(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&Dictionary> {
        match self {
            PdfObject::Dictionary(d) => Some(d),
            PdfObject::Stream(s) => Some(&s.dict),
            _ => None,
        }
    }

    pub fn as_reference(&self) -> Option<ObjectId> {
        match self {
            PdfObject::Reference(id) => Some(*id),
            _ => None,
        }
    }
}

/// A PDF dictionary.
///
/// Keys are interned [`NameId`]s, stored as a sorted vec: real-world PDF
/// dictionaries are small (typically < 16 entries), so binary search over a
/// contiguous array beats a hash map on both speed and memory. Duplicate
/// keys (malformed but seen in the wild) resolve to the **last** occurrence,
/// matching PDFium; construction handles this.
#[derive(Debug, Clone, Default)]
pub struct Dictionary {
    entries: Vec<(NameId, PdfObject)>,
}

impl Dictionary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from possibly-unsorted, possibly-duplicated pairs.
    /// Later duplicates win (stable with respect to input order).
    pub fn from_pairs(pairs: impl IntoIterator<Item = (NameId, PdfObject)>) -> Self {
        let mut entries: Vec<(NameId, PdfObject)> = Vec::new();
        for (k, v) in pairs {
            match entries.binary_search_by_key(&k, |(ek, _)| *ek) {
                Ok(i) => entries[i].1 = v,
                Err(i) => entries.insert(i, (k, v)),
            }
        }
        Self { entries }
    }

    pub fn get(&self, key: NameId) -> Option<&PdfObject> {
        self.entries
            .binary_search_by_key(&key, |(k, _)| *k)
            .ok()
            .map(|i| &self.entries[i].1)
    }

    pub fn contains_key(&self, key: NameId) -> bool {
        self.get(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (NameId, &PdfObject)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }
}

/// Where a stream's raw (still-encoded) bytes live. Streams do not eagerly
/// copy body bytes out of the source; decoding is on-demand and cached at a
/// layer above (blueprint: derived data never lives on canonical objects).
#[derive(Debug, Clone)]
pub enum StreamData {
    /// Byte range in the document source (offset, length after /Length
    /// validation or repair).
    InSource { offset: u64, len: u64 },
    /// Materialized bytes (e.g. objects reconstructed during recovery, or
    /// synthetic streams in tests).
    Owned(Arc<[u8]>),
}

/// A PDF stream: its dictionary plus a handle to its raw bytes.
#[derive(Debug, Clone)]
pub struct PdfStream {
    pub dict: Dictionary,
    pub data: StreamData,
}

/// Errors attributable to the object layer.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ObjectError {
    #[error("expected {expected}, found {found}")]
    TypeMismatch { expected: &'static str, found: &'static str },
    #[error("missing required key /{key}")]
    MissingKey { key: String },
    #[error("reference cycle detected at {0}")]
    ReferenceCycle(ObjectId),
    #[error("object {0} not found")]
    NotFound(ObjectId),
}

/// Well-known name table: names every document uses, pre-interned at
/// interner construction so lookups like `names.type_` are free and
/// hot-path code never allocates to compare a dictionary key.
#[derive(Debug)]
pub struct WellKnownNames {
    pub type_: NameId,
    pub subtype: NameId,
    pub length: NameId,
    pub filter: NameId,
    pub decode_parms: NameId,
    pub root: NameId,
    pub pages: NameId,
    pub page: NameId,
    pub kids: NameId,
    pub count: NameId,
    pub parent: NameId,
    pub contents: NameId,
    pub resources: NameId,
    pub media_box: NameId,
    pub crop_box: NameId,
    pub rotate: NameId,
    pub encrypt: NameId,
    pub info: NameId,
    pub id: NameId,
    pub size: NameId,
    pub prev: NameId,
    pub xref_stm: NameId,
    pub w: NameId,
    pub index: NameId,
    pub first: NameId,
    pub n: NameId,
    pub obj_stm: NameId,
    pub xref: NameId,
    pub font: NameId,
    pub x_object: NameId,
    pub ext_g_state: NameId,
    pub color_space: NameId,
    pub pattern: NameId,
    pub shading: NameId,
}

impl WellKnownNames {
    fn intern_all(interner: &NameInterner) -> Self {
        let i = |s: &[u8]| interner.intern(s);
        Self {
            type_: i(b"Type"),
            subtype: i(b"Subtype"),
            length: i(b"Length"),
            filter: i(b"Filter"),
            decode_parms: i(b"DecodeParms"),
            root: i(b"Root"),
            pages: i(b"Pages"),
            page: i(b"Page"),
            kids: i(b"Kids"),
            count: i(b"Count"),
            parent: i(b"Parent"),
            contents: i(b"Contents"),
            resources: i(b"Resources"),
            media_box: i(b"MediaBox"),
            crop_box: i(b"CropBox"),
            rotate: i(b"Rotate"),
            encrypt: i(b"Encrypt"),
            info: i(b"Info"),
            id: i(b"ID"),
            size: i(b"Size"),
            prev: i(b"Prev"),
            xref_stm: i(b"XRefStm"),
            w: i(b"W"),
            index: i(b"Index"),
            first: i(b"First"),
            n: i(b"N"),
            obj_stm: i(b"ObjStm"),
            xref: i(b"XRef"),
            font: i(b"Font"),
            x_object: i(b"XObject"),
            ext_g_state: i(b"ExtGState"),
            color_space: i(b"ColorSpace"),
            pattern: i(b"Pattern"),
            shading: i(b"Shading"),
        }
    }
}

/// A per-document name table: the interner plus pre-resolved well-known ids.
/// One of these lives on the `DocumentSnapshot`; content-stream operands and
/// dictionary keys across the whole document share it.
#[derive(Debug)]
pub struct NameTable {
    interner: NameInterner,
    pub known: WellKnownNames,
}

impl NameTable {
    pub fn new() -> Self {
        let interner = NameInterner::new();
        let known = WellKnownNames::intern_all(&interner);
        Self { interner, known }
    }

    pub fn intern(&self, name: &[u8]) -> NameId {
        self.interner.intern(name)
    }

    pub fn lookup(&self, name: &[u8]) -> Option<NameId> {
        self.interner.lookup(name)
    }

    pub fn resolve(&self, id: NameId) -> Arc<[u8]> {
        self.interner.resolve(id)
    }
}

impl Default for NameTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const fn assert_send_sync<T: Send + Sync>() {}
    const _: () = assert_send_sync::<PdfObject>();
    const _: () = assert_send_sync::<NameTable>();

    #[test]
    fn dictionary_last_duplicate_wins() {
        let names = NameTable::new();
        let k = names.intern(b"Length");
        let d = Dictionary::from_pairs([
            (k, PdfObject::Integer(1)),
            (k, PdfObject::Integer(2)),
        ]);
        assert_eq!(d.len(), 1);
        assert_eq!(d.get(k).and_then(PdfObject::as_int), Some(2));
    }

    #[test]
    fn well_known_names_are_stable() {
        let names = NameTable::new();
        assert_eq!(names.lookup(b"Type"), Some(names.known.type_));
        assert_eq!(names.resolve(names.known.media_box).as_ref(), b"MediaBox");
    }
}
