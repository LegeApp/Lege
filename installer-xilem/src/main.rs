#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
mod windows_app {
    use std::env;
    use std::fs;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, CoCreateInstance, CoInitialize, IPersistFile,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, IsUserAnAdmin, ShellExecuteW, ShellLink};
    use windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD;
    use windows::core::{Interface, PCWSTR};
    use xilem::core::fork;
    use xilem::masonry::dpi::LogicalSize;
    use xilem::masonry::layout::{AsUnit, Dim};
    use xilem::masonry::parley::FontFamily;
    use xilem::masonry::parley::style::FontWeight;
    use xilem::palette;
    use xilem::style::Style as _;
    use xilem::tokio::time;
    use xilem::view::{
        CrossAxisAlignment, FlexSpacer, MainAxisAlignment, button, checkbox, flex_col, flex_row,
        label, prose, task, text_input,
    };
    use xilem::winit::error::EventLoopError;
    use xilem::{EventLoop, WidgetView, WindowOptions, Xilem};

    const CONSTANTINE_FONT: &[u8] =
        include_bytes!("../../installer-wpf/Installer.Wpf/Assets/Fonts/Constantine.ttf");

    const WINDOW_BG: xilem::Color = xilem::Color::from_rgb8(0xEE, 0xEC, 0xEF);
    const INPUT_BG: xilem::Color = xilem::Color::from_rgb8(0xFC, 0xFB, 0xFE);
    const PRIMARY: xilem::Color = xilem::Color::from_rgb8(0x2C, 0x21, 0x4A);
    const PRIMARY_HOVER: xilem::Color = xilem::Color::from_rgb8(0x3B, 0x2D, 0x60);
    const PRIMARY_PRESSED: xilem::Color = xilem::Color::from_rgb8(0x24, 0x1B, 0x3D);
    const INPUT_BORDER: xilem::Color = xilem::Color::from_rgb8(0xBF, 0xB8, 0xCC);
    const SECTION_TEXT: xilem::Color = xilem::Color::from_rgb8(0x3E, 0x35, 0x52);
    const MUTED_TEXT: xilem::Color = xilem::Color::from_rgb8(0x5D, 0x55, 0x6F);
    const PAYLOAD: &[u8] = include_bytes!("../Lege-win64.tar.zst");

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum InstallPhase {
        Ready,
        Installing,
        Complete,
        Failed,
    }

    struct InstallerApp {
        install_path: String,
        create_desktop_shortcut: bool,
        create_start_menu_shortcut: bool,
        phase: InstallPhase,
        progress: f64,
        log_lines: Vec<String>,
        install_rx: Option<Receiver<InstallEvent>>,
    }

    #[derive(Debug)]
    enum InstallEvent {
        Log(String),
        Progress(f64),
        Done(Result<(), String>),
    }

    #[derive(Debug, Clone)]
    struct InstallRequest {
        install_path: String,
        create_desktop_shortcut: bool,
        create_start_menu_shortcut: bool,
    }

    #[derive(Debug, Clone)]
    struct LaunchOptions {
        install_path: Option<String>,
        create_desktop_shortcut: Option<bool>,
        create_start_menu_shortcut: Option<bool>,
        auto_start: bool,
    }

    impl InstallerApp {
        fn is_installing(&self) -> bool {
            self.phase == InstallPhase::Installing
        }

        fn add_log(&mut self, message: impl Into<String>) {
            self.log_lines.push(message.into());
        }

        fn primary_button_label(&self) -> &'static str {
            match self.phase {
                InstallPhase::Ready => "Install",
                InstallPhase::Installing => "Installing",
                InstallPhase::Complete => "Install",
                InstallPhase::Failed => "Retry install",
            }
        }

        fn start_install(&mut self) {
            if self.phase == InstallPhase::Installing {
                return;
            }

            if !is_process_elevated() && install_requires_elevation(&self.install_path) {
                match relaunch_elevated(InstallRequest {
                    install_path: self.install_path.clone(),
                    create_desktop_shortcut: self.create_desktop_shortcut,
                    create_start_menu_shortcut: self.create_start_menu_shortcut,
                }) {
                    Ok(()) => {
                        self.phase = InstallPhase::Ready;
                        self.add_log(
                            "Requested administrator privileges. Approve the UAC prompt to continue installation."
                                .to_string(),
                        );
                        return;
                    }
                    Err(error) => {
                        self.phase = InstallPhase::Failed;
                        self.add_log(format!("Unable to relaunch elevated installer: {error}"));
                        return;
                    }
                }
            }

            self.phase = InstallPhase::Installing;
            self.progress = 0.0;
            self.log_lines.clear();
            self.add_log(format!("Target path: {}", self.install_path));

            let request = InstallRequest {
                install_path: self.install_path.clone(),
                create_desktop_shortcut: self.create_desktop_shortcut,
                create_start_menu_shortcut: self.create_start_menu_shortcut,
            };

            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                run_install(request, tx);
            });
            self.install_rx = Some(rx);
        }

        fn tick_install(&mut self) {
            if self.phase != InstallPhase::Installing {
                return;
            }

            let mut clear_receiver = false;
            let mut receiver = self.install_rx.take();
            if let Some(rx) = receiver.as_ref() {
                loop {
                    match rx.try_recv() {
                        Ok(event) => match event {
                            InstallEvent::Log(line) => self.add_log(line),
                            InstallEvent::Progress(value) => self.progress = value.clamp(0.0, 1.0),
                            InstallEvent::Done(result) => {
                                match result {
                                    Ok(()) => {
                                        self.phase = InstallPhase::Complete;
                                        self.progress = 1.0;
                                        self.add_log("Installation finished successfully.");
                                    }
                                    Err(error) => {
                                        self.phase = InstallPhase::Failed;
                                        self.add_log(format!("Install failed: {error}"));
                                    }
                                }
                                clear_receiver = true;
                                break;
                            }
                        },
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            self.phase = InstallPhase::Failed;
                            self.add_log("Install worker disconnected unexpectedly.");
                            clear_receiver = true;
                            break;
                        }
                    }
                }
            }

            if !clear_receiver {
                self.install_rx = receiver.take();
            }
        }

        fn primary_button_action(&mut self) {
            match self.phase {
                InstallPhase::Ready | InstallPhase::Failed => self.start_install(),
                InstallPhase::Installing | InstallPhase::Complete => {}
            }
        }

        fn run_lege_action(&mut self) {
            if launch_installed_lege(&self.install_path).is_ok() {
                std::process::exit(0);
            } else {
                self.phase = InstallPhase::Failed;
                self.add_log("Unable to launch installed Lege.".to_string());
            }
        }
    }

    impl Default for InstallerApp {
        fn default() -> Self {
            Self {
                install_path: default_install_path(),
                create_desktop_shortcut: true,
                create_start_menu_shortcut: true,
                phase: InstallPhase::Ready,
                progress: 0.0,
                log_lines: vec!["Ready to install Lege.".to_string()],
                install_rx: None,
            }
        }
    }

    fn default_install_path() -> String {
        env::var("LOCALAPPDATA")
            .map(|root| PathBuf::from(root).join("Programs").join("Lege"))
            .unwrap_or_else(|_| PathBuf::from(r"C:\Users\Public\AppData\Local\Programs\Lege"))
            .display()
            .to_string()
    }

    impl InstallerApp {
        fn apply_launch_options(&mut self, options: LaunchOptions) {
            if let Some(path) = options.install_path {
                self.install_path = path;
            }
            if let Some(value) = options.create_desktop_shortcut {
                self.create_desktop_shortcut = value;
            }
            if let Some(value) = options.create_start_menu_shortcut {
                self.create_start_menu_shortcut = value;
            }
            if options.auto_start {
                self.start_install();
            }
        }
    }

    fn run_install(request: InstallRequest, tx: Sender<InstallEvent>) {
        let result = install_impl(&request, &tx);
        let _ = tx.send(InstallEvent::Done(result));
    }

    fn install_impl(request: &InstallRequest, tx: &Sender<InstallEvent>) -> Result<(), String> {
        let mut send_log = |line: String| {
            let _ = tx.send(InstallEvent::Log(line));
        };
        let send_progress = |value: f64| {
            let _ = tx.send(InstallEvent::Progress(value));
        };

        send_log("Loading embedded payload...".to_string());
        send_progress(0.05);
        let payload = PAYLOAD;
        send_log(format!(
            "Payload loaded: {:.1} MB",
            payload.len() as f64 / 1024.0 / 1024.0
        ));

        send_log("Decompressing zstd archive...".to_string());
        let decompressed = {
            let mut decompressor = zstd::bulk::Decompressor::new()
                .map_err(|error| format!("Failed to create zstd decompressor: {error}"))?;
            if let Err(error) =
                decompressor.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(27))
            {
                send_log(format!("Warning: unable to adjust zstd window: {error}"));
            }
            decompressor
                .decompress(payload, 512 * 1024 * 1024)
                .map_err(|error| format!("Payload decompression failed: {error}"))?
        };
        send_progress(0.33);

        send_log("Unpacking tar payload...".to_string());
        let temp_dir = tempdir().map_err(|error| format!("Failed to create temp dir: {error}"))?;
        let mut archive = tar::Archive::new(Cursor::new(decompressed));
        archive
            .unpack(temp_dir.path())
            .map_err(|error| format!("Failed to unpack payload tar: {error}"))?;
        send_progress(0.56);

        let install_path = PathBuf::from(&request.install_path);
        send_log(format!("Copying files to {}", install_path.display()));
        fs::create_dir_all(&install_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                format!(
                    "Administrator privileges are required to create {}. Choose a user-writable folder or rerun elevated.",
                    install_path.display()
                )
            } else {
                format!(
                    "Failed to create install directory {}: {error}",
                    install_path.display()
                )
            }
        })?;
        copy_recursively(temp_dir.path(), &install_path)
            .map_err(|error| format!("Failed while copying payload files: {error}"))?;
        send_progress(0.74);

        create_user_data_directory(&mut send_log);
        write_install_manifest(&install_path, &mut send_log);

        if request.create_desktop_shortcut || request.create_start_menu_shortcut {
            match create_shortcuts_windows(
                &install_path,
                request.create_desktop_shortcut,
                request.create_start_menu_shortcut,
            ) {
                Ok(shortcut_logs) => {
                    for line in shortcut_logs {
                        send_log(line);
                    }
                }
                Err(error) => {
                    send_log(format!("Warning: shortcut creation failed: {error}"));
                }
            }
        }

        send_progress(0.93);
        send_log("Installation complete.".to_string());
        send_progress(1.0);
        Ok(())
    }

    fn create_user_data_directory(send_log: &mut impl FnMut(String)) {
        if let Ok(localappdata) = std::env::var("LOCALAPPDATA") {
            let lege_data = PathBuf::from(localappdata).join("Lege");
            match fs::create_dir_all(&lege_data) {
                Ok(()) => send_log(format!("User data directory: {}", lege_data.display())),
                Err(error) => send_log(format!(
                    "Warning: failed creating user data directory {}: {error}",
                    lege_data.display()
                )),
            }
        }
    }

    fn write_install_manifest(install_path: &Path, send_log: &mut impl FnMut(String)) {
        let manifest = format!(
            "{{\n  \"install_path\": \"{}\"\n}}\n",
            install_path.display()
        );
        let install_manifest = install_path.join("install_manifest.json");
        match fs::write(&install_manifest, manifest.as_bytes()) {
            Ok(()) => send_log(format!("Install manifest: {}", install_manifest.display())),
            Err(error) => send_log(format!(
                "Warning: failed to write install manifest {}: {error}",
                install_manifest.display()
            )),
        }

        if let Ok(program_data) = std::env::var("ProgramData") {
            let global_dir = PathBuf::from(program_data).join("Lege");
            if fs::create_dir_all(&global_dir).is_ok() {
                let _ = fs::write(global_dir.join("install.json"), manifest.as_bytes());
            }
        }
    }

    fn create_shortcuts_windows(
        install_path: &Path,
        desktop: bool,
        start_menu: bool,
    ) -> Result<Vec<String>, String> {
        let mut logs = Vec::new();

        let gui_target = install_path.join("lege-gui.exe");
        let cli_target = install_path.join("lege.exe");

        let primary_target = if gui_target.exists() {
            gui_target.clone()
        } else if cli_target.exists() {
            cli_target.clone()
        } else {
            return Err("no executable found for shortcut creation".to_string());
        };

        unsafe {
            let hr = CoInitialize(None);
            if hr.is_err() {
                return Err(format!("COM initialization failed: {hr:?}"));
            }
        }

        if desktop {
            if let Ok(user_profile) = std::env::var("USERPROFILE") {
                let desktop_path = PathBuf::from(user_profile).join("Desktop").join("Lege.lnk");
                create_windows_shortcut(&primary_target, &desktop_path, "Lege", install_path)?;
                logs.push(format!("Desktop shortcut: {}", desktop_path.display()));
            }
        }

        if start_menu {
            if let Ok(appdata) = std::env::var("APPDATA") {
                let folder = PathBuf::from(appdata)
                    .join("Microsoft\\Windows\\Start Menu\\Programs")
                    .join("Lege");
                fs::create_dir_all(&folder).map_err(|error| {
                    format!(
                        "Failed creating Start Menu folder {}: {error}",
                        folder.display()
                    )
                })?;

                let gui_shortcut = folder.join("Lege.lnk");
                create_windows_shortcut(&primary_target, &gui_shortcut, "Lege", install_path)?;
                logs.push(format!("Start Menu shortcut: {}", gui_shortcut.display()));

                if cli_target.exists() {
                    let cli_shortcut = folder.join("Lege CLI.lnk");
                    create_windows_shortcut(&cli_target, &cli_shortcut, "Lege CLI", install_path)?;
                    logs.push(format!("Start Menu shortcut: {}", cli_shortcut.display()));
                }

                if let Some(uninstall_exe) = stage_uninstaller_exe(install_path, &mut logs)? {
                    let uninstall_shortcut = folder.join("Uninstall Lege.lnk");
                    create_windows_shortcut(
                        &uninstall_exe,
                        &uninstall_shortcut,
                        "Uninstall Lege",
                        install_path,
                    )?;
                    logs.push(format!(
                        "Start Menu shortcut: {}",
                        uninstall_shortcut.display()
                    ));
                }
            }
        }

        Ok(logs)
    }

    fn stage_uninstaller_exe(
        install_path: &Path,
        logs: &mut Vec<String>,
    ) -> Result<Option<PathBuf>, String> {
        let destination = install_path.join("uninstall.exe");
        if destination.exists() {
            logs.push(format!(
                "Uninstaller already present: {}",
                destination.display()
            ));
            return Ok(Some(destination));
        }

        let candidates = [
            PathBuf::from("uninstaller/target/release/lege-uninstaller.exe"),
            PathBuf::from("uninstaller/target/debug/lege-uninstaller.exe"),
        ];
        for source in candidates {
            if source.exists() {
                fs::copy(&source, &destination).map_err(|error| {
                    format!(
                        "Failed copying uninstaller from {} to {}: {error}",
                        source.display(),
                        destination.display()
                    )
                })?;
                logs.push(format!("Uninstaller staged: {}", destination.display()));
                return Ok(Some(destination));
            }
        }

        logs.push(
            "Uninstaller not found at uninstaller/target/(release|debug)/lege-uninstaller.exe"
                .to_string(),
        );
        Ok(None)
    }

    fn create_windows_shortcut(
        target: &Path,
        shortcut_path: &Path,
        _name: &str,
        working_dir: &Path,
    ) -> Result<(), String> {
        unsafe {
            let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| format!("Failed to create shell link: {error:?}"))?;

            let target_wide = to_wide(target);
            shell_link
                .SetPath(PCWSTR::from_raw(target_wide.as_ptr()))
                .map_err(|error| format!("Failed setting shortcut target: {error:?}"))?;

            let working_wide = to_wide(working_dir);
            shell_link
                .SetWorkingDirectory(PCWSTR::from_raw(working_wide.as_ptr()))
                .map_err(|error| format!("Failed setting shortcut working dir: {error:?}"))?;

            let persist_file: IPersistFile = shell_link
                .cast()
                .map_err(|error| format!("Failed casting to IPersistFile: {error:?}"))?;

            let shortcut_wide = to_wide(shortcut_path);
            persist_file
                .Save(PCWSTR::from_raw(shortcut_wide.as_ptr()), true)
                .map_err(|error| format!("Failed saving shortcut: {error:?}"))?;
        }
        Ok(())
    }

    fn find_installed_executable(install_path: &Path) -> Option<PathBuf> {
        let gui_target = install_path.join("lege-gui.exe");
        let cli_target = install_path.join("lege.exe");

        if gui_target.exists() {
            Some(gui_target)
        } else if cli_target.exists() {
            Some(cli_target)
        } else {
            None
        }
    }

    fn launch_installed_lege(install_path: &str) -> Result<(), String> {
        let install_path = PathBuf::from(install_path);
        let target = find_installed_executable(&install_path)
            .ok_or_else(|| "installed executable not found".to_string())?;

        let exe_wide = to_wide(&target);
        let working_dir_wide = to_wide(&install_path);
        let result = unsafe {
            ShellExecuteW(
                Some(HWND(std::ptr::null_mut())),
                PCWSTR::null(),
                PCWSTR::from_raw(exe_wide.as_ptr()),
                PCWSTR::null(),
                PCWSTR::from_raw(working_dir_wide.as_ptr()),
                SHOW_WINDOW_CMD(1),
            )
        };

        let code = result.0 as isize;
        if code <= 32 {
            Err(format!("ShellExecuteW failed with code {code}"))
        } else {
            Ok(())
        }
    }

    fn to_wide(path: &Path) -> Vec<u16> {
        path.to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    fn copy_recursively(src: &Path, dst: &Path) -> std::io::Result<()> {
        if src.is_dir() {
            fs::create_dir_all(dst)?;
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                let dst_path = dst.join(entry.file_name());
                copy_recursively(&entry.path(), &dst_path)?;
            }
        } else {
            fs::copy(src, dst)?;
        }
        Ok(())
    }

    fn parse_launch_options() -> LaunchOptions {
        let mut options = LaunchOptions {
            install_path: None,
            create_desktop_shortcut: None,
            create_start_menu_shortcut: None,
            auto_start: false,
        };

        for arg in env::args().skip(1) {
            if let Some(value) = arg.strip_prefix("--install-path=") {
                options.install_path = Some(value.to_string());
            } else if let Some(value) = arg.strip_prefix("--desktop-shortcut=") {
                options.create_desktop_shortcut = parse_bool_flag(value);
            } else if let Some(value) = arg.strip_prefix("--start-menu-shortcut=") {
                options.create_start_menu_shortcut = parse_bool_flag(value);
            } else if arg == "--auto-start" {
                options.auto_start = true;
            }
        }

        options
    }

    fn parse_bool_flag(value: &str) -> Option<bool> {
        match value {
            "1" | "true" | "True" => Some(true),
            "0" | "false" | "False" => Some(false),
            _ => None,
        }
    }

    fn install_requires_elevation(install_path: &str) -> bool {
        let lower = install_path.to_ascii_lowercase();
        lower.starts_with("c:\\program files") || lower.starts_with("c:\\program files (x86)")
    }

    fn is_process_elevated() -> bool {
        unsafe { IsUserAnAdmin().as_bool() }
    }

    fn relaunch_elevated(request: InstallRequest) -> Result<(), String> {
        let exe = env::current_exe()
            .map_err(|error| format!("Failed to resolve current executable: {error}"))?;

        let params = format!(
            "--install-path=\"{}\" --desktop-shortcut={} --start-menu-shortcut={} --auto-start",
            escape_windows_arg(&request.install_path),
            if request.create_desktop_shortcut {
                1
            } else {
                0
            },
            if request.create_start_menu_shortcut {
                1
            } else {
                0
            }
        );

        let exe_wide = to_wide_string(exe.to_string_lossy().as_ref());
        let verb_wide = to_wide_string("runas");
        let params_wide = to_wide_string(&params);

        let result = unsafe {
            ShellExecuteW(
                Some(HWND(std::ptr::null_mut())),
                PCWSTR::from_raw(verb_wide.as_ptr()),
                PCWSTR::from_raw(exe_wide.as_ptr()),
                PCWSTR::from_raw(params_wide.as_ptr()),
                PCWSTR::null(),
                SHOW_WINDOW_CMD(1),
            )
        };

        let code = result.0 as isize;
        if code <= 32 {
            Err(format!("ShellExecuteW failed with code {code}"))
        } else {
            Ok(())
        }
    }

    fn escape_windows_arg(value: &str) -> String {
        value.replace('\"', "\\\"")
    }

    fn install_path_input(state: &InstallerApp) -> impl WidgetView<InstallerApp> + use<> {
        text_input(
            state.install_path.clone(),
            |state: &mut InstallerApp, value| {
                state.install_path = value;
            },
        )
        .text_size(14.)
        .font(FontFamily::Named("Times New Roman".into()))
        .disabled(state.is_installing())
        .text_color(SECTION_TEXT)
        .caret_color(PRIMARY)
        .background_color(INPUT_BG)
        .disabled_background_color(INPUT_BG)
        .border(INPUT_BORDER, 1.)
        .focused_border_color(PRIMARY_HOVER)
        .corner_radius(4.)
        .padding(8.)
        .width(352.px())
    }

    fn shortcut_option_row<Callback>(
        text: &'static str,
        checked: bool,
        on_toggle: Callback,
        disabled: bool,
    ) -> impl WidgetView<InstallerApp> + use<Callback>
    where
        Callback: Fn(&mut InstallerApp, bool) + Send + Sync + 'static,
    {
        flex_row((
            checkbox("", checked, on_toggle)
                .disabled(disabled)
                .background_color(PRIMARY)
                .disabled_background_color(PRIMARY)
                .focused_border_color(PRIMARY_HOVER)
                .padding(4.),
            prose::<InstallerApp, ()>(text)
                .text_size(15.)
                .text_color(SECTION_TEXT),
        ))
        .cross_axis_alignment(CrossAxisAlignment::Center)
        .gap(8.px())
        .width(360.px())
    }

    fn shortcut_options(state: &InstallerApp) -> impl WidgetView<InstallerApp> + use<> {
        flex_col((
            shortcut_option_row(
                "Create desktop shortcuts",
                state.create_desktop_shortcut,
                |state: &mut InstallerApp, checked| state.create_desktop_shortcut = checked,
                state.is_installing(),
            ),
            shortcut_option_row(
                "Create Start Menu shortcuts",
                state.create_start_menu_shortcut,
                |state: &mut InstallerApp, checked| state.create_start_menu_shortcut = checked,
                state.is_installing(),
            ),
        ))
        .cross_axis_alignment(CrossAxisAlignment::Center)
        .gap(6.px())
    }

    fn install_button(state: &InstallerApp) -> impl WidgetView<InstallerApp> + use<> {
        button(
            label(state.primary_button_label())
                .text_size(22.)
                .weight(FontWeight::BOLD)
                .font(FontFamily::Named("Times New Roman".into()))
                .color(palette::css::WHITE),
            |state: &mut InstallerApp| {
                state.primary_button_action();
            },
        )
        .disabled(state.phase == InstallPhase::Installing)
        .padding(8.)
        .background_color(PRIMARY)
        .active_background_color(PRIMARY_PRESSED)
        .disabled_background_color(PRIMARY)
        .border(PRIMARY, 0.)
        .corner_radius(10.)
        .width(220.px())
        .height(48.px())
    }

    fn run_button() -> impl WidgetView<InstallerApp> {
        button(
            label("Run Lege")
                .text_size(20.)
                .weight(FontWeight::BOLD)
                .font(FontFamily::Named("Times New Roman".into()))
                .color(palette::css::WHITE),
            |state: &mut InstallerApp| {
                state.run_lege_action();
            },
        )
        .padding(8.)
        .background_color(PRIMARY_HOVER)
        .active_background_color(PRIMARY_PRESSED)
        .disabled_background_color(PRIMARY_HOVER)
        .border(PRIMARY_HOVER, 0.)
        .corner_radius(10.)
        .width(220.px())
        .height(44.px())
    }

    fn main_panel(state: &InstallerApp) -> impl WidgetView<InstallerApp> + use<> {
        flex_col((
            label("LEGE")
                .text_size(48.)
                .font(FontFamily::Named("Constantine".into()))
                .color(PRIMARY),
            prose::<InstallerApp, ()>(
                "Scanned book PDF preparation for pleasant reading on e-ink readers",
            )
            .text_alignment(xilem::TextAlign::Center)
            .text_size(15.)
            .text_color(MUTED_TEXT)
            .width(360.px()),
            label("Install path:")
                .text_size(18.)
                .font(FontFamily::Named("Times New Roman".into()))
                .color(SECTION_TEXT),
            install_path_input(state),
            shortcut_options(state),
            (state.phase != InstallPhase::Complete).then(|| install_button(state)),
            (state.phase == InstallPhase::Complete).then(|| {
                prose::<InstallerApp, ()>("Lege installed successfully")
                    .text_size(18.)
                    .text_color(SECTION_TEXT)
                    .text_alignment(xilem::TextAlign::Center)
            }),
            (state.phase == InstallPhase::Complete).then(run_button),
        ))
        .main_axis_alignment(MainAxisAlignment::Center)
        .cross_axis_alignment(CrossAxisAlignment::Center)
        .gap(14.px())
        .padding(18.)
        .background_color(WINDOW_BG)
        .border(WINDOW_BG, 0.)
        .width(400.px())
    }

    fn app_logic(state: &mut InstallerApp) -> impl WidgetView<InstallerApp> + use<> {
        let body = flex_col((
            FlexSpacer::Flex(0.7),
            main_panel(state),
            FlexSpacer::Flex(1.3),
        ))
        .main_axis_alignment(MainAxisAlignment::Center)
        .cross_axis_alignment(CrossAxisAlignment::Center)
        .padding(0.)
        .background_color(WINDOW_BG)
        .dims((Dim::Stretch, Dim::Stretch));

        fork(
            body,
            state.is_installing().then(|| {
                task(
                    |proxy, _| async move {
                        let mut interval = time::interval(Duration::from_millis(60));
                        loop {
                            interval.tick().await;
                            if proxy.message(()).is_err() {
                                break;
                            }
                        }
                    },
                    |state: &mut InstallerApp, ()| {
                        state.tick_install();
                    },
                )
            }),
        )
    }

    pub fn run() -> Result<(), EventLoopError> {
        let launch_options = parse_launch_options();
        let options = WindowOptions::new("Lege Installer")
            .with_resizable(false)
            .with_min_inner_size(LogicalSize::new(460., 600.))
            .with_max_inner_size(LogicalSize::new(460., 600.))
            .with_initial_inner_size(LogicalSize::new(460., 600.));

        let mut initial_state = InstallerApp::default();
        initial_state.apply_launch_options(launch_options);

        let app = Xilem::new_simple(initial_state, app_logic, options)
            .with_font(CONSTANTINE_FONT.to_vec());
        app.run_in(EventLoop::with_user_event())
    }

    fn to_wide_string(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(target_os = "windows")]
fn main() -> Result<(), xilem::winit::error::EventLoopError> {
    windows_app::run()
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("This installer build is Windows-only.");
}
