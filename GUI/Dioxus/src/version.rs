//! Version management for Lege GUI
//!
//! Handles the separation between internal development version and external user-facing version.

use std::env;
use std::fs;

// Internal version from Cargo.toml
const INTERNAL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns the external version string (e.g., "1.1.4.0")
pub fn display_version() -> &'static str {
    // Try multiple methods to get the external version, in order of preference
    // 1. Try to get from the main lege crate (which should have read from the file via build.rs)
    match lege::get_external_version() {
        "0.0.0" | "" | "1.0.0" => {  // Add common placeholder versions
            // 2. Try to read from the version file directly during runtime
            if let Ok(content) = read_external_version_file() {
                if !content.is_empty() && content != "0.0.0" {
                    return Box::leak(content.into_boxed_str());
                }
            }
            
            // 3. Fallback to environment variable set by build.rs
            match option_env!("LEGE_EXTERNAL_VERSION") {
                Some(external_version) if !external_version.is_empty() => external_version,
                _ => INTERNAL_VERSION,
            }
        },
        version => version,
    }
}

/// Read external version from the file in the project root at runtime
fn read_external_version_file() -> Result<String, Box<dyn std::error::Error>> {
    // Try different possible locations for the version file
    let possible_paths = [
        "../../../external_version.txt", // From GUI/Dioxus/src/
        "../../external_version.txt",   // From GUI/Dioxus/src (alternative)
        "../external_version.txt",      // From GUI/Dioxus/
        "./external_version.txt",       // From current dir
        "external_version.txt",         // From relative path
    ];

    for path_str in &possible_paths {
        let path = std::path::Path::new(path_str);
        if path.exists() {
            if let Ok(content) = fs::read_to_string(path) {
                let content = content.trim();
                if !content.is_empty() && content != "0.0.0" {
                    return Ok(content.to_string());
                }
            }
        }
    }
    
    Err("Version file not found".into())
}

/// Returns the internal version string (e.g., "0.7.0")
#[cfg(test)]
pub fn internal_version() -> &'static str {
    INTERNAL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_versions() {
        // Just verify the functions don't panic
        let _ = display_version();
        let _ = internal_version();
    }
}
