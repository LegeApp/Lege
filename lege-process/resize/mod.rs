pub mod cpu;

use bytemuck::Pod;
use fast_image_resize::{
    Filter, FilterType as FirFilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
    images::Image as FirImage,
};
use lege_gpu::resize::wgpu::{
    FilterType as WgpuFilterType, ResizeParameters as WgpuResizeParameters, WgpuResizer,
};
use log::warn;
use once_cell::sync::OnceCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::sync::atomic::{AtomicUsize, Ordering};
static WGPU_RESIZER_POOL: OnceCell<WgpuResizerPool> = OnceCell::new();
static WGPU_RESIZE_COUNT: AtomicUsize = AtomicUsize::new(0);
static RESIZE_BACKEND_PREFERENCE: AtomicU8 = AtomicU8::new(0); // 0=auto, 1=fast_cpu

/// Per-job gate for the GPU resize backend. Default: enabled.
static GPU_RESIZE_GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_gpu_resize_enabled(enabled: bool) {
    GPU_RESIZE_GATE.store(enabled, AtomicOrdering::Relaxed);
}

pub fn gpu_resize_enabled() -> bool {
    GPU_RESIZE_GATE.load(AtomicOrdering::Relaxed)
}

/// Pool of `WgpuResizer` instances so page workers can resize concurrently.
///
/// A single resizer owns its GPU scratch/readback buffers and must be used by
/// one thread at a time, so the old design serialized every resize behind one
/// `Mutex<WgpuResizer>`. The renderer and inference paths are now multithreaded,
/// so this pool hands each caller its own resizer (checked out from a free list
/// or created on demand) and retains up to `max_idle` for reuse. The shared
/// wgpu device/queue underneath is `Sync`, so concurrent submits are safe.
struct WgpuResizerPool {
    prototype: Mutex<WgpuResizer>,
    free: Mutex<Vec<WgpuResizer>>,
    max_idle: usize,
    active: AtomicUsize,
    peak_active: AtomicUsize,
}

impl WgpuResizerPool {
    fn new(max_idle: usize) -> Result<Self, ResizeError> {
        let prototype = WgpuResizer::new().map_err(|e| {
            ResizeError::BackendError(format!("Failed to initialize WGPU resizer: {e}"))
        })?;
        Ok(Self {
            prototype: Mutex::new(prototype),
            free: Mutex::new(Vec::new()),
            max_idle: max_idle.max(1),
            active: AtomicUsize::new(0),
            peak_active: AtomicUsize::new(0),
        })
    }

    fn checkout(&self) -> Result<PooledResizer<'_>, ResizeError> {
        let resizer = self.free.lock().unwrap().pop().unwrap_or_else(|| {
            // Sibling creation only clones the already compiled pipeline
            // handles and allocates scratch lazily on first use.
            self.prototype.lock().unwrap().build_sibling()
        });
        let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        let previous_peak = self.peak_active.fetch_max(active, Ordering::Relaxed);
        #[cfg(feature = "debug-logging")]
        if active > previous_peak {
            println!("WGPU resize pool concurrency reached {active}");
        }
        Ok(PooledResizer {
            pool: self,
            resizer: Some(resizer),
        })
    }

    fn checkin(&self, resizer: WgpuResizer) {
        let mut free = self.free.lock().unwrap();
        if free.len() < self.max_idle {
            free.push(resizer);
        }
        // Otherwise drop it: a transient burst allocated more resizers than the
        // steady-state pool needs to retain.
    }
}

/// RAII handle that returns its resizer to the pool on drop, even on the error
/// paths of `resize_bytes`, so a failed resize never leaks a pool slot.
struct PooledResizer<'a> {
    pool: &'a WgpuResizerPool,
    resizer: Option<WgpuResizer>,
}

impl std::ops::Deref for PooledResizer<'_> {
    type Target = WgpuResizer;
    fn deref(&self) -> &Self::Target {
        self.resizer.as_ref().expect("resizer present until drop")
    }
}

impl std::ops::DerefMut for PooledResizer<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.resizer.as_mut().expect("resizer present until drop")
    }
}

impl Drop for PooledResizer<'_> {
    fn drop(&mut self) {
        self.pool.active.fetch_sub(1, Ordering::Relaxed);
        if let Some(resizer) = self.resizer.take() {
            self.pool.checkin(resizer);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeBackendPreference {
    Auto,
    FastCpu,
}

pub fn set_resize_backend_preference(preference: ResizeBackendPreference) {
    let encoded = match preference {
        ResizeBackendPreference::Auto => 0,
        ResizeBackendPreference::FastCpu => 1,
    };
    RESIZE_BACKEND_PREFERENCE.store(encoded, AtomicOrdering::Relaxed);
}

pub fn resize_backend_preference() -> ResizeBackendPreference {
    if std::env::var("LEGE_FORCE_CPU_RESIZE")
        .map(|value| value != "0")
        .unwrap_or(false)
        || std::env::var("LEGE_RESIZE_BACKEND")
            .map(|value| {
                value.eq_ignore_ascii_case("cpu") || value.eq_ignore_ascii_case("fast_cpu")
            })
            .unwrap_or(false)
    {
        return ResizeBackendPreference::FastCpu;
    }

    match RESIZE_BACKEND_PREFERENCE.load(AtomicOrdering::Relaxed) {
        1 => ResizeBackendPreference::FastCpu,
        _ => ResizeBackendPreference::Auto,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeMethod {
    Nearest,
    Bilinear,
    Bicubic,
    Bell,
    Lanczos3,
}

#[derive(Debug, Clone, Copy)]
pub struct ResizeParams {
    pub target_width: u32,
    pub target_height: u32,
    pub method: ResizeMethod,
    pub letterbox: bool,
    pub border_value: f32,
    pub swap_rb: bool,
}

#[derive(Debug)]
pub enum ResizeError {
    InvalidDimensions,
    BackendError(String),
    EmptyInput,
}

impl std::fmt::Display for ResizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResizeError::InvalidDimensions => write!(f, "Invalid dimensions"),
            ResizeError::BackendError(e) => write!(f, "Backend error: {e}"),
            ResizeError::EmptyInput => write!(f, "Empty input batch"),
        }
    }
}
impl std::error::Error for ResizeError {}

fn resize_alg_from_method(method: ResizeMethod) -> ResizeAlg {
    match method {
        ResizeMethod::Nearest => ResizeAlg::Nearest,
        ResizeMethod::Bilinear => ResizeAlg::Convolution(FirFilterType::Bilinear),
        ResizeMethod::Bicubic => ResizeAlg::Convolution(FirFilterType::CatmullRom),
        ResizeMethod::Bell => ResizeAlg::Convolution(bell_filter_type()),
        ResizeMethod::Lanczos3 => ResizeAlg::Convolution(FirFilterType::Lanczos3),
    }
}

fn bell_filter(mut x: f64) -> f64 {
    x = x.abs();
    if x < 0.5 {
        0.75 - x * x
    } else if x < 1.5 {
        let t = 1.5 - x;
        0.5 * t * t
    } else {
        0.0
    }
}

fn bell_filter_type() -> FirFilterType {
    FirFilterType::Custom(Filter::new("Bell", bell_filter, 1.5).expect("valid Bell filter"))
}

fn wgpu_resizer_pool() -> Result<&'static WgpuResizerPool, ResizeError> {
    WGPU_RESIZER_POOL.get_or_try_init(|| {
        let default_size = std::thread::available_parallelism()
            .map(|threads| threads.get().clamp(2, 4))
            .unwrap_or(2);
        let pool_size = std::env::var("LEGE_GPU_RESIZE_SESSIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(default_size)
            .clamp(1, 8);
        #[cfg(feature = "debug-logging")]
        println!("WGPU resize pool configured for up to {pool_size} idle session(s)");
        WgpuResizerPool::new(pool_size)
    })
}

fn wgpu_filter_from_method(method: ResizeMethod) -> WgpuFilterType {
    match method {
        ResizeMethod::Nearest | ResizeMethod::Bilinear => WgpuFilterType::Bilinear,
        ResizeMethod::Bicubic | ResizeMethod::Bell => WgpuFilterType::Bell,
        ResizeMethod::Lanczos3 => WgpuFilterType::Lanczos3,
    }
}

fn cpu_resize_bytes(
    src_data: &[u8],
    src_width: u32,
    src_height: u32,
    params: &ResizeParams,
    channel_count: u32,
) -> Result<Vec<u8>, ResizeError> {
    let dst_width = params.target_width;
    let dst_height = params.target_height;
    if dst_width == 0 || dst_height == 0 {
        return Err(ResizeError::InvalidDimensions);
    }

    let pixel_type = match channel_count {
        1 => PixelType::U8,
        3 => PixelType::U8x3,
        4 => PixelType::U8x4,
        _ => {
            return Err(ResizeError::BackendError(format!(
                "Unsupported channel count: {channel_count}"
            )));
        }
    };

    let mut owned = src_data.to_vec();
    let src_image = FirImage::from_slice_u8(src_width, src_height, &mut owned, pixel_type)
        .map_err(|e| ResizeError::BackendError(format!("Failed to create source image: {e:?}")))?;

    let mut dst = vec![0u8; (dst_width * dst_height * channel_count) as usize];
    let mut dst_image = FirImage::from_slice_u8(dst_width, dst_height, &mut dst, pixel_type)
        .map_err(|e| ResizeError::BackendError(format!("Failed to create dest image: {e:?}")))?;

    let resize_options = ResizeOptions::new().resize_alg(resize_alg_from_method(params.method));
    let mut resizer = Resizer::new();
    resizer
        .resize(&src_image, &mut dst_image, Some(&resize_options))
        .map_err(|e| ResizeError::BackendError(format!("CPU resize failed: {e:?}")))?;

    if params.swap_rb && channel_count >= 3 {
        for px in dst.chunks_exact_mut(channel_count as usize) {
            px.swap(0, 2);
        }
    }

    Ok(dst)
}

fn wgpu_resize_bytes(
    src_data: &[u8],
    src_width: u32,
    src_height: u32,
    params: &ResizeParams,
    channel_count: u32,
) -> Result<Vec<u8>, ResizeError> {
    let mut resizer = wgpu_resizer_pool()?.checkout()?;

    let mut wgpu_params = WgpuResizeParameters::new(
        src_width,
        src_height,
        params.target_width,
        params.target_height,
    );
    wgpu_params.filter = wgpu_filter_from_method(params.method);
    wgpu_params.border_value = params.border_value;
    wgpu_params.channel_count = channel_count;
    wgpu_params.no_srgb = true;

    let mut data = resizer
        .resize(src_data, &wgpu_params)
        .map_err(|e| ResizeError::BackendError(format!("WGPU resize failed: {e}")))?;

    if params.swap_rb && channel_count >= 3 {
        for px in data.chunks_exact_mut(channel_count as usize) {
            px.swap(0, 2);
        }
    }

    Ok(data)
}

/// Resize image bytes using WGPU acceleration when available, with CPU fallback.
pub fn resize_bytes(
    src_data: &[u8],
    src_width: u32,
    src_height: u32,
    params: &ResizeParams,
    channel_count: u32,
) -> Result<Vec<u8>, ResizeError> {
    if matches!(
        resize_backend_preference(),
        ResizeBackendPreference::FastCpu
    ) || !gpu_resize_enabled()
    {
        return cpu_resize_bytes(src_data, src_width, src_height, params, channel_count).map_err(
            |e| {
                log::error!(
                    "CPU-fast resize failed for {}x{} -> {}x{}: {}",
                    src_width,
                    src_height,
                    params.target_width,
                    params.target_height,
                    e
                );
                e
            },
        );
    }

    match wgpu_resize_bytes(src_data, src_width, src_height, params, channel_count) {
        Ok(data) => {
            let _count = WGPU_RESIZE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

            #[cfg(feature = "debug-logging")]
            {
                if _count == 1 {
                    println!(
                        "WGPU resize #1: {}x{} -> {}x{} ({} channels) - Hardware acceleration active!",
                        src_width,
                        src_height,
                        params.target_width,
                        params.target_height,
                        channel_count
                    );
                } else if _count % 50 == 0 {
                    println!(
                        "WGPU resize #{}: {}x{} -> {}x{} ({} channels)",
                        _count,
                        src_width,
                        src_height,
                        params.target_width,
                        params.target_height,
                        channel_count
                    );
                }
            }

            log::debug!(
                "WGPU resize successful: {}x{} -> {}x{} ({} channels)",
                src_width,
                src_height,
                params.target_width,
                params.target_height,
                channel_count
            );
            return Ok(data);
        }
        Err(err) => {
            warn!(
                "WGPU resize failed ({}x{} -> {}x{}): {}; falling back to CPU",
                src_width, src_height, params.target_width, params.target_height, err
            );
            #[cfg(feature = "debug-logging")]
            crate::info_log!(
                "[resize] WGPU fallback to CPU ({}x{} -> {}x{}): {}",
                src_width,
                src_height,
                params.target_width,
                params.target_height,
                err
            );
        }
    }

    // CPU fallback
    cpu_resize_bytes(src_data, src_width, src_height, params, channel_count).map_err(|e| {
        log::error!(
            "Resize failed for {}x{} -> {}x{}: {}",
            src_width,
            src_height,
            params.target_width,
            params.target_height,
            e
        );
        e
    })
}

pub trait PixelComponent: Copy + Clone + Default + Send + Sync + Pod + 'static {
    fn pixel_type_of(channels: u32) -> Option<PixelType>;
    fn to_f32(self) -> f32;
    fn from_f32(val: f32) -> Self;
}

impl PixelComponent for u8 {
    fn pixel_type_of(channels: u32) -> Option<PixelType> {
        match channels {
            1 => Some(PixelType::U8),
            2 => Some(PixelType::U8x2),
            3 => Some(PixelType::U8x3),
            4 => Some(PixelType::U8x4),
            _ => None,
        }
    }
    fn to_f32(self) -> f32 {
        self as f32 / 255.0
    }
    fn from_f32(val: f32) -> Self {
        let v = (val.clamp(0.0, 1.0) * 255.0 + 0.5) as i32;
        v.max(0).min(255) as u8
    }
}

impl PixelComponent for f32 {
    fn pixel_type_of(channels: u32) -> Option<PixelType> {
        match channels {
            1 => Some(PixelType::F32),
            3 => Some(PixelType::F32), // Use F32 for 3-channel
            4 => Some(PixelType::F32), // Use F32 for 4-channel
            _ => None,
        }
    }
    fn to_f32(self) -> f32 {
        self
    }
    fn from_f32(val: f32) -> Self {
        val
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_bytes_cpu_fallback_preserves_channel_shape() {
        set_resize_backend_preference(ResizeBackendPreference::FastCpu);
        let src = vec![255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let params = ResizeParams {
            target_width: 4,
            target_height: 3,
            method: ResizeMethod::Bilinear,
            letterbox: false,
            border_value: 0.0,
            swap_rb: false,
        };

        let out = resize_bytes(&src, 2, 2, &params, 3).expect("resize should succeed");
        assert_eq!(out.len(), 4 * 3 * 3);
        set_resize_backend_preference(ResizeBackendPreference::Auto);
    }
}
