//! Decoder resource limits (jbig2decplan.md §12, §23).
//!
//! JBIG2 input arrives from untrusted PDF files, so limits are part of the
//! decoder architecture, not later hardening. Every allocation sized from
//! encoded values must be checked against a `DecodeLimits` first, so a
//! malformed stream returns an error instead of panicking or attempting a
//! multi-gigabyte allocation.

/// Upper bounds enforced while decoding untrusted input.
///
/// The [`Default`] is deliberately generous enough for real scanned documents
/// (well beyond A4 at 600 dpi) while still bounding worst-case memory. Callers
/// that know their corpus can tighten any field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Maximum width, in pixels, of any bitmap (page, region, or symbol).
    pub max_width: u32,
    /// Maximum height, in pixels, of any bitmap.
    pub max_height: u32,
    /// Maximum total pixels in a page bitmap.
    pub max_page_pixels: u64,
    /// Maximum total pixels in a single decoded region bitmap.
    pub max_region_pixels: u64,
    /// Maximum number of segments in one document.
    pub max_segments: usize,
    /// Maximum number of referred-to segments for a single segment.
    pub max_referred_segments: usize,
    /// Maximum number of symbols across all dictionaries reachable by a region.
    pub max_symbols: usize,
    /// Maximum pixels in a single symbol bitmap.
    pub max_symbol_pixels: u64,
    /// Maximum total pixels summed over every symbol in a dictionary set.
    pub max_total_dictionary_pixels: u64,
    /// Maximum depth of the segment dependency graph.
    pub max_dependency_depth: usize,
    /// Maximum bytes of retained (referenced-later) segment data.
    pub max_retained_bytes: usize,
    /// Maximum number of entries in a custom Huffman table.
    pub max_huffman_table_entries: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            // 1,048,576 px per side comfortably exceeds any real scan while
            // keeping a single row's word count small.
            max_width: 1 << 20,
            max_height: 1 << 20,
            // 268 Mpx page ≈ 33.5 MiB packed at 1 bpp.
            max_page_pixels: 256 * 1024 * 1024,
            max_region_pixels: 256 * 1024 * 1024,
            max_segments: 1 << 20,
            max_referred_segments: 1 << 16,
            max_symbols: 1 << 20,
            // A single symbol larger than a 16 Mpx tile is almost certainly
            // malformed.
            max_symbol_pixels: 16 * 1024 * 1024,
            max_total_dictionary_pixels: 256 * 1024 * 1024,
            max_dependency_depth: 1 << 12,
            // 256 MiB of retained segment payloads.
            max_retained_bytes: 256 * 1024 * 1024,
            max_huffman_table_entries: 1 << 16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_internally_consistent() {
        let l = DecodeLimits::default();
        // A max-area page must fit the per-side caps.
        assert!(l.max_page_pixels <= (l.max_width as u64) * (l.max_height as u64));
        assert!(l.max_symbol_pixels <= l.max_total_dictionary_pixels);
        assert!(l.max_width >= 1 && l.max_height >= 1);
    }
}
