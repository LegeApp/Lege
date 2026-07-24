//! Lazy region-local clip masks (performance advice §8).
//!
//! A non-rectangular clip contributes an `Alpha8` coverage mask. It is built
//! **on demand**, once per page execution, sized to the clip's device
//! envelope (never the full page), and cached by dense `ClipId`. Purely
//! rectangular clip chains never reach here — they are handled by arithmetic
//! bounds intersection during lowering.

use pdf_page_ir::DeviceRect;

use crate::kernels::mul_div_255;
use crate::prepared::{ClipKind, CpuPreparedPage};
use crate::raster::RasterKernel;

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
pub fn build_clip_mask(raster: &mut RasterKernel, page: &CpuPreparedPage, cid: u32) -> ClipMask {
    let cb = page.clips[cid as usize].bounds;
    let w = cb.width as usize;
    let h = cb.height as usize;
    let (bx, by) = (cb.x as usize, cb.y as usize);
    let ow = page.size.width as usize;
    let oh = page.size.height as usize;

    let mut acc = vec![255u8; w * h];
    let mut tmp = vec![0u8; w * h];

    let mut cur = Some(cid);
    while let Some(id) = cur {
        let clip = &page.clips[id as usize];
        if let ClipKind::Path { subpath_range, rule } = clip.kind {
            let subs = &page.subpaths[subpath_range.0 as usize..subpath_range.1 as usize];
            tmp.iter_mut().for_each(|t| *t = 0);
            raster.fill(&page.points, subs, ow, oh, rule, |y, x0, x1, cov| {
                if y < by || y >= by + h {
                    return;
                }
                let ly = y - by;
                let lx0 = x0.max(bx);
                let lx1 = x1.min(bx + w - 1);
                for x in lx0..=lx1 {
                    tmp[ly * w + (x - bx)] = cov[x];
                }
            });
            for (a, &t) in acc.iter_mut().zip(&tmp) {
                *a = mul_div_255(*a as u16, t as u16) as u8;
            }
        }
        cur = clip.parent;
    }

    ClipMask { bounds: cb, data: acc, outside: 0 }
}
