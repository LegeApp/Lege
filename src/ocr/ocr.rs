use anyhow::Result;
use once_cell::sync::Lazy;
use tokio::sync::Semaphore;

/// Limit concurrent OCR operations to avoid WinRT memory pressure
pub static OCR_SEMAPHORE: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(2));

/// Performs OCR on binarized image data
pub async fn perform_ocr_on_binarized(
    binarized: &[u8],
    width: usize,
    height: usize,
) -> Result<String> {
    // Concurrency limiter to keep OCR memory usage in check
    let _permit = OCR_SEMAPHORE.acquire().await;
    // Clone the data to move into the blocking task
    let binarized_data = binarized.to_vec();

    // Run the synchronous OCR in a blocking task to avoid runtime conflicts
    let ocr_result = tokio::task::spawn_blocking(move || {
        crate::ocr::run_ocr(
            &binarized_data,
            width,
            height,
            true, // is_binary = true for binarized data
        )
    })
    .await?;

    match ocr_result {
        Some(result) => {
            // Return HOCR format for PDF text layer, not plain text
            if !result.hocr.is_empty() {
                #[cfg(feature = "debug-logging")]
                println!(
                    "DEBUG OCR: OCR succeeded, HOCR data has {} characters",
                    result.hocr.len()
                );
                Ok(result.hocr)
            } else {
                #[cfg(feature = "debug-logging")]
                println!("DEBUG OCR: OCR succeeded but no HOCR data generated");
                Ok(String::new())
            }
        }
        None => {
            #[cfg(feature = "debug-logging")]
            println!("DEBUG OCR: OCR failed completely");
            Ok(String::new()) // Return empty string if OCR fails
        }
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
    let mut out = String::with_capacity(hocr_body.len());
    let bytes = hocr_body.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if i + 12 < bytes.len() && &bytes[i..i + 12] == b"title=\"bbox " {
            out.push_str("title=\"bbox ");
            i += 12;
            // parse four integers until '"'
            let mut nums = [0i32; 4];
            let mut nidx = 0usize;
            let mut num_buf = String::new();
            while i < bytes.len() && nidx < 4 {
                let c = bytes[i] as char;
                if c.is_ascii_digit() || c == '-' {
                    num_buf.push(c);
                    i += 1;
                    continue;
                }
                if c.is_ascii_whitespace() {
                    if !num_buf.is_empty() {
                        if let Ok(v) = num_buf.parse::<i32>() {
                            nums[nidx] = v;
                        }
                        nidx += 1;
                        num_buf.clear();
                    }
                    out.push(' ');
                    i += 1;
                    continue;
                }
                if c == '"' {
                    if !num_buf.is_empty() && nidx < 4 {
                        if let Ok(v) = num_buf.parse::<i32>() {
                            nums[nidx] = v;
                        }
                        nidx += 1;
                        num_buf.clear();
                    }
                    // apply offsets
                    if nidx == 4 {
                        nums[0] += dx;
                        nums[1] += dy;
                        nums[2] += dx;
                        nums[3] += dy;
                    }
                    out.push_str(&format!(
                        "{} {} {} {}\"",
                        nums[0], nums[1], nums[2], nums[3]
                    ));
                    i += 1; // skip '"'
                    break;
                }
                // unexpected char; write and advance
                out.push(c);
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
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
) -> Result<String> {
    let mut tasks = Vec::new();
    let classifier = crate::types::LabelClassifier::default();
    let text_regions: Vec<_> = detections
        .iter()
        .filter(|d| classifier.should_process_with_ocr(d))
        .collect();
    
    #[cfg(feature = "debug-logging")]
    println!("[perform_region_based_ocr] {} total detections, {} text regions for OCR", 
        detections.len(), text_regions.len());
    
    for det in text_regions {
        let bbox = det.bbox;
        let (region_data, region_w, region_h) =
            extract_region_from_image(binarized, page_width, page_height, bbox)?;
        tasks.push(tokio::spawn(async move {
            let hocr = super::ocr::perform_ocr_on_binarized(&region_data, region_w, region_h)
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
            println!("[perform_region_based_ocr] Region {:?}: HOCR {} chars", bbox, hocr.len());
            let body = strip_hocr_to_body(&hocr);
            #[cfg(feature = "debug-logging")]
            println!("[perform_region_based_ocr] After strip_hocr_to_body: {} chars", body.len());
            let adjusted =
                adjust_hocr_offsets(&body, bbox[0].round() as i32, bbox[1].round() as i32);
            #[cfg(feature = "debug-logging")]
            println!("[perform_region_based_ocr] After adjust_hocr_offsets: {} chars", adjusted.len());
            if !adjusted.trim().is_empty() {
                regions_with_text += 1;
            }
            stitched.push_str(&adjusted);
        }
    }
    
    #[cfg(feature = "debug-logging")]
    println!("[perform_region_based_ocr] Stitched result: {} chars from {} regions with text", 
        stitched.len(), regions_with_text);
    
    if stitched.trim().is_empty() {
        // Fallback: if region-based OCR produced no text, try tiling-based OCR,
        // and finally full-page OCR to ensure some text layer is generated.
        #[cfg(feature = "debug-logging")]
        println!("[perform_region_based_ocr] Stitched is empty, trying tiling fallback...");
        if let Ok(hocr_tiles) = perform_tiling_based_ocr(binarized, page_width, page_height).await {
            if !strip_hocr_to_body(&hocr_tiles).trim().is_empty() {
                return Ok(hocr_tiles);
            }
        }
        #[cfg(feature = "debug-logging")]
        println!("[perform_region_based_ocr] Tiling fallback empty, trying full-page OCR...");
        if let Ok(hocr_full) = perform_ocr_on_binarized(binarized, page_width, page_height).await {
            let body = strip_hocr_to_body(&hocr_full);
            #[cfg(feature = "debug-logging")]
            println!("[perform_region_based_ocr] Full-page OCR body: {} chars", body.len());
            return Ok(finalize_hocr(&body, page_width, page_height));
        }
    }
    let result = finalize_hocr(&stitched, page_width, page_height);
    #[cfg(feature = "debug-logging")]
    println!("[perform_region_based_ocr] Final HOCR: {} chars", result.len());
    Ok(result)
}

/// Slices the page into overlapping horizontal tiles and runs OCR on each
pub async fn perform_tiling_based_ocr(
    binarized: &[u8],
    page_width: usize,
    page_height: usize,
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
        tasks.push(tokio::spawn(async move {
            let hocr = super::ocr::perform_ocr_on_binarized(&tile_data, tile_w, tile_h)
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
        if let Ok(hocr_full) = perform_ocr_on_binarized(binarized, page_width, page_height).await {
            let body = strip_hocr_to_body(&hocr_full);
            return Ok(finalize_hocr(&body, page_width, page_height));
        }
    }
    Ok(finalize_hocr(&stitched, page_width, page_height))
}
