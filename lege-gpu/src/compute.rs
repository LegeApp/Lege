//! Shared process-wide GPU context for Lege compute clients.
//!
//! Resize, binarization, vision inference, and renderer postprocessing must
//! use this context rather than independently requesting adapters/devices.
//! This keeps adapter selection and driver fallback behavior identical across
//! Lege and avoids unstable concurrent Vulkan device creation.

use std::sync::Arc;

use anyhow::Result;

pub use wgpu;

/// Stable adapter metadata used by automatic backend selection and telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    pub name: Arc<str>,
    pub backend: wgpu::Backend,
    pub device_type: wgpu::DeviceType,
    pub supports_timestamps: bool,
}

impl AdapterInfo {
    /// Automatic compute paths run only on a discrete or integrated GPU.
    pub fn is_hardware_gpu(&self) -> bool {
        matches!(
            self.device_type,
            wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu
        )
    }
}

/// Cloneable handle to Lege's single process-wide wgpu device and queue.
#[derive(Clone)]
pub struct SharedGpuContext {
    inner: crate::vision::runtime::device::GpuContext,
}

impl std::fmt::Debug for SharedGpuContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedGpuContext")
            .field("adapter", &self.adapter_info())
            .finish_non_exhaustive()
    }
}

impl SharedGpuContext {
    /// Initialize or clone the shared context using Lege's normal adapter
    /// detection (`WGPU_BACKEND`, `WGPU_ADAPTER_NAME`, and
    /// `WGPU_REQUIRE_REAL_GPU` are honored).
    pub fn get() -> Result<Self> {
        pollster::block_on(Self::get_async())
    }

    /// Async form of [`SharedGpuContext::get`].
    pub async fn get_async() -> Result<Self> {
        Ok(Self {
            inner: crate::vision::runtime::device::GpuContext::shared().await?,
        })
    }

    pub fn device(&self) -> &Arc<wgpu::Device> {
        &self.inner.device
    }

    pub fn queue(&self) -> &Arc<wgpu::Queue> {
        &self.inner.queue
    }

    pub fn adapter_info(&self) -> AdapterInfo {
        AdapterInfo {
            name: Arc::clone(&self.inner.adapter_name),
            backend: self.inner.adapter_backend,
            device_type: self.inner.adapter_device_type,
            supports_timestamps: self.inner.supports_timestamps,
        }
    }

    /// Wait for all work submitted before this call to complete.
    pub fn wait(&self) -> Result<()> {
        self.inner.wait()
    }
}
