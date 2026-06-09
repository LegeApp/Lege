use super::super::{HashKey, Jbig2Encoder, PageData, hash_key};
use super::types::{RefinementPlan, encoder_diagnostics_enabled};
use crate::jbig2comparator::Comparator;
use crate::jbig2context::build_symbol_context_model;
use crate::jbig2cost::symbol_dictionary_entries_bytes;
use crate::jbig2structs::LossySymbolMode;
use crate::jbig2sym::BitImage;
use crate::jbig2unify::{SymbolUnifyInputs, UnifiedClass};
use anyhow::Result;
use rustc_hash::{FxHashMap, FxHashSet};

impl<'a> Jbig2Encoder<'a> {
    pub(crate) fn estimate_local_symbol_gain(&self, page: &PageData, symbol_index: usize) -> i64 {
        let uses = page
            .symbol_instances
            .iter()
            .filter(|instance| instance.symbol_index == symbol_index)
            .count() as i64;
        let symbol = &self.global_symbols[symbol_index];
        let area = (symbol.width * symbol.height) as i64;
        let dict_cost = 24 + (area / 8);
        let saved_per_use = (area / 10).max(2);
        (uses * saved_per_use) - dict_cost
    }

    pub(crate) fn estimate_global_symbol_gain(&self, symbol_index: usize) -> i64 {
        let uses = self.symbol_usage[symbol_index] as i64;
        let page_span = self.symbol_page_count[symbol_index] as i64;
        let symbol = &self.global_symbols[symbol_index];
        let area = (symbol.width * symbol.height) as i64;
        let dict_cost = 24 + (area / 8);
        let id_savings = ((uses - page_span).max(0)) * 2;
        let reuse_value = (uses * (area / 12).max(2)) + (page_span * 3);
        reuse_value + id_savings - dict_cost
    }

    pub(crate) fn should_keep_text_local_symbol(
        &self,
        page: &PageData,
        symbol_index: usize,
    ) -> bool {
        let _ = (page, symbol_index);
        false
    }

    pub(crate) fn choose_cluster_prototype(&self, members: &[usize]) -> usize {
        if members.len() <= 1 || !self.config.text_refine {
            return *members
                .iter()
                .max_by(|&&lhs, &&rhs| {
                    self.symbol_usage[lhs]
                        .cmp(&self.symbol_usage[rhs])
                        .then_with(|| {
                            self.symbol_pixel_counts[lhs].cmp(&self.symbol_pixel_counts[rhs])
                        })
                        .then_with(|| rhs.cmp(&lhs))
                })
                .unwrap();
        }

        let mut comparator = Comparator::default();
        let mut best_idx = members[0];
        let mut best_cost = u64::MAX;

        for &candidate in members {
            let candidate_symbol = &self.global_symbols[candidate];
            let mut total_cost = 0u64;
            for &other in members {
                if candidate == other {
                    continue;
                }
                let other_symbol = &self.global_symbols[other];
                let area = candidate_symbol.width.max(other_symbol.width)
                    * candidate_symbol.height.max(other_symbol.height);
                let max_err = ((self.symbol_pixel_counts[candidate]
                    .max(self.symbol_pixel_counts[other]) as f32
                    * 0.10) as u32)
                    .max((area / self.config.match_tolerance.max(1) as usize) as u32)
                    .clamp(3, 20);

                match comparator.compare_for_refine_family(
                    other_symbol,
                    candidate_symbol,
                    max_err,
                    2,
                    1,
                ) {
                    Some(result) => {
                        let err = result.total_err;
                        let dx = result.dx;
                        let dy = result.dy;
                        let refinement_penalty = err as u64 + ((dx.abs() + dy.abs()) as u64 * 2);
                        total_cost += refinement_penalty * self.symbol_usage[other] as u64;
                    }
                    None => total_cost += 1_000_000,
                }
            }

            if total_cost < best_cost
                || (total_cost == best_cost
                    && (
                        self.symbol_usage[candidate],
                        self.symbol_pixel_counts[candidate],
                    ) > (
                        self.symbol_usage[best_idx],
                        self.symbol_pixel_counts[best_idx],
                    ))
            {
                best_cost = total_cost;
                best_idx = candidate;
            }
        }

        best_idx
    }

    pub(crate) fn note_symbol_page(&mut self, symbol_index: usize, page_num: usize) {
        if self.symbol_last_page_seen[symbol_index] != Some(page_num) {
            self.symbol_last_page_seen[symbol_index] = Some(page_num);
            self.symbol_page_count[symbol_index] += 1;
            self.page_symbol_indices[page_num].push(symbol_index);
        }
    }

    pub(crate) fn push_symbol(
        &mut self,
        symbol: BitImage,
        pixel_count: usize,
        page_num: usize,
    ) -> usize {
        let idx = self.global_symbols.len();
        self.symbol_signatures
            .push(Self::compute_symbol_signature(&symbol));
        self.symbol_pixel_counts.push(pixel_count);
        self.global_symbols.push(symbol);
        self.symbol_usage.push(1);
        self.symbol_page_count.push(0);
        self.symbol_last_page_seen.push(None);
        self.note_symbol_page(idx, page_num);
        idx
    }

    pub(crate) fn rebuild_symbol_metadata(&mut self) {
        self.symbol_usage = vec![0; self.global_symbols.len()];
        self.symbol_page_count = vec![0; self.global_symbols.len()];
        self.symbol_last_page_seen = vec![None; self.global_symbols.len()];
        self.page_symbol_indices = vec![Vec::new(); self.pages.len()];
        self.symbol_pixel_counts = self
            .global_symbols
            .iter()
            .map(BitImage::count_ones)
            .collect();
        self.symbol_signatures = self
            .global_symbols
            .iter()
            .map(Self::compute_symbol_signature)
            .collect();

        for page_num in 0..self.pages.len() {
            let instance_indices: Vec<usize> = self.pages[page_num]
                .symbol_instances
                .iter()
                .map(|inst| inst.symbol_index)
                .collect();
            for symbol_index in instance_indices {
                self.symbol_usage[symbol_index] += 1;
                self.note_symbol_page(symbol_index, page_num);
            }
        }
    }

    pub(crate) fn rebuild_hash_map(&mut self) {
        self.hash_map.clear();
        self.hash_map.reserve(self.global_symbols.len());
        for (idx, symbol) in self.global_symbols.iter().enumerate() {
            let key = hash_key(symbol);
            self.hash_map.entry(key).or_default().push(idx);
        }
    }

    pub(crate) fn build_symbol_unify_classes(&mut self) -> Vec<UnifiedClass> {
        let diagnostics_enabled = encoder_diagnostics_enabled();
        let context_model =
            build_symbol_context_model(&self.pages, &self.global_symbols, &self.symbol_signatures);
        let (classes, diagnostics) =
            crate::jbig2unify::build_symbol_unify_classes(SymbolUnifyInputs {
                config: self.config,
                global_symbols: &self.global_symbols,
                symbol_usage: &self.symbol_usage,
                symbol_page_count: &self.symbol_page_count,
                symbol_signatures: &self.symbol_signatures,
                symbol_pixel_counts: &self.symbol_pixel_counts,
                context_model: Some(&context_model),
                collect_diagnostics: diagnostics_enabled,
            });
        if diagnostics_enabled {
            self.state.decision_debug_lines.extend(diagnostics.lines);
        }
        classes
    }

    pub(crate) fn compact_symbol_table_after_remap(&mut self) {
        let mut used = vec![false; self.global_symbols.len()];
        for page in &self.pages {
            for instance in &page.symbol_instances {
                if instance.symbol_index < used.len() {
                    used[instance.symbol_index] = true;
                }
            }
        }

        let old_symbols = self.global_symbols.clone();
        let mut new_index = vec![usize::MAX; old_symbols.len()];
        let mut new_symbols = Vec::new();

        for (old_index, symbol) in old_symbols.into_iter().enumerate() {
            if used[old_index] {
                new_index[old_index] = new_symbols.len();
                new_symbols.push(symbol);
            }
        }

        for page in &mut self.pages {
            for instance in &mut page.symbol_instances {
                instance.symbol_index = new_index[instance.symbol_index];
            }
        }

        self.global_symbols = new_symbols;
        self.rebuild_symbol_metadata();
        self.rebuild_hash_map();
    }

    pub(crate) fn alias_local_symbols_to_globals(&mut self) -> Result<()> {
        if self.pages.len() <= 1 || self.global_symbols.is_empty() {
            return Ok(());
        }
        let text_refine = self.config.text_refine;
        let global_indices: Vec<usize> = self
            .global_symbols
            .iter()
            .enumerate()
            .filter(|(i, _)| self.symbol_page_count[*i] > 1)
            .map(|(i, _)| i)
            .collect();
        if global_indices.is_empty() {
            return Ok(());
        }

        let mut global_bucket_map: FxHashMap<HashKey, Vec<usize>> =
            FxHashMap::with_capacity_and_hasher(global_indices.len(), Default::default());
        for &symbol_index in &global_indices {
            global_bucket_map
                .entry(hash_key(&self.global_symbols[symbol_index]))
                .or_default()
                .push(symbol_index);
        }

        let mut comparator = Comparator::default();
        let mut changed = false;
        let mut aliased_symbols = 0usize;
        let mut aliased_instances = 0usize;
        let mut alias_samples = Vec::new();
        for page in &mut self.pages {
            let mut page_local_symbols: FxHashSet<usize> =
                FxHashSet::with_capacity_and_hasher(256, Default::default());
            for instance in &page.symbol_instances {
                if self.symbol_page_count[instance.symbol_index] <= 1 {
                    page_local_symbols.insert(instance.symbol_index);
                }
            }

            for local_symbol_index in page_local_symbols {
                let local_symbol = &self.global_symbols[local_symbol_index];
                let local_sig = self.symbol_signatures[local_symbol_index];
                let pixel_count = self.symbol_pixel_counts[local_symbol_index];
                let area = (local_symbol.width * local_symbol.height) as u32;
                let max_err = if self.config.text_refine {
                    (area / self.config.match_tolerance.max(1)).max(3)
                } else {
                    ((area as f32 * 0.05) as u32).max(2)
                };
                let dim_range: u64 = if self.config.text_refine || self.config.refine {
                    2
                } else {
                    0
                };

                let mut best_match: Option<(usize, u32, i32, i32, bool)> = None;
                let h = local_symbol.height as u64;
                let w = local_symbol.width as u64;
                'bucket_search: for dh_off in 0..=(dim_range * 2) {
                    let dh = h.wrapping_add(dh_off).wrapping_sub(dim_range);
                    if dh >= 10_000 {
                        continue;
                    }
                    for dw_off in 0..=(dim_range * 2) {
                        let dw = w.wrapping_add(dw_off).wrapping_sub(dim_range);
                        if dw >= 10_000 {
                            continue;
                        }
                        let bucket_key = HashKey(dh * 10_000 + dw);
                        let Some(bucket) = global_bucket_map.get(&bucket_key) else {
                            continue;
                        };
                        for &global_symbol_index in bucket {
                            if self.symbol_pixel_counts[global_symbol_index].abs_diff(pixel_count)
                                > max_err as usize + if self.config.text_refine { 8 } else { 6 }
                            {
                                continue;
                            }
                            let stored = self.symbol_signatures[global_symbol_index];
                            let black_tol = if text_refine { 12 } else { 8 };
                            let pos_tol = 2;
                            let centroid_tol = if text_refine { 96 } else { 64 };
                            if local_sig.black.abs_diff(stored.black) > black_tol
                                || local_sig.left_col.abs_diff(stored.left_col) > pos_tol
                                || local_sig.right_col.abs_diff(stored.right_col) > pos_tol
                                || local_sig.top_row.abs_diff(stored.top_row) > pos_tol
                                || local_sig.bottom_row.abs_diff(stored.bottom_row) > pos_tol
                                || local_sig.cx_times_256.abs_diff(stored.cx_times_256)
                                    > centroid_tol
                                || local_sig.cy_times_256.abs_diff(stored.cy_times_256)
                                    > centroid_tol
                            {
                                continue;
                            }
                            let max_dx = if text_refine { 1 } else { 1 };
                            let max_dy = if text_refine { 1 } else { 0 };
                            let Some(result) = comparator.compare_for_refine_family(
                                local_symbol,
                                &self.global_symbols[global_symbol_index],
                                max_err,
                                max_dx,
                                max_dy,
                            ) else {
                                continue;
                            };
                            let err = result.total_err;
                            let dx = result.dx;
                            let dy = result.dy;
                            let exact_dims = local_symbol.width
                                == self.global_symbols[global_symbol_index].width
                                && local_symbol.height
                                    == self.global_symbols[global_symbol_index].height;
                            let (accept, needs_refinement) =
                                if err == 0 && dx == 0 && dy == 0 && exact_dims {
                                    (true, false)
                                } else if text_refine {
                                    (
                                        dx.abs() <= 1
                                            && dy.abs() <= 1
                                            && err <= (max_err / 2).max(2),
                                        true,
                                    )
                                } else if dx.abs() <= 1 && dy == 0 {
                                    // Non-exact: alias onto the global prototype but
                                    // refine per-instance so the original bitmap is
                                    // reconstructed losslessly (no substitution). In an
                                    // explicitly lossy mode (sym-unify) substitution is
                                    // intended, so refinement is not forced there.
                                    (true, self.config.lossy_symbol_mode == LossySymbolMode::Off)
                                } else {
                                    (false, false)
                                };
                            if !accept {
                                continue;
                            }
                            best_match = Some((global_symbol_index, err, dx, dy, needs_refinement));
                            if err == 0 && dx == 0 && dy == 0 {
                                break 'bucket_search;
                            }
                        }
                    }
                }

                let Some((global_symbol_index, _err, dx, dy, needs_refinement)) = best_match else {
                    continue;
                };
                aliased_symbols += 1;
                for instance in &mut page.symbol_instances {
                    if instance.symbol_index == local_symbol_index {
                        instance.symbol_index = global_symbol_index;
                        instance.needs_refinement = needs_refinement;
                        instance.refinement_dx = if needs_refinement { dx } else { 0 };
                        instance.refinement_dy = if needs_refinement { dy } else { 0 };
                        changed = true;
                        aliased_instances += 1;
                    }
                }
                if alias_samples.len() < 64 {
                    alias_samples.push(format!(
                        "alias local->global: local={} global={} dx={} dy={} refine={}",
                        local_symbol_index, global_symbol_index, dx, dy, needs_refinement
                    ));
                }
            }
        }

        if encoder_diagnostics_enabled() {
            if changed {
                self.state.decision_debug_lines.push(format!(
                    "alias pass: {} local symbols / {} instances remapped onto globals",
                    aliased_symbols, aliased_instances
                ));
                self.state.decision_debug_lines.extend(alias_samples);
            } else {
                self.state
                    .decision_debug_lines
                    .push("alias pass: no local symbols remapped onto globals".to_string());
            }
        }
        if changed {
            self.compact_symbol_table_after_remap();
        }

        Ok(())
    }

    pub(crate) fn apply_symbol_unify(&mut self) -> Result<()> {
        if !self.config.uses_symbol_unify() || self.state.lossy_symbol_mode_applied {
            return Ok(());
        }

        let diagnostics_enabled = encoder_diagnostics_enabled();
        let before_exported = self.global_symbols.len();
        let before_estimated_dict_bytes =
            symbol_dictionary_entries_bytes(self.global_symbols.iter());
        let classes = self.build_symbol_unify_classes();
        if classes.is_empty() {
            if diagnostics_enabled {
                self.state
                    .decision_debug_lines
                    .push("sym_unify: no eligible classes".to_string());
            }
            self.state.lossy_symbol_mode_applied = true;
            return Ok(());
        }

        let mut remap: Vec<usize> = (0..self.global_symbols.len()).collect();
        let mut refinement_remap: Vec<Option<RefinementPlan>> =
            vec![None; self.global_symbols.len()];
        let mut unified_members = 0usize;
        let mut border_unified_members = 0usize;
        let mut refined_members = 0usize;
        let mut refinement_subclusters = 0usize;
        let mut retained_border_members = 0usize;
        let mut retained_outlier_members = 0usize;

        if diagnostics_enabled {
            self.state.decision_debug_lines.push(format!(
                "sym_unify: {} classes eligible across {} symbols",
                classes.len(),
                self.global_symbols.len()
            ));

            for class in classes.iter().take(64) {
                self.state.decision_debug_lines.push(format!(
                    "sym_unify class: representative={} class_size={} core_size={} unified={} border_unified={} refined_subclusters={} refined_members={} retained_border={} retained_outliers={} total_usage={} page_span={} representative_score={} estimated_gain={} subclusters={}",
                    class.representative_index,
                    class.class_size,
                    class.dense_core_size,
                    class.core_members.len(),
                    class.border_members.len(),
                    class.refinement_subclusters.len(),
                    class.refinement_subclusters
                        .iter()
                        .map(|subcluster| subcluster.refined_members.len())
                        .sum::<usize>(),
                    class.retained_border_members,
                    class.retained_outlier_members,
                    class.total_usage,
                    class.page_span,
                    class.representative_score,
                    class.estimated_gain,
                    class.candidate_subclusters
                ));
            }
        }

        for class in &classes {
            retained_border_members += class.retained_border_members;
            retained_outlier_members += class.retained_outlier_members;
            for member in &class.core_members {
                remap[member.member_index] = class.representative_index;
                unified_members += 1;
            }
            for member in &class.border_members {
                remap[member.member_index] = class.representative_index;
                border_unified_members += 1;
            }
            refinement_subclusters += class.refinement_subclusters.len();
            for subcluster in &class.refinement_subclusters {
                for member in &subcluster.refined_members {
                    refinement_remap[member.member_index] = Some(RefinementPlan {
                        prototype_input_index: subcluster.prototype_index,
                        refinement_dx: member.dx,
                        refinement_dy: member.dy,
                    });
                    refined_members += 1;
                }
            }
        }

        for page in &mut self.pages {
            for instance in &mut page.symbol_instances {
                let original_index = instance.symbol_index;
                if let Some(refinement) = refinement_remap[original_index] {
                    instance.symbol_index = refinement.prototype_input_index;
                    instance.needs_refinement = true;
                    instance.refinement_dx = refinement.refinement_dx;
                    instance.refinement_dy = refinement.refinement_dy;
                } else {
                    instance.symbol_index = remap[original_index];
                    instance.needs_refinement = false;
                    instance.refinement_dx = 0;
                    instance.refinement_dy = 0;
                }
            }
        }

        self.compact_symbol_table_after_remap();
        if diagnostics_enabled {
            let after_estimated_dict_bytes =
                symbol_dictionary_entries_bytes(self.global_symbols.iter());
            self.state.decision_debug_lines.push(format!(
                "sym_unify export summary: before={} after={} removed={} dict_bytes_before={} dict_bytes_after={} dict_bytes_saved={} unified_members={} border_unified_members={} refined_members={} refinement_subclusters={} retained_border_members={} retained_outlier_members={}",
                before_exported,
                self.global_symbols.len(),
                before_exported.saturating_sub(self.global_symbols.len()),
                before_estimated_dict_bytes,
                after_estimated_dict_bytes,
                before_estimated_dict_bytes.saturating_sub(after_estimated_dict_bytes),
                unified_members,
                border_unified_members,
                refined_members,
                refinement_subclusters,
                retained_border_members,
                retained_outlier_members
            ));
        }
        self.state.lossy_symbol_mode_applied = true;
        Ok(())
    }
}
