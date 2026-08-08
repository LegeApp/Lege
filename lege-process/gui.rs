#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(not(any(feature = "viewer", feature = "freya")))]
fn main() {
    eprintln!("Enable a GUI backend with `--features viewer` or `--features freya`.");
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// Freya backend
//
// The Freya sources under `GUI/Freya/src` were authored as their own binary
// crate, so their modules reach each other through `crate::`. Declaring those
// modules directly at this bin crate's root (instead of nesting them under a
// single `mod`) keeps every `crate::` path valid. The launcher that used to
// live in `GUI/Freya/src/main.rs` is reproduced below so that file can stay the
// entry point of the stand-alone `lege-gui-freya` crate.
// ---------------------------------------------------------------------------
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/app.rs"]
mod app;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/appearance.rs"]
mod appearance;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/backend.rs"]
mod backend;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/colors.rs"]
mod colors;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/gui_text.rs"]
mod gui_text;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/logging.rs"]
mod logging;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/models.rs"]
mod models;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/sanzowada.rs"]
mod sanzowada;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/settings.rs"]
mod settings;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/version.rs"]
mod version;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/widgets.rs"]
mod widgets;
#[cfg(feature = "freya")]
#[path = "GUI/Freya/src/worker_process.rs"]
mod worker_process;

#[cfg(feature = "freya")]
fn main() {
    use freya::prelude::*;

    /// Blocking-pool cap for the GUI shell. It mirrors
    /// `lege::runtime_stats::MAX_BLOCKING_THREADS`; the GUI talks to the worker
    /// over IPC and does not link the processing crate.
    const MAX_BLOCKING_THREADS: usize = 4;
    const ICON: &[u8] = include_bytes!("../lege-misc/assets/icon.png");

    #[cfg(target_os = "windows")]
    unsafe {
        use winapi::um::wincon::GetConsoleWindow;
        use winapi::um::winuser::{SW_HIDE, ShowWindow};

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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .enable_all()
        .build()
        .expect("failed to create tokio runtime for Freya GUI");
    let _rt = runtime.enter();

    let (window_width, window_height) = (990.0, 726.0);
    let (window_min_width, window_min_height) = (990.0, 726.0);

    launch(
        LaunchConfig::new().with_window(
            WindowConfig::new(crate::app::app)
                .with_title("Lege")
                .with_size(window_width, window_height)
                .with_min_size(window_min_width, window_min_height)
                .with_max_size(1980.0, 1452.0)
                .with_aspect_ratio_range(1.30, 1.45)
                .with_decorations(true)
                .with_resizable(true)
                .with_icon(LaunchConfig::window_icon(ICON))
                .with_window_attributes(move |attributes, el| {
                    use freya::winit::dpi::PhysicalPosition;
                    use freya::winit::window::WindowButtons;

                    let attributes = attributes
                        .with_enabled_buttons(WindowButtons::MINIMIZE | WindowButtons::CLOSE);

                    if let Some(monitor) = el
                        .primary_monitor()
                        .or_else(|| el.available_monitors().next())
                    {
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
}

#[cfg(feature = "viewer")]
#[path = "../lege-viewer/src/main.rs"]
mod selected_gui;

#[cfg(feature = "viewer")]
fn main() {
    if let Err(error) = selected_gui::main() {
        eprintln!("lege-gui: {error}");
        std::process::exit(1);
    }
}
