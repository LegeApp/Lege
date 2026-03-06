#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Local modules (bin-only crate)
mod app;
mod backend;
mod gui_text;
mod logging;
mod models;
mod settings;
mod static_styles;
mod theme;
mod version;

// Re-export for inter-module access
pub use gui_text::GUI_TEXT;

use crate::app::App;
use crate::static_styles::BASE_STYLES;
use crate::theme::ThemeManager;
use dioxus::prelude::*;
#[cfg(target_os = "windows")]
use dioxus_desktop::tao::dpi::PhysicalPosition;
use dioxus_desktop::tao::window::Icon;
use dioxus_desktop::{Config, LogicalSize, WindowBuilder};

#[cfg(target_os = "windows")]
use winapi::um::wincon::GetConsoleWindow;
#[cfg(target_os = "windows")]
use winapi::um::winuser::{SW_HIDE, ShowWindow};

#[cfg(feature = "debug-logging")]
use lege::debug_log::write_debug_line;

fn main() {
    // Configure dynamic runtime library paths early (before ONNX sessions initialize).
    lege::configure_runtime_env();

    // Hide console window on Windows to prevent flicker
    #[cfg(target_os = "windows")]
    unsafe {
        let console_window = GetConsoleWindow();
        if !console_window.is_null() {
            ShowWindow(console_window, SW_HIDE);
        }
    }

    // For Linux, redirect stdout/stderr to null when not in debug mode and launched from GUI
    #[cfg(all(target_os = "linux", not(debug_assertions)))]
    {
        use std::fs::OpenOptions;
        use std::os::unix::io::AsRawFd;

        if std::env::var("LAUNCHED_FROM_GUI").is_ok()
            || (std::env::var("TERM").is_err() && std::env::var("DISPLAY").is_ok())
        {
            if let Ok(null) = OpenOptions::new().write(true).open("/dev/null") {
                let null_fd = null.as_raw_fd();
                unsafe {
                    libc::dup2(null_fd, 1); // stdout
                    libc::dup2(null_fd, 2); // stderr
                }
            }
        }
    }

    // Ensure user data directories exist before starting WebView2 (Windows only)
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = lege::windows_dirs::ensure_directories() {
            eprintln!("Failed to create user directories: {}", e);
        }

        if let Ok(webview_dir) = lege::windows_dirs::get_webview_data_dir() {
            // Windows build treats this std call as unsafe, so fence it locally.
            unsafe {
                std::env::set_var(
                    "WEBVIEW2_USER_DATA_FOLDER",
                    webview_dir.to_string_lossy().to_string(),
                );
            }
        }
    }

    // Window size: use the same scaled dimensions across platforms
    // to ensure all content fits without scrolling
    let (window_width, window_height) = {
        let base_w = 900.0;
        let base_h = 600.0;
        // Slightly smaller initial size to reduce whitespace; panels grow via CSS
        (base_w * 1.1, base_h * 1.1)
    };

    // Build the window.
    // IMPORTANT: keep it BORDERLESS everywhere (as you preferred) and explicitly kill menus.
    let mut window = WindowBuilder::new()
        .with_title("Lege")
        .with_inner_size(LogicalSize::new(window_width, window_height))
        .with_min_inner_size(LogicalSize::new(window_width * 0.85, window_height * 0.85))
        .with_resizable(true)
        .with_decorations(false); // stay borderless on Linux (and other OSes)

    // Center the window on Windows at creation time to avoid visible snapping.
    #[cfg(target_os = "windows")]
    {
        use winapi::um::winuser::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) } as i32;
        let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) } as i32;
        let win_w = window_width as i32;
        let win_h = window_height as i32;
        let center_x = ((screen_w - win_w) / 2).max(0);
        let center_y = ((screen_h - win_h) / 2).max(0);
        window = window.with_position(PhysicalPosition::new(center_x, center_y));
    }

    // Set custom icon for the application - using embedded icon data
    fn try_load_icon() -> Option<Icon> {
        // Prefer Windows .ico on Windows, PNG elsewhere
        #[cfg(target_os = "windows")]
        const ICO_DATA: &[u8] = include_bytes!("../../../assets/icon.ico");
        #[cfg(target_os = "windows")]
        {
            if let Ok(img) = image::load_from_memory(ICO_DATA) {
                let img = img.into_rgba8();
                let (width, height) = img.dimensions();
                if let Ok(icon) = Icon::from_rgba(img.into_raw(), width, height) {
                    return Some(icon);
                }
            }
        }

        // Fallback PNG (works across platforms)
        const PNG_DATA: &[u8] = include_bytes!("../../../assets/icon.png");
        if let Ok(img) = image::load_from_memory(PNG_DATA) {
            let img = img.into_rgba8();
            let (width, height) = img.dimensions();
            if let Ok(icon) = Icon::from_rgba(img.into_raw(), width, height) {
                return Some(icon);
            }
        }
        None
    }

    if let Some(icon) = try_load_icon() {
        window = window.with_window_icon(Some(icon));
    }

    // Initialize theme manager (lazy initialization)
    let theme_manager = ThemeManager::new();

    // Inject base + theme CSS
    let custom_head = format!(
        r#"
        <meta charset="utf-8">
        <style>
          {base}{theme}
        </style>
        "#,
        base = BASE_STYLES,
        theme = theme_manager.get_css()
    );

    // Build app head with CSS
    let config = Config::new()
        .with_window(window)
        .with_custom_head(custom_head);

    // Launch the application.
    LaunchBuilder::desktop().with_cfg(config).launch(App);

    #[cfg(feature = "debug-logging")]
    {
        write_debug_line("Lege GUI launched (borderless, menu-none).");
    }
}
