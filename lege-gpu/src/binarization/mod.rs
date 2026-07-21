//! GPU binarization backends.
//!
//! The public API is intentionally small: callers pass grayscale bytes and receive the
//! same raw one-byte-per-pixel binary buffer as the CPU implementation.

use bytemuck::{Pod, Zeroable};
use once_cell::sync::OnceCell;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinarizationMode {
    FixedThreshold,
    Adaptive,
}

#[derive(Debug, Clone, Copy)]
pub struct AdaptiveBinarizeGpuConstants {
    pub sauvola_window: u32,
    pub bg_window: u32,
    pub percentile_c: u8,
    pub otsu_threshold: u8,
}

/// Canonical default Sauvola k-factor shared by Lege's binarization callers.
pub const DEFAULT_K_FACTOR: f32 = 0.05;

#[derive(Debug, Clone, Copy)]
pub struct BinarizationParams {
    pub width: u32,
    pub height: u32,
    pub mode: BinarizationMode,
    pub invert_output: bool,
    pub k_factor: f32,
    pub fixed_threshold: u8,
    pub adaptive: AdaptiveBinarizeGpuConstants,
    pub debug_mode: u32,
}

#[derive(Debug)]
pub enum GpuBinarizationError {
    InvalidDimensions { width: u32, height: u32 },
    BufferSizeMismatch { expected: usize, actual: usize },
    Unsupported(String),
    Initialization(String),
    Shader(String),
    Execution(String),
}

impl std::fmt::Display for GpuBinarizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDimensions { width, height } => {
                write!(f, "invalid dimensions: {}x{}", width, height)
            }
            Self::BufferSizeMismatch { expected, actual } => {
                write!(
                    f,
                    "buffer size mismatch: expected {}, got {}",
                    expected, actual
                )
            }
            Self::Unsupported(msg) => write!(f, "unsupported GPU binarization path: {}", msg),
            Self::Initialization(msg) => {
                write!(f, "GPU binarization initialization failed: {}", msg)
            }
            Self::Shader(msg) => write!(f, "GPU binarization shader failed: {}", msg),
            Self::Execution(msg) => write!(f, "GPU binarization execution failed: {}", msg),
        }
    }
}

impl std::error::Error for GpuBinarizationError {}

pub type Result<T> = std::result::Result<T, GpuBinarizationError>;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct BinarizeParamsStd140 {
    pub width: u32,
    pub height: u32,
    pub mode: u32,
    pub invert_output: u32,
    pub fixed_threshold: u32,
    pub sauvola_window: u32,
    pub bg_window: u32,
    pub otsu_threshold: u32,
    pub k_factor: f32,
    pub percentile_c: f32,
    pub padded_width: u32,
    pub padded_height: u32,
    pub integral_width: u32,
    pub sauvola_radius: u32,
    pub debug_mode: u32,
    pub _pad2: u32,
    pub _pad3: u32,
    pub _pad4: u32,
    pub _pad5: u32,
    pub _pad6: u32,
    pub _pad7: u32,
}

impl BinarizationParams {
    pub fn validate_gray(&self, gray: &[u8]) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(GpuBinarizationError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        let expected = self.width as usize * self.height as usize;
        if gray.len() != expected {
            return Err(GpuBinarizationError::BufferSizeMismatch {
                expected,
                actual: gray.len(),
            });
        }
        Ok(())
    }
}

pub mod wgpu;

type PlatformGpuBinarizer = wgpu::WgpuBinarizer;

static GPU_BINARIZER: OnceCell<Mutex<PlatformGpuBinarizer>> = OnceCell::new();

fn backend_is_cpu_forced() -> bool {
    if std::env::var("LEGE_FORCE_CPU_BINARIZATION")
        .map(|v| v != "0")
        .unwrap_or(false)
    {
        return true;
    }
    std::env::var("LEGE_BINARIZATION_BACKEND")
        .map(|v| v.eq_ignore_ascii_case("cpu"))
        .unwrap_or(false)
}

/// Try to binarize a batch of pages using the platform GPU backend.
///
/// Pages may have different sizes. Work buffers are sized to the largest page in
/// the batch; the readback buffer covers all pages. Only one GPU poll and one
/// buffer map are issued regardless of batch size.
///
/// Returns `None` if GPU is unavailable, disabled, or any page errors — caller
/// should fall back to CPU for the whole batch.
pub fn try_binarize_batch(pages: &[(&[u8], &BinarizationParams)]) -> Option<Vec<Vec<u8>>> {
    if backend_is_cpu_forced() {
        return None;
    }

    let binarizer_lock = match GPU_BINARIZER.get_or_try_init(|| {
        PlatformGpuBinarizer::new()
            .map(Mutex::new)
            .map_err(|e| e.to_string())
    }) {
        Ok(lock) => lock,
        Err(err) => {
            log::debug!("[GPU binarizer] initialization failed: {err}");
            return None;
        }
    };

    let mut binarizer = match binarizer_lock.lock() {
        Ok(guard) => guard,
        Err(_) => {
            log::debug!("[GPU binarizer] mutex poisoned; falling back to CPU");
            return None;
        }
    };

    match binarizer.binarize_batch(pages) {
        Ok(results) => {
            log::debug!("[GPU binarizer] batch completed {} pages", results.len());
            Some(results)
        }
        Err(err) => {
            log::debug!("[GPU binarizer] batch error, falling back to CPU: {err}");
            None
        }
    }
}

/// Try to binarize `gray` using the platform GPU backend.
/// Returns `None` if GPU is unavailable, disabled by env var, or encounters an error
/// — the caller should fall back to CPU.
pub fn try_binarize_gray(gray: &[u8], params: &BinarizationParams) -> Option<Vec<u8>> {
    if backend_is_cpu_forced() {
        return None;
    }

    let binarizer_lock = match GPU_BINARIZER.get_or_try_init(|| {
        PlatformGpuBinarizer::new()
            .map(Mutex::new)
            .map_err(|e| e.to_string())
    }) {
        Ok(lock) => lock,
        Err(err) => {
            log::debug!("[GPU binarizer] initialization failed: {err}");
            return None;
        }
    };

    let mut binarizer = match binarizer_lock.lock() {
        Ok(guard) => guard,
        Err(_) => {
            log::debug!("[GPU binarizer] mutex poisoned; falling back to CPU");
            return None;
        }
    };

    match binarizer.binarize_gray_raw(gray, params) {
        Ok(result) => {
            log::debug!(
                "[GPU binarizer] completed {}x{} -> {} bytes",
                params.width,
                params.height,
                result.len()
            );
            Some(result)
        }
        Err(err) => {
            log::debug!("[GPU binarizer] error, falling back to CPU: {err}");
            None
        }
    }
}

/// Try to binarize `gray` using the platform GPU backend, with a callback that receives
/// the mapped data directly, avoiding an extra copy.
/// Returns `None` if GPU is unavailable, disabled by env var, or encounters an error.
pub fn try_binarize_gray_with<F, R>(gray: &[u8], params: &BinarizationParams, f: F) -> Option<R>
where
    F: FnOnce(&[u8]) -> R,
{
    if backend_is_cpu_forced() {
        return None;
    }

    let binarizer_lock = match GPU_BINARIZER.get_or_try_init(|| {
        PlatformGpuBinarizer::new()
            .map(Mutex::new)
            .map_err(|e| e.to_string())
    }) {
        Ok(lock) => lock,
        Err(err) => {
            log::debug!("[GPU binarizer] initialization failed: {err}");
            return None;
        }
    };

    let mut binarizer = match binarizer_lock.lock() {
        Ok(guard) => guard,
        Err(_) => {
            log::debug!("[GPU binarizer] mutex poisoned; falling back to CPU");
            return None;
        }
    };

    match binarizer.binarize_gray_raw_with(gray, params, f) {
        Ok(result) => {
            log::debug!(
                "[GPU binarizer] completed {}x{} (with callback)",
                params.width,
                params.height,
            );
            Some(result)
        }
        Err(err) => {
            log::debug!("[GPU binarizer] error, falling back to CPU: {err}");
            None
        }
    }
}

/// Try to binarize from RGB bytes using the platform GPU backend, with a callback.
/// The GPU's linearize pre-pass applies sRGB→linear LUT + BT.709 luma + sRGB re-encode.
/// Returns `None` if GPU is unavailable, disabled, or errors.
pub fn try_binarize_rgb_with<F, R>(rgb: &[u8], params: &BinarizationParams, f: F) -> Option<R>
where
    F: FnOnce(&[u8]) -> R,
{
    if backend_is_cpu_forced() {
        return None;
    }
    let binarizer_lock = match GPU_BINARIZER.get_or_try_init(|| {
        PlatformGpuBinarizer::new()
            .map(Mutex::new)
            .map_err(|e| e.to_string())
    }) {
        Ok(lock) => lock,
        Err(err) => {
            log::debug!("[GPU binarizer] initialization failed: {err}");
            return None;
        }
    };
    let mut binarizer = match binarizer_lock.lock() {
        Ok(guard) => guard,
        Err(_) => {
            log::debug!("[GPU binarizer] mutex poisoned; falling back to CPU");
            return None;
        }
    };
    match binarizer.binarize_rgb_raw_with(rgb, params, f) {
        Ok(result) => Some(result),
        Err(err) => {
            log::debug!("[GPU binarizer] RGB error, falling back to CPU: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static GPU_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn gpu_test_guard() -> MutexGuard<'static, ()> {
        GPU_TEST_LOCK.lock().expect("GPU test lock poisoned")
    }

    #[test]
    fn gpu_fixed_threshold_smoke_when_enabled() {
        if std::env::var("LEGE_RUN_GPU_TESTS").ok().as_deref() != Some("1") {
            return;
        }
        let _gpu_guard = gpu_test_guard();

        let gray = vec![0u8, 255, 128, 32];
        let params = BinarizationParams {
            width: 2,
            height: 2,
            mode: BinarizationMode::FixedThreshold,
            invert_output: false,
            k_factor: DEFAULT_K_FACTOR,
            fixed_threshold: 50,
            adaptive: AdaptiveBinarizeGpuConstants {
                sauvola_window: 31,
                bg_window: 3,
                percentile_c: 0,
                otsu_threshold: 0,
            },
            debug_mode: 0,
        };

        let mut binarizer = wgpu::WgpuBinarizer::new().expect("WGPU binarizer");
        let out = binarizer
            .binarize_gray_raw(&gray, &params)
            .expect("GPU fixed threshold");
        assert_eq!(out, vec![0, 255, 255, 0]);
    }

    /// Checks that binarize_batch matches binarize_gray_raw on two same-size pages
    /// and two variable-size pages.
    #[test]
    fn wgpu_batch_matches_single_page() {
        if std::env::var("LEGE_RUN_GPU_TESTS").ok().as_deref() != Some("1") {
            return;
        }
        let _gpu_guard = gpu_test_guard();

        let make_params = |w: u32, h: u32| BinarizationParams {
            width: w,
            height: h,
            mode: BinarizationMode::Adaptive,
            invert_output: false,
            k_factor: DEFAULT_K_FACTOR,
            fixed_threshold: 128,
            adaptive: AdaptiveBinarizeGpuConstants {
                sauvola_window: 31,
                bg_window: 3,
                percentile_c: 128,
                otsu_threshold: 128,
            },
            debug_mode: 9, // all-white — deterministic output, easy to verify
        };

        // Two pages with different sizes.
        let (w0, h0) = (64u32, 48u32);
        let (w1, h1) = (80u32, 32u32);
        let gray0: Vec<u8> = (0..(w0 * h0) as usize).map(|i| (i % 256) as u8).collect();
        let gray1: Vec<u8> = (0..(w1 * h1) as usize)
            .map(|i| (255 - i % 256) as u8)
            .collect();
        let p0 = make_params(w0, h0);
        let p1 = make_params(w1, h1);

        let mut binarizer = wgpu::WgpuBinarizer::new().expect("WGPU binarizer");

        // Ground truth: run each page individually.
        let single0 = binarizer
            .binarize_gray_raw(&gray0, &p0)
            .expect("single page 0");
        let single1 = binarizer
            .binarize_gray_raw(&gray1, &p1)
            .expect("single page 1");

        // Batch: should produce identical results.
        let batch = binarizer
            .binarize_batch(&[(&gray0, &p0), (&gray1, &p1)])
            .expect("batch");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0], single0, "batch page 0 mismatch");
        assert_eq!(batch[1], single1, "batch page 1 mismatch");
    }

    /// Checks that the WGPU adaptive path produces an all-white image when debug_mode=9.
    /// This exercises pipeline creation, buffer allocation, and readback without any math.
    #[test]
    fn wgpu_adaptive_debug_all_white() {
        if std::env::var("LEGE_RUN_GPU_TESTS").ok().as_deref() != Some("1") {
            return;
        }
        let _gpu_guard = gpu_test_guard();

        let w = 64usize;
        let h = 64usize;
        let gray: Vec<u8> = (0..(w * h)).map(|i| (i % 256) as u8).collect();
        let params = BinarizationParams {
            width: w as u32,
            height: h as u32,
            mode: BinarizationMode::Adaptive,
            invert_output: false,
            k_factor: DEFAULT_K_FACTOR,
            fixed_threshold: 128,
            adaptive: AdaptiveBinarizeGpuConstants {
                sauvola_window: 31,
                bg_window: 3,
                percentile_c: 128,
                otsu_threshold: 128,
            },
            debug_mode: 9, // all-white
        };
        let mut binarizer = wgpu::WgpuBinarizer::new().expect("WGPU binarizer");
        let out = binarizer
            .binarize_gray_raw(&gray, &params)
            .expect("adaptive all-white");
        assert_eq!(out.len(), w * h, "output size mismatch");
        assert!(
            out.iter().all(|&v| v == 255),
            "expected all 255 for debug_mode=9 (all-white)"
        );
    }

    /// Checks that x-ramp debug mode returns x & 0xFF for each pixel (stride sanity).
    #[test]
    fn wgpu_adaptive_debug_x_ramp() {
        if std::env::var("LEGE_RUN_GPU_TESTS").ok().as_deref() != Some("1") {
            return;
        }
        let _gpu_guard = gpu_test_guard();

        let w = 64usize;
        let h = 32usize;
        let gray = vec![128u8; w * h];
        let params = BinarizationParams {
            width: w as u32,
            height: h as u32,
            mode: BinarizationMode::Adaptive,
            invert_output: false,
            k_factor: DEFAULT_K_FACTOR,
            fixed_threshold: 128,
            adaptive: AdaptiveBinarizeGpuConstants {
                sauvola_window: 31,
                bg_window: 3,
                percentile_c: 128,
                otsu_threshold: 128,
            },
            debug_mode: 7, // x ramp
        };
        let mut binarizer = wgpu::WgpuBinarizer::new().expect("WGPU binarizer");
        let out = binarizer
            .binarize_gray_raw(&gray, &params)
            .expect("x-ramp debug");
        assert_eq!(out.len(), w * h);
        for y in 0..h {
            for x in 0..w {
                let expected = (x & 255) as u8;
                let actual = out[y * w + x];
                assert_eq!(actual, expected, "x-ramp mismatch at ({x},{y})");
            }
        }
    }

    #[test]
    fn wgpu_callback_exposes_exact_pixel_count() {
        if std::env::var("LEGE_RUN_GPU_TESTS").ok().as_deref() != Some("1") {
            return;
        }
        let _gpu_guard = gpu_test_guard();

        let gray = vec![0u8, 128, 255];
        let params = BinarizationParams {
            width: 3,
            height: 1,
            mode: BinarizationMode::FixedThreshold,
            invert_output: false,
            k_factor: DEFAULT_K_FACTOR,
            fixed_threshold: 100,
            adaptive: AdaptiveBinarizeGpuConstants {
                sauvola_window: 31,
                bg_window: 3,
                percentile_c: 128,
                otsu_threshold: 128,
            },
            debug_mode: 0,
        };
        let mut binarizer = wgpu::WgpuBinarizer::new().expect("WGPU binarizer");
        let observed_len = binarizer
            .binarize_gray_raw_with(&gray, &params, |data| data.len())
            .expect("callback fixed threshold");
        assert_eq!(observed_len, gray.len());
    }

    #[test]
    fn wgpu_adaptive_handles_window_larger_than_dimension() {
        if std::env::var("LEGE_RUN_GPU_TESTS").ok().as_deref() != Some("1") {
            return;
        }
        let _gpu_guard = gpu_test_guard();

        let w = 3usize;
        let h = 1usize;
        let gray = vec![128u8; w * h];
        let params = BinarizationParams {
            width: w as u32,
            height: h as u32,
            mode: BinarizationMode::Adaptive,
            invert_output: false,
            k_factor: DEFAULT_K_FACTOR,
            fixed_threshold: 128,
            adaptive: AdaptiveBinarizeGpuConstants {
                sauvola_window: 31,
                bg_window: 31,
                percentile_c: 128,
                otsu_threshold: 128,
            },
            debug_mode: 9,
        };
        let mut binarizer = wgpu::WgpuBinarizer::new().expect("WGPU binarizer");
        let out = binarizer
            .binarize_gray_raw(&gray, &params)
            .expect("adaptive small dimension");
        assert_eq!(out, vec![255u8; w * h]);
    }
}
