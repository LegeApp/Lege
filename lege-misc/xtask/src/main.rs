use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("appimage") => build_appimage(args.collect()),
        Some("help") | Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(format!(
            "unknown xtask command `{other}`\n\nRun `cargo appimage` or `cargo run -p xtask -- appimage`."
        )),
    }
}

fn build_appimage(extra_args: Vec<String>) -> Result<(), String> {
    if !extra_args.is_empty() {
        return Err(format!(
            "appimage does not accept positional arguments yet: {}",
            extra_args.join(" ")
        ));
    }

    if !cfg!(target_os = "linux") {
        return Err("AppImage packaging is only supported from Linux/WSL.".to_string());
    }

    let root = workspace_root()?;
    let script = root.join("lege-misc/scripts/build-appimage.sh");
    if !script.exists() {
        return Err(format!("missing AppImage script: {}", script.display()));
    }

    println!("Building Lege AppImage via {}", script.display());
    let status = Command::new("bash")
        .arg(script)
        .current_dir(&root)
        .status()
        .map_err(|err| format!("failed to launch AppImage build script: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("AppImage build failed with status {status}"))
    }
}

fn workspace_root() -> Result<PathBuf, String> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "could not determine workspace root from {}",
                manifest_dir.display()
            )
        })
}

fn print_help() {
    println!(
        "Lege packaging tasks\n\n  cargo appimage\n      Build target/appimage/Lege-<version>-<arch>.AppImage\n\nEnvironment forwarded to scripts/build-appimage.sh:\n  APPIMAGETOOL                appimagetool executable/path\n  APPIMAGE_UPDATE_INFORMATION AppImage update metadata string\n  LEGE_CARGO_FEATURES         Additional Cargo features"
    );
}
