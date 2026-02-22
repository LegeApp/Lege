//! Simple logging utilities for the DjVu library.
//!
//! Re-exports the `log` crate for structured logging.
//!
//! # Usage
//!
//! Use the standard log macros throughout the code:
//! `trace!`, `debug!`, `info!`, `warn!`, `error!`.
//!
//! Example:
//! ```
//! use log::debug;
//!
//! fn my_function(arg: i32) {
//!     debug!("Starting computation with argument: {}", arg);
//!     // ...
//! }
//! ```

pub use log::{debug, error, info, trace, warn, Level};

/// Initialize logging (no-op, applications should set up their own logger).
///
/// Applications using this library should initialize their own logging backend.
/// For example, with `env_logger`:
///
/// ```ignore
/// env_logger::init();
/// ```
pub fn init_subscriber(_max_level: Level) {
    // No-op - applications should initialize their own logging backend
    // This function is kept for API compatibility
}

