/// Synchronous helpers for the **fast** OCR path (region/tile/full-page).
///
/// These functions contain no async or tokio code. The async orchestration
/// (semaphore, spawn_blocking, fan-out) lives in `src/ocr/fast.rs` in the
/// root lege crate.
use anyhow::Result;

/// Extract a rectangular sub-region from a flat row-major single-channel image.
///
/// `bbox` is `[x1, y1, x2, y2]` (floating-point, will be rounded to pixel boundaries).
/// Returns `(region_bytes, width, height)`.
pub fn extract_region(
    image_data: &[u8],
    image_width: usize,
    image_height: usize,
    bbox: [f32; 4],
) -> Result<(Vec<u8>, usize, usize)> {
    if image_width == 0 || image_height == 0 {
        anyhow::bail!("cannot extract a region from an empty image");
    }
    let expected = image_width
        .checked_mul(image_height)
        .ok_or_else(|| anyhow::anyhow!("image dimensions overflow"))?;
    if image_data.len() < expected {
        anyhow::bail!(
            "image buffer is truncated: expected at least {expected} bytes, got {}",
            image_data.len()
        );
    }
    if !bbox.iter().all(|coord| coord.is_finite()) {
        anyhow::bail!("region bbox contains a non-finite coordinate");
    }

    let x1 = bbox[0].floor().clamp(0.0, image_width as f32) as usize;
    let y1 = bbox[1].floor().clamp(0.0, image_height as f32) as usize;
    let x2 = bbox[2].ceil().clamp(0.0, image_width as f32) as usize;
    let y2 = bbox[3].ceil().clamp(0.0, image_height as f32) as usize;

    if x2 <= x1 || y2 <= y1 {
        anyhow::bail!("degenerate region bbox: [{x1},{y1},{x2},{y2}]");
    }

    let rw = x2 - x1;
    let rh = y2 - y1;
    let output_len = rw
        .checked_mul(rh)
        .ok_or_else(|| anyhow::anyhow!("region dimensions overflow"))?;
    let mut out = Vec::with_capacity(output_len);

    for row in y1..y2 {
        let start = row
            .checked_mul(image_width)
            .and_then(|offset| offset.checked_add(x1))
            .ok_or_else(|| anyhow::anyhow!("region row offset overflow"))?;
        let end = start
            .checked_add(rw)
            .ok_or_else(|| anyhow::anyhow!("region row end overflow"))?;
        out.extend_from_slice(&image_data[start..end]);
    }
    Ok((out, rw, rh))
}

/// Decide whether to use region-based OCR (layout detections available and count
/// is within the per-page cap).
pub fn should_use_region_ocr(enable_layout_detection: bool, detection_count: usize) -> bool {
    if !enable_layout_detection || detection_count == 0 {
        return false;
    }
    detection_count <= MAX_REGION_OCR_BOXES
}

/// Cap on the number of regions per page for region-based fast OCR.
/// Beyond this, tiling is used to avoid per-region setup overhead.
pub const MAX_REGION_OCR_BOXES: usize = 12;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_region_basic() {
        let image = vec![0u8; 100]; // 10x10
        let (region, w, h) = extract_region(&image, 10, 10, [2.0, 2.0, 8.0, 8.0]).unwrap();
        assert_eq!(w, 6);
        assert_eq!(h, 6);
        assert_eq!(region.len(), 36);
    }

    #[test]
    fn extract_region_degenerate() {
        let image = vec![0u8; 100];
        assert!(extract_region(&image, 10, 10, [5.0, 5.0, 5.0, 5.0]).is_err());
    }

    #[test]
    fn extract_region_rejects_truncated_image_and_non_finite_bbox() {
        assert!(extract_region(&[0; 99], 10, 10, [0.0, 0.0, 1.0, 1.0]).is_err());
        assert!(extract_region(&[0; 100], 10, 10, [0.0, f32::NAN, 1.0, 1.0]).is_err());
    }

    #[test]
    fn should_use_region_ocr_logic() {
        assert!(!should_use_region_ocr(false, 5));
        assert!(!should_use_region_ocr(true, 0));
        assert!(should_use_region_ocr(true, 5));
        assert!(should_use_region_ocr(true, MAX_REGION_OCR_BOXES));
        assert!(!should_use_region_ocr(true, MAX_REGION_OCR_BOXES + 1));
    }
}
