use crate::t2::TilePartPayload;

#[derive(Debug, Clone)]
pub(crate) struct CodestreamParts {
    pub main_header_segments: Vec<Vec<u8>>,
    pub tile_parts: Vec<TilePart>,
}

#[derive(Debug, Clone)]
pub(crate) struct TilePart {
    pub header: TilePartHeader,
    pub header_segments: Vec<Vec<u8>>,
    pub payload: TilePartPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TilePartHeader {
    pub tile_index: u16,
    pub part_index: u8,
    pub total_parts: u8,
}
