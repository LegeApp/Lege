//! General-purpose utility modules.

pub mod error;
pub mod log;
pub mod color_checker;
pub mod progress;
pub mod write_ext;

// Re-export commonly used items
pub use error::{DjvuError, Result};
