//! Arithmetic symbol-dictionary decoding (jbig2decplan.md §16, T.88 §6.5/§7.4.2).
//!
//! This inverts the encoder's default dictionary path
//! ([`crate::encode::document::symbol::dictionary::encode_symbol_dict_with_order`]):
//! arithmetic coding, template 0, `SDHUFF = 0`, `SDREFAGG = 0`, symbols sorted
//! by height then width, direct-coded bitmaps, run-length export flags. Every
//! new symbol bitmap is generic-decoded (template 0) from the *same* arithmetic
//! stream and generic-context bank, so contexts carry across symbols exactly as
//! the encoder's single coder does.
//!
//! Huffman, refinement/aggregate, and non-zero templates surface as typed
//! `Unsupported` errors (Phases 3/5). Limits are checked before every symbol
//! allocation and against the running dictionary pixel total.

use std::sync::Arc;

use crate::decode::arith::ArithmeticDecoder;
use crate::decode::error::{DecodeError, LimitError, usize_from_u32};
use crate::decode::generic::{GenericScratch, decode_generic_bitmap};
use crate::decode::huffman::{BitReader, HuffmanTable, HuffmanValue, standard_table};
use crate::decode::iaid::IaidContexts;
use crate::decode::integer::{DecodedInteger, IntegerContexts};
use crate::decode::mmr::decode_mmr_bitmap;
use crate::decode::refinement::{REFINEMENT_CONTEXT_COUNT, decode_refinement_region_templated};
use crate::decode::text_region::{
    HuffTextParams, HuffTextTables, TextArithParams, decode_huffman_text_core,
    decode_text_region_arith,
};
use crate::shared::bitmap::MonoBitmap;
use crate::shared::int_proc::IntProc;
use crate::shared::limits::DecodeLimits;
use crate::shared::mq_table::MqContext;
use crate::shared::reader::Reader;

/// `ceil(log2(v))` for symbol-ID code lengths (0 for `v <= 1`).
#[inline]
fn ceil_log2(v: u32) -> u32 {
    if v <= 1 {
        0
    } else if v.is_power_of_two() {
        v.trailing_zeros()
    } else {
        32 - (v - 1).leading_zeros()
    }
}

/// A decoded symbol dictionary: the exported symbols, shareable across pages.
pub struct SymbolDictionary {
    /// Exported symbols in export order (jbig2decplan.md §16).
    pub exported_symbols: Box<[Arc<MonoBitmap>]>,
}

impl SymbolDictionary {
    /// The exported symbols.
    #[inline]
    pub fn symbols(&self) -> &[Arc<MonoBitmap>] {
        &self.exported_symbols
    }
}

/// Decode an arithmetic symbol-dictionary segment payload (T.88 §7.4.2).
///
/// `imported` holds the exported symbols of every referred dictionary, in
/// reference order; they precede this dictionary's new symbols in the combined
/// symbol space that the export flags select from.
///
/// `int_ctx` and `generic_ctx` are reused, worker-local scratch: both are reset
/// here (a fresh dictionary starts every context at zero, matching the
/// encoder's per-segment coder). `generic_ctx` must be at least `1 << 16`
/// entries.
#[allow(clippy::too_many_arguments)]
pub fn decode_symbol_dictionary(
    payload: &[u8],
    imported: &[Arc<MonoBitmap>],
    huffman_tables: &[Arc<HuffmanTable>],
    limits: &DecodeLimits,
    int_ctx: &mut IntegerContexts,
    iaid_ctx: &mut IaidContexts,
    generic_ctx: &mut [MqContext],
    refine_ctx: &mut [MqContext],
    scratch: &mut GenericScratch,
    reset_bitmap_ctx: bool,
) -> Result<SymbolDictionary, DecodeError> {
    let mut r = Reader::new(payload);

    // §7.4.2.1.1 symbol dictionary flags (16-bit).
    let flags = r.read_u16_be()?;
    let sdhuff = flags & 0x0001 != 0;
    let sdrefagg = flags & 0x0002 != 0;
    let sdtemplate = ((flags >> 10) & 0x0003) as u8;
    let sdrtemplate = ((flags >> 12) & 0x0001) as u8;
    // §7.4.2.1.2 SDAT: present only when SDHUFF=0. Template 0 carries 4 adaptive
    // pixels; templates 1–3 carry a single adaptive pixel.
    let mut at = [(0i8, 0i8); 4];
    if !sdhuff {
        let at_count = if sdtemplate == 0 { 4 } else { 1 };
        for slot in at.iter_mut().take(at_count) {
            let ax = r.read_i8()?;
            let ay = r.read_i8()?;
            *slot = (ax, ay);
        }
    }
    // §7.4.2.1.3 SDRAT: refinement AT, present only when SDREFAGG=1 and
    // SDRTEMPLATE=0. GRTEMPLATE-0 uses two AT pairs (4 bytes): SDRAT1 is the
    // target adaptive pixel, SDRAT2 the reference adaptive pixel (T.88 Fig. 12,
    // context bit 8). Both must flow into the refinement context or a custom-AT
    // symbol desyncs (pdf.js bitmap-symbol-symbolrefineone-customat).
    let mut sdrat: (i8, i8) = (-1, -1);
    let mut sdrat2: (i8, i8) = (-1, -1);
    if sdrefagg && sdrtemplate == 0 {
        let x = r.read_i8()?;
        let y = r.read_i8()?;
        let x2 = r.read_i8()?;
        let y2 = r.read_i8()?;
        sdrat = (x, y);
        sdrat2 = (x2, y2);
    }

    // §7.4.2.1.5/.6 exported and new symbol counts.
    let num_ex = r.read_u32_be()?;
    let num_new = r.read_u32_be()?;
    let num_new = usize_from_u32(num_new, "SDNUMNEWSYMS")?;
    let num_ex_usize = usize_from_u32(num_ex, "SDNUMEXSYMS")?;

    if num_new > limits.max_symbols {
        return Err(DecodeError::limit(LimitError::Count {
            what: "new symbols",
            value: num_new as u64,
            limit: limits.max_symbols as u64,
        }));
    }
    let total = imported
        .len()
        .checked_add(num_new)
        .ok_or(DecodeError::Overflow {
            operation: "dictionary symbol total",
        })?;
    if total > limits.max_symbols {
        return Err(DecodeError::limit(LimitError::Count {
            what: "dictionary symbols",
            value: total as u64,
            limit: limits.max_symbols as u64,
        }));
    }

    let data = &payload[r.position()..];

    if sdhuff && sdrefagg {
        // Huffman refinement/aggregate dictionary (Figure 25 layout with
        // Huffman-coded deltas and byte-aligned arithmetic refinement blocks).
        return decode_symbol_dictionary_huffman_refagg(
            data,
            flags,
            imported,
            huffman_tables,
            refine_ctx,
            num_new,
            num_ex_usize,
            total,
            sdrtemplate,
            sdrat,
            sdrat2,
            limits,
        );
    }
    if sdhuff {
        return decode_symbol_dictionary_huffman(
            data,
            flags,
            imported,
            huffman_tables,
            num_new,
            num_ex_usize,
            total,
            limits,
        );
    }

    // Integer/IAID statistics always start fresh; the generic and refinement
    // (bitmap-coding) statistics reset only when not importing retained
    // contexts (T.88 §6.5.5 steps 3/4, the "bitmap coding context used" flag).
    int_ctx.reset();
    if reset_bitmap_ctx {
        for c in generic_ctx.iter_mut() {
            *c = MqContext(0);
        }
    }
    // SBSYMCODELEN for the refinement symbol-ID (§6.5.8.2.3).
    let code_len = ceil_log2(total as u32) as u8;
    if sdrefagg {
        iaid_ctx.reset_for_bits(code_len)?;
        if refine_ctx.len() < REFINEMENT_CONTEXT_COUNT {
            return Err(DecodeError::Overflow {
                operation: "refinement context array too small",
            });
        }
        if reset_bitmap_ctx {
            for c in refine_ctx.iter_mut() {
                *c = MqContext(0);
            }
        }
    }

    let mut dec = ArithmeticDecoder::new(data);

    let mut new_symbols: Vec<Arc<MonoBitmap>> = Vec::with_capacity(num_new.min(4096));
    let mut hc_height: i64 = 0;
    let mut total_pixels: u64 = 0;

    // §6.5.5 height-class loop.
    while new_symbols.len() < num_new {
        let dh = match int_ctx.decode(&mut dec, IntProc::Iadh) {
            DecodedInteger::Value(v) => v as i64,
            DecodedInteger::OutOfBand => {
                return Err(DecodeError::Malformed {
                    reason: "OOB where height-class delta expected",
                });
            }
        };
        hc_height = hc_height.checked_add(dh).ok_or(DecodeError::Overflow {
            operation: "height class height",
        })?;
        if hc_height <= 0 || hc_height > limits.max_height as i64 {
            return Err(DecodeError::Malformed {
                reason: "non-positive or oversized height class",
            });
        }

        let mut sym_width: i64 = 0;
        // §6.5.5 4) width loop within the class, terminated by OOB(IADW).
        loop {
            match int_ctx.decode(&mut dec, IntProc::Iadw) {
                DecodedInteger::OutOfBand => break,
                DecodedInteger::Value(v) => {
                    sym_width = sym_width
                        .checked_add(v as i64)
                        .ok_or(DecodeError::Overflow {
                            operation: "symbol width",
                        })?;
                }
            }
            if sym_width <= 0 || sym_width > limits.max_width as i64 {
                return Err(DecodeError::Malformed {
                    reason: "non-positive or oversized symbol width",
                });
            }
            if new_symbols.len() >= num_new {
                return Err(DecodeError::Malformed {
                    reason: "more symbols coded than SDNUMNEWSYMS",
                });
            }

            let px =
                (sym_width as u64)
                    .checked_mul(hc_height as u64)
                    .ok_or(DecodeError::Overflow {
                        operation: "symbol pixel count",
                    })?;
            if px > limits.max_symbol_pixels {
                return Err(DecodeError::limit(LimitError::Pixels {
                    what: "symbol",
                    value: px,
                    limit: limits.max_symbol_pixels,
                }));
            }
            total_pixels = total_pixels.checked_add(px).ok_or(DecodeError::Overflow {
                operation: "dictionary pixel total",
            })?;
            if total_pixels > limits.max_total_dictionary_pixels {
                return Err(DecodeError::limit(LimitError::Pixels {
                    what: "dictionary",
                    value: total_pixels,
                    limit: limits.max_total_dictionary_pixels,
                }));
            }

            let bitmap = if sdrefagg {
                decode_refagg_symbol(
                    &mut dec,
                    int_ctx,
                    iaid_ctx,
                    refine_ctx,
                    imported,
                    &new_symbols,
                    code_len,
                    sdrtemplate,
                    sdrat,
                    sdrat2,
                    sym_width as u32,
                    hc_height as u32,
                    limits,
                )?
            } else {
                decode_generic_bitmap(
                    &mut dec,
                    sym_width as u32,
                    hc_height as u32,
                    sdtemplate,
                    at,
                    generic_ctx,
                    limits,
                    scratch,
                )?
            };
            new_symbols.push(Arc::new(bitmap));
        }
    }

    // §6.5.10 export flags: alternating run lengths over [imported .. new].
    let exported = decode_export_flags(
        &mut dec,
        int_ctx,
        imported,
        &new_symbols,
        total,
        num_ex_usize,
    )?;

    Ok(SymbolDictionary {
        exported_symbols: exported.into_boxed_slice(),
    })
}

/// Decode one refinement/aggregate-coded new symbol (SDREFAGG=1, arithmetic,
/// T.88 §6.5.8.2). Only the REFAGGNINST=1 fast path (§6.5.8.2.2) is implemented;
/// true aggregates (REFAGGNINST>1, an internal text region) are rejected.
#[allow(clippy::too_many_arguments)]
fn decode_refagg_symbol(
    dec: &mut ArithmeticDecoder<'_>,
    int_ctx: &mut IntegerContexts,
    iaid_ctx: &mut IaidContexts,
    refine_ctx: &mut [MqContext],
    imported: &[Arc<MonoBitmap>],
    new_symbols: &[Arc<MonoBitmap>],
    code_len: u8,
    sdrtemplate: u8,
    sdrat: (i8, i8),
    sdrat2: (i8, i8),
    sym_width: u32,
    hc_height: u32,
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    // §6.5.8.2.1 number of instances in the aggregation.
    let refagg_ninst = match int_ctx.decode(dec, IntProc::Iaai) {
        DecodedInteger::Value(v) if v >= 0 => v,
        _ => {
            return Err(DecodeError::Malformed {
                reason: "invalid REFAGGNINST",
            });
        }
    };
    if refagg_ninst > 1 {
        // §6.5.8.2 step 2: a true aggregate is decoded as an internal text
        // region (Table 17) over SBSYMS = imported ++ already-decoded new
        // symbols, sharing this dictionary's arithmetic decoder and contexts.
        let mut combined: Vec<Arc<MonoBitmap>> =
            Vec::with_capacity(imported.len() + new_symbols.len());
        combined.extend(imported.iter().cloned());
        combined.extend(new_symbols.iter().cloned());
        let params = TextArithParams {
            width: sym_width,
            height: hc_height,
            num_instances: refagg_ninst as u32,
            log_strips: 0, // SBSTRIPS = 1
            ref_corner: 1, // TOPLEFT
            transposed: false,
            sb_comb_op: 0, // OR
            sb_def_pixel: false,
            sb_ds_offset: 0,
            sbrefine: true,
            sb_rtemplate: sdrtemplate,
            grat: sdrat,
            grat2: sdrat2,
            code_len,
        };
        return decode_text_region_arith(
            dec, &params, &combined, int_ctx, iaid_ctx, refine_ctx, limits,
        );
    }

    // §6.5.8.2.2: symbol ID, then refinement offsets, then refine the reference.
    let id = iaid_ctx.decode(dec, code_len) as usize;
    let rdx = match int_ctx.decode(dec, IntProc::Iardx) {
        DecodedInteger::Value(v) => v,
        DecodedInteger::OutOfBand => {
            return Err(DecodeError::Malformed {
                reason: "OOB for RDX",
            });
        }
    };
    let rdy = match int_ctx.decode(dec, IntProc::Iardy) {
        DecodedInteger::Value(v) => v,
        DecodedInteger::OutOfBand => {
            return Err(DecodeError::Malformed {
                reason: "OOB for RDY",
            });
        }
    };

    // IBOI = SBSYMS[IDI] = imported ++ already-decoded new symbols.
    let reference = if id < imported.len() {
        &imported[id]
    } else if id - imported.len() < new_symbols.len() {
        &new_symbols[id - imported.len()]
    } else {
        return Err(DecodeError::Malformed {
            reason: "refinement symbol id out of range",
        });
    };

    decode_refinement_region_templated(
        dec,
        reference,
        sym_width,
        hc_height,
        rdx,
        rdy,
        sdrtemplate,
        false,
        sdrat,
        sdrat2,
        refine_ctx,
        limits,
    )
}

/// Select the standard or custom Huffman table for a two-bit selection field.
/// `custom` is an iterator over the referred custom tables, consumed in field
/// order (T.88 §7.4.3.1.6 — the same rule applies to the symbol dictionary).
fn select_table<'a, I>(
    selection: u32,
    std_a: u8,
    std_b: u8,
    custom: &mut I,
) -> Result<TableRef, DecodeError>
where
    I: Iterator<Item = &'a Arc<HuffmanTable>>,
{
    match selection {
        0 => Ok(TableRef::Owned(standard_table(std_a)?)),
        1 => Ok(TableRef::Owned(standard_table(std_b)?)),
        3 => custom
            .next()
            .map(|t| TableRef::Shared(t.clone()))
            .ok_or(DecodeError::Malformed {
                reason: "custom Huffman table referenced but not supplied",
            }),
        _ => Err(DecodeError::Malformed {
            reason: "reserved Huffman table selection (value 2)",
        }),
    }
}

/// A one-bit selection (standard table B.1, or custom).
fn select_table_bit<'a, I>(bit: bool, custom: &mut I) -> Result<TableRef, DecodeError>
where
    I: Iterator<Item = &'a Arc<HuffmanTable>>,
{
    if bit {
        custom
            .next()
            .map(|t| TableRef::Shared(t.clone()))
            .ok_or(DecodeError::Malformed {
                reason: "custom Huffman table referenced but not supplied",
            })
    } else {
        Ok(TableRef::Owned(standard_table(1)?))
    }
}

/// Either an owned standard table or a shared referred custom table.
enum TableRef {
    Owned(HuffmanTable),
    Shared(Arc<HuffmanTable>),
}

impl TableRef {
    #[inline]
    fn get(&self) -> &HuffmanTable {
        match self {
            TableRef::Owned(t) => t,
            TableRef::Shared(t) => t,
        }
    }
}

/// Decode a Huffman refinement/aggregate symbol dictionary (SDHUFF=1,
/// SDREFAGG=1; T.88 §6.5 Figure 25 with §6.5.8.2). Height-class delta heights
/// and symbol delta widths are Huffman-coded; each new symbol is a refinement
/// (REFAGGNINST=1, §6.5.8.2.2) or an aggregate text region (REFAGGNINST>1,
/// Table 17) whose refinement bitmaps are byte-aligned arithmetic blocks.
#[allow(clippy::too_many_arguments)]
fn decode_symbol_dictionary_huffman_refagg(
    data: &[u8],
    flags: u16,
    imported: &[Arc<MonoBitmap>],
    huffman_tables: &[Arc<HuffmanTable>],
    refine_ctx: &mut [MqContext],
    num_new: usize,
    num_ex: usize,
    total: usize,
    sdrtemplate: u8,
    sdrat: (i8, i8),
    sdrat2: (i8, i8),
    limits: &DecodeLimits,
) -> Result<SymbolDictionary, DecodeError> {
    // §7.4.2.1.6 table selection, custom tables consumed in field order:
    // SDHUFFDH, SDHUFFDW, SDHUFFBMSIZE (unused here but still consumed),
    // SDHUFFAGGINST.
    let mut custom = huffman_tables.iter();
    let dh_sel = ((flags >> 2) & 0x0003) as u32;
    let dw_sel = ((flags >> 4) & 0x0003) as u32;
    let bmsize_sel = (flags >> 6) & 0x0001 != 0;
    let agg_sel = (flags >> 7) & 0x0001 != 0;
    let dh_table = select_table(dh_sel, 4, 5, &mut custom)?;
    let dw_table = select_table(dw_sel, 2, 3, &mut custom)?;
    let _bmsize_table = select_table_bit(bmsize_sel, &mut custom)?;
    let agg_table = select_table_bit(agg_sel, &mut custom)?;

    // Standard tables for the refinement offsets/size and the aggregate text
    // region (§6.5.8.2.2, Table 17): RDX/RDY = B.15, RSIZE = B.1, FS = B.6,
    // DS = B.8, DT = B.11, RDW/RDH = B.15.
    let b1 = standard_table(1)?;
    let b6 = standard_table(6)?;
    let b8 = standard_table(8)?;
    let b11 = standard_table(11)?;
    let b15 = standard_table(15)?;

    // §6.5.8.2.3: SBSYMCODES are equal-length codes, length
    // max(ceil(log2(SDNUMINSYMS + SDNUMNEWSYMS)), 1), code[I] = I.
    let code_len = ceil_log2(total as u32).max(1);
    let sym_table = HuffmanTable::from_code_lengths(&vec![code_len; total])?;

    let mut r = BitReader::new(data);
    let mut new_symbols: Vec<Arc<MonoBitmap>> = Vec::with_capacity(num_new.min(4096));
    let mut hc_height: i64 = 0;
    let mut total_pixels: u64 = 0;

    while new_symbols.len() < num_new {
        let hcdh = match dh_table.get().decode(&mut r)? {
            HuffmanValue::Value(v) => v as i64,
            HuffmanValue::Oob => {
                return Err(DecodeError::Malformed {
                    reason: "OOB where height-class delta expected",
                });
            }
        };
        hc_height = hc_height.checked_add(hcdh).ok_or(DecodeError::Overflow {
            operation: "height class height",
        })?;
        if hc_height <= 0 || hc_height > limits.max_height as i64 {
            return Err(DecodeError::Malformed {
                reason: "non-positive or oversized height class",
            });
        }

        let mut sym_width: i64 = 0;
        loop {
            match dw_table.get().decode(&mut r)? {
                HuffmanValue::Oob => break,
                HuffmanValue::Value(dw) => {
                    sym_width = sym_width
                        .checked_add(dw as i64)
                        .ok_or(DecodeError::Overflow {
                            operation: "symbol width",
                        })?;
                }
            }
            if sym_width <= 0 || sym_width > limits.max_width as i64 {
                return Err(DecodeError::Malformed {
                    reason: "non-positive or oversized symbol width",
                });
            }
            if new_symbols.len() >= num_new {
                return Err(DecodeError::Malformed {
                    reason: "more symbols coded than SDNUMNEWSYMS",
                });
            }
            let px =
                (sym_width as u64)
                    .checked_mul(hc_height as u64)
                    .ok_or(DecodeError::Overflow {
                        operation: "symbol pixel count",
                    })?;
            if px > limits.max_symbol_pixels {
                return Err(DecodeError::limit(LimitError::Pixels {
                    what: "symbol",
                    value: px,
                    limit: limits.max_symbol_pixels,
                }));
            }
            total_pixels = total_pixels.checked_add(px).ok_or(DecodeError::Overflow {
                operation: "dictionary pixel total",
            })?;
            if total_pixels > limits.max_total_dictionary_pixels {
                return Err(DecodeError::limit(LimitError::Pixels {
                    what: "dictionary",
                    value: total_pixels,
                    limit: limits.max_total_dictionary_pixels,
                }));
            }

            // §6.5.8.2.1 number of instances in the aggregation.
            let refagg_ninst = match agg_table.get().decode(&mut r)? {
                HuffmanValue::Value(v) if v >= 0 => v,
                _ => {
                    return Err(DecodeError::Malformed {
                        reason: "invalid REFAGGNINST",
                    });
                }
            };

            let bitmap = if refagg_ninst == 1 {
                // §6.5.8.2.2 with SBHUFF=1: symbol id, RDX/RDY (B.15), BMSIZE
                // (B.1), byte-align, arithmetic refinement block, byte-align.
                let id = match sym_table.decode(&mut r)? {
                    HuffmanValue::Value(v) => v as usize,
                    HuffmanValue::Oob => {
                        return Err(DecodeError::Malformed {
                            reason: "OOB decoding refinement symbol id",
                        });
                    }
                };
                let rdx = match b15.decode(&mut r)? {
                    HuffmanValue::Value(v) => v,
                    HuffmanValue::Oob => {
                        return Err(DecodeError::Malformed {
                            reason: "OOB for RDX",
                        });
                    }
                };
                let rdy = match b15.decode(&mut r)? {
                    HuffmanValue::Value(v) => v,
                    HuffmanValue::Oob => {
                        return Err(DecodeError::Malformed {
                            reason: "OOB for RDY",
                        });
                    }
                };
                let bmsize = match b1.decode(&mut r)? {
                    HuffmanValue::Value(v) if v >= 0 => v as usize,
                    _ => {
                        return Err(DecodeError::Malformed {
                            reason: "invalid refinement bitmap size",
                        });
                    }
                };
                r.align_to_byte();
                let reference = if id < imported.len() {
                    &imported[id]
                } else if id - imported.len() < new_symbols.len() {
                    &new_symbols[id - imported.len()]
                } else {
                    return Err(DecodeError::Malformed {
                        reason: "refinement symbol id out of range",
                    });
                };
                for c in refine_ctx.iter_mut() {
                    *c = MqContext(0);
                }
                let refine_bytes = r.remaining_from_byte();
                let block = if bmsize > 0 && bmsize <= refine_bytes.len() {
                    &refine_bytes[..bmsize]
                } else {
                    refine_bytes
                };
                let mut rdec = ArithmeticDecoder::new(block);
                let refined = decode_refinement_region_templated(
                    &mut rdec,
                    reference,
                    sym_width as u32,
                    hc_height as u32,
                    rdx,
                    rdy,
                    sdrtemplate,
                    false,
                    sdrat,
                    sdrat2,
                    refine_ctx,
                    limits,
                )?;
                for _ in 0..bmsize {
                    let _ = r.read_bits(8);
                }
                r.align_to_byte();
                refined
            } else if refagg_ninst > 1 {
                // §6.5.8.2 step 2 / Table 17: an internal Huffman text region.
                let mut combined: Vec<Arc<MonoBitmap>> =
                    Vec::with_capacity(imported.len() + new_symbols.len());
                combined.extend(imported.iter().cloned());
                combined.extend(new_symbols.iter().cloned());
                let tables = HuffTextTables {
                    sym: &sym_table,
                    fs: &b6,
                    ds: &b8,
                    dt: &b11,
                    rdw: &b15,
                    rdh: &b15,
                    rdx: &b15,
                    rdy: &b15,
                    rsize: &b1,
                };
                let params = HuffTextParams {
                    width: sym_width as u32,
                    height: hc_height as u32,
                    num_instances: refagg_ninst as u32,
                    log_strips: 0,
                    ref_corner: 1, // TOPLEFT
                    transposed: false,
                    sb_comb_op: 0, // OR
                    sb_def_pixel: false,
                    sb_ds_offset: 0,
                    sbrefine: true,
                    sb_rtemplate: sdrtemplate,
                    grat: sdrat,
                    grat2: sdrat2,
                };
                decode_huffman_text_core(&mut r, &tables, &params, &combined, refine_ctx, limits)?
            } else {
                return Err(DecodeError::Malformed {
                    reason: "REFAGGNINST is zero",
                });
            };
            new_symbols.push(Arc::new(bitmap));
        }
    }

    let export_table = standard_table(1)?;
    let exported =
        export_symbols_huffman(&mut r, &export_table, imported, &new_symbols, total, num_ex)?;
    Ok(SymbolDictionary {
        exported_symbols: exported.into_boxed_slice(),
    })
}

/// Decode a Huffman-coded symbol dictionary (SDHUFF=1, SDREFAGG=0; T.88 §6.5
/// with Figure 24 height-class layout).
#[allow(clippy::too_many_arguments)]
fn decode_symbol_dictionary_huffman(
    data: &[u8],
    flags: u16,
    imported: &[Arc<MonoBitmap>],
    huffman_tables: &[Arc<HuffmanTable>],
    num_new: usize,
    num_ex: usize,
    total: usize,
    limits: &DecodeLimits,
) -> Result<SymbolDictionary, DecodeError> {
    // §7.4.2.1.1 table selection fields, resolved to concrete tables. Custom
    // tables are consumed from the referred list in field order.
    let mut custom = huffman_tables.iter();
    let dh_sel = ((flags >> 2) & 0x0003) as u32;
    let dw_sel = ((flags >> 4) & 0x0003) as u32;
    let bmsize_sel = (flags >> 6) & 0x0001 != 0;
    let agg_sel = (flags >> 7) & 0x0001 != 0;
    let dh_table = select_table(dh_sel, 4, 5, &mut custom)?;
    let dw_table = select_table(dw_sel, 2, 3, &mut custom)?;
    let bmsize_table = select_table_bit(bmsize_sel, &mut custom)?;
    // SDHUFFAGGINST is unused when SDREFAGG=0, but its custom table (if any) is
    // still consumed from the referred list.
    let _agg_table = select_table_bit(agg_sel, &mut custom)?;

    let export_table = standard_table(1)?; // §6.5.10 export runs use Table B.1.

    let mut r = BitReader::new(data);
    let mut new_symbols: Vec<Arc<MonoBitmap>> = Vec::with_capacity(num_new.min(4096));
    let mut widths: Vec<u32> = Vec::with_capacity(num_new.min(4096));
    let mut hc_height: i64 = 0;
    let mut total_pixels: u64 = 0;

    // §6.5.5 height-class loop.
    while new_symbols.len() < num_new {
        let hcdh = match dh_table.get().decode(&mut r)? {
            HuffmanValue::Value(v) => v as i64,
            HuffmanValue::Oob => {
                return Err(DecodeError::Malformed {
                    reason: "OOB where height-class delta expected",
                });
            }
        };
        hc_height = hc_height.checked_add(hcdh).ok_or(DecodeError::Overflow {
            operation: "height class height",
        })?;
        if hc_height <= 0 || hc_height > limits.max_height as i64 {
            return Err(DecodeError::Malformed {
                reason: "non-positive or oversized height class",
            });
        }

        let hc_first = new_symbols.len();
        let mut sym_width: i64 = 0;
        let mut tot_width: i64 = 0;
        // §6.5.5 4c) width loop, terminated by OOB(DW).
        loop {
            match dw_table.get().decode(&mut r)? {
                HuffmanValue::Oob => break,
                HuffmanValue::Value(dw) => {
                    sym_width = sym_width
                        .checked_add(dw as i64)
                        .ok_or(DecodeError::Overflow {
                            operation: "symbol width",
                        })?;
                }
            }
            if sym_width <= 0 || sym_width > limits.max_width as i64 {
                return Err(DecodeError::Malformed {
                    reason: "non-positive or oversized symbol width",
                });
            }
            if new_symbols.len() >= num_new {
                return Err(DecodeError::Malformed {
                    reason: "more symbols coded than SDNUMNEWSYMS",
                });
            }
            tot_width = tot_width
                .checked_add(sym_width)
                .ok_or(DecodeError::Overflow {
                    operation: "height class total width",
                })?;
            let px =
                (sym_width as u64)
                    .checked_mul(hc_height as u64)
                    .ok_or(DecodeError::Overflow {
                        operation: "symbol pixel count",
                    })?;
            if px > limits.max_symbol_pixels {
                return Err(DecodeError::limit(LimitError::Pixels {
                    what: "symbol",
                    value: px,
                    limit: limits.max_symbol_pixels,
                }));
            }
            total_pixels = total_pixels.checked_add(px).ok_or(DecodeError::Overflow {
                operation: "dictionary pixel total",
            })?;
            if total_pixels > limits.max_total_dictionary_pixels {
                return Err(DecodeError::limit(LimitError::Pixels {
                    what: "dictionary",
                    value: total_pixels,
                    limit: limits.max_total_dictionary_pixels,
                }));
            }
            // Reserve a slot; the bitmap is filled from the collective bitmap.
            new_symbols.push(Arc::new(MonoBitmap::new(1, 1, false, limits)?));
            widths.push(sym_width as u32);
        }

        // §6.5.9 height class collective bitmap.
        let bmsize = match bmsize_table.get().decode(&mut r)? {
            HuffmanValue::Value(v) if v >= 0 => v as usize,
            _ => {
                return Err(DecodeError::Malformed {
                    reason: "invalid height-class collective bitmap size",
                });
            }
        };
        r.align_to_byte();
        let tot_width_u = u32::try_from(tot_width).map_err(|_| DecodeError::Overflow {
            operation: "collective bitmap width",
        })?;
        let hc_height_u = hc_height as u32;

        let collective = if tot_width_u == 0 {
            MonoBitmap::new(0, hc_height_u, false, limits)?
        } else if bmsize == 0 {
            // Uncompressed: HCHEIGHT rows of TOTWIDTH pixels, byte-padded.
            let row_bytes = (tot_width_u as usize).div_ceil(8);
            let need =
                row_bytes
                    .checked_mul(hc_height_u as usize)
                    .ok_or(DecodeError::Overflow {
                        operation: "uncompressed collective bitmap size",
                    })?;
            let bytes = take_bytes(&mut r, need)?;
            bitmap_from_uncompressed(bytes, tot_width_u, hc_height_u, row_bytes, limits)?
        } else {
            // MMR-coded collective bitmap of exactly `bmsize` bytes.
            let bytes = take_bytes(&mut r, bmsize)?;
            decode_mmr_bitmap(bytes, tot_width_u, hc_height_u, limits)?
        };
        r.align_to_byte();

        // Split the collective bitmap into the height class's symbols.
        let mut x0 = 0u32;
        for i in hc_first..new_symbols.len() {
            let w = widths[i];
            let sym = extract_columns(&collective, x0, w, hc_height_u, limits)?;
            new_symbols[i] = Arc::new(sym);
            x0 += w;
        }
    }

    // §6.5.10 export flags via Table B.1 over the same bit reader.
    let exported =
        export_symbols_huffman(&mut r, &export_table, imported, &new_symbols, total, num_ex)?;
    Ok(SymbolDictionary {
        exported_symbols: exported.into_boxed_slice(),
    })
}

/// Take exactly `n` bytes from the byte-aligned reader position.
fn take_bytes<'a>(r: &mut BitReader<'a>, n: usize) -> Result<&'a [u8], DecodeError> {
    let start = r.byte_position();
    let slice = r.remaining_from_byte();
    if slice.len() < n {
        return Err(DecodeError::Parse(
            crate::decode::error::ParseError::UnexpectedEof {
                offset: start,
                needed: n - slice.len(),
            },
        ));
    }
    // Advance the reader past the consumed bytes.
    for _ in 0..n {
        let _ = r.read_bits(8);
    }
    Ok(&slice[..n])
}

/// Build a bitmap from uncompressed, byte-padded, MSB-first row data.
fn bitmap_from_uncompressed(
    bytes: &[u8],
    width: u32,
    height: u32,
    row_bytes: usize,
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    let mut bm = MonoBitmap::new(width, height, false, limits)?;
    for y in 0..height {
        let row = &bytes[(y as usize) * row_bytes..(y as usize + 1) * row_bytes];
        for x in 0..width {
            let byte = row[(x as usize) >> 3];
            let bit = 7 - (x & 7);
            if (byte >> bit) & 1 != 0 {
                bm.set(x, y, true);
            }
        }
    }
    Ok(bm)
}

/// Extract columns `[x0, x0+width)` of `src` as a new bitmap.
fn extract_columns(
    src: &MonoBitmap,
    x0: u32,
    width: u32,
    height: u32,
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    let mut bm = MonoBitmap::new(width, height, false, limits)?;
    for y in 0..height {
        for x in 0..width {
            if src.get(x0 + x, y) {
                bm.set(x, y, true);
            }
        }
    }
    Ok(bm)
}

/// §6.5.10 export flags for the Huffman path: run lengths decoded with Table
/// B.1 over the Huffman bit reader; otherwise identical to the arithmetic path.
fn export_symbols_huffman(
    r: &mut BitReader<'_>,
    table: &HuffmanTable,
    imported: &[Arc<MonoBitmap>],
    new_symbols: &[Arc<MonoBitmap>],
    total: usize,
    expected_exports: usize,
) -> Result<Vec<Arc<MonoBitmap>>, DecodeError> {
    let mut exported: Vec<Arc<MonoBitmap>> = Vec::with_capacity(expected_exports.min(total));
    let mut index = 0usize;
    let mut ex_flag = false;
    let max_runs = total.saturating_mul(2).saturating_add(4);
    let mut runs = 0usize;

    while index < total {
        runs += 1;
        if runs > max_runs {
            return Err(DecodeError::Malformed {
                reason: "export-flag run count exceeded",
            });
        }
        let run = match table.decode(r)? {
            HuffmanValue::Value(v) if v >= 0 => v as usize,
            _ => {
                return Err(DecodeError::Malformed {
                    reason: "invalid export run length",
                });
            }
        };
        if run > total - index {
            return Err(DecodeError::Malformed {
                reason: "export run overruns symbol space",
            });
        }
        if ex_flag {
            for j in index..index + run {
                let sym = if j < imported.len() {
                    imported[j].clone()
                } else {
                    new_symbols[j - imported.len()].clone()
                };
                exported.push(sym);
            }
        }
        index += run;
        ex_flag = !ex_flag;
    }
    if index != total {
        return Err(DecodeError::Malformed {
            reason: "export runs do not cover symbol space exactly",
        });
    }
    if exported.len() != expected_exports {
        return Err(DecodeError::Malformed {
            reason: "exported symbol count does not match SDNUMEXSYMS",
        });
    }
    Ok(exported)
}

/// Decode the run-length export flags and materialise the exported symbol list.
///
/// The combined symbol space is `imported` followed by `new_symbols`; runs
/// alternate not-exported/exported starting from not-exported, and must cover
/// the space exactly and export exactly `expected_exports` symbols.
fn decode_export_flags(
    dec: &mut ArithmeticDecoder<'_>,
    int_ctx: &mut IntegerContexts,
    imported: &[Arc<MonoBitmap>],
    new_symbols: &[Arc<MonoBitmap>],
    total: usize,
    expected_exports: usize,
) -> Result<Vec<Arc<MonoBitmap>>, DecodeError> {
    let mut exported: Vec<Arc<MonoBitmap>> = Vec::with_capacity(expected_exports.min(total));
    let mut index = 0usize;
    let mut ex_flag = false;
    // Each run either advances `index` or flips `ex_flag`; bound the iterations
    // so a malicious all-zero-run stream cannot spin forever.
    let max_runs = total.saturating_mul(2).saturating_add(4);
    let mut runs = 0usize;

    while index < total {
        runs += 1;
        if runs > max_runs {
            return Err(DecodeError::Malformed {
                reason: "export-flag run count exceeded",
            });
        }
        let run = match int_ctx.decode(dec, IntProc::Iaex) {
            DecodedInteger::Value(v) if v >= 0 => v as usize,
            _ => {
                return Err(DecodeError::Malformed {
                    reason: "invalid export run length",
                });
            }
        };
        if run > total - index {
            return Err(DecodeError::Malformed {
                reason: "export run overruns symbol space",
            });
        }
        if ex_flag {
            for j in index..index + run {
                let sym = if j < imported.len() {
                    imported[j].clone()
                } else {
                    new_symbols[j - imported.len()].clone()
                };
                exported.push(sym);
            }
        }
        index += run;
        ex_flag = !ex_flag;
    }

    if index != total {
        return Err(DecodeError::Malformed {
            reason: "export runs do not cover symbol space exactly",
        });
    }
    if exported.len() != expected_exports {
        return Err(DecodeError::Malformed {
            reason: "exported symbol count does not match SDNUMEXSYMS",
        });
    }
    Ok(exported)
}
