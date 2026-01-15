// src/ocr/mod.rs
// Unified OCR interface for Windows (winocr) and Linux (tesseract)


pub mod ocr;

use std::env;
use std::path:: {Path, PathBuf};
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
) -> Option<OcrResult> {
    winocr::run_winocr(image_data, width, height, is_binary)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn run_ocr(
    image_data: &[u8],
    width: usize,
    height: usize,
    is_binary: bool,
) -> Option<OcrResult> {
    tesseract::run_tesseract(image_data, width, height, is_binary)
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn run_ocr(
    _image_data: &[u8],
    _width: usize,
    _height: usize,
    _is_binary: bool,
) -> Option<OcrResult> {
    None
}

/// Check if Tesseract is available on Linux/macOS systems
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn check_tesseract_availability() -> Result<String, String> {
    // First check if we have local eng.traineddata in Lege directory
    let local_traineddata = check_local_traineddata();

    // Method 1: Check if tesseract command is available in PATH
    let tesseract_binary = if let Ok(output) = Command::new("tesseract").arg("--version").output() {
        if output.status.success() {
            let version_info = String::from_utf8_lossy(&output.stdout);
            let version_line = version_info.lines().next().unwrap_or("version unknown");
            Some(format!("Tesseract found in PATH: {}", version_line))
        } else {
            None
        }
    } else {
        None
    };

    // If no tesseract binary found in PATH, check common installation paths
    let tesseract_binary = if tesseract_binary.is_none() {
        let common_paths = [
            "/usr/bin/tesseract",                       // apt/deb packages
            "/usr/local/bin/tesseract",                 // manual install
            "/snap/bin/tesseract",                      // snap packages
            "/home/linuxbrew/.linuxbrew/bin/tesseract", // homebrew
            "/opt/homebrew/bin/tesseract",              // homebrew (Apple Silicon)
        ];

        let mut found_binary = None;
        for path in &common_paths {
            if Path::new(path).exists() {
                // Try to get version from this specific path
                if let Ok(output) = Command::new(path).arg("--version").output() {
                    if output.status.success() {
                        let version_info = String::from_utf8_lossy(&output.stdout);
                        let version_line = version_info.lines().next().unwrap_or("version unknown");
                        found_binary =
                            Some(format!("Tesseract found at {}: {}", path, version_line));
                        break;
                    }
                }
            }
        }
        found_binary
    } else {
        tesseract_binary
    };

    // Now determine the final result based on what we found
    match (tesseract_binary, local_traineddata) {
        (Some(binary_info), Some(local_data)) => Ok(format!(
            "{} with custom eng.traineddata at {}",
            binary_info,
            local_data.display()
        )),
        (Some(binary_info), None) => {
            // Check if system has eng.traineddata
            if has_system_traineddata() {
                Ok(format!("{} with system tessdata", binary_info))
            } else {
                Err(format!(
                    "{} but no eng.traineddata found. \
                    Place eng.traineddata in Lege directory or install with: sudo apt install tesseract-ocr-eng",
                    binary_info
                ))
            }
        }
        (None, Some(local_data)) => Err(format!(
            "Custom eng.traineddata found at {} but no Tesseract binary. \
                Install with: sudo apt install tesseract-ocr",
            local_data.display()
        )),
        (None, None) => {
            // Continue with existing fallback detection logic...
            check_for_partial_installation()
        }
    }
}

/// Check if system has eng.traineddata in standard locations
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn has_system_traineddata() -> bool {
    let data_paths = [
        "/usr/share/tesseract-ocr/tessdata/eng.traineddata",
        "/usr/share/tessdata/eng.traineddata",
        "/usr/local/share/tessdata/eng.traineddata",
    ];

    data_paths.iter().any(|path| Path::new(path).exists())
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
    // First check for local eng.traineddata in Lege directory
    if let Some(local_traineddata) = check_local_traineddata() {
        // Return the directory containing the local eng.traineddata
        if let Some(parent) = local_traineddata.parent() {
            return Some(parent.to_string_lossy().to_string());
        }
    }

    // Fall back to system tessdata directories
    let system_paths = [
        "/usr/share/tesseract-ocr/tessdata/",
        "/usr/share/tessdata/",
        "/usr/local/share/tessdata/",
    ];

    for path in &system_paths {
        let eng_path = Path::new(path).join("eng.traineddata");
        if eng_path.exists() {
            return Some(path.to_string());
        }
    }

    None
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

/// Check for local eng.traineddata in Lege program directory
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn check_local_traineddata() -> Option<PathBuf> {
    // Get the directory where the Lege executable is located
    let exe_path = env::current_exe().ok()?;
    let exe_dir = exe_path.parent()?;

    // Check for eng.traineddata in the same directory as the executable
    let local_traineddata = exe_dir.join("eng.traineddata");
    if local_traineddata.exists() {
        Some(local_traineddata)
    } else {
        // Also check current working directory as fallback
        let cwd_traineddata = PathBuf::from("eng.traineddata");
        if cwd_traineddata.exists() {
            Some(cwd_traineddata)
        } else {
            None
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn check_tesseract_availability() -> Result<String, String> {
    Err("OCR not supported on this platform".to_string())
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn get_tessdata_path() -> Option<String> {
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
    match run_ocr(&final_image_data, width, height, is_binary) {
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
