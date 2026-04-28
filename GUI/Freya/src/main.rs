#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod backend;
mod gui_text;
mod logging;
mod models;
mod settings;
mod version;
mod widgets;

use freya::prelude::*;

#[cfg(target_os = "windows")]
use winapi::um::wincon::GetConsoleWindow;
#[cfg(target_os = "windows")]
use winapi::um::winuser::{SW_HIDE, ShowWindow};

#[cfg(feature = "debug-logging")]
use lege::debug_log::write_debug_line;

const ICON: &[u8] = include_bytes!("../../../assets/icon.png");

fn main() {
    lege::configure_runtime_env();

    #[cfg(target_os = "windows")]
    unsafe {
        let console_window = GetConsoleWindow();
        if !console_window.is_null() {
            ShowWindow(console_window, SW_HIDE);
        }
    }

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
                    libc::dup2(null_fd, 1);
                    libc::dup2(null_fd, 2);
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Err(e) = lege::windows_dirs::ensure_directories() {
            eprintln!("Failed to create user directories: {e}");
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime for Freya GUI");
    let _rt = runtime.enter();

    let (window_width, window_height) = (990.0, 726.0);

    launch(
        LaunchConfig::new().with_window(
            WindowConfig::new(app::app)
                .with_title("Lege")
                .with_size(window_width, window_height)
                .with_min_size(841.5, 617.0)
                .with_decorations(true)
                .with_resizable(true)
                .with_icon(LaunchConfig::window_icon(ICON))
                .with_window_attributes(move |attributes, el| {
                    use freya::winit::dpi::PhysicalPosition;

                    if let Some(monitor) = el.primary_monitor().or_else(|| el.available_monitors().next()) {
                        let origin = monitor.position();
                        let size = monitor.size();
                        let scale = monitor.scale_factor();
                        let physical_window_width = (window_width * scale).round() as i32;
                        let physical_window_height = (window_height * scale).round() as i32;
                        attributes.with_position(PhysicalPosition {
                            x: origin.x + (size.width as i32 / 2) - (physical_window_width / 2),
                            y: origin.y + (size.height as i32 / 2) - (physical_window_height / 2),
                        })
                    } else {
                        attributes
                    }
                }),
        ),
    );

    #[cfg(feature = "debug-logging")]
    {
        write_debug_line("Lege Freya GUI launched.");
    }
}
