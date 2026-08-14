//! Evidence-preserving PDF text intake with OCR fallback.

pub mod correction;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use lege_docir::{
    Document, GeometrySource, Page, PageSourceKind, Point, ProcessingManifest, Provenance,
    RecognitionConfidence, Region, RegionConfidence, RegionContent, RegionKind, Size, SizeF,
    SourceIdentity, TextBlock, TextEvidence, TextLine, TextWord, Transform, rect_polygon,
};
pub use lege_ocr::backend::OcrSchedulerConfig;
use lege_ocr::backend::{LegacyPageAdapter, PageBatch, PageOcrBackend, RecognitionBatch};
pub use lege_ocr::engine_tensorrt::TensorRtPaddleConfig;
use lege_pdf_read::{RasterPlane, RasterProduct, RenderSession};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PipelineConfig {
    pub profile: lege_docir::ProcessingProfile,
    pub quality: lege_docir::QualityMode,
    pub backend: BackendChoice,
    pub language: String,
    pub render_dpi: u32,
    pub max_page_pixels: u64,
    /// Ignore trustworthy embedded text and run OCR. Intended for evaluation
    /// against a PDF's existing text layer; normal production runs leave this
    /// disabled so native text is preserved.
    pub force_ocr: bool,
    pub scheduler: lege_ocr::backend::OcrSchedulerConfig,
    pub paddle_model_pack: Option<std::path::PathBuf>,
    /// Native TensorRT PP-OCRv6 worker. `None` permits auto-discovery when the
    /// backend is `auto` or `tensorrt-paddle`.
    pub tensorrt_paddle: Option<TensorRtPaddleConfig>,
    pub correction_mode: correction::CorrectionMode,
    pub correction_dictionary: Option<std::path::PathBuf>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            profile: lege_docir::ProcessingProfile::Search,
            quality: lege_docir::QualityMode::Thorough,
            backend: BackendChoice::Auto,
            language: "eng".to_string(),
            render_dpi: 300,
            max_page_pixels: 40_000_000,
            force_ocr: false,
            scheduler: lege_ocr::backend::OcrSchedulerConfig::default(),
            paddle_model_pack: None,
            tensorrt_paddle: None,
            correction_mode: correction::CorrectionMode::Conservative,
            correction_dictionary: None,
        }
    }
}

impl PipelineConfig {
    pub fn hash(&self) -> Result<String, PipelineError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&serde_json::to_vec(self)?);
        if let Some(dictionary) = &self.correction_dictionary {
            hasher.update(&std::fs::read(dictionary)?);
        }
        if let Some(model_pack) = &self.paddle_model_pack {
            hasher.update(&std::fs::read(model_pack.join("manifest.json"))?);
        }
        if let Some(runtime) = &self.tensorrt_paddle {
            for path in [
                &runtime.executable,
                &runtime.detector,
                &runtime.recognizer,
                &runtime.dictionary,
            ] {
                hasher.update(&std::fs::read(path)?);
            }
        }
        Ok(format!("blake3:{}", hasher.finalize().to_hex()))
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BackendChoice {
    Auto,
    #[serde(rename = "tensorrt-paddle")]
    TensorRtPaddle,
    Paddle,
    WindowsAi,
    WinOcrLegacy,
}

pub struct DocumentProcessor {
    config: PipelineConfig,
    backend: Box<dyn PageOcrBackend>,
    corrector: Option<correction::EnglishCorrector>,
    model_identity: Option<lege_docir::ModelIdentity>,
    backend_selection_warning: Option<String>,
}

impl std::fmt::Debug for DocumentProcessor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DocumentProcessor")
            .field("config", &self.config)
            .field("backend", &self.backend.name())
            .field("model_identity", &self.model_identity)
            .finish()
    }
}

impl DocumentProcessor {
    pub fn new(mut config: PipelineConfig) -> Result<Self, PipelineError> {
        if config.backend == BackendChoice::WindowsAi {
            return Err(PipelineError::UnavailableBackend("windows-ai"));
        }
        let (backend, resolved, tensorrt_paddle, backend_selection_warning) =
            initialize_backend(&config)?;
        config.backend = resolved;
        config.tensorrt_paddle = tensorrt_paddle;
        let corrector = config
            .correction_dictionary
            .as_deref()
            .map(|path| {
                correction::EnglishCorrector::from_frequency_file_for_mode(
                    path,
                    config.correction_mode,
                )
            })
            .transpose()?;
        let model_identity = if config.backend == BackendChoice::Paddle {
            config
                .paddle_model_pack
                .as_deref()
                .map(|directory| {
                    let manifest_path = directory.join("manifest.json");
                    let manifest_bytes = std::fs::read(&manifest_path)?;
                    let manifest =
                        lege_ocr::engine_paddle::PaddleOcrEngine::verify_model_pack(directory)
                            .map_err(|error| PipelineError::ModelPack(error.to_string()))?;
                    Ok::<_, PipelineError>(lege_docir::ModelIdentity {
                        provider: manifest.provider,
                        name: manifest.name,
                        version: manifest.version,
                        content_hash: Some(format!(
                            "blake3:{}",
                            blake3::hash(&manifest_bytes).to_hex()
                        )),
                        license: Some(manifest.license),
                        source: Some(manifest.source),
                    })
                })
                .transpose()?
        } else {
            None
        }
        .or_else(|| builtin_model_identity(&config));
        Ok(Self {
            config,
            backend,
            corrector,
            model_identity,
            backend_selection_warning,
        })
    }

    fn backend(&self) -> Result<&dyn PageOcrBackend, PipelineError> {
        Ok(self.backend.as_ref())
    }

    pub fn selected_backend(&self) -> BackendChoice {
        self.config.backend
    }

    pub fn selected_backend_name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn configuration_hash(&self) -> Result<String, PipelineError> {
        self.config.hash()
    }

    pub fn process_path(&self, path: &Path) -> Result<Document, PipelineError> {
        self.process_path_with_checkpoints(path, BTreeMap::new(), |_| Ok(()))
    }

    /// Process a document while reusing validated zero-based page shards.
    /// Newly computed pages are reported before document-level correction so
    /// checkpoints remain independent of presentation text-view selection.
    pub fn process_path_with_checkpoints(
        &self,
        path: &Path,
        existing_pages: BTreeMap<u32, Page>,
        mut checkpoint: impl FnMut(&Page) -> Result<(), String>,
    ) -> Result<Document, PipelineError> {
        let bytes = std::fs::read(path)?;
        let source_hash = format!("blake3:{}", blake3::hash(&bytes).to_hex());
        self.process_bytes_with_checkpoints(
            path,
            source_hash,
            Arc::from(bytes),
            existing_pages,
            &mut checkpoint,
        )
    }

    pub fn process_bytes(
        &self,
        path: &Path,
        source_hash: String,
        bytes: Arc<[u8]>,
    ) -> Result<Document, PipelineError> {
        self.process_bytes_with_checkpoints(path, source_hash, bytes, BTreeMap::new(), &mut |_| {
            Ok(())
        })
    }

    fn process_bytes_with_checkpoints(
        &self,
        path: &Path,
        source_hash: String,
        bytes: Arc<[u8]>,
        mut existing_pages: BTreeMap<u32, Page>,
        checkpoint: &mut impl FnMut(&Page) -> Result<(), String>,
    ) -> Result<Document, PipelineError> {
        let session = RenderSession::open(Arc::clone(&bytes), None)?;
        let configuration_hash = self.config.hash()?;
        let id = source_hash
            .strip_prefix("blake3:")
            .unwrap_or(&source_hash)
            .chars()
            .take(24)
            .collect::<String>();
        let source = SourceIdentity {
            path: path.to_string_lossy().into_owned(),
            content_hash: source_hash,
            byte_len: bytes.len() as u64,
            mime_type: "application/pdf".to_string(),
        };
        let mut document = Document::new(
            id,
            source,
            ProcessingManifest {
                pipeline_version: env!("CARGO_PKG_VERSION").to_string(),
                profile: self.config.profile,
                quality: self.config.quality,
                configuration_hash,
                models: self.model_identity.clone().into_iter().collect(),
                warnings: self.backend_selection_warning.clone().into_iter().collect(),
            },
        );
        document.metadata.title = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned());
        for page_index in 0..session.page_count() {
            let page = if let Some(page) = existing_pages.remove(&page_index) {
                if page.index != page_index
                    || page.source_size.width == 0
                    || page.source_size.height == 0
                {
                    return Err(PipelineError::InvalidCheckpoint(page_index));
                }
                page
            } else {
                let page = self.process_page(&session, page_index)?;
                checkpoint(&page).map_err(PipelineError::Checkpoint)?;
                page
            };
            document.pages.push(page);
        }
        document.outline = lege_pdf_read::extract_outline(&session)
            .into_iter()
            .map(map_outline)
            .collect();
        if self.config.profile != lege_docir::ProcessingProfile::Search {
            apply_lightweight_layout(&mut document);
            document.processing.warnings.push(
                "Applied deterministic geometry-based heading and repeated header/footer layout reconstruction; configured table/formula specialists run on OCR pages"
                    .to_string(),
            );
        }
        if let Some(corrector) = &self.corrector {
            let summary = corrector.correct_document(&mut document, self.config.correction_mode);
            document.processing.warnings.push(format!(
                "English correction examined {} tokens, suggested {}, and applied {}",
                summary.examined, summary.suggested, summary.applied
            ));
        } else if self.config.correction_mode != correction::CorrectionMode::Disabled {
            document.processing.warnings.push(
                "Spell correction requested but no licensed correction dictionary was configured; raw OCR evidence was preserved"
                    .to_string(),
            );
        }
        document
            .validate()
            .map_err(|error| PipelineError::InvalidDocument(error.to_string()))?;
        Ok(document)
    }

    fn process_page(
        &self,
        session: &RenderSession,
        page_index: u32,
    ) -> Result<Page, PipelineError> {
        let geometry = session.page_geometry(page_index)?;
        let (mut width, mut height) = raster_dimensions(
            geometry.display_width(),
            geometry.display_height(),
            self.config.render_dpi,
            self.config.max_page_pixels,
        )?;
        let evidence = lege_pdf_read::page_text_evidence(session, page_index, width, height)?;
        let (source_kind, regions, warnings) = if evidence.trustworthy && !self.config.force_ocr {
            (
                match evidence.kind {
                    lege_pdf_read::PageContentKind::Hybrid => PageSourceKind::Hybrid,
                    _ => PageSourceKind::NativeText,
                },
                native_regions(
                    page_index,
                    width,
                    height,
                    &evidence.text,
                    &evidence.words,
                    &self.config.language,
                ),
                Vec::new(),
            )
        } else {
            let (gray, direct_scan) = if let Some(scan) = evidence.direct_scan.as_ref() {
                let decoded = image::load_from_memory(&scan.jpeg)
                    .map_err(|error| PipelineError::DirectScan(error.to_string()))?
                    .to_luma8();
                let decoded = limit_gray_pixels(decoded, self.config.max_page_pixels);
                width = decoded.width();
                height = decoded.height();
                (decoded, true)
            } else {
                let compiled = session.compile(page_index)?;
                let plane = session.render(&compiled, &RasterProduct::gray8(width, height))?;
                (gray_image(plane)?, false)
            };
            let backend = self.backend()?;
            let mut pages = backend
                .recognize_pages(PageBatch {
                    pages: std::slice::from_ref(&gray),
                    language: &self.config.language,
                })
                .map_err(PipelineError::Backend)?;
            let mut lines = pages.pop().unwrap_or_default();
            let retry_count = selective_retry(backend, &gray, &mut lines, &self.config.language)?;
            let mut warnings = if self.config.force_ocr && evidence.trustworthy {
                vec![
                    "Trustworthy native PDF text was deliberately ignored for OCR evaluation"
                        .to_string(),
                ]
            } else if evidence.text.trim().is_empty() {
                Vec::new()
            } else {
                vec![
                    "Native PDF text failed quality checks and was replaced by OCR evidence"
                        .to_string(),
                ]
            };
            if retry_count > 0 {
                warnings.push(format!(
                    "Selective alternate preprocessing replaced {retry_count} low-confidence OCR line(s)"
                ));
            }
            let mut regions = ocr_regions(
                page_index,
                width,
                height,
                lines,
                backend.name(),
                &self.config.language,
            );
            if self.config.profile != lege_docir::ProcessingProfile::Search {
                let specialists = backend
                    .specialize_page(&gray, &self.config.language)
                    .map_err(PipelineError::Backend)?;
                if !specialists.is_empty() {
                    let specialist_count = specialists.len();
                    merge_specialist_regions(
                        page_index,
                        width,
                        height,
                        &mut regions,
                        specialists,
                        &self.config.language,
                    );
                    warnings.push(format!(
                        "Applied {specialist_count} table/formula specialist region(s)"
                    ));
                }
            }
            (
                if direct_scan {
                    PageSourceKind::ScannedImage
                } else {
                    PageSourceKind::Rendered
                },
                regions,
                warnings,
            )
        };
        let reading_order = regions.iter().map(|region| region.id.clone()).collect();
        Ok(Page {
            index: page_index,
            source_size: Size { width, height },
            page_size_points: SizeF {
                width: geometry.display_width(),
                height: geometry.display_height(),
            },
            source_to_page: Transform {
                matrix: [
                    geometry.display_width() / width.max(1) as f64,
                    0.0,
                    0.0,
                    geometry.display_height() / height.max(1) as f64,
                    0.0,
                    0.0,
                ],
            },
            source_kind,
            image: None,
            regions,
            reading_order,
            warnings,
        })
    }
}

fn map_outline(node: lege_pdf_read::OwnedBookmarkNode) -> lege_docir::OutlineNode {
    lege_docir::OutlineNode {
        title: node.title,
        page_index: node.source_page as u32,
        children: node.children.into_iter().map(map_outline).collect(),
    }
}

fn apply_lightweight_layout(document: &mut Document) {
    use std::collections::HashMap;
    let mut edge_text = HashMap::<(bool, String), usize>::new();
    for page in &document.pages {
        for region in &page.regions {
            let Some((_, top, _, bottom)) = docir_polygon_bounds(&region.polygon) else {
                continue;
            };
            let text = region
                .content
                .plain_text(lege_docir::TextView::Normalized)
                .unwrap_or_default();
            let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                continue;
            }
            let height = page.source_size.height as f32;
            if bottom <= height * 0.1 {
                *edge_text.entry((true, normalized)).or_default() += 1;
            } else if top >= height * 0.9 {
                *edge_text.entry((false, normalized)).or_default() += 1;
            }
        }
    }
    let repeat_threshold = document.pages.len().div_ceil(2).max(2);
    for page in &mut document.pages {
        let heights = page
            .regions
            .iter()
            .filter_map(|region| {
                docir_polygon_bounds(&region.polygon).map(|(_, y0, _, y1)| y1 - y0)
            })
            .filter(|height| *height > 0.0)
            .collect::<Vec<_>>();
        let mut sorted = heights.clone();
        sorted.sort_by(f32::total_cmp);
        let median = sorted.get(sorted.len() / 2).copied().unwrap_or(1.0);
        for region in &mut page.regions {
            let Some((_, top, _, bottom)) = docir_polygon_bounds(&region.polygon) else {
                continue;
            };
            let text = region
                .content
                .plain_text(lege_docir::TextView::Normalized)
                .unwrap_or_default();
            let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let page_height = page.source_size.height as f32;
            if edge_text
                .get(&(true, normalized.clone()))
                .copied()
                .unwrap_or(0)
                >= repeat_threshold
            {
                region.kind = RegionKind::Header;
            } else if edge_text.get(&(false, normalized)).copied().unwrap_or(0) >= repeat_threshold
            {
                region.kind = RegionKind::Footer;
            } else if top < page_height * 0.2
                && bottom - top > median * 1.45
                && text.chars().count() < 160
            {
                region.kind = RegionKind::Title;
            } else if bottom - top > median * 1.2 && text.chars().count() < 160 {
                region.kind = RegionKind::Heading;
            }
        }
    }
}

fn docir_polygon_bounds(polygon: &[Point]) -> Option<(f32, f32, f32, f32)> {
    let first = polygon.first()?;
    Some(
        polygon
            .iter()
            .skip(1)
            .fold((first.x, first.y, first.x, first.y), |bounds, point| {
                (
                    bounds.0.min(point.x),
                    bounds.1.min(point.y),
                    bounds.2.max(point.x),
                    bounds.3.max(point.y),
                )
            }),
    )
}

fn selective_retry(
    backend: &dyn PageOcrBackend,
    page: &image::GrayImage,
    lines: &mut [lege_ocr::types::OcrLineResult],
    language: &str,
) -> Result<usize, PipelineError> {
    let selected = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line_quality(line) < 0.72)
        .filter_map(|(index, line)| {
            let [x0, y0, x1, y1] = line.bbox_highres;
            let x1 = x1.min(page.width());
            let y1 = y1.min(page.height());
            (x1 > x0 && y1 > y0).then_some((index, [x0, y0, x1, y1]))
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok(0);
    }
    let alternatives = selected
        .iter()
        .map(|(_, [x0, y0, x1, y1])| {
            let crop = image::imageops::crop_imm(page, *x0, *y0, x1 - x0, y1 - y0).to_image();
            image::imageops::contrast(&crop, 28.0)
        })
        .collect::<Vec<_>>();
    let retries = backend
        .recognize_lines(RecognitionBatch {
            lines: &alternatives,
            language,
        })
        .map_err(PipelineError::Backend)?;
    let mut replaced = 0;
    for ((index, bbox), mut retry) in selected.into_iter().zip(retries) {
        retry.bbox_highres = bbox;
        if line_quality(&retry) > line_quality(&lines[index]) + 0.04 {
            lines[index] = retry;
            replaced += 1;
        }
    }
    Ok(replaced)
}

fn line_quality(line: &lege_ocr::types::OcrLineResult) -> f32 {
    let text = line.text.trim();
    if text.is_empty() {
        return 0.0;
    }
    let abnormal = text
        .chars()
        .filter(|character| character.is_control() || *character == '\u{fffd}')
        .count() as f32
        / text.chars().count().max(1) as f32;
    line.confidence.unwrap_or(0.55) - abnormal.min(0.5)
}

fn native_regions(
    page: u32,
    width: u32,
    height: u32,
    text: &str,
    words: &[lege_pdf_read::NativeTextWord],
    language: &str,
) -> Vec<Region> {
    let provenance = Provenance {
        provider: "native-pdf".to_string(),
        model: None,
        preprocessing: None,
        language: Some(language.to_string()),
    };
    let text_words = words
        .iter()
        .map(|word| TextWord {
            text: TextEvidence::raw(word.text.clone()),
            polygon: rect_polygon(word.bbox[0], word.bbox[1], word.bbox[2], word.bbox[3]),
            confidence: RecognitionConfidence::default(),
            geometry_source: GeometrySource::NativePdf,
        })
        .collect();
    vec![Region {
        id: format!("p{page}-r0"),
        kind: RegionKind::Paragraph,
        polygon: rect_polygon(0.0, 0.0, width as f32, height as f32),
        confidence: RegionConfidence::default(),
        content: RegionContent::Text(TextBlock {
            lines: vec![TextLine {
                text: TextEvidence::raw(text.trim().to_string()),
                polygon: rect_polygon(0.0, 0.0, width as f32, height as f32),
                confidence: RecognitionConfidence::default(),
                words: text_words,
                provenance: provenance.clone(),
            }],
        }),
        provenance,
    }]
}

fn ocr_regions(
    page: u32,
    width: u32,
    height: u32,
    lines: Vec<lege_ocr::types::OcrLineResult>,
    provider: &str,
    language: &str,
) -> Vec<Region> {
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let [left, top, right, bottom] = line.bbox_highres;
            let provenance = Provenance {
                provider: provider.to_string(),
                model: None,
                preprocessing: Some("natural-grayscale".to_string()),
                language: Some(language.to_string()),
            };
            let words = line
                .words
                .into_iter()
                .map(|word| TextWord {
                    text: TextEvidence::raw(word.text),
                    polygon: rect_polygon(
                        (left + word.bbox_crop_local[0]) as f32,
                        (top + word.bbox_crop_local[1]) as f32,
                        (left + word.bbox_crop_local[2]) as f32,
                        (top + word.bbox_crop_local[3]) as f32,
                    ),
                    confidence: RecognitionConfidence {
                        mean_token: word.confidence,
                        ..Default::default()
                    },
                    geometry_source: if provider == "paddle" {
                        GeometrySource::CtcEstimated
                    } else {
                        GeometrySource::OcrBackend
                    },
                })
                .collect();
            Region {
                id: format!("p{page}-r{index}"),
                kind: RegionKind::Paragraph,
                polygon: bounded_rect(left, top, right, bottom, width, height),
                confidence: RegionConfidence {
                    recognition: line.confidence,
                    ..Default::default()
                },
                content: RegionContent::Text(TextBlock {
                    lines: vec![TextLine {
                        text: TextEvidence::raw(line.text),
                        polygon: bounded_rect(left, top, right, bottom, width, height),
                        confidence: RecognitionConfidence {
                            mean_token: line.confidence,
                            ..Default::default()
                        },
                        words,
                        provenance: provenance.clone(),
                    }],
                }),
                provenance,
            }
        })
        .collect()
}

fn merge_specialist_regions(
    page: u32,
    width: u32,
    height: u32,
    regions: &mut Vec<Region>,
    specialists: Vec<lege_ocr::backend::SpecialistRegion>,
    language: &str,
) {
    let mut consumed = vec![false; regions.len()];
    let mut additions = Vec::new();
    for (specialist_index, specialist) in specialists.into_iter().enumerate() {
        let bounds = [
            specialist.bbox[0].min(width),
            specialist.bbox[1].min(height),
            specialist.bbox[2].min(width),
            specialist.bbox[3].min(height),
        ];
        if bounds[2] <= bounds[0] || bounds[3] <= bounds[1] {
            continue;
        }
        let mut evidence = Vec::<(TextBlock, [f32; 4])>::new();
        for (index, region) in regions.iter().enumerate() {
            if consumed[index] || !region_overlaps(bounds, region) {
                continue;
            }
            if let RegionContent::Text(block) = &region.content {
                let region_bounds = docir_polygon_bounds(&region.polygon)
                    .map(|(x0, y0, x1, y1)| [x0, y0, x1, y1])
                    .unwrap_or([0.0; 4]);
                evidence.push((block.clone(), region_bounds));
                consumed[index] = true;
            }
        }
        let provenance = Provenance {
            provider: specialist.provider,
            model: specialist.model,
            preprocessing: Some("layout-crop".to_string()),
            language: Some(language.to_string()),
        };
        let (kind, content) = match specialist.content {
            lege_ocr::backend::SpecialistContent::Table {
                rows,
                columns,
                cells,
            } => {
                let cells = cells
                    .into_iter()
                    .map(|cell| {
                        let cell_bounds = table_cell_bounds(bounds, rows, columns, &cell);
                        let blocks = evidence
                            .iter()
                            .filter(|(_, text_bounds)| {
                                let center_x = (text_bounds[0] + text_bounds[2]) * 0.5;
                                let center_y = (text_bounds[1] + text_bounds[3]) * 0.5;
                                center_x >= cell_bounds[0]
                                    && center_x <= cell_bounds[2]
                                    && center_y >= cell_bounds[1]
                                    && center_y <= cell_bounds[3]
                            })
                            .map(|(block, _)| block.clone())
                            .collect();
                        lege_docir::TableCell {
                            row: cell.row,
                            column: cell.column,
                            row_span: cell.row_span,
                            column_span: cell.column_span,
                            polygon: rect_polygon(
                                cell_bounds[0],
                                cell_bounds[1],
                                cell_bounds[2],
                                cell_bounds[3],
                            ),
                            blocks,
                            is_header: cell.row == 0,
                            confidence: specialist.recognition_confidence,
                        }
                    })
                    .collect();
                (
                    RegionKind::Table,
                    RegionContent::Table(lege_docir::Table {
                        rows,
                        columns,
                        cells,
                    }),
                )
            }
            lege_ocr::backend::SpecialistContent::Formula { latex, display } => {
                let lines = evidence
                    .into_iter()
                    .flat_map(|(block, _)| block.lines)
                    .collect::<Vec<_>>();
                (
                    RegionKind::Formula,
                    RegionContent::Formula(lege_docir::Formula {
                        latex: Some(latex),
                        mathml: None,
                        display: if display {
                            lege_docir::FormulaDisplay::Display
                        } else {
                            lege_docir::FormulaDisplay::Inline
                        },
                        source_crop: None,
                        confidence: specialist.recognition_confidence,
                        raw_ocr: (!lines.is_empty()).then_some(TextBlock { lines }),
                    }),
                )
            }
        };
        additions.push(Region {
            id: format!("p{page}-s{specialist_index}"),
            kind,
            polygon: bounded_rect(bounds[0], bounds[1], bounds[2], bounds[3], width, height),
            confidence: RegionConfidence {
                detection: specialist.detection_confidence,
                layout: specialist.detection_confidence,
                recognition: specialist.recognition_confidence,
            },
            content,
            provenance,
        });
    }
    let mut kept = regions
        .drain(..)
        .enumerate()
        .filter_map(|(index, region)| (!consumed[index]).then_some(region))
        .collect::<Vec<_>>();
    kept.extend(additions);
    kept.sort_by(|left, right| {
        let left = docir_polygon_bounds(&left.polygon).unwrap_or_default();
        let right = docir_polygon_bounds(&right.polygon).unwrap_or_default();
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.0.total_cmp(&right.0))
    });
    *regions = kept;
}

fn region_overlaps(bounds: [u32; 4], region: &Region) -> bool {
    let Some((x0, y0, x1, y1)) = docir_polygon_bounds(&region.polygon) else {
        return false;
    };
    let center_x = (x0 + x1) * 0.5;
    let center_y = (y0 + y1) * 0.5;
    center_x >= bounds[0] as f32
        && center_x <= bounds[2] as f32
        && center_y >= bounds[1] as f32
        && center_y <= bounds[3] as f32
}

fn table_cell_bounds(
    table: [u32; 4],
    rows: u32,
    columns: u32,
    cell: &lege_ocr::backend::TableCellStructure,
) -> [f32; 4] {
    let width = table[2].saturating_sub(table[0]) as f32;
    let height = table[3].saturating_sub(table[1]) as f32;
    let columns = columns.max(1) as f32;
    let rows = rows.max(1) as f32;
    [
        table[0] as f32 + width * cell.column as f32 / columns,
        table[1] as f32 + height * cell.row as f32 / rows,
        table[0] as f32 + width * cell.column.saturating_add(cell.column_span) as f32 / columns,
        table[1] as f32 + height * cell.row.saturating_add(cell.row_span) as f32 / rows,
    ]
}

fn bounded_rect(
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
    width: u32,
    height: u32,
) -> Vec<Point> {
    rect_polygon(
        left.min(width) as f32,
        top.min(height) as f32,
        right.min(width) as f32,
        bottom.min(height) as f32,
    )
}

fn gray_image(plane: RasterPlane) -> Result<image::GrayImage, PipelineError> {
    let RasterPlane::Gray8(surface) = plane else {
        return Err(PipelineError::UnexpectedRaster);
    };
    if surface.stride == surface.width as usize {
        return image::GrayImage::from_raw(surface.width, surface.height, surface.pixels.to_vec())
            .ok_or(PipelineError::UnexpectedRaster);
    }
    let mut packed = Vec::with_capacity(surface.width as usize * surface.height as usize);
    for row in 0..surface.height as usize {
        let start = row
            .checked_mul(surface.stride)
            .ok_or(PipelineError::UnexpectedRaster)?;
        let end = start
            .checked_add(surface.width as usize)
            .ok_or(PipelineError::UnexpectedRaster)?;
        packed.extend_from_slice(
            surface
                .pixels
                .get(start..end)
                .ok_or(PipelineError::UnexpectedRaster)?,
        );
    }
    image::GrayImage::from_raw(surface.width, surface.height, packed)
        .ok_or(PipelineError::UnexpectedRaster)
}

fn limit_gray_pixels(image: image::GrayImage, max_pixels: u64) -> image::GrayImage {
    let pixels = u64::from(image.width()) * u64::from(image.height());
    if pixels <= max_pixels.max(1) {
        return image;
    }
    let scale = (max_pixels.max(1) as f64 / pixels as f64).sqrt();
    let width = (image.width() as f64 * scale).round().max(1.0) as u32;
    let height = (image.height() as f64 * scale).round().max(1.0) as u32;
    image::imageops::resize(&image, width, height, image::imageops::FilterType::Triangle)
}

fn raster_dimensions(
    width_points: f64,
    height_points: f64,
    dpi: u32,
    max_pixels: u64,
) -> Result<(u32, u32), PipelineError> {
    if !width_points.is_finite()
        || !height_points.is_finite()
        || width_points <= 0.0
        || height_points <= 0.0
    {
        return Err(PipelineError::InvalidGeometry);
    }
    let mut width = (width_points * dpi as f64 / 72.0).round().max(1.0);
    let mut height = (height_points * dpi as f64 / 72.0).round().max(1.0);
    let pixels = width * height;
    if pixels > max_pixels.max(1) as f64 {
        let scale = (max_pixels.max(1) as f64 / pixels).sqrt();
        width *= scale;
        height *= scale;
    }
    if width > u32::MAX as f64 || height > u32::MAX as f64 {
        return Err(PipelineError::InvalidGeometry);
    }
    Ok((
        width.round().max(1.0) as u32,
        height.round().max(1.0) as u32,
    ))
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("PDF read error: {0}")]
    Pdf(#[from] lege_pdf_read::ReadError),
    #[error("backend error: {0}")]
    Backend(#[source] anyhow::Error),
    #[error("backend `{0}` is not available in this build or on this platform")]
    UnavailableBackend(&'static str),
    #[error("failed to initialize OCR backend: {0}")]
    BackendInitialization(String),
    #[error("unexpected raster format or stride")]
    UnexpectedRaster,
    #[error("could not decode directly extracted scan image: {0}")]
    DirectScan(String),
    #[error("invalid PDF page geometry")]
    InvalidGeometry,
    #[error("invalid document: {0}")]
    InvalidDocument(String),
    #[error("invalid page checkpoint for page {0}")]
    InvalidCheckpoint(u32),
    #[error("could not persist page checkpoint: {0}")]
    Checkpoint(String),
    #[error("configuration serialization error: {0}")]
    Configuration(#[from] serde_json::Error),
    #[error("invalid OCR model pack: {0}")]
    ModelPack(String),
    #[error("correction error: {0}")]
    Correction(#[from] correction::CorrectionError),
}

type InitializedBackend = (
    Box<dyn PageOcrBackend>,
    BackendChoice,
    Option<TensorRtPaddleConfig>,
    Option<String>,
);

fn initialize_backend(config: &PipelineConfig) -> Result<InitializedBackend, PipelineError> {
    match config.backend {
        BackendChoice::Auto => {
            #[cfg(target_os = "windows")]
            {
                let candidate = config.tensorrt_paddle.clone().or_else(|| {
                    TensorRtPaddleConfig::discover(config.scheduler.max_batch_lines.min(8))
                });
                if let Some(runtime) = candidate {
                    match lege_ocr::engine_tensorrt::TensorRtPaddleEngine::start(&runtime) {
                        Ok(engine) => {
                            return Ok((
                                Box::new(engine),
                                BackendChoice::TensorRtPaddle,
                                Some(runtime),
                                None,
                            ));
                        }
                        Err(tensorrt_error) => {
                            let fallback = initialize_winocr().map_err(|winocr_error| {
                                PipelineError::BackendInitialization(format!(
                                    "TensorRT preflight failed ({tensorrt_error:#}); Windows OCR fallback also failed ({winocr_error})"
                                ))
                            })?;
                            return Ok((
                                fallback,
                                BackendChoice::WinOcrLegacy,
                                None,
                                Some(format!(
                                    "TensorRT PP-OCRv6 preflight failed before the job; selected Windows Runtime OCR for the complete job: {tensorrt_error:#}"
                                )),
                            ));
                        }
                    }
                }
                Ok((
                    initialize_winocr()?,
                    BackendChoice::WinOcrLegacy,
                    None,
                    Some(
                        "TensorRT PP-OCRv6 runtime was not discovered; selected Windows Runtime OCR for the complete job"
                            .to_string(),
                    ),
                ))
            }
            #[cfg(not(target_os = "windows"))]
            {
                Ok((
                    initialize_paddle(config)?,
                    BackendChoice::Paddle,
                    None,
                    None,
                ))
            }
        }
        BackendChoice::TensorRtPaddle => {
            let runtime = config.tensorrt_paddle.clone().or_else(|| {
                TensorRtPaddleConfig::discover(config.scheduler.max_batch_lines.min(8))
            });
            let runtime = runtime.ok_or_else(|| {
                PipelineError::BackendInitialization(
                    "TensorRT OCR runtime was not found; pass --tensorrt-ocr-root or set LEGE_TENSORRT_OCR_ROOT"
                        .to_string(),
                )
            })?;
            let engine = lege_ocr::engine_tensorrt::TensorRtPaddleEngine::start(&runtime)
                .map_err(|error| PipelineError::BackendInitialization(format!("{error:#}")))?;
            Ok((
                Box::new(engine),
                BackendChoice::TensorRtPaddle,
                Some(runtime),
                None,
            ))
        }
        BackendChoice::Paddle => Ok((
            initialize_paddle(config)?,
            BackendChoice::Paddle,
            None,
            None,
        )),
        BackendChoice::WinOcrLegacy => Ok((
            initialize_winocr()?,
            BackendChoice::WinOcrLegacy,
            None,
            None,
        )),
        BackendChoice::WindowsAi => Err(PipelineError::UnavailableBackend("windows-ai")),
    }
}

#[cfg(feature = "paddle-ocr")]
fn initialize_paddle(config: &PipelineConfig) -> Result<Box<dyn PageOcrBackend>, PipelineError> {
    let engine = if let Some(directory) = config.paddle_model_pack.as_deref() {
        lege_ocr::engine_paddle::PaddleOcrEngine::from_model_pack_with_scheduler(
            directory,
            config.scheduler,
        )
        .map_err(|error| PipelineError::BackendInitialization(error.to_string()))?
        .0
    } else {
        lege_ocr::engine_paddle::PaddleOcrEngine::from_embedded_with_scheduler(config.scheduler)
            .map_err(|error| PipelineError::BackendInitialization(error.to_string()))?
    };
    let probe = image::GrayImage::from_pixel(32, 32, image::Luma([255]));
    engine
        .recognize_pages(PageBatch {
            pages: std::slice::from_ref(&probe),
            language: &config.language,
        })
        .map_err(|error| {
            PipelineError::BackendInitialization(format!("Paddle/WGPU preflight failed: {error:#}"))
        })?;
    Ok(Box::new(engine))
}

#[cfg(not(feature = "paddle-ocr"))]
fn initialize_paddle(_config: &PipelineConfig) -> Result<Box<dyn PageOcrBackend>, PipelineError> {
    Err(PipelineError::BackendInitialization(
        "paddle is not compiled into this build".to_string(),
    ))
}

#[cfg(target_os = "windows")]
fn initialize_winocr() -> Result<Box<dyn PageOcrBackend>, PipelineError> {
    Ok(Box::new(LegacyPageAdapter::new(Box::new(
        lege_ocr::engine::WinOcrEngine,
    ))))
}

#[cfg(not(target_os = "windows"))]
fn initialize_winocr() -> Result<Box<dyn PageOcrBackend>, PipelineError> {
    Err(PipelineError::UnavailableBackend("winocr-legacy"))
}

fn builtin_model_identity(config: &PipelineConfig) -> Option<lege_docir::ModelIdentity> {
    if config.backend == BackendChoice::TensorRtPaddle {
        let runtime = config.tensorrt_paddle.as_ref()?;
        let mut hasher = blake3::Hasher::new();
        for path in [&runtime.detector, &runtime.recognizer, &runtime.dictionary] {
            hasher.update(&std::fs::read(path).ok()?);
        }
        Some(lege_docir::ModelIdentity {
            provider: "PaddlePaddle/TurboOCR".into(),
            name: "PP-OCRv6-tiny TensorRT".into(),
            version: "native-windows-worker-v1".into(),
            content_hash: Some(format!("blake3:{}", hasher.finalize().to_hex())),
            license: Some("Apache-2.0".into()),
            source: Some("https://github.com/aiptimizer/TurboOCR".into()),
        })
    } else if config.backend == BackendChoice::Paddle {
        Some(lege_docir::ModelIdentity {
            provider: "PaddlePaddle".into(),
            name: "PP-OCRv5 embedded compatibility".into(),
            version: "workspace-assets".into(),
            content_hash: None,
            license: Some("Apache-2.0".into()),
            source: Some("https://github.com/PaddlePaddle/PaddleOCR".into()),
        })
    } else if config.backend == BackendChoice::WinOcrLegacy {
        Some(lege_docir::ModelIdentity {
            provider: "Microsoft".into(),
            name: "Windows Runtime OCR".into(),
            version: "operating-system".into(),
            content_hash: None,
            license: Some("Windows component".into()),
            source: Some("https://learn.microsoft.com/windows/ai/apis/text-recognition".into()),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn render_dimensions_respect_pixel_budget() {
        let (width, height) = raster_dimensions(720.0, 720.0, 300, 1_000_000).unwrap();
        assert!((width as u64 * height as u64) <= 1_001_000);
    }

    #[test]
    fn force_ocr_changes_the_resumable_configuration_identity() {
        let normal = PipelineConfig::default();
        let mut forced = normal.clone();
        forced.force_ocr = true;
        assert_ne!(normal.hash().unwrap(), forced.hash().unwrap());
    }

    fn text_region(id: &str, bbox: [f32; 4], text: &str) -> Region {
        let provenance = Provenance {
            provider: "test-ocr".into(),
            model: None,
            preprocessing: None,
            language: Some("eng".into()),
        };
        Region {
            id: id.into(),
            kind: RegionKind::Paragraph,
            polygon: rect_polygon(bbox[0], bbox[1], bbox[2], bbox[3]),
            confidence: RegionConfidence::default(),
            content: RegionContent::Text(TextBlock {
                lines: vec![TextLine {
                    text: TextEvidence::raw(text),
                    polygon: rect_polygon(bbox[0], bbox[1], bbox[2], bbox[3]),
                    confidence: RecognitionConfidence::default(),
                    words: Vec::new(),
                    provenance: provenance.clone(),
                }],
            }),
            provenance,
        }
    }

    #[test]
    fn table_specialist_rehomes_ocr_evidence_into_cells() {
        let mut regions = vec![
            text_region("left", [5.0, 5.0, 45.0, 45.0], "alpha"),
            text_region("right", [55.0, 5.0, 95.0, 45.0], "beta"),
        ];
        merge_specialist_regions(
            0,
            100,
            50,
            &mut regions,
            vec![lege_ocr::backend::SpecialistRegion {
                bbox: [0, 0, 100, 50],
                detection_confidence: Some(0.9),
                recognition_confidence: Some(0.8),
                provider: "test-specialist".into(),
                model: Some("table-model".into()),
                content: lege_ocr::backend::SpecialistContent::Table {
                    rows: 1,
                    columns: 2,
                    cells: vec![
                        lege_ocr::backend::TableCellStructure {
                            row: 0,
                            column: 0,
                            row_span: 1,
                            column_span: 1,
                        },
                        lege_ocr::backend::TableCellStructure {
                            row: 0,
                            column: 1,
                            row_span: 1,
                            column_span: 1,
                        },
                    ],
                },
            }],
            "eng",
        );
        assert_eq!(regions.len(), 1);
        let RegionContent::Table(table) = &regions[0].content else {
            panic!("expected table region")
        };
        assert_eq!(table.plain_text(lege_docir::TextView::Raw), "alpha\tbeta");
    }

    #[test]
    fn formula_specialist_retains_raw_ocr_block() {
        let mut regions = vec![text_region("formula", [0.0, 0.0, 100.0, 50.0], "x2")];
        merge_specialist_regions(
            0,
            100,
            50,
            &mut regions,
            vec![lege_ocr::backend::SpecialistRegion {
                bbox: [0, 0, 100, 50],
                detection_confidence: Some(0.9),
                recognition_confidence: Some(0.8),
                provider: "test-specialist".into(),
                model: Some("formula-model".into()),
                content: lege_ocr::backend::SpecialistContent::Formula {
                    latex: "x^2".into(),
                    display: true,
                },
            }],
            "eng",
        );
        let RegionContent::Formula(formula) = &regions[0].content else {
            panic!("expected formula region")
        };
        assert_eq!(formula.latex.as_deref(), Some("x^2"));
        assert_eq!(
            formula
                .raw_ocr
                .as_ref()
                .map(|block| block.plain_text(lege_docir::TextView::Raw)),
            Some("x2".into())
        );
    }
}
