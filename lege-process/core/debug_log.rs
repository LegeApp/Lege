/// Simplified unified debug logging.
///
/// Requirements (per user directive):
/// - The cargo features `debug-logging` OR `djvu_debug` enable debug output.
/// - No environment variables influence logging (removed all conditional env logic).
/// - When either feature is ON: every `dbglog!` / `debug_log!` writes to a persistent
///   `debug.log` file (append) plus the in-process GUI debug buffer and does not
///   spam stdout/stderr (keeps CLI output clean unless explicitly using info/error macros).
/// - When feature is OFF: zero overhead (macros expand to nothing; no file open).
/// - Single stream (no categories). If selective debugging is needed, developer
///   simply comments/uncomments or adds temporary dbglog! calls in code.
///
/// This keeps a permanent historical trail in `debug.log` while allowing
/// ephemeral instrumentation via the feature flag only.

// Enable debug logging infrastructure if either feature flag is present.
#[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::Mutex,
};

#[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
fn debug_file() -> &'static Mutex<Option<File>> {
    static HANDLE: Mutex<Option<File>> = Mutex::new(None);
    &HANDLE
}

#[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
fn ensure_debug_file_open() {
    let mut handle = debug_file().lock().unwrap();
    if handle.is_none() {
        *handle = OpenOptions::new()
            .create(true)
            .append(true)
            .open("debug.log")
            .ok();
    }
}

#[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
pub fn write_debug_line(line: &str) {
    ensure_debug_file_open();
    if let Ok(mut handle) = debug_file().lock() {
        if let Some(ref mut f) = *handle {
            let _ = f.write_all(line.as_bytes());
            let _ = f.write_all(b"\n");
            let _ = f.flush();
        }
    }
    // Mirror into GUI buffer only when full debug-logging feature (GUI integration) is enabled.
    #[cfg(feature = "debug-logging")]
    {
        // Re-export only exists under debug-logging; guard the call to avoid compile errors when only djvu_debug is active.
        crate::encoding::streamline::log_debug_message(line);
    }
}

/// Clear debug.log file at pipeline start to ensure full log capture for each run
#[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
pub fn clear_debug_log() {
    use std::fs;
    // Close the current file handle
    {
        let mut handle = debug_file().lock().unwrap();
        *handle = None;
    }
    // Remove existing log file if present
    let _ = fs::remove_file("debug.log");
    // Re-open with a fresh file on next write
    ensure_debug_file_open();
}

#[cfg(not(any(feature = "debug-logging", feature = "djvu_debug")))]
pub fn clear_debug_log() {
    // No-op when feature disabled
}

/// Core debug macro (no categories). Expands to nothing when feature off.
#[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
#[macro_export]
macro_rules! dbglog {
    ($($arg:tt)*) => {{
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let msg = format!($($arg)*);
        $crate::debug_log::write_debug_line(&format!("[{}] {}", ts, msg));
    }};
}

#[cfg(not(any(feature = "debug-logging", feature = "djvu_debug")))]
#[macro_export]
macro_rules! dbglog {
    ($($arg:tt)*) => {{
        // Still reference the arguments so a binding used only by this log does not
        // read as unused with the feature off. `if false` keeps them unevaluated.
        if false {
            let _ = ::core::format_args!($($arg)*);
        }
    }};
}

// Legacy aliases forward to dbglog!
#[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
#[macro_export]
macro_rules! debug_log { ($($arg:tt)*) => { $crate::dbglog!($($arg)*); } }

#[cfg(not(any(feature = "debug-logging", feature = "djvu_debug")))]
#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {{
        // Still reference the arguments so a binding used only by this log does not
        // read as unused with the feature off. `if false` keeps them unevaluated.
        if false {
            let _ = ::core::format_args!($($arg)*);
        }
    }};
}

#[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
#[macro_export]
macro_rules! debug_println { ($($arg:tt)*) => { $crate::dbglog!($($arg)*); } }

#[cfg(not(any(feature = "debug-logging", feature = "djvu_debug")))]
#[macro_export]
macro_rules! debug_println {
    ($($arg:tt)*) => {{
        // Still reference the arguments so a binding used only by this log does not
        // read as unused with the feature off. `if false` keeps them unevaluated.
        if false {
            let _ = ::core::format_args!($($arg)*);
        }
    }};
}

#[cfg(any(feature = "debug-logging", feature = "djvu_debug"))]
#[macro_export]
macro_rules! debug_eprintln { ($($arg:tt)*) => { $crate::dbglog!($($arg)*); } }

#[cfg(not(any(feature = "debug-logging", feature = "djvu_debug")))]
#[macro_export]
macro_rules! debug_eprintln {
    ($($arg:tt)*) => {{
        // Still reference the arguments so a binding used only by this log does not
        // read as unused with the feature off. `if false` keeps them unevaluated.
        if false {
            let _ = ::core::format_args!($($arg)*);
        }
    }};
}

/// Always print important messages (PDF info, ONNX provider, final results)
#[macro_export]
macro_rules! info_println {
    ($($arg:tt)*) => {
        println!($($arg)*)
    };
}

/// Always print error messages
#[macro_export]
macro_rules! error_println {
    ($($arg:tt)*) => {
        eprintln!($($arg)*)
    };
}
