//! Version management for Lege
//!
//! Handles the separation between internal development version and external user-facing version.

use std::env;

// Internal version from Cargo.toml
const INTERNAL_VERSION: &str = env!("CARGO_PKG_VERSION");

// External version for user display (can be overridden at build time)
const EXTERNAL_VERSION: &str = env!(
    "LEGE_EXTERNAL_VERSION",
    "External version not set at build time, using internal version"
);

/// Returns the external version string (e.g., "1.1.4.0")
pub fn external_version() -> &'static str {
    if EXTERNAL_VERSION == "External version not set at build time" {
        INTERNAL_VERSION
    } else {
        EXTERNAL_VERSION
    }
}

/// Returns the internal version string (e.g., "0.7.0")
pub fn internal_version() -> &'static str {
    INTERNAL_VERSION
}

/// Returns the version string to display to users
/// This will be the external version if set, otherwise falls back to internal version
pub fn display_version() -> &'static str {
    external_version()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_versions() {
        // Just verify the functions don't panic
        let _ = external_version();
        let _ = internal_version();
        let _ = display_version();
    }
}
