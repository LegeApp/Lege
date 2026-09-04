//! The region executor: walk a [`CpuPreparedPage`]'s compact commands in
//! painter order and paint them into a [`Surface`] via span dispatch.
//!
//! Per command: apply the clip (rectangular envelope by arithmetic, non-rect
//! clips by a lazily-built `Alpha8` mask), generate analytic coverage, then
//! for each contiguous span pick a kernel **once** — opaque full spans
//! overwrite, partial-alpha full spans use the constant-alpha blend, and
//! anti-aliased/masked runs use the per-pixel mask blend (advice §4, §5, §8).
//! No allocation, `Arc` indexing, or transform work happens here.

#[cfg(feature = "profiling")]
use std::time::Instant;

use crate::kernels::{
    BlendFn, KernelSet, NonSepBlendFn, blend_span, blend_span_nonsep, composite_px, mul_div_255,
    nonseparable_blend, separable_blend,
};

/// The blend path chosen once per command (never per pixel).
#[derive(Clone, Copy)]
enum BlendChoice {
    Normal,
    Separable(BlendFn),
    NonSeparable(NonSepBlendFn),
}

fn choose_blend(mode: pdf_page_ir::BlendMode) -> BlendChoice {
    if let Some(f) = separable_blend(mode) {
        BlendChoice::Separable(f)
    } else if let Some(f) = nonseparable_blend(mode) {
        BlendChoice::NonSeparable(f)
    } else {
        BlendChoice::Normal
    }
}
use crate::mask::{ClipGeom, ClipMask, build_clip_mask_cancellable};
use pdf_page_ir::DeviceRect;

use crate::prepared::{CpuPreparedPage, DrawClass, PreparedCommand, PreparedGlyphRun, PreparedOp};
use crate::raster::RasterKernel;
use crate::stats::RenderStats;
use crate::surface::{Surface, unique_arc_mut, zeroed_arc};
use rayon::prelude::*;

/// Per-worker scratch: the coverage kernel and the kernel-set table. Reusable
/// geometry buffers live on the prepared page and the kernel; nothing here
/// allocates per command after warm-up (advice §13).
#[derive(Debug)]
pub struct CpuWorkerContext {
    raster: RasterKernel,
    kernels: KernelSet,
    pub(crate) fonts: crate::prepared::FontProgramCache,
    /// The previous render's page buffer, kept so the next render of the same
    /// geometry repaints resident memory instead of faulting in a fresh
    /// mapping. At most one buffer, always the most recent page's size, and
    /// only ever handed back out when its consumer has dropped it — so this
    /// retains no more than the one page surface the worker was already sized
    /// for, and nothing at all until a page has been rendered.
    pub(crate) surface_buffer: Option<std::sync::Arc<[u8]>>,
}

impl Default for CpuWorkerContext {
    fn default() -> Self {
        Self {
            raster: RasterKernel::default(),
            kernels: KernelSet::select(),
            fonts: crate::prepared::FontProgramCache::default(),
            surface_buffer: None,
        }
    }
}

impl CpuWorkerContext {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Execute a page into `surface`, rendering transparency groups into bounded
/// offscreen surfaces that composite back (advice §9).
pub fn execute(
    page: &CpuPreparedPage,
    surface: &mut Surface,
    ctx: &mut CpuWorkerContext,
    stats: &mut RenderStats,
) {
    #[cfg(feature = "profiling")]
    let start = Instant::now();
    let _in_flight = PageInFlight::enter();
    let CpuWorkerContext {
        raster, kernels, ..
    } = ctx;
    // Region-local clip masks, built once per page and indexed by dense
    // ClipId (advice §8 — no hash map in the draw loop).
    let mut masks: Vec<Option<ClipMask>> = (0..page.clips.len()).map(|_| None).collect();
    // Clip-dense pages (basin maps, music notation) spend nearly all their time
    // rasterizing hundreds of large independent path-clip masks. Each mask is a
    // pure function of the page and is byte-identical regardless of build order,
    // so pre-build every consumed mask across the rayon pool before the serial
    // draw loop. The draw loop then finds them cached. Peak memory is unchanged
    // (every consumed mask is cached by page end anyway); this only front-loads
    // and parallelizes the build.
    prebuild_masks(page, &mut masks);
    // Active soft masks (top = current); `None` entry = /SMask /None.
    let mut soft: Vec<Option<ClipMask>> = Vec::new();
    run_ops(
        page,
        0,
        page.ops.len(),
        surface,
        raster,
        kernels,
        &mut masks,
        &mut soft,
        stats,
    );
    #[cfg(feature = "profiling")]
    {
        stats.raster += start.elapsed();
    }
}

/// Derive every top-level page soft mask once for the experimental image-page
/// preparation seam. Mask content still runs through the normative CPU
/// executor, including nested masks, groups, clips, `/BC`, and `/TR`; callers
/// can then attach the resulting device-space coverage plane to GPU image
/// draws without teaching the GPU backend how to paint arbitrary mask content.
///
/// The returned vector is indexed like `page.ops`; only `PushSoftMask` slots
/// are populated. `None` for the whole result means cooperative cancellation.
pub(crate) fn prepare_soft_masks(page: &CpuPreparedPage) -> Option<Vec<Option<ClipMask>>> {
    let mut prepared: Vec<Option<ClipMask>> = std::iter::repeat_with(|| None)
        .take(page.ops.len())
        .collect();
    let mut raster = RasterKernel::default();
    let kernels = KernelSet::select();
    let mut masks: Vec<Option<ClipMask>> = (0..page.clips.len()).map(|_| None).collect();
    let mut stats = RenderStats::default();
    let mut i = 0;

    while i < page.ops.len() {
        if let Some(cancel) = &page.decode_limits.should_cancel
            && cancel()
        {
            return None;
        }
        match &page.ops[i] {
            PreparedOp::PushSoftMask {
                kind,
                transfer,
                content_end,
                bounds,
            } => {
                let content_end = *content_end as usize;
                prepared[i] = Some(render_soft_mask(
                    page,
                    i + 1,
                    content_end,
                    *kind,
                    transfer.as_deref(),
                    *bounds,
                    &mut raster,
                    &kernels,
                    &mut masks,
                    &mut stats,
                ));
                if stats.cancelled {
                    return None;
                }
                i = content_end;
            }
            _ => i += 1,
        }
    }

    Some(prepared)
}

/// Rasterize one patterned `/ImageMask` brush without its page-level alpha or
/// blend. The resulting bounded plane is straight RGB plus independent alpha,
/// ready for the experimental GPU image compositor to apply painter-order
/// state. `None` means cancellation or a degraded nested pattern-cell draw;
/// callers then route the complete request through the normative CPU backend
/// so diagnostics cannot disappear at the GPU seam.
pub(crate) fn prepare_patterned_stencil(
    page: &CpuPreparedPage,
    tiling: &crate::prepared::PreparedTiling,
) -> Option<crate::PreparedPatternedStencil> {
    tiling.stencil.as_ref()?;
    let mut normalized = tiling.clone();
    normalized.alpha = 255;
    normalized.blend = pdf_page_ir::BlendMode::Normal;

    let mut surface = Surface::offscreen(normalized.bounds);
    let mut raster = RasterKernel::default();
    let kernels = KernelSet::select();
    let mut masks: Vec<Option<ClipMask>> = (0..page.clips.len()).map(|_| None).collect();
    let mut stats = RenderStats::default();
    render_tiling(
        page,
        &normalized,
        &mut surface,
        &mut raster,
        &kernels,
        &mut masks,
        &mut stats,
    );
    if stats.cancelled || stats.degraded_draws > 0 {
        return None;
    }

    let (_, rgba) = surface.into_output(pdf_render_api::OutputFormat::Rgba8PremultipliedSrgb);
    let pixels = normalized.bounds.width as usize * normalized.bounds.height as usize;
    let mut samples = zeroed_arc(pixels * 3);
    let mut opacity = zeroed_arc(pixels);
    let samples_data = unique_arc_mut(&mut samples);
    let opacity_data = unique_arc_mut(&mut opacity);
    for (index, pixel) in rgba.chunks_exact(4).enumerate() {
        let alpha = u16::from(pixel[3]);
        opacity_data[index] = pixel[3];
        let sample = &mut samples_data[index * 3..index * 3 + 3];
        if alpha == 0 {
            sample.fill(0);
        } else {
            for (dst, &channel) in sample.iter_mut().zip(&pixel[..3]) {
                *dst = ((u16::from(channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    Some(crate::PreparedPatternedStencil {
        bounds: normalized.bounds,
        samples,
        opacity,
    })
}

/// Pre-build every clip mask the ops will consume, across the rayon pool, so the
/// serial draw loop finds them cached. Each mask is a pure function of `page`
/// and byte-identical regardless of build order, so this is safe to parallelize
/// and never changes output. Engaged only past a threshold where the fan-out
/// pays for itself; below it the draw loop builds the few masks lazily.
fn prebuild_masks(page: &CpuPreparedPage, masks: &mut [Option<ClipMask>]) {
    // Cooperative cancellation: skip the (bounded) pre-pass if already cancelled.
    if let Some(cancel) = &page.decode_limits.should_cancel
        && cancel()
    {
        return;
    }
    // The distinct mask-source clips actually drawn under (a mask pushed but
    // never consumed is never built — matching the lazy loop's behavior).
    let mut wanted = vec![false; page.clips.len()];
    for op in &page.ops {
        let clip = match op {
            PreparedOp::Draw(c) if c.clip_has_mask => c.clip,
            PreparedOp::GlyphRun(g) if g.clip_has_mask => g.clip,
            PreparedOp::Image(i) if i.clip_has_mask => i.clip,
            PreparedOp::TiledFill(t) if t.clip_has_mask => t.clip,
            _ => None,
        };
        if let Some(cid) = clip
            && let Some(src) = page.clips[cid as usize].mask_source
        {
            wanted[src as usize] = true;
        }
    }
    let ids: Vec<u32> = (0..page.clips.len() as u32)
        .filter(|&i| wanted[i as usize])
        .collect();
    // Below this count, rayon fan-out costs more than it saves.
    const PARALLEL_MASK_THRESHOLD: usize = 16;
    if ids.len() < PARALLEL_MASK_THRESHOLD {
        return;
    }
    // One reused scratch kernel per worker thread (grows on first fill).
    let geom = ClipGeom::of(page);
    let should_cancel = page.decode_limits.should_cancel.as_deref();
    let built: Vec<(u32, ClipMask)> = ids
        .par_iter()
        .map_init(RasterKernel::default, |rk, &cid| {
            build_clip_mask_cancellable(rk, geom, cid, should_cancel).map(|mask| (cid, mask))
        })
        .filter_map(|built| built)
        .collect();
    for (cid, m) in built {
        masks[cid as usize] = Some(m);
    }
}

/// Execute ops `[start, end)` into `surface`. `BeginGroup` recurses into a
/// fresh offscreen surface and composites back; `PushSoftMask` renders its
/// content offscreen, derives a per-pixel mask, and pushes it.
#[allow(clippy::too_many_arguments)]
fn run_ops(
    page: &CpuPreparedPage,
    start: usize,
    end: usize,
    surface: &mut Surface,
    raster: &mut RasterKernel,
    kernels: &KernelSet,
    masks: &mut Vec<Option<ClipMask>>,
    soft: &mut Vec<Option<ClipMask>>,
    stats: &mut RenderStats,
) {
    let mut i = start;
    while i < end {
        // Cooperative mid-render cancellation (roadmap §4.4): checked at op
        // boundaries every 16 commands, so a cancelled viewer request stops
        // within a bounded slice of work instead of at page completion.
        if i.trailing_zeros() >= 4
            && let Some(cancel) = &page.decode_limits.should_cancel
            && cancel()
        {
            stats.cancelled = true;
            return;
        }
        match &page.ops[i] {
            PreparedOp::Draw(cmd) => {
                let active_soft = soft.last().and_then(Option::as_ref);
                paint_command(
                    page,
                    cmd,
                    surface,
                    raster,
                    kernels,
                    masks,
                    active_soft,
                    stats,
                );
                i += 1;
            }
            PreparedOp::GlyphRun(gr) => {
                let active_soft = soft.last().and_then(Option::as_ref);
                paint_glyph_run(
                    page,
                    gr,
                    surface,
                    raster,
                    kernels,
                    masks,
                    active_soft,
                    stats,
                );
                i += 1;
            }
            PreparedOp::TiledFill(t) => {
                render_tiling(page, t, surface, raster, kernels, masks, stats);
                i += 1;
            }
            PreparedOp::Image(img) => {
                let active_soft = soft.last().and_then(Option::as_ref);
                paint_image(page, img, surface, raster, masks, active_soft, stats);
                i += 1;
            }
            PreparedOp::BeginGroup {
                group,
                end: group_end,
            } => {
                let ge = *group_end as usize;
                if group.bounds.width == 0 || group.bounds.height == 0 {
                    i = ge + 1;
                    continue;
                }
                // Group backdrop (ISO 32000-1 §11.4.7): an *isolated* group
                // starts transparent; a *non-isolated* group's elements
                // composite against the backdrop behind it. We seed the
                // backdrop only for a non-isolated group whose own composite
                // blend is Normal (the common wrapper case) — there the
                // seeded offscreen is composited back by a plain replace/lerp,
                // which is exact for an opaque backdrop and cannot double-count
                // it. A non-isolated group with a *non-Normal* composite blend
                // (e.g. a Multiply group) is still rendered on a transparent
                // offscreen (its content isolated) and composited with that
                // blend against the parent — which, when the parent is the page
                // or an already-seeded non-isolated wrapper, now holds the real
                // backdrop, so `Multiply` finally hits the tone underneath
                // instead of transparency (the Young-Turks gold, the Medieval
                // page tone). This is Stage 1: full non-isolated compositing
                // with backdrop removal for a non-Normal group whose *content*
                // carries its own non-Normal blends remains approximated.
                let blend = choose_blend(group.blend);
                // The soft mask in force at the invocation modulates the
                // *group's* composite, not its contents (§11.6.6 — lowering
                // pushes a None reset inside the group for that reason). Its
                // stack slot is stable across the group's ops, which are
                // balanced, so remember the depth rather than borrowing across
                // the `run_ops` mutable borrow.
                let soft_slot = soft.len().checked_sub(1);
                let seed = !group.isolated && matches!(blend, BlendChoice::Normal);
                let mut off = if seed {
                    Surface::offscreen_seeded(surface, group.bounds)
                } else {
                    Surface::offscreen(group.bounds)
                };
                if group.knockout {
                    run_knockout_group(
                        page,
                        i + 1,
                        ge,
                        &mut off,
                        raster,
                        kernels,
                        masks,
                        soft,
                        stats,
                    );
                } else {
                    run_ops(
                        page,
                        i + 1,
                        ge,
                        &mut off,
                        raster,
                        kernels,
                        masks,
                        soft,
                        stats,
                    );
                }
                let group_soft = soft_slot
                    .and_then(|ix| soft.get(ix))
                    .and_then(Option::as_ref);
                if seed {
                    // Seeded offscreen already holds backdrop∘content; fold it
                    // back at group opacity without re-compositing the backdrop.
                    composite_group_seeded(surface, &off, group.opacity, group_soft);
                } else {
                    composite_group(surface, &off, group.opacity, blend, group_soft);
                }
                stats.transparency_groups += 1;
                i = ge + 1;
            }
            PreparedOp::EndGroup => i += 1,
            PreparedOp::PushSoftMask {
                kind,
                transfer,
                content_end,
                bounds,
            } => {
                let ce = *content_end as usize;
                let mask = render_soft_mask(
                    page,
                    i + 1,
                    ce,
                    *kind,
                    transfer.as_deref(),
                    *bounds,
                    raster,
                    kernels,
                    masks,
                    stats,
                );
                soft.push(Some(mask));
                stats.soft_masks += 1;
                i = ce;
            }
            PreparedOp::PushSoftMaskNone => {
                soft.push(None);
                i += 1;
            }
            PreparedOp::PopSoftMask => {
                soft.pop();
                i += 1;
            }
        }
        if stats.cancelled {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_soft_mask(
    page: &CpuPreparedPage,
    content_start: usize,
    content_end: usize,
    kind: pdf_page_ir::MaskKind,
    transfer: Option<&[u8; 256]>,
    bounds: DeviceRect,
    raster: &mut RasterKernel,
    kernels: &KernelSet,
    masks: &mut Vec<Option<ClipMask>>,
    stats: &mut RenderStats,
) -> ClipMask {
    if bounds.width == 0 || bounds.height == 0 {
        // An empty real mask is not `/SMask /None`: its outside value still
        // gates every subsequent pixel. Keeping an explicit zero-sized mask
        // also fixes the formerly inverted Alpha/plain-Luminosity empty-mask
        // case, where `None` accidentally disabled masking altogether.
        return ClipMask {
            bounds,
            data: Vec::new(),
            outside: apply_transfer(soft_mask_outside(kind), transfer),
            all_opaque: false,
        };
    }

    let mut off = Surface::offscreen(bounds);
    // A /BC luminosity mask composites its content against the backdrop color,
    // not transparent black (§11.6.5.2).
    if let pdf_page_ir::MaskKind::LuminosityBc { backdrop } = kind {
        for ly in 0..off.height {
            let row = off.row_mut(off.origin_y + ly);
            for px in row.chunks_exact_mut(4) {
                px[0] = backdrop[0];
                px[1] = backdrop[1];
                px[2] = backdrop[2];
                px[3] = 255;
            }
        }
    }
    // The mask content renders isolated from any outer soft mask.
    let mut inner: Vec<Option<ClipMask>> = Vec::new();
    run_ops(
        page,
        content_start,
        content_end,
        &mut off,
        raster,
        kernels,
        masks,
        &mut inner,
        stats,
    );
    derive_soft_mask(&off, kind, transfer)
}

fn build_render_clip_mask(
    page: &CpuPreparedPage,
    raster: &mut RasterKernel,
    cid: u32,
    stats: &mut RenderStats,
) -> Option<ClipMask> {
    let mask = build_clip_mask_cancellable(
        raster,
        ClipGeom::of(page),
        cid,
        page.decode_limits.should_cancel.as_deref(),
    );
    if mask.is_none() {
        stats.cancelled = true;
    }
    mask
}

/// Execute a *knockout* group's ops `[start, end)` into `off` (ISO 32000-1
/// §11.6.6): every painting element composites against the group's **initial**
/// backdrop rather than the accumulated result, later elements replacing
/// earlier ones where they overlap.
///
/// Implementation: `initial` snapshots the offscreen; each top-level element
/// (a draw, or a whole nested group/tiled fill) renders onto a fresh copy of
/// `initial`, and every pixel the element changed is copied into the
/// accumulator (`off`). Diffing against `initial` detects "touched": an
/// element that writes exactly the backdrop value leaves the accumulator
/// unchanged, which is value-identical. Soft-mask push/pop ops are graphics
/// *state*, not elements — they execute once, in order, and stay active for
/// the following elements.
#[allow(clippy::too_many_arguments)]
fn run_knockout_group(
    page: &CpuPreparedPage,
    start: usize,
    end: usize,
    off: &mut Surface,
    raster: &mut RasterKernel,
    kernels: &KernelSet,
    masks: &mut Vec<Option<ClipMask>>,
    soft: &mut Vec<Option<ClipMask>>,
    stats: &mut RenderStats,
) {
    let initial = off.clone();
    let mut scratch = initial.clone();
    let mut scratch_is_initial = true;
    let mut i = start;
    while i < end {
        // One element's op extent, and whether it paints (vs pure state).
        let (next, paints) = match &page.ops[i] {
            PreparedOp::Draw(_)
            | PreparedOp::GlyphRun(_)
            | PreparedOp::TiledFill(_)
            | PreparedOp::Image(_) => (i + 1, true),
            PreparedOp::BeginGroup { end: ge, .. } => (*ge as usize + 1, true),
            PreparedOp::PushSoftMask { content_end, .. } => (*content_end as usize, false),
            PreparedOp::PushSoftMaskNone | PreparedOp::PopSoftMask | PreparedOp::EndGroup => {
                (i + 1, false)
            }
        };
        if paints {
            if !scratch_is_initial {
                scratch.copy_pixels_from(&initial);
            }
            run_ops(
                page,
                i,
                next,
                &mut scratch,
                raster,
                kernels,
                masks,
                soft,
                stats,
            );
            scratch_is_initial = false;
            // Fold changed pixels into the accumulator.
            let (gw, gh) = (off.width, off.height);
            for ly in 0..gh {
                let srow = scratch.local_row(ly);
                let irow = initial.local_row(ly);
                let orow = off.row_mut(initial.origin_y + ly);
                for lx in 0..gw {
                    let r = lx * 4..lx * 4 + 4;
                    if srow[r.clone()] != irow[r.clone()] {
                        orow[r.clone()].copy_from_slice(&srow[r]);
                    }
                }
            }
        } else {
            // State op: execute against the accumulator (it paints nothing).
            run_ops(page, i, next, off, raster, kernels, masks, soft, stats);
        }
        i = next;
    }
}

/// Blit an image: sample every device pixel in its bounds back through the
/// inverse transform, apply the clip + soft mask, and composite — source-over
/// on the fast paths (Normal blend), the general per-pixel compositor for a
/// non-Normal image blend.
#[allow(clippy::too_many_arguments)]
fn paint_image(
    page: &CpuPreparedPage,
    img: &crate::image::PreparedImage,
    surface: &mut Surface,
    raster: &mut RasterKernel,
    masks: &mut [Option<ClipMask>],
    soft: Option<&ClipMask>,
    stats: &mut RenderStats,
) {
    #[cfg(feature = "profiling")]
    let image_start = Instant::now();
    stats.commands += 1;
    let cmask_cid = if img.clip_has_mask {
        // Lowering guarantees a masked draw carries its clip id; a missing id
        // means the IR invariant broke upstream. Skip the draw (never panic a
        // page) and record the degradation.
        let Some(cid) = img.clip else {
            page.diagnostics
                .note_degraded("image draw skipped: masked clip without a clip id".into());
            return;
        };
        // Share the nearest path-clip ancestor's mask across rect descendants.
        let cid = page.clips[cid as usize].mask_source.unwrap_or(cid);
        if masks[cid as usize].is_none() {
            let Some(mask) = build_render_clip_mask(page, raster, cid, stats) else {
                return;
            };
            masks[cid as usize] = Some(mask);
        }
        Some(cid as usize)
    } else {
        None
    };
    let raw_cmask = cmask_cid.and_then(|cid| masks[cid].as_ref());
    // Path clips that rasterize to full coverage over their envelope are
    // semantically rectangular for this draw. Avoid a mask lookup and
    // multiply for every destination pixel once that is proven.
    let opaque_clip_elided = raw_cmask.is_some_and(|cm| cm.all_opaque);
    let cmask = raw_cmask.filter(|_| !opaque_clip_elided);

    let b = img.bounds;
    let (x0, y0) = (b.x as usize, b.y as usize);
    let x1 = (b.x + b.width as i32) as usize;
    let y1 = (b.y + b.height as i32) as usize;
    let ox = surface.origin_x;
    // Pixels this image actually marks. `covered_pixels` feeds
    // `is_silent_blank()`; an image-only page that DID paint must not be
    // mislabeled a silent blank just because no `SolidPath` ran.
    let mut covered: u64 = 0;
    #[cfg(feature = "profiling")]
    let mut sample_attempts: u64 = 0;
    #[cfg(feature = "profiling")]
    let mut sample_taps: u64 = 0;
    #[cfg(feature = "profiling")]
    let mut fast_cmyk_area_used = false;
    // Non-Normal image blend takes the generic per-pixel path with the
    // general compositor; every fast path below is source-over.
    let blend = choose_blend(img.blend);
    let blend_is_normal = matches!(blend, BlendChoice::Normal);
    // Eight-bit gray sources (book scans, MRC pages) are one byte per texel
    // where every fast path below reads three. Promote the axis-aligned
    // opaque ones to RGB8 once so they qualify; sampling the promoted image
    // is byte-identical (see `PreparedImage::gray8_promoted_to_rgb8`). A
    // magnified draw under a mask has no fast path either way, so it is not
    // expanded for nothing.
    let promoted_gray;
    let img = if blend_is_normal
        && cmask.is_none()
        && soft.is_none()
        && img.alpha == 255
        && img.inv.b == 0.0
        && img.inv.c == 0.0
        && matches!(img.color_space, pdf_page_ir::ImageColorSpace::Gray)
        && (img.footprint[0] > 1.0
            || img.footprint[1] > 1.0
            || (img.smask.is_none() && img.mask.is_none()))
    {
        match img.gray8_promoted_to_rgb8() {
            Some(rgb) => {
                promoted_gray = rgb;
                &promoted_gray
            }
            None => img,
        }
    } else {
        img
    };
    let fast_rgb8_base = blend_is_normal
        && cmask.is_none()
        && soft.is_none()
        && img.alpha == 255
        && img.smask.is_none()
        && img.mask.is_none()
        && !img.is_stencil
        && img.bpc == 8
        && img.decode.is_none()
        && matches!(img.color_space, pdf_page_ir::ImageColorSpace::Rgb)
        && img.footprint[0] <= 1.0
        && img.footprint[1] <= 1.0
        && img.inv.b.abs() < 1e-12
        && img.inv.c.abs() < 1e-12
        && img.samples.len() >= img.width as usize * img.height as usize * 3;
    let fast_rgb8 = fast_rgb8_base && img.interpolation == pdf_page_ir::InterpolationMode::Nearest;
    // Magnified/1:1 continuous-tone JPEG/JPX is forced to Bilinear in
    // prepared.rs; without a dedicated path those draws fall into the generic
    // per-pixel bilinear loop (~tens of Mpix/s). Same axis-aligned eligibility
    // as nearest, with bilinear taps matching `PreparedImage::bilinear`.
    let fast_rgb8_bilinear =
        fast_rgb8_base && img.interpolation == pdf_page_ir::InterpolationMode::Bilinear;
    // The area-minified twin of `fast_rgb8`: same eligibility, but the image is
    // minified on at least one axis (footprint > 1), so each destination pixel
    // box-averages its source footprint (what the generic path's
    // `area_average` does) instead of point-sampling. Off-diagonal inverse
    // terms must be *exactly* zero so per-column/row source boxes reproduce the
    // generic per-pixel `inv.apply` bit-for-bit. Interpolation is not checked:
    // minification ignores it (`shade` area-averages regardless of the
    // Nearest/Bilinear hint), so a minified Bilinear draw lands here too.
    let rgb8_area_shape = blend_is_normal
        && cmask.is_none()
        && soft.is_none()
        && img.alpha == 255
        && !img.is_stencil
        && img.bpc == 8
        && img.decode.is_none()
        && matches!(img.color_space, pdf_page_ir::ImageColorSpace::Rgb)
        && (img.footprint[0] > 1.0 || img.footprint[1] > 1.0)
        && img.inv.b == 0.0
        && img.inv.c == 0.0
        && img.samples.len() >= img.width as usize * img.height as usize * 3;
    let fast_rgb8_area = rgb8_area_shape && img.smask.is_none() && img.mask.is_none();
    // The MRC shape: the same opaque axis-aligned minified RGB8 source, but cut
    // out by a one-bit JBIG2 layer. Without this the draw falls into the
    // generic per-pixel loop and pays a fresh inverse map, two base tap boxes,
    // and a mask lookup for every destination pixel — all of which depend only
    // on the destination column or row once the placement is axis-aligned.
    let area_min_mask = if rgb8_area_shape {
        area_min_bilevel_mask(img)
    } else {
        None
    };
    // The same, for an opaque axis-aligned minified CMYK image: the source is
    // converted to RGB8 once and then box-averaged identically. Eligibility
    // mirrors `fast_rgb8_area` but for the CMYK colour space; the sample-length
    // and size checks live in `cmyk_source_as_rgb8`.
    let fast_cmyk_area = blend_is_normal
        && cmask.is_none()
        && soft.is_none()
        && img.alpha == 255
        && img.smask.is_none()
        && img.mask.is_none()
        && !img.is_stencil
        && img.bpc == 8
        && img.decode.is_none()
        && matches!(img.color_space, pdf_page_ir::ImageColorSpace::Cmyk)
        && (img.footprint[0] > 1.0 || img.footprint[1] > 1.0)
        && img.inv.b == 0.0
        && img.inv.c == 0.0;
    let fast_binary = blend_is_normal && soft.is_none() && img.is_binary_box_filterable();

    if fast_rgb8 {
        let result = paint_axis_aligned_rgb8_nearest_opaque(img, surface, x0, y0, x1, y1);
        covered = result.0;
        #[cfg(feature = "profiling")]
        {
            sample_attempts = (x1 - x0) as u64 * (y1 - y0) as u64;
            sample_taps = result.1;
        }
    } else if fast_rgb8_bilinear {
        let result = paint_axis_aligned_rgb8_bilinear_opaque(img, surface, x0, y0, x1, y1);
        covered = result.0;
        #[cfg(feature = "profiling")]
        {
            sample_attempts = (x1 - x0) as u64 * (y1 - y0) as u64;
            sample_taps = result.1;
        }
    } else if fast_rgb8_area {
        let result = paint_axis_aligned_rgb8_area_min_opaque(img, surface, x0, y0, x1, y1);
        covered = result.0;
        #[cfg(feature = "profiling")]
        {
            sample_attempts = (x1 - x0) as u64 * (y1 - y0) as u64;
            sample_taps = result.1;
        }
    } else if let Some(mask) = area_min_mask {
        let result = paint_axis_aligned_rgb8_area_min_masked(img, mask, surface, x0, y0, x1, y1);
        covered = result.0;
        #[cfg(feature = "profiling")]
        {
            sample_attempts = (x1 - x0) as u64 * (y1 - y0) as u64;
            sample_taps = result.1;
        }
    } else if let Some(result) = fast_cmyk_area
        .then(|| paint_axis_aligned_cmyk_area_min_opaque(img, surface, x0, y0, x1, y1))
        .flatten()
    {
        covered = result.0;
        #[cfg(feature = "profiling")]
        {
            fast_cmyk_area_used = true;
            sample_attempts = (x1 - x0) as u64 * (y1 - y0) as u64;
            sample_taps = result.1;
        }
    } else if fast_binary {
        let result = paint_axis_aligned_binary_box(img, surface, cmask, x0, y0, x1, y1);
        covered = result.0;
        #[cfg(feature = "profiling")]
        {
            sample_attempts = result.1;
            sample_taps = result.2;
        }
    } else {
        // Mask geometry is fixed for the whole draw; resolve it once.
        let clip_win = cmask.map(ClipMask::clip_window);
        let soft_win = soft.map(ClipMask::soft_window);
        // One destination row. Every term is a pure function of the source,
        // the masks and this pixel's own device coordinate, and the only write
        // is into this row, so the rows are independent and the per-pixel
        // arithmetic does not depend on how they are scheduled. Returns
        // `(covered, sample_attempts, sample_taps)`; the last two stay zero
        // unless the profiling counters are compiled in.
        let paint_row = |y: usize, row: &mut [u8]| -> (u64, u64, u64) {
            let mut covered = 0u64;
            // Profiling-only accumulators: unread, and never assigned, in an
            // ordinary build.
            #[allow(unused_mut)]
            let mut sample_attempts = 0u64;
            #[allow(unused_mut)]
            let mut sample_taps = 0u64;
            for x in x0..x1 {
                // Clip mask coverage.
                let mut cov: u16 = 255;
                if let Some(cw) = &clip_win {
                    cov = cw.coverage(x, y);
                }
                if let Some(sw) = &soft_win {
                    cov = mul_div_255(cov, sw.coverage(x, y));
                }
                if cov == 0 {
                    continue;
                }
                // A10 image-edge anti-aliasing: pixels straddling the image's
                // device-space quad edge paint at their fractional coverage
                // (`None` = provably interior, full weight — byte-identical
                // to the pre-A10 path there). Matches PDFium's softer
                // Type 3 / stencil edges (workstream H).
                let (dx, dy) = (x as f64 + 0.5, y as f64 + 0.5);
                let edge = img.edge_coverage(dx, dy);
                if edge == Some(0) {
                    continue;
                }
                #[cfg(feature = "profiling")]
                {
                    sample_attempts += 1;
                }
                let color = match edge {
                    None => {
                        #[cfg(feature = "profiling")]
                        {
                            let (color, taps) = img.shade_profiled(dx, dy);
                            sample_taps += taps;
                            color
                        }
                        #[cfg(not(feature = "profiling"))]
                        img.shade(dx, dy)
                    }
                    // Edge sliver: sample at the nearest in-quad point.
                    Some(_) => img.shade_clamped(dx, dy),
                };
                let Some(color) = color else {
                    continue;
                };
                let cov = match edge {
                    Some(ec) => mul_div_255(cov, ec),
                    None => cov,
                };
                if cov == 0 {
                    continue;
                }
                let a = mul_div_255(mul_div_255(color[3] as u16, img.alpha as u16), cov);
                if a == 0 {
                    continue;
                }
                let px = &mut row[(x - ox) * 4..(x - ox) * 4 + 4];
                composite_px_blended(px, [color[0], color[1], color[2]], a as u8, blend);
                covered += 1;
            }
            (covered, sample_attempts, sample_taps)
        };

        let (buf, first_abs_y, row_stride) = surface.rows_mut_abs(y0, y1);
        let parallel = image_row_parallel_enabled()
            && (x1 - x0).saturating_mul(y1 - y0) >= IMAGE_ROW_PAR_PIXEL_THRESHOLD;
        let totals = if parallel {
            buf.par_chunks_mut(row_stride)
                .enumerate()
                .map(|(i, row)| paint_row(first_abs_y + i, row))
                .reduce(|| (0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2))
        } else {
            buf.chunks_exact_mut(row_stride)
                .enumerate()
                .map(|(i, row)| paint_row(first_abs_y + i, row))
                .fold((0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2))
        };
        covered += totals.0;
        #[cfg(feature = "profiling")]
        {
            sample_attempts += totals.1;
            sample_taps += totals.2;
        }
        #[cfg(not(feature = "profiling"))]
        let _ = (totals.1, totals.2);
    }
    stats.covered_pixels += covered;
    stats.ops_painted += 1;
    #[cfg(feature = "profiling")]
    {
        let profile = &mut stats.profile;
        profile.add_duration("render.image", image_start.elapsed());
        profile.increment("image.draws", 1);
        profile.increment(
            "image.destination_pixels",
            (x1 - x0) as u64 * (y1 - y0) as u64,
        );
        profile.increment("image.sample_attempts", sample_attempts);
        profile.increment("image.source_sample_taps", sample_taps);
        profile.increment("image.painted_pixels", covered);
        if fast_rgb8 {
            profile.increment("image.fast_rgb8_nearest_pixels", covered);
        }
        if fast_rgb8_bilinear {
            profile.increment("image.fast_rgb8_bilinear_pixels", covered);
        }
        if fast_rgb8_area {
            profile.increment("image.fast_rgb8_area_min_pixels", covered);
        }
        if area_min_mask.is_some() {
            profile.increment("image.fast_rgb8_area_min_masked_pixels", covered);
        }
        if fast_cmyk_area_used {
            profile.increment("image.fast_cmyk_area_min_pixels", covered);
        }
        if fast_binary {
            profile.increment("image.fast_binary_box_pixels", covered);
        }
        if opaque_clip_elided {
            profile.increment("image.opaque_clip_elisions", 1);
        }
        if img.footprint[0] > 1.0 || img.footprint[1] > 1.0 {
            profile.increment("image.minified_draws", 1);
            profile.increment("image.area_average_pixels", sample_attempts);
        } else {
            profile.increment("image.magnified_or_1to1_draws", 1);
            match img.interpolation {
                pdf_page_ir::InterpolationMode::Nearest => {
                    profile.increment("image.nearest_pixels", sample_attempts);
                }
                pdf_page_ir::InterpolationMode::Bilinear => {
                    profile.increment("image.bilinear_pixels", sample_attempts);
                }
            }
        }
        match &img.color_space {
            pdf_page_ir::ImageColorSpace::Gray => profile.increment("image.gray_draws", 1),
            pdf_page_ir::ImageColorSpace::Rgb => profile.increment("image.rgb_draws", 1),
            pdf_page_ir::ImageColorSpace::Cmyk => profile.increment("image.cmyk_draws", 1),
            pdf_page_ir::ImageColorSpace::Indexed { .. } => {
                profile.increment("image.indexed_draws", 1)
            }
            pdf_page_ir::ImageColorSpace::TintLut { .. }
            | pdf_page_ir::ImageColorSpace::TintLut2 { .. } => {
                profile.increment("image.tint_lut_draws", 1)
            }
            pdf_page_ir::ImageColorSpace::Lab { .. } => profile.increment("image.lab_draws", 1),
            pdf_page_ir::ImageColorSpace::IccRgb { .. } => {
                profile.increment("image.icc_rgb_draws", 1)
            }
            pdf_page_ir::ImageColorSpace::IccCmyk { .. } => {
                profile.increment("image.icc_cmyk_draws", 1)
            }
        }
        if img.is_stencil {
            profile.increment("image.stencil_draws", 1);
        }
        if img.smask.is_some() || img.mask.is_some() {
            profile.increment("image.resource_masked_draws", 1);
        }
        if img.inv.b.abs() < 1e-12 && img.inv.c.abs() < 1e-12 {
            profile.increment("image.axis_aligned_draws", 1);
        } else {
            profile.increment("image.general_affine_draws", 1);
        }
        if img.clip_has_mask {
            profile.increment("image.clip_masked_draws", 1);
        }
        if soft.is_some() {
            profile.increment("image.soft_masked_draws", 1);
        }
    }
}

/// Fast path for an axis-aligned minified one-bit image. Device X and Y map
/// independently into source space, so prepare the inclusive source box for
/// each destination column/row once instead of repeating affine and footprint
/// math for every pixel.
fn paint_axis_aligned_binary_box(
    img: &crate::image::PreparedImage,
    surface: &mut Surface,
    cmask: Option<&ClipMask>,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> (u64, u64, u64) {
    let mut source_columns: Vec<Option<crate::image::AxisTaps>> = Vec::with_capacity(x1 - x0);
    for x in x0..x1 {
        let u = img.inv.a * (x as f64 + 0.5) + img.inv.e;
        if !(0.0..1.0).contains(&u) {
            source_columns.push(None);
            continue;
        }
        let fx = u * img.width as f64 - 0.5;
        source_columns.push(img.box_taps_x(fx));
    }

    let mut source_rows: Vec<Option<crate::image::AxisTaps>> = Vec::with_capacity(y1 - y0);
    for y in y0..y1 {
        let v = img.inv.d * (y as f64 + 0.5) + img.inv.f;
        if !(0.0..1.0).contains(&v) {
            source_rows.push(None);
            continue;
        }
        let fy = (1.0 - v) * img.height as f64 - 0.5;
        source_rows.push(img.box_taps_y(fy));
    }

    let output_origin = surface.origin_x;

    // Prefer the summed-area (integral image) box filter: build a running
    // ones-count table over the referenced source sub-rectangle once, then each
    // destination box's population count is four table reads instead of a packed
    // popcount loop. Pure reordering of the same integer arithmetic — the `ones`
    // count, box area `n`, two-entry mix, and source-over math are identical to
    // `binary_box_average`, so the output is byte-for-byte unchanged.
    if let Some(lut) = img.binary_box_lut()
        && let Some(sat) = BilevelIntegral::build(img, &source_columns, &source_rows)
    {
        return paint_binary_box_sat(
            img,
            lut,
            surface,
            cmask,
            &source_columns,
            &source_rows,
            &sat,
            x0,
            y0,
            output_origin,
        );
    }

    let mut painted = 0u64;
    let mut attempts = 0u64;
    let mut taps = 0u64;
    // Fixed for the whole draw; resolve it once rather than per pixel.
    let clip_win = cmask.map(ClipMask::clip_window);
    for (local_y, row_taps) in source_rows.iter().enumerate() {
        let y = y0 + local_y;
        let row = surface.row_mut(y);
        for (local_x, col_taps) in source_columns.iter().enumerate() {
            let x = x0 + local_x;
            let cov = clip_win.as_ref().map_or(255, |cw| cw.coverage(x, y));
            if cov == 0 {
                continue;
            }
            attempts += 1;
            let (Some(tx), Some(ty)) = (col_taps, row_taps) else {
                continue;
            };
            let Some((color, source_taps)) = img.binary_box_average(tx, ty) else {
                continue;
            };
            taps += source_taps;
            let a = mul_div_255(mul_div_255(color[3] as u16, img.alpha as u16), cov);
            if a == 0 {
                continue;
            }
            let target = (x - output_origin) * 4;
            if a == 255 {
                row[target] = color[0];
                row[target + 1] = color[1];
                row[target + 2] = color[2];
                row[target + 3] = 255;
            } else {
                let ia = 255 - a;
                row[target] =
                    (mul_div_255(color[0] as u16, a) + mul_div_255(row[target] as u16, ia)) as u8;
                row[target + 1] = (mul_div_255(color[1] as u16, a)
                    + mul_div_255(row[target + 1] as u16, ia))
                    as u8;
                row[target + 2] = (mul_div_255(color[2] as u16, a)
                    + mul_div_255(row[target + 2] as u16, ia))
                    as u8;
                row[target + 3] = (a + mul_div_255(row[target + 3] as u16, ia)) as u8;
            }
            painted += 1;
        }
    }
    (painted, attempts, taps)
}

/// A summed-area table of set bits over the referenced source sub-rectangle of a
/// prepared one-bit image. `sat[(ay+1)*stride + (ax+1)]` holds the number of set
/// bits in the local sub-rectangle rows `0..=ay`, columns `0..=ax`; the first
/// row and column are zero, so any inclusive box `[ax0..=ax1] × [ay0..=ay1]`
/// counts its ones in four reads.
struct BilevelIntegral {
    sat: Vec<u32>,
    stride: usize,
    col_lo: u32,
    row_lo: u32,
}

impl BilevelIntegral {
    /// Fill `sat` with the same table the serial walk in [`Self::build`]
    /// produces, in three passes: every block of rows sums from zero in
    /// parallel; the block-boundary carries are chained once (a handful of
    /// rows); every block but the first then adds its carry back, in parallel.
    /// Each cell ends as the same sum of the same `u32` terms, so the table is
    /// bit-identical to the serial one.
    #[allow(clippy::too_many_arguments)]
    fn fill_parallel(
        sat: &mut [u32],
        samples: &[u8],
        stride: usize,
        sw: usize,
        sh: usize,
        row_bits: usize,
        col_lo: u32,
        row_lo: u32,
    ) {
        // Row 0 of the table is the all-zero sentinel and is never written.
        let body = &mut sat[stride..];
        let threads = rayon::current_num_threads().max(1);
        // Two blocks per thread keeps the tail balanced without making the
        // serial carry chain (one row of `stride` adds per block) matter.
        let block_rows = sh.div_ceil(threads * 2).max(1);
        let chunk = block_rows * stride;

        body.par_chunks_mut(chunk).enumerate().for_each(|(b, blk)| {
            let first_ly = b * block_rows;
            let rows = blk.len() / stride;
            for i in 0..rows {
                let source_row = row_lo as usize + first_ly + i;
                let base_bit = source_row * row_bits + col_lo as usize;
                let (before, rest) = blk.split_at_mut(i * stride);
                let cur = &mut rest[..stride];
                let mut rowsum = 0u32;
                if i == 0 {
                    for (lx, bitpos) in (base_bit..base_bit + sw).enumerate() {
                        let byte = samples[bitpos >> 3];
                        rowsum += ((byte >> (7 - (bitpos & 7))) & 1) as u32;
                        cur[lx + 1] = rowsum;
                    }
                } else {
                    let prev = &before[(i - 1) * stride..i * stride];
                    for (lx, bitpos) in (base_bit..base_bit + sw).enumerate() {
                        let byte = samples[bitpos >> 3];
                        rowsum += ((byte >> (7 - (bitpos & 7))) & 1) as u32;
                        cur[lx + 1] = prev[lx + 1] + rowsum;
                    }
                }
            }
        });

        let blocks = body.len().div_ceil(chunk);
        if blocks < 2 {
            return;
        }
        // `carries[b]` is the running column total of every row before block
        // `b`; block 0 needs none. Only whole blocks are ever a predecessor,
        // so the last row of block `b - 1` is always at `b * block_rows - 1`.
        let mut carries = vec![0u32; blocks * stride];
        for b in 1..blocks {
            let last = (b * block_rows - 1) * stride;
            let (done, pending) = carries.split_at_mut(b * stride);
            let prev_carry = &done[(b - 1) * stride..];
            for ((dst, carry), tail) in pending[..stride]
                .iter_mut()
                .zip(prev_carry)
                .zip(&body[last..last + stride])
            {
                *dst = *carry + *tail;
            }
        }

        body.par_chunks_mut(chunk)
            .enumerate()
            .skip(1)
            .for_each(|(b, blk)| {
                let carry = &carries[b * stride..(b + 1) * stride];
                for row in blk.chunks_exact_mut(stride) {
                    for (cell, add) in row.iter_mut().zip(carry) {
                        *cell += *add;
                    }
                }
            });
    }

    /// Cap on the integral-image allocation. Above this the referenced source
    /// region is large enough that per-pixel popcount (whose total work is
    /// bounded by the box footprint, not the source area) is preferred, and the
    /// table is not built. 128 MiB of `u32` covers a ~180-megapixel scan.
    const MAX_ENTRIES: usize = 32 * 1024 * 1024;

    fn build(
        img: &crate::image::PreparedImage,
        source_columns: &[Option<crate::image::AxisTaps>],
        source_rows: &[Option<crate::image::AxisTaps>],
    ) -> Option<Self> {
        img.binary_box_lut()?;
        let (mut col_lo, mut col_hi, mut any_col) = (u32::MAX, 0u32, false);
        for taps in source_columns.iter().flatten() {
            col_lo = col_lo.min(taps.lo);
            col_hi = col_hi.max(taps.hi);
            any_col = true;
        }
        let (mut row_lo, mut row_hi, mut any_row) = (u32::MAX, 0u32, false);
        for taps in source_rows.iter().flatten() {
            row_lo = row_lo.min(taps.lo);
            row_hi = row_hi.max(taps.hi);
            any_row = true;
        }
        if !any_col || !any_row {
            return None;
        }
        let sw = (col_hi - col_lo + 1) as usize;
        let sh = (row_hi - row_lo + 1) as usize;
        let stride = sw + 1;
        let entries = stride.checked_mul(sh + 1)?;
        if entries > Self::MAX_ENTRIES {
            return None;
        }

        let row_bits = img.packed_row_bits();
        let samples: &[u8] = &img.samples;
        let mut sat = vec![0u32; entries];
        // The table is a running sum, so the rows form one dependency chain —
        // but a *block* of rows can be summed from zero independently and then
        // shifted by the running total that reaches it, and integer addition
        // is associative, so the finished table is bit-for-bit the same one
        // the serial walk below produces. That matters here because the table
        // is the draw's dominant cost: a full-page bilevel scan writes ~100 MB
        // of `u32`, most of it first-touch page faults, and those parallelize.
        if image_row_parallel_enabled() && entries >= BILEVEL_SAT_PAR_MIN_ENTRIES {
            Self::fill_parallel(&mut sat, samples, stride, sw, sh, row_bits, col_lo, row_lo);
        } else {
            for ly in 0..sh {
                let source_row = row_lo as usize + ly;
                let base_bit = source_row * row_bits + col_lo as usize;
                let (prev, cur) = sat.split_at_mut((ly + 1) * stride);
                let prev = &prev[ly * stride..ly * stride + stride];
                let cur = &mut cur[..stride];
                let mut rowsum = 0u32;
                for (lx, bitpos) in (base_bit..base_bit + sw).enumerate() {
                    let byte = samples[bitpos >> 3];
                    rowsum += ((byte >> (7 - (bitpos & 7))) & 1) as u32;
                    cur[lx + 1] = prev[lx + 1] + rowsum;
                }
            }
        }
        Some(Self {
            sat,
            stride,
            col_lo,
            row_lo,
        })
    }

    /// Set bits in the inclusive source box, in four table reads.
    #[inline]
    fn ones(&self, sx0: u32, sx1: u32, sy0: u32, sy1: u32) -> u32 {
        let ax0 = (sx0 - self.col_lo) as usize;
        let ax1 = (sx1 - self.col_lo + 1) as usize;
        let ay0 = (sy0 - self.row_lo) as usize;
        let ay1 = (sy1 - self.row_lo + 1) as usize;
        let s = self.stride;
        self.sat[ay1 * s + ax1] + self.sat[ay0 * s + ax0]
            - self.sat[ay0 * s + ax1]
            - self.sat[ay1 * s + ax0]
    }

    /// Fractionally-weighted set-bit count of the tap box, in `AXIS_TAP_SCALE²`
    /// units. Interior taps weigh a full `S²`; the edge rows/columns carry
    /// their overlap weight. Expansion of `Σ wy·wx·bit` with per-axis edge
    /// deficits `u = S − w`:
    ///
    /// `S²·box − S·Σ u_c·colstrip(c) − S·Σ u_r·rowstrip(r) + Σ u_c·u_r·corner`
    ///
    /// — every term a SAT box read, so the cost stays O(1) per pixel.
    fn weighted_ones(&self, cols: &crate::image::AxisTaps, rows: &crate::image::AxisTaps) -> u64 {
        const S: i64 = crate::image::AXIS_TAP_SCALE as i64;
        let box_ones = self.ones(cols.lo, cols.hi, rows.lo, rows.hi) as i64;
        let mut acc = S * S * box_ones;

        // Unique edge indices with their weight deficits (lo == hi collapses
        // to a single entry carrying the whole deficit).
        let col_edges: [(u32, i64); 2] = if cols.lo == cols.hi {
            [(cols.lo, S - cols.w_lo as i64), (cols.lo, 0)]
        } else {
            [
                (cols.lo, S - cols.w_lo as i64),
                (cols.hi, S - cols.w_hi as i64),
            ]
        };
        let row_edges: [(u32, i64); 2] = if rows.lo == rows.hi {
            [(rows.lo, S - rows.w_lo as i64), (rows.lo, 0)]
        } else {
            [
                (rows.lo, S - rows.w_lo as i64),
                (rows.hi, S - rows.w_hi as i64),
            ]
        };

        for &(c, u) in &col_edges {
            if u > 0 {
                acc -= S * u * self.ones(c, c, rows.lo, rows.hi) as i64;
            }
        }
        for &(r, u) in &row_edges {
            if u > 0 {
                acc -= S * u * self.ones(cols.lo, cols.hi, r, r) as i64;
            }
        }
        for &(c, uc) in &col_edges {
            if uc == 0 {
                continue;
            }
            for &(r, ur) in &row_edges {
                if ur == 0 {
                    continue;
                }
                acc += uc * ur * self.ones(c, c, r, r) as i64;
            }
        }
        acc.max(0) as u64
    }
}

/// Composite a prepared axis-aligned minified one-bit image using a summed-area
/// table for the per-box population count. The clip coverage and source-over
/// arithmetic are identical to the per-pixel path; only the ones-count and the
/// two-entry mix are reordered (memoized across identical `(n, ones)` runs,
/// which dominate bilevel scans' long white/black stretches).
#[allow(clippy::too_many_arguments)]
fn paint_binary_box_sat(
    img: &crate::image::PreparedImage,
    (zero, one): ([u8; 4], [u8; 4]),
    surface: &mut Surface,
    cmask: Option<&ClipMask>,
    source_columns: &[Option<crate::image::AxisTaps>],
    source_rows: &[Option<crate::image::AxisTaps>],
    sat: &BilevelIntegral,
    x0: usize,
    y0: usize,
    output_origin: usize,
) -> (u64, u64, u64) {
    let alpha = img.alpha as u16;
    // Fixed for the whole draw; resolve it once rather than per pixel.
    let clip_win = cmask.map(ClipMask::clip_window);

    // One destination row: it reads the shared summed-area table and the axis
    // tables, and writes only its own output row, so rows are independent.
    // The mix memo below is a pure function of `(weight, weighted ones)`, so
    // giving each row its own memo changes only how often the division is
    // repeated, never the colour it yields.
    let paint_row =
        |y: usize, row_taps: &Option<crate::image::AxisTaps>, row: &mut [u8]| -> (u64, u64, u64) {
            let mut painted = 0u64;
            let mut attempts = 0u64;
            let mut taps = 0u64;
            // All-zero / all-one boxes short-circuit without division; the mixed
            // boxes memoize on `(weight, weighted ones)` — long runs of identical
            // fractional boxes (repeated column patterns) then cost one division
            // set, not one per pixel. Clip coverage and destination blending stay
            // per pixel.
            let mut memo_key = (u64::MAX, u64::MAX);
            let mut memo_color = [0u8; 4];
            for (local_x, col_taps) in source_columns.iter().enumerate() {
                let x = x0 + local_x;
                let cov = clip_win.as_ref().map_or(255, |cw| cw.coverage(x, y));
                if cov == 0 {
                    continue;
                }
                attempts += 1;
                let (Some(tx), Some(ty)) = (col_taps, row_taps) else {
                    continue;
                };
                let weight = tx.total * ty.total;
                if weight == 0 {
                    continue;
                }
                taps += (tx.hi - tx.lo + 1) as u64 * (ty.hi - ty.lo + 1) as u64;
                let ones_w = sat.weighted_ones(tx, ty);
                let color = if ones_w == 0 {
                    zero
                } else if ones_w >= weight {
                    one
                } else if memo_key == (weight, ones_w) {
                    memo_color
                } else {
                    let c = crate::image::mix_bilevel(zero, one, ones_w, weight);
                    memo_key = (weight, ones_w);
                    memo_color = c;
                    c
                };
                let a = mul_div_255(mul_div_255(color[3] as u16, alpha), cov);
                if a == 0 {
                    continue;
                }
                let target = (x - output_origin) * 4;
                if a == 255 {
                    row[target] = color[0];
                    row[target + 1] = color[1];
                    row[target + 2] = color[2];
                    row[target + 3] = 255;
                } else {
                    let ia = 255 - a;
                    row[target] = (mul_div_255(color[0] as u16, a)
                        + mul_div_255(row[target] as u16, ia))
                        as u8;
                    row[target + 1] = (mul_div_255(color[1] as u16, a)
                        + mul_div_255(row[target + 1] as u16, ia))
                        as u8;
                    row[target + 2] = (mul_div_255(color[2] as u16, a)
                        + mul_div_255(row[target + 2] as u16, ia))
                        as u8;
                    row[target + 3] = (a + mul_div_255(row[target + 3] as u16, ia)) as u8;
                }
                painted += 1;
            }
            (painted, attempts, taps)
        };

    let dest_w = source_columns.len();
    let dest_h = source_rows.len();
    let (output, first_output_y, output_stride) = surface.rows_mut_abs(y0, y0 + dest_h);
    // Align the row table with the clipped output window.
    let row_offset = first_output_y.saturating_sub(y0);
    let row_slice = source_rows.get(row_offset..).unwrap_or(&[]);

    let parallel = image_row_parallel_enabled()
        && dest_w.saturating_mul(dest_h) >= IMAGE_ROW_PAR_PIXEL_THRESHOLD;
    if parallel {
        output
            .par_chunks_mut(output_stride)
            .zip(row_slice.par_iter())
            .enumerate()
            .map(|(i, (row, taps))| paint_row(first_output_y + i, taps, row))
            .reduce(|| (0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2))
    } else {
        output
            .chunks_exact_mut(output_stride)
            .zip(row_slice.iter())
            .enumerate()
            .map(|(i, (row, taps))| paint_row(first_output_y + i, taps, row))
            .fold((0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2))
    }
}

/// Fast path for the common scan/viewer case: an opaque, unmasked, axis-aligned
/// RGB8 image sampled nearest-neighbor. Source X indices depend only on device
/// X, so compute them once per draw; then copy bytes directly into the
/// premultiplied RGBA surface (opaque straight RGB equals premultiplied RGB).
fn paint_axis_aligned_rgb8_nearest_opaque(
    img: &crate::image::PreparedImage,
    surface: &mut Surface,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> (u64, u64) {
    const OUTSIDE: usize = usize::MAX;

    let mut source_columns = Vec::with_capacity(x1 - x0);
    for x in x0..x1 {
        let u = img.inv.a * (x as f64 + 0.5) + img.inv.e;
        if !(0.0..1.0).contains(&u) {
            source_columns.push(OUTSIDE);
            continue;
        }
        let fx = u * img.width as f64 - 0.5;
        source_columns
            .push((fx.round() as i64).clamp(0, img.width.saturating_sub(1) as i64) as usize);
    }

    let source_stride = img.width as usize * 3;
    let output_origin = surface.origin_x;
    let mut painted = 0u64;
    for y in y0..y1 {
        let v = img.inv.d * (y as f64 + 0.5) + img.inv.f;
        if !(0.0..1.0).contains(&v) {
            continue;
        }
        let fy = (1.0 - v) * img.height as f64 - 0.5;
        let source_row = (fy.round() as i64).clamp(0, img.height.saturating_sub(1) as i64) as usize;
        let source_base = source_row * source_stride;
        let row = surface.row_mut(y);
        for (local_x, &source_column) in source_columns.iter().enumerate() {
            if source_column == OUTSIDE {
                continue;
            }
            let source = source_base + source_column * 3;
            let target = (x0 + local_x - output_origin) * 4;
            row[target] = img.samples[source];
            row[target + 1] = img.samples[source + 1];
            row[target + 2] = img.samples[source + 2];
            row[target + 3] = 255;
            painted += 1;
        }
    }
    (painted, painted)
}

/// Precomputed bilinear sample coordinates for one destination column/row.
#[derive(Clone, Copy)]
struct BilinearAxis {
    /// Source texel floor index (clamped).
    i0: u32,
    /// Source texel ceil index (clamped).
    i1: u32,
    /// Fractional weight toward `i1` (matches `PreparedImage::bilinear`).
    t: f32,
    one_minus_t: f32,
}

/// Fast path for opaque, unmasked, axis-aligned RGB8 magnification / 1:1 with
/// bilinear sampling — the continuous-tone JPEG/JPX case.
///
/// Mirrors [`paint_axis_aligned_rgb8_nearest_opaque`]: source U depends only on
/// device X and V only on Y, so axis tables are prepared once. Each destination
/// pixel then does the same four-tap f32 lerp + truncate as
/// `PreparedImage::bilinear`, writing opaque premultiplied RGBA.
///
/// Rows are independent and painted with Rayon when the destination is large
/// enough that the parallel overhead is recovered. On x86_64, each row's
/// interior uses SSE (`f32x4`) to bilinear-filter four consecutive pixels at
/// a time with the same f32 op order + truncate-to-u8 as the scalar path.
#[allow(
    unsafe_code,
    reason = "dispatches the SSE2 row kernel behind a target check; see SAFETY comment"
)]
fn paint_axis_aligned_rgb8_bilinear_opaque(
    img: &crate::image::PreparedImage,
    surface: &mut Surface,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> (u64, u64) {
    let w = img.width;
    let h = img.height;
    let mut columns: Vec<Option<BilinearAxis>> = Vec::with_capacity(x1 - x0);
    for x in x0..x1 {
        let u = img.inv.a * (x as f64 + 0.5) + img.inv.e;
        if !(0.0..1.0).contains(&u) {
            columns.push(None);
            continue;
        }
        let fx = u * w as f64 - 0.5;
        let x_floor = fx.floor();
        let tx = (fx - x_floor) as f32;
        let ix0 = (x_floor as i64).clamp(0, (w as i64 - 1).max(0)) as u32;
        let ix1 = (x_floor as i64 + 1).clamp(0, (w as i64 - 1).max(0)) as u32;
        columns.push(Some(BilinearAxis {
            i0: ix0,
            i1: ix1,
            t: tx,
            one_minus_t: 1.0 - tx,
        }));
    }

    let mut rows: Vec<Option<BilinearAxis>> = Vec::with_capacity(y1 - y0);
    for y in y0..y1 {
        let v = img.inv.d * (y as f64 + 0.5) + img.inv.f;
        if !(0.0..1.0).contains(&v) {
            rows.push(None);
            continue;
        }
        let fy = (1.0 - v) * h as f64 - 0.5;
        let y_floor = fy.floor();
        let ty = (fy - y_floor) as f32;
        let iy0 = (y_floor as i64).clamp(0, (h as i64 - 1).max(0)) as u32;
        let iy1 = (y_floor as i64 + 1).clamp(0, (h as i64 - 1).max(0)) as u32;
        rows.push(Some(BilinearAxis {
            i0: iy0,
            i1: iy1,
            t: ty,
            one_minus_t: 1.0 - ty,
        }));
    }

    let source_stride = w as usize * 3;
    let samples = img.samples.as_ref();
    let output_origin = surface.origin_x;
    let dest_w = x1 - x0;
    let dest_h = y1 - y0;
    // Parallelize when the destination is large enough that row scheduling
    // pays off (JPEG scans at scale ≥1 are tens of megapixels).
    const PAR_PIXEL_THRESHOLD: usize = 256 * 256;

    let use_sse = bilinear_row_use_sse();
    let paint_row = |row_axis: &Option<BilinearAxis>, row: &mut [u8]| -> u64 {
        let Some(ry) = *row_axis else {
            return 0;
        };
        #[cfg(target_arch = "x86_64")]
        if use_sse {
            // SAFETY: SSE2 is required by x86_64 System V / Windows x64 ABIs;
            // `bilinear_row_use_sse` only returns true on those targets.
            return unsafe {
                paint_bilinear_row_sse2(
                    samples,
                    source_stride,
                    &columns,
                    ry,
                    row,
                    x0,
                    output_origin,
                )
            };
        }
        let _ = use_sse;
        paint_bilinear_row_scalar(samples, source_stride, &columns, ry, row, x0, output_origin)
    };

    let (buf, first_abs_y, stride) = surface.rows_mut_abs(y0, y1);
    // Align row table with the clipped buffer window.
    let row_offset = first_abs_y.saturating_sub(y0);
    let row_slice = if row_offset < rows.len() {
        &rows[row_offset..]
    } else {
        &[]
    };

    let painted =
        if image_row_parallel_enabled() && dest_w.saturating_mul(dest_h) >= PAR_PIXEL_THRESHOLD {
            buf.par_chunks_mut(stride)
                .zip(row_slice.par_iter())
                .map(|(row, axis)| paint_row(axis, row))
                .sum()
        } else {
            buf.chunks_exact_mut(stride)
                .zip(row_slice.iter())
                .map(|(row, axis)| paint_row(axis, row))
                .sum()
        };

    // Four source taps per painted destination pixel (edge clamps may alias).
    (painted, painted.saturating_mul(4))
}

#[inline]
fn bilinear_row_use_sse() -> bool {
    // x86_64 always has SSE2; the kernel uses only SSE2 ops (`f32x4` mul/add
    // + truncate convert), so no runtime feature probe is required.
    cfg!(target_arch = "x86_64")
}

/// Scalar bilinear row: four-tap f32 lerp + truncate, one dest pixel at a time.
#[inline]
fn paint_bilinear_row_scalar(
    samples: &[u8],
    source_stride: usize,
    columns: &[Option<BilinearAxis>],
    ry: BilinearAxis,
    row: &mut [u8],
    x0: usize,
    output_origin: usize,
) -> u64 {
    let base0 = ry.i0 as usize * source_stride;
    let base1 = ry.i1 as usize * source_stride;
    let mut painted = 0u64;
    for (local_x, col) in columns.iter().enumerate() {
        let Some(rx) = *col else {
            continue;
        };
        let rgb = bilinear_rgb8_pixel(samples, base0, base1, rx, ry);
        let target = (x0 + local_x - output_origin) * 4;
        row[target] = rgb[0];
        row[target + 1] = rgb[1];
        row[target + 2] = rgb[2];
        row[target + 3] = 255;
        painted += 1;
    }
    painted
}

/// One opaque RGB bilinear sample. Op order matches `PreparedImage::bilinear`
/// (horizontal then vertical, f32, truncate toward zero).
#[inline(always)]
fn bilinear_rgb8_pixel(
    samples: &[u8],
    base0: usize,
    base1: usize,
    rx: BilinearAxis,
    ry: BilinearAxis,
) -> [u8; 3] {
    let s00 = base0 + rx.i0 as usize * 3;
    let s10 = base0 + rx.i1 as usize * 3;
    let s01 = base1 + rx.i0 as usize * 3;
    let s11 = base1 + rx.i1 as usize * 3;
    let mut rgb = [0u8; 3];
    for ch in 0..3 {
        let top = samples[s00 + ch] as f32 * rx.one_minus_t + samples[s10 + ch] as f32 * rx.t;
        let bot = samples[s01 + ch] as f32 * rx.one_minus_t + samples[s11 + ch] as f32 * rx.t;
        rgb[ch] = (top * ry.one_minus_t + bot * ry.t).clamp(0.0, 255.0) as u8;
    }
    rgb
}

/// SSE2 row painter: groups of four consecutive *present* columns share one
/// vertical weight pair and run channel lerps as `f32x4`. Gaps / leftovers use
/// the scalar path. Byte-identical to [`paint_bilinear_row_scalar`].
///
/// # Safety
/// Caller must ensure this is an x86_64 target (SSE2 is part of the baseline
/// ABI). Sample indices derived from `columns`/`ry` must lie in `samples`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(
    unsafe_code,
    reason = "SSE2 intrinsics and one unaligned store; see the Safety section"
)]
unsafe fn paint_bilinear_row_sse2(
    samples: &[u8],
    source_stride: usize,
    columns: &[Option<BilinearAxis>],
    ry: BilinearAxis,
    row: &mut [u8],
    x0: usize,
    output_origin: usize,
) -> u64 {
    use std::arch::x86_64::{
        _mm_add_ps, _mm_cvttps_epi32, _mm_mul_ps, _mm_set_ps, _mm_set1_ps, _mm_storeu_si128,
    };

    let base0 = ry.i0 as usize * source_stride;
    let base1 = ry.i1 as usize * source_stride;
    let omty = _mm_set1_ps(ry.one_minus_t);
    let tyv = _mm_set1_ps(ry.t);
    let mut painted = 0u64;
    let n = columns.len();
    let mut local_x = 0usize;

    while local_x + 4 <= n {
        let c0 = columns[local_x];
        let c1 = columns[local_x + 1];
        let c2 = columns[local_x + 2];
        let c3 = columns[local_x + 3];
        if let (Some(rx0), Some(rx1), Some(rx2), Some(rx3)) = (c0, c1, c2, c3) {
            let omtx = _mm_set_ps(
                rx3.one_minus_t,
                rx2.one_minus_t,
                rx1.one_minus_t,
                rx0.one_minus_t,
            );
            let tx = _mm_set_ps(rx3.t, rx2.t, rx1.t, rx0.t);
            let mut out_r = [0u8; 4];
            let mut out_g = [0u8; 4];
            let mut out_b = [0u8; 4];
            for ch in 0..3 {
                let mut p00 = [0f32; 4];
                let mut p10 = [0f32; 4];
                let mut p01 = [0f32; 4];
                let mut p11 = [0f32; 4];
                for (lane, rx) in [rx0, rx1, rx2, rx3].into_iter().enumerate() {
                    let s00 = base0 + rx.i0 as usize * 3 + ch;
                    let s10 = base0 + rx.i1 as usize * 3 + ch;
                    let s01 = base1 + rx.i0 as usize * 3 + ch;
                    let s11 = base1 + rx.i1 as usize * 3 + ch;
                    p00[lane] = samples[s00] as f32;
                    p10[lane] = samples[s10] as f32;
                    p01[lane] = samples[s01] as f32;
                    p11[lane] = samples[s11] as f32;
                }
                // `_mm_set_ps` is high→low: lane3..lane0.
                let v00 = _mm_set_ps(p00[3], p00[2], p00[1], p00[0]);
                let v10 = _mm_set_ps(p10[3], p10[2], p10[1], p10[0]);
                let v01 = _mm_set_ps(p01[3], p01[2], p01[1], p01[0]);
                let v11 = _mm_set_ps(p11[3], p11[2], p11[1], p11[0]);
                // Same op order as scalar: top/bot horizontal, then vertical.
                let top = _mm_add_ps(_mm_mul_ps(v00, omtx), _mm_mul_ps(v10, tx));
                let bot = _mm_add_ps(_mm_mul_ps(v01, omtx), _mm_mul_ps(v11, tx));
                let out = _mm_add_ps(_mm_mul_ps(top, omty), _mm_mul_ps(bot, tyv));
                // Truncate toward zero (matches `as u8` for values in [0, 255]).
                let ti = _mm_cvttps_epi32(out);
                let mut tmp = [0i32; 4];
                // SAFETY: `tmp` has room for 4×i32; storeu needs no alignment.
                unsafe {
                    _mm_storeu_si128(tmp.as_mut_ptr().cast(), ti);
                }
                let dest = match ch {
                    0 => &mut out_r,
                    1 => &mut out_g,
                    _ => &mut out_b,
                };
                for lane in 0..4 {
                    dest[lane] = tmp[lane].clamp(0, 255) as u8;
                }
            }
            for lane in 0..4 {
                let target = (x0 + local_x + lane - output_origin) * 4;
                row[target] = out_r[lane];
                row[target + 1] = out_g[lane];
                row[target + 2] = out_b[lane];
                row[target + 3] = 255;
            }
            painted += 4;
            local_x += 4;
            continue;
        }
        if let Some(rx) = c0 {
            let rgb = bilinear_rgb8_pixel(samples, base0, base1, rx, ry);
            let target = (x0 + local_x - output_origin) * 4;
            row[target] = rgb[0];
            row[target + 1] = rgb[1];
            row[target + 2] = rgb[2];
            row[target + 3] = 255;
            painted += 1;
        }
        local_x += 1;
    }
    while local_x < n {
        if let Some(rx) = columns[local_x] {
            let rgb = bilinear_rgb8_pixel(samples, base0, base1, rx, ry);
            let target = (x0 + local_x - output_origin) * 4;
            row[target] = rgb[0];
            row[target + 1] = rgb[1];
            row[target + 2] = rgb[2];
            row[target + 3] = 255;
            painted += 1;
        }
        local_x += 1;
    }
    painted
}

/// Fast path for an opaque, unmasked, axis-aligned RGB8 image being
/// area-minified. The minification twin of `paint_axis_aligned_rgb8_nearest_opaque`:
/// each destination pixel averages the inclusive source-texel box that
/// `image.rs::area_average` computes, then — the image being fully opaque — the
/// averaged RGB is written straight to the surface (opaque source-over is a
/// copy), skipping the generic path's per-pixel float shade dispatch, `/Decode`,
/// mask tests, `average_rgba`, and blend.
///
/// Source X boxes are prepared once per destination column and Y boxes once per
/// row. The dispatch gate requires exactly-zero off-diagonal inverse terms, so
/// `u = inv.a·dx + inv.e` and `v = inv.d·dy + inv.f` equal the generic path's
/// `inv.apply` bit-for-bit (adding `inv.c·dy = 0.0` / `inv.b·dx = 0.0` is the
/// identity on a finite float). The box bounds, the uniform-weight sum, and the
/// integer-truncating divide all mirror `area_average` exactly, so the output is
/// byte-for-byte identical to the generic path. Returns `(painted, taps)` where
/// `taps` is the summed box area, matching `image.source_sample_taps`.
fn paint_axis_aligned_rgb8_area_min_opaque(
    img: &crate::image::PreparedImage,
    surface: &mut Surface,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> (u64, u64) {
    // RGB8 source samples are already the exact output component representation.
    area_min_box_average_opaque(img, surface, &img.samples, x0, y0, x1, y1)
}

/// Area-minify an opaque axis-aligned CMYK image. The source is converted to
/// packed RGB8 **once** (each pixel through the identical `cmyk_to_rgb`/`to_u8`
/// as the generic `pixel()`), then box-averaged like the RGB8 path. This is
/// byte-identical to the generic convert-each-tap-then-average, but the
/// conversion is hoisted out of the per-destination-pixel loop. `None` (→ the
/// caller keeps the generic path) if the source is not convertible or too large.
fn paint_axis_aligned_cmyk_area_min_opaque(
    img: &crate::image::PreparedImage,
    surface: &mut Surface,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> Option<(u64, u64)> {
    let rgb = img.cmyk_source_as_rgb8()?;
    Some(area_min_box_average_opaque(
        img, surface, &rgb, x0, y0, x1, y1,
    ))
}

/// The bilevel cut-out a minified RGB8 draw carries. MRC producers write one of
/// two encodings: a grayscale `/SMask` coverage layer whose alpha is
/// box-filtered with the base image, or a hard `/Mask` stencil that is
/// point-sampled all-or-nothing. Both are JBIG2 in practice.
#[derive(Clone, Copy)]
enum AreaMinMask<'a> {
    Soft(&'a pdf_page_ir::ImageSMask),
    Stencil(&'a pdf_page_ir::ImageSMask),
}

/// Classify `img`'s mask for the axis-aligned area-minification fast path, or
/// `None` to keep the generic per-pixel path.
fn area_min_bilevel_mask(img: &crate::image::PreparedImage) -> Option<AreaMinMask<'_>> {
    if let Some(sm) = img.smask.as_deref() {
        // `sample_smask` only takes its packed-bit box-filter branch at 1 bpc
        // and only box-filters at all when the mask minifies on an axis;
        // point-sampled masks are not what this path reproduces.
        let footprint = img.smask_footprint(sm);
        let usable = sm.bits_per_component == 1
            && sm.width > 0
            && sm.height > 0
            && (footprint[0] > 1.0 || footprint[1] > 1.0);
        return usable.then_some(AreaMinMask::Soft(sm));
    }
    match img.mask.as_ref()? {
        // A colour-key mask tests the base image's own samples, not an
        // independent bitmap, so it does not separate per axis.
        pdf_page_ir::ImageMask::ColorKey(_) => None,
        pdf_page_ir::ImageMask::Stencil(sm) => {
            (sm.width > 0 && sm.height > 0).then_some(AreaMinMask::Stencil(sm))
        }
    }
}

/// The prepared mask lookup for one destination column or row.
#[derive(Clone, Copy)]
enum MaskAxis {
    /// Box-filter taps into a one-bit `/SMask`.
    Taps(crate::image::AxisTaps),
    /// The point-sampled texel index of a hard `/Mask` stencil.
    Texel(u32),
}

/// Area-minify an axis-aligned opaque RGB8 image cut out by a bilevel mask —
/// the MRC scanned-page shape: a JPX foreground over a JPX background, cut out
/// by a JBIG2 layer sharing the foreground's placement.
///
/// The generic per-pixel loop recomputes, for every destination pixel, the
/// inverse map, the base image's two box-filter tap ranges, and the mask's own
/// per-axis lookup. With `inv.b == inv.c == 0` every one of those depends only
/// on the destination column (X) or row (Y), so they are prepared once each
/// here. What remains per pixel — the weighted box average, the mask's
/// coverage or stencil test, the `/Decode` remap, and the source-over
/// composite — is the generic path's arithmetic term for term, so the painted
/// pixels are unchanged.
// Called once per draw, and large enough (two prepared axis passes, the box
// average, both mask encodings, and the generic border fallback) that inlining
// it into `paint_image` measurably degrades the *sibling* fast paths' codegen —
// the unmasked JPX scan lost 37 % of its raster time to the bloat before this.
#[inline(never)]
fn paint_axis_aligned_rgb8_area_min_masked(
    img: &crate::image::PreparedImage,
    mask: AreaMinMask<'_>,
    surface: &mut Surface,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> (u64, u64) {
    let stride = img.width as usize * 3;
    let rgb: &[u8] = &img.samples;
    // `edge_coverage` returns `None` — provably interior, full weight — exactly
    // when the mapped pixel square stays inside the unit square on both axes.
    // With `inv.b == inv.c == 0` that test separates, so the interior is a
    // rectangle of whole destination columns and rows: the prepared body runs
    // there, and the anti-aliased border band keeps the generic per-pixel
    // treatment. The draw therefore stays pixel-identical, edges included.
    let half_u = 0.5 * img.inv.a.abs();
    let half_v = 0.5 * img.inv.d.abs();
    let mask_footprint = match mask {
        AreaMinMask::Soft(sm) => img.smask_footprint(sm),
        AreaMinMask::Stencil(_) => [0.0, 0.0],
    };

    // Per destination column: whether it is interior in u, the base image's X
    // tap box, and the mask's X lookup.
    let mut columns: Vec<(bool, Option<(crate::image::AxisTaps, MaskAxis)>)> =
        Vec::with_capacity(x1 - x0);
    for x in x0..x1 {
        let u = img.inv.a * (x as f64 + 0.5) + img.inv.e;
        let interior = u >= half_u && u <= 1.0 - half_u;
        if !(0.0..1.0).contains(&u) {
            columns.push((interior, None));
            continue;
        }
        let base = img.box_taps_x(u * img.width as f64 - 0.5);
        let axis = match mask {
            AreaMinMask::Soft(sm) => crate::image::PreparedImage::smask_box_taps(
                u * sm.width as f64 - 0.5,
                mask_footprint[0],
                sm.width,
            )
            .map(MaskAxis::Taps),
            AreaMinMask::Stencil(sm) => Some(MaskAxis::Texel(crate::image::stencil_col(sm, u))),
        };
        columns.push((interior, base.zip(axis)));
    }

    // Per destination row, the same.
    let mut rows: Vec<(bool, Option<(crate::image::AxisTaps, MaskAxis)>)> =
        Vec::with_capacity(y1 - y0);
    for y in y0..y1 {
        let v = img.inv.d * (y as f64 + 0.5) + img.inv.f;
        let interior = v >= half_v && v <= 1.0 - half_v;
        if !(0.0..1.0).contains(&v) {
            rows.push((interior, None));
            continue;
        }
        let base = img.box_taps_y((1.0 - v) * img.height as f64 - 0.5);
        let axis = match mask {
            AreaMinMask::Soft(sm) => crate::image::PreparedImage::smask_box_taps(
                (1.0 - v) * sm.height as f64 - 0.5,
                mask_footprint[1],
                sm.height,
            )
            .map(MaskAxis::Taps),
            AreaMinMask::Stencil(sm) => Some(MaskAxis::Texel(crate::image::stencil_row(sm, v))),
        };
        rows.push((interior, base.zip(axis)));
    }

    let output_origin = surface.origin_x;
    let dest_w = x1 - x0;
    let dest_h = y1 - y0;
    let columns = columns.as_slice();

    // One destination row: reads only its own base/mask source rows and writes
    // only its own output row, so rows are independent and the per-pixel
    // arithmetic is untouched by how they are scheduled.
    let paint_row =
        |y: usize,
         &(row_interior, row_axes): &(bool, Option<(crate::image::AxisTaps, MaskAxis)>),
         row: &mut [u8]|
         -> (u64, u64) {
            let mut painted = 0u64;
            let mut taps = 0u64;
            for (local_x, &(col_interior, col_axes)) in columns.iter().enumerate() {
                let x = x0 + local_x;
                let px = &mut row[(x - output_origin) * 4..][..4];

                let (Some((tx, mx)), Some((ty, my)), true) =
                    (col_axes, row_axes, col_interior && row_interior)
                else {
                    // Border band, or a column/row whose center maps outside the
                    // image: the generic per-pixel treatment, minus the clip and
                    // soft-clip terms this path's eligibility already excluded.
                    let (dx, dy) = (x as f64 + 0.5, y as f64 + 0.5);
                    let edge = img.edge_coverage(dx, dy);
                    if edge == Some(0) {
                        continue;
                    }
                    let color = match edge {
                        None => img.shade(dx, dy),
                        Some(_) => img.shade_clamped(dx, dy),
                    };
                    let Some(color) = color else { continue };
                    let cov = match edge {
                        Some(ec) => mul_div_255(255, ec),
                        None => 255,
                    };
                    let a = mul_div_255(color[3] as u16, cov);
                    if a == 0 {
                        continue;
                    }
                    composite_px_blended(
                        px,
                        [color[0], color[1], color[2]],
                        a as u8,
                        BlendChoice::Normal,
                    );
                    painted += 1;
                    continue;
                };

                let weight = tx.total * ty.total;
                if weight == 0 {
                    continue;
                }
                taps += (tx.hi - tx.lo + 1) as u64 * (ty.hi - ty.lo + 1) as u64;

                // The mask first: a fully transparent destination pixel skips the
                // box average entirely, and on a scanned page the foreground layer
                // is transparent over most of the sheet.
                let alpha = match (mask, mx, my) {
                    (AreaMinMask::Soft(sm), MaskAxis::Taps(mtx), MaskAxis::Taps(mty)) => {
                        let coverage = crate::image::bilevel_smask_coverage(sm, &mtx, &mty);
                        (255.0 * crate::image::apply_smask_decode(sm, coverage)) as u8
                    }
                    (AreaMinMask::Stencil(sm), MaskAxis::Texel(col), MaskAxis::Texel(srow)) => {
                        if crate::image::stencil_hides_at(sm, col, srow) {
                            0
                        } else {
                            255
                        }
                    }
                    // The axis kinds are built from `mask` above, so the mixed
                    // combinations cannot occur.
                    _ => 0,
                };
                if alpha == 0 {
                    continue;
                }

                let (mut a0, mut a1, mut a2) = (0u64, 0u64, 0u64);
                for sr in ty.lo..=ty.hi {
                    let wy = ty.weight_at(sr);
                    let base = sr as usize * stride + tx.lo as usize * 3;
                    let end = sr as usize * stride + (tx.hi as usize + 1) * 3;
                    for (col, texel) in (tx.lo..).zip(rgb[base..end].chunks_exact(3)) {
                        let w = wy * tx.weight_at(col);
                        a0 += w * texel[0] as u64;
                        a1 += w * texel[1] as u64;
                        a2 += w * texel[2] as u64;
                    }
                }
                let color = [
                    ((a0 + weight / 2) / weight).min(255) as u8,
                    ((a1 + weight / 2) / weight).min(255) as u8,
                    ((a2 + weight / 2) / weight).min(255) as u8,
                ];

                composite_px_blended(px, color, alpha, BlendChoice::Normal);
                painted += 1;
            }
            (painted, taps)
        };

    let (output, first_output_y, output_stride) = surface.rows_mut_abs(y0, y1);
    // Align the row table with the clipped output window.
    let row_offset = first_output_y.saturating_sub(y0);
    let row_slice = rows.get(row_offset..).unwrap_or(&[]);

    let parallel = image_row_parallel_enabled()
        && dest_w.saturating_mul(dest_h) >= IMAGE_ROW_PAR_PIXEL_THRESHOLD;
    if parallel {
        output
            .par_chunks_mut(output_stride)
            .zip(row_slice.par_iter())
            .enumerate()
            .map(|(i, (row, axes))| paint_row(first_output_y + i, axes, row))
            .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    } else {
        output
            .chunks_exact_mut(output_stride)
            .zip(row_slice.iter())
            .enumerate()
            .map(|(i, (row, axes))| paint_row(first_output_y + i, axes, row))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    }
}

/// Core of the opaque axis-aligned area-minification fast paths. `rgb` is a
/// packed `width*height*3` RGB8 view of the source (the samples themselves for
/// an RGB image; a converted buffer for CMYK). Prepares per-column source X
/// boxes and per-row Y boxes, then box-averages each destination pixel exactly
/// as `image.rs::area_average` does and writes it opaque.
fn area_min_box_average_opaque(
    img: &crate::image::PreparedImage,
    surface: &mut Surface,
    rgb: &[u8],
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> (u64, u64) {
    let stride = img.width as usize * 3;

    let mut source_columns: Vec<Option<crate::image::AxisTaps>> = Vec::with_capacity(x1 - x0);
    for x in x0..x1 {
        let u = img.inv.a * (x as f64 + 0.5) + img.inv.e;
        if !(0.0..1.0).contains(&u) {
            source_columns.push(None);
            continue;
        }
        let fx = u * img.width as f64 - 0.5;
        source_columns.push(img.box_taps_x(fx));
    }

    let mut source_rows: Vec<Option<crate::image::AxisTaps>> = Vec::with_capacity(y1 - y0);
    for y in y0..y1 {
        let v = img.inv.d * (y as f64 + 0.5) + img.inv.f;
        if !(0.0..1.0).contains(&v) {
            source_rows.push(None);
            continue;
        }
        let fy = (1.0 - v) * img.height as f64 - 0.5;
        source_rows.push(img.box_taps_y(fy));
    }

    let output_origin = surface.origin_x;
    let dest_w = x1 - x0;
    let dest_h = y1 - y0;
    let columns = source_columns.as_slice();

    // One destination row: reads only its own source rows and writes only its
    // own output row, so rows are independent and the per-pixel arithmetic is
    // untouched by how they are scheduled.
    let paint_row = |row_taps: &Option<crate::image::AxisTaps>, row: &mut [u8]| -> (u64, u64) {
        let Some(ty) = *row_taps else {
            return (0, 0);
        };
        let mut painted = 0u64;
        let mut taps = 0u64;
        for (local_x, col_taps) in columns.iter().enumerate() {
            let Some(tx) = *col_taps else { continue };
            let weight = tx.total * ty.total;
            if weight == 0 {
                continue;
            }
            let (mut a0, mut a1, mut a2) = (0u64, 0u64, 0u64);
            for sr in ty.lo..=ty.hi {
                let wy = ty.weight_at(sr);
                let base = sr as usize * stride + tx.lo as usize * 3;
                let end = sr as usize * stride + (tx.hi as usize + 1) * 3;
                for (col, texel) in (tx.lo..).zip(rgb[base..end].chunks_exact(3)) {
                    let w = wy * tx.weight_at(col);
                    a0 += w * texel[0] as u64;
                    a1 += w * texel[1] as u64;
                    a2 += w * texel[2] as u64;
                }
            }
            let target = (x0 + local_x - output_origin) * 4;
            row[target] = ((a0 + weight / 2) / weight).min(255) as u8;
            row[target + 1] = ((a1 + weight / 2) / weight).min(255) as u8;
            row[target + 2] = ((a2 + weight / 2) / weight).min(255) as u8;
            row[target + 3] = 255;
            painted += 1;
            taps += (tx.hi - tx.lo + 1) as u64 * (ty.hi - ty.lo + 1) as u64;
        }
        (painted, taps)
    };

    let (output, first_output_y, output_stride) = surface.rows_mut_abs(y0, y1);
    // Align the row table with the clipped output window.
    let row_offset = first_output_y.saturating_sub(y0);
    let row_slice = source_rows.get(row_offset..).unwrap_or(&[]);

    // Same fan-out policy as the sibling bilinear fast path: below this many
    // destination pixels the rayon scheduling costs more than it saves.
    let parallel = image_row_parallel_enabled()
        && dest_w.saturating_mul(dest_h) >= IMAGE_ROW_PAR_PIXEL_THRESHOLD;
    let (painted, taps) = if parallel {
        output
            .par_chunks_mut(output_stride)
            .zip(row_slice.par_iter())
            .map(|(row, ty)| paint_row(ty, row))
            .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    } else {
        output
            .chunks_exact_mut(output_stride)
            .zip(row_slice.iter())
            .map(|(row, ty)| paint_row(ty, row))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    };
    (painted, taps)
}

/// Table size above which the bilevel summed-area table is filled across the
/// rayon pool. Below it the serial walk is already a fraction of a millisecond
/// and the fan-out would not pay for itself.
const BILEVEL_SAT_PAR_MIN_ENTRIES: usize = 1 << 20;

/// Destination-pixel count above which an image draw paints its independent
/// destination rows across the rayon pool. Mirrors the bilinear fast path's
/// own long-standing `PAR_PIXEL_THRESHOLD`.
const IMAGE_ROW_PAR_PIXEL_THRESHOLD: usize = 256 * 256;

/// Page executions running in this process right now. Intra-page row fan-out
/// only pays when the render has the machine to itself: under the document
/// scheduler every core is already carrying another page, and fanning out
/// there oversubscribes the box — measured at −31 % whole-document throughput
/// on a 640-page bilevel scan — for no extra work done.
static PAGES_IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// RAII counter for [`PAGES_IN_FLIGHT`], held for one top-level page execution.
struct PageInFlight;

impl PageInFlight {
    fn enter() -> Self {
        PAGES_IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

impl Drop for PageInFlight {
    fn drop(&mut self) {
        PAGES_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How the image row loops decide to fan out over the rayon pool.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowParPolicy {
    /// Always serial.
    Never,
    /// Fan out only while this is the sole page render in flight.
    Auto,
    /// Always fan out above the pixel threshold.
    Always,
}

/// `PDF_RENDERER_IMAGE_ROW_PAR` selects the policy: unset or `auto` is
/// load-aware, `off`/`0`/`false` forces the serial row loop and `on`/`1`/
/// `always` forces the fan-out. The override exists for paired A/B isolation
/// on one binary. Read once.
fn row_par_policy() -> RowParPolicy {
    use std::sync::OnceLock;
    static POLICY: OnceLock<RowParPolicy> = OnceLock::new();
    *POLICY.get_or_init(|| match std::env::var("PDF_RENDERER_IMAGE_ROW_PAR") {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" => RowParPolicy::Never,
            "on" | "1" | "true" | "always" => RowParPolicy::Always,
            _ => RowParPolicy::Auto,
        },
        Err(_) => RowParPolicy::Auto,
    })
}

/// Whether the image row loops should fan their destination rows out over the
/// rayon pool. Consulted once per draw, never per pixel.
fn image_row_parallel_enabled() -> bool {
    match row_par_policy() {
        RowParPolicy::Never => false,
        RowParPolicy::Always => true,
        RowParPolicy::Auto => PAGES_IN_FLIGHT.load(std::sync::atomic::Ordering::Relaxed) <= 1,
    }
}

/// Render a tiling-pattern fill: replicate the compiled cell across the
/// lattice into a fill-sized offscreen, then composite it onto `surface`
/// masked by the fill shape (advice §9-style bounded offscreen).
#[allow(clippy::too_many_arguments)]
fn render_tiling(
    page: &CpuPreparedPage,
    t: &crate::prepared::PreparedTiling,
    surface: &mut Surface,
    raster: &mut RasterKernel,
    kernels: &KernelSet,
    masks: &mut [Option<ClipMask>],
    stats: &mut RenderStats,
) {
    let b = t.bounds;
    let (w, h) = (b.width as usize, b.height as usize);
    let (bx, by) = (b.x as usize, b.y as usize);
    if w == 0 || h == 0 {
        return;
    }

    // Cell instances render into a fill-sized surface whose local origin is
    // the fill's device origin (so cell draws are clipped to the fill bounds).
    let mut pat = Surface::new(w, h, pdf_render_api::Background::Transparent);
    let size = pdf_page_ir::DeviceSize {
        width: b.width,
        height: b.height,
    };
    let shift = pdf_page_ir::Matrix::translate(-(b.x as f64), -(b.y as f64));
    // Tile-invariant state hoisted out of the instance loop (A3): one parsed-
    // font residency shared by every tile instance of this fill. Geometry
    // still lowers per instance — each tile's device transform differs by a
    // generally-fractional translation, and floating-point point transforms
    // are not translation-invariant, so a lower-once-translate-many cell
    // would not be byte-identical to per-instance lowering.
    let mut cell_fonts = crate::prepared::FontProgramCache::default();
    for tile in &t.tiles {
        // A cancelled request stops between tile instances too — an
        // adversarial many-tile fill must not pin the worker.
        if let Some(cancel) = &page.decode_limits.should_cancel
            && cancel()
        {
            stats.cancelled = true;
            return;
        }
        let cell_prepared = crate::prepared::lower_cell(
            &t.cell,
            tile.then(shift),
            size,
            page,
            t.color_policy,
            &mut cell_fonts,
        );
        let mut cmasks: Vec<Option<ClipMask>> =
            (0..cell_prepared.clips.len()).map(|_| None).collect();
        let mut csoft: Vec<Option<ClipMask>> = Vec::new();
        run_ops(
            &cell_prepared,
            0,
            cell_prepared.ops.len(),
            &mut pat,
            raster,
            kernels,
            &mut cmasks,
            &mut csoft,
            stats,
        );
        // A codec image dropped while lowering this tile cell counts too.
        stats.absorb_diagnostics(&cell_prepared.diagnostics);
    }

    // The fill shape as an Alpha8 coverage mask over the bounds.
    let subs = &page.subpaths[t.fill_subpaths.0 as usize..t.fill_subpaths.1 as usize];
    let mut fmask = vec![0u8; w * h];
    let dev_w = page.size.width as usize;
    let dev_h = page.size.height as usize;
    if !raster.fill_cancellable(
        &page.points,
        subs,
        dev_w,
        dev_h,
        t.fill_rule,
        page.decode_limits.should_cancel.as_deref(),
        |y, x0, x1, cov| {
            if y < by || y >= by + h {
                return;
            }
            let ly = y - by;
            let lx0 = x0.max(bx);
            let lx1 = x1.min(bx + w - 1);
            for x in lx0..=lx1 {
                fmask[ly * w + (x - bx)] = cov[x];
            }
        },
    ) {
        stats.cancelled = true;
        return;
    }

    // Intersect with the outer non-rectangular clip mask, if any.
    if t.clip_has_mask {
        // Lowering guarantees a masked draw carries its clip id; skip the
        // draw (never panic a page) if that IR invariant broke upstream.
        let Some(cid) = t.clip else {
            page.diagnostics
                .note_degraded("tiling fill skipped: masked clip without a clip id".into());
            return;
        };
        if masks[cid as usize].is_none() {
            let Some(mask) = build_render_clip_mask(page, raster, cid, stats) else {
                return;
            };
            masks[cid as usize] = Some(mask);
        }
        let Some(cm) = masks[cid as usize].as_ref() else {
            return; // unreachable: just built above
        };
        let mw = cm.stride();
        for ly in 0..h {
            let dy = by + ly;
            if dy < cm.bounds.y as usize || dy >= cm.bounds.y as usize + cm.bounds.height as usize {
                for lx in 0..w {
                    fmask[ly * w + lx] = 0;
                }
                continue;
            }
            let base = (dy - cm.bounds.y as usize) * mw;
            let cmx = cm.bounds.x as usize;
            let cmw = cm.bounds.width as usize;
            for lx in 0..w {
                let dx = bx + lx;
                let v = if dx >= cmx && dx < cmx + cmw {
                    cm.data[base + (dx - cmx)]
                } else {
                    0
                };
                fmask[ly * w + lx] = mul_div_255(fmask[ly * w + lx] as u16, v as u16) as u8;
            }
        }
    }

    // An /ImageMask stencil painted with this pattern: the pattern shows only
    // through the stencil's per-pixel coverage. Sampled with the same edge-AA
    // path as an ordinary stencil blit, then folded into the fill mask.
    if let Some(stencil) = &t.stencil {
        for ly in 0..h {
            for lx in 0..w {
                let idx = ly * w + lx;
                if fmask[idx] == 0 {
                    continue;
                }
                let (dx, dy) = ((bx + lx) as f64 + 0.5, (by + ly) as f64 + 0.5);
                let edge = stencil.edge_coverage(dx, dy);
                let scov = if edge == Some(0) {
                    0
                } else {
                    let color = match edge {
                        None => stencil.shade(dx, dy),
                        Some(_) => stencil.shade_clamped(dx, dy),
                    };
                    let base = color.map(|c| c[3] as u16).unwrap_or(0);
                    match edge {
                        Some(ec) => mul_div_255(base, ec),
                        None => base,
                    }
                };
                fmask[idx] = mul_div_255(fmask[idx] as u16, scov) as u8;
            }
        }
    }

    // Composite the tiled offscreen onto `surface`, masked by the fill shape
    // and scaled by the fill's constant alpha.
    let sox = surface.origin_x;
    // Composite blend: `Normal` keeps the exact integer source-over below; a
    // non-Normal pattern fill un-premultiplies the cell pixel and goes
    // through the general per-pixel compositor.
    let blend = choose_blend(t.blend);
    let blend_is_normal = matches!(blend, BlendChoice::Normal);
    for ly in 0..h {
        let dy = by + ly;
        // `pat` is this function's own offscreen (built above), distinct from
        // `surface`, so the row can be borrowed rather than copied.
        let prow = pat.local_row(ly);
        let srow = surface.row_mut(dy);
        for lx in 0..w {
            let cov = fmask[ly * w + lx];
            if cov == 0 {
                continue;
            }
            let src = &prow[lx * 4..lx * 4 + 4];
            if src[3] == 0 {
                continue;
            }
            let kk = mul_div_255(cov as u16, t.alpha as u16);
            let dst = &mut srow[(bx + lx - sox) * 4..(bx + lx - sox) * 4 + 4];
            if !blend_is_normal {
                // Straight source color + effective alpha for the compositor.
                let (rgb, sa) = if t.uncolored {
                    let stencil = src[3] as u16;
                    let a = mul_div_255(mul_div_255(t.under[3] as u16, stencil), kk);
                    ([t.under[0], t.under[1], t.under[2]], a as u8)
                } else {
                    let s3 = src[3] as u16;
                    let un = |ch: usize| ((src[ch] as u16 * 255 + s3 / 2) / s3).min(255) as u8;
                    ([un(0), un(1), un(2)], mul_div_255(s3, kk) as u8)
                };
                composite_px_blended(dst, rgb, sa, blend);
                continue;
            }
            // Effective premultiplied source: the cell pixel (colored) or the
            // under-color through the cell's alpha stencil (uncolored).
            let (spr, sa) = if t.uncolored {
                let stencil = src[3] as u16;
                let a = mul_div_255(mul_div_255(t.under[3] as u16, stencil), kk);
                let pm = |ch: usize| {
                    mul_div_255(
                        mul_div_255(mul_div_255(t.under[ch] as u16, t.under[3] as u16), stencil),
                        kk,
                    ) as u8
                };
                ([pm(0), pm(1), pm(2)], a as u8)
            } else {
                let pm = |ch: usize| mul_div_255(src[ch] as u16, kk) as u8;
                ([pm(0), pm(1), pm(2)], mul_div_255(src[3] as u16, kk) as u8)
            };
            if sa == 0 {
                continue;
            }
            let ia = 255 - sa as u16;
            for ch in 0..3 {
                dst[ch] = (spr[ch] as u16 + mul_div_255(dst[ch] as u16, ia)) as u8;
            }
            dst[3] = (sa as u16 + mul_div_255(dst[3] as u16, ia)) as u8;
        }
    }
    stats.commands += 1;
    stats.ops_painted += 1;
}

/// Evaluate a shading at device point `(dx, dy)`. Returns the straight RGBA
/// ramp color, or `None` when the point falls outside the shading's painted
/// extent (before the axis start / past the end without `/Extend`).
fn shade_pixel(sh: &crate::prepared::PreparedShading, dx: f64, dy: f64) -> Option<[u8; 4]> {
    use crate::prepared::ShadingSpanKind;
    // The shading's `/BBox` clip (§8.7.4.3): a device-space box outside which
    // nothing is painted, regardless of shading type or `/Extend`.
    if let Some(bb) = sh.bbox {
        let (x, y) = (dx as f32, dy as f32);
        if x < bb[0] || x > bb[2] || y < bb[1] || y > bb[3] {
            return None;
        }
    }
    // Pre-rasterized mesh layer: direct device-pixel lookup (alpha 0 = the
    // mesh does not paint here; the /Background, when honored, was baked in).
    if let ShadingSpanKind::Layer { x0, y0, w, h } = sh.kind {
        let lx = dx.floor() as i64 - x0 as i64;
        let ly = dy.floor() as i64 - y0 as i64;
        if lx < 0 || ly < 0 || lx >= w as i64 || ly >= h as i64 {
            return sh.background;
        }
        let c = sh.ramp[ly as usize * w + lx as usize];
        return if c[3] == 0 { None } else { Some(c) };
    }
    // Map the device point back into the shading's coordinate space.
    let p = sh.inv.apply(pdf_page_ir::Point { x: dx, y: dy });
    if let ShadingSpanKind::Grid { domain, gw, gh } = sh.kind {
        let dw = domain[1] - domain[0];
        let dh = domain[3] - domain[2];
        if dw.abs() < 1e-12 || dh.abs() < 1e-12 {
            return sh.background;
        }
        let s = (p.x - domain[0]) / dw;
        let t = (p.y - domain[2]) / dh;
        if !(0.0..=1.0).contains(&s) || !(0.0..=1.0).contains(&t) {
            return sh.background;
        }
        let gx = ((s * gw as f64) as usize).min(gw - 1);
        let gy = ((t * gh as f64) as usize).min(gh - 1);
        return Some(sh.ramp[gy * gw + gx]);
    }
    let s = match sh.kind {
        ShadingSpanKind::Axial { p0, d, dd } => ((p.x - p0[0]) * d[0] + (p.y - p0[1]) * d[1]) / dd,
        // Outside both circles with `/Extend` off: the `/Background`, if any.
        ShadingSpanKind::Radial { c0, c1 } => match radial_param(c0, c1, p.x, p.y, sh.extend) {
            Some(s) => s,
            None => return sh.background,
        },
        // Handled above.
        ShadingSpanKind::Grid { .. } | ShadingSpanKind::Layer { .. } => return None,
    };
    // Past the axis with `/Extend` off: paint the `/Background`, if any.
    let Some(s) = apply_extend(s, sh.extend) else {
        return sh.background;
    };
    let n = sh.ramp.len();
    if n == 0 {
        return None;
    }
    let idx = (s * (n - 1) as f64).round().clamp(0.0, (n - 1) as f64) as usize;
    Some(sh.ramp[idx])
}

/// Color a glyph-coverage span from a shading (a shading-pattern text fill):
/// each covered pixel takes the shading's color at its device center, with the
/// glyph coverage folded into the source alpha. `dst` and `cov` are aligned —
/// `cov[i]`'s device coordinate is `(x0 + i, y)`.
fn shade_glyph_span(
    dst: &mut [u8],
    cov: &[u8],
    sh: &crate::prepared::PreparedShading,
    x0: usize,
    y: usize,
    alpha: u8,
    blend: BlendChoice,
) {
    for (i, &c) in cov.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let Some(color) = shade_pixel(sh, (x0 + i) as f64 + 0.5, y as f64 + 0.5) else {
            continue;
        };
        let a = mul_div_255(c as u16, mul_div_255(color[3] as u16, alpha as u16)) as u8;
        if a == 0 {
            continue;
        }
        composite_px_blended(
            &mut dst[i * 4..i * 4 + 4],
            [color[0], color[1], color[2]],
            a,
            blend,
        );
    }
}

/// Clamp a shading parameter to `[0, 1]` honoring the extend flags; returns
/// `None` when the parameter is out of range and extension is disabled.
#[inline]
fn apply_extend(s: f64, extend: [bool; 2]) -> Option<f64> {
    if s < 0.0 {
        extend[0].then_some(0.0)
    } else if s > 1.0 {
        extend[1].then_some(1.0)
    } else {
        Some(s)
    }
}

/// Solve the radial-shading parameter at point `(px, py)`: the largest `s`
/// (outer circles paint over inner) with a non-negative radius that lies in
/// the painted range. Returns the *unclamped* `s` (extend handling is applied
/// by the caller).
fn radial_param(c0: [f64; 3], c1: [f64; 3], px: f64, py: f64, extend: [bool; 2]) -> Option<f64> {
    let dcx = c1[0] - c0[0];
    let dcy = c1[1] - c0[1];
    let dr = c1[2] - c0[2];
    let fx = px - c0[0];
    let fy = py - c0[1];
    let a = dcx * dcx + dcy * dcy - dr * dr;
    let b = -2.0 * (fx * dcx + fy * dcy + c0[2] * dr);
    let c = fx * fx + fy * fy - c0[2] * c0[2];

    // Candidate roots, largest first.
    let mut roots = [f64::NAN; 2];
    if a.abs() < 1e-9 {
        if b.abs() < 1e-12 {
            return None;
        }
        roots[0] = -c / b;
    } else {
        let disc = b * b - 4.0 * a * c;
        if disc < 0.0 {
            return None;
        }
        let sq = disc.sqrt();
        let r0 = (-b + sq) / (2.0 * a);
        let r1 = (-b - sq) / (2.0 * a);
        roots = if r0 >= r1 { [r0, r1] } else { [r1, r0] };
    }
    for &s in roots.iter() {
        if s.is_nan() {
            continue;
        }
        // Radius must be non-negative at this parameter.
        if c0[2] + s * dr < 0.0 {
            continue;
        }
        let ok = (0.0..=1.0).contains(&s) || (s < 0.0 && extend[0]) || (s > 1.0 && extend[1]);
        if ok {
            return Some(s);
        }
    }
    None
}

/// Rec.601 luma of one premultiplied pixel's RGB bytes.
#[inline]
fn luma601(r: u8, g: u8, b: u8) -> u8 {
    (0.3 * r as f32 + 0.59 * g as f32 + 0.11 * b as f32 + 0.5) as u8
}

/// Derive an `Alpha8` soft mask from a rendered mask-group offscreen: its alpha
/// channel (Alpha) or luminosity (Luminosity / LuminosityBc — for the latter
/// the offscreen was pre-filled with the `/BC` backdrop, and pixels outside
/// the mask's extent take the backdrop's luminosity).
///
/// The `/TR` transfer function (a compile-side sampled 256-entry LUT)
/// applies here: `value = lut[value]` after the per-pixel derivation and to
/// `outside`.
fn derive_soft_mask(
    off: &Surface,
    kind: pdf_page_ir::MaskKind,
    transfer: Option<&[u8; 256]>,
) -> ClipMask {
    let (w, h) = (off.width, off.height);
    let mut data = vec![0u8; w * h];
    for ly in 0..h {
        let row = off.local_row(ly);
        for lx in 0..w {
            let px = &row[lx * 4..lx * 4 + 4];
            let value = match kind {
                pdf_page_ir::MaskKind::Alpha => px[3],
                // Premultiplied RGB already equals the color composited over
                // the backdrop (black, or the pre-filled /BC), so its Rec.601
                // luma is the luminosity mask value.
                pdf_page_ir::MaskKind::Luminosity | pdf_page_ir::MaskKind::LuminosityBc { .. } => {
                    luma601(px[0], px[1], px[2])
                }
            };
            data[ly * w + lx] = apply_transfer(value, transfer);
        }
    }
    ClipMask {
        bounds: DeviceRect {
            x: off.origin_x as i32,
            y: off.origin_y as i32,
            width: w as u32,
            height: h as u32,
        },
        data,
        outside: apply_transfer(soft_mask_outside(kind), transfer),
        // A soft mask, not an elidable rectangular clip.
        all_opaque: false,
    }
}

/// Route one derived mask value through the `/TR` LUT (identity when absent).
#[inline]
fn apply_transfer(value: u8, transfer: Option<&[u8; 256]>) -> u8 {
    match transfer {
        Some(lut) => lut[value as usize],
        None => value,
    }
}

/// The soft-mask value outside the mask's rendered extent: 0 for Alpha and
/// plain Luminosity (black backdrop), the backdrop's luminosity for `/BC`.
fn soft_mask_outside(kind: pdf_page_ir::MaskKind) -> u8 {
    match kind {
        pdf_page_ir::MaskKind::Alpha | pdf_page_ir::MaskKind::Luminosity => 0,
        pdf_page_ir::MaskKind::LuminosityBc { backdrop } => {
            luma601(backdrop[0], backdrop[1], backdrop[2])
        }
    }
}

/// Blit a run of cached glyph coverage bitmaps (PDFium's per-run
/// accumulate-and-blit). Each placement's bbox-tight `u8` coverage is composited
/// at its snapped device position: the fast case (Normal blend, no non-rect clip
/// mask, no soft mask) is a single `blend_mask` per row; masked or non-Normal
/// runs take a per-row scratch that applies the clip/soft coverage before the
/// general compositor. No rasterization happens here — that was memoized at
/// lowering time.
#[allow(clippy::too_many_arguments)]
fn paint_glyph_run(
    page: &CpuPreparedPage,
    gr: &PreparedGlyphRun,
    surface: &mut Surface,
    raster: &mut RasterKernel,
    kernels: &KernelSet,
    masks: &mut [Option<ClipMask>],
    soft: Option<&ClipMask>,
    stats: &mut RenderStats,
) {
    #[cfg(feature = "profiling")]
    let command_start = Instant::now();
    stats.commands += 1;

    let mask_cid = if gr.clip_has_mask {
        // Lowering guarantees a masked draw carries its clip id; skip the
        // draw (never panic a page) if that IR invariant broke upstream.
        let Some(cid) = gr.clip else {
            page.diagnostics
                .note_degraded("glyph run skipped: masked clip without a clip id".into());
            return;
        };
        // Use the nearest path-clip ancestor's mask (rect children only narrow
        // bounds, applied per draw): one build shared across all descendants.
        let cid = page.clips[cid as usize].mask_source.unwrap_or(cid) as usize;
        if masks[cid].is_none() {
            let Some(mask) = build_render_clip_mask(page, raster, cid as u32, stats) else {
                return;
            };
            masks[cid] = Some(mask);
        }
        Some(cid)
    } else {
        None
    };
    let raw_mask = mask_cid.and_then(|cid| masks[cid].as_ref());
    // A path clip that rasterized to full coverage over its envelope is
    // rectangular for this draw (already reflected in `bounds`): drop the mask.
    let opaque_clip_elided = raw_mask.is_some_and(|cm| cm.all_opaque);
    let mask = raw_mask.filter(|_| !opaque_clip_elided);

    let blend = choose_blend(gr.blend);
    let simple = matches!(blend, BlendChoice::Normal) && mask.is_none() && soft.is_none();

    let b = gr.bounds;
    let (cbx0, cby0) = (b.x, b.y);
    let cbx1 = b.x + b.width as i32; // exclusive
    let cby1 = b.y + b.height as i32;
    let ox = surface.origin_x;
    let mut scratch: Vec<u8> = Vec::new();
    let mut covered: u64 = 0;

    for pl in &gr.placements {
        let bmp = &pl.bitmap;
        let bw = bmp.width as usize;
        let (gw, gh) = (bmp.width as i32, bmp.height as i32);
        // Device columns/rows this bitmap touches, clamped to the clip envelope.
        let rx0 = pl.dx.max(cbx0);
        let rx1 = (pl.dx + gw).min(cbx1);
        let ry0 = pl.dy.max(cby0);
        let ry1 = (pl.dy + gh).min(cby1);
        if rx0 >= rx1 || ry0 >= ry1 {
            continue;
        }
        let span = (rx1 - rx0) as usize;
        let src_off = (rx0 - pl.dx) as usize;
        let dst0 = (rx0 as usize - ox) * 4;
        let dst1 = (rx1 as usize - ox) * 4;

        for y in ry0..ry1 {
            let ly = (y - pl.dy) as usize;
            let src = &bmp.cov[ly * bw + src_off..ly * bw + src_off + span];
            let row = surface.row_mut(y as usize);

            if simple {
                if let Some(sh) = &gr.shading {
                    shade_glyph_span(
                        &mut row[dst0..dst1],
                        src,
                        sh,
                        rx0 as usize,
                        y as usize,
                        gr.alpha,
                        blend,
                    );
                } else {
                    (kernels.blend_mask)(&mut row[dst0..dst1], src, gr.rgb, gr.alpha);
                }
                covered += src.iter().filter(|&&c| c != 0).count() as u64;
                continue;
            }

            // Masked / non-Normal path: apply clip + soft coverage into scratch.
            scratch.clear();
            scratch.extend_from_slice(src);
            if let Some(m) = mask {
                let mw = m.stride();
                let (mbx, mby) = (m.bounds.x as usize, m.bounds.y as usize);
                let (mbw, mbh) = (m.bounds.width as usize, m.bounds.height as usize);
                let in_row = (y as usize) >= mby && (y as usize) < mby + mbh;
                let base = if in_row { (y as usize - mby) * mw } else { 0 };
                for (i, c) in scratch.iter_mut().enumerate() {
                    let x = rx0 as usize + i;
                    let mv = if in_row && x >= mbx && x < mbx + mbw {
                        m.data[base + (x - mbx)]
                    } else {
                        0
                    };
                    *c = mul_div_255(*c as u16, mv as u16) as u8;
                }
            }
            if let Some(sm) = soft {
                let sw = sm.stride();
                let (sbx, sby) = (sm.bounds.x as usize, sm.bounds.y as usize);
                let (sbw, sbh) = (sm.bounds.width as usize, sm.bounds.height as usize);
                let in_row = (y as usize) >= sby && (y as usize) < sby + sbh;
                let base = if in_row { (y as usize - sby) * sw } else { 0 };
                for (i, c) in scratch.iter_mut().enumerate() {
                    let x = rx0 as usize + i;
                    let sv = if in_row && x >= sbx && x < sbx + sbw {
                        sm.data[base + (x - sbx)]
                    } else {
                        sm.outside
                    };
                    *c = mul_div_255(*c as u16, sv as u16) as u8;
                }
            }
            covered += scratch.iter().filter(|&&c| c != 0).count() as u64;
            let dst = &mut row[dst0..dst1];
            if let Some(sh) = &gr.shading {
                shade_glyph_span(dst, &scratch, sh, rx0 as usize, y as usize, gr.alpha, blend);
            } else {
                match blend {
                    BlendChoice::Normal => (kernels.blend_mask)(dst, &scratch, gr.rgb, gr.alpha),
                    BlendChoice::Separable(f) => blend_span(dst, &scratch, gr.rgb, gr.alpha, f),
                    BlendChoice::NonSeparable(f) => {
                        blend_span_nonsep(dst, &scratch, gr.rgb, gr.alpha, f)
                    }
                }
            }
        }
    }

    stats.covered_pixels += covered;
    stats.ops_painted += 1;
    #[cfg(feature = "profiling")]
    {
        let profile = &mut stats.profile;
        profile.add_duration("render.path", command_start.elapsed());
        profile.increment("render.glyph_runs", 1);
        profile.increment("render.glyph_blits", gr.placements.len() as u64);
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_command(
    page: &CpuPreparedPage,
    cmd: &PreparedCommand,
    surface: &mut Surface,
    raster: &mut RasterKernel,
    kernels: &KernelSet,
    masks: &mut [Option<ClipMask>],
    soft: Option<&ClipMask>,
    stats: &mut RenderStats,
) {
    #[cfg(feature = "profiling")]
    let command_start = Instant::now();
    stats.commands += 1;
    let mask_cid = if cmd.clip_has_mask {
        // Lowering guarantees a masked draw carries its clip id; skip the
        // draw (never panic a page) if that IR invariant broke upstream.
        let Some(cid) = cmd.clip else {
            page.diagnostics
                .note_degraded("fill skipped: masked clip without a clip id".into());
            return;
        };
        // Share the nearest path-clip ancestor's mask across rect descendants.
        let cid = page.clips[cid as usize].mask_source.unwrap_or(cid) as usize;
        if masks[cid].is_none() {
            let Some(mask) = build_render_clip_mask(page, raster, cid as u32, stats) else {
                return;
            };
            masks[cid] = Some(mask);
        }
        Some(cid)
    } else {
        None
    };

    match cmd.class {
        DrawClass::OpaqueRect => {
            paint_opaque_rect(cmd, surface, kernels);
            // An opaque rect marks its whole device rectangle; count it toward
            // coverage so an opaque-fill-only page is never a "silent blank".
            let b = cmd.bounds;
            stats.covered_pixels += b.width as u64 * b.height as u64;
            stats.ops_painted += 1;
        }
        DrawClass::SolidPath => {
            let (rs, re) = cmd.subpath_range;
            let subs = &page.subpaths[rs as usize..re as usize];
            let b = cmd.bounds;
            let (bx0, by0) = (b.x as usize, b.y as usize);
            let bx1 = (b.x as i64 + b.width as i64) as usize; // exclusive
            let by1 = (b.y as i64 + b.height as i64) as usize;
            let mask = mask_cid.and_then(|cid| masks[cid].as_ref());
            let (rgb, alpha, opaque, premul) = (cmd.rgb, cmd.alpha, cmd.opaque, cmd.premul);
            let blend = choose_blend(cmd.blend);
            let shading = cmd.shading.as_deref();
            let stencil = cmd.stencil.as_deref();
            // The coverage buffer is sized to the device plane, not the
            // (possibly offset) surface; pass full device dims.
            let width = surface.origin_x + surface.width;
            let height = surface.origin_y + surface.height;

            if !raster.fill_cancellable(
                &page.points,
                subs,
                width,
                height,
                cmd.rule,
                page.decode_limits.should_cancel.as_deref(),
                |y, x0, x1, cov| {
                    if y < by0 || y >= by1 {
                        return;
                    }
                    dispatch_row(
                        surface, kernels, y, x0, x1, cov, bx0, bx1, mask, soft, rgb, alpha, opaque,
                        premul, blend, shading, stencil,
                    );
                },
            ) {
                stats.cancelled = true;
                return;
            }
            stats.ops_painted += 1;
            stats.edges += raster.last_edges;
            stats.rows_rasterized += raster.last_rows;
            stats.covered_pixels += raster.last_covered;
        }
    }
    #[cfg(feature = "profiling")]
    {
        let profile = &mut stats.profile;
        profile.add_duration("render.path", command_start.elapsed());
        match cmd.class {
            DrawClass::OpaqueRect => profile.increment("render.opaque_rects", 1),
            DrawClass::SolidPath => profile.increment("render.solid_paths", 1),
        }
    }
}

/// Composite an isolated group's offscreen surface back onto `parent` with
/// constant `opacity` and blend mode.
fn composite_group(
    parent: &mut Surface,
    group: &Surface,
    opacity: u8,
    blend: BlendChoice,
    soft: Option<&ClipMask>,
) {
    let pox = parent.origin_x;
    for ly in 0..group.height {
        let abs_y = group.origin_y + ly;
        let grow = group.local_row(ly);
        let prow = parent.row_mut(abs_y);
        for lx in 0..group.width {
            let src = &grow[lx * 4..lx * 4 + 4];
            if src[3] == 0 {
                continue;
            }
            let alpha = match soft {
                Some(sm) => {
                    let sv = soft_mask_at(sm, group.origin_x + lx, abs_y);
                    if sv == 0 {
                        continue;
                    }
                    mul_div_255(opacity as u16, sv as u16) as u8
                }
                None => opacity,
            };
            let plx = group.origin_x + lx - pox;
            composite_layer_px(&mut prow[plx * 4..plx * 4 + 4], src, alpha, blend);
        }
    }
}

/// Sample a soft mask at absolute device coordinates, yielding `sm.outside`
/// beyond its rendered extent (the same convention the per-paint path uses).
#[inline]
fn soft_mask_at(sm: &ClipMask, x: usize, y: usize) -> u8 {
    let (sbx, sby) = (sm.bounds.x as usize, sm.bounds.y as usize);
    let (sbw, sbh) = (sm.bounds.width as usize, sm.bounds.height as usize);
    if y >= sby && y < sby + sbh && x >= sbx && x < sbx + sbw {
        sm.data[(y - sby) * sm.stride() + (x - sbx)]
    } else {
        sm.outside
    }
}

/// Fold a *seeded* non-isolated group (Normal composite blend) back onto its
/// parent. The offscreen already holds `backdrop ∘ content`, so we do not
/// re-composite the backdrop; we linearly blend it over the parent at the
/// group's constant opacity: `parent = lerp(parent, offscreen, opacity)`. At
/// opacity 1 this is a replace (exact for the group); at opacity 0 a no-op.
/// Since offscreen and parent share the backdrop, untouched pixels lerp to
/// themselves — no darkening halo around the group's marks.
fn composite_group_seeded(
    parent: &mut Surface,
    group: &Surface,
    opacity: u8,
    soft: Option<&ClipMask>,
) {
    if opacity == 0 {
        return;
    }
    let pox = parent.origin_x;
    for ly in 0..group.height {
        let abs_y = group.origin_y + ly;
        let grow = group.local_row(ly);
        let prow = parent.row_mut(abs_y);
        for lx in 0..group.width {
            let src = &grow[lx * 4..lx * 4 + 4];
            // Masking the lerp weight is exactly right here: at mask 0 the
            // parent keeps the backdrop it already shares with the offscreen.
            let t = match soft {
                Some(sm) => {
                    let sv = soft_mask_at(sm, group.origin_x + lx, abs_y);
                    if sv == 0 {
                        continue;
                    }
                    mul_div_255(opacity as u16, sv as u16)
                }
                None => opacity as u16,
            };
            let plx = group.origin_x + lx - pox;
            let dst = &mut prow[plx * 4..plx * 4 + 4];
            if t == 255 {
                dst.copy_from_slice(src);
                continue;
            }
            for c in 0..4 {
                // lerp(dst, src, t/255) in integer space.
                dst[c] =
                    (mul_div_255(dst[c] as u16, 255 - t) + mul_div_255(src[c] as u16, t)) as u8;
            }
        }
    }
}

/// Composite one straight-alpha source color onto a premultiplied pixel under
/// an arbitrary blend choice. `rgb` is the straight source color; `a` is the
/// *effective* source alpha (coverage, constant alpha, and the source color's
/// own alpha already folded in). The `Normal` arm is the exact integer
/// source-over that the fast paths inline (byte-identical); the non-Normal
/// arms are the per-pixel-color analog of `blend_span`/`blend_span_nonsep`,
/// used by paints whose color varies per pixel (shadings, images, tiling
/// cells).
#[inline]
fn composite_px_blended(px: &mut [u8], rgb: [u8; 3], a: u8, blend: BlendChoice) {
    if a == 0 {
        return;
    }
    match blend {
        BlendChoice::Normal => {
            let a = a as u16;
            let ia = 255 - a;
            px[0] = (mul_div_255(rgb[0] as u16, a) + mul_div_255(px[0] as u16, ia)) as u8;
            px[1] = (mul_div_255(rgb[1] as u16, a) + mul_div_255(px[1] as u16, ia)) as u8;
            px[2] = (mul_div_255(rgb[2] as u16, a) + mul_div_255(px[2] as u16, ia)) as u8;
            px[3] = (a + mul_div_255(px[3] as u16, ia)) as u8;
        }
        BlendChoice::Separable(f) => {
            let cs = [
                rgb[0] as f32 / 255.0,
                rgb[1] as f32 / 255.0,
                rgb[2] as f32 / 255.0,
            ];
            let a_s = a as f32 / 255.0;
            let da = px[3] as f32 / 255.0;
            let inv_da = if da > 0.0 { 1.0 / da } else { 0.0 };
            let b = [
                f(px[0] as f32 / 255.0 * inv_da, cs[0]),
                f(px[1] as f32 / 255.0 * inv_da, cs[1]),
                f(px[2] as f32 / 255.0 * inv_da, cs[2]),
            ];
            composite_px(px, cs, a_s, b, da);
        }
        BlendChoice::NonSeparable(f) => {
            let cs = [
                rgb[0] as f32 / 255.0,
                rgb[1] as f32 / 255.0,
                rgb[2] as f32 / 255.0,
            ];
            let a_s = a as f32 / 255.0;
            let da = px[3] as f32 / 255.0;
            let inv_da = if da > 0.0 { 1.0 / da } else { 0.0 };
            let cb = [
                px[0] as f32 / 255.0 * inv_da,
                px[1] as f32 / 255.0 * inv_da,
                px[2] as f32 / 255.0 * inv_da,
            ];
            let b = f(cb, cs);
            composite_px(px, cs, a_s, b, da);
        }
    }
}

/// Composite one premultiplied group pixel `src` onto `dst` at group opacity.
#[inline]
fn composite_layer_px(dst: &mut [u8], src: &[u8], opacity: u8, blend: BlendChoice) {
    let sa = src[3] as f32 / 255.0;
    if sa <= 0.0 {
        return;
    }
    let a_s = sa * (opacity as f32 / 255.0);
    if a_s <= 0.0 {
        return;
    }
    let inv = 1.0 / sa;
    let cs = [
        src[0] as f32 / 255.0 * inv,
        src[1] as f32 / 255.0 * inv,
        src[2] as f32 / 255.0 * inv,
    ];
    let da = dst[3] as f32 / 255.0;
    let b = match blend {
        BlendChoice::Normal => cs,
        BlendChoice::Separable(f) => {
            let inv_da = if da > 0.0 { 1.0 / da } else { 0.0 };
            [
                f(dst[0] as f32 / 255.0 * inv_da, cs[0]),
                f(dst[1] as f32 / 255.0 * inv_da, cs[1]),
                f(dst[2] as f32 / 255.0 * inv_da, cs[2]),
            ]
        }
        BlendChoice::NonSeparable(f) => {
            let inv_da = if da > 0.0 { 1.0 / da } else { 0.0 };
            let cb = [
                dst[0] as f32 / 255.0 * inv_da,
                dst[1] as f32 / 255.0 * inv_da,
                dst[2] as f32 / 255.0 * inv_da,
            ];
            f(cb, cs)
        }
    };
    composite_px(dst, cs, a_s, b, da);
}

/// Direct row fill for an integer-aligned opaque rectangle — no coverage. The
/// bounds already include any rectangular clip intersection.
fn paint_opaque_rect(cmd: &PreparedCommand, surface: &mut Surface, kernels: &KernelSet) {
    let b = cmd.bounds;
    let ox = surface.origin_x;
    let x0 = b.x as usize;
    let x1 = (b.x as i64 + b.width as i64) as usize;
    let y0 = b.y as usize;
    let y1 = (b.y as i64 + b.height as i64) as usize;
    for y in y0..y1 {
        let row = surface.row_mut(y);
        (kernels.fill_opaque)(&mut row[(x0 - ox) * 4..(x1 - ox) * 4], cmd.premul);
    }
}

/// Clip the coverage row to `[bx0, bx1)`, apply the optional mask, then
/// classify the surviving coverage into spans and dispatch a kernel per span.
#[allow(clippy::too_many_arguments)]
#[inline]
fn dispatch_row(
    surface: &mut Surface,
    kernels: &KernelSet,
    y: usize,
    x0: usize,
    x1: usize,
    cov: &mut [u8],
    bx0: usize,
    bx1: usize,
    mask: Option<&ClipMask>,
    soft: Option<&ClipMask>,
    rgb: [u8; 3],
    alpha: u8,
    opaque: bool,
    premul: [u8; 4],
    blend: BlendChoice,
    shading: Option<&crate::prepared::PreparedShading>,
    stencil: Option<&crate::image::PreparedImage>,
) {
    // Restrict to the clip's column envelope.
    let cxa = x0.max(bx0);
    let cxb = x1.min(bx1.saturating_sub(1));
    if cxa > cxb {
        return;
    }
    // Apply the non-rectangular clip mask (per-pixel multiply).
    if let Some(m) = mask {
        let mw = m.stride();
        let base = (y - m.bounds.y as usize) * mw;
        let mbx = m.bounds.x as usize;
        #[allow(clippy::needless_range_loop)] // cov and mask index at different offsets
        for x in cxa..=cxb {
            let mv = m.data[base + (x - mbx)];
            if mv != 255 {
                cov[x] = mul_div_255(cov[x] as u16, mv as u16) as u8;
            }
        }
    }
    // Apply the active soft mask: pixels outside its bounds are fully masked.
    if let Some(sm) = soft {
        let sw = sm.stride();
        let (sbx, sby) = (sm.bounds.x as usize, sm.bounds.y as usize);
        let sbw = sm.bounds.width as usize;
        let sbh = sm.bounds.height as usize;
        let in_row = y >= sby && y < sby + sbh;
        let base = if in_row { (y - sby) * sw } else { 0 };
        #[allow(clippy::needless_range_loop)] // cov and mask index at different offsets
        for x in cxa..=cxb {
            let sv = if in_row && x >= sbx && x < sbx + sbw {
                sm.data[base + (x - sbx)]
            } else {
                sm.outside
            };
            cov[x] = mul_div_255(cov[x] as u16, sv as u16) as u8;
        }
    }
    // A shading-painted `/ImageMask` contributes coverage rather than color.
    // Sampling the already-prepared image preserves decode polarity,
    // interpolation, minification, rotation and shear.
    if let Some(stencil) = stencil {
        #[allow(clippy::needless_range_loop)]
        for x in cxa..=cxb {
            let alpha = stencil
                .shade(x as f64 + 0.5, y as f64 + 0.5)
                .map_or(0, |sample| sample[3]);
            cov[x] = mul_div_255(cov[x] as u16, alpha as u16) as u8;
        }
    }

    // Local column offset for a possibly-offset (group offscreen) surface.
    let ox = surface.origin_x;

    // Shading paint: evaluate the ramp per pixel and composite under the
    // command's blend (`composite_px_blended`'s Normal arm is the exact
    // integer source-over this path always used).
    if let Some(sh) = shading {
        let row = surface.row_mut(y);
        for x in cxa..=cxb {
            let c = cov[x];
            if c == 0 {
                continue;
            }
            let Some(color) = shade_pixel(sh, x as f64 + 0.5, y as f64 + 0.5) else {
                continue;
            };
            let a = mul_div_255(c as u16, mul_div_255(color[3] as u16, alpha as u16)) as u8;
            if a == 0 {
                continue;
            }
            let px = &mut row[(x - ox) * 4..(x - ox) * 4 + 4];
            composite_px_blended(px, [color[0], color[1], color[2]], a, blend);
        }
        return;
    }

    let row = surface.row_mut(y);

    // Non-Normal blend modes take the general per-pixel compositor.
    match blend {
        BlendChoice::Separable(bfn) => {
            blend_span(
                &mut row[(cxa - ox) * 4..(cxb + 1 - ox) * 4],
                &cov[cxa..=cxb],
                rgb,
                alpha,
                bfn,
            );
            return;
        }
        BlendChoice::NonSeparable(bfn) => {
            blend_span_nonsep(
                &mut row[(cxa - ox) * 4..(cxb + 1 - ox) * 4],
                &cov[cxa..=cxb],
                rgb,
                alpha,
                bfn,
            );
            return;
        }
        BlendChoice::Normal => {}
    }

    let mut x = cxa;
    while x <= cxb {
        let c = cov[x];
        if c == 0 {
            x += 1;
            continue;
        }
        if c == 255 {
            let start = x;
            while x <= cxb && cov[x] == 255 {
                x += 1;
            }
            let dst = &mut row[(start - ox) * 4..(x - ox) * 4];
            if opaque {
                (kernels.fill_opaque)(dst, premul);
            } else {
                (kernels.blend_const)(dst, rgb, alpha);
            }
        } else {
            let start = x;
            while x <= cxb && cov[x] != 0 && cov[x] != 255 {
                x += 1;
            }
            let dst = &mut row[(start - ox) * 4..(x - ox) * 4];
            (kernels.blend_mask)(dst, &cov[start..x], rgb, alpha);
        }
    }
}

#[cfg(test)]
mod rgb8_area_min_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::image::PreparedImage;
    use crate::surface::Surface;
    use pdf_page_ir::{BlendMode, DeviceRect, ImageColorSpace, InterpolationMode, Matrix};
    use pdf_render_api::Background;
    use std::sync::Arc;

    /// Tiny deterministic xorshift, so the fuzz is reproducible.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn byte(&mut self) -> u8 {
            (self.next() >> 24) as u8
        }
    }

    fn make_image_cs(
        sw: u32,
        sh: u32,
        dev_w: u32,
        dev_h: u32,
        color_space: ImageColorSpace,
        samples: Vec<u8>,
    ) -> PreparedImage {
        let mut img = make_image(sw, sh, dev_w, dev_h, Vec::new());
        img.color_space = color_space;
        img.samples = Arc::from(samples);
        img
    }

    fn make_image(sw: u32, sh: u32, dev_w: u32, dev_h: u32, samples: Vec<u8>) -> PreparedImage {
        // Image unit square maps to device [0,dev_w)x[0,dev_h): u = dx/dev_w,
        // v = dy/dev_h, off-diagonals exactly zero. Footprint = source texels
        // per device pixel, exactly as lowering computes it.
        let inv = Matrix {
            a: 1.0 / dev_w as f64,
            b: 0.0,
            c: 0.0,
            d: 1.0 / dev_h as f64,
            e: 0.0,
            f: 0.0,
        };
        PreparedImage {
            origin: pdf_page_ir::PaintOrigin::PageContent,
            bounds: DeviceRect {
                x: 0,
                y: 0,
                width: dev_w,
                height: dev_h,
            },
            clip: None,
            clip_has_mask: false,
            inv,
            width: sw,
            height: sh,
            bpc: 8,
            color_space: ImageColorSpace::Rgb,
            decode: None,
            samples: Arc::from(samples),
            sample_lut: None,
            smask: None,
            mask: None,
            interpolation: InterpolationMode::Nearest,
            footprint: [sw as f64 / dev_w as f64, sh as f64 / dev_h as f64],
            is_stencil: false,
            stencil_rgb: [0, 0, 0],
            alpha: 255,
            blend: BlendMode::Normal,
        }
    }

    /// The fast path must reproduce, byte for byte, the generic per-pixel
    /// `shade()` area-average + opaque write, over a fuzzed image and several
    /// minifying footprints.
    #[test]
    fn fast_path_matches_generic_shade() {
        let mut rng = Rng(0x1234_5678_9abc_def0);
        // A spread of source sizes and device sizes (all minifying: dev < src).
        let cases = [
            (7u32, 5u32, 3u32, 3u32),
            (16, 16, 5, 7),
            (33, 9, 8, 4),
            (9, 33, 4, 8),
            (64, 48, 20, 15),
            (5, 5, 4, 2), // one axis barely minified, other strongly
        ];
        for &(sw, sh, dev_w, dev_h) in &cases {
            for _trial in 0..8 {
                let samples: Vec<u8> = (0..(sw * sh * 3)).map(|_| rng.byte()).collect();
                let img = make_image(sw, sh, dev_w, dev_h, samples);
                // Fast path renders into `fast`.
                let mut fast =
                    Surface::new(dev_w as usize, dev_h as usize, Background::Transparent);
                let (painted, taps) = paint_axis_aligned_rgb8_area_min_opaque(
                    &img,
                    &mut fast,
                    0,
                    0,
                    dev_w as usize,
                    dev_h as usize,
                );
                // Reference: generic `shade()` + opaque source-over over a
                // transparent surface (which reduces to a direct copy).
                let mut reference =
                    Surface::new(dev_w as usize, dev_h as usize, Background::Transparent);
                let mut ref_painted = 0u64;
                let _ = taps; // profiling-only telemetry; not asserted here
                for y in 0..dev_h as usize {
                    let row = reference.row_mut(y);
                    for x in 0..dev_w as usize {
                        let color = img.shade(x as f64 + 0.5, y as f64 + 0.5);
                        if let Some(c) = color {
                            let a = mul_div_255(mul_div_255(c[3] as u16, 255), 255);
                            if a == 0 {
                                continue;
                            }
                            let ia = 255 - a;
                            let px = &mut row[x * 4..x * 4 + 4];
                            px[0] =
                                (mul_div_255(c[0] as u16, a) + mul_div_255(px[0] as u16, ia)) as u8;
                            px[1] =
                                (mul_div_255(c[1] as u16, a) + mul_div_255(px[1] as u16, ia)) as u8;
                            px[2] =
                                (mul_div_255(c[2] as u16, a) + mul_div_255(px[2] as u16, ia)) as u8;
                            px[3] = (a + mul_div_255(px[3] as u16, ia)) as u8;
                            ref_painted += 1;
                        }
                    }
                }
                assert_eq!(
                    painted, ref_painted,
                    "painted count sw={sw} sh={sh} dev={dev_w}x{dev_h}"
                );
                for y in 0..dev_h as usize {
                    let fr = fast.row_mut(y).to_vec();
                    let rr = reference.row_mut(y).to_vec();
                    assert_eq!(
                        fr, rr,
                        "row {y} differs sw={sw} sh={sh} dev={dev_w}x{dev_h}"
                    );
                }
            }
        }
    }

    /// A packed one-bit mask bitmap of `w x h`, byte-aligned rows.
    fn bilevel(w: u32, h: u32, rng: &mut Rng) -> Arc<pdf_page_ir::ImageSMask> {
        let row_bytes = (w as usize).div_ceil(8);
        let samples: Vec<u8> = (0..row_bytes * h as usize).map(|_| rng.byte()).collect();
        Arc::new(pdf_page_ir::ImageSMask {
            width: w,
            height: h,
            bits_per_component: 1,
            decode: None,
            samples: Arc::from(samples),
            codec: None,
            codec_data: None,
            codec_parms: None,
        })
    }

    /// The generic `paint_image` body for the shape the masked area-min path
    /// serves: no clip mask, no soft clip, opaque `alpha`, Normal blend.
    fn generic_masked_reference(img: &PreparedImage, dev_w: u32, dev_h: u32) -> (Surface, u64) {
        let mut reference = Surface::new(dev_w as usize, dev_h as usize, Background::Transparent);
        let mut painted = 0u64;
        for y in 0..dev_h as usize {
            let row = reference.row_mut(y);
            for x in 0..dev_w as usize {
                let (dx, dy) = (x as f64 + 0.5, y as f64 + 0.5);
                let edge = img.edge_coverage(dx, dy);
                if edge == Some(0) {
                    continue;
                }
                let color = match edge {
                    None => img.shade(dx, dy),
                    Some(_) => img.shade_clamped(dx, dy),
                };
                let Some(c) = color else { continue };
                let cov = match edge {
                    Some(ec) => mul_div_255(255, ec),
                    None => 255,
                };
                let a = mul_div_255(mul_div_255(c[3] as u16, 255), cov);
                if a == 0 {
                    continue;
                }
                let px = &mut row[x * 4..x * 4 + 4];
                composite_px_blended(px, [c[0], c[1], c[2]], a as u8, BlendChoice::Normal);
                painted += 1;
            }
        }
        (reference, painted)
    }

    /// The MRC path — minified RGB8 cut out by a bilevel layer — must reproduce
    /// the generic per-pixel body byte for byte, in **both** mask encodings and
    /// including the anti-aliased border band. Mask dimensions are varied
    /// independently of the base image's, as MRC producers do.
    #[test]
    fn masked_fast_path_matches_generic_shade() {
        let mut rng = Rng(0x00c0_ffee_0bad_f00d);
        let cases = [
            (7u32, 5u32, 3u32, 3u32),
            (16, 16, 5, 7),
            (33, 9, 8, 4),
            (9, 33, 4, 8),
            (64, 48, 20, 15),
            (5, 5, 4, 2),
        ];
        for &(sw, sh, dev_w, dev_h) in &cases {
            // Mask geometries: same as the base, coarser, and finer.
            for &(mw, mh) in &[(sw, sh), (sw * 2, sh * 2), (sw.div_ceil(2), sh.div_ceil(2))] {
                for trial in 0..6 {
                    let samples: Vec<u8> = (0..(sw * sh * 3)).map(|_| rng.byte()).collect();
                    let mut img = make_image(sw, sh, dev_w, dev_h, samples);
                    let mask = bilevel(mw, mh, &mut rng);
                    // Alternate the two encodings, and exercise `/Decode [1 0]`
                    // polarity on the soft-mask side.
                    let soft = trial % 2 == 0;
                    if soft {
                        let mut sm = (*mask).clone();
                        if trial % 4 == 0 {
                            sm.decode = Some(Arc::from(vec![[1.0f32, 0.0f32]]));
                        }
                        img.smask = Some(Arc::new(sm));
                    } else {
                        img.mask = Some(pdf_page_ir::ImageMask::Stencil(mask));
                    }

                    let Some(kind) = area_min_bilevel_mask(&img) else {
                        // Only a non-minifying soft mask is turned away, and
                        // then the generic path is already what runs.
                        assert!(soft, "a stencil mask is always eligible");
                        continue;
                    };
                    let mut fast =
                        Surface::new(dev_w as usize, dev_h as usize, Background::Transparent);
                    let (painted, _taps) = paint_axis_aligned_rgb8_area_min_masked(
                        &img,
                        kind,
                        &mut fast,
                        0,
                        0,
                        dev_w as usize,
                        dev_h as usize,
                    );

                    let (mut reference, ref_painted) = generic_masked_reference(&img, dev_w, dev_h);
                    let what = if soft { "smask" } else { "stencil" };
                    assert_eq!(
                        painted, ref_painted,
                        "{what} painted count sw={sw} sh={sh} mask={mw}x{mh} dev={dev_w}x{dev_h}"
                    );
                    for y in 0..dev_h as usize {
                        assert_eq!(
                            fast.row_mut(y).to_vec(),
                            reference.row_mut(y).to_vec(),
                            "{what} row {y} differs sw={sw} sh={sh} mask={mw}x{mh} dev={dev_w}x{dev_h}"
                        );
                    }
                }
            }
        }
    }

    /// The CMYK fast path (convert source to RGB8 once, then box-average) must
    /// reproduce the generic per-tap `pixel()`→`cmyk_to_rgb`→average, byte for
    /// byte, over a fuzzed CMYK image and several minifying footprints.
    #[test]
    fn cmyk_fast_path_matches_generic_shade() {
        let mut rng = Rng(0x0fee_1dea_dbad_c0de);
        let cases = [
            (7u32, 5u32, 3u32, 3u32),
            (16, 16, 5, 7),
            (33, 9, 8, 4),
            (64, 48, 20, 15),
            (5, 5, 4, 2),
        ];
        for &(sw, sh, dev_w, dev_h) in &cases {
            for _trial in 0..8 {
                let samples: Vec<u8> = (0..(sw * sh * 4)).map(|_| rng.byte()).collect();
                let img = make_image_cs(sw, sh, dev_w, dev_h, ImageColorSpace::Cmyk, samples);
                let mut fast =
                    Surface::new(dev_w as usize, dev_h as usize, Background::Transparent);
                let (painted, _taps) = paint_axis_aligned_cmyk_area_min_opaque(
                    &img,
                    &mut fast,
                    0,
                    0,
                    dev_w as usize,
                    dev_h as usize,
                )
                .expect("cmyk fast path applies to this small image");
                let mut reference =
                    Surface::new(dev_w as usize, dev_h as usize, Background::Transparent);
                let mut ref_painted = 0u64;
                for y in 0..dev_h as usize {
                    let row = reference.row_mut(y);
                    for x in 0..dev_w as usize {
                        if let Some(c) = img.shade(x as f64 + 0.5, y as f64 + 0.5) {
                            let a = mul_div_255(mul_div_255(c[3] as u16, 255), 255);
                            if a == 0 {
                                continue;
                            }
                            let ia = 255 - a;
                            let px = &mut row[x * 4..x * 4 + 4];
                            px[0] =
                                (mul_div_255(c[0] as u16, a) + mul_div_255(px[0] as u16, ia)) as u8;
                            px[1] =
                                (mul_div_255(c[1] as u16, a) + mul_div_255(px[1] as u16, ia)) as u8;
                            px[2] =
                                (mul_div_255(c[2] as u16, a) + mul_div_255(px[2] as u16, ia)) as u8;
                            px[3] = (a + mul_div_255(px[3] as u16, ia)) as u8;
                            ref_painted += 1;
                        }
                    }
                }
                assert_eq!(
                    painted, ref_painted,
                    "cmyk painted count sw={sw} sh={sh} dev={dev_w}x{dev_h}"
                );
                for y in 0..dev_h as usize {
                    assert_eq!(
                        fast.row_mut(y).to_vec(),
                        reference.row_mut(y).to_vec(),
                        "cmyk row {y} differs sw={sw} sh={sh} dev={dev_w}x{dev_h}"
                    );
                }
            }
        }
    }

    /// The summed-area-table weighted ones count must agree exactly with the
    /// direct per-box weighted popcount (`binary_box_average`) — both are
    /// integer arithmetic over the same fractional tap weights, so equality
    /// is exact, over fuzzed bilevel images and minifying footprints.
    #[test]
    fn sat_weighted_ones_matches_direct_binary_average() {
        let mut rng = Rng(0xfeed_beef_dead_cafe);
        let cases = [(13u32, 7u32, 5u32, 3u32), (32, 32, 9, 11), (7, 19, 3, 6)];
        for &(sw, sh, dev_w, dev_h) in &cases {
            let row_bytes = (sw as usize).div_ceil(8);
            let samples: Vec<u8> = (0..row_bytes * sh as usize).map(|_| rng.byte()).collect();
            let mut img = make_image(sw, sh, dev_w, dev_h, samples);
            img.bpc = 1;
            img.color_space = ImageColorSpace::Gray;
            img.sample_lut = crate::image::build_sample_lut(1, &ImageColorSpace::Gray, None, false);
            assert!(img.is_binary_box_filterable());
            let (zero, one) = img.binary_box_lut().unwrap();

            let cols: Vec<Option<crate::image::AxisTaps>> = (0..dev_w)
                .map(|x| {
                    let u = img.inv.a * (x as f64 + 0.5) + img.inv.e;
                    let fx = u * img.width as f64 - 0.5;
                    img.box_taps_x(fx)
                })
                .collect();
            let rows: Vec<Option<crate::image::AxisTaps>> = (0..dev_h)
                .map(|y| {
                    let v = img.inv.d * (y as f64 + 0.5) + img.inv.f;
                    let fy = (1.0 - v) * img.height as f64 - 0.5;
                    img.box_taps_y(fy)
                })
                .collect();
            let sat = BilevelIntegral::build(&img, &cols, &rows).unwrap();

            for ty in rows.iter().flatten() {
                for tx in cols.iter().flatten() {
                    let weight = tx.total * ty.total;
                    let via_sat =
                        crate::image::mix_bilevel(zero, one, sat.weighted_ones(tx, ty), weight);
                    let (direct, _) = img.binary_box_average(tx, ty).unwrap();
                    assert_eq!(via_sat, direct, "sw={sw} sh={sh} tx={tx:?} ty={ty:?}");
                }
            }
        }
    }

    /// Magnified / 1:1 bilinear fast path must match generic `shade()`
    /// byte-for-byte (continuous-tone JPEG/JPX magnification policy).
    #[test]
    fn bilinear_fast_path_matches_generic_shade() {
        let mut rng = Rng(0xc0ff_ee12_3456_789a);
        // All magnifying or 1:1: dest ≥ source on both axes.
        let cases = [
            (2u32, 1u32, 8u32, 8u32),
            (3, 3, 3, 3),
            (4, 4, 16, 16),
            (7, 5, 21, 15),
            (16, 12, 32, 24),
            (5, 9, 11, 19),
            (1, 1, 8, 8),
        ];
        for &(sw, sh, dev_w, dev_h) in &cases {
            for _trial in 0..8 {
                let samples: Vec<u8> = (0..(sw * sh * 3)).map(|_| rng.byte()).collect();
                let mut img = make_image(sw, sh, dev_w, dev_h, samples);
                img.interpolation = InterpolationMode::Bilinear;
                let mut fast =
                    Surface::new(dev_w as usize, dev_h as usize, Background::Transparent);
                let (painted, taps) = paint_axis_aligned_rgb8_bilinear_opaque(
                    &img,
                    &mut fast,
                    0,
                    0,
                    dev_w as usize,
                    dev_h as usize,
                );
                assert_eq!(taps, painted * 4, "tap accounting");
                let mut reference =
                    Surface::new(dev_w as usize, dev_h as usize, Background::Transparent);
                let mut ref_painted = 0u64;
                for y in 0..dev_h as usize {
                    let row = reference.row_mut(y);
                    for x in 0..dev_w as usize {
                        if let Some(c) = img.shade(x as f64 + 0.5, y as f64 + 0.5) {
                            let a = mul_div_255(mul_div_255(c[3] as u16, 255), 255);
                            if a == 0 {
                                continue;
                            }
                            // Opaque over transparent → direct write.
                            let px = &mut row[x * 4..x * 4 + 4];
                            px[0] = c[0];
                            px[1] = c[1];
                            px[2] = c[2];
                            px[3] = 255;
                            ref_painted += 1;
                        }
                    }
                }
                assert_eq!(
                    painted, ref_painted,
                    "bilinear painted count sw={sw} sh={sh} dev={dev_w}x{dev_h}"
                );
                for y in 0..dev_h as usize {
                    assert_eq!(
                        fast.row_mut(y).to_vec(),
                        reference.row_mut(y).to_vec(),
                        "bilinear row {y} differs sw={sw} sh={sh} dev={dev_w}x{dev_h}"
                    );
                }
            }
        }
    }

    /// Known truncation sample from the integration bilinear fixture:
    /// 2×1 red/blue magnified to 8×8, dest x=2 → [223, 0, 31, 255].
    #[test]
    fn bilinear_fast_path_truncates_like_reference_stretchers() {
        let mut img = make_image(2, 1, 8, 8, vec![255, 0, 0, 0, 0, 255]);
        img.interpolation = InterpolationMode::Bilinear;
        let mut surface = Surface::new(8, 8, Background::Transparent);
        let _ = paint_axis_aligned_rgb8_bilinear_opaque(&img, &mut surface, 0, 0, 8, 8);
        let row = surface.row_mut(4);
        let px = &row[2 * 4..2 * 4 + 4];
        assert_eq!(px, [223, 0, 31, 255]);
    }
}

#[cfg(test)]
mod cancel_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use pdf_page_ir::DeviceSize;
    use pdf_render_api::Background;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A prepared page of `n` identical opaque square fills.
    fn page_with_fills(
        n: usize,
        cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> crate::prepared::CpuPreparedPage {
        let points = vec![[10.0f32, 10.0], [50.0, 10.0], [50.0, 50.0], [10.0, 50.0]];
        let subpaths = vec![(0usize, 4usize)];
        let cmd = crate::prepared::PreparedCommand {
            origin: pdf_page_ir::PaintOrigin::PageContent,
            class: crate::prepared::DrawClass::SolidPath,
            subpath_range: (0, 1),
            rule: crate::raster::FillRule::NonZero,
            rgb: [0, 0, 0],
            premul: [0, 0, 0, 255],
            alpha: 255,
            opaque: true,
            bounds: pdf_page_ir::DeviceRect {
                x: 10,
                y: 10,
                width: 40,
                height: 40,
            },
            clip: None,
            clip_has_mask: false,
            blend: pdf_page_ir::BlendMode::Normal,
            shading: None,
            stencil: None,
        };
        let ops = (0..n).map(|_| PreparedOp::Draw(cmd.clone())).collect();
        crate::prepared::CpuPreparedPage {
            size: DeviceSize {
                width: 64,
                height: 64,
            },
            ops,
            clips: Vec::new(),
            points,
            subpaths,
            codecs: pdf_image::CodecRegistry::default(),
            decode_limits: pdf_image::DecodeLimits {
                should_cancel: cancel,
                ..Default::default()
            },
            hinting: pdf_font::HintingPolicy::None,
            color_policy: pdf_render_api::RenderColorPolicy::Original,
            diagnostics: Default::default(),
            image_cache: None,
            #[cfg(feature = "profiling")]
            profile: Default::default(),
            #[cfg(feature = "profiling")]
            decode_cache: None,
        }
    }

    #[test]
    fn mid_render_cancellation_stops_between_ops() {
        // The token fires from the very first check: execution must stop at
        // an op boundary with the cancelled flag set, not run all 64 draws.
        let checks = Arc::new(AtomicUsize::new(0));
        let seen = checks.clone();
        let cancel: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
            true
        });
        let page = page_with_fills(64, Some(cancel));
        let mut surface = Surface::new(64, 64, Background::White);
        let mut ctx = CpuWorkerContext::default();
        let mut stats = RenderStats::default();
        execute(&page, &mut surface, &mut ctx, &mut stats);
        assert!(stats.cancelled, "mid-render cancellation must be recorded");
        assert_eq!(stats.commands, 0, "no command runs after the flag fires");
        assert!(checks.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn cancellation_stops_inside_a_single_coverage_command() {
        // Let the pre-pass and op-boundary probes pass, then fire from the
        // RasterKernel checkpoints while the first (and only) draw is active.
        let checks = Arc::new(AtomicUsize::new(0));
        let seen = checks.clone();
        let cancel: Arc<dyn Fn() -> bool + Send + Sync> =
            Arc::new(move || seen.fetch_add(1, Ordering::Relaxed) >= 3);
        let page = page_with_fills(1, Some(cancel));
        let mut surface = Surface::new(64, 64, Background::White);
        let mut ctx = CpuWorkerContext::default();
        let mut stats = RenderStats::default();
        execute(&page, &mut surface, &mut ctx, &mut stats);
        assert!(stats.cancelled);
        assert_eq!(
            stats.commands, 1,
            "the command began, so cancellation was not merely an op-boundary check"
        );
        assert!(checks.load(Ordering::Relaxed) >= 4);
    }

    #[test]
    fn uncancelled_render_executes_every_op() {
        let page = page_with_fills(64, None);
        let mut surface = Surface::new(64, 64, Background::White);
        let mut ctx = CpuWorkerContext::default();
        let mut stats = RenderStats::default();
        execute(&page, &mut surface, &mut ctx, &mut stats);
        assert!(!stats.cancelled);
        assert_eq!(stats.commands, 64);
    }
}

#[cfg(test)]
mod bilevel_sat_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// The serial reference walk from `BilevelIntegral::build`, verbatim.
    #[allow(clippy::too_many_arguments)]
    fn serial_fill(
        sat: &mut [u32],
        samples: &[u8],
        stride: usize,
        sw: usize,
        sh: usize,
        row_bits: usize,
        col_lo: u32,
        row_lo: u32,
    ) {
        for ly in 0..sh {
            let source_row = row_lo as usize + ly;
            let base_bit = source_row * row_bits + col_lo as usize;
            let (prev, cur) = sat.split_at_mut((ly + 1) * stride);
            let prev = &prev[ly * stride..ly * stride + stride];
            let cur = &mut cur[..stride];
            let mut rowsum = 0u32;
            for (lx, bitpos) in (base_bit..base_bit + sw).enumerate() {
                let byte = samples[bitpos >> 3];
                rowsum += ((byte >> (7 - (bitpos & 7))) & 1) as u32;
                cur[lx + 1] = prev[lx + 1] + rowsum;
            }
        }
    }

    /// The blocked parallel fill must reproduce the serial table exactly —
    /// including its zero sentinel row and column — for row counts that fall
    /// short of, land on, and overrun the block boundary.
    #[test]
    fn parallel_sat_fill_matches_serial_bit_exact() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for &(width, height) in &[
            (1usize, 1usize),
            (7, 3),
            (8, 8),
            (13, 1),
            (1, 97),
            (64, 64),
            (129, 251),
            (255, 129),
        ] {
            let row_bits = width.next_multiple_of(8);
            let samples: Vec<u8> = (0..(row_bits / 8) * height).map(|_| next() as u8).collect();
            // Exercise both a full-image window and an offset sub-window.
            for &(want_col_lo, want_row_lo) in &[(0usize, 0usize), (1, 1)] {
                let (col_lo, row_lo) = (
                    if want_col_lo < width { want_col_lo } else { 0 },
                    if want_row_lo < height { want_row_lo } else { 0 },
                );
                let (sw, sh) = (width - col_lo, height - row_lo);
                let (col_lo, row_lo) = (col_lo as u32, row_lo as u32);
                let stride = sw + 1;
                let entries = stride * (sh + 1);
                let mut want = vec![0u32; entries];
                serial_fill(
                    &mut want, &samples, stride, sw, sh, row_bits, col_lo, row_lo,
                );
                let mut got = vec![0u32; entries];
                BilevelIntegral::fill_parallel(
                    &mut got, &samples, stride, sw, sh, row_bits, col_lo, row_lo,
                );
                assert_eq!(got, want, "{width}x{height} window ({col_lo},{row_lo})");
            }
        }
    }
}
