use crate::text_loader::CLI_TEXT;
#[allow(unused_imports)] // Protect debug macros from removal - used conditionally
use crate::{debug_println, error_println, info_println};
use crate::nms::{Detection as NmsDetection, DetectionContext, two_pass_nms};
use anyhow::{Result, anyhow};
use image::RgbImage;
use log::info;
use memmap2::Mmap;
use ndarray::{Array, s};
use once_cell::sync::OnceCell;
use ort::session::RunOptions;
use ort::session::Session;
use ort::session::SessionInputValue;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Value;
use std::fs::File;
use std::cell::RefCell;

use bytemuck;

const PADDLE_IMG_SIZE: u32 = 640;

// Thread-local buffers to reduce allocations in preprocessing
thread_local! {
    static PREPROCESS_U8_BUFFER: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(640 * 640 * 3));
    static PREPROCESS_F32_BUFFER: RefCell<Vec<f32>> = RefCell::new(Vec::with_capacity(640 * 640 * 3));
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub class_id: i32,
    pub class_name: Option<String>,
    pub confidence: f32,
    pub bbox: [f32; 4], // [x1, y1, x2, y2]
    pub context: Option<DetectionContext>,
}


#[derive(Debug, Clone)]
/// Configuration for PaddleX engine
pub struct PaddleXConfig {
    pub confidence_threshold: f32,
    pub nms_threshold: f32,
    pub iou_threshold: f32,
    pub batch_size: usize,
}
impl PaddleXConfig {
    pub fn new(
        confidence_threshold: f32,
        nms_threshold: f32,
        iou_threshold: f32,
        batch_size: usize,
    ) -> Self {
        Self {
            confidence_threshold,
            nms_threshold,
            iou_threshold,
            batch_size,
        }
    }
}
impl Default for PaddleXConfig {
    fn default() -> Self {
        Self {
            confidence_threshold: 0.2,
            nms_threshold: 0.5,
            iou_threshold: 0.5,
            batch_size: 1,
        }
    }
}

#[derive(Debug, Clone)]
struct PaddleXPreprocessed {
    image_data: Vec<f32>,
    im_shape: [f32; 2],
    scale_factor: [f32; 2],
}

#[derive(Debug, Clone)]
struct PaddleXPreprocessedMetadata {
    im_shape: [f32; 2],
    scale_factor: [f32; 2],
}

pub struct PaddleXEngine {
    session: Session,
    config: PaddleXConfig,
    output_index: usize,      // The index for the Bounding Boxes (float32)
    count_index: Option<usize>, // The index for the Box Counts (int32) - NEW
    provider_name: String,
}
// SAFETY: PaddleXEngine is designed to be used in a static context where access
// is controlled through proper synchronization mechanisms.
unsafe impl Sync for PaddleXEngine {}

impl PaddleXEngine {
    pub fn new(model_path: &str, config: PaddleXConfig) -> Result<Self> {
        // Ensure ONNX Runtime is initialized (this should be the FIRST time it happens)
        static ONNX_INIT: OnceCell<()> = OnceCell::new();
        ONNX_INIT.get_or_try_init(|| -> Result<()> {
            ort::init()
                .with_name("lege")
                .commit();
            Ok(())
        })?;
        let (session, provider_name) = Self::build_session(model_path)?;
        
        // 1. Find the main detection output (usually float32)
        let output_index = session
            .outputs
            .iter()
            .position(|o| o.name == "reshape2_95.tmp_0" || o.output_type == ort::value::ValueType::Tensor { ty: ort::tensor::TensorElementType::Float32, shape: ort::tensor::Shape::new([-1, -1, -1, -1]), dimension_symbols: ort::tensor::SymbolicDimensions::new([String::default(), String::default(), String::default(), String::default()]) })
            .ok_or_else(|| anyhow!("Missing box output 'reshape2_95.tmp_0'"))?;

        // 2. Find the count output (usually int32)
        // This is the magic key that lets us batch correctly.
        // Common names in Paddle exports: "bbox_num", "multiclass_nms3_0.tmp_2"
        let count_index = session
            .outputs
            .iter()
            .position(|o| 
                o.name == "tile_3.tmp_0" || // Specific name from PaddlePaddle Graph 0
                o.name.contains("bbox_num") || 
                o.name.contains("multiclass_nms") ||
                o.output_type == ort::value::ValueType::Tensor { ty: ort::tensor::TensorElementType::Int32, shape: ort::tensor::Shape::new([-1]), dimension_symbols: ort::tensor::SymbolicDimensions::new([String::default()]) }
            );

        if count_index.is_none() {
            info_println!("Warning: Model lacks a 'bbox_num' (int32) output. Batching will be simulated sequentially.");
        }

        Ok(Self {
            session,
            config,
            output_index,
            count_index, // Store the index
            provider_name,
        })
    }

    fn build_session(model_path: &str) -> Result<(Session, String)> {
        info!("Building session for model: {}", model_path);
        // Helper to create a new builder
        let new_builder = || -> anyhow::Result<ort::session::builder::SessionBuilder> {
            Ok(Session::builder()?
                .with_optimization_level(GraphOptimizationLevel::Level3)?
                // Memory optimization: Enable memory pattern
                .with_memory_pattern(true)?)
        };

        // Platform-specific GPU provider attempts
        #[cfg(target_os = "linux")]
        fn get_provider_attempts() -> Vec<(
            Vec<ort::execution_providers::ExecutionProviderDispatch>,
            Vec<&'static str>,
        )> {
            use ort::execution_providers::CPUExecutionProvider;
            use crate::gpu::webgpu_execution_provider_dispatch;

            let gpu_disabled = std::env::var("LEGE_DISABLE_GPU")
                .ok()
                .as_deref() == Some("1");

            if gpu_disabled {
                return vec![(vec![CPUExecutionProvider::default().build()], vec!["CPU"])];
            }

            let mut attempts = Vec::with_capacity(2);
            if let Some(webgpu_provider) = webgpu_execution_provider_dispatch() {
                // Register WebGPU first and CPU second so ORT can place unsupported ops on CPU.
                attempts.push((
                    vec![webgpu_provider, CPUExecutionProvider::default().build()],
                    vec!["WebGPU", "CPU"],
                ));
            }
            attempts.push((vec![CPUExecutionProvider::default().build()], vec!["CPU"]));
            attempts
        }

        #[cfg(target_os = "windows")]
        fn get_provider_attempts() -> Vec<(
            Vec<ort::execution_providers::ExecutionProviderDispatch>,
            Vec<&'static str>,
        )> {
            use ort::execution_providers::CPUExecutionProvider;
            use crate::gpu::directml_execution_provider_dispatch;

            let gpu_disabled = std::env::var("LEGE_DISABLE_GPU")
                .ok()
                .as_deref() == Some("1");

            if gpu_disabled {
                vec![(vec![CPUExecutionProvider::default().build()], vec!["CPU"])]
            } else {
                vec![
                    (vec![directml_execution_provider_dispatch()], vec!["DirectML"]),
                    (vec![CPUExecutionProvider::default().build()], vec!["CPU"]),
                ]
            }
        }

        #[cfg(target_os = "macos")]
        fn get_provider_attempts() -> Vec<(
            Vec<ort::execution_providers::ExecutionProviderDispatch>,
            Vec<&'static str>,
        )> {
            use ort::execution_providers::{CPUExecutionProvider, CoreMLExecutionProvider};

            let gpu_disabled = std::env::var("LEGE_DISABLE_GPU")
                .ok()
                .as_deref() == Some("1");

            if gpu_disabled {
                vec![(vec![CPUExecutionProvider::default().build()], vec!["CPU"])]
            } else {
                vec![
                    (vec![CoreMLExecutionProvider::default().build()], vec!["CoreML"]),
                    (vec![CPUExecutionProvider::default().build()], vec!["CPU"]),
                ]
            }
        }

        // Memory-map the model once to avoid extra file I/O and copies
        let file = File::open(model_path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        let provider_msg = CLI_TEXT.get_provider_messages();

        // Try each provider in order until one succeeds
        let provider_attempts = get_provider_attempts();
        for (providers, names) in provider_attempts {
            let primary = names.first().copied().unwrap_or("CPU");

            match new_builder()?.with_execution_providers(providers)?.commit_from_memory(&mmap) {
                Ok(session) => {
                    if primary == "CPU" {
                        info_println!("{}", provider_msg.using_cpu());
                    } else {
                        info_println!("Using {} execution for ONNX Runtime", primary);
                    }
                    return Ok((session, primary.to_string()));
                }
                Err(e) => {
                    if primary != "CPU" {
                        info_println!("{} failed: {}, falling back...", primary, e);
                    }
                    continue;
                }
            }
        }

        anyhow::bail!("Failed to create ONNX session with any provider")
    }

    /// Preprocesses a single image for the PaddleX model.
    fn preprocess(&self, img: &RgbImage) -> Result<PaddleXPreprocessed> {
        use crate::resize::{ResizeMethod, ResizeParams};
        let (orig_width, orig_height) = img.dimensions();
        let target_size = PADDLE_IMG_SIZE;
        // Direct stretch/squash to 640x640 without letterboxing
        let resize_params = ResizeParams {
            target_width: target_size,
            target_height: target_size,
            method: ResizeMethod::Bilinear,
            letterbox: false,
            border_value: 114.0, // ignored when letterbox=false
            swap_rb: false,
        };
        // fast_image_resize: convert to Image and resize via our CPU helper
        // Reuse thread-local u8 buffer for image data
    let resized = PREPROCESS_U8_BUFFER.with(|buf_cell| -> Result<RgbImage> {
            let mut buf = buf_cell.borrow_mut();
            buf.clear();
            buf.extend_from_slice(img.as_raw());
            
            let src_image = fast_image_resize::images::Image::from_slice_u8(
                img.width(),
                img.height(),
                &mut buf,
                fast_image_resize::PixelType::U8x3,
            )
            .map_err(|e| anyhow!("Failed to create source image: {:?}", e))?;
            
            crate::resize::cpu::resize_single(src_image, &resize_params, target_size, target_size)
                .map_err(|e| anyhow!("Resize failed: {:?}", e))
        })?;
        
        // Simple scale factors - no padding to account for
        let scale_x = orig_width as f32 / target_size as f32;
        let scale_y = orig_height as f32 / target_size as f32;
        
        // Convert to NCHW and normalize, channel order B, G, R (as in last working impl)
        // Reuse thread-local f32 buffer for preprocessed data
        let input_data = PREPROCESS_F32_BUFFER.with(|buf_cell| {
            let mut buf = buf_cell.borrow_mut();
            buf.clear();
            buf.resize((3 * target_size * target_size) as usize, 0.0);
            
            let channel_size = (target_size * target_size) as usize;
            for y in 0..target_size {
                for x in 0..target_size {
                    let p = resized.get_pixel(x, y);
                    let base = (y * target_size + x) as usize;
                    buf[base] = (p[2] as f32) / 255.0; // B
                    buf[base + channel_size] = (p[1] as f32) / 255.0; // G
                    buf[base + 2 * channel_size] = (p[0] as f32) / 255.0; // R
                }
            }
            
            buf.clone() // Clone is needed since we need to return owned data
        });
        
        Ok(PaddleXPreprocessed {
            image_data: input_data,
            im_shape: [orig_height as f32, orig_width as f32],
            scale_factor: [scale_y, scale_x],
        })
    }

    /// Zero-copy style preprocess into a caller-provided buffer slice (len must be 3*H*W)
    /// Uses parallel processing for optimal performance on multi-core CPUs.
    fn preprocess_into(
        &self,
        img: &RgbImage,
        output_buffer: &mut [f32],
    ) -> Result<PaddleXPreprocessedMetadata> {
        use crate::resize::{ResizeMethod, ResizeParams};
        let (orig_width, orig_height) = img.dimensions();
        let target_size = PADDLE_IMG_SIZE;
        let resize_params = ResizeParams {
            target_width: target_size,
            target_height: target_size,
            method: ResizeMethod::Bilinear,
            letterbox: false,
            border_value: 114.0,
            swap_rb: false,
        };
        // Reuse thread-local u8 buffer for image data
    let resized = PREPROCESS_U8_BUFFER.with(|buf_cell| -> Result<RgbImage> {
            let mut buf = buf_cell.borrow_mut();
            buf.clear();
            buf.extend_from_slice(img.as_raw());
            
            let src_image = fast_image_resize::images::Image::from_slice_u8(
                img.width(),
                img.height(),
                &mut buf,
                fast_image_resize::PixelType::U8x3,
            )
            .map_err(|e| anyhow!("Failed to create source image: {:?}", e))?;
            
            crate::resize::cpu::resize_single(src_image, &resize_params, target_size, target_size)
                .map_err(|e| anyhow!("Resize failed: {:?}", e))
        })?;
        
        let scale_x = orig_width as f32 / target_size as f32;
        let scale_y = orig_height as f32 / target_size as f32;
        let channel_size = (target_size * target_size) as usize;
        anyhow::ensure!(
            output_buffer.len() == 3 * channel_size,
            "Output buffer has incorrect size"
        );
        // SERIAL PROCESSING - Actually faster for this data size
        for y in 0..target_size {
            for x in 0..target_size {
                let p = resized.get_pixel(x, y);
                let base = (y * target_size + x) as usize;
                output_buffer[base] = (p[2] as f32) / 255.0; // B
                output_buffer[base + channel_size] = (p[1] as f32) / 255.0; // G
                output_buffer[base + 2 * channel_size] = (p[0] as f32) / 255.0; // R
            }
        }
        Ok(PaddleXPreprocessedMetadata {
            im_shape: [orig_height as f32, orig_width as f32],
            scale_factor: [scale_y, scale_x],
        })
    }

    // --- New Async-First Inference Methods ---

    /// DEPRECATED: As of ONNX Runtime threading limitations, synchronously detecting objects in a single image is not supported.
    /// Use detect_single_async instead.
    #[deprecated(note = "This method is deprecated. Use detect_single_async instead.")]
    pub fn detect_single(&mut self, _image: &RgbImage) -> Result<Vec<Detection>> {
        anyhow::bail!("Synchronous inference is deprecated and no longer supported. Use detect_single_async instead.")
    }

    /// Asynchronously detects objects in a single image.
    /// This is the primary method for single-image inference.
    pub async fn detect_single_async(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        let mut inputs = self.preprocess(image)?;
        let outputs = self.run_inference_single_async(&mut inputs).await?;
        let detections = self.postprocess_detections(&outputs, &inputs)?;
        Ok(detections)
    }
    /// Asynchronously detects objects in a batch of images with page tracking.
    /// This is the primary method for batch inference.
    pub async fn detect_batch_async(&mut self, images: &[RgbImage]) -> Result<Vec<Vec<Detection>>> {
        let page_indices: Vec<usize> = (0..images.len()).collect();
        self.detect_batch_with_indices_async(images, &page_indices)
            .await
    }
    /// Asynchronously detects objects in a batch of images with explicit page indices.
    /// This method ensures proper page-to-detection mapping.
    pub async fn detect_batch_with_indices_async(
        &mut self,
        images: &[RgbImage],
        page_indices: &[usize],
    ) -> Result<Vec<Vec<Detection>>> {
        let batch_size = images.len();
        if batch_size == 0 {
            return Ok(Vec::new());
        }
        let item_elems = 3 * PADDLE_IMG_SIZE as usize * PADDLE_IMG_SIZE as usize;
        let total_elems = batch_size * item_elems;
        let mut batch_image_data = vec![0.0f32; total_elems];
        let mut batch_metadata = Vec::with_capacity(batch_size);
        for (i, image) in images.iter().enumerate() {
            let start = i * item_elems;
            let end = start + item_elems;
            let meta = self.preprocess_into(image, &mut batch_image_data[start..end])?;
            batch_metadata.push(meta);
        }
        let batch_outputs = self.run_inference_batch_with_indices_from_flat_async(
            &batch_image_data,
            &batch_metadata,
            page_indices,
        )
        .await?;
        let mut results = Vec::with_capacity(batch_outputs.len());
        for (i, output) in batch_outputs.iter().enumerate() {
            let pseudo_inputs = PaddleXPreprocessed {
                image_data: Vec::new(),
                im_shape: batch_metadata[i].im_shape,
                scale_factor: batch_metadata[i].scale_factor,
            };
            let detections = self.postprocess_detections(output, &pseudo_inputs)?;
            results.push(detections);
        }
        Ok(results)
    }
    /// Core async inference logic for a single preprocessed image.
    async fn run_inference_single_async(
        &mut self,
        inputs: &mut PaddleXPreprocessed,
    ) -> Result<Array<f32, ndarray::Ix2>> {
        // Consume the large image buffer to avoid an extra CPU copy before upload
        let image_data_vec = std::mem::take(&mut inputs.image_data);
        let image_data = Array::from_shape_vec(
            (1, 3, PADDLE_IMG_SIZE as usize, PADDLE_IMG_SIZE as usize),
            image_data_vec,
        )?;
        let im_shape = Array::from_shape_vec((1, 2), inputs.im_shape.to_vec())?;
        let scale_factor = Array::from_shape_vec((1, 2), inputs.scale_factor.to_vec())?;
        let input_values = [
            SessionInputValue::from(Value::from_array(im_shape)?),
            SessionInputValue::from(Value::from_array(image_data)?),
            SessionInputValue::from(Value::from_array(scale_factor)?),
        ];
        let run_options = RunOptions::new()?;
        let outputs = self.session.run_async(input_values, &run_options)?.await?;
        // Handle potentially different output tensor types (for int8 vs fp32 models)
        let output_value = &outputs[self.output_index];

        // Determine the tensor type by attempting to extract different types
        let out = if let Ok(tensor) = output_value.try_extract_tensor::<f32>() {
            let (shape, data) = tensor;
            // Handle different output shapes: [B, N, 6], [N, 6], or flattened [Total, 6]
            // Match batch inference logic for consistency
            match &shape[..] {
                // [B, N, 6] where B=1 for single inference
                [b, n, k] => {
                    anyhow::ensure!(*b == 1 && *k == 6, "Unexpected output shape: {:?}", shape);
                    let tmp =
                        Array::from_shape_vec((*b as usize, *n as usize, *k as usize), data.to_vec())?;
                    tmp.slice(s![0, .., ..]).to_owned()
                }
                // [N, 6] - single batch case
                [n, k] => {
                    anyhow::ensure!(*k == 6, "Unexpected K ({}), expected 6", k);
                    Array::from_shape_vec((*n as usize, *k as usize), data.to_vec())?
                }
                _ => {
                    // Conservative fallback: try to interpret as Nx6
                    anyhow::ensure!(
                        data.len() % 6 == 0,
                        "Output size {} not divisible by 6",
                        data.len()
                    );
                    Array::from_shape_vec((data.len() / 6, 6), data.to_vec())?
                }
            }
        } else if let Ok(tensor) = output_value.try_extract_tensor::<i8>() {
            // Handle int8 output by converting to f32
            let (shape, data) = tensor;
            // Convert i8 data to f32 data
            let f32_data: Vec<f32> = data.iter().map(|&x| x as f32).collect();
            // Handle different output shapes: [B, N, 6], [N, 6], or flattened [Total, 6]
            match &shape[..] {
                // [B, N, 6] where B=1 for single inference
                [b, n, k] => {
                    anyhow::ensure!(*b == 1 && *k == 6, "Unexpected output shape: {:?}", shape);
                    let tmp =
                        Array::from_shape_vec((*b as usize, *n as usize, *k as usize), f32_data)?;
                    tmp.slice(s![0, .., ..]).to_owned()
                }
                // [N, 6] - single batch case
                [n, k] => {
                    anyhow::ensure!(*k == 6, "Unexpected K ({}), expected 6", k);
                    Array::from_shape_vec((*n as usize, *k as usize), f32_data)?
                }
                _ => {
                    // Conservative fallback: try to interpret as Nx6
                    anyhow::ensure!(
                        f32_data.len() % 6 == 0,
                        "Output size {} not divisible by 6",
                        f32_data.len()
                    );
                    Array::from_shape_vec((f32_data.len() / 6, 6), f32_data)?
                }
            }
        } else if let Ok(tensor) = output_value.try_extract_tensor::<u8>() {
            // Handle uint8 output by converting to f32
            let (shape, data) = tensor;
            // Convert u8 data to f32 data
            let f32_data: Vec<f32> = data.iter().map(|&x| x as f32).collect();
            // Handle different output shapes: [B, N, 6], [N, 6], or flattened [Total, 6]
            match &shape[..] {
                // [B, N, 6] where B=1 for single inference
                [b, n, k] => {
                    anyhow::ensure!(*b == 1 && *k == 6, "Unexpected output shape: {:?}", shape);
                    let tmp =
                        Array::from_shape_vec((*b as usize, *n as usize, *k as usize), f32_data)?;
                    tmp.slice(s![0, .., ..]).to_owned()
                }
                // [N, 6] - single batch case
                [n, k] => {
                    anyhow::ensure!(*k == 6, "Unexpected K ({}), expected 6", k);
                    Array::from_shape_vec((*n as usize, *k as usize), f32_data)?
                }
                _ => {
                    // Conservative fallback: try to interpret as Nx6
                    anyhow::ensure!(
                        f32_data.len() % 6 == 0,
                        "Output size {} not divisible by 6",
                        f32_data.len()
                    );
                    Array::from_shape_vec((f32_data.len() / 6, 6), f32_data)?
                }
            }
        } else {
            // If none of the above work, return error
            return Err(anyhow::anyhow!("Unsupported tensor element type in output"));
        };
        Ok(out)
    }


    /// Core async inference logic for a batch of preprocessed images.
    #[allow(dead_code)]
    async fn run_inference_batch_async(
        &mut self,
        inputs: &[PaddleXPreprocessed],
    ) -> Result<Vec<Array<f32, ndarray::Ix2>>> {
        let page_indices: Vec<usize> = (0..inputs.len()).collect();
        self.run_inference_batch_with_indices_async(inputs, &page_indices)
            .await
    }
    /// Core async inference logic for a batch of preprocessed images with page index tracking.
    #[allow(dead_code)]
    async fn run_inference_batch_with_indices_async(
        &mut self,
        inputs: &[PaddleXPreprocessed],
        _page_indices: &[usize],
    ) -> Result<Vec<Array<f32, ndarray::Ix2>>> {
        let batch_size = inputs.len();
        let capacity = batch_size * (3 * (PADDLE_IMG_SIZE as usize) * (PADDLE_IMG_SIZE as usize));
        let mut image_data = Vec::with_capacity(capacity);
        // Pack im_shape and scale_factor efficiently using bytemuck
        let mut im_shapes_pairs: Vec<[f32; 2]> = Vec::with_capacity(batch_size);
        let mut scale_pairs: Vec<[f32; 2]> = Vec::with_capacity(batch_size);
        for input in inputs {
            image_data.extend_from_slice(&input.image_data);
            im_shapes_pairs.push(input.im_shape);
            scale_pairs.push(input.scale_factor);
        }
        let im_shapes: Vec<f32> = bytemuck::cast_vec(im_shapes_pairs);
        let scale_factors: Vec<f32> = bytemuck::cast_vec(scale_pairs);
        let image_data = Array::from_shape_vec(
            (
                batch_size,
                3,
                PADDLE_IMG_SIZE as usize,
                PADDLE_IMG_SIZE as usize,
            ),
            image_data,
        )?;
        let im_shape = Array::from_shape_vec((batch_size, 2), im_shapes)?;
        let scale_factor = Array::from_shape_vec((batch_size, 2), scale_factors)?;
        let input_values = [
            SessionInputValue::from(Value::from_array(im_shape)?),
            SessionInputValue::from(Value::from_array(image_data)?),
            SessionInputValue::from(Value::from_array(scale_factor)?),
        ];
        let run_options = RunOptions::new()?;
        let outputs = self.session.run_async(input_values, &run_options)?.await?;
        // Handle potentially different output tensor types (for int8 vs fp32 models)
        let output_value = &outputs[self.output_index];
        let mut results = Vec::with_capacity(batch_size);

        // Determine the tensor type by attempting to extract different types
        if let Ok(tensor) = output_value.try_extract_tensor::<f32>() {
            let (shape, data) = tensor;
            // Handle different output shapes: [B, N, 6], [N, 6], or flattened [Total, 6]
            match &shape[..] {
                [bb, nn, kk] => {
                    anyhow::ensure!(*kk == 6, "Unexpected K ({}), expected 6", kk);
                    let view = ndarray::ArrayView::from_shape(
                        (*bb as usize, *nn as usize, *kk as usize),
                        data,
                    )?;
                    let owned = view.to_owned();
                    let take = batch_size.min(*bb as usize);
                    for i in 0..take {
                        results.push(owned.slice(s![i, .., ..]).to_owned());
                    }
                    // Ensure we have exactly batch_size results
                    while results.len() < batch_size {
                        let empty = Array::from_shape_vec((0, 6), vec![])?;
                        results.push(empty);
                    }
                }
                [nn, kk] => {
                    anyhow::ensure!(*kk == 6, "Unexpected K ({}), expected 6", kk);
                    // This happens when the model concatenates all batch outputs into a single [N, 6] tensor
                    let arr = Array::from_shape_vec((*nn as usize, *kk as usize), data.to_vec())?;
                    if batch_size == 1 {
                        // Single image case
                        results.push(arr);
                    } else {
                        // Multiple images: decode page indices from concatenated output
                        debug_println!(
                            "Batch processing: got [N={}, K={}] for batch_size={}",
                            nn,
                            kk,
                            batch_size
                        );
                        let total_detections = *nn as usize;
                        let arr = Array::from_shape_vec((*nn as usize, *kk as usize), data.to_vec())?;
                        // Create per-page detection collections
                        let mut detections_per_page: Vec<Vec<[f32; 6]>> = vec![Vec::new(); batch_size];
                        // Process each detection and assign to correct page based on encoded information
                        for i in 0..total_detections {
                            let raw_class_id = arr[[i, 0]];
                            let confidence = arr[[i, 1]];
                            let x1 = arr[[i, 2]];
                            let y1 = arr[[i, 3]];
                            let x2 = arr[[i, 4]];
                            let y2 = arr[[i, 5]];
                            // The PP-Doclayout model concatenates outputs from all batch items
                            // We need to determine which batch item this detection belongs to
                            // Since we can't reliably encode page info in class_id without changing the model,
                            // we'll use a more robust approach: spatial clustering and confidence analysis
                            // For now, use a heuristic based on detection order and confidence patterns
                            let detection = [raw_class_id, confidence, x1, y1, x2, y2];
                            // Assign based on position in the detection array
                            // This assumes model outputs detections in batch order
                            let estimated_page_idx = (i * batch_size) / total_detections.max(1);
                            let page_idx = estimated_page_idx.min(batch_size - 1);
                            detections_per_page[page_idx].push(detection);
                        }
                        debug_println!(
                            "Heuristic assignment: {} detections distributed as {:?}",
                            total_detections,
                            detections_per_page
                                .iter()
                                .map(|v| v.len())
                                .collect::<Vec<_>>()
                        );
                        // Convert to ndarray format
                        for page_detections in detections_per_page {
                            if page_detections.is_empty() {
                                let empty = Array::from_shape_vec((0, 6), vec![])?;
                                results.push(empty);
                            } else {
                                let mut flat_data = Vec::new();
                                for detection in page_detections {
                                    flat_data.extend_from_slice(&detection);
                                }
                                let item_array =
                                    Array::from_shape_vec((flat_data.len() / 6, 6), flat_data)?;
                                results.push(item_array);
                            }
                        }
                    }
                }
                _ => {
                    anyhow::bail!("Unsupported output shape: {:?}", shape);
                }
            }
        } else if let Ok(tensor) = output_value.try_extract_tensor::<i8>() {
            let (shape, data) = tensor;
            // Convert i8 data to f32 data
            let f32_data: Vec<f32> = data.iter().map(|&x| x as f32).collect();
            // Handle different output shapes: [B, N, 6], [N, 6], or flattened [Total, 6]
            match &shape[..] {
                [bb, nn, kk] => {
                    anyhow::ensure!(*kk == 6, "Unexpected K ({}), expected 6", kk);
                    let view = ndarray::ArrayView::from_shape(
                        (*bb as usize, *nn as usize, *kk as usize),
                        &f32_data,
                    )?;
                    let owned = view.to_owned();
                    let take = batch_size.min(*bb as usize);
                    for i in 0..take {
                        results.push(owned.slice(s![i, .., ..]).to_owned());
                    }
                    // Ensure we have exactly batch_size results
                    while results.len() < batch_size {
                        let empty = Array::from_shape_vec((0, 6), vec![])?;
                        results.push(empty);
                    }
                }
                [nn, kk] => {
                    anyhow::ensure!(*kk == 6, "Unexpected K ({}), expected 6", kk);
                    // This happens when the model concatenates all batch outputs into a single [N, 6] tensor
                    let arr = Array::from_shape_vec((*nn as usize, *kk as usize), f32_data.to_vec())?;
                    if batch_size == 1 {
                        // Single image case
                        results.push(arr);
                    } else {
                        // Multiple images: decode page indices from concatenated output
                        debug_println!(
                            "Batch processing: got [N={}, K={}] for batch_size={}",
                            nn,
                            kk,
                            batch_size
                        );
                        let total_detections = *nn as usize;
                        let arr = Array::from_shape_vec((*nn as usize, *kk as usize), f32_data.to_vec())?;
                        // Create per-page detection collections
                        let mut detections_per_page: Vec<Vec<[f32; 6]>> = vec![Vec::new(); batch_size];
                        // Process each detection and assign to correct page based on encoded information
                        for i in 0..total_detections {
                            let raw_class_id = arr[[i, 0]];
                            let confidence = arr[[i, 1]];
                            let x1 = arr[[i, 2]];
                            let y1 = arr[[i, 3]];
                            let x2 = arr[[i, 4]];
                            let y2 = arr[[i, 5]];
                            // The PP-Doclayout model concatenates outputs from all batch items
                            // We need to determine which batch item this detection belongs to
                            // Since we can't reliably encode page info in class_id without changing the model,
                            // we'll use a more robust approach: spatial clustering and confidence analysis
                            // For now, use a heuristic based on detection order and confidence patterns
                            let detection = [raw_class_id, confidence, x1, y1, x2, y2];
                            // Assign based on position in the detection array
                            // This assumes model outputs detections in batch order
                            let estimated_page_idx = (i * batch_size) / total_detections.max(1);
                            let page_idx = estimated_page_idx.min(batch_size - 1);
                            detections_per_page[page_idx].push(detection);
                        }
                        debug_println!(
                            "Heuristic assignment: {} detections distributed as {:?}",
                            total_detections,
                            detections_per_page
                                .iter()
                                .map(|v| v.len())
                                .collect::<Vec<_>>()
                        );
                        // Convert to ndarray format
                        for page_detections in detections_per_page {
                            if page_detections.is_empty() {
                                let empty = Array::from_shape_vec((0, 6), vec![])?;
                                results.push(empty);
                            } else {
                                let mut flat_data = Vec::new();
                                for detection in page_detections {
                                    flat_data.extend_from_slice(&detection);
                                }
                                let item_array =
                                    Array::from_shape_vec((flat_data.len() / 6, 6), flat_data)?;
                                results.push(item_array);
                            }
                        }
                    }
                }
                _ => {
                    anyhow::bail!("Unsupported output shape: {:?}", shape);
                }
            }
        } else if let Ok(tensor) = output_value.try_extract_tensor::<u8>() {
            let (shape, data) = tensor;
            // Convert u8 data to f32 data
            let f32_data: Vec<f32> = data.iter().map(|&x| x as f32).collect();
            // Handle different output shapes: [B, N, 6], [N, 6], or flattened [Total, 6]
            match &shape[..] {
                [bb, nn, kk] => {
                    anyhow::ensure!(*kk == 6, "Unexpected K ({}), expected 6", kk);
                    let view = ndarray::ArrayView::from_shape(
                        (*bb as usize, *nn as usize, *kk as usize),
                        &f32_data,
                    )?;
                    let owned = view.to_owned();
                    let take = batch_size.min(*bb as usize);
                    for i in 0..take {
                        results.push(owned.slice(s![i, .., ..]).to_owned());
                    }
                    // Ensure we have exactly batch_size results
                    while results.len() < batch_size {
                        let empty = Array::from_shape_vec((0, 6), vec![])?;
                        results.push(empty);
                    }
                }
                [nn, kk] => {
                    anyhow::ensure!(*kk == 6, "Unexpected K ({}), expected 6", kk);
                    // This happens when the model concatenates all batch outputs into a single [N, 6] tensor
                    let arr = Array::from_shape_vec((*nn as usize, *kk as usize), f32_data.to_vec())?;
                    if batch_size == 1 {
                        // Single image case
                        results.push(arr);
                    } else {
                        // Multiple images: decode page indices from concatenated output
                        debug_println!(
                            "Batch processing: got [N={}, K={}] for batch_size={}",
                            nn,
                            kk,
                            batch_size
                        );
                        let total_detections = *nn as usize;
                        let arr = Array::from_shape_vec((*nn as usize, *kk as usize), f32_data.to_vec())?;
                        // Create per-page detection collections
                        let mut detections_per_page: Vec<Vec<[f32; 6]>> = vec![Vec::new(); batch_size];
                        // Process each detection and assign to correct page based on encoded information
                        for i in 0..total_detections {
                            let raw_class_id = arr[[i, 0]];
                            let confidence = arr[[i, 1]];
                            let x1 = arr[[i, 2]];
                            let y1 = arr[[i, 3]];
                            let x2 = arr[[i, 4]];
                            let y2 = arr[[i, 5]];
                            // The PP-Doclayout model concatenates outputs from all batch items
                            // We need to determine which batch item this detection belongs to
                            // Since we can't reliably encode page info in class_id without changing the model,
                            // we'll use a more robust approach: spatial clustering and confidence analysis
                            // For now, use a heuristic based on detection order and confidence patterns
                            let detection = [raw_class_id, confidence, x1, y1, x2, y2];
                            // Assign based on position in the detection array
                            // This assumes model outputs detections in batch order
                            let estimated_page_idx = (i * batch_size) / total_detections.max(1);
                            let page_idx = estimated_page_idx.min(batch_size - 1);
                            detections_per_page[page_idx].push(detection);
                        }
                        debug_println!(
                            "Heuristic assignment: {} detections distributed as {:?}",
                            total_detections,
                            detections_per_page
                                .iter()
                                .map(|v| v.len())
                                .collect::<Vec<_>>()
                        );
                        // Convert to ndarray format
                        for page_detections in detections_per_page {
                            if page_detections.is_empty() {
                                let empty = Array::from_shape_vec((0, 6), vec![])?;
                                results.push(empty);
                            } else {
                                let mut flat_data = Vec::new();
                                for detection in page_detections {
                                    flat_data.extend_from_slice(&detection);
                                }
                                let item_array =
                                    Array::from_shape_vec((flat_data.len() / 6, 6), flat_data)?;
                                results.push(item_array);
                            }
                        }
                    }
                }
                _ => {
                    anyhow::bail!("Unsupported output shape: {:?}", shape);
                }
            }
        } else {
            return Err(anyhow::anyhow!("Unsupported tensor element type in output"));
        }
        Ok(results)
    }
    async fn run_inference_batch_with_indices_from_flat_async(
        &mut self,
        batch_image_data: &[f32],
        batch_metadata: &[PaddleXPreprocessedMetadata],
        _page_indices: &[usize],
    ) -> Result<Vec<Array<f32, ndarray::Ix2>>> {
        let batch_size = batch_metadata.len();
        
        // SAFETY: If we didn't find the count_index during init, we MUST run sequentially
        // or we risk assigning detections to the wrong page.
        if self.count_index.is_none() {
            return self.run_inference_sequential_fallback(batch_image_data, batch_metadata).await;
        }
        let count_idx = self.count_index.unwrap();

        // 1. Prepare Batch Tensors
        let im_shapes_pairs: Vec<[f32; 2]> = batch_metadata.iter().map(|m| m.im_shape).collect();
        let scale_pairs: Vec<[f32; 2]> = batch_metadata.iter().map(|m| m.scale_factor).collect();
        
        let image_data = Array::from_shape_vec(
            (batch_size, 3, PADDLE_IMG_SIZE as usize, PADDLE_IMG_SIZE as usize),
            batch_image_data.to_vec(),
        )?;
        let im_shape = Array::from_shape_vec((batch_size, 2), bytemuck::cast_vec::<[f32; 2], f32>(im_shapes_pairs))?;
        let scale_factor = Array::from_shape_vec((batch_size, 2), bytemuck::cast_vec::<[f32; 2], f32>(scale_pairs))?;

        let input_values = [
            SessionInputValue::from(Value::from_array(im_shape)?),
            SessionInputValue::from(Value::from_array(image_data)?),
            SessionInputValue::from(Value::from_array(scale_factor)?),
        ];

        // 2. Run Inference (One big call to GPU)
        let run_options = RunOptions::new()?;
        let outputs = self.session.run_async(input_values, &run_options)?.await?;

        // 3. Extract The "Big List" of Boxes
        // Handle potentially different output tensor types (for int8 vs fp32 models)
        let box_value = &outputs[self.output_index];
        let all_boxes = if let Ok(tensor) = box_value.try_extract_tensor::<f32>() {
            let (_box_shape, box_data) = tensor;
            Array::from_shape_vec((box_data.len() / 6, 6), box_data.to_vec())?
        } else if let Ok(tensor) = box_value.try_extract_tensor::<i8>() {
            let (_box_shape, box_data) = tensor;
            let f32_data: Vec<f32> = box_data.iter().map(|&x| x as f32).collect();
            Array::from_shape_vec((f32_data.len() / 6, 6), f32_data)?
        } else if let Ok(tensor) = box_value.try_extract_tensor::<u8>() {
            let (_box_shape, box_data) = tensor;
            let f32_data: Vec<f32> = box_data.iter().map(|&x| x as f32).collect();
            Array::from_shape_vec((f32_data.len() / 6, 6), f32_data)?
        } else {
            return Err(anyhow::anyhow!("Unsupported tensor element type for box output"));
        };

        // 4. Extract The "Map" (bbox_num) - This should remain as i32 since it's counting boxes
        // Note: bbox_num typically remains i32 regardless of quantization of main output
        let count_tensor = outputs[count_idx].try_extract_tensor::<i32>()?;
        let (_count_shape, count_data) = count_tensor;

        // 5. Re-identify / Unzip
        let mut results = Vec::with_capacity(batch_size);
        let mut current_offset = 0;

        for (i, &count) in count_data.iter().enumerate() {
            let count = count as usize;

            if count > 0 {
                // Slice the exact rows belonging to this page
                // Corresponds to batch_metadata[i]
                let page_boxes = all_boxes.slice(s![current_offset..current_offset + count, ..]).to_owned();
                results.push(page_boxes);
                current_offset += count;
            } else {
                // Handle blank pages correctly (empty array)
                let empty = Array::from_shape_vec((0, 6), vec![])?;
                results.push(empty);
            }

            // Safety check
            if i >= batch_size { break; }
        }

        Ok(results)
    }

    // Helper fallback if model version is old and lacks bbox_num output
    async fn run_inference_sequential_fallback(
        &mut self,
        batch_image_data: &[f32],
        batch_metadata: &[PaddleXPreprocessedMetadata],
    ) -> Result<Vec<Array<f32, ndarray::Ix2>>> {
        let batch_size = batch_metadata.len();
        let item_len = 3 * (PADDLE_IMG_SIZE as usize) * (PADDLE_IMG_SIZE as usize);
        let mut results = Vec::with_capacity(batch_size); // Initialize results here
        let run_options = RunOptions::new()?; // Bind RunOptions to a variable

        for i in 0..batch_size {
            let start = i * item_len;
            let slice = &batch_image_data[start..start + item_len];
            
            // Construct single-item inputs
            let im_shape = Array::from_shape_vec((1, 2), batch_metadata[i].im_shape.to_vec())?;
            let scale_factor = Array::from_shape_vec((1, 2), batch_metadata[i].scale_factor.to_vec())?;
            let img = Array::from_shape_vec((1, 3, PADDLE_IMG_SIZE as usize, PADDLE_IMG_SIZE as usize), slice.to_vec())?;

            let inputs = [
                SessionInputValue::from(Value::from_array(im_shape)?),
                SessionInputValue::from(Value::from_array(img)?),
                SessionInputValue::from(Value::from_array(scale_factor)?),
            ];
            
            let out = self.session.run_async(inputs, &run_options)?.await?; // Use the bound variable
            // Handle potentially different output tensor types (for int8 vs fp32 models)
            let output_value = &out[self.output_index];
            let res = if let Ok(tensor) = output_value.try_extract_tensor::<f32>() {
                let (dims, data) = tensor;

                // Handle [1, N, 6] or [N, 6]
                if dims.len() == 3 {
                    Array::from_shape_vec((dims[1] as usize, 6), data.to_vec())?
                } else {
                    Array::from_shape_vec((dims[0] as usize, 6), data.to_vec())?
                }
            } else if let Ok(tensor) = output_value.try_extract_tensor::<i8>() {
                let (dims, data) = tensor;
                let f32_data: Vec<f32> = data.iter().map(|&x| x as f32).collect();

                // Handle [1, N, 6] or [N, 6]
                if dims.len() == 3 {
                    Array::from_shape_vec((dims[1] as usize, 6), f32_data)?
                } else {
                    Array::from_shape_vec((dims[0] as usize, 6), f32_data)?
                }
            } else if let Ok(tensor) = output_value.try_extract_tensor::<u8>() {
                let (dims, data) = tensor;
                let f32_data: Vec<f32> = data.iter().map(|&x| x as f32).collect();

                // Handle [1, N, 6] or [N, 6]
                if dims.len() == 3 {
                    Array::from_shape_vec((dims[1] as usize, 6), f32_data)?
                } else {
                    Array::from_shape_vec((dims[0] as usize, 6), f32_data)?
                }
            } else {
                return Err(anyhow::anyhow!("Unsupported tensor element type in sequential fallback"));
            };
            results.push(res);
        }
        Ok(results)
    }
    /// Postprocesses raw model output into a list of final detections.
    fn postprocess_detections(
        &self,
        output: &Array<f32, ndarray::Ix2>,
        inputs: &PaddleXPreprocessed,
    ) -> Result<Vec<Detection>> {
        let mut detections = Vec::new();
        // No padding for PaddleX direct resize
        let scale_x = inputs.scale_factor[1];
        let scale_y = inputs.scale_factor[0];
        for row in output.outer_iter() {
            if row.len() < 6 {
                continue;
            }
            let class_id = row[0] as i32;
            let confidence = row[1];
            if confidence >= self.config.confidence_threshold {
                let raw_x1 = row[2].max(0.0);
                let raw_y1 = row[3].max(0.0);
                let raw_x2 = row[4].max(0.0);
                let raw_y2 = row[5].max(0.0);
                // Map back to original coordinates (multiply by scale factors)
                let x1 = raw_x1 * scale_x;
                let y1 = raw_y1 * scale_y;
                let x2 = raw_x2 * scale_x;
                let y2 = raw_y2 * scale_y;
                if (x2 - x1) < 1.0 || (y2 - y1) < 1.0 {
                    continue;
                }
                detections.push(Detection {
                    class_id,
                    class_name: Some(get_paddlex_class_name(class_id)),
                    confidence,
                    bbox: [x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)],
                    context: Some(DetectionContext {
                        original_width: inputs.im_shape[1],
                        original_height: inputs.im_shape[0],
                        scale_x,
                        scale_y,
                    }),
                });
            }
        }
        // Apply NMS
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
        let kept_detections = two_pass_nms(
            nms_detections,
            self.config.nms_threshold,
            self.config.iou_threshold,
        );
        let final_detections: Vec<Detection> = kept_detections
            .into_iter()
            .map(|d| Detection {
                class_id: d.class_id,
                class_name: d.class_name,
                confidence: d.confidence,
                bbox: d.bbox,
                context: d.context,
            })
            .collect();
        Ok(final_detections)
    }

    /// Returns the name of the execution provider being used
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }
}

pub fn detect_layout_paddlex_batch(
    _image_paths: &[String],
    _model_path: &str,
) -> Result<Vec<Vec<Detection>>> {
    unimplemented!("Use detect_batch_async instead")
}

use crate::types::LabelInfo;
use once_cell::sync::Lazy;

// Global instance for efficient label lookups
static LABEL_INFO: Lazy<LabelInfo> = Lazy::new(|| LabelInfo::new());

pub fn get_paddlex_class_name(class_id: i32) -> String {
    LABEL_INFO.get_class_name(class_id)
}
