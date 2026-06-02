use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use tokio::sync::Semaphore;

/// Limit concurrent OCR operations to avoid WinRT memory pressure
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

/// For Tesseract, many tiny region OCR calls can be slower than tiled/full-page OCR
/// due to setup overhead. Cap region fan-out to keep Linux throughput stable.
const MAX_REGION_OCR_BOXES: usize = 12;

pub fn should_use_region_ocr(
    enable_layout_detection: bool,
    detections: &[crate::engine::Detection],
) -> bool {
    if !enable_layout_detection || detections.is_empty() {
        return false;
    }

    let classifier = crate::types::LabelClassifier::default();
    let text_regions = detections
        .iter()
        .filter(|d| classifier.should_process_with_ocr(d))
        .take(MAX_REGION_OCR_BOXES + 1)
        .count();

    text_regions > 0 && text_regions <= MAX_REGION_OCR_BOXES
}

/// Performs OCR on binarized image data
pub async fn perform_ocr_on_binarized(
    binarized: Vec<u8>,
    width: usize,
    height: usize,
    language: &str,
) -> Result<String> {
    // Concurrency limiter to keep OCR memory usage in check
    let _permit = OCR_SEMAPHORE.acquire().await;
    // Move the owned buffer into the blocking task (no copy).
    let binarized_data = binarized;
    let language = language.to_string();

    // Run the synchronous OCR in a blocking task to avoid runtime conflicts
    let ocr_result = tokio::task::spawn_blocking(move || {
        crate::ocr::run_ocr(
            &binarized_data,
            width,
            height,
            true, // is_binary = true for binarized data
            &language,
        )
    })
    .await?;

    match ocr_result {
        Some(result) => {
            // Return HOCR format for PDF text layer, not plain text
            if !result.hocr.is_empty() {
                Ok(result.hocr)
            } else {
                Ok(String::new())
            }
        }
        None => Ok(String::new()),
    }
}

// --- Helpers for region/tile OCR and HOCR stitching ---

/// Extract a sub-rectangle from a flat, row-major grayscale buffer
pub fn extract_region_from_image(
    image_data: &[u8],
    image_width: usize,
    image_height: usize,
    bbox: [f32; 4], // [x1,y1,x2,y2]
) -> Result<(Vec<u8>, usize, usize)> {
    let x1 = bbox[0].floor().max(0.0) as usize;
    let y1 = bbox[1].floor().max(0.0) as usize;
    let x2 = bbox[2].ceil().min(image_width as f32) as usize;
    let y2 = bbox[3].ceil().min(image_height as f32) as usize;

    if x2 <= x1 || y2 <= y1 {
        return Err(anyhow::anyhow!("Invalid region dimensions"));
    }

    let region_width = x2 - x1;
    let region_height = y2 - y1;

    let mut region_data = Vec::with_capacity(region_width * region_height);
    for y in y1..y2 {
        let start = y * image_width + x1;
        let end = start + region_width;
        if end > image_data.len() {
            return Err(anyhow::anyhow!("Region extraction out of bounds"));
        }
        region_data.extend_from_slice(&image_data[start..end]);
    }
    Ok((region_data, region_width, region_height))
}

/// Strip HOCR to just the inner content (ideally spans under an ocr_carea)
/// This removes redundant wrapper HTML and keeps only the essential word/line elements
fn strip_hocr_to_body(hocr: &str) -> String {
    // Primary path: extract ONLY the inner content of the first ocr_carea div
    if let Some(div_start) = hocr.find("<div class=\"ocr_carea\"") {
        // Find the end of the opening <div ...> tag
        if let Some(tag_close_rel) = hocr[div_start..].find('>') {
            let content_start = div_start + tag_close_rel + 1;
            // Find the trailing closing sequence emitted by our OCR generators
            if let Some(end) = hocr.rfind("</div></div></body></html>") {
                if end >= content_start {
                    let content = &hocr[content_start..end];
                    // Further strip redundant whitespace and newlines to minimize size
                    return content.trim().replace("\n", " ").replace("  ", " ");
                }
            }
        }
    }
    // Fallback: extract the content inside <body> ... </body>
    if let (Some(bstart), Some(bend)) = (hocr.find("<body>"), hocr.rfind("</body>")) {
        let content_start = bstart + "<body>".len();
        if bend >= content_start {
            let content = &hocr[content_start..bend];
            return content.trim().replace("\n", " ").replace("  ", " ");
        }
    }
    // Last resort: return original if expected structure not found, but still clean it up
    hocr.trim().replace("\n", " ").replace("  ", " ")
}

/// Adjusts bbox coordinates inside title="bbox x1 y1 x2 y2" by offsets
fn adjust_hocr_offsets(hocr_body: &str, dx: i32, dy: i32) -> String {
    static BBOX_TITLE_DQ_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"title="bbox\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)([^"]*)""#)
            .expect("valid bbox title regex (double quote)")
    });
    static BBOX_TITLE_SQ_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"title='bbox\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)([^']*)'"#)
            .expect("valid bbox title regex (single quote)")
    });

    fn replace_bbox_titles(input: &str, re: &Regex, quote: &str, dx: i32, dy: i32) -> String {
        re.replace_all(input, |caps: &regex::Captures| {
            let x1 = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok());
            let y1 = caps.get(2).and_then(|m| m.as_str().parse::<i32>().ok());
            let x2 = caps.get(3).and_then(|m| m.as_str().parse::<i32>().ok());
            let y2 = caps.get(4).and_then(|m| m.as_str().parse::<i32>().ok());
            let tail = caps.get(5).map_or("", |m| m.as_str());

            match (x1, y1, x2, y2) {
                (Some(x1), Some(y1), Some(x2), Some(y2)) => format!(
                    "title={}bbox {} {} {} {}{}{}",
                    quote,
                    x1 + dx,
                    y1 + dy,
                    x2 + dx,
                    y2 + dy,
                    tail,
                    quote
                ),
                _ => caps
                    .get(0)
                    .map_or_else(String::new, |m| m.as_str().to_string()),
            }
        })
        .into_owned()
    }

    let adjusted_dq = replace_bbox_titles(hocr_body, &BBOX_TITLE_DQ_RE, "\"", dx, dy);
    replace_bbox_titles(&adjusted_dq, &BBOX_TITLE_SQ_RE, "'", dx, dy)
}

fn finalize_hocr(body: &str, width: usize, height: usize) -> String {
    // Create minimal HOCR structure to reduce redundant data
    // Remove XML declaration and DOCTYPE as they're not essential for PDF text layers
    format!(
        r#"<html><head></head><body><div class="ocr_page" title="bbox 0 0 {} {}"><div class="ocr_carea" title="bbox 0 0 {} {}">{}</div></div></body></html>"#,
        width,
        height,
        width,
        height,
        body.trim()
    )
}

/// Runs OCR on detected text regions and stitches results into a page HOCR
pub async fn perform_region_based_ocr(
    binarized: &[u8],
    page_width: usize,
    page_height: usize,
    detections: &[crate::engine::Detection],
    language: &str,
) -> Result<String> {
    let mut tasks = Vec::new();
    let classifier = crate::types::LabelClassifier::default();
    let text_regions: Vec<_> = detections
        .iter()
        .filter(|d| classifier.should_process_with_ocr(d))
        .collect();

    #[cfg(feature = "debug-logging")]
    println!(
        "[perform_region_based_ocr] {} total detections, {} text regions for OCR",
        detections.len(),
        text_regions.len()
    );

    for det in text_regions {
        let bbox = det.bbox;
        let (region_data, region_w, region_h) =
            extract_region_from_image(binarized, page_width, page_height, bbox)?;
        let language = language.to_string();
        tasks.push(tokio::spawn(async move {
            let hocr =
                super::ocr::perform_ocr_on_binarized(region_data, region_w, region_h, &language)
                    .await
                    .ok();
            (hocr, bbox)
        }));
    }

    let mut stitched = String::new();
    let mut regions_with_text = 0;
    for res in futures::future::join_all(tasks).await {
        if let Ok((Some(hocr), bbox)) = res {
            #[cfg(feature = "debug-logging")]
            println!(
                "[perform_region_based_ocr] Region {:?}: HOCR {} chars",
                bbox,
                hocr.len()
            );
            let body = strip_hocr_to_body(&hocr);
            #[cfg(feature = "debug-logging")]
            println!(
                "[perform_region_based_ocr] After strip_hocr_to_body: {} chars",
                body.len()
            );
            let adjusted =
                adjust_hocr_offsets(&body, bbox[0].round() as i32, bbox[1].round() as i32);
            #[cfg(feature = "debug-logging")]
            println!(
                "[perform_region_based_ocr] After adjust_hocr_offsets: {} chars",
                adjusted.len()
            );
            if !adjusted.trim().is_empty() {
                regions_with_text += 1;
            }
            stitched.push_str(&adjusted);
        }
    }

    #[cfg(feature = "debug-logging")]
    println!(
        "[perform_region_based_ocr] Stitched result: {} chars from {} regions with text",
        stitched.len(),
        regions_with_text
    );

    if stitched.trim().is_empty() {
        // Fallback: if region-based OCR produced no text, try tiling-based OCR,
        // and finally full-page OCR to ensure some text layer is generated.
        #[cfg(feature = "debug-logging")]
        println!("[perform_region_based_ocr] Stitched is empty, trying tiling fallback...");
        if let Ok(hocr_tiles) =
            perform_tiling_based_ocr(binarized, page_width, page_height, language).await
        {
            if !strip_hocr_to_body(&hocr_tiles).trim().is_empty() {
                return Ok(hocr_tiles);
            }
        }
        #[cfg(feature = "debug-logging")]
        println!("[perform_region_based_ocr] Tiling fallback empty, trying full-page OCR...");
        if let Ok(hocr_full) =
            perform_ocr_on_binarized(binarized.to_vec(), page_width, page_height, language).await
        {
            let body = strip_hocr_to_body(&hocr_full);
            #[cfg(feature = "debug-logging")]
            println!(
                "[perform_region_based_ocr] Full-page OCR body: {} chars",
                body.len()
            );
            return Ok(finalize_hocr(&body, page_width, page_height));
        }
    }
    let result = finalize_hocr(&stitched, page_width, page_height);
    #[cfg(feature = "debug-logging")]
    println!(
        "[perform_region_based_ocr] Final HOCR: {} chars",
        result.len()
    );
    Ok(result)
}

/// Slices the page into overlapping horizontal tiles and runs OCR on each
pub async fn perform_tiling_based_ocr(
    binarized: &[u8],
    page_width: usize,
    page_height: usize,
    language: &str,
) -> Result<String> {
    let tile_height: usize = 400;
    let overlap: usize = 50;
    let mut tasks = Vec::new();

    let mut y_start: usize = 0;
    while y_start < page_height {
        let y_end = (y_start + tile_height).min(page_height);
        let bbox = [0.0, y_start as f32, page_width as f32, y_end as f32];
        let (tile_data, tile_w, tile_h) =
            extract_region_from_image(binarized, page_width, page_height, bbox)?;
        let language = language.to_string();
        tasks.push(tokio::spawn(async move {
            let hocr = super::ocr::perform_ocr_on_binarized(tile_data, tile_w, tile_h, &language)
                .await
                .ok();
            (hocr, bbox)
        }));
        if y_end == page_height {
            break;
        }
        y_start = y_start + tile_height - overlap;
    }

    let mut stitched = String::new();
    for res in futures::future::join_all(tasks).await {
        if let Ok((Some(hocr), bbox)) = res {
            let body = strip_hocr_to_body(&hocr);
            let adjusted =
                adjust_hocr_offsets(&body, bbox[0].round() as i32, bbox[1].round() as i32);
            stitched.push_str(&adjusted);
        }
    }
    if stitched.trim().is_empty() {
        // Fallback: run a single full-page OCR to ensure we produce a text layer
        if let Ok(hocr_full) =
            perform_ocr_on_binarized(binarized.to_vec(), page_width, page_height, language).await
        {
            let body = strip_hocr_to_body(&hocr_full);
            return Ok(finalize_hocr(&body, page_width, page_height));
        }
    }
    Ok(finalize_hocr(&stitched, page_width, page_height))
}
