#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Lege Document OCR uninstaller is Windows-only.");
}

#[cfg(target_os = "windows")]
fn main() {
    use lege_document_ocr_installer_winsafe::win_utils::{
        delete_uninstall_entry, desktop_shortcut_path, is_safe_install_root, read_install_location,
        start_menu_shortcut_path,
    };
    use std::io::{self, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;
    use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
    use windows::core::PCWSTR;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--remove-after-exit") {
        let Some(path) = args.get(1).map(PathBuf::from) else {
            std::process::exit(2);
        };
        if !is_safe_install_root(&path) {
            std::process::exit(3);
        }
        let mut removed = false;
        for _ in 0..100 {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    removed = true;
                    break;
                }
                Err(_) if path.exists() => std::thread::sleep(Duration::from_millis(200)),
                Err(_) => {
                    removed = true;
                    break;
                }
            }
        }
        schedule_self_delete();
        std::process::exit(if removed { 0 } else { 4 });
    }

    let quiet = args.iter().any(|arg| arg == "--quiet");
    let explicit_path = option_value(&args, "--install-dir").map(PathBuf::from);
    let install_dir = explicit_path
        .or_else(read_install_location)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from(r"C:\LegeDocumentOCR"))
        });

    if !is_safe_install_root(&install_dir) {
        eprintln!(
            "Refusing to remove {}: the Lege Document OCR install marker and payload are not present.",
            install_dir.display()
        );
        std::process::exit(2);
    }

    if !quiet {
        println!("Lege Document OCR Uninstaller");
        println!("-----------------------------");
        println!("Install location: {}", install_dir.display());
        println!();
        print!("Uninstall Lege Document OCR from this location? [y/N] ");
        let _ = io::stdout().flush();

        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err() {
            eprintln!("Failed to read input. Aborting.");
            std::process::exit(1);
        }
        let answer = answer.trim().to_lowercase();
        if answer != "y" && answer != "yes" {
            println!("Aborted.");
            return;
        }
    }

    remove_shortcut(desktop_shortcut_path().as_deref(), "desktop");
    if let Some(path) = start_menu_shortcut_path() {
        remove_shortcut(Some(&path), "Start Menu");
        if let Some(folder) = path.parent() {
            let _ = std::fs::remove_dir(folder);
        }
    }
    delete_uninstall_entry();

    let helper = std::env::temp_dir().join(format!(
        "lege-document-ocr-uninstall-{}.exe",
        std::process::id()
    ));
    let current_exe = std::env::current_exe().unwrap_or_else(|error| {
        eprintln!("Could not locate the uninstaller executable: {error}");
        std::process::exit(1);
    });
    if let Err(error) = std::fs::copy(&current_exe, &helper) {
        eprintln!("Could not stage the safe removal helper: {error}");
        std::process::exit(1);
    }

    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    match Command::new(&helper)
        .arg("--remove-after-exit")
        .arg(&install_dir)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
    {
        Ok(_) => {
            if !quiet {
                println!("Lege Document OCR has been scheduled for removal.");
            }
        }
        Err(error) => {
            eprintln!("Could not start the safe removal helper: {error}");
            std::process::exit(1);
        }
    }

    fn remove_shortcut(path: Option<&Path>, label: &str) {
        if let Some(path) = path.filter(|path| path.is_file()) {
            if let Err(error) = std::fs::remove_file(path) {
                eprintln!("Warning: could not remove {label} shortcut: {error}");
            }
        }
    }

    fn option_value(args: &[String], name: &str) -> Option<String> {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].clone())
    }

    fn schedule_self_delete() {
        let Ok(current_exe) = std::env::current_exe() else {
            return;
        };
        let wide: Vec<u16> = current_exe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let _ = MoveFileExW(
                PCWSTR::from_raw(wide.as_ptr()),
                PCWSTR::null(),
                MOVEFILE_DELAY_UNTIL_REBOOT,
            );
        }
    }
}
