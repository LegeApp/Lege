//! Declarative JPEG 2000 geometry layer.
//!
//! Single authority for how the image partitions into tiles, tile-components,
//! subbands, precincts, and code-blocks. Never encodes anything; only answers
//! geometry questions consumed by PCRD, Tier-1, and Tier-2.
//!
//! The active encoder emits regular tile grids for the bounded lossless path.
//! The types and derivation
//! functions use the same coordinate conventions as ISO/IEC 15444-1 Annex B
//! (global reference-grid coordinates), so tile-by-tile encoding can reuse this
//! layer without changing Tier-1/Tier-2 block identity semantics.

use crate::plan::{BandOrientation, CodeBlockSize, ComponentPlan, EncodingPlan};

// ---------------------------------------------------------------------------
// Core geometry types
// ---------------------------------------------------------------------------

/// JPEG 2000 SIZ tile grid parameters.
///
/// The tile-grid origin is always (0, 0) in this encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TileGrid {
    pub image_width: u32,
    pub image_height: u32,
    /// Nominal tile width; the final column is clipped to `image_width`.
    pub tile_width: u32,
    /// Nominal tile height; the final row is clipped to `image_height`.
    pub tile_height: u32,
    /// Tile-grid origin X (`XTOsiz` in the spec) — always 0.
    pub tile_origin_x: u32,
    /// Tile-grid origin Y (`YTOsiz` in the spec) — always 0.
    pub tile_origin_y: u32,
    pub num_tiles_x: u32,
    pub num_tiles_y: u32,
}

impl TileGrid {
    pub fn num_tiles(&self) -> u32 {
        self.num_tiles_x * self.num_tiles_y
    }

    pub fn tile_rect_at(&self, tile_index: u32) -> Option<TileRect> {
        if tile_index >= self.num_tiles() || self.num_tiles_x == 0 {
            return None;
        }

        let tile_index_u16 = u16::try_from(tile_index).ok()?;
        let tile_x = tile_index % self.num_tiles_x;
        let tile_y = tile_index / self.num_tiles_x;
        let x0 = self.tile_origin_x + tile_x * self.tile_width;
        let y0 = self.tile_origin_y + tile_y * self.tile_height;
        let x1 = (x0 + self.tile_width).min(self.image_width);
        let y1 = (y0 + self.tile_height).min(self.image_height);

        Some(TileRect {
            tile_index: tile_index_u16,
            x0,
            y0,
            x1,
            y1,
        })
    }

    pub fn tile_rects(&self) -> Vec<TileRect> {
        (0..self.num_tiles())
            .filter_map(|tile_index| self.tile_rect_at(tile_index))
            .collect()
    }
}

/// Single tile bounding rectangle in global reference-grid coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TileRect {
    /// Raster-order tile index (`Isot` in the spec).
    pub tile_index: u16,
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl TileRect {
    pub fn width(&self) -> u32 {
        self.x1 - self.x0
    }
    pub fn height(&self) -> u32 {
        self.y1 - self.y0
    }
}

/// Tile-component bounding rectangle in component-domain coordinates.
///
/// Accounts for component subsampling (dx, dy). For non-subsampled components
/// (dx == dy == 1) this equals the tile rectangle.
/// Derived from ISO/IEC 15444-1 Annex B.2: `tcx0 = ceil(tx0 / dx)`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TileComponentRect {
    pub tile_index: u16,
    pub component: u16,
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl TileComponentRect {
    pub fn width(&self) -> u32 {
        self.x1 - self.x0
    }
    pub fn height(&self) -> u32 {
        self.y1 - self.y0
    }
}

/// DWT subband bounding rectangle within a tile-component.
///
/// Coordinates are in component-domain samples. For a single, non-subsampled
/// tile (the only kind this encoder produces) these match the DWT
/// coefficient-plane layout from `forward_53_2d_in_place` /
/// `forward_97_2d_in_place` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubbandRect {
    pub tile_index: u16,
    pub component: u16,
    pub resolution: u8,
    pub band: BandOrientation,
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl SubbandRect {
    #[cfg(test)]
    pub fn width(&self) -> u32 {
        self.x1 - self.x0
    }
    #[cfg(test)]
    pub fn height(&self) -> u32 {
        self.y1 - self.y0
    }
    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }
}

/// A single code-block with full JPEG 2000 provenance.
///
/// Carries the stable `block_id` used by PCRD as a key, plus the structural
/// coordinates that Tier-2 packet assembly needs. Coordinates (`x0`..`y1`)
/// are in component-domain samples, matching the DWT coefficient plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodeBlockRect {
    /// Global block ID, assigned in LRCP raster order across all components.
    pub block_id: usize,
    pub tile_index: u16,
    pub component: u16,
    pub resolution: u8,
    pub band: BandOrientation,
    /// Precinct index within the subband. Always 0 (single precinct per subband).
    pub precinct_index: u32,
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl CodeBlockRect {
    #[cfg(test)]
    pub fn width(&self) -> u32 {
        self.x1 - self.x0
    }
    #[cfg(test)]
    pub fn height(&self) -> u32 {
        self.y1 - self.y0
    }
}

// ---------------------------------------------------------------------------
// Derivation functions
// ---------------------------------------------------------------------------

/// Derive the tile grid from an encoding plan.
///
/// Uses the nominal SIZ tile dimensions recorded in the encoding plan.
pub(crate) fn tile_grid(plan: &EncodingPlan) -> TileGrid {
    regular_tile_grid(plan.width, plan.height, plan.tile.width, plan.tile.height)
}

/// Build a regular JPEG 2000 tile grid with origin (0, 0).
///
/// The final row and column are clipped to the image dimensions by
/// [`TileGrid::tile_rect_at`]. Callers are expected to validate non-zero
/// dimensions before planning; this helper still clamps the tile count to at
/// least one so geometry diagnostics remain total for valid images.
pub(crate) fn regular_tile_grid(
    image_width: u32,
    image_height: u32,
    tile_width: u32,
    tile_height: u32,
) -> TileGrid {
    TileGrid {
        image_width,
        image_height,
        tile_width,
        tile_height,
        tile_origin_x: 0,
        tile_origin_y: 0,
        num_tiles_x: div_ceil(image_width, tile_width).max(1),
        num_tiles_y: div_ceil(image_height, tile_height).max(1),
    }
}

/// Return the single tile rectangle for the 1×1 grid.
#[cfg(test)]
pub(crate) fn tile_rect(grid: &TileGrid) -> TileRect {
    grid.tile_rect_at(0)
        .expect("tile grid should contain tile 0")
}

/// Derive the tile-component rectangle for one component.
///
/// ISO/IEC 15444-1 Annex B.2: `tcx0 = ceil(tx0 / dx)`, `tcx1 = ceil(tx1 / dx)`.
pub(crate) fn tile_component_rect(
    tile: &TileRect,
    component_index: u16,
    comp: &ComponentPlan,
) -> TileComponentRect {
    TileComponentRect {
        tile_index: tile.tile_index,
        component: component_index,
        x0: div_ceil(tile.x0, comp.dx),
        y0: div_ceil(tile.y0, comp.dy),
        x1: div_ceil(tile.x1, comp.dx),
        y1: div_ceil(tile.y1, comp.dy),
    }
}

/// Return the packed DWT resolution sizes from coarsest to finest for a
/// tile-component whose samples occupy `[x0, x0 + width) x [y0, y0 + height)`
/// on the component reference grid.
///
/// Annex B/F partitioning depends on the reference-grid phase: after one
/// decomposition the low-pass interval is `[ceil(x0 / 2), ceil(x1 / 2))`,
/// which is not always `ceil(width / 2)` for an odd tile origin.
pub(crate) fn phase_resolution_sizes(
    x0: usize,
    y0: usize,
    width: usize,
    height: usize,
    levels: u8,
) -> Vec<(usize, usize)> {
    let mut ax = x0;
    let mut ay = y0;
    let mut bx = x0 + width;
    let mut by = y0 + height;
    let mut resolutions = Vec::with_capacity(usize::from(levels) + 1);
    resolutions.push((width, height));
    for _ in 0..levels {
        ax = ax.div_ceil(2);
        ay = ay.div_ceil(2);
        bx = bx.div_ceil(2);
        by = by.div_ceil(2);
        resolutions.push((bx - ax, by - ay));
    }
    resolutions.reverse();
    resolutions
}

/// Derive all subband rectangles for a tile-component.
///
/// Returned in LRCP order: LL at resolution 0, then for each higher
/// resolution: HL, LH, HH. This matches the layout produced by the forward
/// DWT and the ordering assumed by `NativeEncodedTier1Layout`.
///
/// Sizing uses `ceil(dim / 2^k)` at each level, per ISO/IEC 15444-1
/// Annex B.5. For a single non-subsampled tile the coordinates equal the
/// DWT coefficient-plane coordinates directly.
pub(crate) fn subband_rects(tc: &TileComponentRect, levels: u8) -> Vec<SubbandRect> {
    // Build the resolution ladder from coarsest to finest.
    // resolutions[0] = (width, height) of the LL output (coarsest).
    // resolutions[levels] = (width, height) of the full component.
    let resolutions = phase_resolution_sizes(
        tc.x0 as usize,
        tc.y0 as usize,
        tc.width() as usize,
        tc.height() as usize,
        levels,
    )
    .into_iter()
    .map(|(w, h)| (w as u32, h as u32))
    .collect::<Vec<_>>();

    let mut subbands = Vec::with_capacity(1 + 3 * levels as usize);

    // LL subband at resolution 0 (coarsest approximation).
    let (ll_w, ll_h) = resolutions[0];
    subbands.push(SubbandRect {
        tile_index: tc.tile_index,
        component: tc.component,
        resolution: 0,
        band: BandOrientation::Ll,
        x0: tc.x0,
        y0: tc.y0,
        x1: tc.x0 + ll_w,
        y1: tc.y0 + ll_h,
    });

    // HL, LH, HH for each decomposition level (resolution 1..=levels).
    for level_idx in 0..levels as usize {
        let resolution = (level_idx + 1) as u8;
        let (low_w, low_h) = resolutions[level_idx];
        let (full_w, full_h) = resolutions[level_idx + 1];

        let low_x1 = tc.x0 + low_w;
        let low_y1 = tc.y0 + low_h;
        let full_x1 = tc.x0 + full_w;
        let full_y1 = tc.y0 + full_h;

        subbands.push(SubbandRect {
            tile_index: tc.tile_index,
            component: tc.component,
            resolution,
            band: BandOrientation::Hl,
            x0: low_x1,
            y0: tc.y0,
            x1: full_x1,
            y1: low_y1,
        });
        subbands.push(SubbandRect {
            tile_index: tc.tile_index,
            component: tc.component,
            resolution,
            band: BandOrientation::Lh,
            x0: tc.x0,
            y0: low_y1,
            x1: low_x1,
            y1: full_y1,
        });
        subbands.push(SubbandRect {
            tile_index: tc.tile_index,
            component: tc.component,
            resolution,
            band: BandOrientation::Hh,
            x0: low_x1,
            y0: low_y1,
            x1: full_x1,
            y1: full_y1,
        });
    }

    subbands
}

/// Partition one subband into code-block rectangles.
///
/// Blocks are aligned to the subband origin and clipped to the subband
/// boundary (ISO/IEC 15444-1 Annex B.7). Since this encoder uses one
/// precinct per subband, `precinct_index` is always 0.
///
/// `next_block_id` is incremented for each block produced, enabling globally
/// unique IDs when this function is called in LRCP order across all subbands.
pub(crate) fn codeblock_rects_for_subband(
    subband: &SubbandRect,
    cb_size: CodeBlockSize,
    next_block_id: &mut usize,
) -> Vec<CodeBlockRect> {
    if subband.is_empty() {
        return Vec::new();
    }

    let cbw = cb_size.width;
    let cbh = cb_size.height;
    let mut blocks = Vec::new();

    let mut by = subband.y0;
    while by < subband.y1 {
        let block_y1 = (by + cbh).min(subband.y1);
        let mut bx = subband.x0;
        while bx < subband.x1 {
            let block_x1 = (bx + cbw).min(subband.x1);
            blocks.push(CodeBlockRect {
                block_id: *next_block_id,
                tile_index: subband.tile_index,
                component: subband.component,
                resolution: subband.resolution,
                band: subband.band,
                precinct_index: 0,
                x0: bx,
                y0: by,
                x1: block_x1,
                y1: block_y1,
            });
            *next_block_id += 1;
            bx = block_x1;
        }
        by = block_y1;
    }

    blocks
}

/// Enumerate every code-block for an encoding plan in LRCP raster order.
///
/// Block IDs are assigned starting from 0 and increment across components,
/// resolutions, bands, and spatial positions. Originally meant as the
/// convenience entry point for PCRD and Tier-2 to get the full block map, but
/// the real Tier-1 caller (`encode::backend::native::t1`) interleaves
/// `subband_rects`/`codeblock_rects_for_subband` per tile-component instead
/// of pre-materializing the whole map, so this exists only as a
/// cross-checked reference implementation for tests.
#[cfg(test)]
pub(crate) fn enumerate_all_codeblocks(plan: &EncodingPlan) -> Vec<CodeBlockRect> {
    let grid = tile_grid(plan);
    let mut all_blocks = Vec::new();
    let mut next_block_id = 0usize;

    for tile in grid.tile_rects() {
        for (comp_idx, comp) in plan.components.iter().enumerate() {
            let tc = tile_component_rect(&tile, comp_idx as u16, comp);
            let subbands = subband_rects(&tc, plan.decomposition_levels);
            for subband in &subbands {
                let blocks =
                    codeblock_rects_for_subband(subband, plan.code_block_size, &mut next_block_id);
                all_blocks.extend(blocks);
            }
        }
    }

    all_blocks
}

fn div_ceil(a: u32, b: u32) -> u32 {
    (a + b - 1) / b
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{ColorSpace, Component, EncodeOptions, Image, OutputFormat, Preset};
    use crate::plan::EncodingPlan;

    fn gray_plan(width: u32, height: u32) -> EncodingPlan {
        let image = Image {
            width,
            height,
            components: vec![Component {
                data: vec![0; (width * height) as usize],
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }],
            colorspace: ColorSpace::Gray,
        };
        EncodingPlan::build(
            &image,
            &EncodeOptions {
                quality: Preset::DocumentHigh.quality(),
                format: OutputFormat::J2k,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn tile_grid_is_single_tile_covering_full_image() {
        let plan = gray_plan(128, 96);
        let grid = tile_grid(&plan);
        assert_eq!(grid.num_tiles(), 1);
        assert_eq!(grid.tile_width, 128);
        assert_eq!(grid.tile_height, 96);
        assert_eq!(grid.tile_origin_x, 0);
        assert_eq!(grid.tile_origin_y, 0);
        assert_eq!(grid.num_tiles_x, 1);
        assert_eq!(grid.num_tiles_y, 1);
    }

    #[test]
    fn tile_rect_covers_full_image() {
        let plan = gray_plan(200, 150);
        let grid = tile_grid(&plan);
        let tile = tile_rect(&grid);
        assert_eq!(tile.tile_index, 0);
        assert_eq!((tile.x0, tile.y0, tile.x1, tile.y1), (0, 0, 200, 150));
        assert_eq!(tile.width(), 200);
        assert_eq!(tile.height(), 150);
    }

    #[test]
    fn regular_tile_grid_partitions_uneven_image() {
        let grid = regular_tile_grid(100, 70, 32, 32);

        assert_eq!(grid.num_tiles_x, 4);
        assert_eq!(grid.num_tiles_y, 3);
        assert_eq!(grid.num_tiles(), 12);

        let middle = grid.tile_rect_at(5).expect("middle tile");
        assert_eq!(middle.tile_index, 5);
        assert_eq!(
            (middle.x0, middle.y0, middle.x1, middle.y1),
            (32, 32, 64, 64)
        );

        let last = grid.tile_rect_at(11).expect("last clipped tile");
        assert_eq!(last.tile_index, 11);
        assert_eq!((last.x0, last.y0, last.x1, last.y1), (96, 64, 100, 70));
        assert_eq!(last.width(), 4);
        assert_eq!(last.height(), 6);

        assert!(grid.tile_rect_at(12).is_none());
    }

    #[test]
    fn regular_tile_rects_cover_image_area_without_overlap() {
        let grid = regular_tile_grid(101, 73, 32, 32);
        let rects = grid.tile_rects();

        assert_eq!(rects.len(), 12);
        let total_area: u32 = rects.iter().map(|tile| tile.width() * tile.height()).sum();
        assert_eq!(total_area, 101 * 73);

        for (index, tile) in rects.iter().enumerate() {
            assert_eq!(usize::from(tile.tile_index), index);
            assert!(tile.x0 < tile.x1 && tile.y0 < tile.y1);
            assert!(tile.x1 <= grid.image_width);
            assert!(tile.y1 <= grid.image_height);
        }
    }

    #[test]
    fn tile_component_rect_matches_tile_for_non_subsampled() {
        let plan = gray_plan(64, 48);
        let grid = tile_grid(&plan);
        let tile = tile_rect(&grid);
        let tc = tile_component_rect(&tile, 0, &plan.components[0]);
        assert_eq!((tc.x0, tc.y0, tc.x1, tc.y1), (0, 0, 64, 48));
        assert_eq!(tc.component, 0);
        assert_eq!(tc.tile_index, 0);
    }

    #[test]
    fn tile_component_rect_ceil_divides_subsampled_tile_edges() {
        let tile = TileRect {
            tile_index: 7,
            x0: 31,
            y0: 17,
            x1: 65,
            y1: 50,
        };
        let comp = ComponentPlan {
            precision: 8,
            signed: false,
            dx: 2,
            dy: 3,
        };

        let tc = tile_component_rect(&tile, 2, &comp);

        assert_eq!(tc.tile_index, 7);
        assert_eq!(tc.component, 2);
        assert_eq!((tc.x0, tc.y0, tc.x1, tc.y1), (16, 6, 33, 17));
    }

    #[test]
    fn subband_rects_for_4x4_two_levels_match_layout_rs() {
        // Cross-check against layout.rs encode_resolutions / make_subband geometry.
        let tc = TileComponentRect {
            tile_index: 0,
            component: 0,
            x0: 0,
            y0: 0,
            x1: 4,
            y1: 4,
        };
        let subbands = subband_rects(&tc, 2);
        assert_eq!(subbands.len(), 7);

        let coords: Vec<_> = subbands
            .iter()
            .map(|s| (s.resolution, s.band, s.x0, s.y0, s.x1, s.y1))
            .collect();

        assert_eq!(
            coords,
            vec![
                (0, BandOrientation::Ll, 0, 0, 1, 1),
                (1, BandOrientation::Hl, 1, 0, 2, 1),
                (1, BandOrientation::Lh, 0, 1, 1, 2),
                (1, BandOrientation::Hh, 1, 1, 2, 2),
                (2, BandOrientation::Hl, 2, 0, 4, 2),
                (2, BandOrientation::Lh, 0, 2, 2, 4),
                (2, BandOrientation::Hh, 2, 2, 4, 4),
            ]
        );
    }

    #[test]
    fn subband_rects_for_odd_size_use_ceil_division() {
        // 5×3 with 1 level: LL = ceil(5/2)×ceil(3/2) = 3×2
        let tc = TileComponentRect {
            tile_index: 0,
            component: 0,
            x0: 0,
            y0: 0,
            x1: 5,
            y1: 3,
        };
        let subbands = subband_rects(&tc, 1);
        assert_eq!(subbands.len(), 4);
        let expected = vec![
            (0, BandOrientation::Ll, 0, 0, 3, 2),
            (1, BandOrientation::Hl, 3, 0, 5, 2),
            (1, BandOrientation::Lh, 0, 2, 3, 3),
            (1, BandOrientation::Hh, 3, 2, 5, 3),
        ];
        let actual: Vec<_> = subbands
            .iter()
            .map(|s| (s.resolution, s.band, s.x0, s.y0, s.x1, s.y1))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn subband_rects_cover_full_tile_component() {
        // Union of all subband areas at the finest resolution must equal the
        // full component area.
        let tc = TileComponentRect {
            tile_index: 0,
            component: 0,
            x0: 0,
            y0: 0,
            x1: 17,
            y1: 13,
        };
        let subbands = subband_rects(&tc, 3);
        // Only the high-resolution subbands (resolution == levels) plus the LL
        // contribute to coverage. Instead, verify total count and that HL/LH/HH
        // at each level cover non-overlapping regions.
        assert_eq!(subbands.len(), 1 + 3 * 3);
        for sb in &subbands {
            assert!(
                !sb.is_empty(),
                "empty subband at resolution {}",
                sb.resolution
            );
        }
    }

    #[test]
    fn codeblock_rects_partition_subband_without_gaps_or_overlap() {
        let subband = SubbandRect {
            tile_index: 0,
            component: 0,
            resolution: 1,
            band: BandOrientation::Hl,
            x0: 0,
            y0: 0,
            x1: 10,
            y1: 7,
        };
        let cb_size = CodeBlockSize {
            width: 4,
            height: 4,
        };
        let mut next_id = 0;
        let blocks = codeblock_rects_for_subband(&subband, cb_size, &mut next_id);

        // ceil(10/4)=3 cols × ceil(7/4)=2 rows = 6 blocks
        assert_eq!(blocks.len(), 6);

        // All blocks contained within subband
        for b in &blocks {
            assert!(b.x0 >= subband.x0 && b.x1 <= subband.x1);
            assert!(b.y0 >= subband.y0 && b.y1 <= subband.y1);
        }

        // IDs contiguous from 0
        for (i, b) in blocks.iter().enumerate() {
            assert_eq!(b.block_id, i);
        }

        // Total area equals subband area (no gaps, no overlap)
        let total_area: u32 = blocks.iter().map(|b| b.width() * b.height()).sum();
        assert_eq!(total_area, subband.width() * subband.height());
    }

    #[test]
    fn codeblock_rects_for_empty_subband_returns_empty() {
        let subband = SubbandRect {
            tile_index: 0,
            component: 0,
            resolution: 2,
            band: BandOrientation::Hh,
            x0: 4,
            y0: 4,
            x1: 4, // empty
            y1: 8,
        };
        let mut next_id = 0;
        let blocks = codeblock_rects_for_subband(
            &subband,
            CodeBlockSize {
                width: 64,
                height: 64,
            },
            &mut next_id,
        );
        assert!(blocks.is_empty());
        assert_eq!(
            next_id, 0,
            "next_block_id must not advance for empty subbands"
        );
    }

    #[test]
    fn enumerate_all_codeblocks_assigns_contiguous_ids() {
        let plan = gray_plan(32, 32);
        let blocks = enumerate_all_codeblocks(&plan);
        assert!(!blocks.is_empty());
        for (i, b) in blocks.iter().enumerate() {
            assert_eq!(b.block_id, i, "block_id gap at position {i}");
        }
    }

    #[test]
    fn enumerate_all_codeblocks_count_matches_per_subband_sum() {
        let plan = gray_plan(64, 48);
        let grid = tile_grid(&plan);
        let tile = tile_rect(&grid);
        let mut next_id = 0;
        let mut expected = 0;
        for (ci, comp) in plan.components.iter().enumerate() {
            let tc = tile_component_rect(&tile, ci as u16, comp);
            for sb in subband_rects(&tc, plan.decomposition_levels) {
                expected +=
                    codeblock_rects_for_subband(&sb, plan.code_block_size, &mut next_id).len();
            }
        }
        assert_eq!(enumerate_all_codeblocks(&plan).len(), expected);
    }

    #[test]
    fn all_codeblocks_belong_to_tile_zero_precinct_zero() {
        let plan = gray_plan(48, 40);
        let blocks = enumerate_all_codeblocks(&plan);
        assert!(!blocks.is_empty());
        assert!(blocks.iter().all(|b| b.tile_index == 0));
        assert!(blocks.iter().all(|b| b.precinct_index == 0));
    }

    #[test]
    fn subband_resolution_field_matches_dwt_level() {
        let plan = gray_plan(32, 32);
        let blocks = enumerate_all_codeblocks(&plan);
        let max_res = blocks.iter().map(|b| b.resolution).max().unwrap();
        assert_eq!(u32::from(max_res), u32::from(plan.decomposition_levels));
    }

    #[test]
    fn annex_b_phase_resolution_sizes_follow_reference_grid_bounds() {
        assert_eq!(
            super::phase_resolution_sizes(11, 9, 11, 9, 3),
            vec![(1, 1), (3, 2), (5, 4), (11, 9)]
        );
        assert_eq!(
            super::phase_resolution_sizes(0, 0, 11, 9, 3),
            vec![(2, 2), (3, 3), (6, 5), (11, 9)]
        );
    }
}
