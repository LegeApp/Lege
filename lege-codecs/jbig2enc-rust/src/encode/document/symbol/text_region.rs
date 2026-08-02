use super::dictionary::{
    TextRegionSymbolInstance, build_dictionary_and_get_instances,
    encode_symbol_dictionary_segments, plan_symbol_dictionary_layout,
};
use super::text_region_refine::encode_text_region_with_refinement;
use super::types::SymbolInstance;
use crate::jbig2arith::{IntProc, Jbig2ArithCoder};
#[cfg(feature = "symboldict")]
use crate::jbig2cc::analyze_page;
use crate::jbig2comparator::Comparator;
use crate::jbig2structs::{Jbig2Config, Segment, SegmentType, TextRegionParams};
use crate::jbig2sym::{BitImage, Rect};
use crate::{debug, trace};
use anyhow::{Result, anyhow};

/// Computes the bounding box that contains all symbol instances.
///
/// # Arguments
/// * `instances` - Slice of symbol instances to compute bounds for
/// * `all_known_symbols` - All available symbol bitmaps
///
/// # Returns
/// A tuple of (min_x, min_y, width, height) representing the bounding box
fn compute_region_bounds(
    instances: &[TextRegionSymbolInstance],
    all_known_symbols: &[&BitImage],
) -> (u32, u32, u32, u32) {
    if instances.is_empty() {
        return (0, 0, 0, 0);
    }
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x_coord = 0u32;
    let mut max_y_coord = 0u32;

    for instance in instances {
        let sym_idx = instance.symbol_id as usize;
        if sym_idx >= all_known_symbols.len() {
            continue; // Skip invalid symbol indices
        }

        let pos = Rect {
            x: instance.x as u32, // Convert i32 to u32
            y: instance.y as u32, // Convert i32 to u32
            width: crate::jbig2shared::usize_to_u32(all_known_symbols[sym_idx].width),
            height: crate::jbig2shared::usize_to_u32(all_known_symbols[sym_idx].height),
        };

        min_x = min_x.min(pos.x);
        min_y = min_y.min(pos.y);
        max_x_coord = max_x_coord.max(pos.x + pos.width);
        max_y_coord = max_y_coord.max(pos.y + pos.height);
    }

    // Handle potential underflow if max < min (shouldn't happen with valid coordinates)
    let region_width = if max_x_coord > min_x {
        max_x_coord - min_x
    } else {
        0
    };

    let region_height = if max_y_coord > min_y {
        max_y_coord - min_y
    } else {
        0
    };

    (min_x, min_y, region_width, region_height)
}

// (The previous `encode_refine` helper was removed: it built only 4 refinement
// contexts instead of the 8192-state GRREF template required by T.88 §6.3, so
// any output it produced was not decodable. It had no callers in the crate.
// Use `Jbig2ArithCoder::encode_refinement_region` (called from
// `encode_text_region_with_refinement`) for spec-compliant refinement coding.)

/// Encodes a text region segment using pre-computed dictionary position maps.
///
/// Unlike `encode_text_region` which maps by list position, this function uses
/// explicit global_symbols_index → dictionary_position maps that account for the
/// canonical (filter/dedup/sort) order produced by `encode_symbol_dict_with_order`.
///
/// The decoder concatenates dictionary exports: global_dict[0..N] then local_dict[0..M].
/// Symbol IDs 0..N-1 map to the global dict, N..N+M-1 map to the local dict.
#[inline]
pub(crate) fn symbol_id_from_dense_maps(
    symbol_index: usize,
    global_sym_to_dict_pos: &[u32],
    num_global_dict_symbols: u32,
    local_sym_to_dict_pos: &[u32],
) -> Option<u32> {
    let global = global_sym_to_dict_pos
        .get(symbol_index)
        .copied()
        .unwrap_or(u32::MAX);
    if global != u32::MAX {
        return Some(global);
    }
    let local = local_sym_to_dict_pos
        .get(symbol_index)
        .copied()
        .unwrap_or(u32::MAX);
    if local != u32::MAX {
        Some(num_global_dict_symbols + local)
    } else {
        None
    }
}

pub fn encode_text_region_mapped(
    instances: &[SymbolInstance],
    config: &Jbig2Config,
    all_symbols: &[BitImage],
    global_sym_to_dict_pos: &[u32],
    num_global_dict_symbols: u32,
    local_sym_to_dict_pos: &[u32],
    page_num: usize,
    num_local_dict_symbols: u32,
) -> Result<Vec<u8>> {
    if instances.is_empty() {
        return Err(anyhow!("No symbol instances provided for text region"));
    }

    let debug_encoding = page_num == 0 && std::env::var("JBIG2_DEBUG").map_or(false, |v| v == "1");
    let mut enc_debug_lines: Vec<String> = Vec::new();

    let num_total_dict_symbols = num_global_dict_symbols + num_local_dict_symbols;

    let mut payload = Vec::new();
    let mut coder = Jbig2ArithCoder::new();

    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x_coord = 0u32;
    let mut max_y_coord = 0u32;

    for instance in instances {
        let sym = &all_symbols[instance.symbol_index];
        min_x = min_x.min(instance.position.x);
        min_y = min_y.min(instance.position.y);
        max_x_coord = max_x_coord.max(instance.position.x + sym.width as u32);
        max_y_coord = max_y_coord.max(instance.position.y + sym.height as u32);
    }

    let region_width = max_x_coord.saturating_sub(min_x);
    let region_height = max_y_coord.saturating_sub(min_y);

    let params = TextRegionParams {
        width: region_width,
        height: region_height,
        x: min_x,
        y: min_y,
        ds_offset: config.text_ds_offset,
        refine: config.text_refine,
        log_strips: config.text_log_strips,
        ref_corner: config.text_ref_corner,
        transposed: config.text_transposed,
        comb_op: config.text_comb_op,
        refine_template: config.text_refine_template,
    };

    payload.extend(params.to_bytes());
    payload.extend_from_slice(&(instances.len() as u32).to_be_bytes());

    // T.88 §7.4.3.1.7 / §6.5.8.2.3 (SDHUFF=0): arithmetic text-region IAID uses
    // SBSYMCODELEN = ceil(log2(SBNUMSYMS)) with NO floor of 1. The formulas differ
    // only at SBNUMSYMS == 1, where the decoder reads zero ID bits; emitting a
    // spurious 1-bit IAID there desyncs the arithmetic decoder for every later
    // symbol (S-coordinate corruption). encode_iaid is skipped when this is 0.
    let symbol_id_bits = log2up(num_total_dict_symbols.max(1));

    #[derive(Clone, Copy)]
    struct EncodedInstance {
        strip_base: i32,
        x: i32,
        t_offset: i32,
        symbol_id: u32,
        symbol_width: i32,
    }

    let strip_width = 1i32 << params.log_strips.min(3);
    let mut encoded_instances = Vec::with_capacity(instances.len());

    for instance in instances {
        let gs_idx = instance.symbol_index;
        let sym = &all_symbols[gs_idx];

        // Map to dictionary position using the canonical order maps.
        // First check global dict, then local dict (offset by num_global_dict_symbols).
        let symbol_id = if let Some(symbol_id) = symbol_id_from_dense_maps(
            gs_idx,
            global_sym_to_dict_pos,
            num_global_dict_symbols,
            local_sym_to_dict_pos,
        ) {
            symbol_id
        } else {
            anyhow::bail!(
                "Symbol instance (global_symbols index {}) not found in any dictionary!",
                gs_idx
            );
        };

        let abs = instance.position;
        let rel_x = abs.x as i32 - min_x as i32;
        // REFCORNER=TOPLEFT (value 1): T is the top of the original bounding box.
        let rel_y = abs.y as i32 - min_y as i32;
        let strip_base = (rel_y / strip_width) * strip_width;
        let t_offset = rel_y - strip_base;

        encoded_instances.push(EncodedInstance {
            strip_base,
            x: rel_x,
            t_offset,
            symbol_id,
            symbol_width: sym.width as i32,
        });
    }

    encoded_instances.sort_by_key(|e| (e.strip_base, e.x));

    if debug_encoding {
        enc_debug_lines.push(format!("=== PAGE 0 ENCODING LOG ==="));
        enc_debug_lines.push(format!(
            "Region: {}x{} at ({},{})",
            params.width, params.height, params.x, params.y
        ));
        enc_debug_lines.push(format!(
            "min_x={} min_y={} strip_width={}",
            min_x, min_y, strip_width
        ));
        enc_debug_lines.push(format!(
            "Total instances: {}, dict symbols: {}",
            encoded_instances.len(),
            num_total_dict_symbols
        ));
        enc_debug_lines.push(String::new());

        // Show mapping from symbol_id to dimensions for reference
        enc_debug_lines.push("Symbol ID -> dimensions lookup (first 30):".to_string());
        for (dict_id, sym) in all_symbols.iter().enumerate().take(30) {
            let dict_pos = symbol_id_from_dense_maps(
                dict_id,
                global_sym_to_dict_pos,
                num_global_dict_symbols,
                local_sym_to_dict_pos,
            )
            .unwrap_or(u32::MAX);
            enc_debug_lines.push(format!(
                "  gs_idx={} -> dict_pos={} ({}x{})",
                dict_id, dict_pos, sym.width, sym.height
            ));
        }
        enc_debug_lines.push(String::new());

        enc_debug_lines.push(format!(
            "{:<6} {:<8} {:<8} {:<10} {:<8} {:<10} {:<10} {:<10}",
            "Idx", "SymID", "SymW", "StripBase", "TOffset", "RelX", "DeltaT", "DeltaS"
        ));
    }

    let mut strip_t = 0i32;
    let mut first_s = 0i32;
    let mut idx = 0usize;

    // §6.4.5 step 1: initial STRIPT value (decoder reads one IADT before the loop)
    coder.encode_integer(IntProc::Iadt, 0)?;

    let sbdsoffset = params.ds_offset as i32;

    while idx < encoded_instances.len() {
        let current_strip = encoded_instances[idx].strip_base;
        let delta_t = current_strip - strip_t;
        coder.encode_integer(IntProc::Iadt, delta_t / strip_width)?;

        if debug_encoding && delta_t != 0 {
            enc_debug_lines.push(format!(
                "--- strip break: IADT delta_t={} (strip_t {} → {})",
                delta_t, strip_t, current_strip
            ));
        }
        strip_t = current_strip;

        let mut first_symbol_in_strip = true;
        let mut current_s = 0i32;
        while idx < encoded_instances.len() && encoded_instances[idx].strip_base == current_strip {
            let item = encoded_instances[idx];
            let delta_s;
            if first_symbol_in_strip {
                delta_s = item.x - first_s;
                coder.encode_integer(IntProc::Iafs, delta_s)?;
                first_s += delta_s;
                current_s = first_s;
                first_symbol_in_strip = false;
            } else {
                // T.88 §6.4.5 step 4d: decoder computes CURS = CURS + IADS + SBDSOFFSET.
                // Therefore encode IADS = (item.x - current_s) - sbdsoffset so the
                // decoder lands on item.x. With the default ds_offset = 0 this is a
                // no-op, but it makes the encoder behave correctly when callers set a
                // non-zero text_ds_offset.
                delta_s = item.x - current_s - sbdsoffset;
                coder.encode_integer(IntProc::Iads, delta_s)?;
                current_s += delta_s + sbdsoffset;
            }

            if debug_encoding {
                enc_debug_lines.push(format!(
                    "{:<6} {:<8} {:<8} {:<10} {:<8} {:<10} {:<10} {:<10}",
                    idx,
                    item.symbol_id,
                    item.symbol_width,
                    item.strip_base,
                    item.t_offset,
                    item.x,
                    delta_t,
                    delta_s
                ));
            }

            if strip_width > 1 {
                coder.encode_integer(IntProc::Iait, item.t_offset)?;
            }
            if symbol_id_bits > 0 {
                coder.encode_iaid(item.symbol_id, symbol_id_bits as u8)?;
            }
            current_s += item.symbol_width - 1;
            idx += 1;
        }
        coder.encode_oob(IntProc::Iads)?;
    }

    // Decode simulation: replay §6.4.5 from the encoder's perspective
    // to verify positions match what the decoder will compute.
    if debug_encoding {
        enc_debug_lines.push(String::new());
        enc_debug_lines.push(format!("=== DECODE SIMULATION ==="));
        enc_debug_lines.push(format!(
            "{:<6} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<8}",
            "Idx", "ExpX", "ExpY", "DecS", "DecT", "AbsX", "AbsY", "Match?"
        ));

        // Collect the IADT, IAFS, IADS, IAIT values and symbol_ids in encoding order for replay
        let sbstrips = strip_width;
        let sbdsoffset = params.ds_offset as i32;
        let mut dec_stript = 0i32;
        let mut dec_firsts = 0i32;
        let mut sim_idx = 0usize;
        let mut strip_start = 0usize;

        // Group instances by strip_base (they're already sorted)
        while sim_idx < encoded_instances.len() {
            let current_strip = encoded_instances[sim_idx].strip_base;
            // Compute delta_t the same way the encoder did
            let delta_t = if sim_idx == 0 && current_strip == 0 {
                0 // first IADT is the initial STRIPT value
            } else if sim_idx == strip_start {
                // Changed strip: the encoder emits IADT = (current_strip - prev_strip_t) / strip_width
                // But we need to replay the exact values. Let's just recompute.
                current_strip - dec_stript
            } else {
                0 // same strip, no IADT
            };

            // §6.4.5 step 2: STRIPT = STRIPT + IADT × SBSTRIPS
            if sim_idx == strip_start || sim_idx == 0 {
                let iadt_value = (current_strip - dec_stript) / sbstrips;
                dec_stript += iadt_value * sbstrips;
            }

            let mut first_in_strip = true;
            let mut dec_curs = 0i32;
            let strip_base = current_strip;

            while sim_idx < encoded_instances.len()
                && encoded_instances[sim_idx].strip_base == strip_base
            {
                let item = encoded_instances[sim_idx];

                if first_in_strip {
                    // §6.4.5: FIRSTS = FIRSTS + IAFS; CURS = FIRSTS
                    let iafs = item.x - dec_firsts;
                    dec_firsts += iafs;
                    dec_curs = dec_firsts;
                    first_in_strip = false;
                } else {
                    // §6.4.5: encoder emits IADS = item.x - dec_curs - SBDSOFFSET so that
                    // the decoder's CURS = CURS + IADS + SBDSOFFSET lands on item.x.
                    let iads = item.x - dec_curs - sbdsoffset;
                    dec_curs += iads + sbdsoffset;
                }

                // §6.4.5: TI = STRIPT * SBSTRIPS + IAIT (IAIT=0 when SBSTRIPS=1)
                let dec_ti = dec_stript;
                let dec_si = dec_curs;

                // Absolute page coords the decoder would compute
                let abs_x = dec_si + min_x as i32;
                let abs_y = dec_ti + min_y as i32;

                // Expected absolute coords (what we intended)
                let exp_x = item.x + min_x as i32;
                let exp_y = item.strip_base + min_y as i32;

                let ok = abs_x == exp_x && abs_y == exp_y;
                let tag = if ok { "OK" } else { "MISMATCH!" };

                if !ok || sim_idx < 60 {
                    enc_debug_lines.push(format!(
                        "{:<6} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<8}",
                        sim_idx, exp_x, exp_y, dec_si, dec_ti, abs_x, abs_y, tag
                    ));
                }

                // §6.4.5 step 4g: CURS = CURS + WI - 1
                dec_curs += item.symbol_width - 1;
                sim_idx += 1;
            }
            strip_start = sim_idx;
        }
    }

    // Write encoding debug log for page 0
    if debug_encoding && !enc_debug_lines.is_empty() {
        let log_path = std::path::Path::new("jbig2_debug_page0.log");
        // Append to same file as matching log
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            use std::io::Write;
            let _ = writeln!(f, "");
            for line in &enc_debug_lines {
                let _ = writeln!(f, "{}", line);
            }
        }
    }

    coder.flush(true);
    payload.extend(coder.as_bytes());

    Ok(payload)
}
/// Encodes a text region segment to the output.
///
/// This function takes a list of symbols and their instances in the text region,
/// and encodes them according to JBIG2 spec §6.4.10. It supports both absolute coordinates
/// and IADW/IADH delta encoding for more efficient compression.
pub fn encode_text_region(
    instances: &[SymbolInstance],
    config: &Jbig2Config,
    all_known_symbols: &[&BitImage],
    global_dict_indices: &[usize],
    local_dict_indices: &[usize],
) -> Result<Vec<u8>> {
    // Validate instances
    if instances.is_empty() {
        return Err(anyhow!("No symbol instances provided for text region"));
    }

    // Validate global dictionary indices
    if global_dict_indices
        .iter()
        .any(|&idx| idx >= all_known_symbols.len())
    {
        return Err(anyhow!("Invalid global dictionary index in text region"));
    }

    // Validate local dictionary indices if provided
    if !local_dict_indices.is_empty() {
        if local_dict_indices
            .iter()
            .any(|&idx| idx >= all_known_symbols.len())
        {
            return Err(anyhow!("Invalid local dictionary index in text region"));
        }
    }

    // Validate each instance
    for (i, instance) in instances.iter().enumerate() {
        if instance.symbol_index >= all_known_symbols.len() {
            return Err(anyhow!(
                "Symbol instance {} references invalid symbol index {} (max {})",
                i,
                instance.symbol_index,
                all_known_symbols.len() - 1
            ));
        }

        let symbol = &all_known_symbols[instance.symbol_index];
        if instance.position.x as u64 + symbol.width as u64 > u32::MAX as u64
            || instance.position.y as u64 + symbol.height as u64 > u32::MAX as u64
        {
            return Err(anyhow!(
                "Symbol instance {} at position ({}, {}) would overflow 32-bit coordinates",
                i,
                instance.position.x,
                instance.position.y
            ));
        }
    }
    let mut payload = Vec::new();
    let mut coder = Jbig2ArithCoder::new();

    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x_coord = 0;
    let mut max_y_coord = 0;

    if instances.is_empty() {
        min_x = 0;
        min_y = 0;
    } else {
        for instance in instances {
            let pos = instance.position();
            let sym_idx_in_all_known_list = instance.symbol_index();
            let symbol_width = all_known_symbols[sym_idx_in_all_known_list].width as i32;
            let symbol_height = all_known_symbols[sym_idx_in_all_known_list].height as i32;

            min_x = min_x.min(pos.x as u32);
            min_y = min_y.min(pos.y as u32);
            max_x_coord = max_x_coord.max((pos.x as i32 + symbol_width) as u32);
            max_y_coord = max_y_coord.max((pos.y as i32 + symbol_height) as u32);
        }
    }

    let region_width = if max_x_coord > min_x {
        max_x_coord - min_x
    } else {
        0
    };
    let region_height = if max_y_coord > min_y {
        max_y_coord - min_y
    } else {
        0
    };

    let params = TextRegionParams {
        width: region_width,
        height: region_height,
        x: min_x,
        y: min_y,
        ds_offset: config.text_ds_offset,
        refine: config.text_refine,
        log_strips: config.text_log_strips,
        ref_corner: config.text_ref_corner,
        transposed: config.text_transposed,
        comb_op: config.text_comb_op,
        refine_template: config.text_refine_template,
    };
    if cfg!(debug_assertions) {
        trace!("encode_text_region: TextRegionParams details: {:?}", params);
    }
    // Write text-region header and number of instances (SBNUMINSTANCES).
    payload.extend(params.to_bytes());
    payload.extend_from_slice(&(instances.len() as u32).to_be_bytes());

    // Number of bits used by IAID symbol coding.
    let num_total_dict_symbols = (global_dict_indices.len() + local_dict_indices.len()) as u32;
    // T.88 §7.4.3.1.7 / §6.5.8.2.3 (SDHUFF=0): arithmetic text-region IAID uses
    // SBSYMCODELEN = ceil(log2(SBNUMSYMS)) with NO floor of 1. The formulas differ
    // only at SBNUMSYMS == 1, where the decoder reads zero ID bits; emitting a
    // spurious 1-bit IAID there desyncs the arithmetic decoder for every later
    // symbol (S-coordinate corruption). encode_iaid is skipped when this is 0.
    let symbol_id_bits = log2up(num_total_dict_symbols.max(1));

    #[derive(Clone, Copy)]
    struct EncodedInstance {
        strip_base: i32,
        x: i32,
        t_offset: i32,
        symbol_id: u32,
        symbol_width: i32,
    }

    let strip_width = 1i32 << params.log_strips.min(3);
    let mut encoded_instances = Vec::with_capacity(instances.len());

    for instance in instances {
        let sym_idx_in_all_known_list = instance.symbol_index();
        let symbol_props = &all_known_symbols[sym_idx_in_all_known_list];
        let symbol_id_to_encode = if let Some(pos_global) = global_dict_indices
            .iter()
            .position(|&idx| idx == sym_idx_in_all_known_list)
        {
            pos_global as u32
        } else if let Some(pos_local) = local_dict_indices
            .iter()
            .position(|&idx| idx == sym_idx_in_all_known_list)
        {
            (global_dict_indices.len() + pos_local) as u32
        } else {
            anyhow::bail!(
                "Symbol instance (index {}) not found in referred dictionaries!",
                sym_idx_in_all_known_list
            );
        };

        // REFCORNER=TOPLEFT (value 1): T is the top of the original bounding box.
        let abs = instance.position();
        let rel_x = abs.x as i32 - min_x as i32;
        let rel_y = abs.y as i32 - min_y as i32;
        let strip_base = (rel_y / strip_width) * strip_width;
        let t_offset = rel_y - strip_base;

        encoded_instances.push(EncodedInstance {
            strip_base,
            x: rel_x,
            t_offset,
            symbol_id: symbol_id_to_encode,
            symbol_width: symbol_props.width as i32,
        });
    }

    // Sort strip-wise (top to bottom), then left to right inside each strip.
    encoded_instances.sort_by_key(|e| (e.strip_base, e.x));

    let mut strip_t = 0i32;
    let mut first_s = 0i32;
    let mut idx = 0usize;

    // §6.4.5 step 1: initial STRIPT value (decoder reads one IADT before the loop)
    coder.encode_integer(IntProc::Iadt, 0)?;

    while idx < encoded_instances.len() {
        let current_strip = encoded_instances[idx].strip_base;
        let delta_t = current_strip - strip_t;
        coder.encode_integer(IntProc::Iadt, delta_t / strip_width)?;
        strip_t = current_strip;

        let mut first_symbol_in_strip = true;
        let mut current_s = 0i32;
        while idx < encoded_instances.len() && encoded_instances[idx].strip_base == current_strip {
            let item = encoded_instances[idx];
            if first_symbol_in_strip {
                let delta_fs = item.x - first_s;
                coder.encode_integer(IntProc::Iafs, delta_fs)?;
                first_s += delta_fs;
                current_s = first_s;
                first_symbol_in_strip = false;
            } else {
                let delta_s = item.x - current_s;
                coder.encode_integer(IntProc::Iads, delta_s)?;
                current_s += delta_s;
            }

            if strip_width > 1 {
                coder.encode_integer(IntProc::Iait, item.t_offset)?;
            }
            if symbol_id_bits > 0 {
                coder.encode_iaid(item.symbol_id, symbol_id_bits as u8)?;
            }
            current_s += item.symbol_width - 1;
            idx += 1;
        }
        coder.encode_oob(IntProc::Iads)?;
    }

    coder.flush(true);
    payload.extend(coder.as_bytes());

    Ok(payload)
}
// ── Union-Find helpers for symbol clustering ──────────────────────────

pub(crate) fn uf_find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]]; // path halving
        i = parent[i];
    }
    i
}

pub(crate) fn uf_union(parent: &mut [usize], rank: &mut [u32], a: usize, b: usize) {
    let ra = uf_find(parent, a);
    let rb = uf_find(parent, b);
    if ra == rb {
        return;
    }
    if rank[ra] < rank[rb] {
        parent[ra] = rb;
    } else if rank[ra] > rank[rb] {
        parent[rb] = ra;
    } else {
        parent[rb] = ra;
        rank[ra] += 1;
    }
}

pub(crate) fn compute_symbol_hash(symbol: &BitImage) -> u32 {
    let w = symbol.width as u32;
    let h = symbol.height as u32;
    (10 * h + 10000 * w) % 10000000
}

pub(crate) fn log2up(v: u32) -> u32 {
    if v == 0 {
        return 0;
    }
    let is_pow_of_2 = (v & (v - 1)) == 0;
    let mut r = 0;
    let mut val = v;
    while val > 1 {
        val >>= 1;
        r += 1;
    }
    r + if is_pow_of_2 { 0 } else { 1 }
}
/// Encodes a single page image using a symbol dictionary.
/// This is a high-level function that demonstrates the new encoding pipeline.
pub fn encode_page_with_symbol_dictionary(
    image: &BitImage,
    config: &Jbig2Config,
    next_segment_num: u32,
) -> Result<(Vec<u8>, u32)> {
    // 1. Extract symbols from the page image using CC analysis
    #[cfg(feature = "symboldict")]
    let extracted_symbols = {
        let dpi = 300; // Default DPI
        let losslevel = if config.is_lossless { 0 } else { 1 };
        let cc_image = analyze_page(image, dpi, losslevel);
        let shapes = cc_image.extract_shapes();
        // Convert to (Rect, BitImage) format
        shapes
            .into_iter()
            .map(|(bitmap, bbox)| {
                let rect = Rect {
                    x: bbox.xmin as u32,
                    y: bbox.ymin as u32,
                    width: bbox.width() as u32,
                    height: bbox.height() as u32,
                };
                (rect, bitmap)
            })
            .collect::<Vec<_>>()
    };
    #[cfg(not(feature = "symboldict"))]
    let extracted_symbols: Vec<(Rect, BitImage)> = Vec::new();

    if extracted_symbols.is_empty() {
        return Ok((Vec::new(), next_segment_num));
    }

    // 2. Build the symbol dictionary and get symbol instances
    let mut comparator = Comparator::default();
    let (dictionary_symbols, text_region_instances) =
        build_dictionary_and_get_instances(&extracted_symbols, &mut comparator);
    debug!(
        "Built dictionary with {} symbols and {} instances",
        dictionary_symbols.len(),
        text_region_instances.len()
    );

    let mut output = Vec::new();
    let mut current_segment_number = next_segment_num;

    // 3. Encode the symbol dictionary segment, getting the final symbol-ID mapping.
    let dict_refs: Vec<&BitImage> = dictionary_symbols.iter().collect();
    let dict_layout = plan_symbol_dictionary_layout(&dict_refs, config, None)?;
    let encoded_dict = encode_symbol_dictionary_segments(&dict_refs, config, &dict_layout)?;
    let dict_segment_number = current_segment_number;
    current_segment_number += 1;
    Segment {
        number: dict_segment_number,
        seg_type: SegmentType::SymbolDictionary,
        referred_to: Vec::new(),
        page: Some(1),
        payload: encoded_dict.payload.clone(),
        ..Default::default()
    }
    .write_into(&mut output)?;

    // 4. Encode the text region segment using canonical symbol IDs.
    let mut symbol_instances: Vec<SymbolInstance> = text_region_instances
        .iter()
        .map(|instance| {
            let orig_id = instance.symbol_id as usize;
            let symbol_bitmap = if orig_id < dictionary_symbols.len() {
                &dictionary_symbols[orig_id]
            } else {
                &dictionary_symbols[0]
            };
            SymbolInstance {
                symbol_index: orig_id,
                position: instance.position(),
                instance_bitmap: symbol_bitmap.clone(),
                needs_refinement: instance.is_refinement,
                refinement_dx: instance.dx,
                refinement_dy: instance.dy,
            }
        })
        .collect();

    for (orig_idx, refinement) in dict_layout.refinements.iter().enumerate() {
        if let Some(refinement) = refinement {
            for instance in &mut symbol_instances {
                if instance.symbol_index == orig_idx {
                    instance.symbol_index = refinement.prototype_input_index;
                    instance.needs_refinement = true;
                    instance.refinement_dx = refinement.refinement_dx;
                    instance.refinement_dy = refinement.refinement_dy;
                }
            }
        }
    }

    let region_payload = if !config.uses_lossy_symbol_dictionary()
        && (config.refine || symbol_instances.iter().any(|inst| inst.needs_refinement))
    {
        encode_text_region_with_refinement(
            &symbol_instances,
            config,
            &dictionary_symbols,
            &encoded_dict.input_to_exported_pos,
            encoded_dict.exported_symbol_count,
            &[],
            0,
        )?
    } else {
        encode_text_region_mapped(
            &symbol_instances,
            config,
            &dictionary_symbols,
            &encoded_dict.input_to_exported_pos,
            encoded_dict.exported_symbol_count,
            &[],
            0,
            0,
        )?
    };

    let region_segment = Segment {
        number: current_segment_number,
        seg_type: SegmentType::ImmediateTextRegion,
        retain_flags: 0,
        referred_to: vec![dict_segment_number],
        page: Some(1), // Assuming page 1
        payload: region_payload,
        ..Default::default()
    };

    // You might want to log text_region_params here too if they are accessible
    region_segment.write_into(&mut output)?;
    current_segment_number += 1;

    Ok((output, current_segment_number))
}
