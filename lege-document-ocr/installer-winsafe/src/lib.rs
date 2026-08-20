#[cfg(target_os = "windows")]
pub mod win_utils {
    use std::path::PathBuf;
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
        REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegOpenKeyExW,
        RegQueryValueExW, RegSetValueExW,
    };
    use windows::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_LocalAppData, FOLDERID_Programs, KNOWN_FOLDER_FLAG,
        SHGetKnownFolderPath,
    };
    use windows::core::{GUID, PCWSTR};

    pub const PRODUCT_NAME: &str = "Lege Document OCR";
    pub const INSTALL_MARKER: &str = ".lege-document-ocr-install";
    pub const UNINSTALL_SUBKEY: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\LegeDocumentOCR";

    pub fn to_wide(path: &std::path::Path) -> Vec<u16> {
        path.to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn to_wide_str(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn known_folder_path(id: &GUID) -> Option<PathBuf> {
        unsafe {
            let pwstr = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok()?;
            let result = pwstr.to_string().ok().map(PathBuf::from);
            CoTaskMemFree(Some(pwstr.as_ptr() as *const std::ffi::c_void));
            result
        }
    }

    pub fn desktop_shortcut_path() -> Option<PathBuf> {
        known_folder_path(&FOLDERID_Desktop).map(|p| p.join("Lege Document OCR.lnk"))
    }

    pub fn start_menu_shortcut_path() -> Option<PathBuf> {
        known_folder_path(&FOLDERID_Programs)
            .map(|p| p.join(PRODUCT_NAME).join("Lege Document OCR.lnk"))
    }

    pub fn default_install_path() -> PathBuf {
        known_folder_path(&FOLDERID_LocalAppData)
            .map(|p| p.join("Programs").join(PRODUCT_NAME))
            .unwrap_or_else(|| PathBuf::from(r"C:\LegeDocumentOCR"))
    }

    pub fn write_uninstall_entry(install_path: &std::path::Path, version: &str) {
        let _ = try_write_uninstall_entry(install_path, version);
    }

    fn try_write_uninstall_entry(
        install_path: &std::path::Path,
        version: &str,
    ) -> windows::core::Result<()> {
        let subkey = to_wide_str(UNINSTALL_SUBKEY);
        let mut hkey = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR::from_raw(subkey.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut hkey,
                None,
            )
            .ok()?;
        }

        let display_name = format!("{PRODUCT_NAME} {version}");
        let icon_path = install_path
            .join("LegeDocumentOCR.ico")
            .display()
            .to_string();
        let uninstall_str = install_path
            .join("lege-document-ocr-uninstaller.exe")
            .display()
            .to_string();
        let quiet_uninstall_str = format!("{uninstall_str} --quiet");
        let install_loc = install_path.display().to_string();

        set_reg_sz(hkey, "DisplayName", &display_name);
        set_reg_sz(hkey, "DisplayVersion", version);
        set_reg_sz(hkey, "DisplayIcon", &icon_path);
        set_reg_sz(hkey, "Publisher", "Lege");
        set_reg_sz(hkey, "InstallLocation", &install_loc);
        set_reg_sz(hkey, "UninstallString", &uninstall_str);
        set_reg_sz(hkey, "QuietUninstallString", &quiet_uninstall_str);
        set_reg_dword(hkey, "NoModify", 1);
        set_reg_dword(hkey, "NoRepair", 1);

        unsafe { RegCloseKey(hkey).ok() }
    }

    fn set_reg_sz(hkey: HKEY, name: &str, value: &str) {
        let name_w = to_wide_str(name);
        let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes =
            unsafe { std::slice::from_raw_parts(value_w.as_ptr() as *const u8, value_w.len() * 2) };
        unsafe {
            let _ = RegSetValueExW(
                hkey,
                PCWSTR::from_raw(name_w.as_ptr()),
                None,
                REG_SZ,
                Some(bytes),
            );
        }
    }

    fn set_reg_dword(hkey: HKEY, name: &str, value: u32) {
        let name_w = to_wide_str(name);
        let bytes = value.to_le_bytes();
        unsafe {
            let _ = RegSetValueExW(
                hkey,
                PCWSTR::from_raw(name_w.as_ptr()),
                None,
                REG_DWORD,
                Some(&bytes),
            );
        }
    }

    pub fn delete_uninstall_entry() {
        let subkey = to_wide_str(UNINSTALL_SUBKEY);
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR::from_raw(subkey.as_ptr()));
        }
    }

    pub fn read_install_location() -> Option<PathBuf> {
        let subkey = to_wide_str(UNINSTALL_SUBKEY);
        let mut hkey = HKEY::default();
        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR::from_raw(subkey.as_ptr()),
                None,
                KEY_READ,
                &mut hkey,
            )
        };
        if opened != ERROR_SUCCESS {
            return None;
        }

        let value_name = to_wide_str("InstallLocation");
        let mut buf = vec![0u16; 520];
        let mut data_size = (buf.len() * 2) as u32;
        let mut kind = REG_VALUE_TYPE::default();
        let queried = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR::from_raw(value_name.as_ptr()),
                None,
                Some(&mut kind),
                Some(buf.as_mut_ptr() as *mut u8),
                Some(&mut data_size),
            )
        };
        unsafe {
            let _ = RegCloseKey(hkey);
        }

        if queried != ERROR_SUCCESS {
            return None;
        }

        let char_count = (data_size as usize / 2).saturating_sub(1);
        let s = String::from_utf16_lossy(&buf[..char_count]);
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    }

    pub fn try_single_instance() -> Option<SingleInstanceGuard> {
        use windows::Win32::System::Threading::CreateMutexW;

        let name = to_wide_str("Local\\LegeDocumentOCRInstaller");
        let handle = unsafe { CreateMutexW(None, true, PCWSTR::from_raw(name.as_ptr())) }.ok()?;

        let last_err = unsafe { windows::Win32::Foundation::GetLastError() };
        if last_err == ERROR_ALREADY_EXISTS {
            return None;
        }
        Some(SingleInstanceGuard(handle))
    }

    pub struct SingleInstanceGuard(windows::Win32::Foundation::HANDLE);

    impl Drop for SingleInstanceGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }

    pub fn is_safe_install_root(path: &std::path::Path) -> bool {
        if !path.is_absolute() || !path.join(INSTALL_MARKER).is_file() {
            return false;
        }
        let component_count = path.components().count();
        if component_count < 3 || path.parent().is_none() {
            return false;
        }
        path.join("lege-ocr.exe").is_file()
            && path.join("package-manifest.json").is_file()
            && path.join("tensorrt").is_dir()
    }

    #[cfg(test)]
    mod tests {
        use super::{INSTALL_MARKER, is_safe_install_root};

        #[test]
        fn deletion_guard_rejects_missing_marker_and_shallow_paths() {
            let root = std::env::temp_dir()
                .join(format!("lege-ocr-uninstall-guard-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("tensorrt")).unwrap();
            std::fs::write(root.join("lege-ocr.exe"), []).unwrap();
            std::fs::write(root.join("package-manifest.json"), "{}").unwrap();
            assert!(!is_safe_install_root(&root));
            std::fs::write(root.join(INSTALL_MARKER), "Lege Document OCR").unwrap();
            assert!(is_safe_install_root(&root));
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn deletion_guard_rejects_drive_root() {
            assert!(!is_safe_install_root(std::path::Path::new(r"C:\")));
        }
    }
}
