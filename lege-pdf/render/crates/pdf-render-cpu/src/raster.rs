//! Analytic scanline coverage — exact-area anti-aliasing (performance advice
//! §3, §4). This is the AGG/font-rs family of algorithm: device-space edges
//! accumulate signed area, and a prefix-sum sweep turns that into per-pixel
//! coverage (exact in both x and y, no supersampling). The sweep hands the
//! executor a `u8` coverage row, from which it classifies contiguous spans and
//! dispatches a kernel once per span.
//!
//! Fast path (the realized font-rs kernel): each edge is walked **once**,
//! depositing signed area only into the pixels it actually crosses, into a
//! path-bbox-local 2D accumulation buffer; a per-row prefix-sum then converts
//! to coverage. This replaces the historical O(edges × rows) rescan (every edge
//! tested against every scanline of the band). The two structures produce
//! **byte-identical** coverage — the reorganization preserves, for every
//! accumulation cell, both the operands and their order (edge-index order), so
//! the float sums are bit-for-bit the same. `fill_scanline_ref` is the original
//! per-scanline implementation, retained (test-only) purely as the byte-identity
//! oracle; the production oversized/sparse-window fallback is
//! `fill_scanline_aet`.
//!
//! Vertical edges (A2) take a dedicated deposit that skips the x-walk. AVX2
//! (A3) accelerates only the per-element coverage *conversion* (abs/clamp/scale/
//! truncate/pack); the running prefix-sum stays scalar, so the AVX2 path is
//! bit-for-bit identical to the scalar path (verified by
//! `tests::avx2_convert_matches_scalar_bit_exact`). All scratch lives on the
//! kernel and is reused across fills; only touched cells are reset (advice §13).

/// Fill rule for coverage accumulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

/// A monotonic-in-y edge in device space.
#[derive(Debug, Clone, Copy)]
struct Edge {
    y_top: f32,
    y_bot: f32,
    x_at_top: f32,
    dxdy: f32,
    dir: f32,
}

/// Largest bbox-local accumulation window (in f32 cells) the fast path will
/// allocate. Beyond this, a fill defers to `fill_scanline_aet`, which needs only
/// an O(device-width) scanline buffer. 1 Mi cells = 4 MiB; glyphs and ordinary
/// vector fills sit far below it, so this only guards a pathologically large
/// single path against a device-area allocation.
const MAX_WINDOW_CELLS: usize = 1 << 20;

/// A large-but-sparse local window (`> CACHE_WINDOW_CELLS` cells with fewer than
/// `cells / 8` edges) is routed to the scanline path: there the fast path's
/// 2D accumulation buffer loses cache locality to the reference's single reused
/// O(device-width) row, and few edges mean little rescan waste to eliminate.
/// Both paths are byte-identical, so this only trades speed, never correctness,
/// and can never regress below the pre-rewrite baseline (measured: a spread-out
/// multi-glyph CJK run, few edges over a wide bbox, is faster on the reference).
/// Compact fills (glyph runs, ordinary vector paths) stay on the fast path.
const CACHE_WINDOW_CELLS: usize = 1 << 13; // 8 Ki cells (32 KiB f32) ≈ L1

/// Minimum covered-span width (pixels) before the AVX2 coverage conversion is
/// engaged. It is bit-for-bit identical to the scalar path (A3, verified by
/// `tests::avx2_convert_matches_scalar_bit_exact`) but **disabled by default**
/// (`usize::MAX`): measurement — both the page A/B and the kernel micro-bench —
/// showed it net-negative for glyph/vector fills. The kernel is deposit- and
/// (sequential) prefix-sum-bound, not convert-bound; SIMD-converting requires a
/// separate prefix-sum write-back pass whose cost exceeds the convert saving.
/// Retained, dispatched, and tested so a future convert-bound workload can lower
/// this constant; the sequential prefix-sum precludes a bit-exact SIMD sum.
const AVX2_MIN_SPAN: usize = usize::MAX;

#[derive(Debug, Default)]
pub struct RasterKernel {
    edges: Vec<Edge>,
    /// Bbox-local 2D signed-area accumulation, row-major with per-fill `stride`.
    /// Kept zeroed between fills by resetting only touched cells.
    acc: Vec<f32>,
    /// Per-local-row touched column range (local column indices). `col_min > col_max`
    /// marks an untouched row.
    row_min: Vec<u32>,
    row_max: Vec<u32>,
    /// Per-scanline coverage bytes, indexed by absolute device column, length
    /// `width`. Only the emitted span is written and then re-zeroed.
    cov: Vec<u8>,
    /// Scanline accumulation buffer for the reference/fallback path, length
    /// `width + 1`.
    acc_row: Vec<f32>,
    // --- Active-edge-table (AET) scratch for `fill_scanline_aet`. All sized by
    // `edges.len()` / band height and reused across fills (advice §13). ---
    /// Contributing edge indices grouped by activation local-row via a CSR
    /// counting sort; scattered in edge-index order so each bucket is
    /// index-sorted.
    aet_order: Vec<u32>,
    /// CSR bucket offsets, length `bh + 1`: local row `lr`'s activating edges
    /// are `aet_order[aet_bucket_start[lr]..aet_bucket_start[lr + 1]]`.
    aet_bucket_start: Vec<u32>,
    /// Per-edge retirement row `er_end` (the `ceil(y_bot)`-derived exclusive
    /// end), only meaningful for edges that reach the active list.
    aet_erend: Vec<u32>,
    /// The active edge indices at the current row, kept index-sorted.
    aet_active: Vec<u32>,
    /// Double-buffer for the per-row merge of `aet_active` with a new bucket.
    aet_active_next: Vec<u32>,
    pub last_edges: u64,
    pub last_rows: u64,
    pub last_covered: u64,
}

impl RasterKernel {
    /// Rasterize `subpaths` (device-space polylines as index ranges into
    /// `points`) and invoke `row(y, x0, x1, cov)` once per covered row, where
    /// `cov[x0..=x1]` is `u8` coverage (255 = full) and all else is zero.
    pub fn fill(
        &mut self,
        points: &[[f32; 2]],
        subpaths: &[(usize, usize)],
        width: usize,
        height: usize,
        rule: FillRule,
        row: impl FnMut(usize, usize, usize, &mut [u8]),
    ) {
        let _ = self.fill_cancellable(points, subpaths, width, height, rule, None, row);
    }

    /// [`Self::fill`] with a cooperative cancellation probe. Returns `false`
    /// when cancellation interrupted the fill; scratch buffers are reset so
    /// the worker can be reused safely for a later request.
    #[allow(clippy::too_many_arguments)]
    pub fn fill_cancellable(
        &mut self,
        points: &[[f32; 2]],
        subpaths: &[(usize, usize)],
        width: usize,
        height: usize,
        rule: FillRule,
        should_cancel: Option<&(dyn Fn() -> bool + Send + Sync)>,
        row: impl FnMut(usize, usize, usize, &mut [u8]),
    ) -> bool {
        if width == 0 || height == 0 {
            return true;
        }
        if should_cancel.is_some_and(|cancel| cancel()) {
            return false;
        }
        if self.cov.len() < width {
            self.cov.resize(width, 0);
        }

        // Build monotonic-in-y edges and the path's device-space bounds.
        self.edges.clear();
        let mut y_min = f32::INFINITY;
        let mut y_max = f32::NEG_INFINITY;
        let mut x_min = f32::INFINITY;
        let mut x_max = f32::NEG_INFINITY;
        for (subpath_index, &(start, end)) in subpaths.iter().enumerate() {
            if subpath_index & 0x3f == 0 && should_cancel.is_some_and(|cancel| cancel()) {
                self.reset_cancelled_fill();
                return false;
            }
            let n = end - start;
            if n < 2 {
                continue;
            }
            for i in 0..n {
                let a = points[start + i];
                let b = points[start + (i + 1) % n];
                if a[1] == b[1] {
                    continue;
                }
                let (y_top, y_bot, x_at_top, dir) = if a[1] < b[1] {
                    (a[1], b[1], a[0], 1.0)
                } else {
                    (b[1], a[1], b[0], -1.0)
                };
                let dxdy = (b[0] - a[0]) / (b[1] - a[1]);
                self.edges.push(Edge {
                    y_top,
                    y_bot,
                    x_at_top,
                    dxdy,
                    dir,
                });
                y_min = y_min.min(y_top);
                y_max = y_max.max(y_bot);
                // x-extent of this edge's endpoints (device space).
                let x_at_bot = x_at_top + dxdy * (y_bot - y_top);
                x_min = x_min.min(x_at_top.min(x_at_bot));
                x_max = x_max.max(x_at_top.max(x_at_bot));
            }
        }
        if self.edges.is_empty() {
            return true;
        }
        self.last_edges = self.edges.len() as u64;

        let row_start = y_min.floor().max(0.0) as usize;
        let row_end = (y_max.ceil().min(height as f32)) as usize;
        self.last_rows = row_end.saturating_sub(row_start) as u64;
        self.last_covered = 0;
        if row_end <= row_start {
            return true;
        }

        // Device-clamped x-window for the local accumulation buffer.
        let fwidth = width as f32;
        let bx = (x_min.floor().clamp(0.0, (width - 1) as f32)) as usize;
        let right = (x_max.ceil().clamp(0.0, fwidth)) as usize;
        let bw = right.saturating_sub(bx).max(1);
        let bh = row_end - row_start;
        let stride = bw + 2; // + carry cell (+1) + rounding slack (+1)
        let cells = stride.saturating_mul(bh);

        // Route to the scanline reference for a pathologically large single-path
        // window (device-area allocation guard) or a large-but-sparse window
        // (cache locality — see `CACHE_WINDOW_CELLS`). Both are byte-identical to
        // the fast path, so this only trades speed and never regresses output.
        let sparse = cells > CACHE_WINDOW_CELLS && self.edges.len().saturating_mul(8) < cells;
        if cells > MAX_WINDOW_CELLS || sparse {
            // The active-edge-table fallback: byte-identical to the naive
            // `fill_scanline_ref` (asserted by `aet_matches_reference_bit_exact`)
            // but O(edges + Σ active) instead of O(edges × rows), which is the
            // whole win on edge-dense sparse-window pages (map contours, etc.).
            let completed =
                self.fill_scanline_aet(width, height, rule, row_start, row_end, should_cancel, row);
            if !completed {
                self.reset_cancelled_fill();
            }
            return completed;
        }

        if self.acc.len() < cells {
            self.acc.resize(cells, 0.0);
        }
        if self.row_min.len() < bh {
            self.row_min.resize(bh, 0);
            self.row_max.resize(bh, 0);
        }
        for lr in 0..bh {
            self.row_min[lr] = u32::MAX;
            self.row_max[lr] = 0;
        }

        // --- Pass 1: deposit each edge once, into only the rows it crosses. ---
        for edge_index in 0..self.edges.len() {
            if edge_index & 0x3f == 0 && should_cancel.is_some_and(|cancel| cancel()) {
                self.reset_cancelled_fill();
                return false;
            }
            let e = self.edges[edge_index];
            let er_start = (e.y_top.floor() as isize).max(row_start as isize) as usize;
            let er_end = (e.y_bot.ceil() as isize).min(row_end as isize).max(0) as usize;
            if e.dxdy == 0.0 {
                // A2: vertical fast path. x is constant; precompute column split.
                let xc = e.x_at_top.clamp(0.0, fwidth);
                let c = (xc.floor().clamp(0.0, fwidth - 1.0)) as usize;
                let lc = c.saturating_sub(bx).min(stride - 2);
                let frac = (c as f32 + 1.0) - xc; // == (c+1) - xmid, with xmid == xc
                for y in er_start..er_end {
                    if y & 0xff == 0 && should_cancel.is_some_and(|cancel| cancel()) {
                        self.reset_cancelled_fill();
                        return false;
                    }
                    let yf = y as f32;
                    let ya = e.y_top.max(yf);
                    let yb = e.y_bot.min(yf + 1.0);
                    if yb <= ya {
                        continue;
                    }
                    let cover = (yb - ya) * e.dir;
                    let this_px = cover * frac;
                    let lr = y - row_start;
                    let base = lr * stride;
                    self.acc[base + lc] += this_px;
                    self.acc[base + lc + 1] += cover - this_px;
                    if (lc as u32) < self.row_min[lr] {
                        self.row_min[lr] = lc as u32;
                    }
                    if (lc as u32) > self.row_max[lr] {
                        self.row_max[lr] = lc as u32;
                    }
                }
            } else {
                for y in er_start..er_end {
                    if y & 0xff == 0 && should_cancel.is_some_and(|cancel| cancel()) {
                        self.reset_cancelled_fill();
                        return false;
                    }
                    let yf = y as f32;
                    let ya = e.y_top.max(yf);
                    let yb = e.y_bot.min(yf + 1.0);
                    if yb <= ya {
                        continue;
                    }
                    let xa = e.x_at_top + (ya - e.y_top) * e.dxdy;
                    let xb = e.x_at_top + (yb - e.y_top) * e.dxdy;
                    let lr = y - row_start;
                    let base = lr * stride;
                    deposit_edge_row(
                        &mut self.acc,
                        base,
                        xa,
                        ya,
                        xb,
                        yb,
                        e.dir,
                        fwidth,
                        bx,
                        stride,
                        &mut self.row_min[lr],
                        &mut self.row_max[lr],
                    );
                }
            }
        }

        // --- Pass 2: per-row prefix-sum → coverage, emit spans, reset. ---
        // The scalar branch is a single fused pass (prefix-sum, convert, emit,
        // reset) — byte-identical to the reference inner loop and with no
        // per-row overhead. Wide rows take the AVX2 conversion, which requires a
        // separate prefix-sum write-back (the SIMD convert cannot fuse with the
        // sequential sum); it is bit-for-bit identical to the scalar branch.
        let use_avx2 = coverage_use_avx2();
        let mut row = row;
        for lr in 0..bh {
            if lr & 0x1f == 0 && should_cancel.is_some_and(|cancel| cancel()) {
                self.reset_cancelled_fill();
                return false;
            }
            let cmin = self.row_min[lr];
            let cmax = self.row_max[lr];
            if cmin > cmax {
                continue;
            }
            let (cmin, cmax) = (cmin as usize, cmax as usize);
            let base = lr * stride;
            let mut first = width;
            let mut last = 0usize;

            // `AVX2_MIN_SPAN == usize::MAX` is the documented "disabled"
            // sentinel (see its doc comment), making this deliberately
            // always-false until a workload lowers the constant.
            #[allow(clippy::absurd_extreme_comparisons)]
            if use_avx2 && cmax - cmin + 1 >= AVX2_MIN_SPAN {
                let acc = &mut self.acc[base..base + stride];
                let mut running = 0.0f32;
                for c in cmin..=cmax {
                    running += acc[c];
                    acc[c] = running;
                }
                convert_coverage(&acc[cmin..=cmax], &mut self.cov, bx + cmin, rule);
                for c in cmin..=cmax {
                    let x = bx + c;
                    if self.cov[x] != 0 {
                        first = first.min(x);
                        last = x;
                    }
                    acc[c] = 0.0;
                }
                acc[cmax + 1] = 0.0;
            } else {
                let acc = &mut self.acc[base..base + stride];
                let mut running = 0.0f32;
                for c in cmin..=cmax {
                    running += acc[c];
                    acc[c] = 0.0;
                    let byte = coverage_byte(running, rule);
                    let x = bx + c;
                    self.cov[x] = byte;
                    if byte != 0 {
                        first = first.min(x);
                        last = x;
                    }
                }
                acc[cmax + 1] = 0.0;
            }

            if first <= last {
                self.last_covered += (last - first + 1) as u64;
                let y = row_start + lr;
                row(y, first, last, &mut self.cov);
                for c in &mut self.cov[first..=last] {
                    *c = 0;
                }
            }
        }
        true
    }

    /// Cancellation may leave signed-area deposits in reusable scratch. Clear
    /// them on the exceptional path so the next render cannot inherit coverage.
    fn reset_cancelled_fill(&mut self) {
        self.acc.fill(0.0);
        self.acc_row.fill(0.0);
        self.cov.fill(0);
        self.aet_active.clear();
        self.aet_active_next.clear();
    }

    /// Original per-scanline implementation: for each row, every edge is clipped
    /// to `[y, y+1)` and accumulated into an O(width) buffer, then prefix-summed.
    ///
    /// Test-only. Production routes oversized/sparse windows through
    /// [`Self::fill_scanline_aet`]; this naive version is kept solely as the
    /// byte-identity oracle that pins it (`aet_matches_reference_bit_exact`),
    /// so it is compiled out of shipping builds. Edges and bounds are already
    /// built by `fill`.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn fill_scanline_ref(
        &mut self,
        width: usize,
        _height: usize,
        rule: FillRule,
        row_start: usize,
        row_end: usize,
        mut row: impl FnMut(usize, usize, usize, &mut [u8]),
    ) {
        if self.acc_row.len() < width + 1 {
            self.acc_row.resize(width + 1, 0.0);
        }
        let fwidth = width as f32;
        for y in row_start..row_end {
            let yf = y as f32;
            let mut col_min = width;
            let mut col_max = 0usize;
            for e in &self.edges {
                let ya = e.y_top.max(yf);
                let yb = e.y_bot.min(yf + 1.0);
                if yb <= ya {
                    continue;
                }
                let xa = e.x_at_top + (ya - e.y_top) * e.dxdy;
                let xb = e.x_at_top + (yb - e.y_top) * e.dxdy;
                accumulate(
                    &mut self.acc_row,
                    xa,
                    ya,
                    xb,
                    yb,
                    e.dir,
                    fwidth,
                    &mut col_min,
                    &mut col_max,
                );
            }
            if col_min > col_max {
                continue;
            }
            let mut running = 0.0f32;
            let mut first = width;
            let mut last = 0usize;
            for x in col_min..=col_max {
                running += self.acc_row[x];
                self.acc_row[x] = 0.0;
                let byte = coverage_byte(running, rule);
                self.cov[x] = byte;
                if byte != 0 {
                    first = first.min(x);
                    last = x;
                }
            }
            self.acc_row[col_max + 1] = 0.0;
            if first <= last {
                self.last_covered += (last - first + 1) as u64;
                row(y, first, last, &mut self.cov);
                for c in &mut self.cov[first..=last] {
                    *c = 0;
                }
            }
        }
    }

    /// Active-edge-table variant of [`Self::fill_scanline_ref`]: same O(width)
    /// scanline buffer, but instead of rescanning every edge per row it keeps
    /// only the edges overlapping the current row, so cost is O(edges + Σ active)
    /// rather than O(edges × rows). This is the production oversized/sparse-window
    /// fallback; `fill_scanline_ref` is retained naive as the byte-identity
    /// oracle. Bit-exactness holds because, per row, the *contributing* edges
    /// (those passing the `yb <= ya` clip) are accumulated in the identical
    /// original-edge-index order — only non-contributing float no-ops are skipped.
    #[allow(clippy::too_many_arguments)]
    fn fill_scanline_aet(
        &mut self,
        width: usize,
        _height: usize,
        rule: FillRule,
        row_start: usize,
        row_end: usize,
        should_cancel: Option<&(dyn Fn() -> bool + Send + Sync)>,
        mut row: impl FnMut(usize, usize, usize, &mut [u8]),
    ) -> bool {
        if self.acc_row.len() < width + 1 {
            self.acc_row.resize(width + 1, 0.0);
        }
        let fwidth = width as f32;
        let bh = row_end - row_start;

        // --- CSR counting sort: bucket contributing edges by activation row. ---
        // An edge's row span is exactly the fast path's (raster.rs deposit loop):
        // the saturating `as` casts also fix NaN (→0) and out-of-range coords, so
        // the AET must reproduce them verbatim rather than "improve" on them.
        self.aet_bucket_start.clear();
        self.aet_bucket_start.resize(bh + 1, 0);
        if self.aet_erend.len() < self.edges.len() {
            self.aet_erend.resize(self.edges.len(), 0);
        }
        let mut contributing = 0usize;
        for (edge_index, e) in self.edges.iter().enumerate() {
            if edge_index & 0xff == 0 && should_cancel.is_some_and(|cancel| cancel()) {
                return false;
            }
            let er_start = (e.y_top.floor() as isize).max(row_start as isize);
            let er_end = (e.y_bot.ceil() as isize).min(row_end as isize).max(0);
            if er_start >= er_end {
                continue; // touches no row — a pure no-op in the reference too
            }
            let lr = er_start as usize - row_start;
            self.aet_bucket_start[lr + 1] += 1; // count into [lr+1] for the scan
            contributing += 1;
        }
        for lr in 0..bh {
            self.aet_bucket_start[lr + 1] += self.aet_bucket_start[lr];
        }
        // Scatter edge indices in edge-index order → every bucket is index-sorted
        // for free. `aet_active_next` doubles as the per-row write cursor here.
        self.aet_order.clear();
        self.aet_order.resize(contributing, 0);
        self.aet_active_next.clear();
        self.aet_active_next
            .extend_from_slice(&self.aet_bucket_start[..bh]);
        for (i, e) in self.edges.iter().enumerate() {
            if i & 0xff == 0 && should_cancel.is_some_and(|cancel| cancel()) {
                return false;
            }
            let er_start = (e.y_top.floor() as isize).max(row_start as isize);
            let er_end = (e.y_bot.ceil() as isize).min(row_end as isize).max(0);
            if er_start >= er_end {
                continue;
            }
            let lr = er_start as usize - row_start;
            let pos = self.aet_active_next[lr] as usize;
            self.aet_order[pos] = i as u32;
            self.aet_active_next[lr] += 1;
            self.aet_erend[i] = er_end as u32;
        }

        // --- Sweep rows, maintaining the index-sorted active list. ---
        self.aet_active.clear();
        for lr in 0..bh {
            if lr & 0x0f == 0 && should_cancel.is_some_and(|cancel| cancel()) {
                return false;
            }
            let y = row_start + lr;
            let yf = y as f32;
            let yu = y as u32;

            // Merge row `lr`'s newly-activating edges into the active list,
            // dropping retired ones (er_end <= y), all order-preserving. Bucket
            // edges have er_start == y so they never retire this row; active and
            // bucket index-sets are disjoint, so no equal-key case arises.
            let b0 = self.aet_bucket_start[lr] as usize;
            let b1 = self.aet_bucket_start[lr + 1] as usize;
            self.aet_active_next.clear();
            let mut i = 0usize;
            let mut j = b0;
            while i < self.aet_active.len() && j < b1 {
                let a = self.aet_active[i];
                if self.aet_erend[a as usize] <= yu {
                    i += 1;
                    continue;
                }
                let bk = self.aet_order[j];
                if a < bk {
                    self.aet_active_next.push(a);
                    i += 1;
                } else {
                    self.aet_active_next.push(bk);
                    j += 1;
                }
            }
            while i < self.aet_active.len() {
                let a = self.aet_active[i];
                if self.aet_erend[a as usize] > yu {
                    self.aet_active_next.push(a);
                }
                i += 1;
            }
            while j < b1 {
                self.aet_active_next.push(self.aet_order[j]);
                j += 1;
            }
            std::mem::swap(&mut self.aet_active, &mut self.aet_active_next);

            // Accumulate active edges in edge-index order — identical operands
            // and order to the reference's `for e in &self.edges` filtered loop.
            let active = std::mem::take(&mut self.aet_active);
            let mut col_min = width;
            let mut col_max = 0usize;
            for (active_index, &ei) in active.iter().enumerate() {
                if active_index & 0xfff == 0 && should_cancel.is_some_and(|cancel| cancel()) {
                    self.aet_active = active;
                    return false;
                }
                let e = self.edges[ei as usize];
                let ya = e.y_top.max(yf);
                let yb = e.y_bot.min(yf + 1.0);
                if yb <= ya {
                    continue;
                }
                let xa = e.x_at_top + (ya - e.y_top) * e.dxdy;
                let xb = e.x_at_top + (yb - e.y_top) * e.dxdy;
                accumulate(
                    &mut self.acc_row,
                    xa,
                    ya,
                    xb,
                    yb,
                    e.dir,
                    fwidth,
                    &mut col_min,
                    &mut col_max,
                );
            }
            self.aet_active = active;

            if col_min > col_max {
                continue;
            }
            let mut running = 0.0f32;
            let mut first = width;
            let mut last = 0usize;
            for x in col_min..=col_max {
                running += self.acc_row[x];
                self.acc_row[x] = 0.0;
                let byte = coverage_byte(running, rule);
                self.cov[x] = byte;
                if byte != 0 {
                    first = first.min(x);
                    last = x;
                }
            }
            self.acc_row[col_max + 1] = 0.0;
            if first <= last {
                self.last_covered += (last - first + 1) as u64;
                row(y, first, last, &mut self.cov);
                for c in &mut self.cov[first..=last] {
                    *c = 0;
                }
            }
        }
        true
    }
}

/// Convert a run of prefix-summed signed-area values (`running[i]`) into coverage
/// bytes written at `cov[dst_start + i]`. Runtime-dispatched to AVX2 when
/// available; the scalar reference and the AVX2 path are bit-for-bit identical.
/// Whether the AVX2 coverage-conversion path is available. The feature probe is
/// cached by the intrinsic, so this is a cheap load.
#[inline]
fn coverage_use_avx2() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

#[inline]
#[allow(
    unsafe_code,
    reason = "dispatches the AVX2 coverage kernel behind a runtime probe; see SAFETY comment"
)]
fn convert_coverage(running: &[f32], cov: &mut [u8], dst_start: usize, rule: FillRule) {
    // Slice the destination once, here, so the AVX2 kernel's length
    // precondition is a slice invariant rather than a caller promise: a wrong
    // `dst_start` panics on this index in *both* paths instead of becoming an
    // out-of-bounds write in the vector one.
    let dst = &mut cov[dst_start..dst_start + running.len()];
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 is present (runtime probe above) and
            // `dst.len() == running.len()` by construction of the slice.
            unsafe {
                convert_coverage_avx2(running, dst, rule);
            }
            return;
        }
    }
    convert_coverage_scalar(running, dst, rule);
}

/// Scalar reference for [`convert_coverage`]. `cov` is the already-offset
/// destination, so it and the AVX2 kernel take the same shape of argument.
#[inline]
fn convert_coverage_scalar(running: &[f32], cov: &mut [u8], rule: FillRule) {
    for (i, &r) in running.iter().enumerate() {
        cov[i] = coverage_byte(r, rule);
    }
}

/// The single source of truth for signed-area → coverage byte. Scalar; the AVX2
/// path replicates this op sequence per lane.
#[inline(always)]
fn coverage_byte(running: f32, rule: FillRule) -> u8 {
    let c = match rule {
        FillRule::NonZero => running.abs().min(1.0),
        FillRule::EvenOdd => {
            let m = running - 2.0 * (running * 0.5).floor();
            if m > 1.0 { 2.0 - m } else { m }
        }
    };
    (c * 255.0 + 0.5) as u8
}

/// AVX2 coverage conversion. Each lane runs the exact scalar `coverage_byte` op
/// sequence — abs via andnot, `min`/`floor`/compare via their intrinsics, the
/// `* 255.0` and `+ 0.5` as separate rounded ops (no FMA), and a truncating
/// f32→i32 cast (`cvtt`) matching `as u8` — so the result is bit-for-bit
/// identical to the scalar path. Verified by
/// `tests::avx2_convert_matches_scalar_bit_exact`.
///
/// # Safety
/// * The CPU must support AVX2 (the caller's runtime probe establishes this).
/// * `cov.len() >= running.len()` — the kernel writes `running.len()` bytes at
///   `cov[0..]` through unchecked stores. [`convert_coverage`] guarantees this
///   by slicing the destination to exactly `running.len()` before the call.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(unsafe_code)]
unsafe fn convert_coverage_avx2(running: &[f32], cov: &mut [u8], rule: FillRule) {
    use core::arch::x86_64::*;

    let n = running.len();
    debug_assert!(
        cov.len() >= n,
        "convert_coverage_avx2: destination {} shorter than {n} coverage values",
        cov.len()
    );
    let sign = _mm256_set1_ps(-0.0);
    let one = _mm256_set1_ps(1.0);
    let two = _mm256_set1_ps(2.0);
    let half = _mm256_set1_ps(0.5);
    let s255 = _mm256_set1_ps(255.0);

    let mut i = 0usize;
    while i + 8 <= n {
        // SAFETY: `i + 8 <= n == running.len()`.
        let r = unsafe { _mm256_loadu_ps(running.as_ptr().add(i)) };
        let c = match rule {
            FillRule::NonZero => {
                // abs(r) then min(_, 1.0)
                let a = _mm256_andnot_ps(sign, r);
                _mm256_min_ps(a, one)
            }
            FillRule::EvenOdd => {
                // m = r - 2*floor(r*0.5); c = if m > 1 { 2 - m } else { m }
                let h = _mm256_mul_ps(r, half);
                let fh = _mm256_floor_ps(h);
                let m = _mm256_sub_ps(r, _mm256_mul_ps(two, fh));
                let gt = _mm256_cmp_ps::<_CMP_GT_OQ>(m, one);
                _mm256_blendv_ps(m, _mm256_sub_ps(two, m), gt)
            }
        };
        // (c * 255.0 + 0.5), separate mul/add to match scalar rounding.
        let scaled = _mm256_add_ps(_mm256_mul_ps(c, s255), half);
        // Truncating cast (matches `as u8`); values sit in [0.5, 255.5].
        let ints = _mm256_cvttps_epi32(scaled);
        // Pack 8×i32 → 8×u8: lane 0-3 and 4-7 packed to u16, then to u8.
        let lo = _mm256_castsi256_si128(ints);
        let hi = _mm256_extracti128_si256::<1>(ints);
        let p16 = _mm_packus_epi32(lo, hi); // 8×u16, in order 0..7
        let p8 = _mm_packus_epi16(p16, p16); // low 8 bytes = elems 0..7
        let bytes = _mm_cvtsi128_si64(p8) as u64;
        // SAFETY: `i + 8 <= n <= cov.len()` (the function's safety contract),
        // so the eight bytes written are inside `cov`.
        unsafe {
            let dst = cov.as_mut_ptr().add(i);
            core::ptr::copy_nonoverlapping(&bytes as *const u64 as *const u8, dst, 8);
        }
        i += 8;
    }
    // Scalar tail.
    while i < n {
        // SAFETY: `i < n <= cov.len()` (the function's safety contract).
        unsafe {
            *cov.get_unchecked_mut(i) = coverage_byte(*running.get_unchecked(i), rule);
        }
        i += 1;
    }
}

/// Deposit one edge segment (from `(xa, ya)` to `(xb, yb)`, `ya < yb`, both
/// within one scanline row) into a single local row of the 2D accumulation
/// buffer. `row_acc` is that row's slice (length `stride`); `bx` maps absolute
/// device columns to local indices. Byte-identical to `accumulate` — same area
/// math, only the storage index is local.
#[allow(clippy::too_many_arguments)]
#[inline]
fn deposit_edge_row(
    acc: &mut [f32],
    base: usize,
    xa: f32,
    ya: f32,
    xb: f32,
    yb: f32,
    dir: f32,
    width: f32,
    bx: usize,
    stride: usize,
    col_min: &mut u32,
    col_max: &mut u32,
) {
    let xa = xa.clamp(0.0, width);
    let xb = xb.clamp(0.0, width);
    let cover_total = (yb - ya) * dir;
    let (x0, x1) = if xa <= xb { (xa, xb) } else { (xb, xa) };
    let span = x1 - x0;

    if span < 1e-9 {
        deposit_local(
            acc,
            base,
            x0.floor(),
            cover_total,
            x0,
            width,
            bx,
            stride,
            col_min,
            col_max,
        );
        return;
    }

    let cover_per_x = cover_total / span;
    let mut c = x0.floor();
    while c < x1 - 1e-9 {
        let lo = c.max(x0);
        let hi = (c + 1.0).min(x1);
        if hi > lo {
            let cover_sub = cover_per_x * (hi - lo);
            let xmid = 0.5 * (lo + hi);
            deposit_local(
                acc, base, c, cover_sub, xmid, width, bx, stride, col_min, col_max,
            );
        }
        c += 1.0;
    }
}

/// Single-column deposit into local row `base` of `acc` (analog of
/// `deposit_partial`). `col` and `xmid` are absolute device coordinates (so the
/// area math matches the reference exactly); the write lands at `base + (col - bx)`.
#[allow(clippy::too_many_arguments)]
#[inline]
fn deposit_local(
    acc: &mut [f32],
    base: usize,
    col: f32,
    cover: f32,
    xmid: f32,
    width: f32,
    bx: usize,
    stride: usize,
    col_min: &mut u32,
    col_max: &mut u32,
) {
    let c = col.clamp(0.0, width - 1.0) as usize;
    let this_px = cover * ((c as f32 + 1.0) - xmid);
    let lc = c.saturating_sub(bx).min(stride - 2);
    acc[base + lc] += this_px;
    acc[base + lc + 1] += cover - this_px;
    if (lc as u32) < *col_min {
        *col_min = lc as u32;
    }
    if (lc as u32) > *col_max {
        *col_max = lc as u32;
    }
}

/// Reference per-scanline accumulate (unchanged original). Distributes the total
/// vertical crossing across the columns the segment spans.
#[allow(clippy::too_many_arguments)]
#[inline]
fn accumulate(
    acc: &mut [f32],
    xa: f32,
    ya: f32,
    xb: f32,
    yb: f32,
    dir: f32,
    width: f32,
    col_min: &mut usize,
    col_max: &mut usize,
) {
    let xa = xa.clamp(0.0, width);
    let xb = xb.clamp(0.0, width);
    let cover_total = (yb - ya) * dir;
    let (x0, x1) = if xa <= xb { (xa, xb) } else { (xb, xa) };
    let span = x1 - x0;

    if span < 1e-9 {
        deposit_partial(acc, x0.floor(), cover_total, x0, width, col_min, col_max);
        return;
    }

    let cover_per_x = cover_total / span;
    let mut c = x0.floor();
    while c < x1 - 1e-9 {
        let lo = c.max(x0);
        let hi = (c + 1.0).min(x1);
        if hi > lo {
            let cover_sub = cover_per_x * (hi - lo);
            let xmid = 0.5 * (lo + hi);
            deposit_partial(acc, c, cover_sub, xmid, width, col_min, col_max);
        }
        c += 1.0;
    }
}

#[inline]
fn deposit_partial(
    acc: &mut [f32],
    col: f32,
    cover: f32,
    xmid: f32,
    width: f32,
    col_min: &mut usize,
    col_max: &mut usize,
) {
    let c = col.clamp(0.0, width - 1.0) as usize;
    let this_px = cover * ((c as f32 + 1.0) - xmid);
    acc[c] += this_px;
    acc[c + 1] += cover - this_px;
    *col_min = (*col_min).min(c);
    *col_max = (*col_max).max(c);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn coverage_grid(
        points: &[[f32; 2]],
        subpaths: &[(usize, usize)],
        w: usize,
        h: usize,
        rule: FillRule,
    ) -> Vec<u8> {
        let mut grid = vec![0u8; w * h];
        let mut k = RasterKernel::default();
        k.fill(points, subpaths, w, h, rule, |y, x0, x1, cov: &mut [u8]| {
            grid[y * w + x0..y * w + x1 + 1].copy_from_slice(&cov[x0..=x1]);
        });
        grid
    }

    /// Drive the reference per-scanline path directly (independent of the
    /// fast-path window cap) for byte-for-byte comparison.
    fn coverage_grid_ref(
        points: &[[f32; 2]],
        subpaths: &[(usize, usize)],
        w: usize,
        h: usize,
        rule: FillRule,
    ) -> Vec<u8> {
        let mut grid = vec![0u8; w * h];
        let mut k = RasterKernel::default();
        k.run_reference(points, subpaths, w, h, rule, |y, x0, x1, cov: &mut [u8]| {
            grid[y * w + x0..y * w + x1 + 1].copy_from_slice(&cov[x0..=x1]);
        });
        grid
    }

    /// Drive the production AET fallback directly (independent of the window
    /// cap) for byte-for-byte comparison against the naive reference.
    fn coverage_grid_aet(
        points: &[[f32; 2]],
        subpaths: &[(usize, usize)],
        w: usize,
        h: usize,
        rule: FillRule,
    ) -> Vec<u8> {
        let mut grid = vec![0u8; w * h];
        let mut k = RasterKernel::default();
        k.run_aet(points, subpaths, w, h, rule, |y, x0, x1, cov: &mut [u8]| {
            grid[y * w + x0..y * w + x1 + 1].copy_from_slice(&cov[x0..=x1]);
        });
        grid
    }

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> (Vec<[f32; 2]>, Vec<(usize, usize)>) {
        (vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]], vec![(0, 4)])
    }

    #[test]
    fn integer_rect_is_exact() {
        let (p, s) = rect(1.0, 1.0, 3.0, 3.0);
        let g = coverage_grid(&p, &s, 4, 4, FillRule::NonZero);
        for y in 0..4 {
            for x in 0..4 {
                let expected = if (1..3).contains(&x) && (1..3).contains(&y) {
                    255
                } else {
                    0
                };
                assert_eq!(g[y * 4 + x], expected, "pixel ({x},{y})");
            }
        }
    }

    #[test]
    fn cancellable_fill_stops_inside_one_path_and_resets_worker_scratch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (points, subpaths) = rect(0.0, 0.0, 128.0, 4096.0);
        let checks = AtomicUsize::new(0);
        let cancel = || checks.fetch_add(1, Ordering::Relaxed) >= 2;
        let mut rows = 0usize;
        let mut kernel = RasterKernel::default();
        let completed = kernel.fill_cancellable(
            &points,
            &subpaths,
            128,
            4096,
            FillRule::NonZero,
            Some(&cancel),
            |_, _, _, _| rows += 1,
        );
        assert!(!completed, "the single long fill must observe cancellation");
        assert!(rows < 4096, "cancellation must stop before page completion");

        // A cancelled deposit pass must not leak signed-area scratch into the
        // next request handled by this worker.
        let mut recovered = vec![0u8; 4];
        let (small_points, small_subpaths) = rect(0.0, 0.0, 2.0, 2.0);
        kernel.fill(
            &small_points,
            &small_subpaths,
            2,
            2,
            FillRule::NonZero,
            |y, x0, x1, cov| {
                recovered[y * 2 + x0..=y * 2 + x1].copy_from_slice(&cov[x0..=x1]);
            },
        );
        assert_eq!(recovered, vec![255; 4]);
    }

    #[test]
    fn half_pixel_horizontal_is_exact() {
        let (p, s) = rect(0.0, 0.0, 0.5, 2.0);
        let g = coverage_grid(&p, &s, 2, 2, FillRule::NonZero);
        for y in 0..2 {
            assert!(
                (g[y * 2] as i32 - 128).abs() <= 1,
                "left col row {y} = {}",
                g[y * 2]
            );
            assert_eq!(g[y * 2 + 1], 0);
        }
    }

    #[test]
    fn half_pixel_vertical_is_exact() {
        let (p, s) = rect(0.0, 0.0, 2.0, 0.5);
        let g = coverage_grid(&p, &s, 2, 2, FillRule::NonZero);
        assert!((g[0] as i32 - 128).abs() <= 1, "row0 = {}", g[0]);
        assert_eq!(g[2], 0, "row1 uncovered");
    }

    #[test]
    fn even_odd_hole() {
        let mut p = vec![[0.0, 0.0], [6.0, 0.0], [6.0, 6.0], [0.0, 6.0]];
        p.extend_from_slice(&[[2.0, 2.0], [4.0, 2.0], [4.0, 4.0], [2.0, 4.0]]);
        let s = vec![(0, 4), (4, 8)];
        let g = coverage_grid(&p, &s, 6, 6, FillRule::EvenOdd);
        assert_eq!(g[3 * 6 + 3], 0, "hole center");
        assert_eq!(g[3 * 6 + 1], 255, "ring pixel");
    }

    #[test]
    fn nonzero_overlap_saturates() {
        let mut p = vec![[0.0, 0.0], [3.0, 0.0], [3.0, 3.0], [0.0, 3.0]];
        p.extend_from_slice(&[[1.0, 0.0], [4.0, 0.0], [4.0, 3.0], [1.0, 3.0]]);
        let s = vec![(0, 4), (4, 8)];
        let g = coverage_grid(&p, &s, 4, 3, FillRule::NonZero);
        assert_eq!(g[2], 255, "overlap column stays 255");
    }

    #[test]
    fn triangle_area_matches() {
        let p = vec![[0.0, 0.0], [4.0, 0.0], [0.0, 4.0]];
        let s = vec![(0, 3)];
        let g = coverage_grid(&p, &s, 4, 4, FillRule::NonZero);
        let total: f32 = g.iter().map(|&b| b as f32 / 255.0).sum();
        assert!((total - 8.0).abs() < 0.1, "area ~= {total}");
    }

    // Deterministic PRNG for fuzzing edge geometry.
    struct Lcg(u64);
    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn f(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (self.next_u32() as f32 / u32::MAX as f32) * (hi - lo)
        }
    }

    fn random_path(rng: &mut Lcg, w: f32, h: f32) -> (Vec<[f32; 2]>, Vec<(usize, usize)>) {
        let mut pts = Vec::new();
        let mut subs = Vec::new();
        let nsub = 1 + (rng.next_u32() % 3) as usize;
        for _ in 0..nsub {
            let start = pts.len();
            let nv = 3 + (rng.next_u32() % 6) as usize;
            for _ in 0..nv {
                pts.push([rng.f(-2.0, w + 2.0), rng.f(-2.0, h + 2.0)]);
            }
            subs.push((start, pts.len()));
        }
        (pts, subs)
    }

    /// The fast path must be byte-for-byte identical to the reference
    /// per-scanline path across a large corpus of random geometry, for both
    /// fill rules. This is the determinism gate: if it holds, page hashes are
    /// unchanged from the pre-rewrite kernel.
    #[test]
    fn fast_path_matches_reference_bit_exact() {
        let mut rng = Lcg(0xC0FFEE_1234_5678);
        for w in [1usize, 3, 8, 17, 40, 128] {
            for h in [1usize, 3, 8, 17, 40, 96] {
                for rule in [FillRule::NonZero, FillRule::EvenOdd] {
                    for _ in 0..40 {
                        let (p, s) = random_path(&mut rng, w as f32, h as f32);
                        let fast = coverage_grid(&p, &s, w, h, rule);
                        let refr = coverage_grid_ref(&p, &s, w, h, rule);
                        assert_eq!(
                            fast, refr,
                            "fast vs reference mismatch at {w}x{h} rule {rule:?}"
                        );
                    }
                }
            }
        }
    }

    /// The AET fallback must be byte-for-byte identical to the naive reference
    /// per-scanline path — the same determinism gate, applied to the path
    /// production actually routes edge-dense/sparse-window fills through.
    #[test]
    fn aet_matches_reference_bit_exact() {
        let mut rng = Lcg(0x5EED_A57_9911_2244);
        for w in [1usize, 3, 8, 17, 40, 128] {
            for h in [1usize, 3, 8, 17, 40, 96] {
                for rule in [FillRule::NonZero, FillRule::EvenOdd] {
                    for _ in 0..40 {
                        let (p, s) = random_path(&mut rng, w as f32, h as f32);
                        let aet = coverage_grid_aet(&p, &s, w, h, rule);
                        let refr = coverage_grid_ref(&p, &s, w, h, rule);
                        assert_eq!(
                            aet, refr,
                            "aet vs reference mismatch at {w}x{h} rule {rule:?}"
                        );
                    }
                }
            }
        }
    }

    /// AET bit-exactness on the boundary geometry the activation/retirement
    /// bookkeeping is most likely to get wrong.
    #[test]
    fn aet_matches_reference_on_edge_cases() {
        let cases: Vec<(Vec<[f32; 2]>, Vec<(usize, usize)>)> = vec![
            rect(1.0, 2.0, 5.0, 6.0),   // integral y_top/y_bot
            rect(0.5, 2.3, 4.5, 5.7),   // fractional band
            rect(-3.0, -2.0, 4.0, 5.0), // negative y, row_start clamp
            rect(2.0, 0.0, 3.0, 8.0),   // spans the whole band
            (vec![[1.0, 1.5], [7.0, 1.5], [4.0, 6.5]], vec![(0, 3)]), // triangle, shared birth row
        ];
        // Plus many thin edges all activating on the same row (stress the merge).
        let mut many = Vec::new();
        let mut subs = Vec::new();
        for k in 0..500u32 {
            let x = 0.1 + (k as f32) * 0.01;
            let base = many.len();
            many.extend_from_slice(&[[x, 0.2], [x + 0.005, 0.2], [x + 0.005, 7.8], [x, 7.8]]);
            subs.push((base, base + 4));
        }
        let mut all = cases;
        all.push((many, subs));
        for (p, s) in &all {
            for rule in [FillRule::NonZero, FillRule::EvenOdd] {
                let aet = coverage_grid_aet(p, s, 8, 8, rule);
                let refr = coverage_grid_ref(p, s, 8, 8, rule);
                assert_eq!(
                    aet, refr,
                    "aet vs reference edge-case mismatch, rule {rule:?}"
                );
            }
        }
    }

    /// AVX2 coverage conversion must be bit-for-bit identical to the scalar
    /// reference over a wide range of signed-area values and both rules.
    #[test]
    fn avx2_convert_matches_scalar_bit_exact() {
        let mut rng = Lcg(0xA5A5_0F0F_1111_2222);
        let n = 8 * 200 + 5; // exercise the SIMD body and the scalar tail
        let mut running = Vec::with_capacity(n);
        for _ in 0..n {
            // Include out-of-[0,1] and negative values (nonzero winding, even-odd
            // multi-wrap) so clamping/triangle-wave/truncation are all exercised.
            running.push(rng.f(-8.0, 8.0));
        }
        for rule in [FillRule::NonZero, FillRule::EvenOdd] {
            let mut a = vec![0u8; n];
            let mut b = vec![0u8; n];
            convert_coverage_scalar(&running, &mut a, rule);
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx2") {
                    unsafe { convert_coverage_avx2(&running, &mut b, rule) };
                } else {
                    convert_coverage_scalar(&running, &mut b, rule);
                }
            }
            #[cfg(not(target_arch = "x86_64"))]
            convert_coverage_scalar(&running, &mut b, rule);
            assert_eq!(a, b, "avx2 vs scalar coverage mismatch, rule {rule:?}");
        }
    }
}

#[cfg(test)]
impl RasterKernel {
    /// Test-only: build edges/bounds (as `fill` does) then rasterize via a
    /// scanline fallback — the naive reference (`aet == false`) or the AET
    /// (`aet == true`) — so tests can drive either independent of the window cap.
    #[allow(clippy::too_many_arguments)]
    fn run_fallback(
        &mut self,
        points: &[[f32; 2]],
        subpaths: &[(usize, usize)],
        width: usize,
        height: usize,
        rule: FillRule,
        row: impl FnMut(usize, usize, usize, &mut [u8]),
        aet: bool,
    ) {
        if width == 0 || height == 0 {
            return;
        }
        if self.cov.len() < width {
            self.cov.resize(width, 0);
        }
        self.edges.clear();
        let mut y_min = f32::INFINITY;
        let mut y_max = f32::NEG_INFINITY;
        for &(start, end) in subpaths {
            let n = end - start;
            if n < 2 {
                continue;
            }
            for i in 0..n {
                let a = points[start + i];
                let b = points[start + (i + 1) % n];
                if a[1] == b[1] {
                    continue;
                }
                let (y_top, y_bot, x_at_top, dir) = if a[1] < b[1] {
                    (a[1], b[1], a[0], 1.0)
                } else {
                    (b[1], a[1], b[0], -1.0)
                };
                let dxdy = (b[0] - a[0]) / (b[1] - a[1]);
                self.edges.push(Edge {
                    y_top,
                    y_bot,
                    x_at_top,
                    dxdy,
                    dir,
                });
                y_min = y_min.min(y_top);
                y_max = y_max.max(y_bot);
            }
        }
        if self.edges.is_empty() {
            return;
        }
        self.last_edges = self.edges.len() as u64;
        let row_start = y_min.floor().max(0.0) as usize;
        let row_end = (y_max.ceil().min(height as f32)) as usize;
        self.last_rows = row_end.saturating_sub(row_start) as u64;
        self.last_covered = 0;
        if row_end <= row_start {
            return;
        }
        if aet {
            let _ = self.fill_scanline_aet(width, height, rule, row_start, row_end, None, row);
        } else {
            self.fill_scanline_ref(width, height, rule, row_start, row_end, row);
        }
    }

    fn run_reference(
        &mut self,
        points: &[[f32; 2]],
        subpaths: &[(usize, usize)],
        width: usize,
        height: usize,
        rule: FillRule,
        row: impl FnMut(usize, usize, usize, &mut [u8]),
    ) {
        self.run_fallback(points, subpaths, width, height, rule, row, false);
    }

    fn run_aet(
        &mut self,
        points: &[[f32; 2]],
        subpaths: &[(usize, usize)],
        width: usize,
        height: usize,
        rule: FillRule,
        row: impl FnMut(usize, usize, usize, &mut [u8]),
    ) {
        self.run_fallback(points, subpaths, width, height, rule, row, true);
    }
}
