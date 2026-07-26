//! Immutable, positional-read access to PDF bytes.
//!
//! Every worker reads the same source concurrently. There is deliberately no
//! `Read + Seek` anywhere in this crate: a shared seek cursor would serialize
//! workers (concurrency plan §"Immutable source bytes"). All reads are
//! offset-addressed and `&self`.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

/// Errors produced by byte-source access.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SourceError {
    #[error("read of {len} bytes at offset {offset} exceeds source length {source_len}")]
    OutOfBounds {
        offset: u64,
        len: u64,
        source_len: u64,
    },
    #[error("I/O error reading source: {0}")]
    Io(Arc<std::io::Error>),
}

impl From<std::io::Error> for SourceError {
    fn from(e: std::io::Error) -> Self {
        SourceError::Io(Arc::new(e))
    }
}

/// A random-access, thread-safe byte source.
///
/// Implementations must be safe for unlimited concurrent calls: no interior
/// cursor, no interior mutability observable through this trait.
pub trait PdfSource: Send + Sync + std::fmt::Debug {
    /// Total length in bytes.
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fill `dst` from `offset`. Fails (without partial reads being
    /// observable) if the range is out of bounds.
    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), SourceError>;

    /// Zero-copy view of the entire source, if it is contiguous in memory
    /// (mmap, owned bytes). `None` for genuinely streaming sources.
    fn as_contiguous(&self) -> Option<&[u8]> {
        None
    }

    /// Read a range, borrowing when the backing store is contiguous.
    fn read_range(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, SourceError> {
        let len = range
            .end
            .checked_sub(range.start)
            .ok_or(SourceError::OutOfBounds {
                offset: range.start,
                len: 0,
                source_len: self.len(),
            })?;
        if let Some(all) = self.as_contiguous() {
            let start = usize::try_from(range.start).map_err(|_| SourceError::OutOfBounds {
                offset: range.start,
                len,
                source_len: self.len(),
            })?;
            let end = usize::try_from(range.end).map_err(|_| SourceError::OutOfBounds {
                offset: range.start,
                len,
                source_len: self.len(),
            })?;
            let slice = all.get(start..end).ok_or(SourceError::OutOfBounds {
                offset: range.start,
                len,
                source_len: self.len(),
            })?;
            Ok(Cow::Borrowed(slice))
        } else {
            let mut buf = vec![
                0u8;
                usize::try_from(len).map_err(|_| SourceError::OutOfBounds {
                    offset: range.start,
                    len,
                    source_len: self.len(),
                })?
            ];
            self.read_exact_at(range.start, &mut buf)?;
            Ok(Cow::Owned(buf))
        }
    }
}

/// In-memory source backed by shared bytes. The simplest implementation and
/// the one unit tests use.
#[derive(Debug, Clone)]
pub struct OwnedBytesSource {
    bytes: Arc<[u8]>,
}

impl OwnedBytesSource {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }
}

impl PdfSource for OwnedBytesSource {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), SourceError> {
        let start = usize::try_from(offset).ok();
        let end = start.and_then(|s| s.checked_add(dst.len()));
        match (start, end) {
            (Some(s), Some(e)) if e <= self.bytes.len() => {
                dst.copy_from_slice(&self.bytes[s..e]);
                Ok(())
            }
            _ => Err(SourceError::OutOfBounds {
                offset,
                len: dst.len() as u64,
                source_len: self.len(),
            }),
        }
    }

    fn as_contiguous(&self) -> Option<&[u8]> {
        Some(&self.bytes)
    }
}

/// Memory-mapped file source.
///
/// The preferred source for local files: all workers share one mapping of
/// the file with zero copies, and `as_contiguous` lets parsers slice
/// directly into the map.
#[derive(Debug)]
pub struct MmapSource {
    map: memmap2::Mmap,
}

impl MmapSource {
    /// Map `path` read-only.
    #[expect(
        unsafe_code,
        reason = "memmap2::Mmap::map is unavoidably unsafe; see SAFETY comment"
    )]
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, SourceError> {
        let file = std::fs::File::open(path)?;
        // SAFETY: mapping a file is unsafe because another process could
        // truncate or modify the file while mapped, invalidating the slice
        // we hand out (on most platforms this raises SIGBUS / an access
        // violation rather than yielding stale memory). We accept the same
        // risk profile every mmap-based PDF reader accepts: the engine is
        // read-only, opens files it does not modify, and documents that
        // callers must not truncate a file while a snapshot is open. No
        // aliasing &mut ever exists — the map is read-only for its lifetime.
        let map = unsafe { memmap2::Mmap::map(&file) }?;
        Ok(Self { map })
    }
}

impl PdfSource for MmapSource {
    fn len(&self) -> u64 {
        self.map.len() as u64
    }

    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), SourceError> {
        let start = usize::try_from(offset).ok();
        let end = start.and_then(|s| s.checked_add(dst.len()));
        match (start, end) {
            (Some(s), Some(e)) if e <= self.map.len() => {
                dst.copy_from_slice(&self.map[s..e]);
                Ok(())
            }
            _ => Err(SourceError::OutOfBounds {
                offset,
                len: dst.len() as u64,
                source_len: self.len(),
            }),
        }
    }

    fn as_contiguous(&self) -> Option<&[u8]> {
        Some(&self.map)
    }
}

/// Positional-read file source for platforms/situations where mapping is
/// undesirable (network filesystems, files larger than address space on
/// 32-bit hosts). Reads go straight to the OS with an explicit offset —
/// there is no shared seek cursor to serialize workers.
///
/// Note (Windows): `seek_read` also moves the file's cursor as a side
/// effect, but every read in this type passes an absolute offset, so
/// concurrent readers never observe each other's cursor movement.
#[derive(Debug)]
pub struct FileReadAtSource {
    file: std::fs::File,
    /// Length captured at open; the snapshot contract assumes the file is
    /// not mutated while open.
    len: u64,
}

impl FileReadAtSource {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, SourceError> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }

    /// Positional read of as many bytes as the OS returns in one call.
    fn read_at_once(&self, offset: u64, dst: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(dst, offset)
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_at(dst, offset)
        }
        #[cfg(not(any(windows, unix)))]
        {
            compile_error!("FileReadAtSource requires a positional-read primitive")
        }
    }
}

impl PdfSource for FileReadAtSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), SourceError> {
        // Bounds-check against the captured length first so short files fail
        // uniformly with OutOfBounds rather than a platform-specific EOF.
        let end = offset.checked_add(dst.len() as u64);
        if end.is_none() || end.unwrap_or(u64::MAX) > self.len {
            return Err(SourceError::OutOfBounds {
                offset,
                len: dst.len() as u64,
                source_len: self.len,
            });
        }
        let mut filled = 0usize;
        while filled < dst.len() {
            match self.read_at_once(offset + filled as u64, &mut dst[filled..]) {
                Ok(0) => {
                    // EOF before the promised end: file shrank underneath us.
                    return Err(SourceError::OutOfBounds {
                        offset,
                        len: dst.len() as u64,
                        source_len: self.len,
                    });
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const fn assert_send_sync<T: Send + Sync>() {}
    const _: () = assert_send_sync::<OwnedBytesSource>();

    const _: () = assert_send_sync::<MmapSource>();
    const _: () = assert_send_sync::<FileReadAtSource>();

    fn temp_file_with(bytes: &[u8]) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "pdf-source-test-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn mmap_source_reads_and_slices() {
        let path = temp_file_with(b"%PDF-1.7 mmap works");
        let src = MmapSource::open(&path).unwrap();
        assert_eq!(src.len(), 19);
        assert_eq!(src.as_contiguous().unwrap(), b"%PDF-1.7 mmap works");
        let mut buf = [0u8; 4];
        src.read_exact_at(9, &mut buf).unwrap();
        assert_eq!(&buf, b"mmap");
        assert!(matches!(
            src.read_exact_at(18, &mut buf),
            Err(SourceError::OutOfBounds { .. })
        ));
        // read_range borrows straight from the map.
        assert!(matches!(src.read_range(0..4).unwrap(), Cow::Borrowed(_)));
        drop(src);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_read_at_source_positional_reads() {
        let path = temp_file_with(b"positional read test bytes");
        let src = FileReadAtSource::open(&path).unwrap();
        assert_eq!(src.len(), 26);
        assert!(src.as_contiguous().is_none());
        let mut buf = [0u8; 4];
        src.read_exact_at(11, &mut buf).unwrap();
        assert_eq!(&buf, b"read");
        // Out-of-bounds fails without partial writes being relevant.
        assert!(matches!(
            src.read_exact_at(23, &mut buf),
            Err(SourceError::OutOfBounds { .. })
        ));
        // read_range falls back to an owned copy.
        assert!(matches!(src.read_range(0..10).unwrap(), Cow::Owned(_)));
        assert_eq!(src.read_range(0..10).unwrap().as_ref(), b"positional");
        drop(src);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_read_at_source_concurrent_reads_do_not_interfere() {
        // The whole point of positional reads: concurrent readers at
        // different offsets always see their own bytes (no shared cursor).
        let bytes: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
        let path = temp_file_with(&bytes);
        let src = std::sync::Arc::new(FileReadAtSource::open(&path).unwrap());
        std::thread::scope(|s| {
            for t in 0..8u64 {
                let src = Arc::clone(&src);
                let bytes = &bytes;
                s.spawn(move || {
                    for i in 0..200u64 {
                        let off = (t * 8011 + i * 259) % (64 * 1024 - 16);
                        let mut buf = [0u8; 16];
                        src.read_exact_at(off, &mut buf).unwrap();
                        assert_eq!(&buf, &bytes[off as usize..off as usize + 16]);
                    }
                });
            }
        });
        drop(src);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn owned_bytes_roundtrip() {
        let src = OwnedBytesSource::new(&b"%PDF-1.7 hello"[..]);
        let mut buf = [0u8; 5];
        src.read_exact_at(9, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        assert!(matches!(
            src.read_exact_at(10, &mut buf),
            Err(SourceError::OutOfBounds { .. })
        ));
        assert_eq!(src.read_range(0..8).unwrap().as_ref(), b"%PDF-1.7");
    }
}
