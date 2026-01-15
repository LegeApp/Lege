use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Get external version from the file, with fallback to environment or default
    let external_version = read_external_version_file()
        .unwrap_or_else(|| {
            env::var("LEGE_EXTERNAL_VERSION").unwrap_or_else(|_| "1.20.5.0".to_string())
        });

    // Pass the version to the build script
    println!("cargo:rustc-env=LEGE_EXTERNAL_VERSION={}", external_version);
    
    // Print a warning to help with debugging
    println!("cargo:warning=Using external version: {}", external_version);

    // Windows: embed icon into .exe ONLY when this is the primary package being built.
    // This avoids duplicate VERSION resources when other workspace members depend on `lege`.
    #[cfg(windows)]
    {
        // Always embed resources for the lege binary on Windows
        let icon_path = std::path::Path::new("assets/icon.ico");
        if icon_path.exists() {
            let mut res = winres::WindowsResource::new();
            res.set_icon(icon_path.to_str().unwrap());
            // Populate version metadata with external version for Windows resources
            res.set("FileDescription", "Lege - PDF processing CLI");
            res.set("ProductName", "Lege");
            res.set("CompanyName", "Lege Apps");
            // String version fields
            res.set("FileVersion", &external_version);
            res.set("ProductVersion", &external_version);
            if let Err(e) = res.compile() {
                eprintln!("Warning: resource compile failed: {e}");
            }
        } else {
            eprintln!("Warning: assets/icon.ico not found; skipping icon embedding");
        }
    }

    // Ensure build recompiles when icon changes
    println!("cargo:rerun-if-changed=assets/icon.png");
    println!("cargo:rerun-if-changed=assets/icon.ico");

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

    #[cfg(target_os = "windows")]
    compile_hlsl_shaders();

    // Print build completion message with timestamp
    let now = std::time::SystemTime::now();
    let since_epoch = now.duration_since(std::time::UNIX_EPOCH).unwrap();
    println!("cargo:warning=Build script completed at {} seconds since epoch", since_epoch.as_secs());
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

#[cfg(target_os = "windows")]
fn compile_hlsl_shaders() {
    use std::fs;

    const SHADERS: &[(&str, &str)] = &[
        ("bilinear_resize.hlsl", "bilinear_resize.cso"),
        ("bell_resize.hlsl", "bell_resize.cso"),
        ("lanczos3_resize.hlsl", "lanczos3_resize.cso"),
        ("rgba8_to_nchw_f32.hlsl", "rgba8_to_nchw_f32.cso"),
    ];

    let shader_dir = Path::new("src/resize/hlsl/shaders");
    println!("cargo:rerun-if-changed={}", shader_dir.display());
    for (src, _) in SHADERS {
        println!("cargo:rerun-if-changed={}", shader_dir.join(src).display());
    }

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let shader_out_dir = PathBuf::from(out_dir).join("hlsl-shaders");
    if let Err(err) = fs::create_dir_all(&shader_out_dir) {
        panic!("Failed to create shader output directory: {err}");
    }

    let dxc_path = find_dxc().expect(
        "DirectX Shader Compiler (dxc.exe) not found. Install the Windows SDK or add dxc to PATH.",
    );

    for (hlsl_file, cso_file) in SHADERS {
        let hlsl_path = shader_dir.join(hlsl_file);
        let cso_path = shader_out_dir.join(cso_file);

        let output = Command::new(&dxc_path)
            .args([
                "-T",
                "cs_6_0",
                "-E",
                "main",
                "-O3",
                "-Fo",
                cso_path.to_str().expect("Invalid output path"),
                hlsl_path.to_str().expect("Invalid shader path"),
            ])
            .output()
            .expect("Failed to execute dxc");

        if !output.status.success() {
            panic!(
                "DXC failed to compile {}.\nstdout:{}\nstderr:{}",
                hlsl_path.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    let mut include_content = String::from("// Auto-generated shader includes\n\n");
    for (const_name, cso_file) in [
        ("BILINEAR_SHADER", "bilinear_resize.cso"),
        ("BELL_SHADER", "bell_resize.cso"),
        ("LANCZOS3_SHADER", "lanczos3_resize.cso"),
        ("RGBA8_TO_NCHW_F32_SHADER", "rgba8_to_nchw_f32.cso"),
    ] {
        let cso_path = shader_out_dir.join(cso_file);
        let normalized = cso_path
            .to_str()
            .expect("Invalid shader path")
            .replace('\\', "/");
        include_content.push_str(&format!(
            "pub const {}: &[u8] = include_bytes!(r\"{}\");\n",
            const_name, normalized
        ));
    }

    let include_path = shader_out_dir.join("shaders.rs");
    if let Err(err) = fs::write(&include_path, include_content) {
        panic!("Failed to write shader include file: {err}");
    }

    println!(
        "cargo:rustc-env=SHADER_INCLUDE_PATH={}",
        include_path.display()
    );
}

#[cfg(target_os = "windows")]
fn find_dxc() -> Option<PathBuf> {
    const SDK_PATHS: &[&str] = &[
        r"C:\\Program Files (x86)\\Windows Kits\\10\\bin\\10.0.22621.0\\x64\\dxc.exe",
        r"C:\\Program Files (x86)\\Windows Kits\\10\\bin\\10.0.22000.0\\x64\\dxc.exe",
        r"C:\\Program Files (x86)\\Windows Kits\\10\\bin\\10.0.20348.0\\x64\\dxc.exe",
        r"C:\\Program Files (x86)\\Windows Kits\\10\\bin\\x64\\dxc.exe",
    ];

    for path in SDK_PATHS {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    if let Ok(output) = Command::new("where").arg("dxc").output() {
        if output.status.success() {
            if let Some(line) = String::from_utf8_lossy(&output.stdout).lines().next() {
                let path = PathBuf::from(line.trim());
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }

    None
}
