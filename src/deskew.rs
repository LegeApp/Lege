// deskew.rs
// Document deskewing and rotation correction module
// Integrates rotation classification and document unwarping models

#[allow(unused_imports)]
use crate::{debug_println, error_println, info_println};
#[cfg(target_os = "linux")]
use crate::gpu::webgpu_execution_provider_dispatch;
#[cfg(windows)]
use crate::gpu::directml_execution_provider_dispatch;
use anyhow::{Context, Result, anyhow};
use image::{ImageBuffer, Rgb, RgbImage};
use log::info;
use memmap2::Mmap;
use ndarray::Array;
use once_cell::sync::OnceCell;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::tensor::Shape;
use ort::value::Value;
use std::fs::File;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

// Model input sizes
const ROTATION_CLASSIFIER_SIZE: u32 = 224; // PP-LCNet rotation classifier expects 224x224
// UVDoc unwarp model accepts dynamic sizes

#[cfg(target_os = "linux")]
fn gpu_execution_disabled() -> bool {
    std::env::var("LEGE_DISABLE_WEBGPU").ok().as_deref() == Some("1")
}

/// Rotation angle classification results
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationAngle {
    /// No rotation needed (0°)
    NoRotation = 0,
    /// 90° clockwise rotation
    Rotate90 = 1,
    /// 180° rotation
    Rotate180 = 2,
    /// 270° clockwise (90° counter-clockwise) rotation
    Rotate270 = 3,
}

impl RotationAngle {
    /// Convert from classification index
    fn from_index(idx: usize) -> Result<Self> {
        match idx {
            0 => Ok(RotationAngle::NoRotation),
            1 => Ok(RotationAngle::Rotate90),
            2 => Ok(RotationAngle::Rotate180),
            3 => Ok(RotationAngle::Rotate270),
            _ => Err(anyhow!("Invalid rotation index: {}", idx)),
        }
    }

    /// Get the rotation angle in degrees
    pub fn degrees(&self) -> f32 {
        match self {
            RotationAngle::NoRotation => 0.0,
            RotationAngle::Rotate90 => 90.0,
            RotationAngle::Rotate180 => 180.0,
            RotationAngle::Rotate270 => 270.0,
        }
    }

    /// Check if rotation is needed
    pub fn needs_rotation(&self) -> bool {
        !matches!(self, RotationAngle::NoRotation)
    }
}

/// Configuration for deskewing operations
#[derive(Debug, Clone)]
pub struct DeskewConfig {
    /// Confidence threshold for rotation classification (0.0-1.0)
    pub rotation_confidence_threshold: f32,
    /// Whether to apply rotation correction
    pub enable_rotation_correction: bool,
    /// Whether to apply document unwarping
    pub enable_unwarping: bool,
}

impl Default for DeskewConfig {
    fn default() -> Self {
        Self {
            rotation_confidence_threshold: 0.5,
            enable_rotation_correction: true,
            enable_unwarping: true,
        }
    }
}

/// Rotation classifier engine using PP-LCNet model
pub struct RotationClassifier {
    session: Mutex<Session>,
    #[allow(dead_code)]
    provider_name: String,
}

impl RotationClassifier {
    /// Load the rotation classification model
    pub fn new(model_path: &str) -> Result<Self> {
        // Ensure ONNX Runtime is initialized (should already be done by main engine)
        static ONNX_INIT: OnceCell<()> = OnceCell::new();
        ONNX_INIT.get_or_try_init(|| -> Result<()> {
            ort::init()
                .with_name("lege")
                .commit();
            Ok(())
        })?;

        let (session, provider_name) = Self::build_session(model_path)?;

        debug_println!(
            "Rotation classifier loaded with provider: {}",
            provider_name
        );

        Ok(Self {
            session: Mutex::new(session),
            provider_name,
        })
    }

    fn build_session(model_path: &str) -> Result<(Session, String)> {
        info!("Building rotation classifier session: {}", model_path);

        let mmap = unsafe {
            let file = File::open(model_path)
                .with_context(|| format!("Failed to open model file: {}", model_path))?;
            Mmap::map(&file)
                .with_context(|| format!("Failed to memory-map model file: {}", model_path))?
        };

        // Try hardware acceleration first, fallback to CPU
        let attempts = Self::get_provider_attempts();

        for (providers, names) in attempts {
            let attempt_result = catch_unwind(AssertUnwindSafe({
                let mmap_ref = &mmap;
                move || -> Result<Session> {
                    let builder = Session::builder()?
                        .with_optimization_level(GraphOptimizationLevel::Level3)?
                        .with_execution_providers(providers)?;
                    Ok(builder.commit_from_memory(mmap_ref)?)
                }
            }));

            match attempt_result {
                Ok(Ok(session)) => {
                    let primary = names.first().copied().unwrap_or("CPU").to_string();
                    info_println!("Rotation classifier using {}", primary);
                    return Ok((session, primary));
                }
                Ok(Err(err)) => {
                    debug_println!("Provider {:?} failed: {}", names, err);
                }
                Err(_) => {
                    debug_println!("Provider {:?} panicked", names);
                }
            }
        }

        Err(anyhow!(
            "All execution providers failed for rotation classifier"
        ))
    }

    #[cfg(target_os = "linux")]
    fn get_provider_attempts() -> Vec<(
        Vec<ort::execution_providers::ExecutionProviderDispatch>,
        Vec<&'static str>,
    )> {
        use ort::execution_providers::CPUExecutionProvider;

        if gpu_execution_disabled() {
            vec![(vec![CPUExecutionProvider::default().build()], vec!["CPU"])]
        } else if let Some(provider) = webgpu_execution_provider_dispatch() {
            vec![
                (vec![provider], vec!["WebGPU"]),
                (vec![CPUExecutionProvider::default().build()], vec!["CPU"]),
            ]
        } else {
            vec![
                (vec![CPUExecutionProvider::default().build()], vec!["CPU"]),
            ]
        }
    }

    #[cfg(target_os = "windows")]
    fn get_provider_attempts() -> Vec<(
        Vec<ort::execution_providers::ExecutionProviderDispatch>,
        Vec<&'static str>,
    )> {
        use ort::execution_providers::CPUExecutionProvider;
        vec![
            (
                vec![directml_execution_provider_dispatch()],
                vec!["DirectML"],
            ),
            (vec![CPUExecutionProvider::default().build()], vec!["CPU"]),
        ]
    }

    #[cfg(target_os = "macos")]
    fn get_provider_attempts() -> Vec<(
        Vec<ort::execution_providers::ExecutionProviderDispatch>,
        Vec<&'static str>,
    )> {
        use ort::execution_providers::{CPUExecutionProvider, CoreMLExecutionProvider};
        vec![
            (
                vec![CoreMLExecutionProvider::default().build()],
                vec!["CoreML"],
            ),
            (vec![CPUExecutionProvider::default().build()], vec!["CPU"]),
        ]
    }

    /// Classify the rotation of an image
    pub fn classify(&self, image: &RgbImage) -> Result<(RotationAngle, f32)> {
        // Preprocess: resize to 224x224 and normalize
        let preprocessed = self.preprocess(image)?;

        // Run inference
        let input_value = Value::from_array(preprocessed)?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow!("Rotation classifier session lock poisoned"))?;

        let outputs = session.run(ort::inputs!["x" => input_value])?;

        // Extract and interpret output
        let output_tensor = outputs["fetch_name_0"]
            .try_extract_tensor::<f32>()
            .context("Failed to extract rotation classification output")?;

        let (_shape, data) = output_tensor;

        // Find the class with highest confidence
        let (max_idx, max_confidence) = data
            .iter()
            .take(4)
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .ok_or_else(|| anyhow!("No classification output"))?;

        let angle = RotationAngle::from_index(max_idx)?;

        debug_println!(
            "Rotation classification: {:?} with confidence {:.2}%",
            angle,
            max_confidence * 100.0
        );

        Ok((angle, *max_confidence))
    }

    /// Preprocess image for rotation classification
    fn preprocess(&self, img: &RgbImage) -> Result<Array<f32, ndarray::Ix4>> {
        // Resize to 224x224
        let resized = image::imageops::resize(
            img,
            ROTATION_CLASSIFIER_SIZE,
            ROTATION_CLASSIFIER_SIZE,
            image::imageops::FilterType::Lanczos3,
        );

        // Convert to NCHW format and normalize to [0, 1]
        let mut input_data =
            vec![0.0f32; (3 * ROTATION_CLASSIFIER_SIZE * ROTATION_CLASSIFIER_SIZE) as usize];
        let channel_size = (ROTATION_CLASSIFIER_SIZE * ROTATION_CLASSIFIER_SIZE) as usize;

        for y in 0..ROTATION_CLASSIFIER_SIZE {
            for x in 0..ROTATION_CLASSIFIER_SIZE {
                let pixel = resized.get_pixel(x, y);
                let base = (y * ROTATION_CLASSIFIER_SIZE + x) as usize;

                // RGB order, normalized to [0, 1]
                input_data[base] = pixel[0] as f32 / 255.0;
                input_data[base + channel_size] = pixel[1] as f32 / 255.0;
                input_data[base + 2 * channel_size] = pixel[2] as f32 / 255.0;
            }
        }

        Array::from_shape_vec(
            (
                1,
                3,
                ROTATION_CLASSIFIER_SIZE as usize,
                ROTATION_CLASSIFIER_SIZE as usize,
            ),
            input_data,
        )
        .context("Failed to create rotation classifier input array")
    }
}

/// Document unwarping engine using UVDoc model
pub struct DocumentUnwarper {
    session: Mutex<Session>,
    #[allow(dead_code)]
    provider_name: String,
}

impl DocumentUnwarper {
    /// Load the document unwarping model
    pub fn new(model_path: &str) -> Result<Self> {
        // Ensure ONNX Runtime is initialized
        static ONNX_INIT: OnceCell<()> = OnceCell::new();
        ONNX_INIT.get_or_try_init(|| -> Result<()> {
            ort::init()
                .with_name("lege")
                .commit();
            Ok(())
        })?;

        let (session, provider_name) = Self::build_session(model_path)?;

        debug_println!("Document unwarper loaded with provider: {}", provider_name);

        Ok(Self {
            session: Mutex::new(session),
            provider_name,
        })
    }

    fn build_session(model_path: &str) -> Result<(Session, String)> {
        info!("Building document unwarper session: {}", model_path);

        let mmap = unsafe {
            let file = File::open(model_path)
                .with_context(|| format!("Failed to open model file: {}", model_path))?;
            Mmap::map(&file)
                .with_context(|| format!("Failed to memory-map model file: {}", model_path))?
        };

        // Try hardware acceleration first
        let attempts = Self::get_provider_attempts();

        for (providers, names) in attempts {
            let attempt_result = catch_unwind(AssertUnwindSafe({
                let mmap_ref = &mmap;
                move || -> Result<Session> {
                    let builder = Session::builder()?
                        .with_optimization_level(GraphOptimizationLevel::Level3)?
                        .with_intra_threads(4)?
                        .with_execution_providers(providers)?;
                    Ok(builder.commit_from_memory(mmap_ref)?)
                }
            }));

            match attempt_result {
                Ok(Ok(session)) => {
                    let primary = names.first().copied().unwrap_or("CPU").to_string();
                    info_println!("Document unwarper using {}", primary);
                    return Ok((session, primary));
                }
                Ok(Err(err)) => {
                    debug_println!("Provider {:?} failed: {}", names, err);
                }
                Err(_) => {
                    debug_println!("Provider {:?} panicked", names);
                }
            }
        }

        Err(anyhow!(
            "All execution providers failed for document unwarper"
        ))
    }

    #[cfg(target_os = "linux")]
    fn get_provider_attempts() -> Vec<(
        Vec<ort::execution_providers::ExecutionProviderDispatch>,
        Vec<&'static str>,
    )> {
        use ort::execution_providers::CPUExecutionProvider;

        if gpu_execution_disabled() {
            vec![(vec![CPUExecutionProvider::default().build()], vec!["CPU"])]
        } else if let Some(provider) = webgpu_execution_provider_dispatch() {
            vec![
                (vec![provider], vec!["WebGPU"]),
                (vec![CPUExecutionProvider::default().build()], vec!["CPU"]),
            ]
        } else {
            vec![
                (vec![CPUExecutionProvider::default().build()], vec!["CPU"]),
            ]
        }
    }

    #[cfg(target_os = "windows")]
    fn get_provider_attempts() -> Vec<(
        Vec<ort::execution_providers::ExecutionProviderDispatch>,
        Vec<&'static str>,
    )> {
        use ort::execution_providers::CPUExecutionProvider;
        vec![
            (
                vec![directml_execution_provider_dispatch()],
                vec!["DirectML"],
            ),
            (vec![CPUExecutionProvider::default().build()], vec!["CPU"]),
        ]
    }

    #[cfg(target_os = "macos")]
    fn get_provider_attempts() -> Vec<(
        Vec<ort::execution_providers::ExecutionProviderDispatch>,
        Vec<&'static str>,
    )> {
        use ort::execution_providers::{CPUExecutionProvider, CoreMLExecutionProvider};
        vec![
            (
                vec![CoreMLExecutionProvider::default().build()],
                vec!["CoreML"],
            ),
            (vec![CPUExecutionProvider::default().build()], vec!["CPU"]),
        ]
    }

    /// Unwarp a document image
    pub fn unwarp(&self, image: &RgbImage) -> Result<RgbImage> {
        let (width, height) = image.dimensions();
        debug_println!("Unwarping image: {}x{}", width, height);

        // Preprocess: convert to NCHW float32 [0, 1]
        let preprocessed = self.preprocess(image)?;

        // Run inference
        let input_value = Value::from_array(preprocessed)?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow!("Document unwarper session lock poisoned"))?;

        let session_outputs = session.run(ort::inputs!["image" => input_value])?;

        let output_value = session_outputs
            .into_iter()
            .find(|(name, _)| *name == "fetch_name_0")
            .map(|(_, value)| value)
            .context("Unwarp output missing fetch_name_0")?;

        let (output_shape, output_data) = output_value
            .try_extract_tensor::<f32>()
            .context("Failed to extract unwarp output")?;

        // Postprocess: convert back to RGB image
        let unwarped = self.postprocess(&output_data, &output_shape)?;

        debug_println!("Unwarping complete");

        Ok(unwarped)
    }

    /// Preprocess image to NCHW float32 [0, 1]
    fn preprocess(&self, img: &RgbImage) -> Result<Array<f32, ndarray::Ix4>> {
        let (width, height) = img.dimensions();
        let width = width as usize;
        let height = height as usize;

        let mut input_data = vec![0.0f32; 3 * height * width];
        let channel_size = height * width;

        for y in 0..height {
            for x in 0..width {
                let pixel = img.get_pixel(x as u32, y as u32);
                let base_idx = y * width + x;

                // RGB channels, normalized to [0, 1]
                input_data[base_idx] = pixel[0] as f32 / 255.0;
                input_data[base_idx + channel_size] = pixel[1] as f32 / 255.0;
                input_data[base_idx + 2 * channel_size] = pixel[2] as f32 / 255.0;
            }
        }

        Array::from_shape_vec((1, 3, height, width), input_data)
            .context("Failed to create unwarp input array")
    }

    /// Postprocess NCHW float32 output back to RGB image
    fn postprocess(&self, data: &[f32], shape: &Shape) -> Result<RgbImage> {
        anyhow::ensure!(
            shape.len() == 4 && shape[0] == 1 && shape[1] == 3,
            "Unexpected unwarp output shape: {:?}",
            shape
        );

        let height = shape[2] as usize;
        let width = shape[3] as usize;
        let channel_size = height * width;

        let mut img_buffer = ImageBuffer::new(width as u32, height as u32);

        for y in 0..height {
            for x in 0..width {
                let base_idx = y * width + x;

                let r = (data[base_idx].clamp(0.0, 1.0) * 255.0) as u8;
                let g = (data[base_idx + channel_size].clamp(0.0, 1.0) * 255.0) as u8;
                let b = (data[base_idx + 2 * channel_size].clamp(0.0, 1.0) * 255.0) as u8;

                img_buffer.put_pixel(x as u32, y as u32, Rgb([r, g, b]));
            }
        }

        Ok(img_buffer)
    }
}

/// Apply rotation correction to an image based on rotation angle
pub fn apply_rotation_correction(image: &RgbImage, angle: RotationAngle) -> RgbImage {
    use image::imageops::{rotate90, rotate180, rotate270};

    match angle {
        RotationAngle::NoRotation => image.clone(),
        RotationAngle::Rotate90 => rotate270(image), // Correct 90° CW by rotating 270° CCW
        RotationAngle::Rotate180 => rotate180(image),
        RotationAngle::Rotate270 => rotate90(image), // Correct 270° CW by rotating 90° CCW
    }
}

/// Combined deskew engine that handles both rotation and unwarping
pub struct DeskewEngine {
    rotation_classifier: Option<RotationClassifier>,
    document_unwarper: Option<DocumentUnwarper>,
    config: DeskewConfig,
}

impl DeskewEngine {
    /// Create a new deskew engine with both models
    pub fn new(
        rotation_model_path: Option<&str>,
        unwarp_model_path: Option<&str>,
        config: DeskewConfig,
    ) -> Result<Self> {
        let rotation_classifier = if config.enable_rotation_correction {
            if let Some(path) = rotation_model_path {
                Some(RotationClassifier::new(path)?)
            } else {
                None
            }
        } else {
            None
        };

        let document_unwarper = if config.enable_unwarping {
            if let Some(path) = unwarp_model_path {
                Some(DocumentUnwarper::new(path)?)
            } else {
                None
            }
        } else {
            None
        };

        Ok(Self {
            rotation_classifier,
            document_unwarper,
            config,
        })
    }

    /// Process a single image: detect rotation, correct it, then unwarp if needed
    pub fn process_image(&self, image: &RgbImage) -> Result<RgbImage> {
        let mut processed_image = image.clone();

        // Step 1: Rotation classification and correction
        if let Some(ref classifier) = self.rotation_classifier {
            let (angle, confidence) = classifier.classify(&processed_image)?;

            if confidence >= self.config.rotation_confidence_threshold && angle.needs_rotation() {
                info_println!(
                    "Detected rotation: {:.0}° (confidence: {:.1}%), correcting...",
                    angle.degrees(),
                    confidence * 100.0
                );
                processed_image = apply_rotation_correction(&processed_image, angle);
            } else if angle.needs_rotation() {
                debug_println!(
                    "Rotation detected ({:.0}°) but confidence {:.1}% below threshold {:.1}%",
                    angle.degrees(),
                    confidence * 100.0,
                    self.config.rotation_confidence_threshold * 100.0
                );
            }
        }

        // Step 2: Document unwarping
        if let Some(ref unwarper) = self.document_unwarper {
            info_println!("Applying document unwarp correction...");
            processed_image = unwarper.unwarp(&processed_image)?;
        }

        Ok(processed_image)
    }

    /// Process multiple images in batch
    pub fn process_images(&self, images: &[RgbImage]) -> Result<Vec<RgbImage>> {
        images.iter().map(|img| self.process_image(img)).collect()
    }
}

/// High-level function to process an image with deskewing
/// This is the main entry point for the pipeline
pub fn process_image_with_deskew(
    image: &RgbImage,
    rotation_model_path: Option<&str>,
    unwarp_model_path: Option<&str>,
    config: &DeskewConfig,
) -> Result<RgbImage> {
    let engine = DeskewEngine::new(rotation_model_path, unwarp_model_path, config.clone())?;

    engine.process_image(image)
}

// Safety: The engines are designed to be used in controlled contexts with proper synchronization
unsafe impl Send for RotationClassifier {}
unsafe impl Sync for RotationClassifier {}
unsafe impl Send for DocumentUnwarper {}
unsafe impl Sync for DocumentUnwarper {}
unsafe impl Send for DeskewEngine {}
unsafe impl Sync for DeskewEngine {}
