//! The backend-neutral rendering API (roadmap §6).
//!
//! Rules:
//! - No `wgpu` (or any backend) types anywhere in this crate — ever.
//! - Backends are *job-based* (`submit` → [`RenderTicket`]); synchronous
//!   callers use [`render_blocking`].
//! - `HostPage` is the stable result type. GPU-resident surfaces arrive
//!   later as an opaque `ResidentPage` without changing this API's shape.

use std::sync::Arc;
use std::sync::mpsc;

use pdf_page_ir::{CompiledPage, DeviceRect, DeviceSize, Matrix, PageFeatures};

pub mod contract;

/// Identifies a backend implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendId {
    Cpu,
    Wgpu,
    /// Test/diagnostic backends.
    Other(u32),
}

/// Frozen output surface contract (roadmap §7 Phase 6): channel order,
/// premultiplication, and transfer function are part of the *format*, so
/// CPU and GPU cannot silently diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    /// 8-bit RGBA, premultiplied alpha, sRGB transfer.
    Rgba8PremultipliedSrgb,
    /// 8-bit single-channel gray, sRGB transfer.
    Gray8,
}

impl OutputFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            OutputFormat::Rgba8PremultipliedSrgb => 4,
            OutputFormat::Gray8 => 1,
        }
    }
}

/// Page background handling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Background {
    /// Composite over opaque white (the common rasterization default).
    White,
    /// Leave unpainted areas transparent.
    Transparent,
    /// Composite over a solid color.
    Solid(pdf_page_ir::Color),
}

/// Static-annotation handling (interactive features are out of scope by
/// project boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationMode {
    None,
    /// Render existing appearance streams as page content.
    StaticAppearances,
}

/// Annotations render by default (PDFium `FPDF_ANNOT` display parity):
/// static appearance streams are part of what a viewer shows, so opting
/// *out* is the explicit choice, not opting in.
impl Default for AnnotationMode {
    fn default() -> Self {
        AnnotationMode::StaticAppearances
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderQuality {
    /// Fast preview: reduced AA, nearest-neighbor images allowed.
    Draft,
    /// Full quality per the frozen surface contract.
    Normal,
}

/// Render-time resource limits (blueprint §4.4) — enforced per job.
#[derive(Debug, Clone)]
pub struct RenderLimits {
    /// Peak intermediate bytes one page job may hold.
    pub max_page_bytes: u64,
    /// Transparency-group nesting depth.
    pub max_group_depth: u32,
    /// Wall-clock hint; backends check between operations.
    pub cancellation: Option<CancellationToken>,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_page_bytes: 2 << 30,
            max_group_depth: 64,
            cancellation: None,
        }
    }
}

/// Cooperative cancellation. Cheap to clone, checked at operation
/// boundaries.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a renderer token over a caller-owned shared flag. This is the
    /// zero-adapter cancellation seam used by tightly integrated schedulers:
    /// the viewer conductor and renderer observe the same atomic state.
    pub fn from_shared(cancelled: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { cancelled }
    }

    /// Share the underlying flag with an outer scheduler or composite job.
    pub fn shared_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.cancelled.clone()
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Where the caller wants the result to live (roadmap §6.1). During the
/// CPU-only phases both modes return host data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputResidency {
    /// The caller requires host memory (`HostPage`).
    HostRequired,
    /// The backend may keep the surface resident (GPU) and return an opaque
    /// handle; falls back to host data where residency is unsupported.
    BackendPreferred,
}

/// Mapping from IR page space to the output surface.
#[derive(Debug, Clone, Copy)]
pub struct PageTransform {
    /// User-space → device-space transform (already includes scale, flip,
    /// and rotation).
    pub matrix: Matrix,
}

/// Renderer-owned recoloring policy. Vector paint, text, shadings, stencil
/// images, and the page background are transformed before rasterization;
/// ordinary photographic images retain their source colors.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenderColorPolicy {
    #[default]
    Original,
    Night {
        paper_rgb: [u8; 3],
        text_rgb: [u8; 3],
    },
    WarmPaper {
        paper_rgb: [u8; 3],
    },
}

impl RenderColorPolicy {
    pub fn paper_rgb(self) -> [u8; 3] {
        match self {
            Self::Original => [255, 255, 255],
            Self::Night { paper_rgb, .. } | Self::WarmPaper { paper_rgb } => paper_rgb,
        }
    }
}

/// A complete, backend-neutral render request (roadmap §6.1).
#[derive(Debug, Clone)]
pub struct RenderRequest {
    pub page: Arc<CompiledPage>,
    pub transform: PageTransform,
    /// Optional device-space crop of the output.
    pub crop: Option<DeviceRect>,
    pub output_size: DeviceSize,
    pub output_format: OutputFormat,
    pub background: Background,
    pub color_policy: RenderColorPolicy,
    pub annotations: AnnotationMode,
    pub quality: RenderQuality,
    pub limits: RenderLimits,
    pub residency: OutputResidency,
}

/// A rendered page in host memory — the stable result type.
#[derive(Debug, Clone)]
pub struct HostPage {
    pub width: u32,
    pub height: u32,
    /// Row stride in bytes (>= width * bytes_per_pixel).
    pub stride: usize,
    pub format: OutputFormat,
    pub pixels: Arc<[u8]>,
}

/// Opaque backend-resident surface (Stage B, roadmap §10). The owning
/// backend interprets `handle`; nothing else may.
#[derive(Debug, Clone)]
pub struct ResidentPage {
    pub backend: BackendId,
    pub handle: u64,
    pub size: DeviceSize,
    pub format: OutputFormat,
}

#[derive(Debug, Clone)]
pub enum RenderedPage {
    Host(HostPage),
    Resident(ResidentPage),
}

impl RenderedPage {
    pub fn as_host(&self) -> Option<&HostPage> {
        match self {
            RenderedPage::Host(h) => Some(h),
            RenderedPage::Resident(_) => None,
        }
    }
}

/// Why a backend declines a page (preflight, roadmap §2.4).
#[derive(Debug, Clone)]
pub struct UnsupportedFeature {
    /// Features the page needs that the backend lacks.
    pub missing: PageFeatures,
    pub detail: &'static str,
}

#[derive(Debug, Clone)]
pub enum SupportLevel {
    /// Backend implements every feature the page requires.
    Native,
    /// Backend cannot render this page; route elsewhere.
    Unsupported(UnsupportedFeature),
}

/// Post-processing operations that can be executed after rasterization.
///
/// This vocabulary deliberately lives in `pdf-render-api` rather than
/// `pdf-postprocess`: the latter consumes the frozen render API, so putting
/// capability negotiation there would create a dependency cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PostprocessOperations(u16);

impl PostprocessOperations {
    pub const CROP: Self = Self(1 << 0);
    pub const RESIZE: Self = Self(1 << 1);
    pub const CONVERT_TO_GRAY: Self = Self(1 << 2);
    pub const APPLY_TONE_CURVE: Self = Self(1 << 3);
    pub const OTSU: Self = Self(1 << 4);
    pub const SAUVOLA: Self = Self(1 << 5);
    pub const FUSE_THRESHOLDS: Self = Self(1 << 6);
    pub const DITHER: Self = Self(1 << 7);
    pub const PACK_MONOCHROME: Self = Self(1 << 8);

    const ALL_BITS: u16 = (1 << 9) - 1;

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn all() -> Self {
        Self(Self::ALL_BITS)
    }

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

impl std::ops::BitOr for PostprocessOperations {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

/// Post-processing support associated with one render backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PostprocessCapabilities {
    pub operations: PostprocessOperations,
    /// Operations can consume and produce this backend's resident surfaces
    /// without an intervening host readback.
    pub resident_execution: bool,
}

impl PostprocessCapabilities {
    pub const NONE: Self = Self {
        operations: PostprocessOperations::empty(),
        resident_execution: false,
    };

    /// The complete host-memory CPU executor shipped with the renderer.
    pub const HOST_ALL: Self = Self {
        operations: PostprocessOperations::all(),
        resident_execution: false,
    };

    pub fn supports(self, required: PostprocessOperations) -> bool {
        self.operations.contains(required)
    }
}

impl Default for PostprocessCapabilities {
    fn default() -> Self {
        Self::NONE
    }
}

/// What a backend can do — consulted by the scheduler's routing policy.
#[derive(Debug, Clone)]
pub struct BackendCapabilities {
    pub formats: Vec<OutputFormat>,
    pub max_surface: DeviceSize,
    pub features: PageFeatures,
    pub resident_surfaces: bool,
    pub postprocess: PostprocessCapabilities,
}

/// Render failure taxonomy (roadmap §13). GPU variants exist now so
/// fallback policy code can be written and tested before the GPU does.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RenderError {
    #[error("page requires unsupported features: {0:?}")]
    Unsupported(PageFeatures),
    #[error("render limit exceeded: {0}")]
    LimitExceeded(&'static str),
    #[error("render cancelled")]
    Cancelled,
    #[error("backend failure: {0}")]
    Backend(String),
    #[error("GPU unavailable: {0}")]
    GpuUnavailable(String),
    #[error("GPU device lost")]
    GpuDeviceLost,
    #[error("GPU out of memory")]
    GpuOutOfMemory,
    #[error("readback failure: {0}")]
    Readback(String),
    #[error("result channel disconnected (backend dropped the job)")]
    Disconnected,
    /// The backend panicked while compiling/rendering this page. The panic
    /// was caught at the API boundary (see [`render_blocking`] /
    /// [`submit_caught`]): it fails this page only, never the process or the
    /// worker pool.
    #[error("backend panicked: {message}")]
    Panic { message: String },
}

/// Handle to an in-flight render job.
#[derive(Debug)]
pub struct RenderTicket {
    pub job_id: u64,
    receiver: mpsc::Receiver<Result<RenderedPage, RenderError>>,
}

impl RenderTicket {
    pub fn new(job_id: u64) -> (Self, mpsc::Sender<Result<RenderedPage, RenderError>>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                job_id,
                receiver: rx,
            },
            tx,
        )
    }

    /// Block until the job completes.
    pub fn wait(self) -> Result<RenderedPage, RenderError> {
        self.receiver
            .recv()
            .unwrap_or(Err(RenderError::Disconnected))
    }

    /// Non-blocking poll; `None` while still in flight.
    pub fn try_wait(&self) -> Option<Result<RenderedPage, RenderError>> {
        match self.receiver.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(RenderError::Disconnected)),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SubmitError {
    #[error("backend queue is full")]
    QueueFull,
    #[error("backend is shutting down")]
    ShuttingDown,
    #[error("request invalid: {0}")]
    InvalidRequest(&'static str),
}

/// The backend contract (roadmap §6.2). Object-safe; `Arc<dyn
/// RenderBackend>` is the ubiquitous handle.
pub trait RenderBackend: Send + Sync + std::fmt::Debug {
    fn id(&self) -> BackendId;
    fn capabilities(&self) -> BackendCapabilities;
    fn supports(&self, page: &CompiledPage, request: &RenderRequest) -> SupportLevel;
    fn submit(&self, request: RenderRequest) -> Result<RenderTicket, SubmitError>;
}

/// Best-effort extraction of a human-readable message from a panic payload.
/// Public so other panic boundaries (e.g. the scheduler's render workers)
/// produce the same [`RenderError::Panic`] taxonomy.
pub fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Submit a job with a panic boundary: a panic escaping the backend's
/// `submit` (the CPU backend fulfills synchronously inside `submit`) is
/// caught and converted into a ticket that resolves to
/// [`RenderError::Panic`]. Worker pools driving `submit` directly should use
/// this so one poisoned page can never take down a worker thread.
///
/// `AssertUnwindSafe` is justified: the request's `CompiledPage` is behind an
/// immutable `Arc` snapshot (backends never mutate it), and all backend
/// scratch state is per-call and discarded on unwind, so no observable
/// broken invariants survive the catch.
pub fn submit_caught(
    backend: &dyn RenderBackend,
    request: RenderRequest,
) -> Result<RenderTicket, SubmitError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| backend.submit(request))) {
        Ok(outcome) => outcome,
        Err(payload) => {
            // Fabricate a resolved ticket carrying the typed panic error so
            // callers observe a normal per-page failure.
            let (ticket, tx) = RenderTicket::new(u64::MAX);
            let _ = tx.send(Err(RenderError::Panic {
                message: panic_message(payload),
            }));
            Ok(ticket)
        }
    }
}

/// Blocking convenience wrapper — the API most simple embedders use.
///
/// This is a panic boundary: a panic anywhere in the backend's per-page
/// compile/render path becomes [`RenderError::Panic`] for *that page only* —
/// never a process abort or a dead worker. `AssertUnwindSafe` is sound here
/// because the document snapshot behind the request is immutable and every
/// piece of backend scratch state is per-call and discarded on unwind.
pub fn render_blocking(
    backend: &dyn RenderBackend,
    request: RenderRequest,
) -> Result<RenderedPage, RenderError> {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match backend.submit(request) {
            Ok(ticket) => ticket.wait(),
            Err(e) => Err(RenderError::Backend(e.to_string())),
        }
    }));
    match outcome {
        Ok(result) => result,
        Err(payload) => Err(RenderError::Panic {
            message: panic_message(payload),
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const fn assert_send_sync<T: Send + Sync>() {}
    const _: () = assert_send_sync::<RenderRequest>();
    const _: () = assert_send_sync::<HostPage>();

    #[test]
    fn ticket_roundtrip() {
        let (ticket, tx) = RenderTicket::new(7);
        assert!(ticket.try_wait().is_none());
        let page = HostPage {
            width: 1,
            height: 1,
            stride: 4,
            format: OutputFormat::Rgba8PremultipliedSrgb,
            pixels: Arc::from([0u8; 4]),
        };
        tx.send(Ok(RenderedPage::Host(page))).unwrap();
        assert!(matches!(ticket.wait(), Ok(RenderedPage::Host(_))));
    }

    #[test]
    fn dropped_sender_is_disconnected() {
        let (ticket, tx) = RenderTicket::new(1);
        drop(tx);
        assert!(matches!(ticket.wait(), Err(RenderError::Disconnected)));
    }

    /// Test-only backend that panics inside `submit`, standing in for any
    /// bug in the per-page compile/render path.
    #[derive(Debug)]
    struct PanickingBackend;

    impl RenderBackend for PanickingBackend {
        fn id(&self) -> BackendId {
            BackendId::Other(0xDEAD)
        }
        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                formats: vec![OutputFormat::Rgba8PremultipliedSrgb],
                max_surface: DeviceSize {
                    width: 1,
                    height: 1,
                },
                features: PageFeatures::empty(),
                resident_surfaces: false,
                postprocess: PostprocessCapabilities::NONE,
            }
        }
        fn supports(&self, _page: &CompiledPage, _request: &RenderRequest) -> SupportLevel {
            SupportLevel::Native
        }
        fn submit(&self, _request: RenderRequest) -> Result<RenderTicket, SubmitError> {
            panic!("injected test panic: page 0 poisoned");
        }
    }

    fn dummy_request() -> RenderRequest {
        let page = CompiledPage::empty(pdf_page_ir::PageBounds {
            crop: pdf_page_ir::Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 10.0,
                y1: 10.0,
            },
            rotate: 0,
        });
        RenderRequest {
            page: Arc::new(page),
            transform: PageTransform {
                matrix: Matrix::IDENTITY,
            },
            crop: None,
            output_size: DeviceSize {
                width: 4,
                height: 4,
            },
            output_format: OutputFormat::Rgba8PremultipliedSrgb,
            background: Background::White,
            annotations: AnnotationMode::None,
            color_policy: RenderColorPolicy::Original,
            quality: RenderQuality::Normal,
            limits: RenderLimits::default(),
            residency: OutputResidency::HostRequired,
        }
    }

    #[test]
    fn injected_panic_becomes_typed_error_in_render_blocking() {
        // Silence the default panic hook's backtrace spew for this test; the
        // hook is process-global, so restore it afterwards.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = render_blocking(&PanickingBackend, dummy_request());
        std::panic::set_hook(prev);
        match result {
            Err(RenderError::Panic { message }) => {
                assert!(
                    message.contains("injected test panic"),
                    "message: {message}"
                );
            }
            other => panic!("expected RenderError::Panic, got {other:?}"),
        }
    }

    #[test]
    fn injected_panic_becomes_typed_error_in_submit_caught() {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let submitted = submit_caught(&PanickingBackend, dummy_request());
        std::panic::set_hook(prev);
        let ticket = submitted.expect("panic must yield a resolved ticket, not a SubmitError");
        match ticket.wait() {
            Err(RenderError::Panic { message }) => {
                assert!(
                    message.contains("injected test panic"),
                    "message: {message}"
                );
            }
            other => panic!("expected RenderError::Panic, got {other:?}"),
        }
    }
}
