use serde::Serialize;

use super::text::TextProvenance;

#[derive(Debug, Clone, Serialize)]
pub struct SearchPageData {
    pub unit: &'static str,
    pub query: String,
    pub match_count: usize,
    pub matches: Vec<MatchView>,
    pub provenance: TextProvenance,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatchView {
    pub text: String,
    pub context: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<[f64; 4]>,
    pub first_char: usize,
    pub char_count: usize,
}
