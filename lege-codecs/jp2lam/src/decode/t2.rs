//! Tier-2 packet-header decoding primitives.
//!
//! This is the inverse side of ISO/IEC 15444-1 Annex B.10, including Annex
//! B.6 precinct routing and all Annex B.12 progression orders.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Range;
use std::time::Instant;

use crate::decode::DecodeLimits;
use crate::error::{Jp2LamError, Result};
use crate::j2k::decode_markers::{CodestreamHeader, ProgressionOrder};
use crate::plan::BandOrientation;

const TGT_NO_PARENT: u32 = u32::MAX;
const MARKER_SOP: [u8; 2] = [0xff, 0x91];
const MARKER_EPH: [u8; 2] = [0xff, 0x92];
const SOP_SEGMENT_LEN: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedTilePackets<'a> {
    pub(crate) packet_stats: PacketStats,
    #[cfg(test)]
    pub(crate) packets: Vec<DecodedPacket>,
    pub(crate) codeblocks: Vec<DecodedCodeBlock<'a>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PacketStats {
    pub(crate) count: usize,
    pub(crate) header_bytes: usize,
    pub(crate) body_bytes: usize,
    pub(crate) contribution_count: usize,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedPacket {
    pub(crate) layer: u16,
    pub(crate) resolution: u8,
    pub(crate) component: usize,
    pub(crate) precinct: usize,
    pub(crate) header_len: usize,
    pub(crate) body_len: usize,
    pub(crate) contribution_count: usize,
}

/// One code-block, with every quality layer's contribution merged.
///
/// A code-block may be included in several layers; each inclusion appends
/// coding passes to the *same* block. Unterminated contributions concatenate
/// into one MQ codeword segment across layers; TERMALL contributions retain
/// the per-pass segments signalled by Annex B.10.7.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedCodeBlock<'a> {
    pub(crate) component: usize,
    pub(crate) band_index: usize,
    pub(crate) block_index: usize,
    pub(crate) resolution: u8,
    pub(crate) band: BandOrientation,
    pub(crate) x0: u32,
    pub(crate) y0: u32,
    pub(crate) x1: u32,
    pub(crate) y1: u32,
    pub(crate) zero_bitplanes: u32,
    pub(crate) passes: u32,
    pub(crate) segments: Vec<DecodedCodewordSegment<'a>>,
}

/// A packed-coordinate coefficient rectangle that a region decode must retain
/// for one subband so the inverse DWT reproduces the region's pixels exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegionBandWindow {
    pub(crate) resolution: u8,
    pub(crate) band: BandOrientation,
    pub(crate) x0: u32,
    pub(crate) y0: u32,
    pub(crate) x1: u32,
    pub(crate) y1: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedCodewordSegment<'a> {
    pub(crate) passes: u32,
    pub(crate) data: Cow<'a, [u8]>,
}

#[derive(Debug, Clone)]
struct BandState {
    component: usize,
    resolution: u8,
    band: BandOrientation,
    blocks: Vec<BlockState>,
    precincts: Vec<BandPrecinctState>,
}

#[derive(Debug, Clone)]
struct BandPrecinctState {
    block_indices: Range<usize>,
    inclusion: TagTreeReader,
    zero_bitplanes: TagTreeReader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

#[derive(Debug, Clone, Copy)]
struct PrecinctGrid {
    resolution_bounds: Rect,
    pp_x: u8,
    pp_y: u8,
    start_x: u32,
    start_y: u32,
    width: usize,
    height: usize,
}

#[derive(Debug, Clone, Copy)]
struct BandGeometry {
    resolution: u8,
    band: BandOrientation,
    bounds: Rect,
    packed_x0: u32,
    packed_y0: u32,
}

#[derive(Debug, Clone)]
struct BlockState {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    included: bool,
    numlenbits: u32,
    zero_bitplanes: Option<u32>,
}
#[derive(Debug, Clone)]
struct Contribution {
    band_index: usize,
    block_index: usize,
    zero_bitplanes: u32,
    passes: u32,
    segments: InlineValues<ContributionSegment>,
}

impl Contribution {
    fn length(&self) -> Result<usize> {
        self.segments.as_slice().iter().try_fold(0usize, |total, segment| {
            total
                .checked_add(segment.length)
                .ok_or_else(|| invalid("codeword contribution length overflow"))
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ContributionSegment {
    passes: u32,
    length: usize,
}

/// One value inline for the overwhelmingly common case, spilling only when
/// packet segmentation or repeated quality-layer chunks require it.
#[derive(Debug, Clone)]
enum InlineValues<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> InlineValues<T> {
    fn as_slice(&self) -> &[T] {
        match self {
            Self::One(value) => std::slice::from_ref(value),
            Self::Many(values) => values,
        }
    }

    fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn last_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::One(value) => Some(value),
            Self::Many(values) => values.last_mut(),
        }
    }

    fn try_push(&mut self, value: T) -> Result<()> {
        match self {
            Self::Many(values) => {
                values
                    .try_reserve(1)
                    .map_err(|_| invalid("inline value spill allocation failed"))?;
                values.push(value);
            }
            Self::One(_) => {
                let first = match std::mem::replace(self, Self::Many(Vec::new())) {
                    Self::One(first) => first,
                    Self::Many(_) => unreachable!(),
                };
                let mut values = Vec::new();
                values
                    .try_reserve_exact(2)
                    .map_err(|_| invalid("inline value spill allocation failed"))?;
                values.push(first);
                values.push(value);
                *self = Self::Many(values);
            }
        }
        Ok(())
    }

    fn try_extend(&mut self, other: Self) -> Result<()> {
        match other {
            Self::One(value) => self.try_push(value)?,
            Self::Many(values) => {
                for value in values {
                    self.try_push(value)?;
                }
            }
        }
        Ok(())
    }

    fn into_vec(self) -> Vec<T> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

/// Stateful Annex B.10 packet reader for one tile.
///
/// Annex B.11 permits a tile's ordered packet sequence to be split over
/// several tile-parts, which may be interleaved with tile-parts from other
/// tiles. The inclusion trees, zero-bit-plane trees, code-block Lblock state,
/// and packet-progression position consequently belong to the tile, not to an individual
/// tile-part. Feed every part for this tile to [`Self::push_tile_part`] in its
/// `TPsot` order, then consume the result with [`Self::finish`].
pub(crate) struct TilePacketDecoder<'a> {
    bands: Vec<BandState>,
    band_lookup: Vec<Vec<Vec<usize>>>,
    sop_markers: bool,
    eph_markers: bool,
    terminate_each_pass: bool,
    next_sop_sequence: u16,
    packet_positions: PacketProgression,
    next_packet_index: usize,
    packet_stats: PacketStats,
    #[cfg(test)]
    packet_records: Option<Vec<DecodedPacket>>,
    // Contributions accumulate per code-block across layers. `order` keeps
    // first-inclusion order so output is deterministic regardless of how the
    // layers or tile-parts are distributed.
    merged: HashMap<(usize, usize), MergedBlock<'a>>,
    order: Vec<(usize, usize)>,
    contribution_scratch: Vec<Contribution>,
    profile: bool,
    merge_ns: u64,
    highest_resolution: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PacketPosition {
    layer: u16,
    resolution: u8,
    component: usize,
    precinct: usize,
}

#[derive(Debug)]
struct PacketProgression {
    header: CodestreamHeader,
    grids: Vec<Vec<PrecinctGrid>>,
    layers: u16,
    state: PacketProgressionState,
    emitted: usize,
    total: usize,
}

#[derive(Debug)]
enum PacketProgressionState {
    Lrcp {
        layer: u16,
        resolution: u8,
        component: usize,
        precinct: usize,
    },
    Rlcp {
        resolution: u8,
        layer: u16,
        component: usize,
        precinct: usize,
    },
    Spatial {
        order: ProgressionOrder,
        streams: Vec<PacketPositionStream>,
        heap: BinaryHeap<Reverse<SpatialHeapEntry>>,
        pending: Option<(PacketPosition, u16)>,
    },
}

#[derive(Debug)]
struct PacketPositionStream {
    resolution: u8,
    component: usize,
    next_precinct: usize,
    precinct_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SpatialHeapEntry {
    key: [u64; 4],
    stream: usize,
}

impl PacketProgression {
    fn new(header: &CodestreamHeader, grids: Vec<Vec<PrecinctGrid>>, total: usize) -> Result<Self> {
        let state = match header.cod.progression_order {
            ProgressionOrder::Lrcp => PacketProgressionState::Lrcp {
                layer: 0,
                resolution: 0,
                component: 0,
                precinct: 0,
            },
            ProgressionOrder::Rlcp => PacketProgressionState::Rlcp {
                resolution: 0,
                layer: 0,
                component: 0,
                precinct: 0,
            },
            order @ (ProgressionOrder::Rpcl | ProgressionOrder::Pcrl | ProgressionOrder::Cprl) => {
                let mut streams = Vec::new();
                streams
                    .try_reserve_exact(
                        header
                            .siz
                            .components
                            .len()
                            .checked_mul(usize::from(header.cod.decomposition_levels) + 1)
                            .ok_or_else(|| invalid("packet-position stream count overflow"))?,
                    )
                    .map_err(|_| invalid("packet-position stream allocation failed"))?;
                for component in 0..header.siz.components.len() {
                    for resolution in 0..=header.cod.decomposition_levels {
                        let grid = grids
                            .get(component)
                            .and_then(|component_grids| {
                                component_grids.get(usize::from(resolution))
                            })
                            .ok_or_else(|| {
                                invalid("packet plan references a missing precinct grid")
                            })?;
                        let precinct_count = grid
                            .width
                            .checked_mul(grid.height)
                            .ok_or_else(|| invalid("precinct count overflow"))?;
                        if precinct_count != 0 {
                            streams.push(PacketPositionStream {
                                resolution,
                                component,
                                next_precinct: 0,
                                precinct_count,
                            });
                        }
                    }
                }
                let mut heap = BinaryHeap::new();
                heap.try_reserve(streams.len())
                    .map_err(|_| invalid("spatial packet heap allocation failed"))?;
                for (stream_index, stream) in streams.iter().enumerate() {
                    heap.push(Reverse(SpatialHeapEntry {
                        key: spatial_packet_key(
                            header,
                            &grids,
                            order,
                            stream.component,
                            stream.resolution,
                            0,
                        )?,
                        stream: stream_index,
                    }));
                }
                PacketProgressionState::Spatial {
                    order,
                    streams,
                    heap,
                    pending: None,
                }
            }
        };
        Ok(Self {
            header: header.clone(),
            grids,
            layers: header.cod.layers,
            state,
            emitted: 0,
            total,
        })
    }

    fn next(&mut self) -> Result<Option<PacketPosition>> {
        let header = &self.header;
        let packet = match &mut self.state {
            PacketProgressionState::Lrcp {
                layer,
                resolution,
                component,
                precinct,
            } => next_lrcp(
                &self.grids,
                self.layers,
                header.cod.decomposition_levels,
                layer,
                resolution,
                component,
                precinct,
            )?,
            PacketProgressionState::Rlcp {
                resolution,
                layer,
                component,
                precinct,
            } => next_rlcp(
                &self.grids,
                self.layers,
                header.cod.decomposition_levels,
                resolution,
                layer,
                component,
                precinct,
            )?,
            PacketProgressionState::Spatial {
                order,
                streams,
                heap,
                pending,
            } => {
                if let Some((base, next_layer)) = pending {
                    let packet = PacketPosition {
                        layer: *next_layer,
                        ..*base
                    };
                    *next_layer += 1;
                    if *next_layer == self.layers {
                        *pending = None;
                    }
                    Some(packet)
                } else {
                    let Some(Reverse(entry)) = heap.pop() else {
                        return Ok(None);
                    };
                    let stream = &mut streams[entry.stream];
                    let precinct = stream.next_precinct;
                    let base = PacketPosition {
                        layer: 0,
                        resolution: stream.resolution,
                        component: stream.component,
                        precinct,
                    };
                    stream.next_precinct += 1;
                    if stream.next_precinct < stream.precinct_count {
                        heap.push(Reverse(SpatialHeapEntry {
                            key: spatial_packet_key(
                                header,
                                &self.grids,
                                *order,
                                stream.component,
                                stream.resolution,
                                stream.next_precinct,
                            )?,
                            stream: entry.stream,
                        }));
                    }
                    if self.layers > 1 {
                        *pending = Some((base, 1));
                    }
                    Some(base)
                }
            }
        };
        if packet.is_some() {
            self.emitted = self
                .emitted
                .checked_add(1)
                .ok_or_else(|| invalid("emitted packet count overflow"))?;
        }
        debug_assert!(self.emitted <= self.total);
        Ok(packet)
    }
}

/// One axis of the packet-progression odometer.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PacketAxis {
    Layer,
    Resolution,
    Component,
}

/// The packet iteration order as axes from innermost (fastest-varying) to
/// outermost for the single-precinct test case. With P=1, Annex B.12's
/// resolution/position-ordered progressions reduce to permutations of
/// (layer, resolution, component), and PCRL and CPRL coincide.
#[cfg(test)]
fn packet_axis_order(order: ProgressionOrder) -> [PacketAxis; 3] {
    use PacketAxis::{Component, Layer, Resolution};
    match order {
        ProgressionOrder::Lrcp => [Component, Resolution, Layer],
        ProgressionOrder::Rlcp => [Component, Layer, Resolution],
        ProgressionOrder::Rpcl => [Layer, Component, Resolution],
        ProgressionOrder::Pcrl | ProgressionOrder::Cprl => [Layer, Resolution, Component],
    }
}

impl<'a> TilePacketDecoder<'a> {
    pub(crate) fn new(header: &CodestreamHeader) -> Result<Self> {
        Self::new_with_limits(
            header,
            false,
            header.cod.decomposition_levels,
            &DecodeLimits::default(),
        )
    }

    pub(crate) fn new_with_limits(
        header: &CodestreamHeader,
        profile: bool,
        highest_resolution: u8,
        limits: &DecodeLimits,
    ) -> Result<Self> {
        validate_packet_scope(header)?;
        if highest_resolution > header.cod.decomposition_levels {
            return Err(invalid(
                "requested resolution exceeds COD decomposition levels",
            ));
        }
        let precinct_grids = build_precinct_grids(header)?;
        let precinct_count = precinct_position_count(header, &precinct_grids)?;
        if precinct_count > limits.max_precincts {
            return Err(invalid(format!(
                "tile precinct count {precinct_count} exceeds decode limit {}",
                limits.max_precincts
            )));
        }
        let packet_count = precinct_count
            .checked_mul(usize::from(header.cod.layers))
            .ok_or_else(|| invalid("packet-position count overflow"))?;
        if packet_count > limits.max_packets {
            return Err(invalid(format!(
                "tile packet count {packet_count} exceeds decode limit {}",
                limits.max_packets
            )));
        }
        let bands = build_band_states(header, &precinct_grids, limits.max_code_blocks)?;
        let component_count = header.siz.components.len();
        let levels = header.cod.decomposition_levels;
        let packet_positions = PacketProgression::new(header, precinct_grids, packet_count)?;
        Ok(Self {
            band_lookup: build_band_lookup(&bands, component_count, levels),
            bands,
            sop_markers: header.cod.sop_markers,
            eph_markers: header.cod.eph_markers,
            terminate_each_pass: header.cod.code_block_style.terminate_each_pass,
            next_sop_sequence: 0,
            packet_positions,
            next_packet_index: 0,
            packet_stats: PacketStats::default(),
            #[cfg(test)]
            packet_records: None,
            merged: HashMap::new(),
            order: Vec::new(),
            contribution_scratch: Vec::new(),
            profile,
            merge_ns: 0,
            highest_resolution,
        })
    }

    pub(crate) fn merge_ns(&self) -> u64 {
        self.merge_ns
    }

    /// Decode the complete compressed-data portion of one tile-part.
    ///
    /// Tile-part boundaries are packet boundaries (ISO/IEC 15444-1 B.11), so
    /// any non-empty trailing fragment is malformed rather than padding, and the
    /// strict "bytes after the packet sequence" error below stands.
    ///
    /// HISTORY: a `TPsot==TNsot` book scan (Prussian-Line-Infantry) once tripped
    /// this error on exactly one of its 35 tiles, and simply tolerating the tail
    /// left a 256-row garbage stripe. The real cause was NOT tile-part assembly
    /// (every tile is structured identically) but a missing packet-header
    /// byte-realignment: a packet header ending on 0xFF must consume the
    /// following bit-stuff byte ([`PacketBioReader::inalign`], Annex B.10.1).
    /// Without it that one header counted a byte short, sliced its body early,
    /// and desynchronised the tile — surfacing as a spurious trailing byte here.
    /// With `inalign` in place the whole stream decodes bit-close to OpenJPEG.
    pub(crate) fn push_tile_part(&mut self, payload: &'a [u8]) -> Result<()> {
        let mut pos = 0usize;
        while pos < payload.len() {
            let Some(packet) = self.packet_positions.next()? else {
                return Err(invalid(format!(
                    "tile-part contains {} bytes after the tile's {}-packet sequence",
                    payload.len() - pos,
                    self.next_packet_index
                )));
            };
            let packet_start = pos;
            let sop_len = self.consume_sop(payload, packet_start)?;
            let header_start = packet_start
                .checked_add(sop_len)
                .ok_or_else(|| invalid("SOP-adjusted packet offset overflow"))?;
            let mut bio = PacketBioReader::new(
                payload
                    .get(header_start..)
                    .ok_or_else(|| invalid("packet offset past tile-part payload"))?,
            );
            let packet_present = bio.read_bit()? != 0;
            let mut contributions = std::mem::take(&mut self.contribution_scratch);
            contributions.clear();

            if packet_present {
                for &band_index in &self.band_lookup[packet.component][packet.resolution as usize] {
                    read_band_contributions(
                        &mut bio,
                        packet.layer,
                        packet.precinct,
                        band_index,
                        self.terminate_each_pass,
                        &mut self.bands,
                        &mut contributions,
                    )?;
                }
            }

            // Realign to the byte boundary that closes the bit-stuffed packet
            // header (Annex B.10.1), consuming the trailing stuff byte when the
            // header ends on 0xFF. Done for present and empty packets alike, as
            // OpenJPEG does.
            bio.inalign()?;
            let packet_header_bytes = bio.bytes_consumed();
            pos = header_start
                .checked_add(packet_header_bytes)
                .ok_or_else(|| invalid("packet header offset overflow"))?;
            let eph_len = self.consume_eph(payload, pos)?;
            pos = pos
                .checked_add(eph_len)
                .ok_or_else(|| invalid("EPH-adjusted packet offset overflow"))?;
            let header_len = sop_len
                .checked_add(packet_header_bytes)
                .and_then(|len| len.checked_add(eph_len))
                .ok_or_else(|| invalid("packet header length overflow"))?;
            let body_len = contributions
                .iter()
                .try_fold(0usize, |total, contribution| {
                    total
                        .checked_add(contribution.length()?)
                        .ok_or_else(|| invalid("packet body length overflow"))
                })?;
            let body_end = pos
                .checked_add(body_len)
                .ok_or_else(|| invalid("packet body offset overflow"))?;
            let body = payload
                .get(pos..body_end)
                .ok_or_else(|| invalid("packet body extends past tile-part payload"))?;
            let contribution_count = contributions.len();
            if std::env::var_os("JP2LAM_TRACE_T2").is_some() {
                eprintln!(
                    "t2 packet={} L={} R={} C={} P={} start={} header={} body={} contributions={} payload={}",
                    self.next_packet_index,
                    packet.layer,
                    packet.resolution,
                    packet.component,
                    packet.precinct,
                    packet_start,
                    header_len,
                    body_len,
                    contribution_count,
                    payload.len()
                );
            }
            let merge_start = self.profile.then(Instant::now);
            self.merge_contributions(packet, &mut contributions, body)?;
            if let Some(start) = merge_start {
                self.merge_ns = self
                    .merge_ns
                    .saturating_add(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
            }
            self.contribution_scratch = contributions;
            pos = body_end;
            self.packet_stats.count = self
                .packet_stats
                .count
                .checked_add(1)
                .ok_or_else(|| invalid("decoded packet count overflow"))?;
            self.packet_stats.header_bytes = self
                .packet_stats
                .header_bytes
                .checked_add(header_len)
                .ok_or_else(|| invalid("decoded packet-header byte count overflow"))?;
            self.packet_stats.body_bytes = self
                .packet_stats
                .body_bytes
                .checked_add(body_len)
                .ok_or_else(|| invalid("decoded packet-body byte count overflow"))?;
            self.packet_stats.contribution_count = self
                .packet_stats
                .contribution_count
                .checked_add(contribution_count)
                .ok_or_else(|| invalid("decoded contribution count overflow"))?;
            #[cfg(test)]
            if let Some(records) = &mut self.packet_records {
                records.push(DecodedPacket {
                    layer: packet.layer,
                    resolution: packet.resolution,
                    component: packet.component,
                    precinct: packet.precinct,
                    header_len,
                    body_len,
                    contribution_count,
                });
            }
            if self.sop_markers {
                // Nsop counts every packet in the coded tile, including a
                // packet whose optional SOP segment is omitted (Annex A.8.1).
                self.next_sop_sequence = self.next_sop_sequence.wrapping_add(1);
            }
            self.next_packet_index += 1;
        }
        Ok(())
    }

    /// Consume an SOP marker segment when COD permits it and one is present.
    ///
    /// Annex A.8.1 defines SOP as `FF91 Lsop Nsop`, with `Lsop=4`. The COD bit
    /// means SOP segments may be used, so absence remains valid; a marker that
    /// is present is validated and its packet sequence number must advance
    /// modulo 65536.
    fn consume_sop(&mut self, payload: &[u8], pos: usize) -> Result<usize> {
        let marker_end = pos
            .checked_add(MARKER_SOP.len())
            .ok_or_else(|| invalid("SOP marker offset overflow"))?;
        if !self.sop_markers || payload.get(pos..marker_end) != Some(MARKER_SOP.as_slice()) {
            return Ok(0);
        }
        let segment_end = pos
            .checked_add(SOP_SEGMENT_LEN)
            .ok_or_else(|| invalid("SOP segment offset overflow"))?;
        let segment = payload
            .get(pos..segment_end)
            .ok_or_else(|| invalid("truncated SOP marker segment"))?;
        let lsop = u16::from_be_bytes([segment[2], segment[3]]);
        if lsop != 4 {
            return Err(invalid(format!(
                "invalid SOP marker length {lsop}; expected 4"
            )));
        }
        let nsop = u16::from_be_bytes([segment[4], segment[5]]);
        if nsop != self.next_sop_sequence {
            return Err(invalid(format!(
                "SOP packet sequence mismatch: expected {}, found {nsop}",
                self.next_sop_sequence
            )));
        }
        Ok(SOP_SEGMENT_LEN)
    }

    /// Consume the fixed two-byte EPH marker immediately following a packet
    /// header when COD requires EPH markers (Annex A.8.2 / B.11).
    fn consume_eph(&self, payload: &[u8], pos: usize) -> Result<usize> {
        if !self.eph_markers {
            return Ok(0);
        }
        let marker_end = pos
            .checked_add(MARKER_EPH.len())
            .ok_or_else(|| invalid("EPH marker offset overflow"))?;
        if payload.get(pos..marker_end) == Some(MARKER_EPH.as_slice()) {
            Ok(MARKER_EPH.len())
        } else {
            Err(invalid(
                "COD requires an EPH marker after every packet header",
            ))
        }
    }

    pub(crate) fn finish(mut self) -> Result<DecodedTilePackets<'a>> {
        if let Some(next) = self.packet_positions.next()? {
            return Err(invalid(format!(
                "tile packet sequence ended early at layer {} resolution {} component {} precinct {}",
                next.layer, next.resolution, next.component, next.precinct
            )));
        }

        let codeblocks = self
            .order
            .into_iter()
            .map(|key| {
                let block = self
                    .merged
                    .remove(&key)
                    .expect("merged entry present for ordered key");
                let segments = block
                    .segments
                    .into_vec()
                    .into_iter()
                    .map(|segment| {
                        let data = match segment.chunks {
                            InlineValues::One(chunk) => Cow::Borrowed(chunk),
                            InlineValues::Many(chunks) => Cow::Owned(chunks.concat()),
                        };
                        DecodedCodewordSegment {
                            passes: segment.passes,
                            data,
                        }
                    })
                    .collect();
                DecodedCodeBlock {
                    component: block.component,
                    band_index: block.band_index,
                    block_index: block.block_index,
                    resolution: block.resolution,
                    band: block.band,
                    x0: block.x0,
                    y0: block.y0,
                    x1: block.x1,
                    y1: block.y1,
                    zero_bitplanes: block.zero_bitplanes,
                    passes: block.passes,
                    segments,
                }
            })
            .collect();

        Ok(DecodedTilePackets {
            packet_stats: self.packet_stats,
            #[cfg(test)]
            packets: self.packet_records.unwrap_or_default(),
            codeblocks,
        })
    }

    fn merge_contributions(
        &mut self,
        packet: PacketPosition,
        contributions: &mut Vec<Contribution>,
        body: &'a [u8],
    ) -> Result<()> {
        let mut body_pos = 0usize;
        for contribution in contributions.drain(..) {
            let retain = packet.resolution <= self.highest_resolution;
            let mut new_segments = if retain && contribution.segments.len() > 1 {
                let mut values = Vec::new();
                values
                    .try_reserve_exact(contribution.segments.len())
                    .map_err(|_| invalid("merged segment allocation failed"))?;
                Some(InlineValues::Many(values))
            } else {
                None
            };
            for segment in contribution.segments.as_slice() {
                let end = body_pos
                    .checked_add(segment.length)
                    .ok_or_else(|| invalid("codeword-segment length overflow"))?;
                let data = body
                    .get(body_pos..end)
                    .ok_or_else(|| invalid("codeword segment exceeds packet body"))?;
                body_pos = end;
                if retain {
                    let merged = MergedSegment {
                        passes: segment.passes,
                        chunks: InlineValues::One(data),
                    };
                    if let Some(new_segments) = &mut new_segments {
                        new_segments.try_push(merged)?;
                    } else {
                        new_segments = Some(InlineValues::One(merged));
                    }
                }
            }
            if !retain {
                continue;
            }
            let new_segments = new_segments.expect("retained contribution has segment storage");
            let band_state = &self.bands[contribution.band_index];
            let block = &band_state.blocks[contribution.block_index];
            let key = (contribution.band_index, contribution.block_index);
            match self.merged.entry(key) {
                Entry::Occupied(mut slot) => {
                    let existing = slot.get_mut();
                    existing.passes += contribution.passes;
                    if self.terminate_each_pass {
                        existing.segments.try_extend(new_segments)?;
                    } else {
                        let segment = existing
                            .segments
                            .last_mut()
                            .expect("included code-block has one MQ segment");
                        segment.passes += contribution.passes;
                        let new_segment = match new_segments {
                            InlineValues::One(segment) => segment,
                            InlineValues::Many(_) => {
                                unreachable!("unterminated contribution has one segment")
                            }
                        };
                        segment.chunks.try_extend(new_segment.chunks)?;
                    }
                }
                Entry::Vacant(slot) => {
                    self.order.push(key);
                    slot.insert(MergedBlock {
                        component: band_state.component,
                        band_index: contribution.band_index,
                        block_index: contribution.block_index,
                        resolution: packet.resolution,
                        band: band_state.band,
                        x0: block.x0,
                        y0: block.y0,
                        x1: block.x1,
                        y1: block.y1,
                        // Signalled once, on first inclusion.
                        zero_bitplanes: contribution.zero_bitplanes,
                        passes: contribution.passes,
                        segments: new_segments,
                    });
                }
            }
        }
        debug_assert_eq!(body_pos, body.len());
        Ok(())
    }
}

/// Decode a single tile-part containing the complete packet sequence for one
/// tile. Multi-tile-part decoding uses [`TilePacketDecoder`] directly.
#[cfg(test)]
pub(crate) fn parse_tile_part_payload<'a>(
    header: &CodestreamHeader,
    payload: &'a [u8],
) -> Result<DecodedTilePackets<'a>> {
    let mut decoder = TilePacketDecoder::new(header)?;
    decoder.packet_records = Some(Vec::new());
    decoder.push_tile_part(payload)?;
    decoder.finish()
}

/// A code-block's contributions gathered across quality layers, before the
/// segments are concatenated into a single MQ codeword segment.
#[derive(Debug, Clone)]
struct MergedBlock<'a> {
    component: usize,
    band_index: usize,
    block_index: usize,
    resolution: u8,
    band: BandOrientation,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    zero_bitplanes: u32,
    passes: u32,
    segments: InlineValues<MergedSegment<'a>>,
}

#[derive(Debug, Clone)]
struct MergedSegment<'a> {
    passes: u32,
    chunks: InlineValues<&'a [u8]>,
}

fn validate_packet_scope(header: &CodestreamHeader) -> Result<()> {
    // All five Annex B.12 progression orders are traversed by the stateful
    // packet plan, including the precinct-position axis.
    // The packet plan is component-count agnostic (it iterates the component
    // axis); 2 is admitted for Gray + in-data alpha.
    if !matches!(header.siz.components.len(), 1 | 2 | 3 | 4) {
        return Err(invalid(
            "only 1-, 2-, 3-, and 4-component packet decoding is implemented",
        ));
    }
    if header.cod.uses_precincts {
        let expected = usize::from(header.cod.decomposition_levels) + 1;
        if header.cod.precinct_sizes.len() != expected {
            return Err(invalid(format!(
                "COD has {} precinct sizes, expected {expected}",
                header.cod.precinct_sizes.len()
            )));
        }
        for (resolution, precinct) in header.cod.precinct_sizes.iter().enumerate() {
            if resolution > 0 && (precinct.pp_x == 0 || precinct.pp_y == 0) {
                return Err(invalid(
                    "COD precinct exponents must be at least one above resolution zero",
                ));
            }
        }
    }
    // Segmentation symbols are handled entirely inside Tier-1 (they add four
    // UNIFORM-context symbols at each cleanup pass) and leave packet
    // segmentation untouched, so they are permitted here. The remaining
    // styles do change how passes are segmented into codeword segments,
    // which this concatenating parser does not model.
    if header.cod.code_block_style.bypass {
        return Err(invalid(
            "selective arithmetic bypass packet segmentation is unsupported",
        ));
    }
    Ok(())
}

fn read_band_contributions(
    bio: &mut PacketBioReader<'_>,
    layer: u16,
    precinct_index: usize,
    band_index: usize,
    terminate_each_pass: bool,
    bands: &mut [BandState],
    contributions: &mut Vec<Contribution>,
) -> Result<()> {
    let band = bands
        .get_mut(band_index)
        .ok_or_else(|| invalid("packet references an invalid subband"))?;
    let precinct = band
        .precincts
        .get_mut(precinct_index)
        .ok_or_else(|| invalid("packet references an invalid precinct"))?;
    for (leaf_index, block_index) in precinct.block_indices.clone().enumerate() {
        let was_included = band.blocks[block_index].included;
        let included = if was_included {
            bio.read_bit()? != 0
        } else {
            precinct
                .inclusion
                .decode(bio, leaf_index, i32::from(layer) + 1)?
        };

        if !included {
            continue;
        }

        let zero_bitplanes = if let Some(value) = band.blocks[block_index].zero_bitplanes {
            value
        } else {
            let tree = &mut precinct.zero_bitplanes;
            tree.decode(bio, leaf_index, 999)?;
            let value = tree
                .value(leaf_index)
                .ok_or_else(|| invalid("zero-bitplane tag tree did not terminate"))?;
            let value = u32::try_from(value)
                .map_err(|_| invalid("negative zero-bitplane tag-tree value"))?;
            band.blocks[block_index].zero_bitplanes = Some(value);
            value
        };

        let passes = read_numpasses(bio)?;
        let increment = read_commacode(bio)?;
        band.blocks[block_index].numlenbits = band.blocks[block_index]
            .numlenbits
            .checked_add(increment)
            .ok_or_else(|| invalid("codeword length-bit count overflow"))?;
        let segments = if terminate_each_pass {
            let segment_count = usize::try_from(passes)
                .map_err(|_| invalid("coding-pass count exceeds usize"))?;
            let len_bits = band.blocks[block_index]
                .numlenbits
                .checked_add(floor_log2(1))
                .ok_or_else(|| invalid("codeword segment length-bit count overflow"))?;
            if segment_count == 1 {
                InlineValues::One(ContributionSegment {
                    passes: 1,
                    length: bio.read_bits_usize(len_bits)?,
                })
            } else {
                let mut segments = Vec::new();
                segments
                    .try_reserve_exact(segment_count)
                    .map_err(|_| invalid("codeword contribution allocation failed"))?;
                for _ in 0..segment_count {
                    segments.push(ContributionSegment {
                        passes: 1,
                        length: bio.read_bits_usize(len_bits)?,
                    });
                }
                InlineValues::Many(segments)
            }
        } else {
            let len_bits = band.blocks[block_index]
                .numlenbits
                .checked_add(floor_log2(passes))
                .ok_or_else(|| invalid("codeword segment length-bit count overflow"))?;
            InlineValues::One(ContributionSegment {
                passes,
                length: bio.read_bits_usize(len_bits)?,
            })
        };
        band.blocks[block_index].included = true;
        contributions.push(Contribution {
            band_index,
            block_index,
            zero_bitplanes,
            passes,
            segments,
        });
    }
    Ok(())
}

fn build_band_lookup(
    bands: &[BandState],
    component_count: usize,
    levels: u8,
) -> Vec<Vec<Vec<usize>>> {
    let res_count = usize::from(levels) + 1;
    let mut lookup = vec![vec![Vec::new(); res_count]; component_count];
    for (index, band) in bands.iter().enumerate() {
        lookup[band.component][band.resolution as usize].push(index);
    }
    lookup
}

fn build_precinct_grids(header: &CodestreamHeader) -> Result<Vec<Vec<PrecinctGrid>>> {
    let mut grids = Vec::with_capacity(header.siz.components.len());
    for component in 0..header.siz.components.len() {
        let resolution_bounds = resolution_bounds(header, component)?;
        let mut component_grids = Vec::with_capacity(resolution_bounds.len());
        for (resolution, &bounds) in resolution_bounds.iter().enumerate() {
            let (pp_x, pp_y) = if header.cod.uses_precincts {
                let precinct = header.cod.precinct_sizes[resolution];
                (precinct.pp_x, precinct.pp_y)
            } else {
                // Scod=0 supplies the Part 1 default PPx=PPy=15 rather than
                // removing the precinct partition altogether.
                (15, 15)
            };
            let precinct_width = 1u32 << pp_x;
            let precinct_height = 1u32 << pp_y;
            let start_x = bounds.x0 / precinct_width;
            let start_y = bounds.y0 / precinct_height;
            let end_x = bounds.x1.div_ceil(precinct_width);
            let end_y = bounds.y1.div_ceil(precinct_height);
            component_grids.push(PrecinctGrid {
                resolution_bounds: bounds,
                pp_x,
                pp_y,
                start_x,
                start_y,
                width: usize::try_from(end_x - start_x)
                    .map_err(|_| invalid("precinct-grid width exceeds usize"))?,
                height: usize::try_from(end_y - start_y)
                    .map_err(|_| invalid("precinct-grid height exceeds usize"))?,
            });
        }
        grids.push(component_grids);
    }
    Ok(grids)
}

#[cfg(test)]
fn build_packet_positions(
    header: &CodestreamHeader,
    precinct_grids: &[Vec<PrecinctGrid>],
) -> Result<Vec<PacketPosition>> {
    let packet_count = packet_position_count(header, precinct_grids)?;
    let mut progression = PacketProgression::new(header, precinct_grids.to_vec(), packet_count)?;
    let mut packets = Vec::new();
    packets
        .try_reserve_exact(packet_count)
        .map_err(|_| invalid("packet-position test plan allocation failed"))?;
    while let Some(packet) = progression.next()? {
        packets.push(packet);
    }
    Ok(packets)
}

#[cfg(test)]
fn packet_position_count(
    header: &CodestreamHeader,
    precinct_grids: &[Vec<PrecinctGrid>],
) -> Result<usize> {
    precinct_position_count(header, precinct_grids)?
        .checked_mul(usize::from(header.cod.layers))
        .ok_or_else(|| invalid("packet-position count overflow"))
}

fn precinct_position_count(
    header: &CodestreamHeader,
    precinct_grids: &[Vec<PrecinctGrid>],
) -> Result<usize> {
    let mut precincts = 0usize;
    for resolution in 0..=header.cod.decomposition_levels {
        for component in 0..header.siz.components.len() {
            let grid = precinct_grids
                .get(component)
                .and_then(|grids| grids.get(usize::from(resolution)))
                .ok_or_else(|| invalid("packet plan references a missing precinct grid"))?;
            let precinct_count = grid
                .width
                .checked_mul(grid.height)
                .ok_or_else(|| invalid("precinct count overflow"))?;
            precincts = precincts
                .checked_add(precinct_count)
                .ok_or_else(|| invalid("packet-position count overflow"))?;
        }
    }
    Ok(precincts)
}

#[allow(clippy::too_many_arguments)]
fn next_lrcp(
    grids: &[Vec<PrecinctGrid>],
    layers: u16,
    max_resolution: u8,
    layer: &mut u16,
    resolution: &mut u8,
    component: &mut usize,
    precinct: &mut usize,
) -> Result<Option<PacketPosition>> {
    while *layer < layers {
        let grid = grids
            .get(*component)
            .and_then(|component_grids| component_grids.get(usize::from(*resolution)))
            .ok_or_else(|| invalid("LRCP packet plan references a missing precinct grid"))?;
        let precinct_count = grid
            .width
            .checked_mul(grid.height)
            .ok_or_else(|| invalid("precinct count overflow"))?;
        if *precinct < precinct_count {
            let packet = PacketPosition {
                layer: *layer,
                resolution: *resolution,
                component: *component,
                precinct: *precinct,
            };
            *precinct += 1;
            return Ok(Some(packet));
        }
        *precinct = 0;
        *component += 1;
        if *component == grids.len() {
            *component = 0;
            if *resolution == max_resolution {
                *resolution = 0;
                *layer += 1;
            } else {
                *resolution += 1;
            }
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn next_rlcp(
    grids: &[Vec<PrecinctGrid>],
    layers: u16,
    max_resolution: u8,
    resolution: &mut u8,
    layer: &mut u16,
    component: &mut usize,
    precinct: &mut usize,
) -> Result<Option<PacketPosition>> {
    while *resolution <= max_resolution {
        let grid = grids
            .get(*component)
            .and_then(|component_grids| component_grids.get(usize::from(*resolution)))
            .ok_or_else(|| invalid("RLCP packet plan references a missing precinct grid"))?;
        let precinct_count = grid
            .width
            .checked_mul(grid.height)
            .ok_or_else(|| invalid("precinct count overflow"))?;
        if *precinct < precinct_count {
            let packet = PacketPosition {
                layer: *layer,
                resolution: *resolution,
                component: *component,
                precinct: *precinct,
            };
            *precinct += 1;
            return Ok(Some(packet));
        }
        *precinct = 0;
        *component += 1;
        if *component == grids.len() {
            *component = 0;
            *layer += 1;
            if *layer == layers {
                *layer = 0;
                if *resolution == max_resolution {
                    *resolution = max_resolution.saturating_add(1);
                } else {
                    *resolution += 1;
                }
            }
        }
    }
    Ok(None)
}

fn spatial_packet_key(
    header: &CodestreamHeader,
    precinct_grids: &[Vec<PrecinctGrid>],
    order: ProgressionOrder,
    component: usize,
    resolution: u8,
    precinct: usize,
) -> Result<[u64; 4]> {
    let grid = *precinct_grids
        .get(component)
        .and_then(|grids| grids.get(usize::from(resolution)))
        .ok_or_else(|| invalid("spatial packet plan references a missing precinct grid"))?;
    let (position_x, position_y) = precinct_reference_position(header, grid, resolution, precinct)?;
    Ok(match order {
        ProgressionOrder::Rpcl => [
            u64::from(resolution),
            position_y,
            position_x,
            component as u64,
        ],
        ProgressionOrder::Pcrl => [
            position_y,
            position_x,
            component as u64,
            u64::from(resolution),
        ],
        ProgressionOrder::Cprl => [
            component as u64,
            position_y,
            position_x,
            u64::from(resolution),
        ],
        ProgressionOrder::Lrcp | ProgressionOrder::Rlcp => {
            unreachable!("spatial packet key requested for a linear progression")
        }
    })
}

fn precinct_reference_position(
    header: &CodestreamHeader,
    grid: PrecinctGrid,
    resolution: u8,
    precinct: usize,
) -> Result<(u64, u64)> {
    let x = precinct % grid.width;
    let y = precinct / grid.width;
    let precinct_x = u64::from(grid.start_x)
        .checked_add(x as u64)
        .and_then(|value| value.checked_shl(u32::from(grid.pp_x)))
        .ok_or_else(|| invalid("precinct reference x coordinate overflow"))?;
    let precinct_y = u64::from(grid.start_y)
        .checked_add(y as u64)
        .and_then(|value| value.checked_shl(u32::from(grid.pp_y)))
        .ok_or_else(|| invalid("precinct reference y coordinate overflow"))?;
    let scale = u32::from(header.cod.decomposition_levels - resolution);
    let reference_x = if precinct_x < u64::from(grid.resolution_bounds.x0) {
        u64::from(header.siz.x_origin)
    } else {
        precinct_x
            .checked_shl(scale)
            .ok_or_else(|| invalid("precinct reference x scaling overflow"))?
    };
    let reference_y = if precinct_y < u64::from(grid.resolution_bounds.y0) {
        u64::from(header.siz.y_origin)
    } else {
        precinct_y
            .checked_shl(scale)
            .ok_or_else(|| invalid("precinct reference y scaling overflow"))?
    };
    Ok((reference_x, reference_y))
}

fn build_band_states(
    header: &CodestreamHeader,
    precinct_grids: &[Vec<PrecinctGrid>],
    max_code_blocks: usize,
) -> Result<Vec<BandState>> {
    build_precinct_band_states(header, precinct_grids, max_code_blocks)
}

fn build_precinct_band_states(
    header: &CodestreamHeader,
    precinct_grids: &[Vec<PrecinctGrid>],
    max_code_blocks: usize,
) -> Result<Vec<BandState>> {
    let band_count = 1 + usize::from(header.cod.decomposition_levels) * 3;
    let mut bands = Vec::with_capacity(header.siz.components.len().saturating_mul(band_count));
    let mut remaining_code_blocks = max_code_blocks;
    for component in 0..header.siz.components.len() {
        let geometries = band_geometries(header, component)?;
        for &geometry in &geometries {
            let grid = *precinct_grids
                .get(component)
                .and_then(|grids| grids.get(usize::from(geometry.resolution)))
                .ok_or_else(|| invalid("subband references a missing precinct grid"))?;
            bands.push(build_precinct_band(
                header,
                component,
                geometry,
                grid,
                &mut remaining_code_blocks,
            )?);
        }
    }
    Ok(bands)
}

fn build_precinct_band(
    header: &CodestreamHeader,
    component: usize,
    geometry: BandGeometry,
    grid: PrecinctGrid,
    remaining_code_blocks: &mut usize,
) -> Result<BandState> {
    let band_pp_x = if geometry.resolution == 0 {
        grid.pp_x
    } else {
        grid.pp_x - 1
    };
    let band_pp_y = if geometry.resolution == 0 {
        grid.pp_y
    } else {
        grid.pp_y - 1
    };
    let precinct_width = 1u32 << band_pp_x;
    let precinct_height = 1u32 << band_pp_y;
    let code_block_width = header.cod.code_block_width.min(precinct_width);
    let code_block_height = header.cod.code_block_height.min(precinct_height);
    let precinct_count = grid
        .width
        .checked_mul(grid.height)
        .ok_or_else(|| invalid("subband precinct count overflow"))?;
    let mut blocks = Vec::new();
    let mut precincts = Vec::with_capacity(precinct_count);

    for precinct_index in 0..precinct_count {
        let precinct_x = precinct_index % grid.width;
        let precinct_y = precinct_index / grid.width;
        let cell_x0 = (u64::from(grid.start_x) + precinct_x as u64)
            .checked_shl(u32::from(band_pp_x))
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| invalid("subband precinct x coordinate overflow"))?;
        let cell_y0 = (u64::from(grid.start_y) + precinct_y as u64)
            .checked_shl(u32::from(band_pp_y))
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| invalid("subband precinct y coordinate overflow"))?;
        let cell = Rect {
            x0: cell_x0.max(geometry.bounds.x0),
            y0: cell_y0.max(geometry.bounds.y0),
            x1: cell_x0
                .saturating_add(precinct_width)
                .min(geometry.bounds.x1),
            y1: cell_y0
                .saturating_add(precinct_height)
                .min(geometry.bounds.y1),
        };
        let first_block = blocks.len();
        let mut columns = 0usize;
        let mut rows = 0usize;
        if cell.x0 < cell.x1 && cell.y0 < cell.y1 {
            let block_grid_x0 = cell.x0 / code_block_width;
            let block_grid_x1 = cell.x1.div_ceil(code_block_width);
            let block_grid_y0 = cell.y0 / code_block_height;
            let block_grid_y1 = cell.y1.div_ceil(code_block_height);
            columns = usize::try_from(block_grid_x1 - block_grid_x0)
                .map_err(|_| invalid("code-block column count exceeds usize"))?;
            rows = usize::try_from(block_grid_y1 - block_grid_y0)
                .map_err(|_| invalid("code-block row count exceeds usize"))?;
            let new_blocks = columns
                .checked_mul(rows)
                .ok_or_else(|| invalid("code-block count overflow"))?;
            if new_blocks > *remaining_code_blocks {
                return Err(invalid(format!(
                    "tile code-block count exceeds remaining decode budget {}",
                    *remaining_code_blocks
                )));
            }
            blocks
                .try_reserve(new_blocks)
                .map_err(|_| invalid("code-block state allocation failed"))?;
            *remaining_code_blocks -= new_blocks;
            for block_y in block_grid_y0..block_grid_y1 {
                let nominal_y0 = block_y * code_block_height;
                let y0 = nominal_y0.max(cell.y0).max(geometry.bounds.y0);
                let y1 = nominal_y0
                    .saturating_add(code_block_height)
                    .min(cell.y1)
                    .min(geometry.bounds.y1);
                for block_x in block_grid_x0..block_grid_x1 {
                    let nominal_x0 = block_x * code_block_width;
                    let x0 = nominal_x0.max(cell.x0).max(geometry.bounds.x0);
                    let x1 = nominal_x0
                        .saturating_add(code_block_width)
                        .min(cell.x1)
                        .min(geometry.bounds.x1);
                    blocks.push(BlockState {
                        x0: geometry.packed_x0 + (x0 - geometry.bounds.x0),
                        y0: geometry.packed_y0 + (y0 - geometry.bounds.y0),
                        x1: geometry.packed_x0 + (x1 - geometry.bounds.x0),
                        y1: geometry.packed_y0 + (y1 - geometry.bounds.y0),
                        included: false,
                        numlenbits: 3,
                        zero_bitplanes: None,
                    });
                }
            }
        }
        precincts.push(BandPrecinctState {
            block_indices: first_block..blocks.len(),
            inclusion: TagTreeReader::try_new(columns.max(1), rows.max(1))?,
            zero_bitplanes: TagTreeReader::try_new(columns.max(1), rows.max(1))?,
        });
    }

    Ok(BandState {
        component,
        resolution: geometry.resolution,
        band: geometry.band,
        blocks,
        precincts,
    })
}

fn resolution_bounds(header: &CodestreamHeader, component: usize) -> Result<Vec<Rect>> {
    let levels = header.cod.decomposition_levels;
    // Derive every resolution's bounds from the tile-component reference grid
    // (`ceil(tile / dx)`), so a subsampled chroma plane sizes its subbands from
    // its own half-resolution extent. For `dx == dy == 1` these equal the tile
    // bounds and the geometry is byte-identical to the non-subsampled path.
    let (tcx0, tcy0, tcx1, tcy1) = header.tile_component_bounds(component)?;
    let mut bounds = Vec::with_capacity(usize::from(levels) + 1);
    for resolution in 0..=levels {
        let scale = 1u32 << (levels - resolution);
        bounds.push(Rect {
            x0: tcx0.div_ceil(scale),
            y0: tcy0.div_ceil(scale),
            x1: tcx1.div_ceil(scale),
            y1: tcy1.div_ceil(scale),
        });
    }
    Ok(bounds)
}

fn band_geometries(header: &CodestreamHeader, component: usize) -> Result<Vec<BandGeometry>> {
    let levels = header.cod.decomposition_levels;
    // All subband partitioning is in the tile-component reference grid, so a
    // subsampled chroma plane's HL/LH/HH bounds shrink with its own extent.
    let (tcx0, tcy0, tcx1, tcy1) = header.tile_component_bounds(component)?;
    let bounds = resolution_bounds(header, component)?;
    let sizes = resolution_ladder(tcx0, tcy0, tcx1 - tcx0, tcy1 - tcy0, levels);
    let mut bands = Vec::with_capacity(1 + usize::from(levels) * 3);
    bands.push(BandGeometry {
        resolution: 0,
        band: BandOrientation::Ll,
        bounds: bounds[0],
        packed_x0: 0,
        packed_y0: 0,
    });

    let tile_x1 = tcx1;
    let tile_y1 = tcy1;
    for resolution in 1..=levels {
        let decomposition = levels - resolution + 1;
        let scale = 1i64 << decomposition;
        let high_offset = 1i64 << (decomposition - 1);
        let low_x = subband_axis_bounds(tcx0, tile_x1, scale, 0)?;
        let high_x = subband_axis_bounds(tcx0, tile_x1, scale, high_offset)?;
        let low_y = subband_axis_bounds(tcy0, tile_y1, scale, 0)?;
        let high_y = subband_axis_bounds(tcy0, tile_y1, scale, high_offset)?;
        let low_size = sizes[usize::from(resolution - 1)];
        bands.push(BandGeometry {
            resolution,
            band: BandOrientation::Hl,
            bounds: Rect {
                x0: high_x.0,
                y0: low_y.0,
                x1: high_x.1,
                y1: low_y.1,
            },
            packed_x0: low_size.0,
            packed_y0: 0,
        });
        bands.push(BandGeometry {
            resolution,
            band: BandOrientation::Lh,
            bounds: Rect {
                x0: low_x.0,
                y0: high_y.0,
                x1: low_x.1,
                y1: high_y.1,
            },
            packed_x0: 0,
            packed_y0: low_size.1,
        });
        bands.push(BandGeometry {
            resolution,
            band: BandOrientation::Hh,
            bounds: Rect {
                x0: high_x.0,
                y0: high_y.0,
                x1: high_x.1,
                y1: high_y.1,
            },
            packed_x0: low_size.0,
            packed_y0: low_size.1,
        });
    }
    Ok(bands)
}

/// Compute, per subband, the packed-coordinate coefficient window a region
/// decode must retain so the multi-level inverse DWT reproduces the region's
/// output pixels bit-exactly.
///
/// `region_*` is the requested region as an absolute rectangle in the reduced
/// tile-component reference grid (i.e. resolution `L' = header.cod.
/// decomposition_levels`, the reconstructed sample grid). Starting from that
/// finest-resolution window, the window is projected down one synthesis level
/// at a time (`x -> x / 2`) and grown by `margin` samples on every side per
/// level. `margin` must cover the inverse-lifting filter support (5/3: 1, 9/7:
/// 2 per lifting step) plus the half-sample offset between a resolution's
/// low-pass and high-pass coordinate systems; callers pass a conservative
/// value. Over-approximating only ever retains extra (correctly decoded) code
/// blocks, never fewer than the true support, so the region output stays
/// identical to the same pixels from a full decode.
pub(crate) fn region_band_windows(
    header: &CodestreamHeader,
    region_x0: u32,
    region_y0: u32,
    region_x1: u32,
    region_y1: u32,
    margin: u32,
) -> Result<Vec<RegionBandWindow>> {
    // Region decode over subsampled components is not supported; the caller
    // rejects it before reaching here. Component 0 (full resolution for the
    // supported layouts) drives the window geometry, identical to the tile grid
    // for every non-subsampled component.
    let geometries = band_geometries(header, 0)?;
    let levels = header.cod.decomposition_levels;
    let margin = i64::from(margin);

    // Per-resolution coefficient windows, each in that resolution's own
    // (absolute) low-coordinate space. `res_window[L']` is the requested region
    // on the reconstructed grid; each coarser level halves it (with margin).
    let mut res_window = vec![(0i64, 0i64, 0i64, 0i64); usize::from(levels) + 1];
    res_window[usize::from(levels)] = (
        i64::from(region_x0),
        i64::from(region_y0),
        i64::from(region_x1),
        i64::from(region_y1),
    );
    for r in (1..=levels).rev() {
        let cur = res_window[usize::from(r)];
        let low = (
            cur.0.div_euclid(2) - margin,
            cur.1.div_euclid(2) - margin,
            ceil_div_i64(cur.2, 2) + margin,
            ceil_div_i64(cur.3, 2) + margin,
        );
        res_window[usize::from(r) - 1] = low;
    }

    let mut windows = Vec::with_capacity(geometries.len());
    for geom in &geometries {
        // The LL band lives at resolution 0; every detail band at resolution `r`
        // shares the coordinate scale of resolution `r - 1` (both feed the `r`
        // synthesis), so it uses that window.
        let win = if geom.resolution == 0 {
            res_window[0]
        } else {
            res_window[usize::from(geom.resolution) - 1]
        };
        let bx0 = i64::from(geom.bounds.x0);
        let by0 = i64::from(geom.bounds.y0);
        let bx1 = i64::from(geom.bounds.x1);
        let by1 = i64::from(geom.bounds.y1);
        let cx0 = win.0.clamp(bx0, bx1);
        let cy0 = win.1.clamp(by0, by1);
        let cx1 = win.2.clamp(bx0, bx1);
        let cy1 = win.3.clamp(by0, by1);
        if cx0 < cx1 && cy0 < cy1 {
            windows.push(RegionBandWindow {
                resolution: geom.resolution,
                band: geom.band,
                x0: geom.packed_x0 + (cx0 - bx0) as u32,
                y0: geom.packed_y0 + (cy0 - by0) as u32,
                x1: geom.packed_x0 + (cx1 - bx0) as u32,
                y1: geom.packed_y0 + (cy1 - by0) as u32,
            });
        }
    }
    Ok(windows)
}

fn subband_axis_bounds(start: u32, end: u32, scale: i64, offset: i64) -> Result<(u32, u32)> {
    let start = ceil_div_i64(i64::from(start) - offset, scale);
    let end = ceil_div_i64(i64::from(end) - offset, scale);
    Ok((
        u32::try_from(start).map_err(|_| invalid("negative subband start coordinate"))?,
        u32::try_from(end).map_err(|_| invalid("negative subband end coordinate"))?,
    ))
}

fn ceil_div_i64(value: i64, divisor: i64) -> i64 {
    -(-value).div_euclid(divisor)
}

fn resolution_ladder(x0: u32, y0: u32, width: u32, height: u32, levels: u8) -> Vec<(u32, u32)> {
    crate::tiling::phase_resolution_sizes(
        x0 as usize,
        y0 as usize,
        width as usize,
        height as usize,
        levels,
    )
    .into_iter()
    .map(|(w, h)| (w as u32, h as u32))
    .collect()
}

fn floor_log2(v: u32) -> u32 {
    if v == 0 { 0 } else { 31 - v.leading_zeros() }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PacketBioReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    reg: u8,
    ct: u8,
    previous_was_ff: bool,
}

impl<'a> PacketBioReader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            reg: 0,
            ct: 0,
            previous_was_ff: false,
        }
    }

    pub(crate) fn read_bit(&mut self) -> Result<u32> {
        if self.ct == 0 {
            self.bytein()?;
        }
        self.ct -= 1;
        Ok(u32::from((self.reg >> self.ct) & 1))
    }

    pub(crate) fn read_bits(&mut self, count: u32) -> Result<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | self.read_bit()?;
        }
        Ok(value)
    }

    fn read_bits_usize(&mut self, count: u32) -> Result<usize> {
        let mut value = 0usize;
        for _ in 0..count {
            let bit = self.read_bit()? as usize;
            value = value
                .checked_mul(2)
                .and_then(|value| value.checked_add(bit))
                .ok_or_else(|| invalid("packet-header integer exceeds usize"))?;
        }
        Ok(value)
    }

    pub(crate) fn bytes_consumed(&self) -> usize {
        self.pos
    }

    /// Byte-align the reader at the end of a packet header (ISO/IEC 15444-1
    /// Annex B.10.1), mirroring OpenJPEG's `opj_bio_inalign`.
    ///
    /// The packet header is a bit-stuffed segment: the encoder inserts a stuff
    /// bit (a leading 0) after every 0xFF so a 0xFF byte is always followed by a
    /// byte whose top bit is clear. When the *last byte consumed* by the header
    /// is 0xFF, that trailing stuff byte belongs to the header and must be
    /// consumed before the packet body begins. Skipping it counts the header one
    /// byte short and slices the body one byte early, desynchronising every later
    /// packet of the tile (and leaving a spurious trailing byte). For headers not
    /// ending on 0xFF this is a no-op, which is why it only bites the rare packet
    /// whose header happens to terminate on 0xFF.
    fn inalign(&mut self) -> Result<()> {
        if self.reg == 0xff {
            self.bytein()?;
        }
        self.ct = 0;
        Ok(())
    }

    fn bytein(&mut self) -> Result<()> {
        let byte = *self
            .bytes
            .get(self.pos)
            .ok_or_else(|| invalid("packet header ended before requested bit"))?;
        self.pos += 1;
        self.reg = byte;
        self.ct = if self.previous_was_ff { 7 } else { 8 };
        self.previous_was_ff = byte == 0xff;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TagTreeReader {
    nodes: Vec<TgtNode>,
}

#[derive(Debug, Clone)]
struct TgtNode {
    parent: u32,
    value: i32,
    low: i32,
    known: bool,
}

impl TagTreeReader {
    #[cfg(test)]
    pub(crate) fn new(numleafsh: usize, numleafsv: usize) -> Self {
        Self::try_new(numleafsh, numleafsv).expect("valid tag-tree dimensions")
    }

    pub(crate) fn try_new(numleafsh: usize, numleafsv: usize) -> Result<Self> {
        if numleafsh == 0
            || numleafsv == 0
            || numleafsh > i32::MAX as usize
            || numleafsv > i32::MAX as usize
        {
            return Err(invalid("tag-tree dimensions are out of range"));
        }
        let mut nplh = [0i32; 32];
        let mut nplv = [0i32; 32];
        let mut numlvls = 0usize;
        let mut numnodes = 0usize;
        nplh[0] = numleafsh as i32;
        nplv[0] = numleafsv as i32;
        loop {
            let n = (nplh[numlvls] * nplv[numlvls]) as usize;
            nplh[numlvls + 1] = (nplh[numlvls] + 1) / 2;
            nplv[numlvls + 1] = (nplv[numlvls] + 1) / 2;
            numnodes = numnodes
                .checked_add(n)
                .ok_or_else(|| invalid("tag-tree node count overflow"))?;
            numlvls += 1;
            if n <= 1 {
                break;
            }
        }

        if numnodes > u32::MAX as usize {
            return Err(invalid("tag-tree node count exceeds u32 parent indices"));
        }
        let node = TgtNode {
            parent: TGT_NO_PARENT,
            value: i32::MAX,
            low: 0,
            known: false,
        };
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(numnodes)
            .map_err(|_| invalid("tag-tree node allocation failed"))?;
        nodes.resize(numnodes, node);
        link_parents(&mut nodes, numleafsh, numleafsv, numlvls, &nplh, &nplv);
        Ok(Self { nodes })
    }

    /// Decode whether `leafno` is included below `threshold`.
    ///
    /// Returns `true` when the decoded tag-tree value is less than the supplied
    /// threshold. The decoded value remains stored in the tree so later packets
    /// can continue from the Annex B.10.2 `low` state.
    pub(crate) fn decode(
        &mut self,
        bio: &mut PacketBioReader<'_>,
        leafno: usize,
        threshold: i32,
    ) -> Result<bool> {
        if leafno >= self.nodes.len() {
            return Err(invalid("tag-tree leaf index out of range"));
        }

        let mut stack = [0usize; 32];
        let mut depth = 0usize;
        let mut node_idx = leafno;
        while self.nodes[node_idx].parent != TGT_NO_PARENT {
            stack[depth] = node_idx;
            depth += 1;
            node_idx = self.nodes[node_idx].parent as usize;
        }

        let mut low = 0i32;
        loop {
            {
                let node = &mut self.nodes[node_idx];
                if low > node.low {
                    node.low = low;
                } else {
                    low = node.low;
                }
                while low < threshold && !node.known {
                    if bio.read_bit()? == 1 {
                        node.value = low;
                        node.known = true;
                    } else {
                        low += 1;
                    }
                }
                if node.known && node.value > low {
                    low = node.value;
                }
                node.low = low;
            }

            if depth == 0 {
                break;
            }
            depth -= 1;
            node_idx = stack[depth];
        }

        Ok(self.nodes[leafno].known && self.nodes[leafno].value < threshold)
    }

    pub(crate) fn value(&self, leafno: usize) -> Option<i32> {
        self.nodes
            .get(leafno)
            .and_then(|node| node.known.then_some(node.value))
    }
}

pub(crate) fn read_commacode(bio: &mut PacketBioReader<'_>) -> Result<u32> {
    let mut count = 0u32;
    while bio.read_bit()? == 1 {
        count = count
            .checked_add(1)
            .ok_or_else(|| invalid("comma-code length overflow"))?;
    }
    Ok(count)
}

pub(crate) fn read_numpasses(bio: &mut PacketBioReader<'_>) -> Result<u32> {
    // ISO/IEC 15444-1 Annex B.10.6, Table B-4.
    if bio.read_bit()? == 0 {
        return Ok(1);
    }
    if bio.read_bit()? == 0 {
        return Ok(2);
    }
    let next = bio.read_bits(2)?;
    if next != 0b11 {
        return Ok(3 + next);
    }
    let next = bio.read_bits(5)?;
    if next != 0b1_1111 {
        return Ok(6 + next);
    }
    Ok(37 + bio.read_bits(7)?)
}

fn link_parents(
    nodes: &mut [TgtNode],
    numleafsh: usize,
    numleafsv: usize,
    numlvls: usize,
    nplh: &[i32; 32],
    nplv: &[i32; 32],
) {
    let mut node_idx = 0usize;
    let mut l_parent_idx = numleafsh * numleafsv;
    let mut l_parent_idx0 = l_parent_idx;
    for i in 0..numlvls.saturating_sub(1) {
        let mut j = 0i32;
        while j < nplv[i] {
            let mut k = nplh[i];
            loop {
                k -= 1;
                if k < 0 {
                    break;
                }
                nodes[node_idx].parent = l_parent_idx as u32;
                node_idx += 1;
                k -= 1;
                if k >= 0 {
                    nodes[node_idx].parent = l_parent_idx as u32;
                    node_idx += 1;
                }
                l_parent_idx += 1;
            }
            if j & 1 != 0 || j == nplv[i] - 1 {
                l_parent_idx0 = l_parent_idx;
            } else {
                l_parent_idx = l_parent_idx0;
                l_parent_idx0 += nplh[i] as usize;
            }
            j += 1;
        }
    }
    if node_idx < nodes.len() {
        nodes[node_idx].parent = TGT_NO_PARENT;
    }
}

fn invalid(message: impl Into<String>) -> Jp2LamError {
    Jp2LamError::DecodeFailed(message.into())
}

#[cfg(test)]
mod tests {
    use super::{
        DecodedTilePackets, InlineValues, PacketAxis, PacketBioReader, PacketPosition,
        PacketProgression, PacketProgressionState, PrecinctGrid, TagTreeReader,
        TilePacketDecoder, band_geometries, build_band_states, build_packet_positions,
        build_precinct_grids, packet_axis_order, packet_position_count, parse_tile_part_payload,
        precinct_reference_position, read_commacode, read_numpasses,
    };
    use crate::decode::{
        CodSegment, CodeBlockStyle, CodestreamHeader, ComponentSiz, DecodeLimits, PrecinctSize,
        ProgressionOrder, QcdSegment, QuantizationStep, QuantizationStyle, SizSegment,
        WaveletTransform,
    };

    fn assert_packet_stats_match_records(decoded: &DecodedTilePackets<'_>) {
        assert_eq!(decoded.packet_stats.count, decoded.packets.len());
        assert_eq!(
            decoded.packet_stats.header_bytes,
            decoded.packets.iter().map(|packet| packet.header_len).sum()
        );
        assert_eq!(
            decoded.packet_stats.body_bytes,
            decoded.packets.iter().map(|packet| packet.body_len).sum()
        );
        assert_eq!(
            decoded.packet_stats.contribution_count,
            decoded
                .packets
                .iter()
                .map(|packet| packet.contribution_count)
                .sum()
        );
    }

    #[test]
    fn normal_decoder_packet_accounting_is_constant_size_at_default_limit() {
        let limits = DecodeLimits::default();
        assert_eq!(limits.max_packets, 16_000_000);
        let decoder = TilePacketDecoder::new_with_limits(&tiny_header(), false, 0, &limits)
            .expect("packet decoder");

        assert!(decoder.packet_records.is_none());
        assert_eq!(
            std::mem::size_of_val(&decoder.packet_stats),
            4 * std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn inline_values_spill_only_for_a_second_value() {
        let mut values = InlineValues::One(11u8);
        assert!(matches!(&values, InlineValues::One(11)));
        values.try_push(22).expect("spill");
        let InlineValues::Many(spilled) = values else {
            panic!("a second value must spill inline storage");
        };
        assert_eq!(spilled, [11, 22]);
    }

    #[test]
    fn packet_axis_order_matches_annex_b12_under_single_precinct() {
        use PacketAxis::{Component, Layer, Resolution};
        // Axes are innermost (fastest) first; with one precinct per resolution
        // the progressions reduce to permutations of (L, R, C). PCRL and CPRL
        // coincide. See ISO/IEC 15444-1 Annex B.12.
        assert_eq!(
            packet_axis_order(ProgressionOrder::Lrcp),
            [Component, Resolution, Layer]
        );
        assert_eq!(
            packet_axis_order(ProgressionOrder::Rlcp),
            [Component, Layer, Resolution]
        );
        assert_eq!(
            packet_axis_order(ProgressionOrder::Rpcl),
            [Layer, Component, Resolution]
        );
        assert_eq!(
            packet_axis_order(ProgressionOrder::Pcrl),
            [Layer, Resolution, Component]
        );
        assert_eq!(
            packet_axis_order(ProgressionOrder::Cprl),
            [Layer, Resolution, Component]
        );
    }

    #[test]
    fn b6_b12_explicit_precinct_packet_plans_cover_every_position() {
        for order in [
            ProgressionOrder::Lrcp,
            ProgressionOrder::Rlcp,
            ProgressionOrder::Rpcl,
            ProgressionOrder::Pcrl,
            ProgressionOrder::Cprl,
        ] {
            let header = precinct_header(order);
            let grids = build_precinct_grids(&header).unwrap();
            assert!(
                grids
                    .iter()
                    .flatten()
                    .all(|grid| grid.width == 5 && grid.height == 4)
            );
            let packets = build_packet_positions(&header, &grids).unwrap();
            assert_eq!(packets.len(), 2 * 3 * 3 * 20);

            let mut unique = packets
                .iter()
                .map(|packet| {
                    (
                        packet.layer,
                        packet.resolution,
                        packet.component,
                        packet.precinct,
                    )
                })
                .collect::<Vec<_>>();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), packets.len(), "{order:?}");

            match order {
                ProgressionOrder::Lrcp => {
                    assert!(
                        packets[..20]
                            .iter()
                            .enumerate()
                            .all(|(p, packet)| packet.layer == 0
                                && packet.resolution == 0
                                && packet.component == 0
                                && packet.precinct == p)
                    );
                }
                ProgressionOrder::Rlcp => {
                    assert!(
                        packets[..20]
                            .iter()
                            .enumerate()
                            .all(|(p, packet)| packet.resolution == 0
                                && packet.layer == 0
                                && packet.component == 0
                                && packet.precinct == p)
                    );
                }
                ProgressionOrder::Rpcl => {
                    assert!(
                        packets[..6]
                            .iter()
                            .all(|packet| packet.resolution == 0 && packet.precinct == 0)
                    );
                }
                ProgressionOrder::Pcrl => {
                    assert_eq!(packets[0].component, 0);
                    assert_eq!(packets[6].component, 1);
                    assert_eq!(packets[6].precinct, 0);
                }
                ProgressionOrder::Cprl => {
                    assert!(packets[..120].iter().all(|packet| packet.component == 0));
                    assert_eq!(packets[120].component, 1);
                }
            }
        }
    }

    fn materialized_packet_oracle(
        header: &CodestreamHeader,
        grids: &[Vec<PrecinctGrid>],
    ) -> Vec<PacketPosition> {
        let mut keyed = Vec::new();
        for component in 0..header.siz.components.len() {
            for resolution in 0..=header.cod.decomposition_levels {
                let grid = grids[component][usize::from(resolution)];
                for precinct in 0..grid.width * grid.height {
                    let (x, y) =
                        precinct_reference_position(header, grid, resolution, precinct).unwrap();
                    for layer in 0..header.cod.layers {
                        let packet = PacketPosition {
                            layer,
                            resolution,
                            component,
                            precinct,
                        };
                        let key = match header.cod.progression_order {
                            ProgressionOrder::Lrcp => [
                                u64::from(layer),
                                u64::from(resolution),
                                component as u64,
                                precinct as u64,
                                0,
                                0,
                            ],
                            ProgressionOrder::Rlcp => [
                                u64::from(resolution),
                                u64::from(layer),
                                component as u64,
                                precinct as u64,
                                0,
                                0,
                            ],
                            ProgressionOrder::Rpcl => [
                                u64::from(resolution),
                                y,
                                x,
                                component as u64,
                                u64::from(layer),
                                precinct as u64,
                            ],
                            ProgressionOrder::Pcrl => [
                                y,
                                x,
                                component as u64,
                                u64::from(resolution),
                                u64::from(layer),
                                precinct as u64,
                            ],
                            ProgressionOrder::Cprl => [
                                component as u64,
                                y,
                                x,
                                u64::from(resolution),
                                u64::from(layer),
                                precinct as u64,
                            ],
                        };
                        keyed.push((key, packet));
                    }
                }
            }
        }
        keyed.sort_unstable_by_key(|(key, _)| *key);
        keyed.into_iter().map(|(_, packet)| packet).collect()
    }

    #[test]
    fn packet_iterators_match_materialized_oracle_for_all_progression_orders() {
        for order in [
            ProgressionOrder::Lrcp,
            ProgressionOrder::Rlcp,
            ProgressionOrder::Rpcl,
            ProgressionOrder::Pcrl,
            ProgressionOrder::Cprl,
        ] {
            let header = precinct_header(order);
            let grids = build_precinct_grids(&header).expect("precinct grids");
            let actual = build_packet_positions(&header, &grids).expect("packet iterator");
            let oracle = materialized_packet_oracle(&header, &grids);
            assert_eq!(actual, oracle, "{order:?}");
        }
    }

    #[test]
    fn spatial_progressions_keep_only_one_heap_entry_per_component_resolution() {
        for order in [
            ProgressionOrder::Rpcl,
            ProgressionOrder::Pcrl,
            ProgressionOrder::Cprl,
        ] {
            let header = precinct_header(order);
            let grids = build_precinct_grids(&header).expect("precinct grids");
            let total = packet_position_count(&header, &grids).expect("packet count");
            let progression =
                PacketProgression::new(&header, grids, total).expect("packet progression");
            let PacketProgressionState::Spatial { streams, heap, .. } = &progression.state else {
                panic!("{order:?} must use the spatial heap");
            };
            assert_eq!(streams.len(), 3 * 3);
            assert_eq!(heap.len(), streams.len());
            assert!(heap.len() * 10 < total);
        }
    }

    #[test]
    fn b6_b7_precinct_partition_covers_each_subband_once() {
        let header = precinct_header(ProgressionOrder::Lrcp);
        let grids = build_precinct_grids(&header).unwrap();
        let bands = build_band_states(&header, &grids, usize::MAX).unwrap();
        let geometries = band_geometries(&header, 0).unwrap();

        for (band, geometry) in bands[..geometries.len()].iter().zip(&geometries) {
            let expected_area = u64::from(geometry.bounds.x1 - geometry.bounds.x0)
                * u64::from(geometry.bounds.y1 - geometry.bounds.y0);
            let block_area = band
                .blocks
                .iter()
                .map(|block| u64::from(block.x1 - block.x0) * u64::from(block.y1 - block.y0))
                .sum::<u64>();
            assert_eq!(block_area, expected_area, "{:?}", geometry.band);
            assert_eq!(
                band.precincts
                    .iter()
                    .map(|precinct| precinct.block_indices.len())
                    .sum::<usize>(),
                band.blocks.len()
            );
            assert_eq!(
                std::mem::size_of_val(&band.precincts[0].block_indices),
                2 * std::mem::size_of::<usize>(),
                "contiguous precinct membership must remain a range"
            );
        }
    }

    #[test]
    fn packet_bio_reads_msb_first() {
        let mut bio = PacketBioReader::new(&[0b1010_0000]);
        assert_eq!(bio.read_bit().unwrap(), 1);
        assert_eq!(bio.read_bit().unwrap(), 0);
        assert_eq!(bio.read_bit().unwrap(), 1);
        assert_eq!(bio.read_bit().unwrap(), 0);
        assert_eq!(bio.bytes_consumed(), 1);
    }

    #[test]
    fn packet_bio_rejects_length_values_wider_than_usize() {
        let mut bytes = vec![0u8; usize::BITS.div_ceil(8) as usize + 1];
        bytes[0] = 0x80;
        let mut bio = PacketBioReader::new(&bytes);
        let error = bio
            .read_bits_usize(usize::BITS + 1)
            .expect_err("a set bit above usize width must be rejected");
        assert!(error.to_string().contains("exceeds usize"), "{error}");
    }

    #[test]
    fn packet_bio_skips_ff_stuffed_bit() {
        let mut bio = PacketBioReader::new(&[0xff, 0b1010_1010]);
        for _ in 0..8 {
            assert_eq!(bio.read_bit().unwrap(), 1);
        }
        // The MSB after an 0xff byte is a stuffed bit. The next data bits are
        // read from bit 6 downwards.
        assert_eq!(bio.read_bit().unwrap(), 0);
        assert_eq!(bio.read_bit().unwrap(), 1);
    }

    #[test]
    fn b1010_inalign_consumes_stuff_byte_after_trailing_ff() {
        // A packet header whose last consumed byte is 0xFF is followed by a
        // stuff byte (top bit clear) that the header realignment must consume
        // before the body (Annex B.10.1 / OpenJPEG opj_bio_inalign). Without
        // this, a header ending on 0xFF is one byte short and the body is
        // sliced a byte early, desynchronising the tile's remaining packets.
        let mut bio = PacketBioReader::new(&[0xff, 0x00, 0xab]);
        assert_eq!(bio.read_bit().unwrap(), 1); // fetches the 0xff byte
        assert_eq!(bio.bytes_consumed(), 1);
        bio.inalign().unwrap();
        assert_eq!(
            bio.bytes_consumed(),
            2,
            "the 0x00 stuff byte after 0xff must be consumed by the header"
        );
    }

    #[test]
    fn b1010_inalign_is_noop_when_header_does_not_end_on_ff() {
        // The common case: the final header byte is not 0xFF, so realignment
        // consumes nothing and the byte count is unchanged.
        let mut bio = PacketBioReader::new(&[0x80, 0xab]);
        assert_eq!(bio.read_bit().unwrap(), 1);
        assert_eq!(bio.bytes_consumed(), 1);
        bio.inalign().unwrap();
        assert_eq!(bio.bytes_consumed(), 1);
    }

    #[test]
    fn b107_commacode_reads_unary_one_bits_then_zero() {
        let mut bio = PacketBioReader::new(&[0b1110_0000]);
        assert_eq!(read_commacode(&mut bio).unwrap(), 3);
    }

    #[test]
    fn b106_numpasses_reads_table_b4_boundaries() {
        let cases = [
            (&[0b0000_0000][..], 1),
            (&[0b1000_0000][..], 2),
            (&[0b1100_0000][..], 3),
            (&[0b1110_0000][..], 5),
            (&[0b1111_0000, 0][..], 6),
            (&[0b1111_1111, 0b0100_0000, 0][..], 37),
            (&[0b1111_1111, 0b0111_1111, 0b1000_0000][..], 164),
        ];
        for (bytes, expected) in cases {
            let mut bio = PacketBioReader::new(bytes);
            assert_eq!(read_numpasses(&mut bio).unwrap(), expected);
        }
    }

    #[test]
    fn b102_tagtree_single_leaf_decodes_value_zero() {
        let mut tree = TagTreeReader::new(1, 1);
        let mut bio = PacketBioReader::new(&[0b1000_0000]);
        assert!(tree.decode(&mut bio, 0, 1).unwrap());
        assert_eq!(tree.value(0), Some(0));
    }

    #[test]
    fn b102_tagtree_single_leaf_decodes_later_threshold() {
        let mut tree = TagTreeReader::new(1, 1);
        let mut bio = PacketBioReader::new(&[0b0010_0000]);
        assert!(!tree.decode(&mut bio, 0, 1).unwrap());
        assert!(!tree.decode(&mut bio, 0, 2).unwrap());
        assert!(tree.decode(&mut bio, 0, 3).unwrap());
        assert_eq!(tree.value(0), Some(2));
    }

    #[test]
    fn b107_zero_length_contribution_is_preserved_for_mq_synthesis() {
        let header = tiny_header();
        let payload = [0b1110_0000];

        let decoded =
            parse_tile_part_payload(&header, &payload).expect("zero-byte contribution is valid");
        assert_packet_stats_match_records(&decoded);
        assert_eq!(decoded.packet_stats.contribution_count, 1);
        assert_eq!(decoded.codeblocks.len(), 1);
        assert_eq!(decoded.codeblocks[0].passes, 1);
        assert_eq!(decoded.codeblocks[0].segments.len(), 1);
        assert!(decoded.codeblocks[0].segments[0].data.is_empty());
    }

    #[test]
    fn b1072_termall_signals_one_length_per_terminated_pass() {
        let mut header = tiny_header();
        header.cod.code_block_style.terminate_each_pass = true;
        // Present, first inclusion, zero missing planes, two passes, unchanged
        // Lblock, then 3-bit segment lengths 1 and 2.
        let payload = [0b1111_0000, 0b1010_0000, 0xaa, 0xbb, 0xcc];

        let decoded = parse_tile_part_payload(&header, &payload).expect("TERMALL packet");
        assert_packet_stats_match_records(&decoded);
        assert_eq!(decoded.packet_stats.body_bytes, 3);
        assert_eq!(decoded.packet_stats.contribution_count, 1);
        assert_eq!(decoded.codeblocks.len(), 1);
        let block = &decoded.codeblocks[0];
        assert_eq!(block.passes, 2);
        assert_eq!(block.segments.len(), 2);
        assert_eq!(block.segments[0].passes, 1);
        assert_eq!(block.segments[0].data.as_ref(), &[0xaa]);
        assert_eq!(block.segments[1].passes, 1);
        assert_eq!(block.segments[1].data.as_ref(), &[0xbb, 0xcc]);
    }

    #[test]
    fn b107_unterminated_layers_concatenate_only_when_a_second_chunk_arrives() {
        let mut header = tiny_header();
        header.cod.layers = 2;
        // Layer 0 first-includes the block with one one-byte pass. Layer 1
        // adds one one-byte pass to the same unterminated MQ segment.
        let payload = [0b1110_0001, 0xaa, 0b1100_0010, 0xbb];

        let decoded = parse_tile_part_payload(&header, &payload).expect("two-layer packet body");
        assert_packet_stats_match_records(&decoded);
        assert_eq!(decoded.packet_stats.count, 2);
        assert_eq!(decoded.packet_stats.body_bytes, 2);
        assert_eq!(decoded.packet_stats.contribution_count, 2);
        let block = &decoded.codeblocks[0];
        assert_eq!(block.passes, 2);
        assert_eq!(block.segments.len(), 1);
        assert_eq!(block.segments[0].passes, 2);
        assert_eq!(block.segments[0].data.as_ref(), &[0xaa, 0xbb]);
    }

    #[test]
    fn a81_a82_sop_and_eph_delimit_an_empty_packet() {
        let mut header = tiny_header();
        header.cod.sop_markers = true;
        header.cod.eph_markers = true;
        let payload = [
            0xff, 0x91, 0x00, 0x04, 0x00, 0x00, // SOP, Lsop=4, Nsop=0
            0x00, // empty packet header
            0xff, 0x92, // EPH
        ];

        let decoded = parse_tile_part_payload(&header, &payload).expect("SOP/EPH packet");
        assert_packet_stats_match_records(&decoded);
        assert_eq!(decoded.packets.len(), 1);
        assert_eq!(decoded.packets[0].header_len, payload.len());
        assert_eq!(decoded.packets[0].body_len, 0);
    }

    #[test]
    fn a81_nsop_counts_packets_whose_optional_sop_is_omitted() {
        let mut header = tiny_header();
        header.cod.layers = 2;
        header.cod.sop_markers = true;
        header.cod.eph_markers = true;
        let payload = [
            0x00, 0xff, 0x92, // packet 0 omits its optional SOP
            0xff, 0x91, 0x00, 0x04, 0x00, 0x01, // packet 1 uses Nsop=1
            0x00, 0xff, 0x92,
        ];

        let decoded = parse_tile_part_payload(&header, &payload).expect("mixed SOP usage");
        assert_packet_stats_match_records(&decoded);
        assert_eq!(decoded.packets.len(), 2);
    }

    #[test]
    fn a82_missing_required_eph_is_rejected() {
        let mut header = tiny_header();
        header.cod.eph_markers = true;
        let err = parse_tile_part_payload(&header, &[0x00])
            .expect_err("EPH is mandatory when COD requests it")
            .to_string();
        assert!(err.contains("requires an EPH"), "{err}");
    }

    fn tiny_header() -> CodestreamHeader {
        CodestreamHeader {
            siz: SizSegment {
                rsiz: 0,
                width: 1,
                height: 1,
                x_origin: 0,
                y_origin: 0,
                tile_width: 1,
                tile_height: 1,
                tile_x_origin: 0,
                tile_y_origin: 0,
                components: vec![ComponentSiz {
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                }],
            },
            cod: CodSegment {
                progression_order: ProgressionOrder::Lrcp,
                layers: 1,
                use_mct: false,
                decomposition_levels: 0,
                code_block_width: 64,
                code_block_height: 64,
                code_block_style: CodeBlockStyle::default(),
                transform: WaveletTransform::Irreversible97,
                uses_precincts: false,
                sop_markers: false,
                eph_markers: false,
                precinct_sizes: Vec::new(),
            },
            qcd: QcdSegment {
                style: QuantizationStyle::ScalarExpounded,
                guard_bits: 1,
                steps: vec![QuantizationStep {
                    exponent: 8,
                    mantissa: 0,
                }],
            },
            qcc: Vec::new(),
            coc: Vec::new(),
            comment_count: 0,
        }
    }

    #[test]
    fn subsampled_420_sizes_each_component_from_its_own_reference_grid() {
        // 4:2:0 sYCC: luma (comp 0) full-resolution, chroma (comps 1, 2) at
        // half width AND half height. Each component's finest-resolution band
        // geometry must be sized from its tile-component rectangle
        // (ceil(dim / d)), so the chroma planes decode at half resolution.
        let mut header = tiny_header();
        header.siz.width = 40;
        header.siz.height = 24;
        header.siz.tile_width = 40;
        header.siz.tile_height = 24;
        header.cod.decomposition_levels = 2;
        header.siz.components = vec![
            ComponentSiz {
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            },
            ComponentSiz {
                precision: 8,
                signed: false,
                dx: 2,
                dy: 2,
            },
            ComponentSiz {
                precision: 8,
                signed: false,
                dx: 2,
                dy: 2,
            },
        ];
        header.qcd.steps = vec![
            QuantizationStep {
                exponent: 8,
                mantissa: 0,
            };
            7
        ];

        // Tile-component dims: luma 40x24, chroma 20x12.
        assert_eq!(header.tile_component_dims(0).unwrap(), (40, 24));
        assert_eq!(header.tile_component_dims(1).unwrap(), (20, 12));
        assert_eq!(header.tile_component_dims(2).unwrap(), (20, 12));

        // Every subband (LL at resolution 0 plus HL/LH/HH at each level)
        // partitions the packed coefficient plane, so the total band area equals
        // each component's own full extent: 40x24 for luma, 20x12 per chroma.
        for (component, (cw, ch)) in [(0usize, (40u32, 24u32)), (1, (20, 12)), (2, (20, 12))] {
            let geometries = band_geometries(&header, component).unwrap();
            let area: u32 = geometries
                .iter()
                .map(|g| (g.bounds.x1 - g.bounds.x0) * (g.bounds.y1 - g.bounds.y0))
                .sum();
            assert_eq!(area, cw * ch, "component {component} band area");
        }

        // The per-component precinct grids and band states build without the
        // whole tile being assumed full resolution.
        let grids = build_precinct_grids(&header).unwrap();
        let bands = build_band_states(&header, &grids, usize::MAX).unwrap();
        // 3 components x (1 + 3*levels) bands.
        assert_eq!(bands.len(), 3 * (1 + 3 * 2));
        let packets = build_packet_positions(&header, &grids).unwrap();
        assert!(!packets.is_empty());
    }

    fn precinct_header(order: ProgressionOrder) -> CodestreamHeader {
        let mut header = tiny_header();
        header.siz.width = 130;
        header.siz.height = 98;
        header.siz.tile_width = 130;
        header.siz.tile_height = 98;
        header.siz.components = vec![header.siz.components[0]; 3];
        header.cod.progression_order = order;
        header.cod.layers = 2;
        header.cod.decomposition_levels = 2;
        header.cod.code_block_width = 16;
        header.cod.code_block_height = 16;
        header.cod.uses_precincts = true;
        // COD stores precinct sizes from resolution zero (coarsest) to the
        // full-resolution precinct grid.
        header.cod.precinct_sizes = vec![
            PrecinctSize { pp_x: 3, pp_y: 3 },
            PrecinctSize { pp_x: 4, pp_y: 4 },
            PrecinctSize { pp_x: 5, pp_y: 5 },
        ];
        header.qcd.steps = vec![
            QuantizationStep {
                exponent: 8,
                mantissa: 0,
            };
            7
        ];
        header
    }
}
