// src/log.rs

//! A structured logging system for the DjVu library.
//!
//! This replaces the C++ `debug.h` macros with the `tracing` crate, providing
//! level-based, structured, and context-aware logging.
//!
//! # Usage
//!
//! Add `tracing` and `tracing-subscriber` to your `Cargo.toml`.
//! Before using the library, initialize the subscriber:
//!
//! ```ignore
//! // In your main.rs or a test setup
//! djvu_encoder::utils::log::init_subscriber(tracing::Level::DEBUG);
//! ```
//!
//! Then use the logging macros throughout the code:
//! `trace!`, `debug!`, `info!`, `warn!`, `error!`.
//! The `instrument` attribute can be used to trace function entry/exit.
//!
//! Example:
//! ```

//! use tracing::{debug, instrument};
//!
//! #[instrument]
//! fn my_function(arg: i32) {
//!     debug!(argument = arg, "Starting computation.");
//!     // ...
//! }
//! ```

pub use tracing::{debug, error, info, instrument, span, trace, warn, Level};
use tracing_subscriber::FmtSubscriber;

/// Initializes a global logging subscriber.
///
/// This should be called once at the beginning of the program's execution.
/// It sets up a simple subscriber that logs messages to standard error.
///
/// # Arguments
/// * `max_level` - The maximum level of messages to log (e.g., `Level::INFO`, `Level::DEBUG`).
pub fn init_subscriber(max_level: Level) {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(max_level)
        .with_thread_ids(true) // Replicates the thread ID logging from the C++ version
        .with_target(false) // Don't print the module path
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("Setting default tracing subscriber failed");
}
