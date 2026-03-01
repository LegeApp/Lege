//! This module defines the core data structures for JB2 symbols and bitmaps,
//! and provides utilities for their manipulation, such as sorting for optimal
//! dictionary encoding.

use crate::encode::zc::ZEncoder;
use crate::encode::jb2::context;
use crate::encode::jb2::error::Jb2Error;
use crate::encode::jb2::num_coder::NumCoder;
use bitvec::order::Msb0;
use bitvec::prelude::*;
use lutz;
use once_cell::unsync::OnceCell;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::Write;

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
    packed_cache: OnceCell<Vec<u32>>,
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
            packed_cache: OnceCell::new(),
        })
    }

    pub fn from_bytes(width: usize, height: usize, bytes: &[u8]) -> Self {
        let mut bv = BitVec::from_slice(bytes);
        bv.truncate(width * height);
        Self {
            width,
            height,
            bits: bv,
            packed_cache: OnceCell::new(),
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

impl lutz::Image for BitImage {
    fn width(&self) -> u32 {
        self.width as u32
    }
    fn height(&self) -> u32 {
        self.height as u32
    }

    fn has_pixel(&self, x: u32, y: u32) -> bool {
        // Return true only if there's a foreground pixel at this location
        if x < self.width as u32 && y < self.height as u32 {
            self.get_pixel_unchecked(x as usize, y as usize)
        } else {
            false
        }
    }
}

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

/// Finds connected components using Lutz algorithm
pub fn find_connected_components(image: &BitImage, min_size: usize) -> Vec<ConnectedComponent> {
    let components = lutz::lutz::<_, Vec<lutz::Pixel>>(image);
    let mut result = Vec::new();
    for pixels in components {
        if pixels.len() >= min_size {
            let mut min_x = u32::MAX;
            let mut min_y = u32::MAX;
            let mut max_x = 0;
            let mut max_y = 0;
            for p in &pixels {
                min_x = min_x.min(p.x);
                min_y = min_y.min(p.y);
                max_x = max_x.max(p.x);
                max_y = max_y.max(p.y);
            }

            let width = max_x - min_x + 1;
            let height = max_y - min_y + 1;
            if let Ok(mut bitmap) = BitImage::new(width, height) {
                for p in &pixels {
                    bitmap.set_usize((p.x - min_x) as usize, (p.y - min_y) as usize, true);
                }

                let component = ConnectedComponent {
                    bitmap,
                    bounds: Rect {
                        x: min_x,
                        y: min_y,
                        width,
                        height,
                    },
                    dict_symbol_index: None,
                    pixel_count: pixels.len(),
                    pixels: pixels.into_iter().map(|p| (p.x, p.y)).collect(),
                };
                result.push(component);
            }
        }
    }
    result
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

/// Builds a symbol dictionary from a page image by finding and clustering symbols.
pub struct SymDictBuilder {
    comparator: Comparator,
    max_error: u32,
    exact_matches: HashMap<BitImage, usize>,
}

impl SymDictBuilder {
    /// Creates a new symbol dictionary builder.
    pub fn new(max_error: u32) -> Self {
        Self {
            comparator: Comparator::default(),
            max_error,
            exact_matches: HashMap::new(),
        }
    }

    /// Builds a dictionary from the given image.
    ///
    /// Returns the dictionary (as a vector of `BitImage`s) and a vector of
    /// `ConnectedComponent`s which includes the index of the dictionary symbol
    /// that each component was matched with.
    pub fn build(&mut self, image: &BitImage) -> (Vec<BitImage>, Vec<ConnectedComponent>) {
        let mut components = find_connected_components(image, 4);
        let mut dictionary: Vec<BitImage> = Vec::new();
        self.exact_matches.clear();

        for component in &mut components {
            // 1. Check for an exact match, which is fast.
            if let Some(&dict_idx) = self.exact_matches.get(&component.bitmap) {
                component.dict_symbol_index = Some(dict_idx);
                continue;
            }

            // 2. If no exact match, and if lossy compression is allowed, search for a close match.
            let mut best_match: Option<(u32, usize)> = None;
            if self.max_error > 0 {
                for (dict_idx, dict_symbol) in dictionary.iter().enumerate() {
                    if let Some((error, _dx, _dy)) =
                        self.comparator
                            .distance(&component.bitmap, dict_symbol, self.max_error)
                    {
                        if best_match.map_or(true, |(e, _)| error < e) {
                            best_match = Some((error, dict_idx));
                        }
                    }
                }
            }

            // 3. Decide whether to use the found match or add a new symbol to the dictionary.
            if let Some((error, dict_idx)) = best_match {
                if error <= self.max_error {
                    component.dict_symbol_index = Some(dict_idx);
                    // Don't add to exact_matches because it wasn't an exact match.
                    continue;
                }
            }

            // 4. No suitable match found, add this component's bitmap as a new symbol.
            let new_symbol_idx = dictionary.len();
            component.dict_symbol_index = Some(new_symbol_idx);
            dictionary.push(component.bitmap.clone());
            self.exact_matches
                .insert(component.bitmap.clone(), new_symbol_idx);
        }

        (dictionary, components)
    }
}

/// Encodes a symbol dictionary to the output stream.
pub struct SymDictEncoder {
    nc: NumCoder,
    direct_base_context: u32,
    ctx_sym_count: usize,
    ctx_sym_width: usize,
    ctx_sym_height: usize,
}

impl SymDictEncoder {
    /// Creates a new symbol dictionary encoder.
    pub fn new(base_context_index: u32, max_contexts: u32, direct_base_context: u32) -> Self {
        let nc = NumCoder::new(base_context_index as u8, max_contexts as u8);
        let ctx_sym_count = base_context_index as usize;
        let ctx_sym_width = base_context_index as usize + 1;
        let ctx_sym_height = base_context_index as usize + 2;

        Self {
            nc,
            direct_base_context,
            ctx_sym_count,
            ctx_sym_width,
            ctx_sym_height,
        }
    }

    /// Encodes the dictionary symbols to the arithmetic coder.
    pub fn encode<W: Write>(
        &mut self,
        ac: &mut ZEncoder<W>,
        dictionary: &[BitImage],
        contexts: &mut [u8], // Add global context array parameter
    ) -> Result<(), Jb2Error> {
        // 1. Encode the number of symbols in the dictionary.
        self.nc.encode_integer(
            ac,
            contexts,
            self.ctx_sym_count,
            dictionary.len() as i32,
            0,
            65535,
        )?;

        // 2. Encode each symbol.
        for symbol in dictionary {
            // Encode width and height.
            self.nc.encode_integer(
                ac,
                contexts,
                self.ctx_sym_width,
                symbol.width as i32,
                1,
                65535,
            )?;
            self.nc.encode_integer(
                ac,
                contexts,
                self.ctx_sym_height,
                symbol.height as i32,
                1,
                65535,
            )?;

            // Encode the raw bitmap data using the centralized direct coding function.
            context::encode_bitmap_direct(ac, symbol, self.direct_base_context as usize)?;
        }

        Ok(())
    }
}
