use std::env;
use std::path::Path;

/// Emit `lege_paddle_ocr` when PP-OCR is the active OCR backend.
///
/// The condition — "the `paddle-ocr` feature is on *and* this platform uses it"
/// — was previously spelled out at sixteen `#[cfg]` sites across six files as
/// `all(any(target_os = "linux", target_os = "macos"), feature = "paddle-ocr")`.
/// Adding a platform meant editing all sixteen. Deriving it once here keeps
/// platform knowledge in one place and leaves those sites reading
/// `#[cfg(lege_paddle_ocr)]`, which is also what they actually mean.
fn emit_paddle_ocr_cfg() {
    println!("cargo::rustc-check-cfg=cfg(lege_paddle_ocr)");

    if env::var_os("CARGO_FEATURE_PADDLE_OCR").is_none() {
        return;
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let platform_uses_paddle = match target_os.as_str() {
        "linux" | "macos" => true,
        // Android has no system OCR service to fall back to, so PP-OCR is the
        // only backend. Gated on the platform feature to match the rest of the
        // Android surface.
        "android" => env::var_os("CARGO_FEATURE_ANDROID").is_some(),
        _ => false,
    };

    if platform_uses_paddle {
        println!("cargo::rustc-cfg=lege_paddle_ocr");
    }
}

fn main() {
    emit_paddle_ocr_cfg();

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
    println!("cargo:rerun-if-changed=build.rs");
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
