//! Lege Vision WGPU inference bridge.
//!
//! This module vendors the production inference runtime from the WGPU bridge.
//! CLI/tester-only entry points and oracle helpers are intentionally excluded.

pub(crate) use ::wgpu;

pub(crate) mod decode;
pub(crate) mod onnx;
pub(crate) mod onnx_pb;
pub(crate) mod ops;
pub(crate) mod preprocess;
pub(crate) mod reference;
pub(crate) mod runtime;

mod api;

pub use api::{
    LayoutConfig, LayoutDetection, LayoutDetector, SauvolaConfig, SauvolaCpuProcessor,
    SauvolaProcessor, is_layout_gpu_device_error, is_layout_software_adapter_error,
};
