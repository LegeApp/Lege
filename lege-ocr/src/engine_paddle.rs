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
use lege_gpu::vision::{
    Detector, LayoutConfig, LayoutDetector, RecLine, RecRecognizer, TextBox, TokenRecognizer,
    TokenRecognizerConfig,
};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

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
    recognition: RecognitionScheduler,
    recognizer_classes: usize,
    specialists: Option<PaddleSpecialists>,
}

struct PaddleSpecialists {
    layout: std::sync::Mutex<LayoutDetector>,
    table: Option<TokenRecognizer>,
    formula: Option<TokenRecognizer>,
    table_model: Option<String>,
    formula_model: Option<String>,
}

struct RecognitionRequest {
    lines: Vec<GrayImage>,
    response: mpsc::SyncSender<std::result::Result<Vec<OcrLineResult>, String>>,
}

struct RecognitionScheduler {
    sender: mpsc::SyncSender<RecognitionRequest>,
}

impl RecognitionScheduler {
    fn new(recognizer: RecRecognizer, config: crate::backend::OcrSchedulerConfig) -> Result<Self> {
        let config = config.normalized();
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        std::thread::Builder::new()
            .name("lege-ocr-gpu-scheduler".to_string())
            .spawn(move || recognition_worker(recognizer, receiver, config))
            .context("failed to start OCR recognition scheduler")?;
        Ok(Self { sender })
    }

    fn recognize(&self, lines: Vec<GrayImage>) -> Result<Vec<OcrLineResult>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let (response, result) = mpsc::sync_channel(1);
        self.sender
            .send(RecognitionRequest { lines, response })
            .context("OCR recognition scheduler stopped")?;
        result
            .recv()
            .context("OCR recognition scheduler dropped a response")?
            .map_err(anyhow::Error::msg)
    }
}

fn recognition_worker(
    recognizer: RecRecognizer,
    receiver: mpsc::Receiver<RecognitionRequest>,
    config: crate::backend::OcrSchedulerConfig,
) {
    let mut deferred = None;
    loop {
        let first = match deferred.take().or_else(|| receiver.recv().ok()) {
            Some(request) => request,
            None => return,
        };
        let started = Instant::now();
        let mut requests = vec![first];
        let mut line_count = requests[0].lines.len();
        let mut pixels = request_pixels(&requests[0]);
        while line_count < config.max_batch_lines && pixels < config.max_batch_pixels {
            let remaining =
                Duration::from_millis(config.max_wait_ms).saturating_sub(started.elapsed());
            let next = if remaining.is_zero() {
                receiver.try_recv().ok()
            } else {
                receiver.recv_timeout(remaining).ok()
            };
            let Some(next) = next else { break };
            let next_lines = next.lines.len();
            let next_pixels = request_pixels(&next);
            if !requests.is_empty()
                && (line_count.saturating_add(next_lines) > config.max_batch_lines
                    || pixels.saturating_add(next_pixels) > config.max_batch_pixels)
            {
                deferred = Some(next);
                break;
            }
            line_count += next_lines;
            pixels = pixels.saturating_add(next_pixels);
            requests.push(next);
        }

        let counts = requests
            .iter()
            .map(|request| request.lines.len())
            .collect::<Vec<_>>();
        let mut lines = Vec::with_capacity(line_count);
        for request in &mut requests {
            lines.append(&mut request.lines);
        }
        let recognized = recognizer
            .recognize_words_gray_batch(&lines)
            .context("scheduled grayscale recognition failed")
            .map(|values| {
                lines
                    .iter()
                    .zip(values)
                    .map(|(line, value)| rec_line_result(line, value))
                    .collect::<Vec<_>>()
            })
            .map_err(|error| format!("{error:#}"));
        match recognized {
            Ok(values) => {
                let mut offset = 0;
                for (request, count) in requests.into_iter().zip(counts) {
                    let end = offset + count;
                    let _ = request.response.send(Ok(values[offset..end].to_vec()));
                    offset = end;
                }
            }
            Err(error) => {
                for request in requests {
                    let _ = request.response.send(Err(error.clone()));
                }
            }
        }
    }
}

fn request_pixels(request: &RecognitionRequest) -> u64 {
    request.lines.iter().fold(0_u64, |total, line| {
        total.saturating_add(u64::from(line.width()) * u64::from(line.height()))
    })
}

impl PaddleSpecialists {
    fn from_pack(directory: &Path, manifest: &PaddleModelPack) -> Result<Option<Self>> {
        let Some(layout_entry) = &manifest.layout else {
            return Ok(None);
        };
        let layout_bytes = read_pack_file(directory, layout_entry)?;
        let layout = LayoutDetector::from_model_bytes(&layout_bytes, LayoutConfig::default())
            .context("failed to build layout specialist")?;
        let table = manifest
            .table
            .as_ref()
            .map(|pack| build_specialist(directory, pack))
            .transpose()?;
        let formula = manifest
            .formula
            .as_ref()
            .map(|pack| build_specialist(directory, pack))
            .transpose()?;
        Ok(Some(Self {
            layout: std::sync::Mutex::new(layout),
            table,
            formula,
            table_model: manifest.table.as_ref().map(|model| model.name.clone()),
            formula_model: manifest.formula.as_ref().map(|model| model.name.clone()),
        }))
    }

    fn recognize(&self, page: &GrayImage) -> Result<Vec<crate::backend::SpecialistRegion>> {
        let rgb = image::DynamicImage::ImageLuma8(page.clone()).to_rgb8();
        let detections = self
            .layout
            .lock()
            .map_err(|_| anyhow::anyhow!("layout specialist lock poisoned"))?
            .detect_rgb(&rgb)
            .context("layout specialist inference failed")?;
        let mut regions = Vec::new();
        for detection in detections {
            let x0 = detection.bbox[0].floor().clamp(0.0, page.width() as f32) as u32;
            let y0 = detection.bbox[1].floor().clamp(0.0, page.height() as f32) as u32;
            let x1 = detection.bbox[2].ceil().clamp(0.0, page.width() as f32) as u32;
            let y1 = detection.bbox[3].ceil().clamp(0.0, page.height() as f32) as u32;
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let crop = image::imageops::crop_imm(page, x0, y0, x1 - x0, y1 - y0).to_image();
            let (content, model, recognition_confidence) = match detection.class_name {
                "table" => {
                    let Some(recognizer) = &self.table else {
                        continue;
                    };
                    let sequence = recognizer.recognize_gray(&crop)?;
                    let Some((rows, columns, cells)) = parse_table_tokens(&sequence.tokens) else {
                        continue;
                    };
                    (
                        crate::backend::SpecialistContent::Table {
                            rows,
                            columns,
                            cells,
                        },
                        self.table_model.clone(),
                        sequence.confidence,
                    )
                }
                "isolate_formula" | "formula" | "formula_number" | "algorithm" => {
                    let Some(recognizer) = &self.formula else {
                        continue;
                    };
                    let sequence = recognizer.recognize_gray(&crop)?;
                    let latex = clean_formula_tokens(&sequence.tokens);
                    if latex.trim().is_empty() {
                        continue;
                    }
                    (
                        crate::backend::SpecialistContent::Formula {
                            latex,
                            display: detection.class_name != "formula_number",
                        },
                        self.formula_model.clone(),
                        sequence.confidence,
                    )
                }
                _ => continue,
            };
            regions.push(crate::backend::SpecialistRegion {
                bbox: [x0, y0, x1, y1],
                detection_confidence: Some(detection.confidence),
                recognition_confidence,
                provider: "paddle-wgpu-specialist".to_string(),
                model,
                content,
            });
        }
        Ok(regions)
    }
}

fn build_specialist(directory: &Path, pack: &SpecialistModelPack) -> Result<TokenRecognizer> {
    let model = read_pack_file(directory, &pack.model)?;
    let dictionary = read_pack_file(directory, &pack.dictionary)?;
    let dictionary = std::str::from_utf8(&dictionary)
        .with_context(|| format!("specialist `{}` dictionary is not UTF-8", pack.name))?;
    TokenRecognizer::from_bytes(
        &model,
        dictionary,
        TokenRecognizerConfig {
            input_width: pack.input_width,
            input_height: pack.input_height,
            blank_index: pack.blank_index,
            collapse_repeats: pack.collapse_repeats,
            end_token: pack.end_token.clone(),
            mean: pack.mean,
            scale: pack.scale,
        },
    )
    .with_context(|| format!("failed to build specialist `{}`", pack.name))
}

fn parse_table_tokens(
    tokens: &[String],
) -> Option<(u32, u32, Vec<crate::backend::TableCellStructure>)> {
    let markup = tokens.join("");
    let mut row = 0_u32;
    let mut column = 0_u32;
    let mut columns = 0_u32;
    let mut cells = Vec::<crate::backend::TableCellStructure>::new();
    let mut cursor = 0;
    while let Some(start) = markup[cursor..].find('<') {
        let start = cursor + start;
        let Some(end_offset) = markup[start..].find('>') else {
            break;
        };
        let end = start + end_offset + 1;
        let tag = &markup[start..end];
        if tag.starts_with("<tr") && !tag.starts_with("</") {
            if !cells.is_empty() || row > 0 {
                row += 1;
            }
            column = 0;
        } else if (tag.starts_with("<td") || tag.starts_with("<th")) && !tag.starts_with("</") {
            while cells.iter().any(|cell| {
                row >= cell.row
                    && row < cell.row.saturating_add(cell.row_span)
                    && column >= cell.column
                    && column < cell.column.saturating_add(cell.column_span)
            }) {
                column = column.saturating_add(1);
            }
            let row_span = html_span(tag, "rowspan").unwrap_or(1);
            let column_span = html_span(tag, "colspan").unwrap_or(1);
            cells.push(crate::backend::TableCellStructure {
                row,
                column,
                row_span,
                column_span,
            });
            column = column.saturating_add(column_span);
            columns = columns.max(column);
        }
        cursor = end;
    }
    let rows = cells
        .iter()
        .map(|cell| cell.row.saturating_add(cell.row_span))
        .max()
        .unwrap_or(0);
    (rows > 0 && columns > 0).then_some((rows, columns, cells))
}

fn html_span(tag: &str, attribute: &str) -> Option<u32> {
    let start = tag.find(attribute)? + attribute.len();
    let value = tag[start..].trim_start();
    let value = value.strip_prefix('=')?.trim_start();
    let value = value.trim_start_matches(['\'', '"']);
    let digits = value
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok().filter(|span| *span > 0)
}

fn clean_formula_tokens(tokens: &[String]) -> String {
    tokens
        .iter()
        .filter(|token| !matches!(token.as_str(), "<s>" | "</s>" | "<pad>" | "<bos>" | "<eos>"))
        .cloned()
        .collect()
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

/// Versioned, checksum-pinned external Paddle model pack. The embedded
/// compatibility assets remain v5; newer generations can be installed without
/// recompiling the CLI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaddleModelPack {
    pub schema_version: u32,
    pub provider: String,
    pub name: String,
    pub version: String,
    pub generation: String,
    pub license: String,
    pub source: String,
    pub detector: ModelPackFile,
    pub recognizer: ModelPackFile,
    pub dictionary: ModelPackFile,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub layout: Option<ModelPackFile>,
    #[serde(default)]
    pub table: Option<SpecialistModelPack>,
    #[serde(default)]
    pub formula: Option<SpecialistModelPack>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpecialistModelPack {
    pub name: String,
    pub model: ModelPackFile,
    pub dictionary: ModelPackFile,
    pub input_width: u32,
    pub input_height: u32,
    #[serde(default)]
    pub blank_index: Option<usize>,
    #[serde(default = "default_true")]
    pub collapse_repeats: bool,
    #[serde(default)]
    pub end_token: Option<String>,
    #[serde(default = "default_specialist_mean")]
    pub mean: [f32; 3],
    #[serde(default = "default_specialist_scale")]
    pub scale: [f32; 3],
}

fn default_true() -> bool {
    true
}

fn default_specialist_mean() -> [f32; 3] {
    [0.5; 3]
}

fn default_specialist_scale() -> [f32; 3] {
    [2.0; 3]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPackFile {
    pub path: PathBuf,
    /// Lowercase BLAKE3 hex, optionally prefixed with `blake3:`.
    pub blake3: String,
}

static SHARED_EMBEDDED_ENGINE: Lazy<std::result::Result<PaddleOcrEngine, String>> =
    Lazy::new(|| PaddleOcrEngine::from_embedded().map_err(|err| format!("{err:#}")));

impl PaddleOcrEngine {
    /// Build from the models embedded in the binary. This is the production
    /// constructor — no external asset files are needed.
    pub fn from_embedded() -> Result<Self> {
        Self::new(EMBEDDED_DET, EMBEDDED_REC, EMBEDDED_DICT)
    }

    pub fn from_embedded_with_scheduler(
        scheduler: crate::backend::OcrSchedulerConfig,
    ) -> Result<Self> {
        Self::new_with_scheduler(EMBEDDED_DET, EMBEDDED_REC, EMBEDDED_DICT, scheduler)
    }

    /// Load `manifest.json` and verify every model asset before GPU graph
    /// construction.
    pub fn from_model_pack(directory: &Path) -> Result<(Self, PaddleModelPack)> {
        Self::from_model_pack_with_scheduler(
            directory,
            crate::backend::OcrSchedulerConfig::default(),
        )
    }

    pub fn from_model_pack_with_scheduler(
        directory: &Path,
        scheduler: crate::backend::OcrSchedulerConfig,
    ) -> Result<(Self, PaddleModelPack)> {
        let manifest = Self::verify_model_pack(directory)?;
        let detector = read_pack_file(directory, &manifest.detector)?;
        let recognizer = read_pack_file(directory, &manifest.recognizer)?;
        let dictionary = read_pack_file(directory, &manifest.dictionary)?;
        let dictionary =
            std::str::from_utf8(&dictionary).context("model dictionary is not UTF-8")?;
        let specialists = PaddleSpecialists::from_pack(directory, &manifest)?;
        Ok((
            Self::new_components(&detector, &recognizer, dictionary, scheduler, specialists)?,
            manifest,
        ))
    }

    /// Validate manifest schema, safe relative paths, and all BLAKE3 hashes
    /// without compiling a GPU graph.
    pub fn verify_model_pack(directory: &Path) -> Result<PaddleModelPack> {
        let manifest_path = directory.join("manifest.json");
        let manifest: PaddleModelPack = serde_json::from_slice(
            &std::fs::read(&manifest_path)
                .with_context(|| format!("read {}", manifest_path.display()))?,
        )
        .context("parse Paddle model-pack manifest")?;
        anyhow::ensure!(
            manifest.schema_version == 1,
            "unsupported model-pack schema {}",
            manifest.schema_version
        );
        anyhow::ensure!(
            !manifest.license.trim().is_empty(),
            "model-pack license is required"
        );
        anyhow::ensure!(
            !manifest.source.trim().is_empty(),
            "model-pack source is required"
        );
        drop(read_pack_file(directory, &manifest.detector)?);
        drop(read_pack_file(directory, &manifest.recognizer)?);
        let dictionary = read_pack_file(directory, &manifest.dictionary)?;
        std::str::from_utf8(&dictionary).context("model dictionary is not UTF-8")?;
        if manifest.table.is_some() || manifest.formula.is_some() {
            anyhow::ensure!(
                manifest.layout.is_some(),
                "table/formula specialists require a layout model"
            );
        }
        if let Some(layout) = &manifest.layout {
            drop(read_pack_file(directory, layout)?);
        }
        for specialist in [&manifest.table, &manifest.formula].into_iter().flatten() {
            anyhow::ensure!(
                specialist.input_width > 0 && specialist.input_height > 0,
                "specialist `{}` input dimensions must be non-zero",
                specialist.name
            );
            drop(read_pack_file(directory, &specialist.model)?);
            let tokens = read_pack_file(directory, &specialist.dictionary)?;
            std::str::from_utf8(&tokens).with_context(|| {
                format!("specialist `{}` dictionary is not UTF-8", specialist.name)
            })?;
        }
        Ok(manifest)
    }

    /// Build from prepared det/rec model bytes and dictionary text (one glyph
    /// per line). Suitable for `include_bytes!`-embedded assets.
    pub fn new(det_bytes: &[u8], rec_bytes: &[u8], dict_text: &str) -> Result<Self> {
        Self::new_with_scheduler(
            det_bytes,
            rec_bytes,
            dict_text,
            crate::backend::OcrSchedulerConfig::default(),
        )
    }

    pub fn new_with_scheduler(
        det_bytes: &[u8],
        rec_bytes: &[u8],
        dict_text: &str,
        scheduler: crate::backend::OcrSchedulerConfig,
    ) -> Result<Self> {
        Self::new_components(det_bytes, rec_bytes, dict_text, scheduler, None)
    }

    fn new_components(
        det_bytes: &[u8],
        rec_bytes: &[u8],
        dict_text: &str,
        scheduler: crate::backend::OcrSchedulerConfig,
        specialists: Option<PaddleSpecialists>,
    ) -> Result<Self> {
        let detector = Detector::from_bytes(det_bytes).context("failed to build det model")?;
        let recognizer =
            RecRecognizer::from_bytes(rec_bytes, dict_text).context("failed to build rec model")?;
        let recognizer_classes = recognizer.num_classes();
        Ok(Self {
            inner: Arc::new(PaddleOcrInner {
                detector,
                recognition: RecognitionScheduler::new(recognizer, scheduler)?,
                recognizer_classes,
                specialists,
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

    pub fn recognizer_classes(&self) -> usize {
        self.inner.recognizer_classes
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
            .recognition
            .recognize(vec![probe])
            .context("embedded PP-OCR recognizer probe failed")?;
        Ok(())
    }

    /// Recognize an already-segmented group of grayscale lines in fixed-shape
    /// GPU batches. Results retain crop-local word geometry.
    pub fn recognize_gray_lines_batch(&self, lines: &[GrayImage]) -> Result<Vec<OcrLineResult>> {
        self.inner.recognition.recognize(lines.to_vec())
    }

    /// Run DBNet only and return page-space text-line boxes. Used by layout
    /// correction so DocLayout can stay authoritative for vertical bands
    /// while OCR-det supplies the horizontal extent.
    pub fn detect_gray_boxes(&self, gray: &GrayImage) -> Result<Vec<TextBox>> {
        self.inner
            .detector
            .detect_gray(gray)
            .context("grayscale detection failed")
    }

    /// Detect text lines on `rgb`, recognize each, and return per-line results
    /// with page-global bboxes in column-aware reading order.
    fn ocr_rgb_linked(
        &self,
        detection_image: &RgbImage,
        recognition_source: &RgbImage,
    ) -> Result<Vec<OcrLineResult>> {
        let boxes = self
            .inner
            .detector
            .detect(detection_image)
            .context("detection failed")?;
        let crops = Self::ordered_source_crops(
            boxes,
            detection_image.dimensions(),
            recognition_source.dimensions(),
        );
        let line_images = crops
            .iter()
            .map(|(_, [x, y, x1, y1])| {
                let crop = image::imageops::crop_imm(recognition_source, *x, *y, x1 - x, y1 - y)
                    .to_image();
                image::DynamicImage::ImageRgb8(crop).to_luma8()
            })
            .collect::<Vec<_>>();
        let recognized = self
            .inner
            .recognition
            .recognize(line_images)?
            .into_iter()
            .map(ocr_line_to_rec_line)
            .collect();
        Ok(Self::assemble_lines(crops, recognized))
    }

    /// Grayscale-native page path. Detection generates the model's replicated
    /// channels directly from luma, and only small recognized line crops are
    /// converted to RGB.
    fn ocr_gray(&self, gray: &GrayImage) -> Result<Vec<OcrLineResult>> {
        self.ocr_gray_linked(gray, gray)
    }

    fn ocr_gray_linked(
        &self,
        detection_image: &GrayImage,
        recognition_source: &GrayImage,
    ) -> Result<Vec<OcrLineResult>> {
        let boxes = self
            .inner
            .detector
            .detect_gray(detection_image)
            .context("grayscale detection failed")?;
        let crops = Self::ordered_source_crops(
            boxes,
            detection_image.dimensions(),
            recognition_source.dimensions(),
        );
        let line_images = crops
            .iter()
            .map(|(_, [x, y, x1, y1])| {
                image::imageops::crop_imm(recognition_source, *x, *y, x1 - x, y1 - y).to_image()
            })
            .collect::<Vec<_>>();
        let recognized = self
            .inner
            .recognition
            .recognize(line_images)?
            .into_iter()
            .map(ocr_line_to_rec_line)
            .collect();
        Ok(Self::assemble_lines(crops, recognized))
    }

    fn ordered_source_crops(
        boxes: Vec<TextBox>,
        detection_size: (u32, u32),
        source_size: (u32, u32),
    ) -> Vec<(TextBox, [u32; 4])> {
        let (det_w, det_h) = detection_size;
        let (source_w, source_h) = source_size;
        let scale_x = source_w as f32 / det_w.max(1) as f32;
        let scale_y = source_h as f32 / det_h.max(1) as f32;
        let crops = boxes
            .into_iter()
            .filter_map(|b| {
                let (dx, dy, dw, dh) = b.crop_rect(det_w, det_h);
                let x = (dx as f32 * scale_x).floor().clamp(0.0, source_w as f32) as u32;
                let y = (dy as f32 * scale_y).floor().clamp(0.0, source_h as f32) as u32;
                let x1 = ((dx + dw) as f32 * scale_x)
                    .ceil()
                    .clamp(0.0, source_w as f32) as u32;
                let y1 = ((dy + dh) as f32 * scale_y)
                    .ceil()
                    .clamp(0.0, source_h as f32) as u32;
                let (w, h) = (x1.saturating_sub(x), y1.saturating_sub(y));
                (w >= 2 && h >= 2).then_some((b, [x, y, x + w, y + h]))
            })
            .collect::<Vec<_>>();
        let crop_bboxes: Vec<_> = crops.iter().map(|(_, bbox)| *bbox).collect();
        let reading_order = crate::reading_order::order_bboxes(&crop_bboxes, source_w);
        reading_order
            .into_iter()
            .map(|index| crops[index])
            .collect()
    }

    fn assemble_lines(
        crops: Vec<(TextBox, [u32; 4])>,
        recognized: Vec<RecLine>,
    ) -> Vec<OcrLineResult> {
        let mut lines = Vec::with_capacity(crops.len());
        for ((_detection, [x, y, x1, y1]), rec) in crops.into_iter().zip(recognized) {
            let (w, h) = (x1 - x, y1 - y);
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
                    confidence: Some(word.confidence),
                })
                .collect();
            lines.push(OcrLineResult {
                text: rec.text,
                // Keep recognition evidence distinct from DBNet detection
                // confidence. The latter remains useful for routing but must
                // not masquerade as OCR certainty in hOCR.
                confidence: rec.confidence,
                words,
                bbox_highres: [x, y, x1, y1],
            });
        }
        lines
    }
}

fn read_pack_file(directory: &Path, entry: &ModelPackFile) -> Result<Vec<u8>> {
    anyhow::ensure!(
        !entry.path.is_absolute()
            && entry
                .path
                .components()
                .all(|part| !matches!(part, std::path::Component::ParentDir)),
        "model-pack asset path must stay inside the pack: {}",
        entry.path.display()
    );
    let path = directory.join(&entry.path);
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let expected = entry
        .blake3
        .strip_prefix("blake3:")
        .unwrap_or(&entry.blake3);
    let actual = blake3::hash(&bytes).to_hex();
    anyhow::ensure!(
        actual.as_str() == expected,
        "BLAKE3 mismatch for {}",
        path.display()
    );
    Ok(bytes)
}

fn rec_line_result(image: &GrayImage, rec: RecLine) -> OcrLineResult {
    let width = image.width();
    let height = image.height();
    let words = rec
        .words
        .iter()
        .map(|word| OcrWord {
            text: word.text.clone(),
            bbox_crop_local: [word.x0, 0, word.x1.min(width), height],
            confidence: Some(word.confidence),
        })
        .collect();
    OcrLineResult {
        text: rec.text,
        confidence: rec.confidence,
        words,
        bbox_highres: [0, 0, width, height],
    }
}

fn ocr_line_to_rec_line(line: OcrLineResult) -> RecLine {
    RecLine {
        text: line.text,
        confidence: line.confidence,
        words: line
            .words
            .into_iter()
            .map(|word| lege_gpu::vision::RecWord {
                text: word.text,
                x0: word.bbox_crop_local[0],
                x1: word.bbox_crop_local[2],
                confidence: word.confidence.unwrap_or(0.0),
            })
            .collect(),
    }
}

impl crate::backend::LineRecognizerBackend for PaddleOcrEngine {
    fn name(&self) -> &'static str {
        "paddle"
    }

    fn capabilities(&self) -> crate::backend::BackendCapabilities {
        crate::backend::BackendCapabilities {
            page_ocr: true,
            line_batching: true,
            word_geometry: true,
            recognition_confidence: true,
            layout_analysis: self.inner.specialists.is_some(),
            table_recognition: self
                .inner
                .specialists
                .as_ref()
                .is_some_and(|specialists| specialists.table.is_some()),
            formula_recognition: self
                .inner
                .specialists
                .as_ref()
                .is_some_and(|specialists| specialists.formula.is_some()),
            accelerator: crate::backend::AcceleratorKind::Gpu,
        }
    }

    fn recognize_batch(
        &self,
        batch: crate::backend::RecognitionBatch<'_>,
    ) -> Result<Vec<OcrLineResult>> {
        self.recognize_gray_lines_batch(batch.lines)
    }
}

impl crate::backend::PageOcrBackend for PaddleOcrEngine {
    fn name(&self) -> &'static str {
        "paddle"
    }

    fn capabilities(&self) -> crate::backend::BackendCapabilities {
        <Self as crate::backend::LineRecognizerBackend>::capabilities(self)
    }

    fn recognize_pages(
        &self,
        batch: crate::backend::PageBatch<'_>,
    ) -> Result<Vec<Vec<OcrLineResult>>> {
        batch.pages.iter().map(|page| self.ocr_gray(page)).collect()
    }

    fn recognize_lines(
        &self,
        batch: crate::backend::RecognitionBatch<'_>,
    ) -> Result<Vec<OcrLineResult>> {
        self.recognize_gray_lines_batch(batch.lines)
    }

    fn specialize_page(
        &self,
        page: &GrayImage,
        _language: &str,
    ) -> Result<Vec<crate::backend::SpecialistRegion>> {
        self.inner
            .specialists
            .as_ref()
            .map_or_else(|| Ok(Vec::new()), |specialists| specialists.recognize(page))
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
        let (detection_data, detection_width, detection_height) = if pixels > MAX_INPUT_PIXELS {
            let scale = (MAX_INPUT_PIXELS as f64 / pixels as f64).sqrt();
            let resized_width = ((width as f64 * scale).round() as usize).max(1);
            let resized_height = ((height as f64 * scale).round() as usize).max(1);
            resized =
                super::engine::resize_cpu(data, width, height, resized_width, resized_height, bpp);
            (resized, resized_width, resized_height)
        } else {
            (data.to_vec(), width, height)
        };

        let lines = if bpp == 1 {
            let source = GrayImage::from_raw(width as u32, height as u32, data.to_vec())?;
            let detection = GrayImage::from_raw(
                detection_width as u32,
                detection_height as u32,
                detection_data,
            )?;
            self.ocr_gray_linked(&detection, &source).ok()?
        } else {
            let source = RgbImage::from_raw(width as u32, height as u32, data.to_vec())?;
            let detection = RgbImage::from_raw(
                detection_width as u32,
                detection_height as u32,
                detection_data,
            )?;
            self.ocr_rgb_linked(&detection, &source).ok()?
        };
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
        let mut results = self.inner.recognition.recognize(vec![image.clone()])?;
        Ok(results.pop().unwrap_or(OcrLineResult {
            text: String::new(),
            confidence: None,
            words: Vec::new(),
            bbox_highres: bbox,
        }))
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

    #[test]
    fn model_pack_rejects_paths_outside_the_pack() {
        let entry = ModelPackFile {
            path: PathBuf::from("../model.onnx"),
            blake3: "0".repeat(64),
        };
        assert!(read_pack_file(Path::new("pack"), &entry).is_err());
    }

    #[test]
    fn specialist_model_pack_requires_layout_and_verifies_every_asset() {
        let directory = tempfile::tempdir().unwrap();
        let asset = |name: &str, bytes: &[u8]| {
            std::fs::write(directory.path().join(name), bytes).unwrap();
            serde_json::json!({
                "path": name,
                "blake3": blake3::hash(bytes).to_hex().to_string()
            })
        };
        let detector = asset("det.onnx", b"det");
        let recognizer = asset("rec.onnx", b"rec");
        let dictionary = asset("dict.txt", b"a\n");
        let table_model = asset("table.onnx", b"table");
        let table_tokens = asset("table.txt", b"<blank>\n<tr>\n<td>\n");
        let mut manifest = serde_json::json!({
            "schema_version": 1,
            "provider": "test",
            "name": "test pack",
            "version": "1",
            "generation": "test",
            "license": "Apache-2.0",
            "source": "https://example.invalid/model",
            "detector": detector,
            "recognizer": recognizer,
            "dictionary": dictionary,
            "table": {
                "name": "table",
                "model": table_model,
                "dictionary": table_tokens,
                "input_width": 32,
                "input_height": 32
            }
        });
        std::fs::write(
            directory.path().join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        assert!(PaddleOcrEngine::verify_model_pack(directory.path()).is_err());

        manifest["layout"] = asset("layout.onnx", b"layout");
        std::fs::write(
            directory.path().join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let verified = PaddleOcrEngine::verify_model_pack(directory.path()).unwrap();
        assert_eq!(verified.table.unwrap().name, "table");
    }
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
        assert_eq!(engine.recognizer_classes(), 18_385);
    }

    #[test]
    fn detector_boxes_map_back_to_high_resolution_recognition_source() {
        let crops = PaddleOcrEngine::ordered_source_crops(
            vec![TextBox {
                x0: 10.0,
                y0: 20.0,
                x1: 50.0,
                y1: 30.0,
                score: 0.9,
            }],
            (100, 100),
            (400, 300),
        );
        assert_eq!(crops.len(), 1);
        assert_eq!(crops[0].1, [40, 60, 200, 90]);
    }

    #[test]
    fn table_tokens_preserve_grid_and_spans() {
        let tokens = vec![
            "<table>",
            "<tr>",
            "<th colspan=\"2\">",
            "</th>",
            "</tr>",
            "<tr>",
            "<td>",
            "</td>",
            "<td>",
            "</td>",
            "</tr>",
            "</table>",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let (rows, columns, cells) = parse_table_tokens(&tokens).expect("table structure");
        assert_eq!((rows, columns), (2, 2));
        assert_eq!(cells[0].column_span, 2);
        assert_eq!((cells[2].row, cells[2].column), (1, 1));
    }

    #[test]
    fn formula_tokens_drop_sequence_control_markers() {
        let tokens = ["<bos>", "x", "^", "2", "</s>"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(clean_formula_tokens(&tokens), "x^2");
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
