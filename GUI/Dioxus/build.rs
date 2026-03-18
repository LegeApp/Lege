use std::env;
use std::fs;
use std::path::Path;

/// Try to get the external version from the file in the project root
fn get_external_version_from_main_project() -> Option<String> {
    // Look for the external version file in the project root
    // The GUI build runs from GUI/Dioxus/, so project root is two levels up
    let current_dir = std::env::current_dir().ok()?;

    // Go up two levels to reach project root: GUI/Dioxus -> GUI -> project root
    if let Some(gui_dir) = current_dir.parent() {
        if let Some(project_root) = gui_dir.parent() {
            let version_file = project_root.join("external_version.txt");
            if version_file.exists() {
                if let Ok(content) = fs::read_to_string(&version_file) {
                    let content = content.trim();
                    if !content.is_empty() {
                        println!(
                            "cargo:warning=Found external version from file: {}",
                            content
                        );
                        return Some(content.to_string());
                    }
                }
            }
        }
    }

    // As another approach, try to read directly from a relative path
    // Since Cargo typically runs with the workspace root as the current directory for workspace members
    let version_file = Path::new("external_version.txt");
    if version_file.exists() {
        if let Ok(content) = fs::read_to_string(&version_file) {
            let content = content.trim();
            if !content.is_empty() {
                println!(
                    "cargo:warning=Found external version from file (relative path): {}",
                    content
                );
                return Some(content.to_string());
            }
        }
    }

    // Also try from a different relative perspective
    let version_file = Path::new("../external_version.txt");
    if version_file.exists() {
        if let Ok(content) = fs::read_to_string(&version_file) {
            let content = content.trim();
            if !content.is_empty() {
                println!(
                    "cargo:warning=Found external version from file (../): {}",
                    content
                );
                return Some(content.to_string());
            }
        }
    }

    None
}

fn main() {
    // Try to get the external version from the main project build
    let external_version = get_external_version_from_main_project().unwrap_or_else(|| {
        // Fallback to environment variable or default
        env::var("LEGE_EXTERNAL_VERSION").unwrap_or_else(|_| "1.1.5.0".to_string())
    });

    // Expose to Rust code in this crate.
    println!("cargo:rustc-env=LEGE_EXTERNAL_VERSION={}", external_version);

    // Only embed icon resource on Windows
    #[cfg(target_os = "windows")]
    {
        // Bin-only crate; safe to embed resources without duplication.
        let mut res = winres::WindowsResource::new();
        // Use the shared app icon (dark purple L) from the root assets
        res.set_icon("../../assets/icon.ico");
        // Optional: application metadata
        res.set(
            "FileDescription",
            "Lege - PDF Preparation for E-ink Readers",
        );
        res.set("ProductName", "Lege");
        res.set("CompanyName", "Lege Apps");
        // Keep Windows resource version aligned with external version
        res.set("FileVersion", &external_version);
        res.set("ProductVersion", &external_version);
        if let Err(e) = res.compile() {
            eprintln!("Warning: Could not embed icon: {}", e);
            eprintln!("Make sure assets/icon.ico exists in the project root");
        }
    }

    // Tell cargo to re-run this script if the icon file changes
    #[cfg(target_os = "windows")]
    println!("cargo:rerun-if-changed=../../assets/icon.ico");

    // For Linux, copy icon files and desktop entry for proper GUI integration
    #[cfg(target_os = "linux")]
    {
        use std::fs;
        use std::path::Path;

        let out_dir = std::env::var("OUT_DIR").unwrap();
        let target_dir = Path::new(&out_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();

        // Copy icon files
        if let Err(e) = fs::copy("../../assets/icon.png", target_dir.join("lege-gui-256.png")) {
            eprintln!("Warning: Could not copy 256x256 icon: {}", e);
        }
        if let Err(e) = fs::copy(
            "../../assets/icon2.png",
            target_dir.join("lege-gui-128.png"),
        ) {
            eprintln!("Warning: Could not copy 128x128 icon: {}", e);
        }

        // Copy desktop entry
        if let Err(e) = fs::copy("lege-gui.desktop", target_dir.join("lege-gui.desktop")) {
            eprintln!("Warning: Could not copy desktop entry: {}", e);
        }

        println!("cargo:rerun-if-changed=../../assets/icon.png");
        println!("cargo:rerun-if-changed=../../assets/icon2.png");
        println!("cargo:rerun-if-changed=lege-gui.desktop");
    }
}
