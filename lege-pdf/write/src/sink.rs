//! Append-only PdfSink: BufWriter over the output file, u64 offset table
//! indexed by object number, arrival-order ID allocation, StreamBody
//! (Shared Arc<[u8]> / Owned) written without copies.
//!
//! The sink is the byte-level heart of the emitter. It hands out object numbers
//! in arrival order, records each object's byte offset when it is written, and
//! frames indirect objects and stream objects. It never buffers whole objects
//! in memory beyond what the caller already holds — a shared image body is
//! written straight from its `Arc<[u8]>`.

use std::io::Write;
use std::sync::Arc;

use crate::serialize::{PdfValue, write_u64, write_value};
use crate::types::{ObjectId, Result};

/// The 4-byte binary marker recommended by PDF 32000-1 §7.5.2 so tools treat
/// the file as binary.
const BINARY_MARKER: &[u8] = b"%\xE2\xE3\xCF\xD3\n";

/// A stream body, written without copying where possible.
pub enum StreamBody {
    /// Shared, immutable payload straight from an encoder (`Arc<[u8]>`).
    Shared(Arc<[u8]>),
    /// Owned payload (e.g. a freshly compressed content stream).
    Owned(Vec<u8>),
    /// No bytes (a zero-length stream).
    Empty,
}

impl std::fmt::Debug for StreamBody {
    // Manual: a derived impl would dump the raw stream payload (up to
    // multi-MB image bytes); report the variant and length instead.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamBody::Shared(a) => f
                .debug_tuple("Shared")
                .field(&format_args!("{} byte(s)", a.len()))
                .finish(),
            StreamBody::Owned(v) => f
                .debug_tuple("Owned")
                .field(&format_args!("{} byte(s)", v.len()))
                .finish(),
            StreamBody::Empty => write!(f, "Empty"),
        }
    }
}

impl StreamBody {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            StreamBody::Shared(a) => a,
            StreamBody::Owned(v) => v,
            StreamBody::Empty => &[],
        }
    }

    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Append-only PDF byte sink with an object-offset table.
pub struct PdfSink<W: Write> {
    inner: W,
    pos: u64,
    /// `offsets[num]` = byte offset of object `num`. Index 0 is the free head
    /// and is never a real object.
    offsets: Vec<u64>,
}

impl<W: Write> std::fmt::Debug for PdfSink<W> {
    // Manual: `#[derive(Debug)]` would add a `where W: Debug` bound that
    // constrains every call site; `inner` is opaque anyway, so it's skipped.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdfSink")
            .field("pos", &self.pos)
            .field("offsets.len()", &self.offsets.len())
            .finish_non_exhaustive()
    }
}

impl<W: Write> PdfSink<W> {
    /// Create a sink and emit the file header for the given PDF version
    /// (e.g. `"1.7"`).
    pub fn new(inner: W, version: &str) -> Result<Self> {
        let mut s = Self {
            inner,
            pos: 0,
            offsets: vec![0], // index 0 reserved for the free object
        };
        s.write_bytes(b"%PDF-")?;
        s.write_bytes(version.as_bytes())?;
        s.write_bytes(b"\n")?;
        s.write_bytes(BINARY_MARKER)?;
        Ok(s)
    }

    /// Current byte position (used by xref for `startxref`).
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// The object-offset table, index 0 = free head. `len()` is the xref
    /// `Size`.
    pub fn offsets(&self) -> &[u64] {
        &self.offsets
    }

    /// Reserve the next object number in arrival order.
    pub fn alloc_id(&mut self) -> ObjectId {
        let num = self.offsets.len() as u32;
        self.offsets.push(0);
        ObjectId::new(num)
    }

    /// Write a non-stream indirect object whose body is already serialized.
    pub fn write_indirect(&mut self, id: ObjectId, body: &[u8]) -> Result<()> {
        self.begin_object(id)?;
        self.write_bytes(body)?;
        self.write_bytes(b"\nendobj\n")?;
        Ok(())
    }

    /// Write an indirect object from a generic `PdfValue` (cold path).
    pub fn write_value_object(&mut self, id: ObjectId, value: &PdfValue<'_>) -> Result<()> {
        let mut body = Vec::new();
        write_value(&mut body, value);
        self.write_indirect(id, &body)
    }

    /// Write a stream object. `dict` must be the complete `<< … >>` dictionary
    /// INCLUDING a correct `/Length` equal to `body.len()`. The body bytes are
    /// written verbatim between `stream`/`endstream` with no copy.
    pub fn write_stream(&mut self, id: ObjectId, dict: &[u8], body: &StreamBody) -> Result<()> {
        self.begin_object(id)?;
        self.write_bytes(dict)?;
        self.write_bytes(b"\nstream\n")?;
        self.write_bytes(body.as_bytes())?;
        self.write_bytes(b"\nendstream\nendobj\n")?;
        Ok(())
    }

    /// Append raw bytes to the trailer region (xref table, trailer dict,
    /// startxref). Used by `xref.rs`.
    pub fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.write_bytes(bytes)
    }

    /// Flush the underlying writer and return it.
    pub fn finish(mut self) -> Result<W> {
        self.inner.flush().map_err(crate::types::WriteError::from)?;
        Ok(self.inner)
    }

    // --- internals ------------------------------------------------------

    fn begin_object(&mut self, id: ObjectId) -> Result<()> {
        // Record the offset of this object number.
        let num = id.num as usize;
        debug_assert!(num < self.offsets.len(), "object id was not alloc'd");
        self.offsets[num] = self.pos;
        let mut header = Vec::with_capacity(16);
        write_u64(&mut header, id.num as u64);
        header.push(b' ');
        write_u64(&mut header, id.generation as u64);
        header.extend_from_slice(b" obj\n");
        self.write_bytes(&header)
    }

    fn write_bytes(&mut self, buf: &[u8]) -> Result<()> {
        self.inner
            .write_all(buf)
            .map_err(crate::types::WriteError::from)?;
        self.pos += buf.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_offsets() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        // header length: "%PDF-1.7\n" (9) + marker (6) = 15
        assert_eq!(sink.position(), 15);

        let id1 = sink.alloc_id();
        assert_eq!(id1.num, 1);
        let off1 = sink.position();
        sink.write_indirect(id1, b"<</Type /Catalog>>").unwrap();

        let id2 = sink.alloc_id();
        assert_eq!(id2.num, 2);
        let off2 = sink.position();
        let body = StreamBody::Owned(b"hello".to_vec());
        sink.write_stream(id2, b"<</Length 5>>", &body).unwrap();

        // offsets[0] is the free head; 1 and 2 are the two objects.
        assert_eq!(sink.offsets().len(), 3);
        assert_eq!(sink.offsets()[1], off1);
        assert_eq!(sink.offsets()[2], off2);

        let bytes = sink.finish().unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("%PDF-1.7\n"));
        assert!(text.contains("1 0 obj\n<</Type /Catalog>>\nendobj\n"));
        assert!(text.contains("2 0 obj\n<</Length 5>>\nstream\nhello\nendstream\nendobj\n"));
    }

    #[test]
    fn shared_body_is_written_verbatim() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let id = sink.alloc_id();
        let payload: Arc<[u8]> = Arc::from(&b"\x00\x01\xFF"[..]);
        let dict = b"<</Length 3>>".to_vec();
        sink.write_stream(id, &dict, &StreamBody::Shared(payload))
            .unwrap();
        let bytes = sink.finish().unwrap();
        // The three raw payload bytes appear between stream/endstream.
        let needle = b"stream\n\x00\x01\xFF\nendstream";
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle),
            "payload not found verbatim"
        );
    }
}
