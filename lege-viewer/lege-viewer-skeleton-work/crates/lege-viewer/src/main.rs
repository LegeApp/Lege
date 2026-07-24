use std::sync::Arc;

use lege_viewer::ViewerApp;
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::<ViewerEvent>::with_user_event().build()?;
    let wake = Arc::new(WinitWake {
        proxy: event_loop.create_proxy(),
    });
    let updates = UpdateQueue::new(4096, wake);

    let engine: Arc<dyn DocumentEngine> = open_engine()?;
    let mut app = ViewerApp::new(engine, updates);
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn open_engine() -> Result<Arc<dyn DocumentEngine>, Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1);
    #[cfg(feature = "pdf-engine")]
    if let Some(path) = path {
        return Ok(Arc::new(
            lege_viewer::document::pdf_engine::PdfEngine::open(path, None)
                .map_err(|error| std::io::Error::other(format!("failed to open PDF: {error}")))?,
        ));
    }
    #[cfg(not(feature = "pdf-engine"))]
    let _ = path;
    Ok(Arc::new(SyntheticEngine::new(10_000)))
}
