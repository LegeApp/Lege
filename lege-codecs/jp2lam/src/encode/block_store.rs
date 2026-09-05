//! Bounded storage for immutable Tier-1 code-block payloads.
//!
//! Phase 5 keeps rate-distortion metadata in memory while retaining each
//! compressed code-block payload exactly once. Payloads up to the configured
//! threshold stay in RAM; later payloads are appended to an anonymous scratch
//! file and addressed by stable [`PayloadRef`] values.

use crate::encode::backend::{NativeEncodedTier1Layout, NativeEncodedTier1Pass};
use crate::error::{Jp2LamError, Result};
use crate::model::ResourceLimits;
use crate::plan::BandOrientation;
use crate::profile::BlockClass;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockId {
    pub tile_index: u16,
    pub component: u16,
    pub resolution: u8,
    pub band: BandOrientation,
    pub block_index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadLocation {
    Memory { index: u32 },
    Spill { offset: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PayloadRef {
    location: PayloadLocation,
    len: usize,
}

impl PayloadRef {
    pub(crate) fn len(self) -> usize {
        self.len
    }

    /// Only exercised by tests, to assert that memory-pressure spilling
    /// actually happened rather than silently staying resident.
    #[cfg(test)]
    pub(crate) fn is_spilled(self) -> bool {
        matches!(self.location, PayloadLocation::Spill { .. })
    }
}

/// Only exercised by tests, as the return type of [`EncodedBlockStore::stats`].
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BlockStoreStats {
    pub memory_bytes: usize,
    pub spilled_bytes: u64,
    pub payload_count: usize,
    pub spilled_payload_count: usize,
}

#[derive(Debug)]
pub(crate) struct EncodedBlockStore {
    memory_limit: usize,
    memory_payloads: Vec<Vec<u8>>,
    memory_bytes: usize,
    spill: Option<File>,
    spill_directory: Option<std::path::PathBuf>,
    spilled_bytes: u64,
    payload_count: usize,
    spilled_payload_count: usize,
}

impl EncodedBlockStore {
    pub(crate) fn from_resource_limits(limits: &ResourceLimits) -> Self {
        Self {
            memory_limit: limits.encoded_store_memory_limit.unwrap_or(usize::MAX),
            memory_payloads: Vec::new(),
            memory_bytes: 0,
            spill: None,
            spill_directory: limits.spill_directory.clone(),
            spilled_bytes: 0,
            payload_count: 0,
            spilled_payload_count: 0,
        }
    }

    pub(crate) fn insert(&mut self, payload: Vec<u8>) -> Result<PayloadRef> {
        let len = payload.len();
        let next_memory_bytes = self.memory_bytes.checked_add(len).ok_or_else(|| {
            Jp2LamError::EncodeFailed("encoded block store memory accounting overflow".into())
        })?;
        self.payload_count = self.payload_count.checked_add(1).ok_or_else(|| {
            Jp2LamError::EncodeFailed("encoded block store payload count overflow".into())
        })?;

        if next_memory_bytes <= self.memory_limit {
            let index = u32::try_from(self.memory_payloads.len()).map_err(|_| {
                Jp2LamError::EncodeFailed("encoded block store has too many RAM payloads".into())
            })?;
            self.memory_payloads.push(payload);
            self.memory_bytes = next_memory_bytes;
            crate::encode::counters::record_encoded_store(self.memory_bytes, self.spilled_bytes);
            return Ok(PayloadRef {
                location: PayloadLocation::Memory { index },
                len,
            });
        }

        let offset = self.spilled_bytes;
        let spill = self.spill_file()?;
        spill.seek(SeekFrom::End(0)).map_err(io_error)?;
        spill.write_all(&payload).map_err(io_error)?;
        self.spilled_bytes = self
            .spilled_bytes
            .checked_add(len as u64)
            .ok_or_else(|| Jp2LamError::EncodeFailed("spill byte count overflow".into()))?;
        self.spilled_payload_count += 1;
        crate::encode::counters::record_encoded_store(self.memory_bytes, self.spilled_bytes);
        Ok(PayloadRef {
            location: PayloadLocation::Spill { offset },
            len,
        })
    }

    pub(crate) fn copy_prefix(&self, payload: PayloadRef, prefix_len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.write_prefix_to(payload, prefix_len, &mut out)?;
        Ok(out)
    }

    pub(crate) fn write_prefix_to<W: Write>(
        &self,
        payload: PayloadRef,
        prefix_len: usize,
        writer: &mut W,
    ) -> Result<()> {
        if prefix_len > payload.len {
            return Err(Jp2LamError::EncodeFailed(format!(
                "selected payload prefix {prefix_len} exceeds stored length {}",
                payload.len
            )));
        }
        match payload.location {
            PayloadLocation::Memory { index } => {
                let bytes = self.memory_payloads.get(index as usize).ok_or_else(|| {
                    Jp2LamError::EncodeFailed("invalid RAM payload reference".into())
                })?;
                writer.write_all(&bytes[..prefix_len]).map_err(io_error)
            }
            PayloadLocation::Spill { offset } => {
                let spill = self.spill.as_ref().ok_or_else(|| {
                    Jp2LamError::EncodeFailed("missing spill backing file".into())
                })?;
                let mut remaining = prefix_len;
                let mut read_offset = offset;
                let mut buffer = [0u8; COPY_BUFFER_BYTES];
                while remaining > 0 {
                    let chunk = remaining.min(buffer.len());
                    read_exact_at(spill, &mut buffer[..chunk], read_offset).map_err(io_error)?;
                    writer.write_all(&buffer[..chunk]).map_err(io_error)?;
                    remaining -= chunk;
                    read_offset = read_offset.checked_add(chunk as u64).ok_or_else(|| {
                        Jp2LamError::EncodeFailed("spill read offset overflow".into())
                    })?;
                }
                Ok(())
            }
        }
    }

    /// Only exercised by tests; see [`BlockStoreStats`].
    #[cfg(test)]
    pub(crate) fn stats(&self) -> BlockStoreStats {
        BlockStoreStats {
            memory_bytes: self.memory_bytes,
            spilled_bytes: self.spilled_bytes,
            payload_count: self.payload_count,
            spilled_payload_count: self.spilled_payload_count,
        }
    }

    pub(crate) fn into_shared(self) -> SharedEncodedBlockStore {
        Arc::new(self)
    }

    fn spill_file(&mut self) -> Result<&mut File> {
        if self.spill.is_none() {
            let file = match &self.spill_directory {
                Some(directory) => tempfile::tempfile_in(directory),
                None => tempfile::tempfile(),
            }
            .map_err(io_error)?;
            self.spill = Some(file);
        }
        self.spill
            .as_mut()
            .ok_or_else(|| Jp2LamError::EncodeFailed("failed to create spill file".into()))
    }
}

/// Immutable after construction; positional reads avoid a shared file cursor,
/// so independent tile payload writers do not serialize on a global mutex.
pub(crate) type SharedEncodedBlockStore = Arc<EncodedBlockStore>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredCodeBlock {
    pub id: BlockId,
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
    pub magnitude_bitplanes: u8,
    pub zero_bitplanes: u8,
    pub block_class: BlockClass,
    pub payload: PayloadRef,
    pub passes: Vec<NativeEncodedTier1Pass>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredBand {
    pub resolution: u8,
    pub band: BandOrientation,
    pub blocks: Vec<StoredCodeBlock>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredTier1Layout {
    pub tile_index: u16,
    pub component: u16,
    pub bands: Vec<StoredBand>,
}

pub(crate) fn stored_layout_metadata_bytes(layout: &StoredTier1Layout) -> usize {
    std::mem::size_of::<StoredTier1Layout>()
        .saturating_add(layout.bands.capacity() * std::mem::size_of::<StoredBand>())
        .saturating_add(
            layout
                .bands
                .iter()
                .map(|band| {
                    band.blocks.capacity() * std::mem::size_of::<StoredCodeBlock>()
                        + band
                            .blocks
                            .iter()
                            .map(|block| {
                                block.passes.capacity()
                                    * std::mem::size_of::<NativeEncodedTier1Pass>()
                            })
                            .sum::<usize>()
                })
                .sum::<usize>(),
        )
}

pub(crate) fn store_tier1_layout(
    store: &mut EncodedBlockStore,
    component: u16,
    layout: NativeEncodedTier1Layout,
) -> Result<StoredTier1Layout> {
    let tile_index = layout
        .bands
        .iter()
        .flat_map(|band| &band.blocks)
        .map(|block| block.tile_index)
        .next()
        .unwrap_or(0);
    let mut bands = Vec::with_capacity(layout.bands.len());
    for band in layout.bands {
        let mut blocks = Vec::with_capacity(band.blocks.len());
        for (block_index, block) in band.blocks.into_iter().enumerate() {
            if block.tile_index != tile_index {
                return Err(Jp2LamError::EncodeFailed(
                    "Tier-1 layout contains code-blocks from multiple tiles".into(),
                ));
            }
            let payload = store.insert(block.payload)?;
            blocks.push(StoredCodeBlock {
                id: BlockId {
                    tile_index,
                    component,
                    resolution: block.resolution,
                    band: block.band,
                    block_index: u32::try_from(block_index).map_err(|_| {
                        Jp2LamError::EncodeFailed("code-block index exceeds u32".into())
                    })?,
                },
                x0: block.x0,
                y0: block.y0,
                x1: block.x1,
                y1: block.y1,
                magnitude_bitplanes: block.magnitude_bitplanes,
                zero_bitplanes: block.zero_bitplanes,
                block_class: block.block_class,
                payload,
                passes: block.passes,
            });
        }
        bands.push(StoredBand {
            resolution: band.resolution,
            band: band.band,
            blocks,
        });
    }
    Ok(StoredTier1Layout {
        tile_index,
        component,
        bands,
    })
}

fn io_error(error: std::io::Error) -> Jp2LamError {
    Jp2LamError::EncodeFailed(format!("encoded block store I/O failed: {error}"))
}

fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buffer.is_empty() {
        #[cfg(unix)]
        let read = {
            use std::os::unix::fs::FileExt;
            file.read_at(buffer, offset)?
        };
        #[cfg(windows)]
        let read = {
            use std::os::windows::fs::FileExt;
            file.seek_read(buffer, offset)?
        };
        #[cfg(not(any(unix, windows)))]
        let read = {
            let mut cloned = file.try_clone()?;
            cloned.seek(SeekFrom::Start(offset))?;
            std::io::Read::read(&mut cloned, buffer)?
        };
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "spill payload ended before selected prefix",
            ));
        }
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("spill read offset overflow"))?;
        buffer = &mut buffer[read..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(memory_limit: usize, spill_directory: Option<std::path::PathBuf>) -> ResourceLimits {
        ResourceLimits {
            encoded_store_memory_limit: Some(memory_limit),
            spill_directory,
            ..Default::default()
        }
    }

    #[test]
    fn memory_payload_prefix_roundtrips() {
        let mut store = EncodedBlockStore::from_resource_limits(&limits(16, None));
        let payload = store.insert(vec![1, 2, 3, 4]).expect("insert");
        let mut out = Vec::new();
        store.write_prefix_to(payload, 3, &mut out).expect("read");
        assert_eq!(out, vec![1, 2, 3]);
        assert!(!payload.is_spilled());
        assert_eq!(store.stats().memory_bytes, 4);
    }

    #[test]
    fn threshold_spills_later_payload_and_keeps_references_stable() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut store = EncodedBlockStore::from_resource_limits(&limits(
            4,
            Some(directory.path().to_path_buf()),
        ));
        let memory = store.insert(vec![1, 2, 3, 4]).expect("memory insert");
        let spilled = store.insert(vec![5, 6, 7, 8, 9]).expect("spill insert");

        let mut out = Vec::new();
        store
            .write_prefix_to(spilled, 5, &mut out)
            .expect("spill read");
        store
            .write_prefix_to(memory, 4, &mut out)
            .expect("memory reread");
        assert_eq!(out, vec![5, 6, 7, 8, 9, 1, 2, 3, 4]);
        assert!(spilled.is_spilled());
        assert_eq!(
            store.stats(),
            BlockStoreStats {
                memory_bytes: 4,
                spilled_bytes: 5,
                payload_count: 2,
                spilled_payload_count: 1,
            }
        );
    }

    #[test]
    fn invalid_selected_prefix_is_rejected() {
        let mut store = EncodedBlockStore::from_resource_limits(&limits(16, None));
        let payload = store.insert(vec![1, 2]).expect("insert");
        let error = store
            .write_prefix_to(payload, 3, &mut Vec::new())
            .expect_err("oversized prefix");
        assert!(error.to_string().contains("exceeds stored length"));
    }

    #[test]
    fn spilled_payloads_support_concurrent_positional_reads() {
        let mut store = EncodedBlockStore::from_resource_limits(&limits(0, None));
        let first_bytes = (0..100_000).map(|index| index as u8).collect::<Vec<_>>();
        let second_bytes = (0..90_000)
            .map(|index| (index as u8).wrapping_mul(17))
            .collect::<Vec<_>>();
        let first = store.insert(first_bytes.clone()).expect("first spill");
        let second = store.insert(second_bytes.clone()).expect("second spill");
        let shared = store.into_shared();

        let first_store = Arc::clone(&shared);
        let first_thread = std::thread::spawn(move || {
            let mut output = Vec::new();
            first_store
                .write_prefix_to(first, first.len(), &mut output)
                .expect("first positional read");
            output
        });
        let second_store = Arc::clone(&shared);
        let second_thread = std::thread::spawn(move || {
            let mut output = Vec::new();
            second_store
                .write_prefix_to(second, second.len(), &mut output)
                .expect("second positional read");
            output
        });

        assert_eq!(first_thread.join().expect("first reader"), first_bytes);
        assert_eq!(second_thread.join().expect("second reader"), second_bytes);
    }
}
