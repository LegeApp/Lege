//! Dioxus GUI implementation for Reduced Lege

#![cfg_attr(not(feature = "gui"), allow(unused_imports))]

pub mod app;
pub mod backend;
pub mod gui_text;
pub mod logging;
pub mod models;
pub mod settings;
pub mod static_styles;
pub mod theme;
pub mod version;

// Re-export commonly used types
#[cfg(feature = "gui")]
pub use dioxus::prelude::*;

/// Main entry point for the GUI application
#[cfg(feature = "gui")]
pub fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    launch(app::App);
    Ok(())
}

#[cfg(not(feature = "gui"))]
pub fn main() -> anyhow::Result<()> {
    println!("GUI feature is not enabled. Please run with --features=gui");
    Ok(())
}
