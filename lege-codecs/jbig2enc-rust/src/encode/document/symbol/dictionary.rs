use super::super::first_black_pixel;
#[cfg(all(test, feature = "refine"))]
use super::text_region::{uf_find, uf_union};
use super::types::{
    EncodedSymbolDictionary, SymbolDictDiagnostics, SymbolDictLayout, SymbolInstance,
};
#[cfg(all(test, feature = "refine"))]
use super::types::RefinementPlan;
use crate::jbig2arith::{IntProc, Jbig2ArithCoder};
#[cfg(all(test, feature = "refine"))]
use crate::jbig2classify::{
    FamilyBucketKey, SymbolSignature, compute_symbol_signature as compute_symbol_signature_shared,
    family_bucket_key_for_symbol, family_bucket_neighbors, family_match_details,
    refine_compare_score,
};
use crate::jbig2comparator::{Comparator, MAX_DIMENSION_DELTA};
#[cfg(all(test, feature = "refine"))]
use crate::jbig2cost::symbol_dictionary_entry_bytes;
use crate::jbig2structs::{Jbig2Config, SymbolDictParams};
use crate::jbig2sym::{BitImage, Rect};
use crate::{debug, trace};
use anyhow::{Result, anyhow};
#[cfg(all(test, feature = "refine"))]
use std::collections::HashMap;

pub fn encode_symbol_dict(
    symbols: &[&BitImage],
    _config: &Jbig2Config,
    num_imported_symbols: u32,
) -> Result<Vec<u8>> {
    let (payload, _order) = encode_symbol_dict_with_order(symbols, _config, num_imported_symbols)?;
    Ok(payload)
}

/// Computes the canonical encoding order for a list of symbols.
///
/// Returns a `Vec<usize>` where each element is an index into the input `symbols` slice,
/// giving the order symbols will appear in the encoded dictionary (after filtering out
/// zero-size symbols, deduplication, and sorting by height class then width).
///
/// This order must be used when mapping symbol instance IDs in text regions.
pub fn canonicalize_dict_symbols(symbols: &[&BitImage]) -> Vec<usize> {
    // Step 1: Filter zero-size, tracking original indices
    let mut valid: Vec<(usize, &BitImage)> = symbols
        .iter()
        .enumerate()
        .filter(|(_, sym)| sym.width > 0 && sym.height > 0)
        .map(|(i, sym)| (i, *sym))
        .collect();

    // Step 2: Sort by (height ASC, width ASC) — same order as sort_symbols_for_dictionary
    // Use stable sort to preserve input order for identical dimensions.
    // No dedup here: the encoder already deduplicates during extraction + auto_threshold.
    // Removing a symbol would leave text region instances without a valid dictionary mapping.
    valid.sort_by(|a, b| (a.1.height, a.1.width).cmp(&(b.1.height, b.1.width)));

    // Return original indices in canonical order
    valid.into_iter().map(|(orig_idx, _)| orig_idx).collect()
}

pub(crate) fn plan_symbol_dictionary_layout(
    symbols: &[&BitImage],
    config: &Jbig2Config,
    usage_weights: Option<&[usize]>,
) -> Result<SymbolDictLayout> {
    let canonical_order = canonicalize_dict_symbols(symbols);
    if canonical_order.is_empty() {
        return Err(anyhow!(
            "encode_symbol_dict: no valid symbols supplied (all symbols had zero width or height)"
        ));
    }

    let _ = (config, usage_weights);
    Ok(SymbolDictLayout {
        export_input_indices: canonical_order,
        refinements: vec![None; symbols.len()],
        diagnostics: SymbolDictDiagnostics {
            singleton_family_count: symbols.len(),
            exported_member_count: symbols.len(),
            ..Default::default()
        },
    })
}

/// Dictionary-level refinement-family collapse policy. Not used by the
/// production pipeline: `plan_symbol_dictionary_layout` is a deliberate stub
/// that never collapses families, because the encoder performs refinement at
/// the text-region instance level instead, not in the dictionary (see the
/// Phase 3 findings in jbig2dec-gaps-plan.md; commit 4977e24). Kept as a
/// tested reference implementation behind the `refine` feature — see
/// `tests.rs`'s `refinement_layout_collapses_to_prototypes`.
#[cfg(all(test, feature = "refine"))]
pub(crate) fn build_refinement_family_layout(
    symbols: &[&BitImage],
    canonical_order: &[usize],
    usage_weights: Option<&[usize]>,
) -> SymbolDictLayout {
    let mut comparator = Comparator::default();
    let signatures: Vec<SymbolSignature> = symbols
        .iter()
        .map(|sym| compute_symbol_signature_shared(sym))
        .collect();
    let black_counts: Vec<usize> = symbols.iter().map(|sym| sym.count_ones()).collect();

    let mut canonical_pos = vec![usize::MAX; symbols.len()];
    for (pos, &input_index) in canonical_order.iter().enumerate() {
        canonical_pos[input_index] = pos;
    }

    let mut bucket_map: HashMap<FamilyBucketKey, Vec<usize>> = HashMap::new();
    for &input_index in canonical_order {
        let key = family_bucket_key_for_symbol(symbols[input_index], &signatures[input_index]);
        bucket_map.entry(key).or_default().push(input_index);
    }

    let mut parent: Vec<usize> = (0..symbols.len()).collect();
    let mut rank = vec![0u32; symbols.len()];

    for &input_index in canonical_order {
        let symbol = symbols[input_index];
        let key = family_bucket_key_for_symbol(symbol, &signatures[input_index]);

        for neighbor in family_bucket_neighbors(key) {
            let Some(bucket) = bucket_map.get(&neighbor) else {
                continue;
            };
            for &other_input_index in bucket {
                if canonical_pos[other_input_index] >= canonical_pos[input_index] {
                    continue;
                }
                if family_match_details(
                    &mut comparator,
                    symbol,
                    input_index,
                    symbols[other_input_index],
                    other_input_index,
                    &signatures,
                    &black_counts,
                )
                .is_some()
                {
                    uf_union(&mut parent, &mut rank, input_index, other_input_index);
                }
            }
        }
    }

    let mut families: HashMap<usize, Vec<usize>> = HashMap::new();
    for &input_index in canonical_order {
        let root = uf_find(&mut parent, input_index);
        families.entry(root).or_default().push(input_index);
    }

    let mut export_input_indices = Vec::new();
    let mut refinements = vec![None; symbols.len()];
    let mut diagnostics = SymbolDictDiagnostics::default();

    let mut family_members: Vec<Vec<usize>> = families.into_values().collect();
    family_members.sort_by_key(|members| canonical_pos[members[0]]);
    diagnostics.family_count = family_members.len();

    for mut members in family_members {
        members.sort_by_key(|&input_index| canonical_pos[input_index]);
        if members.len() == 1 {
            diagnostics.singleton_family_count += 1;
            diagnostics.exported_member_count += 1;
            export_input_indices.push(members[0]);
            continue;
        }

        let prototype_input_index = choose_family_prototype(
            &members,
            symbols,
            usage_weights,
            &canonical_pos,
            &signatures,
            &black_counts,
        );
        if diagnostics.sample_lines.len() < 128 {
            diagnostics.sample_lines.push(format!(
                "refine family: prototype={} members={} prototype_usage={}",
                prototype_input_index,
                members.len(),
                usage_weights
                    .and_then(|weights| weights.get(prototype_input_index).copied())
                    .unwrap_or(1)
            ));
        }
        export_input_indices.push(prototype_input_index);
        diagnostics.exported_member_count += 1;

        for &member_input_index in &members {
            if member_input_index == prototype_input_index {
                continue;
            }

            let maybe_match = family_match_details(
                &mut comparator,
                symbols[member_input_index],
                member_input_index,
                symbols[prototype_input_index],
                prototype_input_index,
                &signatures,
                &black_counts,
            );

            match maybe_match {
                Some((err, dx, dy))
                    if family_should_refine(
                        symbols[member_input_index],
                        symbols[prototype_input_index],
                        err,
                        dx,
                        dy,
                        usage_weights
                            .and_then(|weights| weights.get(member_input_index).copied())
                            .unwrap_or(1),
                    ) =>
                {
                    refinements[member_input_index] = Some(RefinementPlan {
                        prototype_input_index,
                        refinement_dx: dx,
                        refinement_dy: dy,
                    });
                    diagnostics.refined_member_count += 1;
                    if diagnostics.sample_lines.len() < 128 {
                        diagnostics.sample_lines.push(format!(
                            "  refine member={} -> prototype={} dx={} dy={} err={} usage={}",
                            member_input_index,
                            prototype_input_index,
                            dx,
                            dy,
                            err,
                            usage_weights
                                .and_then(|weights| weights.get(member_input_index).copied())
                                .unwrap_or(1)
                        ));
                    }
                }
                _ => {
                    export_input_indices.push(member_input_index);
                    diagnostics.exported_member_count += 1;
                    if diagnostics.sample_lines.len() < 128 {
                        diagnostics.sample_lines.push(format!(
                            "  export member={} as standalone usage={}",
                            member_input_index,
                            usage_weights
                                .and_then(|weights| weights.get(member_input_index).copied())
                                .unwrap_or(1)
                        ));
                    }
                }
            }
        }
    }

    export_input_indices.sort_by_key(|&input_index| canonical_pos[input_index]);

    SymbolDictLayout {
        export_input_indices,
        refinements,
        diagnostics,
    }
}

#[cfg(all(test, feature = "refine"))]
fn family_refinement_gain(
    target: &BitImage,
    reference: &BitImage,
    err: u32,
    dx: i32,
    dy: i32,
) -> i64 {
    let plain_cost = symbol_dictionary_entry_bytes(target) as i64 + 10;
    let refine_cost = 10
        + err as i64
        + ((dx.abs() + dy.abs()) as i64 * 3)
        + (target.width.abs_diff(reference.width) + target.height.abs_diff(reference.height))
            as i64
            * 2;
    plain_cost - refine_cost
}

#[cfg(all(test, feature = "refine"))]
fn family_should_refine(
    target: &BitImage,
    reference: &BitImage,
    err: u32,
    dx: i32,
    dy: i32,
    usage_count: usize,
) -> bool {
    if usage_count > 1 {
        return false;
    }
    let export_gain = family_refinement_gain(target, reference, err, dx, dy);
    export_gain > 12
}

#[cfg(all(test, feature = "refine"))]
fn choose_family_prototype(
    members: &[usize],
    symbols: &[&BitImage],
    usage_weights: Option<&[usize]>,
    canonical_pos: &[usize],
    signatures: &[SymbolSignature],
    black_counts: &[usize],
) -> usize {
    if members.len() == 1 {
        return members[0];
    }

    let mut comparator = Comparator::default();
    let mut best_idx = members[0];
    let mut best_cost = u64::MAX;
    let mut best_support = 0u64;

    for &candidate in members {
        let mut total_cost = 0u64;
        for &other in members {
            if candidate == other {
                continue;
            }
            let weight = usage_weights
                .and_then(|weights| weights.get(other).copied())
                .unwrap_or(1) as u64;
            match family_match_details(
                &mut comparator,
                symbols[other],
                other,
                symbols[candidate],
                candidate,
                signatures,
                black_counts,
            ) {
                Some((err, dx, dy)) => {
                    total_cost += (refine_compare_score(err, dx, dy) as u64 + 4) * weight;
                }
                None => total_cost += 1_000_000 * weight,
            }
        }

        let candidate_support = usage_weights
            .and_then(|weights| weights.get(candidate).copied())
            .unwrap_or(1) as u64;
        let score_close = if best_cost == u64::MAX {
            false
        } else {
            total_cost <= best_cost + best_cost / 50
        };

        if total_cost < best_cost
            || (score_close && candidate_support > best_support)
            || (total_cost == best_cost
                && candidate_support == best_support
                && canonical_pos[candidate] < canonical_pos[best_idx])
        {
            best_cost = total_cost;
            best_idx = candidate;
            best_support = candidate_support;
        }
    }

    best_idx
}

pub(crate) fn encode_symbol_dictionary_segments(
    symbols: &[&BitImage],
    config: &Jbig2Config,
    layout: &SymbolDictLayout,
) -> Result<EncodedSymbolDictionary> {
    let mut encoded = EncodedSymbolDictionary {
        payload: Vec::new(),
        input_to_exported_pos: vec![u32::MAX; symbols.len()],
        exported_symbol_count: 0,
    };

    let (dict_payload, base_order) =
        encode_symbol_dict_subset_with_order(symbols, config, &layout.export_input_indices, 0)?;
    for (dict_pos, &input_index) in base_order.iter().enumerate() {
        encoded.input_to_exported_pos[input_index] = dict_pos as u32;
    }
    encoded.exported_symbol_count = base_order.len() as u32;
    encoded.payload = dict_payload;

    for (input_index, refinement) in layout.refinements.iter().enumerate() {
        if let Some(refinement) = refinement {
            let prototype_pos = encoded.input_to_exported_pos[refinement.prototype_input_index];
            if prototype_pos != u32::MAX {
                encoded.input_to_exported_pos[input_index] = prototype_pos;
            }
        }
    }

    Ok(encoded)
}

fn encode_symbol_dict_subset_with_order(
    symbols: &[&BitImage],
    config: &Jbig2Config,
    subset_indices: &[usize],
    num_imported_symbols: u32,
) -> Result<(Vec<u8>, Vec<usize>)> {
    let subset_symbols: Vec<&BitImage> = subset_indices.iter().map(|&i| symbols[i]).collect();
    let (payload, subset_order) =
        encode_symbol_dict_with_order(&subset_symbols, config, num_imported_symbols)?;
    let input_order = subset_order
        .into_iter()
        .map(|subset_index| subset_indices[subset_index])
        .collect();
    Ok((payload, input_order))
}

/// Encodes a symbol dictionary, returning both the payload and the mapping from
/// encoded dictionary position → input index.
pub fn encode_symbol_dict_with_order(
    symbols: &[&BitImage],
    _config: &Jbig2Config,
    num_imported_symbols: u32,
) -> Result<(Vec<u8>, Vec<usize>)> {
    // Compute canonical order (filter + dedup + sort)
    let canonical_order = canonicalize_dict_symbols(symbols);

    if canonical_order.is_empty() {
        return Err(anyhow!(
            "encode_symbol_dict: no valid symbols supplied (all symbols had zero width or height)"
        ));
    }

    // Build the ordered symbol list
    let ordered_symbols: Vec<&BitImage> = canonical_order.iter().map(|&i| symbols[i]).collect();

    // Verify symbol dimensions are within JBIG2 limits
    for (i, sym) in ordered_symbols.iter().enumerate() {
        if sym.width > (1 << 24) || sym.height > (1 << 24) {
            return Err(anyhow!(
                "Symbol at index {} exceeds maximum dimensions ({}x{})",
                i,
                sym.width,
                sym.height
            ));
        }
    }

    let mut payload = Vec::new();
    let mut coder = Jbig2ArithCoder::new();

    let num_new_syms = ordered_symbols.len() as u32;
    // Per T.88 §7.4.2.1.6, SDNUMEXSYMS is the total count of exported symbols which
    // includes any imported symbols carried forward from referenced dictionaries.
    let num_export_syms = num_imported_symbols.saturating_add(num_new_syms);

    // Create symbol dictionary parameters
    let params = SymbolDictParams {
        sd_template: 0, // Use standard template 0
        // Match jbig2enc's template-0 adaptive pixels for symbol dictionaries.
        at: [(3, -1), (-3, -1), (2, -2), (-2, -2)],
        refine_aggregate: false,
        refine_template: 0,
        refine_at: [(0, 0), (0, 0)],
        exsyms: num_export_syms,
        newsyms: num_new_syms,
    };

    if cfg!(debug_assertions) {
        debug!("encode_symbol_dict: Exporting {} symbols", num_export_syms);
        trace!("encode_symbol_dict: SymbolDictParams details: {:?}", params);
    }

    // Write the symbol dictionary parameters
    payload.extend(params.to_bytes());

    // Symbols are already in canonical (height, width) order from canonicalize_dict_symbols.
    // We need to encode them in this exact order, grouped by height class for delta encoding.
    // Build height classes from the already-sorted ordered_symbols to preserve the canonical order.
    let mut height_classes: Vec<Vec<&BitImage>> = Vec::new();
    let mut current_height: Option<usize> = None;
    let mut current_class: Vec<&BitImage> = Vec::new();

    for &sym in &ordered_symbols {
        match current_height {
            None => {
                // First symbol
                current_height = Some(sym.height);
                current_class.push(sym);
            }
            Some(h) if sym.height == h => {
                // Same height class
                current_class.push(sym);
            }
            Some(_) => {
                // New height class - push previous and start new
                height_classes.push(current_class);
                current_height = Some(sym.height);
                current_class = vec![sym];
            }
        }
    }
    if !current_class.is_empty() {
        height_classes.push(current_class);
    }

    // Debug: log the encoding order and first few pixels of each symbol for verification
    #[cfg(debug_assertions)]
    {
        debug!(
            "Symbol dictionary encoding order ({} symbols):",
            ordered_symbols.len()
        );
        let mut dict_pos = 0u32;
        for (hc_idx, symbols_in_class) in height_classes.iter().enumerate() {
            debug!(
                "  Height class {}: {} symbols",
                hc_idx,
                symbols_in_class.len()
            );
            for (sym_idx, sym) in symbols_in_class.iter().enumerate() {
                // Log first pixel position for each symbol
                let first_pixel = first_black_pixel(sym);
                if sym_idx < 5 || sym_idx >= symbols_in_class.len() - 2 {
                    debug!(
                        "    dict_pos={} -> {}x{} first_pixel={:?}",
                        dict_pos, sym.width, sym.height, first_pixel
                    );
                } else if sym_idx == 5 {
                    debug!(
                        "    ... ({} symbols omitted) ...",
                        symbols_in_class.len() - 7
                    );
                }
                dict_pos += 1;
            }
        }
    }

    let mut last_height = 0;

    // 4. Encode the height classes
    for symbols_in_class in &height_classes {
        let h = symbols_in_class[0].height; // All symbols in class have same height
        // A. Encode Delta Height
        let delta_h = h as i32 - last_height as i32;
        coder.encode_integer(crate::jbig2arith::IntProc::Iadh, delta_h)?;
        last_height = h;

        let mut last_width = 0;
        #[cfg(debug_assertions)]
        let mut dict_pos = 0u32;

        // Debug: check symbols in this height class (disabled in release)
        #[cfg(debug_assertions)]
        {
            debug!("Height class {} has {} symbols:", h, symbols_in_class.len());
            for (i, symbol) in symbols_in_class.iter().enumerate() {
                debug!("  Symbol {}: {}x{}", i, symbol.width, symbol.height);
            }
        }

        // B. Encode symbols within this height class
        // Symbols within each height class are already sorted by width from canonicalize_dict_symbols.
        for (i, symbol) in symbols_in_class.iter().enumerate() {
            // I. Encode Delta Width
            let delta_w = symbol.width as i32 - last_width;

            // Debug output to help diagnose the issue (disabled in release)
            #[cfg(debug_assertions)]
            debug!(
                "Height class {}, Symbol {}: width={}, last_width={}, delta_w={}",
                h, i, symbol.width, last_width, delta_w
            );

            coder.encode_integer(crate::jbig2arith::IntProc::Iadw, delta_w)?;
            last_width = symbol.width as i32; // last_width becomes current width

            // II. Encode Symbol Bitmap using Generic Region Procedure
            let packed = symbol.packed_words();

            // Debug: dump first few symbols' bitmap data for verification
            #[cfg(debug_assertions)]
            {
                debug!(
                    "  dict_pos={} {}x{} first_word={:08x}",
                    dict_pos,
                    symbol.width,
                    symbol.height,
                    packed.get(0).unwrap_or(&0)
                );
            }

            // Verify bit-order correctness: first black pixel should match between symbol and packed data
            if let Some(expected_first_pixel) = first_black_pixel(symbol) {
                let actual_first_pixel = crate::jbig2sym::first_black_pixel_in_packed(
                    packed,
                    symbol.width,
                    symbol.height,
                );
                assert_eq!(
                    actual_first_pixel,
                    Some(expected_first_pixel),
                    "bit-order / row-order mismatch in symbol dict packer! Expected first black pixel at {:?}, got {:?}",
                    expected_first_pixel,
                    actual_first_pixel
                );
            }

            coder.encode_generic_region(
                packed,
                symbol.width,
                symbol.height,
                params.sd_template,
                &[(3, -1), (-3, -1), (2, -2), (-2, -2)],
            )?;

            #[cfg(debug_assertions)]
            {
                dict_pos += 1;
            }
        }

        // OOB marks the end of this height class.
        coder.encode_oob(IntProc::Iadw)?;
    }

    // Export flags come after the symbol bitmap data (run-length form).
    // T.88 §7.4.2.2: the run-length must cover SDNUMINSYMS + SDNUMNEWSYMS slots.
    // We export every symbol (both imported and new), so the sequence is a single
    // "all exported" run covering num_export_syms = num_imported + num_new.
    coder.encode_integer(IntProc::Iaex, 0)?;
    let num_export_syms =
        i32::try_from(num_export_syms).map_err(|_| anyhow!("too many exported symbols"))?;
    coder.encode_integer(IntProc::Iaex, num_export_syms)?;

    // 5. flush the coder ONCE
    coder.flush(true);

    // 6. Append the single, complete arithmetic payload
    payload.extend(coder.as_bytes());

    Ok((payload, canonical_order))
}
/// Represents a single symbol instance in a text region, with refinement info.
#[derive(Debug, Clone)]
pub struct TextRegionSymbolInstance {
    /// The ID of the symbol in the dictionary.
    pub symbol_id: u32,
    /// The x-coordinate of the instance's top-left corner.
    pub x: i32,
    /// The y-coordinate of the instance's top-left corner.
    pub y: i32,
    /// The horizontal refinement offset.
    pub dx: i32,
    /// The vertical refinement offset.
    pub dy: i32,
    /// Whether this instance is a refinement of a dictionary symbol.
    pub is_refinement: bool,
}

impl TextRegionSymbolInstance {
    /// Returns the position of this symbol instance as a Rect.
    pub fn position(&self) -> crate::jbig2sym::Rect {
        crate::jbig2sym::Rect {
            x: self.x as u32,
            y: self.y as u32,
            width: 0,  // These will be set by the caller
            height: 0, // These will be set by the caller
        }
    }

    /// Returns the symbol index for this instance.
    pub fn symbol_index(&self) -> usize {
        self.symbol_id as usize
    }

    /// Converts to a SymbolInstance
    pub fn to_symbol_instance(&self, symbol_bitmap: &BitImage) -> SymbolInstance {
        SymbolInstance {
            symbol_index: self.symbol_id as usize,
            position: self.position(),
            instance_bitmap: symbol_bitmap.clone(),
            needs_refinement: self.is_refinement,
            refinement_dx: self.dx,
            refinement_dy: self.dy,
        }
    }
}

pub fn build_dictionary_and_get_instances(
    symbols: &[(Rect, BitImage)],
    comparator: &mut Comparator,
) -> (Vec<BitImage>, Vec<TextRegionSymbolInstance>) {
    let mut dictionary_symbols: Vec<BitImage> = Vec::with_capacity(symbols.len());
    let mut dictionary_black_pixels = Vec::with_capacity(symbols.len());
    let mut instances = Vec::with_capacity(symbols.len());

    for (rect, symbol_image) in symbols.iter() {
        let mut found_match = false;
        // Use a 10% error threshold for matching, as recommended.
        let max_err = ((symbol_image.width * symbol_image.height) / 10).max(3) as u32;
        let symbol_black_pixels = symbol_image.count_ones();

        for (dict_idx, dict_symbol) in dictionary_symbols.iter().enumerate() {
            if symbol_image.width.abs_diff(dict_symbol.width) > MAX_DIMENSION_DELTA
                || symbol_image.height.abs_diff(dict_symbol.height) > MAX_DIMENSION_DELTA
            {
                continue;
            }

            if symbol_black_pixels.abs_diff(dictionary_black_pixels[dict_idx]) > max_err as usize {
                continue;
            }

            // Use a low max_err for finding near-duplicates
            if let Some((err, dx, dy)) = comparator.distance(symbol_image, dict_symbol, max_err) {
                instances.push(TextRegionSymbolInstance {
                    symbol_id: dict_idx as u32,
                    x: rect.x as i32,
                    y: rect.y as i32,
                    dx,
                    dy,
                    is_refinement: err > 0,
                });
                found_match = true;
                break;
            }
        }

        if !found_match {
            let new_idx = dictionary_symbols.len();
            dictionary_symbols.push(symbol_image.clone());
            dictionary_black_pixels.push(symbol_black_pixels);
            instances.push(TextRegionSymbolInstance {
                symbol_id: new_idx as u32,
                x: rect.x as i32,
                y: rect.y as i32,
                dx: 0,
                dy: 0,
                is_refinement: false,
            });
        }
    }

    (dictionary_symbols, instances)
}
