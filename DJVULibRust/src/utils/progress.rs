use std::ffi::c_char;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Type signature for a progress callback: task name, current step, total steps.
pub type ProgressCallback = unsafe extern "C" fn(*const c_char, u64, u64) -> bool;

static PROGRESS_CALLBACK: AtomicPtr<ProgressCallback> = AtomicPtr::new(ptr::null_mut());

/// Sets the global progress callback. Returns the previous callback if any.
pub fn set_progress_callback(callback: Option<ProgressCallback>) -> Option<ProgressCallback> {
    let old = PROGRESS_CALLBACK.swap(
        callback.map_or(ptr::null_mut(), |c| c as *mut _),
        Ordering::SeqCst,
    );
    if old.is_null() {
        None
    } else {
        Some(unsafe { *old })
    }
}

/// Returns true if progress callbacks are supported (always true in this implementation).
pub fn supports_progress_callback() -> bool {
    true
}

/// Represents a progress-tracking task (for hierarchical progress reporting).
#[derive(Debug, Clone)]
pub struct DjVuProgressTask {
    pub task: String,
    pub nsteps: u32,
    pub runtostep: u32,
    pub startdate: Instant,
    pub parent: Option<Arc<DjVuProgressTask>>,
}

// (Add any impls as needed for your UI or CLI integration)
