use std::path::{Path, PathBuf};

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

/// Application data directory. On Windows this is the LOCAL (non-roaming)
/// app-data dir: the tree holds logs and scratch data, which are
/// machine-specific and must not roam, and it keeps all Lege state in the one
/// folder the GUI already uses. Must stay in sync with `lege_ipc::data_dir`.
#[cfg_attr(feature = "android", allow(unreachable_code))]
pub fn data_dir() -> PathBuf {
    // Android has no HOME and no XDG dirs, so every `dirs::*` lookup below
    // returns None and the chain lands on `PathBuf::from(".")` — the process
    // working directory, which is not writable in an app sandbox. The host
    // passes its Context directories in through `android::init` instead.
    #[cfg(feature = "android")]
    return ensure_directory(&crate::android::data_dir());

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

pub fn logs_dir() -> PathBuf {
    data_dir()
}

pub fn djvu_base_dir() -> PathBuf {
    ensure_directory(&data_dir().join("djvu_temp"))
}

pub fn djvu_work_dir_for(output: Option<&Path>) -> PathBuf {
    let mut dir = djvu_base_dir();

    if let Some(path) = output {
        if let Some(stem) = path.file_stem() {
            let sanitized: String = stem
                .to_string_lossy()
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                .collect();
            dir = dir.join(sanitized);
        }
    }

    ensure_directory(&dir)
}
