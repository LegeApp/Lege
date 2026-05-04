use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    #[cfg(target_os = "windows")]
    compile_hlsl_shaders();
}

#[cfg(target_os = "windows")]
fn compile_hlsl_shaders() {
    use std::fs;

    const SHADERS: &[(&str, &str, &str)] = &[
        (
            "src/resize/hlsl/shaders/bilinear_resize.hlsl",
            "bilinear_resize.cso",
            "BILINEAR_SHADER",
        ),
        (
            "src/resize/hlsl/shaders/bell_resize.hlsl",
            "bell_resize.cso",
            "BELL_SHADER",
        ),
        (
            "src/resize/hlsl/shaders/lanczos3_resize.hlsl",
            "lanczos3_resize.cso",
            "LANCZOS3_SHADER",
        ),
        (
            "src/resize/hlsl/shaders/rgba8_to_nchw_f32.hlsl",
            "rgba8_to_nchw_f32.cso",
            "RGBA8_TO_NCHW_F32_SHADER",
        ),
        (
            "src/binarization/hlsl/shaders/build_reflect_padded.hlsl",
            "build_reflect_padded.cso",
            "BINARIZE_PAD_SHADER",
        ),
        (
            "src/binarization/hlsl/shaders/integral_horizontal_prefix.hlsl",
            "integral_horizontal_prefix.cso",
            "BINARIZE_INTEGRAL_H_SHADER",
        ),
        (
            "src/binarization/hlsl/shaders/integral_vertical_prefix.hlsl",
            "integral_vertical_prefix.cso",
            "BINARIZE_INTEGRAL_V_SHADER",
        ),
        (
            "src/binarization/hlsl/shaders/integral_horizontal_prefix_f32.hlsl",
            "integral_horizontal_prefix_f32.cso",
            "BINARIZE_INTEGRAL_H_F32_SHADER",
        ),
        (
            "src/binarization/hlsl/shaders/integral_vertical_prefix_f32.hlsl",
            "integral_vertical_prefix_f32.cso",
            "BINARIZE_INTEGRAL_V_F32_SHADER",
        ),
        (
            "src/binarization/hlsl/shaders/bg_max_horizontal.hlsl",
            "bg_max_horizontal.cso",
            "BINARIZE_BG_MAX_H_SHADER",
        ),
        (
            "src/binarization/hlsl/shaders/bg_max_vertical.hlsl",
            "bg_max_vertical.cso",
            "BINARIZE_BG_MAX_V_SHADER",
        ),
        (
            "src/binarization/hlsl/shaders/sauvola_otsu_final_from_integral.hlsl",
            "sauvola_otsu_final_from_integral.cso",
            "BINARIZE_FINAL_SHADER",
        ),
        (
            "src/binarization/hlsl/shaders/sauvola_otsu_binarize.hlsl",
            "sauvola_otsu_binarize.cso",
            "BINARIZE_SHADER",
        ),
        (
            "src/binarization/hlsl/shaders/adaptive_linearize.hlsl",
            "adaptive_linearize.cso",
            "BINARIZE_LINEARIZE_SHADER",
        ),
    ];

    for (src, _, _) in SHADERS {
        println!("cargo:rerun-if-changed={src}");
    }

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let shader_out_dir = PathBuf::from(out_dir).join("hlsl-shaders");
    if let Err(err) = fs::create_dir_all(&shader_out_dir) {
        panic!("Failed to create shader output directory: {err}");
    }

    let dxc_path = find_dxc().expect(
        "DirectX Shader Compiler (dxc.exe) not found. Install the Windows SDK or add dxc to PATH.",
    );

    for (hlsl_file, cso_file, _) in SHADERS {
        let hlsl_path = Path::new(hlsl_file);
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
    for (_, cso_file, const_name) in SHADERS {
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
        "cargo:rustc-env=LEGE_GPU_SHADER_INCLUDE_PATH={}",
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
