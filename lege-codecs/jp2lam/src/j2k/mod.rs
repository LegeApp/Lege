pub(crate) mod decode_markers;
mod markers;
mod parse;
mod types;
mod write;

use crate::error::Result;
use crate::plan::EncodingPlan;
pub(crate) use types::{CodestreamParts, TilePart, TilePartHeader};

pub(crate) const MARKER_SOC: u16 = 0xff4f;
pub(crate) const MARKER_SOT: u16 = 0xff90;
pub(crate) const MARKER_SOD: u16 = 0xff93;
pub(crate) const MARKER_EOC: u16 = 0xffd9;
pub(crate) const MARKER_COM: u16 = 0xff64;
pub(crate) const MARKER_CAP: u16 = 0xff50;
pub(crate) const MARKER_COC: u16 = 0xff53;
pub(crate) const MARKER_TLM: u16 = 0xff55;
pub(crate) const MARKER_PLM: u16 = 0xff57;
pub(crate) const MARKER_PLT: u16 = 0xff58;
pub(crate) const MARKER_QCC: u16 = 0xff5d;
pub(crate) const MARKER_RGN: u16 = 0xff5e;
pub(crate) const MARKER_POC: u16 = 0xff5f;
pub(crate) const MARKER_PPM: u16 = 0xff60;
pub(crate) const MARKER_PPT: u16 = 0xff61;
pub(crate) const MARKER_CRG: u16 = 0xff63;

pub(crate) fn build_main_header_segments(plan: &EncodingPlan) -> Result<Vec<Vec<u8>>> {
    Ok(vec![
        markers::encode_siz(plan)?,
        markers::encode_cod(plan),
        markers::encode_qcd(plan)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::CodestreamParts;
    use crate::model::{ColorSpace, EncodeOptions, Image, OutputFormat, Preset};
    use crate::plan::EncodingPlan;
    use crate::t2::PacketSequenceBuilder;

    #[test]
    fn parser_reemits_single_tile_codestream() {
        let bytes = [
            0xff, 0x4f, 0xff, 0x51, 0x00, 0x04, 0x12, 0x34, 0xff, 0x52, 0x00, 0x05, 0xaa, 0xbb,
            0xcc, 0xff, 0x90, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x17, 0x00, 0x00, 0xff,
            0x64, 0x00, 0x03, 0x01, 0xff, 0x93, 0xde, 0xad, 0xbe, 0xef, 0xff, 0xd9,
        ];
        let image = Image {
            width: 1,
            height: 1,
            components: vec![crate::model::Component {
                data: vec![0],
                width: 1,
                height: 1,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }],
            colorspace: ColorSpace::Gray,
        };
        let plan = EncodingPlan::build(
            &image,
            &EncodeOptions {
                quality: Preset::DocumentHigh.quality(),
                format: OutputFormat::J2k,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("build plan");

        let parts = CodestreamParts::parse_single_tile(&bytes).expect("parse");
        assert_eq!(parts.tile_parts.len(), 1);
        assert_eq!(parts.tile_parts[0].payload.packet_count(), 1);
        let rebuilt = parts.encode(&plan).expect("encode");
        assert_eq!(rebuilt, bytes);
    }

    #[test]
    fn writer_accepts_locally_constructed_packet_sequence() {
        let image = Image {
            width: 1,
            height: 1,
            components: vec![crate::model::Component {
                data: vec![0],
                width: 1,
                height: 1,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }],
            colorspace: ColorSpace::Gray,
        };
        let plan = EncodingPlan::build(
            &image,
            &EncodeOptions {
                quality: Preset::DocumentHigh.quality(),
                format: OutputFormat::J2k,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("build plan");

        let codestream = super::types::CodestreamParts {
            main_header_segments: vec![
                vec![0xff, 0x51, 0x00, 0x04, 0x12, 0x34],
                vec![0xff, 0x52, 0x00, 0x05, 0xaa, 0xbb, 0xcc],
            ],
            tile_parts: vec![super::types::TilePart {
                header: super::types::TilePartHeader {
                    tile_index: 0,
                    part_index: 0,
                    total_parts: 0,
                },
                header_segments: vec![vec![0xff, 0x64, 0x00, 0x03, 0x01]],
                payload: PacketSequenceBuilder::new()
                    .push_header_body_packet(vec![0x10, 0x20], vec![0x30])
                    .push_opaque_packet(vec![0x40, 0x50])
                    .finish_payload(),
            }],
        };

        let rebuilt = codestream.encode(&plan).expect("encode codestream");
        assert_eq!(
            rebuilt,
            vec![
                0xff, 0x4f, 0xff, 0x51, 0x00, 0x04, 0x12, 0x34, 0xff, 0x52, 0x00, 0x05, 0xaa, 0xbb,
                0xcc, 0xff, 0x90, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0xff,
                0x64, 0x00, 0x03, 0x01, 0xff, 0x93, 0x10, 0x20, 0x30, 0x40, 0x50, 0xff, 0xd9,
            ]
        );
    }
}
