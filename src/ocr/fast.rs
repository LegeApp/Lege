/// Fast OCR path: async async orchestration (semaphore, region fan-out, tiling).
///
/// The synchronous OCR work delegates to `lege_ocr::engine` (run_image) and
/// the hOCR helpers in `lege_ocr::hocr`. This module owns the tokio semaphore
/// and the async wrapper/fan-out logic.
use anyhow::Result;
use image::RgbImage;
use once_cell::sync::Lazy;
use tokio::sync::Semaphore;

use lege_ocr::engine::default_engine;
use lege_ocr::fast as ocr_fast;
use lege_ocr::hocr::{adjust_offsets, finalize, strip_to_body};

use crate::reflow::{PlacedKind, PxRect, ReflowPage};

/// Limit concurrent OCR operations to avoid WinRT / Tesseract memory pressure.
pub static OCR_SEMAPHORE: Lazy<Semaphore> = Lazy::new(|| {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    #[cfg(target_os = "linux")]
    let permits = cores.clamp(2, 8);
    #[cfg(not(target_os = "linux"))]
    let permits = 2;
    Semaphore::new(permits)
});

pub fn should_use_region_ocr(
    enable_layout_detection: bool,
    detections: &[crate::engine::Detection],
) -> bool {
    let classifier = crate::types::LabelClassifier::default();
    let text_count = detections
        .iter()
        .filter(|d| classifier.should_process_with_ocr(d))
        .count();
    ocr_fast::should_use_region_ocr(enable_layout_detection, text_count)
}

/// Run OCR on a binarized page image; returns the hOCR string.
pub async fn perform_ocr_on_binarized(
    binarized: Vec<u8>,
    width: usize,
    height: usize,
    language: &str,
) -> Result<String> {
    let _permit = OCR_SEMAPHORE.acquire().await;
    let lang = language.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let engine = default_engine();
        engine.run_image(&binarized, width, height, true, &lang)
    })
    .await?;

    Ok(result.map(|r| r.hocr).unwrap_or_default())
}

/// Fan out over layout-detected text regions, OCR each, and stitch results.
pub async fn perform_region_based_ocr(
    binarized: &[u8],
    page_width: usize,
    page_height: usize,
    detections: &[crate::engine::Detection],
    language: &str,
) -> Result<String> {
    // Fail fast if the OCR engine itself is unavailable, rather than letting every
    // region task return an empty result and silently producing a textless page.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    crate::ocr::check_tesseract_availability_for_language(language)
        .map_err(|e| anyhow::anyhow!("OCR engine unavailable: {e}"))?;

    let classifier = crate::types::LabelClassifier::default();
    let text_regions: Vec<_> = detections
        .iter()
        .filter(|d| classifier.should_process_with_ocr(d))
        .collect();

    let mut tasks = Vec::new();
    for det in &text_regions {
        let bbox = det.bbox;
        let (region_data, rw, rh) =
            ocr_fast::extract_region(binarized, page_width, page_height, bbox)?;
        let lang = language.to_string();
        tasks.push(tokio::spawn(async move {
            let hocr = perform_ocr_on_binarized(region_data, rw, rh, &lang)
                .await
                .ok();
            (hocr, bbox)
        }));
    }

    let mut stitched = String::new();
    for res in futures::future::join_all(tasks).await {
        match res {
            Ok((Some(hocr), bbox)) => {
                let body = strip_to_body(&hocr);
                let adjusted =
                    adjust_offsets(&body, bbox[0].round() as i32, bbox[1].round() as i32);
                stitched.push_str(&adjusted);
            }
            Ok((None, bbox)) => log::warn!("region OCR produced no text at {bbox:?}"),
            Err(e) => log::warn!("region OCR task failed: {e}"),
        }
    }

    if stitched.trim().is_empty() {
        if let Ok(hocr) =
            perform_tiling_based_ocr(binarized, page_width, page_height, language).await
        {
            if !strip_to_body(&hocr).trim().is_empty() {
                return Ok(hocr);
            }
        }
        if let Ok(hocr) =
            perform_ocr_on_binarized(binarized.to_vec(), page_width, page_height, language).await
        {
            let body = strip_to_body(&hocr);
            return Ok(finalize(&body, page_width, page_height));
        }
    }

    Ok(finalize(&stitched, page_width, page_height))
}

/// Slice the page into overlapping horizontal tiles and OCR each tile.
pub async fn perform_tiling_based_ocr(
    binarized: &[u8],
    page_width: usize,
    page_height: usize,
    language: &str,
) -> Result<String> {
    const TILE_HEIGHT: usize = 400;
    const OVERLAP: usize = 50;

    let mut tasks = Vec::new();
    let mut y_start = 0usize;
    while y_start < page_height {
        let y_end = (y_start + TILE_HEIGHT).min(page_height);
        let bbox = [0.0f32, y_start as f32, page_width as f32, y_end as f32];
        let (tile, tw, th) = ocr_fast::extract_region(binarized, page_width, page_height, bbox)?;
        let lang = language.to_string();
        tasks.push(tokio::spawn(async move {
            let hocr = perform_ocr_on_binarized(tile, tw, th, &lang).await.ok();
            (hocr, bbox)
        }));
        if y_end == page_height {
            break;
        }
        y_start = y_start + TILE_HEIGHT - OVERLAP;
    }

    let mut stitched = String::new();
    for res in futures::future::join_all(tasks).await {
        match res {
            Ok((Some(hocr), bbox)) => {
                let body = strip_to_body(&hocr);
                let adjusted =
                    adjust_offsets(&body, bbox[0].round() as i32, bbox[1].round() as i32);
                stitched.push_str(&adjusted);
            }
            Ok((None, bbox)) => log::warn!("tile OCR produced no text at {bbox:?}"),
            Err(e) => log::warn!("tile OCR task failed: {e}"),
        }
    }

    if stitched.trim().is_empty() {
        if let Ok(hocr) =
            perform_ocr_on_binarized(binarized.to_vec(), page_width, page_height, language).await
        {
            let body = strip_to_body(&hocr);
            return Ok(finalize(&body, page_width, page_height));
        }
    }

    Ok(finalize(&stitched, page_width, page_height))
}

//==============================================================================
// Reflow ↔ fast-OCR integration
//==============================================================================
//
// The fast path is the direct analog of slow-OCR's reflow integration. Instead
// of recognizing source crops and re-projecting them, it OCRs the *composed
// reflow raster* — the final page image, with text already at its reflowed
// output positions — and reuses reflow's layout to decide where to look. The
// reflowed placements are grouped into vertically-contiguous text blocks (the
// same notion of "region" the streaming fast path gets from layout detection),
// each block is OCR'd out of the binarized composed page, and its hOCR is
// stitched back at the block's output position. Because every block is a rigid
// rectangle on the composed page, a single offset re-projects recognition
// exactly onto the reflowed text — no per-word matching needed.

/// Build a page-level hOCR text layer for a *reflowed* output page with the fast
/// OCR engine. `composed` is the final reflowed page raster (output-pixel
/// space); the returned hOCR is expressed in those same coordinates so the
/// invisible text overlays the reflowed bitmaps. Returns `None` when the page
/// has no recognizable text.
pub async fn perform_reflow_page_fast_ocr(
    page: &ReflowPage,
    composed: &RgbImage,
    language: &str,
) -> Result<Option<String>> {
    // Fail fast if the engine is unavailable, so reflow pages aren't silently
    // emitted without the text layer the user asked for.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    crate::ocr::check_tesseract_availability_for_language(language)
        .map_err(|e| anyhow::anyhow!("OCR engine unavailable: {e}"))?;

    let blocks = reflow_output_text_blocks(page);
    if blocks.is_empty() {
        return Ok(None);
    }

    let page_width = composed.width() as usize;
    let page_height = composed.height() as usize;
    if page_width == 0 || page_height == 0 {
        return Ok(None);
    }
    let binarized = binarize_composed(composed);

    let mut tasks = Vec::new();
    for rect in blocks {
        let bbox = [
            rect.x as f32,
            rect.y as f32,
            rect.right() as f32,
            rect.bottom() as f32,
        ];
        let (region_data, rw, rh) =
            match ocr_fast::extract_region(&binarized, page_width, page_height, bbox) {
                Ok(region) => region,
                Err(e) => {
                    log::warn!("reflow OCR region extract failed at {bbox:?}: {e}");
                    continue;
                }
            };
        let lang = language.to_string();
        tasks.push(tokio::spawn(async move {
            let hocr = perform_ocr_on_binarized(region_data, rw, rh, &lang)
                .await
                .ok();
            (hocr, bbox)
        }));
    }

    let mut stitched = String::new();
    for res in futures::future::join_all(tasks).await {
        match res {
            Ok((Some(hocr), bbox)) => {
                let body = strip_to_body(&hocr);
                if !body.trim().is_empty() {
                    let adjusted =
                        adjust_offsets(&body, bbox[0].round() as i32, bbox[1].round() as i32);
                    stitched.push_str(&adjusted);
                }
            }
            Ok((None, bbox)) => log::warn!("reflow OCR produced no text at {bbox:?}"),
            Err(e) => log::warn!("reflow OCR task failed: {e}"),
        }
    }

    if stitched.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(finalize(&stitched, page_width, page_height)))
}

/// Binarize a composed reflow raster (black text on white) into the 1bpp buffer
/// the fast engine expects: `0` = ink, `255` = background.
fn binarize_composed(img: &RgbImage) -> Vec<u8> {
    img.pixels()
        .map(|p| {
            let luma = (p[0] as u32 * 30 + p[1] as u32 * 59 + p[2] as u32 * 11) / 100;
            if luma < 128 { 0 } else { 255 }
        })
        .collect()
}

/// Group a reflowed page's text placements into vertically-contiguous blocks in
/// output-page coordinates. Words sharing an output row first merge into lines
/// (by vertical-band overlap); lines separated by no more than a body line's
/// height then merge into a block, so paragraph spacers and figures — which
/// reflow renders as larger vertical gaps — naturally split blocks apart.
fn reflow_output_text_blocks(page: &ReflowPage) -> Vec<PxRect> {
    let mut rects: Vec<PxRect> = page
        .items
        .iter()
        .filter(|it| matches!(it.kind, PlacedKind::Word | PlacedKind::Run))
        .map(|it| it.out_rect)
        .filter(|r| !r.is_empty())
        .collect();
    if rects.is_empty() {
        return Vec::new();
    }
    rects.sort_by_key(|r| (r.y, r.x));

    // Merge placements that share an output row into line rectangles.
    let mut lines: Vec<PxRect> = Vec::new();
    for r in rects {
        if let Some(last) = lines.last_mut() {
            let overlap = last.bottom().min(r.bottom()).saturating_sub(last.y.max(r.y));
            let h = r.h.max(1);
            if (overlap as f32) >= 0.5 * (h as f32) {
                *last = last.union(&r);
                continue;
            }
        }
        lines.push(r);
    }

    // Median line height sets the gap threshold for merging lines into blocks.
    let mut heights: Vec<u32> = lines.iter().map(|l| l.h).collect();
    heights.sort_unstable();
    let median_h = heights[heights.len() / 2].max(1);

    let mut blocks: Vec<PxRect> = Vec::new();
    for l in lines {
        if let Some(last) = blocks.last_mut() {
            let gap = l.y.saturating_sub(last.bottom());
            if gap <= median_h {
                *last = last.union(&l);
                continue;
            }
        }
        blocks.push(l);
    }
    blocks
}

#[cfg(test)]
mod reflow_fast_tests {
    use super::*;
    use crate::reflow::{PlacedItem, PlacedKind, PxRect, ReflowPage, SourceRef};

    fn word_item(x: u32, y: u32, w: u32, h: u32) -> PlacedItem {
        PlacedItem {
            out_rect: PxRect::new(x, y, w, h),
            src: SourceRef {
                page_index: 0,
                rect: PxRect::new(0, 0, w, h),
            },
            scale: 1.0,
            kind: PlacedKind::Word,
        }
    }

    fn page_with(items: Vec<PlacedItem>) -> ReflowPage {
        ReflowPage {
            index: 0,
            width: 600,
            height: 800,
            items,
        }
    }

    #[test]
    fn merges_words_into_lines_then_paragraph_blocks() {
        // Two words on row 1, two on row 2 (tight leading) -> one block.
        // A third row after a large spacer -> a separate block.
        let page = page_with(vec![
            word_item(0, 0, 40, 20),
            word_item(50, 0, 40, 20),
            word_item(0, 26, 40, 20),
            word_item(50, 26, 40, 20),
            word_item(0, 200, 40, 20),
        ]);
        let blocks = reflow_output_text_blocks(&page);
        assert_eq!(blocks.len(), 2);
        // First block spans both tight rows and the full word width.
        assert_eq!(blocks[0], PxRect::new(0, 0, 90, 46));
        // Second block is the isolated row after the spacer.
        assert_eq!(blocks[1], PxRect::new(0, 200, 40, 20));
    }

    #[test]
    fn ignores_empty_pages() {
        let page = page_with(Vec::new());
        assert!(reflow_output_text_blocks(&page).is_empty());
    }

    #[test]
    fn binarize_splits_ink_from_background() {
        let mut img = RgbImage::from_pixel(2, 1, image::Rgb([255, 255, 255]));
        img.put_pixel(0, 0, image::Rgb([0, 0, 0]));
        assert_eq!(binarize_composed(&img), vec![0, 255]);
    }
}
