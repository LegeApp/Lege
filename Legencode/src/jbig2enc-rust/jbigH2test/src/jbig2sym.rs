//! This module defines the core data structures for JBIG2 symbols and bitmaps,
//! and provides utilities for their manipulation, such as sorting for optimal
//! dictionary encoding.

use bitvec::prelude::*;
use bitvec::order::Msb0;
use bitvec::slice::BitSlice;
use once_cell::unsync::OnceCell;
use ndarray::Array2;
use xxhash_rust::xxh3::xxh3_64;
use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::Path;
use std::fs::File;
use std::io::{BufRead, BufReader};

use crate::jbig2shared::{u32_to_usize, usize_to_u32};


// ==============================================
// Bit manipulation utilities
// ==============================================

/// View a byte buffer as a bit-slice (read-only).
pub fn bytes_as_bits(bytes: &[u8]) -> &BitSlice<u8, Msb0> {
    BitSlice::from_slice(bytes)
}

/// Convert a `BitVec` into an owned `Vec<u8>` without copying.
pub fn bitvec_into_bytes(bits: BitVec<u8, Msb0>) -> Vec<u8> {
    bits.into_vec()
}

/// Convert a byte slice to a `BitVec` with MSB-first bit order.
pub fn bytes_to_bitvec(bytes: &[u8], bit_count: usize) -> BitVec<u8, Msb0> {
    let mut bv = BitVec::from_slice(bytes);
    bv.truncate(bit_count);
    bv
}

/// Convert a `BitVec` to a byte vector, ensuring proper padding.
pub fn bitvec_to_bytes(bits: &BitSlice<u8, Msb0>) -> Vec<u8> {
    let mut bytes = bits.to_bitvec().into_vec();
    if bits.len() % 8 != 0 {
        let padding = 8 - (bits.len() % 8);
        bytes.push(0u8);
        *bytes.last_mut().unwrap() &= !(0xFFu8 >> padding);
    }
    bytes
}

// ==============================================
// Bitmap image handling
// ==============================================

/// A bitmap image using MSB-first bit ordering for JBIG2 compatibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitImage {
    /// Width of the bitmap in pixels
    pub width: usize,
    /// Height of the bitmap in pixels
    pub height: usize,
    /// Bitmap data, stored in MSB-first order
    bits: BitVec<u8, Msb0>,
    packed_cache: OnceCell<Vec<u32>>, 
}

impl BitImage {
    pub const MAX_DIMENSION: usize = 1 << 24; // 16M pixels
    pub const MIN_DIMENSION: usize = 1;

    /// Convert the BitImage to JBIG2-compatible format.
    pub fn to_jbig2_format(&self) -> Vec<u8> {
        let bytes_per_row = (self.width + 7) / 8;
        let mut result = Vec::with_capacity(bytes_per_row * self.height);
        for y in 0..self.height {
            let row_offset = y * self.width;
            for byte_x in 0..bytes_per_row {
                let mut byte = 0u8;
                for bit in 0..8 {
                    let x = byte_x * 8 + bit;
                    if x < self.width && self.get_at(row_offset + x) {
                        byte |= 0x80 >> bit;
                    }
                }
                result.push(byte);
            }
        }
        result
    }

    /// Creates a new blank bitmap with specified dimensions.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        if width == 0 || width > Self::MAX_DIMENSION as u32 {
            return Err(format!("width must be between 1 and {}", Self::MAX_DIMENSION));
        }
        if height == 0 || height > Self::MAX_DIMENSION as u32 {
            return Err(format!("height must be between 1 and {}", Self::MAX_DIMENSION));
        }

        let total_bits = u32_to_usize(width) * u32_to_usize(height);
        let mut bits = BitVec::with_capacity(total_bits);
        bits.resize(total_bits, false);

        Ok(Self {
            width: u32_to_usize(width),
            height: u32_to_usize(height),
            bits,
            packed_cache: OnceCell::new(),
        })
    }

    /// Creates a bitmap from raw bytes.
    pub fn from_bytes(width: usize, height: usize, bytes: &[u8]) -> Self {
        let expected_bytes = (width * height + 7) / 8;
        assert_eq!(bytes.len(), expected_bytes,
                   "Expected {} bytes for {}x{} bitmap, got {}",
                   expected_bytes, width, height, bytes.len());
        let bits = bytes_to_bitvec(bytes, width * height);
        Self { width, height, bits, packed_cache: OnceCell::new() }
    }

    /// Creates a bitmap from a bit slice.
    pub fn from_bits(width: usize, height: usize, bits: &BitSlice<u8, Msb0>) -> Self {
        assert_eq!(bits.len(), width * height,
                   "Expected {} bits for {}x{} bitmap, got {}",
                   width * height, width, height, bits.len());
        Self {
            width,
            height,
            bits: bits.to_bitvec(),
            packed_cache: OnceCell::new(),
        }
    }

    /// Converts the bitmap to a byte vector.
    pub fn to_bytes(&self) -> Vec<u8> {
        bitvec_to_bytes(&self.bits)
    }

    /// Converts to a `BitVec`.
    pub fn to_bitvec(&self) -> BitVec<u8, Msb0> {
        self.bits.clone()
    }

    /// Returns a view of the bitmap as a bit slice.
    pub fn as_bits(&self) -> &BitSlice<u8, Msb0> {
        &self.bits
    }

    /// Returns a mutable view of the bitmap.
    pub fn as_mut_bits(&mut self) -> &mut BitSlice<u8, Msb0> {
        &mut self.bits
    }

    /// Gets a single bit by index.
    #[inline]
    pub fn get_at(&self, idx: usize) -> bool {
        self.bits.get(idx).map_or(false, |b| *b)
    }

    /// Converts to packed 32-bit words for efficient comparison. Results are
    /// cached to avoid repeated work when the same image is processed multiple
    /// times.
    pub fn to_packed_words(&self) -> Vec<u32> {
        let cached = self.packed_cache.get_or_init(|| {
            let wpr = (self.width + 31) / 32;
            let mut out = vec![0u32; wpr * self.height];
            for y in 0..self.height {
                for xw in 0..wpr {
                    let mut word = 0u32;
                    for bit in 0..32 {
                        let x = xw * 32 + bit;
                        if x < self.width && self.get(usize_to_u32(x), usize_to_u32(y)) {
                            word |= 1u32 << (31 - bit);
                        }
                    }
                    out[y * wpr + xw] = word;
                }
            }
            out
        });
        cached.clone()
    }

    /// Gets a pixel value at (x, y).
    #[inline]
    pub fn get(&self, x: u32, y: u32) -> bool {
        if x >= usize_to_u32(self.width) || y >= usize_to_u32(self.height) {
            return false;
        }
        let idx = u32_to_usize(y) * self.width + u32_to_usize(x);
        self.get_at(idx)
    }

    /// Gets a pixel value with usize coordinates.
    #[inline]
    pub fn get_usize(&self, x: usize, y: usize) -> bool {
        self.get(usize_to_u32(x), usize_to_u32(y))
    }

    /// Creates a sub-image from a specified rectangle.
    pub fn from_sub_image(source: &BitImage, rect: &Rect) -> Self {
        let width = u32_to_usize(rect.width);
        let height = u32_to_usize(rect.height);
        let mut result = Self::new(rect.width, rect.height).expect("Failed to create sub-image");
        for y in 0..height {
            for x in 0..width {
                let src_x = rect.x + usize_to_u32(x);
                let src_y = rect.y + usize_to_u32(y);
                if source.get(src_x, src_y) {
                    let idx = y * width + x;
                    result.bits.set(idx, true);
                }
            }
        }
        result
    }

    /// Sets a pixel value at (x, y).
    #[inline]
    pub fn set(&mut self, x: u32, y: u32, value: bool) {
        if x < usize_to_u32(self.width) && y < usize_to_u32(self.height) {
            let idx = u32_to_usize(y) * self.width + u32_to_usize(x);
            self.bits.set(idx, value);
        }
    }

    /// Crops the bitmap to a specified rectangle.
    pub fn crop(&self, rect: &Rect) -> Self {
        assert!(rect.x + rect.width <= usize_to_u32(self.width), "crop x + width out of bounds");
        assert!(rect.y + rect.height <= usize_to_u32(self.height), "crop y + height out of bounds");
        let mut cropped = Self::new(rect.width, rect.height).expect("Failed to create cropped image");
        for dy in 0..rect.height {
            for dx in 0..rect.width {
                let src_idx = u32_to_usize(rect.y + dy) * self.width + u32_to_usize(rect.x + dx);
                let dst_idx = u32_to_usize(dy) * u32_to_usize(rect.width) + u32_to_usize(dx);
                if let Some(bit) = self.bits.get(src_idx) {
                    cropped.bits.set(dst_idx, *bit);
                }
            }
        }
        cropped
    }

    /// Trims whitespace from edges, returning the bounding rectangle and cropped image.
    pub fn trim(&self) -> (Rect, BitImage) {
        if self.bits.is_empty() || self.bits.not_any() {
            return (
                Rect { x: 0, y: 0, width: 0, height: 0 },
                Self::new(0, 0).expect("Failed to create empty image")
            );
        }
        
        let mut min_x = self.width;
        let mut min_y = self.height;
        let mut max_x = 0;
        let mut max_y = 0;
        
        // First pass: find min_y and max_y by checking each row
        for y in 0..self.height {
            let row_has_pixels = (0..self.width).any(|x| self.get_usize(x, y));
            if row_has_pixels {
                min_y = y;
                break;
            }
        }
        
        for y in (0..self.height).rev() {
            let row_has_pixels = (0..self.width).any(|x| self.get_usize(x, y));
            if row_has_pixels {
                max_y = y;
                break;
            }
        }

        if min_y > max_y { // Should be unreachable if not_any() is false
            return (
                Rect { x: 0, y: 0, width: 0, height: 0 },
                Self::new(0, 0).expect("Failed to create empty image")
            );
        }

        // Second pass: find min_x and max_x within the vertical bounds
        for y in min_y..=max_y {
            for x in 0..self.width {
                if self.get_usize(x, y) {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                }
            }
        }

        if min_x > max_x { // Should be unreachable
            return (
                Rect { x: 0, y: 0, width: 0, height: 0 },
                Self::new(0, 0).expect("Failed to create empty image")
            );
        }

        let rect = Rect {
            x: usize_to_u32(min_x),
            y: usize_to_u32(min_y),
            width: usize_to_u32(max_x - min_x + 1),
            height: usize_to_u32(max_y - min_y + 1),
        };
        (rect, self.crop(&rect))
    }

    /// Inverts all bits in the bitmap.
    pub fn invert(&mut self) {
        self.bits.iter_mut().for_each(|mut bit| *bit = !*bit);
    }

    /// Performs a logical AND with another bitmap.
    pub fn and(&self, other: &Self) -> Self {
        assert_eq!(self.width, other.width, "Bitmaps must have the same width");
        assert_eq!(self.height, other.height, "Bitmaps must have the same height");
        let mut result = self.clone();
        result.bits &= &other.bits;
        result
    }

    /// Performs a logical OR with another bitmap.
    pub fn or(&self, other: &Self) -> Self {
        assert_eq!(self.width, other.width, "Bitmaps must have the same width");
        assert_eq!(self.height, other.height, "Bitmaps must have the same height");
        let mut result = self.clone();
        result.bits |= &other.bits;
        result
    }

    /// Performs a logical XOR with another bitmap.
    pub fn xor(&self, other: &Self) -> Self {
        assert_eq!(self.width, other.width, "Bitmaps must have the same width");
        assert_eq!(self.height, other.height, "Bitmaps must have the same height");
        let mut result = self.clone();
        result.bits ^= &other.bits;
        result
    }

    /// Counts set bits (1s) in the bitmap.
    pub fn count_ones(&self) -> usize {
        self.bits.count_ones()
    }

    /// Counts unset bits (0s) in the bitmap.
    pub fn count_zeros(&self) -> usize {
        self.bits.len() - self.count_ones()
    }

    /// Gets a pixel value safely, returning 0 for out-of-bounds.
    pub fn get_pixel_safely(&self, x: i32, y: i32) -> u8 {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
            self.get(x as u32, y as u32) as u8
        } else {
            0
        }
    }

    /// Returns pixel data as a byte slice for hashing.
    pub fn as_bytes(&self) -> &[u8] {
        self.bits.as_raw_slice()
    }
}

// ==============================================
// Rectangle and symbol structures
// ==============================================

/// A rectangle defining a region in the bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn infinite() -> Self {
        Self {
            x: 0,
            y: 0,
            width: u32::MAX,
            height: u32::MAX,
        }
    }
}

/// A symbol extracted from a page with its properties.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub image: BitImage,
    pub hash: u64,
}

// ==============================================
// Symbol processing and sorting
// ==============================================

/// Groups symbols by height, and sorts symbols within each height class by width.
/// This prepares symbols for encoding in a JBIG2 symbol dictionary.
/// The logic mirrors the sorting from jbig2enc's `jbig2sym.cc`.
pub fn sort_symbols_for_dictionary<'a>(symbols: &[&'a BitImage]) -> Vec<Vec<&'a BitImage>> {
    let mut height_classes = BTreeMap::new();
    for symbol in symbols {
        height_classes
            .entry(symbol.height)
            .or_insert_with(Vec::new)
            .push(*symbol);
    }

    // BTreeMap keys (heights) are already sorted.
    // Now sort each inner Vec (symbols of same height) by width.
    height_classes
        .into_values()
        .map(|mut symbol_group| {
            symbol_group.sort_by_key(|s| s.width);
            symbol_group
        })
        .collect()
}


/// Computes a hash for a `BitImage` using xxh3.
pub fn compute_glyph_hash(image: &BitImage) -> u64 {
    xxh3_64(image.as_bytes())
}

/// Converts an `ndarray::Array2<u8>` to a `BitImage`.
pub fn array_to_bitimage(array: &Array2<u8>) -> BitImage {
    let (height, width) = array.dim();
    let mut bit_image = BitImage::new(usize_to_u32(width), usize_to_u32(height)).expect("Failed to create image from array");
    
    for (y, row) in array.rows().into_iter().enumerate() {
        for (x, &pixel) in row.iter().enumerate() {
            if pixel > 0 {
                bit_image.set(usize_to_u32(x), usize_to_u32(y), true);
            }
        }
    }
    
    bit_image
}

/// Loads a PBM file into a BitImage
pub fn load_pbm(path: &Path) -> Result<BitImage, String> {
    let mut file = File::open(path).map_err(|e| format!("Failed to open file: {}", e))?;
    let mut reader = BufReader::new(&mut file);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("Failed to read magic number: {}", e))?;
    if line.trim() != "P4" {
        return Err(format!("Unsupported PBM format: {}", line.trim()));
    }

    loop {
        line.clear();
        reader.read_line(&mut line).map_err(|e| format!("Failed to read dimensions: {}", e))?;
        let trimmed = line.trim();
        if !trimmed.starts_with('#') && !trimmed.is_empty() {
            break;
        }
    }

    let dimensions: Vec<&str> = line.trim().split_whitespace().collect();
    if dimensions.len() != 2 {
        return Err("Invalid dimensions".to_string());
    }
    let width = dimensions[0].parse::<usize>().map_err(|_| "Invalid width".to_string())?;
    let height = dimensions[1].parse::<usize>().map_err(|_| "Invalid height".to_string())?;

    let current_pos = reader.stream_position().map_err(|e| format!("Failed to get position: {}", e))?;
    let width_in_bytes = (width + 7) / 8;
    let mut data = vec![0u8; width_in_bytes * height];
    file.seek(std::io::SeekFrom::Start(current_pos)).map_err(|e| format!("Seek failed: {}", e))?;
    file.read_exact(&mut data).map_err(|e| format!("Read failed: {}", e))?;

    Ok(BitImage::from_bytes(width, height, &data))
}