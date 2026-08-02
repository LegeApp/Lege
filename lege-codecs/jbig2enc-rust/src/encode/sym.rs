//! This module defines the core data structures for JBIG2 symbols and bitmaps,
//! and provides utilities for their manipulation, such as sorting for optimal
//! dictionary encoding.

use bitvec::order::Msb0;
use bitvec::prelude::*;
use bitvec::slice::BitSlice;
use ndarray::Array2;
use once_cell::sync::OnceCell;
use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

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

/// Convert a `BitVec` to a byte vector. `BitVec<u8, Msb0>::into_vec()` already
/// returns the underlying byte-aligned storage with trailing bits zero-padded,
/// so no further padding is required.
pub fn bitvec_to_bytes(bits: &BitSlice<u8, Msb0>) -> Vec<u8> {
    let mut bytes = bits.to_bitvec().into_vec();
    // Defensively mask any stale bits in the final byte to ensure the trailing
    // pad bits are zero (matches the contract callers expect for JBIG2 bitmaps).
    let trailing = bits.len() % 8;
    if trailing != 0 {
        if let Some(last) = bytes.last_mut() {
            let mask = 0xFFu8 << (8 - trailing);
            *last &= mask;
        }
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

fn checked_dimensions(width: usize, height: usize) -> Result<usize, String> {
    if !(BitImage::MIN_DIMENSION..=BitImage::MAX_DIMENSION).contains(&width)
        || !(BitImage::MIN_DIMENSION..=BitImage::MAX_DIMENSION).contains(&height)
    {
        return Err(format!(
            "dimensions must each be between {} and {}",
            BitImage::MIN_DIMENSION,
            BitImage::MAX_DIMENSION
        ));
    }
    width
        .checked_mul(height)
        .ok_or_else(|| "bitmap dimensions overflow".to_string())
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
            return Err(format!(
                "width must be between 1 and {}",
                Self::MAX_DIMENSION
            ));
        }
        if height == 0 || height > Self::MAX_DIMENSION as u32 {
            return Err(format!(
                "height must be between 1 and {}",
                Self::MAX_DIMENSION
            ));
        }

        let width = u32_to_usize(width);
        let height = u32_to_usize(height);
        let total_bits = width
            .checked_mul(height)
            .ok_or_else(|| "bitmap dimensions overflow".to_string())?;
        let mut bits = BitVec::with_capacity(total_bits);
        bits.resize(total_bits, false);

        Ok(Self {
            width,
            height,
            bits,
            packed_cache: OnceCell::new(),
        })
    }

    /// Creates a bitmap from raw bytes.
    pub fn from_bytes(width: usize, height: usize, bytes: &[u8]) -> Result<Self, String> {
        let bit_count = checked_dimensions(width, height)?;
        let expected_bytes = bit_count.div_ceil(8);
        if bytes.len() != expected_bytes {
            return Err(format!(
                "expected {expected_bytes} bytes for {width}x{height} bitmap, got {}",
                bytes.len()
            ));
        }
        let bits = bytes_to_bitvec(bytes, bit_count);
        Ok(Self {
            width,
            height,
            bits,
            packed_cache: OnceCell::new(),
        })
    }

    pub fn try_from_bytes(width: usize, height: usize, bytes: &[u8]) -> Result<Self, String> {
        Self::from_bytes(width, height, bytes)
    }

    pub fn from_bits(
        width: usize,
        height: usize,
        bits: &BitSlice<u8, Msb0>,
    ) -> Result<Self, String> {
        let bit_count = checked_dimensions(width, height)?;
        if bits.len() != bit_count {
            return Err(format!(
                "expected {bit_count} bits for {width}x{height} bitmap, got {}",
                bits.len()
            ));
        }
        Ok(Self {
            width,
            height,
            bits: bits.to_bitvec(),
            packed_cache: OnceCell::new(),
        })
    }

    pub fn try_from_bits(
        width: usize,
        height: usize,
        bits: &BitSlice<u8, Msb0>,
    ) -> Result<Self, String> {
        Self::from_bits(width, height, bits)
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
        let _ = self.packed_cache.take();
        &mut self.bits
    }

    /// Gets a single bit by index.
    #[inline]
    pub fn get_at(&self, idx: usize) -> bool {
        self.bits.get(idx).map_or(false, |b| *b)
    }

    /// Returns packed 32-bit words for efficient comparison and generic-region
    /// encoding. Results are cached to avoid repeated repacking work.
    pub fn packed_words(&self) -> &[u32] {
        self.packed_cache.get_or_init(|| {
            let words_per_row = (self.width + 31) / 32;
            let mut out = Vec::with_capacity(words_per_row * self.height);

            for y in 0..self.height {
                let row_offset = y * self.width;
                let row_bits = &self.bits[row_offset..row_offset + self.width];
                let mut row_bytes = row_bits.chunks(8).map(|chunk| {
                    let mut byte = chunk.load_be::<u8>();
                    if chunk.len() < 8 {
                        byte <<= 8 - chunk.len();
                    }
                    byte
                });

                for _ in 0..words_per_row {
                    let mut word = 0u32;
                    for byte_idx in 0..4 {
                        if let Some(byte) = row_bytes.next() {
                            word |= (byte as u32) << (24 - byte_idx * 8);
                        }
                    }
                    out.push(word);
                }
            }

            out
        })
    }

    /// Converts to packed 32-bit words for callers that need owned storage.
    pub fn to_packed_words(&self) -> Vec<u32> {
        self.packed_words().to_vec()
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

    /// Gets the value of a pixel without bounds checking (alias for get_usize for CC analysis compatibility).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `x` and `y` are within the bitmap's bounds.
    #[inline(always)]
    pub fn get_pixel_unchecked(&self, x: usize, y: usize) -> bool {
        self.bits[y * self.width + x]
    }

    /// Creates a sub-image from a specified rectangle.
    pub fn from_sub_image(source: &BitImage, rect: &Rect) -> Result<Self, String> {
        let right = rect
            .x
            .checked_add(rect.width)
            .ok_or_else(|| "sub-image x extent overflow".to_string())?;
        let bottom = rect
            .y
            .checked_add(rect.height)
            .ok_or_else(|| "sub-image y extent overflow".to_string())?;
        if right > usize_to_u32(source.width) || bottom > usize_to_u32(source.height) {
            return Err("sub-image rectangle out of bounds".to_string());
        }
        let width = u32_to_usize(rect.width);
        let height = u32_to_usize(rect.height);
        let mut result = Self::new(rect.width, rect.height)?;
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
        Ok(result)
    }

    /// Sets a pixel value at (x, y).
    #[inline]
    pub fn set(&mut self, x: u32, y: u32, value: bool) {
        if x < usize_to_u32(self.width) && y < usize_to_u32(self.height) {
            let idx = u32_to_usize(y) * self.width + u32_to_usize(x);
            let _ = self.packed_cache.take();
            self.bits.set(idx, value);
        }
    }

    /// Sets a pixel value with usize coordinates (alias for CC analysis compatibility).
    #[inline]
    pub fn set_usize(&mut self, x: usize, y: usize, value: bool) {
        if x < self.width && y < self.height {
            let idx = y * self.width + x;
            let _ = self.packed_cache.take();
            self.bits.set(idx, value);
        }
    }

    /// Crops the bitmap to a specified rectangle.
    pub fn crop(&self, rect: &Rect) -> Result<Self, String> {
        let right = rect
            .x
            .checked_add(rect.width)
            .ok_or_else(|| "crop x extent overflow".to_string())?;
        let bottom = rect
            .y
            .checked_add(rect.height)
            .ok_or_else(|| "crop y extent overflow".to_string())?;
        if right > usize_to_u32(self.width) || bottom > usize_to_u32(self.height) {
            return Err("crop rectangle out of bounds".to_string());
        }
        let mut cropped =
            Self::new(rect.width, rect.height).map_err(|e| format!("invalid crop: {e}"))?;
        for dy in 0..rect.height {
            for dx in 0..rect.width {
                let src_idx = u32_to_usize(rect.y + dy) * self.width + u32_to_usize(rect.x + dx);
                let dst_idx = u32_to_usize(dy) * u32_to_usize(rect.width) + u32_to_usize(dx);
                if let Some(bit) = self.bits.get(src_idx) {
                    cropped.bits.set(dst_idx, *bit);
                }
            }
        }
        Ok(cropped)
    }

    pub fn try_crop(&self, rect: &Rect) -> Result<Self, String> {
        self.crop(rect)
    }

    /// Trims whitespace from edges, returning the bounding rectangle and cropped image.
    pub fn trim(&self) -> (Rect, BitImage) {
        if self.bits.is_empty() || self.bits.not_any() {
            return (
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                Self::new(1, 1).expect("Failed to create minimal empty image"),
            );
        }

        let mut min_x = self.width;
        let mut min_y = self.height;
        let mut max_x = 0;
        let mut max_y = 0;

        // First pass: find min_y and max_y using word-level row scanning
        for y in 0..self.height {
            let row_start = y * self.width;
            let row_bits = &self.bits[row_start..row_start + self.width];
            if row_bits.any() {
                min_y = y;
                break;
            }
        }

        for y in (0..self.height).rev() {
            let row_start = y * self.width;
            let row_bits = &self.bits[row_start..row_start + self.width];
            if row_bits.any() {
                max_y = y;
                break;
            }
        }

        if min_y > max_y {
            // Should be unreachable if not_any() is false
            return (
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                Self::new(1, 1).expect("Failed to create minimal empty image"),
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

        if min_x > max_x {
            // Should be unreachable
            return (
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                Self::new(1, 1).expect("Failed to create minimal empty image"),
            );
        }

        let rect = Rect {
            x: usize_to_u32(min_x),
            y: usize_to_u32(min_y),
            width: usize_to_u32(max_x - min_x + 1),
            height: usize_to_u32(max_y - min_y + 1),
        };
        (
            rect,
            self.crop(&rect)
                .expect("trimmed rectangle is within the source image"),
        )
    }

    /// Inverts all bits in the bitmap.
    pub fn invert(&mut self) {
        self.bits.iter_mut().for_each(|mut bit| *bit = !*bit);
    }

    /// Performs a logical AND with another bitmap.
    pub fn and(&self, other: &Self) -> Self {
        assert_eq!(self.width, other.width, "Bitmaps must have the same width");
        assert_eq!(
            self.height, other.height,
            "Bitmaps must have the same height"
        );
        let mut result = self.clone();
        result.bits &= &other.bits;
        result
    }

    /// Performs a logical OR with another bitmap.
    pub fn or(&self, other: &Self) -> Self {
        assert_eq!(self.width, other.width, "Bitmaps must have the same width");
        assert_eq!(
            self.height, other.height,
            "Bitmaps must have the same height"
        );
        let mut result = self.clone();
        result.bits |= &other.bits;
        result
    }

    /// Performs a logical XOR with another bitmap.
    pub fn xor(&self, other: &Self) -> Self {
        assert_eq!(self.width, other.width, "Bitmaps must have the same width");
        assert_eq!(
            self.height, other.height,
            "Bitmaps must have the same height"
        );
        let mut result = self.clone();
        result.bits ^= &other.bits;
        result
    }

    /// Counts set bits (1s) in the bitmap.
    pub fn count_ones(&self) -> usize {
        crate::jbig2simd::count_packed_words_ones(self.packed_words(), self.width, self.height)
    }

    /// Counts unset bits (0s) in the bitmap.
    pub fn count_zeros(&self) -> usize {
        self.bits.len() - self.count_ones()
    }

    /// Gets a pixel value safely, returning 0 for out-of-bounds.
    ///
    /// Casting negative i32 to u32 yields a huge positive value that naturally
    /// fails the `< width/height` check, collapsing 4 branches into 2.
    #[inline]
    pub fn get_pixel_safely(&self, x: i32, y: i32) -> u8 {
        if (x as u32) < (self.width as u32) && (y as u32) < (self.height as u32) {
            let idx = (y as usize) * self.width + (x as usize);
            self.bits[idx] as u8
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
///
/// Uses stable sorting to preserve the input order for symbols with identical dimensions,
/// ensuring consistency with canonicalize_dict_symbols().
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
    // Use stable sort to preserve input order for equal widths.
    height_classes
        .into_values()
        .map(|mut symbol_group| {
            symbol_group.sort_by(|a, b| a.width.cmp(&b.width));
            symbol_group
        })
        .collect()
}

/// Computes a hash for a `BitImage` using SipHash from std library.
pub fn compute_glyph_hash(image: &BitImage) -> u64 {
    let mut hasher = DefaultHasher::new();
    image.as_bytes().hash(&mut hasher);
    hasher.finish()
}

/// Converts an `ndarray::Array2<u8>` to a `BitImage`.
pub fn array_to_bitimage(array: &Array2<u8>) -> Result<BitImage, String> {
    let (height, width) = array.dim();
    let total = checked_dimensions(width, height)?;
    let mut bits = bitvec::bitvec![u8, Msb0; 0; total];

    let mut idx = 0usize;
    for row in array.rows() {
        for &pixel in row.iter() {
            if pixel > 0 {
                bits.set(idx, true);
            }
            idx += 1;
        }
    }

    BitImage::from_bits(width, height, &bits)
}

/// Converts unpacked binary pixels (one byte per pixel, non-zero = set) to a `BitImage`.
pub fn binary_pixels_to_bitimage(
    pixels: &[u8],
    width: usize,
    height: usize,
) -> Result<BitImage, String> {
    if !(BitImage::MIN_DIMENSION..=BitImage::MAX_DIMENSION).contains(&width) {
        return Err(format!(
            "width must be between {} and {}",
            BitImage::MIN_DIMENSION,
            BitImage::MAX_DIMENSION
        ));
    }
    if !(BitImage::MIN_DIMENSION..=BitImage::MAX_DIMENSION).contains(&height) {
        return Err(format!(
            "height must be between {} and {}",
            BitImage::MIN_DIMENSION,
            BitImage::MAX_DIMENSION
        ));
    }
    let expected_len = width
        .checked_mul(height)
        .ok_or_else(|| "Dimensions too large".to_string())?;
    if pixels.len() < expected_len {
        return Err(format!(
            "Binary pixel buffer too small: expected {}, got {}",
            expected_len,
            pixels.len()
        ));
    }

    let bits = pixels[..expected_len]
        .iter()
        .map(|&pixel| pixel > 0)
        .collect::<BitVec<u8, Msb0>>();

    BitImage::from_bits(width, height, bits.as_bitslice())
}

/// Loads a PBM file into a BitImage
pub fn load_pbm(path: &Path) -> Result<BitImage, String> {
    let data = std::fs::read(path).map_err(|e| format!("failed to read PBM: {e}"))?;
    let mut offset = 0usize;
    let magic = pbm_token(&data, &mut offset)?;
    if magic != b"P4" {
        return Err("unsupported PBM format (expected P4)".to_string());
    }
    let width = std::str::from_utf8(pbm_token(&data, &mut offset)?)
        .map_err(|_| "invalid PBM width".to_string())?
        .parse::<usize>()
        .map_err(|_| "Invalid width".to_string())?;
    let height = std::str::from_utf8(pbm_token(&data, &mut offset)?)
        .map_err(|_| "invalid PBM height".to_string())?
        .parse::<usize>()
        .map_err(|_| "Invalid height".to_string())?;
    if !(BitImage::MIN_DIMENSION..=BitImage::MAX_DIMENSION).contains(&width)
        || !(BitImage::MIN_DIMENSION..=BitImage::MAX_DIMENSION).contains(&height)
    {
        return Err(format!(
            "PBM dimensions must each be between {} and {}",
            BitImage::MIN_DIMENSION,
            BitImage::MAX_DIMENSION
        ));
    }

    let separator = *data
        .get(offset)
        .filter(|b| b.is_ascii_whitespace())
        .ok_or_else(|| "PBM header is not terminated by whitespace".to_string())?;
    offset += 1;
    if separator == b'\r' && data.get(offset) == Some(&b'\n') {
        offset += 1;
    }
    let width_in_bytes = width.div_ceil(8);
    let data_len = width_in_bytes
        .checked_mul(height)
        .ok_or_else(|| "PBM dimensions are too large".to_string())?;
    let raster = data
        .get(offset..offset.saturating_add(data_len))
        .filter(|r| r.len() == data_len)
        .ok_or_else(|| "truncated PBM raster".to_string())?;

    // P4 pads every row to a byte boundary. `BitImage` itself is tightly
    // packed, so copying the whole payload as one bit string would turn each
    // row's padding into pixels on the next row whenever width % 8 != 0.
    let mut image = BitImage::new(width as u32, height as u32)?;
    for y in 0..height {
        let row = &raster[y * width_in_bytes..(y + 1) * width_in_bytes];
        for x in 0..width {
            let black = row[x / 8] & (0x80 >> (x % 8)) != 0;
            if black {
                image.set_usize(x, y, true);
            }
        }
    }
    Ok(image)
}

fn skip_pbm_separators(data: &[u8], offset: &mut usize) {
    loop {
        while data.get(*offset).is_some_and(u8::is_ascii_whitespace) {
            *offset += 1;
        }
        if data.get(*offset) != Some(&b'#') {
            break;
        }
        while data
            .get(*offset)
            .is_some_and(|b| *b != b'\n' && *b != b'\r')
        {
            *offset += 1;
        }
    }
}

fn pbm_token<'a>(data: &'a [u8], offset: &mut usize) -> Result<&'a [u8], String> {
    skip_pbm_separators(data, offset);
    let start = *offset;
    while data
        .get(*offset)
        .is_some_and(|b| !b.is_ascii_whitespace() && *b != b'#')
    {
        *offset += 1;
    }
    if start == *offset {
        Err("missing PBM header token".to_string())
    } else {
        Ok(&data[start..*offset])
    }
}

#[cfg(test)]
mod tests {
    use super::{BitImage, Rect, array_to_bitimage, binary_pixels_to_bitimage, load_pbm};
    use ndarray::array;
    use std::io::Write;

    #[test]
    fn binary_pixels_match_array_conversion() {
        let pixels = vec![0, 1, 0, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1, 1, 0];
        let array = array![[0u8, 1, 0, 1, 1], [1u8, 0, 0, 0, 1], [0u8, 0, 1, 1, 0],];

        let from_pixels = binary_pixels_to_bitimage(&pixels, 5, 3).unwrap();
        let from_array = array_to_bitimage(&array).unwrap();

        assert_eq!(from_pixels, from_array);
    }

    #[test]
    fn p4_loader_discards_each_rows_padding_bits() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"P4\n9 2\n").unwrap();
        // Row 0: x=0 and x=8. Row 1: x=1. The low seven bits of each
        // second byte are padding and deliberately set to catch row bleed.
        file.write_all(&[0x80, 0xFF, 0x40, 0x7F]).unwrap();
        file.flush().unwrap();

        let image = load_pbm(file.path()).unwrap();
        assert_eq!((image.width, image.height), (9, 2));
        assert!(image.get_usize(0, 0));
        assert!(image.get_usize(8, 0));
        assert!(image.get_usize(1, 1));
        assert_eq!(image.count_ones(), 3);
    }

    #[test]
    fn binary_pixels_reject_zero_dimensions() {
        assert!(binary_pixels_to_bitimage(&[], 0, 0).is_err());
    }

    #[test]
    fn fallible_bitmap_apis_reject_bad_geometry_without_panicking() {
        assert!(BitImage::from_bytes(usize::MAX, 2, &[]).is_err());
        let empty_bits = bitvec::vec::BitVec::<u8, bitvec::order::Msb0>::new();
        assert!(BitImage::from_bits(2, 2, empty_bits.as_bitslice()).is_err());
        let image = BitImage::new(2, 2).unwrap();
        assert!(
            image
                .crop(&Rect {
                    x: u32::MAX,
                    y: 0,
                    width: 2,
                    height: 1,
                })
                .is_err()
        );
        assert!(
            BitImage::from_sub_image(
                &image,
                &Rect {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                },
            )
            .is_err()
        );
        assert!(array_to_bitimage(&ndarray::Array2::<u8>::zeros((0, 0))).is_err());
    }

    #[test]
    fn p4_loader_accepts_split_tokens_and_comments_without_eating_raster_whitespace() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"P4 # magic comment\n8\n# size comment\n1\n")
            .unwrap();
        file.write_all(&[0x0A]).unwrap();
        file.flush().unwrap();
        let image = load_pbm(file.path()).unwrap();
        assert_eq!(image.to_jbig2_format(), vec![0x0A]);
    }
}

/// Helper function to find the first black pixel in packed u32 data
/// Returns (x, y) coordinates of the first black pixel, or None if no black pixels
pub fn first_black_pixel_in_packed(
    packed: &[u32],
    width: usize,
    height: usize,
) -> Option<(usize, usize)> {
    let words_per_row = (width + 31) / 32;

    for y in 0..height {
        let row_start = y * words_per_row;
        for word_idx in 0..words_per_row {
            if row_start + word_idx >= packed.len() {
                break;
            }

            let word = packed[row_start + word_idx];
            if word != 0 {
                // Find the first set bit in this word
                let bit_pos = word.leading_zeros() as usize;
                let x = word_idx * 32 + bit_pos;
                if x < width {
                    return Some((x, y));
                }
            }
        }
    }
    None
}

// ==============================================
