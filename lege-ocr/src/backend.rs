//! Backend-neutral batch contracts. Serialization belongs to document exporters.

use anyhow::Result;
use image::GrayImage;

use crate::engine::OcrEngine;
use crate::types::OcrLineResult;

/// Bounded recognition microbatch policy. The scheduler groups line crops from
/// concurrent documents without allowing an unbounded image queue.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct OcrSchedulerConfig {
    pub max_batch_lines: usize,
    pub max_batch_pixels: u64,
    pub max_wait_ms: u64,
    pub queue_capacity: usize,
}

impl Default for OcrSchedulerConfig {
    fn default() -> Self {
        Self {
            max_batch_lines: 64,
            max_batch_pixels: 12_000_000,
            max_wait_ms: 3,
            queue_capacity: 32,
        }
    }
}

impl OcrSchedulerConfig {
    pub fn normalized(self) -> Self {
        Self {
            max_batch_lines: self.max_batch_lines.clamp(1, 1024),
            max_batch_pixels: self.max_batch_pixels.clamp(1, 1_000_000_000),
            max_wait_ms: self.max_wait_ms.min(100),
            queue_capacity: self.queue_capacity.clamp(1, 4096),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceleratorKind {
    Cpu,
    Gpu,
    Npu,
    OperatingSystem,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub page_ocr: bool,
    pub line_batching: bool,
    pub word_geometry: bool,
    pub recognition_confidence: bool,
    pub layout_analysis: bool,
    pub table_recognition: bool,
    pub formula_recognition: bool,
    pub accelerator: AcceleratorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCellStructure {
    pub row: u32,
    pub column: u32,
    pub row_span: u32,
    pub column_span: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpecialistContent {
    Table {
        rows: u32,
        columns: u32,
        cells: Vec<TableCellStructure>,
    },
    Formula {
        latex: String,
        display: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpecialistRegion {
    pub bbox: [u32; 4],
    pub detection_confidence: Option<f32>,
    pub recognition_confidence: Option<f32>,
    pub provider: String,
    pub model: Option<String>,
    pub content: SpecialistContent,
}

pub struct PageBatch<'a> {
    pub pages: &'a [GrayImage],
    pub language: &'a str,
}

pub struct RecognitionBatch<'a> {
    pub lines: &'a [GrayImage],
    pub language: &'a str,
}

pub trait PageOcrBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> BackendCapabilities;
    fn recognize_pages(&self, batch: PageBatch<'_>) -> Result<Vec<Vec<OcrLineResult>>>;
    fn recognize_lines(&self, batch: RecognitionBatch<'_>) -> Result<Vec<OcrLineResult>>;
    fn specialize_page(&self, _page: &GrayImage, _language: &str) -> Result<Vec<SpecialistRegion>> {
        Ok(Vec::new())
    }
}

pub trait LineRecognizerBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> BackendCapabilities;
    fn recognize_batch(&self, batch: RecognitionBatch<'_>) -> Result<Vec<OcrLineResult>>;
}

/// Compatibility adapter while callers migrate from the serialization-oriented
/// `OcrEngine` trait. It deliberately reports no batching even if a concrete
/// engine happens to batch internally.
pub struct LegacyPageAdapter {
    engine: Box<dyn OcrEngine>,
}

impl LegacyPageAdapter {
    pub fn new(engine: Box<dyn OcrEngine>) -> Self {
        Self { engine }
    }
}

impl std::fmt::Debug for LegacyPageAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LegacyPageAdapter")
            .field("engine", &self.engine.name())
            .finish()
    }
}

impl PageOcrBackend for LegacyPageAdapter {
    fn name(&self) -> &'static str {
        self.engine.name()
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            page_ocr: true,
            line_batching: false,
            word_geometry: true,
            recognition_confidence: false,
            layout_analysis: false,
            table_recognition: false,
            formula_recognition: false,
            accelerator: AcceleratorKind::Unknown,
        }
    }

    fn recognize_pages(&self, batch: PageBatch<'_>) -> Result<Vec<Vec<OcrLineResult>>> {
        batch
            .pages
            .iter()
            .map(|page| self.engine.ocr_page(page, batch.language))
            .collect()
    }

    fn recognize_lines(&self, batch: RecognitionBatch<'_>) -> Result<Vec<OcrLineResult>> {
        batch
            .lines
            .iter()
            .map(|line| self.engine.ocr_line(line, batch.language))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_limits_are_bounded_and_non_zero() {
        let normalized = OcrSchedulerConfig {
            max_batch_lines: 0,
            max_batch_pixels: u64::MAX,
            max_wait_ms: u64::MAX,
            queue_capacity: 0,
        }
        .normalized();
        assert_eq!(normalized.max_batch_lines, 1);
        assert_eq!(normalized.max_batch_pixels, 1_000_000_000);
        assert_eq!(normalized.max_wait_ms, 100);
        assert_eq!(normalized.queue_capacity, 1);
    }
}
