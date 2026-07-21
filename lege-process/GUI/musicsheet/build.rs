use std::env;
use std::fs;
use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

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
    let external_version = get_external_version_from_main_project().unwrap_or_else(|| {
        env::var("LEGE_EXTERNAL_VERSION").unwrap_or_else(|_| "1.4.1".to_string())
    });

    println!("cargo:rustc-env=LEGE_EXTERNAL_VERSION={external_version}");

    #[cfg(target_os = "windows")]
    {
        let profile = env::var("PROFILE").unwrap_or_default();
        if profile != "debug" {
            compile_windows_resource(&external_version);
        }
    }

    #[cfg(target_os = "windows")]
    println!("cargo:rerun-if-changed=../../../lege-misc/assets/icon.ico");
}

#[cfg(target_os = "windows")]
fn compile_windows_resource(external_version: &str) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let rc_path = out_dir.join("lege-gui.rc");
    let version_tuple = version_tuple(external_version);
    let escaped_version = external_version.replace('"', "");

    // Resolve the shared ecosystem icon from lege-misc/assets.
    let icon_line = {
        let current_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
        let icon = PathBuf::from(&current_dir)
            .join("..")
            .join("..")
            .join("..")
            .join("lege-misc")
            .join("assets")
            .join("icon.ico");
        if icon.exists() {
            // Use a forward-slash path inside the .rc (rc.exe accepts both separators).
            let icon_str = icon
                .canonicalize()
                .unwrap_or(icon)
                .to_string_lossy()
                .replace('\\', "/");
            format!("IDI_APPLICATION ICON \"{icon_str}\"\n")
        } else {
            String::new()
        }
    };

    let rc_contents = format!(
        r#"{icon_line}VS_VERSION_INFO VERSIONINFO
 FILEVERSION {version_tuple}
 PRODUCTVERSION {version_tuple}
 FILEFLAGSMASK 0x3fL
#ifdef _DEBUG
 FILEFLAGS 0x1L
#else
 FILEFLAGS 0x0L
#endif
 FILEOS 0x40004L
 FILETYPE 0x1L
 FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904E4"
        BEGIN
            VALUE "CompanyName", "Lege Apps\0"
            VALUE "FileDescription", "Lege — Sheet Music Edition\0"
            VALUE "FileVersion", "{escaped_version}\0"
            VALUE "ProductName", "Lege — Sheet Music Edition\0"
            VALUE "ProductVersion", "{escaped_version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#
    );

    fs::write(&rc_path, rc_contents).expect("failed to write GUI resource script");
    let _ = embed_resource::compile(rc_path, embed_resource::NONE);
}

#[cfg(target_os = "windows")]
fn version_tuple(version: &str) -> String {
    let mut parts: Vec<u16> = version
        .split('.')
        .filter_map(|part| part.parse::<u16>().ok())
        .take(4)
        .collect();
    while parts.len() < 4 {
        parts.push(0);
    }
    format!("{},{},{},{}", parts[0], parts[1], parts[2], parts[3])
}
