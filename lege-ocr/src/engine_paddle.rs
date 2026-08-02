//! PP-OCRv5 OCR engine over the lege-gpu wgpu runtime.
//!
//! Replaces Tesseract on Linux/macOS with a pure-Rust, GPU-accelerated pipeline:
//! DBNet text detection → per-line SVTR recognition → CTC decode. No external
//! native library, so it cross-compiles cleanly (the reason Tesseract is being
//! dropped) and runs on the same wgpu device the rest of Lege already uses.
//!
//! The recognition model is multilingual (Latin + CJK via an 18k-glyph
//! dictionary, umlauts included), so the `lang` argument is accepted but not
//! used to pick a model.
//!
//! NOTE: only English has been validated end-to-end. German (ä/ö/ü/ß) is present
//! in the dictionary and should work, but is unverified — it was only ever a test
//! case for the language service, not a shipping requirement.

use anyhow::{Context, Result};
use image::{GrayImage, RgbImage};
use lege_gpu::vision::{Detector, RecLine, RecRecognizer, TextBox};
use once_cell::sync::Lazy;
use std::sync::Arc;

use crate::types::{OcrLineResult, OcrResult, OcrWord};

/// Embedded fp16 PP-OCRv5 mobile detection model (FLOAT16 initializers; the
/// wgpu runtime upcasts to f32 on load).
static EMBEDDED_DET: &[u8] = include_bytes!("../assets/ppocr-det.onnx");
/// Embedded fp16 PP-OCRv5 mobile recognition model.
static EMBEDDED_REC: &[u8] = include_bytes!("../assets/ppocr-rec.onnx");
/// Embedded PP-OCRv5 character dictionary (18383 glyphs, one per line).
static EMBEDDED_DICT: &str = include_str!("../assets/ppocr-dict.txt");

struct PaddleOcrInner {
    detector: Detector,
    recognizer: RecRecognizer,
}

/// PP-OCRv5 detection + recognition engine.
///
/// Clones share the parsed models and compiled-graph caches. This is important
/// for the page-parallel callers: rebuilding the engine for every page would
/// repeatedly parse both embedded models and discard every compiled width/size
/// bucket after one use.
#[derive(Clone)]
pub struct PaddleOcrEngine {
    inner: Arc<PaddleOcrInner>,
}

static SHARED_EMBEDDED_ENGINE: Lazy<std::result::Result<PaddleOcrEngine, String>> =
    Lazy::new(|| PaddleOcrEngine::from_embedded().map_err(|err| format!("{err:#}")));

impl PaddleOcrEngine {
    /// Build from the models embedded in the binary. This is the production
    /// constructor — no external asset files are needed.
    pub fn from_embedded() -> Result<Self> {
        Self::new(EMBEDDED_DET, EMBEDDED_REC, EMBEDDED_DICT)
    }

    /// Build from prepared det/rec model bytes and dictionary text (one glyph
    /// per line). Suitable for `include_bytes!`-embedded assets.
    pub fn new(det_bytes: &[u8], rec_bytes: &[u8], dict_text: &str) -> Result<Self> {
        let detector = Detector::from_bytes(det_bytes).context("failed to build det model")?;
        let recognizer =
            RecRecognizer::from_bytes(rec_bytes, dict_text).context("failed to build rec model")?;
        Ok(Self {
            inner: Arc::new(PaddleOcrInner {
                detector,
                recognizer,
            }),
        })
    }

    /// Return a clone of the process-wide embedded engine. Parsed models and
    /// lazily compiled graph buckets are retained across pages and OCR modes.
    pub fn shared_embedded() -> Result<Self> {
        match &*SHARED_EMBEDDED_ENGINE {
            Ok(engine) => Ok(engine.clone()),
            Err(message) => anyhow::bail!("failed to initialize embedded PP-OCR engine: {message}"),
        }
    }

    /// Verify that both embedded models can compile and execute on the selected
    /// wgpu adapter. Called once before an OCR job starts so GPU/model failures
    /// are reported to the user instead of producing a document with no text.
    pub fn preflight_embedded() -> Result<()> {
        let engine = Self::shared_embedded()?;
        let probe = GrayImage::from_pixel(32, 32, image::Luma([255]));
        engine
            .inner
            .detector
            .detect_gray(&probe)
            .context("embedded PP-OCR detector probe failed")?;
        engine
            .inner
            .recognizer
            .recognize_words_gray(&probe)
            .context("embedded PP-OCR recognizer probe failed")?;
        Ok(())
    }

    /// Detect text lines on `rgb`, recognize each, and return per-line results
    /// with page-global bboxes in column-aware reading order.
    fn ocr_rgb(&self, rgb: &RgbImage) -> Result<Vec<OcrLineResult>> {
        let (iw, ih) = rgb.dimensions();
        let boxes = self
            .inner
            .detector
            .detect(rgb)
            .context("detection failed")?;
        self.recognize_boxes(boxes, iw, ih, |x, y, w, h| {
            let crop = image::imageops::crop_imm(rgb, x, y, w, h).to_image();
            self.inner.recognizer.recognize_words(&crop)
        })
    }

    /// Grayscale-native page path. Detection generates the model's replicated
    /// channels directly from luma, and only small recognized line crops are
    /// converted to RGB.
    fn ocr_gray(&self, gray: &GrayImage) -> Result<Vec<OcrLineResult>> {
        let (iw, ih) = gray.dimensions();
        let boxes = self
            .inner
            .detector
            .detect_gray(gray)
            .context("grayscale detection failed")?;
        self.recognize_boxes(boxes, iw, ih, |x, y, w, h| {
            let crop = image::imageops::crop_imm(gray, x, y, w, h).to_image();
            self.inner.recognizer.recognize_words_gray(&crop)
        })
    }

    fn recognize_boxes(
        &self,
        boxes: Vec<TextBox>,
        iw: u32,
        ih: u32,
        mut recognize: impl FnMut(u32, u32, u32, u32) -> Result<RecLine>,
    ) -> Result<Vec<OcrLineResult>> {
        let crops: Vec<_> = boxes
            .into_iter()
            .filter_map(|b| {
                let (x, y, w, h) = b.crop_rect(iw, ih);
                (w >= 2 && h >= 2).then_some((b, [x, y, x + w, y + h]))
            })
            .collect();
        let crop_bboxes: Vec<_> = crops.iter().map(|(_, bbox)| *bbox).collect();
        let reading_order = crate::reading_order::order_bboxes(&crop_bboxes, iw);

        let mut lines = Vec::with_capacity(crops.len());
        for index in reading_order {
            let (b, [x, y, x1, y1]) = crops[index];
            let (w, h) = (x1 - x, y1 - y);
            let rec = recognize(x, y, w, h)?;
            if rec.text.trim().is_empty() {
                continue;
            }
            let words = rec
                .words
                .iter()
                .map(|word| OcrWord {
                    text: word.text.clone(),
                    // Crop-local; hOCR builder offsets by the line origin. Word y
                    // spans the full line height (rec is line-level in y).
                    bbox_crop_local: [word.x0, 0, word.x1.min(w), h],
                    // DBNet's score measures detection, not recognition. Do not
                    // expose it as the hOCR word confidence.
                    confidence: None,
                })
                .collect();
            lines.push(OcrLineResult {
                text: rec.text,
                confidence: Some(b.score),
                words,
                bbox_highres: [x, y, x1, y1],
            });
        }
        Ok(lines)
    }
}

impl super::engine::OcrEngine for PaddleOcrEngine {
    fn name(&self) -> &'static str {
        "paddle"
    }

    fn run_image(
        &self,
        data: &[u8],
        width: usize,
        height: usize,
        is_binary: bool,
        _lang: &str,
    ) -> Option<OcrResult> {
        let bpp = super::engine::raw_image_bpp(data, width, height, is_binary)?;
        // Uniform image → nothing to OCR (but not a failure).
        if data.iter().all(|&b| b == data[0]) {
            return Some(OcrResult {
                hocr: String::new(),
                plain_text: String::new(),
            });
        }

        // The detector normalizes to its own model size. Avoid first cloning an
        // arbitrarily large source raster only to resize it during preprocessing.
        const MAX_INPUT_PIXELS: usize = 4_000_000;
        let pixels = width.checked_mul(height)?;
        let resized;
        let (data, final_width, final_height) = if pixels > MAX_INPUT_PIXELS {
            let scale = (MAX_INPUT_PIXELS as f64 / pixels as f64).sqrt();
            let resized_width = ((width as f64 * scale).round() as usize).max(1);
            let resized_height = ((height as f64 * scale).round() as usize).max(1);
            resized =
                super::engine::resize_cpu(data, width, height, resized_width, resized_height, bpp);
            (resized, resized_width, resized_height)
        } else {
            (data.to_vec(), width, height)
        };

        let mut lines = if bpp == 1 {
            let gray = GrayImage::from_raw(final_width as u32, final_height as u32, data)?;
            self.ocr_gray(&gray).ok()?
        } else {
            let rgb = RgbImage::from_raw(final_width as u32, final_height as u32, data)?;
            self.ocr_rgb(&rgb).ok()?
        };
        if final_width != width || final_height != height {
            crate::coordinate::scale_lines(
                &mut lines,
                width as f32 / final_width as f32,
                height as f32 / final_height as f32,
                width as u32,
                height as u32,
            );
        }
        let hocr = crate::hocr::build_page_hocr(&lines, width as u32, height as u32);
        let plain_text = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Some(OcrResult { hocr, plain_text })
    }

    fn ocr_line(&self, image: &GrayImage, _lang: &str) -> Result<OcrLineResult> {
        let bbox = [0u32, 0, image.width(), image.height()];
        if image.width() < 2 || image.height() < 2 {
            return Ok(OcrLineResult {
                text: String::new(),
                confidence: None,
                words: Vec::new(),
                bbox_highres: bbox,
            });
        }
        // The crop is already a single line — recognize directly, no detection.
        let rec = self.inner.recognizer.recognize_words_gray(image)?;
        let h = image.height();
        let words = rec
            .words
            .iter()
            .map(|word| OcrWord {
                text: word.text.clone(),
                bbox_crop_local: [word.x0, 0, word.x1.min(image.width()), h],
                confidence: None,
            })
            .collect();
        Ok(OcrLineResult {
            text: rec.text,
            confidence: None,
            words,
            bbox_highres: bbox,
        })
    }

    fn ocr_region(&self, image: &GrayImage, _lang: &str) -> Result<Vec<OcrLineResult>> {
        self.ocr_gray(image)
    }

    fn ocr_page(&self, image: &GrayImage, _lang: &str) -> Result<Vec<OcrLineResult>> {
        self.ocr_gray(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::OcrEngine;
    use std::path::PathBuf;

    fn gpu_test_fixture() -> Option<PathBuf> {
        if std::env::var_os("LEGE_RUN_GPU_OCR_TESTS").is_none() {
            eprintln!("skipping real-GPU PaddleOCR test; run scripts/test-paddle-ocr-gpu.sh");
            return None;
        }
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../page_0002-original.png"))
    }

    #[test]
    fn embedded_models_parse_and_match_dictionary() {
        let engine = PaddleOcrEngine::from_embedded().expect("embedded PP-OCR models must load");
        assert_eq!(engine.name(), "paddle");
        assert_eq!(engine.inner.recognizer.num_classes(), 18_385);
    }

    /// `run_image` on a downscaled grayscale page (the shape the fast pipeline
    /// feeds after cleaning) must recover text. Regression guard for the finding
    /// that DBNet needs natural grayscale — a hard 1bpp mask yields nothing, so
    /// the pipeline routes cleaned grayscale here, not the binarized mask.
    #[test]
    fn paddle_engine_run_image_gray() {
        let Some(page) = gpu_test_fixture() else {
            return;
        };
        let engine = PaddleOcrEngine::shared_embedded().expect("build embedded engine");
        let gray = image::open(page).unwrap().to_luma8();
        // Downscale to the pipeline's ~1200px height.
        let nw = (gray.width() as f32 * 1200.0 / gray.height() as f32) as u32;
        let small = image::imageops::resize(&gray, nw, 1200, image::imageops::FilterType::Triangle);
        let gray_buf: Vec<u8> = small.pixels().map(|p| p.0[0]).collect();
        let cold_started = std::time::Instant::now();
        let res = engine
            .run_image(&gray_buf, nw as usize, 1200, false, "eng")
            .expect("run_image returned None");
        let cold_elapsed = cold_started.elapsed();
        let lines = res.plain_text.lines().count();
        assert!(
            lines >= 20,
            "grayscale run_image recovered too few lines: {lines}"
        );

        // Run the identical shape again to exercise the production graph
        // caches. This opt-in test doubles as a useful cold-vs-warm smoke
        // benchmark without slowing the normal test suite.
        let warm_started = std::time::Instant::now();
        let warm = engine
            .run_image(&gray_buf, nw as usize, 1200, false, "eng")
            .expect("warm run_image returned None");
        let warm_elapsed = warm_started.elapsed();
        let warm_lines = warm.plain_text.lines().count();
        eprintln!(
            "run_image gray @1200px: {lines} lines cold={cold_elapsed:.2?}, \
             {warm_lines} lines warm={warm_elapsed:.2?}"
        );
        assert_eq!(
            warm_lines, lines,
            "warm OCR changed the detected line count"
        );
        assert!(
            res.plain_text.contains("Houses") && res.plain_text.contains("fashion"),
            "expected known fixture text in OCR output: {}",
            res.plain_text
        );
    }

    /// Full page through the PaddleOcrEngine: detect → recognize → assemble.
    /// Opt-in because it requires a real GPU. The release-gate script sets the
    /// flag and rejects CPU/software wgpu adapters.
    #[test]
    fn paddle_engine_reads_page() {
        let Some(page) = gpu_test_fixture() else {
            return;
        };
        let engine = PaddleOcrEngine::shared_embedded().expect("build embedded engine");

        let gray = image::open(&page).expect("open page").to_luma8();
        let lines = engine.ocr_page(&gray, "eng").expect("ocr_page");
        eprintln!("paddle engine: {} lines", lines.len());
        for l in lines.iter().take(6) {
            eprintln!("  [{} words] {:?}", l.words.len(), l.text);
        }
        if let Some(out) = std::env::var_os("LEGE_DET_DRAW") {
            let mut rgb = image::open(&page).unwrap().to_rgb8();
            for l in &lines {
                let [lx, ly, _, _] = l.bbox_highres;
                for wd in &l.words {
                    let [wx0, wy0, wx1, wy1] = wd.bbox_crop_local;
                    let (x0, y0, x1, y1) = (wx0 + lx, wy0 + ly, wx1 + lx, wy1 + ly);
                    for x in x0..x1.min(rgb.width()) {
                        if y0 < rgb.height() {
                            rgb.put_pixel(x, y0, image::Rgb([0, 128, 255]));
                        }
                        if y1 > 0 && y1 - 1 < rgb.height() {
                            rgb.put_pixel(x, y1 - 1, image::Rgb([0, 128, 255]));
                        }
                    }
                    for y in y0..y1.min(rgb.height()) {
                        if x0 < rgb.width() {
                            rgb.put_pixel(x0, y, image::Rgb([255, 0, 0]));
                        }
                        if x1 > 0 && x1 - 1 < rgb.width() {
                            rgb.put_pixel(x1 - 1, y, image::Rgb([255, 0, 0]));
                        }
                    }
                }
            }
            rgb.save(PathBuf::from(out)).unwrap();
        }
        assert!(
            lines.len() >= 20,
            "expected many lines, got {}",
            lines.len()
        );
        let joined = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("Houses") && joined.contains("fashion"),
            "expected known page text in output"
        );
    }
}
