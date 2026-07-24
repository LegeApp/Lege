//! Page information parsing and region composition (jbig2decplan.md §15).
//!
//! Segments are processed in stream order. A page-information segment allocates
//! the page buffer; each immediate generic region is decoded and composited
//! onto its associated page. When a region exactly covers a blank page at the
//! origin with a compatible operator, the decoded region bitmap is moved into
//! the page with no copy (the dominant single-region scanned-page case).

use std::sync::Arc;

use crate::decode::arith::ArithmeticDecoder;
use crate::decode::context::DecoderContext;
use crate::decode::error::{DecodeError, LimitError, ParseError, UnsupportedFeature};
use crate::decode::file::{ParsedDocument, ParsedSegment};
use crate::decode::generic::{decode_generic_region_into, parse_generic_region};
use crate::decode::globals::DecodedGlobals;
use crate::decode::halftone_region::{decode_halftone_region, parse_halftone_region};
use crate::decode::pattern_dictionary::{PatternDictionary, decode_pattern_dictionary};
use crate::decode::refinement::{
    REFINEMENT_CONTEXT_COUNT, decode_refinement_region_templated, page_reference_window,
    parse_refinement_region,
};
use crate::decode::store::{DecodedSegment, SegmentStore};
use crate::decode::symbol_dictionary::{SymbolDictionary, decode_symbol_dictionary};
use crate::decode::text_region::decode_text_region;
use crate::shared::bitmap::{CombinationOperator, MonoBitmap};
use crate::shared::limits::DecodeLimits;
use crate::shared::reader::Reader;
use crate::shared::segment::SegmentType;

/// The unknown-height sentinel for a striped page (T.88 §7.4.8.5).
const UNKNOWN_HEIGHT: u32 = 0xFFFF_FFFF;

/// Parsed page-information segment (T.88 §7.4.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageInformation {
    pub width: u32,
    pub height: u32,
    pub x_resolution: u32,
    pub y_resolution: u32,
    pub is_lossless: bool,
    pub contains_refinements: bool,
    /// Default pixel value: `true` => the page starts all black.
    pub default_pixel: bool,
    /// Default combination operator (bits 3–4).
    pub default_operator: u8,
    pub requires_aux_buffers: bool,
    pub operator_may_be_overridden: bool,
    pub striping_info: u16,
}

impl PageInformation {
    #[inline]
    pub fn has_unknown_height(&self) -> bool {
        self.height == UNKNOWN_HEIGHT
    }
}

/// Parse a page-information segment payload (T.88 §7.4.8).
pub fn parse_page_info(payload: &[u8]) -> Result<PageInformation, ParseError> {
    let mut r = Reader::new(payload);
    let width = r.read_u32_be()?;
    let height = r.read_u32_be()?;
    let x_resolution = r.read_u32_be()?;
    let y_resolution = r.read_u32_be()?;
    let flags = r.read_u8()?;
    let striping_info = r.read_u16_be()?;
    Ok(PageInformation {
        width,
        height,
        x_resolution,
        y_resolution,
        is_lossless: flags & 0x01 != 0,
        contains_refinements: flags & 0x02 != 0,
        default_pixel: flags & 0x04 != 0,
        default_operator: (flags >> 3) & 0x03,
        requires_aux_buffers: flags & 0x20 != 0,
        operator_may_be_overridden: flags & 0x40 != 0,
        striping_info,
    })
}

/// A decoded page: its association number and pixel bitmap.
#[derive(Clone, Debug)]
pub struct DecodedPage {
    pub page_number: u32,
    pub bitmap: MonoBitmap,
}

#[inline]
fn combination_operator(code: u8) -> CombinationOperator {
    match code {
        0 => CombinationOperator::Or,
        1 => CombinationOperator::And,
        2 => CombinationOperator::Xor,
        3 => CombinationOperator::Xnor,
        _ => CombinationOperator::Replace,
    }
}

/// Process every segment of a parsed document into decoded pages
/// (jbig2decplan.md §15). Unsupported segment types (symbols, halftone,
/// refinement, MMR) surface as typed `Unsupported` errors.
pub fn process_document(
    doc: &ParsedDocument<'_>,
    limits: &DecodeLimits,
    ctx: &mut DecoderContext,
) -> Result<Vec<DecodedPage>, DecodeError> {
    process_document_with_globals(doc, None, limits, ctx)
}

/// Decode a symbol-dictionary segment and register it in `local`, returning the
/// shared handle (jbig2decplan.md §16, §19). Imported symbols are gathered from
/// the segment's referred dictionaries (in `local`, then `globals`).
pub(crate) fn decode_symbol_dict_into(
    seg: &ParsedSegment<'_>,
    local: &mut SegmentStore,
    globals: Option<&SegmentStore>,
    limits: &DecodeLimits,
    ctx: &mut DecoderContext,
) -> Result<Arc<SymbolDictionary>, DecodeError> {
    let imported = local.gather_symbols(seg.header.number, &seg.header.referred_to, globals)?;
    let tables = local.gather_huffman_tables(&seg.header.referred_to, globals);

    // §7.4.2.1.1 bits 8/9: "bitmap coding context used" / "retained".
    let flags = if seg.data.len() >= 2 {
        u16::from_be_bytes([seg.data[0], seg.data[1]])
    } else {
        0
    };
    let ctx_used = flags & 0x0100 != 0;
    let ctx_retained = flags & 0x0200 != 0;

    ctx.ensure_generic();
    if ctx.refinement_contexts.len() < REFINEMENT_CONTEXT_COUNT {
        ctx.refinement_contexts
            .resize(REFINEMENT_CONTEXT_COUNT, Default::default());
    }
    // §6.5.5 step 3: import the generic + refinement statistics of the last
    // referred retained dictionary before decoding.
    if let Some(ret) = ctx_used
        .then(|| local.last_retained(&seg.header.referred_to, globals))
        .flatten()
    {
        let n = ret.generic.len().min(ctx.generic_contexts.len());
        ctx.generic_contexts[..n].copy_from_slice(&ret.generic[..n]);
        let rn = ret.refine.len().min(ctx.refinement_contexts.len());
        ctx.refinement_contexts[..rn].copy_from_slice(&ret.refine[..rn]);
    }
    // Disjoint field borrows: generic, integer, IAID, and refinement banks.
    let generic = &mut ctx.generic_contexts[..crate::decode::context::GENERIC_CONTEXT_COUNT];
    let int_ctx = &mut ctx.integer_contexts;
    let iaid_ctx = &mut ctx.iaid_contexts;
    let refine_ctx = &mut ctx.refinement_contexts[..REFINEMENT_CONTEXT_COUNT];
    let scratch = &mut ctx.generic_scratch;
    let dict = decode_symbol_dictionary(
        seg.data, &imported, &tables, limits, int_ctx, iaid_ctx, generic, refine_ctx, scratch,
        !ctx_used,
    )
    .map_err(|source| annotate(seg.header.number, source))?;

    // §6.5.5 step 7: preserve this dictionary's statistics for a later importer.
    if ctx_retained {
        local.insert_retained(
            seg.header.number,
            crate::decode::store::RetainedContexts {
                generic: ctx.generic_contexts[..crate::decode::context::GENERIC_CONTEXT_COUNT]
                    .to_vec(),
                refine: ctx.refinement_contexts[..REFINEMENT_CONTEXT_COUNT].to_vec(),
            },
        );
    }

    let arc = Arc::new(dict);
    local.insert(
        seg.header.number,
        DecodedSegment::SymbolDictionary(arc.clone()),
    )?;
    Ok(arc)
}

/// Decode a pattern-dictionary segment and register it in `local`, returning the
/// shared handle (jbig2decplan.md §18, §19).
pub(crate) fn decode_pattern_dict_into(
    seg: &ParsedSegment<'_>,
    local: &mut SegmentStore,
    limits: &DecodeLimits,
    ctx: &mut DecoderContext,
) -> Result<Arc<PatternDictionary>, DecodeError> {
    let (generic, scratch) = ctx.generic_and_scratch();
    let dict = decode_pattern_dictionary(seg.data, limits, generic, scratch)
        .map_err(|source| annotate(seg.header.number, source))?;
    let arc = Arc::new(dict);
    local.insert(
        seg.header.number,
        DecodedSegment::PatternDictionary(arc.clone()),
    )?;
    Ok(arc)
}

/// Parse a custom Huffman-table segment (type 53, T.88 §7.4.13) and register it
/// in `local` for later symbol dictionaries / text regions to reference.
pub(crate) fn decode_tables_into(
    seg: &ParsedSegment<'_>,
    local: &mut SegmentStore,
    limits: &DecodeLimits,
) -> Result<(), DecodeError> {
    let table = crate::decode::huffman::parse_custom_table(seg.data, limits)
        .map_err(|source| annotate(seg.header.number, source))?;
    local.insert(
        seg.header.number,
        DecodedSegment::HuffmanTable(Arc::new(table)),
    )
}

/// Attach the segment number to a bare parse error; other errors pass through.
fn annotate(segment: u32, err: DecodeError) -> DecodeError {
    match err {
        DecodeError::Parse(source) => DecodeError::Segment { segment, source },
        other => other,
    }
}

/// Reusable per-document scratch threaded through [`run_segments`]. The page
/// list, height-flags, segment store, and recyclable-bitmap pool are pooled in
/// the [`DecoderContext`] for the zero-copy `decode_embedded_into` path, or
/// created fresh for the by-value entry points.
struct SegmentScratch {
    pages: Vec<DecodedPage>,
    unknown_height: Vec<bool>,
    store: SegmentStore,
    bitmap_pool: Vec<MonoBitmap>,
}

/// Draw a `width` × `height` bitmap from `pool`, recycling a pooled buffer when
/// one is free, otherwise allocating. The recycled buffer is reset to all-white
/// (or all-black if `fill_black`).
fn acquire_bitmap(
    pool: &mut Vec<MonoBitmap>,
    width: u32,
    height: u32,
    fill_black: bool,
    limits: &DecodeLimits,
) -> Result<MonoBitmap, DecodeError> {
    match pool.pop() {
        Some(mut bm) => {
            bm.reset(width, height, fill_black, limits)?;
            Ok(bm)
        }
        None => MonoBitmap::new(width, height, fill_black, limits),
    }
}

/// Process a parsed document, resolving referred symbol dictionaries against an
/// optional set of shared globals (jbig2decplan.md §6, §16, §17).
pub fn process_document_with_globals(
    doc: &ParsedDocument<'_>,
    globals: Option<&DecodedGlobals>,
    limits: &DecodeLimits,
    ctx: &mut DecoderContext,
) -> Result<Vec<DecodedPage>, DecodeError> {
    // By-value entry point: pool the store and height flags across documents,
    // but allocate page bitmaps fresh (they are moved out to the caller) — the
    // empty pool makes `acquire_bitmap` always allocate.
    let mut unknown_height = std::mem::take(&mut ctx.unknown_height_scratch);
    unknown_height.clear();
    let mut store = std::mem::take(&mut ctx.segment_store);
    store.clear();
    let scratch = run_segments(
        doc,
        globals,
        limits,
        ctx,
        SegmentScratch {
            pages: Vec::new(),
            unknown_height,
            store,
            bitmap_pool: Vec::new(),
        },
    )?;
    let SegmentScratch {
        pages,
        mut unknown_height,
        mut store,
        bitmap_pool: _,
    } = scratch;
    store.clear();
    ctx.segment_store = store;
    unknown_height.clear();
    ctx.unknown_height_scratch = unknown_height;
    Ok(pages)
}

/// Decode a document straight into a caller-provided page bitmap, pooling every
/// intermediate buffer in `ctx` so the steady state does not allocate
/// (jbig2decplan.md §13, the zero-alloc renderer API). The first page's content
/// is swapped into `target` (reusing its backing allocation); all page bitmaps
/// are recycled into the context pool for the next call. Returns `false` when
/// the document produced no page.
pub fn process_document_into(
    doc: &ParsedDocument<'_>,
    globals: Option<&DecodedGlobals>,
    limits: &DecodeLimits,
    ctx: &mut DecoderContext,
    target: &mut MonoBitmap,
) -> Result<bool, DecodeError> {
    let mut pages = std::mem::take(&mut ctx.pages_scratch);
    let mut bitmap_pool = std::mem::take(&mut ctx.bitmap_pool);
    // Recycle any page bitmaps left over from a prior error path.
    for p in pages.drain(..) {
        bitmap_pool.push(p.bitmap);
    }
    let mut unknown_height = std::mem::take(&mut ctx.unknown_height_scratch);
    unknown_height.clear();
    let mut store = std::mem::take(&mut ctx.segment_store);
    store.clear();

    let scratch = run_segments(
        doc,
        globals,
        limits,
        ctx,
        SegmentScratch {
            pages,
            unknown_height,
            store,
            bitmap_pool,
        },
    )?;
    let SegmentScratch {
        mut pages,
        mut unknown_height,
        mut store,
        mut bitmap_pool,
    } = scratch;

    let produced = !pages.is_empty();
    if produced {
        // Swap the first page's content into the caller's buffer (reusing the
        // caller's backing allocation for the pool), then recycle every page
        // bitmap for the next decode.
        std::mem::swap(target, &mut pages[0].bitmap);
    }
    for p in pages.drain(..) {
        bitmap_pool.push(p.bitmap);
    }

    ctx.pages_scratch = pages;
    ctx.bitmap_pool = bitmap_pool;
    store.clear();
    ctx.segment_store = store;
    unknown_height.clear();
    ctx.unknown_height_scratch = unknown_height;
    Ok(produced)
}

/// The per-segment decoding loop shared by [`process_document_with_globals`] and
/// [`process_document_into`]. Takes ownership of the [`SegmentScratch`] so the
/// loop body works on plain locals, and hands it back on success (an early error
/// return drops the scratch — off the happy path, pooling does not matter).
fn run_segments(
    doc: &ParsedDocument<'_>,
    globals: Option<&DecodedGlobals>,
    limits: &DecodeLimits,
    ctx: &mut DecoderContext,
    scratch: SegmentScratch,
) -> Result<SegmentScratch, DecodeError> {
    let SegmentScratch {
        mut pages,
        mut unknown_height,
        mut store,
        mut bitmap_pool,
    } = scratch;
    let mut current: Option<usize> = None;
    let globals_store = globals.map(|g| g.store());

    // Surface any parse-time recoveries (Compatible mode, jbig2decplan.md §20).
    ctx.recovery_events.clear();
    ctx.recovery_events.extend(doc.recovery.iter().cloned());

    for seg in &doc.segments {
        let ty = seg.header.segment_type();
        match ty {
            Some(SegmentType::SymbolDictionary) => {
                decode_symbol_dict_into(seg, &mut store, globals_store, limits, ctx)?;
                continue;
            }
            Some(
                SegmentType::ImmediateTextRegion
                | SegmentType::ImmediateLosslessTextRegion
                | SegmentType::IntermediateTextRegion,
            ) => {
                let symbols = store.gather_symbols(
                    seg.header.number,
                    &seg.header.referred_to,
                    globals_store,
                )?;
                let tables = store.gather_huffman_tables(&seg.header.referred_to, globals_store);
                // Ensure the refinement bank is sized, then split disjoint field
                // borrows for the integer, IAID, and refinement context banks.
                if ctx.refinement_contexts.len() < REFINEMENT_CONTEXT_COUNT {
                    ctx.refinement_contexts
                        .resize(REFINEMENT_CONTEXT_COUNT, Default::default());
                }
                let int_ctx = &mut ctx.integer_contexts;
                let iaid_ctx = &mut ctx.iaid_contexts;
                let refine_ctx = &mut ctx.refinement_contexts[..REFINEMENT_CONTEXT_COUNT];
                let result = decode_text_region(
                    seg.data, &symbols, &tables, limits, int_ctx, iaid_ctx, refine_ctx,
                )
                .map_err(|source| annotate(seg.header.number, source))?;
                if let Some(spare) = place_or_store(
                    matches!(ty, Some(SegmentType::IntermediateTextRegion)),
                    seg.header.number,
                    &mut store,
                    &mut pages,
                    &unknown_height,
                    current,
                    seg.header.page_association,
                    result.bitmap,
                    result.x,
                    result.y,
                    result.comb_operator,
                    limits,
                )? {
                    bitmap_pool.push(spare);
                }
                continue;
            }
            _ => {}
        }
        match ty {
            Some(SegmentType::PageInformation) => {
                let info = parse_page_info(seg.data).map_err(|source| DecodeError::Segment {
                    segment: seg.header.number,
                    source,
                })?;
                let is_unknown = info.has_unknown_height();
                // A page of unknown height starts empty and grows as stripes /
                // regions arrive; a known-height page is allocated up front.
                let init_height = if is_unknown { 0 } else { info.height };
                if !is_unknown {
                    check_page_pixels(&info, limits)?;
                }
                let bitmap = acquire_bitmap(
                    &mut bitmap_pool,
                    info.width,
                    init_height,
                    info.default_pixel,
                    limits,
                )?;
                pages.push(DecodedPage {
                    page_number: seg.header.page_association,
                    bitmap,
                });
                unknown_height.push(is_unknown);
                current = Some(pages.len() - 1);
            }
            Some(
                SegmentType::ImmediateGenericRegion
                | SegmentType::ImmediateLosslessGenericRegion
                | SegmentType::IntermediateGenericRegion,
            ) => {
                let mut region =
                    parse_generic_region(seg.data).map_err(|source| DecodeError::Segment {
                        segment: seg.header.number,
                        source,
                    })?;
                if seg.header.is_unknown_length() {
                    // §7.2.7 / §6.2.6: the data part ends with a four-byte row
                    // count; the region's true height is that count (<= the
                    // region-info height). Trim it from the coded data.
                    let dlen = region.data.len();
                    if dlen < 4 {
                        return Err(DecodeError::Malformed {
                            reason: "unknown-length generic region missing row count",
                        });
                    }
                    let rc = &region.data[dlen - 4..];
                    let row_count = u32::from_be_bytes([rc[0], rc[1], rc[2], rc[3]]);
                    if row_count > region.height {
                        return Err(DecodeError::Malformed {
                            reason: "unknown-length row count exceeds region height",
                        });
                    }
                    region.height = row_count;
                    region.data = &region.data[..dlen - 4];
                }
                let mut region_bm =
                    acquire_bitmap(&mut bitmap_pool, region.width, region.height, false, limits)?;
                let (gctx, scr) = ctx.generic_and_scratch();
                decode_generic_region_into(&region, limits, gctx, scr, &mut region_bm)?;
                if let Some(spare) = place_or_store(
                    matches!(ty, Some(SegmentType::IntermediateGenericRegion)),
                    seg.header.number,
                    &mut store,
                    &mut pages,
                    &unknown_height,
                    current,
                    seg.header.page_association,
                    region_bm,
                    region.x,
                    region.y,
                    region.comb_operator,
                    limits,
                )? {
                    bitmap_pool.push(spare);
                }
            }
            // §7.4.10 end of stripe: a 4-byte end row. For an unknown-height
            // page this communicates the page size, so grow to include it.
            Some(SegmentType::EndOfStripe) => {
                if seg.data.len() >= 4 {
                    let end_row =
                        u32::from_be_bytes([seg.data[0], seg.data[1], seg.data[2], seg.data[3]]);
                    if let Some(idx) = current.filter(|&i| unknown_height[i]) {
                        let target_h = end_row.saturating_add(1);
                        pages[idx].bitmap.grow_to_height(
                            target_h,
                            limits.max_page_pixels,
                            limits,
                        )?;
                    }
                }
            }
            // Segments the self-decoder ignores structurally.
            Some(
                SegmentType::EndOfPage
                | SegmentType::EndOfFile
                | SegmentType::Profiles
                | SegmentType::Extension,
            ) => {}
            // Not handled in Phase 1.
            Some(
                SegmentType::SymbolDictionary
                | SegmentType::IntermediateTextRegion
                | SegmentType::ImmediateTextRegion
                | SegmentType::ImmediateLosslessTextRegion,
            ) => {
                return Err(DecodeError::Unsupported(UnsupportedFeature::SymbolCoding));
            }
            Some(SegmentType::PatternDictionary) => {
                decode_pattern_dict_into(seg, &mut store, limits, ctx)?;
            }
            Some(
                SegmentType::IntermediateHalftoneRegion
                | SegmentType::ImmediateHalftoneRegion
                | SegmentType::ImmediateLosslessHalftoneRegion,
            ) => {
                let region =
                    parse_halftone_region(seg.data).map_err(|source| DecodeError::Segment {
                        segment: seg.header.number,
                        source,
                    })?;
                let patterns = store
                    .pattern_dictionary(seg.header.number, &seg.header.referred_to, globals_store)?
                    .clone();
                let (generic, scratch) = ctx.generic_and_scratch();
                let region_bm =
                    decode_halftone_region(&region, &patterns, limits, generic, scratch)
                        .map_err(|source| annotate(seg.header.number, source))?;
                if let Some(spare) = place_or_store(
                    matches!(ty, Some(SegmentType::IntermediateHalftoneRegion)),
                    seg.header.number,
                    &mut store,
                    &mut pages,
                    &unknown_height,
                    current,
                    seg.header.page_association,
                    region_bm,
                    region.x,
                    region.y,
                    region.comb_operator,
                    limits,
                )? {
                    bitmap_pool.push(spare);
                }
            }
            Some(
                SegmentType::ImmediateGenericRefinementRegion
                | SegmentType::ImmediateLosslessGenericRefinementRegion
                | SegmentType::IntermediateGenericRefinementRegion,
            ) => {
                // T.88 §7.4.7.4: GRREFERENCE is the referred region segment's
                // auxiliary buffer (an intermediate region's stored bitmap) when
                // this segment refers to one, otherwise the page-buffer window
                // under the region box. GRREFERENCEDX/DY = 0 (Table 35).
                let region = parse_refinement_region(seg.data)
                    .map_err(|source| annotate(seg.header.number, source))?;
                let intermediate =
                    matches!(ty, Some(SegmentType::IntermediateGenericRefinementRegion));

                let (reference, ext_comb) = if seg.header.referred_to.is_empty() {
                    // No referred region: refine the page buffer window in place;
                    // §7.4.7.5 step 1 external combination operator is REPLACE.
                    let target = resolve_page(&mut pages, current, seg.header.page_association)?;
                    grow_if_unknown(
                        &mut pages[target].bitmap,
                        unknown_height[target],
                        region.y,
                        region.height,
                        limits,
                    )?;
                    let reference = page_reference_window(
                        &pages[target].bitmap,
                        region.x,
                        region.y,
                        region.width,
                        region.height,
                        limits,
                    )?;
                    (reference, 4u8)
                } else {
                    // Referred region: its retained bitmap is GRREFERENCE.
                    let src = store
                        .referred_region(&seg.header.referred_to, globals_store)
                        .ok_or(DecodeError::MissingReferredSegment {
                            segment: seg.header.number,
                            referred: seg.header.referred_to.first().copied().unwrap_or(0),
                        })?;
                    ((**src).clone(), region.comb_operator)
                };

                // §7.4.7.5 step 2: fresh arithmetic statistics per segment.
                let refine_ctx = ctx.refinement_contexts();
                let mut dec = ArithmeticDecoder::new(region.data);
                let refined = decode_refinement_region_templated(
                    &mut dec,
                    &reference,
                    region.width,
                    region.height,
                    0,
                    0,
                    region.grtemplate,
                    region.tpgron,
                    region.grat,
                    region.grat2,
                    refine_ctx,
                    limits,
                )
                .map_err(|source| annotate(seg.header.number, source))?;
                if let Some(spare) = place_or_store(
                    intermediate,
                    seg.header.number,
                    &mut store,
                    &mut pages,
                    &unknown_height,
                    current,
                    seg.header.page_association,
                    refined,
                    region.x,
                    region.y,
                    ext_comb,
                    limits,
                )? {
                    bitmap_pool.push(spare);
                }
            }
            Some(SegmentType::Tables) => {
                decode_tables_into(seg, &mut store, limits)?;
            }
            Some(SegmentType::ColorPalette) | Some(SegmentType::FileHeader) => {}
            None => {
                return Err(DecodeError::Unsupported(UnsupportedFeature::SegmentType(
                    seg.header.type_code,
                )));
            }
        }
    }

    Ok(SegmentScratch {
        pages,
        unknown_height,
        store,
        bitmap_pool,
    })
}

fn check_page_pixels(info: &PageInformation, limits: &DecodeLimits) -> Result<(), DecodeError> {
    let pixels =
        (info.width as u64)
            .checked_mul(info.height as u64)
            .ok_or(DecodeError::Overflow {
                operation: "page pixel count",
            })?;
    if pixels > limits.max_page_pixels {
        return Err(DecodeError::limit(LimitError::Pixels {
            what: "page",
            value: pixels,
            limit: limits.max_page_pixels,
        }));
    }
    Ok(())
}

/// Find the page a region should draw onto: prefer a page whose association
/// matches, else the most recently created page.
fn resolve_page(
    pages: &mut [DecodedPage],
    current: Option<usize>,
    association: u32,
) -> Result<usize, DecodeError> {
    if let Some(idx) = pages.iter().position(|p| p.page_number == association) {
        return Ok(idx);
    }
    current.ok_or(DecodeError::Unsupported(UnsupportedFeature::SegmentType(
        // A region before any page-information segment.
        38,
    )))
}

/// Composite a decoded region onto a page. When the region exactly covers a
/// blank page at the origin with an OR/REPLACE operator, move it in with no
/// per-word combine.
/// Compose `region` onto `page` and return the now-spare bitmap for recycling:
/// the region buffer after a combine, or (for the full-cover fast path that
/// swaps the region in) the page's former buffer.
fn compose_region(
    page: &mut MonoBitmap,
    mut region: MonoBitmap,
    x: u32,
    y: u32,
    comb_operator: u8,
) -> MonoBitmap {
    let op = combination_operator(comb_operator);
    let full_cover =
        x == 0 && y == 0 && region.width() == page.width() && region.height() == page.height();
    let movable =
        matches!(op, CombinationOperator::Or | CombinationOperator::Replace) && page_is_blank(page);
    if full_cover && movable {
        // Swap rather than assign so the page's old buffer is returned for reuse
        // instead of dropped.
        std::mem::swap(page, &mut region);
        return region;
    }
    page.combine(&region, x as i32, y as i32, op);
    region
}

/// Whether a region segment type is *intermediate* (its bitmap is retained as an
/// auxiliary buffer for a later refinement, not drawn on the page — T.88 §8.2).
#[inline]
fn is_intermediate(ty: SegmentType) -> bool {
    matches!(
        ty,
        SegmentType::IntermediateTextRegion
            | SegmentType::IntermediateHalftoneRegion
            | SegmentType::IntermediateGenericRegion
            | SegmentType::IntermediateGenericRefinementRegion
    )
}

/// Either store an intermediate region's bitmap as an auxiliary buffer, or
/// composite an immediate region onto its page.
#[allow(clippy::too_many_arguments)]
fn place_or_store(
    intermediate: bool,
    seg_number: u32,
    store: &mut SegmentStore,
    pages: &mut [DecodedPage],
    unknown_height: &[bool],
    current: Option<usize>,
    page_association: u32,
    region_bm: MonoBitmap,
    x: u32,
    y: u32,
    comb: u8,
    limits: &DecodeLimits,
) -> Result<Option<MonoBitmap>, DecodeError> {
    if intermediate {
        store.insert(seg_number, DecodedSegment::Region(Arc::new(region_bm)))?;
        Ok(None)
    } else {
        let target = resolve_page(pages, current, page_association)?;
        grow_if_unknown(
            &mut pages[target].bitmap,
            unknown_height[target],
            y,
            region_bm.height(),
            limits,
        )?;
        Ok(Some(compose_region(
            &mut pages[target].bitmap,
            region_bm,
            x,
            y,
            comb,
        )))
    }
}

/// Grow an unknown-height page so a region of height `h` placed at row `y`
/// fits (T.88 §7.4.8.5). A no-op for known-height pages.
fn grow_if_unknown(
    page: &mut MonoBitmap,
    unknown: bool,
    y: u32,
    h: u32,
    limits: &DecodeLimits,
) -> Result<(), DecodeError> {
    if unknown {
        let needed = (y as u64)
            .checked_add(h as u64)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(DecodeError::Overflow {
                operation: "striped page height",
            })?;
        page.grow_to_height(needed, limits.max_page_pixels, limits)?;
    }
    Ok(())
}

/// Whether every word of the page is zero (blank / all-white default).
fn page_is_blank(page: &MonoBitmap) -> bool {
    for y in 0..page.height() {
        if page.row(y).iter().any(|&w| w != 0) {
            return false;
        }
    }
    true
}

#[cfg(all(test, feature = "encode"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::encode::structs::PageInfo;

    #[test]
    fn parse_page_info_matches_encoder() {
        let payload = PageInfo {
            width: 640,
            height: 480,
            xres: 300,
            yres: 300,
            is_lossless: true,
            default_pixel: false,
            default_operator: 2,
            ..Default::default()
        }
        .to_bytes();
        let info = parse_page_info(&payload).unwrap();
        assert_eq!(info.width, 640);
        assert_eq!(info.height, 480);
        assert_eq!(info.x_resolution, 300);
        assert!(info.is_lossless);
        assert!(!info.default_pixel);
        assert_eq!(info.default_operator, 2);
    }

    #[test]
    fn default_pixel_bit_sets_black_page() {
        let payload = PageInfo {
            width: 8,
            height: 2,
            default_pixel: true,
            ..Default::default()
        }
        .to_bytes();
        let info = parse_page_info(&payload).unwrap();
        assert!(info.default_pixel);
    }
}
