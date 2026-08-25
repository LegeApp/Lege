use super::packet::{Packet, PacketSequence};
use super::payload::TilePartPayload;

#[derive(Debug, Default)]
pub(crate) struct PacketSequenceBuilder {
    packets: Vec<Packet>,
}

impl PacketSequenceBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn push_opaque_packet(mut self, bytes: Vec<u8>) -> Self {
        self.packets.push(Packet::opaque(bytes));
        self
    }

    #[cfg(test)]
    pub(crate) fn push_header_body_packet(mut self, header: Vec<u8>, body: Vec<u8>) -> Self {
        self.packets.push(Packet::header_body(header, body));
        self
    }

    pub(crate) fn push_header_body_segments(
        mut self,
        header: Vec<u8>,
        body_segments: Vec<Vec<u8>>,
    ) -> Self {
        self.packets
            .push(Packet::header_body_segments(header, body_segments));
        self
    }

    pub(crate) fn finish(self) -> PacketSequence {
        PacketSequence::from_packets(self.packets)
    }

    pub(crate) fn finish_payload(self) -> TilePartPayload {
        TilePartPayload::from_packet_sequence(self.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::PacketSequenceBuilder;

    #[test]
    fn builder_constructs_mixed_packet_sequence() {
        let payload = PacketSequenceBuilder::new()
            .push_header_body_packet(vec![0x01], vec![0xa0, 0xa1])
            .push_opaque_packet(vec![0xbe, 0xef])
            .finish_payload();
        let mut out = Vec::new();
        payload.write_to(&mut out).expect("write payload");
        assert_eq!(payload.packet_count(), 2);
        assert_eq!(payload.byte_len(), 5);
        assert_eq!(out, vec![0x01, 0xa0, 0xa1, 0xbe, 0xef]);
    }

    #[test]
    fn builder_constructs_segmented_body_packet() {
        let payload = PacketSequenceBuilder::new()
            .push_header_body_segments(vec![0x01], vec![vec![0xa0], vec![0xa1, 0xa2]])
            .finish_payload();
        let mut out = Vec::new();
        payload.write_to(&mut out).expect("write payload");
        assert_eq!(payload.packet_count(), 1);
        assert_eq!(payload.byte_len(), 4);
        assert_eq!(out, vec![0x01, 0xa0, 0xa1, 0xa2]);
    }
}
