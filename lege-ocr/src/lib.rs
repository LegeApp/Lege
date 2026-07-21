pub mod coordinate;
pub mod debug;
pub mod document;
pub mod engine;
#[cfg(feature = "paddle-ocr")]
pub mod engine_paddle;
pub mod fast;
pub mod hocr;
pub mod normalize;
pub mod reading_order;
pub mod segment;
pub mod types;

pub use types::OcrResult;

use std::path::Path;

use anyhow::Result;
use coordinate::CoordinateMap;
use engine::OcrEngine;
use image::RgbImage;
use segment::SegmentParams;
use types::{OcrLineResult, SegmentationConfidence, SlowOcrPage, TextRegion};

/// Controls which expensive page-level artifacts the slow OCR pipeline builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowOcrOutputMode {
    /// Build structured line/block data and a page hOCR document.
    Hocr,
    /// Build structured line/block data only. Debug output may still write hOCR.
    Structured,
}

/// Parameters controlling the slow OCR pipeline.
#[derive(Debug, Clone)]
pub struct SlowOcrConfig {
    pub segment: SegmentParams,
    /// OCR language code (e.g. "eng")
    pub language: String,
    /// If true, write debug files under `debug_out_dir`
    pub debug: bool,
    pub debug_out_dir: Option<std::path::PathBuf>,
    /// Minimum confidence level required to use line-segmented OCR; pages below
    /// this fall back to region-level OCR on the full bbox.
    pub min_segment_confidence: SegmentationConfidence,
    /// Controls whether page-level hOCR is built for the returned page.
    pub output_mode: SlowOcrOutputMode,
    /// True when the `process_page` binary argument is already a normalized
    /// analysis mask with 0=ink and 255=background.
    pub binary_is_analysis_mask: bool,
}

impl Default for SlowOcrConfig {
    fn default() -> Self {
        Self {
            segment: SegmentParams::default(),
            language: "eng".to_string(),
            debug: false,
            debug_out_dir: None,
            min_segment_confidence: SegmentationConfidence::Medium,
            output_mode: SlowOcrOutputMode::Hocr,
            binary_is_analysis_mask: false,
        }
    }
}

/// The slow OCR pipeline. Construct once (expensive: initialises the OCR engine),
/// then call `process_page` for each page.
pub struct OcrPipeline {
    engine: Box<dyn OcrEngine>,
    config: SlowOcrConfig,
}

impl OcrPipeline {
    /// Create a pipeline with the platform default OCR engine.
    pub fn new(config: SlowOcrConfig) -> Self {
        Self {
            engine: engine::default_engine(),
            config,
        }
    }

    /// Create a pipeline with a custom engine (useful for testing).
    pub fn with_engine(engine: Box<dyn OcrEngine>, config: SlowOcrConfig) -> Self {
        Self { engine, config }
    }

    /// OCR a single pre-cropped grayscale line image.
    ///
    /// Unlike [`process_page`], this skips layout detection and line
    /// segmentation: the caller has already isolated one text line (for example
    /// a source text-row already detected by the reflow pipeline). Word
    /// bounding boxes in the returned result are in the crop's own pixel space
    /// (crop-local), and `bbox_highres` spans the whole crop. Returns `None`
    /// when the engine produces no usable text.
    pub fn ocr_gray_line(&self, gray_img: &image::GrayImage) -> Option<OcrLineResult> {
        if gray_img.width() == 0 || gray_img.height() == 0 {
            return None;
        }
        let (gray_ocr, scale_x, scale_y) = normalize::prepare_line_for_ocr(gray_img, false);
        match self.engine.ocr_line(&gray_ocr, &self.config.language) {
            Ok(mut result) if !result.text.trim().is_empty() => {
                rescale_word_bboxes(
                    &mut result,
                    scale_x,
                    scale_y,
                    gray_img.width(),
                    gray_img.height(),
                );
                result.bbox_highres = [0, 0, gray_img.width(), gray_img.height()];
                Some(result)
            }
            _ => None,
        }
    }

    /// Process a single page.
    ///
    /// `high_res`: the page rendered at full resolution (same pixel space as `binary`).
    /// `binary`: binary or grayscale page analysis buffer; same dimensions as
    /// `high_res`. It is normalized to 0=ink, 255=background before use.
    /// `regions`: text regions already in `high_res` pixel space (post coordinate-mapping).
    /// `coord_map`: coordinate transform for this page (used to produce PDF-space bboxes in hOCR).
    pub fn process_page(
        &self,
        high_res: &RgbImage,
        binary: &[u8],
        regions: &[TextRegion],
        _coord_map: &CoordinateMap,
        page_index: usize,
    ) -> Result<SlowOcrPage> {
        let page_w = high_res.width();
        let page_h = high_res.height();
        let expected = page_w as usize * page_h as usize;
        let analysis_binary = if self.config.binary_is_analysis_mask && binary.len() == expected {
            binary.to_vec()
        } else {
            normalize::build_analysis_binary(binary, high_res)
        };

        let debug_dir = if self.config.debug {
            let base = self
                .config
                .debug_out_dir
                .as_deref()
                .unwrap_or(Path::new("debug"));
            Some(debug::make_page_debug_dir(base, page_index)?)
        } else {
            None
        };

        // For each text region: segment into lines, OCR each line. Lines are
        // kept grouped per region so reading order can be reconstructed across
        // regions (column-aware) without interleaving columns.
        let mut per_region_lines: Vec<Vec<OcrLineResult>> = Vec::with_capacity(regions.len());
        for region in regions {
            let region_lines = self.process_region(
                high_res,
                &analysis_binary,
                region,
                page_w,
                page_h,
                debug_dir.as_deref(),
            )?;
            per_region_lines.push(region_lines);
        }

        // Order regions into reading order (handles multi-column / full-width
        // bands), then emit each region's lines top-to-bottom within the region.
        let region_order = reading_order::order_regions(regions, page_w);
        let mut all_lines: Vec<OcrLineResult> = Vec::new();
        let mut blocks: Vec<document::TextBlock> = Vec::new();
        for &ri in &region_order {
            let mut lines = std::mem::take(&mut per_region_lines[ri]);
            lines.sort_by_key(|l| (l.bbox_highres[1], l.bbox_highres[0]));

            // One detected region == one block (≈ paragraph). Preserve the
            // region's class so downstream output can distinguish headings,
            // captions, footnotes, etc.
            let region = &regions[ri];
            let block_bbox = line_union_bbox(&lines).unwrap_or(region.bbox_highres);
            let block_lines: Vec<document::TextLine> = lines
                .iter()
                .map(|l| document::TextLine {
                    text: l.text.clone(),
                    bbox: l.bbox_highres,
                    words: l
                        .words
                        .iter()
                        .map(|word| {
                            let line_w = l.bbox_highres[2].saturating_sub(l.bbox_highres[0]);
                            let line_h = l.bbox_highres[3].saturating_sub(l.bbox_highres[1]);
                            document::TextWord {
                                text: word.text.clone(),
                                bbox: [
                                    l.bbox_highres[0]
                                        .saturating_add(word.bbox_crop_local[0].min(line_w)),
                                    l.bbox_highres[1]
                                        .saturating_add(word.bbox_crop_local[1].min(line_h)),
                                    l.bbox_highres[0]
                                        .saturating_add(word.bbox_crop_local[2].min(line_w)),
                                    l.bbox_highres[1]
                                        .saturating_add(word.bbox_crop_local[3].min(line_h)),
                                ],
                                confidence: word.confidence,
                            }
                        })
                        .collect(),
                })
                .collect();
            if block_lines.iter().any(|l| !l.text.trim().is_empty()) {
                blocks.push(document::TextBlock {
                    kind: document::BlockKind::from_class_name(
                        region.class_name.as_deref().unwrap_or("text"),
                    ),
                    bbox: block_bbox,
                    lines: block_lines,
                    indent_px: 0,
                    spacing_before_px: 0,
                    spacing_after_px: 0,
                });
            }

            all_lines.extend(lines);
        }

        let should_build_hocr =
            self.config.output_mode == SlowOcrOutputMode::Hocr || self.config.debug;
        let hocr = if should_build_hocr {
            hocr::build_page_hocr(&all_lines, page_w, page_h)
        } else {
            String::new()
        };

        if let Some(ref dir) = debug_dir {
            debug::save_page_hocr(&dir.join("page.hocr"), &hocr)?;
            debug::save_overlay_svg(
                &dir.join("page_overlay.svg"),
                page_w,
                page_h,
                regions,
                &all_lines,
            )?;
        }

        Ok(SlowOcrPage {
            page_index,
            hocr,
            regions: regions.to_vec(),
            lines: all_lines,
            blocks,
        })
    }

    fn process_region(
        &self,
        high_res: &RgbImage,
        binary: &[u8],
        region: &TextRegion,
        page_w: u32,
        page_h: u32,
        debug_dir: Option<&Path>,
    ) -> Result<Vec<OcrLineResult>> {
        let rid = region.region_id;
        let bbox = region.bbox_highres;

        // 1. Extract binary region crop for segmentation
        let (region_bin, rw, rh) = normalize::extract_binary_crop(binary, page_w, page_h, bbox)?;

        // 2. Line segmentation on binary crop
        // The segmentation operates in the coordinate space of the full page binary image
        let seg = segment::segment_region(binary, page_w, page_h, bbox, &self.config.segment);

        let use_line_seg = region_supports_line_segmentation(region)
            && seg
                .confidence
                .meets_minimum(&self.config.min_segment_confidence);

        if let Some(dir) = debug_dir {
            let _ = debug::save_binary_crop(
                &dir.join(format!("region_{rid:03}_binary.png")),
                &region_bin,
                rw,
                rh,
            );
            // Projection CSV
            let proj = segment::horizontal_projection(
                binary,
                page_w as usize,
                bbox[0] as usize,
                bbox[1] as usize,
                rw as usize,
                rh as usize,
            );
            let smoothed: Vec<f32> = {
                // Recompute smooth; this is debug-only so duplication is fine
                let n = proj.len();
                let r = self.config.segment.smooth_radius;
                (0..n)
                    .map(|i| {
                        let lo = i.saturating_sub(r);
                        let hi = (i + r + 1).min(n);
                        let s: u32 = proj[lo..hi].iter().sum();
                        s as f32 / (hi - lo) as f32
                    })
                    .collect()
            };
            let _ = debug::save_projection_csv(
                &dir.join(format!("region_{rid:03}_projection.csv")),
                &proj,
                &smoothed,
            );
            let _ = debug::save_lines_annotated(
                &dir.join(format!("region_{rid:03}_lines.png")),
                &region_bin,
                rw,
                rh,
                &seg,
                bbox[0],
                bbox[1],
            );
        }

        // Low-confidence or structurally complex regions must use the engine's
        // multi-line/block mode. Treating them as one line destroys layout and
        // forces unrelated glyphs to compete with each other.
        if !use_line_seg {
            let region_lines = self.ocr_region_bbox(
                high_res,
                bbox,
                rid,
                region.class_name.as_deref() == Some("page"),
            );
            return Ok(if region_supports_line_refinement(region) {
                self.refine_detected_lines(
                    high_res,
                    binary,
                    page_w,
                    page_h,
                    region_lines,
                    rid,
                    debug_dir,
                )
            } else {
                region_lines
            });
        }

        let line_bboxes: Vec<[u32; 4]> = seg
            .line_bboxes
            .iter()
            .map(|&line_bbox| {
                normalize::tighten_bbox_to_ink(binary, page_w, page_h, line_bbox)
                    .unwrap_or(line_bbox)
            })
            .collect();

        // 4. OCR each line crop
        let mut results: Vec<OcrLineResult> = Vec::new();
        let mut failed_lines = 0usize;
        for (li, &line_bbox) in line_bboxes.iter().enumerate() {
            match self.ocr_line_bbox(
                high_res, binary, page_w, page_h, line_bbox, rid, li, debug_dir, None,
            ) {
                Some(ocr_result) => results.push(ocr_result),
                None => failed_lines += 1,
            }
        }

        // Recover ink-bearing lines that line mode could not recognize. Prefer
        // non-overlapping region-mode lines; if most lines failed and the
        // backend can only return one block, favor completeness over partial
        // line-mode output.
        if failed_lines > 0 {
            let recovery = self.ocr_region_bbox(high_res, bbox, rid, false);
            results = merge_region_recovery(results, recovery, failed_lines, line_bboxes.len());
        }

        Ok(results)
    }

    #[allow(clippy::too_many_arguments)]
    fn ocr_line_bbox(
        &self,
        high_res: &RgbImage,
        binary: &[u8],
        page_w: u32,
        page_h: u32,
        line_bbox: [u32; 4],
        region_id: usize,
        line_index: usize,
        debug_dir: Option<&Path>,
        reference: Option<OcrLineResult>,
    ) -> Option<OcrLineResult> {
        let (gray_img, _, _) = match normalize::extract_gray_crop(high_res, line_bbox) {
            Ok(crop) => crop,
            Err(e) => {
                log::warn!("OCR crop failed for region {region_id} line {line_index}: {e}");
                return None;
            }
        };

        if let Some(dir) = debug_dir {
            let _ = debug::save_gray_crop(
                &dir.join(format!("line_{region_id:03}_{line_index:03}_gray.png")),
                &gray_img,
            );
        }

        let (gray_ocr, gray_scale_x, gray_scale_y) =
            normalize::prepare_line_for_ocr(&gray_img, false);
        let first = self.engine.ocr_line(&gray_ocr, &self.config.language);
        let grayscale_result = match first {
            Ok(mut result) if !result.text.trim().is_empty() => {
                rescale_word_bboxes(
                    &mut result,
                    gray_scale_x,
                    gray_scale_y,
                    gray_img.width(),
                    gray_img.height(),
                );
                Some(result)
            }
            Ok(_) => None,
            Err(e) => {
                log::warn!("OCR failed for region {region_id} line {line_index}: {e}");
                None
            }
        };

        // Evaluate a clean binary variant only for engines that can use it to
        // choose a materially better result. WinOCR takes the grayscale path;
        // Tesseract keeps the confidence-based retry.
        let mut binary_result = None;
        if self.engine.use_binary_line_retry()
            && !gray_img.as_raw().iter().all(|&p| p == 0 || p == 255)
            && let Ok((binary_crop, width, height)) =
                normalize::extract_binary_crop(binary, page_w, page_h, line_bbox)
            && let Some(binary_img) = image::GrayImage::from_raw(width, height, binary_crop)
        {
            if let Some(dir) = debug_dir {
                let _ = debug::save_gray_crop(
                    &dir.join(format!("line_{region_id:03}_{line_index:03}_binary.png")),
                    &binary_img,
                );
            }
            let (binary_ocr, binary_scale_x, binary_scale_y) =
                normalize::prepare_line_for_ocr(&binary_img, true);
            match self.engine.ocr_line(&binary_ocr, &self.config.language) {
                Ok(mut retry) if !retry.text.trim().is_empty() => {
                    rescale_word_bboxes(
                        &mut retry,
                        binary_scale_x,
                        binary_scale_y,
                        binary_img.width(),
                        binary_img.height(),
                    );
                    binary_result = Some(retry);
                }
                Ok(_) => {}
                Err(e) => log::warn!(
                    "Binary OCR retry failed for region {region_id} line {line_index}: {e}"
                ),
            }
        }

        let mut result = match reference {
            Some(reference) => {
                choose_refined_ocr_variant(reference, grayscale_result, binary_result)
            }
            None => choose_ocr_variant(grayscale_result, binary_result)?,
        };
        result.bbox_highres = line_bbox;
        if let Some(dir) = debug_dir {
            let _ = debug::save_ocr_result_json(
                &dir.join(format!("line_{region_id:03}_{line_index:03}_ocr.json")),
                &result,
            );
        }
        Some(result)
    }

    fn ocr_region_bbox(
        &self,
        high_res: &RgbImage,
        bbox: [u32; 4],
        region_id: usize,
        page_mode: bool,
    ) -> Vec<OcrLineResult> {
        let Ok((gray_img, _, _)) = normalize::extract_gray_crop(high_res, bbox) else {
            return Vec::new();
        };
        let call = if page_mode {
            self.engine.ocr_page(&gray_img, &self.config.language)
        } else {
            self.engine.ocr_region(&gray_img, &self.config.language)
        };
        let region_results = match call {
            Ok(results) => results,
            Err(e) => {
                log::warn!("Region OCR failed for region {region_id}: {e}");
                return Vec::new();
            }
        };

        region_results
            .into_iter()
            .filter_map(|mut line| {
                if line.text.trim().is_empty() {
                    return None;
                }
                line.bbox_highres = offset_local_bbox(line.bbox_highres, bbox);
                Some(line)
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn refine_detected_lines(
        &self,
        high_res: &RgbImage,
        binary: &[u8],
        page_w: u32,
        page_h: u32,
        detected_lines: Vec<OcrLineResult>,
        region_id: usize,
        debug_dir: Option<&Path>,
    ) -> Vec<OcrLineResult> {
        detected_lines
            .into_iter()
            .enumerate()
            .map(|(line_index, mut detected)| {
                let Some(line_bbox) =
                    normalize::tighten_bbox_to_ink(binary, page_w, page_h, detected.bbox_highres)
                else {
                    return detected;
                };
                remap_line_bbox(&mut detected, line_bbox);
                let fallback = detected.clone();
                self.ocr_line_bbox(
                    high_res,
                    binary,
                    page_w,
                    page_h,
                    line_bbox,
                    region_id,
                    line_index,
                    debug_dir,
                    Some(detected),
                )
                .unwrap_or(fallback)
            })
            .collect()
    }
}

fn line_union_bbox(lines: &[OcrLineResult]) -> Option<[u32; 4]> {
    let mut left = u32::MAX;
    let mut top = u32::MAX;
    let mut right = 0;
    let mut bottom = 0;
    let mut found = false;
    for line in lines.iter().filter(|line| !line.text.trim().is_empty()) {
        if line.bbox_highres[2] <= line.bbox_highres[0]
            || line.bbox_highres[3] <= line.bbox_highres[1]
        {
            continue;
        }
        found = true;
        left = left.min(line.bbox_highres[0]);
        top = top.min(line.bbox_highres[1]);
        right = right.max(line.bbox_highres[2]);
        bottom = bottom.max(line.bbox_highres[3]);
    }
    found.then_some([left, top, right, bottom])
}

fn region_supports_line_segmentation(region: &TextRegion) -> bool {
    !matches!(
        region.class_name.as_deref(),
        Some(
            "table"
                | "isolate_formula"
                | "formula"
                | "algorithm"
                | "seal"
                | "chart"
                | "formula_number"
                | "page"
        )
    )
}

fn region_supports_line_refinement(region: &TextRegion) -> bool {
    !matches!(
        region.class_name.as_deref(),
        Some(
            "table"
                | "isolate_formula"
                | "formula"
                | "algorithm"
                | "seal"
                | "chart"
                | "formula_number"
        )
    )
}

fn offset_local_bbox(local: [u32; 4], parent: [u32; 4]) -> [u32; 4] {
    let parent_w = parent[2].saturating_sub(parent[0]);
    let parent_h = parent[3].saturating_sub(parent[1]);
    if local[2] <= local[0] || local[3] <= local[1] {
        return parent;
    }
    [
        parent[0] + local[0].min(parent_w),
        parent[1] + local[1].min(parent_h),
        parent[0] + local[2].min(parent_w),
        parent[1] + local[3].min(parent_h),
    ]
}

fn merge_region_recovery(
    mut line_results: Vec<OcrLineResult>,
    region_results: Vec<OcrLineResult>,
    failed_lines: usize,
    expected_lines: usize,
) -> Vec<OcrLineResult> {
    if line_results.is_empty() {
        return region_results;
    }
    if region_results.is_empty() {
        return line_results;
    }

    let replacement = region_results.clone();
    for candidate in region_results {
        let overlaps_existing = line_results.iter().any(|existing| {
            vertical_overlap_ratio(candidate.bbox_highres, existing.bbox_highres) >= 0.45
        });
        if !overlaps_existing {
            line_results.push(candidate);
        }
    }

    if failed_lines.saturating_mul(2) >= expected_lines
        && line_results.len() < expected_lines.saturating_sub(failed_lines) + 1
    {
        replacement
    } else {
        line_results
    }
}

fn vertical_overlap_ratio(a: [u32; 4], b: [u32; 4]) -> f32 {
    let overlap = a[3].min(b[3]).saturating_sub(a[1].max(b[1]));
    let smaller = a[3].saturating_sub(a[1]).min(b[3].saturating_sub(b[1]));
    if smaller == 0 {
        0.0
    } else {
        overlap as f32 / smaller as f32
    }
}

fn rescale_word_bboxes(
    result: &mut OcrLineResult,
    scale_x: f32,
    scale_y: f32,
    original_width: u32,
    original_height: u32,
) {
    if scale_x == 1.0 && scale_y == 1.0 {
        return;
    }
    for word in &mut result.words {
        word.bbox_crop_local = [
            (word.bbox_crop_local[0] as f32 * scale_x).floor() as u32,
            (word.bbox_crop_local[1] as f32 * scale_y).floor() as u32,
            ((word.bbox_crop_local[2] as f32 * scale_x).ceil() as u32).min(original_width),
            ((word.bbox_crop_local[3] as f32 * scale_y).ceil() as u32).min(original_height),
        ];
    }
}

fn remap_line_bbox(result: &mut OcrLineResult, new_bbox: [u32; 4]) {
    let old_bbox = result.bbox_highres;
    let new_width = new_bbox[2].saturating_sub(new_bbox[0]);
    let new_height = new_bbox[3].saturating_sub(new_bbox[1]);
    for word in &mut result.words {
        let page_bbox = [
            old_bbox[0].saturating_add(word.bbox_crop_local[0]),
            old_bbox[1].saturating_add(word.bbox_crop_local[1]),
            old_bbox[0].saturating_add(word.bbox_crop_local[2]),
            old_bbox[1].saturating_add(word.bbox_crop_local[3]),
        ];
        word.bbox_crop_local = [
            page_bbox[0].saturating_sub(new_bbox[0]).min(new_width),
            page_bbox[1].saturating_sub(new_bbox[1]).min(new_height),
            page_bbox[2].saturating_sub(new_bbox[0]).min(new_width),
            page_bbox[3].saturating_sub(new_bbox[1]).min(new_height),
        ];
    }
    result.bbox_highres = new_bbox;
}

fn choose_ocr_variant(
    grayscale: Option<OcrLineResult>,
    binary: Option<OcrLineResult>,
) -> Option<OcrLineResult> {
    match (grayscale, binary) {
        (None, None) => None,
        (Some(result), None) | (None, Some(result)) => Some(result),
        (Some(grayscale), Some(binary)) => {
            match (
                effective_confidence(&grayscale),
                effective_confidence(&binary),
            ) {
                (Some(gray), Some(bin)) if bin > gray => Some(binary),
                (None, Some(bin)) if bin >= 0.75 => Some(binary),
                _ => Some(grayscale),
            }
        }
    }
}

fn choose_refined_ocr_variant(
    reference: OcrLineResult,
    grayscale: Option<OcrLineResult>,
    binary: Option<OcrLineResult>,
) -> OcrLineResult {
    let mut candidates = vec![reference];
    candidates.extend(grayscale);
    candidates.extend(binary);

    if candidates.len() == 1 {
        return candidates.remove(0);
    }

    // Confidence is only comparable to the reference when the reference has a
    // value too. Otherwise, require three-pass character-level agreement; two
    // conflicting confidence-less readings provide no evidence for replacing
    // the backend's original line.
    if let Some(reference_confidence) = effective_confidence(&candidates[0]) {
        let mut best_index = 0usize;
        let mut best_confidence = reference_confidence;
        for (index, candidate) in candidates.iter().enumerate().skip(1) {
            if let Some(confidence) = effective_confidence(candidate)
                && confidence > best_confidence
            {
                best_index = index;
                best_confidence = confidence;
            }
        }
        if best_index != 0 {
            return candidates.swap_remove(best_index);
        }
    }

    if candidates.len() < 3 {
        return candidates.swap_remove(0);
    }

    let comparison_texts: Vec<String> = candidates
        .iter()
        .map(|candidate| normalize_comparison_text(&candidate.text))
        .collect();
    let mut best_index = 0usize;
    let mut best_score = f32::INFINITY;
    for index in 0..candidates.len() {
        let score = (0..candidates.len())
            .filter(|&other| other != index)
            .map(|other| {
                normalized_edit_distance(&comparison_texts[index], &comparison_texts[other])
            })
            .sum::<f32>();
        // Strict comparison deliberately favors the reference on a tie.
        if score < best_score {
            best_index = index;
            best_score = score;
        }
    }
    candidates.swap_remove(best_index)
}

fn normalize_comparison_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalized_edit_distance(left: &str, right: &str) -> f32 {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let max_len = left.len().max(right.len());
    if max_len == 0 {
        return 0.0;
    }

    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (left_index, left_char) in left.iter().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right.iter().enumerate() {
            current[right_index + 1] = if left_char == right_char {
                previous[right_index]
            } else {
                1 + previous[right_index]
                    .min(current[right_index])
                    .min(previous[right_index + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[right.len()] as f32 / max_len as f32
}

fn effective_confidence(result: &OcrLineResult) -> Option<f32> {
    result.confidence.or_else(|| {
        let mut sum = 0.0;
        let mut count = 0usize;
        for confidence in result.words.iter().filter_map(|word| word.confidence) {
            sum += confidence;
            count += 1;
        }
        (count > 0).then_some(sum / count as f32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Rgb};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use types::OcrResult;

    struct MockEngine {
        line_calls: Arc<AtomicUsize>,
        region_calls: Arc<AtomicUsize>,
        binary_line_text: Option<&'static str>,
        region_lines: Vec<OcrLineResult>,
    }

    impl OcrEngine for MockEngine {
        fn run_image(
            &self,
            _data: &[u8],
            _width: usize,
            _height: usize,
            _is_binary: bool,
            _lang: &str,
        ) -> Option<OcrResult> {
            None
        }

        fn ocr_line(&self, image: &GrayImage, _lang: &str) -> Result<OcrLineResult> {
            self.line_calls.fetch_add(1, Ordering::SeqCst);
            let is_binary = image.as_raw().iter().all(|&p| p == 0 || p == 255);
            Ok(OcrLineResult {
                text: if is_binary {
                    self.binary_line_text.unwrap_or_default().to_string()
                } else {
                    String::new()
                },
                confidence: None,
                words: Vec::new(),
                bbox_highres: [0, 0, image.width(), image.height()],
            })
        }

        fn ocr_region(&self, _image: &GrayImage, _lang: &str) -> Result<Vec<OcrLineResult>> {
            self.region_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.region_lines.clone())
        }

        fn name(&self) -> &'static str {
            "mock"
        }

        fn use_binary_line_retry(&self) -> bool {
            true
        }
    }

    fn line(text: &str, bbox: [u32; 4]) -> OcrLineResult {
        OcrLineResult {
            text: text.to_string(),
            confidence: None,
            words: Vec::new(),
            bbox_highres: bbox,
        }
    }

    #[test]
    fn page_region_uses_multiline_ocr_and_offsets_results() {
        let line_calls = Arc::new(AtomicUsize::new(0));
        let region_calls = Arc::new(AtomicUsize::new(0));
        let engine = MockEngine {
            line_calls: line_calls.clone(),
            region_calls: region_calls.clone(),
            binary_line_text: None,
            region_lines: vec![line("one", [2, 3, 30, 12]), line("two", [2, 20, 30, 29])],
        };
        let pipeline = OcrPipeline::with_engine(Box::new(engine), SlowOcrConfig::default());
        let image = RgbImage::from_pixel(100, 60, Rgb([255, 255, 255]));
        let region = TextRegion {
            page_index: 0,
            region_id: 0,
            class_name: Some("page".to_string()),
            bbox_highres: [10, 10, 90, 50],
            confidence: 1.0,
        };

        let result = pipeline
            .process_page(
                &image,
                &vec![255; 100 * 60],
                &[region],
                &CoordinateMap::identity(100, 60, 100.0, 60.0),
                0,
            )
            .unwrap();

        assert_eq!(line_calls.load(Ordering::SeqCst), 0);
        assert_eq!(region_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[0].bbox_highres, [12, 13, 40, 22]);
        assert_eq!(result.lines[1].bbox_highres, [12, 30, 40, 39]);
    }

    #[test]
    fn failed_gray_line_is_retried_with_clean_binary_crop() {
        let line_calls = Arc::new(AtomicUsize::new(0));
        let region_calls = Arc::new(AtomicUsize::new(0));
        let engine = MockEngine {
            line_calls: line_calls.clone(),
            region_calls: region_calls.clone(),
            binary_line_text: Some("recovered"),
            region_lines: Vec::new(),
        };
        let pipeline = OcrPipeline::with_engine(Box::new(engine), SlowOcrConfig::default());
        let mut image = RgbImage::from_pixel(100, 40, Rgb([240, 240, 240]));
        let mut binary = vec![255; 100 * 40];
        for y in 15..21 {
            for x in 20..80 {
                image.put_pixel(x, y, Rgb([20, 20, 20]));
                binary[y as usize * 100 + x as usize] = 0;
            }
        }
        let region = TextRegion {
            page_index: 0,
            region_id: 0,
            class_name: Some("plain_text".to_string()),
            bbox_highres: [0, 0, 100, 40],
            confidence: 1.0,
        };

        let result = pipeline
            .process_page(
                &image,
                &binary,
                &[region],
                &CoordinateMap::identity(100, 40, 100.0, 40.0),
                0,
            )
            .unwrap();

        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].text, "recovered");
        assert_eq!(line_calls.load(Ordering::SeqCst), 2);
        assert_eq!(region_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn recovery_adds_only_missing_non_overlapping_lines() {
        let existing = vec![line("one", [0, 0, 100, 10])];
        let recovery = vec![
            line("duplicate", [0, 1, 100, 9]),
            line("missing", [0, 20, 100, 30]),
        ];
        let merged = merge_region_recovery(existing, recovery, 1, 2);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|line| line.text == "one"));
        assert!(merged.iter().any(|line| line.text == "missing"));
        assert!(!merged.iter().any(|line| line.text == "duplicate"));
    }

    #[test]
    fn prepared_line_word_boxes_are_mapped_back_to_original_crop() {
        let mut result = OcrLineResult {
            text: "word".to_string(),
            confidence: None,
            words: vec![types::OcrWord {
                text: "word".to_string(),
                bbox_crop_local: [8, 4, 40, 20],
                confidence: None,
            }],
            bbox_highres: [0, 0, 80, 48],
        };
        rescale_word_bboxes(&mut result, 0.25, 0.25, 20, 12);
        assert_eq!(result.words[0].bbox_crop_local, [2, 1, 10, 5]);
    }

    #[test]
    fn refinement_keeps_reference_on_two_way_confidence_less_disagreement() {
        let reference = line("(Photo A. Frequin.)", [0, 0, 100, 20]);
        let refined = line("(P oto A. Fre ulna)", [0, 0, 100, 20]);

        let selected = choose_refined_ocr_variant(reference, Some(refined), None);

        assert_eq!(selected.text, "(Photo A. Frequin.)");
    }

    #[test]
    fn refinement_uses_three_pass_character_consensus() {
        let reference = line("thc wziied town", [0, 0, 100, 20]);
        let grayscale = line("the walled town", [0, 0, 100, 20]);
        let binary = line("the walled town", [0, 0, 100, 20]);

        let selected = choose_refined_ocr_variant(reference, Some(grayscale), Some(binary));

        assert_eq!(selected.text, "the walled town");
    }

    #[test]
    fn refinement_uses_comparable_confidence() {
        let mut reference = line("reference", [0, 0, 100, 20]);
        reference.confidence = Some(0.70);
        let mut grayscale = line("grayscale", [0, 0, 100, 20]);
        grayscale.confidence = Some(0.92);
        let mut binary = line("binary", [0, 0, 100, 20]);
        binary.confidence = Some(0.80);

        let selected = choose_refined_ocr_variant(reference, Some(grayscale), Some(binary));

        assert_eq!(selected.text, "grayscale");
    }

    #[test]
    fn tightened_reference_preserves_page_word_coordinates() {
        let mut result = OcrLineResult {
            text: "word".to_string(),
            confidence: None,
            words: vec![types::OcrWord {
                text: "word".to_string(),
                bbox_crop_local: [15, 8, 45, 18],
                confidence: None,
            }],
            bbox_highres: [100, 200, 200, 230],
        };

        remap_line_bbox(&mut result, [110, 205, 190, 225]);

        assert_eq!(result.bbox_highres, [110, 205, 190, 225]);
        assert_eq!(result.words[0].bbox_crop_local, [5, 3, 35, 13]);
    }
}
