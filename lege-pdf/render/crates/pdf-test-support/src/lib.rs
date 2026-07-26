//! Test infrastructure: image comparison metrics, determinism harnesses,
//! and corpus access (roadmap Phase 0, §14).
//!
//! Dev-dependency only — never a dependency of engine crates.

// Test-support code ships only into tests, where panics are the assertion
// mechanism; the never-panic invariant applies to engine crates only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use pdf_render_api::HostPage;

pub mod builder;
pub mod fonts;

/// Image comparison metrics (roadmap Phase 0 §3). SSIM and edge-weighted
/// difference join as rendering lands; these four already gate stub output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiffMetrics {
    pub max_channel_diff: u8,
    pub mean_abs_diff: f64,
    /// Fraction of pixels with any channel difference, 0.0–1.0.
    pub changed_fraction: f64,
    pub identical: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum DiffError {
    #[error("dimension mismatch: {0}x{1} vs {2}x{3}")]
    DimensionMismatch(u32, u32, u32, u32),
    #[error("format mismatch")]
    FormatMismatch,
}

/// Compare two host pages channel-wise.
pub fn diff_pages(a: &HostPage, b: &HostPage) -> Result<DiffMetrics, DiffError> {
    if a.width != b.width || a.height != b.height {
        return Err(DiffError::DimensionMismatch(
            a.width, a.height, b.width, b.height,
        ));
    }
    if a.format != b.format {
        return Err(DiffError::FormatMismatch);
    }
    let bpp = a.format.bytes_per_pixel();
    let row_bytes = a.width as usize * bpp;

    let mut max_diff = 0u8;
    let mut total_abs = 0u64;
    let mut changed_pixels = 0u64;

    for y in 0..a.height as usize {
        let ra = &a.pixels[y * a.stride..y * a.stride + row_bytes];
        let rb = &b.pixels[y * b.stride..y * b.stride + row_bytes];
        for (pa, pb) in ra.chunks_exact(bpp).zip(rb.chunks_exact(bpp)) {
            let mut pixel_changed = false;
            for (&ca, &cb) in pa.iter().zip(pb) {
                let d = ca.abs_diff(cb);
                if d > 0 {
                    pixel_changed = true;
                    max_diff = max_diff.max(d);
                    total_abs += d as u64;
                }
            }
            if pixel_changed {
                changed_pixels += 1;
            }
        }
    }

    let pixel_count = (a.width as u64) * (a.height as u64);
    let channel_count = pixel_count * bpp as u64;
    Ok(DiffMetrics {
        max_channel_diff: max_diff,
        mean_abs_diff: total_abs as f64 / channel_count.max(1) as f64,
        changed_fraction: changed_pixels as f64 / pixel_count.max(1) as f64,
        identical: max_diff == 0,
    })
}

/// FNV-1a over pixel content (stride-independent): the deterministic page
/// hash used by determinism and worker-count-equivalence tests (§14.5).
pub fn page_content_hash(page: &HostPage) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let row_bytes = page.width as usize * page.format.bytes_per_pixel();
    let mut h = OFFSET;
    for y in 0..page.height as usize {
        for &b in &page.pixels[y * page.stride..y * page.stride + row_bytes] {
            h ^= b as u64;
            h = h.wrapping_mul(PRIME);
        }
    }
    h
}

/// Determinism harness: run `render` under each worker count and assert all
/// outputs hash identically. The scheduler tests plug into this (§14.5
/// "output nondeterminism").
pub fn assert_deterministic_across_worker_counts<F>(worker_counts: &[usize], mut render: F)
where
    F: FnMut(usize) -> Vec<HostPage>,
{
    let mut baseline: Option<(usize, Vec<u64>)> = None;
    for &n in worker_counts {
        let hashes: Vec<u64> = render(n).iter().map(page_content_hash).collect();
        match &baseline {
            None => baseline = Some((n, hashes)),
            Some((n0, expected)) => {
                assert_eq!(
                    expected, &hashes,
                    "output differs between {n0} and {n} workers"
                );
            }
        }
    }
}

/// Locate the repository corpus directory.
pub fn corpus_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use pdf_render_api::OutputFormat;
    use std::sync::Arc;

    fn page(fill: u8) -> HostPage {
        HostPage {
            width: 4,
            height: 2,
            stride: 4,
            format: OutputFormat::Gray8,
            pixels: Arc::from(vec![fill; 8]),
        }
    }

    #[test]
    fn identical_pages_diff_zero() {
        let m = diff_pages(&page(7), &page(7)).unwrap();
        assert!(m.identical);
        assert_eq!(m.changed_fraction, 0.0);
    }

    #[test]
    fn differing_pages_measured() {
        let m = diff_pages(&page(10), &page(13)).unwrap();
        assert!(!m.identical);
        assert_eq!(m.max_channel_diff, 3);
        assert_eq!(m.changed_fraction, 1.0);
    }

    #[test]
    fn content_hash_ignores_stride_padding() {
        let a = page(9);
        let mut wide_pixels = vec![0xEEu8; 12]; // stride 6, 2 padding bytes/row
        for y in 0..2 {
            for x in 0..4 {
                wide_pixels[y * 6 + x] = 9;
            }
        }
        let b = HostPage {
            width: 4,
            height: 2,
            stride: 6,
            format: OutputFormat::Gray8,
            pixels: Arc::from(wide_pixels),
        };
        assert_eq!(page_content_hash(&a), page_content_hash(&b));
    }
}
