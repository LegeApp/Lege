use crate::error::{Jp2LamError, Result};
use crate::plan::{BandOrientation, CodeBlockSize};

use super::NativeComponentCoefficients;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeCodeBlock {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
    pub coefficients: Vec<i32>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeSubband {
    pub resolution: u8,
    pub band: BandOrientation,
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
    pub codeblocks: Vec<NativeCodeBlock>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeComponentLayout {
    pub width: usize,
    pub height: usize,
    pub levels: u8,
    pub subbands: Vec<NativeSubband>,
}

pub(crate) fn build_component_layout(
    coefficients: &NativeComponentCoefficients,
    code_block_size: CodeBlockSize,
) -> Result<NativeComponentLayout> {
    let resolutions = crate::tiling::phase_resolution_sizes(
        coefficients.x0,
        coefficients.y0,
        coefficients.width,
        coefficients.height,
        coefficients.levels,
    );
    let mut subbands = Vec::new();
    let ll = resolutions[0];
    subbands.push(NativeSubband {
        resolution: 0,
        band: BandOrientation::Ll,
        x0: 0,
        y0: 0,
        x1: ll.0,
        y1: ll.1,
        codeblocks: split_codeblocks(coefficients, 0, 0, ll.0, ll.1, code_block_size)?,
    });

    // ISO/IEC 15444-1 Annex B.5 defines the resolution ladder used here.
    for (index, w) in resolutions.windows(2).enumerate() {
        let (low, full) = (w[0], w[1]);
        let resolution = (index + 1) as u8;
        subbands.push(make_subband(
            coefficients,
            resolution,
            BandOrientation::Hl,
            low.0,
            0,
            full.0,
            low.1,
            code_block_size,
        )?);
        subbands.push(make_subband(
            coefficients,
            resolution,
            BandOrientation::Lh,
            0,
            low.1,
            low.0,
            full.1,
            code_block_size,
        )?);
        subbands.push(make_subband(
            coefficients,
            resolution,
            BandOrientation::Hh,
            low.0,
            low.1,
            full.0,
            full.1,
            code_block_size,
        )?);
    }

    Ok(NativeComponentLayout {
        width: coefficients.width,
        height: coefficients.height,
        levels: coefficients.levels,
        subbands,
    })
}

fn make_subband(
    coefficients: &NativeComponentCoefficients,
    resolution: u8,
    band: BandOrientation,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    code_block_size: CodeBlockSize,
) -> Result<NativeSubband> {
    Ok(NativeSubband {
        resolution,
        band,
        x0,
        y0,
        x1,
        y1,
        codeblocks: split_codeblocks(coefficients, x0, y0, x1, y1, code_block_size)?,
    })
}

fn split_codeblocks(
    coefficients: &NativeComponentCoefficients,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    code_block_size: CodeBlockSize,
) -> Result<Vec<NativeCodeBlock>> {
    if x1 < x0 || y1 < y0 || x1 > coefficients.width || y1 > coefficients.height {
        return Err(Jp2LamError::EncodeFailed(
            "invalid subband rectangle for coefficient layout".to_string(),
        ));
    }
    let cbw = code_block_size.width as usize;
    let cbh = code_block_size.height as usize;
    let mut codeblocks = Vec::new();
    let mut by = y0;
    while by < y1 {
        let block_y1 = (by + cbh).min(y1);
        let mut bx = x0;
        while bx < x1 {
            let block_x1 = (bx + cbw).min(x1);
            let mut block = Vec::with_capacity((block_x1 - bx) * (block_y1 - by));
            for y in by..block_y1 {
                let row = y * coefficients.width;
                block.extend_from_slice(&coefficients.data[row + bx..row + block_x1]);
            }
            codeblocks.push(NativeCodeBlock {
                x0: bx,
                y0: by,
                x1: block_x1,
                y1: block_y1,
                coefficients: block,
            });
            bx = block_x1;
        }
        by = block_y1;
    }
    Ok(codeblocks)
}

#[cfg(test)]
mod tests {
    use super::{NativeComponentCoefficients, build_component_layout};
    use crate::plan::{BandOrientation, CodeBlockSize};

    #[test]
    fn layout_extracts_expected_subband_rectangles() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 4,
            height: 4,
            levels: 2,
            data: vec![-38, 36, 0, 16, 144, 0, 0, 16, 0, 0, 0, 0, 64, 64, 0, 0],
        };
        let layout = build_component_layout(
            &coefficients,
            CodeBlockSize {
                width: 64,
                height: 64,
            },
        )
        .expect("build layout");

        assert_eq!(layout.subbands.len(), 7);
        assert_eq!(
            layout
                .subbands
                .iter()
                .map(|band| (
                    band.resolution,
                    band.band,
                    band.x0,
                    band.y0,
                    band.x1,
                    band.y1
                ))
                .collect::<Vec<_>>(),
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
    fn layout_splits_subbands_into_codeblocks() {
        let coefficients = NativeComponentCoefficients {
            x0: 0,
            y0: 0,
            width: 6,
            height: 6,
            levels: 1,
            data: (0..36).collect(),
        };
        let layout = build_component_layout(
            &coefficients,
            CodeBlockSize {
                width: 2,
                height: 2,
            },
        )
        .expect("build layout");

        let hh = layout
            .subbands
            .iter()
            .find(|band| band.band == BandOrientation::Hh)
            .expect("hh band");
        assert_eq!(hh.codeblocks.len(), 4);
        assert_eq!(hh.codeblocks[0].coefficients, vec![21, 22, 27, 28]);
    }
}
