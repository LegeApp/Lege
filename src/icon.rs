// Icon handling for runtime window icons and OS integration

// Embed the icon at compile time for packaging and Linux install
pub const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

/// Install icons on Linux using XDG paths and create a .desktop entry.
/// On Windows, icon is embedded in the .exe (no-op here).
pub fn install_icon() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::fs;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let home = std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp"));
        let app_dir = format!("{}/.local/share/applications", home);
        let icon_dir = format!("{}/.local/share/icons/hicolor/256x256/apps", home);
        fs::create_dir_all(&app_dir)?;
        fs::create_dir_all(&icon_dir)?;

        // Write .desktop file
        let desktop_path = format!("{}/lege.desktop", app_dir);
        let mut desktop_file = fs::File::create(&desktop_path)?;
        writeln!(desktop_file, "[Desktop Entry]")?;
        writeln!(desktop_file, "Type=Application")?;
        writeln!(desktop_file, "Name=Lege")?;
        writeln!(desktop_file, "Comment=Document processing tool")?;
        writeln!(desktop_file, "Exec=lege %F")?;
        writeln!(desktop_file, "Icon=lege")?;
        writeln!(desktop_file, "Terminal=false")?;
        writeln!(desktop_file, "Categories=Office;Graphics;")?;

        // Make .desktop file executable
        let mut perms = desktop_file.metadata()?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&desktop_path, perms)?;

        // Write icon file from embedded bytes
        let icon_dst = format!("{}/lege.png", icon_dir);
        fs::write(&icon_dst, ICON_PNG)?;

        // Try to update desktop database (best-effort)
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&app_dir)
            .output();
    }

    #[cfg(windows)]
    {
        // On Windows, the icon is embedded via build.rs; nothing to install
    }

    Ok(())
}

/// Writes the icon to a common pixmaps directory on Linux and returns the path.
/// On non-Linux platforms returns an empty string.
pub fn write_icon_to_system() -> Result<String, Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME")?;
        let icon_dir = format!("{}/.local/share/pixmaps", home);
        std::fs::create_dir_all(&icon_dir)?;

        let icon_path = format!("{}/lege.png", icon_dir);
        std::fs::write(&icon_path, ICON_PNG)?;

        Ok(icon_path)
    }

    #[cfg(not(target_os = "linux"))]
    {
        // For Windows/macOS, return empty path as icons are handled differently
        Ok(String::new())
    }
}

pub fn create_desktop_entry() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME")?;
        let desktop_dir = format!("{}/.local/share/applications", home);
        std::fs::create_dir_all(&desktop_dir)?;

        let exe_path = std::env::current_exe()?;
        let icon_path = write_icon_to_system()?;

        let desktop_content = format!(
            r#"[Desktop Entry]
Name=Lege
Comment=Document processing application
Exec={}
Icon={}
Type=Application
Categories=Office;Graphics;
Terminal=false
StartupNotify=true"#,
            exe_path.display(),
            icon_path
        );

        let desktop_file = format!("{}/lege.desktop", desktop_dir);
        std::fs::write(desktop_file, desktop_content)?;

        // Try to update desktop database
        std::process::Command::new("update-desktop-database")
            .arg(&desktop_dir)
            .output()
            .ok(); // Ignore errors
    }

    Ok(())
}
