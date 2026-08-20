//! Android GPU policy.
//!
//! Compiles only with the `android` feature; see the `compile_error!` in
//! `lib.rs` for why targeting Android without it is rejected.

/// Backends to enable on Android.
///
/// Vulkan only — deliberately not `Backends::all()`, which would also admit
/// GL. GLES 3.1 guarantees only 4 storage buffers per shader stage, and the
/// convolution bind group uses 5 (input, weight, bias, output, params). A
/// device that fell through to GL would therefore enumerate an adapter,
/// satisfy `request_device`, and then fail when the first conv pipeline is
/// created — halfway through a job.
///
/// Restricting to Vulkan moves that failure to adapter selection, where
/// `initialize_inference_or_fallback` already handles it by disabling layout
/// detection and letting the run continue.
pub fn default_backends() -> wgpu::Backends {
    wgpu::Backends::VULKAN
}
