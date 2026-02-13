// src/image_formats.rs

//! In-memory representations for color and grayscale images.
//!
//! This module replaces the C++ `GPixmap` and `GBitmap` classes. It uses the
//! `image` crate to provide safe and efficient image buffers. An extension
//! trait, `DjvuImageExt`, is defined to add DjVu-specific rendering operations
//! like `stencil` and `attenuate` directly to these standard image types.

use crate::image::geom::Rect;
use ::image::{GrayImage as LumaImage, Luma, Rgb, RgbImage};

// --- Type Aliases for Clarity ---

/// A color image buffer, equivalent to `GPixmap`.
/// Each pixel is an `Rgb<u8>`.
pub type Pixmap = RgbImage;

/// A grayscale (luma) image buffer, equivalent to `GBitmap`.
/// Each pixel is a `Luma<u8>`.
pub type Bitmap = LumaImage;

/// An RGB pixel, equivalent to `GPixel`.
pub type Pixel = Rgb<u8>;

/// A grayscale pixel.
pub type GrayPixel = Luma<u8>;

/// An extension trait for DjVu-specific image manipulation operations.
pub trait DjvuImageExt {
    /// Attenuates the pixmap's colors based on an alpha mask.
    ///
    /// This operation modifies the pixmap in-place. For each pixel, the new
    /// color `C'` is calculated from the original color `C` and the alpha
    /// value `A` (from 0.0 to 1.0) as: `C' = C * (1.0 - A)`.
    ///
    /// # Arguments
    /// * `mask` - A grayscale `Bitmap` where pixel values represent the alpha channel.
    /// * `x_pos`, `y_pos` - The top-left coordinates where the mask should be applied.
    fn attenuate(&mut self, mask: &Bitmap, x_pos: i32, y_pos: i32);

    /// Blends a solid color onto the pixmap using an alpha mask.
    ///
    /// This operation modifies the pixmap in-place. For each pixel, the new
    /// color `C'` is calculated from the original color `C`, the blend color `B`,
    /// and the alpha value `A` as: `C' = C + B * A`.
    ///
    /// # Arguments
    /// * `mask` - The alpha mask.
    /// * `x_pos`, `y_pos` - The top-left position to apply the blend.
    /// * `color` - The solid color to blend onto the image.
    fn blit_solid(&mut self, mask: &Bitmap, x_pos: i32, y_pos: i32, color: &Pixel);

    /// Blends a foreground pixmap onto a background pixmap using an alpha mask.
    ///
    /// This is the core "stencil" operation for compositing DjVu layers.
    /// New Color = `Background * (1 - Alpha) + Foreground * Alpha`.
    ///
    /// # Arguments
    /// * `mask` - The alpha mask.
    /// * `foreground` - The pixmap to blend on top.
    /// * `x_pos`, `y_pos` - The top-left position for the operation.
    fn stencil(&mut self, mask: &Bitmap, foreground: &Pixmap, x_pos: i32, y_pos: i32);
}

impl DjvuImageExt for Pixmap {
    fn attenuate(&mut self, mask: &Bitmap, x_pos: i32, y_pos: i32) {
        // Determine the overlapping rectangle to iterate over.
        let self_rect = Rect::new(0, 0, self.width(), self.height());
        let mask_rect = Rect::new(x_pos, y_pos, mask.width(), mask.height());
        let overlap = self_rect.intersection(&mask_rect);

        if overlap.is_empty() {
            return;
        }

        // Pre-calculate multipliers for performance.
        // The mask values are inverted (0 = transparent, 255 = opaque).
        let grays = 255; // Assume mask is 8-bit
        let multipliers: Vec<u32> = (0..=grays)
            .map(|i| 0x10000 * i as u32 / grays as u32)
            .collect();

        for y in 0..overlap.height {
            for x in 0..overlap.width {
                let self_x = (overlap.x + x as i32) as u32;
                let self_y = (overlap.y + y as i32) as u32;
                let mask_x = (self_x as i32 - x_pos) as u32;
                let mask_y = (self_y as i32 - y_pos) as u32;

                let alpha_val = mask.get_pixel(mask_x, mask_y).0[0];
                if alpha_val == 0 {
                    continue;
                }

                let bg_pixel = self.get_pixel_mut(self_x, self_y);

                if alpha_val == 255 {
                    // Fully opaque mask, color becomes black.
                    *bg_pixel = Rgb([0, 0, 0]);
                } else {
                    let level = multipliers[alpha_val as usize];
                    bg_pixel.0[0] =
                        (bg_pixel.0[0] as u32 - ((bg_pixel.0[0] as u32 * level) >> 16)) as u8;
                    bg_pixel.0[1] =
                        (bg_pixel.0[1] as u32 - ((bg_pixel.0[1] as u32 * level) >> 16)) as u8;
                    bg_pixel.0[2] =
                        (bg_pixel.0[2] as u32 - ((bg_pixel.0[2] as u32 * level) >> 16)) as u8;
                }
            }
        }
    }

    fn blit_solid(&mut self, mask: &Bitmap, x_pos: i32, y_pos: i32, color: &Pixel) {
        let self_rect = Rect::new(0, 0, self.width(), self.height());
        let mask_rect = Rect::new(x_pos, y_pos, mask.width(), mask.height());
        let overlap = self_rect.intersection(&mask_rect);

        if overlap.is_empty() {
            return;
        }

        let multipliers: Vec<u32> = (0..=255).map(|i| 0x10000 * i as u32 / 255).collect();

        for y in 0..overlap.height {
            for x in 0..overlap.width {
                let self_x = (overlap.x + x as i32) as u32;
                let self_y = (overlap.y + y as i32) as u32;
                let mask_x = (self_x as i32 - x_pos) as u32;
                let mask_y = (self_y as i32 - y_pos) as u32;

                let alpha_val = mask.get_pixel(mask_x, mask_y).0[0];
                if alpha_val == 0 {
                    continue;
                }

                let dest_pixel = self.get_pixel_mut(self_x, self_y);

                if alpha_val == 255 {
                    dest_pixel.0[0] = dest_pixel.0[0].saturating_add(color.0[0]);
                    dest_pixel.0[1] = dest_pixel.0[1].saturating_add(color.0[1]);
                    dest_pixel.0[2] = dest_pixel.0[2].saturating_add(color.0[2]);
                } else {
                    let level = multipliers[alpha_val as usize];
                    dest_pixel.0[0] =
                        dest_pixel.0[0].saturating_add(((color.0[0] as u32 * level) >> 16) as u8);
                    dest_pixel.0[1] =
                        dest_pixel.0[1].saturating_add(((color.0[1] as u32 * level) >> 16) as u8);
                    dest_pixel.0[2] =
                        dest_pixel.0[2].saturating_add(((color.0[2] as u32 * level) >> 16) as u8);
                }
            }
        }
    }

    fn stencil(&mut self, mask: &Bitmap, foreground: &Pixmap, x_pos: i32, y_pos: i32) {
        let self_rect = Rect::new(0, 0, self.width(), self.height());
        let op_rect = Rect::new(x_pos, y_pos, mask.width(), mask.height());
        let overlap = self_rect.intersection(&op_rect);

        if overlap.is_empty() {
            return;
        }

        // This is a direct port of the logic:
        // C' = C_bg - (C_bg - C_fg) * Alpha
        // which is equivalent to: C_bg * (1 - Alpha) + C_fg * Alpha
        let multipliers: Vec<u32> = (0..=255).map(|i| 0x10000 * i as u32 / 255).collect();

        for y in 0..overlap.height {
            for x in 0..overlap.width {
                let self_x = (overlap.x + x as i32) as u32;
                let self_y = (overlap.y + y as i32) as u32;
                let mask_x = (self_x as i32 - x_pos) as u32;
                let mask_y = (self_y as i32 - y_pos) as u32;

                let alpha_val = mask.get_pixel(mask_x, mask_y).0[0];
                if alpha_val == 0 {
                    continue;
                }

                let bg_pixel = self.get_pixel_mut(self_x, self_y);
                let fg_pixel = foreground.get_pixel(mask_x, mask_y);

                if alpha_val == 255 {
                    *bg_pixel = *fg_pixel;
                } else {
                    let level = multipliers[alpha_val as usize];
                    // Component-wise blend
                    for i in 0..3 {
                        let bg = bg_pixel.0[i] as i32;
                        let fg = fg_pixel.0[i] as i32;
                        let blended = bg - (((bg - fg) * level as i32) >> 16);
                        bg_pixel.0[i] = blended as u8;
                    }
                }
            }
        }
    }
}
