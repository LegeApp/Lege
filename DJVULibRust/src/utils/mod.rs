//! General-purpose utility modules.

pub mod color_checker;
pub mod error;
pub mod file_path;
pub mod log;
pub mod progress;
pub mod write_ext;

// Re-export commonly used items
pub use error::{DjvuError, Result};
