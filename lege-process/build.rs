use std::env;
use std::path::Path;

fn main() {
    let profile = env::var("PROFILE").unwrap_or_default();
    let debug_logging_enabled = env::var_os("CARGO_FEATURE_DEBUG_LOGGING").is_some();
    if profile == "release" && debug_logging_enabled {
        println!(
            "cargo:warning=debug-logging is enabled in the release profile; Cargo's default release build still uses LTO."
        );
        println!(
            "cargo:warning=Use `cargo build-debug-logging` or `cargo run-debug-logging` to build debug-logging with the repo's no-LTO profile."
        );
    }

    // Get external version from the file, with fallback to environment or default
    let external_version = read_external_version_file().unwrap_or_else(|| {
        env::var("LEGE_EXTERNAL_VERSION").unwrap_or_else(|_| "1.20.5.0".to_string())
    });

    // Pass the version to the build script
    println!("cargo:rustc-env=LEGE_EXTERNAL_VERSION={}", external_version);

    // Print a warning to help with debugging
    println!("cargo:warning=Using external version: {}", external_version);

    // Windows: embed version metadata into the CLI .exe (no icon — icon lives in lege-gui).
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set("FileDescription", "Lege - PDF processor CLI");
        res.set("ProductName", "Lege");
        res.set("CompanyName", "Lege Apps");
        res.set("FileVersion", &external_version);
        res.set("ProductVersion", &external_version);
        if let Err(e) = res.compile() {
            eprintln!("Warning: resource compile failed: {e}");
        }
    }

    // Ensure build recompiles when icon changes
    println!("cargo:rerun-if-changed=../lege-misc/assets/icon.png");
    println!("cargo:rerun-if-changed=../lege-misc/assets/icon.ico");

    // Check if we need to link against PDFium
    println!("cargo:warning=To enable static linking with Pdfium:");
    println!("cargo:warning=1. Set PDFIUM_STATIC_LIB_PATH environment variable");
    println!("cargo:warning=2. Build with: cargo build --features static");

    if cfg!(feature = "static") {
        // Link against system libraries on Windows
        if cfg!(target_os = "windows") {
            println!("cargo:rustc-link-lib=user32");
            println!("cargo:rustc-link-lib=gdi32");
            println!("cargo:rustc-link-lib=advapi32");
            println!("cargo:rustc-link-lib=shell32");
            println!("cargo:rustc-link-lib=ole32");
            println!("cargo:rustc-link-lib=oleaut32");
            println!("cargo:rustc-link-lib=uuid");
            println!("cargo:rustc-link-lib=comctl32");
            println!("cargo:rustc-link-lib=comdlg32");
            println!("cargo:rustc-link-lib=winspool");
        }
    }

    // Build Linux GUI on release builds (Linux only) - DISABLED FOR FASTER BUILDS
    // Uncomment this block if you want to auto-build GUI during cargo build
    // #[cfg(target_os = "linux")]
    // {
    //     let profile = env::var("PROFILE").unwrap_or_default();
    //     if profile == "release" {
    //         build_linux_gui();
    //     }
    // }

    // Re-run this build script if environment variables change
    println!("cargo:rerun-if-env-changed=PDFIUM_STATIC_LIB_PATH");
    println!("cargo:rerun-if-env-changed=PDFIUM_DYNAMIC_LIB_PATH");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=GUI/DocBrakeGUI/DocBrakeGUI.csproj");
    println!("cargo:rerun-if-changed=GUI/iced-gui/Cargo.toml");

    // Print build completion message with timestamp
    let now = std::time::SystemTime::now();
    let since_epoch = now.duration_since(std::time::UNIX_EPOCH).unwrap();
    println!(
        "cargo:warning=Build script completed at {} seconds since epoch",
        since_epoch.as_secs()
    );
}

/// Read external version from the file in the project root
fn read_external_version_file() -> Option<String> {
    let version_file = Path::new("external_version.txt");
    if version_file.exists() {
        if let Ok(content) = std::fs::read_to_string(version_file) {
            let content = content.trim();
            if !content.is_empty() {
                return Some(content.to_string());
            }
        }
    }
    None
}
