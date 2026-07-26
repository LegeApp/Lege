//! Lazy region-local clip masks (performance advice §8).
//!
//! A non-rectangular clip contributes an `Alpha8` coverage mask. It is built
//! **on demand**, once per page execution, sized to the clip's device
//! envelope (never the full page), and cached by dense `ClipId`. Purely
//! rectangular clip chains never reach here — they are handled by arithmetic
//! bounds intersection during lowering.

use pdf_page_ir::DeviceRect;

use crate::kernels::mul_div_255;
use crate::prepared::{ClipKind, CpuPreparedPage, PreparedClip};
use crate::raster::RasterKernel;

/// The immutable, `Sync` slice of a prepared page that mask building reads. The
/// full [`CpuPreparedPage`] is `!Sync` (diagnostic cells), so masks are built
/// from this borrow instead — letting a whole page's masks build in parallel.
#[derive(Clone, Copy)]
pub struct ClipGeom<'a> {
    pub clips: &'a [PreparedClip],
    pub subpaths: &'a [(usize, usize)],
    pub points: &'a [[f32; 2]],
    pub width: usize,
    pub height: usize,
}

impl<'a> ClipGeom<'a> {
    pub fn of(page: &'a CpuPreparedPage) -> Self {
        Self {
            clips: &page.clips,
            subpaths: &page.subpaths,
            points: &page.points,
            width: page.size.width as usize,
            height: page.size.height as usize,
        }
    }
}

/// An 8-bit coverage mask over a device rectangle.
#[derive(Debug)]
pub struct ClipMask {
    pub bounds: DeviceRect,
    /// `bounds.width * bounds.height` coverage bytes, row-major.
    pub data: Vec<u8>,
    /// Coverage value for pixels *outside* `bounds`. Clip masks and ordinary
    /// soft masks use 0 (fully masked); a luminosity soft mask with a `/BC`
    /// backdrop uses the backdrop's luminosity (ISO 32000-1 §11.6.5.2).
    pub outside: u8,
    /// Whether `data` is entirely 255 (the clip is exactly its rectangular
    /// envelope, so a draw can drop the mask and use `bounds` alone). Computed
    /// once at build so consumers never rescan the whole mask — a per-glyph-run
    /// rescan was O(runs × mask area), the dominant cost on clipped-text pages.
    pub all_opaque: bool,
}

impl ClipMask {
    #[inline]
    pub fn stride(&self) -> usize {
        self.bounds.width as usize
    }
}

/// Build the combined mask for clip `cid`: the product of every path clip in
/// its ancestor chain, over the clip's rectangular envelope. Rectangular
/// ancestors contribute only to the envelope (already reflected in `bounds`).
pub fn build_clip_mask(raster: &mut RasterKernel, geom: ClipGeom, cid: u32) -> ClipMask {
    // The non-rendering attribution path has no cancellation token.
    if let Some(mask) = build_clip_mask_cancellable(raster, geom, cid, None) {
        return mask;
    }
    // `None` is unreachable without a probe, but keep the never-panic policy
    // even if that invariant changes later.
    let bounds = geom.clips[cid as usize].bounds;
    ClipMask {
        bounds,
        data: vec![0; bounds.width as usize * bounds.height as usize],
        outside: 0,
        all_opaque: false,
    }
}

/// Cancellable form of [`build_clip_mask`]. A cancelled partial mask is
/// discarded rather than cached or consumed by a draw.
pub fn build_clip_mask_cancellable(
    raster: &mut RasterKernel,
    geom: ClipGeom,
    cid: u32,
    should_cancel: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> Option<ClipMask> {
    let cb = geom.clips[cid as usize].bounds;
    let w = cb.width as usize;
    let h = cb.height as usize;
    let (bx, by) = (cb.x as usize, cb.y as usize);
    let ow = geom.width;
    let oh = geom.height;

    // `acc` starts empty (fully masked). The first path clip is filled directly
    // into it; later path clips intersect via `tmp` + multiply. Most chains hold
    // exactly one path clip (rect ancestors contribute only to `bounds`), so the
    // common case does one pass — no zeroed scratch, no per-pixel multiply. On
    // clip-dense maps this is the dominant per-page cost, once per basin polygon.
    let mut acc = vec![0u8; w * h];
    let mut tmp: Vec<u8> = Vec::new();
    let mut path_seen = 0u32;

    let mut cur = Some(cid);
    while let Some(id) = cur {
        let clip = &geom.clips[id as usize];
        if let ClipKind::Path {
            subpath_range,
            rule,
        } = clip.kind
        {
            let subs = &geom.subpaths[subpath_range.0 as usize..subpath_range.1 as usize];
            path_seen += 1;
            if path_seen == 1 {
                // First (usually only) path: write coverage straight into `acc`,
                // which is 0 everywhere the fill does not touch (correctly masked).
                if !raster.fill_cancellable(
                    &geom.points,
                    subs,
                    ow,
                    oh,
                    rule,
                    should_cancel,
                    |y, x0, x1, cov| {
                        if y < by || y >= by + h {
                            return;
                        }
                        let ly = y - by;
                        let lx0 = x0.max(bx);
                        let lx1 = x1.min(bx + w - 1);
                        for x in lx0..=lx1 {
                            acc[ly * w + (x - bx)] = cov[x];
                        }
                    },
                ) {
                    return None;
                }
            } else {
                // Additional path clip: rasterize into `tmp` and multiply in.
                if tmp.is_empty() {
                    tmp = vec![0u8; w * h];
                } else {
                    tmp.iter_mut().for_each(|t| *t = 0);
                }
                if !raster.fill_cancellable(
                    &geom.points,
                    subs,
                    ow,
                    oh,
                    rule,
                    should_cancel,
                    |y, x0, x1, cov| {
                        if y < by || y >= by + h {
                            return;
                        }
                        let ly = y - by;
                        let lx0 = x0.max(bx);
                        let lx1 = x1.min(bx + w - 1);
                        for x in lx0..=lx1 {
                            tmp[ly * w + (x - bx)] = cov[x];
                        }
                    },
                ) {
                    return None;
                }
                for (index, (a, &t)) in acc.iter_mut().zip(&tmp).enumerate() {
                    if index & 0xffff == 0 && should_cancel.is_some_and(|cancel| cancel()) {
                        return None;
                    }
                    *a = mul_div_255(*a as u16, t as u16) as u8;
                }
            }
        }
        cur = clip.parent;
    }

    let all_opaque = acc.iter().all(|&v| v == 255);
    Some(ClipMask {
        bounds: cb,
        data: acc,
        outside: 0,
        all_opaque,
    })
}
