// Freya-side copy of the Dioxus GUI version module.
// Keep in sync manually until GUI support code is consolidated.

//! Version management for Lege GUI
//!
//! Handles the separation between internal development version and external user-facing version.

// Internal version from Cargo.toml
const INTERNAL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns the external version string (e.g., "1.1.4.0")
pub fn display_version() -> &'static str {
    match option_env!("LEGE_EXTERNAL_VERSION") {
        Some(external_version) if !external_version.is_empty() && external_version != "0.0.0" => {
            external_version
        }
        _ => INTERNAL_VERSION,
    }
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
