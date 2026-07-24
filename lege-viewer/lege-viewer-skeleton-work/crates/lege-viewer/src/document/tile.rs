use std::sync::Arc;

use crate::geometry::{RectF, RectI};
use crate::paint::PixelSurface;

use super::{DocumentId, PageIndex};

pub const TILE_SIZE: u32 = 256;
const SQRT_2: f64 = std::f64::consts::SQRT_2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ZoomBucket(pub i16);

impl ZoomBucket {
    pub const ONE: Self = Self(0);

    pub fn from_zoom(zoom: f64) -> Self {
        if !zoom.is_finite() || zoom <= 0.0 {
            return Self::ONE;
        }
        Self((zoom.ln() / SQRT_2.ln()).round() as i16)
    }

    pub fn scale(self) -> f64 {
        SQRT_2.powi(i32::from(self.0))
    }

    pub fn distance(self, other: Self) -> u16 {
        self.0.abs_diff(other.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileCoord {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TileTier {
    Thumbnail,
    Draft,
    TextFirst,
    Final,
}

impl TileTier {
    pub fn rank(self) -> u8 {
        match self {
            TileTier::Thumbnail => 0,
            TileTier::Draft => 1,
            TileTier::TextFirst => 2,
            TileTier::Final => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub document: DocumentId,
    pub page: PageIndex,
    pub bucket: ZoomBucket,
    pub coord: TileCoord,
    pub tier: TileTier,
}

#[derive(Debug, Clone)]
pub struct TileSurface {
    pub key: TileKey,
    pub generation: u64,
    pub page_device_rect: RectI,
    /// The exact page region covered by this tile in document space. This is
    /// the cross-bucket identity; `(x, y)` alone is not geometrically stable
    /// when bucket scale changes.
    pub page_document_rect: RectF,
    pub pixels: PixelSurface,
    pub degraded: bool,
}

impl TileSurface {
    pub fn byte_len(&self) -> u64 {
        self.pixels.byte_len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileDemand {
    pub page: PageIndex,
    pub coord: TileCoord,
    pub page_device_rect: RectI,
    pub page_document_rect: RectF,
    pub distance_from_viewport: f64,
    pub visible: bool,
}

impl TileDemand {
    pub fn key(self, document: DocumentId, bucket: ZoomBucket, tier: TileTier) -> TileKey {
        TileKey {
            document,
            page: self.page,
            bucket,
            coord: self.coord,
            tier,
        }
    }
}
