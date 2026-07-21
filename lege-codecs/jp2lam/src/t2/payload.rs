use super::packet::{Packet, PacketSequence};
use crate::encode::block_store::{PayloadRef, SharedEncodedBlockStore};
use crate::error::Jp2LamError;
use crate::error::Result;
use std::io::Write;

#[derive(Debug, Clone)]
pub(crate) enum TilePartPayload {
    PacketSequence(PacketSequence),
    StoredPacketSequence {
        store: SharedEncodedBlockStore,
        packets: Vec<StoredPacket>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct StoredPacket {
    pub header: Vec<u8>,
    pub body: Vec<StoredPayloadRange>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StoredPayloadRange {
    pub payload: PayloadRef,
    pub len: usize,
}

impl TilePartPayload {
    pub(crate) fn from_raw_bytes(bytes: Vec<u8>) -> Self {
        Self::PacketSequence(PacketSequence::from_opaque_bytes(bytes))
    }

    pub(crate) fn from_packet_sequence(sequence: PacketSequence) -> Self {
        Self::PacketSequence(sequence)
    }

    pub(crate) fn from_stored_packets(
        store: SharedEncodedBlockStore,
        packets: Vec<StoredPacket>,
    ) -> Self {
        Self::StoredPacketSequence { store, packets }
    }

    #[allow(dead_code)]
    pub(crate) fn from_packets(packets: Vec<Packet>) -> Self {
        Self::from_packet_sequence(PacketSequence::from_packets(packets))
    }

    pub(crate) fn byte_len(&self) -> usize {
        match self {
            Self::PacketSequence(sequence) => sequence.byte_len(),
            Self::StoredPacketSequence { packets, .. } => packets
                .iter()
                .map(|packet| {
                    packet.header.len()
                        + packet.body.iter().map(|range| range.len).sum::<usize>()
                })
                .sum(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn packet_count(&self) -> usize {
        match self {
            Self::PacketSequence(sequence) => sequence.packet_count(),
            Self::StoredPacketSequence { packets, .. } => packets.len(),
        }
    }

    pub(crate) fn write_to(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::PacketSequence(sequence) => {
                sequence.write_to(out);
                Ok(())
            }
            Self::StoredPacketSequence { .. } => self.write_to_writer(out),
        }
    }

    pub(crate) fn write_to_writer<W: Write>(&self, writer: &mut W) -> Result<()> {
        match self {
            Self::PacketSequence(sequence) => sequence.write_to_writer(writer),
            Self::StoredPacketSequence { store, packets } => {
                let mut store = store.lock().map_err(|_| {
                    Jp2LamError::EncodeFailed("encoded block store lock was poisoned".into())
                })?;
                for packet in packets {
                    writer.write_all(&packet.header).map_err(|error| {
                        Jp2LamError::EncodeFailed(format!("packet header write failed: {error}"))
                    })?;
                    for range in &packet.body {
                        store.write_prefix_to(range.payload, range.len, writer)?;
                    }
                }
                Ok(())
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.byte_len());
        self.write_to(&mut out)
            .expect("writing a tile payload to Vec should not fail");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::TilePartPayload;
    use crate::t2::Packet;

    #[test]
    fn payload_from_packets_preserves_header_body_order() {
        let payload = TilePartPayload::from_packets(vec![
            Packet::header_body(vec![0x01, 0x02], vec![0xa0]),
            Packet::opaque(vec![0xbb, 0xcc]),
        ]);
        let mut out = Vec::new();
        payload.write_to(&mut out).expect("write payload");
        assert_eq!(payload.packet_count(), 2);
        assert_eq!(payload.byte_len(), 5);
        assert_eq!(out, vec![0x01, 0x02, 0xa0, 0xbb, 0xcc]);
    }
}
