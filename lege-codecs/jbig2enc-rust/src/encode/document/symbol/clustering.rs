use super::super::Jbig2Encoder;
use super::text_region::{uf_find, uf_union};
use crate::debug;
use crate::jbig2comparator::Comparator;
use crate::jbig2sym::BitImage;
use anyhow::Result;
use rustc_hash::FxHashMap;
use std::collections::{HashMap, HashSet};

impl<'a> Jbig2Encoder<'a> {
    /// Cluster similar symbols into groups and select prototypes.
    ///
    /// This is the key optimization for symbol-mode compression. After all pages
    /// have been extracted, we group symbols that look similar (e.g., different
    /// renderings of the letter "e") into clusters. Only one prototype per cluster
    /// is stored in the dictionary. Instances that don't exactly match their
    /// prototype are marked for refinement coding (SPM).
    ///
    /// This replaces the naive O(n²) auto_threshold with a dimension-bucketed
    /// approach that's much faster for large symbol sets.
    pub(crate) fn cluster_symbols(&mut self) -> Result<()> {
        let n = self.global_symbols.len();
        if n < 2 {
            return Ok(());
        }

        // Union-find with path compression and union by rank
        let mut parent: Vec<usize> = (0..n).collect();
        let mut uf_rank: Vec<u32> = vec![0; n];
        let mut comparator = Comparator::default();

        // Group by exact dimensions and compare only neighboring sizes.
        let mut buckets: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
        for (i, sym) in self.global_symbols.iter().enumerate() {
            buckets.entry((sym.height, sym.width)).or_default().push(i);
        }

        // Compare within each bucket and adjacent buckets
        let mut bucket_keys: Vec<(usize, usize)> = buckets.keys().copied().collect();
        bucket_keys.sort_unstable();

        let mut compare_pair = |a_idx: usize, b_idx: usize| {
            if uf_find(&mut parent, a_idx) == uf_find(&mut parent, b_idx) {
                return;
            }

            let a_sym = &self.global_symbols[a_idx];
            let b_sym = &self.global_symbols[b_idx];
            let dim_limit = if self.config.text_refine { 2 } else { 1 };
            if (a_sym.width as i32 - b_sym.width as i32).abs() > dim_limit
                || (a_sym.height as i32 - b_sym.height as i32).abs() > dim_limit
            {
                return;
            }

            let area = a_sym.width.max(b_sym.width) * a_sym.height.max(b_sym.height);
            let max_err = if self.config.text_refine {
                ((self.symbol_pixel_counts[a_idx].max(self.symbol_pixel_counts[b_idx]) as f32
                    * 0.10) as u32)
                    .max(((area as f32) * 0.05) as u32)
                    .clamp(3, 20)
            } else {
                ((area as f32 * 0.04) as u32).clamp(2, 12)
            };
            if self.symbol_pixel_counts[a_idx].abs_diff(self.symbol_pixel_counts[b_idx])
                > max_err as usize
            {
                return;
            }

            let dy_limit = if self.config.text_refine { 1 } else { 0 };
            if let Some(result) =
                comparator.compare_for_refine_family(a_sym, b_sym, max_err, dim_limit, dy_limit)
            {
                let dx = result.dx;
                let dy = result.dy;
                if dx.abs() <= dim_limit && dy.abs() <= dy_limit {
                    uf_union(&mut parent, &mut uf_rank, a_idx, b_idx);
                }
            }
        };

        for &(bh, bw) in &bucket_keys {
            let current_bucket = &buckets[&(bh, bw)];
            for ci in 0..current_bucket.len() {
                for cj in (ci + 1)..current_bucket.len() {
                    compare_pair(current_bucket[ci], current_bucket[cj]);
                }
            }

            for dh in -1i32..=1 {
                for dw in -1i32..=1 {
                    let nh = bh as i32 + dh;
                    let nw = bw as i32 + dw;
                    if nh < 0 || nw < 0 {
                        continue;
                    }
                    let neighbor_key = (nh as usize, nw as usize);
                    if neighbor_key <= (bh, bw) {
                        continue;
                    }
                    if let Some(neighbor_bucket) = buckets.get(&neighbor_key) {
                        for &a_idx in current_bucket {
                            for &b_idx in neighbor_bucket {
                                compare_pair(a_idx, b_idx);
                            }
                        }
                    }
                }
            }
        }

        // Build cluster groups
        let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..n {
            let root = uf_find(&mut parent, i);
            clusters.entry(root).or_default().push(i);
        }

        // Select prototype deterministically by usage, then black pixels, then original index.
        let mut old_to_prototype: Vec<usize> = (0..n).collect();
        for (_, members) in &clusters {
            if members.len() <= 1 {
                continue;
            }
            let prototype = self.choose_cluster_prototype(members);
            for &m in members {
                old_to_prototype[m] = prototype;
            }
        }

        // Build new compact symbol list (prototypes only) and index mapping
        let mut seen_prototypes: HashMap<usize, usize> = HashMap::new();
        let mut new_symbols: Vec<BitImage> = Vec::new();
        let mut old_to_new: Vec<usize> = vec![0; n];

        // Process in order so prototype positions are deterministic
        for i in 0..n {
            let proto = old_to_prototype[i];
            if let Some(&new_idx) = seen_prototypes.get(&proto) {
                old_to_new[i] = new_idx;
            } else {
                let new_idx = new_symbols.len();
                new_symbols.push(self.global_symbols[proto].clone());
                seen_prototypes.insert(proto, new_idx);
                old_to_new[i] = new_idx;
            }
        }

        let old_count = n;
        let new_count = new_symbols.len();

        // Remap all instances and mark which ones need refinement
        for page in &mut self.pages {
            for inst in &mut page.symbol_instances {
                let old_idx = inst.symbol_index;
                let new_idx = old_to_new[old_idx];
                let proto = old_to_prototype[old_idx];

                // If this instance's original symbol was NOT the prototype,
                // it needs refinement encoding to preserve quality
                if old_idx != proto {
                    inst.needs_refinement = true;
                    // Compute alignment offset between instance and prototype.
                    // Use a generous error limit (not u32::MAX which overflows in Comparator).
                    let (_, trimmed_inst) = inst.instance_bitmap.trim();
                    let max_ref_err = (trimmed_inst.width * trimmed_inst.height) as u32;
                    if let Some((_, dx, dy)) =
                        comparator.distance(&trimmed_inst, &new_symbols[new_idx], max_ref_err)
                    {
                        inst.refinement_dx = dx;
                        inst.refinement_dy = dy;
                    }
                }

                inst.symbol_index = new_idx;
            }
        }

        // Replace internal state
        self.global_symbols = new_symbols;
        self.symbol_pixel_counts = self
            .global_symbols
            .iter()
            .map(BitImage::count_ones)
            .collect();
        self.rebuild_symbol_metadata();
        self.rebuild_hash_map();

        debug!(
            "Clustering: {} -> {} prototype symbols ({:.1}% reduction)",
            old_count,
            new_count,
            (1.0 - new_count as f64 / old_count.max(1) as f64) * 100.0
        );

        Ok(())
    }

    pub(crate) fn validate_symbol_instance_indices(&self) -> Result<()> {
        for (page_num, page) in self.pages.iter().enumerate() {
            for instance in &page.symbol_instances {
                if instance.symbol_index >= self.global_symbols.len() {
                    anyhow::bail!(
                        "Page {} has symbol instance {} out of range after pruning (max {})",
                        page_num + 1,
                        instance.symbol_index,
                        self.global_symbols.len().saturating_sub(1)
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn validate_symbol_partition(
        &self,
        global_symbol_indices: &[usize],
        page_local_symbols: &[Vec<usize>],
        page_residual_symbols: &[Vec<usize>],
        page_residual_anchor_remaps: &[FxHashMap<usize, usize>],
        page_uses_generic_region: &[bool],
    ) -> Result<()> {
        let global_set: HashSet<usize> = global_symbol_indices.iter().copied().collect();
        for (page_num, page) in self.pages.iter().enumerate() {
            if page_uses_generic_region[page_num] {
                continue;
            }
            let local_set: HashSet<usize> = page_local_symbols[page_num].iter().copied().collect();
            let residual_set: HashSet<usize> =
                page_residual_symbols[page_num].iter().copied().collect();
            for inst in &page.symbol_instances {
                let idx = inst.symbol_index;
                if !global_set.contains(&idx)
                    && !page_residual_anchor_remaps[page_num].contains_key(&idx)
                    && !local_set.contains(&idx)
                    && !residual_set.contains(&idx)
                {
                    anyhow::bail!(
                        "Page {} symbol {} was not resolved to global, local, or residual output",
                        page_num + 1,
                        idx
                    );
                }
            }
        }
        Ok(())
    }
}
