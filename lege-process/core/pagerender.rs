use anyhow::{Result, anyhow};
use std::sync::Arc;

pub use lege_pdf_read::{NativeTextWord, OwnedBookmarkNode};

/// Configuration retained at the Lege process boundary so callers can select
/// an exact output width without depending on renderer-internal request types.
#[derive(Clone, Debug)]
pub struct RasterConfig {
    pub render_forms: bool,
    pub target_width: Option<u32>,
}

impl Default for RasterConfig {
    fn default() -> Self {
        Self {
            render_forms: true,
            target_width: None,
        }
    }
}

/// Rendered page result consumed by the processing pipelines.
#[derive(Debug, Clone)]
pub struct RgbPage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub original_width_pts: f32,
    pub original_height_pts: f32,
}

/// Lege's single PDF renderer. One document session is shared by raster,
/// geometry, outline, and native-text consumers for the lifetime of a job.
pub struct PdfRenderer {
    document: Arc<lege_pdf_read::RenderSession>,
    raster_config: RasterConfig,
}

impl PdfRenderer {
    pub fn new_from_bytes(pdf_bytes: Arc<[u8]>, raster_config: RasterConfig) -> Result<Self> {
        let document = Arc::new(
            lege_pdf_read::RenderSession::open(pdf_bytes, None)
                .map_err(|error| anyhow!("Failed to read PDF document: {error}"))?,
        );
        Ok(Self {
            document,
            raster_config,
        })
    }

    pub fn page_count(&self) -> u32 {
        self.document.page_count()
    }

    pub fn document_session(&self) -> Arc<lege_pdf_read::RenderSession> {
        Arc::clone(&self.document)
    }

    pub async fn render_page_rgb(
        &self,
        page_index: u32,
        target_height: u32,
        target_width: Option<u32>,
    ) -> Result<RgbPage> {
        self.render_page_rgb_cancellable(page_index, target_height, target_width, None)
            .await
    }

    pub async fn render_page_rgb_cancellable(
        &self,
        page_index: u32,
        target_height: u32,
        target_width: Option<u32>,
        cancellation: Option<lege_pdf_read::CancellationToken>,
    ) -> Result<RgbPage> {
        let document = Arc::clone(&self.document);
        let target_width = target_width.or(self.raster_config.target_width);
        crate::runtime_stats::spawn_blocking(move || {
            render_session_page_rgb_cancellable(
                &document,
                page_index,
                target_height,
                target_width,
                cancellation.as_ref(),
            )
        })
        .await?
    }

    pub fn render_page_rgb_sync(
        &self,
        page_index: u32,
        target_height: u32,
        target_width: Option<u32>,
    ) -> Result<RgbPage> {
        render_session_page_rgb(
            &self.document,
            page_index,
            target_height,
            target_width.or(self.raster_config.target_width),
        )
    }
}

/// Count pages through the same document reader used by the render pipeline.
pub fn count_pdf_pages_from_bytes(pdf_bytes: &[u8]) -> Result<u16> {
    let session = lege_pdf_read::RenderSession::open(Arc::<[u8]>::from(pdf_bytes), None)
        .map_err(|error| anyhow!("Failed to read PDF document: {error}"))?;
    u16::try_from(session.page_count()).map_err(|_| anyhow!("PDF has more than 65,535 pages"))
}

pub fn render_session_page_rgb(
    document: &lege_pdf_read::RenderSession,
    page_index: u32,
    target_height: u32,
    target_width: Option<u32>,
) -> Result<RgbPage> {
    render_session_page_rgb_cancellable(document, page_index, target_height, target_width, None)
}

pub fn render_session_page_rgb_cancellable(
    document: &lege_pdf_read::RenderSession,
    page_index: u32,
    target_height: u32,
    target_width: Option<u32>,
    cancellation: Option<&lege_pdf_read::CancellationToken>,
) -> Result<RgbPage> {
    if cancellation.is_some_and(lege_pdf_read::CancellationToken::is_cancelled) {
        return Err(anyhow!("PDF page render cancelled"));
    }
    let geometry = document
        .page_geometry(page_index)
        .map_err(|error| anyhow!("Failed to read page {page_index} geometry: {error}"))?;
    let (width, height) = render_dimensions(
        geometry.display_width(),
        geometry.display_height(),
        target_height,
        target_width,
    )?;
    let page = document
        .compile(page_index)
        .map_err(|error| anyhow!("Failed to compile page {page_index}: {error}"))?;
    let plane = document
        .render_cancellable(
            &page,
            &lege_pdf_read::RasterProduct::rgb8(width, height),
            cancellation,
        )
        .map_err(|error| anyhow!("Failed to render page {page_index}: {error}"))?;
    let lege_pdf_read::RasterPlane::Rgb8(surface) = plane else {
        unreachable!("an RGB raster product must return an RGB plane");
    };

    Ok(RgbPage {
        width: surface.width,
        height: surface.height,
        data: surface.pixels.to_vec(),
        original_width_pts: geometry.display_width() as f32,
        original_height_pts: geometry.display_height() as f32,
    })
}

fn render_dimensions(
    page_width: f64,
    page_height: f64,
    target_height: u32,
    target_width: Option<u32>,
) -> Result<(u32, u32)> {
    if target_height == 0 {
        return Err(anyhow!("render target height must be non-zero"));
    }
    if let Some(width) = target_width {
        if width == 0 {
            return Err(anyhow!("render target width must be non-zero"));
        }
        return Ok((width, target_height));
    }
    if !page_width.is_finite()
        || !page_height.is_finite()
        || page_width <= 0.0
        || page_height <= 0.0
    {
        return Err(anyhow!(
            "invalid displayed page dimensions {page_width}x{page_height}"
        ));
    }
    let width = (page_width * f64::from(target_height) / page_height)
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32;
    Ok((width, target_height))
}

pub mod prelude {
    pub use super::{
        PdfRenderer, RasterConfig, RgbPage, render_session_page_rgb,
        render_session_page_rgb_cancellable,
    };
}

#[cfg(test)]
mod render_engine_tests {
    use super::render_dimensions;

    #[test]
    fn renderer_dimensions_preserve_aspect_or_honor_explicit_size() {
        assert_eq!(
            render_dimensions(612.0, 792.0, 1200, None).unwrap(),
            (927, 1200)
        );
        assert_eq!(
            render_dimensions(612.0, 792.0, 1200, Some(800)).unwrap(),
            (800, 1200)
        );
        assert_eq!(
            render_dimensions(612.0, 792.0, 4800, None).unwrap(),
            (3709, 4800)
        );
        assert!(render_dimensions(0.0, 792.0, 1200, None).is_err());
        assert!(render_dimensions(612.0, 792.0, 0, None).is_err());
    }
}
