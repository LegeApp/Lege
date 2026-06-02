// src/image_types.rs

//! Lightweight image buffer types for Legencode.
//!
//! This module provides minimal, custom implementations of `Rgb`, `GrayPixel`,
//! `RgbImage`, and `GrayImage` types to replace the `image` crate dependency.
//!
//! These types are optimized for our encoding workflows and provide zero-copy
//! byte access where safe (via repr(C) and careful unsafe blocks).

use std::slice;

// --- Pixel Type Definitions ---

/// A single RGB pixel with 8-bit components.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    #[inline]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    #[inline]
    pub const fn black() -> Self {
        Rgb { r: 0, g: 0, b: 0 }
    }

    #[inline]
    pub const fn white() -> Self {
        Rgb {
            r: 255,
            g: 255,
            b: 255,
        }
    }
}

impl From<[u8; 3]> for Rgb {
    #[inline]
    fn from(arr: [u8; 3]) -> Self {
        Rgb {
            r: arr[0],
            g: arr[1],
            b: arr[2],
        }
    }
}

impl From<Rgb> for [u8; 3] {
    #[inline]
    fn from(p: Rgb) -> [u8; 3] {
        [p.r, p.g, p.b]
    }
}

/// A single grayscale pixel with an 8-bit intensity value.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct GrayPixel {
    pub y: u8,
}

impl GrayPixel {
    #[inline]
    pub const fn new(y: u8) -> Self {
        GrayPixel { y }
    }

    #[inline]
    pub const fn black() -> Self {
        GrayPixel { y: 0 }
    }

    #[inline]
    pub const fn white() -> Self {
        GrayPixel { y: 255 }
    }
}

impl From<u8> for GrayPixel {
    #[inline]
    fn from(y: u8) -> Self {
        GrayPixel { y }
    }
}

impl From<GrayPixel> for u8 {
    #[inline]
    fn from(p: GrayPixel) -> u8 {
        p.y
    }
}

// --- RgbImage Type (Color Image Buffer) ---

/// A 2D buffer of RGB pixels.
/// Stores pixels in row-major order.
#[derive(Clone, Debug)]
pub struct RgbImage {
    width: u32,
    height: u32,
    data: Vec<Rgb>,
}

impl RgbImage {
    /// Creates a new RGB image with the given dimensions, initialized to black.
    pub fn new(width: u32, height: u32) -> Self {
        RgbImage {
            width,
            height,
            data: vec![Rgb::black(); (width * height) as usize],
        }
    }

    /// Creates an RGB image from a raw vector of pixels.
    /// Returns None if the vector length doesn't match width * height.
    pub fn from_raw(width: u32, height: u32, data: Vec<u8>) -> Option<Self> {
        let expected_len = (width as usize) * (height as usize) * 3;
        if data.len() != expected_len {
            return None;
        }

        let pixels: Vec<Rgb> = data
            .chunks_exact(3)
            .map(|chunk| Rgb::new(chunk[0], chunk[1], chunk[2]))
            .collect();

        Some(RgbImage {
            width,
            height,
            data: pixels,
        })
    }

    /// Creates an RGB image from a vector of Rgb pixels.
    pub fn from_vec(width: u32, height: u32, data: Vec<Rgb>) -> Option<Self> {
        if data.len() != (width as usize) * (height as usize) {
            return None;
        }
        Some(RgbImage {
            width,
            height,
            data,
        })
    }

    /// Creates an image filled with a single pixel value.
    pub fn from_pixel(width: u32, height: u32, pixel: Rgb) -> Self {
        RgbImage {
            width,
            height,
            data: vec![pixel; (width * height) as usize],
        }
    }

    /// Creates an image by calling a function for each pixel.
    pub fn from_fn<F>(width: u32, height: u32, mut f: F) -> Self
    where
        F: FnMut(u32, u32) -> Rgb,
    {
        let mut data = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                data.push(f(x, y));
            }
        }
        RgbImage {
            width,
            height,
            data,
        }
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    #[inline]
    pub fn get_pixel(&self, x: u32, y: u32) -> Rgb {
        assert!(x < self.width && y < self.height);
        self.data[(y * self.width + x) as usize]
    }

    #[inline]
    pub fn get_pixel_mut(&mut self, x: u32, y: u32) -> &mut Rgb {
        assert!(x < self.width && y < self.height);
        &mut self.data[(y * self.width + x) as usize]
    }

    #[inline]
    pub fn put_pixel(&mut self, x: u32, y: u32, pixel: Rgb) {
        assert!(x < self.width && y < self.height);
        self.data[(y * self.width + x) as usize] = pixel;
    }

    #[inline]
    pub fn pixels(&self) -> &[Rgb] {
        &self.data
    }

    #[inline]
    pub fn pixels_mut(&mut self) -> &mut [Rgb] {
        &mut self.data
    }

    /// Returns raw pixel data as a byte slice (RGB triplets).
    ///
    /// # Safety
    ///
    /// This is safe because:
    /// - Rgb is #[repr(C)] with three u8 fields
    /// - The alignment and size constraints are satisfied
    /// - No padding exists in the struct
    pub fn as_raw(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.data.as_ptr() as *const u8, self.data.len() * 3) }
    }

    /// Returns mutable raw pixel data as a byte slice.
    ///
    /// # Safety
    ///
    /// Same safety reasoning as as_raw().
    pub fn as_raw_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.data.as_mut_ptr() as *mut u8, self.data.len() * 3) }
    }

    /// Converts to a grayscale image using BT.709 luminance formula.
    pub fn to_luma(&self) -> GrayImage {
        let data = self
            .data
            .iter()
            .map(|p| {
                // BT.709 formula: Y = 0.2126*R + 0.7152*G + 0.0722*B
                let gray = (0.2126 * p.r as f32 + 0.7152 * p.g as f32 + 0.0722 * p.b as f32) as u8;
                GrayPixel::new(gray)
            })
            .collect();
        GrayImage {
            width: self.width,
            height: self.height,
            data,
        }
    }

    /// Consumes the image and returns the underlying pixel vector.
    pub fn into_raw(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.data.len() * 3);
        for pixel in self.data {
            bytes.push(pixel.r);
            bytes.push(pixel.g);
            bytes.push(pixel.b);
        }
        bytes
    }
}

// --- GrayImage Type (Grayscale Image Buffer) ---

/// A 2D buffer of grayscale pixels.
/// Stores pixels in row-major order.
#[derive(Clone, Debug)]
pub struct GrayImage {
    width: u32,
    height: u32,
    data: Vec<GrayPixel>,
}

impl GrayImage {
    /// Creates a new grayscale image with the given dimensions, initialized to black.
    pub fn new(width: u32, height: u32) -> Self {
        GrayImage {
            width,
            height,
            data: vec![GrayPixel::black(); (width * height) as usize],
        }
    }

    /// Creates a grayscale image from a raw byte vector.
    /// Returns None if the vector length doesn't match width * height.
    pub fn from_raw(width: u32, height: u32, data: Vec<u8>) -> Option<Self> {
        let expected_len = (width as usize) * (height as usize);
        if data.len() != expected_len {
            return None;
        }

        let pixels: Vec<GrayPixel> = data.into_iter().map(GrayPixel::new).collect();

        Some(GrayImage {
            width,
            height,
            data: pixels,
        })
    }

    /// Creates a grayscale image from a vector of GrayPixel.
    pub fn from_vec(width: u32, height: u32, data: Vec<GrayPixel>) -> Option<Self> {
        if data.len() != (width as usize) * (height as usize) {
            return None;
        }
        Some(GrayImage {
            width,
            height,
            data,
        })
    }

    /// Creates an image filled with a single pixel value.
    pub fn from_pixel(width: u32, height: u32, pixel: GrayPixel) -> Self {
        GrayImage {
            width,
            height,
            data: vec![pixel; (width * height) as usize],
        }
    }

    /// Creates an image by calling a function for each pixel.
    pub fn from_fn<F>(width: u32, height: u32, mut f: F) -> Self
    where
        F: FnMut(u32, u32) -> GrayPixel,
    {
        let mut data = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                data.push(f(x, y));
            }
        }
        GrayImage {
            width,
            height,
            data,
        }
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    #[inline]
    pub fn get_pixel(&self, x: u32, y: u32) -> GrayPixel {
        assert!(x < self.width && y < self.height);
        self.data[(y * self.width + x) as usize]
    }

    #[inline]
    pub fn get_pixel_mut(&mut self, x: u32, y: u32) -> &mut GrayPixel {
        assert!(x < self.width && y < self.height);
        &mut self.data[(y * self.width + x) as usize]
    }

    #[inline]
    pub fn put_pixel(&mut self, x: u32, y: u32, pixel: GrayPixel) {
        assert!(x < self.width && y < self.height);
        self.data[(y * self.width + x) as usize] = pixel;
    }

    #[inline]
    pub fn pixels(&self) -> &[GrayPixel] {
        &self.data
    }

    #[inline]
    pub fn pixels_mut(&mut self) -> &mut [GrayPixel] {
        &mut self.data
    }

    /// Returns raw pixel data as a byte slice.
    ///
    /// # Safety
    ///
    /// This is safe because:
    /// - GrayPixel is #[repr(C)] with a single u8 field
    /// - The alignment and size constraints are satisfied
    /// - No padding exists in the struct
    pub fn as_raw(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.data.as_ptr() as *const u8, self.data.len()) }
    }

    /// Returns mutable raw pixel data as a byte slice.
    ///
    /// # Safety
    ///
    /// Same safety reasoning as as_raw().
    pub fn as_raw_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.data.as_mut_ptr() as *mut u8, self.data.len()) }
    }

    /// Consumes the image and returns the underlying byte vector.
    pub fn into_raw(self) -> Vec<u8> {
        self.data.into_iter().map(|p| p.y).collect()
    }
}

/// Type alias for compatibility with some existing code patterns.
pub type Luma = GrayPixel;

/// Trait for pixel types to enable generic ImageBuffer construction
pub trait PixelTrait: Clone {
    fn channels() -> u8;
}

impl PixelTrait for Rgb {
    fn channels() -> u8 {
        3
    }
}

impl PixelTrait for GrayPixel {
    fn channels() -> u8 {
        1
    }
}

/// Helper to create ImageBuffer-compatible types
impl GrayImage {
    /// Create from raw bytes (compatibility with image crate)
    pub fn from_raw_compat(width: u32, height: u32, data: Vec<u8>) -> Option<Self> {
        Self::from_raw(width, height, data)
    }
}

impl RgbImage {
    /// Create from raw bytes (compatibility with image crate)
    pub fn from_raw_compat(width: u32, height: u32, data: Vec<u8>) -> Option<Self> {
        Self::from_raw(width, height, data)
    }
}

/// Dynamic image type that can hold different pixel formats.
/// Simplified version of image crate's DynamicImage for local image tools.
#[derive(Clone, Debug)]
pub enum DynamicImage {
    /// 8-bit grayscale image
    ImageLuma8(GrayImage),
    /// 8-bit RGB image
    ImageRgb8(RgbImage),
    /// 16-bit grayscale (stored as GrayImage but interpreted as u16 per pixel)
    ImageLuma16 {
        width: u32,
        height: u32,
        data: Vec<u16>,
    },
    /// 16-bit RGB (stored as separate u16 vec, 3 values per pixel)
    ImageRgb16 {
        width: u32,
        height: u32,
        data: Vec<u16>,
    },
}

impl DynamicImage {
    /// Get image dimensions
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            DynamicImage::ImageLuma8(img) => img.dimensions(),
            DynamicImage::ImageRgb8(img) => img.dimensions(),
            DynamicImage::ImageLuma16 { width, height, .. } => (*width, *height),
            DynamicImage::ImageRgb16 { width, height, .. } => (*width, *height),
        }
    }

    /// Get width
    pub fn width(&self) -> u32 {
        self.dimensions().0
    }

    /// Get height
    pub fn height(&self) -> u32 {
        self.dimensions().1
    }

    /// Convert to 8-bit RGB image (converts grayscale to RGB, downsamples 16-bit)
    pub fn to_rgb8(&self) -> RgbImage {
        match self {
            DynamicImage::ImageRgb8(img) => img.clone(),
            DynamicImage::ImageLuma8(gray) => {
                let (width, height) = gray.dimensions();
                let data: Vec<Rgb> = gray
                    .pixels()
                    .iter()
                    .map(|g| Rgb::new(g.y, g.y, g.y))
                    .collect();
                RgbImage::from_vec(width, height, data).unwrap()
            }
            DynamicImage::ImageLuma16 {
                width,
                height,
                data,
            } => {
                let pixels = data
                    .iter()
                    .map(|&y| {
                        let y8 = (y >> 8) as u8; // Downsample to 8-bit
                        Rgb::new(y8, y8, y8)
                    })
                    .collect();
                RgbImage::from_vec(*width, *height, pixels).unwrap()
            }
            DynamicImage::ImageRgb16 {
                width,
                height,
                data,
            } => {
                let pixels = data
                    .chunks(3)
                    .map(|rgb| {
                        let r = (rgb[0] >> 8) as u8;
                        let g = (rgb[1] >> 8) as u8;
                        let b = (rgb[2] >> 8) as u8;
                        Rgb::new(r, g, b)
                    })
                    .collect();
                RgbImage::from_vec(*width, *height, pixels).unwrap()
            }
        }
    }

    /// Convert to 8-bit grayscale image
    pub fn to_luma8(&self) -> GrayImage {
        match self {
            DynamicImage::ImageLuma8(img) => img.clone(),
            DynamicImage::ImageRgb8(img) => img.to_luma(),
            DynamicImage::ImageLuma16 {
                width,
                height,
                data,
            } => {
                let pixels = data
                    .iter()
                    .map(|&y| GrayPixel::new((y >> 8) as u8))
                    .collect();
                GrayImage::from_vec(*width, *height, pixels).unwrap()
            }
            DynamicImage::ImageRgb16 {
                width,
                height,
                data,
            } => {
                let pixels = data
                    .chunks(3)
                    .map(|rgb| {
                        let r = (rgb[0] >> 8) as f32;
                        let g = (rgb[1] >> 8) as f32;
                        let b = (rgb[2] >> 8) as f32;
                        let y = (0.2126 * r + 0.7152 * g + 0.0722 * b) as u8;
                        GrayPixel::new(y)
                    })
                    .collect();
                GrayImage::from_vec(*width, *height, pixels).unwrap()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rgb_creation() {
        let img = RgbImage::new(10, 10);
        assert_eq!(img.width(), 10);
        assert_eq!(img.height(), 10);
        assert_eq!(img.pixels().len(), 100);
    }

    #[test]
    fn test_gray_creation() {
        let img = GrayImage::new(5, 5);
        assert_eq!(img.width(), 5);
        assert_eq!(img.height(), 5);
        assert_eq!(img.pixels().len(), 25);
    }

    #[test]
    fn test_rgb_from_raw() {
        let data = vec![255u8; 300]; // 100 pixels * 3 channels
        let img = RgbImage::from_raw(10, 10, data).unwrap();
        assert_eq!(img.get_pixel(0, 0), Rgb::white());
    }

    #[test]
    fn test_gray_from_raw() {
        let data = vec![128u8; 100];
        let img = GrayImage::from_raw(10, 10, data).unwrap();
        assert_eq!(img.get_pixel(0, 0).y, 128);
    }

    #[test]
    fn test_as_raw_rgb() {
        let mut img = RgbImage::new(2, 2);
        img.put_pixel(0, 0, Rgb::new(255, 0, 0));
        let raw = img.as_raw();
        assert_eq!(raw[0], 255);
        assert_eq!(raw[1], 0);
        assert_eq!(raw[2], 0);
    }

    #[test]
    fn test_as_raw_gray() {
        let mut img = GrayImage::new(2, 2);
        img.put_pixel(0, 0, GrayPixel::new(200));
        let raw = img.as_raw();
        assert_eq!(raw[0], 200);
    }

    #[test]
    fn test_to_luma() {
        let mut img = RgbImage::new(1, 1);
        img.put_pixel(0, 0, Rgb::new(255, 255, 255));
        let gray = img.to_luma();
        assert!(gray.get_pixel(0, 0).y > 250); // Should be close to 255
    }
}
