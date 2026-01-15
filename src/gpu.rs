use log::info;
use std::sync::OnceLock;

#[cfg(target_os = "linux")]
static WEBGPU_PROVIDER_CACHE: OnceLock<Option<ort::execution_providers::ExecutionProviderDispatch>> = OnceLock::new();

#[cfg(target_os = "linux")]
pub fn webgpu_execution_provider_dispatch() -> Option<ort::execution_providers::ExecutionProviderDispatch> {
    // Clone cached provider to avoid rebuilding it for every engine instance
    // If provider creation fails, cache the None result to indicate unavailability
    WEBGPU_PROVIDER_CACHE.get_or_init(|| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Some(build_webgpu_execution_provider())
        })).unwrap_or_else(|_| {
            log::error!("WebGPU provider initialization panicked, falling back to CPU");
            None
        })
    }).clone()
}

#[cfg(target_os = "linux")]
fn build_webgpu_execution_provider() -> ort::execution_providers::ExecutionProviderDispatch {
    use ort::execution_providers::{
        WebGPUExecutionProvider, WebGPUDawnBackendType, WebGPUBufferCacheMode,
        WebGPUPreferredLayout, WebGPUValidationMode
    };

    info!("Initializing WebGPU with Vulkan backend");

    WebGPUExecutionProvider::default()
        .with_dawn_backend_type(WebGPUDawnBackendType::Vulkan)
        // REMOVED: .with_preferred_layout(WebGPUPreferredLayout::NCHW)
        // Let ORT/Dawn choose the optimal layout. WebGPU often performs better with NHWC
        // even though models are typically NCHW, due to memory access patterns on GPU.
        .with_storage_buffer_cache_mode(WebGPUBufferCacheMode::Bucket)
        .with_uniform_buffer_cache_mode(WebGPUBufferCacheMode::LazyRelease)
        .with_default_buffer_cache_mode(WebGPUBufferCacheMode::LazyRelease)
        .with_enable_graph_capture(true) // Keep graph capture enabled (safe optimization)
        .with_validation_mode(if cfg!(debug_assertions) {
            WebGPUValidationMode::Basic
        } else {
            WebGPUValidationMode::Disabled
        })
        .build()
        // If WebGPU provider build fails, we'll let it propagate up to engine where it will be caught
}

/// Logs GPU memory statistics if available
#[cfg(target_os = "linux")]
pub fn log_gpu_memory_stats() {
    // Note: Memory statistics functionality is not directly available in the current ONNX Runtime API.
    // This is a placeholder for future implementation when the API becomes available.
    // You can monitor GPU memory usage through system tools like nvidia-smi or radeontop.
    info!("GPU memory statistics are not directly available through the ONNX Runtime API");
}

#[cfg(windows)]
static DIRECTML_ADAPTER_CACHE: OnceLock<Option<(i32, String)>> = OnceLock::new();

#[cfg(windows)]
fn preferred_directml_adapter() -> Option<(i32, String)> {
    // Return cached result if available
    DIRECTML_ADAPTER_CACHE.get_or_init(|| {
        probe_directml_adapter()
    }).clone()
}

#[cfg(windows)]
fn probe_directml_adapter() -> Option<(i32, String)> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };

    let factory: IDXGIFactory1 = unsafe {
        match CreateDXGIFactory1::<IDXGIFactory1>() {
            Ok(factory) => factory,
            Err(err) => {
                log::debug!("DirectML adapter probe: CreateDXGIFactory1 failed: {err:?}");
                return None;
            }
        }
    };

    let mut best: Option<(i32, u64, String)> = None;
    let mut index: u32 = 0;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(err) => {
                let code = err.code();
                if code.is_err() {
                    log::debug!(
                        "DirectML adapter probe: EnumAdapters1 stopped at index {index}: {err:?}"
                    );
                }
                break;
            }
        };

        let desc = match unsafe { adapter.GetDesc1() } {
            Ok(desc) => desc,
            Err(err) => {
                log::debug!(
                    "DirectML adapter probe: GetDesc1 failed for adapter {index}: {err:?}"
                );
                index += 1;
                continue;
            }
        };

        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            index += 1;
            continue;
        }

        let name_len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..name_len]).trim().to_string();
        let dedicated_mem = desc.DedicatedVideoMemory as u64;

        let better = best
            .as_ref()
            .map(|(_, best_mem, _)| dedicated_mem > *best_mem)
            .unwrap_or(true);
        if better {
            best = Some((index as i32, dedicated_mem, name));
        }

        index += 1;
    }

    best.map(|(idx, _, name)| (idx, name))
}

#[cfg(windows)]
static DIRECTML_PROVIDER_CACHE: OnceLock<ort::execution_providers::ExecutionProviderDispatch> = OnceLock::new();

#[cfg(windows)]
pub fn directml_execution_provider_dispatch() -> ort::execution_providers::ExecutionProviderDispatch {
    // Clone cached provider to avoid rebuilding it for every engine instance
    DIRECTML_PROVIDER_CACHE.get_or_init(|| {
        build_directml_execution_provider()
    }).clone()
}

#[cfg(windows)]
fn build_directml_execution_provider() -> ort::execution_providers::ExecutionProviderDispatch {
    use ort::execution_providers::DirectMLExecutionProvider;

    let mut provider = DirectMLExecutionProvider::default();
    if let Some((device_id, name)) = preferred_directml_adapter() {
        info!(
            "Selecting DirectML adapter '{}' (device {}) for ONNX Runtime",
            name, device_id
        );
        provider = provider.with_device_id(device_id);
    } else {
        log::debug!("DirectML adapter probe failed; falling back to default selection");
    }

    provider.build()
}
