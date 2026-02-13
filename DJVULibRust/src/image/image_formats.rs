// src/image_formats.rs

//! In-memory representations for color and grayscale images.
//!
//! This module provides lightweight, custom implementations of `Pixel`, `Pixmap`,
//! `Bitmap`, and `GrayPixel` types optimized for DjVu encoding. These replace
//! the dependency on the `image` crate while maintaining the same interface.
//!
//! An extension trait, `DjvuImageExt`, adds DjVu-specific rendering operations
//! like `stencil`, `attenuate`, and `blit_solid`.

use crate::image::geom::Rect;
use bytemuck::{Pod, Zeroable};

// --- Pixel Type Definitions ---

/// A single RGB pixel with 8-bit components.
/// This is the basic unit for color images.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Pixel {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

unsafe impl Pod for Pixel {}
unsafe impl Zeroable for Pixel {}

impl Pixel {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Pixel { r, g, b }
    }

    pub fn black() -> Self {
        Pixel { r: 0, g: 0, b: 0 }
    }

    pub fn white() -> Self {
        Pixel {
            r: 255,
            g: 255,
            b: 255,
        }
    }
}

impl From<[u8; 3]> for Pixel {
    fn from(arr: [u8; 3]) -> Self {
        Pixel {
            r: arr[0],
            g: arr[1],
            b: arr[2],
        }
    }
}

impl Into<[u8; 3]> for Pixel {
    fn into(self) -> [u8; 3] {
        [self.r, self.g, self.b]
    }
}

/// A single grayscale pixel with an 8-bit intensity value.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GrayPixel {
    pub y: u8,
}

unsafe impl Pod for GrayPixel {}
unsafe impl Zeroable for GrayPixel {}

impl GrayPixel {
    pub fn new(y: u8) -> Self {
        GrayPixel { y }
    }

    pub fn black() -> Self {
        GrayPixel { y: 0 }
    }

    pub fn white() -> Self {
        GrayPixel { y: 255 }
    }
}

// --- Pixmap Type (Color Image Buffer) ---

/// A 2D buffer of color pixels, equivalent to the C++ `GPixmap`.
/// Stores pixels in row-major order.
#[derive(Clone, Debug)]
pub struct Pixmap {
    width: u32,
    height: u32,
    data: Vec<Pixel>,
}

impl Pixmap {
    /// Creates a new pixmap with the given dimensions, initialized to black.
    pub fn new(width: u32, height: u32) -> Self {
        Pixmap {
            width,
            height,
            data: vec![Pixel::black(); (width * height) as usize],
        }
    }

    /// Creates a pixmap from a raw vector of pixels.
    /// Assumes the vector is already in row-major order.
    pub fn from_vec(width: u32, height: u32, data: Vec<Pixel>) -> Self {
        assert_eq!(data.len(), (width * height) as usize);
        Pixmap {
            width,
            height,
            data,
        }
    }

    /// Creates a pixmap filled with a single pixel value.
    pub fn from_pixel(width: u32, height: u32, pixel: Pixel) -> Self {
        Pixmap {
            width,
            height,
            data: vec![pixel; (width * height) as usize],
        }
    }

    /// Creates a pixmap by calling a function for each pixel.
    pub fn from_fn<F>(width: u32, height: u32, mut f: F) -> Self
    where
        F: FnMut(u32, u32) -> Pixel,
    {
        let mut data = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                data.push(f(x, y));
            }
        }
        Pixmap {
            width,
            height,
            data,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn get_pixel(&self, x: u32, y: u32) -> Pixel {
        assert!(x < self.width && y < self.height);
        self.data[(y * self.width + x) as usize]
    }

    pub fn get_pixel_mut(&mut self, x: u32, y: u32) -> &mut Pixel {
        assert!(x < self.width && y < self.height);
        &mut self.data[(y * self.width + x) as usize]
    }

    pub fn put_pixel(&mut self, x: u32, y: u32, pixel: Pixel) {
        assert!(x < self.width && y < self.height);
        self.data[(y * self.width + x) as usize] = pixel;
    }

    pub fn pixels(&self) -> &[Pixel] {
        &self.data
    }

    pub fn pixels_mut(&mut self) -> &mut [Pixel] {
        &mut self.data
    }

    /// Returns the dimensions as a tuple (width, height).
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Returns raw pixel data as a byte slice.
    pub fn as_raw(&self) -> &[u8] {
        bytemuck::cast_slice(&self.data)
    }

    /// Returns mutable raw pixel data as a byte slice.
    pub fn as_raw_mut(&mut self) -> &mut [u8] {
        bytemuck::cast_slice_mut(&mut self.data)
    }

    pub fn to_bitmap(&self) -> Bitmap {
        let data = self
            .data
            .iter()
            .map(|p| {
                // Convert RGB to grayscale using standard luminance formula
                let gray = (0.299 * p.r as f32 + 0.587 * p.g as f32 + 0.114 * p.b as f32) as u8;
                GrayPixel::new(gray)
            })
            .collect();
        Bitmap {
            width: self.width,
            height: self.height,
            data,
        }
    }
}

// --- Bitmap Type (Grayscale Image Buffer) ---

/// A 2D buffer of grayscale pixels, equivalent to the C++ `GBitmap`.
/// Stores pixels in row-major order.
#[derive(Clone, Debug)]
pub struct Bitmap {
    width: u32,
    height: u32,
    data: Vec<GrayPixel>,
}

impl Bitmap {
    /// Creates a new bitmap with the given dimensions, initialized to black.
    pub fn new(width: u32, height: u32) -> Self {
        Bitmap {
            width,
            height,
            data: vec![GrayPixel::black(); (width * height) as usize],
        }
    }

    /// Creates a bitmap from a raw vector of pixels.
    pub fn from_vec(width: u32, height: u32, data: Vec<GrayPixel>) -> Self {
        assert_eq!(data.len(), (width * height) as usize);
        Bitmap {
            width,
            height,
            data,
        }
    }

    /// Creates a bitmap filled with a single pixel value.
    pub fn from_pixel(width: u32, height: u32, pixel: GrayPixel) -> Self {
        Bitmap {
            width,
            height,
            data: vec![pixel; (width * height) as usize],
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn get_pixel(&self, x: u32, y: u32) -> GrayPixel {
        assert!(x < self.width && y < self.height);
        self.data[(y * self.width + x) as usize]
    }

    pub fn get_pixel_mut(&mut self, x: u32, y: u32) -> &mut GrayPixel {
        assert!(x < self.width && y < self.height);
        &mut self.data[(y * self.width + x) as usize]
    }

    pub fn put_pixel(&mut self, x: u32, y: u32, pixel: GrayPixel) {
        assert!(x < self.width && y < self.height);
        self.data[(y * self.width + x) as usize] = pixel;
    }

    pub fn pixels(&self) -> &[GrayPixel] {
        &self.data
    }

    pub fn pixels_mut(&mut self) -> &mut [GrayPixel] {
        &mut self.data
    }

    /// Returns the dimensions as a tuple (width, height).
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Returns raw pixel data as a byte slice.
    pub fn as_raw(&self) -> &[u8] {
        bytemuck::cast_slice(&self.data)
    }

    /// Returns mutable raw pixel data as a byte slice.
    pub fn as_raw_mut(&mut self) -> &mut [u8] {
        bytemuck::cast_slice_mut(&mut self.data)
    }
}

/// An extension trait for DjVu-specific image manipulation operations.
pub trait DjvuImageExt {
    /// Attenuates the pixmap's colors based on an alpha mask.
    ///
    /// This operation modifies the pixmap in-place. For each pixel, the new
    /// color `C'` is calculated from the original color `C` and the alpha
    /// value `A` (from 0.0 to 1.0) as: `C' = C * (1.0 - A)`.
    fn attenuate(&mut self, mask: &Bitmap, x_pos: i32, y_pos: i32);

    /// Blends a solid color onto the pixmap using an alpha mask.
    ///
    /// This operation modifies the pixmap in-place. For each pixel, the new
    /// color `C'` is calculated from the original color `C`, the blend color `B`,
    /// and the alpha value `A` as: `C' = C + B * A`.
    fn blit_solid(&mut self, mask: &Bitmap, x_pos: i32, y_pos: i32, color: &Pixel);

    /// Blends a foreground pixmap onto a background pixmap using an alpha mask.
    ///
    /// This is the core "stencil" operation for compositing DjVu layers.
    /// New Color = `Background * (1 - Alpha) + Foreground * Alpha`.
    fn stencil(&mut self, mask: &Bitmap, foreground: &Pixmap, x_pos: i32, y_pos: i32);
}

impl DjvuImageExt for Pixmap {
    fn attenuate(&mut self, mask: &Bitmap, x_pos: i32, y_pos: i32) {
        let self_rect = Rect::new(0, 0, self.width, self.height);
        let mask_rect = Rect::new(x_pos, y_pos, mask.width, mask.height);
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

                let alpha_val = mask.get_pixel(mask_x, mask_y).y;
                if alpha_val == 0 {
                    continue;
                }

                let pixel = self.get_pixel_mut(self_x, self_y);

                if alpha_val == 255 {
                    *pixel = Pixel::black();
                } else {
                    let level = multipliers[alpha_val as usize];
                    pixel.r = (pixel.r as u32 - ((pixel.r as u32 * level) >> 16)) as u8;
                    pixel.g = (pixel.g as u32 - ((pixel.g as u32 * level) >> 16)) as u8;
                    pixel.b = (pixel.b as u32 - ((pixel.b as u32 * level) >> 16)) as u8;
                }
            }
        }
    }

    fn blit_solid(&mut self, mask: &Bitmap, x_pos: i32, y_pos: i32, color: &Pixel) {
        let self_rect = Rect::new(0, 0, self.width, self.height);
        let mask_rect = Rect::new(x_pos, y_pos, mask.width, mask.height);
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

                let alpha_val = mask.get_pixel(mask_x, mask_y).y;
                if alpha_val == 0 {
                    continue;
                }

                let pixel = self.get_pixel_mut(self_x, self_y);

                if alpha_val == 255 {
                    pixel.r = pixel.r.saturating_add(color.r);
                    pixel.g = pixel.g.saturating_add(color.g);
                    pixel.b = pixel.b.saturating_add(color.b);
                } else {
                    let level = multipliers[alpha_val as usize];
                    pixel.r = pixel.r.saturating_add(((color.r as u32 * level) >> 16) as u8);
                    pixel.g = pixel.g.saturating_add(((color.g as u32 * level) >> 16) as u8);
                    pixel.b = pixel.b.saturating_add(((color.b as u32 * level) >> 16) as u8);
                }
            }
        }
    }

    fn stencil(&mut self, mask: &Bitmap, foreground: &Pixmap, x_pos: i32, y_pos: i32) {
        let self_rect = Rect::new(0, 0, self.width, self.height);
        let mask_rect = Rect::new(x_pos, y_pos, mask.width, mask.height);
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

                let alpha_val = mask.get_pixel(mask_x, mask_y).y;
                if alpha_val == 0 {
                    continue;
                }

                let bg_pixel = self.get_pixel_mut(self_x, self_y);
                let fg_pixel = foreground.get_pixel(mask_x, mask_y);

                if alpha_val == 255 {
                    *bg_pixel = fg_pixel;
                } else {
                    let level = multipliers[alpha_val as usize];
                    bg_pixel.r = {
                        let bg = bg_pixel.r as i32;
                        let fg = fg_pixel.r as i32;
                        (bg - (((bg - fg) * level as i32) >> 16)) as u8
                    };
                    bg_pixel.g = {
                        let bg = bg_pixel.g as i32;
                        let fg = fg_pixel.g as i32;
                        (bg - (((bg - fg) * level as i32) >> 16)) as u8
                    };
                    bg_pixel.b = {
                        let bg = bg_pixel.b as i32;
                        let fg = fg_pixel.b as i32;
                        (bg - (((bg - fg) * level as i32) >> 16)) as u8
                    };
                }
            }
        }
    }
}
