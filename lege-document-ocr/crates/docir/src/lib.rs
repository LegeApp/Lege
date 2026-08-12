//! Canonical, backend-neutral document evidence produced by the Lege OCR pipeline.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const SCHEMA_NAME: &str = "lege.document";
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Document {
    pub schema: SchemaIdentity,
    pub id: String,
    pub source: SourceIdentity,
    #[serde(default)]
    pub metadata: DocumentMetadata,
    #[serde(default)]
    pub pages: Vec<Page>,
    #[serde(default)]
    pub outline: Vec<OutlineNode>,
    pub processing: ProcessingManifest,
}

impl Document {
    pub fn new(
        id: impl Into<String>,
        source: SourceIdentity,
        processing: ProcessingManifest,
    ) -> Self {
        Self {
            schema: SchemaIdentity::current(),
            id: id.into(),
            source,
            metadata: DocumentMetadata::default(),
            pages: Vec::new(),
            outline: Vec::new(),
            processing,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema.name != SCHEMA_NAME || self.schema.version != SCHEMA_VERSION {
            return Err(ValidationError::UnsupportedSchema {
                name: self.schema.name.clone(),
                version: self.schema.version,
            });
        }
        for (expected, page) in self.pages.iter().enumerate() {
            if page.index as usize != expected {
                return Err(ValidationError::NonContiguousPage {
                    expected: expected as u32,
                    actual: page.index,
                });
            }
            if page.source_size.width == 0 || page.source_size.height == 0 {
                return Err(ValidationError::EmptyPage(page.index));
            }
        }
        Ok(())
    }

    pub fn full_text(&self, view: TextView) -> String {
        self.pages
            .iter()
            .map(|page| page.plain_text(view))
            .collect::<Vec<_>>()
            .join("\n\u{000c}\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaIdentity {
    pub name: String,
    pub version: u32,
}

impl SchemaIdentity {
    pub fn current() -> Self {
        Self {
            name: SCHEMA_NAME.to_string(),
            version: SCHEMA_VERSION,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceIdentity {
    pub path: String,
    pub content_hash: String,
    pub byte_len: u64,
    #[serde(default)]
    pub mime_type: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub language: Option<String>,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcessingManifest {
    pub pipeline_version: String,
    pub profile: ProcessingProfile,
    pub quality: QualityMode,
    pub configuration_hash: String,
    #[serde(default)]
    pub models: Vec<ModelIdentity>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessingProfile {
    Search,
    Structured,
    Scientific,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum QualityMode {
    Thorough,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelIdentity {
    pub provider: String,
    pub name: String,
    pub version: String,
    pub content_hash: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Page {
    pub index: u32,
    pub source_size: Size,
    pub page_size_points: SizeF,
    pub source_to_page: Transform,
    pub source_kind: PageSourceKind,
    pub image: Option<PageImageRef>,
    #[serde(default)]
    pub regions: Vec<Region>,
    #[serde(default)]
    pub reading_order: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Page {
    pub fn plain_text(&self, view: TextView) -> String {
        let mut chunks = Vec::new();
        for id in &self.reading_order {
            if let Some(region) = self.regions.iter().find(|region| &region.id == id)
                && let Some(text) = region.content.plain_text(view)
            {
                chunks.push(text);
            }
        }
        for region in &self.regions {
            if !self.reading_order.iter().any(|id| id == &region.id)
                && let Some(text) = region.content.plain_text(view)
            {
                chunks.push(text);
            }
        }
        chunks
            .into_iter()
            .filter(|value| !value.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PageSourceKind {
    NativeText,
    ScannedImage,
    Hybrid,
    Rendered,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct SizeF {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Transform {
    pub matrix: [f64; 6],
}

impl Transform {
    pub const IDENTITY: Self = Self {
        matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
    };
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageImageRef {
    pub path: String,
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Region {
    pub id: String,
    pub kind: RegionKind,
    pub polygon: Polygon,
    pub confidence: RegionConfidence,
    pub content: RegionContent,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RegionKind {
    Title,
    Heading,
    Paragraph,
    List,
    Table,
    Formula,
    Figure,
    Caption,
    Header,
    Footer,
    Separator,
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RegionConfidence {
    pub detection: Option<f32>,
    pub layout: Option<f32>,
    pub recognition: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum RegionContent {
    Text(TextBlock),
    Table(Table),
    Formula(Formula),
    Figure(Figure),
    Separator,
    Unknown,
}

impl RegionContent {
    pub fn plain_text(&self, view: TextView) -> Option<String> {
        match self {
            Self::Text(block) => Some(block.plain_text(view)),
            Self::Table(table) => Some(table.plain_text(view)),
            Self::Formula(formula) => formula.latex.clone().or_else(|| formula.mathml.clone()),
            Self::Figure(figure) => figure
                .caption
                .as_ref()
                .map(|caption| caption.plain_text(view)),
            Self::Separator | Self::Unknown => None,
        }
    }
}

pub type Polygon = Vec<Point>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TextBlock {
    #[serde(default)]
    pub lines: Vec<TextLine>,
}

impl TextBlock {
    pub fn plain_text(&self, view: TextView) -> String {
        self.lines
            .iter()
            .map(|line| line.text.select(view))
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextLine {
    pub text: TextEvidence,
    pub polygon: Polygon,
    pub confidence: RecognitionConfidence,
    #[serde(default)]
    pub words: Vec<TextWord>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextWord {
    pub text: TextEvidence,
    pub polygon: Polygon,
    pub confidence: RecognitionConfidence,
    pub geometry_source: GeometrySource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextEvidence {
    pub raw: String,
    pub normalized: Option<String>,
    pub corrected: Option<String>,
    #[serde(default)]
    pub alternatives: Vec<TextAlternative>,
    #[serde(default)]
    pub corrections: Vec<Correction>,
}

impl TextEvidence {
    pub fn raw(text: impl Into<String>) -> Self {
        Self {
            raw: text.into(),
            normalized: None,
            corrected: None,
            alternatives: Vec::new(),
            corrections: Vec::new(),
        }
    }
    pub fn select(&self, view: TextView) -> String {
        match view {
            TextView::Raw => self.raw.clone(),
            TextView::Normalized => self.normalized.clone().unwrap_or_else(|| self.raw.clone()),
            TextView::Corrected => self
                .corrected
                .clone()
                .or_else(|| self.normalized.clone())
                .unwrap_or_else(|| self.raw.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextAlternative {
    pub text: String,
    pub score_micros: Option<u32>,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Correction {
    pub original: String,
    pub replacement: String,
    pub applied: bool,
    pub reason: String,
    pub score_margin_micros: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TextView {
    Raw,
    Normalized,
    Corrected,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RecognitionConfidence {
    pub detection: Option<f32>,
    pub mean_token: Option<f32>,
    pub minimum_token: Option<f32>,
    pub mean_margin: Option<f32>,
    pub blank_ratio: Option<f32>,
    pub abnormal_character_ratio: Option<f32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GeometrySource {
    NativePdf,
    Detector,
    OcrBackend,
    CtcEstimated,
    LayoutModel,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Provenance {
    pub provider: String,
    pub model: Option<String>,
    pub preprocessing: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Table {
    pub rows: u32,
    pub columns: u32,
    #[serde(default)]
    pub cells: Vec<TableCell>,
}

impl Table {
    pub fn plain_text(&self, view: TextView) -> String {
        let mut rows = vec![vec![String::new(); self.columns as usize]; self.rows as usize];
        for cell in &self.cells {
            if let Some(slot) = rows
                .get_mut(cell.row as usize)
                .and_then(|row| row.get_mut(cell.column as usize))
            {
                *slot = cell
                    .blocks
                    .iter()
                    .map(|block| block.plain_text(view))
                    .collect::<Vec<_>>()
                    .join(" ");
            }
        }
        rows.into_iter()
            .map(|row| row.join("\t"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableCell {
    pub row: u32,
    pub column: u32,
    pub row_span: u32,
    pub column_span: u32,
    pub polygon: Polygon,
    #[serde(default)]
    pub blocks: Vec<TextBlock>,
    pub is_header: bool,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Formula {
    pub latex: Option<String>,
    pub mathml: Option<String>,
    pub display: FormulaDisplay,
    pub source_crop: Option<PageImageRef>,
    pub confidence: Option<f32>,
    /// Raw OCR evidence retained when a specialist replaces a text region.
    #[serde(default)]
    pub raw_ocr: Option<TextBlock>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FormulaDisplay {
    Inline,
    Display,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Figure {
    pub source_crop: Option<PageImageRef>,
    pub caption: Option<TextBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutlineNode {
    pub title: String,
    pub page_index: u32,
    #[serde(default)]
    pub children: Vec<OutlineNode>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("unsupported schema {name} version {version}")]
    UnsupportedSchema { name: String, version: u32 },
    #[error("expected page {expected}, found page {actual}")]
    NonContiguousPage { expected: u32, actual: u32 },
    #[error("page {0} has empty source dimensions")]
    EmptyPage(u32),
}

pub fn rect_polygon(left: f32, top: f32, right: f32, bottom: f32) -> Polygon {
    vec![
        Point { x: left, y: top },
        Point { x: right, y: top },
        Point {
            x: right,
            y: bottom,
        },
        Point { x: left, y: bottom },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> Document {
        let mut document = Document::new(
            "doc",
            SourceIdentity {
                path: "book.pdf".into(),
                content_hash: "blake3:x".into(),
                byte_len: 4,
                mime_type: "application/pdf".into(),
            },
            ProcessingManifest {
                pipeline_version: "test".into(),
                profile: ProcessingProfile::Search,
                quality: QualityMode::Thorough,
                configuration_hash: "cfg".into(),
                models: Vec::new(),
                warnings: Vec::new(),
            },
        );
        document.pages.push(Page {
            index: 0,
            source_size: Size {
                width: 100,
                height: 200,
            },
            page_size_points: SizeF {
                width: 50.0,
                height: 100.0,
            },
            source_to_page: Transform::IDENTITY,
            source_kind: PageSourceKind::NativeText,
            image: None,
            regions: vec![Region {
                id: "p0-r0".into(),
                kind: RegionKind::Paragraph,
                polygon: rect_polygon(0.0, 0.0, 100.0, 200.0),
                confidence: RegionConfidence::default(),
                content: RegionContent::Text(TextBlock {
                    lines: vec![TextLine {
                        text: TextEvidence {
                            raw: "teh".into(),
                            normalized: Some("teh".into()),
                            corrected: Some("the".into()),
                            alternatives: Vec::new(),
                            corrections: Vec::new(),
                        },
                        polygon: Vec::new(),
                        confidence: RecognitionConfidence::default(),
                        words: Vec::new(),
                        provenance: Provenance {
                            provider: "native-pdf".into(),
                            model: None,
                            preprocessing: None,
                            language: Some("eng".into()),
                        },
                    }],
                }),
                provenance: Provenance {
                    provider: "native-pdf".into(),
                    model: None,
                    preprocessing: None,
                    language: Some("eng".into()),
                },
            }],
            reading_order: vec!["p0-r0".into()],
            warnings: Vec::new(),
        });
        document
    }

    #[test]
    fn text_views_preserve_raw_evidence() {
        let document = document();
        assert_eq!(document.full_text(TextView::Raw), "teh");
        assert_eq!(document.full_text(TextView::Corrected), "the");
    }

    #[test]
    fn json_round_trip_is_valid() {
        let document = document();
        let json = serde_json::to_string_pretty(&document).unwrap();
        let decoded: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, document);
        assert_eq!(decoded.validate(), Ok(()));
    }
}
