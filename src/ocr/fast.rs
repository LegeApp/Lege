/// Fast OCR path: async async orchestration (semaphore, region fan-out, tiling).
///
/// The synchronous OCR work delegates to `lege_ocr::engine` (run_image) and
/// the hOCR helpers in `lege_ocr::hocr`. This module owns the tokio semaphore
/// and the async wrapper/fan-out logic.
use anyhow::Result;
use once_cell::sync::Lazy;
use tokio::sync::Semaphore;

use lege_ocr::engine::default_engine;
use lege_ocr::fast as ocr_fast;
use lege_ocr::hocr::{adjust_offsets, finalize, strip_to_body};

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
