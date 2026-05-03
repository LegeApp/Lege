//! GPU binarization backends.
//!
//! The public API is intentionally small: callers pass grayscale bytes and receive the
//! same raw one-byte-per-pixel binary buffer as the CPU implementation.

use bytemuck::{Pod, Zeroable};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinarizationMode {
    FixedThreshold,
    Adaptive,
}

#[derive(Debug, Clone, Copy)]
pub struct AdaptiveBinarizeGpuConstants {
    pub sauvola_window: u32,
    pub bg_window: u32,
    pub percentile_c: u8,
    pub otsu_threshold: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct BinarizationParams {
    pub width: u32,
    pub height: u32,
    pub mode: BinarizationMode,
    pub invert_output: bool,
    pub k_factor: f32,
    pub fixed_threshold: u8,
    pub adaptive: AdaptiveBinarizeGpuConstants,
    pub debug_mode: u32,
}

#[derive(Debug)]
pub enum GpuBinarizationError {
    InvalidDimensions { width: u32, height: u32 },
    BufferSizeMismatch { expected: usize, actual: usize },
    Unsupported(String),
    Initialization(String),
    Shader(String),
    Execution(String),
}

impl std::fmt::Display for GpuBinarizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDimensions { width, height } => {
                write!(f, "invalid dimensions: {}x{}", width, height)
            }
            Self::BufferSizeMismatch { expected, actual } => {
                write!(
                    f,
                    "buffer size mismatch: expected {}, got {}",
                    expected, actual
                )
            }
            Self::Unsupported(msg) => write!(f, "unsupported GPU binarization path: {}", msg),
            Self::Initialization(msg) => {
                write!(f, "GPU binarization initialization failed: {}", msg)
            }
            Self::Shader(msg) => write!(f, "GPU binarization shader failed: {}", msg),
            Self::Execution(msg) => write!(f, "GPU binarization execution failed: {}", msg),
        }
    }
}

impl std::error::Error for GpuBinarizationError {}

pub type Result<T> = std::result::Result<T, GpuBinarizationError>;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct BinarizeParamsStd140 {
    pub width: u32,
    pub height: u32,
    pub mode: u32,
    pub invert_output: u32,
    pub fixed_threshold: u32,
    pub sauvola_window: u32,
    pub bg_window: u32,
    pub otsu_threshold: u32,
    pub k_factor: f32,
    pub percentile_c: f32,
    pub padded_width: u32,
    pub padded_height: u32,
    pub integral_width: u32,
    pub sauvola_radius: u32,
    pub debug_mode: u32,
    pub _pad2: u32,
    pub _pad3: u32,
    pub _pad4: u32,
    pub _pad5: u32,
    pub _pad6: u32,
    pub _pad7: u32,
}

impl BinarizationParams {
    pub fn validate_gray(&self, gray: &[u8]) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(GpuBinarizationError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        let expected = self.width as usize * self.height as usize;
        if gray.len() != expected {
            return Err(GpuBinarizationError::BufferSizeMismatch {
                expected,
                actual: gray.len(),
            });
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod wgpu;

#[cfg(windows)]
pub mod hlsl;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_fixed_threshold_smoke_when_enabled() {
        if std::env::var("LEGE_RUN_GPU_TESTS").ok().as_deref() != Some("1") {
            return;
        }

        let gray = vec![0u8, 255, 128, 32];
        let params = BinarizationParams {
            width: 2,
            height: 2,
            mode: BinarizationMode::FixedThreshold,
            invert_output: false,
            k_factor: 0.2,
            fixed_threshold: 50,
            adaptive: AdaptiveBinarizeGpuConstants {
                sauvola_window: 31,
                bg_window: 3,
                percentile_c: 0,
                otsu_threshold: 0,
            },
        };

        #[cfg(windows)]
        {
            let mut binarizer = hlsl::HlslBinarizer::new().expect("HLSL binarizer");
            let out = binarizer
                .binarize_gray_raw(&gray, &params)
                .expect("GPU fixed threshold");
            assert_eq!(out, vec![0, 255, 255, 0]);
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let mut binarizer = wgpu::WgpuBinarizer::new().expect("WGPU binarizer");
            let out = binarizer
                .binarize_gray_raw(&gray, &params)
                .expect("GPU fixed threshold");
            assert_eq!(out, vec![0, 255, 255, 0]);
        }
    }
}
