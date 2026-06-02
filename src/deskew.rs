use std::path::PathBuf;
use std::sync::Mutex;

#[allow(unused_imports)]
use crate::{debug_println, error_println, info_println};
use anyhow::{Context, Result, anyhow};
use image::{Rgb, RgbImage};
use lege_gpu::vision::{
    DeskewConfig as VisionDeskewConfig, DocumentDeskewer as VisionDocumentDeskewer,
};
use lege_gpu::vision::{
    RotationClassifier as VisionRotationClassifier,
    RotationConfig as VisionRotationConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationAngle {
    NoRotation = 0,
    Rotate90 = 1,
    Rotate180 = 2,
    Rotate270 = 3,
}

impl RotationAngle {
    fn from_index(idx: usize) -> Result<Self> {
        match idx {
            0 => Ok(RotationAngle::NoRotation),
            1 => Ok(RotationAngle::Rotate90),
            2 => Ok(RotationAngle::Rotate180),
            3 => Ok(RotationAngle::Rotate270),
            _ => Err(anyhow!("Invalid rotation index: {}", idx)),
        }
    }

    pub fn degrees(&self) -> f32 {
        match self {
            RotationAngle::NoRotation => 0.0,
            RotationAngle::Rotate90 => 90.0,
            RotationAngle::Rotate180 => 180.0,
            RotationAngle::Rotate270 => 270.0,
        }
    }

    pub fn needs_rotation(&self) -> bool {
        !matches!(self, RotationAngle::NoRotation)
    }
}

#[derive(Debug, Clone)]
pub struct DeskewConfig {
    pub rotation_confidence_threshold: f32,
    pub enable_rotation_correction: bool,
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

pub struct RotationClassifier {
    inner: Mutex<VisionRotationClassifier>,
}

impl RotationClassifier {
    pub fn new(model_path: &str) -> Result<Self> {
        let vision = VisionRotationClassifier::from_model_path(model_path)
            .with_context(|| format!("failed to create rotation classifier from {}", model_path))?;
        info_println!("Rotation classifier loaded via lege-vision WGPU bridge");

        Ok(Self {
            inner: Mutex::new(vision),
        })
    }

    pub fn classify(&self, image: &RgbImage) -> Result<(RotationAngle, f32)> {
        let vision = self.inner.lock().map_err(|_| anyhow!("lock poisoned"))?;
        let pred = vision
            .classify_rgb(image)
            .context("rotation classification failed")?;

        let angle = RotationAngle::from_index(pred.class_id)?;

        debug_println!(
            "Rotation classification: {:?} with confidence {:.2}%",
            angle,
            pred.confidence * 100.0
        );

        Ok((angle, pred.confidence))
    }
}

pub struct DocumentUnwarper {
    inner: VisionDocumentDeskewer,
}

impl DocumentUnwarper {
    pub fn new(model_path: &str) -> Result<Self> {
        // The dynamic-resolution deskew model is prepared per page size on first
        // use and cached inside the vision deskewer; the graph runs at the page's
        // native resolution with no resize.
        let cfg = VisionDeskewConfig {
            model_path: PathBuf::from(model_path),
        };
        let inner = VisionDocumentDeskewer::new(cfg)
            .with_context(|| format!("failed to create document unwarper from {}", model_path))?;
        Ok(Self { inner })
    }

    pub fn unwarp(&self, image: &RgbImage) -> Result<RgbImage> {
        self.inner.unwarp_rgb(image)
    }
}

pub fn apply_rotation_correction(image: &RgbImage, angle: RotationAngle) -> RgbImage {
    match angle {
        RotationAngle::NoRotation => image.clone(),
        RotationAngle::Rotate90 => image::imageops::rotate270(image),
        RotationAngle::Rotate180 => image::imageops::rotate180(image),
        RotationAngle::Rotate270 => image::imageops::rotate90(image),
    }
}

pub struct DeskewEngine {
    rotation_classifier: Option<RotationClassifier>,
    document_unwarper: Option<DocumentUnwarper>,
    config: DeskewConfig,
}

impl DeskewEngine {
    pub fn new(
        rotation_model_path: Option<&str>,
        unwarp_model_path: Option<&str>,
        config: DeskewConfig,
    ) -> Result<Self> {
        let rotation_classifier = if config.enable_rotation_correction {
            rotation_model_path
                .map(|path| RotationClassifier::new(path))
                .transpose()?
        } else {
            None
        };

        let document_unwarper = if config.enable_unwarping {
            unwarp_model_path
                .map(|path| DocumentUnwarper::new(path))
                .transpose()?
        } else {
            None
        };

        Ok(Self {
            rotation_classifier,
            document_unwarper,
            config,
        })
    }

    pub fn process_image(&self, image: &RgbImage) -> Result<RgbImage> {
        let mut processed_image = image.clone();

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

        if let Some(ref unwarper) = self.document_unwarper {
            info_println!("Applying document unwarp correction...");
            processed_image = unwarper.unwarp(&processed_image)?;
        }

        Ok(processed_image)
    }

    pub fn process_images(&self, images: &[RgbImage]) -> Result<Vec<RgbImage>> {
        images.iter().map(|img| self.process_image(img)).collect()
    }
}

pub fn process_image_with_deskew(
    image: &RgbImage,
    rotation_model_path: Option<&str>,
    unwarp_model_path: Option<&str>,
    config: &DeskewConfig,
) -> Result<RgbImage> {
    let engine = DeskewEngine::new(rotation_model_path, unwarp_model_path, config.clone())?;
    engine.process_image(image)
}
