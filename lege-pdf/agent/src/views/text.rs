use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct TextProvenance {
    /// `native` or `ocr`.
    pub source: &'static str,
    /// `native-text`, `scanned-image`, `hybrid`, or `render-required`.
    pub page_content_kind: &'static str,
    pub native_text_trustworthy: bool,
    pub native_char_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr_engine: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr_language: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextPageData {
    pub unit: &'static str,
    pub layout: String,
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub words: Option<Vec<WordView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chars: Option<Vec<CharView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Vec<BlockView>>,
    pub char_count: usize,
    pub provenance: TextProvenance,
}

#[derive(Debug, Clone, Serialize)]
pub struct WordView {
    pub text: String,
    pub bbox: [f64; 4],
    pub first_char: usize,
    pub char_count: usize,
    pub continued: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CharView {
    pub unicode: u32,
    pub text: String,
    pub char_code: u32,
    pub cid: u32,
    pub glyph_id: u32,
    pub text_object: u32,
    pub tight_box: [f64; 4],
    pub loose_box: [f64; 4],
    pub char_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockView {
    pub text: String,
    pub bbox: [f64; 4],
    pub lines: Vec<LineView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LineView {
    pub text: String,
    pub bbox: [f64; 4],
    pub words: Vec<WordView>,
}
