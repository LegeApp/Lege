//! The CPU render surface and pixel compositing.
//!
//! The internal buffer is always 8-bit RGBA with **premultiplied alpha**,
//! matching the frozen output contract (`OutputFormat::Rgba8PremultipliedSrgb`)
//! so the common path is a straight copy. Compositing is source-over in the
//! device (sRGB-encoded) space — the same space every 8-bit PDF compositor
//! uses; linear-light compositing is a later, opt-in refinement.
//!
//! `Gray8` output is produced by a final downconversion, so there is a single
//! compositing path regardless of the requested format.

use pdf_page_ir::DeviceRect;
use pdf_render_api::{Background, OutputFormat};

/// An RGBA8 premultiplied render target. A surface may be a sub-region of the
/// device plane: `origin` is the device coordinate of its local `(0, 0)`, so
/// bounded offscreen group buffers share the executor's absolute-coordinate
/// math (advice §9).
// `Clone` snapshots the pixel buffer — used by knockout groups, whose every
// element composites against the group's *initial* backdrop (§11.6.6).
#[derive(Debug, Clone)]
pub struct Surface {
    pub width: usize,
    pub height: usize,
    pub origin_x: usize,
    pub origin_y: usize,
    /// `width * height * 4` bytes, premultiplied RGBA, row-major.
    data: Vec<u8>,
}

impl Surface {
    /// Allocate the main page surface (origin `(0,0)`) and paint its background.
    pub fn new(width: usize, height: usize, background: Background) -> Self {
        let mut data = vec![0u8; width * height * 4];
        match background {
            Background::Transparent => {}
            Background::White => data.fill(0xFF),
            Background::Solid(c) => {
                let a = c.a.clamp(0.0, 1.0);
                let px = [
                    to_u8(c.r.clamp(0.0, 1.0) * a),
                    to_u8(c.g.clamp(0.0, 1.0) * a),
                    to_u8(c.b.clamp(0.0, 1.0) * a),
                    to_u8(a),
                ];
                for chunk in data.chunks_exact_mut(4) {
                    chunk.copy_from_slice(&px);
                }
            }
        }
        Self { width, height, origin_x: 0, origin_y: 0, data }
    }

    /// Allocate a transparent offscreen surface covering `bounds` (device
    /// space) — for an isolated transparency group.
    pub fn offscreen(bounds: DeviceRect) -> Self {
        let width = bounds.width as usize;
        let height = bounds.height as usize;
        Self {
            width,
            height,
            origin_x: bounds.x.max(0) as usize,
            origin_y: bounds.y.max(0) as usize,
            data: vec![0u8; width * height * 4],
        }
    }

    /// Allocate an offscreen covering `bounds` (device space) **seeded with the
    /// parent's backdrop** — for a *non-isolated* transparency group, whose
    /// elements composite against the group's backdrop rather than a transparent
    /// one (ISO 32000-1 §11.4.7; mirrors PDFium's `GetDIBits` +
    /// `CreateWithBackdrop` in `CPDF_RenderStatus::ProcessTransparency`). Pixels
    /// of `bounds` outside `parent` are left transparent.
    pub fn offscreen_seeded(parent: &Surface, bounds: DeviceRect) -> Self {
        let mut off = Surface::offscreen(bounds);
        let stride = off.width * 4;
        for ly in 0..off.height {
            let abs_y = off.origin_y + ly;
            if abs_y < parent.origin_y || abs_y >= parent.origin_y + parent.height {
                continue;
            }
            let prow = parent.local_row(abs_y - parent.origin_y);
            let orow = &mut off.data[ly * stride..(ly + 1) * stride];
            for lx in 0..off.width {
                let abs_x = off.origin_x + lx;
                if abs_x < parent.origin_x || abs_x >= parent.origin_x + parent.width {
                    continue;
                }
                let src = &prow[(abs_x - parent.origin_x) * 4..(abs_x - parent.origin_x) * 4 + 4];
                orow[lx * 4..lx * 4 + 4].copy_from_slice(src);
            }
        }
        off
    }

    pub fn bytes(&self) -> u64 {
        self.data.len() as u64
    }

    /// Mutable access to one row's RGBA bytes, addressed by **absolute** device
    /// `y` — the buffer the span kernels write into.
    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut [u8] {
        let stride = self.width * 4;
        let local = y - self.origin_y;
        &mut self.data[local * stride..(local + 1) * stride]
    }

    /// Immutable access to one row by **local** index (for compositing a group
    /// back onto its parent).
    #[inline]
    pub fn local_row(&self, local_y: usize) -> &[u8] {
        let stride = self.width * 4;
        &self.data[local_y * stride..(local_y + 1) * stride]
    }

    /// Consume the surface into output bytes of the requested format.
    /// Returns `(stride, pixels)`.
    pub fn into_output(self, format: OutputFormat) -> (usize, Vec<u8>) {
        match format {
            OutputFormat::Rgba8PremultipliedSrgb => (self.width * 4, self.data),
            OutputFormat::Gray8 => {
                let mut out = vec![0u8; self.width * self.height];
                for (i, px) in self.data.chunks_exact(4).enumerate() {
                    // Rec. 709 luma of the premultiplied (composited-over-black)
                    // color.
                    let y = 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
                    out[i] = y.round().clamp(0.0, 255.0) as u8;
                }
                (self.width, out)
            }
        }
    }

}

#[inline]
fn to_u8(v: f32) -> u8 {
    (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}
