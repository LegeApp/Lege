//! Shared wgpu backend/instance policy for every lege-gpu compute path
//! (vision/YOLO, resize, binarization).
//!
//! Production policy: on Windows we **insist on DX12**. It is measurably faster
//! than Vulkan in our testing and is the validated production path; allowing
//! wgpu to fall onto Vulkan silently (which an all-backends instance permits)
//! is not acceptable. macOS uses Metal, Linux uses all available backends.
//!
//! The choice is overridable via the `WGPU_BACKEND` environment variable
//! (comma-separated: `dx12`, `vulkan`, `metal`, `gl`, `webgpu`, or `all`).

/// Backends to enable for lege-gpu compute work, honouring `WGPU_BACKEND`.
pub fn requested_backends() -> wgpu::Backends {
    let Ok(raw) = std::env::var("WGPU_BACKEND") else {
        return default_backends();
    };
    let mut backends = wgpu::Backends::empty();
    for part in raw.split(',').map(|part| part.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "vulkan" | "vk" => backends |= wgpu::Backends::VULKAN,
            "metal" | "mtl" => backends |= wgpu::Backends::METAL,
            "dx12" | "d3d12" => backends |= wgpu::Backends::DX12,
            "gl" | "opengl" | "gles" => backends |= wgpu::Backends::GL,
            "browser_webgpu" | "webgpu" => backends |= wgpu::Backends::BROWSER_WEBGPU,
            "all" => backends |= wgpu::Backends::all(),
            _ => {}
        }
    }
    if backends.is_empty() {
        default_backends()
    } else {
        backends
    }
}

#[cfg(target_os = "windows")]
fn default_backends() -> wgpu::Backends {
    // Insist on DX12 in production on Windows.
    wgpu::Backends::DX12
}

#[cfg(target_os = "macos")]
fn default_backends() -> wgpu::Backends {
    wgpu::Backends::METAL
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn default_backends() -> wgpu::Backends {
    wgpu::Backends::all()
}

/// Create a wgpu instance constrained to [`requested_backends`], so that
/// `request_adapter` cannot silently select a backend we did not ask for
/// (e.g. Vulkan on Windows when DX12 is the production target).
pub fn create_instance() -> wgpu::Instance {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = requested_backends();
    wgpu::Instance::new(desc)
}
