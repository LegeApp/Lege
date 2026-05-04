#![cfg(target_os = "windows")]
#![allow(dead_code)]

mod windows_impl {
    use anyhow::{Context, Result};
    use std::env;
    use std::path::PathBuf;

    pub fn get_user_data_dir() -> Result<PathBuf> {
        let data_dir = env::var("LOCALAPPDATA")
            .or_else(|_| env::var("APPDATA"))
            .map(PathBuf::from)
            .context("Could not find LOCALAPPDATA or APPDATA environment variable")?
            .join("Lege");

        Ok(data_dir)
    }

    pub fn get_webview_data_dir() -> Result<PathBuf> {
        Ok(get_user_data_dir()?.join("WebView2"))
    }

    pub fn get_cache_dir() -> Result<PathBuf> {
        let cache_dir = env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .context("Could not find LOCALAPPDATA environment variable")?
            .join("Lege")
            .join("Cache");

        Ok(cache_dir)
    }

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

    pub fn get_install_dir() -> Result<PathBuf> {
        if let Ok(exe_path) = env::current_exe() {
            if let Some(parent) = exe_path.parent() {
                return Ok(parent.to_path_buf());
            }
        }

        Ok(PathBuf::from("C:\\Program Files\\Lege"))
    }

    pub fn get_install_file<P: AsRef<std::path::Path>>(filename: P) -> Result<PathBuf> {
        Ok(get_install_dir()?.join(filename))
    }
}

pub use windows_impl::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_data_dir() {
        let dir = get_user_data_dir().unwrap();
        assert!(dir.to_string_lossy().contains("Lege"));
    }

    #[test]
    fn test_webview_data_dir() {
        let dir = get_webview_data_dir().unwrap();
        assert!(dir.to_string_lossy().ends_with("WebView2"));
    }
}