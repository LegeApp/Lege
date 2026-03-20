#![cfg_attr(target_os = "windows", windows_subsystem = "console")]

#[cfg(target_os = "windows")]
mod windows_uninstaller {
    use std::env;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, exit};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
    use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::{SHOW_WINDOW_CMD, SW_SHOWNORMAL};
    use windows::core::{PCWSTR, w};

    pub fn run() {
        let args = CleanupArgs::parse();
        if let Some(cleanup) = args.cleanup {
            cleanup_mode(cleanup);
            return;
        }

        println!("Lege Uninstaller");
        println!("----------------");

        let install_path = detect_install_path();
        if !is_valid_install_root(&install_path) {
            eprintln!(
                "Could not verify install directory. Aborting for safety: {}",
                install_path.display()
            );
            exit(2);
        }

        println!("Install path: {}", install_path.display());
        println!("Press Enter to continue uninstall or Ctrl+C to cancel.");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);

        close_lege_processes();
        remove_start_menu_entries();
        remove_desktop_shortcuts();
        remove_user_data();

        if remove_install_directory_now(&install_path) {
            println!("Install files removed.");
            println!("Uninstall complete.");
            return;
        }

        match spawn_cleanup_helper(&install_path, env::current_exe().ok()) {
            Ok(()) => {
                println!("Scheduled final cleanup.");
                println!("Uninstall complete.");
            }
            Err(error) => {
                eprintln!("Failed to fully remove install directory: {error}");
                exit(1);
            }
        }
    }

    #[derive(Debug, Clone)]
    struct CleanupLaunch {
        install_path: PathBuf,
        helper_path: PathBuf,
        wait_pid: u32,
    }

    #[derive(Debug, Default)]
    struct CleanupArgs {
        cleanup: Option<CleanupLaunch>,
    }

    impl CleanupArgs {
        fn parse() -> Self {
            let mut install_path = None;
            let mut helper_path = None;
            let mut wait_pid = None;

            for arg in env::args().skip(1) {
                if let Some(value) = arg.strip_prefix("--cleanup-install=") {
                    install_path = Some(PathBuf::from(value));
                } else if let Some(value) = arg.strip_prefix("--cleanup-self=") {
                    helper_path = Some(PathBuf::from(value));
                } else if let Some(value) = arg.strip_prefix("--wait-pid=") {
                    wait_pid = value.parse::<u32>().ok();
                }
            }

            Self {
                cleanup: match (install_path, helper_path, wait_pid) {
                    (Some(install_path), Some(helper_path), Some(wait_pid)) => {
                        Some(CleanupLaunch {
                            install_path,
                            helper_path,
                            wait_pid,
                        })
                    }
                    _ => None,
                },
            }
        }
    }

    fn cleanup_mode(cleanup: CleanupLaunch) {
        wait_for_process_exit(cleanup.wait_pid);
        thread::sleep(Duration::from_millis(250));

        close_lege_processes();

        if remove_install_directory_now(&cleanup.install_path) {
            println!("Install files removed.");
        } else {
            eprintln!(
                "Cleanup helper could not remove {}",
                cleanup.install_path.display()
            );
            exit(1);
        }

        schedule_self_delete_on_reboot(&cleanup.helper_path);
    }

    fn detect_install_path() -> PathBuf {
        let current_exe = env::current_exe().ok();

        let mut install_path = current_exe
            .as_ref()
            .and_then(|path| path.parent().map(|dir| dir.to_path_buf()))
            .unwrap_or_else(default_install_path);

        if !is_valid_install_root(&install_path) {
            if let Some(path) = read_manifest_install_path() {
                install_path = path;
            }
        }

        if !is_valid_install_root(&install_path) {
            install_path = default_install_path();
        }

        install_path
    }

    fn default_install_path() -> PathBuf {
        env::var("LOCALAPPDATA")
            .map(|root| PathBuf::from(root).join("Programs").join("Lege"))
            .unwrap_or_else(|_| PathBuf::from(r"C:\Users\Public\AppData\Local\Programs\Lege"))
    }

    fn is_valid_install_root(path: &Path) -> bool {
        path.join("lege.exe").exists()
            || path.join("lege-gui.exe").exists()
            || path.join("uninstall.exe").exists()
    }

    fn read_manifest_install_path() -> Option<PathBuf> {
        let mut candidates = Vec::new();

        if let Ok(program_data) = env::var("ProgramData") {
            candidates.push(
                PathBuf::from(program_data)
                    .join("Lege")
                    .join("install.json"),
            );
        }
        if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(local_app_data)
                    .join("Lege")
                    .join("install.json"),
            );
        }

        for path in candidates {
            if let Ok(content) = fs::read_to_string(path) {
                if let Some(value) = extract_install_path_json(&content) {
                    return Some(PathBuf::from(value));
                }
            }
        }

        None
    }

    fn extract_install_path_json(json: &str) -> Option<String> {
        let key = "\"install_path\"";
        let idx = json.find(key)?;
        let rest = &json[idx + key.len()..];
        let q1 = rest.find('"')?;
        let rest2 = &rest[q1 + 1..];
        let q2 = rest2.find('"')?;
        Some(rest2[..q2].to_string())
    }

    fn close_lege_processes() {
        let _ = Command::new("taskkill")
            .args(["/IM", "lege.exe", "/T", "/F"])
            .output();
        let _ = Command::new("taskkill")
            .args(["/IM", "lege-gui.exe", "/T", "/F"])
            .output();
        thread::sleep(Duration::from_millis(400));
    }

    fn remove_start_menu_entries() {
        if let Ok(appdata) = env::var("APPDATA") {
            let path =
                PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs\\Lege");
            let _ = fs::remove_dir_all(path);
        }
        if let Ok(program_data) = env::var("ProgramData") {
            let path =
                PathBuf::from(program_data).join("Microsoft\\Windows\\Start Menu\\Programs\\Lege");
            let _ = fs::remove_dir_all(path);
        }
    }

    fn remove_desktop_shortcuts() {
        if let Ok(user_profile) = env::var("USERPROFILE") {
            let _ = fs::remove_file(PathBuf::from(user_profile).join("Desktop\\Lege.lnk"));
        }
        if let Ok(public) = env::var("PUBLIC") {
            let _ = fs::remove_file(PathBuf::from(public).join("Desktop\\Lege.lnk"));
        }
    }

    fn remove_user_data() {
        if let Ok(local) = env::var("LOCALAPPDATA") {
            let local_root = PathBuf::from(&local);
            let _ = fs::remove_dir_all(local_root.join("Lege"));
            let _ = fs::remove_file(local_root.join("Lege").join("install.json"));
        }
        if let Ok(roaming) = env::var("APPDATA") {
            let _ = fs::remove_dir_all(PathBuf::from(roaming).join("Lege"));
        }
        if let Ok(program_data) = env::var("ProgramData") {
            let _ = fs::remove_file(
                PathBuf::from(program_data)
                    .join("Lege")
                    .join("install.json"),
            );
        }
    }

    fn remove_install_directory_now(path: &Path) -> bool {
        fs::remove_dir_all(path).is_ok()
    }

    fn spawn_cleanup_helper(
        install_path: &Path,
        current_exe: Option<PathBuf>,
    ) -> Result<(), String> {
        let current_exe =
            current_exe.ok_or_else(|| "Failed to resolve current executable.".to_string())?;
        let helper_path = helper_exe_path()?;
        fs::copy(&current_exe, &helper_path).map_err(|error| {
            format!(
                "Failed to copy cleanup helper from {} to {}: {error}",
                current_exe.display(),
                helper_path.display()
            )
        })?;

        let mut command = Command::new(&helper_path);
        command
            .arg(format!(
                "--cleanup-install={}",
                install_path.to_string_lossy()
            ))
            .arg(format!("--cleanup-self={}", helper_path.to_string_lossy()))
            .arg(format!("--wait-pid={}", std::process::id()));

        command
            .spawn()
            .map_err(|error| format!("Failed to launch cleanup helper: {error}"))?;

        Ok(())
    }

    fn helper_exe_path() -> Result<PathBuf, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("Clock error: {error}"))?
            .as_secs();
        Ok(env::temp_dir().join(format!("lege-uninstall-helper-{stamp}.exe")))
    }

    fn wait_for_process_exit(pid: u32) {
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, false, pid);
            if let Ok(process) = process {
                let _ = WaitForSingleObject(process, 15_000);
                let _ = CloseHandle(process);
            } else {
                thread::sleep(Duration::from_secs(2));
            }
        }
    }

    fn schedule_self_delete_on_reboot(path: &Path) {
        let wide = to_wide_os(path.as_os_str());
        unsafe {
            let _ = MoveFileExW(
                PCWSTR::from_raw(wide.as_ptr()),
                PCWSTR::null(),
                MOVEFILE_DELAY_UNTIL_REBOOT,
            );
        }
    }

    #[allow(dead_code)]
    fn relaunch_elevated_or_exit() -> ! {
        let exe = env::current_exe().unwrap_or_default();
        let exe_wide: Vec<u16> = exe
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let result = ShellExecuteW(
                Some(HWND(std::ptr::null_mut())),
                w!("runas"),
                PCWSTR::from_raw(exe_wide.as_ptr()),
                None,
                None,
                SHOW_WINDOW_CMD(SW_SHOWNORMAL.0),
            );
            if result.0 as usize <= 32 {
                eprintln!("Failed to elevate uninstaller.");
                exit(1);
            }
        }
        exit(0);
    }

    #[allow(dead_code)]
    fn is_elevated() -> bool {
        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(
                GetCurrentProcess(),
                windows::Win32::Security::TOKEN_QUERY,
                &mut token,
            )
            .is_err()
            {
                return false;
            }
            let _ = CloseHandle(token);
            true
        }
    }

    fn to_wide_os(value: &OsStr) -> Vec<u16> {
        value
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }
}

#[cfg(target_os = "windows")]
fn main() {
    windows_uninstaller::run();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("This uninstaller is Windows-only.");
}
