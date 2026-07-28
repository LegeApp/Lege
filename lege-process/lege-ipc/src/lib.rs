//! Small surface shared between the `lege` CLI and its GUI front-ends
//! (`lege-gui`, `lege-music-gui`). The GUIs drive processing by spawning the
//! `lege` CLI as a subprocess and reading its newline-delimited JSON; they need
//! only these log/interchange types, not the whole `lege` library. Keeping them
//! here lets the GUIs avoid linking `lege` (and its wgpu/renderer/OCR build
//! graph) entirely.

use std::path::{Path, PathBuf};

pub mod processing_log;

/// Default Sauvola k-factor surfaced in the GUI binarization control. Mirrors
/// `lege_gpu::binarization::DEFAULT_K_FACTOR` by value; kept here so the GUI
/// need not link the GPU crate for a single constant.
pub const DEFAULT_K_FACTOR: f32 = 0.05;

fn ensure_directory(path: &Path) -> PathBuf {
    if let Err(e) = std::fs::create_dir_all(path) {
        eprintln!(
            "Warning: failed to create directory {}: {}",
            path.display(),
            e
        );
    }
    path.to_path_buf()
}

/// Application data directory (`%LOCALAPPDATA%\Lege` on Windows,
/// `~/.local/share/Lege` or the platform equivalent elsewhere). Matches
/// `lege`'s `app_dirs::data_dir()` so the CLI and GUI read/write the same
/// processing log.
///
/// Windows deliberately uses the LOCAL (non-roaming) app-data dir: this tree
/// holds logs and scratch data, which are machine-specific and must not be
/// synced by roaming profiles — and it consolidates everything under the one
/// `Lege` folder the GUI already uses for its settings and worker log.
pub fn data_dir() -> PathBuf {
    let base = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .or_else(|| {
            dirs::home_dir().map(|mut dir| {
                dir.push(".local");
                dir.push("share");
                dir
            })
        })
        .unwrap_or_else(|| PathBuf::from("."));

    ensure_directory(&base.join("Lege"))
}

/// One-time, lazy migration of a file this app historically wrote to the
/// Windows ROAMING app-data dir. No-op when the target already exists, when
/// there is no legacy copy, or on platforms where both dirs coincide.
pub fn migrate_legacy_roaming_file(file_name: &str, target: &Path) {
    if target.exists() {
        return;
    }
    let Some(legacy_base) = dirs::data_dir() else {
        return;
    };
    let legacy = legacy_base.join("Lege").join(file_name);
    if legacy == target || !legacy.is_file() {
        return;
    }
    if std::fs::rename(&legacy, target).is_err() {
        // Cross-volume fallback (e.g. redirected profile folders).
        if std::fs::copy(&legacy, target).is_ok() {
            let _ = std::fs::remove_file(&legacy);
        }
    }
}

/// Directory holding the processing log (same as [`data_dir`]).
pub fn logs_dir() -> PathBuf {
    data_dir()
}
