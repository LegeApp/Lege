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
    /// The surface's pixel count, or `None` when `width * height * 4` does not
    /// fit a `usize`.
    ///
    /// Release builds have `overflow-checks = false`, so an unchecked
    /// `width * height * 4` would wrap silently and leave a small allocation
    /// paired with large `width`/`height` fields — the buffer and the geometry
    /// would disagree, which is the shape every `assume_init` and raw-store in
    /// this module relies on being impossible. Callers already bound the
    /// output against `max_page_bytes`, so this only ever fires on a caller
    /// bug; degrade to an empty surface rather than a torn one.
    fn checked_pixels(width: usize, height: usize) -> Option<usize> {
        let pixels = width.checked_mul(height)?;
        pixels.checked_mul(4).map(|_| pixels)
    }

    /// Allocate the main page surface (origin `(0,0)`) and paint its background.
    pub fn new(width: usize, height: usize, background: Background) -> Self {
        let Some(pixels) = Self::checked_pixels(width, height) else {
            return Self::empty();
        };
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
        // Same reasoning as `Surface::new`: group bounds are clamped to the
        // page in lowering, so this cannot overflow in practice — but the
        // buffer and the geometry must never be allowed to disagree.
        let Some(pixels) = Self::checked_pixels(width, height) else {
            return Self::empty();
        };
        Self {
            width,
            height,
            origin_x: bounds.x.max(0) as usize,
            origin_y: bounds.y.max(0) as usize,
            data: filled_arc(pixels * 4, 0),
        }
    }

    /// A zero-sized surface. Every row/pixel loop over it is a no-op, so it is
    /// the safe degradation for a geometry that cannot be allocated.
    fn empty() -> Self {
        Self {
            width: 0,
            height: 0,
            origin_x: 0,
            origin_y: 0,
            data: filled_arc(0, 0),
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
        // The overlap is a rectangle, so its column span is the same on every
        // row: resolve it once and copy each row in one `copy_from_slice`
        // rather than testing and copying pixel by pixel. This is the common
        // wrapper case for non-isolated groups, so it runs over whole-page
        // areas.
        let x_start = off.origin_x.max(parent.origin_x);
        let x_end = (off.origin_x + off.width).min(parent.origin_x + parent.width);
        if x_end <= x_start {
            return off;
        }
        let span = (x_end - x_start) * 4;
        let dst_off = (x_start - off.origin_x) * 4;
        let src_off = (x_start - parent.origin_x) * 4;

        let y_start = off.origin_y.max(parent.origin_y);
        let y_end = (off.origin_y + off.height).min(parent.origin_y + parent.height);
        for abs_y in y_start..y_end {
            let ly = abs_y - off.origin_y;
            let prow = parent.local_row(abs_y - parent.origin_y);
            let orow = &mut unique_arc_mut(&mut off.data)[ly * stride..(ly + 1) * stride];
            orow[dst_off..dst_off + span].copy_from_slice(&prow[src_off..src_off + span]);
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
    #[allow(
        unsafe_code,
        reason = "Arc::new_uninit_slice hands back MaybeUninit; see SAFETY comment"
    )]
    pub fn into_output(self, format: OutputFormat) -> (usize, Arc<[u8]>) {
        match format {
            OutputFormat::Rgba8PremultipliedSrgb => (self.width * 4, self.data),
            OutputFormat::Gray8 => {
                let pixels = self.width * self.height;
                // `zip` stops at the shorter side. The RGBA buffer is exactly
                // four bytes per output pixel by construction, so the two
                // lengths agree — but "the invariant holds" is not something
                // `assume_init` may be asked to take on trust: a short buffer
                // would leave a tail of `out` uninitialized and reading it is
                // UB, not a panic. Count what was written and initialize any
                // remainder explicitly, so soundness does not depend on the
                // invariant at all.
                debug_assert_eq!(self.data.len(), pixels * 4);
                let mut out = Arc::<[u8]>::new_uninit_slice(pixels);
                let out_mut = unique_arc_mut(&mut out);
                let mut written = 0usize;
                for (dst, px) in out_mut.iter_mut().zip(self.data.chunks_exact(4)) {
                    // Rec. 709 luma of the premultiplied (composited-over-black)
                    // color.
                    let y = 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
                    dst.write(y.round().clamp(0.0, 255.0) as u8);
                    written += 1;
                }
                for dst in &mut out_mut[written..] {
                    dst.write(0);
                }
                // SAFETY: the loop initialized `out[..written]` and the fill
                // above initialized `out[written..]`, so every element of
                // `out` is initialized regardless of the source length.
                (self.width, unsafe { out.assume_init() })
            }
        }
    }
}

/// Allocate the render buffer directly in the `Arc<[u8]>` layout returned by
/// `HostPage`. Going through a `Vec<u8>` forces `Arc::from(Vec<_>)` to allocate
/// and copy the entire page because the Arc refcounts need adjacent storage.
#[allow(
    unsafe_code,
    reason = "Arc::new_uninit_slice/new_zeroed_slice hand back MaybeUninit; see SAFETY comments"
)]
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

#[allow(
    unsafe_code,
    reason = "Arc::new_uninit_slice/new_zeroed_slice hand back MaybeUninit; see SAFETY comments"
)]
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
