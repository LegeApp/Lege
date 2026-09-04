//! Sheet -> PNG, for a GUI print preview. Phase 2 aimed at a file rather
//! than a spooler.

use crate::{ComposeOptions, PrintError, Sheet};

/// Preview settings. Deliberately a thin wrapper: a preview is a low-DPI
/// composition and nothing else.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewOptions {
    pub dpi: f64,
    pub grayscale: bool,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        Self {
            dpi: 96.0,
            grayscale: false,
        }
    }
}

impl PreviewOptions {
    #[must_use]
    pub fn to_compose(self) -> ComposeOptions {
        ComposeOptions {
            dpi: self.dpi,
            grayscale: self.grayscale,
            ..ComposeOptions::default()
        }
    }
}

/// Compose `sheet` at preview resolution and encode it as PNG.
pub fn render_preview_png(
    session: &lege_pdf_read::RenderSession,
    sheet: &Sheet,
    options: PreviewOptions,
) -> Result<Vec<u8>, PrintError> {
    let _ = (session, sheet, options);
    unimplemented!("phase 2")
}
