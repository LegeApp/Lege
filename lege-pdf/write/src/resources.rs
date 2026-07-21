//! Shared-resource registry: SharedResourceId -> ObjectId (JBIG2 globals,
//! embedded OCR font graph, ToUnicode CMap, palettes). Content-hash upstream,
//! never hash multi-MB buffers here.
//!
//! The identity of a shared resource is a `u64` computed *once upstream* (a
//! content hash or a monotonic counter). The writer never re-hashes payloads;
//! it maps the id to bytes registered with it, and writes each payload at most
//! once — the first time an artifact references it.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use crate::serialize::write_u64;
use crate::sink::{PdfSink, StreamBody};
use crate::types::{ObjectId, Result, WriteError};

/// A stable identity for a shared, immutable resource. Computed upstream (the
/// encoder or scheduler), never derived here.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SharedResourceId(pub u64);

struct Entry {
    bytes: Arc<[u8]>,
    written: Option<ObjectId>,
}

/// Maps shared-resource ids to their bytes and (lazily) their written object.
#[derive(Default)]
pub struct ResourceRegistry {
    entries: HashMap<u64, Entry>,
}

impl ResourceRegistry {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register a resource's bytes under its id. Re-registering the same id is
    /// a no-op (first registration wins); the id is assumed content-stable.
    pub fn register(&mut self, id: SharedResourceId, bytes: Arc<[u8]>) {
        self.entries.entry(id.0).or_insert(Entry {
            bytes,
            written: None,
        });
    }

    /// Ensure the resource is written to the sink, returning its object id.
    /// Writes a bare `<< /Length n >>` stream (no filter) — the shape JBIG2
    /// globals use — exactly once, and returns the same id on later calls.
    pub fn ensure_written<W: Write>(
        &mut self,
        sink: &mut PdfSink<W>,
        id: SharedResourceId,
    ) -> Result<ObjectId> {
        let entry = self.entries.get(&id.0).ok_or_else(|| {
            WriteError::InvalidArtifact(format!("shared resource {} not registered", id.0))
        })?;
        if let Some(obj) = entry.written {
            return Ok(obj);
        }

        let bytes = entry.bytes.clone();
        let obj = sink.alloc_id();
        let mut dict = Vec::with_capacity(24);
        dict.extend_from_slice(b"<</Length ");
        write_u64(&mut dict, bytes.len() as u64);
        dict.extend_from_slice(b">>");
        sink.write_stream(obj, &dict, &StreamBody::Shared(bytes))?;

        // Record so a second reference reuses the object.
        if let Some(e) = self.entries.get_mut(&id.0) {
            e.written = Some(obj);
        }
        Ok(obj)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_once_and_dedups() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let mut reg = ResourceRegistry::new();
        let id = SharedResourceId(42);
        reg.register(id, Arc::from(&b"GLOBALS"[..]));

        let a = reg.ensure_written(&mut sink, id).unwrap();
        let b = reg.ensure_written(&mut sink, id).unwrap();
        assert_eq!(a, b, "second reference must reuse the object");

        let bytes = sink.finish().unwrap();
        let text = String::from_utf8_lossy(&bytes);
        // Exactly one globals stream object emitted.
        assert_eq!(text.matches("stream\nGLOBALS\nendstream").count(), 1);
        assert!(text.contains("<</Length 7>>"));
    }

    #[test]
    fn unregistered_is_an_error() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let mut reg = ResourceRegistry::new();
        let err = reg
            .ensure_written(&mut sink, SharedResourceId(1))
            .unwrap_err();
        assert!(matches!(err, WriteError::InvalidArtifact(_)));
    }
}
