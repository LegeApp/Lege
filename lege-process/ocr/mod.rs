// src/ocr/mod.rs
// OCR interface — delegates to lege-ocr for all engine and pipeline logic.
//
// Four mutually exclusive implementations of the same small interface follow:
// Tesseract discovery on Linux/macOS, the Windows built-in OCR, a
// feature-disabled stub, and an unsupported-platform stub. Each lives in its
// own `#[cfg]`-gated module and is re-exported so callers see one flat API.

pub mod fast;
pub mod slow;

/// Re-export the shared `OcrResult` type from lege-ocr.
pub use lege_ocr::OcrResult;

/// Run the platform OCR engine on raw image data and return hOCR + plain text.
///
/// `is_binary` = true when `image_data` is 1bpp (0=ink, 255=background).
/// Delegates to `lege_ocr::engine::default_engine().run_image(…)`.
pub fn run_ocr(
    image_data: &[u8],
    width: usize,
    height: usize,
    is_binary: bool,
    language: &str,
) -> Option<OcrResult> {
    lege_ocr::engine::default_engine().run_image(image_data, width, height, is_binary, language)
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "tesseract-ocr"
))]
pub use tesseract::*;

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "tesseract-ocr"
))]
mod tesseract {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Directories searched for `<language>.traineddata`, in priority order.
    const SYSTEM_TESSDATA_DIRS: [&str; 4] = [
        "/usr/share/tesseract-ocr/tessdata/",
        "/usr/share/tessdata/",
        "/usr/local/share/tessdata/",
        "/usr/share/lege/tessdata/",
    ];

    /// Check if Tesseract is available on Linux/macOS systems
    pub fn check_tesseract_availability() -> Result<String, String> {
        check_tesseract_availability_for_language("eng")
    }

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
        for data_path in &SYSTEM_TESSDATA_DIRS {
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
    pub fn get_tessdata_path() -> Option<String> {
        get_tessdata_path_for_language("eng")
    }

    pub fn get_tessdata_path_for_language(language: &str) -> Option<String> {
        let normalized = language.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return None;
        }
        find_traineddata_path(&normalized).and_then(|traineddata| {
            traineddata
                .parent()
                .map(|p| p.to_string_lossy().to_string())
        })
    }

    fn find_tesseract_binary() -> Option<String> {
        if let Some(version_line) = tesseract_version_line(Path::new("tesseract")) {
            return Some(format!("Tesseract found in PATH: {}", version_line));
        }

        let common_paths = [
            "/usr/bin/tesseract",
            "/usr/local/bin/tesseract",
            "/snap/bin/tesseract",
            "/home/linuxbrew/.linuxbrew/bin/tesseract",
            "/opt/homebrew/bin/tesseract",
        ];

        for path in &common_paths {
            let path = Path::new(path);
            if path.exists()
                && let Some(version_line) = tesseract_version_line(path)
            {
                return Some(format!(
                    "Tesseract found at {}: {}",
                    path.display(),
                    version_line
                ));
            }
        }
        None
    }

    /// Run `<binary> --version` and return its first line, or `None` when the
    /// binary is missing or reports failure.
    fn tesseract_version_line(binary: &Path) -> Option<String> {
        let output = Command::new(binary).arg("--version").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let version_info = String::from_utf8_lossy(&output.stdout);
        Some(
            version_info
                .lines()
                .next()
                .unwrap_or("version unknown")
                .to_string(),
        )
    }

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

        if let Ok(exe_path) = env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            push_unique(exe_dir.to_path_buf());
            push_unique(exe_dir.join("tessdata"));
        }

        if let Ok(cwd) = env::current_dir() {
            push_unique(cwd.clone());
            push_unique(cwd.join("tessdata"));
        }

        for path in SYSTEM_TESSDATA_DIRS {
            push_unique(PathBuf::from(path));
        }

        dirs
    }

    fn find_traineddata_path(language: &str) -> Option<PathBuf> {
        let filename = format!("{}.traineddata", language);
        tessdata_search_dirs()
            .into_iter()
            .map(|dir| dir.join(&filename))
            .find(|candidate| candidate.is_file())
    }
}

#[cfg(target_os = "windows")]
pub use windows_ocr::*;

#[cfg(target_os = "windows")]
mod windows_ocr {
    // Windows uses Windows OCR automatically, so there is nothing to discover
    // and no tessdata to locate.
    const AVAILABLE: &str = "Windows OCR available (no Tesseract required)";

    pub fn check_tesseract_availability() -> Result<String, String> {
        Ok(AVAILABLE.to_string())
    }

    pub fn check_tesseract_availability_for_language(_language: &str) -> Result<String, String> {
        Ok(AVAILABLE.to_string())
    }

    pub fn get_tessdata_path() -> Option<String> {
        None
    }

    pub fn get_tessdata_path_for_language(_language: &str) -> Option<String> {
        None
    }
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    not(feature = "tesseract-ocr")
))]
pub use ocr_not_compiled::*;

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    not(feature = "tesseract-ocr")
))]
mod ocr_not_compiled {
    const UNAVAILABLE: &str = "OCR support was not compiled into this build";

    pub fn check_tesseract_availability() -> Result<String, String> {
        Err(UNAVAILABLE.to_string())
    }

    pub fn check_tesseract_availability_for_language(_language: &str) -> Result<String, String> {
        Err(UNAVAILABLE.to_string())
    }

    pub fn get_tessdata_path() -> Option<String> {
        None
    }

    pub fn get_tessdata_path_for_language(_language: &str) -> Option<String> {
        None
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub use ocr_unsupported::*;

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
mod ocr_unsupported {
    const UNSUPPORTED: &str = "OCR not supported on this platform";

    pub fn check_tesseract_availability() -> Result<String, String> {
        Err(UNSUPPORTED.to_string())
    }

    pub fn check_tesseract_availability_for_language(_language: &str) -> Result<String, String> {
        Err(UNSUPPORTED.to_string())
    }

    pub fn get_tessdata_path() -> Option<String> {
        None
    }

    pub fn get_tessdata_path_for_language(_language: &str) -> Option<String> {
        None
    }
}
