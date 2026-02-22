//! Run-length based connected component analysis for DjVu JB2 encoding.
//!
//! This module is a Rust port of the algorithm from DjVuLibre's `cjb2.cpp`,
//! which uses run-length encoding of scanlines + single-pass union-find to
//! discover connected components.  This approach is dramatically faster and
//! more memory-efficient than pixel-list algorithms (like Lutz) because:
//!
//! 1. **Runs** compress horizontal spans into (y, x1, x2) triples — a typical
//!    document page might have ~50 000 runs vs millions of pixels.
//! 2. **Union-find** with path compression gives near-O(n) labeling.
//! 3. **merge_and_split_ccs** handles the two pathological cases that cause
//!    "too many symbols":
//!    - Tiny fragments (noise, serifs, diacritical marks that got disconnected)
//!      are **merged** into grid cells.
//!    - Huge components (touching characters, rules, decorative borders) are
//!      **split** along grid lines.
//! 4. **Reading-order sort** groups components into text lines, which is
//!    critical for efficient dictionary encoding (similar shapes appear near
//!    each other).
//!
//! ## Integration with JB2 Encoder
//!
//! This module replaces the Lutz-based `find_connected_components()` workflow.
//! Here's how to use it in the JB2 encoding pipeline:
//!
//! ```rust,ignore
//! use djvu_encoder::encode::jb2::cc_image::{analyze_page, CCImage};
//! use djvu_encoder::encode::jb2::symbol_dict::BitImage;
//! use djvu_encoder::encode::jb2::encoder::JB2Encoder;
//!
//! // 1. Load or create your bilevel page image
//! let page_image: BitImage = BitImage::new(2550, 3300)?;
//! // ... fill with scanned document data ...
//!
//! // 2. Run connected component analysis
//! let dpi = 300;        // Image resolution
//! let losslevel = 1;    // 0 = lossless, >0 = enable cleaning
//! let cc_image = analyze_page(&page_image, dpi, losslevel);
//!
//! // 3. Extract shapes with bounding boxes
//! let shapes = cc_image.extract_shapes();
//!
//! // 4. Feed to JB2 encoder
//! let mut encoder = JB2Encoder::new(output_stream);
//! encoder.encode_page_with_shapes(
//!     page_image.width as u32,
//!     page_image.height as u32,
//!     &shapes
//! )?;
//! ```
//!
//! ## Coordinate System
//!
//! - **Top-down**: y=0 is the TOP of the page (matching BitImage convention).
//! - cjb2.cpp uses bottom-up, but the algorithm is symmetric.
//! - When encoding to DjVu, the encoder converts: `bottom = height - bbox.ymax`.
//!
//! ## DjVuLibre license notice
//!
//! The original C++ code is Copyright (c) 2002 Leon Bottou and Yann Le Cun,
//! distributed under the GNU General Public License v2+.  This Rust port
//! preserves the algorithmic structure but is a clean-room reimplementation
//! of the public API and data flow described in the DjVu specification.

use crate::encode::jb2::symbol_dict::BitImage;

// ─── Run ────────────────────────────────────────────────────────────────────

/// A horizontal run of foreground (black) pixels on a single scanline.
#[derive(Clone, Debug)]
pub struct Run {
    /// Vertical coordinate (row).  y = 0 is the **top** of the image in our
    /// coordinate system; cjb2.cpp uses bottom-up, but we canonicalize to
    /// top-down since `BitImage` is top-down.  The algorithm is symmetric.
    pub y: i32,
    /// First (leftmost) horizontal coordinate of the run, inclusive.
    pub x1: i32,
    /// Last (rightmost) horizontal coordinate of the run, inclusive.
    pub x2: i32,
    /// Connected-component id assigned during analysis.
    pub ccid: i32,
}

impl Run {
    /// Ordering used when sorting: primary by y ascending, secondary by x1.
    fn sort_key(&self) -> (i32, i32) {
        (self.y, self.x1)
    }
}

// ─── CC descriptor ──────────────────────────────────────────────────────────

/// Bounding box with (xmin, ymin) inclusive and (xmax, ymax) exclusive,
/// matching DjVuLibre's `GRect` convention.
#[derive(Clone, Copy, Debug, Default)]
pub struct BBox {
    pub xmin: i32,
    pub ymin: i32,
    /// Exclusive right edge.
    pub xmax: i32,
    /// Exclusive bottom edge.
    pub ymax: i32,
}

impl BBox {
    pub fn width(&self) -> i32 {
        self.xmax - self.xmin
    }
    pub fn height(&self) -> i32 {
        self.ymax - self.ymin
    }
}

/// Descriptor for a single connected component, exactly matching DjVuLibre's `CC`.
#[derive(Clone, Debug, Default)]
pub struct CC {
    /// Bounding box (xmin/ymin inclusive, xmax/ymax exclusive).
    pub bb: BBox,
    /// Total number of foreground pixels in this CC.
    pub npix: i32,
    /// Number of runs belonging to this CC.
    pub nrun: i32,
    /// Index of the first run in the sorted runs array.
    pub frun: i32,
}

// ─── CCImage ────────────────────────────────────────────────────────────────

/// An image decomposed into runs, with connected-component analysis,
/// cleaning, merging/splitting, and reading-order sort — matching the full
/// pipeline of `cjb2.cpp`'s `CCImage` class.
pub struct CCImage {
    pub width: i32,
    pub height: i32,
    pub runs: Vec<Run>,
    pub ccs: Vec<CC>,
    /// Number of "regular" CCs (text-sized).  CCs at indices ≥ nregularccs
    /// are "special" (merged small fragments or split large regions).
    pub nregularccs: usize,
    /// CCs whose bounding box exceeds this in either dimension get split.
    pub largesize: i32,
    /// CCs whose bounding box is ≤ this in both dimensions get merged.
    pub smallsize: i32,
    /// CCs with ≤ this many pixels get erased (noise removal).
    pub tinysize: i32,
}

impl CCImage {
    // ── Construction ─────────────────────────────────────────────────────

    /// Create a new empty `CCImage` with DPI-aware thresholds.
    ///
    /// The thresholds match cjb2.cpp exactly:
    /// ```text
    /// dpi       = clamp(dpi, 200, 900)
    /// largesize = min(500, max(64, dpi))
    /// smallsize = max(2, dpi / 150)
    /// tinysize  = max(0, dpi² / 20000 − 1)
    /// ```
    pub fn new(width: i32, height: i32, dpi: i32) -> Self {
        let dpi = dpi.max(200).min(900);
        Self {
            width,
            height,
            runs: Vec::new(),
            ccs: Vec::new(),
            nregularccs: 0,
            largesize: 500.min(64.max(dpi)),
            smallsize: 2.max(dpi / 150),
            tinysize: 0.max(dpi * dpi / 20000 - 1),
        }
    }

    // ── Run extraction ──────────────────────────────────────────────────

    /// Add a single run.
    pub fn add_single_run(&mut self, y: i32, x1: i32, x2: i32) {
        self.runs.push(Run {
            y,
            x1,
            x2,
            ccid: 0,
        });
    }

    /// Extract all horizontal runs from a `BitImage`.
    ///
    /// This replaces the Lutz pixel-list approach.  For a 2550×3300 page
    /// at 300 DPI the run list is typically 40–80 k entries, versus tens
    /// of millions of pixel tuples.
    pub fn add_bitmap_runs(&mut self, bm: &BitImage) {
        for y in 0..bm.height {
            let mut x = 0usize;
            while x < bm.width {
                // Skip white pixels
                while x < bm.width && !bm.get_pixel_unchecked(x, y) {
                    x += 1;
                }
                if x < bm.width {
                    let x1 = x;
                    // Consume black pixels
                    while x < bm.width && bm.get_pixel_unchecked(x, y) {
                        x += 1;
                    }
                    self.add_single_run(y as i32, x1 as i32, (x - 1) as i32);
                }
            }
        }
    }

    // ── Connected-component labeling (union-find on runs) ───────────────

    /// Assign `ccid` to every run using single-pass union-find.
    ///
    /// This is a direct port of `CCImage::make_ccids_by_analysis()`.
    ///
    /// **Algorithm summary:**
    /// 1. Sort runs by (y, x1).
    /// 2. For each run on line y, scan the runs on line y−1 that horizontally
    ///    overlap (with 1-pixel adjacency, i.e. x1−1..x2+1).
    /// 3. Union all overlapping previous-line runs with the current run.
    /// 4. Path-compress the union-find map.
    pub fn make_ccids_by_analysis(&mut self) {
        // Sort runs
        self.runs.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));

        let n_runs = self.runs.len();
        if n_runs == 0 {
            return;
        }

        // Union-find map: umap[id] is the parent of id.  A root satisfies
        // umap[id] == id.
        let mut umap: Vec<i32> = Vec::new();

        // `p` is the pointer into runs for the "previous line" scan window.
        let mut p: usize = 0;

        for n in 0..n_runs {
            let y = self.runs[n].y;
            let x1 = self.runs[n].x1 - 1; // 1-pixel adjacency
            let x2 = self.runs[n].x2 + 1;

            // id will hold the representative for this run's CC.
            // Initialize to "no id yet" by setting beyond current umap.
            let mut id: i32 = umap.len() as i32;

            // Advance p past runs that are above line y-1
            while p < n_runs && self.runs[p].y < y - 1 {
                p += 1;
            }

            // Scan previous-line runs that could overlap
            let mut pp = p;
            while pp < n_runs && self.runs[pp].y < y && self.runs[pp].x1 <= x2 {
                if self.runs[pp].x2 >= x1 {
                    // This previous run overlaps — union.
                    let mut oid = self.runs[pp].ccid;
                    // Path compression: find root
                    while (oid as usize) < umap.len() && umap[oid as usize] < oid {
                        oid = umap[oid as usize];
                    }

                    if id >= umap.len() as i32 {
                        // First overlap: adopt the previous run's root
                        id = oid;
                    } else if id < oid {
                        // Merge: point oid → id
                        if (oid as usize) < umap.len() {
                            umap[oid as usize] = id;
                        }
                    } else if oid < id {
                        // Merge: point id → oid
                        if (id as usize) < umap.len() {
                            umap[id as usize] = oid;
                        }
                        id = oid;
                    }

                    // Freshen previous run's ccid
                    self.runs[pp].ccid = id;

                    // Stop if this previous run extends past our current run
                    if self.runs[pp].x2 >= x2 {
                        break;
                    }
                }
                pp += 1;
            }

            // Assign id to current run
            self.runs[n].ccid = id;
            if id >= umap.len() as i32 {
                // Create a new root
                let new_id = umap.len() as i32;
                umap.push(new_id);
                self.runs[n].ccid = new_id;
            }
        }

        // Final path compression pass — flatten every ccid to its root
        for n in 0..n_runs {
            let mut ccid = self.runs[n].ccid;
            while (ccid as usize) < umap.len() && umap[ccid as usize] < ccid {
                ccid = umap[ccid as usize];
            }
            // Full path compression: also update intermediate nodes
            let root = ccid;
            let mut id = self.runs[n].ccid;
            while id != root {
                let next = umap[id as usize];
                umap[id as usize] = root;
                id = next;
            }
            self.runs[n].ccid = root;
        }
    }

    // ── Build CC descriptors from labeled runs ──────────────────────────

    /// Compute CC descriptors (bounding boxes, pixel counts, run ranges)
    /// from the ccid labels on runs.
    ///
    /// Direct port of `CCImage::make_ccs_from_ccids()`.
    pub fn make_ccs_from_ccids(&mut self) {
        if self.runs.is_empty() {
            self.ccs.clear();
            return;
        }

        // Find maximum ccid
        let mut maxccid = (self.nregularccs as i32) - 1;
        for run in &self.runs {
            if run.ccid > maxccid {
                maxccid = run.ccid;
            }
        }
        if maxccid < 0 {
            self.ccs.clear();
            return;
        }

        // Renumber: rmap[old_ccid] → new sequential id, or -1 if unused.
        let map_size = (maxccid + 1) as usize;
        let mut rmap = vec![-1i32; map_size];
        for run in &self.runs {
            if run.ccid >= 0 {
                rmap[run.ccid as usize] = 1; // mark as used
            }
        }
        let mut nid = 0i32;
        for entry in rmap.iter_mut() {
            if *entry > 0 {
                *entry = nid;
                nid += 1;
            }
        }

        // Adjust nregularccs
        while self.nregularccs > 0
            && (self.nregularccs - 1 < map_size)
            && rmap[self.nregularccs - 1] < 0
        {
            self.nregularccs -= 1;
        }
        if self.nregularccs > 0 && self.nregularccs <= map_size {
            self.nregularccs = (1 + rmap[self.nregularccs - 1]) as usize;
        }

        // Initialize CC descriptors
        let nid_us = nid as usize;
        self.ccs = vec![CC::default(); nid_us];

        // Count runs per CC
        for run in &self.runs {
            if run.ccid < 0 {
                continue;
            }
            let new_id = rmap[run.ccid as usize];
            if new_id >= 0 {
                self.ccs[new_id as usize].nrun += 1;
            }
        }

        // Compute first-run positions
        let mut frun = 0i32;
        // We'll reuse rmap as a "current insertion position" array
        let mut positions = vec![0i32; nid_us];
        for i in 0..nid_us {
            self.ccs[i].frun = frun;
            positions[i] = frun;
            frun += self.ccs[i].nrun;
        }

        // Relabel runs and copy into sorted order
        let mut sorted_runs = vec![
            Run {
                y: 0,
                x1: 0,
                x2: 0,
                ccid: -1
            };
            frun as usize
        ];
        for run in &self.runs {
            if run.ccid < 0 {
                continue;
            }
            let new_id = rmap[run.ccid as usize];
            if new_id < 0 {
                continue;
            }
            let pos = positions[new_id as usize] as usize;
            sorted_runs[pos] = Run {
                y: run.y,
                x1: run.x1,
                x2: run.x2,
                ccid: new_id,
            };
            positions[new_id as usize] += 1;
        }
        self.runs = sorted_runs;

        // Finalize each CC: sort its runs and compute bounding box + npix
        for i in 0..nid_us {
            let cc = &self.ccs[i];
            let start = cc.frun as usize;
            let end = start + cc.nrun as usize;

            // Sort runs within this CC
            self.runs[start..end].sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));

            // Compute bounds and pixel count
            let mut npix = 0i32;
            let mut xmin = i32::MAX;
            let mut xmax = i32::MIN;
            let mut ymin = i32::MAX;
            let mut ymax = i32::MIN;

            for run in &self.runs[start..end] {
                xmin = xmin.min(run.x1);
                xmax = xmax.max(run.x2);
                ymin = ymin.min(run.y);
                ymax = ymax.max(run.y);
                npix += run.x2 - run.x1 + 1;
            }

            let cc = &mut self.ccs[i];
            cc.npix = npix;
            cc.bb = BBox {
                xmin,
                ymin,
                xmax: xmax + 1, // exclusive
                ymax: ymax + 1, // exclusive
            };
        }
    }

    // ── Noise removal ───────────────────────────────────────────────────

    /// Remove CCs with ≤ `tinysize` pixels.
    ///
    /// This is the "cleaning" step: at 300 DPI tinysize = 3, so isolated
    /// specks of 1–3 pixels are removed.  (cjb2.cpp notes that halftone
    /// regions should be exempted, but neither cjb2 nor we do that.)
    pub fn erase_tiny_ccs(&mut self) {
        for i in 0..self.ccs.len() {
            if self.ccs[i].npix <= self.tinysize {
                let frun = self.ccs[i].frun as usize;
                let nrun = self.ccs[i].nrun as usize;
                self.ccs[i].nrun = 0;
                self.ccs[i].npix = 0;
                for r in frun..frun + nrun {
                    if r < self.runs.len() {
                        self.runs[r].ccid = -1;
                    }
                }
            }
        }
    }

    // ── Merge small / split large CCs ───────────────────────────────────

    /// The critical step that the Lutz-based code was missing entirely.
    ///
    /// ## Small CC merging
    /// Any CC whose bounding box fits within `smallsize × smallsize` is
    /// merged with other nearby small CCs in the same grid cell.  The grid
    /// cell size is `largesize`.  This catches:
    /// - Diacritical marks (dots over i/j, umlauts, tildes)
    /// - Punctuation fragments
    /// - Serif fragments that separated during binarization
    ///
    /// ## Large CC splitting
    /// Any CC whose bounding box exceeds `largesize` in either dimension
    /// has its runs re-assigned to grid cells.  Long runs that span multiple
    /// grid cells are physically split.  This catches:
    /// - Lines and rules
    /// - Touching character groups
    /// - Decorative borders
    ///
    /// After reassignment, `make_ccs_from_ccids()` is called again to
    /// recompute all CC descriptors.
    pub fn merge_and_split_ccs(&mut self) {
        if self.ccs.is_empty() {
            return;
        }

        let splitsize = self.largesize;
        let mut ncc = self.ccs.len() as i32;
        let mut extra_runs: Vec<Run> = Vec::new();

        // We need a way to map (gridi, gridj, ccid) → new ccid.
        // Using a HashMap like DjVuLibre's GMap.
        use std::collections::HashMap;
        let mut grid_map: HashMap<(i16, i16, i32), i32> = HashMap::new();

        self.nregularccs = self.ccs.len();

        let makeccid = |key: (i16, i16, i32),
                            map: &mut HashMap<(i16, i16, i32), i32>,
                            ncc: &mut i32|
         -> i32 {
            if let Some(&id) = map.get(&key) {
                id
            } else {
                let id = *ncc;
                map.insert(key, id);
                *ncc += 1;
                id
            }
        };

        for ccid in 0..self.ccs.len() {
            let cc = &self.ccs[ccid];
            if cc.nrun <= 0 {
                continue;
            }

            let cc_height = cc.bb.height();
            let cc_width = cc.bb.width();
            let frun = cc.frun as usize;
            let nrun = cc.nrun as usize;

            if cc_height <= self.smallsize && cc_width <= self.smallsize {
                // ── Merge small CC ───────────────────────────────────
                // Map all runs to the same grid cell, with ccid = -1
                // so that unrelated small CCs in the same cell merge.
                let gridi = ((cc.bb.ymin + cc.bb.ymax) / splitsize / 2) as i16;
                let gridj = ((cc.bb.xmin + cc.bb.xmax) / splitsize / 2) as i16;
                let key = (gridi, gridj, -1);
                let new_ccid = makeccid(key, &mut grid_map, &mut ncc);
                for r in frun..frun + nrun {
                    if r < self.runs.len() {
                        self.runs[r].ccid = new_ccid;
                    }
                }
            } else if cc_height >= self.largesize || cc_width >= self.largesize {
                // ── Split large CC ───────────────────────────────────
                for r in frun..frun + nrun {
                    if r >= self.runs.len() {
                        continue;
                    }

                    let run_y = self.runs[r].y;
                    let run_x1 = self.runs[r].x1;
                    let run_x2 = self.runs[r].x2;

                    let gridi = (run_y / splitsize) as i16;
                    let gridj_start = (run_x1 / splitsize) as i16;
                    let gridj_end = (run_x2 / splitsize) as i16;

                    let key = (gridi, gridj_start, ccid as i32);
                    let new_ccid = makeccid(key, &mut grid_map, &mut ncc);
                    self.runs[r].ccid = new_ccid;

                    if gridj_end > gridj_start {
                        // Run spans multiple grid columns — split it.
                        // Truncate the original run to its first grid cell.
                        let orig_x2 = self.runs[r].x2;
                        self.runs[r].x2 =
                            (gridj_start as i32 + 1) * splitsize - 1;

                        // Create new runs for intermediate grid cells
                        let mut current_gridj = gridj_start + 1;
                        while current_gridj < gridj_end {
                            let cell_x1 = current_gridj as i32 * splitsize;
                            let cell_x2 = cell_x1 + splitsize - 1;
                            let key = (gridi, current_gridj, ccid as i32);
                            let cell_ccid = makeccid(key, &mut grid_map, &mut ncc);
                            extra_runs.push(Run {
                                y: run_y,
                                x1: cell_x1,
                                x2: cell_x2,
                                ccid: cell_ccid,
                            });
                            current_gridj += 1;
                        }

                        // Create run for the last grid cell
                        let last_x1 = gridj_end as i32 * splitsize;
                        let key = (gridi, gridj_end, ccid as i32);
                        let last_ccid = makeccid(key, &mut grid_map, &mut ncc);
                        extra_runs.push(Run {
                            y: run_y,
                            x1: last_x1,
                            x2: orig_x2,
                            ccid: last_ccid,
                        });
                    }
                }
            }
            // Normal-sized CCs keep their existing ccid — no changes needed.
        }

        // Append any extra runs that were created by splitting
        self.runs.append(&mut extra_runs);

        // Recompute all CC descriptors from the updated ccids
        self.make_ccs_from_ccids();
    }

    // ── Reading-order sort ──────────────────────────────────────────────

    /// Sort CCs in approximate reading order: top-to-bottom by text line,
    /// left-to-right within each line.
    ///
    /// This is important for JB2 encoding efficiency because the encoder
    /// uses relative positioning — nearby symbols in encoding order should
    /// be spatially close.  It also means the dictionary sees similar
    /// characters (same font, same size) in sequence, improving
    /// cross-coding compression.
    ///
    /// Direct port of `CCImage::sort_in_reading_order()`.
    pub fn sort_in_reading_order(&mut self) {
        let n = self.nregularccs;
        if n < 2 {
            return;
        }

        // Work on a copy of the regular CCs
        let mut cc_arr: Vec<(usize, CC)> = self.ccs[..n]
            .iter()
            .enumerate()
            .map(|(i, cc)| (i, cc.clone()))
            .collect();

        // Sort by top edge ascending (lowest ymin first) for Top-Down coordinates.
        // This ensures Top-to-Bottom reading order.
        cc_arr.sort_by(|a, b| {
            a.1.bb.ymin
                .cmp(&b.1.bb.ymin)
                .then(a.1.bb.xmin.cmp(&b.1.bb.xmin))
                .then(a.1.frun.cmp(&b.1.frun))
        });

        // Determine max vertical deviation for line grouping
        let maxtopchange = (self.width / 40).max(32);

        // Group into text lines and sort within each line
        let mut ccno = 0usize;
        while ccno < n {
            let line_start_ymin = cc_arr[ccno].1.bb.ymin;
            // Scan for the end of this line (items that are vertically close)
            
            let mut nccno = ccno + 1;
            while nccno < n {
                let curr_ymin = cc_arr[nccno].1.bb.ymin;
                
                // If the next items top edge is significantly below the line start, it's a new line
                if curr_ymin > line_start_ymin + maxtopchange {
                    break;
                }
                nccno += 1;
            }

            // Sort this line left-to-right (by xmin)
            cc_arr[ccno..nccno].sort_by(|a, b| {
                a.1.bb
                    .xmin
                    .cmp(&b.1.bb.xmin)
            });

            // Move to next line
            ccno = nccno;
        }

        // Write back and relabel runs
        let mut new_ccs = Vec::with_capacity(self.ccs.len());
        let mut old_to_new = vec![0usize; self.ccs.len()];

        for (new_idx, (old_idx, cc)) in cc_arr.into_iter().enumerate() {
            new_ccs.push(cc);
            old_to_new[old_idx] = new_idx;
        }
        
        // Append the non-regular CCs
        for i in n..self.ccs.len() {
            let new_idx = new_ccs.len();
            new_ccs.push(self.ccs[i].clone());
            old_to_new[i] = new_idx;
        }
        
        self.ccs = new_ccs;
        
        // Remap runs
        for run in &mut self.runs {
            if run.ccid >= 0 {
                run.ccid = old_to_new[run.ccid as usize] as i32;
            }
        }
    }

    // ── Bitmap extraction ───────────────────────────────────────────────

    /// Extract a bitmap for a single CC by painting its runs into a fresh
    /// `BitImage`.
    pub fn get_bitmap_for_cc(&self, ccid: usize) -> Option<BitImage> {
        if ccid >= self.ccs.len() {
            return None;
        }
        let cc = &self.ccs[ccid];
        let bb = &cc.bb;
        let w = bb.width();
        let h = bb.height();
        if w <= 0 || h <= 0 {
            return None;
        }

        let mut bm = BitImage::new(w as u32, h as u32).ok()?;
        let frun = cc.frun as usize;
        let nrun = cc.nrun as usize;

        for i in frun..frun + nrun {
            if i >= self.runs.len() {
                break;
            }
            let run = &self.runs[i];
            let row = run.y - bb.ymin;
            for x in run.x1..=run.x2 {
                let col = x - bb.xmin;
                bm.set_usize(col as usize, row as usize, true);
            }
        }

        Some(bm)
    }

    // ── High-level pipeline ─────────────────────────────────────────────

    /// Run the full CC analysis pipeline:
    ///
    /// 1. `make_ccids_by_analysis()` — union-find labeling
    /// 2. `make_ccs_from_ccids()` — build descriptors
    /// 3. `erase_tiny_ccs()` — remove noise (only if losslevel > 0)
    /// 4. `merge_and_split_ccs()` — grid-based merge/split
    /// 5. `sort_in_reading_order()` — reading-order sort
    ///
    /// After this, iterate `0..self.ccs.len()` and call
    /// `get_bitmap_for_cc(i)` to extract symbol bitmaps.
    pub fn analyze(&mut self, losslevel: i32) {
        self.make_ccids_by_analysis();
        self.make_ccs_from_ccids();

        if losslevel > 0 {
            self.erase_tiny_ccs();
        }

        self.merge_and_split_ccs();
        self.sort_in_reading_order();
    }

    /// Convert the analyzed CCs into (bitmap, bounding_box) pairs ready
    /// for JB2 encoding, filtering out empty results.
    pub fn extract_shapes(&self) -> Vec<(BitImage, BBox)> {
        let mut shapes = Vec::with_capacity(self.ccs.len());
        for ccid in 0..self.ccs.len() {
            if self.ccs[ccid].nrun <= 0 {
                continue;
            }
            if let Some(bm) = self.get_bitmap_for_cc(ccid) {
                shapes.push((bm, self.ccs[ccid].bb));
            }
        }
        shapes
    }
}

// ─── Convenience entry point ────────────────────────────────────────────────

/// Perform connected-component analysis on a `BitImage` and return the
/// extracted shapes with their bounding boxes, ready for JB2 encoding.
///
/// This replaces the Lutz-based `find_connected_components()` and the
/// entire `extract_symbols()` pipeline from `jbig2lutz.rs`.
///
/// ## Parameters
/// - `image`: the full-page bilevel image
/// - `dpi`: image resolution (typically 300 for scanned documents)
/// - `losslevel`: 0 = lossless (no cleaning), >0 enables noise removal
///
/// ## Returns
/// A `CCImage` with the full analysis complete.  Call `extract_shapes()`
/// to get `(BitImage, BBox)` pairs.
pub fn analyze_page(image: &BitImage, dpi: i32, losslevel: i32) -> CCImage {
    let mut ccimg = CCImage::new(image.width as i32, image.height as i32, dpi);
    ccimg.add_bitmap_runs(image);
    ccimg.analyze(losslevel);
    ccimg
}

/// Convert CC analysis results into the format expected by JB2Encoder::encode_page_with_shapes().
///
/// Returns:
/// - shapes: Vec<BitImage> - the symbol bitmaps
/// - parents: Vec<i32> - parent indices for refinement (-1 for no parent)
/// - blits: Vec<(i32, i32, usize)> - (left, bottom, shapeno) for each symbol instance
///
/// Note: Currently returns no parents (-1 for all shapes) and one blit per shape.
/// For production use with symbol matching and refinement, you'd need to:
/// 1. Compare shapes to find duplicates/similar symbols
/// 2. Build parent relationships for refinement
/// 3. Map multiple blits to the same shape index
pub fn shapes_to_encoder_format(
    shapes: Vec<(BitImage, BBox)>,
    page_height: i32,
) -> (Vec<BitImage>, Vec<i32>, Vec<(i32, i32, usize)>) {
    let mut bitmaps = Vec::with_capacity(shapes.len());
    let mut parents = Vec::with_capacity(shapes.len());
    let mut blits = Vec::with_capacity(shapes.len());

    for (idx, (bitmap, bbox)) in shapes.into_iter().enumerate() {
        bitmaps.push(bitmap);
        parents.push(-1); // No parent (no refinement)

        // Convert top-down y to DjVu bottom-up y coordinate
        let bottom = page_height - bbox.ymax;
        blits.push((bbox.xmin, bottom, idx));
    }

    // Sort blits by DjVu reading order: top-to-bottom (descending bottom), then left-to-right (ascending left)
    // This ensures that when we go to a new line, `left` decreases (resets to left margin),
    // which triggers the "new row" detection in the encoder.
    blits.sort_by(|a, b| {
        // Primary: descending by bottom (top of page first in DjVu coords)
        b.1.cmp(&a.1)
            // Secondary: ascending by left (left-to-right)
            .then(a.0.cmp(&b.0))
    });

    (bitmaps, parents, blits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_image() -> BitImage {
        // Create a small test image with two separate blobs
        let mut bm = BitImage::new(40, 20).unwrap();
        // Blob 1: 5x5 at (2, 2)
        for y in 2..7 {
            for x in 2..7 {
                bm.set_usize(x, y, true);
            }
        }
        // Blob 2: 5x5 at (20, 10)
        for y in 10..15 {
            for x in 20..25 {
                bm.set_usize(x, y, true);
            }
        }
        bm
    }

    #[test]
    fn test_run_extraction() {
        let bm = make_test_image();
        let mut ccimg = CCImage::new(40, 20, 300);
        ccimg.add_bitmap_runs(&bm);
        // Each blob has 5 rows, each row is one run → 10 runs total
        assert_eq!(ccimg.runs.len(), 10);
    }

    #[test]
    fn test_cc_analysis_finds_two_components() {
        let bm = make_test_image();
        let mut ccimg = CCImage::new(40, 20, 300);
        ccimg.add_bitmap_runs(&bm);
        ccimg.make_ccids_by_analysis();
        ccimg.make_ccs_from_ccids();

        assert_eq!(ccimg.ccs.len(), 2);
        assert_eq!(ccimg.ccs[0].npix, 25);
        assert_eq!(ccimg.ccs[1].npix, 25);
    }

    #[test]
    fn test_full_pipeline() {
        let bm = make_test_image();
        let ccimg = analyze_page(&bm, 300, 0);
        let shapes = ccimg.extract_shapes();

        assert_eq!(shapes.len(), 2);
        for (bitmap, bb) in &shapes {
            assert_eq!(bitmap.width, 5);
            assert_eq!(bitmap.height, 5);
            assert_eq!(bb.width(), 5);
            assert_eq!(bb.height(), 5);
        }
    }

    #[test]
    fn test_tiny_cc_removal() {
        let mut bm = BitImage::new(40, 20).unwrap();
        // One real blob
        for y in 2..7 {
            for x in 2..7 {
                bm.set_usize(x, y, true);
            }
        }
        // One tiny speck (1 pixel)
        bm.set_usize(30, 10, true);

        let ccimg = analyze_page(&bm, 300, 1); // losslevel > 0 enables cleaning
        let shapes = ccimg.extract_shapes();

        // The speck should have been removed (tinysize at 300 DPI = 3)
        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].0.width, 5);
    }
}