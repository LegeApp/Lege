use std::env;
use std::fs;
use std::path::Path;

fn get_external_version_from_main_project() -> Option<String> {
    let current_dir = std::env::current_dir().ok()?;

    if let Some(gui_dir) = current_dir.parent() {
        if let Some(project_root) = gui_dir.parent() {
            let version_file = project_root.join("external_version.txt");
            if version_file.exists() {
                if let Ok(content) = fs::read_to_string(&version_file) {
                    let content = content.trim();
                    if !content.is_empty() {
                        return Some(content.to_string());
                    }
                }
            }
        }
    }

    let version_file = Path::new("external_version.txt");
    if version_file.exists() {
        if let Ok(content) = fs::read_to_string(version_file) {
            let content = content.trim();
            if !content.is_empty() {
                return Some(content.to_string());
            }
        }
    }

    None
}

fn main() {
    let external_version = get_external_version_from_main_project()
        .unwrap_or_else(|| env::var("LEGE_EXTERNAL_VERSION").unwrap_or_else(|_| "1.4.1".to_string()));

    println!("cargo:rustc-env=LEGE_EXTERNAL_VERSION={external_version}");

    #[cfg(target_os = "windows")]
    {
        // The current Freya debug binary ends up linking the generated resource
        // library twice, which causes LINK/CVTRES duplicate VERSION failures.
        // Keep Windows resource embedding for non-debug builds only so normal
        // debug iteration works.
        let profile = env::var("PROFILE").unwrap_or_default();
        if profile != "debug" {
            let mut res = winres::WindowsResource::new();
            res.set_icon("../../assets/icon.ico");
            res.set(
                "FileDescription",
                "Lege - Freya GUI for E-ink PDF Preparation",
            );
            res.set("ProductName", "Lege Freya GUI");
            res.set("CompanyName", "Lege Apps");
            res.set("FileVersion", &external_version);
            res.set("ProductVersion", &external_version);
            let _ = res.compile();
        }
    }

    #[cfg(target_os = "windows")]
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
}
