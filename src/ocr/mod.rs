// src/ocr/mod.rs
// Unified OCR interface for Windows (winocr) and Linux (tesseract)

pub mod ocr;

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod tesseract;
#[cfg(target_os = "windows")]
mod winocr;

#[derive(Clone, Debug)]
pub struct OcrResult {
    pub hocr: String,
    pub plain_text: String,
}

#[cfg(target_os = "windows")]
pub fn run_ocr(
    image_data: &[u8],
    width: usize,
    height: usize,
    is_binary: bool,
    _language: &str,
) -> Option<OcrResult> {
    winocr::run_winocr(image_data, width, height, is_binary)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn run_ocr(
    image_data: &[u8],
    width: usize,
    height: usize,
    is_binary: bool,
    language: &str,
) -> Option<OcrResult> {
    tesseract::run_tesseract(image_data, width, height, is_binary, language)
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn run_ocr(
    _image_data: &[u8],
    _width: usize,
    _height: usize,
    _is_binary: bool,
    _language: &str,
) -> Option<OcrResult> {
    None
}

/// Check if Tesseract is available on Linux/macOS systems
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn check_tesseract_availability() -> Result<String, String> {
    check_tesseract_availability_for_language("eng")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn check_tesseract_availability_for_language(language: &str) -> Result<String, String> {
    let normalized = language.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err("OCR language cannot be empty".to_string());
    }

    let tesseract_binary = find_tesseract_binary();
    let traineddata = find_traineddata_path(&normalized);

    match (tesseract_binary, traineddata) {
        (Some(binary_info), Some(data_path)) => Ok(format!(
            "{} with {}.traineddata at {}",
            binary_info,
            normalized,
            data_path.display()
        )),
        (Some(binary_info), None) => {
            let searched = tessdata_search_dirs()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "{} but no {}.traineddata found. Expected in one of: {}",
                binary_info, normalized, searched
            ))
        }
        (None, Some(data_path)) => Err(format!(
            "Found {}.traineddata at {} but Tesseract binary is not accessible. \
             Install with: sudo apt install tesseract-ocr",
            normalized,
            data_path.display()
        )),
        (None, None) => check_for_partial_installation(),
    }
}

/// Check for partial Tesseract installation (libraries/data without binary)
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn check_for_partial_installation() -> Result<String, String> {
    // Check for library files (indicates installation)
    let lib_paths = [
        "/usr/lib/x86_64-linux-gnu/libtesseract.so",
        "/usr/lib/libtesseract.so",
        "/usr/local/lib/libtesseract.so",
        "/usr/lib64/libtesseract.so",
    ];

    for lib_path in &lib_paths {
        if Path::new(lib_path).exists() {
            return Err(format!(
                "Tesseract library found at {} but binary not accessible. \
                Try: sudo apt install tesseract-ocr or add Tesseract to your PATH",
                lib_path
            ));
        }
    }

    // Check for tessdata directories (indicates partial installation)
    let data_paths = [
        "/usr/share/tesseract-ocr/tessdata/",
        "/usr/share/tessdata/",
        "/usr/local/share/tessdata/",
        "/usr/share/lege/tessdata/",
    ];

    for data_path in &data_paths {
        if Path::new(data_path).exists() {
            return Err(format!(
                "Tesseract data found at {} but binary not accessible. \
                Try: sudo apt install tesseract-ocr or add Tesseract to your PATH",
                data_path
            ));
        }
    }

    Err(
        "Tesseract not found. Install with: sudo apt install tesseract-ocr libtesseract-dev"
            .to_string(),
    )
}

/// Get the appropriate tessdata directory path, preferring local eng.traineddata
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn get_tessdata_path() -> Option<String> {
    get_tessdata_path_for_language("eng")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn get_tessdata_path_for_language(language: &str) -> Option<String> {
    let normalized = language.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    find_traineddata_path(&normalized)
        .and_then(|traineddata| traineddata.parent().map(|p| p.to_string_lossy().to_string()))
}

#[cfg(target_os = "windows")]
pub fn check_tesseract_availability() -> Result<String, String> {
    // Windows uses Windows OCR automatically, no setup required
    Ok("Windows OCR available (no Tesseract required)".to_string())
}

#[cfg(target_os = "windows")]
pub fn get_tessdata_path() -> Option<String> {
    // Windows uses Windows OCR, no tessdata needed
    None
}

#[cfg(target_os = "windows")]
pub fn check_tesseract_availability_for_language(_language: &str) -> Result<String, String> {
    Ok("Windows OCR available (no Tesseract required)".to_string())
}

#[cfg(target_os = "windows")]
pub fn get_tessdata_path_for_language(_language: &str) -> Option<String> {
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn find_tesseract_binary() -> Option<String> {
    if let Ok(output) = Command::new("tesseract").arg("--version").output() {
        if output.status.success() {
            let version_info = String::from_utf8_lossy(&output.stdout);
            let version_line = version_info.lines().next().unwrap_or("version unknown");
            return Some(format!("Tesseract found in PATH: {}", version_line));
        }
    }

    let common_paths = [
        "/usr/bin/tesseract",
        "/usr/local/bin/tesseract",
        "/snap/bin/tesseract",
        "/home/linuxbrew/.linuxbrew/bin/tesseract",
        "/opt/homebrew/bin/tesseract",
    ];

    for path in &common_paths {
        if Path::new(path).exists() {
            if let Ok(output) = Command::new(path).arg("--version").output() {
                if output.status.success() {
                    let version_info = String::from_utf8_lossy(&output.stdout);
                    let version_line = version_info.lines().next().unwrap_or("version unknown");
                    return Some(format!("Tesseract found at {}: {}", path, version_line));
                }
            }
        }
    }
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn tessdata_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push_unique = |path: PathBuf| {
        if !dirs.iter().any(|p| p == &path) {
            dirs.push(path);
        }
    };

    if let Ok(tess_prefix) = env::var("TESSDATA_PREFIX") {
        let prefix = PathBuf::from(tess_prefix);
        push_unique(prefix.clone());
        push_unique(prefix.join("tessdata"));
    }

    if let Ok(exe_path) = env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            push_unique(exe_dir.to_path_buf());
            push_unique(exe_dir.join("tessdata"));
        }
    }

    if let Ok(cwd) = env::current_dir() {
        push_unique(cwd.clone());
        push_unique(cwd.join("tessdata"));
    }

    for path in [
        "/usr/share/tesseract-ocr/tessdata/",
        "/usr/share/tessdata/",
        "/usr/local/share/tessdata/",
        "/usr/share/lege/tessdata/",
    ] {
        push_unique(PathBuf::from(path));
    }

    dirs
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn find_traineddata_path(language: &str) -> Option<PathBuf> {
    let filename = format!("{}.traineddata", language);
    for dir in tessdata_search_dirs() {
        let candidate = dir.join(&filename);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn check_tesseract_availability() -> Result<String, String> {
    Err("OCR not supported on this platform".to_string())
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn get_tessdata_path() -> Option<String> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn check_tesseract_availability_for_language(_language: &str) -> Result<String, String> {
    Err("OCR not supported on this platform".to_string())
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn get_tessdata_path_for_language(_language: &str) -> Option<String> {
    None
}

pub fn extract_text_layer(
    image_data: &[u8],
    width: usize,
    height: usize,
    figure_mask: &Option<Vec<u8>>,
) -> Result<Option<String>, anyhow::Error> {
    // Validate inputs to prevent OCR from failing
    if width == 0 || height == 0 {
        return Ok(None); // Empty image, no text to extract
    }

    if image_data.is_empty() {
        return Ok(None); // No image data
    }

    // Reasonable size limits to prevent memory issues
    const MAX_DIMENSION: usize = 50000; // 50k pixels per dimension
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(anyhow::anyhow!(
            "Image dimensions too large for OCR: {}x{} (max: {}x{})",
            width,
            height,
            MAX_DIMENSION,
            MAX_DIMENSION
        ));
    }

    // Determine if this is binary data (grayscale) or RGB data
    let expected_rgb_len = width * height * 3;
    let expected_binary_len = width * height;

    let is_binary = match image_data.len() {
        len if len == expected_binary_len => true,
        len if len == expected_rgb_len => false,
        _ => {
            return Err(anyhow::anyhow!(
                "Invalid image data length: got {}, expected {} (RGB) or {} (binary) for {}x{}",
                image_data.len(),
                expected_rgb_len,
                expected_binary_len,
                width,
                height
            ));
        }
    };

    let final_image_data = if let Some(_mask) = figure_mask {
        // Apply mask if provided - for now just use original data
        image_data.to_vec()
    } else {
        image_data.to_vec()
    };

    // Enhanced OCR call with better error handling
    match run_ocr(&final_image_data, width, height, is_binary, "eng") {
        Some(ocr_result) => {
            // Validate the OCR result
            if ocr_result.hocr.is_empty() && ocr_result.plain_text.is_empty() {
                // OCR succeeded but found no text - this is normal
                Ok(None)
            } else {
                // OCR succeeded and found text
                Ok(Some(ocr_result.hocr))
            }
        }
        None => {
            // OCR failed completely - this should never happen on Windows with our robust implementation
            // Return empty result instead of failing the entire processing pipeline
            #[cfg(feature = "debug-logging")]
            eprintln!(
                "Warning: OCR failed for {}x{} image, continuing without text layer",
                width, height
            );
            Ok(None)
        }
    }
}
