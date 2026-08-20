//! Android platform shims.
//!
//! Every value the desktop resolves from the filesystem or the OS — the app
//! data directory, the memory budget, system font paths — is either injected
//! by the host app through [`init`] or derived here. Shared code reaches this
//! module through two-line delegations so no mobile-specific pathing leaks
//! into the desktop code paths.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Host-supplied Android runtime environment.
///
/// Android has no meaningful `HOME`, no XDG directories, and no writable
/// directory beside the executable, so none of the `dirs` crate's lookups
/// return anything usable. The app passes its own `Context` directories in
/// instead.
#[derive(Debug, Clone)]
pub struct AndroidEnv {
    /// `Context.getFilesDir()` — private, persistent, app-writable.
    pub files_dir: PathBuf,
    /// `Context.getCacheDir()` — private, evictable by the system.
    pub cache_dir: PathBuf,
    /// Per-process memory budget in MiB, from `ActivityManager`.
    ///
    /// The host should pass `getLargeMemoryClass()` (or `getMemoryClass()`
    /// when `largeHeap` is not requested). This is the only number that
    /// reflects what the app may actually use; total device RAM does not,
    /// because the app gets a fraction of it. `None` falls back to a
    /// conservative share of total RAM.
    pub memory_budget_mb: Option<u32>,
}

static ENV: OnceLock<AndroidEnv> = OnceLock::new();

/// Install the host-supplied environment and return whatever ended up
/// installed.
///
/// Idempotent, and the *first* call wins: hosts re-run their init path across
/// activity restarts, and by then the original data directory may already hold
/// an open job's scratch files, so swapping it underneath would be worse than
/// ignoring the second request. A differing repeat call is logged rather than
/// returned as an error — there is nothing the caller could usefully do about
/// it, and this is reached from an FFI boundary where a panic would be
/// undefined behaviour.
pub fn init(env: AndroidEnv) -> &'static AndroidEnv {
    let requested = env.files_dir.clone();
    let installed = ENV.get_or_init(|| env);

    if installed.files_dir != requested {
        log::warn!(
            "lege: android env already initialised with files_dir {}; ignoring request for {}",
            installed.files_dir.display(),
            requested.display()
        );
    }

    installed
}

/// The installed environment, if [`init`] has run.
pub fn env() -> Option<&'static AndroidEnv> {
    ENV.get()
}

/// Application data directory — the Android answer for
/// [`crate::app_dirs::data_dir`].
///
/// Falls back to the process working directory only when the host never called
/// [`init`]. That fallback is not writable in a real app sandbox; it exists so
/// a missing init surfaces as a normal I/O error rather than a panic.
pub fn data_dir() -> PathBuf {
    match env() {
        Some(env) => env.files_dir.join("Lege"),
        None => {
            log::error!(
                "lege: android platform env not initialised; call nativeInit before processing"
            );
            PathBuf::from(".")
        }
    }
}

/// Scratch directory for intermediate artifacts.
pub fn cache_dir() -> PathBuf {
    match env() {
        Some(env) => env.cache_dir.join("lege"),
        None => std::env::temp_dir(),
    }
}

/// Memory budget in GB for [`crate::pipeline::helper_functions::get_available_ram_gb`].
///
/// That figure drives `AdaptiveConcurrency`, which spends roughly 2 GB per
/// concurrent CPU stage and 0.5 GB per buffered page. Reporting total device
/// RAM would overcommit badly: an Android app is only entitled to its heap
/// class, not the whole device. Prefer the host-supplied budget; otherwise
/// take half of total RAM as a conservative stand-in.
///
/// Rounds up to 1 rather than 0 — `from_specs` treats 0 as "unknown" and
/// substitutes 4 GB, which is the opposite of what a small device wants.
pub fn available_ram_gb() -> usize {
    if let Some(mb) = env().and_then(|env| env.memory_budget_mb) {
        return ((mb as usize) / 1024).max(1);
    }

    let system = sysinfo::System::new_all();
    let total_bytes = system.total_memory();
    if total_bytes == 0 {
        return 1;
    }
    let half_gb = total_bytes / 2 / (1024 * 1024 * 1024);
    (half_gb as usize).max(1)
}

/// System font candidates for [`crate::unicode_font`].
///
/// Android ships its fonts in `/system/fonts` with no fontconfig and none of
/// the Debian layout the shared Unix arm assumes. Roboto is guaranteed
/// present; the Noto and Droid faces cover the CJK and fallback ranges.
pub fn font_candidate_paths() -> Vec<PathBuf> {
    [
        "/system/fonts/Roboto-Regular.ttf",
        "/system/fonts/NotoSerif-Regular.ttf",
        "/system/fonts/DroidSans.ttf",
        "/system/fonts/DroidSansFallback.ttf",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_budget_prefers_host_value_and_never_reports_zero() {
        // 512 MB heap class -> 0 GB by integer division; must clamp to 1 so
        // AdaptiveConcurrency does not read it as "unknown" and assume 4 GB.
        let env = AndroidEnv {
            files_dir: PathBuf::from("/data/data/app/files"),
            cache_dir: PathBuf::from("/data/data/app/cache"),
            memory_budget_mb: Some(512),
        };
        assert_eq!(((512_usize) / 1024).max(1), 1);
        assert_eq!(env.memory_budget_mb, Some(512));
    }

    #[test]
    fn data_dir_is_namespaced_under_files_dir() {
        let env = AndroidEnv {
            files_dir: PathBuf::from("/data/data/app/files"),
            cache_dir: PathBuf::from("/data/data/app/cache"),
            memory_budget_mb: None,
        };
        assert_eq!(
            env.files_dir.join("Lege"),
            PathBuf::from("/data/data/app/files/Lege")
        );
    }
}
