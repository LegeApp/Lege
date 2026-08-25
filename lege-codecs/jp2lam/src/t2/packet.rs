use crate::error::{Jp2LamError, Result};
use std::io::Write;

#[derive(Debug, Clone)]
pub(crate) struct PacketBytes {
    bytes: Vec<u8>,
}

impl PacketBytes {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PacketEncoding {
    Opaque(PacketBytes),
    /// Only ever constructed by [`Packet::header_body`], which is itself
    /// `#[cfg(test)]`-gated — production always uses `HeaderBodySegments`
    /// (`Packet::header_body_segments`).
    #[cfg(test)]
    HeaderBody {
        header: PacketBytes,
        body: PacketBytes,
    },
    HeaderBodySegments {
        header: PacketBytes,
        body_segments: Vec<PacketBytes>,
    },
}

impl PacketEncoding {
    pub(crate) fn byte_len(&self) -> usize {
        match self {
            Self::Opaque(bytes) => bytes.len(),
            #[cfg(test)]
            Self::HeaderBody { header, body } => header.len() + body.len(),
            Self::HeaderBodySegments {
                header,
                body_segments,
            } => header.len() + body_segments.iter().map(PacketBytes::len).sum::<usize>(),
        }
    }

    pub(crate) fn write_to(&self, out: &mut Vec<u8>) {
        match self {
            Self::Opaque(bytes) => out.extend_from_slice(bytes.as_slice()),
            #[cfg(test)]
            Self::HeaderBody { header, body } => {
                out.extend_from_slice(header.as_slice());
                out.extend_from_slice(body.as_slice());
            }
            Self::HeaderBodySegments {
                header,
                body_segments,
            } => {
                out.extend_from_slice(header.as_slice());
                for segment in body_segments {
                    out.extend_from_slice(segment.as_slice());
                }
            }
        }
    }

    pub(crate) fn write_to_writer<W: Write>(&self, writer: &mut W) -> Result<()> {
        match self {
            Self::Opaque(bytes) => write_all(writer, bytes.as_slice()),
            #[cfg(test)]
            Self::HeaderBody { header, body } => {
                write_all(writer, header.as_slice())?;
                write_all(writer, body.as_slice())
            }
            Self::HeaderBodySegments {
                header,
                body_segments,
            } => {
                write_all(writer, header.as_slice())?;
                for segment in body_segments {
                    write_all(writer, segment.as_slice())?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Packet {
    encoding: PacketEncoding,
}

impl Packet {
    pub(crate) fn opaque(bytes: Vec<u8>) -> Self {
        Self {
            encoding: PacketEncoding::Opaque(PacketBytes::new(bytes)),
        }
    }

    #[cfg(test)]
    pub(crate) fn header_body(header: Vec<u8>, body: Vec<u8>) -> Self {
        Self {
            encoding: PacketEncoding::HeaderBody {
                header: PacketBytes::new(header),
                body: PacketBytes::new(body),
            },
        }
    }

    pub(crate) fn header_body_segments(header: Vec<u8>, body_segments: Vec<Vec<u8>>) -> Self {
        Self {
            encoding: PacketEncoding::HeaderBodySegments {
                header: PacketBytes::new(header),
                body_segments: body_segments.into_iter().map(PacketBytes::new).collect(),
            },
        }
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.encoding.byte_len()
    }

    pub(crate) fn write_to(&self, out: &mut Vec<u8>) {
        self.encoding.write_to(out);
    }

    pub(crate) fn write_to_writer<W: Write>(&self, writer: &mut W) -> Result<()> {
        self.encoding.write_to_writer(writer)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PacketSequence {
    packets: Vec<Packet>,
}

impl PacketSequence {
    pub(crate) fn from_packets(packets: Vec<Packet>) -> Self {
        Self { packets }
    }

    pub(crate) fn from_opaque_bytes(bytes: Vec<u8>) -> Self {
        Self::from_packets(vec![Packet::opaque(bytes)])
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.packets.iter().map(Packet::byte_len).sum()
    }

    #[cfg(test)]
    pub(crate) fn packet_count(&self) -> usize {
        self.packets.len()
    }

    pub(crate) fn write_to(&self, out: &mut Vec<u8>) {
        for packet in &self.packets {
            packet.write_to(out);
        }
    }

    pub(crate) fn write_to_writer<W: Write>(&self, writer: &mut W) -> Result<()> {
        for packet in &self.packets {
            packet.write_to_writer(writer)?;
        }
        Ok(())
    }
}

fn write_all<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    writer
        .write_all(bytes)
        .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{Packet, PacketBytes, PacketEncoding, PacketSequence};

    #[test]
    fn packet_sequence_preserves_opaque_bytes() {
        let seq = PacketSequence::from_packets(vec![
            Packet::opaque(vec![0xde, 0xad]),
            Packet::opaque(vec![0xbe, 0xef]),
        ]);
        let mut out = Vec::new();
        seq.write_to(&mut out);
        assert_eq!(seq.packet_count(), 2);
        assert_eq!(seq.byte_len(), 4);
        assert_eq!(out, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn header_body_packet_writes_in_order() {
        let packet = PacketEncoding::HeaderBody {
            header: PacketBytes::new(vec![0x01, 0x02]),
            body: PacketBytes::new(vec![0xa0, 0xb0, 0xc0]),
        };
        let mut out = Vec::new();
        packet.write_to(&mut out);
        assert_eq!(packet.byte_len(), 5);
        assert_eq!(out, vec![0x01, 0x02, 0xa0, 0xb0, 0xc0]);
    }

    #[test]
    fn header_body_segments_write_without_concatenating_body_first() {
        let packet = Packet::header_body_segments(vec![0x01], vec![vec![0xa0], vec![0xb0, 0xc0]]);
        let mut out = Vec::new();
        packet.write_to(&mut out);
        assert_eq!(packet.byte_len(), 4);
        assert_eq!(out, vec![0x01, 0xa0, 0xb0, 0xc0]);
    }
}
