use bytemuck::Pod;
use fast_image_resize::{
    Filter, FilterType as FirFilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
    images::Image as FirImage,
};
use image::{GrayImage, RgbImage};
use lege_gpu::resize::wgpu::{
    FilterType as WgpuFilterType, ResizeParameters as WgpuResizeParameters, WgpuResizer,
};
use log::warn;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
static WGPU_RESIZER_POOL: OnceLock<WgpuResizerPool> = OnceLock::new();
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
        let _previous_peak = self.peak_active.fetch_max(active, Ordering::Relaxed);
        #[cfg(feature = "debug-logging")]
        if active > _previous_peak {
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

/// Fallible one-time init of the GPU resizer pool.
///
/// `OnceLock` has no stable `get_or_try_init`, so this is the double-checked
/// equivalent: an uncontended hit is a single atomic load, and the init lock
/// guarantees at most one pool is ever built. A failed build leaves the cell
/// empty so a later call can retry — the same semantics the previous
/// `get_or_try_init` had.
fn wgpu_resizer_pool() -> Result<&'static WgpuResizerPool, ResizeError> {
    if let Some(pool) = WGPU_RESIZER_POOL.get() {
        return Ok(pool);
    }

    static INIT_LOCK: Mutex<()> = Mutex::new(());
    let _guard = match INIT_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    // Another thread may have finished initialising while we waited.
    if let Some(pool) = WGPU_RESIZER_POOL.get() {
        return Ok(pool);
    }

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

    let pool = WgpuResizerPool::new(pool_size)?;
    // We hold the init lock and just observed the cell empty, so this sets it.
    Ok(WGPU_RESIZER_POOL.get_or_init(|| pool))
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

    let dst_len = image_buffer_len(dst_width, dst_height, channel_count)?;
    let mut dst = vec![0u8; dst_len];
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

fn image_buffer_len(width: u32, height: u32, channel_count: u32) -> Result<usize, ResizeError> {
    if width == 0 || height == 0 || channel_count == 0 {
        return Err(ResizeError::InvalidDimensions);
    }

    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(channel_count as usize))
        .ok_or(ResizeError::InvalidDimensions)
}

// ---------------------------------------------------------------------------
// Typed CPU entry points
// ---------------------------------------------------------------------------
//
// These deliberately bypass `resize_bytes`' GPU dispatch and go straight to the
// `fast_image_resize` SIMD backend. Their callers resize small intermediate
// buffers — background-estimation downscales, per-item reflow crops (hundreds
// per page), debug rasters — where a GPU round-trip plus resizer-pool
// contention costs far more than the resample itself. The GPU path stays the
// right choice for whole-page output rasters, which still call `resize_bytes`.
//
// Both are infallible so call sites remain expression-shaped: if the SIMD
// backend rejects a request they fall back to `image`'s own resampler, which is
// what every one of these call sites used before. That keeps the fallback in
// one place instead of scattered across the pipeline.

/// `ResizeParams` for a plain scale-to-size, no letterboxing or channel swap.
fn plain_params(target_width: u32, target_height: u32, method: ResizeMethod) -> ResizeParams {
    ResizeParams {
        target_width,
        target_height,
        method,
        letterbox: false,
        border_value: 0.0,
        swap_rb: false,
    }
}

/// `image` resampler matching a [`ResizeMethod`], for the fallback path.
fn imageops_filter_from_method(method: ResizeMethod) -> image::imageops::FilterType {
    match method {
        ResizeMethod::Nearest => image::imageops::FilterType::Nearest,
        // `Bell` has no `image` equivalent; Triangle is the closest soft filter.
        ResizeMethod::Bilinear | ResizeMethod::Bell => image::imageops::FilterType::Triangle,
        ResizeMethod::Bicubic => image::imageops::FilterType::CatmullRom,
        ResizeMethod::Lanczos3 => image::imageops::FilterType::Lanczos3,
    }
}

/// Resize an RGB8 image on the SIMD CPU backend.
///
/// `width`/`height` are clamped to at least 1, so the result always has exactly
/// the requested dimensions.
pub fn resize_rgb_cpu(src: &RgbImage, width: u32, height: u32, method: ResizeMethod) -> RgbImage {
    let (width, height) = (width.max(1), height.max(1));
    if (src.width(), src.height()) == (width, height) {
        return src.clone();
    }

    let params = plain_params(width, height, method);
    let resized = cpu_resize_bytes(src.as_raw(), src.width(), src.height(), &params, 3)
        .ok()
        .and_then(|bytes| RgbImage::from_raw(width, height, bytes));

    match resized {
        Some(image) => image,
        None => {
            warn!(
                "SIMD RGB resize {}x{} -> {width}x{height} failed; falling back to image crate",
                src.width(),
                src.height()
            );
            image::imageops::resize(src, width, height, imageops_filter_from_method(method))
        }
    }
}

/// Resize an 8-bit grayscale image on the SIMD CPU backend.
///
/// `width`/`height` are clamped to at least 1, so the result always has exactly
/// the requested dimensions.
pub fn resize_gray_cpu(
    src: &GrayImage,
    width: u32,
    height: u32,
    method: ResizeMethod,
) -> GrayImage {
    let (width, height) = (width.max(1), height.max(1));
    if (src.width(), src.height()) == (width, height) {
        return src.clone();
    }

    let params = plain_params(width, height, method);
    let resized = cpu_resize_bytes(src.as_raw(), src.width(), src.height(), &params, 1)
        .ok()
        .and_then(|bytes| GrayImage::from_raw(width, height, bytes));

    match resized {
        Some(image) => image,
        None => {
            warn!(
                "SIMD gray resize {}x{} -> {width}x{height} failed; falling back to image crate",
                src.width(),
                src.height()
            );
            image::imageops::resize(src, width, height, imageops_filter_from_method(method))
        }
    }
}

/// Resize image bytes using WGPU acceleration when available, with CPU fallback.
pub fn resize_bytes(
    src_data: &[u8],
    src_width: u32,
    src_height: u32,
    params: &ResizeParams,
    channel_count: u32,
) -> Result<Vec<u8>, ResizeError> {
    let expected_source_len = image_buffer_len(src_width, src_height, channel_count)?;
    image_buffer_len(params.target_width, params.target_height, channel_count)?;
    if src_data.len() < expected_source_len {
        return Err(ResizeError::BackendError(format!(
            "Source buffer is too small: expected at least {expected_source_len} bytes, got {}",
            src_data.len()
        )));
    }

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

    #[test]
    fn resize_bytes_rejects_short_source_before_backend_selection() {
        let params = ResizeParams {
            target_width: 1,
            target_height: 1,
            method: ResizeMethod::Nearest,
            letterbox: false,
            border_value: 0.0,
            swap_rb: false,
        };

        let error = resize_bytes(&[0; 11], 2, 2, &params, 3).expect_err("short source");
        assert!(matches!(error, ResizeError::BackendError(_)));
    }

    #[test]
    fn resize_bytes_rejects_zero_sized_images() {
        let params = ResizeParams {
            target_width: 1,
            target_height: 1,
            method: ResizeMethod::Nearest,
            letterbox: false,
            border_value: 0.0,
            swap_rb: false,
        };

        assert!(matches!(
            resize_bytes(&[], 0, 1, &params, 3),
            Err(ResizeError::InvalidDimensions)
        ));
    }
}

#[cfg(test)]
mod typed_cpu_resize_tests {
    use super::*;

    fn rgb(width: u32, height: u32, fill: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(width, height, image::Rgb(fill))
    }

    fn gray(width: u32, height: u32, fill: u8) -> GrayImage {
        GrayImage::from_pixel(width, height, image::Luma([fill]))
    }

    /// These wrappers replaced `image::imageops::resize` at call sites that
    /// depend on getting back exactly the dimensions they asked for.
    #[test]
    fn rgb_downscale_and_upscale_hit_the_requested_size() {
        let src = rgb(64, 40, [10, 20, 30]);

        let down = resize_rgb_cpu(&src, 16, 10, ResizeMethod::Bilinear);
        assert_eq!((down.width(), down.height()), (16, 10));

        let up = resize_rgb_cpu(&src, 128, 80, ResizeMethod::Lanczos3);
        assert_eq!((up.width(), up.height()), (128, 80));
    }

    #[test]
    fn gray_downscale_and_upscale_hit_the_requested_size() {
        let src = gray(64, 40, 128);

        let down = resize_gray_cpu(&src, 8, 5, ResizeMethod::Bilinear);
        assert_eq!((down.width(), down.height()), (8, 5));
        assert_eq!(down.as_raw().len(), 8 * 5);

        let up = resize_gray_cpu(&src, 100, 100, ResizeMethod::Bilinear);
        assert_eq!((up.width(), up.height()), (100, 100));
        assert_eq!(up.as_raw().len(), 100 * 100);
    }

    /// The call sites in `reflow_pipeline` used to guard this case with an
    /// explicit dimension check; that guard is now folded into the wrapper.
    #[test]
    fn matching_dimensions_short_circuit_to_a_copy() {
        let src = rgb(9, 7, [1, 2, 3]);
        let out = resize_rgb_cpu(&src, 9, 7, ResizeMethod::Bilinear);
        assert_eq!(out.as_raw(), src.as_raw(), "expected an unmodified copy");

        let src_gray = gray(9, 7, 200);
        let out_gray = resize_gray_cpu(&src_gray, 9, 7, ResizeMethod::Bilinear);
        assert_eq!(out_gray.as_raw(), src_gray.as_raw());
    }

    /// Zero is clamped to 1 rather than producing an empty buffer, so callers
    /// passing a degenerate rect still get a usable image back.
    #[test]
    fn zero_dimensions_are_clamped_to_one() {
        let src = gray(16, 16, 77);
        let out = resize_gray_cpu(&src, 0, 0, ResizeMethod::Bilinear);
        assert_eq!((out.width(), out.height()), (1, 1));

        let src_rgb = rgb(16, 16, [4, 5, 6]);
        let out_rgb = resize_rgb_cpu(&src_rgb, 0, 4, ResizeMethod::Bilinear);
        assert_eq!((out_rgb.width(), out_rgb.height()), (1, 4));
    }

    /// A flat image must stay flat through the resampler — this is what the
    /// background-estimation path in `clean_gray` relies on.
    #[test]
    fn flat_input_stays_flat() {
        let src = gray(50, 50, 210);
        let out = resize_gray_cpu(&src, 12, 12, ResizeMethod::Bilinear);
        assert!(
            out.as_raw().iter().all(|&v| v == 210),
            "flat 210 input produced varying output"
        );
    }
}
