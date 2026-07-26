//! The CPU raster backend — the engine's **normative implementation** of
//! rendering semantics (roadmap §2.3) and permanent fallback/oracle for the
//! GPU backend.
//!
//! Architecture (Phase 4): a tiled rasterizer; `CompiledPage` lowers to
//! per-tile operation lists preserving painter order; every worker owns a
//! [`CpuWorkerContext`] with all scratch buffers — no shared mutable state,
//! no global allocator pressure.
//!
//! This is the **production scalar raster engine**, not a throwaway reference:
//! it is designed for speed from the first commit, and its scalar kernels are
//! the reference for later SIMD kernels. The pipeline (per the performance
//! architecture) is:
//!
//! ```text
//! CompiledPage + RenderRequest
//!   → CpuPreparedPage      (transform, cull, classify, flatten — once/request)
//!   → executor             (compact commands; no Arc/lookup in the hot loop)
//!   → RasterKernel         (analytic scanline coverage → u8 rows)
//!   → KernelSet spans      (opaque-fill / const-blend / mask-blend, chosen once/span)
//!   → Surface / HostPage
//! ```
//!
//! Current state (Phase 4a): **solid fills with constant alpha**, with an
//! opaque-integer-rect fast path and per-render instrumentation
//! ([`RenderStats`]). Clipping, strokes, images, text, blend modes, and
//! transparency arrive in later 4x checkpoints (advice §16 order); preflight
//! (`supports()`) advertises exactly what is implemented so unsupported pages
//! route elsewhere. Adaptive band/tile execution is deferred until direct-page
//! rendering is competitive (advice §2).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pdf_page_ir::{CompiledPage, DeviceSize, Matrix, PageFeatures};
use pdf_render_api::{
    BackendCapabilities, BackendId, HostPage, OutputFormat, PostprocessCapabilities, RenderBackend,
    RenderError, RenderRequest, RenderTicket, RenderedPage, SubmitError, SupportLevel,
    UnsupportedFeature,
};

pub mod attribution;
mod exec;
mod image;
mod kernels;
mod mask;
mod prepared;
mod raster;
mod stats;
mod stroke;
mod surface;

pub use exec::CpuWorkerContext;
pub use kernels::KernelSet;
#[cfg(feature = "profiling")]
pub use prepared::DecodedImageCache;
pub use prepared::{CpuPreparedPage, DrawClass};
pub use stats::RenderStats;

use surface::Surface;

/// One opaque image draw converted to RGB8 for a non-CPU raster backend.
///
/// This deliberately exposes only the small, semantics-neutral subset used
/// by the experimental GPU image renderer. PDF image decoding, `/Decode`,
/// color-space conversion, clipping, and request transforms remain owned by
/// this crate.
#[derive(Debug, Clone)]
pub struct PreparedRgbImage {
    pub bounds: pdf_page_ir::DeviceRect,
    /// Device coordinates to the image's normalized `[0, 1]²` square.
    pub device_to_image: Matrix,
    pub width: u32,
    pub height: u32,
    pub samples: Arc<[u8]>,
    pub interpolation: pdf_page_ir::InterpolationMode,
    /// Source texels covered by one destination pixel on each image axis.
    pub footprint: [f64; 2],
}

/// An image-only page prepared for an offscreen raster backend.
#[derive(Debug, Clone)]
pub struct PreparedRgbImagePage {
    pub size: DeviceSize,
    pub images: Vec<PreparedRgbImage>,
}

/// Maximum full-source RGB8 allocation made solely to prepare a non-RGB8
/// image for GPU upload. Larger converted sources stay on the CPU's
/// destination-driven sampler instead of multiplying a compact bilevel or
/// grayscale source into a very large transient/cache allocation.
pub const MAX_PREPARED_RGB_CONVERSION_BYTES: usize = 64 * 1024 * 1024;

/// Source-texel footprint above which automatic routing keeps packed
/// bilevel images on the CPU. The CPU has a destination-driven popcount/box
/// filter for this minifying case, while preparing RGB8 would expand the
/// source and add upload work. Forced GPU routing deliberately ignores this
/// policy boundary.
pub const AUTO_CPU_BILEVEL_MINIFICATION_FOOTPRINT: f64 = 1.0;

/// Default logical tile size (roadmap §7 Phase 4); revisit after benchmarks.
pub const DEFAULT_TILE_SIZE: u32 = 512;

/// Scale an oversized render down uniformly so its RGBA8 surface fits
/// `max_bytes`, adjusting the page transform to match so content still fills
/// the (now smaller) device box.
///
/// A poster-sized MediaBox — e.g. a 9600×14400 pt foldout rendered at screen
/// DPI — would otherwise demand a multi-GB surface and fail the page outright.
/// Real viewers cap effective DPI for such pages; clamping does the same,
/// preserving the whole page at lower resolution instead of dropping it. The
/// downscale is uniform (aspect preserved); the exact per-axis ratios are
/// folded into the transform so the mapping stays consistent after flooring.
///
/// Returns the (possibly reduced) size, the adjusted matrix, and whether a
/// clamp occurred. A `max_bytes` of 0 disables clamping (no budget).
fn clamp_output_to_budget(
    size: DeviceSize,
    matrix: Matrix,
    max_bytes: u64,
) -> (DeviceSize, Matrix, bool) {
    let surface_bytes = (size.width as u64)
        .saturating_mul(size.height as u64)
        .saturating_mul(4);
    if max_bytes == 0 || surface_bytes <= max_bytes {
        return (size, matrix, false);
    }
    // Uniform factor k with (w·k)·(h·k)·4 == max_bytes. Flooring only ever
    // shrinks the result, so the clamped surface is guaranteed ≤ max_bytes.
    let k = ((max_bytes as f64) / (surface_bytes as f64)).sqrt();
    let new_w = ((size.width as f64 * k).floor() as u32).max(1);
    let new_h = ((size.height as f64 * k).floor() as u32).max(1);
    let kx = new_w as f64 / size.width as f64;
    let ky = new_h as f64 / size.height as f64;
    let adjusted = matrix.then(Matrix::scale(kx, ky));
    (
        DeviceSize {
            width: new_w,
            height: new_h,
        },
        adjusted,
        true,
    )
}

/// CPU backend configuration.
#[derive(Debug, Clone)]
pub struct CpuBackendOptions {
    /// Rayon threads for intra-backend parallelism; `None` = rayon default.
    /// Page-level parallelism is the scheduler's job, so this mostly
    /// matters for tile parallelism inside very large single pages.
    pub threads: Option<usize>,
    pub tile_size: u32,
    /// The injected image-codec registry (codec.rs: never global). The
    /// default deployment bundles JPEG (in-house), JPEG 2000 (jp2lam), and
    /// JBIG2 (temporary, see `pdf_image::jbig2`); pass
    /// [`pdf_image::CodecRegistry::empty()`] for a codec-free build.
    pub codecs: pdf_image::CodecRegistry,
    /// Glyph grid-fitting (fonts.md Font Phase 4).
    ///
    /// Defaults to [`HintingPolicy::None`]: this backend is the *normative*
    /// implementation of the frozen surface contract, and hinting moves glyph
    /// geometry, so it is opt-in rather than a silent default. A screen-facing
    /// caller wanting crisp small text sets [`HintingPolicy::Auto`], which
    /// hints only where it pays (axis-aligned, at or below
    /// `pdf_font::AUTO_HINT_MAX_PPEM`).
    pub hinting: pdf_font::HintingPolicy,
}

impl Default for CpuBackendOptions {
    fn default() -> Self {
        let codecs = pdf_image::CodecRegistry::new([
            std::sync::Arc::new(pdf_image::JpegCodec) as std::sync::Arc<dyn pdf_image::ImageCodec>,
            std::sync::Arc::new(pdf_image::JpxCodec),
            std::sync::Arc::new(pdf_image::Jbig2Codec),
            std::sync::Arc::new(pdf_image::CcittCodec),
        ]);
        Self {
            threads: None,
            tile_size: DEFAULT_TILE_SIZE,
            codecs,
            hinting: pdf_font::HintingPolicy::None,
        }
    }
}

/// The CPU render backend.
#[derive(Debug)]
pub struct CpuBackend {
    options: CpuBackendOptions,
    job_counter: AtomicU64,
    /// Document/render-session-scoped parsed-font cache, shared by every worker
    /// that renders through this backend. One backend is created per document
    /// render (the scheduler owns it for the page range), so its lifetime scopes
    /// the cache to that document. See [`prepared::SharedFontProgramCache`].
    shared_fonts: Arc<prepared::SharedFontProgramCache>,
    /// Document/render-session-scoped rendered-glyph coverage cache (PDFium
    /// `CFX_GlyphCache` analog), shared by every worker. Scoped like
    /// `shared_fonts`. See [`prepared::SharedGlyphCache`].
    shared_glyphs: Arc<prepared::SharedGlyphCache>,
    /// Document/render-session-scoped decoded-image cache, shared by every
    /// worker. Scoped like `shared_fonts`. See [`prepared::SharedImageCache`].
    shared_images: Arc<prepared::SharedImageCache>,
    /// Document/render-session-scoped RGB8 conversion cache for the
    /// experimental GPU preparation seam. Stable Arc identity lets the GPU
    /// upload cache serve subsequent tiles without converting or uploading
    /// the same source again.
    shared_rgb_images: Arc<image::SharedRgbImageCache>,
}

impl CpuBackend {
    pub fn new(options: CpuBackendOptions) -> Self {
        Self {
            options,
            job_counter: AtomicU64::new(0),
            shared_fonts: Arc::new(prepared::SharedFontProgramCache::default()),
            shared_glyphs: Arc::new(prepared::SharedGlyphCache::default()),
            shared_images: Arc::new(prepared::SharedImageCache::default()),
            shared_rgb_images: Arc::new(image::SharedRgbImageCache::default()),
        }
    }

    /// Features the rasterizer implements today. Grows phase by phase; the
    /// scheduler routes pages by comparing this to `page.features`.
    ///
    /// Solid paths + constant alpha + clipping (rect and path). Alpha needs no
    /// feature flag; `TRANSPARENCY` denotes non-Normal blend / groups, not yet
    /// implemented.
    fn implemented_features() -> PageFeatures {
        PageFeatures::BASIC_PATHS
            | PageFeatures::CLIPPING
            | PageFeatures::DASHED_STROKES
            | PageFeatures::NONSEPARABLE_BLENDS
            | PageFeatures::TRANSPARENCY
            // Text renders as synthetic placement boxes (fonts.md Font Phase 1);
            // real glyph outlines are a later font phase.
            | PageFeatures::TEXT
            // Axial + radial shadings (`sh` and shading patterns).
            | PageFeatures::SHADINGS
            // Tiling patterns (PatternType 1) + shading patterns.
            | PageFeatures::PATTERNS
            // Images with Flate/RunLength/raw data (codec images route away
            // via the NEEDS_* flags, which stay outside this set).
            | PageFeatures::IMAGES
            | PageFeatures::STENCIL_MASKS
    }

    /// The full advertised feature set: the rasterizer's static
    /// capabilities plus `NEEDS_*` codec coverage from the injected
    /// registry (an empty registry advertises no codecs and codec pages
    /// keep routing away, exactly as before).
    pub fn features(&self) -> PageFeatures {
        Self::implemented_features() | pdf_image::registry_features(&self.options.codecs)
    }

    /// Render into host memory, returning instrumentation. This is the direct
    /// entry point benchmarks and tests use; `submit` wraps it.
    pub fn render_to_host(
        &self,
        request: &RenderRequest,
    ) -> Result<(HostPage, RenderStats), RenderError> {
        let mut ctx = CpuWorkerContext::new();
        self.render_with(request, &mut ctx)
    }

    /// Lower a request into the exact prepared representation consumed by the
    /// diagnostic attribution pass. Production painting should continue to
    /// use [`Self::render_to_host`]; this intentionally exposes preparation
    /// only for non-normative renderer diagnostics.
    pub fn prepare_attribution(
        &self,
        request: &RenderRequest,
    ) -> Result<CpuPreparedPage, RenderError> {
        if let Some(token) = &request.limits.cancellation
            && token.is_cancelled()
        {
            return Err(RenderError::Cancelled);
        }
        let DeviceSize { width, height } = request.output_size;
        if width == 0 || height == 0 {
            return Err(RenderError::LimitExceeded("zero output dimension"));
        }
        let surface_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(RenderError::LimitExceeded("output size overflow"))?;
        if surface_bytes as u64 > request.limits.max_page_bytes {
            return Err(RenderError::LimitExceeded("max_page_bytes"));
        }
        let decode_limits = pdf_image::DecodeLimits {
            max_output_bytes: request.limits.max_page_bytes,
            should_cancel: request.limits.cancellation.clone().map(|token| {
                std::sync::Arc::new(move || token.is_cancelled())
                    as std::sync::Arc<dyn Fn() -> bool + Send + Sync>
            }),
            ..pdf_image::DecodeLimits::default()
        };
        let mut worker = CpuWorkerContext::new();
        Ok(prepared::lower_with_font_cache(
            &request.page,
            request.transform.matrix,
            request.output_size,
            &self.options.codecs,
            &decode_limits,
            self.options.hinting,
            request.color_policy,
            &mut worker.fonts,
            shared_font_cache_enabled().then_some(self.shared_fonts.as_ref()),
            glyph_cache_enabled().then_some(self.shared_glyphs.as_ref()),
            image_cache_enabled().then_some(&self.shared_images),
        ))
    }

    /// Decode and lower an image-only request into the narrow opaque-RGB8
    /// vocabulary consumed by the experimental GPU renderer. Color-space and
    /// `/Decode` semantics are resolved here through the CPU renderer's
    /// normative source-pixel path; WGPU receives display-ready RGB8.
    ///
    /// `Ok(None)` is a normal capability decline: the page contains another
    /// paint operation or an image requiring alpha, masks, clipping, or
    /// non-Normal blending. The caller should route that request to the
    /// regular CPU renderer.
    pub fn prepare_rgb_image_page(
        &self,
        request: &RenderRequest,
    ) -> Result<Option<PreparedRgbImagePage>, RenderError> {
        self.prepare_rgb_image_page_with_policy(request, false)
    }

    /// Prepare an image-only request using the measured automatic routing
    /// policy. Minified 1-bit images decline before RGB expansion so the
    /// caller can use the CPU's packed-bilevel destination sampler.
    pub fn prepare_rgb_image_page_for_auto(
        &self,
        request: &RenderRequest,
    ) -> Result<Option<PreparedRgbImagePage>, RenderError> {
        self.prepare_rgb_image_page_with_policy(request, true)
    }

    fn prepare_rgb_image_page_with_policy(
        &self,
        request: &RenderRequest,
        automatic_routing: bool,
    ) -> Result<Option<PreparedRgbImagePage>, RenderError> {
        let prepared = self.prepare_attribution(request)?;
        let mut images = Vec::with_capacity(prepared.ops.len());

        for op in &prepared.ops {
            let image = match op {
                prepared::PreparedOp::Image(image) => image,
                // Searchable scan PDFs often overlay zero-alpha OCR text.
                // It is semantically non-painting and may be discarded at
                // this narrow image-renderer seam just as the CPU executor's
                // alpha blend would discard it.
                prepared::PreparedOp::GlyphRun(run) if run.alpha == 0 => continue,
                _ => return Ok(None),
            };
            let eligible = !image.clip_has_mask
                && image.alpha == 255
                && image.smask.is_none()
                && image.mask.is_none()
                && !image.is_stencil
                && matches!(image.blend, pdf_page_ir::BlendMode::Normal);
            if !eligible {
                return Ok(None);
            }
            if automatic_routing
                && image.bpc == 1
                && image
                    .footprint
                    .iter()
                    .any(|axis| *axis > AUTO_CPU_BILEVEL_MINIFICATION_FOOTPRINT)
            {
                return Ok(None);
            }
            let Some(samples) = self
                .shared_rgb_images
                .get_or_convert(image, request.limits.cancellation.as_ref())?
            else {
                return Ok(None);
            };

            images.push(PreparedRgbImage {
                bounds: image.bounds,
                device_to_image: image.inv,
                width: image.width,
                height: image.height,
                samples,
                interpolation: image.interpolation,
                footprint: image.footprint,
            });
        }

        if images.is_empty() {
            return Ok(None);
        }
        Ok(Some(PreparedRgbImagePage {
            size: prepared.size,
            images,
        }))
    }

    /// Render reusing a caller-owned worker context (avoids reallocating the
    /// coverage/flatten scratch across pages).
    pub fn render_with(
        &self,
        request: &RenderRequest,
        ctx: &mut CpuWorkerContext,
    ) -> Result<(HostPage, RenderStats), RenderError> {
        if let Some(token) = &request.limits.cancellation
            && token.is_cancelled()
        {
            return Err(RenderError::Cancelled);
        }
        if request.output_size.width == 0 || request.output_size.height == 0 {
            return Err(RenderError::LimitExceeded("zero output dimension"));
        }
        // Clamp an oversized page down to the byte budget rather than failing
        // it (a poster-sized MediaBox at screen DPI). Output dimensions and the
        // transform shrink together; the page renders whole, at lower res.
        let (output_size, transform_matrix, was_clamped) = clamp_output_to_budget(
            request.output_size,
            request.transform.matrix,
            request.limits.max_page_bytes,
        );
        let DeviceSize { width, height } = output_size;
        let bpp = request.output_format.bytes_per_pixel();
        let out_stride = width as usize * bpp;
        let out_bytes = out_stride
            .checked_mul(height as usize)
            .ok_or(RenderError::LimitExceeded("output size overflow"))?;
        // The internal surface is always RGBA8; bound against that. After the
        // clamp the RGBA8 surface fits by construction — this stays a hard
        // backstop for an output format wider than 4 bytes/pixel.
        let surface_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|p| p.checked_mul(4))
            .ok_or(RenderError::LimitExceeded("output size overflow"))?;
        if surface_bytes.max(out_bytes) as u64 > request.limits.max_page_bytes {
            return Err(RenderError::LimitExceeded("max_page_bytes"));
        }

        let mut stats = RenderStats::default();
        if was_clamped {
            stats.recovery_notes.push(format!(
                "oversized page clamped to {width}x{height} to fit max_page_bytes"
            ));
        }

        // Request-specific lowering: transform, flatten, cull, classify, once.
        #[cfg(feature = "profiling")]
        let lower_start = std::time::Instant::now();
        let decode_limits = pdf_image::DecodeLimits {
            max_output_bytes: request.limits.max_page_bytes,
            should_cancel: request.limits.cancellation.clone().map(|t| {
                std::sync::Arc::new(move || t.is_cancelled())
                    as std::sync::Arc<dyn Fn() -> bool + Send + Sync>
            }),
            ..pdf_image::DecodeLimits::default()
        };
        let prepared = prepared::lower_with_font_cache(
            &request.page,
            transform_matrix,
            output_size,
            &self.options.codecs,
            &decode_limits,
            self.options.hinting,
            request.color_policy,
            &mut ctx.fonts,
            shared_font_cache_enabled().then_some(self.shared_fonts.as_ref()),
            glyph_cache_enabled().then_some(self.shared_glyphs.as_ref()),
            image_cache_enabled().then_some(&self.shared_images),
        );
        #[cfg(feature = "profiling")]
        {
            stats.lower = lower_start.elapsed();
        }
        stats.ops_total = request.page.operations.len() as u32;

        #[cfg(feature = "profiling")]
        let prep_start = std::time::Instant::now();
        let mut surface = Surface::new(width as usize, height as usize, request.background);
        stats.surface_bytes = surface.bytes();
        #[cfg(feature = "profiling")]
        {
            stats.prep = prep_start.elapsed();
        }

        exec::execute(&prepared, &mut surface, ctx, &mut stats);
        if stats.cancelled {
            return Err(RenderError::Cancelled);
        }
        // Fold in codec draws dropped during top-level lowering (tiling-cell
        // drops are absorbed inside `execute`). A page left blank by an
        // undecodable image is now observable, never a silent clean pass.
        stats.absorb_diagnostics(&prepared.diagnostics);
        #[cfg(feature = "profiling")]
        stats.profile.merge(&prepared.profile_report());
        let _ = &self.options;

        #[cfg(feature = "profiling")]
        let output_start = std::time::Instant::now();
        let (stride, pixels) = surface.into_output(request.output_format);
        let pixels: Arc<[u8]> = Arc::from(pixels);
        stats.output_bytes = pixels.len() as u64;
        #[cfg(feature = "profiling")]
        {
            stats.output = output_start.elapsed();
        }
        Ok((
            HostPage {
                width,
                height,
                stride,
                format: request.output_format,
                pixels,
            },
            stats,
        ))
    }

    /// Render with the same semantics as [`Self::render_with`] while returning
    /// the feature-gated performance report consumed by benchmark tooling.
    #[cfg(feature = "profiling")]
    pub fn render_profiled_with(
        &self,
        request: &RenderRequest,
        ctx: &mut CpuWorkerContext,
    ) -> Result<(HostPage, RenderStats, pdf_profiling::ProfileReport), RenderError> {
        let total_start = std::time::Instant::now();
        let (host, stats) = self.render_with(request, ctx)?;
        let mut profile = pdf_profiling::ProfileReport::new();
        profile.add_duration("render.lower", stats.lower);
        profile.add_duration("render.surface", stats.prep);
        profile.add_duration("render.execute", stats.raster);
        profile.add_duration("render.output", stats.output);
        profile.add_duration("render.total", total_start.elapsed());
        profile.increment("render.operations", stats.ops_total as u64);
        profile.increment("render.commands", stats.commands as u64);
        profile.increment("render.painted_ops", stats.ops_painted as u64);
        profile.increment("render.edges", stats.edges);
        profile.increment("render.covered_pixels", stats.covered_pixels);
        profile.increment(
            "render.transparency_groups",
            stats.transparency_groups as u64,
        );
        profile.increment("render.soft_masks", stats.soft_masks as u64);
        profile.increment("render.output_bytes", stats.output_bytes);
        profile.merge(&stats.profile);
        let transient_render_bytes = stats.surface_bytes.saturating_add(stats.output_bytes);
        profile.allocate_bytes(transient_render_bytes);
        profile.release_bytes(transient_render_bytes);
        Ok((host, stats, profile))
    }

    /// Lower a compiled page once for repeated prepared-page execution
    /// experiments. The opaque prepared page is intentionally feature-gated:
    /// production callers should keep using `render_with`.
    #[cfg(feature = "profiling")]
    pub fn prepare_profiled(
        &self,
        request: &RenderRequest,
    ) -> Result<(CpuPreparedPage, pdf_profiling::ProfileReport), RenderError> {
        let DeviceSize { width, height } = request.output_size;
        if width == 0 || height == 0 {
            return Err(RenderError::LimitExceeded("zero output dimension"));
        }
        let surface_bytes = width as u64 * height as u64 * 4;
        if surface_bytes > request.limits.max_page_bytes {
            return Err(RenderError::LimitExceeded("max_page_bytes"));
        }
        let start = std::time::Instant::now();
        let decode_limits = pdf_image::DecodeLimits {
            max_output_bytes: request.limits.max_page_bytes,
            should_cancel: request.limits.cancellation.clone().map(|t| {
                std::sync::Arc::new(move || t.is_cancelled())
                    as std::sync::Arc<dyn Fn() -> bool + Send + Sync>
            }),
            ..pdf_image::DecodeLimits::default()
        };
        let prepared = prepared::lower(
            &request.page,
            request.transform.matrix,
            request.output_size,
            &self.options.codecs,
            &decode_limits,
            self.options.hinting,
        );
        let mut profile = prepared.profile_report();
        profile.add_duration("render.prepare", start.elapsed());
        Ok((prepared, profile))
    }

    /// Prepare using profiling-only decoded-image residency shared by the
    /// caller across repeated preparations.
    #[cfg(feature = "profiling")]
    pub fn prepare_with_decode_cache_profiled(
        &self,
        request: &RenderRequest,
        cache: DecodedImageCache,
    ) -> Result<(CpuPreparedPage, pdf_profiling::ProfileReport), RenderError> {
        let DeviceSize { width, height } = request.output_size;
        if width == 0 || height == 0 {
            return Err(RenderError::LimitExceeded("zero output dimension"));
        }
        let surface_bytes = width as u64 * height as u64 * 4;
        if surface_bytes > request.limits.max_page_bytes {
            return Err(RenderError::LimitExceeded("max_page_bytes"));
        }
        let start = std::time::Instant::now();
        let decode_limits = pdf_image::DecodeLimits {
            max_output_bytes: request.limits.max_page_bytes,
            should_cancel: request.limits.cancellation.clone().map(|t| {
                std::sync::Arc::new(move || t.is_cancelled())
                    as std::sync::Arc<dyn Fn() -> bool + Send + Sync>
            }),
            ..pdf_image::DecodeLimits::default()
        };
        let prepared = prepared::lower_with_decode_cache(
            &request.page,
            request.transform.matrix,
            request.output_size,
            &self.options.codecs,
            &decode_limits,
            self.options.hinting,
            cache,
        );
        let mut profile = prepared.profile_report();
        profile.add_duration("render.prepare", start.elapsed());
        Ok((prepared, profile))
    }

    /// Decode the compiled page's unique embedded codec payloads without
    /// lowering geometry or rasterizing.
    #[cfg(feature = "profiling")]
    pub fn decode_page_profiled(
        &self,
        request: &RenderRequest,
    ) -> Result<pdf_profiling::ProfileReport, RenderError> {
        let decode_limits = pdf_image::DecodeLimits {
            max_output_bytes: request.limits.max_page_bytes,
            should_cancel: request.limits.cancellation.clone().map(|t| {
                std::sync::Arc::new(move || t.is_cancelled())
                    as std::sync::Arc<dyn Fn() -> bool + Send + Sync>
            }),
            ..pdf_image::DecodeLimits::default()
        };
        Ok(prepared::decode_page(
            &request.page,
            &self.options.codecs,
            &decode_limits,
            self.options.hinting,
        ))
    }

    /// Execute a page previously returned by [`Self::prepare_profiled`].
    #[cfg(feature = "profiling")]
    pub fn execute_prepared_profiled(
        &self,
        request: &RenderRequest,
        prepared: &CpuPreparedPage,
        ctx: &mut CpuWorkerContext,
    ) -> Result<(HostPage, RenderStats, pdf_profiling::ProfileReport), RenderError> {
        let DeviceSize { width, height } = request.output_size;
        if prepared.size != request.output_size || width == 0 || height == 0 {
            return Err(RenderError::Backend(
                "prepared page does not match request".into(),
            ));
        }
        let total_start = std::time::Instant::now();
        let surface_start = std::time::Instant::now();
        let mut surface = Surface::new(width as usize, height as usize, request.background);
        let mut stats = RenderStats {
            surface_bytes: surface.bytes(),
            ..RenderStats::default()
        };
        stats.prep = surface_start.elapsed();
        exec::execute(prepared, &mut surface, ctx, &mut stats);
        stats.absorb_diagnostics(&prepared.diagnostics);
        let output_start = std::time::Instant::now();
        let (stride, pixels) = surface.into_output(request.output_format);
        let pixels: Arc<[u8]> = Arc::from(pixels);
        stats.output_bytes = pixels.len() as u64;
        stats.output = output_start.elapsed();
        let mut profile = stats.profile.clone();
        profile.add_duration("render.surface", stats.prep);
        profile.add_duration("render.execute", stats.raster);
        profile.add_duration("render.output", stats.output);
        profile.add_duration("render.prepared_total", total_start.elapsed());
        profile.increment("render.output_bytes", stats.output_bytes);
        let transient_render_bytes = stats.surface_bytes.saturating_add(stats.output_bytes);
        profile.allocate_bytes(transient_render_bytes);
        profile.release_bytes(transient_render_bytes);
        Ok((
            HostPage {
                width,
                height,
                stride,
                format: request.output_format,
                pixels,
            },
            stats,
            profile,
        ))
    }
}

impl Default for CpuBackend {
    fn default() -> Self {
        Self::new(CpuBackendOptions::default())
    }
}

/// Whether the document-scoped shared font parse cache is active. On by default;
/// `PDF_RENDERER_FONTCACHE=off` (or `0`) disables it, for paired A/B isolation
/// of the cache on one binary. Read once.
fn shared_font_cache_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("PDF_RENDERER_FONTCACHE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "off" && v != "0" && v != "false"
        }
        Err(_) => true,
    })
}

/// Whether the document-scoped rendered-glyph coverage cache is active. On by
/// default; `PDF_RENDERER_GLYPHCACHE=off` (or `0`) disables it, for paired A/B
/// isolation of the cache on one binary. Read once.
fn glyph_cache_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("PDF_RENDERER_GLYPHCACHE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "off" && v != "0" && v != "false"
        }
        Err(_) => true,
    })
}

/// Whether the document-scoped decoded-image cache is active. On by default;
/// `PDF_RENDERER_IMAGECACHE=off` (or `0`) disables it, for paired A/B
/// isolation of the cache on one binary. Read once.
fn image_cache_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("PDF_RENDERER_IMAGECACHE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "off" && v != "0" && v != "false"
        }
        Err(_) => true,
    })
}

impl Drop for CpuBackend {
    fn drop(&mut self) {
        // Optional observability for the parse-cache measurement: on backend
        // teardown (one document render session), report how often the shared
        // Type 1 / bare-CFF parse cache served a program without reparsing.
        if std::env::var_os("PDF_RENDERER_FONTCACHE_STATS").is_some() {
            let (hits, inserts) = self.shared_fonts.stats();
            eprintln!("shared-font-cache: hits={hits} inserts={inserts}");
        }
        if std::env::var_os("PDF_RENDERER_GLYPHCACHE_STATS").is_some() {
            let (hits, misses, inserts) = self.shared_glyphs.stats();
            let total = hits + misses;
            let rate = if total > 0 {
                hits as f64 / total as f64
            } else {
                0.0
            };
            eprintln!(
                "shared-glyph-cache: hits={hits} misses={misses} inserts={inserts} hit_rate={rate:.4}"
            );
        }
        if std::env::var_os("PDF_RENDERER_IMAGECACHE_STATS").is_some() {
            let (hits, misses, inserts) = self.shared_images.stats();
            let total = hits + misses;
            let rate = if total > 0 {
                hits as f64 / total as f64
            } else {
                0.0
            };
            eprintln!(
                "shared-image-cache: hits={hits} misses={misses} inserts={inserts} hit_rate={rate:.4}"
            );
        }
    }
}

impl RenderBackend for CpuBackend {
    fn id(&self) -> BackendId {
        BackendId::Cpu
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            formats: vec![OutputFormat::Rgba8PremultipliedSrgb, OutputFormat::Gray8],
            max_surface: DeviceSize {
                width: 1 << 16,
                height: 1 << 16,
            },
            features: self.features(),
            resident_surfaces: false,
            postprocess: PostprocessCapabilities::HOST_ALL,
        }
    }

    fn supports(&self, page: &CompiledPage, _request: &RenderRequest) -> SupportLevel {
        let missing = page.features.difference(self.features());
        if missing.is_empty() {
            SupportLevel::Native
        } else {
            SupportLevel::Unsupported(UnsupportedFeature {
                missing,
                detail: "feature not yet implemented in CPU rasterizer",
            })
        }
    }

    fn submit(&self, request: RenderRequest) -> Result<RenderTicket, SubmitError> {
        let job_id = self.job_counter.fetch_add(1, Ordering::Relaxed);
        let (ticket, tx) = RenderTicket::new(job_id);
        // Synchronous fulfillment for now; the scheduler provides page-level
        // parallelism by calling submit from its own worker pool. Phase 4
        // may move fulfillment onto an internal rayon pool for tile
        // parallelism inside huge pages.
        let result = self
            .render_to_host(&request)
            .map(|(host, _stats)| RenderedPage::Host(host));
        let _ = tx.send(result);
        Ok(ticket)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn clamp_leaves_within_budget_pages_untouched() {
        let size = DeviceSize {
            width: 1000,
            height: 800,
        };
        let (out, m, clamped) = clamp_output_to_budget(size, Matrix::IDENTITY, 2 << 30);
        assert!(!clamped);
        assert_eq!(out, size);
        assert_eq!(m, Matrix::IDENTITY);
    }

    #[test]
    fn clamp_zero_budget_disables() {
        let size = DeviceSize {
            width: 100_000,
            height: 100_000,
        };
        let (out, _, clamped) = clamp_output_to_budget(size, Matrix::IDENTITY, 0);
        assert!(!clamped);
        assert_eq!(out, size);
    }

    #[test]
    fn capabilities_advertise_complete_host_postprocessing() {
        let postprocess = CpuBackend::default().capabilities().postprocess;
        assert_eq!(
            postprocess.operations,
            pdf_render_api::PostprocessOperations::all()
        );
        assert!(!postprocess.resident_execution);
    }

    #[test]
    fn clamp_oversized_fits_budget_and_folds_scale_into_transform() {
        // A poster page whose RGBA8 surface far exceeds the budget.
        let size = DeviceSize {
            width: 20_000,
            height: 30_000,
        };
        let budget = 2u64 << 30; // 2 GiB
        let (out, m, clamped) = clamp_output_to_budget(size, Matrix::scale(2.0, 2.0), budget);
        assert!(clamped);
        // Clamped surface fits the budget by construction.
        let surface = out.width as u64 * out.height as u64 * 4;
        assert!(surface <= budget, "surface {surface} > budget {budget}");
        // Aspect ratio is preserved within a pixel of rounding.
        let want = size.width as f64 / size.height as f64;
        let got = out.width as f64 / out.height as f64;
        assert!((want - got).abs() < 1e-3);
        // The original scale-2 matrix mapped page (10000,15000) onto the old
        // device far corner (20000,30000); after the clamp the same page point
        // must land on the new box's far corner.
        let corner = m.apply(pdf_page_ir::Point {
            x: 10_000.0,
            y: 15_000.0,
        });
        assert!((corner.x - out.width as f64).abs() < 2.0);
        assert!((corner.y - out.height as f64).abs() < 2.0);
    }

    use pdf_page_ir::{PageBounds, Rect};
    use pdf_render_api::{
        AnnotationMode, Background, OutputResidency, PageTransform, RenderLimits, RenderQuality,
        render_blocking,
    };

    fn basic_request(format: OutputFormat, background: Background) -> RenderRequest {
        let page = CompiledPage::empty(PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 612.0,
                y1: 792.0,
            },
            rotate: 0,
        });
        RenderRequest {
            page: Arc::new(page),
            transform: PageTransform {
                matrix: pdf_page_ir::Matrix::IDENTITY,
            },
            crop: None,
            output_size: DeviceSize {
                width: 8,
                height: 4,
            },
            output_format: format,
            background,
            annotations: AnnotationMode::None,
            color_policy: pdf_render_api::RenderColorPolicy::Original,
            quality: RenderQuality::Normal,
            limits: RenderLimits::default(),
            residency: OutputResidency::HostRequired,
        }
    }

    #[test]
    fn renders_white_background_end_to_end() {
        let backend = CpuBackend::default();
        let request = basic_request(OutputFormat::Rgba8PremultipliedSrgb, Background::White);
        assert!(matches!(
            backend.supports(&request.page, &request),
            SupportLevel::Native
        ));
        let page = render_blocking(&backend, request).unwrap();
        let host = page.as_host().unwrap();
        assert_eq!((host.width, host.height, host.stride), (8, 4, 32));
        assert!(host.pixels.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn respects_memory_limit() {
        // A budget too small for the full surface clamps the page down to fit
        // rather than rejecting it: the render succeeds and the RGBA8 surface
        // stays within budget (the OOM guard is preserved, just non-fatally).
        let backend = CpuBackend::default();
        let mut request = basic_request(OutputFormat::Gray8, Background::White);
        request.limits.max_page_bytes = 8;
        let page = render_blocking(&backend, request).unwrap();
        let host = page.as_host().unwrap();
        assert!(host.width >= 1 && host.height >= 1);
        assert!(
            host.width as u64 * host.height as u64 * 4 <= 8,
            "clamped surface {}x{} exceeds the 8-byte budget",
            host.width,
            host.height
        );
    }

    #[test]
    fn declines_pages_with_unimplemented_features() {
        let backend = CpuBackend::default();
        let request = basic_request(OutputFormat::Gray8, Background::White);
        let mut page = (*request.page).clone();
        // Soft masks are not implemented yet, so such a page is declined.
        page.features = PageFeatures::SOFT_MASKS;
        assert!(matches!(
            backend.supports(&page, &request),
            SupportLevel::Unsupported(_)
        ));
    }
}
