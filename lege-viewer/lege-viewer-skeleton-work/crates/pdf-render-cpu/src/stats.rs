//! Per-render performance instrumentation (roadmap §15, CPU renderer).
//!
//! Cheap to collect (counters + a couple of `Instant`s) and returned alongside
//! the rendered page, so iterative performance work has a stable signal to
//! measure against without a profiler in the loop.

use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct RenderStats {
    /// The request's cancellation token fired mid-render: execution stopped
    /// at an op boundary and the surface is incomplete. The backend maps
    /// this to `RenderError::Cancelled`.
    pub cancelled: bool,
    /// Request-specific CPU lowering (transform, flatten, cull, classify).
    pub lower: Duration,
    /// Surface allocation + background fill.
    pub prep: Duration,
    /// Coverage rasterization + span compositing.
    pub raster: Duration,
    /// Conversion from the internal RGBA surface to the requested host format,
    /// including transfer into the reference-counted host buffer.
    pub output: Duration,
    /// Total display operations walked (during lowering).
    pub ops_total: u32,
    /// Prepared commands executed.
    pub commands: u32,
    /// Commands that produced paint.
    pub ops_painted: u32,
    /// Commands skipped because the feature is not implemented yet.
    pub ops_skipped: u32,
    /// Edges fed to the coverage kernel.
    pub edges: u64,
    /// Output rows touched by coverage.
    pub rows_rasterized: u64,
    /// Pixels with non-zero coverage generated.
    pub covered_pixels: u64,
    /// Transparency groups composited (offscreen layers).
    pub transparency_groups: u32,
    /// Soft masks derived and applied.
    pub soft_masks: u32,
    /// Peak surface bytes held.
    pub surface_bytes: u64,
    /// Bytes in the final host pixel buffer. During output conversion this can
    /// overlap with the internal surface allocation.
    pub output_bytes: u64,
    /// Image draws dropped because their codec (JPX/DCT/JBIG2/CCITT) could not
    /// decode. A page with `covered_pixels == 0` **and** `degraded_draws > 0`
    /// is a *silent blank* — content PDFium would paint that we dropped — not a
    /// genuinely empty page. Surfaced so differential tooling can flag it
    /// instead of scoring it clean (Workstream B3 / DEFERRED.md corpus item 1).
    pub degraded_draws: u32,
    /// A bounded, human-readable sample of the degradations (codec + reason).
    pub recovery_notes: Vec<String>,
    /// Fine-grained feature-only lowering/codec attribution.
    #[cfg(feature = "profiling")]
    pub(crate) profile: pdf_profiling::ProfileReport,
}

impl RenderStats {
    /// Fold a prepared page's lowering diagnostics into these stats. Called for
    /// the top-level page and for every tiling-pattern cell lowered during
    /// execution, so no dropped codec draw is missed.
    pub(crate) fn absorb_diagnostics(&mut self, diag: &crate::prepared::RenderDiagnostics) {
        self.degraded_draws += diag.degraded_draws();
        let room = 32usize.saturating_sub(self.recovery_notes.len());
        if room > 0 {
            self.recovery_notes
                .extend(diag.notes().iter().take(room).cloned());
        }
    }

    /// Whether this page is a *silent blank*: an undecodable image dropped its
    /// draw and nothing else painted, so the surface is (near) empty where real
    /// content belonged. The differential/sweep layer treats this as a dropped
    /// page rather than a clean render.
    pub fn is_silent_blank(&self) -> bool {
        self.degraded_draws > 0 && self.covered_pixels == 0
    }

    /// Pages-per-second for this single page (raster stage only), for quick
    /// A/B comparisons in benchmarks.
    pub fn raster_pages_per_sec(&self) -> f64 {
        let s = self.raster.as_secs_f64();
        if s > 0.0 { 1.0 / s } else { f64::INFINITY }
    }
}
