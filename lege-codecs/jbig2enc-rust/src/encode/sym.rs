//! This module defines the core data structures for JBIG2 symbols and bitmaps,
//! and provides utilities for their manipulation, such as sorting for optimal
//! dictionary encoding.

use ndarray::Array2;
use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use crate::jbig2shared::{u32_to_usize, usize_to_u32};

// ==============================================
// Bit manipulation utilities
// ==============================================

/// Read bit `idx` of a contiguous MSB-first bit buffer, `false` past the end.
///
/// "Contiguous" here means the packing used by [`BitImage::to_bytes`] and
/// [`BitImage::from_bytes`]: `width * height` bits laid end to end with no
/// per-row padding. It is *not* the crate's internal row-strided layout.
#[inline]
fn contiguous_bit(bytes: &[u8], idx: usize) -> bool {
    match bytes.get(idx >> 3) {
        Some(byte) => (byte >> (7 - (idx & 7))) & 1 != 0,
        None => false,
    }
}

/// Index of the first set bit at or after `from_bit` in a row of packed words,
/// or `None` if the row has none before `width`.
///
/// Scans a word at a time (`leading_zeros` on the masked remainder), which is
/// what the connected-component run extractor in `cc.rs` needs to skip runs of
/// white without touching individual pixels.
#[inline]
pub(crate) fn row_first_one(row: &[u32], from_bit: usize, width: usize) -> Option<usize> {
    if from_bit >= width {
        return None;
    }
    let mut word_index = from_bit >> 5;
    // Mask off the bits before `from_bit` in the first word.
    let mut word = *row.get(word_index)? & (u32::MAX >> (from_bit & 31));
    loop {
        if word != 0 {
            let bit = (word_index << 5) + word.leading_zeros() as usize;
            return (bit < width).then_some(bit);
        }
        word_index += 1;
        if (word_index << 5) >= width {
            return None;
        }
        word = *row.get(word_index)?;
    }
}

/// Index of the first clear bit at or after `from_bit`, or `None` if every bit
/// through `width - 1` is set. Padding bits past `width` are always zero, so
/// the width check is what stops this from reporting a padding bit.
#[inline]
pub(crate) fn row_first_zero(row: &[u32], from_bit: usize, width: usize) -> Option<usize> {
    if from_bit >= width {
        return None;
    }
    let mut word_index = from_bit >> 5;
    let mut word = !*row.get(word_index)? & (u32::MAX >> (from_bit & 31));
    loop {
        if word != 0 {
            let bit = (word_index << 5) + word.leading_zeros() as usize;
            return (bit < width).then_some(bit);
        }
        word_index += 1;
        if (word_index << 5) >= width {
            return None;
        }
        word = !*row.get(word_index)?;
    }
}

// ==============================================
// Bitmap image handling
// ==============================================

/// A bitmap image using MSB-first bit ordering for JBIG2 compatibility.
///
/// Storage is row-strided packed words: each row occupies
/// `stride_words = ceil(width / 32)` `u32`s, bit `x` of a row living at
/// `1 << (31 - (x % 32))` of word `x / 32`. This is byte-for-byte the layout
/// JBIG2 generic-region coding and the symbol comparator want, so
/// [`BitImage::packed_words`] hands out the storage directly and
/// [`BitImage::to_jbig2_format`] is a per-row copy rather than a per-pixel
/// loop. It is also the same layout as the decoder's `shared::bitmap::
/// MonoBitmap`, which is what lets the two halves of the crate convert in
/// bulk.
///
/// # Invariant
///
/// Padding bits — those past `width` in the last word of each row — are always
/// zero. Every mutator preserves this, which is what makes `PartialEq`,
/// `count_ones`, and the `any`/`all` row scans correct without masking at each
/// use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitImage {
    /// Width of the bitmap in pixels
    pub width: usize,
    /// Height of the bitmap in pixels
    pub height: usize,
    /// `u32`s per row, `ceil(width / 32)`.
    stride_words: usize,
    /// Row-major packed words, MSB-first within each word.
    words: Vec<u32>,
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

    /// `u32`s needed for one row of `width` pixels.
    #[inline]
    const fn stride_words_for(width: usize) -> usize {
        width.div_ceil(32)
    }

    /// Allocate an all-clear bitmap after validating the dimensions.
    fn alloc(width: usize, height: usize) -> Result<Self, String> {
        checked_dimensions(width, height)?;
        let stride_words = Self::stride_words_for(width);
        let total_words = stride_words
            .checked_mul(height)
            .ok_or_else(|| "bitmap storage overflow".to_string())?;
        Ok(Self {
            width,
            height,
            stride_words,
            words: vec![0u32; total_words],
        })
    }

    /// The packed words of row `y`.
    #[inline]
    pub(crate) fn row_words(&self, y: usize) -> &[u32] {
        let start = y * self.stride_words;
        &self.words[start..start + self.stride_words]
    }

    /// Restore the zero-padding invariant after a whole-word mutation.
    fn mask_row_padding(&mut self) {
        let used = self.width % 32;
        if used == 0 || self.stride_words == 0 {
            return;
        }
        let mask = u32::MAX << (32 - used);
        let last = self.stride_words - 1;
        for y in 0..self.height {
            self.words[y * self.stride_words + last] &= mask;
        }
    }

    #[inline(always)]
    fn get_xy(&self, x: usize, y: usize) -> bool {
        let word = self.words[y * self.stride_words + (x >> 5)];
        (word >> (31 - (x & 31))) & 1 != 0
    }

    #[inline(always)]
    fn set_xy(&mut self, x: usize, y: usize, value: bool) {
        let bit = 1u32 << (31 - (x & 31));
        let word = &mut self.words[y * self.stride_words + (x >> 5)];
        if value {
            *word |= bit;
        } else {
            *word &= !bit;
        }
    }

    /// Build from a contiguous (unpadded) MSB-first bit source.
    fn from_contiguous(
        width: usize,
        height: usize,
        mut bit: impl FnMut(usize) -> bool,
    ) -> Result<Self, String> {
        let mut image = Self::alloc(width, height)?;
        let mut idx = 0usize;
        for y in 0..height {
            for x in 0..width {
                if bit(idx) {
                    image.set_xy(x, y, true);
                }
                idx += 1;
            }
        }
        Ok(image)
    }

    /// Convert the BitImage to JBIG2-compatible format: row-major, MSB-first,
    /// each row padded out to a whole number of bytes.
    ///
    /// Because rows are already word-aligned in storage this is a big-endian
    /// copy per row plus a truncation of the row's padding bytes — where it
    /// used to be a triple-nested per-bit loop over the whole bitmap.
    pub fn to_jbig2_format(&self) -> Vec<u8> {
        let bytes_per_row = self.width.div_ceil(8);
        let mut result = Vec::with_capacity(bytes_per_row * self.height);
        for y in 0..self.height {
            let row_start = result.len();
            for word in self.row_words(y) {
                result.extend_from_slice(&word.to_be_bytes());
            }
            result.truncate(row_start + bytes_per_row);
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
        Self::alloc(u32_to_usize(width), u32_to_usize(height))
    }

    /// Creates a bitmap from raw bytes in the contiguous (unpadded) packing.
    pub fn from_bytes(width: usize, height: usize, bytes: &[u8]) -> Result<Self, String> {
        let bit_count = checked_dimensions(width, height)?;
        let expected_bytes = bit_count.div_ceil(8);
        if bytes.len() != expected_bytes {
            return Err(format!(
                "expected {expected_bytes} bytes for {width}x{height} bitmap, got {}",
                bytes.len()
            ));
        }
        Self::from_contiguous(width, height, |idx| contiguous_bit(bytes, idx))
    }

    pub fn try_from_bytes(width: usize, height: usize, bytes: &[u8]) -> Result<Self, String> {
        Self::from_bytes(width, height, bytes)
    }

    /// Creates a bitmap from a contiguous MSB-first bit buffer. Identical to
    /// [`Self::from_bytes`]; retained for callers that think in bits.
    pub fn from_bits(width: usize, height: usize, bits: &[u8]) -> Result<Self, String> {
        Self::from_bytes(width, height, bits)
    }

    pub fn try_from_bits(width: usize, height: usize, bits: &[u8]) -> Result<Self, String> {
        Self::from_bytes(width, height, bits)
    }

    /// Converts the bitmap to a byte vector in the contiguous (unpadded)
    /// packing — the inverse of [`Self::from_bytes`].
    ///
    /// For the row-padded form JBIG2 segments actually carry, use
    /// [`Self::to_jbig2_format`].
    pub fn to_bytes(&self) -> Vec<u8> {
        let total_bits = self.width * self.height;
        let mut out = vec![0u8; total_bits.div_ceil(8)];
        let mut idx = 0usize;
        for y in 0..self.height {
            for x in 0..self.width {
                if self.get_xy(x, y) {
                    out[idx >> 3] |= 0x80 >> (idx & 7);
                }
                idx += 1;
            }
        }
        out
    }

    /// Gets a single bit by contiguous (unpadded) index.
    #[inline]
    pub fn get_at(&self, idx: usize) -> bool {
        if self.width == 0 || idx >= self.width * self.height {
            return false;
        }
        self.get_xy(idx % self.width, idx / self.width)
    }

    /// Returns packed 32-bit words for efficient comparison and generic-region
    /// encoding.
    ///
    /// This is the storage itself — no repacking, and no cache to invalidate.
    #[inline]
    pub fn packed_words(&self) -> &[u32] {
        &self.words
    }

    /// Converts to packed 32-bit words for callers that need owned storage.
    pub fn to_packed_words(&self) -> Vec<u32> {
        self.words.clone()
    }

    /// Overwrite the storage from an identically-shaped packed-word buffer.
    ///
    /// Used by the decoder's `MonoBitmap` bridge, which shares this layout.
    /// Errors if the lengths disagree; `words` must already satisfy the
    /// zero-padding invariant, which every `MonoBitmap` does.
    pub(crate) fn copy_words_from(&mut self, words: &[u32]) -> Result<(), String> {
        if words.len() != self.words.len() {
            return Err(format!(
                "expected {} packed words for {}x{} bitmap, got {}",
                self.words.len(),
                self.width,
                self.height,
                words.len()
            ));
        }
        self.words.copy_from_slice(words);
        Ok(())
    }

    /// Gets a pixel value at (x, y).
    #[inline]
    pub fn get(&self, x: u32, y: u32) -> bool {
        if x >= usize_to_u32(self.width) || y >= usize_to_u32(self.height) {
            return false;
        }
        self.get_xy(u32_to_usize(x), u32_to_usize(y))
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
        self.get_xy(x, y)
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
                    result.set_xy(x, y, true);
                }
            }
        }
        Ok(result)
    }

    /// Sets a pixel value at (x, y).
    #[inline]
    pub fn set(&mut self, x: u32, y: u32, value: bool) {
        if x < usize_to_u32(self.width) && y < usize_to_u32(self.height) {
            self.set_xy(u32_to_usize(x), u32_to_usize(y), value);
        }
    }

    /// Sets a pixel value with usize coordinates (alias for CC analysis compatibility).
    #[inline]
    pub fn set_usize(&mut self, x: usize, y: usize, value: bool) {
        if x < self.width && y < self.height {
            self.set_xy(x, y, value);
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
        for dy in 0..u32_to_usize(rect.height) {
            for dx in 0..u32_to_usize(rect.width) {
                if self.get_xy(u32_to_usize(rect.x) + dx, u32_to_usize(rect.y) + dy) {
                    cropped.set_xy(dx, dy, true);
                }
            }
        }
        Ok(cropped)
    }

    pub fn try_crop(&self, rect: &Rect) -> Result<Self, String> {
        self.crop(rect)
    }

    /// Whether row `y` has any set pixel. Padding bits are zero, so a plain
    /// word scan is exact.
    #[inline]
    fn row_any(&self, y: usize) -> bool {
        self.row_words(y).iter().any(|word| *word != 0)
    }

    /// Trims whitespace from edges, returning the bounding rectangle and cropped image.
    pub fn trim(&self) -> (Rect, BitImage) {
        if self.words.iter().all(|word| *word == 0) {
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
            if self.row_any(y) {
                min_y = y;
                break;
            }
        }

        for y in (0..self.height).rev() {
            if self.row_any(y) {
                max_y = y;
                break;
            }
        }

        if min_y > max_y {
            // Should be unreachable if the all-zero check above did not fire
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
                if self.get_xy(x, y) {
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
        for word in &mut self.words {
            *word = !*word;
        }
        // Inversion is the one operation that dirties row padding.
        self.mask_row_padding();
    }

    /// Performs a logical AND with another bitmap.
    pub fn and(&self, other: &Self) -> Self {
        self.zip_with(other, |a, b| a & b)
    }

    /// Performs a logical OR with another bitmap.
    pub fn or(&self, other: &Self) -> Self {
        self.zip_with(other, |a, b| a | b)
    }

    /// Performs a logical XOR with another bitmap.
    pub fn xor(&self, other: &Self) -> Self {
        self.zip_with(other, |a, b| a ^ b)
    }

    /// Elementwise word combination. AND/OR/XOR of two zero-padded operands is
    /// itself zero-padded, so no re-masking is needed.
    fn zip_with(&self, other: &Self, op: impl Fn(u32, u32) -> u32) -> Self {
        assert_eq!(self.width, other.width, "Bitmaps must have the same width");
        assert_eq!(
            self.height, other.height,
            "Bitmaps must have the same height"
        );
        let mut result = self.clone();
        for (word, rhs) in result.words.iter_mut().zip(other.words.iter()) {
            *word = op(*word, *rhs);
        }
        result
    }

    /// Counts set bits (1s) in the bitmap.
    pub fn count_ones(&self) -> usize {
        crate::jbig2simd::count_packed_words_ones(self.packed_words(), self.width, self.height)
    }

    /// Counts unset bits (0s) in the bitmap.
    pub fn count_zeros(&self) -> usize {
        self.width * self.height - self.count_ones()
    }

    /// Gets a pixel value safely, returning 0 for out-of-bounds.
    ///
    /// Casting negative i32 to u32 yields a huge positive value that naturally
    /// fails the `< width/height` check, collapsing 4 branches into 2.
    #[inline]
    pub fn get_pixel_safely(&self, x: i32, y: i32) -> u8 {
        if (x as u32) < (self.width as u32) && (y as u32) < (self.height as u32) {
            self.get_xy(x as usize, y as usize) as u8
        } else {
            0
        }
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
    image.width.hash(&mut hasher);
    image.height.hash(&mut hasher);
    image.packed_words().hash(&mut hasher);
    hasher.finish()
}

/// Converts an `ndarray::Array2<u8>` to a `BitImage`.
pub fn array_to_bitimage(array: &Array2<u8>) -> Result<BitImage, String> {
    let (height, width) = array.dim();
    checked_dimensions(width, height)?;
    let mut image = BitImage::alloc(width, height)?;

    for (y, row) in array.rows().into_iter().enumerate() {
        for (x, &pixel) in row.iter().enumerate() {
            if pixel > 0 {
                image.set_xy(x, y, true);
            }
        }
    }

    Ok(image)
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

    let mut image = BitImage::alloc(width, height)?;
    let mut idx = 0usize;
    for y in 0..height {
        for x in 0..width {
            if pixels[idx] > 0 {
                image.set_xy(x, y, true);
            }
            idx += 1;
        }
    }

    Ok(image)
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
        assert!(BitImage::from_bits(2, 2, &[]).is_err());
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
