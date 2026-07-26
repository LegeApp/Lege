use crate::geometry::RectI;
use crate::scene::FrameScene;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Xrgb8888,
}

#[derive(Debug, thiserror::Error)]
pub enum PresentError {
    #[error("presenter backend failed: {0}")]
    Backend(String),
    #[error("invalid surface size")]
    InvalidSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenterBackend {
    Software,
    Gpu,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PresenterPreference {
    #[default]
    Auto,
    Gpu,
    Software,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresenterStats {
    pub adapter: Option<String>,
    pub backend: Option<String>,
    pub frames_presented: u64,
    pub frames_skipped: u64,
    pub atlas_uploads: u64,
    pub atlas_upload_bytes: u64,
    pub atlas_resident_images: usize,
    pub atlas_bytes: u64,
    pub draw_calls: u32,
    pub vertices: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollReuse {
    pub canvas: RectI,
    pub delta_x: i32,
    pub delta_y: i32,
}

pub trait Presenter {
    fn resize(&mut self, width: u32, height: u32) -> Result<(), PresentError>;
    fn present(
        &mut self,
        scene: &FrameScene,
        damage: &[RectI],
        scroll_reuse: Option<ScrollReuse>,
    ) -> Result<(), PresentError>;
    fn format(&self) -> PixelFormat;
    fn backend(&self) -> PresenterBackend;
    fn stats(&self) -> PresenterStats {
        PresenterStats::default()
    }
}

#[cfg(feature = "softbuffer-presenter")]
pub mod softbuffer;
#[cfg(feature = "wgpu-presenter")]
pub mod wgpu;
