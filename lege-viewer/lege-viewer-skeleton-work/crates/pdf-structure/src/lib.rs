//! Document structure: cross-reference data, trailers, revisions, object
//! streams, and structural recovery.
//!
//! Everything produced here is **built once during document open and
//! immutable afterwards** (concurrency plan §"Eager immutable structural
//! index"). Workers never touch a mutable xref.

use std::sync::Arc;

use pdf_object::{Dictionary, ObjectId};

pub mod decode;
mod loader;
pub mod objstm;
pub mod predictor;

pub use loader::{StructureLimits, load_structure, scan_for_object_header};

/// Where an object's bytes can be found, after all revisions are merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectLocation {
    /// Uncompressed object at a byte offset in the source.
    Offset(u64),
    /// Compressed member of an object stream.
    InObjectStream {
        /// Object number of the containing `/Type /ObjStm` stream.
        container: u32,
        /// Zero-based index within the container.
        index: u32,
    },
    /// Free entry (or object shadowed by a free entry in a later revision).
    Free,
}

/// One resolved cross-reference entry.
#[derive(Debug, Clone, Copy)]
pub struct XrefEntry {
    pub id: ObjectId,
    pub location: ObjectLocation,
}

/// The merged cross-reference map for the *current* view of the document:
/// all incremental updates applied, later revisions shadowing earlier ones.
/// Indexed by object number for O(1) lookup; dense because object numbers
/// are, in practice, dense.
#[derive(Debug)]
pub struct XrefMap {
    /// `entries[n]` is the location for object number `n`.
    entries: Box<[ObjectLocation]>,
    /// Generation currently live for each object number (0 for entries in
    /// object streams, per spec).
    generations: Box<[u16]>,
}

impl XrefMap {
    pub fn from_parts(entries: Box<[ObjectLocation]>, generations: Box<[u16]>) -> Self {
        assert_eq!(entries.len(), generations.len());
        Self { entries, generations }
    }

    pub fn locate(&self, id: ObjectId) -> ObjectLocation {
        let n = id.number as usize;
        if n >= self.entries.len() || self.generations[n] != id.generation {
            return ObjectLocation::Free;
        }
        self.entries[n]
    }

    /// Highest object number + 1 (the trailer's `/Size`, possibly repaired).
    pub fn size(&self) -> u32 {
        self.entries.len() as u32
    }
}

/// A single revision (original document or one incremental update),
/// retained for diagnostics and future signature verification.
#[derive(Debug, Clone)]
pub struct Revision {
    /// Offset of this revision's xref section (table or stream).
    pub xref_offset: u64,
    /// The trailer dictionary of this revision.
    pub trailer: Arc<Dictionary>,
}

/// The resolved trailer view of the current document.
#[derive(Debug, Clone)]
pub struct Trailer {
    pub dict: Arc<Dictionary>,
    pub root: ObjectId,
    pub info: Option<ObjectId>,
    pub encrypt: Option<ObjectId>,
    /// The two-element /ID byte strings, if present (needed for encryption
    /// key derivation and cache keying).
    pub file_id: Option<[pdf_object::PdfString; 2]>,
}

/// Non-fatal deviations tolerated (and repaired) during structure loading.
/// Recorded on the snapshot so corpus/differential tooling can observe what
/// recovery ran — repairs must be visible, not silent (blueprint §4.5).
#[derive(Debug, Clone)]
pub enum RecoveryEvent {
    /// `startxref` missing or pointing outside the file; recovered by scan.
    StartXrefRecovered { reported: Option<u64>, used: u64 },
    /// An xref entry's offset didn't land on the declared object; object was
    /// found elsewhere by scanning.
    ObjectOffsetRepaired { id: ObjectId, declared: u64, actual: u64 },
    /// A stream /Length was wrong; actual length determined by scanning for
    /// `endstream`.
    StreamLengthRepaired { id: ObjectId, declared: Option<i64>, actual: u64 },
    /// Full-file reconstruction: xref unusable, objects indexed by scanning
    /// for `N G obj` headers (PDFium's `RebuildCrossRef` equivalent).
    XrefRebuilt,
    /// Trailer /Size disagreed with actual entries.
    SizeRepaired { declared: i64, actual: u32 },
    /// Other tolerated deviation, described for logs.
    Other(String),
}

/// Fatal structural errors: the document could not be brought to a usable
/// state even with recovery.
#[derive(Debug, Clone, thiserror::Error)]
pub enum StructureError {
    #[error("not a PDF: header not found")]
    NoHeader,
    #[error("no usable cross-reference information (recovery failed)")]
    NoUsableXref,
    #[error("trailer has no /Root catalog reference")]
    NoCatalog,
    #[error("cross-reference data forms a cycle at offset {0}")]
    XrefCycle(u64),
    #[error("structural limit exceeded: {0}")]
    LimitExceeded(&'static str),
    #[error(transparent)]
    Syntax(#[from] pdf_syntax::SyntaxError),
    #[error(transparent)]
    Source(#[from] pdf_source::SourceError),
    #[error(transparent)]
    Decode(#[from] decode::DecodeError),
}

/// Output of the document-open structural phase, consumed by
/// `pdf-document` to build the snapshot.
#[derive(Debug)]
pub struct DocumentStructure {
    pub version: PdfVersion,
    /// Bytes of leading garbage before `%PDF`. Every structural offset
    /// (xref entries, `Revision::xref_offset`) is relative to the header,
    /// i.e. the *source* position is `header_offset + structural offset`
    /// (PDFium's `header_offset_` convention).
    pub header_offset: u64,
    pub xref: XrefMap,
    pub trailer: Trailer,
    pub revisions: Box<[Revision]>,
    pub recovery: Box<[RecoveryEvent]>,
}

/// PDF version from the header (possibly overridden by the catalog's
/// /Version).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PdfVersion {
    pub major: u8,
    pub minor: u8,
}

// `load_structure` (see `loader`) covers: header scan (incl. leading-garbage
// tolerance), startxref, classic xref tables, xref streams (/W decoding),
// hybrid files (/XRefStm), /Prev chains with cycle detection, and the
// rebuild scan. Object-stream layouts are in `objstm`; filter decoding in
// `decode`; predictors in `predictor`.

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn xref_map_shadows_by_generation() {
        let map = XrefMap::from_parts(
            vec![ObjectLocation::Free, ObjectLocation::Offset(15)].into_boxed_slice(),
            vec![0, 0].into_boxed_slice(),
        );
        assert_eq!(map.locate(ObjectId::new(1, 0)), ObjectLocation::Offset(15));
        // Stale generation → treated as free.
        assert_eq!(map.locate(ObjectId::new(1, 1)), ObjectLocation::Free);
        // Out of range → free.
        assert_eq!(map.locate(ObjectId::new(9, 0)), ObjectLocation::Free);
    }
}
