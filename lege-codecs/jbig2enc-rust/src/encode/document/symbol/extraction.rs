use super::types::SymbolCandidate;
#[cfg(feature = "symboldict")]
use crate::jbig2cc::analyze_page;
use crate::jbig2sym::BitImage;
#[cfg(feature = "symboldict")]
use crate::jbig2sym::Rect;
use anyhow::Result;

/// Segment a document image into symbol candidates.
///
/// This function finds connected components in the input image and returns
/// them as symbol candidates. Each candidate has a bitmap and a bounding box.
///
/// # Arguments
/// * `image` - The input binary image to segment
/// * `dpi` - Resolution in dots per inch (typically 300 for scanned documents)
/// * `losslevel` - 0 for lossless, >0 to enable noise removal
pub fn segment_symbols(image: &BitImage, dpi: i32, losslevel: i32) -> Result<Vec<SymbolCandidate>> {
    #[cfg(feature = "symboldict")]
    {
        // Use the new CC analysis pipeline from jbig2cc
        let cc_image = analyze_page(image, dpi, losslevel);
        let shapes = cc_image.extract_shapes();

        let mut candidates = Vec::with_capacity(shapes.len());
        for (bitmap, bbox) in shapes {
            let rect = Rect {
                x: bbox.xmin as u32,
                y: bbox.ymin as u32,
                width: bbox.width() as u32,
                height: bbox.height() as u32,
            };
            candidates.push(SymbolCandidate { bitmap, bbox: rect });
        }
        Ok(candidates)
    }
    #[cfg(not(feature = "symboldict"))]
    {
        Err(anyhow::anyhow!(
            "symbol segmentation requires the `symboldict` feature \
             (refused {}x{} page at {dpi} dpi, losslevel {losslevel})",
            image.width,
            image.height
        ))
    }
}
