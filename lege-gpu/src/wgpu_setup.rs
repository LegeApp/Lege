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
//!
//! Adapter selection is explicit rather than relying on wgpu's
//! `PowerPreference::HighPerformance` hint. Some Linux hybrid-graphics drivers
//! return the integrated GPU for that hint even when a discrete GPU is
//! available. Every automatic Lege GPU path therefore orders enumerated
//! adapters Discrete -> Integrated -> Other -> Virtual -> Cpu and tries them in
//! that order. `WGPU_PREFER_INTEGRATED` swaps the first two ranks;
//! `WGPU_ADAPTER_SKIP` drops name-matched adapters for driver-specific
//! troubleshooting or deployments that reserve a device for other work.

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
    // DX12 is the validated production path on Windows. GL is included as a
    // last-resort fallback for older Intel iGPUs (pre-6th-gen) that lack DX12
    // support. wgpu's adapter ordering still places DX12 adapters first, so this
    // doesn't change behaviour on any system that already works.
    wgpu::Backends::DX12 | wgpu::Backends::GL
}

#[cfg(target_os = "macos")]
fn default_backends() -> wgpu::Backends {
    wgpu::Backends::METAL
}

// Must match the `cfg` on the `android` module itself. Keying this on
// `target_os` alone would make an unfeatured Android build report an
// unresolved-import error on top of the `compile_error!` that already explains
// the real problem.
#[cfg(all(target_os = "android", feature = "android"))]
fn default_backends() -> wgpu::Backends {
    crate::android::default_backends()
}

#[cfg(not(any(
    target_os = "windows",
    target_os = "macos",
    all(target_os = "android", feature = "android")
)))]
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

/// Rank an adapter type for Lege's automatic GPU selection policy.
///
/// A lower value is preferred. Explicit `WGPU_ADAPTER_NAME` selection is
/// applied before this policy and therefore still takes precedence.
///
/// `WGPU_PREFER_INTEGRATED=1` swaps discrete and integrated when a deployment
/// wants to reserve the discrete device. Default production order is unchanged.
pub(crate) fn adapter_type_preference(device_type: wgpu::DeviceType) -> u8 {
    let prefer_integrated = std::env::var_os("WGPU_PREFER_INTEGRATED").is_some_and(|value| {
        let value = value.to_string_lossy();
        value != "0" && !value.eq_ignore_ascii_case("false")
    });
    match (device_type, prefer_integrated) {
        (wgpu::DeviceType::IntegratedGpu, true) => 0,
        (wgpu::DeviceType::DiscreteGpu, true) => 1,
        (wgpu::DeviceType::DiscreteGpu, false) => 0,
        (wgpu::DeviceType::IntegratedGpu, false) => 1,
        (wgpu::DeviceType::Other, _) => 2,
        (wgpu::DeviceType::VirtualGpu, _) => 3,
        (wgpu::DeviceType::Cpu, _) => 4,
    }
}

/// Comma-separated name fragments from `WGPU_ADAPTER_SKIP`. Empty if unset.
pub(crate) fn adapter_skip_filters() -> Vec<String> {
    std::env::var("WGPU_ADAPTER_SKIP")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// True when `name` matches any `WGPU_ADAPTER_SKIP` fragment.
pub(crate) fn adapter_is_skipped(name: &str, filters: &[String]) -> bool {
    filters
        .iter()
        .any(|filter| adapter_name_matches(name, filter))
}

/// Order adapters according to [`adapter_type_preference`].
///
/// `sort_by_key` is stable, so the backend's enumeration order remains the
/// tie-breaker between adapters of the same type.
pub(crate) fn sort_adapters_by_preference(adapters: &mut [wgpu::Adapter]) {
    adapters.sort_by_key(|adapter| adapter_type_preference(adapter.get_info().device_type));
}

/// Match wgpu's explicit adapter-name override consistently across compute and
/// presentation paths.
pub(crate) fn adapter_name_matches(name: &str, filter: &str) -> bool {
    name.to_ascii_lowercase()
        .contains(&filter.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_adapter_order_prefers_discrete_hardware() {
        // This test documents the default production order. It must not
        // inherit WGPU_PREFER_INTEGRATED from the surrounding environment.
        let original = std::env::var_os("WGPU_PREFER_INTEGRATED");
        unsafe {
            std::env::remove_var("WGPU_PREFER_INTEGRATED");
        }
        let mut types = [
            wgpu::DeviceType::Cpu,
            wgpu::DeviceType::IntegratedGpu,
            wgpu::DeviceType::VirtualGpu,
            wgpu::DeviceType::DiscreteGpu,
            wgpu::DeviceType::Other,
        ];
        types.sort_by_key(|device_type| adapter_type_preference(*device_type));
        match original {
            Some(value) => unsafe { std::env::set_var("WGPU_PREFER_INTEGRATED", value) },
            None => unsafe { std::env::remove_var("WGPU_PREFER_INTEGRATED") },
        }

        assert_eq!(
            types,
            [
                wgpu::DeviceType::DiscreteGpu,
                wgpu::DeviceType::IntegratedGpu,
                wgpu::DeviceType::Other,
                wgpu::DeviceType::VirtualGpu,
                wgpu::DeviceType::Cpu,
            ]
        );
    }

    #[test]
    fn adapter_skip_matches_name_fragments() {
        assert!(adapter_is_skipped(
            "NVIDIA GeForce RTX 4060",
            &["4060".to_string()]
        ));
        assert!(!adapter_is_skipped(
            "Intel(R) Iris(R) Xe Graphics",
            &["4060".to_string()]
        ));
    }

    #[test]
    fn explicit_adapter_name_filter_is_case_insensitive_and_partial() {
        assert!(adapter_name_matches("NVIDIA GeForce RTX 4060", "rtx 4060"));
        assert!(adapter_name_matches(
            "Intel(R) Iris(R) Xe Graphics",
            "INTEL"
        ));
        assert!(!adapter_name_matches("NVIDIA GeForce RTX 4060", "Intel"));
    }
}
