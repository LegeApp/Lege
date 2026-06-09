use super::text_region::{log2up, symbol_id_from_dense_maps};
use super::types::SymbolInstance;
use crate::jbig2arith::{IntProc, Jbig2ArithCoder};
use crate::jbig2structs::{Jbig2Config, TextRegionParams};
use crate::jbig2sym::BitImage;
use anyhow::{Result, anyhow};

/// Encodes a text region with Soft Pattern Matching (SPM / refinement coding).
///
/// This is the SBREFINE=1 variant of text region encoding. For each symbol instance:
/// - Encode the symbol ID and position (same as non-refinement)
/// - Encode RI (refinement indicator) via IARI
///   - RI=0: direct substitution of the dictionary symbol (no refinement)
///   - RI=1: encode size deltas (IARDW, IARDH), position offsets (IARDX, IARDY),
///     then a pixel-by-pixel refinement region using the dictionary symbol as reference
///
/// This allows lossy symbol clustering (small dictionary) while preserving
/// per-instance fidelity through the refinement residual.
pub fn encode_text_region_with_refinement(
    instances: &[SymbolInstance],
    config: &Jbig2Config,
    all_symbols: &[BitImage],
    global_sym_to_dict_pos: &[u32],
    num_global_dict_symbols: u32,
    local_sym_to_dict_pos: &[u32],
    num_local_dict_symbols: u32,
) -> Result<Vec<u8>> {
    if instances.is_empty() {
        return Err(anyhow!("No symbol instances provided for text region"));
    }

    let num_total_dict_symbols = num_global_dict_symbols + num_local_dict_symbols;

    let mut payload = Vec::new();
    let mut coder = Jbig2ArithCoder::new();

    // Compute region bounds. For refined instances, use the actual instance
    // bitmap size (which may be larger than the prototype) so the region is
    // large enough to hold the refined glyphs.
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x_coord = 0u32;
    let mut max_y_coord = 0u32;

    for instance in instances {
        let (w, h) = if instance.needs_refinement {
            let (_, trimmed) = instance.instance_bitmap.trim();
            (trimmed.width as u32, trimmed.height as u32)
        } else {
            let sym = &all_symbols[instance.symbol_index];
            (sym.width as u32, sym.height as u32)
        };

        min_x = min_x.min(instance.position.x);
        min_y = min_y.min(instance.position.y);
        max_x_coord = max_x_coord.max(instance.position.x + w);
        max_y_coord = max_y_coord.max(instance.position.y + h);
    }

    let region_width = max_x_coord.saturating_sub(min_x);
    let region_height = max_y_coord.saturating_sub(min_y);

    // SBREFINE=1 in the text region params
    let params = TextRegionParams {
        width: region_width,
        height: region_height,
        x: min_x,
        y: min_y,
        ds_offset: config.text_ds_offset,
        refine: true, // SBREFINE = 1
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

    // Prepare instances with dictionary mapping (same structure as non-refinement)
    #[derive(Clone)]
    struct RefinedInstance {
        strip_base: i32,
        x: i32,
        t_offset: i32,
        symbol_id: u32,
        symbol_width: i32,
        // Refinement data
        needs_refinement: bool,
        /// Index into original instances array (for accessing instance_bitmap)
        orig_idx: usize,
    }

    let strip_width = 1i32 << params.log_strips.min(3);
    let mut encoded_instances = Vec::with_capacity(instances.len());

    for (orig_idx, instance) in instances.iter().enumerate() {
        let gs_idx = instance.symbol_index;
        let sym = &all_symbols[gs_idx];

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

        encoded_instances.push(RefinedInstance {
            strip_base,
            x: rel_x,
            t_offset,
            symbol_id,
            symbol_width: sym.width as i32,
            needs_refinement: instance.needs_refinement,
            orig_idx,
        });
    }

    encoded_instances.sort_by_key(|e| (e.strip_base, e.x));

    // Encode strip-by-strip, symbol-by-symbol (same loop structure as non-refinement)
    let mut strip_t = 0i32;
    let mut first_s = 0i32;
    let mut idx = 0usize;

    // Default refinement AT pixel: (-1, -1), matching jbig2enc convention
    let grat: [(i8, i8); 1] = [(-1, -1)];

    // §6.4.5 step 1: initial STRIPT value (decoder reads one IADT before the loop)
    let _ = coder.encode_integer(IntProc::Iadt, 0);

    while idx < encoded_instances.len() {
        let current_strip = encoded_instances[idx].strip_base;
        let delta_t = current_strip - strip_t;
        let _ = coder.encode_integer(IntProc::Iadt, delta_t / strip_width);
        strip_t = current_strip;

        let mut first_symbol_in_strip = true;
        let mut current_s = 0i32;

        while idx < encoded_instances.len() && encoded_instances[idx].strip_base == current_strip {
            let item = &encoded_instances[idx];
            if first_symbol_in_strip {
                let delta_fs = item.x - first_s;
                let _ = coder.encode_integer(IntProc::Iafs, delta_fs);
                first_s += delta_fs;
                current_s = first_s;
                first_symbol_in_strip = false;
            } else {
                let delta_s = item.x - current_s;
                let _ = coder.encode_integer(IntProc::Iads, delta_s);
                current_s += delta_s;
            }

            if strip_width > 1 {
                let _ = coder.encode_integer(IntProc::Iait, item.t_offset);
            }

            // Symbol ID
            if symbol_id_bits > 0 {
                let _ = coder.encode_iaid(item.symbol_id, symbol_id_bits as u8);
            }

            // ── SPM: Refinement indicator (RI) ──
            let ri = if item.needs_refinement { 1i32 } else { 0i32 };
            let _ = coder.encode_integer(IntProc::Iari, ri);

            if item.needs_refinement {
                // Get the original instance data and the prototype
                let orig_instance = &instances[item.orig_idx];
                let prototype = &all_symbols[orig_instance.symbol_index];

                // Trim the instance bitmap to get the actual glyph
                let (_, trimmed_instance) = orig_instance.instance_bitmap.trim();

                // Size deltas: how much wider/taller is the instance vs prototype
                let rdwi = trimmed_instance.width as i32 - prototype.width as i32;
                let rdhi = trimmed_instance.height as i32 - prototype.height as i32;

                let _ = coder.encode_integer(IntProc::Iardw, rdwi);
                let _ = coder.encode_integer(IntProc::Iardh, rdhi);

                // Position offsets for aligning the reference within the target.
                // Per §6.4.11.3.2: GRDX = (RDWI/2) + RDXI, GRDY = (RDHI/2) + RDYI
                // Use the pre-computed alignment offsets from clustering
                let rdxi = orig_instance.refinement_dx;
                let rdyi = orig_instance.refinement_dy;

                let _ = coder.encode_integer(IntProc::Iardx, rdxi);
                let _ = coder.encode_integer(IntProc::Iardy, rdyi);

                // Compute GRDX/GRDY for the refinement region.
                // §6.4.11.3.2 / §6.3: GRREFERENCEDX = floor(RDW/2) + RDX. Rust's `/`
                // truncates toward zero, which differs from floor for negative RDW
                // (instance narrower/shorter than the prototype) and would shift the
                // reference by one pixel vs the decoder. Use floor division.
                let grdx = rdwi.div_euclid(2) + rdxi;
                let grdy = rdhi.div_euclid(2) + rdyi;

                // Encode the refinement region: pixel-by-pixel difference
                // between the trimmed instance and the prototype
                coder.encode_refinement_region(
                    &trimmed_instance,
                    prototype,
                    grdx,
                    grdy,
                    config.text_refine_template,
                    &grat,
                )?;

                // Do NOT reset the refinement (GR) contexts between symbol
                // instances. T.88 §6.4.11 codes each instance's refinement with the
                // generic refinement procedure using the text region's shared GR
                // statistics; jbig2dec carries them across instances, so resetting
                // here desynchronises every refinement after the first.
            }

            current_s += item.symbol_width - 1;
            idx += 1;
        }
        let _ = coder.encode_oob(IntProc::Iads);
    }

    coder.flush(true);
    payload.extend(coder.as_bytes());

    Ok(payload)
}
