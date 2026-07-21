use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::Result;
use image::{GrayImage, RgbImage};

use lege_ocr::{
    OcrPipeline, SlowOcrConfig,
    coordinate::CoordinateMap,
    coordinate::scale_lines,
    hocr, normalize,
    types::{OcrLineResult, OcrWord, TextRegion},
};

use crate::engine::Detection;
use crate::pipeline::config::PipelineConfig;
use crate::reflow::{PlacedKind, PxRect, ReflowPage, SourcePageImage};
use crate::types::ContentCategory;

thread_local! {
    /// Per-worker cache of slow-OCR pipelines, keyed by language. Constructing
    /// an `OcrPipeline` initializes the platform OCR engine, which is expensive;
    /// reusing one per blocking-pool worker thread avoids paying that cost on
    /// every page.
    static PIPELINES: RefCell<HashMap<String, OcrPipeline>> = RefCell::new(HashMap::new());
}

/// Convert a lege `Detection` slice to `lege_ocr::TextRegion`s in the OCR
/// image's pixel space, keeping only text-like detections.
///
/// `detections` are expressed in *output* space (`output_w` × `output_h`); they
/// are scaled by (`image_w`/`output_w`, `image_h`/`output_h`) so they line up
/// with the (possibly higher-resolution) OCR raster.
fn detections_to_regions(
    detections: &[Detection],
    page_index: usize,
    image_w: u32,
    image_h: u32,
    output_w: u32,
    output_h: u32,
) -> Vec<TextRegion> {
    let sx = image_w as f32 / output_w.max(1) as f32;
    let sy = image_h as f32 / output_h.max(1) as f32;
    let classifier = crate::types::LabelClassifier::default();
    detections
        .iter()
        .filter(|d| {
            classifier.should_process_with_ocr(d) && !matches!(d.category, ContentCategory::Abandon)
        })
        .enumerate()
        .filter_map(|(i, d)| {
            let bbox = [
                (d.bbox[0] * sx).floor().clamp(0.0, image_w as f32) as u32,
                (d.bbox[1] * sy).floor().clamp(0.0, image_h as f32) as u32,
                (d.bbox[2] * sx).ceil().clamp(0.0, image_w as f32) as u32,
                (d.bbox[3] * sy).ceil().clamp(0.0, image_h as f32) as u32,
            ];
            (bbox[2] > bbox[0] && bbox[3] > bbox[1]).then(|| TextRegion {
                page_index,
                region_id: i,
                class_name: d.class_name.clone(),
                bbox_highres: bbox,
                confidence: d.confidence,
            })
        })
        .collect()
}

/// Run the slow OCR pipeline on one page and return a page-level hOCR string.
///
/// `image` is the raster to recognize — the high-resolution OCR raster when
/// "render high, resize low" is active, otherwise the page-resolution image.
/// `binarized` is a binary mask matching `image`'s dimensions; when it does not
/// match (e.g. only a page-resolution mask is available) the OCR raster is
/// re-binarized internally.
///
/// `detections` and the produced hOCR are both expressed in *output* space
/// (`output_width` × `output_height`) — the page-pixel space the PDF/DJVU writer
/// maps 1:1 onto the page. Recognition happens at `image`'s resolution and the
/// results are scaled back down to output space.
///
/// This is a drop-in replacement for the fast OCR path when
/// `config.slow_ocr_enabled()` is true.
#[allow(clippy::too_many_arguments)]
pub async fn perform_slow_ocr(
    image: &RgbImage,
    binarized: &[u8],
    detections: &[Detection],
    output_width: u32,
    output_height: u32,
    config: &PipelineConfig,
    page_index: usize,
) -> Result<Option<String>> {
    let image_w = image.width();
    let image_h = image.height();
    if image_w == 0 || image_h == 0 {
        return Ok(None);
    }

    let regions = detections_to_regions(
        detections,
        page_index,
        image_w,
        image_h,
        output_width,
        output_height,
    );

    if regions.is_empty() {
        return Ok(None);
    }

    let language = config.ocr_language().to_string();
    let coord_map = CoordinateMap::identity(image_w, image_h, image_w as f32, image_h as f32);

    // Run in spawn_blocking because OCR engine calls (and GPU binarization) are
    // synchronous. The pipeline (and its OCR engine) is cached per worker thread.
    let image_clone = image.clone();
    let has_matching_binary = binarized.len() == image_w as usize * image_h as usize;
    let binarized_clone = if has_matching_binary {
        binarized.to_vec()
    } else {
        Vec::new()
    };

    let mut slow_page = tokio::task::spawn_blocking(move || {
        // Binarize the OCR raster ourselves when the caller could not supply a
        // mask at this resolution (the high-res render has none).
        let binary = if binarized_clone.is_empty() {
            normalize::binarize_page(&image_clone)
        } else {
            binarized_clone
        };
        PIPELINES.with(|cell| {
            let mut pipelines = cell.borrow_mut();
            let pipeline = pipelines.entry(language.clone()).or_insert_with(|| {
                OcrPipeline::new(SlowOcrConfig {
                    language: language.clone(),
                    debug: false,
                    debug_out_dir: None,
                    ..Default::default()
                })
            });
            pipeline.process_page(&image_clone, &binary, &regions, &coord_map, page_index)
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("slow OCR task panicked: {e}"))??;

    // Scale recognition results from the OCR raster's space back to output space
    // and rebuild the page hOCR there (the writer treats hOCR coordinates as
    // page-pixel == PDF units).
    let sx = output_width as f32 / image_w as f32;
    let sy = output_height as f32 / image_h as f32;
    scale_lines(&mut slow_page.lines, sx, sy, output_width, output_height);
    let hocr = hocr::build_page_hocr(&slow_page.lines, output_width, output_height);

    Ok(Some(hocr))
}

//==============================================================================
// Reflow ↔ slow-OCR integration
//==============================================================================
//
// When raster reflow and slow OCR are both enabled, the reflow stage has
// already detected the page's text rows and word spans (see
// `reflow::reflow_page_flow`) and re-composed them onto new output pages. Rather
// than re-running layout detection + line segmentation, we reuse those
// detections: each reflowed text item (`PlacedKind::Word`) carries both the
// source rectangle it was lifted from (`src`) and the rectangle it now occupies
// on the output page (`out_rect`). Words from one source row share an identical
// source `y`/height, so we can reconstruct each source line, OCR it once at
// source resolution (best accuracy), then re-project the recognized words onto
// their reflowed output positions. The result is a text layer whose words
// overlay the reflowed bitmaps exactly.

/// One reflowed text item: where it came from and where it landed.
struct ReflowWord {
    src_page: usize,
    src_rect: PxRect,
    out_rect: PxRect,
}

/// A word's source x-range within a row crop, tied back to its placement.
struct WordSlot {
    placement_index: usize,
    local_x0: u32,
    local_x1: u32,
}

/// One OCR job: a grayscale crop of a whole source row plus the word slots it
/// must be split back into.
struct RowJob {
    crop: GrayImage,
    slots: Vec<WordSlot>,
}

/// A recognized word positioned in output-page coordinates.
struct OutputWord {
    rect: PxRect,
    text: String,
    confidence: Option<f32>,
}

/// Build a page-level hOCR text layer for a *reflowed* output page by reusing
/// the text-row / word regions the reflow stage already detected.
///
/// `sources` are the rendered source pages reflow composed from (high-resolution
/// when slow OCR is active, since `source_render_height` scales them up). The
/// returned hOCR is expressed in `page`'s output-pixel coordinates — the space
/// the PDF/DJVU writer maps 1:1 onto the page — so the invisible text overlays
/// the reflowed bitmaps. Returns `None` when the page has no recognizable text.
pub async fn perform_reflow_page_ocr(
    page: &ReflowPage,
    sources: &[SourcePageImage],
    config: &PipelineConfig,
) -> Result<Option<String>> {
    // Gather reflowed text items. Figures and tables carry no recognizable
    // inline text and are skipped.
    let placements: Vec<ReflowWord> = page
        .items
        .iter()
        .filter(|item| matches!(item.kind, PlacedKind::Word | PlacedKind::Run))
        .filter(|item| !item.out_rect.is_empty() && !item.src.rect.is_empty())
        .map(|item| ReflowWord {
            src_page: item.src.page_index,
            src_rect: item.src.rect,
            out_rect: item.out_rect,
        })
        .collect();
    if placements.is_empty() {
        return Ok(None);
    }

    // Group placements by source text row: words from one row share
    // (source page, source y, source height). A `BTreeMap` keeps the row order
    // deterministic.
    let mut groups: std::collections::BTreeMap<(usize, u32, u32), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, w) in placements.iter().enumerate() {
        groups
            .entry((w.src_page, w.src_rect.y, w.src_rect.h))
            .or_default()
            .push(i);
    }

    // Build one OCR job per source row: a gray crop spanning all its words and
    // the per-word source x-ranges (relative to the crop origin).
    let mut jobs: Vec<RowJob> = Vec::new();
    for indices in groups.values() {
        let first = &placements[indices[0]];
        let Some(source) = sources.get(first.src_page) else {
            continue;
        };
        let gray = &source.gray;
        let y0 = first.src_rect.y;
        let h = first.src_rect.h;
        let mut x0 = u32::MAX;
        let mut x1 = 0u32;
        for &idx in indices {
            let r = placements[idx].src_rect;
            x0 = x0.min(r.x);
            x1 = x1.max(r.right());
        }
        if x1 <= x0 || h == 0 || x1 > gray.width() || y0.saturating_add(h) > gray.height() {
            continue;
        }
        let crop = image::imageops::crop_imm(gray, x0, y0, x1 - x0, h).to_image();
        let mut slots: Vec<WordSlot> = indices
            .iter()
            .map(|&idx| {
                let r = placements[idx].src_rect;
                WordSlot {
                    placement_index: idx,
                    local_x0: r.x.saturating_sub(x0),
                    local_x1: r.right().saturating_sub(x0),
                }
            })
            .collect();
        slots.sort_by_key(|s| s.local_x0);
        jobs.push(RowJob { crop, slots });
    }
    if jobs.is_empty() {
        return Ok(None);
    }

    let language = config.ocr_language().to_string();

    // OCR is synchronous and the engine is cached per worker thread, so run all
    // row crops on the blocking pool. Returns (placement_index, text, conf).
    let recognized: Vec<(usize, String, Option<f32>)> = tokio::task::spawn_blocking(move || {
        PIPELINES.with(|cell| {
            let mut pipelines = cell.borrow_mut();
            let pipeline = pipelines.entry(language.clone()).or_insert_with(|| {
                OcrPipeline::new(SlowOcrConfig {
                    language: language.clone(),
                    debug: false,
                    debug_out_dir: None,
                    ..Default::default()
                })
            });
            let mut out: Vec<(usize, String, Option<f32>)> = Vec::new();
            for job in &jobs {
                if let Some(line) = pipeline.ocr_gray_line(&job.crop) {
                    assign_words_to_slots(&line, &job.slots, &mut out);
                }
            }
            out
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("reflow OCR task panicked: {e}"))?;

    // Attach recognized text to the reflowed output rectangles.
    let mut out_words: Vec<OutputWord> = recognized
        .into_iter()
        .filter(|(_, text, _)| !text.trim().is_empty())
        .map(|(idx, text, confidence)| OutputWord {
            rect: placements[idx].out_rect,
            text,
            confidence,
        })
        .collect();
    if out_words.is_empty() {
        return Ok(None);
    }

    // Order top-to-bottom, left-to-right and group into output lines so the
    // text layer reads naturally for selection / search.
    out_words.sort_by_key(|w| (w.rect.y, w.rect.x));
    let lines = group_output_lines(out_words);
    let hocr = hocr::build_page_hocr(&lines, page.width, page.height);
    Ok(Some(hocr))
}

/// Distribute a recognized line's words back onto the row's word slots by
/// best source-x overlap, accumulating into `out` as (placement_index, text,
/// average confidence). Both segmentations are gap-based over the same pixels,
/// so x-overlap reliably matches OCR words to reflow words even when the counts
/// differ (a merged OCR word goes to the slot it overlaps most; multiple OCR
/// words in one slot are concatenated).
fn assign_words_to_slots(
    line: &OcrLineResult,
    slots: &[WordSlot],
    out: &mut Vec<(usize, String, Option<f32>)>,
) {
    if slots.is_empty() {
        return;
    }
    let mut texts: Vec<String> = vec![String::new(); slots.len()];
    let mut conf_sum: Vec<f32> = vec![0.0; slots.len()];
    let mut conf_cnt: Vec<u32> = vec![0; slots.len()];

    if line.words.is_empty() {
        // No word-level boxes: drop the whole line onto the leftmost slot.
        let t = line.text.trim();
        if !t.is_empty() {
            texts[0].push_str(t);
            if let Some(c) = line.confidence {
                conf_sum[0] += c;
                conf_cnt[0] += 1;
            }
        }
    } else {
        for word in &line.words {
            let t = word.text.trim();
            if t.is_empty() {
                continue;
            }
            let wx0 = word.bbox_crop_local[0];
            let wx1 = word.bbox_crop_local[2];
            let mut best = 0usize;
            let mut best_overlap = 0i64;
            for (si, slot) in slots.iter().enumerate() {
                let lo = wx0.max(slot.local_x0);
                let hi = wx1.min(slot.local_x1);
                let overlap = hi as i64 - lo as i64;
                if overlap > best_overlap {
                    best_overlap = overlap;
                    best = si;
                }
            }
            // No overlap with any slot: fall back to the nearest slot center.
            if best_overlap <= 0 {
                let center = (wx0 + wx1) / 2;
                best = slots
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, s)| {
                        let c = (s.local_x0 + s.local_x1) / 2;
                        (c as i64 - center as i64).abs()
                    })
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
            if !texts[best].is_empty() {
                texts[best].push(' ');
            }
            texts[best].push_str(t);
            if let Some(c) = word.confidence {
                conf_sum[best] += c;
                conf_cnt[best] += 1;
            }
        }
    }

    for (si, slot) in slots.iter().enumerate() {
        if texts[si].trim().is_empty() {
            continue;
        }
        let conf = (conf_cnt[si] > 0).then(|| conf_sum[si] / conf_cnt[si] as f32);
        out.push((slot.placement_index, std::mem::take(&mut texts[si]), conf));
    }
}

/// Group output-positioned words (pre-sorted by `(y, x)`) into hOCR lines,
/// starting a new line when a word's vertical span no longer overlaps the
/// current line's band by at least half its height.
fn group_output_lines(words: Vec<OutputWord>) -> Vec<OcrLineResult> {
    let mut lines: Vec<OcrLineResult> = Vec::new();
    let mut current: Vec<OutputWord> = Vec::new();
    let mut band_top = 0u32;
    let mut band_bot = 0u32;

    for w in words {
        let wt = w.rect.y;
        let wb = w.rect.bottom();
        let start_new = !current.is_empty() && {
            let overlap = band_bot.min(wb).saturating_sub(band_top.max(wt));
            let h = wb.saturating_sub(wt).max(1);
            (overlap as f32) < 0.5 * (h as f32)
        };
        if start_new {
            lines.push(finish_line(std::mem::take(&mut current)));
        }
        if current.is_empty() {
            band_top = wt;
            band_bot = wb;
        } else {
            band_top = band_top.min(wt);
            band_bot = band_bot.max(wb);
        }
        current.push(w);
    }
    if !current.is_empty() {
        lines.push(finish_line(current));
    }
    lines
}

/// Assemble one output line's words into an `OcrLineResult`. Word boxes are
/// stored crop-local (relative to the line origin) because `hocr::build_line_hocr`
/// offsets them by the line's `bbox_highres` origin.
fn finish_line(mut words: Vec<OutputWord>) -> OcrLineResult {
    words.sort_by_key(|w| w.rect.x);
    let mut x0 = u32::MAX;
    let mut y0 = u32::MAX;
    let mut x1 = 0u32;
    let mut y1 = 0u32;
    for w in &words {
        x0 = x0.min(w.rect.x);
        y0 = y0.min(w.rect.y);
        x1 = x1.max(w.rect.right());
        y1 = y1.max(w.rect.bottom());
    }
    let text = words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let ocr_words = words
        .into_iter()
        .map(|w| OcrWord {
            bbox_crop_local: [
                w.rect.x.saturating_sub(x0),
                w.rect.y.saturating_sub(y0),
                w.rect.right().saturating_sub(x0),
                w.rect.bottom().saturating_sub(y0),
            ],
            text: w.text,
            confidence: w.confidence,
        })
        .collect();
    OcrLineResult {
        text,
        confidence: None,
        words: ocr_words,
        bbox_highres: [x0, y0, x1, y1],
    }
}

#[cfg(test)]
mod reflow_ocr_tests {
    use super::*;

    fn ocr_word(text: &str, x0: u32, x1: u32, conf: f32) -> OcrWord {
        OcrWord {
            text: text.to_string(),
            bbox_crop_local: [x0, 0, x1, 10],
            confidence: Some(conf),
        }
    }

    fn slot(placement_index: usize, x0: u32, x1: u32) -> WordSlot {
        WordSlot {
            placement_index,
            local_x0: x0,
            local_x1: x1,
        }
    }

    fn out_word(x: u32, y: u32, w: u32, h: u32, text: &str) -> OutputWord {
        OutputWord {
            rect: PxRect::new(x, y, w, h),
            text: text.to_string(),
            confidence: None,
        }
    }

    #[test]
    fn assigns_recognized_words_to_slots_by_x_overlap() {
        let line = OcrLineResult {
            text: "the cat".to_string(),
            confidence: Some(0.9),
            words: vec![ocr_word("the", 0, 28, 0.9), ocr_word("cat", 40, 70, 0.8)],
            bbox_highres: [0, 0, 70, 10],
        };
        let slots = vec![slot(5, 0, 30), slot(9, 38, 72)];
        let mut out = Vec::new();
        assign_words_to_slots(&line, &slots, &mut out);
        out.sort_by_key(|(i, _, _)| *i);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], (5, "the".to_string(), Some(0.9)));
        assert_eq!(out[1].0, 9);
        assert_eq!(out[1].1, "cat");
    }

    #[test]
    fn merged_recognized_word_goes_to_best_overlap_slot() {
        // One OCR word spanning both word slots is assigned to the slot it
        // overlaps most (40px vs 9px), not split.
        let line = OcrLineResult {
            text: "hello".to_string(),
            confidence: None,
            words: vec![ocr_word("hello", 0, 50, 0.5)],
            bbox_highres: [0, 0, 50, 10],
        };
        let slots = vec![slot(1, 0, 40), slot(2, 41, 60)];
        let mut out = Vec::new();
        assign_words_to_slots(&line, &slots, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 1);
        assert_eq!(out[0].1, "hello");
    }

    #[test]
    fn wordless_line_drops_text_on_first_slot() {
        let line = OcrLineResult {
            text: "fallback".to_string(),
            confidence: Some(0.4),
            words: Vec::new(),
            bbox_highres: [0, 0, 80, 10],
        };
        let slots = vec![slot(7, 0, 80)];
        let mut out = Vec::new();
        assign_words_to_slots(&line, &slots, &mut out);
        assert_eq!(out, vec![(7, "fallback".to_string(), Some(0.4))]);
    }

    #[test]
    fn groups_output_words_into_lines_by_vertical_band() {
        let words = vec![
            out_word(0, 0, 20, 10, "a"),
            out_word(25, 0, 20, 10, "b"),
            out_word(0, 30, 20, 10, "c"),
        ];
        let lines = group_output_lines(words);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "a b");
        assert_eq!(lines[0].words.len(), 2);
        assert_eq!(lines[1].text, "c");
    }

    #[test]
    fn finish_line_emits_line_local_word_boxes() {
        let line = finish_line(vec![
            out_word(40, 100, 20, 10, "y"),
            out_word(10, 100, 20, 10, "x"),
        ]);
        // Words are sorted left-to-right and the line bbox is their union.
        assert_eq!(line.bbox_highres, [10, 100, 60, 110]);
        assert_eq!(line.text, "x y");
        // Word boxes are relative to the line origin (10, 100).
        assert_eq!(line.words[0].bbox_crop_local, [0, 0, 20, 10]);
        assert_eq!(line.words[1].bbox_crop_local, [30, 0, 50, 10]);
    }
}
