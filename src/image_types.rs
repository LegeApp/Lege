// src/image_types.rs

//! Lightweight image buffer types for Legencode.
//!
//! This module provides minimal, custom implementations of `Rgb`, `GrayPixel`, 
//! `RgbImage`, and `GrayImage` types to replace the `image` crate dependency.
//! 
//! These types are optimized for our encoding workflows and provide zero-copy
//! byte access where safe (via repr(C) and careful unsafe blocks).

use std::ops::Index;

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
        Rgb { r: 255, g: 255, b: 255 }
    }
}

// Implement indexing for Rgb to support pixel[0], pixel[1], pixel[2]
impl Index<usize> for Rgb {
    type Output = u8;
    
    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        match index {
            0 => &self.r,
            1 => &self.g,
            2 => &self.b,
            _ => panic!("Index out of bounds for Rgb: {}", index),
        }
    }
}

impl From<[u8; 3]> for Rgb {
    #[inline]
    fn from(arr: [u8; 3]) -> Self {
        Rgb { r: arr[0], g: arr[1], b: arr[2] }
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
/// Stores pixels in row-major order as contiguous RGB triplets in Vec<u8>.
/// This provides zero-cost conversions to/from raw bytes and eliminates unsafe blocks.
#[derive(Clone, Debug)]
pub struct RgbImage {
    width: u32,
    height: u32,
    data: Vec<u8>,  // RGB triplets, row-major
}

impl RgbImage {
    /// Creates a new RGB image with the given dimensions, initialized to black.
    pub fn new(width: u32, height: u32) -> Self {
        RgbImage {
            width,
            height,
            data: vec![0u8; (width * height * 3) as usize],
        }
    }

    /// Creates an RGB image from a raw vector of bytes (RGB triplets).
    /// Returns None if the vector length doesn't match width * height * 3.
    pub fn from_raw(width: u32, height: u32, data: Vec<u8>) -> Option<Self> {
        let expected_len = (width as usize) * (height as usize) * 3;
        if data.len() != expected_len {
            return None;
        }
        
        Some(RgbImage {
            width,
            height,
            data,
        })
    }

    /// Creates an RGB image from a vector of Rgb pixels.
    pub fn from_vec(width: u32, height: u32, data: Vec<Rgb>) -> Option<Self> {
        if data.len() != (width as usize) * (height as usize) {
            return None;
        }
        
        // Convert Vec<Rgb> to Vec<u8>
        let mut bytes = Vec::with_capacity(data.len() * 3);
        for pixel in data {
            bytes.push(pixel.r);
            bytes.push(pixel.g);
            bytes.push(pixel.b);
        }
        
        Some(RgbImage { 
            width, 
            height, 
            data: bytes 
        })
    }

    /// Creates an image filled with a single pixel value.
    pub fn from_pixel(width: u32, height: u32, pixel: Rgb) -> Self {
        let len = (width * height) as usize;
        let mut data = Vec::with_capacity(len * 3);
        for _ in 0..len {
            data.push(pixel.r);
            data.push(pixel.g);
            data.push(pixel.b);
        }
        RgbImage { width, height, data }
    }

    /// Creates an image by calling a function for each pixel.
    pub fn from_fn<F>(width: u32, height: u32, mut f: F) -> Self
    where
        F: FnMut(u32, u32) -> Rgb,
    {
        let len = (width * height) as usize;
        let mut data = Vec::with_capacity(len * 3);
        for y in 0..height {
            for x in 0..width {
                let pixel = f(x, y);
                data.push(pixel.r);
                data.push(pixel.g);
                data.push(pixel.b);
            }
        }
        RgbImage { width, height, data }
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
        debug_assert!(x < self.width && y < self.height, "Pixel coordinates out of bounds");
        let idx = ((y * self.width + x) as usize) * 3;
        Rgb::new(self.data[idx], self.data[idx + 1], self.data[idx + 2])
    }

    #[inline]
    pub fn put_pixel(&mut self, x: u32, y: u32, pixel: Rgb) {
        debug_assert!(x < self.width && y < self.height, "Pixel coordinates out of bounds");
        let idx = ((y * self.width + x) as usize) * 3;
        self.data[idx] = pixel.r;
        self.data[idx + 1] = pixel.g;
        self.data[idx + 2] = pixel.b;
    }

    /// Returns a slice of Rgb pixels by reinterpreting the raw data.
    /// This returns a reference to the underlying byte buffer for compatibility.
    #[inline]
    pub fn pixels(&self) -> &[u8] {
        &self.data
    }

    /// Returns an iterator over pixels as (x, y, Rgb) tuples.
    pub fn enumerate_pixels(&self) -> impl Iterator<Item = (u32, u32, Rgb)> + '_ {
        (0..self.height).flat_map(move |y| {
            (0..self.width).map(move |x| (x, y, self.get_pixel(x, y)))
        })
    }

    /// Returns an iterator over image rows.
    /// Each row is a slice of bytes (RGB triplets).
    pub fn rows(&self) -> impl Iterator<Item = &[u8]> {
        self.data.chunks_exact((self.width as usize) * 3)
    }

    /// Returns mutable iterator over image rows.
    pub fn rows_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        let width = self.width as usize;
        self.data.chunks_exact_mut(width * 3)
    }

    /// Returns raw pixel data as a byte slice (RGB triplets).
    /// This is now zero-cost with Vec<u8> storage.
    #[inline]
    pub fn as_raw(&self) -> &[u8] {
        &self.data
    }

    /// Returns mutable raw pixel data as a byte slice.
    /// This is now zero-cost with Vec<u8> storage.
    #[inline]
    pub fn as_raw_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Converts to a grayscale image using BT.709 luminance formula with integer arithmetic.
    pub fn to_luma(&self) -> GrayImage {
        let len = (self.width * self.height) as usize;
        let mut gray_data = Vec::with_capacity(len);
        
        for chunk in self.data.chunks_exact(3) {
            // BT.709 coefficients scaled by 2^16: 13933, 46871, 4732
            // Sum = 65536 = 2^16, so >> 16 gives the result
            let r = chunk[0] as u32;
            let g = chunk[1] as u32;
            let b = chunk[2] as u32;
            let gray = ((13933 * r + 46871 * g + 4732 * b) >> 16) as u8;
            gray_data.push(gray);
        }
        
        GrayImage {
            width: self.width,
            height: self.height,
            data: gray_data,
        }
    }

    /// Consumes the image and returns the underlying byte vector.
    /// This is now zero-cost with Vec<u8> storage.
    #[inline]
    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }
    
    /// Compatibility method - returns self (used in imageops patterns)
    pub fn to_image(self) -> Self {
        self
    }
    
    /// Save the image as PNG using the png crate
    pub fn save<P: AsRef<std::path::Path>>(&self, path: P) -> Result<(), String> {
        use std::fs::File;
        use std::io::BufWriter;
        
        let file = File::create(path).map_err(|e| format!("Failed to create file: {}", e))?;
        let w = BufWriter::new(file);
        
        let mut encoder = png::Encoder::new(w, self.width, self.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        
        let mut writer = encoder.write_header().map_err(|e| format!("Failed to write PNG header: {}", e))?;
        writer.write_image_data(self.as_raw()).map_err(|e| format!("Failed to write PNG data: {}", e))?;
        
        Ok(())
    }
}

// --- GrayImage Type (Grayscale Image Buffer) ---

/// A 2D buffer of grayscale pixels.
/// Stores pixels in row-major order as Vec<u8>.
#[derive(Clone, Debug)]
pub struct GrayImage {
    width: u32,
    height: u32,
    data: Vec<u8>,  // grayscale values, row-major
}

impl GrayImage {
    /// Creates a new grayscale image with the given dimensions, initialized to black.
    pub fn new(width: u32, height: u32) -> Self {
        GrayImage {
            width,
            height,
            data: vec![0u8; (width * height) as usize],
        }
    }

    /// Creates a grayscale image from a raw byte vector.
    /// Returns None if the vector length doesn't match width * height.
    pub fn from_raw(width: u32, height: u32, data: Vec<u8>) -> Option<Self> {
        let expected_len = (width as usize) * (height as usize);
        if data.len() != expected_len {
            return None;
        }
        
        Some(GrayImage {
            width,
            height,
            data,
        })
    }

    /// Creates a grayscale image from a vector of GrayPixel.
    pub fn from_vec(width: u32, height: u32, data: Vec<GrayPixel>) -> Option<Self> {
        if data.len() != (width as usize) * (height as usize) {
            return None;
        }
        
        // Convert Vec<GrayPixel> to Vec<u8>
        let bytes: Vec<u8> = data.into_iter().map(|p| p.y).collect();
        
        Some(GrayImage { 
            width, 
            height, 
            data: bytes 
        })
    }

    /// Creates an image filled with a single pixel value.
    pub fn from_pixel(width: u32, height: u32, pixel: GrayPixel) -> Self {
        GrayImage {
            width,
            height,
            data: vec![pixel.y; (width * height) as usize],
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
                data.push(f(x, y).y);
            }
        }
        GrayImage { width, height, data }
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
        debug_assert!(x < self.width && y < self.height, "Pixel coordinates out of bounds");
        GrayPixel::new(self.data[(y * self.width + x) as usize])
    }

    #[inline]
    pub fn put_pixel(&mut self, x: u32, y: u32, pixel: GrayPixel) {
        debug_assert!(x < self.width && y < self.height, "Pixel coordinates out of bounds");
        self.data[(y * self.width + x) as usize] = pixel.y;
    }

    /// Returns the pixel data as a byte slice.
    #[inline]
    pub fn pixels(&self) -> &[u8] {
        &self.data
    }

    /// Returns an iterator over pixels as (x, y, GrayPixel) tuples.
    pub fn enumerate_pixels(&self) -> impl Iterator<Item = (u32, u32, GrayPixel)> + '_ {
        (0..self.height).flat_map(move |y| {
            (0..self.width).map(move |x| (x, y, self.get_pixel(x, y)))
        })
    }

    /// Returns an iterator over image rows.
    pub fn rows(&self) -> impl Iterator<Item = &[u8]> {
        self.data.chunks_exact(self.width as usize)
    }

    /// Returns mutable iterator over image rows.
    pub fn rows_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        let width = self.width as usize;
        self.data.chunks_exact_mut(width)
    }

    /// Returns raw pixel data as a byte slice.
    /// This is now zero-cost with Vec<u8> storage.
    #[inline]
    pub fn as_raw(&self) -> &[u8] {
        &self.data
    }

    /// Returns mutable raw pixel data as a byte slice.
    /// This is now zero-cost with Vec<u8> storage.
    #[inline]
    pub fn as_raw_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Consumes the image and returns the underlying byte vector.
    /// This is now zero-cost with Vec<u8> storage.
    #[inline]
    pub fn into_raw(self) -> Vec<u8> {
        self.data
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
                let mut out = Vec::with_capacity((width as usize) * (height as usize) * 3);
                for &g in gray.pixels().iter() {
                    out.push(g);
                    out.push(g);
                    out.push(g);
                }
                RgbImage::from_raw(width, height, out).unwrap()
            }
            DynamicImage::ImageLuma16 { width, height, data } => {
                let pixels = data
                    .iter()
                    .map(|&y| {
                        let y8 = (y >> 8) as u8; // Downsample to 8-bit
                        Rgb::new(y8, y8, y8)
                    })
                    .collect();
                RgbImage::from_vec(*width, *height, pixels).unwrap()
            }
            DynamicImage::ImageRgb16 { width, height, data } => {
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

    /// Convert to 8-bit grayscale image using integer arithmetic
    pub fn to_luma8(&self) -> GrayImage {
        match self {
            DynamicImage::ImageLuma8(img) => img.clone(),
            DynamicImage::ImageRgb8(img) => img.to_luma(),
            DynamicImage::ImageLuma16 { width, height, data } => {
                // More accurate 16->8 bit conversion: ((val + 128) >> 8)
                let bytes: Vec<u8> = data.iter().map(|&y| ((y + 128) >> 8) as u8).collect();
                GrayImage::from_raw(*width, *height, bytes).unwrap()
            }
            DynamicImage::ImageRgb16 { width, height, data } => {
                // Use integer arithmetic with BT.709 coefficients
                let bytes: Vec<u8> = data
                    .chunks(3)
                    .map(|rgb| {
                        // Convert 16-bit to 8-bit first: ((val + 128) >> 8)
                        let r = ((rgb[0] + 128) >> 8) as u32;
                        let g = ((rgb[1] + 128) >> 8) as u32;
                        let b = ((rgb[2] + 128) >> 8) as u32;
                        // BT.709: 13933*R + 46871*G + 4732*B, then >> 16
                        ((13933 * r + 46871 * g + 4732 * b) >> 16) as u8
                    })
                    .collect();
                GrayImage::from_raw(*width, *height, bytes).unwrap()
            }
        }
    }
}

// --- Rotation Functions ---

impl RgbImage {
    /// Rotate the image 90 degrees clockwise with cache-friendly tiled processing.
    pub fn rotate_90(&self) -> Self {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut result = RgbImage::new(self.height, self.width);
        const BLOCK: usize = 32;

        for by in (0..h).step_by(BLOCK) {
            for bx in (0..w).step_by(BLOCK) {
                let ye = (by + BLOCK).min(h);
                let xe = (bx + BLOCK).min(w);
                for y in by..ye {
                    for x in bx..xe {
                        let pixel = self.get_pixel(x as u32, y as u32);
                        result.put_pixel((h - 1 - y) as u32, x as u32, pixel);
                    }
                }
            }
        }
        result
    }

    /// Rotate the image 180 degrees using efficient row reversal.
    pub fn rotate_180(&self) -> Self {
        let (w, h) = (self.width as usize, self.height as usize);
        let stride = w * 3;
        let mut data = Vec::with_capacity(self.data.len());
        
        // Iterate rows in reverse order
        for y in (0..h).rev() {
            let row_start = y * stride;
            let row = &self.data[row_start..row_start + stride];
            
            // Reverse pixels within the row (in triplets)
            for x in (0..w).rev() {
                let px = x * 3;
                data.push(row[px]);
                data.push(row[px + 1]);
                data.push(row[px + 2]);
            }
        }
        
        RgbImage { 
            width: self.width, 
            height: self.height, 
            data 
        }
    }

    /// Rotate the image 270 degrees clockwise with cache-friendly tiled processing.
    pub fn rotate_270(&self) -> Self {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut result = RgbImage::new(self.height, self.width);
        const BLOCK: usize = 32;

        for by in (0..h).step_by(BLOCK) {
            for bx in (0..w).step_by(BLOCK) {
                let ye = (by + BLOCK).min(h);
                let xe = (bx + BLOCK).min(w);
                for y in by..ye {
                    for x in bx..xe {
                        let pixel = self.get_pixel(x as u32, y as u32);
                        result.put_pixel(y as u32, (w - 1 - x) as u32, pixel);
                    }
                }
            }
        }
        result
    }
}

impl GrayImage {
    /// Rotate the image 90 degrees clockwise with cache-friendly tiled processing.
    pub fn rotate_90(&self) -> Self {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut result = GrayImage::new(self.height, self.width);
        const BLOCK: usize = 32;

        for by in (0..h).step_by(BLOCK) {
            for bx in (0..w).step_by(BLOCK) {
                let ye = (by + BLOCK).min(h);
                let xe = (bx + BLOCK).min(w);
                for y in by..ye {
                    for x in bx..xe {
                        let pixel = self.get_pixel(x as u32, y as u32);
                        result.put_pixel((h - 1 - y) as u32, x as u32, pixel);
                    }
                }
            }
        }
        result
    }

    /// Rotate the image 180 degrees using efficient row reversal.
    pub fn rotate_180(&self) -> Self {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut data = Vec::with_capacity(self.data.len());
        
        // Iterate rows in reverse order
        for y in (0..h).rev() {
            let row_start = y * w;
            let row = &self.data[row_start..row_start + w];
            
            // Reverse pixels within the row
            for &pixel in row.iter().rev() {
                data.push(pixel);
            }
        }
        
        GrayImage { 
            width: self.width, 
            height: self.height, 
            data 
        }
    }

    /// Rotate the image 270 degrees clockwise with cache-friendly tiled processing.
    pub fn rotate_270(&self) -> Self {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut result = GrayImage::new(self.height, self.width);
        const BLOCK: usize = 32;

        for by in (0..h).step_by(BLOCK) {
            for bx in (0..w).step_by(BLOCK) {
                let ye = (by + BLOCK).min(h);
                let xe = (bx + BLOCK).min(w);
                for y in by..ye {
                    for x in bx..xe {
                        let pixel = self.get_pixel(x as u32, y as u32);
                        result.put_pixel(y as u32, (w - 1 - x) as u32, pixel);
                    }
                }
            }
        }
        result
    }
}

// --- Image Operations (imageops compatibility) ---

impl RgbImage {
    /// Create a cropped copy of the image (immutable crop) using efficient row-based copying.
    pub fn crop_imm(&self, x: u32, y: u32, width: u32, height: u32) -> Self {
        assert!(x + width <= self.width && y + height <= self.height,
                "Crop region out of bounds");
        
        let src_stride = (self.width as usize) * 3;
        let dst_stride = (width as usize) * 3;
        let mut data = vec![0u8; dst_stride * height as usize];
        
        for dy in 0..height as usize {
            let src_offset = ((y as usize + dy) * src_stride) + (x as usize * 3);
            let dst_offset = dy * dst_stride;
            data[dst_offset..dst_offset + dst_stride]
                .copy_from_slice(&self.data[src_offset..src_offset + dst_stride]);
        }
        
        RgbImage { width, height, data }
    }
    
    /// Overlay another image onto this one at the specified position.
    pub fn overlay(&mut self, other: &RgbImage, x: i64, y: i64) {
        let (other_w, other_h) = other.dimensions();
        
        for dy in 0..other_h {
            for dx in 0..other_w {
                let dest_x = x + dx as i64;
                let dest_y = y + dy as i64;
                
                if dest_x >= 0 && dest_y >= 0 
                    && dest_x < self.width as i64 
                    && dest_y < self.height as i64 
                {
                    let pixel = other.get_pixel(dx, dy);
                    self.put_pixel(dest_x as u32, dest_y as u32, pixel);
                }
            }
        }
    }
}

impl GrayImage {
    /// Create a cropped copy of the image (immutable crop) using efficient row-based copying.
    pub fn crop_imm(&self, x: u32, y: u32, width: u32, height: u32) -> Self {
        assert!(x + width <= self.width && y + height <= self.height,
                "Crop region out of bounds");
        
        let src_stride = self.width as usize;
        let dst_stride = width as usize;
        let mut data = vec![0u8; dst_stride * height as usize];
        
        for dy in 0..height as usize {
            let src_offset = ((y as usize + dy) * src_stride) + x as usize;
            let dst_offset = dy * dst_stride;
            data[dst_offset..dst_offset + dst_stride]
                .copy_from_slice(&self.data[src_offset..src_offset + dst_stride]);
        }
        
        GrayImage { width, height, data }
    }
}

// --- Image Loading Functions ---

/// Load an image from memory (PNG/JPEG support via png crate)
pub fn load_from_memory(data: &[u8]) -> Result<DynamicImage, String> {
    // Try PNG first
    if data.len() > 8 && &data[0..8] == &[137, 80, 78, 71, 13, 10, 26, 10] {
        return load_png_from_memory(data);
    }
    
    // For JPEG and other formats, we'd need additional dependencies
    // For now, just return error
    Err("Unsupported image format. Only PNG is currently supported.".to_string())
}

/// Load PNG from memory
fn load_png_from_memory(data: &[u8]) -> Result<DynamicImage, String> {
    use std::io::Cursor;
    
    let decoder = png::Decoder::new(Cursor::new(data));
    let mut reader = decoder.read_info().map_err(|e| format!("PNG decode error: {}", e))?;
    
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("PNG frame error: {}", e))?;
    
    let width = info.width;
    let height = info.height;
    
    match info.color_type {
        png::ColorType::Grayscale => {
            let img = GrayImage::from_raw(width, height, buf).ok_or("Invalid grayscale data")?;
            Ok(DynamicImage::ImageLuma8(img))
        }
        png::ColorType::Rgb => {
            let img = RgbImage::from_raw(width, height, buf).ok_or("Invalid RGB data")?;
            Ok(DynamicImage::ImageRgb8(img))
        }
        png::ColorType::Rgba => {
            // Convert RGBA to RGB by dropping alpha
            let rgb_data: Vec<u8> = buf.chunks(4).flat_map(|chunk| [chunk[0], chunk[1], chunk[2]]).collect();
            let img = RgbImage::from_raw(width, height, rgb_data).ok_or("Invalid RGBA data")?;
            Ok(DynamicImage::ImageRgb8(img))
        }
        png::ColorType::GrayscaleAlpha => {
            // Convert to grayscale by dropping alpha
            let gray_data: Vec<u8> = buf.chunks(2).map(|chunk| chunk[0]).collect();
            let img = GrayImage::from_raw(width, height, gray_data).ok_or("Invalid grayscale-alpha data")?;
            Ok(DynamicImage::ImageLuma8(img))
        }
        _ => Err("Unsupported PNG color type".to_string())
    }
}

/// Load an image from a file path
pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<DynamicImage, String> {
    use std::fs::File;
    use std::io::Read;
    
    let mut file = File::open(path).map_err(|e| format!("Failed to open file: {}", e))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).map_err(|e| format!("Failed to read file: {}", e))?;
    
    load_from_memory(&buffer)
}

/// Module providing image::imageops compatibility functions
pub mod imageops {
    use super::{RgbImage, GrayImage};
    
    /// Filter type for resizing (compatibility with image crate)
    #[derive(Debug, Clone, Copy)]
    pub enum FilterType {
        Nearest,
        Triangle,
        CatmullRom,
        Gaussian,
        Lanczos3,
    }
    
    /// Resize an RGB image using our sophisticated resize infrastructure
    pub fn resize(img: &RgbImage, nwidth: u32, nheight: u32, filter: FilterType) -> RgbImage {
        let (w, h) = img.dimensions();
        
        if nwidth == w && nheight == h {
            return img.clone();
        }
        
        // Use our existing resize infrastructure with hardware acceleration
        use crate::resize::{resize_bytes, ResizeParams, ResizeMethod};
        
        let method = match filter {
            FilterType::Nearest => ResizeMethod::Nearest,
            FilterType::Triangle => ResizeMethod::Bilinear,
            FilterType::CatmullRom => ResizeMethod::Bicubic,
            FilterType::Gaussian => ResizeMethod::Bilinear,
            FilterType::Lanczos3 => ResizeMethod::Lanczos3,
        };
        
        let params = ResizeParams {
            target_width: nwidth,
            target_height: nheight,
            method,
            letterbox: false,
            border_value: 0.0,
            swap_rb: false,
        };
        
        // Get raw bytes and resize
        let raw_in = img.as_raw();
        
        match resize_bytes(raw_in, w, h, &params, 3) {
            Ok(resized_bytes) => {
                RgbImage::from_raw(nwidth, nheight, resized_bytes)
                    .expect("Failed to create resized image from valid bytes")
            }
            Err(_) => {
                // Fallback to simple bilinear if resize fails
                fallback_resize(img, nwidth, nheight, filter)
            }
        }
    }
    
    /// Fallback resize using simple bilinear interpolation
    fn fallback_resize(img: &RgbImage, nwidth: u32, nheight: u32, filter: FilterType) -> RgbImage {
        let (w, h) = img.dimensions();
        let mut result = RgbImage::new(nwidth, nheight);
        
        match filter {
            FilterType::Nearest => {
                // Nearest neighbor
                for y in 0..nheight {
                    for x in 0..nwidth {
                        let src_x = ((x as f32 / nwidth as f32) * w as f32) as u32;
                        let src_y = ((y as f32 / nheight as f32) * h as f32) as u32;
                        result.put_pixel(x, y, img.get_pixel(src_x.min(w-1), src_y.min(h-1)));
                    }
                }
            }
            _ => {
                // Bilinear interpolation
                for y in 0..nheight {
                    for x in 0..nwidth {
                        let src_x = (x as f32 / nwidth as f32) * (w - 1) as f32;
                        let src_y = (y as f32 / nheight as f32) * (h - 1) as f32;
                        
                        let x0 = src_x.floor() as u32;
                        let y0 = src_y.floor() as u32;
                        let x1 = (x0 + 1).min(w - 1);
                        let y1 = (y0 + 1).min(h - 1);
                        
                        let fx = src_x - x0 as f32;
                        let fy = src_y - y0 as f32;
                        
                        let p00 = img.get_pixel(x0, y0);
                        let p10 = img.get_pixel(x1, y0);
                        let p01 = img.get_pixel(x0, y1);
                        let p11 = img.get_pixel(x1, y1);
                        
                        let r = lerp2d(p00.r as f32, p10.r as f32, p01.r as f32, p11.r as f32, fx, fy);
                        let g = lerp2d(p00.g as f32, p10.g as f32, p01.g as f32, p11.g as f32, fx, fy);
                        let b = lerp2d(p00.b as f32, p10.b as f32, p01.b as f32, p11.b as f32, fx, fy);
                        
                        result.put_pixel(x, y, super::Rgb::new(r as u8, g as u8, b as u8));
                    }
                }
            }
        }
        
        result
    }
    
    #[inline]
    fn lerp(a: f32, b: f32, t: f32) -> f32 {
        a + (b - a) * t
    }
    
    #[inline]
    fn lerp2d(p00: f32, p10: f32, p01: f32, p11: f32, fx: f32, fy: f32) -> f32 {
        let top = lerp(p00, p10, fx);
        let bottom = lerp(p01, p11, fx);
        lerp(top, bottom, fy)
    }
    
    /// Crop an image immutably (returns new image)
    pub fn crop_imm(img: &RgbImage, x: u32, y: u32, width: u32, height: u32) -> RgbImage {
        img.crop_imm(x, y, width, height)
    }
    
    /// Overlay one image onto another
    pub fn overlay(bottom: &mut RgbImage, top: &RgbImage, x: i64, y: i64) {
        bottom.overlay(top, x, y);
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
