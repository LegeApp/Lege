//! Borrowed codestream framing parser for in-memory decoding.
//!
//! The encoder-facing `CodestreamParts` owns marker and payload bytes because
//! it can re-emit them. The decoder only needs to inspect marker segments and
//! read packet payload bytes, so this view keeps those regions borrowed from
//! the caller's input buffer.

use crate::error::{Jp2LamError, Result};
use crate::j2k::{MARKER_EOC, MARKER_SOC, MARKER_SOD, MARKER_SOT, TilePartHeader};

#[derive(Debug, Clone)]
pub(crate) struct CodestreamView<'a> {
    pub(crate) main_header_segments: Vec<&'a [u8]>,
    pub(crate) tile_parts: Vec<TilePartView<'a>>,
}

#[derive(Debug, Clone)]
pub(crate) struct TilePartView<'a> {
    pub(crate) header: TilePartHeader,
    #[allow(dead_code)]
    pub(crate) header_segments: Vec<&'a [u8]>,
    pub(crate) payload: &'a [u8],
}

pub(crate) fn parse_codestream_view(bytes: &[u8]) -> Result<CodestreamView<'_>> {
    let mut cursor = Cursor::new(bytes);
    let soc = cursor.read_u16()?;
    if soc != MARKER_SOC {
        return Err(invalid("codestream did not start with SOC"));
    }

    let mut main_header_segments = Vec::new();
    loop {
        let marker_start = cursor.position();
        let marker = cursor.read_u16()?;
        if marker == MARKER_SOT {
            let mut tile_parts = Vec::new();
            tile_parts.push(cursor.read_tile_part(marker_start, marker)?);

            while cursor.position() < bytes.len().saturating_sub(2) {
                let next_start = cursor.position();
                let next_marker = cursor.read_u16()?;
                if next_marker == MARKER_EOC {
                    if cursor.position() != bytes.len() {
                        return Err(invalid("trailing bytes after EOC are unsupported"));
                    }
                    return Ok(CodestreamView {
                        main_header_segments,
                        tile_parts,
                    });
                }
                if next_marker != MARKER_SOT {
                    return Err(invalid(format!(
                        "unexpected marker 0x{next_marker:04x} after tile-part payload"
                    )));
                }
                tile_parts.push(cursor.read_tile_part(next_start, next_marker)?);
            }

            if cursor.position() == bytes.len().saturating_sub(2) {
                let eoc = cursor.read_u16()?;
                if eoc == MARKER_EOC {
                    return Ok(CodestreamView {
                        main_header_segments,
                        tile_parts,
                    });
                }
                return Err(invalid(format!(
                    "expected EOC at end of codestream, found 0x{eoc:04x}"
                )));
            }

            return Err(invalid("codestream ended before EOC"));
        }

        main_header_segments.push(cursor.read_segment(marker_start, marker)?);
    }
}

#[derive(Debug, Clone, Copy)]
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn position(&self) -> usize {
        self.pos
    }

    fn read_u16(&mut self) -> Result<u16> {
        let value = read_be_u16(self.bytes, self.pos)?;
        self.pos += 2;
        Ok(value)
    }

    fn read_segment(&mut self, marker_start: usize, marker: u16) -> Result<&'a [u8]> {
        let length = self.read_u16()? as usize;
        if length < 2 {
            return Err(invalid(format!(
                "invalid marker length {length} for marker 0x{marker:04x}"
            )));
        }
        let body_len = length
            .checked_sub(2)
            .ok_or_else(|| invalid("marker length underflow"))?;
        let end = self
            .pos
            .checked_add(body_len)
            .ok_or_else(|| invalid("marker body length overflow"))?;
        if end > self.bytes.len() {
            return Err(invalid(format!(
                "marker 0x{marker:04x} extended past end of codestream"
            )));
        }
        self.pos = end;
        self.bytes
            .get(marker_start..end)
            .ok_or_else(|| invalid("marker segment slice out of bounds"))
    }

    fn read_tile_part(&mut self, sot_start: usize, marker: u16) -> Result<TilePartView<'a>> {
        let sot_segment = self.read_segment(sot_start, marker)?;
        let psot = read_psot(sot_segment).ok_or_else(|| invalid("missing Psot in SOT segment"))?;
        if psot == 0 {
            return Err(invalid("Psot=0 tile-parts are unsupported"));
        }
        let tile_part_end = sot_start
            .checked_add(psot as usize)
            .ok_or_else(|| invalid("tile-part length overflow"))?;
        if tile_part_end > self.bytes.len() {
            return Err(invalid("tile-part length exceeded codestream size"));
        }

        let mut header_segments = Vec::new();
        loop {
            let marker_start = self.position();
            let next_marker = self.read_u16()?;
            if next_marker == MARKER_SOD {
                let payload = self
                    .bytes
                    .get(self.pos..tile_part_end)
                    .ok_or_else(|| invalid("tile-part payload extended past codestream"))?;
                self.pos = tile_part_end;
                return Ok(TilePartView {
                    header: read_tile_part_header(sot_segment)
                        .ok_or_else(|| invalid("missing tile-part fields in SOT segment"))?,
                    header_segments,
                    payload,
                });
            }
            header_segments.push(self.read_segment(marker_start, next_marker)?);
        }
    }
}

fn read_psot(sot_segment: &[u8]) -> Option<u32> {
    if sot_segment.len() < 12 {
        return None;
    }
    Some(u32::from_be_bytes([
        sot_segment[6],
        sot_segment[7],
        sot_segment[8],
        sot_segment[9],
    ]))
}

fn read_tile_part_header(sot_segment: &[u8]) -> Option<TilePartHeader> {
    if sot_segment.len() < 12 {
        return None;
    }
    Some(TilePartHeader {
        tile_index: u16::from_be_bytes([sot_segment[4], sot_segment[5]]),
        part_index: sot_segment[10],
        total_parts: sot_segment[11],
    })
}

fn read_be_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| invalid("u16 offset overflow"))?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("unexpected end of codestream"))?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

fn invalid(message: impl Into<String>) -> Jp2LamError {
    Jp2LamError::DecodeFailed(message.into())
}
