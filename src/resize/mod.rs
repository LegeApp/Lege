pub mod cpu;
#[cfg(windows)]
pub mod hlsl;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod wgpu;

use bytemuck::Pod;
use fast_image_resize::{
    images::Image as FirImage, FilterType as FirFilterType, PixelType, ResizeAlg, ResizeOptions,
    Resizer,
};
use log::warn;
#[cfg(windows)]
use once_cell::sync::OnceCell;
#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(windows)]
use hlsl::{FilterType as HlslFilterType, HlslResizer, ResizeParameters as HlslResizeParameters};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use once_cell::sync::OnceCell;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::Mutex;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use wgpu::{FilterType as WgpuFilterType, WgpuResizer, ResizeParameters as WgpuResizeParameters};
#[cfg(windows)]
static HLSL_RESIZER: OnceCell<Mutex<HlslResizer>> = OnceCell::new();
#[cfg(windows)]
static GPU_RESIZE_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(any(target_os = "linux", target_os = "macos"))]
static WGPU_RESIZER: OnceCell<Mutex<WgpuResizer>> = OnceCell::new();
#[cfg(any(target_os = "linux", target_os = "macos"))]
static WGPU_RESIZE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeMethod {
    Nearest,
    Bilinear,
    Bicubic,
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

#[derive(Debug)]
pub enum ProcessingError {
    InvalidDimensions(String),
    CoordError(String),
    JsonError(String),
    Io(std::io::Error),
    Serde(serde_json::Error),
    Other(String),
}
impl std::fmt::Display for ProcessingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessingError::InvalidDimensions(s) => write!(f, "Invalid dimensions: {s}"),
            ProcessingError::CoordError(s) => write!(f, "Coord error: {s}"),
            ProcessingError::JsonError(s) => write!(f, "Json error: {s}"),
            ProcessingError::Io(e) => write!(f, "IO error: {e}"),
            ProcessingError::Serde(e) => write!(f, "Serde error: {e}"),
            ProcessingError::Other(s) => write!(f, "Other error: {s}"),
        }
    }
}
impl std::error::Error for ProcessingError {}
impl From<std::io::Error> for ProcessingError {
    fn from(e: std::io::Error) -> Self {
        ProcessingError::Io(e)
    }
}
impl From<serde_json::Error> for ProcessingError {
    fn from(e: serde_json::Error) -> Self {
        ProcessingError::Serde(e)
    }
}

fn resize_alg_from_method(method: ResizeMethod) -> ResizeAlg {
    match method {
        ResizeMethod::Nearest => ResizeAlg::Nearest,
        ResizeMethod::Bilinear => ResizeAlg::Convolution(FirFilterType::Bilinear),
        ResizeMethod::Bicubic => ResizeAlg::Convolution(FirFilterType::CatmullRom),
        ResizeMethod::Lanczos3 => ResizeAlg::Convolution(FirFilterType::Lanczos3),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ensure_wgpu_resizer() -> Result<&'static Mutex<WgpuResizer>, ResizeError> {
    WGPU_RESIZER.get_or_try_init(|| {
        let resizer = WgpuResizer::new()
            .map_err(|e| ResizeError::BackendError(format!("Failed to initialize WGPU resizer: {e}")))?;

        #[cfg(feature = "debug-logging")]
        println!("✓ WGPU GPU resizer initialized successfully");

        Ok(Mutex::new(resizer))
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wgpu_filter_from_method(method: ResizeMethod) -> WgpuFilterType {
    match method {
        ResizeMethod::Nearest | ResizeMethod::Bilinear => WgpuFilterType::Bilinear,
        ResizeMethod::Bicubic => WgpuFilterType::Bell,
        ResizeMethod::Lanczos3 => WgpuFilterType::Lanczos3,
    }
}

#[cfg(windows)]
fn ensure_hlsl_resizer() -> Result<&'static Mutex<HlslResizer>, ResizeError> {
    HLSL_RESIZER
        .get_or_try_init(|| {
            let resizer = HlslResizer::new()
                .map_err(|e| ResizeError::BackendError(format!("Failed to initialize HLSL resizer: {e:?}")))?;
            
            #[cfg(feature = "debug-logging")]
            println!("✓ HLSL GPU resizer initialized successfully (DirectX 12 compute shaders)");
            
            Ok(Mutex::new(resizer))
        })
}

#[cfg(windows)]
fn hlsl_filter_from_method(method: ResizeMethod) -> HlslFilterType {
    match method {
        ResizeMethod::Nearest | ResizeMethod::Bilinear => HlslFilterType::Bilinear,
        ResizeMethod::Bicubic => HlslFilterType::Bell,
        ResizeMethod::Lanczos3 => HlslFilterType::Lanczos3,
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
            )))
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

#[cfg(windows)]
fn hlsl_resize_bytes(
    src_data: &[u8],
    src_width: u32,
    src_height: u32,
    params: &ResizeParams,
    channel_count: u32,
) -> Result<Vec<u8>, ResizeError> {
    let resizer_lock = ensure_hlsl_resizer()?;
    let mut resizer = resizer_lock
        .lock()
        .map_err(|_| ResizeError::BackendError("HLSL resizer poisoned".to_string()))?;

    let mut hlsl_params =
        HlslResizeParameters::new(src_width, src_height, params.target_width, params.target_height);
    hlsl_params.filter = hlsl_filter_from_method(params.method);
    hlsl_params.border_value = params.border_value;
    hlsl_params.no_srgb = true;
    hlsl_params.channel_count = channel_count;

    let mut data = resizer
        .resize(src_data, &hlsl_params)
        .map_err(|e| ResizeError::BackendError(format!("HLSL resize failed: {e:?}")))?;

    if params.swap_rb && channel_count >= 3 {
        for px in data.chunks_exact_mut(channel_count as usize) {
            px.swap(0, 2);
        }
    }

    Ok(data)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wgpu_resize_bytes(
    src_data: &[u8],
    src_width: u32,
    src_height: u32,
    params: &ResizeParams,
    channel_count: u32,
) -> Result<Vec<u8>, ResizeError> {
    let resizer_lock = ensure_wgpu_resizer()?;
    let mut resizer = resizer_lock
        .lock()
        .map_err(|_| ResizeError::BackendError("WGPU resizer poisoned".to_string()))?;

    let mut wgpu_params =
        WgpuResizeParameters::new(src_width, src_height, params.target_width, params.target_height);
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

/// Resize image bytes using hardware acceleration when available.
/// On Windows, uses HLSL/DirectX 12 compute shaders for GPU acceleration with CPU fallback.
/// On Linux, uses CPU-based fast_image_resize (future: CUDA support).
pub fn resize_bytes(
    src_data: &[u8],
    src_width: u32,
    src_height: u32,
    params: &ResizeParams,
    channel_count: u32,
) -> Result<Vec<u8>, ResizeError> {
    #[cfg(windows)]
    {
        // Windows: Try HLSL GPU acceleration first, fall back to CPU if needed
        let gpu_memory_required = (src_width as u64 * src_height as u64 * channel_count as u64) +
                                 (params.target_width as u64 * params.target_height as u64 * channel_count as u64);

        match hlsl_resize_bytes(src_data, src_width, src_height, params, channel_count) {
            Ok(data) => {
                let count = GPU_RESIZE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

                #[cfg(feature = "debug-logging")]
                {
                    if count == 1 {
                        println!("HLSL GPU resize #1: {}x{} -> {}x{} ({} channels) - Hardware acceleration active!",
                            src_width, src_height, params.target_width, params.target_height, channel_count);
                    } else if count % 50 == 0 {
                        println!("HLSL GPU resize #{}: {}x{} -> {}x{} ({} channels)",
                            count, src_width, src_height, params.target_width, params.target_height, channel_count);
                    }
                }

                log::debug!(
                    "HLSL GPU resize successful: {}x{} -> {}x{} ({} channels)",
                    src_width, src_height, params.target_width, params.target_height, channel_count
                );
                return Ok(data);
            },
            Err(ResizeError::BackendError(ref msg)) if msg.contains("memory pressure") => {
                warn!(
                    "HLSL GPU resize failed due to memory pressure ({}x{} -> {}x{}, ~{} MB required); falling back to CPU",
                    src_width, src_height, params.target_width, params.target_height,
                    gpu_memory_required / (1024 * 1024)
                );
            },
            Err(ResizeError::BackendError(ref msg)) if msg.contains("ResourceAllocationFailed") => {
                warn!(
                    "HLSL GPU resize failed due to resource allocation ({}x{} -> {}x{}); falling back to CPU: {}",
                    src_width, src_height, params.target_width, params.target_height, msg
                );
            },
            Err(err) => {
                warn!(
                    "HLSL GPU resize failed ({}x{} -> {}x{}): {}; falling back to CPU",
                    src_width, src_height, params.target_width, params.target_height, err
                );
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        match wgpu_resize_bytes(src_data, src_width, src_height, params, channel_count) {
            Ok(data) => {
                let count = WGPU_RESIZE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

                #[cfg(feature = "debug-logging")]
                {
                    if count == 1 {
                        println!(
                            "WGPU resize #1: {}x{} -> {}x{} ({} channels) - Hardware acceleration active!",
                            src_width,
                            src_height,
                            params.target_width,
                            params.target_height,
                            channel_count
                        );
                    } else if count % 50 == 0 {
                        println!(
                            "WGPU resize #{}: {}x{} -> {}x{} ({} channels)",
                            count,
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
            }
        }
    }

    // CPU fallback (primary on Linux, fallback on Windows)
    cpu_resize_bytes(src_data, src_width, src_height, params, channel_count)
        .map_err(|e| {
            log::error!(
                "Resize failed for {}x{} -> {}x{}: {}",
                src_width, src_height, params.target_width, params.target_height, e
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
