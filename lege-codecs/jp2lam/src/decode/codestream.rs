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
        // Psot is the one field encoders reliably get wrong, and A.4.2 already
        // blesses one form of "wrong": the final tile-part may declare Psot=0,
        // meaning it runs to the EOC. `0031730.pdf` declares Psot=12 on a
        // tile-part carrying 3.4 KiB — 12 is the length of the SOT segment
        // alone, and no tile-part containing data can be shorter than
        // MIN_TILE_PART_LEN. A declared end past the buffer is the same class of
        // defect seen from the other side.
        //
        // In every one of those cases the declared end is unusable while the
        // data itself is intact, so fall back to where the tile-part demonstrably
        // ends. This is not a guess about damaged data: it is reading the
        // framing from the codestream instead of from a field that contradicts
        // it. PDFium, MuPDF and hayro all render `0031730`; we alone rendered it
        // blank.
        let declared_end = match psot as usize {
            0 => None,
            len if len < MIN_TILE_PART_LEN => None,
            len => sot_start
                .checked_add(len)
                .filter(|end| *end <= self.bytes.len()),
        };

        let mut header_segments = Vec::new();
        loop {
            let marker_start = self.position();
            let next_marker = self.read_u16()?;
            if next_marker == MARKER_SOD {
                // `self.pos` is past the SOD, so a declared end at or before it
                // describes a tile-part that cannot hold the data that follows.
                let tile_part_end = match declared_end {
                    Some(end) if end >= self.pos => end,
                    _ => next_tile_part_boundary(self.bytes, self.pos),
                };
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

/// Smallest `Psot` that can describe a tile-part carrying data: the 12-byte SOT
/// marker segment plus the two-byte SOD that must follow it.
const MIN_TILE_PART_LEN: usize = 14;

/// Find where the current tile-part actually ends, for use when `Psot` does not
/// say.
///
/// Scanning entropy-coded data for markers is safe here specifically because of
/// Annex B bit stuffing: the byte following any `0xFF` inside a packet body is
/// at most `0x8F`, so `0xFF90` (SOT) and `0xFFD9` (EOC) cannot occur except as
/// real markers. Returns the buffer length when neither is found, which leaves
/// the caller's "codestream ended before EOC" check to reject a truncated file.
fn next_tile_part_boundary(bytes: &[u8], from: usize) -> usize {
    let mut index = from;
    while index + 1 < bytes.len() {
        if bytes[index] == 0xFF {
            let marker = u16::from_be_bytes([bytes[index], bytes[index + 1]]);
            if marker == MARKER_SOT || marker == MARKER_EOC {
                return index;
            }
        }
        index += 1;
    }
    bytes.len()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// SOC, one COD-shaped main-header segment, then a single tile-part whose
    /// `Psot` is `psot`, carrying `payload`, then EOC.
    fn codestream(psot: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MARKER_SOC.to_be_bytes());
        // A minimal main-header marker segment (length 4: itself plus 2 bytes).
        bytes.extend_from_slice(&0xff5cu16.to_be_bytes());
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.extend_from_slice(&[0x00, 0x00]);
        // SOT: Lsot=10, Isot=0, Psot, TPsot=0, TNsot=1.
        bytes.extend_from_slice(&MARKER_SOT.to_be_bytes());
        bytes.extend_from_slice(&10u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&psot.to_be_bytes());
        bytes.extend_from_slice(&[0x00, 0x01]);
        bytes.extend_from_slice(&MARKER_SOD.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&MARKER_EOC.to_be_bytes());
        bytes
    }

    /// Offset of the SOT marker in the buffer `codestream` builds.
    const SOT_START: usize = 8;

    #[test]
    fn honest_psot_frames_the_payload() {
        let payload = [0x11, 0x22, 0x33, 0x44];
        // SOT segment (12) + SOD (2) + payload.
        let psot = (12 + 2 + payload.len()) as u32;
        let bytes = codestream(psot, &payload);
        let view = parse_codestream_view(&bytes).expect("well-formed codestream");
        assert_eq!(view.tile_parts.len(), 1);
        assert_eq!(view.tile_parts[0].payload, &payload);
    }

    /// `0031730.pdf` declares `Psot=12` — the length of the SOT segment alone —
    /// on a tile-part carrying 3.4 KiB. OpenJPEG calls this an "Empty SOT
    /// marker"; PDFium, MuPDF and hayro all decode the image anyway, while we
    /// rejected the whole codestream and rendered the page blank.
    #[test]
    fn psot_shorter_than_the_sot_segment_falls_back_to_the_eoc() {
        let payload = [0xAA, 0xBB, 0xCC];
        let bytes = codestream(12, &payload);
        let view = parse_codestream_view(&bytes).expect("Psot=12 must be recovered");
        assert_eq!(view.tile_parts[0].payload, &payload);
    }

    /// A.4.2: the last tile-part may declare `Psot=0`, meaning "to the EOC".
    #[test]
    fn zero_psot_runs_to_the_eoc() {
        let payload = [0x01, 0x02, 0x03, 0x04, 0x05];
        let bytes = codestream(0, &payload);
        let view = parse_codestream_view(&bytes).expect("Psot=0 is legal per A.4.2");
        assert_eq!(view.tile_parts[0].payload, &payload);
    }

    /// A `Psot` reaching past the buffer is the same defect from the other
    /// side; the data present is still framed by the EOC.
    #[test]
    fn psot_past_the_buffer_falls_back_to_the_eoc() {
        let payload = [0x77; 6];
        let bytes = codestream(9_999, &payload);
        let view = parse_codestream_view(&bytes).expect("over-long Psot must be recovered");
        assert_eq!(view.tile_parts[0].payload, &payload);
    }

    /// The recovery must not swallow a following tile-part.
    #[test]
    fn recovery_stops_at_the_next_sot() {
        let first = [0x10, 0x20];
        let mut bytes = codestream(12, &first);
        // Splice a second, well-formed tile-part in before the EOC.
        let eoc = bytes.len() - 2;
        bytes.truncate(eoc);
        let second = [0x30, 0x40, 0x50];
        bytes.extend_from_slice(&MARKER_SOT.to_be_bytes());
        bytes.extend_from_slice(&10u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&((12 + 2 + second.len()) as u32).to_be_bytes());
        bytes.extend_from_slice(&[0x01, 0x02]);
        bytes.extend_from_slice(&MARKER_SOD.to_be_bytes());
        bytes.extend_from_slice(&second);
        bytes.extend_from_slice(&MARKER_EOC.to_be_bytes());

        let view = parse_codestream_view(&bytes).expect("two tile-parts");
        assert_eq!(view.tile_parts.len(), 2);
        assert_eq!(view.tile_parts[0].payload, &first);
        assert_eq!(view.tile_parts[1].payload, &second);
    }

    /// Bit stuffing (Annex B) guarantees the byte after `0xFF` in a packet body
    /// is at most `0x8F`, so scanning for `0xFF90`/`0xFFD9` cannot trip over
    /// entropy-coded data. Payload bytes that merely *contain* `0xFF` are fine.
    #[test]
    fn stuffed_ff_bytes_in_the_payload_do_not_end_the_tile_part() {
        let payload = [0xFF, 0x7F, 0xFF, 0x00, 0xFF, 0x8F];
        let bytes = codestream(12, &payload);
        let view = parse_codestream_view(&bytes).expect("stuffed payload must survive");
        assert_eq!(view.tile_parts[0].payload, &payload);
    }

    #[test]
    fn sot_start_matches_the_fixture_layout() {
        let bytes = codestream(0, &[]);
        assert_eq!(
            u16::from_be_bytes([bytes[SOT_START], bytes[SOT_START + 1]]),
            MARKER_SOT
        );
    }
}
