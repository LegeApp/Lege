//! Android platform support.
//!
//! Everything Android-specific in `lege-process` lives under this directory and
//! compiles only with the `android` feature. Shared code reaches it through
//! short delegations rather than inline `cfg` branches, so the desktop build
//! is unaffected and mobile pathing stays in one place.
//!
//! Building for Android without the feature is rejected by a `compile_error!`
//! in `core/lib.rs`: several shared fallbacks (the hardcoded 8 GB in
//! `get_available_ram_gb`, `data_dir`'s `PathBuf::from(".")`) compile fine on
//! Android and then misbehave silently, so the feature must not be optional.

pub mod logging;
pub mod platform;

pub use platform::{AndroidEnv, available_ram_gb, cache_dir, data_dir, env, init};
