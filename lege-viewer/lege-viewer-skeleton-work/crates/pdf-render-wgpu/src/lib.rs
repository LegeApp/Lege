//! WGPU render backend — Phase 7+ (roadmap §9).
//!
//! Currently a truthful stub: it implements the [`RenderBackend`] contract
//! by declining every page, which lets scheduler routing / fallback logic
//! be written and tested now. When Phase 7 begins, `wgpu` becomes a
//! dependency *of this crate only*, behind the workspace's `wgpu` feature
//! wiring; nothing else in the engine changes — that is the whole point of
//! the `CompiledPage` contract.

use pdf_page_ir::{CompiledPage, DeviceSize, PageFeatures};
use pdf_render_api::{
    BackendCapabilities, BackendId, RenderBackend, RenderRequest, RenderTicket, SubmitError,
    SupportLevel, UnsupportedFeature,
};

/// Placeholder GPU backend. Declines all work.
#[derive(Debug, Default)]
pub struct WgpuBackend {
    _private: (),
}

impl WgpuBackend {
    /// Will become `pub fn new(options) -> Result<Self, GpuUnavailable>`
    /// performing adapter/device negotiation in Phase 7.
    pub fn placeholder() -> Self {
        Self::default()
    }
}

impl RenderBackend for WgpuBackend {
    fn id(&self) -> BackendId {
        BackendId::Wgpu
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            formats: vec![],
            max_surface: DeviceSize { width: 0, height: 0 },
            features: PageFeatures::empty(),
            resident_surfaces: false,
        }
    }

    fn supports(&self, page: &CompiledPage, _request: &RenderRequest) -> SupportLevel {
        SupportLevel::Unsupported(UnsupportedFeature {
            missing: page.features,
            detail: "WGPU backend not implemented (Phase 7)",
        })
    }

    fn submit(&self, _request: RenderRequest) -> Result<RenderTicket, SubmitError> {
        Err(SubmitError::ShuttingDown)
    }
}
