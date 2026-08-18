use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ImagesPageData {
    pub unit: &'static str,
    pub mode: String,
    pub images: Vec<ImageDrawView>,
    pub draw_count: usize,
    pub unique_object_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageDrawView {
    pub draw_index: usize,
    pub image_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<ObjectRefView>,
    pub width: u32,
    pub height: u32,
    pub bits_per_component: u8,
    pub is_stencil: bool,
    pub color_space: String,
    pub codec: Option<String>,
    pub filters: Vec<String>,
    pub transform: [f64; 6],
    pub painted_bounds: [f64; 4],
    pub has_soft_mask: bool,
    pub has_color_key_mask: bool,
    pub has_stencil_mask: bool,
    pub paint_origin: String,
    pub lowering_degraded: bool,
    pub reuse_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extract_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObjectRefView {
    pub number: u32,
    pub generation: u16,
}
