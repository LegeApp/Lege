//! The CPU render surface and pixel compositing.
//!
//! The internal buffer is always 8-bit RGBA with **premultiplied alpha**,
//! matching the frozen output contract (`OutputFormat::Rgba8PremultipliedSrgb`)
//! so the common path is a straight ownership handoff. Compositing is
//! source-over in the device (sRGB-encoded) space — the same space every 8-bit
//! PDF compositor uses; linear-light compositing is a later, opt-in refinement.
//!
//! `Gray8` output is produced by a final downconversion, so there is a single
//! compositing path regardless of the requested format.

use pdf_page_ir::DeviceRect;
use pdf_render_api::{Background, OutputFormat};
use std::{mem::MaybeUninit, sync::Arc};

/// An RGBA8 premultiplied render target. A surface may be a sub-region of the
/// device plane: `origin` is the device coordinate of its local `(0, 0)`, so
/// bounded offscreen group buffers share the executor's absolute-coordinate
/// math (advice §9).
// `Clone` snapshots the pixel buffer — used by knockout groups, whose every
// element composites against the group's *initial* backdrop (§11.6.6).
#[derive(Debug)]
pub struct Surface {
    pub width: usize,
    pub height: usize,
    pub origin_x: usize,
    pub origin_y: usize,
    /// `width * height * 4` bytes, premultiplied RGBA, row-major.
    data: Arc<[u8]>,
}

// A clone is a knockout-group backdrop snapshot, not another handle to the
// same mutable render target. Keep that value semantics even though the main
// page now lives in its final Arc allocation.
impl Clone for Surface {
    fn clone(&self) -> Self {
        Self {
            width: self.width,
            height: self.height,
            origin_x: self.origin_x,
            origin_y: self.origin_y,
            data: Arc::from(self.data.as_ref()),
        }
    }
}

impl Surface {
    /// Allocate the main page surface (origin `(0,0)`) and paint its background.
    pub fn new(width: usize, height: usize, background: Background) -> Self {
        let pixels = width * height;
        let data = match background {
            Background::Transparent => filled_arc(pixels * 4, 0),
            Background::White => filled_arc(pixels * 4, 0xFF),
            Background::Solid(c) => {
                let a = c.a.clamp(0.0, 1.0);
                let px = [
                    to_u8(c.r.clamp(0.0, 1.0) * a),
                    to_u8(c.g.clamp(0.0, 1.0) * a),
                    to_u8(c.b.clamp(0.0, 1.0) * a),
                    to_u8(a),
                ];
                repeated_rgba_arc(pixels, px)
            }
        };
        Self {
            width,
            height,
            origin_x: 0,
            origin_y: 0,
            data,
        }
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
            data: filled_arc(width * height * 4, 0),
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
            let orow = &mut unique_arc_mut(&mut off.data)[ly * stride..(ly + 1) * stride];
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
        &mut unique_arc_mut(&mut self.data)[local * stride..(local + 1) * stride]
    }

    /// Contiguous mutable rows for absolute device-Y range `[y0, y1)`, clipped
    /// to this surface. Used by the axis-aligned image fast paths to paint
    /// independent rows in parallel without re-borrowing the surface each row.
    ///
    /// Returns `(buffer, first_absolute_y, row_stride_bytes)`.
    #[inline]
    pub(crate) fn rows_mut_abs(&mut self, y0: usize, y1: usize) -> (&mut [u8], usize, usize) {
        let stride = self.width * 4;
        let local0 = y0.saturating_sub(self.origin_y).min(self.height);
        let local1 = y1.saturating_sub(self.origin_y).min(self.height);
        let local0 = local0.min(local1);
        let first_abs_y = self.origin_y + local0;
        (
            &mut unique_arc_mut(&mut self.data)[local0 * stride..local1 * stride],
            first_abs_y,
            stride,
        )
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
    pub fn into_output(self, format: OutputFormat) -> (usize, Arc<[u8]>) {
        match format {
            OutputFormat::Rgba8PremultipliedSrgb => (self.width * 4, self.data),
            OutputFormat::Gray8 => {
                let mut out = Arc::<[u8]>::new_uninit_slice(self.width * self.height);
                let out_mut = unique_arc_mut(&mut out);
                for (dst, px) in out_mut.iter_mut().zip(self.data.chunks_exact(4)) {
                    // Rec. 709 luma of the premultiplied (composited-over-black)
                    // color.
                    let y = 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
                    dst.write(y.round().clamp(0.0, 255.0) as u8);
                }
                // SAFETY: the zip lengths are equal because the RGBA surface
                // has exactly four bytes per output pixel, so every element of
                // `out` was initialized above.
                (self.width, unsafe { out.assume_init() })
            }
        }
    }
}

/// Allocate the render buffer directly in the `Arc<[u8]>` layout returned by
/// `HostPage`. Going through a `Vec<u8>` forces `Arc::from(Vec<_>)` to allocate
/// and copy the entire page because the Arc refcounts need adjacent storage.
fn filled_arc(len: usize, byte: u8) -> Arc<[u8]> {
    if byte == 0 {
        let data = Arc::<[u8]>::new_zeroed_slice(len);
        // SAFETY: `new_zeroed_slice` initialized every `u8` to a valid zero.
        return unsafe { data.assume_init() };
    }
    let mut data = Arc::<[u8]>::new_uninit_slice(len);
    unique_arc_mut(&mut data).fill(MaybeUninit::new(byte));
    // SAFETY: `fill` initialized every element of the allocation.
    unsafe { data.assume_init() }
}

fn repeated_rgba_arc(pixels: usize, rgba: [u8; 4]) -> Arc<[u8]> {
    let mut data = Arc::<[u8]>::new_uninit_slice(pixels * 4);
    for chunk in unique_arc_mut(&mut data).chunks_exact_mut(4) {
        for (dst, src) in chunk.iter_mut().zip(rgba) {
            dst.write(src);
        }
    }
    // SAFETY: `chunks_exact_mut(4)` covers the complete pixels*4 allocation.
    unsafe { data.assume_init() }
}

#[inline]
fn to_u8(v: f32) -> u8 {
    (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

#[inline]
fn unique_arc_mut<T>(data: &mut Arc<[T]>) -> &mut [T] {
    let Some(data) = Arc::get_mut(data) else {
        unreachable!("a render surface is never shared while it is mutable")
    };
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_output_hands_off_the_surface_pixels() {
        let mut surface = Surface::new(2, 1, Background::White);
        surface.row_mut(0)[0] = 17;

        let (stride, output) = surface.into_output(OutputFormat::Rgba8PremultipliedSrgb);

        assert_eq!(stride, 8);
        assert_eq!(&*output, &[17, 255, 255, 255, 255, 255, 255, 255]);
        assert_eq!(Arc::strong_count(&output), 1);
    }

    #[test]
    fn clone_remains_an_independent_pixel_snapshot() {
        let original = Surface::new(
            1,
            1,
            Background::Solid(pdf_page_ir::Color {
                r: 0.25,
                g: 0.5,
                b: 0.75,
                a: 1.0,
            }),
        );
        let mut clone = original.clone();
        clone.row_mut(0).fill(0);

        assert_ne!(original.local_row(0), clone.local_row(0));
        assert_eq!(original.local_row(0), &[64, 128, 191, 255]);
    }

    #[test]
    fn gray_output_is_fully_initialized() {
        let surface = Surface::new(2, 1, Background::White);
        let (stride, output) = surface.into_output(OutputFormat::Gray8);

        assert_eq!(stride, 2);
        assert_eq!(&*output, &[255, 255]);
    }
}
