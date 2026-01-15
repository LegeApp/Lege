// djvu.rs
//! DJVU encoding orchestration module
//!
//! This module centralizes all logic for DJVU document creation:
//! - Exports page data (binarized text as PBM, images as PPM)
//! - Exports layout detection coordinates as JSON
//! - Calls djvulibre executables directly: cjb2, c44, djvumake, djvm
//!
//! The workflow:
//! 1. For each page, export binarized background as PBM
//! 2. Export detected image regions as uncompressed PPM with coordinates
//! 3. Encode PBM with cjb2 (JB2 bilevel compression)
//! 4. Encode PPM images with c44 (IW44 wavelet compression)
//! 5. Assemble background and images into single pages with djvumake
//! 6. Combine pages into multi-page document with djvm
//!
//! Required djvulibre executables (should be bundled or in PATH):
//! - cjb2: JB2 encoder for bilevel images
//! - c44: IW44 encoder for color/grayscale images
//! - djvumake: Page and document assembler
//! - djvm: Multi-page document assembler

#![allow(unused_variables, unused_imports)]

use anyhow::{anyhow, Context, Result};
use image::{ImageBuffer, Rgb};
use serde::{Deserialize, Serialize};
// For Unicode normalization of OCR text
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use unicode_normalization::UnicodeNormalization;
use wait_timeout::ChildExt;

#[cfg(windows)] use std::os::windows::process::CommandExt; // for creation_flags
#[cfg(windows)] const CREATE_NO_WINDOW: u32 = 0x08000000; // prevents console window

const CJB2_TIMEOUT: Duration = Duration::from_secs(45);
const CJB2_RETRY_TIMEOUT: Duration = Duration::from_secs(60);
const DJVUEXTRACT_TIMEOUT: Duration = Duration::from_secs(30);
const DJVUDUMP_TIMEOUT: Duration = Duration::from_secs(20);
const DJVUMAKE_TIMEOUT: Duration = Duration::from_secs(45);
const DJVUMAKE_FALLBACK_TIMEOUT: Duration = Duration::from_secs(30);
const DJVM_TIMEOUT: Duration = Duration::from_secs(60);
const COMMAND_RESPAWN_DELAY_MS: u64 = 200;
const COMMAND_MAX_LOCK_ATTEMPTS: usize = 50;

struct BundleLock {
    path: PathBuf,
}

impl BundleLock {
    fn acquire(output: &Path) -> Result<Self> {
        let mut lock_path = output.to_path_buf();
        lock_path.set_extension("lock");
        for _ in 0..COMMAND_MAX_LOCK_ATTEMPTS {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "pid={}", std::process::id());
                    return Ok(Self { path: lock_path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(COMMAND_RESPAWN_DELAY_MS));
                }
                Err(e) => {
                    return Err(anyhow!(
                        "Failed to create bundling lock {:?}: {}",
                        lock_path,
                        e
                    ))
                }
            }
        }
        Err(anyhow!(
            "Timed out waiting for DJVU bundle lock {:?}",
            lock_path
        ))
    }
}

impl Drop for BundleLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Calculate optimal number of worker threads for DJVU processing based on available RAM and page dimensions
/// 
/// # Arguments
/// * `page_width` - Width of the page in pixels
/// * `page_height` - Height of the page in pixels
/// * `page_count` - Total number of pages to process
/// * `has_color` - Whether the pages contain color content
/// 
/// Returns: (num_threads, estimated_memory_per_thread_mb)
fn calculate_djvu_worker_threads(
    page_width: u32,
    page_height: u32,
    page_count: usize,
    has_color: bool
) -> (usize, usize) {
    // Constants for memory estimation (in bytes)
    const BYTES_PER_PIXEL_RGB: usize = 3;  // 3 bytes per pixel for RGB
    const BYTES_PER_PIXEL_GRAY: usize = 1; // 1 byte per pixel for grayscale
    const MEMORY_OVERHEAD_MB: usize = 200; // Base overhead per thread in MB
    
    // Get total system RAM in MB
    let total_ram_mb = crate::get_available_ram_gb() * 1024;
    
    // Calculate memory needed for one page
    let pixels_per_page = page_width as usize * page_height as usize;
    let bytes_per_page = if has_color {
        pixels_per_page * BYTES_PER_PIXEL_RGB
    } else {
        pixels_per_page * BYTES_PER_PIXEL_GRAY
    };
    
    // Estimate memory per thread (in MB)
    let memory_per_thread_mb = (bytes_per_page * 4) / (1024 * 1024) + MEMORY_OVERHEAD_MB;
    
    // Calculate maximum possible threads based on memory (leave 25% for system)
    let max_threads_by_memory = (total_ram_mb * 3 / 4) / memory_per_thread_mb.max(1);
    
    // Get physical CPU count (not hyperthreaded logical cores)
    // Physical cores perform better for CPU-intensive DJVU encoding
    let physical_cores = num_cpus::get_physical();

    // Calculate optimal thread count
    let thread_count = max_threads_by_memory
        .min(physical_cores)
        .min(page_count)
        .max(1);
    
    dbglog!(
        "DJVU Worker Threads: RAM={}MB, PageSize={}x{}, Pages={}, EstMem/Thread={}MB, Threads={}",
        total_ram_mb,
        page_width,
        page_height,
        page_count,
        memory_per_thread_mb,
        thread_count
    );
    
    (thread_count, memory_per_thread_mb)
}

use crate::app_dirs;
use crate::engine::Detection;
use crate::types::{LabelClassifier, LABEL_CLASSIFIER};
use crate::progress::ProcessingStatus;
// Bring dbglog! macro into scope (exported from crate root via debug_log module)
use crate::dbglog;

/// Configuration for DJVU encoding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DjvuConfig {
    /// DPI setting for output (default: 300)
    pub dpi: u32,
    /// Use clean encoding for bilevel (removes flyspecks)
    pub clean: bool,
    /// Lossy compression level for bilevel (0-200, None for lossless)
    pub lossy: Option<u32>,
    /// IW44 quality in decibels (25-38 recommended for ebooks, higher = better quality)
    pub iw44_decibels: u32,
    /// Working directory for intermediate files
    pub work_dir: PathBuf,
    /// Assemble single-page .djvu files immediately when each page is processed (reused at final bundling)
    pub early_page_assembly: bool,
    /// When true, apply PBM (binarized background) as a transparency stencil when composing
    /// the BG44 color layer: only pixels that are white (255) in the PBM are copied from
    /// detected color regions. This implements the documented workflow in
    /// `DJVU_COLOR_MASKING_SOLUTION.md` where the JB2 mask supplies opaque (text) vs
    /// transparent (background/image) regions. For pages with no image regions this is
    /// automatically skipped (no extra cost, no BG44 layer created). Default: true.
    pub pre_mask_color_layer: bool,
    /// New: if true we enforce a document-wide uniform canvas (center margins)
    pub center_margins: bool,
    /// New: crop margins mode (tight crop to content) mutually exclusive with center in CLI precedence
    pub crop_margins: bool,
    /// If true, process in no-binarization mode (RGB image as IW44, no JB2 mask)
    pub no_binarization_mode: bool,
}

impl Default for DjvuConfig {
    fn default() -> Self {
        Self {
            dpi: 300,
            clean: true,
            lossy: None,
            iw44_decibels: 32,
            work_dir: app_dirs::djvu_base_dir(),
            // Default now false: assemble only at final document stage for determinism
            early_page_assembly: false,
            pre_mask_color_layer: true,
            center_margins: false,
            crop_margins: false,
            no_binarization_mode: false,
        }
    }
}

/// Decode an encoded image (e.g., JPEG/PNG/WebP) to RGB and write as PPM P6.
/// Returns (width, height) on success.
pub fn write_ppm_from_encoded_bytes(encoded: &[u8], out_path: &Path) -> Result<(u32, u32)> {
    // Decode using the image crate
    let dyn_img = image::load_from_memory(encoded)
        .with_context(|| "Failed to decode region image bytes for PPM export")?;
    let rgb = dyn_img.to_rgb8();
    let (w, h) = (rgb.width(), rgb.height());

    // Build PPM P6 buffer
    let header = format!("P6\n{} {}\n255\n", w, h);
    let mut ppm_bytes = Vec::with_capacity(header.len() + (w as usize * h as usize * 3));
    ppm_bytes.extend_from_slice(header.as_bytes());
    ppm_bytes.extend_from_slice(rgb.as_raw());

    // Write to disk
    std::fs::write(out_path, &ppm_bytes)
        .with_context(|| format!("Failed to write PPM file: {:?}", out_path))?;
    Ok((w, h))
}

/// Write a blank (all-white) PBM P4 image of the given size.
/// Useful as a fallback when binarized data is unavailable.
pub fn write_blank_pbm_p4(out_path: &Path, width: u32, height: u32) -> Result<()> {
    let mut file = File::create(out_path)
        .with_context(|| format!("Failed to create PBM file: {:?}", out_path))?;
    file.write_all(format!("P4\n{} {}\n", width, height).as_bytes())
        .with_context(|| format!("Failed to write PBM header: {:?}", out_path))?;
    let bytes_per_row = ((width + 7) / 8) as usize;
    let white = vec![0u8; bytes_per_row * height as usize]; // PBM bit=0 => white
    file.write_all(&white)
        .with_context(|| format!("Failed to write PBM pixel data: {:?}", out_path))?;
    Ok(())
}

/// Write PBM P4 from a binarized buffer that may be 0/255 grayscale or 0/1 binary.
/// Interprets values < 128 as black for 8-bit, and 1 as black for 1-bit-like inputs.
pub fn write_pbm_p4_from_binary(
    binarized: &[u8],
    width: u32,
    height: u32,
    out_path: &Path,
) -> Result<()> {
    let w = width as usize;
    let h = height as usize;
    anyhow::ensure!(binarized.len() >= w * h, "Binarized buffer too small: {} < {}", binarized.len(), w * h);

    let mut file = File::create(out_path)
        .with_context(|| format!("Failed to create PBM file: {:?}", out_path))?;
    file.write_all(format!("P4\n{} {}\n", width, height).as_bytes())
        .with_context(|| format!("Failed to write PBM header: {:?}", out_path))?;

    let bytes_per_row = (w + 7) / 8;
    let mut row = vec![0u8; bytes_per_row];
    for y in 0..h {
        row.fill(0);
        let base = y * w;
        for x in 0..w {
            let v = binarized[base + x];
            let is_black = if v <= 1 { v == 0 } else { v < 128 };
            if is_black {
                row[x >> 3] |= 1 << (7 - (x & 7)); // PBM: 1 bit = black
            }
        }
        file.write_all(&row)
            .with_context(|| format!("Failed to write PBM row {} to {:?}", y, out_path))?;
    }
    Ok(())
}

/// Represents a detected image region with its location and image data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRegion {
    /// Bounding box [x1, y1, x2, y2] in page coordinates
    pub bbox: [f32; 4],
    /// Class ID from detection model
    pub class_id: i32,
    /// Class name (e.g., "image", "chart")
    pub class_name: String,
    /// Confidence score
    pub confidence: f32,
    /// Width of the extracted image
    pub width: u32,
    /// Height of the extracted image
    pub height: u32,
    /// File path to the exported PPM image
    pub image_path: String,
}

/// Metadata for a single page in DJVU format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageMetadata {
    /// Page number (0-indexed)
    pub page_number: usize,
    /// Page width in pixels
    pub width: u32,
    /// Page height in pixels
    pub height: u32,
    /// DPI setting
    pub dpi: u32,
    /// Path to binarized background PBM file
    pub background_path: String,
    /// List of image regions detected on this page
    pub image_regions: Vec<ImageRegion>,
    /// Optional path to DjVu text layer S-expression for this page
    pub text_layer_path: Option<String>,
}

// Removed ColorLayerArtifacts struct - compose_color_background now returns PathBuf directly

/// Data for a single page being processed
pub struct PageData {
    /// Page index (0-based)
    pub index: usize,
    /// High-resolution RGB image
    pub rgb_image: ImageBuffer<Rgb<u8>, Vec<u8>>,
    /// Binarized image data (0 or 255)
    pub binarized: Vec<u8>,
    /// Detected regions from layout detection
    pub detections: Vec<Detection>,
    /// Optional raw HOCR text (already scaled to page coordinates) if OCR enabled
    pub hocr: Option<String>,
}

/// Lightweight page tracker - only stores essential info to avoid memory accumulation
#[derive(Debug, Clone)]
struct AssembledPageInfo {
    page_number: usize,
    page_file: PathBuf,
}

/// Main DJVU orchestrator
pub struct DjvuOrchestrator {
    config: DjvuConfig,
    // Changed from Vec<PageMetadata> to lightweight tracker
    // This avoids accumulating full page data (images, detections, etc.) in memory
    // Wrapped in Mutex to allow parallel page processing without locking entire orchestrator
    assembled_pages: std::sync::Mutex<Vec<AssembledPageInfo>>,
    // Made thread-safe to fix concurrent access bug during parallel page processing
    tool_cache: std::sync::RwLock<std::collections::HashMap<String, OsString>>,
    pages_dir: PathBuf, // Directory where assembled page DJVU files are stored
}

impl DjvuOrchestrator {
    /// Remove only this run's working directory. This is a fast, safe cleanup
    /// that avoids scanning the global bin folder. Use this for CLI runs to
    /// prevent long startup delays when many old temp folders exist.
    pub fn cleanup_work_dir_only(&self) -> Result<()> {
        dbglog!(
            "[djvu] cleanup_work_dir_only() called, work_dir={:?}, exists={}",
            self.config.work_dir,
            self.config.work_dir.exists()
        );

        if self.config.work_dir.exists() {
            std::fs::remove_dir_all(&self.config.work_dir).with_context(|| {
                format!(
                    "Failed to remove work directory: {:?}",
                    self.config.work_dir
                )
            })?;
        }
        Ok(())
    }
    /// Determine if we should apply PBM-based masking for the BG44 color layer on this page.
    /// Masking (pre-masking the composite + supplying -mask to c44) only has value when there
    /// are actual image regions. For text-only pages we skip all color work entirely.
    ///
    /// Returns true only if the global config wants pre-masking AND the page has >=1 image region.
    fn should_use_color_mask(&self, page_meta: &PageMetadata) -> bool {
        self.config.pre_mask_color_layer && !page_meta.image_regions.is_empty()
    }
    fn run_tool_with_timeout(
        &self,
        mut cmd: Command,
        tracker: Option<&crate::progress::ProgressTracker>,
        context: &str,
        timeout: Duration,
        log_path: Option<&Path>,
    ) -> Result<std::process::Output> {
        #[cfg(windows)]
        {
            let prog = PathBuf::from(cmd.get_program());
            if prog.components().count() > 1 {
                if let Some(tool_dir) = prog.parent() {
                    let mut new_path = OsString::new();
                    new_path.push(tool_dir.as_os_str());

                    let djvu_sub = tool_dir.join("djvulibre");
                    if djvu_sub.is_dir() {
                        new_path.push(";");
                        new_path.push(djvu_sub.as_os_str());
                    }

                    if let Some(old) = std::env::var_os("PATH") {
                        new_path.push(";");
                        new_path.push(old);
                    }
                    cmd.env("PATH", new_path);
                }
            }
        }

        if cmd.get_current_dir().is_none() {
            if let Ok(cwd) = std::env::current_dir() {
                cmd.current_dir(cwd);
            }
        }

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        #[cfg(windows)]
        {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut log_handle: Option<File> = match log_path {
            Some(p) => match OpenOptions::new().create(true).append(true).open(p) {
                Ok(file) => Some(file),
                Err(e) => {
                    dbglog!(
                        "[extcmd] {} unable to open log {:?}: {}",
                        context,
                        p,
                        e
                    );
                    None
                }
            },
            None => None,
        };

        // Emit a header into the bundle log so it is never empty when enabled
        if let Some(log) = log_handle.as_mut() {
            let args = cmd.get_args().collect::<Vec<_>>();
            let _ = writeln!(
                log,
                "[{}:exec] program={:?} args={:?} cwd={:?}",
                context,
                cmd.get_program(),
                args,
                cmd.get_current_dir()
            );
        }

        dbglog!(
            "[extcmd] {} exec {:?} {:?} cwd={:?}",
            context,
            cmd.get_program(),
            cmd.get_args().collect::<Vec<_>>(),
            cmd.get_current_dir()
        );

        let start = Instant::now();
        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn {}", context))?;

        let status_opt = child
            .wait_timeout(timeout)
            .with_context(|| format!("Failed waiting for {}", context))?;

        if status_opt.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("{} timed out after {:?}", context, timeout));
        }

        let output = child
            .wait_with_output()
            .with_context(|| format!("Failed collecting output for {}", context))?;
        let duration = start.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        for (label, stream) in [("stdout", &stdout), ("stderr", &stderr)] {
            for line in stream.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                println!("{}: {}", context, trimmed);
                if let Some(t) = tracker {
                    t.update(ProcessingStatus::ExternalToolLine {
                        line: trimmed.to_string(),
                    });
                }
                if let Some(log) = log_handle.as_mut() {
                    let _ = writeln!(log, "[{}:{}] {}", context, label, trimmed);
                }
            }
        }

        if !output.status.success() {
            dbglog!(
                "[extcmd] {} failed status={:?} elapsed_ms={} stderr200='{}' stdout200='{}'",
                context,
                output.status.code(),
                duration.as_millis(),
                stderr.chars().take(200).collect::<String>(),
                stdout.chars().take(200).collect::<String>()
            );
            if let Some(log) = log_handle.as_mut() {
                let _ = writeln!(
                    log,
                    "[{}:fail] status={:?} elapsed_ms={} stderr200='{}' stdout200='{}'",
                    context,
                    output.status.code(),
                    duration.as_millis(),
                    stderr.chars().take(200).collect::<String>(),
                    stdout.chars().take(200).collect::<String>()
                );
            }
            return Err(anyhow!(
                "{} failed (status {:?})",
                context,
                output.status.code()
            ));
        }

        dbglog!(
            "[extcmd] {} succeeded in {}ms",
            context,
            duration.as_millis()
        );
        if let Some(log) = log_handle.as_mut() {
            let _ = writeln!(log, "[{}:ok] elapsed_ms={}", context, duration.as_millis());
        }

        Ok(output)
    }

    fn default_log_for_dir(&self, dir: &Path) -> Option<PathBuf> {
        if dir.exists() {
            Some(dir.join("page.log"))
        } else {
            None
        }
    }

    fn validate_pbm(&self, pbm_path: &Path, expected_w: u32, expected_h: u32) -> Result<()> {
        let data = fs::read(pbm_path)
            .with_context(|| format!("Failed to read PBM for validation: {:?}", pbm_path))?;
        if data.len() < 3 || &data[0..2] != b"P4" {
            return Err(anyhow!(
                "PBM {:?} has invalid magic. Expected P4 header.",
                pbm_path
            ));
        }
        let mut idx = 2;
        let len = data.len();

        let skip_ws = |i: &mut usize| {
            while *i < len && data[*i].is_ascii_whitespace() {
                *i += 1;
            }
        };
        let skip_comments = |i: &mut usize| {
            while *i < len && data[*i] == b'#' {
                while *i < len && data[*i] != b'\n' {
                    *i += 1;
                }
                if *i < len {
                    *i += 1;
                }
            }
        };
        let parse_number = |i: &mut usize| -> Result<u32> {
            skip_ws(i);
            skip_comments(i);
            skip_ws(i);
            if *i >= len {
                return Err(anyhow!("PBM {:?} header truncated", pbm_path));
            }
            let start = *i;
            while *i < len && data[*i].is_ascii_digit() {
                *i += 1;
            }
            if start == *i {
                return Err(anyhow!("PBM {:?} missing header number", pbm_path));
            }
            let num = std::str::from_utf8(&data[start..*i])?.parse::<u32>()?;
            Ok(num)
        };

        let width = parse_number(&mut idx)?;
        let height = parse_number(&mut idx)?;
        if width != expected_w || height != expected_h {
            return Err(anyhow!(
                "PBM {:?} dimensions mismatch (expected {}x{}, got {}x{})",
                pbm_path,
                expected_w,
                expected_h,
                width,
                height
            ));
        }

        skip_ws(&mut idx);
        skip_comments(&mut idx);
        skip_ws(&mut idx);

        if idx >= len {
            return Err(anyhow!(
                "PBM {:?} missing raster after header",
                pbm_path
            ));
        }

        let expected_stride = ((width + 7) / 8) as usize;
        let expected_bytes = expected_stride
            .checked_mul(height as usize)
            .ok_or_else(|| anyhow!("PBM {:?} size overflow", pbm_path))?;
        let actual_bytes = len - idx;
        if actual_bytes != expected_bytes {
            return Err(anyhow!(
                "PBM {:?} payload size mismatch (expected {} bytes, found {})",
                pbm_path,
                expected_bytes,
                actual_bytes
            ));
        }

        Ok(())
    }

    fn djvudump_check(
        &self,
        target: &Path,
        context: &str,
        required_markers: &[&str],
        expected_dims: Option<(u32, u32)>,
        log_path: Option<&Path>,
    ) -> Result<String> {
        let djvudump_path = self.resolve_executable("djvudump")?;
        let mut cmd = Command::new(djvudump_path);
        cmd.arg(target);
        if let Some(dir) = target.parent() {
            cmd.current_dir(dir);
        }

        let output = self.run_tool_with_timeout(
            cmd,
            None,
            context,
            DJVUDUMP_TIMEOUT,
            log_path,
        )?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        for marker in required_markers {
            if !stdout.contains(marker) {
                return Err(anyhow!(
                    "{} missing required marker '{}' in djvudump output",
                    context,
                    marker
                ));
            }
        }

        if let Some((expected_w, expected_h)) = expected_dims {
            // Robustly parse lines like:
            // "INFO [10]         DjVu 830x1200, v24, 300 dpi, gamma=2.2"
            // Extract the "DjVu WxH" token and compare to expected dims.
            let mut dims_ok = false;
            for line in stdout.lines() {
                let trimmed = line.trim();
                // Look for the "DjVu " marker and parse the following WxH token
                if let Some(idx) = trimmed.find("DjVu ") {
                    let after = &trimmed[idx + 5..]; // skip "DjVu "
                    if let Some(first_token) = after.split_whitespace().next() {
                        // Remove trailing comma if present
                        let token = first_token.trim_end_matches(',');
                        if let Some((w_str, h_str)) = token.split_once('x') {
                            if let (Ok(w), Ok(h)) = (w_str.parse::<u32>(), h_str.parse::<u32>()) {
                                if w == expected_w && h == expected_h {
                                    dims_ok = true;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            if !dims_ok {
                return Err(anyhow!(
                    "{} INFO chunk does not report expected dimensions {}x{}",
                    context,
                    expected_w,
                    expected_h
                ));
            }
        }

        Ok(stdout)
    }
    /// Create a new DJVU orchestrator with the given configuration
    pub fn new(config: DjvuConfig) -> Result<Self> {
        // Create working directory if it doesn't exist
        fs::create_dir_all(&config.work_dir)
            .with_context(|| format!("Failed to create work directory: {:?}", config.work_dir))?;

        // Create pages directory for assembled page DJVU files
        let pages_dir = config.work_dir.join("pages");
        fs::create_dir_all(&pages_dir)
            .with_context(|| format!("Failed to create pages directory: {:?}", pages_dir))?;

        Ok(Self {
            config,
            assembled_pages: std::sync::Mutex::new(Vec::new()),
            tool_cache: std::sync::RwLock::new(std::collections::HashMap::new()),
            pages_dir,
        })
    }
    /// Process a single page and export all necessary files
    /// Thread-safe: can be called from multiple threads simultaneously
    pub fn process_page(&self, page_data: PageData) -> Result<()> {
        let page_index = page_data.index;

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] process_page START: page {}", page_index);

        let result = self.export_page_assets(page_data);

        #[cfg(feature = "debug-logging")]
        match &result {
            Ok(_) => crate::success_log!("[DJVU] process_page COMPLETE: page {}", page_index),
            Err(e) => crate::error_log!("[DJVU] process_page FAILED: page {} - {}", page_index, e),
        }

        result
    }

    fn export_page_assets(&self, page_data: PageData) -> Result<()> {
        let page_num = page_data.index;
        let width = page_data.rgb_image.width();
        let height = page_data.rgb_image.height();

        // Create page-specific directory inside the run's work_dir to avoid
        // cross-run mixing and heavy global cleanups
        let page_dir = self.config.work_dir.join(format!("page_{:04}", page_num));
        fs::create_dir_all(&page_dir)
            .with_context(|| format!("Failed to create page directory: {:?}", page_dir))?;

        // For "jpeg" text format (no binarization mode), create blank PBM background and process RGB image as IW44
        let pbm_path = page_dir.join("background.pbm");

        if self.config.no_binarization_mode {
            // In no-binarization mode: create blank PBM and use full RGB image as IW44
            write_blank_pbm_p4(&pbm_path, width, height)?;
        } else {
            // Normal mode: export binarized PBM background
            self.export_pbm(&page_data.binarized, width as usize, height as usize, &pbm_path)?;
        }
        let background_path = pbm_path.to_string_lossy().to_string();

        // Extract image-type regions using centralized LabelClassifier logic identical to main pipeline
        let mut image_regions = Vec::new();
        let classifier = &crate::types::LABEL_CLASSIFIER;

        if self.config.no_binarization_mode {
            // In no-binarization mode: treat the entire page as one IW44 image, no text layer
            let ppm_path = page_dir.join("full_page.ppm");
            self.export_ppm(&page_data.rgb_image, &ppm_path)?;
            image_regions.push(ImageRegion {
                bbox: [0.0, 0.0, width as f32, height as f32], // Full page
                class_id: 1, // Image class
                class_name: "page".to_string(),
                confidence: 1.0,
                width,
                height,
                image_path: ppm_path.to_string_lossy().to_string(),
            });
            dbglog!("[djvu] page {} in NO-BINARIZATION mode - full page as single IW44 image", page_num);
        } else {
            // Normal mode: process detected regions separately
            let mut per_class_counts: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();
            for d in &page_data.detections {
                *per_class_counts.entry(d.class_id).or_insert(0) += 1;
            }
            for detection in &page_data.detections {
                if !classifier.is_image_label(detection) {
                    continue;
                }
                // Detections are expected to already be in page-space coordinates before this step.
                let bbox = detection.bbox;
                let x1 = bbox[0].floor().max(0.0) as u32;
                let y1 = bbox[1].floor().max(0.0) as u32;
                let x2 = bbox[2].ceil().min(width as f32) as u32;
                let y2 = bbox[3].ceil().min(height as f32) as u32;
                if x2 <= x1 || y2 <= y1 {
                    continue;
                }
                let region_width = x2 - x1;
                let region_height = y2 - y1;
                if region_width < 4 || region_height < 4 {
                    continue;
                }
                let sub_image =
                    self.extract_subimage(&page_data.rgb_image, x1, y1, region_width, region_height)?;
                let ppm_path = page_dir.join(format!("image_{:03}.ppm", image_regions.len()));
                self.export_ppm(&sub_image, &ppm_path)?;
                image_regions.push(ImageRegion {
                    bbox: [x1 as f32, y1 as f32, x2 as f32, y2 as f32],
                    class_id: detection.class_id,
                    class_name: detection
                        .class_name
                        .clone()
                        .unwrap_or_else(|| classifier.label_info().get_class_name(detection.class_id)),
                    confidence: detection.confidence,
                    width: region_width,
                    height: region_height,
                    image_path: ppm_path.to_string_lossy().to_string(),
                });
            }
            if image_regions.is_empty() && !page_data.detections.is_empty() {
                dbglog!(
                    "[djvu] page {} NO image regions exported; class_counts={:?}",
                    page_num,
                    per_class_counts
                );
            } else {
                dbglog!(
                    "[djvu] page {} exported {} image regions (total detections={})",
                    page_num,
                    image_regions.len(),
                    page_data.detections.len()
                );
            }
        }

        // Create page metadata
        let mut metadata = PageMetadata {
            page_number: page_num,
            width,
            height,
            dpi: self.config.dpi,
            background_path,
            image_regions,
            text_layer_path: None,
        };

        // If HOCR present, convert to DjVu S-expression and save to file
        if let Some(hocr_raw) = page_data.hocr.as_ref() {
            if !hocr_raw.is_empty() {
                match Self::hocr_to_djvu_text(hocr_raw, width, height) {
                    Ok(expr) => {
                        let text_path = page_dir.join("text_layer.sxp");
                        if let Err(e) = fs::write(&text_path, &expr) {
                            dbglog!(
                                "Warning: Failed writing DjVu text layer for page {}: {}",
                                page_num + 1,
                                e
                            );
                        } else {
                            metadata.text_layer_path =
                                Some(text_path.to_string_lossy().to_string());
                        }
                    }
                    Err(e) => {
                        dbglog!(
                            "Warning: HOCR->DjVu conversion failed (page {}): {}",
                            page_num + 1,
                            e
                        );
                    }
                }
            }
        }

        // Write metadata JSON (still useful for debugging and potential recovery)
        let json_path = page_dir.join("metadata.json");
        let json_content = serde_json::to_string_pretty(&metadata)
            .context("Failed to serialize page metadata")?;
        fs::write(&json_path, json_content)
            .with_context(|| format!("Failed to write metadata JSON: {:?}", json_path))?;

        // **KEY CHANGE**: Assemble the page DJVU file immediately instead of accumulating metadata
        // This prevents memory buildup and enables incremental processing
        let page_file = self.pages_dir.join(format!("page_{:04}.djvu", metadata.page_number));

        dbglog!(
            "[djvu] Assembling page {} immediately to avoid memory accumulation",
            metadata.page_number + 1
        );
        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] BEFORE assemble_page: page {}, output_path: {:?}", metadata.page_number, page_file);

        // Assemble the individual page DJVU file
        self.assemble_page(&metadata, &page_file, None)
            .with_context(|| format!("Failed to assemble page {} immediately", metadata.page_number))?;

        #[cfg(feature = "debug-logging")]
        crate::success_log!("[DJVU] AFTER assemble_page: page {}, checking file existence", metadata.page_number);

        // Verify the file was created
        #[cfg(feature = "debug-logging")]
        if page_file.exists() {
            if let Ok(meta) = std::fs::metadata(&page_file) {
                crate::info_log!("[DJVU] Page file created: {:?}, size: {} bytes", page_file, meta.len());
            }
        } else {
            crate::error_log!("[DJVU] WARNING: Page file not found after assembly: {:?}", page_file);
        }

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] Page {} validation complete, proceeding to registration", metadata.page_number);

        // Only store lightweight tracking info (page number and file path)
        // NOT the full PageMetadata with images and detections
        // Thread-safe: lock only for the duration of the push
        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] BEFORE adding to assembled_pages: page {}", metadata.page_number);

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] Acquiring assembled_pages lock for page {}", metadata.page_number);

        self.assembled_pages.lock().unwrap().push(AssembledPageInfo {
            page_number: metadata.page_number,
            page_file: page_file.clone(),
        });

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] Lock released after push for page {}", metadata.page_number);

        #[cfg(feature = "debug-logging")]
        {
            let count = self.assembled_pages.lock().unwrap().len();
            crate::success_log!("[DJVU] AFTER adding to assembled_pages: page {}, total assembled: {}", metadata.page_number, count);
        }

        dbglog!(
            "[djvu] Page {} assembled successfully at {:?}, freed from memory",
            metadata.page_number + 1,
            page_file
        );

        Ok(())
    }


    /// Export binarized data as PBM P4 format using optimized Legencode helper
    fn export_pbm(&self, binarized: &[u8], width: usize, height: usize, path: &Path) -> Result<()> {
        let pbm_data = Legencode::color::binarization::pbm::make_pbm_p4(binarized, width, height);
        fs::write(path, &pbm_data)
            .with_context(|| format!("Failed to write PBM file: {:?}", path))?;
        dbglog!("[djvu] wrote PBM {:?} size={} dims={}x{} first16={:02X?}", path, pbm_data.len(), width, height, &pbm_data[..pbm_data.len().min(16)]);
        self.validate_pbm(path, width as u32, height as u32)?;
        Ok(())
    }

    /// Export RGB image as PPM P6 format (uncompressed)
    fn export_ppm(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>, path: &Path) -> Result<()> {
        let width = image.width();
        let height = image.height();

        let header = format!("P6\n{} {}\n255\n", width, height);
        let mut ppm_data = Vec::with_capacity(header.len() + (width * height * 3) as usize);
        ppm_data.extend_from_slice(header.as_bytes());
        ppm_data.extend_from_slice(image.as_raw());

        fs::write(path, &ppm_data)
            .with_context(|| format!("Failed to write PPM file: {:?}", path))?;
        dbglog!("[djvu] wrote PPM {:?} size={} dims={}x{} header_ok={} first16={:02X?}", path, ppm_data.len(), width, height, true, &ppm_data[..ppm_data.len().min(16)]);

        Ok(())
    }

    /// Extract a sub-region from an RGB image and copy it into a new buffer
    fn extract_subimage(
        &self,
        image: &ImageBuffer<Rgb<u8>, Vec<u8>>,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<ImageBuffer<Rgb<u8>, Vec<u8>>> {
        if x + width > image.width() || y + height > image.height() {
            return Err(anyhow!(
                "Requested subimage {}x{} at ({}, {}) exceeds source {}x{}",
                width,
                height,
                x,
                y,
                image.width(),
                image.height()
            ));
        }

        let mut sub_img = ImageBuffer::new(width, height);
        for dy in 0..height {
            for dx in 0..width {
                let pixel = image.get_pixel(x + dx, y + dy);
                sub_img.put_pixel(dx, dy, *pixel);
            }
        }

        Ok(sub_img)
    }

    /// Try to resolve a bundled or system executable by name (e.g., "cjb2")
    /// Search order:
    /// 1) LEGE_DJVU_BIN env dir
    /// 2) Next to the current executable: bin/, bin/djvulibre/, djvulibre/, ./
    /// 3) macOS bundle path: ../Resources/bin/
    /// 4) PATH lookup
    fn resolve_executable(&self, name: &str) -> Result<OsString> {
        // Cache hit fast path with read lock
        {
            let cache = self.tool_cache.read().unwrap();
            if let Some(cached) = cache.get(name) {
                return Ok(cached.clone());
            }
        }
        let mut candidates: Vec<PathBuf> = vec![];

        // Platform-specific executable filename
        #[cfg(windows)]
        let exe_name = format!("{}.exe", name);
        #[cfg(not(windows))]
        let exe_name = name.to_string();

        // 1) Environment override
        if let Ok(dir) = env::var("LEGE_DJVU_BIN") {
            let p = PathBuf::from(dir).join(&exe_name);
            candidates.push(p);
        }

        // 2) Next to current executable
        if let Ok(exe) = env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                candidates.push(exe_dir.join("bin").join(&exe_name));
                candidates.push(exe_dir.join("bin").join("djvulibre").join(&exe_name));
                candidates.push(exe_dir.join("djvulibre").join(&exe_name));
                candidates.push(exe_dir.join(&exe_name));

                // 3) macOS bundle Resources
                #[cfg(target_os = "macos")]
                {
                    // app.app/Contents/MacOS/lege -> Resources/bin/
                    if let Some(contents_dir) = exe_dir.parent() {
                        let resources =
                            contents_dir.join("Resources").join("bin").join(&exe_name);
                        candidates.push(resources);
                    }
                }
            }
        }

        // Helper: is executable file
        fn is_executable(path: &Path) -> bool {
            if let Ok(meta) = fs::metadata(path) {
                if !meta.is_file() {
                    return false;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    return meta.permissions().mode() & 0o111 != 0;
                }
                #[cfg(windows)]
                {
                    // On Windows, presence & .exe extension is sufficient
                    return path
                        .extension()
                        .map(|e| e.eq_ignore_ascii_case("exe"))
                        .unwrap_or(false);
                }
                #[allow(unreachable_code)]
                {
                    true
                }
            } else {
                false
            }
        }

    dbglog!("[djvu] resolve '{}' ({} candidates)", name, candidates.len());
    for cand in &candidates { dbglog!("[djvu]   cand: {}", cand.display()); }
        for cand in &candidates {
            if is_executable(cand) {
                dbglog!("[djvu] resolve selected {}", cand.display());
                let os = OsString::from(cand);
                // Thread-safe cache update
                self.tool_cache.write().unwrap().insert(name.to_string(), os.clone());
                return Ok(os);
            }
        }

        // 4) PATH lookup (cross-platform)
        if let Ok(path_var) = env::var("PATH") {
            let sep = if cfg!(windows) { ';' } else { ':' };
            for dir in path_var.split(sep) {
                let p = PathBuf::from(dir).join(&exe_name);
                if is_executable(&p) {
                    dbglog!("[djvu] resolve selected PATH {}", p.display());
                    let os = OsString::from(p);
                    // Thread-safe cache update
                    self.tool_cache.write().unwrap().insert(name.to_string(), os.clone());
                    return Ok(os);
                }
            }
        }

        // As a last attempt on Unix, allow Command to resolve bare name if available in PATH
        #[cfg(not(windows))]
        {
            Ok(OsString::from(name))
        }
        #[cfg(windows)]
        {
            Err(anyhow!(
                "Executable '{}' not found. Set LEGE_DJVU_BIN or bundle in ./bin/",
                name
            ))
        }
    }

    /// Generate document metadata for all pages
    /// **UPDATED**: Now uses lightweight assembled_pages data instead of full PageMetadata
    pub fn write_document_metadata(&self) -> Result<()> {
        // Ensure work_dir exists before writing
        fs::create_dir_all(&self.config.work_dir)
            .with_context(|| format!("Failed to create work directory: {:?}", self.config.work_dir))?;

        let doc_metadata_path = self.config.work_dir.join("document.json");

        #[derive(Serialize)]
        struct SimplifiedDocumentMetadata {
            total_pages: usize,
            dpi: u32,
            page_files: Vec<PathBuf>,
            config: DjvuConfig,
        }

        let assembled = self.assembled_pages.lock().unwrap();
        let doc_meta = SimplifiedDocumentMetadata {
            total_pages: assembled.len(),
            dpi: self.config.dpi,
            page_files: assembled.iter().map(|p| p.page_file.clone()).collect(),
            config: self.config.clone(),
        };
        drop(assembled); // Release lock early

        let json_content = serde_json::to_string_pretty(&doc_meta)
            .context("Failed to serialize document metadata")?;
        fs::write(&doc_metadata_path, json_content).with_context(|| {
            format!(
                "Failed to write document metadata: {:?}",
                doc_metadata_path
            )
        })?;

        Ok(())
    }

    /// Encode a PBM background to DJVU using cjb2
    fn encode_background(
        &self,
        pbm_path: &Path,
        output_path: &Path,
        width: u32,
        height: u32,
    ) -> Result<()> {
        self.validate_pbm(pbm_path, width, height)?;

        let parent_dir = pbm_path.parent().unwrap_or_else(|| Path::new("."));
        let log_path = self.default_log_for_dir(parent_dir);

        let mut attempts: Vec<(&str, bool)> = Vec::new();
        attempts.push(("primary", false));
        if !self.config.clean {
            attempts.push(("clean", true));
        }

        let mut last_err: Option<anyhow::Error> = None;
        for (label, force_clean) in attempts {
            if output_path.exists() {
                let _ = fs::remove_file(output_path);
            }

            let cjb2_path = self.resolve_executable("cjb2")?;
            let mut cmd = Command::new(cjb2_path);
            cmd.arg("-dpi").arg(self.config.dpi.to_string());

            if self.config.clean || force_clean {
                cmd.arg("-clean");
            }

            if let Some(lossy) = self.config.lossy {
                cmd.arg("-lossy");
                cmd.arg("-losslevel").arg(lossy.to_string());
            }

            cmd.arg(pbm_path).arg(output_path);
            cmd.current_dir(parent_dir);

            let timeout = if force_clean {
                CJB2_RETRY_TIMEOUT
            } else {
                CJB2_TIMEOUT
            };

            let log_ref = log_path.as_deref();
            match self.run_tool_with_timeout(
                cmd,
                None,
                &format!("cjb2-{}", label),
                timeout,
                log_ref,
            ) {
                Ok(_) => {
                    match self.djvudump_check(
                        output_path,
                        &format!("djvudump-cjb2-{}", label),
                        &["FORM:DJVU", "Sjbz"],
                        Some((width, height)),
                        log_ref,
                    ) {
                        Ok(_) => return Ok(()),
                        Err(e) => {
                            last_err = Some(e);
                            continue;
                        }
                    }
                }
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            anyhow!(
                "cjb2 failed for {:?} â†’ {:?}",
                pbm_path,
                output_path
            )
        }))
    }

    /// Encode a PPM image to DJVU using c44
    fn encode_image(&self, ppm_path: &Path, output_path: &Path) -> Result<()> {
        self.encode_image_with_mask(ppm_path, output_path, None, false)
    }

    /// Encode a PPM image to DJVU using c44, optionally with a PBM mask
    ///
    /// # Arguments
    /// * `is_cover` - If true, uses higher quality settings optimized for cover pages
    fn encode_image_with_mask(&self, ppm_path: &Path, output_path: &Path, mask_path: Option<&Path>, is_cover: bool) -> Result<()> {
        let c44_path = self.resolve_executable("c44")?;
        let mut cmd = Command::new(c44_path);

        // Quality control via decibels
        // Cover pages get higher quality (38-40 dB) for better visual impression
        // Regular pages use configured value (default 32 dB) for good balance
        let decibels = if is_cover {
            self.config.iw44_decibels.max(38)
        } else {
            self.config.iw44_decibels
        };
        cmd.arg("-decibel").arg(decibels.to_string());

        // Chrominance quality - cover pages use normal resolution, others use half
        // For e-ink readers, this provides good quality without being too obvious
        if is_cover {
            cmd.arg("-crcbnormal");
        } else {
            cmd.arg("-crcbhalf");
            // Add chrominance delay for better compression on non-cover pages
            cmd.arg("-crcbdelay").arg("10");
        }

        // Focus quality estimation on most important blocks (e-ink optimization)
        // This helps ensure text-heavy regions maintain quality
        cmd.arg("-dbfrac").arg("0.5");

        // DPI setting
        cmd.arg("-dpi").arg(self.config.dpi.to_string());

        // If mask provided, use it to encode text areas with minimal bitrate
        if let Some(mask) = mask_path {
            cmd.arg("-mask").arg(mask);
        }

        // Input PPM and output DJVU
        cmd.arg(ppm_path).arg(output_path);
        #[cfg(windows)] { cmd.creation_flags(CREATE_NO_WINDOW); }
        let output = cmd.output().with_context(|| format!("Failed to execute c44 for {:?}", ppm_path))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(anyhow!("c44 failed for {:?}: stderr='{}' stdout='{}'", ppm_path, stderr, stdout));
        }
        #[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
        {
            if let Ok(meta) = fs::metadata(output_path) {
                dbglog!("[djvu] c44 encode success out={:?} size={}B (cover={}, decibels={})", output_path, meta.len(), is_cover, decibels);
            } else {
                dbglog!("[djvu] c44 encode success out={:?} (metadata missing)", output_path);
            }
        }
        Ok(())
    }

    /// White out image region areas in a PBM file to prevent JB2 text from bleeding through
    /// This reads the original PBM, sets image region bboxes to white (0 bits), and writes modified PBM
    fn whiteout_image_regions_in_pbm(
        &self,
        page_meta: &PageMetadata,
        input_pbm: &Path,
        output_pbm: &Path,
    ) -> Result<()> {
        // Read and parse the input PBM
        let data = fs::read(input_pbm)?;
        if !data.starts_with(b"P4") {
            return Err(anyhow!("Not a P4 PBM file: {:?}", input_pbm));
        }
        
        let mut idx = 2usize;
        let len = data.len();
        
        // Skip whitespace and comments helper
        let skip_ws_and_comments = |i: &mut usize| {
            loop {
                while *i < len && matches!(data[*i], b' ' | b'\t' | b'\r' | b'\n') { *i += 1; }
                if *i < len && data[*i] == b'#' {
                    while *i < len && data[*i] != b'\n' { *i += 1; }
                    if *i < len { *i += 1; }
                } else {
                    break;
                }
            }
        };
        
        // Parse number helper
        let parse_number = |i: &mut usize| -> Option<u32> {
            skip_ws_and_comments(i);
            if *i >= len { return None; }
            let start = *i;
            while *i < len && data[*i].is_ascii_digit() { *i += 1; }
            if start == *i { return None; }
            std::str::from_utf8(&data[start..*i]).ok()?.parse::<u32>().ok()
        };
        
        let w = parse_number(&mut idx).ok_or_else(|| anyhow!("Failed to parse PBM width"))?;
        let h = parse_number(&mut idx).ok_or_else(|| anyhow!("Failed to parse PBM height"))?;
        skip_ws_and_comments(&mut idx);
        
        // Read the bitmap data
        let row_stride = ((w + 7) / 8) as usize;
        let bitmap_size = row_stride * h as usize;
        if idx + bitmap_size > len {
            return Err(anyhow!("PBM data truncated"));
        }
        
        // Copy bitmap to a mutable buffer
        let mut bitmap = data[idx..idx + bitmap_size].to_vec();
        
        // White out each image region
        // Bbox coordinates are already in page space (scaled during process_page)
        for region in &page_meta.image_regions {
            let x1 = region.bbox[0].max(0.0).floor() as u32;
            let y1 = region.bbox[1].max(0.0).floor() as u32;
            let x2 = region.bbox[2].min(w as f32).ceil() as u32;
            let y2 = region.bbox[3].min(h as f32).ceil() as u32;
            
            // Set all bits in this region to 0 (white in PBM)
            for y in y1..y2 {
                for x in x1..x2 {
                    let byte_idx = (y * row_stride as u32 + x / 8) as usize;
                    let bit_idx = 7 - (x % 8);
                    if byte_idx < bitmap.len() {
                        bitmap[byte_idx] &= !(1 << bit_idx);
                    }
                }
            }
        }
        
        // Write modified PBM
        let mut output_data = Vec::new();
        output_data.extend_from_slice(b"P4\n");
        output_data.extend_from_slice(format!("{} {}\n", w, h).as_bytes());
        output_data.extend_from_slice(&bitmap);
        fs::write(output_pbm, output_data)?;
        
        Ok(())
    }

    /// Compose a color background (BG44) from all image regions placed onto white canvas
    /// Compose a color background, encode it, and extract the raw IW44 chunk.
    /// Returns the path to the raw IW44 file (`.iw44`) on success.
    fn compose_color_background(&self, page_meta: &PageMetadata, page_dir: &Path) -> Result<Option<PathBuf>> {
        if page_meta.image_regions.is_empty() {
            return Ok(None);
        }

        let w = page_meta.width;
        let h = page_meta.height;
        let mut canvas = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(w, h, Rgb([255, 255, 255]));
        let log_path = self.default_log_for_dir(page_dir);

        // Paste image regions onto canvas (NO masking - JB2 layer will handle that)
        // Bbox coordinates are already in page space (scaled during process_page)
        for region in &page_meta.image_regions {
            let img_path = Path::new(&region.image_path);
            let data = match fs::read(img_path) {
                Ok(d) => d,
                Err(e) => { dbglog!("Failed to read region image {:?}: {}", img_path, e); continue; }
            };
            let (iw, ih, pixels) = match Self::parse_ppm(&data) {
                Ok(t) => t,
                Err(e) => { dbglog!("Invalid PPM for region {:?}: {}", img_path, e); continue; }
            };
            
            // Bbox is already in page space - use directly
            let x1 = region.bbox[0].max(0.0).floor() as u32;
            let y1 = region.bbox[1].max(0.0).floor() as u32;
            
            if x1 >= w || y1 >= h { continue; }
            let avail_w = (w - x1).min(iw);
            let avail_h = (h - y1).min(ih);
            if avail_w == 0 || avail_h == 0 { continue; }
            
            // Paste full image (no masking during composition)
            for ry in 0..avail_h {
                for rx in 0..avail_w {
                    let dst_x = x1 + rx;
                    let dst_y = y1 + ry;
                    let src_idx = ((ry*iw + rx)*3) as usize;
                    if src_idx + 2 < pixels.len() {
                        let r = pixels[src_idx]; 
                        let g = pixels[src_idx+1]; 
                        let b = pixels[src_idx+2];
                        canvas.put_pixel(dst_x, dst_y, Rgb([r,g,b]));
                    }
                }
            }
        }

        // 1. Export the final composite PPM
        let ppm_path = page_dir.join("color_bg.ppm");
        self.export_ppm(&canvas, &ppm_path)?;

        // 2. Encode the PPM with c44 into a temporary DjVu container (apply PBM mask when available)
        let color_container_path = page_dir.join("color_temp.djvu");
        let log_ref = log_path.as_deref();
        let mask_candidate = page_dir.join("background_modified.pbm");
        let mask_path = if mask_candidate.exists() { Some(mask_candidate) } else { Some(Path::new(&page_meta.background_path).to_path_buf()) };

        // Use higher quality for cover pages (page 0)
        let is_cover = page_meta.page_number == 0;

        if self.should_use_color_mask(page_meta) {
            self.encode_image_with_mask(&ppm_path, &color_container_path, mask_path.as_deref(), is_cover)?;
        } else {
            self.encode_image_with_mask(&ppm_path, &color_container_path, None, is_cover)?;
        }
        self.djvudump_check(
            &color_container_path,
            &format!("djvudump-color-{}", page_meta.page_number + 1),
            &["FORM:DJVU", "BG44"],
            Some((w, h)),
            log_ref,
        )?;

        // 3. Extract the raw BG44/IW44 chunk using djvuextract
        let raw_iw44_path = page_dir.join("background.iw44");
        let djvuextract_path = self.resolve_executable("djvuextract")?;
        let mut cmd = Command::new(djvuextract_path);
        cmd.arg(&color_container_path)
           .arg(format!("BG44={}", raw_iw44_path.display()));
        cmd.current_dir(page_dir);
        self.run_tool_with_timeout(
            cmd,
            None,
            &format!("djvuextract-page-{}", page_meta.page_number + 1),
            DJVUEXTRACT_TIMEOUT,
            log_ref,
        )?;

        // 4. Verify the output and return the path to the raw chunk
        if !raw_iw44_path.exists() || fs::metadata(&raw_iw44_path)?.len() == 0 {
            return Err(anyhow!("djvuextract did not create a valid IW44 file."));
        }
        
        Ok(Some(raw_iw44_path))
    }

    /// Minimal binary-safe PPM (P6) parser returning (w,h,data)
    /// Supports comments and arbitrary binary pixel data (previous implementation incorrectly assumed full UTF-8).
    fn parse_ppm(data: &[u8]) -> Result<(u32,u32,Vec<u8>)> {
        if data.len() < 11 { return Err(anyhow!("PPM too small")); }
        if &data[0..2] != b"P6" { return Err(anyhow!("Unsupported PPM magic")); }
        let mut idx = 2; // after magic
        // helper to skip single whitespace
        let skip_ws = |i: &mut usize| {
            while *i < data.len() && (data[*i] == b' ' || data[*i] == b'\n' || data[*i] == b'\r' || data[*i] == b'\t') { *i += 1; }
        };
        // skip initial ws
        skip_ws(&mut idx);
        // skip comments function
        let skip_comments = |i: &mut usize| {
            loop {
                if *i < data.len() && data[*i] == b'#' { // comment until newline
                    while *i < data.len() && data[*i] != b'\n' { *i += 1; }
                    if *i < data.len() { *i += 1; }
                } else { break; }
            }
        };
        skip_comments(&mut idx);
        // parse number helper
        let parse_number = |i: &mut usize| -> Result<u32> {
            if *i >= data.len() { return Err(anyhow!("Unexpected EOF parsing PPM number")); }
            let start = *i;
            while *i < data.len() && data[*i].is_ascii_digit() { *i += 1; }
            if start == *i { return Err(anyhow!("Expected PPM number")); }
            let num = std::str::from_utf8(&data[start..*i])?.parse::<u32>()?;
            Ok(num)
        };
        let w = {
            let n = parse_number(&mut idx)?; skip_ws(&mut idx); skip_comments(&mut idx); n
        };
        let h = {
            let n = parse_number(&mut idx)?; skip_ws(&mut idx); skip_comments(&mut idx); n
        };
        let maxv = {
            let n = parse_number(&mut idx)?; n
        };
        if maxv != 255 { return Err(anyhow!("Unsupported maxval {}", maxv)); }
        // skip single whitespace separating header from binary raster
        if idx >= data.len() { return Err(anyhow!("Truncated PPM before pixel data")); }
        if data[idx].is_ascii_whitespace() { idx += 1; }
        let expected = (w as usize) * (h as usize) * 3;
        if idx + expected > data.len() { return Err(anyhow!("PPM pixel data short: have {} need {}", data.len() - idx, expected)); }
        Ok((w,h,data[idx..idx+expected].to_vec()))
    }

    /// Assemble a single DJVU page from background + optional composite color layer
    /// Assemble a single DJVU page from its components.
    fn assemble_page(
        &self,
        page_meta: &PageMetadata,
        output_path: &Path,
        tracker: Option<&crate::progress::ProgressTracker>,
    ) -> Result<()> {
        dbglog!("[assemble_page] Page {}: Starting", page_meta.page_number + 1);
        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] assemble_page START: page {}, output: {:?}", page_meta.page_number, output_path);

        let background_pbm_path = Path::new(&page_meta.background_path);
        let page_dir = background_pbm_path.parent().unwrap();

        // 2. Encode the background PBM with cjb2, or skip for no-binarization mode
        let background_djvu_path = page_dir.join("background.djvu");
        if !self.config.no_binarization_mode {
            // 1. If there are image regions, white out those areas in the PBM before cjb2 encoding
            //    This prevents JB2 text layer from bleeding through the color images
            let pbm_to_encode = if !page_meta.image_regions.is_empty() {
                let modified_pbm_path = page_dir.join("background_modified.pbm");
                self.whiteout_image_regions_in_pbm(page_meta, background_pbm_path, &modified_pbm_path)?;
                modified_pbm_path
            } else {
                background_pbm_path.to_path_buf()
            };

            // 2. Encode the (possibly modified) PBM with cjb2. This creates our Sjbz source file.
            self.encode_background(
                &pbm_to_encode,
                &background_djvu_path,
                page_meta.width,
                page_meta.height,
            )?;
        } else {
            // In no-binarization mode: we're creating IW44 only, no background text layer
            // So we create a blank background djvu file or skip this step entirely
            // We'll handle this in the djvumake step by skipping the Sjbz part
            dbglog!("[assemble_page] Skipping background encoding for page {} (no-binarization mode)", page_meta.page_number + 1);
        }

        // 3. Prepare the color layer (if any images exist).
        // This function now handles everything: PPM composition, c44, and djvuextract.
        // It returns the path to the final raw `.iw44` file needed by djvumake.
        let bg44_chunk_path = match self.compose_color_background(page_meta, page_dir) {
            Ok(path_opt) => path_opt,
            Err(e) => {
                dbglog!("Warning: Color layer composition failed for page {}: {}. Proceeding without color.", page_meta.page_number + 1, e);
                None
            }
        };

        // 4. Assemble the final page with djvumake using hardened fallbacks.
        let tmp_page = output_path.with_extension("djvu.tmp");
        if tmp_page.exists() {
            let _ = fs::remove_file(&tmp_page);
        }
        if output_path.exists() {
            let _ = fs::remove_file(output_path);
        }

        let djvumake_path = self.resolve_executable("djvumake")?;
        let log_path = self.default_log_for_dir(page_dir);
        let log_ref = log_path.as_deref();
        let mut last_err: Option<anyhow::Error> = None;

        enum PageVariant {
            Full, // Sjbz + BG44 (or just BG44 in no-binarization mode)
            BgOnly, // BG44 only
            SjbzOnly, // Sjbz only
        }

        // In no-binarization mode, we skip the Sjbz (text layer) and create IW44 only
        let variants = if self.config.no_binarization_mode {
            // In no-binarization mode: only BG44 (IW44) layer, no text layer
            if bg44_chunk_path.is_some() {
                vec![PageVariant::BgOnly] // IW44 only
            } else {
                vec![] // This shouldn't happen in no-binarization mode since we create a full-page IW44
            }
        } else {
            // Normal mode: try different combinations
            let mut variants = vec![PageVariant::Full];
            if bg44_chunk_path.is_some() {
                variants.push(PageVariant::BgOnly);
            }
            variants.push(PageVariant::SjbzOnly);
            variants
        };

        let mut assembled = false;
            let mut assembled_variant = "unknown";

        for variant in variants {
            if tmp_page.exists() {
                let _ = fs::remove_file(&tmp_page);
            }

            match variant {
                PageVariant::Full => {
                    let mut cmd = Command::new(&djvumake_path);
                    cmd.arg(&tmp_page);
                    cmd.arg(format!(
                        "INFO={},{},{}",
                        page_meta.width, page_meta.height, page_meta.dpi
                    ));

                    // In no-binarization mode, skip Sjbz (text layer)
                    if !self.config.no_binarization_mode {
                        cmd.arg(format!("Sjbz={}", background_djvu_path.display()));
                    }

                    if let Some(ref bg44_path) = bg44_chunk_path {
                        cmd.arg(format!("BG44={}", bg44_path.display()));
                    }
                    cmd.current_dir(page_dir);

                    let context = format!("djvumake-full-{}", page_meta.page_number + 1);
                    match self.run_tool_with_timeout(
                        cmd,
                        tracker,
                        &context,
                        DJVUMAKE_TIMEOUT,
                        log_ref,
                    ) {
                        Ok(_) => {
                            let check_for = if self.config.no_binarization_mode {
                                // In no-binarization mode, check for BG44 instead of Sjbz
                                &["FORM:DJVU", "BG44"][..]
                            } else {
                                &["FORM:DJVU", "Sjbz"][..]
                            };

                            let dump_output = self.djvudump_check(
                                &tmp_page,
                                &format!("djvudump-page-{}", page_meta.page_number + 1),
                                check_for,
                                Some((page_meta.width, page_meta.height)),
                                log_ref,
                            );
                            match dump_output {
                                Ok(text) => {
                                    let has_expected_layer = if self.config.no_binarization_mode {
                                        text.contains("BG44") // Look for IW44 layer
                                    } else {
                                        text.contains("FGbz") || text.contains("Sjbz") // Look for text layer
                                    };

                                    if has_expected_layer {
                                        assembled = true;
                                              assembled_variant = "Full";
                                              break;
                                    } else {
                                        let expected = if self.config.no_binarization_mode {
                                            "BG44"
                                        } else {
                                            "FGbz or Sjbz"
                                        };
                                        last_err = Some(anyhow!(
                                            "djvumake produced page without expected {} chunk",
                                            expected
                                        ));
                                    }
                                }
                                Err(e) => {
                                    last_err = Some(e);
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            last_err = Some(e);
                            continue;
                        }
                    }
                }
                PageVariant::BgOnly => {
                    if let Some(ref bg44_path) = bg44_chunk_path {
                        let mut cmd = Command::new(&djvumake_path);
                        cmd.arg(&tmp_page);
                        cmd.arg(format!(
                            "INFO={},{},{}",
                            page_meta.width, page_meta.height, page_meta.dpi
                        ));
                        cmd.arg(format!("BG44={}", bg44_path.display()));
                        cmd.current_dir(page_dir);

                        let context =
                            format!("djvumake-bgonly-{}", page_meta.page_number + 1);
                        match self.run_tool_with_timeout(
                            cmd,
                            tracker,
                            &context,
                            DJVUMAKE_FALLBACK_TIMEOUT,
                            log_ref,
                        ) {
                            Ok(_) => {
                                if let Err(e) = self.djvudump_check(
                                    &tmp_page,
                                    &format!(
                                        "djvudump-bgonly-{}",
                                        page_meta.page_number + 1
                                    ),
                                    &["FORM:DJVU", "BG44"],
                                    Some((page_meta.width, page_meta.height)),
                                    log_ref,
                                ) {
                                    last_err = Some(e);
                                } else {
                                    assembled = true;
                                          assembled_variant = "BgOnly";
                                          break;
                                }
                            }
                            Err(e) => last_err = Some(e),
                        }
                    }
                }
                PageVariant::SjbzOnly => {
                    // Only attempt SjbzOnly if not in no-binarization mode
                    if !self.config.no_binarization_mode {
                        if let Err(e) = fs::copy(&background_djvu_path, &tmp_page) {
                            last_err = Some(anyhow!(
                                "Failed to copy JB2 fallback for page {}: {}",
                                page_meta.page_number + 1,
                                e
                            ));
                            continue;
                        }
                        if let Err(e) = self.djvudump_check(
                            &tmp_page,
                            &format!("djvudump-sjbzonly-{}", page_meta.page_number + 1),
                            &["FORM:DJVU", "Sjbz"],
                            Some((page_meta.width, page_meta.height)),
                            log_ref,
                        ) {
                            last_err = Some(e);
                            continue;
                        }
                        assembled = true;
                              assembled_variant = "SjbzOnly";
                              break;
                    }
                }
            }
        }

        if !assembled {
            return Err(last_err.unwrap_or_else(|| {
                anyhow!(
                    "Failed to assemble DjVu page {}",
                    page_meta.page_number + 1
                )
            }));
        }

        // Robust file move for Windows: use rename, fallback to copy+remove if rename fails
        if output_path.exists() {
            let _ = fs::remove_file(output_path);
        }
        if let Err(e) = fs::rename(&tmp_page, output_path) {
            // Fallback if rename fails (e.g. cross-device or locked file on Windows)
            fs::copy(&tmp_page, output_path)
                .with_context(|| format!("Failed to copy temp page to output: {:?}", output_path))?;
            let _ = fs::remove_file(&tmp_page);
            dbglog!(
                "Used copy+remove fallback for page {} (rename failed: {})",
                page_meta.page_number + 1,
                e
            );
        }

              if let Ok(meta) = fs::metadata(output_path) {
                  let sz = meta.len();
                  if let Some(tr) = tracker {
                      tr.update(crate::progress::ProcessingStatus::ExternalToolLine { line: format!("Assembled page {} via {} ({} bytes)", page_meta.page_number + 1, assembled_variant, sz) });
                  }
                  if sz < 1024 {
                      if let Some(tr) = tracker {
                          tr.update(crate::progress::ProcessingStatus::ExternalToolLine { line: format!("Small DJVU page {} ({} bytes)", page_meta.page_number + 1, sz) });
                      }
                  }
              }

              if let Some(text_path_str) = &page_meta.text_layer_path {
            let text_path = Path::new(text_path_str);
            if text_path.exists() {
                if let Err(e) = self.embed_text_layer(output_path, text_path) {
                    dbglog!(
                        "Warning: Failed to embed text layer on page {}: {}",
                        page_meta.page_number + 1,
                        e
                    );
                }
            }
        }

        #[cfg(feature = "debug-logging")]
        crate::success_log!("[DJVU] assemble_page END: page {}, output: {:?}", page_meta.page_number, output_path);

        dbglog!(
            "[assemble_page] Page {} assembly complete: {:?}",
            page_meta.page_number + 1,
            output_path
        );
        Ok(())
    }

    // Removed extract_payload_only, wrap_payload_as_chunk, and extract_chunk functions
    // as they are no longer needed with the simplified workflow

    /// Assemble multi-page DJVU document from all processed pages
    pub fn assemble_djvu(&self, output_path: &Path) -> Result<()> {
        self.assemble_djvu_with_progress(output_path, None)
    }

    /// Assemble multi-page DJVU document with optional progress tracker for bundling phase
    /// **REFACTORED**: Now works with pre-assembled page files instead of assembling on-the-fly
    /// This eliminates memory accumulation since pages were assembled incrementally during processing
    pub fn assemble_djvu_with_progress(
        &self,
        output_path: &Path,
        tracker: Option<&crate::progress::ProgressTracker>,
    ) -> Result<()> {
        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] ========== assemble_djvu_with_progress START ==========");
        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] Output path: {:?}", output_path);

        // Lock once and clone the assembled pages list
        let sorted_pages = {
            #[cfg(feature = "debug-logging")]
            crate::info_log!("[DJVU] Acquiring assembled_pages lock...");

            let assembled = self.assembled_pages.lock().unwrap();

            #[cfg(feature = "debug-logging")]
            crate::info_log!("[DJVU] Lock acquired. Assembled pages count: {}", assembled.len());

            if assembled.is_empty() {
                #[cfg(feature = "debug-logging")]
                crate::error_log!("[DJVU] No pages to assemble!");
                return Err(anyhow!("No pages to assemble"));
            }
            let mut pages = assembled.clone();
            // Sort pages by page_number to ensure correct order
            pages.sort_by_key(|p| p.page_number);

            #[cfg(feature = "debug-logging")]
            crate::info_log!("[DJVU] Pages sorted. Releasing lock.");

            pages
        }; // Lock released here

        let total_pages = sorted_pages.len();
        #[cfg(feature = "debug-logging")]
        {
            crate::info_log!("[DJVU] Starting bundling of {} pre-assembled pages to {:?}", total_pages, output_path);
            dbglog!("[assemble_djvu_with_progress] Starting bundling of {} pre-assembled pages", total_pages);
        }
        if let Some(tr) = tracker {
            tr.update(crate::progress::ProcessingStatus::DjvuBundling { current: 0, total: total_pages });
        }

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] Verifying {} pre-assembled page files...", total_pages);

        let mut page_files = Vec::with_capacity(total_pages);
        for (idx, page_info) in sorted_pages.iter().enumerate() {
            dbglog!(
                "[djvu] Verifying page {} (index {}) - file: {:?}",
                page_info.page_number, idx, page_info.page_file
            );

            #[cfg(feature = "debug-logging")]
            crate::info_log!("[DJVU] Verifying page {} (index {}): {:?}", page_info.page_number, idx, page_info.page_file);

            // Verify the pre-assembled page file exists and has content
            let file_size = std::fs::metadata(&page_info.page_file)
                .map_err(|e| {
                    #[cfg(feature = "debug-logging")]
                    crate::error_log!("[DJVU] Page file MISSING: page {} - {:?} - {}", page_info.page_number, page_info.page_file, e);
                    anyhow!(
                        "Pre-assembled page file missing for page {}: {:?} - {}",
                        page_info.page_number, page_info.page_file, e
                    )
                })?
                .len();

            #[cfg(feature = "debug-logging")]
            crate::info_log!("[DJVU] Page {} file size: {} bytes", page_info.page_number, file_size);

            if file_size == 0 {
                #[cfg(feature = "debug-logging")]
                crate::error_log!("[DJVU] Page file EMPTY: {:?}", page_info.page_file);
                return Err(anyhow!(
                    "Pre-assembled page file is empty: {:?}",
                    page_info.page_file
                ));
            }

            page_files.push(page_info.page_file.clone());

            // Update progress
            if let Some(tr) = tracker {
                tr.update(crate::progress::ProcessingStatus::DjvuBundling {
                    current: idx + 1,
                    total: total_pages
                });
            }
        }

        #[cfg(feature = "debug-logging")]
        crate::success_log!("[DJVU] All {} page files verified successfully", total_pages);

        // Combine into multi-page document (batch approach to prevent hanging during incremental additions)
        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] Starting bundling phase. Total pages: {}", page_files.len());

        match page_files.len() {
            1 => {
                #[cfg(feature = "debug-logging")]
                crate::info_log!("[DJVU] Single page document - copying to output");

                fs::copy(&page_files[0], output_path)
                    .with_context(|| format!("Failed to copy single page to output: {:?}", output_path))?;

                #[cfg(feature = "debug-logging")]
                crate::success_log!("[DJVU] Single page copied successfully");
            }
            n => {
                #[cfg(feature = "debug-logging")]
                crate::info_log!("[DJVU] Multi-page document bundling starting ({} pages)", n);

                let bundle_start = std::time::Instant::now();

                #[cfg(feature = "debug-logging")]
                crate::info_log!("[DJVU] Resolving djvm executable...");

                let djvm_path = self.resolve_executable("djvm")?;

                #[cfg(feature = "debug-logging")]
                crate::info_log!("[DJVU] djvm path: {:?}", djvm_path);

                dbglog!("[djvu] Acquiring bundle lock for {:?}", output_path);

                #[cfg(feature = "debug-logging")]
                crate::info_log!("[DJVU] Acquiring bundle lock for {:?}", output_path);

                let _bundle_lock = BundleLock::acquire(output_path)?;

                dbglog!("[djvu] Bundle lock acquired");

                #[cfg(feature = "debug-logging")]
                crate::success_log!("[DJVU] Bundle lock acquired successfully");

                let bundle_log_path = output_path.with_extension("bundle.log");
                // Only create bundling logs when debug logging is enabled
                #[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
                let log_ref = Some(bundle_log_path.as_path());
                #[cfg(not(any(feature = "debug-logging", feature = "djvu_debug")))]
                let log_ref: Option<&Path> = None;

                // Use batch creation instead of incremental additions to prevent hanging
                // The incremental djvm -i approach was hanging when adding many pages one by one
                // However, avoid extremely long command lines by batching if needed
                const MAX_PAGES_PER_BATCH: usize = 50; // Adjust based on command line limits

                if page_files.len() <= MAX_PAGES_PER_BATCH {
                    // For smaller documents, create all pages at once
                    #[cfg(feature = "debug-logging")]
                    crate::info_log!("[DJVU] Using single-batch creation for {} pages", page_files.len());

                    let mut cmd = Command::new(&djvm_path);
                    cmd.arg("-c").arg(output_path);
                    for page_file in &page_files {
                        cmd.arg(page_file);
                    }
                    cmd.current_dir(&self.pages_dir);

                    #[cfg(feature = "debug-logging")]
                    crate::info_log!("[DJVU] Executing djvm -c command...");

                    self.run_tool_with_timeout(
                        cmd,
                        tracker,
                        "djvm-batch-create",
                        DJVM_TIMEOUT,
                        log_ref,
                    )?;

                    #[cfg(feature = "debug-logging")]
                    crate::success_log!("[DJVU] djvm -c command completed successfully");
                } else {
                    // For larger documents, create in batches to avoid command line limits
                    #[cfg(feature = "debug-logging")]
                    crate::info_log!("[DJVU] Using multi-batch creation for {} pages (batch size: {})", page_files.len(), MAX_PAGES_PER_BATCH);

                    let mut temp_output = output_path.to_path_buf();
                    temp_output.set_extension("djvu.tmp");

                    let mut start_idx = 0;
                    while start_idx < page_files.len() {
                        let end_idx = std::cmp::min(start_idx + MAX_PAGES_PER_BATCH, page_files.len());
                        let batch_files = &page_files[start_idx..end_idx];

                        #[cfg(feature = "debug-logging")]
                        crate::info_log!("[DJVU] Processing batch: pages {}-{}", start_idx + 1, end_idx);

                        if start_idx == 0 {
                            // First batch: create the container
                            #[cfg(feature = "debug-logging")]
                            crate::info_log!("[DJVU] Creating initial container with {} pages", batch_files.len());

                            let mut cmd = Command::new(&djvm_path);
                            cmd.arg("-c").arg(&temp_output);
                            for page_file in batch_files {
                                cmd.arg(page_file);
                            }
                            cmd.current_dir(&self.pages_dir);
                            self.run_tool_with_timeout(
                                cmd,
                                tracker,
                                &format!("djvm-batch-create-{}-{}", start_idx + 1, end_idx),
                                DJVM_TIMEOUT,
                                log_ref,
                            )?;

                            #[cfg(feature = "debug-logging")]
                            crate::success_log!("[DJVU] Initial container created");
                        } else {
                            // Subsequent batches: append to existing container - one page at a time
                            #[cfg(feature = "debug-logging")]
                            crate::info_log!("[DJVU] Appending {} pages to container (one by one)", batch_files.len());

                            for (i, page_file) in batch_files.iter().enumerate() {
                                let mut cmd = Command::new(&djvm_path);
                                cmd.arg("-i").arg(&temp_output).arg(page_file);
                                cmd.current_dir(&self.pages_dir);

                                let page_label = if batch_files.len() == 1 {
                                    format!("djvm-append-single-{}", start_idx + i + 1)
                                } else {
                                    format!("djvm-append-{}-{}-{}", start_idx + 1, end_idx, i + 1)
                                };

                                self.run_tool_with_timeout(
                                    cmd,
                                    tracker,
                                    &page_label,
                                    DJVM_TIMEOUT,
                                    log_ref,
                                )?;

                                // Update progress for each page appended
                                if let Some(tr) = tracker {
                                    let current_progress = std::cmp::min(start_idx + i + 1, n);
                                    tr.update(crate::progress::ProcessingStatus::DjvuBundling {
                                        current: current_progress,
                                        total: n
                                    });
                                }
                            }

                            #[cfg(feature = "debug-logging")]
                            crate::success_log!("[DJVU] Batch pages appended one by one");
                        }

                        start_idx = end_idx;
                    }

                    // Move temp file to final location
                    #[cfg(feature = "debug-logging")]
                    crate::info_log!("[DJVU] Moving temp file to final location: {:?} -> {:?}", temp_output, output_path);

                    std::fs::rename(&temp_output, output_path)?;

                    #[cfg(feature = "debug-logging")]
                    crate::success_log!("[DJVU] File moved successfully");
                }

                // Update progress for all pages at once since batch operation completed
                if let Some(tr) = tracker {
                    tr.update(crate::progress::ProcessingStatus::DjvuBundling { current: n, total: n });
                }

                #[cfg(feature = "debug-logging")]
                {
                    println!("[DJVU] Bundling completed in {:.1}s", bundle_start.elapsed().as_secs_f32());
                    crate::success_log!("[DJVU] Bundling phase completed in {:.1}s", bundle_start.elapsed().as_secs_f32());
                }
            }
        }

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] Bundling complete. Verifying output file...");

        #[cfg(feature = "debug-logging")]
        {
            let djvm_path = self.resolve_executable("djvm")?;
            crate::info_log!("[DJVU] Running djvm -l to list pages...");
            let mut list = Command::new(&djvm_path);
            list.arg("-l").arg(output_path);
            self.run_tool_with_timeout(list, tracker, "djvm-list", DJVM_TIMEOUT, None)?;
            crate::success_log!("[DJVU] djvm -l completed");
        }

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] Checking if output file exists: {:?}", output_path);

        if !output_path.exists() {
            #[cfg(feature = "debug-logging")]
            crate::error_log!("[DJVU] Output file NOT FOUND: {:?}", output_path);

            return Err(anyhow!(
                "Output DJVU file was not created: {:?}",
                output_path
            ));
        }

        #[cfg(feature = "debug-logging")]
        crate::success_log!("[DJVU] Output file exists");

        // Show file size
        let size = fs::metadata(output_path)?.len();
        #[cfg(feature = "debug-logging")]
        {
            crate::info_log!("[DJVU] Bundling complete. Output: {:?}", output_path);
            crate::info_log!("[DJVU] Output size: {} bytes ({:.1} KB)", size, size as f64 / 1024.0);
            println!("Output size: {} bytes ({:.1} KB)", size, size as f64 / 1024.0);
        }

        // Mark progress complete to terminate heartbeat loop
        if let Some(tr) = tracker {
            tr.finish(&format!("Successfully assembled DJVU document"));
        }

        #[cfg(feature = "debug-logging")]
        crate::info_log!("[DJVU] Cleaning up DjVu processes...");

        // Best-effort: ensure any lingering external DjVu tool processes are terminated on Windows.
        // Users have reported occasional djvm.exe persistence; this cleanup is safe when nothing is running.
        self.terminate_djvu_processes_best_effort();

        #[cfg(feature = "debug-logging")]
        crate::success_log!("[DJVU] ========== assemble_djvu_with_progress COMPLETE ==========");

        Ok(())
    }

    /// On Windows, attempt to terminate any lingering DjVu tool processes that may persist rarely.
    /// No-ops on other platforms. Controlled by LEGE_SKIP_TASKKILL to opt-out.
    fn terminate_djvu_processes_best_effort(&self) {
        #[cfg(windows)]
        {
            use std::process::Command;
            if std::env::var("LEGE_SKIP_TASKKILL").is_ok() {
                dbglog!("[djvu] Skipping taskkill cleanup due to LEGE_SKIP_TASKKILL=1");
                return;
            }

            // Known executables we invoke; killing them is safe after our run completes
            let procs = [
                "djvm.exe",
                "djvumake.exe",
                "cjb2.exe",
                "c44.exe",
                "djvused.exe",
                "djvuextract.exe",
            ];

            for image in procs.iter() {
                let mut cmd = Command::new("taskkill");
                cmd.args(["/F", "/T", "/IM", image]);
                // Avoid flashing a window
                cmd.creation_flags(CREATE_NO_WINDOW);
                match cmd.output() {
                    Ok(out) => {
                        // Exit code 0 means killed; 128/255 if not found. Suppress noisy prints, keep debug log.
                        dbglog!(
                            "[djvu] taskkill {} -> status={:?} stderr200='{}'",
                            image,
                            out.status.code(),
                            String::from_utf8_lossy(&out.stderr).chars().take(200).collect::<String>()
                        );
                    }
                    Err(e) => {
                        dbglog!("[djvu] taskkill {} error: {}", image, e);
                    }
                }
            }
        }
    }

    /// Convert HOCR HTML to minimal DjVu text S-expression.
    /// Each OCR word becomes a line+word for simplicity.
    fn hocr_to_djvu_text(hocr: &str, target_w: u32, target_h: u32) -> Result<String> {
        // 1. Extract page bbox to determine scaling
        let page_re = regex::Regex::new(r#"<div[^>]*class=['"]ocr_page['"][^>]*title=['"][^'"]*bbox\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)"#)?;
        let (scale_x, scale_y) = if let Some(cap) = page_re.captures(hocr) {
            let pw: f32 = cap[3].parse::<f32>().unwrap_or(target_w as f32)
                - cap[1].parse::<f32>().unwrap_or(0.0);
            let ph: f32 = cap[4].parse::<f32>().unwrap_or(target_h as f32)
                - cap[2].parse::<f32>().unwrap_or(0.0);
            if pw > 1.0 && ph > 1.0 {
                (target_w as f32 / pw, target_h as f32 / ph)
            } else {
                (1.0, 1.0)
            }
        } else {
            (1.0, 1.0)
        };

        // 2. Regex for lines (capture inner HTML so we can extract words inside)
        let line_re = regex::Regex::new(r#"<span[^>]*class=['"]ocr_line['"][^>]*title=['"][^'"]*bbox\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)[^'"]*['"][^>]*>(.*?)</span>"#)?;
        let word_re = regex::Regex::new(r#"<span[^>]*class=['"]ocrx_word['"][^>]*title=['"][^'"]*bbox\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)[^'"]*['"][^>]*>([^<]+)</span>"#)?;

        #[derive(Debug)]
        struct Word {
            x1: i32,
            y1: i32,
            x2: i32,
            y2: i32,
            text: String,
        }
        #[derive(Debug)]
        struct Line {
            x1: i32,
            y1: i32,
            x2: i32,
            y2: i32,
            words: Vec<Word>,
        }

        let mut lines: Vec<Line> = Vec::new();

        for cap in line_re.captures_iter(hocr) {
            let lx1: i32 = cap[1].parse().unwrap_or(0);
            let ly1: i32 = cap[2].parse().unwrap_or(0);
            let lx2: i32 = cap[3].parse().unwrap_or(0);
            let ly2: i32 = cap[4].parse().unwrap_or(0);
            if lx2 <= lx1 || ly2 <= ly1 {
                continue;
            }
            let inner = &cap[5];
            let mut words: Vec<Word> = Vec::new();
            for wcap in word_re.captures_iter(inner) {
                let x1: i32 = wcap[1].parse().unwrap_or(0);
                let y1: i32 = wcap[2].parse().unwrap_or(0);
                let x2: i32 = wcap[3].parse().unwrap_or(0);
                let y2: i32 = wcap[4].parse().unwrap_or(0);
                if x2 <= x1 || y2 <= y1 {
                    continue;
                }
                let mut txt = wcap[5].trim().to_string();
                if txt.is_empty() {
                    continue;
                }
                // Normalize and clean text
                txt = txt.nfkc().collect::<String>();
                txt.retain(|c| !c.is_control() || c == '\t' || c == ' ');
                // Escape for DjVu
                txt = txt.replace('\\', "\\\\").replace('"', "\\\"");
                words.push(Word { x1, y1, x2, y2, text: txt });
            }
            if words.is_empty() {
                continue;
            }
            lines.push(Line {
                x1: lx1,
                y1: ly1,
                x2: lx2,
                y2: ly2,
                words,
            });
        }

        if lines.is_empty() {
            return Err(anyhow!("No OCR lines parsed from HOCR"));
        }

        // 3. Heuristic paragraph grouping based on vertical gaps
        // Compute median line height
        let mut heights: Vec<i32> = lines.iter().map(|l| l.y2 - l.y1).collect();
        heights.sort_unstable();
        let median_h = heights[heights.len() / 2].max(1);
        let mut paragraphs: Vec<Vec<usize>> = Vec::new();
        let mut current: Vec<usize> = Vec::new();
        let mut prev_bottom: Option<i32> = None;
        for (idx, l) in lines.iter().enumerate() {
            let gap = prev_bottom.map(|b| l.y1 - b).unwrap_or(0);
            if let Some(_pb) = prev_bottom {
                if gap as f32 > (median_h as f32 * 1.5) && !current.is_empty() {
                    paragraphs.push(current);
                    current = Vec::new();
                }
            }
            current.push(idx);
            prev_bottom = Some(l.y2);
        }
        if !current.is_empty() {
            paragraphs.push(current);
        }

        // 4. Emit DjVu S-expression
        let mut out = String::new();
        out.push_str(&format!("(page 0 0 {} {})\n", target_w, target_h));

        for para in &paragraphs {
            // Paragraph bbox
            let mut px1 = i32::MAX;
            let mut py1 = i32::MAX;
            let mut px2 = 0;
            let mut py2 = 0;
            for &li in para {
                let l = &lines[li];
                px1 = px1.min(l.x1);
                py1 = py1.min(l.y1);
                px2 = px2.max(l.x2);
                py2 = py2.max(l.y2);
            }
            // Scale coordinates
            let sx = |v: i32| ((v as f32) * scale_x).round() as i32;
            let sy = |v: i32| ((v as f32) * scale_y).round() as i32;
            out.push_str(&format!(
                "  (para {} {} {} {})\n",
                sx(px1),
                sy(py1),
                sx(px2),
                sy(py2)
            ));
            for &li in para {
                let l = &lines[li];
                out.push_str(&format!(
                    "    (line {} {} {} {}\n",
                    sx(l.x1),
                    sy(l.y1),
                    sx(l.x2),
                    sy(l.y2)
                ));
                for w in &l.words {
                    out.push_str(&format!(
                        "      (word {} {} {} {} \"{}\")\n",
                        sx(w.x1),
                        sy(w.y1),
                        sx(w.x2),
                        sy(w.y2),
                        w.text
                    ));
                }
                out.push_str("    )\n");
            }
            out.push_str("  )\n");
        }
        out.push_str(")\n");
        Ok(out)
    }

    /// Embed text layer into single-page DjVu using djvused.
    fn embed_text_layer(&self, page_file: &Path, text_expr_path: &Path) -> Result<()> {
        let djvused_path = self.resolve_executable("djvused")?;
        let expr = fs::read_to_string(text_expr_path)
            .with_context(|| format!("Failed to read text expression: {:?}", text_expr_path))?;
        let script = format!("select 1\nremove-txt\nset-txt\n{}\n.", expr);
        let script_path = page_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("embed_text.djs");
        fs::write(&script_path, &script)
            .with_context(|| format!("Failed to write script: {:?}", script_path))?;
        let output = Command::new(djvused_path)
            .arg(page_file)
            .arg("-f")
            .arg(&script_path)
            .arg("-s")
            .output()
            .context("Failed to execute djvused")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("djvused failed: {}", stderr));
        }
        Ok(())
    }

    /// Clean up temporary files
    pub fn cleanup(&self) -> Result<()> {
        dbglog!("[djvu] cleanup() called, work_dir={:?}, exists={}", self.config.work_dir, self.config.work_dir.exists());
        
        // Clean up only this run's working directory; do not scan/remove global bin
        if self.config.work_dir.exists() {
            dbglog!("[djvu] Removing work directory: {:?}", self.config.work_dir);
            fs::remove_dir_all(&self.config.work_dir).with_context(|| {
                format!("Failed to remove work directory: {:?}", self.config.work_dir)
            })?;
            dbglog!("[djvu] Work directory removed successfully");
        } else {
            dbglog!("[djvu] Work directory doesn't exist, nothing to clean");
        }
        Ok(())
    }

    /// Get the number of pages processed
    pub fn page_count(&self) -> usize {
        self.assembled_pages.lock().unwrap().len()
    }

    /// Preflight check to ensure required external executables are available.
    /// If OCR/text layer embedding is enabled, also require djvused.
    /// Returns Err with unified missing component list if any are not found.
    pub fn preflight_check(&self, require_text_layer: bool) -> Result<()> {
        let mut missing: Vec<&str> = Vec::new();
        let base_tools = ["cjb2", "c44", "djvumake", "djvm"];
        for tool in base_tools.iter() {
            if self.resolve_executable(tool).is_err() { missing.push(tool); }
        }
        if require_text_layer {
            if self.resolve_executable("djvused").is_err() { missing.push("djvused"); }
        }
        if !missing.is_empty() {
            // Provide a single unified error message (GUI will surface this directly)
            return Err(anyhow!(
                "Missing required component(s): {} (ensure djvulibre tools are bundled in ./bin or set LEGE_DJVU_BIN)",
                missing.join(", ")
            ));
        }
        // Functional cjb2 smoke test (8x8 PBM, 8-byte bitmap for clarity)
        let test_pbm = self.config.work_dir.join("__cjb2_test.pbm");
        let test_out = self.config.work_dir.join("__cjb2_test.djvu");
        if !test_out.exists() { // only once per run-directory
            fs::create_dir_all(&self.config.work_dir).ok();
            // 8x8 PBM (binary) header + 8 bytes of pixel data (patterned)
            // Pattern: alternating upper/lower bits to exercise encoder
            let pbm_content: &[u8] = b"P4\n8 8\n\xAA\x55\xAA\x55\xF0\x0F\x00\xFF"; // 8 bytes follow header
            if let Err(e) = fs::write(&test_pbm, pbm_content) {
                return Err(anyhow!("Preflight write test PBM failed: {}", e));
            }
            // Provide extra diagnostics if cjb2 errors out
            let cjb2_path_res = self.resolve_executable("cjb2");
            let cjb2_path = match cjb2_path_res {
                Ok(p) => p,
                Err(e) => return Err(anyhow!("cjb2 not found after initial presence check: {}", e)),
            };
            match self.encode_background(&test_pbm, &test_out, 8, 8) {
                Ok(_) => { /* success */ }
                Err(e) => {
                    let hex_preview = {
                        let raw = pbm_content;
                        raw.iter().take(32).map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ")
                    };
                    return Err(anyhow!(
                        "cjb2 functional test failed: {} (exe='{:?}' pbm='{}' size={} hex={})\nAdvice: Ensure djvulibre tools are bundled and executable. Try running: '{:?} -dpi 300 {} {}'",
                        e,
                        cjb2_path,
                        test_pbm.display(),
                        pbm_content.len(),
                        hex_preview,
                        cjb2_path,
                        test_pbm.display(),
                        test_out.display()
                    ));
                }
            }
        }
        Ok(())
    }

}
