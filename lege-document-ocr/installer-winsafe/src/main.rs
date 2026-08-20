#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
mod app {
    use anyhow::{Context, Result, anyhow};
    use lege_document_ocr_installer_winsafe::win_utils::{
        INSTALL_MARKER, default_install_path, desktop_shortcut_path, is_safe_install_root,
        start_menu_shortcut_path, to_wide, to_wide_str, try_single_instance, write_uninstall_entry,
    };
    use std::cell::{Cell, RefCell};
    use std::ffi::c_void;
    use std::fs;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::rc::Rc;
    use std::thread;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoTaskMemFree, CoUninitialize, IPersistFile,
    };
    use windows::Win32::UI::Shell::{FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, IShellLinkW, ShellLink};
    use windows::core::{Interface, PCWSTR};
    use winsafe::{self as w, co, gui, prelude::*};

    const PAYLOAD_7Z: &[u8] = include_bytes!(env!("LEGE_OCR_PAYLOAD_PATH"));
    const UNINSTALLER_EXE: &[u8] = include_bytes!(env!("LEGE_OCR_UNINSTALLER_PATH"));
    const SHORTCUT_ICON: &[u8] = include_bytes!(env!("LEGE_OCR_ICON_ICO"));
    const EXPECTED_LEGE_OCR_VERSION: &str = env!("EXPECTED_LEGE_OCR_VERSION");

    // Suppress console windows when spawning child processes from a GUI app.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    const W: i32 = 500;
    const H: i32 = 640;

    // Freya palette — matches the Freya installer colour constants exactly.
    const CINNABAR: (u8, u8, u8) = (154, 73, 55);
    const CINNABAR_HOVER: (u8, u8, u8) = (175, 91, 69);
    const PAPER: (u8, u8, u8) = (247, 244, 234);
    const PANEL_BG: (u8, u8, u8) = (236, 231, 216);
    const TEXT: (u8, u8, u8) = (23, 36, 38);
    const MUTED: (u8, u8, u8) = (93, 104, 102);
    const BORDER: (u8, u8, u8) = (138, 124, 99);
    const TEXT_INVERSE: (u8, u8, u8) = (255, 249, 238); // button label on cinnabar

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Phase {
        Ready,
        Installing,
        Complete,
        Failed,
    }

    /// Minimal installer state. Progress and log text are intentionally NOT stored
    /// here — the ProgressBar control and log Edit control are the canonical stores.
    #[derive(Clone, Debug)]
    struct InstallerState {
        phase: Phase,
        step: String,
        install_path: String,
        already_installed: bool,
    }

    impl Default for InstallerState {
        fn default() -> Self {
            Self {
                phase: Phase::Ready,
                step: "Ready to install Lege Document OCR.".to_string(),
                install_path: default_install_path().display().to_string(),
                already_installed: false,
            }
        }
    }

    #[derive(Clone, Debug)]
    struct InstallRequest {
        install_path: PathBuf,
        desktop_shortcut: bool,
        start_menu_shortcut: bool,
        verify_backend: bool,
        install_launcher: bool,
        register_uninstaller: bool,
    }

    #[derive(Debug)]
    enum InstallerEvent {
        Step(String, f32),
        Log(String),
        AlreadyInstalled,
        Done(Result<(), String>),
    }

    /// RAII guard for COM apartment initialisation — initialises on construction,
    /// calls CoUninitialize on drop (only if init succeeded).
    struct ComGuard {
        initialized: bool,
    }

    impl ComGuard {
        fn init_sta() -> Self {
            let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() };
            Self { initialized }
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.initialized {
                unsafe {
                    CoUninitialize();
                }
            }
        }
    }

    #[derive(Clone)]
    pub struct InstallerWindow {
        wnd: gui::WindowMain,

        // Controls kept as fields so their event handlers and Win32 lifetimes are managed.
        // Fields not read after setup are allowed dead_code.
        #[allow(dead_code)]
        subtitle: gui::Label,
        #[allow(dead_code)]
        path_label: gui::Label,
        path_edit: gui::Edit,
        browse_btn: gui::Button,
        desktop_cb: gui::CheckBox,
        start_menu_cb: gui::CheckBox,
        verify_backend_cb: gui::CheckBox,
        install_launcher_cb: gui::CheckBox,
        step_label: gui::Label,
        // Owner-drawn progress bar (an SS_OWNERDRAW static). The native ProgressBar's
        // PBM_SETBKCOLOR/PBM_SETBARCOLOR rendered an unthemed black trough, so we paint
        // it ourselves on WM_DRAWITEM to guarantee the cinnabar/cream palette.
        progress_bar: gui::Label,
        log_edit: gui::Edit,
        action_btn: gui::Button,

        state: Rc<RefCell<InstallerState>>,
        rx: Rc<RefCell<Option<flume::Receiver<InstallerEvent>>>>,
        install_running: Rc<Cell<bool>>,
        /// Monotonic progress floor for asynchronous install events.
        progress_floor: Rc<Cell<u32>>,
    }

    impl InstallerWindow {
        pub fn create_and_run() -> w::AnyResult<i32> {
            let _mutex = match try_single_instance() {
                Some(guard) => guard,
                None => {
                    eprintln!("Lege Document OCR Installer is already running.");
                    return Ok(0);
                }
            };

            let state = Rc::new(RefCell::new(InstallerState::default()));
            let default_path = state.borrow().install_path.clone();

            // Theme resources — leaked so they live for the process lifetime.
            // Three small GDI objects; intentional leak is standard for per-app brushes/fonts.
            let br_paper = {
                let mut g =
                    w::HBRUSH::CreateSolidBrush(w::COLORREF::from_rgb(PAPER.0, PAPER.1, PAPER.2))?;
                g.leak()
            };
            let br_panel = {
                let mut g = w::HBRUSH::CreateSolidBrush(w::COLORREF::from_rgb(
                    PANEL_BG.0, PANEL_BG.1, PANEL_BG.2,
                ))?;
                g.leak()
            };
            let hfont_subtitle = {
                // Header font — slightly larger than body, semibold.
                let mut lf = w::LOGFONT::new_face(-21, "Segoe UI"); // ≈16pt at 96 DPI
                lf.lfWeight = co::FW::SEMIBOLD;
                let mut g = w::HFONT::CreateFontIndirect(&lf)?;
                g.leak()
            };
            let hfont_ui = {
                let mut g = w::HFONT::CreateFontIndirect(&w::LOGFONT::new_face(-13, "Segoe UI"))?;
                g.leak()
            };

            let wnd = gui::WindowMain::new(gui::WindowMainOpts {
                title: "Lege Document OCR Installer",
                size: gui::dpi(W, H),
                style: co::WS::CAPTION | co::WS::SYSMENU | co::WS::MINIMIZEBOX,
                ..Default::default()
            });

            // Header — centered, slightly larger than body text.
            let subtitle = gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: "TensorRT-first batch PDF OCR pipeline",
                    position: gui::dpi(20, 30),
                    size: gui::dpi(448, 30),
                    control_style: co::SS::CENTER,
                    ..Default::default()
                },
            );

            let path_label = gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: "Install path",
                    position: gui::dpi(20, 92),
                    size: gui::dpi(440, 20),
                    ..Default::default()
                },
            );

            let (edit_w, edit_h) = gui::dpi(398, 26);
            let path_edit = gui::Edit::new(
                &wnd,
                gui::EditOpts {
                    text: &default_path,
                    position: gui::dpi(20, 116),
                    width: edit_w,
                    height: edit_h,
                    ..Default::default()
                },
            );

            let (browse_w, browse_h) = gui::dpi(42, 28);
            let browse_btn = gui::Button::new(
                &wnd,
                gui::ButtonOpts {
                    text: "...",
                    position: gui::dpi(426, 115),
                    width: browse_w,
                    height: browse_h,
                    ..Default::default()
                },
            );

            let desktop_cb = gui::CheckBox::new(
                &wnd,
                gui::CheckBoxOpts {
                    text: "Create desktop shortcut",
                    position: gui::dpi(20, 154),
                    size: gui::dpi(440, 24),
                    check_state: co::BST::CHECKED,
                    ..Default::default()
                },
            );

            let start_menu_cb = gui::CheckBox::new(
                &wnd,
                gui::CheckBoxOpts {
                    text: "Create Start Menu shortcut",
                    position: gui::dpi(20, 182),
                    size: gui::dpi(440, 24),
                    check_state: co::BST::CHECKED,
                    ..Default::default()
                },
            );

            let verify_backend_cb = gui::CheckBox::new(
                &wnd,
                gui::CheckBoxOpts {
                    text: "TensorRT primary on NVIDIA (required and verified)",
                    position: gui::dpi(20, 210),
                    size: gui::dpi(440, 24),
                    check_state: co::BST::CHECKED,
                    ..Default::default()
                },
            );

            let install_launcher_cb = gui::CheckBox::new(
                &wnd,
                gui::CheckBoxOpts {
                    text: "Install a Lege OCR command-prompt launcher",
                    position: gui::dpi(20, 238),
                    size: gui::dpi(440, 24),
                    check_state: co::BST::CHECKED,
                    ..Default::default()
                },
            );

            let step_label = gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: "Ready to install Lege Document OCR.",
                    position: gui::dpi(20, 278),
                    size: gui::dpi(448, 22),
                    ..Default::default()
                },
            );

            let progress_bar = gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: "",
                    position: gui::dpi(20, 306),
                    size: gui::dpi(448, 22),
                    control_style: co::SS::OWNERDRAW,
                    ..Default::default()
                },
            );

            let (log_w, log_h) = gui::dpi(448, 194);
            let log_edit = gui::Edit::new(
                &wnd,
                gui::EditOpts {
                    text: "",
                    position: gui::dpi(20, 348),
                    width: log_w,
                    height: log_h,
                    control_style: co::ES::MULTILINE
                        | co::ES::AUTOVSCROLL
                        | co::ES::READONLY
                        | co::ES::WANTRETURN,
                    window_style: co::WS::CHILD
                        | co::WS::VISIBLE
                        | co::WS::BORDER
                        | co::WS::VSCROLL
                        | co::WS::TABSTOP,
                    ..Default::default()
                },
            );

            let (action_w, action_h) = gui::dpi(448, 38);
            let action_btn = gui::Button::new(
                &wnd,
                gui::ButtonOpts {
                    text: "Install",
                    position: gui::dpi(20, 560),
                    width: action_w,
                    height: action_h,
                    // Owner-drawn so we can paint it CINNABAR (themed buttons ignore colour messages).
                    control_style: co::BS::OWNERDRAW,
                    ..Default::default()
                },
            );

            let new_self = Self {
                wnd,
                subtitle,
                path_label,
                path_edit,
                browse_btn,
                desktop_cb,
                start_menu_cb,
                verify_backend_cb,
                install_launcher_cb,
                step_label,
                progress_bar,
                log_edit,
                action_btn,
                state,
                rx: Rc::new(RefCell::new(None)),
                install_running: Rc::new(Cell::new(false)),
                progress_floor: Rc::new(Cell::new(0)),
            };

            new_self.events(br_paper, br_panel, hfont_subtitle, hfont_ui);
            new_self.wnd.run_main(None)
        }

        fn events(
            &self,
            br_paper: w::HBRUSH,
            br_panel: w::HBRUSH,
            hfont_subtitle: w::HFONT,
            hfont_ui: w::HFONT,
        ) {
            // HBRUSH/HFONT are not Copy. Create raw non-owning copies for closures that
            // only borrow the handle value (correct for WM_CTLCOLOR return and WM_SETFONT).
            // raw_copy() is unsafe but semantically fine: the GDI objects outlive the process.
            let br_paper_for_static = unsafe { br_paper.raw_copy() };
            // Separate copy for the owner-draw button (hfont_ui is moved into wm_create below).
            let hfont_ui_btn = unsafe { hfont_ui.raw_copy() };

            // Clones for captures — gui controls are cheap reference-counted wrappers.
            let wnd_clone = self.wnd.clone();
            let subtitle_ctrl = self.subtitle.clone();
            let path_label_ctrl = self.path_label.clone();
            let step_label_ctrl = self.step_label.clone();
            let path_edit_ctrl = self.path_edit.clone();
            let desktop_cb_ctrl = self.desktop_cb.clone();
            let start_menu_cb_ctrl = self.start_menu_cb.clone();
            let verify_backend_cb_ctrl = self.verify_backend_cb.clone();
            let install_launcher_cb_ctrl = self.install_launcher_cb.clone();
            let log_edit_ctrl = self.log_edit.clone();

            self.wnd.on().wm_create(move |_| {
                wnd_clone.hwnd().SetTimer(1, 75, None)?;

                // Header font on the subtitle, Segoe UI on the rest.
                // raw_copy() borrows the captured handle and produces a non-owning value.
                // This is safe here: the leaked GDI objects live for the process.
                unsafe {
                    subtitle_ctrl.hwnd().SendMessage(w::msg::wm::SetFont {
                        hfont: hfont_subtitle.raw_copy(),
                        redraw: true,
                    });
                    for hwnd in [
                        path_label_ctrl.hwnd(),
                        step_label_ctrl.hwnd(),
                        path_edit_ctrl.hwnd(),
                        desktop_cb_ctrl.hwnd(),
                        start_menu_cb_ctrl.hwnd(),
                        verify_backend_cb_ctrl.hwnd(),
                        install_launcher_cb_ctrl.hwnd(),
                        log_edit_ctrl.hwnd(),
                    ] {
                        hwnd.SendMessage(w::msg::wm::SetFont {
                            hfont: hfont_ui.raw_copy(),
                            redraw: true,
                        });
                    }
                }
                verify_backend_cb_ctrl.hwnd().EnableWindow(false);
                Ok(0)
            });

            // Window background → PAPER (fills all non-control pixels).
            let wnd_for_erase = self.wnd.clone();
            self.wnd.on().wm_erase_bkgnd(move |p| {
                let rc = wnd_for_erase.hwnd().GetClientRect()?;
                p.hdc.FillRect(rc, &br_paper)?;
                Ok(1)
            });

            // Static controls (labels, checkboxes): PAPER background. The subtitle/header
            // uses MUTED; everything else uses TEXT.
            let subtitle_for_color = self.subtitle.clone();
            self.wnd.on().wm_ctl_color_static(move |p| {
                let color = if p.hwnd == *subtitle_for_color.hwnd() {
                    w::COLORREF::from_rgb(MUTED.0, MUTED.1, MUTED.2)
                } else {
                    w::COLORREF::from_rgb(TEXT.0, TEXT.1, TEXT.2)
                };
                p.hdc.SetTextColor(color)?;
                p.hdc
                    .SetBkColor(w::COLORREF::from_rgb(PAPER.0, PAPER.1, PAPER.2))?;
                // raw_copy(): system doesn't take ownership of the returned brush.
                Ok(unsafe { br_paper_for_static.raw_copy() })
            });

            // Edit controls: PANEL_BG background, TEXT colour.
            self.wnd.on().wm_ctl_color_edit(move |p| {
                p.hdc
                    .SetTextColor(w::COLORREF::from_rgb(TEXT.0, TEXT.1, TEXT.2))?;
                p.hdc
                    .SetBkColor(w::COLORREF::from_rgb(PANEL_BG.0, PANEL_BG.1, PANEL_BG.2))?;
                Ok(unsafe { br_panel.raw_copy() })
            });

            // Owner-draw both the action button (flat CINNABAR, matches the Freya primary
            // button) and the progress bar (cinnabar fill on a cream trough). Themed
            // controls ignore colour messages, so we paint them ourselves on WM_DRAWITEM,
            // which the parent receives for every owner-draw child.
            let action_btn_for_draw = self.action_btn.clone();
            let progress_for_draw = self.progress_bar.clone();
            let progress_pct = self.progress_floor.clone();
            self.wnd
                .on()
                .wm(co::WM::DRAWITEM, move |p: w::msg::WndMsg| {
                    let dis = unsafe { &*(p.lparam as *const w::DRAWITEMSTRUCT) };

                    // ── Progress bar ────────────────────────────────────────────
                    if dis.hwndItem == *progress_for_draw.hwnd() {
                        let full = dis.rcItem;
                        let border = w::HBRUSH::CreateSolidBrush(w::COLORREF::from_rgb(
                            BORDER.0, BORDER.1, BORDER.2,
                        ))?;
                        let trough = w::HBRUSH::CreateSolidBrush(w::COLORREF::from_rgb(
                            PANEL_BG.0, PANEL_BG.1, PANEL_BG.2,
                        ))?;
                        // 1px border, then the cream trough inset by 1px.
                        dis.hDC.FillRect(full, &border)?;
                        let inner = w::RECT {
                            left: full.left + 1,
                            top: full.top + 1,
                            right: full.right - 1,
                            bottom: full.bottom - 1,
                        };
                        dis.hDC.FillRect(inner, &trough)?;
                        // Cinnabar fill proportional to percent.
                        let pct = progress_pct.get().min(100);
                        let span = inner.right - inner.left;
                        let fill_w = span * pct as i32 / 100;
                        if fill_w > 0 {
                            let fill = w::HBRUSH::CreateSolidBrush(w::COLORREF::from_rgb(
                                CINNABAR.0, CINNABAR.1, CINNABAR.2,
                            ))?;
                            let bar = w::RECT {
                                left: inner.left,
                                top: inner.top,
                                right: inner.left + fill_w,
                                bottom: inner.bottom,
                            };
                            dis.hDC.FillRect(bar, &fill)?;
                        }
                        return Ok(1);
                    }

                    // ── Action button ───────────────────────────────────────────
                    if dis.hwndItem != *action_btn_for_draw.hwnd() {
                        return Ok(0); // not one of our owner-draw controls
                    }
                    let pressed = dis.itemState.has(co::ODS::SELECTED);
                    let disabled = dis.itemState.has(co::ODS::DISABLED);
                    let fill = if disabled {
                        w::COLORREF::from_rgb(150, 110, 100) // muted cinnabar
                    } else if pressed {
                        w::COLORREF::from_rgb(CINNABAR_HOVER.0, CINNABAR_HOVER.1, CINNABAR_HOVER.2)
                    } else {
                        w::COLORREF::from_rgb(CINNABAR.0, CINNABAR.1, CINNABAR.2)
                    };
                    let brush = w::HBRUSH::CreateSolidBrush(fill)?;
                    dis.hDC.FillRect(dis.rcItem, &brush)?;

                    // Centered label in the inverse (cream) text colour, Segoe UI.
                    // SelectObject returns a guard that re-selects the previous font on drop.
                    let font = unsafe { hfont_ui_btn.raw_copy() };
                    let _prev_font = dis.hDC.SelectObject(&font);
                    dis.hDC.SetBkMode(co::BKMODE::TRANSPARENT)?;
                    dis.hDC.SetTextColor(w::COLORREF::from_rgb(
                        TEXT_INVERSE.0,
                        TEXT_INVERSE.1,
                        TEXT_INVERSE.2,
                    ))?;
                    let label = action_btn_for_draw.hwnd().GetWindowText()?;
                    dis.hDC.DrawText(
                        &label,
                        dis.rcItem,
                        co::DT::CENTER | co::DT::VCENTER | co::DT::SINGLELINE,
                    )?;
                    Ok(1) // handled
                });

            let me = self.clone();
            self.browse_btn.on().bn_clicked(move || {
                if let Some(path) = pick_folder_blocking() {
                    me.path_edit.hwnd().SetWindowText(&path)?;
                }
                Ok(())
            });

            let me = self.clone();
            self.action_btn.on().bn_clicked(move || {
                let phase = me.state.borrow().phase;
                match phase {
                    Phase::Complete => {
                        let path = me.state.borrow().install_path.clone();
                        let _ = Command::new("explorer").arg(path).spawn();
                    }
                    Phase::Installing => {}
                    Phase::Ready | Phase::Failed => me.start_install()?,
                }
                Ok(())
            });

            let me = self.clone();
            self.wnd.on().wm_timer(1, move || {
                me.drain_events()?;
                Ok(())
            });
        }

        fn start_install(&self) -> w::SysResult<()> {
            if self.install_running.get() {
                return Ok(());
            }

            let install_path_text = self.path_edit.hwnd().GetWindowText()?;
            let install_path = match validate_install_path(install_path_text.trim()) {
                Ok(path) => path,
                Err(error) => {
                    {
                        let mut s = self.state.borrow_mut();
                        s.phase = Phase::Failed;
                        s.step = "Install path is not valid.".to_string();
                    }
                    self.set_step("Install path is not valid.")?;
                    self.append_log(&format!("Error: {error}"))?;
                    self.set_action_label("Retry install")?;
                    return Ok(());
                }
            };

            let desktop_checked = self.desktop_cb.is_checked();
            let start_menu_checked = self.start_menu_cb.is_checked();
            let install_launcher = self.install_launcher_cb.is_checked();
            let request = InstallRequest {
                install_path,
                desktop_shortcut: desktop_checked,
                start_menu_shortcut: start_menu_checked,
                verify_backend: true,
                install_launcher,
                register_uninstaller: true,
            };

            // Build display string once, use in both state and UI.
            let path_display = request.install_path.display().to_string();
            {
                let mut s = self.state.borrow_mut();
                s.phase = Phase::Installing;
                s.step = "Starting installation".to_string();
                s.install_path = path_display.clone();
                s.already_installed = false;
            }
            self.install_running.set(true);
            self.progress_floor.set(0);

            self.set_enabled_controls(false)?;
            self.set_action_label("Installing")?;
            self.set_step("Starting installation")?;
            self.repaint_progress()?;
            self.reset_log(&format!("Target path: {path_display}\r\n"))?;

            let (tx, rx) = flume::unbounded::<InstallerEvent>();
            *self.rx.borrow_mut() = Some(rx);

            thread::spawn(move || {
                let result = install_impl(request, tx.clone()).map_err(|e| e.to_string());
                let _ = tx.send(InstallerEvent::Done(result));
            });

            Ok(())
        }

        fn drain_events(&self) -> w::SysResult<()> {
            let Some(rx) = self.rx.borrow().as_ref().cloned() else {
                return Ok(());
            };
            for event in rx.try_iter().take(50) {
                let done = matches!(event, InstallerEvent::Done(_));
                self.apply_event(event)?;
                if done {
                    *self.rx.borrow_mut() = None;
                    self.install_running.set(false);
                    break;
                }
            }
            Ok(())
        }

        fn apply_event(&self, event: InstallerEvent) -> w::SysResult<()> {
            match event {
                InstallerEvent::Step(step, progress_float) => {
                    // Monotonic clamp keeps asynchronous steps from moving backwards.
                    let pct = ((progress_float.clamp(0.0, 1.0) * 100.0).round() as u32)
                        .max(self.progress_floor.get());
                    self.progress_floor.set(pct);
                    {
                        let mut s = self.state.borrow_mut();
                        s.step = step.clone();
                    }
                    self.set_step(&step)?;
                    self.repaint_progress()?;
                    self.append_log(&step)?;
                }
                InstallerEvent::Log(line) => {
                    self.append_log(&line)?;
                }
                InstallerEvent::AlreadyInstalled => {
                    self.state.borrow_mut().already_installed = true;
                }
                InstallerEvent::Done(Ok(())) => {
                    // Collect everything from state in one borrow, then drop before UI calls.
                    let (step_text, log_line) = {
                        let mut s = self.state.borrow_mut();
                        s.phase = Phase::Complete;
                        if s.already_installed {
                            (
                                "Lege Document OCR is already installed — nothing to do."
                                    .to_string(),
                                "Already installed; skipped.",
                            )
                        } else {
                            (
                                "Lege Document OCR installed successfully.".to_string(),
                                "Installation complete.",
                            )
                        }
                    }; // borrow_mut dropped here — safe to call UI methods below
                    self.progress_floor.set(100);
                    self.repaint_progress()?;
                    self.set_enabled_controls(true)?;
                    self.set_action_label("Open install folder")?;
                    self.set_step(&step_text)?;
                    self.append_log(log_line)?;
                }
                InstallerEvent::Done(Err(error)) => {
                    let log_line = format!("Install failed: {error}");
                    {
                        let mut s = self.state.borrow_mut();
                        s.phase = Phase::Failed;
                        s.step = "Install failed.".to_string();
                    }
                    self.set_enabled_controls(true)?;
                    self.set_action_label("Retry install")?;
                    self.set_step("Install failed.")?;
                    self.append_log(&log_line)?;
                }
            }
            Ok(())
        }

        fn set_enabled_controls(&self, enabled: bool) -> w::SysResult<()> {
            self.path_edit.hwnd().EnableWindow(enabled);
            self.browse_btn.hwnd().EnableWindow(enabled);
            self.desktop_cb.hwnd().EnableWindow(enabled);
            self.start_menu_cb.hwnd().EnableWindow(enabled);
            self.verify_backend_cb.hwnd().EnableWindow(false);
            self.install_launcher_cb.hwnd().EnableWindow(enabled);
            // Action button stays clickable throughout (retry / open-folder / observe).
            self.action_btn.hwnd().EnableWindow(true);
            Ok(())
        }

        fn set_step(&self, text: &str) -> w::SysResult<()> {
            self.step_label.hwnd().SetWindowText(text)
        }

        /// Invalidate the owner-drawn progress bar so it repaints with the current
        /// progress_floor percentage (drives a WM_DRAWITEM on the parent).
        fn repaint_progress(&self) -> w::SysResult<()> {
            self.progress_bar.hwnd().InvalidateRect(None, false)
        }

        fn set_action_label(&self, text: &str) -> w::SysResult<()> {
            self.action_btn.hwnd().SetWindowText(text)
        }

        /// Replace the entire log (called at install start to clear previous run).
        fn reset_log(&self, text: &str) -> w::SysResult<()> {
            self.log_edit.hwnd().SetWindowText(text)?;
            unsafe {
                // -1 sentinel: move cursor to end without counting code units.
                self.log_edit
                    .hwnd()
                    .SendMessage(w::msg::em::SetSel { start: -1, end: -1 });
                self.log_edit.hwnd().SendMessage(w::msg::em::ScrollCaret {});
            }
            Ok(())
        }

        /// Append one line to the log. The Edit control is the canonical log store;
        /// no parallel String is maintained in state.
        fn append_log(&self, line: &str) -> w::SysResult<()> {
            let hwnd = self.log_edit.hwnd();
            let mut text = hwnd.GetWindowText()?;
            if !text.is_empty() && !text.ends_with("\r\n") {
                text.push_str("\r\n");
            }
            text.push_str(line);
            text.push_str("\r\n");
            hwnd.SetWindowText(&text)?;
            // -1 sentinel moves cursor to end — no encode_utf16().count() needed.
            unsafe {
                hwnd.SendMessage(w::msg::em::SetSel { start: -1, end: -1 });
                hwnd.SendMessage(w::msg::em::ScrollCaret {});
            }
            Ok(())
        }
    }

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.iter().any(|arg| arg == "--quiet") {
            return run_quiet(&args);
        }
        match InstallerWindow::create_and_run() {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Lege Document OCR Installer error: {error}");
                1
            }
        }
    }

    fn run_quiet(args: &[String]) -> i32 {
        let install_path = option_value(args, "--install-dir")
            .map(PathBuf::from)
            .unwrap_or_else(default_install_path);
        let install_path = match validate_install_path(&install_path.display().to_string()) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("Invalid install path: {error}");
                return 2;
            }
        };
        let no_shortcuts = args.iter().any(|arg| arg == "--no-shortcuts");
        let request = InstallRequest {
            install_path,
            desktop_shortcut: !no_shortcuts,
            start_menu_shortcut: !no_shortcuts,
            verify_backend: true,
            install_launcher: !args.iter().any(|arg| arg == "--no-launcher"),
            register_uninstaller: !args.iter().any(|arg| arg == "--no-register"),
        };
        let (tx, rx) = flume::unbounded();
        let result = install_impl(request, tx);
        for event in rx.try_iter() {
            match event {
                InstallerEvent::Step(label, _) | InstallerEvent::Log(label) => println!("{label}"),
                InstallerEvent::AlreadyInstalled | InstallerEvent::Done(_) => {}
            }
        }
        match result {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("Install failed: {error:#}");
                1
            }
        }
    }

    fn option_value(args: &[String], name: &str) -> Option<String> {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].clone())
    }

    // ── Install backend ───────────────────────────────────────────────────────

    fn install_impl(request: InstallRequest, tx: flume::Sender<InstallerEvent>) -> Result<()> {
        if PAYLOAD_7Z.is_empty() || UNINSTALLER_EXE.is_empty() {
            return Err(anyhow!(
                "this is a development installer shell without an embedded payload; +                 build the distributable with scripts/build_windows_installer.ps1"
            ));
        }

        step(&tx, "Checking for an existing installation", 0.01);
        if let Some(summary) = existing_install_summary(&request) {
            log_msg(&tx, summary);
            let _ = tx.send(InstallerEvent::AlreadyInstalled);
            return Ok(());
        }
        if request.install_path.is_dir()
            && request
                .install_path
                .read_dir()
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false)
            && !is_safe_install_root(&request.install_path)
        {
            return Err(anyhow!(
                "refusing to overwrite non-product directory {}",
                request.install_path.display()
            ));
        }

        step(&tx, "Creating install directory", 0.03);
        fs::create_dir_all(&request.install_path)
            .with_context(|| format!("creating {}", request.install_path.display()))?;

        step(&tx, "Extracting the LZMA2-compressed OCR runtime", 0.08);
        extract_payload_archive(&request.install_path).with_context(|| {
            format!("extracting payload into {}", request.install_path.display())
        })?;

        step(&tx, "Validating the TensorRT-first payload", 0.72);
        for relative in required_payload_paths() {
            let path = request.install_path.join(relative);
            if !path.is_file() {
                return Err(anyhow!(
                    "payload archive did not produce {}",
                    path.display()
                ));
            }
        }
        let exe_path = request.install_path.join("lege-ocr.exe");

        let icon_path = request.install_path.join("LegeDocumentOCR.ico");
        fs::write(&icon_path, SHORTCUT_ICON)
            .with_context(|| format!("writing {}", icon_path.display()))?;

        step(&tx, "Deploying the safe uninstaller", 0.76);
        let uninstaller_path = request
            .install_path
            .join("lege-document-ocr-uninstaller.exe");
        fs::write(&uninstaller_path, UNINSTALLER_EXE)
            .with_context(|| format!("writing {}", uninstaller_path.display()))?;

        let launcher_path = request
            .install_path
            .join("Lege Document OCR Command Prompt.cmd");
        if request.install_launcher {
            fs::write(
                &launcher_path,
                b"@echo off\r\ntitle Lege Document OCR\r\ncd /d \"%~dp0\"\r\necho Lege Document OCR - TensorRT primary, WinOCR no-NVIDIA fallback\r\necho Run lege-ocr.exe --help for commands.\r\ncmd /k\r\n",
            )
            .with_context(|| format!("writing {}", launcher_path.display()))?;
        }

        fs::write(
            request.install_path.join(INSTALL_MARKER),
            format!("Lege Document OCR {EXPECTED_LEGE_OCR_VERSION}\n"),
        )
        .context("writing install safety marker")?;

        if request.verify_backend {
            step(&tx, "Running OCR backend readiness check", 0.82);
            let output = Command::new(&exe_path)
                .args(["doctor", "--backend", "auto", "--json"])
                .current_dir(&request.install_path)
                .env_remove("LEGE_TENSORRT_OCR_ROOT")
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .context("running installed lege-ocr doctor")?;
            if !output.status.success() {
                return Err(anyhow!(
                    "installed OCR backend readiness check failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            log_msg(&tx, format!("Backend check: {}", stdout.trim()));
        }

        create_shortcuts(&request, &exe_path, &launcher_path, &icon_path, &tx)?;

        if request.register_uninstaller {
            step(&tx, "Registering the uninstaller", 0.98);
            write_uninstall_entry(&request.install_path, EXPECTED_LEGE_OCR_VERSION);
        }

        step(&tx, "Lege Document OCR is ready.", 1.0);
        Ok(())
    }

    fn extract_payload_archive(install_path: &Path) -> Result<()> {
        let archive_path = install_path.join(".lege-document-ocr-payload.7z");
        fs::write(&archive_path, PAYLOAD_7Z)
            .with_context(|| format!("writing temporary payload {}", archive_path.display()))?;
        let result = sevenz_rust::decompress_file(&archive_path, install_path)
            .map_err(|e| anyhow!("failed to extract payload archive: {e}"));
        let _ = fs::remove_file(&archive_path);
        result
    }

    fn required_payload_paths() -> &'static [&'static str] {
        &[
            "lege-ocr.exe",
            "package-manifest.json",
            "tensorrt/bin/turboocr-text.exe",
            "tensorrt/models/det_tiny.onnx",
            "tensorrt/models/rec_tiny.onnx",
            "tensorrt/models/keys_tiny.txt",
            "tensorrt/runtime/nvinfer_10.dll",
            "tensorrt/runtime/nvinfer_builder_resource_10.dll",
            "tensorrt/runtime/nvonnxparser_10.dll",
            "licenses/LICENSE",
        ]
    }

    fn create_shortcuts(
        request: &InstallRequest,
        exe_path: &Path,
        launcher_path: &Path,
        icon_path: &Path,
        tx: &flume::Sender<InstallerEvent>,
    ) -> Result<()> {
        step(tx, "Creating shortcuts", 0.97);
        let _com = ComGuard::init_sta();

        let (target, arguments) = if request.install_launcher {
            (launcher_path, "")
        } else {
            (exe_path, "--help")
        };

        if request.desktop_shortcut {
            if let Some(path) = desktop_shortcut_path() {
                create_windows_shortcut(target, arguments, &path, &request.install_path, icon_path)
                    .with_context(|| format!("creating {}", path.display()))?;
                log_msg(tx, format!("Desktop shortcut: {}", path.display()));
            }
        }

        if request.start_menu_shortcut {
            if let Some(path) = start_menu_shortcut_path() {
                if let Some(folder) = path.parent() {
                    fs::create_dir_all(folder)
                        .with_context(|| format!("creating {}", folder.display()))?;
                }
                create_windows_shortcut(target, arguments, &path, &request.install_path, icon_path)
                    .with_context(|| format!("creating {}", path.display()))?;
                log_msg(tx, format!("Start Menu shortcut: {}", path.display()));
            }
        }

        Ok(())
    }

    fn create_windows_shortcut(
        target: &Path,
        arguments: &str,
        shortcut_path: &Path,
        working_dir: &Path,
        icon_path: &Path,
    ) -> Result<()> {
        unsafe {
            let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| anyhow!("creating shell link: {e:?}"))?;

            let target_wide = to_wide(target);
            shell_link
                .SetPath(PCWSTR::from_raw(target_wide.as_ptr()))
                .map_err(|e| anyhow!("setting shortcut target: {e:?}"))?;

            let args_wide = to_wide_str(arguments);
            shell_link
                .SetArguments(PCWSTR::from_raw(args_wide.as_ptr()))
                .map_err(|e| anyhow!("setting shortcut arguments: {e:?}"))?;

            let working_wide = to_wide(working_dir);
            shell_link
                .SetWorkingDirectory(PCWSTR::from_raw(working_wide.as_ptr()))
                .map_err(|e| anyhow!("setting shortcut working dir: {e:?}"))?;

            let icon_wide = to_wide(icon_path);
            shell_link
                .SetIconLocation(PCWSTR::from_raw(icon_wide.as_ptr()), 0)
                .map_err(|e| anyhow!("setting shortcut icon: {e:?}"))?;

            let persist_file: IPersistFile = shell_link
                .cast()
                .map_err(|e| anyhow!("casting shortcut to IPersistFile: {e:?}"))?;

            let shortcut_wide = to_wide(shortcut_path);
            persist_file
                .Save(PCWSTR::from_raw(shortcut_wide.as_ptr()), true)
                .map_err(|e| anyhow!("saving shortcut: {e:?}"))?;
        }
        Ok(())
    }

    fn pick_folder_blocking() -> Option<String> {
        let _com = ComGuard::init_sta(); // reuse RAII guard instead of manual init/uninit
        unsafe {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let opts = dialog.GetOptions().ok()?;
            dialog
                .SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
                .ok()?;
            dialog.Show(None).ok()?;
            let item = dialog.GetResult().ok()?;
            let path_ptr = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let path = path_ptr.to_string().ok()?;
            CoTaskMemFree(Some(path_ptr.0 as *const c_void));
            Some(path)
        }
    }

    // ── Shared helpers ────────────────────────────────────────────────────────

    fn step(tx: &flume::Sender<InstallerEvent>, text: impl Into<String>, progress: f32) {
        let _ = tx.send(InstallerEvent::Step(text.into(), progress));
    }

    fn log_msg(tx: &flume::Sender<InstallerEvent>, text: impl Into<String>) {
        let _ = tx.send(InstallerEvent::Log(text.into()));
    }

    fn existing_install_summary(request: &InstallRequest) -> Option<String> {
        if !is_safe_install_root(&request.install_path) {
            return None;
        }
        let exe_path = request.install_path.join("lege-ocr.exe");
        let installed_version = installed_lege_ocr_version(&exe_path)?;
        if installed_version != EXPECTED_LEGE_OCR_VERSION {
            return None;
        }
        if request.verify_backend && !backend_ready(&exe_path, &request.install_path) {
            return None;
        }
        if request.desktop_shortcut && !desktop_shortcut_path().is_some_and(|p| p.is_file()) {
            return None;
        }
        if request.start_menu_shortcut && !start_menu_shortcut_path().is_some_and(|p| p.is_file()) {
            return None;
        }
        Some(format!(
            "Lege Document OCR {installed_version} is already installed at {} with the \
             TensorRT worker, models, native runtime, and requested shortcuts. Nothing to do.",
            request.install_path.display()
        ))
    }

    fn installed_lege_ocr_version(exe_path: &Path) -> Option<String> {
        if !exe_path.is_file() {
            return None;
        }
        let out = Command::new(exe_path)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .last()
            .map(str::to_string)
    }

    fn backend_ready(exe_path: &Path, install_path: &Path) -> bool {
        Command::new(exe_path)
            .args(["doctor", "--backend", "auto", "--json"])
            .current_dir(install_path)
            .env_remove("LEGE_TENSORRT_OCR_ROOT")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn validate_install_path(value: &str) -> Result<PathBuf, String> {
        if value.trim().is_empty() {
            return Err("install path is empty".to_string());
        }
        let path = PathBuf::from(value);
        let parent = path
            .parent()
            .ok_or_else(|| "install path has no parent directory".to_string())?;
        if parent.exists() {
            let probe = parent.join(format!(".lege-ocr-write-test-{}", std::process::id()));
            fs::create_dir(&probe).map_err(|e| format!("parent directory is not writable: {e}"))?;
            fs::remove_dir(&probe)
                .map_err(|e| format!("failed to clean write test directory: {e}"))?;
        } else {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create parent directory: {e}"))?;
        }
        Ok(path)
    }
}

#[cfg(target_os = "windows")]
fn main() {
    std::process::exit(app::run());
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Lege Document OCR installer is Windows-only.");
}
