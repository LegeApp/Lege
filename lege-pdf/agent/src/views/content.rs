use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct ContentData {
    pub unit: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dump: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ops: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objects: Option<Vec<Value>>,
    pub op_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourcesView {
    pub fonts: Vec<FontResView>,
    pub images: Vec<ImageResView>,
    pub shading_count: usize,
    pub path_count: usize,
    pub text_run_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FontResView {
    pub index: usize,
    pub resource_name: String,
    pub subtype: String,
    pub base_font: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<super::image::ObjectRefView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageResView {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<super::image::ObjectRefView>,
}
