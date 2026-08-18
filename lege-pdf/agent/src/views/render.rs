use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct RenderPageData {
    pub unit: &'static str,
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub dpi: f64,
    pub scale: f64,
    pub format: String,
    pub degraded_draws: u32,
    pub recovery_notes: Vec<String>,
}
