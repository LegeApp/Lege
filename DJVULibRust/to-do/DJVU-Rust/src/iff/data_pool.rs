// src/iff/data_pool.rs
//! A read-only, seekable pool of byte data for DjVu encoding.
//!
//! This module provides a simplified, synchronous, and type-safe Rust equivalent
//! to the C++ `DataPool` class, optimized for encoding DjVu documents. It supports
//! in-memory buffers, file-based data, and sliced views, using `Arc` for shared
//! ownership and `bytemuck` for zero-copy conversions of DjVu data structures.

use crate::utils::error::{DjvuError, Result};
use bytemuck::{Pod, Zeroable};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A trait representing a source of byte data that can be read and sought.
///
/// This abstraction allows `DataPool` to work with different underlying data
/// storage mechanisms (e.g., memory, file) while providing a unified interface.
pub trait DataSource: Read + Seek + Send + Sync + 'static {
    /// Returns the total size of the data source in bytes.
    fn len(&self) -> u64;

    /// Checks if the data source is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a reference to the underlying bytes if available (e.g., for in-memory sources).
    fn as_bytes(&self) -> Option<&[u8]> {
        None
    }
}

// Implement DataSource for a read-only cursor over a shared byte buffer.
#[derive(Clone)]
pub struct ArcCursor {
    data: Arc<Vec<u8>>,
    pos: u64,
    start: u64,
    end: u64,
}

impl ArcCursor {
    pub fn new(data: Arc<Vec<u8>>, start: u64, end: u64) -> Self {
        Self {
            data,
            pos: start,
            start,
            end,
        }
    }
}

impl Read for ArcCursor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.end {
            return Ok(0); // EOF
        }
        let available_data = &self.data[self.pos as usize..self.end as usize];
        let bytes_to_read = buf.len().min(available_data.len());

        buf[..bytes_to_read].copy_from_slice(&available_data[..bytes_to_read]);

        self.pos += bytes_to_read as u64;

        Ok(bytes_to_read)
    }
}

impl Seek for ArcCursor {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let current_pos_in_stream = self.pos - self.start;
        let stream_len = self.end - self.start;

        let new_pos_in_stream = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(p) => stream_len as i64 + p,
            SeekFrom::Current(p) => current_pos_in_stream as i64 + p,
        };

        if new_pos_in_stream < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Seek to a negative position is not allowed.",
            ));
        }

        self.pos = (self.start + new_pos_in_stream as u64).min(self.end);

        Ok(self.pos - self.start)
    }
}

impl DataSource for ArcCursor {
    fn len(&self) -> u64 {
        self.end - self.start
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        Some(&self.data[self.start as usize..self.end as usize])
    }
}

// Implement DataSource for a file.
impl DataSource for File {
    fn len(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0)
    }
}

/// A read-only pool of data providing a unified `Read`, `Seek`, and `ByteStream` interface.
///
/// `DataPool` supports in-memory buffers, file-based data, or slices of another
/// `DataPool`. It is cheap to clone via `Arc` and optimized for DjVu encoding with
/// `bytemuck` for zero-copy conversions.
#[derive(Clone)]
pub struct DataPool {
    source: Arc<Mutex<dyn DataSource>>,
    start: u64,
    end: u64,
    pos: u64,
}

impl DataPool {
    /// Creates a new `DataPool` from an in-memory vector of bytes.
    #[inline]
    pub fn from_vec(data: Vec<u8>) -> Self {
        let len = data.len() as u64;
        DataPool {
            source: Arc::new(Mutex::new(ArcCursor::new(Arc::new(data), 0, len))),
            start: 0,
            end: len,
            pos: 0,
        }
    }

    /// Creates a new `DataPool` from an in-memory `Arc<Vec<u8>>`.
    #[inline]
    pub fn from_arc_vec(data: Arc<Vec<u8>>) -> Self {
        let len = data.len() as u64;
        DataPool {
            source: Arc::new(Mutex::new(ArcCursor::new(data, 0, len))),
            start: 0,
            end: len,
            pos: 0,
        }
    }

    /// Creates a new `DataPool` by opening a file at the given path.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path)?;
        let len = file.len();
        Ok(DataPool {
            source: Arc::new(Mutex::new(file)),
            start: 0,
            end: len,
            pos: 0,
        })
    }

    /// Creates a new `DataPool` that is a view (slice) into another `DataPool`.
    pub fn slice(&self, offset: u64, len: Option<u64>) -> Result<Self> {
        let parent_len = self.len();
        if offset > parent_len {
            return Err(DjvuError::InvalidArg(
                "Slice offset is beyond the end of the data pool.".to_string(),
            ));
        }

        let slice_len = len.unwrap_or(parent_len - offset);
        if offset + slice_len > parent_len {
            return Err(DjvuError::InvalidArg(
                "Slice extends beyond the end of the data pool.".to_string(),
            ));
        }

        Ok(DataPool {
            source: self.source.clone(),
            start: self.start + offset,
            end: self.start + offset + slice_len,
            pos: 0,
        })
    }

    /// Returns the total length of the data available in this pool.
    #[inline]
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Returns `true` if the pool contains no data.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Executes a closure with a reference to the underlying bytes if the source is in-memory.
    ///
    /// This method provides safe, zero-copy access to the data pool's content when
    /// it's backed by an in-memory buffer. The mutex is held for the duration of the
    /// closure's execution.
    #[inline]
    /// Executes a closure with a mutable reference to the underlying `DataSource`.
    /// This is an internal helper for operations that need to modify the source, like `Write`.
    fn _with_data_mut<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut dyn DataSource) -> R,
    {
        let mut guard = self.source.lock().expect("DataPool mutex poisoned");
        f(&mut *guard)
    }

    pub fn with_bytes<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        let guard = self.source.lock().ok()?;
        // The `as_bytes` method on the source returns the full byte slice.
        // We apply the `DataPool`'s view (start/end) to get the correct sub-slice.
        guard.as_bytes().map(|bytes| {
            let pool_bytes = &bytes[self.start as usize..self.end as usize];
            f(pool_bytes)
        })
    }

    /// Converts the entire pool to a `Vec<u8>`.
    pub fn to_vec(&self) -> Result<Vec<u8>> {
        if let Some(vec) = self.with_bytes(|bytes| bytes.to_vec()) {
            Ok(vec)
        } else {
            let mut data = vec![0u8; self.len() as usize];
            let mut cursor = self.clone();
            cursor.seek(SeekFrom::Start(0))?;
            cursor.read_exact(&mut data)?;
            Ok(data)
        }
    }

    /// Reads a slice of `T` values using `bytemuck` for zero-copy conversion.
    pub fn read_pod_slice<T: Pod + Zeroable>(&mut self, count: usize) -> Result<Vec<T>> {
        let byte_count = count * std::mem::size_of::<T>();
        if self.pos + byte_count as u64 > self.len() {
            return Err(DjvuError::InvalidOperation(
                "Not enough data to read pod slice".to_string(),
            ));
        }

        let pod_slice_result = self.with_bytes(|bytes| {
            let start = self.pos as usize;
            let end = start + byte_count;
            // The `bytes` passed to the closure are already sliced to the pool's range.
            if end <= bytes.len() {
                let slice = &bytes[start..end];
                Some(bytemuck::cast_slice::<u8, T>(slice).to_vec())
            } else {
                None
            }
        });

        if let Some(Some(v)) = pod_slice_result {
            self.pos += byte_count as u64; // Update position after successful read
            return Ok(v);
        } else if let Some(None) = pod_slice_result {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Failed to read values",
            )
            .into());
        }

        // Fallback for non-in-memory sources
        let mut buffer = vec![0u8; byte_count];
        self.read_exact(&mut buffer)?;
        Ok(bytemuck::cast_slice(&buffer).to_vec())
    }
}

impl Read for DataPool {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = (self.end - self.pos).min(buf.len() as u64) as usize;
        if available == 0 {
            return Ok(0);
        }

        let mut source_guard = self.source.lock().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("Mutex lock error: {}", e))
        })?;

        source_guard.seek(SeekFrom::Start(self.start + self.pos))?;

        let read = source_guard.read(&mut buf[..available])?;

        self.pos += read as u64;
        Ok(read)
    }
}

impl Seek for DataPool {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let mut source_guard = self.source.lock().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("Mutex lock error: {}", e))
        })?;

        let new_abs_pos = source_guard.seek(pos)?;

        // Update our internal relative position
        self.pos = new_abs_pos - self.start;

        Ok(self.pos)
    }
}
