// A double-clicked viewer must not flash a console window. The Windows GUI
// subsystem is what suppresses it; stderr stays attached when the binary is
// launched from an existing terminal, so command-line use is unaffected.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::sync::Arc;

use lege_viewer::ViewerApp;
use lege_viewer::cli::{LaunchMode, USAGE, parse_launch_config};
use lege_viewer::document::empty::EmptyEngine;
use lege_viewer::document::engine::DocumentEngine;
use lege_viewer::document::session::{UpdateQueue, WakeSink};
use lege_viewer::document::synthetic::SyntheticEngine;
use lege_viewer::event::ViewerEvent;
use winit::event_loop::{EventLoop, EventLoopProxy};

struct WinitWake {
    proxy: EventLoopProxy<ViewerEvent>,
}

impl std::fmt::Debug for WinitWake {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("WinitWake").finish_non_exhaustive()
    }
}

impl WakeSink for WinitWake {
    fn wake(&self) {
        let _ = self.proxy.send_event(ViewerEvent::Wake);
    }
}

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = match parse_launch_config(std::env::args_os().skip(1)) {
        Ok(config) if config.mode == LaunchMode::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        Ok(config) => config,
        Err(error) => {
            report_startup_error(&format!("{error}\n\n{USAGE}"));
            std::process::exit(2);
        }
    };
    let event_loop = EventLoop::<ViewerEvent>::with_user_event().build()?;
    let wake = Arc::new(WinitWake {
        proxy: event_loop.create_proxy(),
    });
    let updates = UpdateQueue::new(4096, wake);

    let engine: Arc<dyn DocumentEngine> = match open_engine(config.mode) {
        Ok(engine) => engine,
        Err(error) => {
            report_startup_error(&error.to_string());
            std::process::exit(1);
        }
    };
    let mut app = ViewerApp::new_with_presenter(engine, updates, config.presenter)?;
    app.set_event_proxy(event_loop.create_proxy());
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Startup failures still have to reach someone who launched the viewer from
/// the desktop rather than a shell.
fn report_startup_error(message: &str) {
    eprintln!("lege-viewer: {message}");
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Lege")
        .set_description(message)
        .show();
}

fn open_engine(mode: LaunchMode) -> Result<Arc<dyn DocumentEngine>, Box<dyn std::error::Error>> {
    match mode {
        LaunchMode::Empty => Ok(Arc::new(EmptyEngine::new())),
        LaunchMode::Synthetic { page_count } => Ok(Arc::new(SyntheticEngine::new(page_count))),
        LaunchMode::Pdf(path) => {
            #[cfg(feature = "pdf-engine")]
            {
                Ok(Arc::new(
                    lege_viewer::document::pdf_engine::PdfEngine::open(path, None).map_err(
                        |error| std::io::Error::other(format!("failed to open PDF: {error}")),
                    )?,
                ))
            }
            #[cfg(not(feature = "pdf-engine"))]
            {
                let _ = path;
                Err(std::io::Error::other(
                    "this build does not include the PDF engine; use --synthetic",
                )
                .into())
            }
        }
        LaunchMode::Help => {
            Err(std::io::Error::other("help mode does not create a document engine").into())
        }
    }
}
