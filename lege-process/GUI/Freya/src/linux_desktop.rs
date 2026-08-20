//! Portable-zip desktop integration for Linux.
//!
//! FreeDesktop launchers need a real PNG on disk and a `.desktop` file; ELF
//! binaries cannot carry a file-manager icon the way PE resources do on Windows.
//! On first run (and whenever the install path or icon bytes change), extract
//! the compile-time-embedded PNG into the user's XDG data dirs and register a
//! menu entry that points at this `lege-gui` binary.

use std::fs;
use std::io;
use std::path::Path;

const DESKTOP_ID: &str = "lege.desktop";
const ICON_BASENAME: &str = "lege.png";

/// Best-effort: never fails app startup.
///
/// No-op when:
/// - `LEGE_SKIP_DESKTOP_INTEGRATION` is set
/// - running inside an AppImage (`APPIMAGE` / `APPDIR` set by the runtime) —
///   AppImage already ships `.desktop` + icons in the image; `current_exe()`
///   would also resolve to a transient FUSE mount under `/tmp/.mount_*`
/// - running as Flatpak (`FLATPAK_ID`) or Snap (`SNAP`) — those own the
///   desktop entry and must not be overwritten with a host path
pub fn ensure_desktop_integration(icon_png: &[u8]) {
    if std::env::var_os("LEGE_SKIP_DESKTOP_INTEGRATION").is_some() {
        return;
    }
    if packaged_runtime_handles_desktop() {
        return;
    }
    if let Err(err) = ensure_desktop_integration_inner(icon_png) {
        eprintln!("lege-gui: desktop integration skipped: {err}");
    }
}

fn packaged_runtime_handles_desktop() -> bool {
    // AppImage runtime exports these for every process inside the image.
    if std::env::var_os("APPIMAGE").is_some() || std::env::var_os("APPDIR").is_some() {
        return true;
    }
    if std::env::var_os("FLATPAK_ID").is_some() || std::env::var_os("SNAP").is_some() {
        return true;
    }
    // Belt-and-braces: even without env vars, never pin Exec to a FUSE mount.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(s) = exe.to_str() {
            if s.contains("/.mount_") {
                return true;
            }
        }
    }
    false
}

fn ensure_desktop_integration_inner(icon_png: &[u8]) -> io::Result<()> {
    let data_dir = dirs::data_local_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG data local dir unavailable"))?;

    // Absolute icon path so we do not depend on icon-theme cache refresh.
    let icon_path = data_dir
        .join("icons")
        .join("hicolor")
        .join("256x256")
        .join("apps")
        .join(ICON_BASENAME);
    write_if_changed(&icon_path, icon_png)?;

    let exe = std::env::current_exe()?;
    // Resolve symlinks when possible so the menu entry survives `PATH` wrappers.
    let exe = fs::canonicalize(&exe).unwrap_or(exe);

    let desktop_dir = data_dir.join("applications");
    fs::create_dir_all(&desktop_dir)?;
    let desktop_path = desktop_dir.join(DESKTOP_ID);

    let desktop_body = desktop_entry(&exe, &icon_path);
    write_if_changed(&desktop_path, desktop_body.as_bytes())?;

    // Best-effort cache refreshes; absence of the tools is fine.
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&desktop_dir)
        .output();
    if let Some(hicolor_root) = icon_path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
    {
        let _ = std::process::Command::new("gtk-update-icon-cache")
            .args(["-f", "-t"])
            .arg(hicolor_root)
            .output();
    }

    Ok(())
}

fn desktop_entry(exe: &Path, icon: &Path) -> String {
    // Quote paths that contain spaces; FreeDesktop allows quoted Exec tokens.
    let exec = shell_quote_path(exe);
    let icon_str = icon.display().to_string();
    format!(
        "\
[Desktop Entry]
Version=1.0
Type=Application
Name=Lege
Comment=Document processing for E-Ink readers
Exec=env LAUNCHED_FROM_GUI=1 {exec}
Icon={icon_str}
Terminal=false
Categories=Office;Graphics;
StartupNotify=true
MimeType=application/pdf;
StartupWMClass=lege-gui
"
    )
}

fn shell_quote_path(path: &Path) -> String {
    let s = path.display().to_string();
    if s.chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '$' | '`'))
    {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s
    }
}

fn write_if_changed(path: &Path, contents: &[u8]) -> io::Result<bool> {
    if let Ok(existing) = fs::read(path) {
        if existing == contents {
            return Ok(false);
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Atomic-ish replace: write temp then rename.
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_includes_absolute_icon_and_exec() {
        let body = desktop_entry(
            Path::new("/opt/lege/lege-gui"),
            Path::new("/home/u/.local/share/icons/hicolor/256x256/apps/lege.png"),
        );
        assert!(body.contains("Exec=env LAUNCHED_FROM_GUI=1 /opt/lege/lege-gui"));
        assert!(body.contains("Icon=/home/u/.local/share/icons/hicolor/256x256/apps/lege.png"));
        assert!(body.contains("StartupWMClass=lege-gui"));
    }

    #[test]
    fn spaces_in_path_are_quoted() {
        let body = desktop_entry(
            Path::new("/home/u/My Apps/lege-gui"),
            Path::new("/tmp/lege.png"),
        );
        assert!(body.contains("Exec=env LAUNCHED_FROM_GUI=1 \"/home/u/My Apps/lege-gui\""));
    }

    #[test]
    fn packaged_runtimes_are_detected_from_env() {
        // Isolation: only assert the helper when vars we control are set.
        let prev_appimage = std::env::var_os("APPIMAGE");
        // SAFETY: tests run single-threaded for this crate's bin tests by default;
        // we restore the previous value immediately.
        unsafe { std::env::set_var("APPIMAGE", "/tmp/Lege.AppImage") };
        assert!(packaged_runtime_handles_desktop());
        unsafe {
            match prev_appimage {
                Some(v) => std::env::set_var("APPIMAGE", v),
                None => std::env::remove_var("APPIMAGE"),
            }
        }
    }
}
