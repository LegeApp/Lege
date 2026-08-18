use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct InspectData {
    pub open: OpenView,
    pub page_count: u32,
    pub version: Option<String>,
    pub encryption: Option<EncryptionView>,
    pub xref: XrefView,
    pub features: FeaturesView,
    pub annotations: AnnotationView,
    pub outline_item_count: usize,
    pub metadata: MetadataView,
    pub pages: Vec<PageView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OpenView {
    Ok,
    Recovered { repairs: Vec<String> },
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct EncryptionView {
    pub version: i64,
    pub revision: i64,
    pub method: String,
    pub user_password_empty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct XrefView {
    pub recovery_used: bool,
    pub rebuilt: bool,
    pub revision_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeaturesView {
    pub has_acroform: bool,
    pub has_xfa: bool,
    pub has_javascript: bool,
    pub has_outlines: bool,
    pub has_optional_content: bool,
    pub uses_object_streams: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnnotationView {
    pub total: u32,
    pub by_subtype: Vec<(String, u32)>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MetadataView {
    pub title: Option<String>,
    pub author: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageView {
    pub page: u32,
    pub page_index: u32,
    pub media_box: [f64; 4],
    pub crop_box: [f64; 4],
    pub rotate: u16,
    pub media_box_present: bool,
    pub compile: CompileView,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CompileView {
    Ok { op_count: usize },
    Degraded { op_count: usize, detail: String },
    Failed { error: String },
    Unknown,
}
