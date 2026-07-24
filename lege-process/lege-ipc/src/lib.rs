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

/// Application data directory (`~/.local/share/Lege` or the platform
/// equivalent). Matches `lege`'s `app_dirs::data_dir()` so the CLI and GUI
/// read/write the same processing log.
pub fn data_dir() -> PathBuf {
    let base = dirs::data_dir()
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

/// Directory holding the processing log (same as [`data_dir`]).
pub fn logs_dir() -> PathBuf {
    data_dir()
}
