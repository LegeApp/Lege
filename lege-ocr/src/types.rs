use serde::{Deserialize, Serialize};

/// Raw OCR output from a single engine call — hOCR markup and plain text.
/// Returned by `OcrEngine::run_image`.
#[derive(Debug, Clone)]
pub struct OcrResult {
    pub hocr: String,
    pub plain_text: String,
}

/// A text region identified by layout detection.
#[derive(Debug, Clone)]
pub struct TextRegion {
    pub page_index: usize,
    pub region_id: usize,
    /// Canonical DocLayout-YOLO class name, e.g. "plain_text" or "table_footnote".
    pub class_name: Option<String>,
    /// [x1, y1, x2, y2] in high-res page pixel space
    pub bbox_highres: [u32; 4],
    pub confidence: f32,
}

/// A single line crop extracted from a text region.
#[derive(Debug, Clone)]
pub struct TextLineCrop {
    pub region_id: usize,
    pub line_index: usize,
    /// [x1, y1, x2, y2] in page-global high-res pixel space
    pub bbox_highres: [u32; 4],
    pub image_gray: image::GrayImage,
    /// Estimated baseline row within the crop (crop-local y)
    pub baseline_y: Option<u32>,
}

/// A single recognized word with crop-local bounding box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrWord {
    pub text: String,
    /// [x1, y1, x2, y2] within the line crop image
    pub bbox_crop_local: [u32; 4],
    pub confidence: Option<f32>,
}

/// OCR result for a single line crop. The bbox is in page-global high-res space.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrLineResult {
    pub text: String,
    pub confidence: Option<f32>,
    pub words: Vec<OcrWord>,
    /// [x1, y1, x2, y2] in page-global high-res pixel space
    pub bbox_highres: [u32; 4],
}

/// Complete slow-OCR output for one page.
#[derive(Debug, Clone)]
pub struct SlowOcrPage {
    pub page_index: usize,
    pub hocr: String,
    pub regions: Vec<TextRegion>,
    pub lines: Vec<OcrLineResult>,
    /// Per-region text blocks in reading order, for structured output (EPUB).
    /// Each detected text region becomes one block (≈ paragraph).
    pub blocks: Vec<crate::document::TextBlock>,
}

/// Confidence level of line segmentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentationConfidence {
    High,
    Medium,
    Low,
}

impl SegmentationConfidence {
    pub fn meets_minimum(&self, minimum: &Self) -> bool {
        self.rank() >= minimum.rank()
    }

    fn rank(&self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
        }
    }
}

/// Output of the line segmentation step.
#[derive(Debug, Clone)]
pub struct SegmentationResult {
    /// Line bounding boxes [x1, y1, x2, y2] in page-global pixel space
    pub line_bboxes: Vec<[u32; 4]>,
    pub confidence: SegmentationConfidence,
}

#[cfg(test)]
mod tests {
    use super::SegmentationConfidence;

    #[test]
    fn segmentation_confidence_respects_requested_minimum() {
        assert!(SegmentationConfidence::High.meets_minimum(&SegmentationConfidence::High));
        assert!(!SegmentationConfidence::Medium.meets_minimum(&SegmentationConfidence::High));
        assert!(SegmentationConfidence::Medium.meets_minimum(&SegmentationConfidence::Low));
        assert!(!SegmentationConfidence::Low.meets_minimum(&SegmentationConfidence::Medium));
    }
}
