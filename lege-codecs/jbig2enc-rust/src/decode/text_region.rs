//! Arithmetic text-region decoding (jbig2decplan.md §17, T.88 §6.4/§7.4.3).
//!
//! Inverts the encoder's `encode_text_region_mapped`
//! ([`crate::encode::document::symbol::text_region`]): the encoder emits, per
//! strip, `IADT` (strip T delta), then for each instance `IAFS`/`IADS` (S
//! coordinate), optional `IAIT` (T offset when `SBSTRIPS > 1`), and `IAID`
//! (symbol id), ending each strip with an `OOB` on `IADS`. Symbols are combined
//! into the region bitmap as they are decoded (no placement list), then the
//! region is composed onto the page by the caller.
//!
//! The encoder emits `REFCORNER = TOPLEFT` and `TRANSPOSED = 0`; this decoder
//! supports all four (non-transposed) reference corners and rejects transposed,
//! Huffman, and refined text regions with typed `Unsupported` errors.

use std::sync::Arc;

use crate::decode::arith::ArithmeticDecoder;
use crate::decode::error::{DecodeError, LimitError};
use crate::decode::huffman::{standard_table, BitReader, HuffmanTable, HuffmanValue};
use crate::decode::iaid::IaidContexts;
use crate::decode::integer::{DecodedInteger, IntegerContexts};
use crate::decode::refinement::decode_refinement_region_templated;
use crate::shared::bitmap::{CombinationOperator, MonoBitmap};
use crate::shared::int_proc::IntProc;
use crate::shared::limits::DecodeLimits;
use crate::shared::mq_table::MqContext;
use crate::shared::reader::Reader;

/// A decoded text region plus where it composes onto the page.
pub struct TextRegionResult {
    pub bitmap: MonoBitmap,
    pub x: u32,
    pub y: u32,
    /// External (region) combination operator (region-info flags low 3 bits).
    pub comb_operator: u8,
}

/// `ceil(log2(v))`, matching the encoder's `log2up` (no floor of 1). Returns 0
/// for `v <= 1` — a single-symbol dictionary reads zero symbol-id bits.
#[inline]
fn log2_ceil(v: u32) -> u32 {
    if v <= 1 {
        return 0;
    }
    let pow2 = v.is_power_of_two();
    let floor = 31 - v.leading_zeros();
    if pow2 { floor } else { floor + 1 }
}

#[inline]
fn comb_op(code: u8) -> CombinationOperator {
    match code {
        0 => CombinationOperator::Or,
        1 => CombinationOperator::And,
        2 => CombinationOperator::Xor,
        3 => CombinationOperator::Xnor,
        _ => CombinationOperator::Replace,
    }
}

#[inline]
fn clamp_i32(v: i64) -> i32 {
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

/// Decode an arithmetic text-region segment payload (T.88 §7.4.3).
///
/// `symbols` is the combined exported-symbol list of every referred dictionary,
/// in reference order. `int_ctx` and `iaid_ctx` are reused, worker-local scratch
/// reset here (a fresh text region starts every context at zero).
#[allow(clippy::too_many_arguments)]
pub fn decode_text_region(
    payload: &[u8],
    symbols: &[Arc<MonoBitmap>],
    huffman_tables: &[Arc<HuffmanTable>],
    limits: &DecodeLimits,
    int_ctx: &mut IntegerContexts,
    iaid_ctx: &mut IaidContexts,
    refine_ctx: &mut [MqContext],
) -> Result<TextRegionResult, DecodeError> {
    let mut r = Reader::new(payload);

    // §7.4.1 region segment information.
    let width = r.read_u32_be()?;
    let height = r.read_u32_be()?;
    let x = r.read_u32_be()?;
    let y = r.read_u32_be()?;
    let region_flags = r.read_u8()?;
    let ext_comb = region_flags & 0x07;

    // §7.4.3.1.1 text region segment flags (16-bit, big-endian).
    let flags = r.read_u16_be()?;
    let sbhuff = flags & 0x0001 != 0;
    let sbrefine = flags & 0x0002 != 0;
    let log_strips = ((flags >> 2) & 0x0003) as u8;
    let ref_corner = ((flags >> 4) & 0x0003) as u8;
    let transposed = (flags >> 6) & 0x0001 != 0;
    let sb_comb_op = ((flags >> 7) & 0x0003) as u8;
    let sb_def_pixel = (flags >> 9) & 0x0001 != 0;
    let ds_offset_raw = ((flags >> 10) & 0x001F) as u32;
    let sb_rtemplate = ((flags >> 15) & 0x0001) as u8;

    // Sign-extend the 5-bit SBDSOFFSET (shared by both coding paths).
    let sb_ds_offset: i32 = if ds_offset_raw >= 16 {
        ds_offset_raw as i32 - 32
    } else {
        ds_offset_raw as i32
    };

    if sbhuff {
        return decode_text_region_huffman(
            &mut r,
            payload,
            TextGeometry { width, height, x, y, ext_comb },
            TextFlags {
                sbrefine,
                log_strips,
                ref_corner,
                transposed,
                sb_comb_op,
                sb_def_pixel,
                sb_ds_offset,
                sb_rtemplate,
            },
            symbols,
            huffman_tables,
            refine_ctx,
            limits,
        );
    }
    // §7.4.3.1.3 SBRAT: refinement adaptive-template pixels, present only when
    // SBREFINE=1 and SBRTEMPLATE=0 (GRTEMPLATE-0 uses two AT pairs = 4 bytes).
    // Only the first (target) pair GRAT1 is used by the template-0 context.
    let mut grat: (i8, i8) = (-1, -1);
    if sbrefine && sb_rtemplate == 0 {
        let g1x = r.read_i8()?;
        let g1y = r.read_i8()?;
        let _g2x = r.read_i8()?;
        let _g2y = r.read_i8()?;
        grat = (g1x, g1y);
    }

    // §7.4.3.1.7 SBNUMINSTANCES.
    let num_instances = r.read_u32_be()?;
    if num_instances as usize > limits.max_symbols {
        return Err(DecodeError::limit(LimitError::Count {
            what: "text instances",
            value: num_instances as u64,
            limit: limits.max_symbols as u64,
        }));
    }

    let code_len = log2_ceil(symbols.len() as u32) as u8;
    iaid_ctx.reset_for_bits(code_len)?;
    int_ctx.reset();
    if sbrefine {
        for c in refine_ctx.iter_mut() {
            *c = MqContext(0);
        }
    }

    let params = TextArithParams {
        width,
        height,
        num_instances,
        log_strips,
        ref_corner,
        transposed,
        sb_comb_op,
        sb_def_pixel,
        sb_ds_offset,
        sbrefine,
        sb_rtemplate,
        grat,
        code_len,
    };
    let data = &payload[r.position()..];
    let mut dec = ArithmeticDecoder::new(data);
    let bitmap = decode_text_region_arith(
        &mut dec, &params, symbols, int_ctx, iaid_ctx, refine_ctx, limits,
    )?;
    Ok(TextRegionResult {
        bitmap,
        x,
        y,
        comb_operator: ext_comb,
    })
}

/// Explicit parameters for the arithmetic text-region core, so it can serve both
/// a text-region segment and an aggregate symbol-dictionary symbol (§6.5.8.2).
pub(crate) struct TextArithParams {
    pub width: u32,
    pub height: u32,
    pub num_instances: u32,
    pub log_strips: u8,
    pub ref_corner: u8,
    pub transposed: bool,
    pub sb_comb_op: u8,
    pub sb_def_pixel: bool,
    pub sb_ds_offset: i32,
    pub sbrefine: bool,
    pub sb_rtemplate: u8,
    pub grat: (i8, i8),
    pub code_len: u8,
}

/// The arithmetic text-region strip/placement loop (T.88 §6.4.5), decoding from
/// an existing arithmetic decoder and context banks (which the caller has reset
/// or is carrying across an aggregate invocation).
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_text_region_arith(
    dec: &mut ArithmeticDecoder<'_>,
    p: &TextArithParams,
    symbols: &[Arc<MonoBitmap>],
    int_ctx: &mut IntegerContexts,
    iaid_ctx: &mut IaidContexts,
    refine_ctx: &mut [MqContext],
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    let sb_strips: i64 = 1i64 << (p.log_strips & 0x03);
    let symbol_op = comb_op(p.sb_comb_op);
    let mut bitmap = MonoBitmap::new(p.width, p.height, p.sb_def_pixel, limits)?;

    // §6.4.5 1) initial STRIPT.
    let dt0 = decode_value(int_ctx.decode(dec, IntProc::Iadt))?;
    let mut strip_t: i64 = -(dt0 as i64 * sb_strips);
    let mut first_s: i64 = 0;
    let mut n_inst: u32 = 0;

    while n_inst < p.num_instances {
        // §6.4.5 3b) strip T delta.
        let dt = decode_value(int_ctx.decode(dec, IntProc::Iadt))?;
        strip_t = strip_t
            .checked_add(dt as i64 * sb_strips)
            .ok_or(DecodeError::Overflow { operation: "strip T" })?;

        // §6.4.5 3c) first symbol S coordinate.
        let dfs = decode_value(int_ctx.decode(dec, IntProc::Iafs))?;
        first_s = first_s
            .checked_add(dfs as i64)
            .ok_or(DecodeError::Overflow { operation: "first S" })?;
        let mut cur_s = first_s;
        let mut first_in_strip = true;

        loop {
            if !first_in_strip {
                // §6.4.5 3c) subsequent S: IADS, OOB ends the strip.
                match int_ctx.decode(dec, IntProc::Iads) {
                    DecodedInteger::OutOfBand => break,
                    DecodedInteger::Value(ids) => {
                        cur_s = cur_s
                            .checked_add(ids as i64 + p.sb_ds_offset as i64)
                            .ok_or(DecodeError::Overflow { operation: "S coordinate" })?;
                    }
                }
            }
            first_in_strip = false;

            if n_inst >= p.num_instances {
                return Err(DecodeError::Malformed {
                    reason: "more text instances than SBNUMINSTANCES",
                });
            }

            // §6.4.5 3c vi) current T within the strip.
            let cur_t = if sb_strips == 1 {
                0
            } else {
                decode_value(int_ctx.decode(dec, IntProc::Iait))? as i64
            };
            let t_i = strip_t
                .checked_add(cur_t)
                .ok_or(DecodeError::Overflow { operation: "T coordinate" })?;

            // §6.4.5 3c x) symbol id.
            let id = iaid_ctx.decode(dec, p.code_len);
            let symbol = symbols.get(id as usize).ok_or(DecodeError::Malformed {
                reason: "symbol id out of range",
            })?;
            let hi = symbol.height() as i64;

            // §6.4.11 refinement indicator (only present when SBREFINE=1).
            let ri = if p.sbrefine {
                decode_value(int_ctx.decode(dec, IntProc::Iari))?
            } else {
                0
            };

            // The glyph actually placed: the reference symbol, or a decoded
            // refinement of it. `placed_height` anchors bottom reference corners.
            let (placed, placed_height): (PlacedSymbol<'_>, i64) = if ri != 0 {
                let rdw = decode_value(int_ctx.decode(dec, IntProc::Iardw))? as i64;
                let rdh = decode_value(int_ctx.decode(dec, IntProc::Iardh))? as i64;
                let rdx = decode_value(int_ctx.decode(dec, IntProc::Iardx))?;
                let rdy = decode_value(int_ctx.decode(dec, IntProc::Iardy))?;
                let grw = symbol.width() as i64 + rdw;
                let grh = symbol.height() as i64 + rdh;
                if grw <= 0
                    || grh <= 0
                    || grw > limits.max_width as i64
                    || grh > limits.max_height as i64
                {
                    return Err(DecodeError::Malformed {
                        reason: "refined glyph dimensions out of range",
                    });
                }
                // §6.3.5.3: GRREFERENCEDX/DY = floor(RDW/2)+RDX, floor(RDH/2)+RDY.
                // RDX/RDY are unbounded decoded integers; saturate the sum.
                let grdx = (rdw.div_euclid(2) as i32).saturating_add(rdx);
                let grdy = (rdh.div_euclid(2) as i32).saturating_add(rdy);
                let refined = decode_refinement_region_templated(
                    dec,
                    symbol,
                    grw as u32,
                    grh as u32,
                    grdx,
                    grdy,
                    p.sb_rtemplate,
                    false,
                    p.grat,
                    refine_ctx,
                    limits,
                )?;
                (PlacedSymbol::Owned(refined), grh)
            } else {
                (PlacedSymbol::Borrowed(symbol), hi)
            };

            // Placement (all four reference corners, transposed or not). CURS
            // advances by the *placed* glyph extent, matching jbig2dec.
            let placed_width = placed.bitmap().width() as i64;
            place_symbol(
                &mut bitmap,
                placed.bitmap(),
                p.ref_corner,
                p.transposed,
                &mut cur_s,
                t_i,
                placed_width,
                placed_height,
                symbol_op,
            )?;
            n_inst += 1;
            // §6.4.5: completion is checked at the strip boundary (step 4a), so
            // decode the whole strip through its terminating IADS OOB before
            // stopping — essential when an aggregate shares its arithmetic
            // stream with the enclosing dictionary.
        }
    }
    Ok(bitmap)
}

/// Geometry from the region-segment information field.
struct TextGeometry {
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    ext_comb: u8,
}

/// The text-region flag fields shared by both coding paths.
struct TextFlags {
    sbrefine: bool,
    log_strips: u8,
    ref_corner: u8,
    transposed: bool,
    sb_comb_op: u8,
    sb_def_pixel: bool,
    sb_ds_offset: i32,
    sb_rtemplate: u8,
}

/// Select an FS/DS/DT/... Huffman table from a two-bit selection, consuming a
/// custom table from `custom` (in field order) for selection value 3.
fn tr_select<'a, I>(
    selection: u32,
    a: u8,
    b: u8,
    c: Option<u8>,
    custom: &mut I,
) -> Result<TrTable, DecodeError>
where
    I: Iterator<Item = &'a Arc<HuffmanTable>>,
{
    match selection {
        0 => Ok(TrTable::Owned(standard_table(a)?)),
        1 => Ok(TrTable::Owned(standard_table(b)?)),
        2 if c.is_some() => {
            let idx = c.ok_or(DecodeError::Malformed {
                reason: "reserved text-region Huffman table selection",
            })?;
            Ok(TrTable::Owned(standard_table(idx)?))
        }
        3 => custom
            .next()
            .map(|t| TrTable::Shared(t.clone()))
            .ok_or(DecodeError::Malformed {
                reason: "custom Huffman table referenced but not supplied",
            }),
        _ => Err(DecodeError::Malformed {
            reason: "reserved text-region Huffman table selection",
        }),
    }
}

enum TrTable {
    Owned(HuffmanTable),
    Shared(Arc<HuffmanTable>),
}
impl TrTable {
    #[inline]
    fn get(&self) -> &HuffmanTable {
        match self {
            TrTable::Owned(t) => t,
            TrTable::Shared(t) => t,
        }
    }
}

/// Decode the symbol-ID Huffman table (T.88 §7.4.3.1.7): 35 four-bit RUNCODE
/// lengths, run-length-coded symbol-ID code lengths, then B.3 code assignment.
fn decode_symbol_id_table(
    r: &mut BitReader<'_>,
    num_syms: usize,
    limits: &DecodeLimits,
) -> Result<HuffmanTable, DecodeError> {
    // 1) 35 RUNCODE code lengths (4 bits each).
    let mut runcode_lengths = [0u32; 35];
    for len in runcode_lengths.iter_mut() {
        *len = r.read_bits(4);
    }
    let runcode_table = HuffmanTable::from_code_lengths(&runcode_lengths)?;

    // 3–5) decode `num_syms` symbol-ID code lengths.
    let mut lengths: Vec<u32> = Vec::with_capacity(num_syms.min(1 << 16));
    let mut prev_len = 0u32;
    let mut guard = 0usize;
    let guard_max = num_syms.saturating_mul(2).saturating_add(64);
    while lengths.len() < num_syms {
        guard += 1;
        if guard > guard_max {
            return Err(DecodeError::Malformed {
                reason: "symbol-ID code-length run overrun",
            });
        }
        let code = match runcode_table.decode(r)? {
            HuffmanValue::Value(v) => v as u32,
            HuffmanValue::Oob => {
                return Err(DecodeError::Malformed {
                    reason: "OOB in symbol-ID runcode stream",
                });
            }
        };
        match code {
            0..=31 => {
                lengths.push(code);
                prev_len = code;
            }
            32 => {
                // Copy the previous length 3–6 times (2 extra bits + 3).
                let repeat = r.read_bits(2) as usize + 3;
                for _ in 0..repeat {
                    if lengths.len() >= num_syms {
                        break;
                    }
                    lengths.push(prev_len);
                }
            }
            33 => {
                // Repeat length 0 for 3–10 times (3 extra bits + 3).
                let repeat = r.read_bits(3) as usize + 3;
                for _ in 0..repeat {
                    if lengths.len() >= num_syms {
                        break;
                    }
                    lengths.push(0);
                }
                prev_len = 0;
            }
            34 => {
                // Repeat length 0 for 11–138 times (7 extra bits + 11).
                let repeat = r.read_bits(7) as usize + 11;
                for _ in 0..repeat {
                    if lengths.len() >= num_syms {
                        break;
                    }
                    lengths.push(0);
                }
                prev_len = 0;
            }
            _ => {
                return Err(DecodeError::Malformed {
                    reason: "invalid symbol-ID runcode",
                });
            }
        }
        if lengths.len() > limits.max_symbols {
            return Err(DecodeError::limit(LimitError::Count {
                what: "symbol-ID code lengths",
                value: lengths.len() as u64,
                limit: limits.max_symbols as u64,
            }));
        }
    }
    // 6) byte-align, then 7) build SBSYMCODES.
    r.align_to_byte();
    HuffmanTable::from_code_lengths(&lengths)
}

/// Decode a Huffman-coded text region (SBHUFF=1), including SBREFINE=1 (the
/// symbol IDs and positions are Huffman-coded; each refinement bitmap is a
/// byte-aligned arithmetic block of the Huffman-coded size, §6.4.11.5).
#[allow(clippy::too_many_arguments)]
fn decode_text_region_huffman(
    r: &mut Reader<'_>,
    payload: &[u8],
    geom: TextGeometry,
    tf: TextFlags,
    symbols: &[Arc<MonoBitmap>],
    huffman_tables: &[Arc<HuffmanTable>],
    refine_ctx: &mut [MqContext],
    limits: &DecodeLimits,
) -> Result<TextRegionResult, DecodeError> {
    // §7.4.3.1.2 text region Huffman flags (16-bit).
    let hflags = r.read_u16_be()?;
    let fs_sel = (hflags & 0x0003) as u32;
    let ds_sel = ((hflags >> 2) & 0x0003) as u32;
    let dt_sel = ((hflags >> 4) & 0x0003) as u32;
    let rdw_sel = ((hflags >> 6) & 0x0003) as u32;
    let rdh_sel = ((hflags >> 8) & 0x0003) as u32;
    let rdx_sel = ((hflags >> 10) & 0x0003) as u32;
    let rdy_sel = ((hflags >> 12) & 0x0003) as u32;
    let rsize_sel = (hflags >> 14) & 0x0001 != 0;

    // §7.4.3.1.3 SBRAT: present when SBREFINE=1 and SBRTEMPLATE=0.
    let mut grat: (i8, i8) = (-1, -1);
    if tf.sbrefine && tf.sb_rtemplate == 0 {
        let g1x = r.read_i8()?;
        let g1y = r.read_i8()?;
        let _g2x = r.read_i8()?;
        let _g2y = r.read_i8()?;
        grat = (g1x, g1y);
    }

    // §7.4.3.1.4 SBNUMINSTANCES.
    let num_instances = r.read_u32_be()?;
    if num_instances as usize > limits.max_symbols {
        return Err(DecodeError::limit(LimitError::Count {
            what: "text instances",
            value: num_instances as u64,
            limit: limits.max_symbols as u64,
        }));
    }

    // Select FS/DS/DT (+ refinement) tables (custom tables consumed in order:
    // §7.4.3.1.6 FS, DS, DT, RDW, RDH, RDX, RDY, RSIZE).
    let mut custom = huffman_tables.iter();
    let fs_table = tr_select(fs_sel, 6, 7, None, &mut custom)?;
    let ds_table = tr_select(ds_sel, 8, 9, Some(10), &mut custom)?;
    let dt_table = tr_select(dt_sel, 11, 12, Some(13), &mut custom)?;
    let (rdw_table, rdh_table, rdx_table, rdy_table, rsize_table);
    if tf.sbrefine {
        rdw_table = tr_select(rdw_sel, 14, 15, None, &mut custom)?;
        rdh_table = tr_select(rdh_sel, 14, 15, None, &mut custom)?;
        rdx_table = tr_select(rdx_sel, 14, 15, None, &mut custom)?;
        rdy_table = tr_select(rdy_sel, 14, 15, None, &mut custom)?;
        rsize_table = if rsize_sel {
            custom
                .next()
                .map(|t| TrTable::Shared(t.clone()))
                .ok_or(DecodeError::Malformed {
                    reason: "custom SBHUFFRSIZE table not supplied",
                })?
        } else {
            TrTable::Owned(standard_table(1)?) // Table B.1
        };
        for c in refine_ctx.iter_mut() {
            *c = MqContext(0);
        }
    } else {
        rdw_table = TrTable::Owned(standard_table(1)?);
        rdh_table = TrTable::Owned(standard_table(1)?);
        rdx_table = TrTable::Owned(standard_table(1)?);
        rdy_table = TrTable::Owned(standard_table(1)?);
        rsize_table = TrTable::Owned(standard_table(1)?);
    }

    // The symbol-ID table and the strip data share one bit stream starting at
    // the current byte position (§7.4.3.1.5 then the strip data).
    let bit_start = r.position();
    let mut br = BitReader::new(&payload[bit_start..]);
    let sym_table = decode_symbol_id_table(&mut br, symbols.len(), limits)?;

    let tables = HuffTextTables {
        sym: &sym_table,
        fs: fs_table.get(),
        ds: ds_table.get(),
        dt: dt_table.get(),
        rdw: rdw_table.get(),
        rdh: rdh_table.get(),
        rdx: rdx_table.get(),
        rdy: rdy_table.get(),
        rsize: rsize_table.get(),
    };
    let params = HuffTextParams {
        width: geom.width,
        height: geom.height,
        num_instances,
        log_strips: tf.log_strips,
        ref_corner: tf.ref_corner,
        transposed: tf.transposed,
        sb_comb_op: tf.sb_comb_op,
        sb_def_pixel: tf.sb_def_pixel,
        sb_ds_offset: tf.sb_ds_offset,
        sbrefine: tf.sbrefine,
        sb_rtemplate: tf.sb_rtemplate,
        grat,
    };
    let bitmap = decode_huffman_text_core(&mut br, &tables, &params, symbols, refine_ctx, limits)?;
    Ok(TextRegionResult {
        bitmap,
        x: geom.x,
        y: geom.y,
        comb_operator: geom.ext_comb,
    })
}

/// Explicit parameters for the Huffman text-region core, so it can serve both a
/// text-region segment and a Huffman aggregate dictionary symbol (§6.5.8.2).
pub(crate) struct HuffTextParams {
    pub width: u32,
    pub height: u32,
    pub num_instances: u32,
    pub log_strips: u8,
    pub ref_corner: u8,
    pub transposed: bool,
    pub sb_comb_op: u8,
    pub sb_def_pixel: bool,
    pub sb_ds_offset: i32,
    pub sbrefine: bool,
    pub sb_rtemplate: u8,
    pub grat: (i8, i8),
}

/// The Huffman tables a text region reads from (already resolved to concrete
/// tables). `sym` is SBSYMCODES.
pub(crate) struct HuffTextTables<'a> {
    pub sym: &'a HuffmanTable,
    pub fs: &'a HuffmanTable,
    pub ds: &'a HuffmanTable,
    pub dt: &'a HuffmanTable,
    pub rdw: &'a HuffmanTable,
    pub rdh: &'a HuffmanTable,
    pub rdx: &'a HuffmanTable,
    pub rdy: &'a HuffmanTable,
    pub rsize: &'a HuffmanTable,
}

/// The Huffman text-region strip/placement loop (T.88 §6.4.5, SBHUFF=1),
/// decoding from an existing bit reader and pre-selected tables. Refinement
/// bitmaps are byte-aligned arithmetic blocks with fresh GR statistics each
/// (§6.4.11.5).
pub(crate) fn decode_huffman_text_core(
    br: &mut BitReader<'_>,
    tables: &HuffTextTables<'_>,
    p: &HuffTextParams,
    symbols: &[Arc<MonoBitmap>],
    refine_ctx: &mut [MqContext],
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    let sb_strips: i64 = 1i64 << (p.log_strips & 0x03);
    let log_strips = (p.log_strips & 0x03) as u32;
    let symbol_op = comb_op(p.sb_comb_op);
    let mut bitmap = MonoBitmap::new(p.width, p.height, p.sb_def_pixel, limits)?;

    // §6.4.5 2) initial STRIPT = -(DT0 * SBSTRIPS); FIRSTS = 0.
    let dt0 = huff_value(tables.dt.decode(br)?)? as i64;
    let mut strip_t: i64 = -(dt0 * sb_strips);
    let mut first_s: i64 = 0;
    let mut n_inst: u32 = 0;

    while n_inst < p.num_instances {
        // §6.4.5 4b) strip delta T.
        let dt = huff_value(tables.dt.decode(br)?)? as i64;
        strip_t = strip_t
            .checked_add(dt * sb_strips)
            .ok_or(DecodeError::Overflow { operation: "strip T" })?;

        // §6.4.5 4c i) first S coordinate.
        let dfs = huff_value(tables.fs.decode(br)?)? as i64;
        first_s = first_s
            .checked_add(dfs)
            .ok_or(DecodeError::Overflow { operation: "first S" })?;
        let mut cur_s = first_s;
        let mut first_in_strip = true;

        loop {
            if !first_in_strip {
                // §6.4.5 4c ii) subsequent S; OOB ends the strip.
                match tables.ds.decode(br)? {
                    HuffmanValue::Oob => break,
                    HuffmanValue::Value(ids) => {
                        cur_s = cur_s
                            .checked_add(ids as i64 + p.sb_ds_offset as i64)
                            .ok_or(DecodeError::Overflow { operation: "S coordinate" })?;
                    }
                }
            }
            first_in_strip = false;

            if n_inst >= p.num_instances {
                return Err(DecodeError::Malformed {
                    reason: "more text instances than SBNUMINSTANCES",
                });
            }

            // §6.4.5 4c iii) T within the strip.
            let cur_t = if sb_strips == 1 {
                0
            } else {
                br.read_bits(log_strips) as i64
            };
            let t_i = strip_t
                .checked_add(cur_t)
                .ok_or(DecodeError::Overflow { operation: "T coordinate" })?;

            // §6.4.5 4c iv) symbol ID via SBSYMCODES.
            let id = match tables.sym.decode(br)? {
                HuffmanValue::Value(v) => v as usize,
                HuffmanValue::Oob => {
                    return Err(DecodeError::Malformed {
                        reason: "OOB decoding symbol ID",
                    });
                }
            };
            let symbol = symbols.get(id).ok_or(DecodeError::Malformed {
                reason: "symbol id out of range",
            })?;

            // §6.4.11 refinement indicator (one bit when SBHUFF=1).
            let ri = if p.sbrefine { br.read_bit() } else { 0 };
            let (placed, placed_w, placed_h): (PlacedSymbol<'_>, i64, i64) = if ri != 0 {
                let rdw = huff_value(tables.rdw.decode(br)?)? as i64;
                let rdh = huff_value(tables.rdh.decode(br)?)? as i64;
                let rdx = huff_value(tables.rdx.decode(br)?)?;
                let rdy = huff_value(tables.rdy.decode(br)?)?;
                // §6.4.11.5 refinement bitmap data size, then byte-align.
                let bmsize = match tables.rsize.decode(br)? {
                    HuffmanValue::Value(v) if v >= 0 => v as usize,
                    _ => {
                        return Err(DecodeError::Malformed {
                            reason: "invalid Huffman refinement bitmap size",
                        });
                    }
                };
                br.align_to_byte();
                let grw = symbol.width() as i64 + rdw;
                let grh = symbol.height() as i64 + rdh;
                if grw <= 0
                    || grh <= 0
                    || grw > limits.max_width as i64
                    || grh > limits.max_height as i64
                {
                    return Err(DecodeError::Malformed {
                        reason: "refined glyph dimensions out of range",
                    });
                }
                let grdx = (rdw.div_euclid(2) as i32).saturating_add(rdx);
                let grdy = (rdh.div_euclid(2) as i32).saturating_add(rdy);
                for c in refine_ctx.iter_mut() {
                    *c = MqContext(0);
                }
                let refine_bytes = br.remaining_from_byte();
                let block = if bmsize > 0 && bmsize <= refine_bytes.len() {
                    &refine_bytes[..bmsize]
                } else {
                    refine_bytes
                };
                let mut rdec = ArithmeticDecoder::new(block);
                let refined = decode_refinement_region_templated(
                    &mut rdec,
                    symbol,
                    grw as u32,
                    grh as u32,
                    grdx,
                    grdy,
                    p.sb_rtemplate,
                    false,
                    p.grat,
                    refine_ctx,
                    limits,
                )?;
                // §6.4.11.5 7): skip exactly BMSIZE bytes, then byte-align.
                for _ in 0..bmsize {
                    let _ = br.read_bits(8);
                }
                br.align_to_byte();
                (PlacedSymbol::Owned(refined), grw, grh)
            } else {
                (
                    PlacedSymbol::Borrowed(symbol),
                    symbol.width() as i64,
                    symbol.height() as i64,
                )
            };

            place_symbol(
                &mut bitmap,
                placed.bitmap(),
                p.ref_corner,
                p.transposed,
                &mut cur_s,
                t_i,
                placed_w,
                placed_h,
                symbol_op,
            )?;

            n_inst += 1;
        }
    }
    Ok(bitmap)
}

/// Symbol placement (T.88 §6.4.5 4c vi–xi) for all four reference corners and
/// both axis orientations, advancing `cur_s` per the spec's before/after rules.
#[allow(clippy::too_many_arguments)]
fn place_symbol(
    bitmap: &mut MonoBitmap,
    symbol: &MonoBitmap,
    ref_corner: u8,
    transposed: bool,
    cur_s: &mut i64,
    t_i: i64,
    wi: i64,
    hi: i64,
    op: CombinationOperator,
) -> Result<(), DecodeError> {
    // REFCORNER: 0 BOTTOMLEFT, 1 TOPLEFT, 2 BOTTOMRIGHT, 3 TOPRIGHT.
    let right = ref_corner == 2 || ref_corner == 3;
    let bottom = ref_corner == 0 || ref_corner == 2;

    if !transposed {
        // vi) right corners advance CURS (the X axis) by WI-1 before placement.
        if right {
            *cur_s = cur_s
                .checked_add(wi - 1)
                .ok_or(DecodeError::Overflow { operation: "S advance (pre)" })?;
        }
        let si = *cur_s;
        // viii) SBREG[SI, TI] with the given reference corner → top-left.
        let x_left = if right { si - (wi - 1) } else { si };
        let y_top = if bottom { t_i - (hi - 1) } else { t_i };
        bitmap.combine(symbol, clamp_i32(x_left), clamp_i32(y_top), op);
        // xi) left corners advance CURS by WI-1 after placement.
        if !right {
            *cur_s = cur_s
                .checked_add(wi - 1)
                .ok_or(DecodeError::Overflow { operation: "S advance (post)" })?;
        }
    } else {
        // Transposed: the S axis is Y and the T axis is X (§6.4.5).
        // vi) bottom corners advance CURS (the Y axis) by HI-1 before placement.
        if bottom {
            *cur_s = cur_s
                .checked_add(hi - 1)
                .ok_or(DecodeError::Overflow { operation: "S advance (pre)" })?;
        }
        let si = *cur_s;
        // viii) SBREG[TI, SI] with the given reference corner → top-left.
        let x_left = if right { t_i - (wi - 1) } else { t_i };
        let y_top = if bottom { si - (hi - 1) } else { si };
        bitmap.combine(symbol, clamp_i32(x_left), clamp_i32(y_top), op);
        // xi) top corners advance CURS by HI-1 after placement.
        if !bottom {
            *cur_s = cur_s
                .checked_add(hi - 1)
                .ok_or(DecodeError::Overflow { operation: "S advance (post)" })?;
        }
    }
    Ok(())
}

/// Require a finite Huffman value (OOB where a value is mandatory is malformed).
#[inline]
fn huff_value(v: HuffmanValue) -> Result<i32, DecodeError> {
    match v {
        HuffmanValue::Value(v) => Ok(v),
        HuffmanValue::Oob => Err(DecodeError::Malformed {
            reason: "unexpected OOB in Huffman text region",
        }),
    }
}

/// The glyph placed for one instance: a borrowed dictionary symbol (RI=0) or a
/// freshly decoded refinement bitmap (RI=1).
enum PlacedSymbol<'a> {
    Borrowed(&'a Arc<MonoBitmap>),
    Owned(MonoBitmap),
}

impl PlacedSymbol<'_> {
    #[inline]
    fn bitmap(&self) -> &MonoBitmap {
        match self {
            PlacedSymbol::Borrowed(s) => s.as_ref(),
            PlacedSymbol::Owned(b) => b,
        }
    }
}

/// Require a finite value (OOB where a value is mandatory is malformed).
#[inline]
fn decode_value(d: DecodedInteger) -> Result<i32, DecodeError> {
    match d {
        DecodedInteger::Value(v) => Ok(v),
        DecodedInteger::OutOfBand => Err(DecodeError::Malformed {
            reason: "unexpected OOB in text region",
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn log2_ceil_matches_encoder() {
        // Mirror encoder::log2up for a spread of counts.
        fn log2up(v: u32) -> u32 {
            if v == 0 {
                return 0;
            }
            let is_pow = (v & (v - 1)) == 0;
            let mut r = 0;
            let mut val = v;
            while val > 1 {
                val >>= 1;
                r += 1;
            }
            r + if is_pow { 0 } else { 1 }
        }
        for v in [0u32, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 255, 256, 257, 1000] {
            assert_eq!(log2_ceil(v.max(1)), log2up(v.max(1)), "v={v}");
        }
    }
}
