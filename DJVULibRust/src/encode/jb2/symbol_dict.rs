//! Core data structures for JB2 encoding.
//!
//! This module provides:
//! - `BitImage`: The canonical bilevel bitmap type used by the encoder
//! - `Rect`: Simple bounding box for regions
//! - `Comparator`: Symbol matching with spatial search for dictionary building
//! - Simple shared dictionary support for multi-page encoding

use bitvec::order::Msb0;
use bitvec::prelude::*;
use std::error::Error;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

/// Errors that can occur when creating or manipulating a `BitImage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BitImageError {
    /// The specified dimensions would result in a bitmap that is too large to allocate.
    TooLarge { width: u32, height: u32 },
}

impl fmt::Display for BitImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BitImageError::TooLarge { width, height } => {
                write!(f, "image dimensions ({}x{}) are too large", width, height)
            }
        }
    }
}

impl Error for BitImageError {}

// ==============================================
// Core Data Structures (from jbig2sym.rs)
// ==============================================

/// A simple rectangle, used for bounding boxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A bitmap image using MSB-first bit ordering for JB2 compatibility.
#[derive(Clone, Debug, Eq)]
pub struct BitImage {
    pub width: usize,
    pub height: usize,
    bits: BitVec<u8, Msb0>,
    packed_cache: OnceLock<Vec<u32>>,
}

impl PartialEq for BitImage {
    fn eq(&self, other: &Self) -> bool {
        if self.width != other.width || self.height != other.height {
            return false;
        }
        self.to_packed_words() == other.to_packed_words()
    }
}

impl Hash for BitImage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.width.hash(state);
        self.height.hash(state);
        self.to_packed_words().hash(state);
    }
}

impl BitImage {
    pub fn new(width: u32, height: u32) -> Result<Self, BitImageError> {
        let width_us = width as usize;
        let height_us = height as usize;
        let total_bits = match width_us.checked_mul(height_us) {
            Some(bits) if bits < (isize::MAX as usize) => bits,
            _ => return Err(BitImageError::TooLarge { width, height }),
        };

        let mut bits = BitVec::with_capacity(total_bits);
        bits.resize(total_bits, false);
        Ok(Self {
            width: width_us,
            height: height_us,
            bits,
            packed_cache: OnceLock::new(),
        })
    }

    pub fn from_bytes(width: usize, height: usize, bytes: &[u8]) -> Self {
        let mut bv = BitVec::from_slice(bytes);
        bv.truncate(width * height);
        Self {
            width,
            height,
            bits: bv,
            packed_cache: OnceLock::new(),
        }
    }

    /// Gets the value of a pixel without bounds checking.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `x` and `y` are within the bitmap's bounds,
    /// otherwise this function will panic.
    #[inline(always)]
    pub fn get_pixel_unchecked(&self, x: usize, y: usize) -> bool {
        self.bits[y * self.width + x]
    }

    pub fn set_usize(&mut self, x: usize, y: usize, val: bool) {
        if x >= self.width || y >= self.height {
            return;
        }
        let idx = y * self.width + x;
        if idx < self.bits.len() {
            self.bits.set(idx, val);
        }
        self.packed_cache.take(); // Invalidate cache
    }

    pub fn to_packed_words(&self) -> &[u32] {
        self.packed_cache.get_or_init(|| {
            let words_per_row = (self.width + 31) / 32;
            let mut out = Vec::with_capacity(words_per_row * self.height);
            for y in 0..self.height {
                for word_x in 0..words_per_row {
                    let mut w = 0u32;
                    for bit in 0..32 {
                        let x = word_x * 32 + bit;
                        if x < self.width && self.get_pixel_unchecked(x, y) {
                            w |= 1u32 << (31 - bit);
                        }
                    }
                    out.push(w);
                }
            }
            out
        })
    }
}

// Lutz trait implementation removed - using homegrown connected components instead

// ==============================================
// Connected Components (from jbig2lutz.rs)
// ==============================================

/// A connected component with bounding box and pixel information
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectedComponent {
    pub bitmap: BitImage,
    pub bounds: Rect,
    // The index of the symbol in the dictionary that this component was matched to.
    pub dict_symbol_index: Option<usize>,
    pub pixel_count: usize,
    pub pixels: Vec<(u32, u32)>,
}

/// Finds connected components using homegrown algorithm
/// Note: This is a placeholder - actual implementation uses the homegrown CC algorithm
pub fn find_connected_components(_image: &BitImage, _min_size: usize) -> Vec<ConnectedComponent> {
    // This function is deprecated - use the homegrown connected components instead
    Vec::new()
}

// ==============================================
// Symbol Comparison (from jbig2comparator.rs)
// ==============================================

const SEARCH_RADIUS: i32 = 2;

#[derive(Default)]
pub struct Comparator {
    tmp: Vec<u32>,
}

impl Comparator {
    fn get_word(row: &[u32], idx: isize) -> u32 {
        if idx < 0 {
            0
        } else {
            row.get(idx as usize).copied().unwrap_or(0)
        }
    }

    pub fn distance(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
    ) -> Option<(u32, i32, i32)> {
        if (a.width as i32 - b.width as i32).abs() > SEARCH_RADIUS * 2
            || (a.height as i32 - b.height as i32).abs() > SEARCH_RADIUS * 2
        {
            return None;
        }

        let awpr = ((a.width + 31) >> 5) as usize;
        let bwpr = ((b.width + 31) >> 5) as usize;
        let wpr_overlap = ((a.width.max(b.width) + 31) >> 5) as usize;
        if self.tmp.len() < wpr_overlap {
            self.tmp.resize(wpr_overlap, 0);
        }

        let mut best_err = max_err + 1;
        let mut best_dx = 0;
        let mut best_dy = 0;

        let a_words = a.to_packed_words();
        let b_words = b.to_packed_words();

        for dy in -SEARCH_RADIUS..=SEARCH_RADIUS {
            for dx in -SEARCH_RADIUS..=SEARCH_RADIUS {
                let mut err = 0u32;
                let bit_dx = (dx & 31) as u32;
                let word_dx = (dx >> 5) as isize;

                let y0 = dy.max(0) as u32;
                let y1 = (a.height as i32 + dy).min(b.height as i32).max(0) as u32;
                if y1 <= y0 {
                    continue;
                }

                for row in 0..(y1 - y0) {
                    let a_row_idx = (row as i32 + y0 as i32 - dy) as usize * awpr;
                    let b_row_idx = (row as i32 + y0 as i32) as usize * bwpr;

                    if a_row_idx >= a_words.len() || b_row_idx >= b_words.len() {
                        continue;
                    }

                    let a_row_end = std::cmp::min(a_row_idx + awpr, a_words.len());
                    let b_row_end = std::cmp::min(b_row_idx + bwpr, b_words.len());

                    let a_row = &a_words[a_row_idx..a_row_end];
                    let b_row = &b_words[b_row_idx..b_row_end];

                    for w in 0..((a.width.max(b.width) + 31) >> 5) {
                        let idx = w as isize + word_dx;
                        let aw = Self::get_word(a_row, idx);
                        let aw_next = if bit_dx == 0 {
                            0
                        } else {
                            Self::get_word(a_row, idx + 1)
                        };
                        let aligned_a = if bit_dx == 0 {
                            aw
                        } else {
                            (aw << bit_dx) | (aw_next >> (32 - bit_dx))
                        };
                        let bw = Self::get_word(b_row, w as isize);
                        let xor_result = aligned_a ^ bw;
                        err += xor_result.count_ones();
                        if err >= best_err {
                            break;
                        }
                    }
                    if err >= best_err {
                        break;
                    }
                }

                if err < best_err {
                    best_err = err;
                    best_dx = dx;
                    best_dy = dy;
                }
            }
        }

        if best_err <= max_err {
            Some((best_err, best_dx, best_dy))
        } else {
            None
        }
    }
}

// ==============================================
// Shared Dictionary Support
// ==============================================

/// Simple shared dictionary for multi-page JB2 encoding.
///
/// This provides the minimal infrastructure needed for DjVu's Djbz/Sjbz
/// multi-page workflow:
/// 1. Encode shared shapes once → Djbz chunk via `JB2Encoder::encode_dictionary()`
/// 2. Each page references inherited shapes → Sjbz chunk via
///    `JB2Encoder::encode_page_with_shapes()` with `inherited_shape_count`
///
/// No RLE compression or complex type hierarchy — just Arc-based sharing.
#[derive(Clone, Debug)]
pub struct SharedDict {
    shapes: Arc<Vec<BitImage>>,
}

impl SharedDict {
    /// Create a new shared dictionary from a vector of shapes.
    pub fn new(shapes: Vec<BitImage>) -> Self {
        Self {
            shapes: Arc::new(shapes),
        }
    }

    /// Get the number of shapes in this dictionary.
    pub fn shape_count(&self) -> usize {
        self.shapes.len()
    }

    /// Get a reference to a shape by index.
    pub fn get_shape(&self, index: usize) -> Option<&BitImage> {
        self.shapes.get(index)
    }

    /// Get a reference to all shapes.
    pub fn shapes(&self) -> &[BitImage] {
        &self.shapes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitimage_creation() {
        let img = BitImage::new(10, 10);
        assert!(img.is_ok());
        let img = img.unwrap();
        assert_eq!(img.width, 10);
        assert_eq!(img.height, 10);
    }

    #[test]
    fn test_comparator_exact_match() {
        let mut img1 = BitImage::new(5, 5).unwrap();
        let mut img2 = BitImage::new(5, 5).unwrap();
        
        // Set same pixels
        img1.set_usize(2, 2, true);
        img2.set_usize(2, 2, true);
        
        let mut comp = Comparator::default();
        let result = comp.distance(&img1, &img2, 100);
        assert!(result.is_some());
        let (err, dx, dy) = result.unwrap();
        assert_eq!(err, 0); // Exact match
        assert_eq!(dx, 0);
        assert_eq!(dy, 0);
    }

    #[test]
    fn test_shared_dict() {
        let shapes = vec![
            BitImage::new(10, 10).unwrap(),
            BitImage::new(15, 15).unwrap(),
        ];
        let dict = SharedDict::new(shapes);
        assert_eq!(dict.shape_count(), 2);
        assert!(dict.get_shape(0).is_some());
        assert!(dict.get_shape(1).is_some());
        assert!(dict.get_shape(2).is_none());
    }
}
