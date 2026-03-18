//! HLSL-based image resizer for Windows
//!
//! Provides high-performance image resizing using HLSL compute shaders
//! and DirectX 12. Optimized for PaddleX inference, OCR preprocessing,
//! and margin calculation workflows on Windows.

mod converter;
mod dx12;
mod error;
mod filters;
mod resizer;

pub use converter::{ConversionType, HlslConverter};
pub use error::{ResizerError, Result};
pub use filters::{BorderMode, FilterType};
pub use resizer::{GpuResizeOutput, HlslResizer, ResizeParameters, UseCase};

/// Re-export commonly used types
pub mod prelude {
    pub use crate::resize::hlsl::{
        BorderMode, FilterType, GpuResizeOutput, HlslResizer, ResizeParameters, ResizerError,
        Result, UseCase,
    };
}

// Include auto-generated shader byte arrays from build.rs if available.
// build.rs sets SHADER_INCLUDE_PATH to a generated file that defines:
//   pub const BILINEAR_SHADER: &[u8] = include_bytes!(...);
//   pub const BELL_SHADER: &[u8] = include_bytes!(...);
//   pub const LANCZOS3_SHADER: &[u8] = include_bytes!(...);
// In tests/docs or when feature `no-include-shaders` is enabled, fall back to empty arrays.
#[allow(dead_code)]
mod shaders_include {
    #[cfg(not(any(doc, test, feature = "no-include-shaders")))]
    include!(env!("SHADER_INCLUDE_PATH"));

    #[cfg(any(doc, test, feature = "no-include-shaders"))]
    pub const BILINEAR_SHADER: &[u8] = &[];
    #[cfg(any(doc, test, feature = "no-include-shaders"))]
    pub const BELL_SHADER: &[u8] = &[];
    #[cfg(any(doc, test, feature = "no-include-shaders"))]
    pub const LANCZOS3_SHADER: &[u8] = &[];
    #[cfg(any(doc, test, feature = "no-include-shaders"))]
    pub const RGBA8_TO_NCHW_F32_SHADER: &[u8] = &[];
}

pub use shaders_include::{
    BELL_SHADER, BILINEAR_SHADER, LANCZOS3_SHADER, RGBA8_TO_NCHW_F32_SHADER,
};
