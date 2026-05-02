use crate::nms::{Detection as NmsDetection, DetectionContext, two_pass_nms};
#[allow(unused_imports)]
use crate::{debug_println, info_println};
use anyhow::{Result, anyhow};
use image::RgbImage;
use ndarray::Array;
use once_cell::sync::OnceCell;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::{RunOptions, Session, SessionInputValue};
use ort::value::{DynValue, TensorElementType, Value, ValueType};

#[path = "engine.rs"]
mod paddle_fallback;

pub use paddle_fallback::{Detection, PaddleXConfig, get_paddlex_class_name};

const YOLO_IMG_SIZE: u32 = 1024;

#[derive(Debug, Clone, Copy)]
struct LetterboxTransform {
    scale: f32,
    pad_x: f32,
    pad_y: f32,
}

#[derive(Debug, Clone)]
struct YoloPreprocessed {
    image_data: Vec<f32>,
    original_width: u32,
    original_height: u32,
    transform: LetterboxTransform,
}

#[derive(Debug, Clone, Copy)]
struct DetectionRow {
    class_id: i32,
    confidence: f32,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

struct YoloLinuxEngine {
    session: Session,
    config: PaddleXConfig,
    output_index: usize,
    provider_name: String,
}

enum EngineBackend {
    Yolo(YoloLinuxEngine),
    Paddle(paddle_fallback::PaddleXEngine),
}

pub struct PaddleXEngine {
    backend: EngineBackend,
}

unsafe impl Sync for PaddleXEngine {}

impl PaddleXEngine {
    pub fn new(model_path: &str, config: PaddleXConfig) -> Result<Self> {
        if is_yolo_model_path(model_path) {
            Ok(Self {
                backend: EngineBackend::Yolo(YoloLinuxEngine::new(model_path, config)?),
            })
        } else {
            Ok(Self {
                backend: EngineBackend::Paddle(paddle_fallback::PaddleXEngine::new(
                    model_path, config,
                )?),
            })
        }
    }

    #[deprecated(note = "This method is deprecated. Use detect_single_async instead.")]
    pub fn detect_single(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        match &mut self.backend {
            EngineBackend::Yolo(engine) => engine.detect_single_sync(image),
            EngineBackend::Paddle(engine) => engine.detect_single(image),
        }
    }

    pub async fn detect_single_async(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        match &mut self.backend {
            EngineBackend::Yolo(engine) => engine.detect_single_sync(image),
            EngineBackend::Paddle(engine) => engine.detect_single_async(image).await,
        }
    }

    pub async fn detect_batch_async(&mut self, images: &[RgbImage]) -> Result<Vec<Vec<Detection>>> {
        match &mut self.backend {
            EngineBackend::Yolo(engine) => engine.detect_batch_sync(images),
            EngineBackend::Paddle(engine) => engine.detect_batch_async(images).await,
        }
    }

    pub async fn detect_batch_with_indices_async(
        &mut self,
        images: &[RgbImage],
        page_indices: &[usize],
    ) -> Result<Vec<Vec<Detection>>> {
        let _ = page_indices;
        match &mut self.backend {
            EngineBackend::Yolo(engine) => engine.detect_batch_sync(images),
            EngineBackend::Paddle(engine) => {
                engine
                    .detect_batch_with_indices_async(images, page_indices)
                    .await
            }
        }
    }

    pub fn detect_single_blocking(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        match &mut self.backend {
            EngineBackend::Yolo(engine) => engine.detect_single_sync(image),
            EngineBackend::Paddle(engine) => {
                // Use tokio runtime to call async method from sync context
                let rt = tokio::runtime::Handle::try_current()
                    .or_else(|_| tokio::runtime::Runtime::new().map(|rt| rt.handle().clone()))?;
                rt.block_on(engine.detect_single_async(image))
            }
        }
    }

    pub fn detect_batch_with_indices_blocking(
        &mut self,
        images: &[RgbImage],
        page_indices: &[usize],
    ) -> Result<Vec<Vec<Detection>>> {
        match &mut self.backend {
            EngineBackend::Yolo(engine) => engine.detect_batch_sync(images),
            EngineBackend::Paddle(engine) => {
                // Use tokio runtime to call async method from sync context
                let rt = tokio::runtime::Handle::try_current()
                    .or_else(|_| tokio::runtime::Runtime::new().map(|rt| rt.handle().clone()))?;
                rt.block_on(engine.detect_batch_with_indices_async(images, page_indices))
            }
        }
    }

    pub fn provider_name(&self) -> &str {
        match &self.backend {
            EngineBackend::Yolo(engine) => &engine.provider_name,
            EngineBackend::Paddle(engine) => engine.provider_name(),
        }
    }
}

impl YoloLinuxEngine {
    fn new(model_path: &str, config: PaddleXConfig) -> Result<Self> {
        static ONNX_INIT: OnceCell<()> = OnceCell::new();
        ONNX_INIT.get_or_try_init(|| -> Result<()> {
            ort::init().with_name("lege").commit();
            Ok(())
        })?;

        let (session, provider_name) = Self::build_session(model_path)?;
        let output_index = session
            .outputs()
            .iter()
            .position(|o| {
                matches!(
                    o.dtype(),
                    ValueType::Tensor {
                        ty: TensorElementType::Float32,
                        ..
                    }
                )
            })
            .ok_or_else(|| anyhow!("Could not locate YOLO float output tensor"))?;

        Ok(Self {
            session,
            config,
            output_index,
            provider_name,
        })
    }

    fn build_session(model_path: &str) -> Result<(Session, String)> {
        use ort::execution_providers::CPUExecutionProvider;

        let new_builder = || -> anyhow::Result<ort::session::builder::SessionBuilder> {
            Ok(Session::builder()
                .map_err(|e| anyhow!("failed to create ORT session builder: {e}"))?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(|e| anyhow!("failed to set ORT optimization level: {e}"))?
                .with_memory_pattern(true)
                .map_err(|e| anyhow!("failed to enable ORT memory pattern: {e}"))?)
        };

        let gpu_disabled = std::env::var("LEGE_DISABLE_GPU").ok().as_deref() == Some("1");
        let mut attempts = Vec::new();
        if !gpu_disabled {
            if let Some(webgpu) = crate::gpu::webgpu_execution_provider_dispatch() {
                attempts.push((vec![webgpu], "WebGPU"));
            }
        }
        attempts.push((vec![CPUExecutionProvider::default().build()], "CPU"));

        for (providers, name) in attempts {
            match new_builder() {
                Ok(mut builder) => match builder.with_execution_providers(providers) {
                    Ok(mut builder_with_providers) => {
                        match builder_with_providers.commit_from_file(model_path) {
                            Ok(session) => {
                                if name == "CPU" {
                                    info_println!("Using CPU execution for ONNX Runtime");
                                } else {
                                    info_println!("Using {} execution for ONNX Runtime", name);
                                }
                                return Ok((session, name.to_string()));
                            }
                            Err(e) => {
                                debug_println!("Failed to commit session with {}: {:?}", name, e);
                            }
                        }
                    }
                    Err(e) => {
                        debug_println!("Failed to add {} providers: {:?}", name, e);
                    }
                },
                Err(e) => {
                    debug_println!("Failed to create builder: {:?}", e);
                }
            }
        }

        Err(anyhow!("Failed to create YOLO ONNX session"))
    }

    fn detect_single_sync(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        let pre = preprocess_yolo(image)?;
        let output = self.run_single(&pre)?;
        self.postprocess(&output, &pre)
    }

    fn detect_batch_sync(&mut self, images: &[RgbImage]) -> Result<Vec<Vec<Detection>>> {
        images
            .iter()
            .map(|img| self.detect_single_sync(img))
            .collect()
    }

    fn run_single(&mut self, inputs: &YoloPreprocessed) -> Result<Vec<DetectionRow>> {
        let image_data = Array::from_shape_vec(
            (1, 3, YOLO_IMG_SIZE as usize, YOLO_IMG_SIZE as usize),
            inputs.image_data.clone(),
        )?;
        let input_values = [SessionInputValue::from(
            Value::from_array(image_data)
                .map_err(|e| anyhow!("failed to create ORT input tensor: {e}"))?,
        )];
        let run_options = RunOptions::new()?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let outputs = rt.block_on(self.session.run_async(input_values, &run_options)?)?;
        parse_yolo_rows(&outputs[self.output_index])
    }

    fn postprocess(
        &self,
        rows: &[DetectionRow],
        inputs: &YoloPreprocessed,
    ) -> Result<Vec<Detection>> {
        let inv_scale = if inputs.transform.scale > 0.0 {
            1.0 / inputs.transform.scale
        } else {
            1.0
        };

        let mut detections = Vec::new();
        for row in rows {
            if row.confidence < self.config.confidence_threshold {
                continue;
            }

            let category = yolo_class_to_category(row.class_id);
            let x1 = ((row.x1 - inputs.transform.pad_x) * inv_scale)
                .clamp(0.0, inputs.original_width as f32);
            let y1 = ((row.y1 - inputs.transform.pad_y) * inv_scale)
                .clamp(0.0, inputs.original_height as f32);
            let x2 = ((row.x2 - inputs.transform.pad_x) * inv_scale)
                .clamp(0.0, inputs.original_width as f32);
            let y2 = ((row.y2 - inputs.transform.pad_y) * inv_scale)
                .clamp(0.0, inputs.original_height as f32);

            if (x2 - x1) < 1.0 || (y2 - y1) < 1.0 {
                continue;
            }

            detections.push(Detection {
                class_id: row.class_id,
                class_name: Some(yolo_class_name(row.class_id).to_string()),
                category,
                confidence: row.confidence,
                bbox: [x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)],
                context: Some(DetectionContext {
                    original_width: inputs.original_width as f32,
                    original_height: inputs.original_height as f32,
                    scale_x: inv_scale,
                    scale_y: inv_scale,
                }),
            });
        }

        let nms_detections: Vec<NmsDetection> = detections
            .iter()
            .map(|d| NmsDetection {
                bbox: d.bbox,
                confidence: d.confidence,
                class_id: d.class_id,
                class_name: d.class_name.clone(),
                context: d.context.clone(),
            })
            .collect();
        let kept = two_pass_nms(
            nms_detections,
            self.config.nms_threshold,
            self.config.iou_threshold,
        );
        Ok(kept
            .into_iter()
            .map(|d| Detection {
                class_id: d.class_id,
                class_name: d.class_name,
                category: yolo_class_to_category(d.class_id),
                confidence: d.confidence,
                bbox: d.bbox,
                context: d.context,
            })
            .collect())
    }
}

fn preprocess_yolo(img: &RgbImage) -> Result<YoloPreprocessed> {
    use crate::resize::{ResizeMethod, ResizeParams};

    let (orig_width, orig_height) = img.dimensions();
    let transform = compute_letterbox_transform(orig_width, orig_height);
    let resize_params = ResizeParams {
        target_width: YOLO_IMG_SIZE,
        target_height: YOLO_IMG_SIZE,
        method: ResizeMethod::Bilinear,
        letterbox: true,
        border_value: 114.0,
        swap_rb: false,
    };
    let resized = if orig_width == YOLO_IMG_SIZE && orig_height == YOLO_IMG_SIZE {
        img.clone()
    } else {
        let bytes =
            crate::resize::resize_bytes(img.as_raw(), orig_width, orig_height, &resize_params, 3)
                .map_err(|e| anyhow!("YOLO resize failed: {e}"))?;
        RgbImage::from_raw(YOLO_IMG_SIZE, YOLO_IMG_SIZE, bytes)
            .ok_or_else(|| anyhow!("Failed to construct YOLO inference image"))?
    };

    let plane = (YOLO_IMG_SIZE * YOLO_IMG_SIZE) as usize;
    let mut image_data = vec![0.0f32; plane * 3];
    for y in 0..YOLO_IMG_SIZE {
        for x in 0..YOLO_IMG_SIZE {
            let pixel = resized.get_pixel(x, y);
            let idx = (y * YOLO_IMG_SIZE + x) as usize;
            image_data[idx] = pixel[0] as f32 / 255.0;
            image_data[idx + plane] = pixel[1] as f32 / 255.0;
            image_data[idx + 2 * plane] = pixel[2] as f32 / 255.0;
        }
    }

    Ok(YoloPreprocessed {
        image_data,
        original_width: orig_width,
        original_height: orig_height,
        transform,
    })
}

fn compute_letterbox_transform(page_w: u32, page_h: u32) -> LetterboxTransform {
    if page_w == 0 || page_h == 0 {
        return LetterboxTransform {
            scale: 1.0,
            pad_x: 0.0,
            pad_y: 0.0,
        };
    }
    let inf = YOLO_IMG_SIZE as f32;
    let scale = (inf / page_w as f32).min(inf / page_h as f32);
    let resized_w = page_w as f32 * scale;
    let resized_h = page_h as f32 * scale;
    LetterboxTransform {
        scale,
        pad_x: (inf - resized_w) * 0.5,
        pad_y: (inf - resized_h) * 0.5,
    }
}

fn parse_yolo_rows(output_value: &DynValue) -> Result<Vec<DetectionRow>> {
    let tensor = output_value
        .try_extract_tensor::<f32>()
        .map_err(|_| anyhow!("Unsupported YOLO output tensor type"))?;
    let (shape, data) = tensor;
    let shape: &[i64] = &shape[..];

    if shape.len() == 2 && shape[1] == 6 {
        let rows = shape[0] as usize;
        return Ok((0..rows)
            .map(|i| DetectionRow {
                class_id: data[i * 6] as i32,
                confidence: data[i * 6 + 1],
                x1: data[i * 6 + 2],
                y1: data[i * 6 + 3],
                x2: data[i * 6 + 4],
                y2: data[i * 6 + 5],
            })
            .collect());
    }

    if shape.len() == 3 && shape[0] == 1 && shape[2] == 6 {
        let rows = shape[1] as usize;
        return Ok((0..rows)
            .map(|i| DetectionRow {
                class_id: data[i * 6 + 5] as i32,
                confidence: data[i * 6 + 4],
                x1: data[i * 6],
                y1: data[i * 6 + 1],
                x2: data[i * 6 + 2],
                y2: data[i * 6 + 3],
            })
            .collect());
    }

    if shape.len() == 3 && shape[0] == 1 && shape[1] >= 5 && shape[2] > 0 {
        let channels = shape[1] as usize;
        let boxes = shape[2] as usize;
        let mut rows = Vec::with_capacity(boxes);
        for i in 0..boxes {
            let cx = data[i];
            let cy = data[boxes + i];
            let w = data[2 * boxes + i];
            let h = data[3 * boxes + i];

            let mut best_class = -1;
            let mut best_score = 0.0f32;
            for c in 4..channels {
                let score = data[c * boxes + i];
                if score > best_score {
                    best_score = score;
                    best_class = (c - 4) as i32;
                }
            }
            if best_class < 0 {
                continue;
            }

            rows.push(DetectionRow {
                class_id: best_class,
                confidence: best_score,
                x1: cx - w * 0.5,
                y1: cy - h * 0.5,
                x2: cx + w * 0.5,
                y2: cy + h * 0.5,
            });
        }
        return Ok(rows);
    }

    Err(anyhow!("Unsupported YOLO output shape: {:?}", shape))
}

/// Map YOLO DocLayout class IDs to PaddleX legacy class IDs used by LabelInfo.
///
/// YOLO: {0:title, 1:plain_text, 2:abandon, 3:figure, 4:figure_caption,
///        5:table, 6:table_caption, 7:table_footnote, 8:isolate_formula, 9:formula_caption}
///
/// Legacy: {0:paragraph_title, 1:image, 2:text, 6:figure_title, 7:formula,
///          8:table, 9:table_title, 12:footnote, 19:formula_number}
/// Map YOLO DocLayout class IDs directly to broad content categories.
fn yolo_class_to_category(class_id: i32) -> crate::types::ContentCategory {
    use crate::types::ContentCategory;
    match class_id {
        0 => ContentCategory::Text,    // title
        1 => ContentCategory::Text,    // plain_text
        2 => ContentCategory::Abandon, // abandon
        3 => ContentCategory::Image,   // figure
        4 => ContentCategory::Text,    // figure_caption
        5 => ContentCategory::Table,   // table
        6 => ContentCategory::Text,    // table_caption
        7 => ContentCategory::Text,    // table_footnote
        8 => ContentCategory::Text,    // isolate_formula
        9 => ContentCategory::Text,    // formula_caption
        _ => ContentCategory::Text,    // unknown
    }
}

fn yolo_class_name(class_id: i32) -> &'static str {
    match class_id {
        0 => "title",
        1 => "plain_text",
        2 => "abandon",
        3 => "figure",
        4 => "figure_caption",
        5 => "table",
        6 => "table_caption",
        7 => "table_footnote",
        8 => "isolate_formula",
        9 => "formula_caption",
        _ => "unknown",
    }
}

fn is_yolo_model_path(model_path: &str) -> bool {
    model_path.ends_with("yolo-layout.onnx") || model_path.contains("/yolo-layout.onnx")
}

pub fn detect_layout_paddlex_batch(
    image_paths: &[String],
    model_path: &str,
) -> Result<Vec<Vec<Detection>>> {
    paddle_fallback::detect_layout_paddlex_batch(image_paths, model_path)
}
