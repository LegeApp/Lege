use crate::error::{Jp2LamError, Result};
use crate::t2::TilePartPayload;

use super::types::{CodestreamParts, TilePart, TilePartHeader};
use super::{MARKER_EOC, MARKER_SOC, MARKER_SOD, MARKER_SOT};

impl CodestreamParts {
    pub(crate) fn parse_single_tile(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        let soc = cursor.read_u16()?;
        if soc != MARKER_SOC {
            return Err(Jp2LamError::EncodeFailed(
                "backend codestream did not start with SOC".to_string(),
            ));
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
                            return Err(Jp2LamError::EncodeFailed(
                                "trailing bytes after EOC are unsupported".to_string(),
                            ));
                        }
                        return Ok(Self {
                            main_header_segments,
                            tile_parts,
                        });
                    }
                    if next_marker != MARKER_SOT {
                        return Err(Jp2LamError::EncodeFailed(format!(
                            "unexpected marker 0x{next_marker:04x} after tile-part payload"
                        )));
                    }
                    tile_parts.push(cursor.read_tile_part(next_start, next_marker)?);
                }

                if cursor.position() == bytes.len().saturating_sub(2) {
                    let eoc = cursor.read_u16()?;
                    if eoc == MARKER_EOC {
                        return Ok(Self {
                            main_header_segments,
                            tile_parts,
                        });
                    }
                    return Err(Jp2LamError::EncodeFailed(format!(
                        "expected EOC at end of codestream, found 0x{eoc:04x}"
                    )));
                }

                return Err(Jp2LamError::EncodeFailed(
                    "backend codestream ended before EOC".to_string(),
                ));
            }

            main_header_segments.push(cursor.read_segment(marker)?);
        }
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

    fn read_segment(&mut self, marker: u16) -> Result<Vec<u8>> {
        let length = self.read_u16()? as usize;
        if length < 2 {
            return Err(Jp2LamError::EncodeFailed(format!(
                "invalid marker length {} for marker 0x{marker:04x}",
                length
            )));
        }
        let body_len = length
            .checked_sub(2)
            .ok_or_else(|| Jp2LamError::EncodeFailed("marker length underflow".to_string()))?;
        let end = self
            .pos
            .checked_add(body_len)
            .ok_or_else(|| Jp2LamError::EncodeFailed("marker body length overflow".to_string()))?;
        if end > self.bytes.len() {
            return Err(Jp2LamError::EncodeFailed(format!(
                "marker 0x{marker:04x} extended past end of codestream"
            )));
        }
        let mut segment = Vec::with_capacity(2 + length);
        segment.extend_from_slice(&marker.to_be_bytes());
        segment.extend_from_slice(&(length as u16).to_be_bytes());
        segment.extend_from_slice(&self.bytes[self.pos..end]);
        self.pos = end;
        Ok(segment)
    }

    fn read_tile_part(&mut self, sot_start: usize, marker: u16) -> Result<TilePart> {
        let sot_segment = self.read_segment(marker)?;
        let psot = read_psot(&sot_segment)
            .ok_or_else(|| Jp2LamError::EncodeFailed("missing Psot in SOT segment".to_string()))?;
        if psot == 0 {
            return Err(Jp2LamError::EncodeFailed(
                "Psot=0 tile-parts are unsupported in v1".to_string(),
            ));
        }
        let tile_part_end = sot_start
            .checked_add(psot as usize)
            .ok_or_else(|| Jp2LamError::EncodeFailed("tile-part length overflow".to_string()))?;
        if tile_part_end > self.bytes.len() {
            return Err(Jp2LamError::EncodeFailed(
                "tile-part length exceeded codestream size".to_string(),
            ));
        }

        let mut header_segments = Vec::new();
        loop {
            let next_marker = self.read_u16()?;
            if next_marker == MARKER_SOD {
                let payload = self
                    .bytes
                    .get(self.pos..tile_part_end)
                    .ok_or_else(|| {
                        Jp2LamError::EncodeFailed(
                            "tile-part payload extended past codestream".to_string(),
                        )
                    })?
                    .to_vec();
                self.pos = tile_part_end;
                return Ok(TilePart {
                    header: read_tile_part_header(&sot_segment).ok_or_else(|| {
                        Jp2LamError::EncodeFailed(
                            "missing tile-part fields in SOT segment".to_string(),
                        )
                    })?,
                    header_segments,
                    payload: TilePartPayload::from_raw_bytes(payload),
                });
            }
            header_segments.push(self.read_segment(next_marker)?);
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
        .ok_or_else(|| Jp2LamError::EncodeFailed("u16 offset overflow".to_string()))?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| Jp2LamError::EncodeFailed("unexpected end of codestream".to_string()))?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}
