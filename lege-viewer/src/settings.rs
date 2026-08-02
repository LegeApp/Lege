use std::io::Write;
use std::path::PathBuf;
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::document::ColorMode;

const SETTINGS_VERSION: u32 = 1;
const SAVE_COALESCE_DELAY: Duration = Duration::from_millis(75);
static SETTINGS_SENDER: OnceLock<mpsc::Sender<ViewerSettings>> = OnceLock::new();

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StoredColorMode {
    Original,
    Night,
    WarmPaper,
}

impl From<ColorMode> for StoredColorMode {
    fn from(value: ColorMode) -> Self {
        match value {
            ColorMode::Original => Self::Original,
            ColorMode::Night => Self::Night,
            ColorMode::WarmPaper => Self::WarmPaper,
        }
    }
}

impl From<StoredColorMode> for ColorMode {
    fn from(value: StoredColorMode) -> Self {
        match value {
            StoredColorMode::Original => Self::Original,
            StoredColorMode::Night => Self::Night,
            StoredColorMode::WarmPaper => Self::WarmPaper,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ViewerSettings {
    pub trim_enabled: bool,
    pub color_mode: ColorMode,
}

impl Default for ViewerSettings {
    fn default() -> Self {
        Self {
            trim_enabled: false,
            color_mode: ColorMode::Original,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredSettings {
    version: u32,
    trim_enabled: bool,
    color_mode: StoredColorMode,
}

impl ViewerSettings {
    pub fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        match std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StoredSettings>(&bytes).ok())
        {
            Some(stored) if stored.version == SETTINGS_VERSION => Self {
                trim_enabled: stored.trim_enabled,
                color_mode: stored.color_mode.into(),
            },
            Some(_) => {
                eprintln!("Lege Viewer ignored settings with an unsupported version");
                Self::default()
            }
            None => Self::default(),
        }
    }

    pub fn save_async(self) {
        let sender = SETTINGS_SENDER.get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || settings_writer(receiver));
            sender
        });
        if sender.send(self).is_err() {
            eprintln!("Lege Viewer settings writer stopped unexpectedly");
        }
    }
}

fn settings_writer(receiver: mpsc::Receiver<ViewerSettings>) {
    while let Ok(mut latest) = receiver.recv() {
        while let Ok(newer) = receiver.recv_timeout(SAVE_COALESCE_DELAY) {
            latest = newer;
        }
        if let Err(error) = write_settings(latest) {
            eprintln!("Lege Viewer could not save settings: {error}");
        }
    }
}

fn write_settings(settings: ViewerSettings) -> std::io::Result<()> {
    let Some(path) = settings_path() else {
        return Ok(());
    };
    write_settings_to_path(settings, &path)
}

fn write_settings_to_path(settings: ViewerSettings, path: &std::path::Path) -> std::io::Result<()> {
    let stored = StoredSettings {
        version: SETTINGS_VERSION,
        trim_enabled: settings.trim_enabled,
        color_mode: settings.color_mode.into(),
    };
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(&stored).map_err(std::io::Error::other)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(&bytes)?;
    temp.as_file_mut().sync_all()?;
    // `persist` atomically replaces an existing destination where the
    // platform supports it. Most importantly, it never deletes the last good
    // settings file before the replacement has been written successfully.
    temp.persist(path).map(|_| ()).map_err(|error| error.error)
}

fn settings_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("Lege").join("viewer-settings.json"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(PathBuf::from).map(|path| {
            path.join("Library")
                .join("Application Support")
                .join("Lege")
                .join("viewer-settings.json")
        })
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|path| path.join("lege").join("viewer-settings.json"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn settings_replace_keeps_a_complete_latest_document() {
        let directory = tempfile::tempdir().expect("temporary settings directory");
        let path = directory.path().join("viewer-settings.json");
        write_settings_to_path(
            ViewerSettings {
                trim_enabled: false,
                color_mode: ColorMode::Original,
            },
            &path,
        )
        .expect("initial settings");
        write_settings_to_path(
            ViewerSettings {
                trim_enabled: true,
                color_mode: ColorMode::Night,
            },
            &path,
        )
        .expect("replacement settings");

        let stored: StoredSettings =
            serde_json::from_slice(&std::fs::read(path).expect("saved settings"))
                .expect("complete JSON document");
        assert_eq!(stored.version, SETTINGS_VERSION);
        assert!(stored.trim_enabled);
        assert!(matches!(stored.color_mode, StoredColorMode::Night));
    }
}
