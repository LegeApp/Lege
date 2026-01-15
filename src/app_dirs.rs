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

pub fn data_dir() -> PathBuf {
    let base = dirs::data_dir()
        .or_else(|| dirs::home_dir().map(|mut dir| {
            dir.push(".local");
            dir.push("share");
            dir
        }))
        .unwrap_or_else(|| PathBuf::from("."));

    ensure_directory(&base.join("Lege"))
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
