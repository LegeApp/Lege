use anyhow::{Context, Result};
use std::env;
use std::path::PathBuf;

/// Get the proper user data directory for Lege application data
/// Uses %LOCALAPPDATA%\Lege on Windows, following Microsoft best practices
pub fn get_user_data_dir() -> Result<PathBuf> {
    let data_dir = if cfg!(windows) {
        // Use LOCALAPPDATA for user-specific application data on Windows
        env::var("LOCALAPPDATA")
            .or_else(|_| env::var("APPDATA"))
            .map(PathBuf::from)
            .context("Could not find LOCALAPPDATA or APPDATA environment variable")?
            .join("Lege")
    } else {
        // For other platforms, use directories crate
        directories::ProjectDirs::from("com", "Lege", "Lege")
            .context("Could not determine user data directory")?
            .data_dir()
            .to_path_buf()
    };

    Ok(data_dir)
}

/// Get the WebView2 user data directory
/// This should be separate from general app data to avoid conflicts
pub fn get_webview_data_dir() -> Result<PathBuf> {
    Ok(get_user_data_dir()?.join("WebView2"))
}

/// Get the cache directory for temporary files
pub fn get_cache_dir() -> Result<PathBuf> {
    let cache_dir = if cfg!(windows) {
        // Use LOCALAPPDATA for cache on Windows
        env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .context("Could not find LOCALAPPDATA environment variable")?
            .join("Lege")
            .join("Cache")
    } else {
        directories::ProjectDirs::from("com", "Lege", "Lege")
            .context("Could not determine cache directory")?
            .cache_dir()
            .to_path_buf()
    };

    Ok(cache_dir)
}

/// Ensure all required directories exist
pub fn ensure_directories() -> Result<()> {
    let dirs = [
        get_user_data_dir()?,
        get_webview_data_dir()?,
        get_cache_dir()?,
    ];

    for dir in &dirs {
        if !dir.exists() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Failed to create directory: {}", dir.display()))?;
        }
    }

    Ok(())
}

/// Get the install directory (where executables are located)
pub fn get_install_dir() -> Result<PathBuf> {
    if cfg!(windows) {
        // Try to get from executable path first
        if let Ok(exe_path) = env::current_exe() {
            if let Some(parent) = exe_path.parent() {
                return Ok(parent.to_path_buf());
            }
        }

        // Fallback to Program Files
        Ok(PathBuf::from("C:\\Program Files\\Lege"))
    } else {
        // For other platforms, use current exe directory
        env::current_exe()
            .context("Could not determine executable path")?
            .parent()
            .map(|p| p.to_path_buf())
            .context("Could not determine install directory")
    }
}

/// Get path to a data file that should be in the install directory
pub fn get_install_file<P: AsRef<std::path::Path>>(filename: P) -> Result<PathBuf> {
    Ok(get_install_dir()?.join(filename))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_data_dir() {
        let dir = get_user_data_dir().unwrap();
        if cfg!(windows) {
            assert!(dir.to_string_lossy().contains("Lege"));
        }
    }

    #[test]
    fn test_webview_data_dir() {
        let dir = get_webview_data_dir().unwrap();
        assert!(dir.to_string_lossy().ends_with("WebView2"));
    }
}
