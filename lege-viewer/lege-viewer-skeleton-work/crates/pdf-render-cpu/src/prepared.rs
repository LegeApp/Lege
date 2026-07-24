//! The request-specific CPU lowering stage (performance advice §1).
//!
//! `CompiledPage` describes *what* the PDF paints and is reusable across output
//! sizes and backends. `CpuPreparedPage` describes *how this render request*
//! executes on the CPU: geometry is already flattened into device space,
//! colors are resolved to premultiplied bytes, bounds are computed and culled
//! to the output, and each op is classified into a fast path. The executor
//! then walks compact commands with **no `Arc` indexing, transform
//! composition, or resource lookup in the hot loop** (advice §14).

use std::collections::HashMap;
#[cfg(feature = "profiling")]
use std::collections::HashSet;
use std::sync::Arc;

use pdf_font::FontProgram;
use pdf_page_ir::{
    CompiledPage, DeviceRect, DeviceSize, DisplayOp, GlyphRun, Matrix, Paint, PathData, PathVerb,
    Point, ShadingKind, ShadingResource, StrokeStyle, TilingPattern,
};

use crate::raster::FillRule;
use crate::stroke;

/// Worker-local parsed-font residency. A document shares each program's
/// `Arc<[u8]>` across compiled pages, so pointer identity remains stable while
/// this cache retains the corresponding parsed program.
#[derive(Debug, Default)]
pub(crate) struct FontProgramCache {
    page_ptr: usize,
    programs: HashMap<u32, Option<FontProgram>>,
}

impl FontProgramCache {
    fn take_for_page(&mut self, page: &CompiledPage) -> (HashMap<u32, Option<FontProgram>>, bool) {
        let page_ptr = page as *const CompiledPage as usize;
        if self.page_ptr == page_ptr {
            (std::mem::take(&mut self.programs), true)
        } else {
            self.page_ptr = page_ptr;
            self.programs.clear();
            (HashMap::new(), false)
        }
    }

    fn store_for_page(
        &mut self,
        page: &CompiledPage,
        mut programs: HashMap<u32, Option<FontProgram>>,
    ) {
        programs.retain(|_, program| {
            program
                .as_ref()
                .is_some_and(FontProgram::benefits_from_parse_cache)
        });
        self.page_ptr = page as *const CompiledPage as usize;
        self.programs = programs;
    }
}

/// Content identity of an embedded font program. The renderer's scheduler
/// compiles each page independently, so the same embedded font yields a *fresh*
/// `Arc<[u8]>` per page — backing-pointer identity would never match across
/// pages (measured: 0 cross-page hits). A content hash of the program bytes
/// *does* match, so the parse is shared across every page that embeds the font.
///
/// The identity is a 128-bit one-pass content hash (two independent 64-bit
/// accumulators) plus the byte length and face index. At document scale (a few
/// hundred distinct programs) a 128-bit hash is collision-free with margin to
/// spare, so no stored-byte verification is needed — which is what keeps the
/// entry from retaining a second copy of the program bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FontProgramKey {
    h0: u64,
    h1: u64,
    len: usize,
    face: u32,
}

impl FontProgramKey {
    fn for_resource(resource: &pdf_page_ir::FontResource) -> Self {
        let (h0, h1) = content_hash_128(&resource.program);
        Self {
            h0,
            h1,
            len: resource.program.len(),
            face: resource.face_index,
        }
    }
}

/// A fast, non-cryptographic 128-bit content hash: two multiply-mix
/// accumulators with distinct constants over the byte stream, eight bytes at a
/// time. Fast enough (multiple GB/s) that hashing each embedded program once per
/// page is negligible beside parsing it, while 128 bits of output make a
/// collision between two distinct programs vanishingly improbable at these
/// sizes.
fn content_hash_128(bytes: &[u8]) -> (u64, u64) {
    const P0: u64 = 0x9E37_79B9_7F4A_7C15;
    const P1: u64 = 0xC2B2_AE3D_27D4_EB4F;
    let mut a: u64 = 0xCBF2_9CE4_8422_2325 ^ (bytes.len() as u64).wrapping_mul(P0);
    let mut b: u64 = 0x1000_0000_1B3 ^ (bytes.len() as u64).wrapping_mul(P1);
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        // `chunks_exact(8)` guarantees the length; the fallback is dead.
        let w = u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
        a = (a ^ w).wrapping_mul(P0).rotate_left(31);
        b = (b.rotate_left(27) ^ w).wrapping_mul(P1);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut buf = [0u8; 8];
        buf[..rem.len()].copy_from_slice(rem);
        let w = u64::from_le_bytes(buf);
        a = (a ^ w).wrapping_mul(P0).rotate_left(31);
        b = (b.rotate_left(27) ^ w).wrapping_mul(P1);
    }
    // Avalanche.
    a ^= a >> 33;
    a = a.wrapping_mul(P1);
    a ^= a >> 29;
    b ^= b >> 31;
    b = b.wrapping_mul(P0);
    b ^= b >> 27;
    (a, b)
}

/// Lock a cache mutex, recovering from poisoning.
///
/// POLICY — the single never-panic mutex rule for this backend, referenced by
/// every `Mutex` use in this module: a poisoned lock means another worker
/// panicked mid-operation, and that panic was already caught at the
/// page/render boundary. Each mutex here guards self-consistent cache state
/// (a map plus byte/clock accounting) whose worst post-panic inconsistency is
/// a stale byte charge, which the LRU sweep self-corrects. Reusing the state
/// is safe; turning an already-recovered page into a second panic is not.
fn lock_unpoisoned<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A document/render-session-scoped, thread-safe cache of parsed font programs,
/// shared by every render worker of one backend.
///
/// The worker-local [`FontProgramCache`] only survives repeated renders of the
/// *same* compiled page; different pages of a document that share a font each
/// re-parse it. Type 1 and bare-CFF programs are expensive to parse (the type1
/// corpus page spends ~129 ms parsing 19 programs), so those — and only those,
/// matching [`FontProgram::benefits_from_parse_cache`] — are retained here so
/// the parse is paid once per document rather than once per page.
///
/// **Identity** is a 128-bit content hash of the program bytes plus length and
/// face index ([`FontProgramKey`]): the scheduler compiles each page
/// independently, so the same embedded font reaches different pages through
/// different stream objects — pointer identity measured zero cross-page hits,
/// content identity shares fully. **Bound**: LRU by retained parsed
/// bytes, sharded so the cap is enforced per shard. **Sharing**: interior
/// mutability via per-shard `Mutex`; parsing happens outside the lock, so a
/// shard is held only for a map lookup/insert. `FontProgram` clones are cheap
/// `Arc` bumps.
#[derive(Debug)]
pub struct SharedFontProgramCache {
    shards: Box<[std::sync::Mutex<FontCacheShard>]>,
    per_shard_bytes: usize,
    /// Lifetime tallies for observability (a Type 1 program served without a
    /// reparse, and a program inserted after a parse). Relaxed: diagnostics only.
    hits: std::sync::atomic::AtomicU64,
    inserts: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Default)]
struct FontCacheShard {
    map: HashMap<FontProgramKey, FontCacheEntry>,
    bytes: usize,
    clock: u64,
}

#[derive(Debug)]
struct FontCacheEntry {
    program: FontProgram,
    charge: usize,
    last_used: u64,
}

impl SharedFontProgramCache {
    /// Default total budget. Embedded font programs run tens of KB each, so
    /// 48 MiB retains hundreds of distinct programs — far beyond any single
    /// document's font set — while bounding worst-case residency. Chosen inside
    /// the 32–64 MiB band the task prescribes.
    const DEFAULT_TOTAL_BYTES: usize = 48 * 1024 * 1024;
    const SHARDS: usize = 8;

    pub fn new(total_bytes: usize) -> Self {
        let shards = (0..Self::SHARDS)
            .map(|_| std::sync::Mutex::new(FontCacheShard::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            per_shard_bytes: (total_bytes / Self::SHARDS).max(1),
            hits: std::sync::atomic::AtomicU64::new(0),
            inserts: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Lifetime `(hits, inserts)` — a served parse avoided, and a program
    /// parsed then retained. Diagnostics for the parse-cache report.
    pub fn stats(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.hits.load(Relaxed), self.inserts.load(Relaxed))
    }

    fn shard(&self, key: &FontProgramKey) -> &std::sync::Mutex<FontCacheShard> {
        &self.shards[(key.h0 >> 32) as usize % Self::SHARDS]
    }

    /// Serve the cached program for `key` (a 128-bit content identity).
    fn get(&self, key: &FontProgramKey) -> Option<FontProgram> {
        let mut shard = lock_unpoisoned(self.shard(key));
        shard.clock += 1;
        let clock = shard.clock;
        let entry = shard.map.get_mut(key)?;
        entry.last_used = clock;
        let program = entry.program.clone();
        drop(shard);
        self.hits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(program)
    }

    fn insert(&self, key: FontProgramKey, program: FontProgram, charge: usize) {
        self.inserts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut shard = lock_unpoisoned(self.shard(&key));
        shard.clock += 1;
        let clock = shard.clock;
        if let Some(prev) = shard.map.insert(
            key,
            FontCacheEntry {
                program,
                charge,
                last_used: clock,
            },
        ) {
            shard.bytes = shard.bytes.saturating_sub(prev.charge);
        }
        shard.bytes += charge;
        // Evict least-recently-used entries until back under the per-shard cap.
        // Font sets are small and stable, so this rarely runs.
        while shard.bytes > self.per_shard_bytes && shard.map.len() > 1 {
            let Some(victim) = shard
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if let Some(removed) = shard.map.remove(&victim) {
                shard.bytes = shard.bytes.saturating_sub(removed.charge);
            }
        }
    }

    /// Test-only view of the total retained entry count across shards.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| lock_unpoisoned(s).map.len())
            .sum()
    }
}

impl Default for SharedFontProgramCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_TOTAL_BYTES)
    }
}

/// Identity of one rendered glyph coverage bitmap (PDFium `CFX_GlyphCache`
/// analog, `core/fxge/cfx_glyphcache.cpp`). The pen translation is deliberately
/// **absent**: coverage is position-independent once the glyph origin is snapped
/// to the pixel grid, so every occurrence of a glyph at a given transform shares
/// one bitmap (the mechanism that collapses per-glyph raster to a blit).
///
/// - `font` — the 128-bit content identity of the embedded program (so the same
///   glyph shared across pages of a document hits, matching the parse cache).
/// - `glyph` — glyph index within that program.
/// - `la..ld` — the effective device-space linear map applied to the design-unit
///   outline (`(font_size/upem)·CTM_linear`), quantized ×10000 as PDFium
///   quantizes its matrix a,b,c,d. This folds in font size, page scale, and any
///   skew/rotation; two runs with the same quantized map produce identical
///   coverage.
/// - `hinted` — grid-fitted vs exact. Hinted outlines depend on ppem (implied by
///   `ld`) and produce different pixels, so they must not share a slot with the
///   unhinted outline (the review's explicit requirement).
///
/// - `sx`, `sy` — the quantized sub-pixel phase of the glyph origin (fractional
///   part × `GLYPH_SUBPIXEL_STEPS`, PDFium's LCD 3-phase idea generalized to a
///   small grid). Snapping the origin to whole pixels maximizes reuse but shifts
///   the anti-aliasing away from the exact PDF position; keeping a few phases per
///   axis holds positioning within a fraction of a pixel (near byte-parity with
///   the exact outline fill) while still collapsing occurrences to a handful of
///   shared bitmaps.
///
/// Fill rule is not in the key: glyph fills are always non-zero (both the cached
/// and the fallback path emit `FillRule::NonZero`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphCacheKey {
    font: FontProgramKey,
    glyph: u32,
    la: i32,
    lb: i32,
    lc: i32,
    ld: i32,
    sx: i16,
    sy: i16,
    hinted: bool,
}

/// A rendered glyph: a bbox-tight `u8` coverage bitmap plus the bearing offsets
/// of that box relative to the (integer, snapped) glyph origin. Blitting places
/// `cov` at device `(origin + (left, top))`. An empty glyph (a space, a missing
/// outline) is stored with `width == 0` so repeat occurrences hit and skip
/// rather than re-probing the font program each page.
#[derive(Debug)]
pub struct GlyphBitmap {
    /// Device-x of the bitmap's left column, relative to the glyph origin.
    pub left: i32,
    /// Device-y of the bitmap's top row, relative to the glyph origin.
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// `width * height` coverage bytes (255 = full), row-major.
    pub cov: Box<[u8]>,
}

impl GlyphBitmap {
    /// LRU charge: the coverage bytes plus a fixed per-entry overhead (key,
    /// `Arc` control block, map slot). Small glyphs are dominated by overhead,
    /// so this keeps the byte accounting honest against the cap.
    fn charge(&self) -> usize {
        self.cov.len() + std::mem::size_of::<Self>() + 64
    }
}

/// A document/render-session-scoped cache of rendered glyph coverage bitmaps —
/// PDFium mechanism #1 (`CFX_GlyphCache`). Same shape as
/// [`SharedFontProgramCache`]: sharded, per-shard `Mutex`, LRU by retained
/// coverage bytes, shared by every render worker of one backend. The expensive
/// work (outline extraction + curve flattening + edge build + scan conversion)
/// happens once per unique glyph; every other occurrence is a map probe and an
/// alpha blit.
#[derive(Debug)]
pub struct SharedGlyphCache {
    shards: Box<[std::sync::Mutex<GlyphCacheShard>]>,
    per_shard_bytes: usize,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
    inserts: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Default)]
struct GlyphCacheShard {
    map: HashMap<GlyphCacheKey, GlyphCacheEntry>,
    bytes: usize,
    clock: u64,
}

#[derive(Debug)]
struct GlyphCacheEntry {
    bitmap: Arc<GlyphBitmap>,
    charge: usize,
    last_used: u64,
}

impl SharedGlyphCache {
    /// Total budget. A body page's working set is a few hundred glyph×size
    /// bitmaps of a few hundred bytes each — well under 1 MiB — so 32 MiB holds
    /// many documents' worth while bounding worst-case residency (large display
    /// glyphs are routed to the outline path and never enter here). Chosen at the
    /// low end of the 32–64 MiB band the plan prescribes.
    const DEFAULT_TOTAL_BYTES: usize = 32 * 1024 * 1024;
    const SHARDS: usize = 8;

    pub fn new(total_bytes: usize) -> Self {
        let shards = (0..Self::SHARDS)
            .map(|_| std::sync::Mutex::new(GlyphCacheShard::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            per_shard_bytes: (total_bytes / Self::SHARDS).max(1),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
            inserts: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Lifetime `(hits, misses, inserts)` for the cache measurement report.
    pub fn stats(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.hits.load(Relaxed),
            self.misses.load(Relaxed),
            self.inserts.load(Relaxed),
        )
    }

    fn shard(&self, key: &GlyphCacheKey) -> &std::sync::Mutex<GlyphCacheShard> {
        // `font.h0` and the glyph index both vary within a page; mixing them
        // spreads a single font's glyphs across shards for better concurrency.
        let h = key.font.h0 ^ (key.glyph as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        &self.shards[(h >> 29) as usize % Self::SHARDS]
    }

    fn get(&self, key: &GlyphCacheKey) -> Option<Arc<GlyphBitmap>> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut shard = lock_unpoisoned(self.shard(key));
        shard.clock += 1;
        let clock = shard.clock;
        let Some(entry) = shard.map.get_mut(key) else {
            drop(shard);
            self.misses.fetch_add(1, Relaxed);
            return None;
        };
        entry.last_used = clock;
        let bitmap = entry.bitmap.clone();
        drop(shard);
        self.hits.fetch_add(1, Relaxed);
        Some(bitmap)
    }

    fn insert(&self, key: GlyphCacheKey, bitmap: Arc<GlyphBitmap>) {
        use std::sync::atomic::Ordering::Relaxed;
        self.inserts.fetch_add(1, Relaxed);
        let charge = bitmap.charge();
        let mut shard = lock_unpoisoned(self.shard(&key));
        shard.clock += 1;
        let clock = shard.clock;
        if let Some(prev) = shard.map.insert(
            key,
            GlyphCacheEntry {
                bitmap,
                charge,
                last_used: clock,
            },
        ) {
            shard.bytes = shard.bytes.saturating_sub(prev.charge);
        }
        shard.bytes += charge;
        while shard.bytes > self.per_shard_bytes && shard.map.len() > 1 {
            let Some(victim) = shard
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if let Some(removed) = shard.map.remove(&victim) {
                shard.bytes = shard.bytes.saturating_sub(removed.charge);
            }
        }
    }

    /// Test-only retained entry count across shards.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| lock_unpoisoned(s).map.len())
            .sum()
    }

    /// Test-only retained byte total across shards.
    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> usize {
        self.shards
            .iter()
            .map(|s| lock_unpoisoned(s).bytes)
            .sum()
    }
}

impl Default for SharedGlyphCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_TOTAL_BYTES)
    }
}

/// Profiling-only decoded-image residency shared across repeated lowering.
///
/// This deliberately is not a production resource cache: it exists only to
/// measure the renderer after encoded image decode has been made warm.
#[cfg(feature = "profiling")]
#[derive(Debug, Clone, Default)]
pub struct DecodedImageCache {
    entries: Arc<std::sync::Mutex<HashMap<DecodeCacheKey, CodecSamples>>>,
}

#[cfg(feature = "profiling")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DecodeCacheKey {
    data_ptr: usize,
    data_len: usize,
    codec: u8,
    width: u32,
    height: u32,
    bpc: u8,
    is_mask: bool,
    /// Device-footprint hint (Phase 2 reduced decode). A reduced decode cached
    /// for one destination scale must not be served for another, so the hint is
    /// part of the identity. `None` = full-resolution decode.
    target: Option<(u32, u32)>,
}

/// Identity of one decoded codec payload in the production image cache.
///
/// The core identity is a 128-bit content hash of the *encoded* bytes (same
/// scheme as [`FontProgramKey`]): `ImageIr::key` alone is not sufficient —
/// inline images synthesize `ResourceKey { 0, 0, variant }` which can collide
/// across pages, and soft masks (`ImageSMask`) carry no `ResourceKey` at all.
/// Hashing the payload costs ~sub-ms/MB against multi-ms decodes and makes the
/// identity exact, so no ResourceKey is needed in the key.
///
/// Every decode-relevant parameter is part of the identity:
/// - `codec`, declared `width`/`height`/`bpc`, `is_mask` — inputs to the
///   codec's descriptor;
/// - `target` — the reduced-decode footprint hint (a JPX decode at half
///   resolution must not be served for a full-resolution draw);
/// - `parms` — a hash over `CodecParms` (CCITT `/K`/`/Columns`/… and the
///   JBIG2 globals stream), which select entirely different bitstream
///   interpretations.
///
/// `/Decode` arrays are deliberately absent: they are applied at *sampling*
/// time (`build_sample_lut`), never during codec decode, so one decoded
/// payload serves every decode-array variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ImageCacheKey {
    h0: u64,
    h1: u64,
    len: usize,
    codec: u8,
    width: u32,
    height: u32,
    bpc: u8,
    is_mask: bool,
    target: Option<(u32, u32)>,
    parms: u64,
}

impl ImageCacheKey {
    #[allow(clippy::too_many_arguments)]
    fn new(
        kind: pdf_page_ir::ImageCodecKind,
        data: &[u8],
        width: u32,
        height: u32,
        bpc: u8,
        is_mask: bool,
        parms: Option<&pdf_page_ir::CodecParms>,
        target: Option<(u32, u32)>,
    ) -> Self {
        let (h0, h1) = content_hash_128(data);
        let parms_hash = match parms {
            None => 0,
            Some(p) => {
                // Fold the scalar CCITT fields and the JBIG2 globals content
                // into one 64-bit identity.
                let scalars = [
                    p.k as u32 as u64,
                    p.columns as u64,
                    p.rows as u64,
                    (p.black_is_1 as u64)
                        | ((p.byte_align as u64) << 1)
                        | ((p.end_of_line as u64) << 2)
                        | ((p.end_of_block as u64) << 3),
                ];
                let mut h: u64 = 0x9E37_79B9_7F4A_7C15;
                for s in scalars {
                    h = (h ^ s).wrapping_mul(0xC2B2_AE3D_27D4_EB4F).rotate_left(29);
                }
                if let Some(globals) = p.jbig2_globals.as_deref() {
                    let (g0, g1) = content_hash_128(globals);
                    h ^= g0 ^ g1.rotate_left(17);
                }
                h | 1 // never collide with the "no parms" 0
            }
        };
        Self {
            h0,
            h1,
            len: data.len(),
            codec: match kind {
                pdf_page_ir::ImageCodecKind::Dct => 0,
                pdf_page_ir::ImageCodecKind::Jpx => 1,
                pdf_page_ir::ImageCodecKind::Jbig2 => 2,
                pdf_page_ir::ImageCodecKind::CcittFax => 3,
            },
            width,
            height,
            bpc,
            is_mask,
            target,
            parms: parms_hash,
        }
    }
}

/// A document/render-session-scoped cache of **decoded image payloads** —
/// the production successor to the profiling-only [`DecodedImageCache`]
/// experiment (which remains for the profiling tooling's warm-decode A/B).
/// Same shape as [`SharedGlyphCache`]: sharded, per-shard `Mutex`, LRU by
/// retained decoded bytes, shared by every render worker of one backend.
///
/// The biggest win is image XObjects reused across pages and draws (a scanned
/// book's per-page background, a repeated logo, an MRC foreground/mask pair):
/// the expensive codec decode (DCT/JPX/JBIG2/CCITT) runs once per unique
/// payload; every other draw is a hash probe returning a shared `Arc`.
///
/// Serving a cached decode is byte-identical to decoding again by
/// construction — codecs are deterministic pure functions of
/// `(payload, descriptor, params)`, all of which are part of
/// [`ImageCacheKey`] — and is asserted by `image_cache_tests`.
#[derive(Debug)]
pub(crate) struct SharedImageCache {
    shards: Box<[std::sync::Mutex<ImageCacheShard>]>,
    per_shard_bytes: usize,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
    inserts: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Default)]
struct ImageCacheShard {
    map: HashMap<ImageCacheKey, ImageCacheEntry>,
    bytes: usize,
    clock: u64,
}

#[derive(Debug)]
struct ImageCacheEntry {
    decoded: CodecSamples,
    charge: usize,
    last_used: u64,
}

impl SharedImageCache {
    /// Total budget: 96 MiB. Decoded full-page images are the largest cached
    /// objects in the renderer — a 300-dpi RGB page scan decodes to ~25 MiB —
    /// so the budget is sized to hold ~3 such full-page payloads (the
    /// background + foreground + mask of an MRC scan working set) or hundreds
    /// of ordinary shared XObjects, while staying well under a typical
    /// per-process raster budget. Larger would mostly retain images no future
    /// page revisits; smaller would evict an MRC triple mid-document.
    const DEFAULT_TOTAL_BYTES: usize = 96 * 1024 * 1024;
    const SHARDS: usize = 8;

    pub(crate) fn new(total_bytes: usize) -> Self {
        let shards = (0..Self::SHARDS)
            .map(|_| std::sync::Mutex::new(ImageCacheShard::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            per_shard_bytes: (total_bytes / Self::SHARDS).max(1),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
            inserts: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Lifetime `(hits, misses, inserts)` for observability.
    pub(crate) fn stats(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.hits.load(Relaxed),
            self.misses.load(Relaxed),
            self.inserts.load(Relaxed),
        )
    }

    fn shard(&self, key: &ImageCacheKey) -> &std::sync::Mutex<ImageCacheShard> {
        &self.shards[(key.h0 >> 32) as usize % Self::SHARDS]
    }

    fn get(&self, key: &ImageCacheKey) -> Option<CodecSamples> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut shard = lock_unpoisoned(self.shard(key));
        shard.clock += 1;
        let clock = shard.clock;
        let Some(entry) = shard.map.get_mut(key) else {
            drop(shard);
            self.misses.fetch_add(1, Relaxed);
            return None;
        };
        entry.last_used = clock;
        let decoded = entry.decoded.clone();
        drop(shard);
        self.hits.fetch_add(1, Relaxed);
        Some(decoded)
    }

    fn insert(&self, key: ImageCacheKey, decoded: CodecSamples) {
        use std::sync::atomic::Ordering::Relaxed;
        self.inserts.fetch_add(1, Relaxed);
        // Decoded bytes plus fixed per-entry overhead (key, Arc control
        // block, map slot).
        let charge = decoded.samples.len() + std::mem::size_of::<ImageCacheEntry>() + 64;
        let mut shard = lock_unpoisoned(self.shard(&key));
        shard.clock += 1;
        let clock = shard.clock;
        if let Some(prev) = shard.map.insert(
            key,
            ImageCacheEntry {
                decoded,
                charge,
                last_used: clock,
            },
        ) {
            shard.bytes = shard.bytes.saturating_sub(prev.charge);
        }
        shard.bytes += charge;
        while shard.bytes > self.per_shard_bytes && shard.map.len() > 1 {
            let Some(victim) = shard
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if let Some(removed) = shard.map.remove(&victim) {
                shard.bytes = shard.bytes.saturating_sub(removed.charge);
            }
        }
    }

    /// Test-only retained entry count across shards.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| lock_unpoisoned(s).map.len())
            .sum()
    }

    /// Test-only retained byte total across shards.
    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> usize {
        self.shards.iter().map(|s| lock_unpoisoned(s).bytes).sum()
    }
}

impl Default for SharedImageCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_TOTAL_BYTES)
    }
}

/// A shading paint prepared for one render request: the ramp resolved to
/// bytes, plus the device→shading-space transform and axis geometry needed to
/// map each device pixel to a ramp index (advice §1 — resolved once, then a
/// per-pixel kernel). The per-pixel evaluation is the documented baseline;
/// span-linear ramp stepping is the noted optimization seam.
#[derive(Debug, Clone)]
pub struct PreparedShading {
    pub kind: ShadingSpanKind,
    /// Maps a device point back into the shading's coordinate space.
    pub inv: Matrix,
    /// Straight RGBA color store: the ramp for Axial/Radial (index 0 =
    /// domain start), the row-major sample grid for `Grid`, the row-major
    /// pre-rasterized device pixels for `Layer`.
    pub ramp: Vec<[u8; 4]>,
    /// Extend past the axis start / end (Axial/Radial only).
    pub extend: [bool; 2],
    /// `/Background` straight RGBA, painted where the shading geometry does
    /// not reach (pattern fills only; ignored for `sh` per §8.7.4.3).
    pub background: Option<[u8; 4]>,
    /// The shading's `/BBox` clip (§8.7.4.3) as a device-space axis-aligned
    /// box `[x0, y0, x1, y1]`: pixels outside it are not painted. Exact for
    /// axis-aligned / flipped CTMs (the common case); a conservative AABB of
    /// the transformed box under rotation.
    pub bbox: Option<[f32; 4]>,
}

/// Axis geometry for the per-pixel parameter, precomputed in shading space.
#[derive(Debug, Clone, Copy)]
pub enum ShadingSpanKind {
    Axial { p0: [f64; 2], d: [f64; 2], dd: f64 },
    Radial { c0: [f64; 3], c1: [f64; 3] },
    /// Type 1 (function-based): `inv` maps device → `/Domain` space; the
    /// pre-sampled `gw × gh` grid lives in `ramp` (row-major, `t`-major).
    Grid { domain: [f64; 4], gw: usize, gh: usize },
    /// Mesh shadings (types 4–7), pre-rasterized at lowering into a
    /// device-space RGBA layer in `ramp` (row-major over `w × h`, alpha 0 =
    /// not painted). `(x0, y0)` is the layer's device origin.
    Layer { x0: i32, y0: i32, w: usize, h: usize },
}

/// Fast-path classification of a draw (advice §5). Extended as features land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawClass {
    /// Axis-aligned, integer-aligned, opaque rectangle → direct row fill, no
    /// coverage generation.
    OpaqueRect,
    /// Any other solid fill → analytic coverage + span dispatch.
    SolidPath,
}

/// One lowered draw command. Geometry lives in the page's shared arenas; this
/// record is small and pointer-free.
#[derive(Debug, Clone)]
pub struct PreparedCommand {
    /// Diagnostic attribution only: the containing construct this operation
    /// came from. Never read while painting — see
    /// [`crate::attribution`].
    pub origin: pdf_page_ir::PaintOrigin,
    pub class: DrawClass,
    /// `[start, end)` range into [`CpuPreparedPage::subpaths`].
    pub subpath_range: (u32, u32),
    pub rule: FillRule,
    /// Straight-alpha color bytes.
    pub rgb: [u8; 3],
    /// Premultiplied pixel for the opaque fast path (`[r, g, b, 255]`).
    pub premul: [u8; 4],
    /// Constant coverage-independent alpha (`ca`·color.a), 0..=255.
    pub alpha: u8,
    /// True when `alpha == 255` and the color is fully opaque.
    pub opaque: bool,
    /// Device-space integer bounds (already culled to the output and to the
    /// active clip's rectangular envelope).
    pub bounds: DeviceRect,
    /// Active clip at this command, if any (index into
    /// [`CpuPreparedPage::clips`]).
    pub clip: Option<u32>,
    /// Whether the active clip chain includes a non-rectangular clip that
    /// needs an `Alpha8` mask. Purely-rectangular clips are handled by
    /// `bounds` alone.
    pub clip_has_mask: bool,
    /// Blend mode (`Normal` uses the fast source-over path; separable modes
    /// use the general blend compositor).
    pub blend: pdf_page_ir::BlendMode,
    /// When set, the fill is painted with this shading (per-pixel color from
    /// the ramp) instead of the solid `rgb`; coverage still applies.
    pub shading: Option<Box<PreparedShading>>,
}

/// One placed occurrence of a cached glyph: a shared coverage bitmap and the
/// device coordinate of its top-left pixel (glyph origin already snapped to the
/// pixel grid and offset by the bitmap bearing).
#[derive(Debug, Clone)]
pub struct GlyphPlacement {
    pub bitmap: Arc<GlyphBitmap>,
    /// Device x of the bitmap's left column.
    pub dx: i32,
    /// Device y of the bitmap's top row.
    pub dy: i32,
}

/// A glyph run rendered from the shared glyph coverage cache (PDFium's
/// per-run accumulate-and-blit): a batch of cached-bitmap placements sharing one
/// solid color, alpha, clip, and blend. The heavy per-glyph raster is already
/// done and memoized; execution blits each bitmap through `blend_mask`.
#[derive(Debug, Clone)]
pub struct PreparedGlyphRun {
    /// Diagnostic attribution only: the containing construct this operation
    /// came from. Never read while painting — see
    /// [`crate::attribution`].
    pub origin: pdf_page_ir::PaintOrigin,
    pub placements: Vec<GlyphPlacement>,
    pub rgb: [u8; 3],
    pub alpha: u8,
    pub opaque: bool,
    /// Device bounds (run bbox intersected with the active clip envelope).
    pub bounds: DeviceRect,
    pub clip: Option<u32>,
    pub clip_has_mask: bool,
    pub blend: pdf_page_ir::BlendMode,
    /// A shading-pattern text fill: color each covered pixel from this shading
    /// instead of `rgb` (which is then a placeholder). `None` for solid text.
    pub shading: Option<Box<PreparedShading>>,
}

/// A clip in the persistent clip graph (advice §8). Dense `u32` ids; the
/// draw loop indexes vectors, never a hash map.
#[derive(Debug, Clone)]
pub struct PreparedClip {
    pub parent: Option<u32>,
    /// Intersection of this clip and all rectangular ancestors, culled to the
    /// output — the rectangular envelope of the whole chain.
    pub bounds: DeviceRect,
    pub kind: ClipKind,
    /// True if this clip or any ancestor is a path clip (needs a mask).
    pub has_mask: bool,
}

#[derive(Debug, Clone)]
pub enum ClipKind {
    /// Purely rectangular: fully captured by `bounds`, no mask.
    Rect,
    /// A non-rectangular path clip contributing an `Alpha8` mask.
    Path {
        subpath_range: (u32, u32),
        rule: FillRule,
    },
}

/// A composited transparency group (advice §9). `bounds` is the device-space
/// union of the group's content — the size of the bounded offscreen surface.
#[derive(Debug, Clone)]
pub struct PreparedGroup {
    pub bounds: DeviceRect,
    /// Constant alpha applied when compositing the group back.
    pub opacity: u8,
    pub blend: pdf_page_ir::BlendMode,
    pub isolated: bool,
    /// Knockout group (§11.6.6): every element composites against the
    /// group's *initial* backdrop, later elements replacing earlier ones
    /// where they overlap.
    pub knockout: bool,
}

/// A tiling-pattern fill prepared for one render request. The fill shape is a
/// device-space clip mask; the cell is replicated across the lattice that
/// covers the fill's device bounds, each instance rendered under its tile
/// transform (advice §1 — geometry resolved once, tiles enumerated up front).
#[derive(Debug, Clone)]
pub struct PreparedTiling {
    /// Diagnostic attribution only: the containing construct this operation
    /// came from. Never read while painting — see
    /// [`crate::attribution`].
    pub origin: pdf_page_ir::PaintOrigin,
    /// Device bounds of the fill (clipped to output and to the active clip).
    pub bounds: DeviceRect,
    pub clip: Option<u32>,
    pub clip_has_mask: bool,
    /// The fill path's device-space subpaths (for the shape mask).
    pub fill_subpaths: (u32, u32),
    pub fill_rule: FillRule,
    pub alpha: u8,
    pub cell: Arc<CompiledPage>,
    /// Pattern space → device.
    pub pattern_to_device: Matrix,
    pub uncolored: bool,
    /// Straight RGBA under-color for an uncolored (PaintType 2) pattern.
    pub under: [u8; 4],
    /// Device transforms, one per lattice tile that overlaps `bounds`.
    pub tiles: Vec<Matrix>,
    /// Blend mode compositing the pattern layer onto the backdrop (`Normal`
    /// keeps the integer source-over path).
    pub blend: pdf_page_ir::BlendMode,
}

/// One entry in the executable op stream: a draw, or a transparency-group
/// scope. Group nesting is expressed by a matching `BeginGroup`/`EndGroup`
/// pair; `BeginGroup.end` is the index of its `EndGroup` for O(1) skipping.
#[derive(Debug, Clone)]
pub enum PreparedOp {
    Draw(PreparedCommand),
    /// A run of cached glyph coverage bitmaps blitted at snapped origins.
    GlyphRun(Box<PreparedGlyphRun>),
    /// A tiling-pattern fill (rendered by replicating the cell).
    TiledFill(Box<PreparedTiling>),
    /// An image blit (sampled per device pixel).
    Image(Box<crate::image::PreparedImage>),
    BeginGroup {
        group: PreparedGroup,
        end: u32,
    },
    EndGroup,
    /// Render the ops `[i+1, content_end)` into a `bounds`-sized offscreen,
    /// derive the per-pixel mask (luminosity or alpha), and push it as the
    /// active soft mask; execution continues at `content_end`.
    PushSoftMask {
        kind: pdf_page_ir::MaskKind,
        /// `/TR` sampled to a 256-entry LUT (`None` = identity), applied
        /// after mask derivation (and to the outside-extent value).
        transfer: Option<pdf_page_ir::TransferLut>,
        content_end: u32,
        bounds: DeviceRect,
    },
    /// `/SMask /None`: push a "no mask" onto the soft-mask stack.
    PushSoftMaskNone,
    /// Pop the active soft mask (at the end of its graphics-state scope).
    PopSoftMask,
}

/// Observable degradations recorded while lowering a page — today, image
/// draws that were **dropped** because their codec (JPX/DCT/JBIG2/CCITT) could
/// not decode. Without this, a failed decode silently produced a blank draw:
/// a page whose only content was an undecodable image rendered white and
/// masqueraded as a clean pass (the "610 silent-blank JPX pages", DEFERRED.md
/// corpus item 1 / Workstream B3). Interior mutability lets the ctx-less
/// decode path (which reads `codecs`/`decode_limits` from the prepared page
/// immutably) record a degradation without a mutable reborrow. The page is
/// request-local and never crosses threads.
#[derive(Debug, Default)]
pub struct RenderDiagnostics {
    /// Count of draws dropped due to a codec-decode failure this page.
    degraded_draws: std::cell::Cell<u32>,
    /// A bounded sample of human-readable reasons (codec + error).
    notes: std::cell::RefCell<Vec<String>>,
}

impl RenderDiagnostics {
    /// Cap on retained note strings; the counter is always exact.
    const MAX_NOTES: usize = 32;

    /// Record one dropped draw and (up to the cap) its reason.
    pub(crate) fn note_degraded(&self, reason: String) {
        self.degraded_draws.set(self.degraded_draws.get() + 1);
        let mut notes = self.notes.borrow_mut();
        if notes.len() < Self::MAX_NOTES {
            notes.push(reason);
        }
    }

    pub fn degraded_draws(&self) -> u32 {
        self.degraded_draws.get()
    }

    pub fn notes(&self) -> std::cell::Ref<'_, Vec<String>> {
        self.notes.borrow()
    }
}

/// A page lowered for one CPU render request.
#[derive(Debug)]
pub struct CpuPreparedPage {
    pub size: DeviceSize,
    pub ops: Vec<PreparedOp>,
    pub clips: Vec<PreparedClip>,
    /// Flattened device-space points, shared by all commands and clips.
    pub points: Vec<[f32; 2]>,
    /// `(start, end)` ranges into `points`, one per subpath.
    pub subpaths: Vec<(usize, usize)>,
    /// The backend's injected codec registry (cheap Arc clone), used when
    /// lowering codec-encoded images — including inside nested
    /// tiling-pattern cells lowered during execution.
    pub codecs: pdf_image::CodecRegistry,
    /// Per-request decode bounds (derived from `RenderLimits`).
    pub decode_limits: pdf_image::DecodeLimits,
    /// Glyph grid-fitting policy for this request (fonts.md Font Phase 4).
    pub hinting: pdf_font::HintingPolicy,
    /// Degradations observed while lowering (dropped codec draws). Surfaced
    /// into `RenderStats` so a silent blank can never pass unnoticed.
    pub diagnostics: RenderDiagnostics,
    /// The backend's document-scoped decoded-image cache (cheap Arc clone),
    /// consulted by codec decodes — including inside nested tiling-pattern
    /// cells lowered during execution. `None` = decode per draw.
    pub(crate) image_cache: Option<Arc<SharedImageCache>>,
    /// Fine-grained timing and codec counters. Interior mutability is used
    /// because image decode helpers deliberately receive `&CpuPreparedPage`.
    #[cfg(feature = "profiling")]
    profile: std::cell::RefCell<pdf_profiling::ProfileReport>,
    /// Optional warm-decode state used only by profiling experiments.
    #[cfg(feature = "profiling")]
    pub(crate) decode_cache: Option<DecodedImageCache>,
}

impl CpuPreparedPage {
    #[cfg(feature = "profiling")]
    pub(crate) fn profile_report(&self) -> pdf_profiling::ProfileReport {
        self.profile.borrow().clone()
    }
}

/// Curve flattening tolerance as a fixed subdivision count. A device-error
/// adaptive flattener is a measurable later optimization; this is the correct,
/// simple baseline.
const CURVE_SEGMENTS: usize = 16;

/// Lower `page` for a specific device transform and output size. `codecs`
/// and `decode_limits` govern the decoding of codec-encoded images.
// In-crate callers all use richer entry points now; this remains the plain
// profiling/bench entry (`CpuBackend::prepare_profiled`).
#[cfg_attr(not(feature = "profiling"), allow(dead_code))]
pub fn lower(
    page: &CompiledPage,
    base: Matrix,
    size: DeviceSize,
    codecs: &pdf_image::CodecRegistry,
    decode_limits: &pdf_image::DecodeLimits,
    hinting: pdf_font::HintingPolicy,
) -> CpuPreparedPage {
    lower_impl(
        page,
        base,
        size,
        codecs,
        decode_limits,
        hinting,
        None,
        None,
        None,
        None,
        #[cfg(feature = "profiling")]
        None,
    )
}

/// Lower a nested tiling-pattern cell during execution.
///
/// Tile-invariant work is hoisted out of the per-tile-instance loop (A3,
/// "lower once" — the *geometry* must still be lowered per instance, see
/// `render_tiling`): the outer page's decoded-image cache is propagated so a
/// cell's codec images decode once per render rather than once per tile, and
/// the caller passes one `FontProgramCache` reused across every instance so a
/// text cell parses each embedded font once per render (parsing is
/// transform-independent, so reuse is byte-identical by construction).
pub(crate) fn lower_cell(
    page: &CompiledPage,
    base: Matrix,
    size: DeviceSize,
    outer: &CpuPreparedPage,
    font_cache: &mut FontProgramCache,
) -> CpuPreparedPage {
    lower_impl(
        page,
        base,
        size,
        &outer.codecs,
        &outer.decode_limits,
        outer.hinting,
        Some(font_cache),
        None,
        None,
        outer.image_cache.as_ref(),
        #[cfg(feature = "profiling")]
        outer.decode_cache.clone(),
    )
}

/// Lower while reusing parsed font programs owned by one render worker, backed
/// by a document-scoped shared parse cache for Type 1 / bare-CFF programs and a
/// document-scoped rendered-glyph coverage cache.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_with_font_cache(
    page: &CompiledPage,
    base: Matrix,
    size: DeviceSize,
    codecs: &pdf_image::CodecRegistry,
    decode_limits: &pdf_image::DecodeLimits,
    hinting: pdf_font::HintingPolicy,
    font_cache: &mut FontProgramCache,
    shared_fonts: Option<&SharedFontProgramCache>,
    shared_glyphs: Option<&SharedGlyphCache>,
    shared_images: Option<&Arc<SharedImageCache>>,
) -> CpuPreparedPage {
    lower_impl(
        page,
        base,
        size,
        codecs,
        decode_limits,
        hinting,
        Some(font_cache),
        shared_fonts,
        shared_glyphs,
        shared_images,
        #[cfg(feature = "profiling")]
        None,
    )
}

/// Lower while retaining decoded codec payloads across calls.
#[cfg(feature = "profiling")]
pub fn lower_with_decode_cache(
    page: &CompiledPage,
    base: Matrix,
    size: DeviceSize,
    codecs: &pdf_image::CodecRegistry,
    decode_limits: &pdf_image::DecodeLimits,
    hinting: pdf_font::HintingPolicy,
    decode_cache: DecodedImageCache,
) -> CpuPreparedPage {
    lower_impl(
        page,
        base,
        size,
        codecs,
        decode_limits,
        hinting,
        None,
        None,
        None,
        None,
        Some(decode_cache),
    )
}

/// Decode every unique codec payload reachable from a compiled page without
/// performing geometry lowering or raster work.
#[cfg(feature = "profiling")]
pub fn decode_page(
    page: &CompiledPage,
    codecs: &pdf_image::CodecRegistry,
    decode_limits: &pdf_image::DecodeLimits,
    hinting: pdf_font::HintingPolicy,
) -> pdf_profiling::ProfileReport {
    let start = std::time::Instant::now();
    let out = CpuPreparedPage {
        size: DeviceSize {
            width: 1,
            height: 1,
        },
        ops: Vec::new(),
        clips: Vec::new(),
        points: Vec::new(),
        subpaths: Vec::new(),
        codecs: codecs.clone(),
        decode_limits: decode_limits.clone(),
        hinting,
        diagnostics: RenderDiagnostics::default(),
        image_cache: None,
        profile: std::cell::RefCell::new(pdf_profiling::ProfileReport::new()),
        decode_cache: None,
    };
    let mut seen = HashSet::new();
    decode_page_images(page, &out, &mut seen);
    let mut profile = out.profile_report();
    profile.add_duration("codec.decode_only.total", start.elapsed());
    profile.increment("codec.decode_only.unique_payloads", seen.len() as u64);
    profile
}

#[cfg(feature = "profiling")]
fn decode_page_images(
    page: &CompiledPage,
    out: &CpuPreparedPage,
    seen: &mut HashSet<DecodeCacheKey>,
) {
    for image in page.images.iter() {
        decode_unique_payload(
            out,
            seen,
            image.codec,
            image.codec_data.as_deref(),
            image.width,
            image.height,
            image.bits_per_component,
            image.is_stencil,
            image.codec_parms.clone(),
        );
        if let Some(mask) = image.smask.as_deref() {
            decode_unique_mask(out, seen, mask);
        }
        if let Some(pdf_page_ir::ImageMask::Stencil(mask)) = image.mask.as_ref() {
            decode_unique_mask(out, seen, mask);
        }
    }
    for tiling in page.tilings.iter() {
        decode_page_images(&tiling.cell, out, seen);
    }
}

#[cfg(feature = "profiling")]
fn decode_unique_mask(
    out: &CpuPreparedPage,
    seen: &mut HashSet<DecodeCacheKey>,
    mask: &pdf_page_ir::ImageSMask,
) {
    decode_unique_payload(
        out,
        seen,
        mask.codec,
        mask.codec_data.as_deref(),
        mask.width,
        mask.height,
        mask.bits_per_component,
        true,
        mask.codec_parms.clone(),
    );
}

#[cfg(feature = "profiling")]
#[allow(clippy::too_many_arguments)]
fn decode_unique_payload(
    out: &CpuPreparedPage,
    seen: &mut HashSet<DecodeCacheKey>,
    kind: Option<pdf_page_ir::ImageCodecKind>,
    data: Option<&[u8]>,
    width: u32,
    height: u32,
    bpc: u8,
    is_mask: bool,
    parms: Option<pdf_page_ir::CodecParms>,
) {
    let (Some(kind), Some(data)) = (kind, data) else {
        return;
    };
    let key = DecodeCacheKey {
        data_ptr: data.as_ptr() as usize,
        data_len: data.len(),
        codec: match kind {
            pdf_page_ir::ImageCodecKind::Dct => 0,
            pdf_page_ir::ImageCodecKind::Jpx => 1,
            pdf_page_ir::ImageCodecKind::Jbig2 => 2,
            pdf_page_ir::ImageCodecKind::CcittFax => 3,
        },
        width,
        height,
        bpc,
        is_mask,
        // Warm-up decodes the payload at full resolution (it has no single draw
        // footprint); real per-draw reduced decodes carry a distinct key.
        target: None,
    };
    if seen.insert(key) {
        if let Some(decoded) =
            decode_codec_samples(out, kind, data, width, height, bpc, is_mask, parms, None, None)
        {
            out.profile
                .borrow_mut()
                .release_bytes(decoded.samples.len() as u64);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_impl(
    page: &CompiledPage,
    base: Matrix,
    size: DeviceSize,
    codecs: &pdf_image::CodecRegistry,
    decode_limits: &pdf_image::DecodeLimits,
    hinting: pdf_font::HintingPolicy,
    mut font_cache: Option<&mut FontProgramCache>,
    shared_fonts: Option<&SharedFontProgramCache>,
    shared_glyphs: Option<&SharedGlyphCache>,
    shared_images: Option<&Arc<SharedImageCache>>,
    #[cfg(feature = "profiling")] decode_cache: Option<DecodedImageCache>,
) -> CpuPreparedPage {
    #[cfg(feature = "profiling")]
    let lower_start = std::time::Instant::now();
    let mut out = CpuPreparedPage {
        size,
        ops: Vec::with_capacity(page.operations.len()),
        clips: Vec::new(),
        points: Vec::new(),
        subpaths: Vec::new(),
        codecs: codecs.clone(),
        decode_limits: decode_limits.clone(),
        hinting,
        diagnostics: RenderDiagnostics::default(),
        image_cache: shared_images.cloned(),
        #[cfg(feature = "profiling")]
        profile: std::cell::RefCell::new(pdf_profiling::ProfileReport::new()),
        #[cfg(feature = "profiling")]
        decode_cache,
    };
    let mut ctm = base;
    let mut ctm_stack: Vec<Matrix> = Vec::new();
    // The clip stack holds active clip ids; `Save`/`Restore` move the CTM,
    // while `PushClip`/`PopClip` move the clip stack (they are emitted
    // balanced by IR lowering).
    let mut clip_stack: Vec<u32> = Vec::new();
    // Open groups / soft-mask-content scopes, accumulating device bounds.
    let mut scope_stack: Vec<ScopeBuilder> = Vec::new();
    // Active soft masks, and the count captured at each Save so `Restore` can
    // pop the masks activated within that graphics-state scope (the IR leaves
    // the pop implicit, like a graphics-state revert).
    let mut soft_count: u32 = 0;
    let mut soft_scope: Vec<u32> = Vec::new();
    // Soft-mask depth at each open group's `BeginGroup`, so `EndGroup` can
    // unwind the §11.6.6 reset from inside the group.
    let mut soft_at_group_entry: Vec<u32> = Vec::new();
    // Parsed embedded font programs, one parse per font (not per run). A
    // worker rendering the exact same compiled page again hands this map back
    // intact; a different page starts with the original empty local cache.
    let (mut font_programs, font_page_cache_hit) = match font_cache.as_deref_mut() {
        Some(cache) => cache.take_for_page(page),
        None => (HashMap::new(), false),
    };
    #[cfg(feature = "profiling")]
    if font_page_cache_hit && !font_programs.is_empty() {
        let mut profile = out.profile.borrow_mut();
        profile.increment("lower.font.page_cache_hits", 1);
        profile.increment(
            "lower.font.page_cached_programs",
            font_programs.len() as u64,
        );
    }
    #[cfg(not(feature = "profiling"))]
    let _ = font_page_cache_hit;

    // Phase B glyph-coverage-cache scratch: a rasterizer reused for cache-miss
    // glyph bitmaps, and a per-font content-identity memo (one 128-bit hash per
    // font per page, not per run).
    let mut glyph_raster = crate::raster::RasterKernel::default();
    let mut glyph_font_ids: HashMap<u32, FontProgramKey> = HashMap::new();

    // Diagnostic attribution (see `crate::attribution`): the stack of enclosing
    // constructs. Painting never reads it; after each operation any ops it
    // appended are stamped with the innermost origin, which is cheaper and far
    // less invasive than threading a parameter through every `lower_*`.
    let mut origin_stack: Vec<pdf_page_ir::PaintOrigin> = Vec::new();

    for op in page.operations.iter() {
        let ops_before = out.ops.len();
        match op {
            DisplayOp::BeginPaintOrigin(o) => origin_stack.push(*o),
            DisplayOp::EndPaintOrigin => {
                origin_stack.pop();
            }
            DisplayOp::Save => {
                ctm_stack.push(ctm);
                soft_scope.push(soft_count);
            }
            DisplayOp::Restore => {
                if let Some(m) = ctm_stack.pop() {
                    ctm = m;
                }
                let target = soft_scope.pop().unwrap_or(0);
                while soft_count > target {
                    out.ops.push(PreparedOp::PopSoftMask);
                    soft_count -= 1;
                }
            }
            DisplayOp::ConcatTransform(m) => ctm = m.then(ctm),
            DisplayOp::PushClip { path, rule } => {
                push_clip(
                    &mut out,
                    &mut clip_stack,
                    &page.paths[path.index()],
                    ctm,
                    *rule,
                );
            }
            DisplayOp::PushClipText { runs } => {
                push_text_clip(
                    &mut out,
                    &mut clip_stack,
                    runs,
                    page,
                    ctm,
                    &mut font_programs,
                    shared_fonts,
                );
            }
            DisplayOp::PopClip => {
                clip_stack.pop();
            }
            DisplayOp::FillPath {
                path,
                paint,
                rule,
                alpha,
                blend,
            } => {
                let clip = clip_stack.last().copied();
                let b = match &page.paints[paint.index()] {
                    // Solid paint under any blend mode (Normal fast path,
                    // separable and non-separable via the general compositor).
                    Paint::Solid(color) => lower_fill(
                        &mut out,
                        &page.paths[path.index()],
                        ctm,
                        *rule,
                        *color,
                        *alpha,
                        clip,
                        *blend,
                        soft_count > 0,
                        None,
                    ),
                    // Shading pattern: fill the path shape, coloring per pixel
                    // from the shading ramp. Shading space maps to device via
                    // the pattern `/Matrix` composed onto the base transform.
                    Paint::Shading { shading, matrix } => {
                        match prepare_shading(
                            &page.shadings[shading.index()],
                            matrix.then(base),
                            size,
                            true,
                        ) {
                            Some(sh) => lower_fill(
                                &mut out,
                                &page.paths[path.index()],
                                ctm,
                                *rule,
                                pdf_page_ir::Color::BLACK,
                                *alpha,
                                clip,
                                *blend,
                                soft_count > 0,
                                Some(Box::new(sh)),
                            ),
                            None => None,
                        }
                    }
                    // Tiling pattern: replicate the compiled cell across the
                    // fill shape.
                    Paint::Pattern { tiling, matrix } => lower_tiled(
                        &mut out,
                        &page.paths[path.index()],
                        ctm,
                        *rule,
                        &page.tilings[tiling.index()],
                        matrix.then(base),
                        *alpha,
                        clip,
                        *blend,
                    ),
                };
                accumulate_scope_bounds(&mut scope_stack, b);
            }
            DisplayOp::StrokePath {
                path,
                paint,
                style,
                alpha,
                blend,
            } => {
                // A stroke may carry a pattern the same way a fill does. The
                // stroke outline is lowered as an ordinary path, so a shading
                // rides along on the draw command and colours it per pixel —
                // identical to the fill case above. pdfjs/issue968 draws its
                // 96 petals as `/Pattern CS /p1 SCN … S`, and dropping
                // non-solid stroke paint left the page as eight bare centres.
                // A *tiling* stroke would need the outline fed through
                // `lower_tiled`, which takes a fill shape; it stays skipped.
                let (color, stroke_shading) = match &page.paints[paint.index()] {
                    Paint::Solid(color) => (*color, None),
                    Paint::Shading { shading, matrix } => {
                        match prepare_shading(
                            &page.shadings[shading.index()],
                            matrix.then(base),
                            size,
                            true,
                        ) {
                            Some(sh) => (pdf_page_ir::Color::BLACK, Some(Box::new(sh))),
                            None => continue,
                        }
                    }
                    _ => continue,
                };
                let clip = clip_stack.last().copied();
                let b = lower_stroke(
                    &mut out,
                    &page.paths[path.index()],
                    ctm,
                    &page.stroke_styles[style.index()],
                    color,
                    *alpha,
                    clip,
                    *blend,
                    stroke_shading,
                );
                accumulate_scope_bounds(&mut scope_stack, b);
            }
            DisplayOp::DrawGlyphRun {
                run,
                paint,
                alpha,
                blend,
                stroke,
            } => {
                // Solid text fills paint `color`; a shading-pattern text fill
                // renders the glyph coverage and colors it per pixel from the
                // shading (attached after lowering). A tiling-pattern text fill
                // is not yet supported and is skipped.
                let (color, run_shading) = match &page.paints[paint.index()] {
                    Paint::Solid(color) => (*color, None),
                    Paint::Shading { shading, matrix } => {
                        match prepare_shading(
                            &page.shadings[shading.index()],
                            matrix.then(base),
                            size,
                            true,
                        ) {
                            Some(sh) => (pdf_page_ir::Color::BLACK, Some(Box::new(sh))),
                            None => continue,
                        }
                    }
                    _ => continue,
                };
                let color = &color;
                let glyph_op_start = out.ops.len();
                let gr = &page.glyph_runs[run.index()];
                // Text render modes (§9.3.1): fill for 0/2/4/6, stroke for
                // 1/2/5/6, nothing painted for 3 (invisible) and 7 (clip-only).
                let mode = gr.render_mode;
                let do_fill = matches!(mode, 0 | 2 | 4 | 6);
                let do_stroke = matches!(mode, 1 | 2 | 5 | 6) && stroke.is_some();
                if !do_fill && !do_stroke {
                    continue;
                }
                // Parse (once) the embedded outline program, if any.
                #[cfg(feature = "profiling")]
                let font_parse_start = std::time::Instant::now();
                #[cfg(feature = "profiling")]
                let font_was_cached = font_programs.contains_key(&gr.font.0);
                let program = resolve_font_program(&mut font_programs, page, gr.font, shared_fonts);
                #[cfg(feature = "profiling")]
                {
                    let mut profile = out.profile.borrow_mut();
                    if font_was_cached {
                        profile.increment("lower.font.program_cache_hits", 1);
                    } else {
                        profile.add_duration("lower.font.parse", font_parse_start.elapsed());
                        profile.increment("lower.font.parses", 1);
                    }
                }
                let clip = clip_stack.last().copied();
                let synth = GlyphSynthesis::for_font(&page.fonts[gr.font.index()]);
                let mut bounds: Option<DeviceRect> = None;
                if do_fill {
                    let b = match &program {
                        Some(prog) => {
                            // Cached path: pure-fill (render mode 0) runs whose glyphs
                            // are small and axis-aligned blit from the shared glyph
                            // coverage cache. `None` means the run is ineligible
                            // (rotated/skewed, oversized, or the cache is off), so it
                            // falls back to the exact outline fill.
                            // Synthetic bold/italic bypasses the shared glyph
                            // cache (its key is font+gid+transform; adding the
                            // synthesis axes buys nothing for the rare symbolic
                            // substitutes that need it) and the hinter.
                            let cached = match shared_glyphs {
                                Some(gc) if gr.render_mode == 0 && !synth.is_active() => {
                                    let font_id = *glyph_font_ids.entry(gr.font.0).or_insert_with(|| {
                                        FontProgramKey::for_resource(&page.fonts[gr.font.index()])
                                    });
                                    try_cached_glyph_run(
                                        &mut out,
                                        gr,
                                        prog,
                                        ctm,
                                        *color,
                                        *alpha,
                                        clip,
                                        *blend,
                                        gc,
                                        font_id,
                                        &mut glyph_raster,
                                    )
                                }
                                _ => None,
                            };
                            match cached {
                                Some(b) => b,
                                None => lower_glyph_outlines(
                                    &mut out, gr, prog, ctm, *color, *alpha, clip, *blend, synth,
                                ),
                            }
                        }
                        None => lower_glyph_boxes(&mut out, gr, ctm, *color, *alpha, clip, *blend),
                    };
                    // Attach the shading to the fill glyph-run op(s) just emitted
                    // (the three lowerings share one emitter with no shading param).
                    if let Some(sh) = &run_shading {
                        for op in &mut out.ops[glyph_op_start..] {
                            if let PreparedOp::GlyphRun(g) = op {
                                g.shading = Some(sh.clone());
                            }
                        }
                    }
                    bounds = union_rect(bounds, b);
                }
                // Stroke the glyph outlines (Tr 1/2/5/6). Only outline fonts can
                // be stroked; a box-substitute run (no program) is fill-only.
                if do_stroke
                    && let (Some(prog), Some(gs)) = (&program, stroke)
                {
                    let (stroke_color, stroke_shading) = match &page.paints[gs.paint.index()] {
                        Paint::Solid(c) => (*c, None),
                        Paint::Shading { shading, matrix } => {
                            match prepare_shading(
                                &page.shadings[shading.index()],
                                matrix.then(base),
                                size,
                                true,
                            ) {
                                Some(sh) => (pdf_page_ir::Color::BLACK, Some(Box::new(sh))),
                                None => (pdf_page_ir::Color::BLACK, None),
                            }
                        }
                        _ => (pdf_page_ir::Color::BLACK, None),
                    };
                    let b = lower_glyph_stroke(
                        &mut out,
                        gr,
                        prog,
                        ctm,
                        &page.stroke_styles[gs.style.index()],
                        stroke_color,
                        gs.alpha,
                        clip,
                        *blend,
                        synth,
                        stroke_shading,
                    );
                    bounds = union_rect(bounds, b);
                }
                accumulate_scope_bounds(&mut scope_stack, bounds);
            }
            DisplayOp::BeginTransparencyGroup { group } => {
                let g = &page.groups[group.index()];
                let begin_index = out.ops.len();
                out.ops.push(PreparedOp::BeginGroup {
                    group: PreparedGroup {
                        bounds: DeviceRect {
                            x: 0,
                            y: 0,
                            width: 0,
                            height: 0,
                        },
                        opacity: to_u8(g.opacity),
                        blend: g.blend,
                        isolated: g.isolated,
                        knockout: g.knockout,
                    },
                    end: 0,
                });
                // §11.6.6: the content of a transparency group starts with the
                // soft mask reset to None. The mask in force at the invocation
                // applies to the *group's* composite instead (see
                // `composite_group`), so leaving it active here would apply it
                // twice. The interpreter already resets alpha and blend for the
                // same reason; this is the third member of that set.
                //
                // Many producers also write `/SMask /None` as the group's first
                // `gs` — `0041790.pdf` does — which made the omission invisible
                // until a file relied on the reset being implicit.
                soft_at_group_entry.push(soft_count);
                out.ops.push(PreparedOp::PushSoftMaskNone);
                soft_count += 1;
                scope_stack.push(ScopeBuilder {
                    begin_index,
                    bounds: None,
                });
            }
            DisplayOp::EndTransparencyGroup => {
                if let Some(gb) = scope_stack.pop() {
                    // Drop the §11.6.6 reset (plus anything the content pushed
                    // and did not pop) while still *inside* the group, so the
                    // executor sees a balanced stack and the mask that was
                    // active at `BeginGroup` is back on top for the composite.
                    let target = soft_at_group_entry.pop().unwrap_or(soft_count);
                    while soft_count > target {
                        out.ops.push(PreparedOp::PopSoftMask);
                        soft_count -= 1;
                    }
                    let end_index = out.ops.len() as u32;
                    out.ops.push(PreparedOp::EndGroup);
                    let bounds = gb.bounds.unwrap_or(DeviceRect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    });
                    if let PreparedOp::BeginGroup { group, end } = &mut out.ops[gb.begin_index] {
                        group.bounds = bounds;
                        *end = end_index;
                    }
                    // A nested group contributes its bounds to its parent.
                    accumulate_scope_bounds(&mut scope_stack, Some(bounds));
                }
            }
            DisplayOp::BeginSoftMask { kind, transfer } => {
                let begin_index = out.ops.len();
                out.ops.push(PreparedOp::PushSoftMask {
                    kind: *kind,
                    transfer: transfer.clone(),
                    content_end: 0,
                    bounds: DeviceRect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    },
                });
                scope_stack.push(ScopeBuilder {
                    begin_index,
                    bounds: None,
                });
            }
            DisplayOp::EndSoftMask => {
                if let Some(sb) = scope_stack.pop() {
                    let content_end = out.ops.len() as u32;
                    let bounds = sb.bounds.unwrap_or(DeviceRect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    });
                    if let PreparedOp::PushSoftMask {
                        content_end: ce,
                        bounds: b,
                        ..
                    } = &mut out.ops[sb.begin_index]
                    {
                        *ce = content_end;
                        *b = bounds;
                    }
                    // The mask is now active for subsequent painting in scope.
                    soft_count += 1;
                }
            }
            DisplayOp::ClearSoftMask => {
                out.ops.push(PreparedOp::PushSoftMaskNone);
                soft_count += 1;
            }
            DisplayOp::DrawShading { shading, .. } => {
                // `sh` paints the shading across the current clip in current
                // user space; shading space maps to device via the CTM.
                let clip = clip_stack.last().copied();
                let b = lower_shading_op(&mut out, &page.shadings[shading.index()], ctm, clip);
                accumulate_scope_bounds(&mut scope_stack, b);
            }
            DisplayOp::DrawImage {
                image,
                paint,
                alpha,
                blend,
                ..
            } => {
                let ir = &page.images[image.index()];
                let stencil_rgb = match &page.paints[paint.index()] {
                    Paint::Solid(c) => [to_u8(c.r), to_u8(c.g), to_u8(c.b)],
                    _ => [0, 0, 0],
                };
                let clip = clip_stack.last().copied();
                let b = lower_image(&mut out, ir, stencil_rgb, ctm, *alpha, *blend, clip);
                accumulate_scope_bounds(&mut scope_stack, b);
            }
            // The deprecated ApplySoftMask op is superseded by BeginSoftMask.
            DisplayOp::ApplySoftMask { .. } => {}
        }
        if let Some(&origin) = origin_stack.last()
            && out.ops.len() > ops_before
        {
            for prepared in &mut out.ops[ops_before..] {
                match prepared {
                    PreparedOp::Draw(cmd) => cmd.origin = origin,
                    PreparedOp::GlyphRun(gr) => gr.origin = origin,
                    PreparedOp::TiledFill(t) => t.origin = origin,
                    PreparedOp::Image(img) => img.origin = origin,
                    _ => {}
                }
            }
        }
    }
    #[cfg(feature = "profiling")]
    {
        let mut profile = out.profile.borrow_mut();
        profile.add_duration("render.lower.total", lower_start.elapsed());
        profile.increment("render.lower.operations", page.operations.len() as u64);
        profile.increment("render.lower.prepared_ops", out.ops.len() as u64);
        profile.increment("render.lower.points", out.points.len() as u64);
    }
    if let Some(cache) = font_cache {
        cache.store_for_page(page, font_programs);
    }
    out
}

/// Lower an image draw: the image fills the unit square in current user space
/// (mapped to device by `ctm`); build its device bounds and inverse transform.
#[allow(clippy::too_many_arguments)]
fn lower_image(
    out: &mut CpuPreparedPage,
    ir: &pdf_page_ir::ImageIr,
    stencil_rgb: [u8; 3],
    ctm: Matrix,
    alpha: f32,
    blend: pdf_page_ir::BlendMode,
    clip: Option<u32>,
) -> Option<DeviceRect> {
    // R1: content lowering already degraded this image (a filter chain it
    // could only partially honour, etc.). The draw still paints, but the
    // degradation must reach RenderStats so the page is never mistaken for a
    // clean pass (same contract as the dropped-draw ticks below).
    if ir.lowering_degraded {
        out.diagnostics
            .note_degraded("image draw degraded during IR lowering (lowering_degraded)".into());
    }
    // Phase 2: destination footprint in device pixels, derived from the
    // placement CTM before decode. A codec that can scale resolution (JPX)
    // decodes a minified draw at a reduced wavelet resolution; other codecs
    // ignore the hint. Computed once and shared by the base image, its soft
    // mask, and any stencil mask (they share the placement).
    let target_size = codec_target_size(ctm);

    // Direct samples, or a codec decode through the injected registry (no
    // resource cache yet — decoded per draw op, see DEFERRED.md). A
    // zero-dimension image (malformed /Width or /Height) has no pixels to
    // sample regardless of its placement rectangle.
    // A JPX in-data opacity channel (`/SMaskInData`) is split out of the base
    // decode below and used as this image's soft mask.
    let mut in_data_smask: Option<std::sync::Arc<pdf_page_ir::ImageSMask>> = None;
    let (samples, img_w, img_h, bpc, color_space) = match ir.samples.clone() {
        Some(s) => (
            s,
            ir.width,
            ir.height,
            ir.bits_per_component,
            ir.color_space.clone(),
        ),
        None => {
            // No direct samples means either a codec-encoded image or an upstream
            // general-filter decode that produced nothing (interpret.rs yields
            // `samples = None, codec = None` when the Flate/LZW/etc. chain fails).
            // The latter is a *content-losing* drop that must never be silent.
            let (Some(_), Some(_)) = (ir.codec, ir.codec_data.as_ref()) else {
                out.diagnostics.note_degraded(
                    "image draw dropped: no decodable samples (upstream filter decode failed)"
                        .into(),
                );
                return None;
            };
            // A codec-decode failure is already recorded inside `decode_codec_image`.
            let d = decode_codec_image(out, ir, target_size)?;
            // The codec reports a device format from its own channels (a
            // 1-component JPEG → Gray). A *reinterpreting* PDF color space —
            // Indexed, or a `/Separation` tint (`TintLut`) — governs how a
            // *single* codec component is read; without this a Separation-DCT
            // scan is misread as DeviceGray and inverts near-white to near-black.
            let cs = match &ir.color_space {
                pdf_page_ir::ImageColorSpace::Indexed { .. }
                | pdf_page_ir::ImageColorSpace::TintLut { .. } => {
                    if d.color_space == pdf_page_ir::ImageColorSpace::Gray {
                        // The single codec component is the index / tint.
                        ir.color_space.clone()
                    } else {
                        // The codec produced *multiple* components (Rgb/Cmyk),
                        // so its stream is not the single-component palette/tint
                        // stream the /ColorSpace describes — e.g. a JPXDecode
                        // image whose real colour model is RGB carried under an
                        // `[/Indexed …]` space with `hival 0` (one lookup
                        // entry). Painting the raw channels would overwrite real
                        // content with unintended pixels — a solid near-white JPX
                        // overlay blanking the cover it sits on. Honouring
                        // neither space faithfully, drop the draw rather than
                        // paint garbage, and record it so the drop is never
                        // silent (the tracking hole R1 exposed: a *successful*
                        // decode can still blank a page).
                        out.diagnostics.note_degraded(format!(
                            "image draw dropped: {} color space over a multi-component {:?} \
                             codec output (palette/tint expects one component)",
                            reinterpreting_space_name(&ir.color_space),
                            d.color_space,
                        ));
                        return None;
                    }
                }
                // `/Lab` also reinterprets the codec's channels rather than
                // describing new ones: a 3-channel DCT stream under `[/Lab …]`
                // carries L*a*b*, not RGB. The codec has no way to know that —
                // it reports Rgb from its channel count — so the declared space
                // has to win. Reading those bytes as RGB renders
                // custom/image_lab yellow-green (a near-zero b* byte is a
                // strong blue, but as a blue channel it is black) where PDFium,
                // hayro and MuPDF all render blue.
                pdf_page_ir::ImageColorSpace::Lab { .. }
                    if d.color_space == pdf_page_ir::ImageColorSpace::Rgb =>
                {
                    ir.color_space.clone()
                }
                // Same shape for a non-sRGB `/ICCBased` space: the codec reports
                // `Rgb` from its three channels and has no way to know they are
                // encoded in some other profile, so the declared space wins.
                pdf_page_ir::ImageColorSpace::IccRgb { .. }
                    if d.color_space == pdf_page_ir::ImageColorSpace::Rgb =>
                {
                    ir.color_space.clone()
                }
                // A 2-colorant `/DeviceN` over a two-channel codec output: the
                // codec reports `Gray` from channel 0 alone, but both channels
                // are tints the baked table needs.
                pdf_page_ir::ImageColorSpace::TintLut2 { .. }
                    if matches!(d.format, pdf_image::DecodedFormat::Multi2) =>
                {
                    ir.color_space.clone()
                }
                _ => d.color_space,
            };
            // A JPX in-data opacity channel becomes this image's soft mask when
            // the dict opts in via `/SMaskInData` (and no `/SMask` overrides it).
            if let Some(opacity) = d.alpha.clone() {
                match ir.smask_in_data {
                    1 => {
                        in_data_smask = Some(std::sync::Arc::new(pdf_page_ir::ImageSMask {
                            width: d.width,
                            height: d.height,
                            bits_per_component: 8,
                            decode: None,
                            samples: opacity,
                            codec: None,
                            codec_data: None,
                            codec_parms: None,
                        }));
                    }
                    2 => {
                        // Premultiplied opacity needs un-premultiplication before
                        // use as an unassociated soft mask; unsupported, so drop
                        // the draw rather than composite premultiplied colour (a
                        // clean B3-flagged blank, never a wrong image).
                        out.diagnostics.note_degraded(
                            "JPX premultiplied in-data alpha (/SMaskInData 2) unsupported, draw skipped"
                                .into(),
                        );
                        return None;
                    }
                    // /SMaskInData 0: ignore the alpha, paint the RGB opaque.
                    _ => {}
                }
            }
            (d.samples, d.width, d.height, d.bpc, cs)
        }
    };
    // A zero-dimension image (malformed /Width or /Height, or a codec that
    // decoded to nothing) has no pixels; PDFium paints nothing here either, so
    // this is not a content-losing drop and is intentionally *not* counted as
    // degraded (that would flag clean pages).
    if img_w == 0 || img_h == 0 {
        return None;
    }
    // Sampling needs a *trustworthy* inverse, not merely a defined one: an
    // ill-conditioned CTM inverts to noise and would smear garbage texels.
    // A degenerate placement collapses the image to no visible area (PDFium
    // likewise paints nothing), so this is a geometry no-op, not a
    // content-losing drop — intentionally not counted as degraded.
    if !ctm.is_stably_invertible() {
        return None;
    }
    let inv = ctm.invert()?;

    // Device bounds of the unit square.
    let corners = [
        ctm.apply(Point { x: 0.0, y: 0.0 }),
        ctm.apply(Point { x: 1.0, y: 0.0 }),
        ctm.apply(Point { x: 1.0, y: 1.0 }),
        ctm.apply(Point { x: 0.0, y: 1.0 }),
    ];
    let pts: Vec<[f32; 2]> = corners.iter().map(|p| [p.x as f32, p.y as f32]).collect();
    let (bx0, by0, bx1, by1) = device_bounds(&pts, out.size);
    let mut bounds = DeviceRect {
        x: bx0,
        y: by0,
        width: (bx1 - bx0).max(0) as u32,
        height: (by1 - by0).max(0) as u32,
    };
    let clip_has_mask = if let Some(cid) = clip {
        let c = &out.clips[cid as usize];
        bounds = intersect(bounds, c.bounds);
        c.has_mask
    } else {
        false
    };
    // The image maps to a sub-pixel or off-surface rectangle: nothing visible
    // to paint (PDFium likewise), so this is a geometry no-op, not a
    // content-losing drop — intentionally not counted as degraded.
    if bounds.width == 0 || bounds.height == 0 {
        return None;
    }

    let sample_lut =
        crate::image::build_sample_lut(bpc, &color_space, ir.decode.as_deref(), ir.is_stencil);
    out.ops
        .push(PreparedOp::Image(Box::new(crate::image::PreparedImage {
            origin: pdf_page_ir::PaintOrigin::default(),
            bounds,
            clip,
            clip_has_mask,
            inv,
            width: img_w,
            height: img_h,
            bpc,
            color_space,
            decode: ir.decode.clone(),
            samples,
            sample_lut,
            smask: in_data_smask.or_else(|| resolve_smask(out, ir.smask.as_deref(), target_size)),
            mask: resolve_mask(out, ir.mask.as_ref(), target_size),
            interpolation: ir.interpolation,
            // How many source texels one device pixel spans. `inv` maps device to
            // the unit square, so a device pixel's edges become (a, b) and (c, d)
            // there; scaling by the image size gives texels. Summing the absolute
            // components takes the bounding box of the rotated case.
            footprint: [
                (inv.a.abs() + inv.c.abs()) * img_w as f64,
                (inv.b.abs() + inv.d.abs()) * img_h as f64,
            ],
            is_stencil: ir.is_stencil,
            stencil_rgb,
            alpha: to_u8(alpha),
            blend,
        })));
    Some(bounds)
}

/// Resolve an image's soft mask, decoding it through the registry when it is
/// codec-encoded (MRC scans mask a JPX foreground with JBIG2). A mask that
/// cannot be decoded is dropped rather than failing the draw — the image
/// then paints unmasked, which preflight normally prevents by routing the
/// page away on the missing `NEEDS_*` feature.
fn resolve_smask(
    out: &CpuPreparedPage,
    smask: Option<&pdf_page_ir::ImageSMask>,
    target_size: Option<(u32, u32)>,
) -> Option<std::sync::Arc<pdf_page_ir::ImageSMask>> {
    let smask = smask?;
    if smask.codec.is_none() {
        return Some(std::sync::Arc::new(smask.clone()));
    }
    // A soft mask shares its base draw's device footprint (same placement CTM),
    // so it takes the same reduced-decode hint.
    let decoded = decode_codec_samples(
        out,
        smask.codec?,
        smask.codec_data.as_deref()?,
        smask.width,
        smask.height,
        smask.bits_per_component,
        true,
        smask.codec_parms.clone(),
        target_size,
        // A soft mask is grayscale by construction; it has no PDF colour space
        // to override the container's.
        None,
    )?;
    Some(std::sync::Arc::new(pdf_page_ir::ImageSMask {
        width: decoded.width,
        height: decoded.height,
        bits_per_component: decoded.bpc,
        decode: smask.decode.clone(),
        samples: decoded.samples,
        codec: None,
        codec_data: None,
        codec_parms: None,
    }))
}

/// Resolve an explicit `/Mask`. Color-key passes through untouched; a stencil
/// mask is decoded through the codec registry when encoded (JBIG2/CCITT masks),
/// exactly like [`resolve_smask`]. A stencil that cannot be decoded is dropped
/// (the image then paints unmasked) rather than failing the draw.
fn resolve_mask(
    out: &CpuPreparedPage,
    mask: Option<&pdf_page_ir::ImageMask>,
    target_size: Option<(u32, u32)>,
) -> Option<pdf_page_ir::ImageMask> {
    match mask? {
        pdf_page_ir::ImageMask::ColorKey(ranges) => {
            Some(pdf_page_ir::ImageMask::ColorKey(ranges.clone()))
        }
        pdf_page_ir::ImageMask::Stencil(sm) => {
            let resolved = resolve_smask(out, Some(sm), target_size)?;
            Some(pdf_page_ir::ImageMask::Stencil(resolved))
        }
    }
}

/// Short name of a single-component *reinterpreting* image color space, for
/// degraded-draw diagnostics. Any other space maps to `"reinterpreting"`.
fn reinterpreting_space_name(cs: &pdf_page_ir::ImageColorSpace) -> &'static str {
    match cs {
        pdf_page_ir::ImageColorSpace::Indexed { .. } => "Indexed",
        pdf_page_ir::ImageColorSpace::TintLut { .. } => "TintLut",
        pdf_page_ir::ImageColorSpace::TintLut2 { .. } => "TintLut2",
        pdf_page_ir::ImageColorSpace::IccRgb { .. } => "IccRgb",
        _ => "reinterpreting",
    }
}

/// A codec's output, repacked for [`crate::image::PreparedImage`] sampling.
#[derive(Debug, Clone)]
struct CodecSamples {
    samples: std::sync::Arc<[u8]>,
    width: u32,
    height: u32,
    bpc: u8,
    color_space: pdf_page_ir::ImageColorSpace,
    /// The codec's own output layout, kept so a *reinterpreting* PDF space can
    /// tell a two-channel duotone from gray+alpha.
    format: pdf_image::DecodedFormat,
    /// A tight 8-bit grayscale opacity plane split out of a JPX RGBA decode
    /// (`/SMaskInData`); `None` for every other codec output.
    alpha: Option<std::sync::Arc<[u8]>>,
}

/// Decode a codec-encoded image through the page's registry. The *decoded*
/// geometry wins over the dictionary's on mismatch, and the decoded format's
/// color space over the declared one — the codec knows what it produced.
fn decode_codec_image(
    out: &CpuPreparedPage,
    ir: &pdf_page_ir::ImageIr,
    target_size: Option<(u32, u32)>,
) -> Option<CodecSamples> {
    decode_codec_samples(
        out,
        ir.codec?,
        ir.codec_data.as_deref()?,
        ir.width,
        ir.height,
        ir.bits_per_component,
        ir.is_stencil,
        ir.codec_parms.clone(),
        target_size,
        Some(&ir.color_space),
    )
}

/// The IR's image colour space as a `pdf_color` family, for the codec API's
/// descriptor. `TintLut` is a pre-resolved `/Separation`, and `Lab` keeps its
/// own family; the rest map by arity.
fn declared_space_family(space: &pdf_page_ir::ImageColorSpace) -> pdf_color::ColorSpaceFamily {
    match space {
        pdf_page_ir::ImageColorSpace::Gray => pdf_color::ColorSpaceFamily::DeviceGray,
        pdf_page_ir::ImageColorSpace::Rgb => pdf_color::ColorSpaceFamily::DeviceRgb,
        pdf_page_ir::ImageColorSpace::Cmyk => pdf_color::ColorSpaceFamily::DeviceCmyk,
        pdf_page_ir::ImageColorSpace::Indexed { .. } => pdf_color::ColorSpaceFamily::Indexed,
        pdf_page_ir::ImageColorSpace::TintLut { .. }
        | pdf_page_ir::ImageColorSpace::TintLut2 { .. } => {
            pdf_color::ColorSpaceFamily::DeviceN
        }
        pdf_page_ir::ImageColorSpace::Lab { .. } => pdf_color::ColorSpaceFamily::Lab,
        pdf_page_ir::ImageColorSpace::IccRgb { .. } => pdf_color::ColorSpaceFamily::IccBased,
    }
}

/// The shared codec-decode path for base images and soft masks.
#[allow(clippy::too_many_arguments)]
fn decode_codec_samples(
    out: &CpuPreparedPage,
    kind: pdf_page_ir::ImageCodecKind,
    data: &[u8],
    width: u32,
    height: u32,
    bits_per_component: u8,
    is_mask: bool,
    parms: Option<pdf_page_ir::CodecParms>,
    target_size: Option<(u32, u32)>,
    declared_space: Option<&pdf_page_ir::ImageColorSpace>,
) -> Option<CodecSamples> {
    use pdf_image::{DecodeParameters, DecodedFormat, ImageDescriptor, StreamFilter};
    #[cfg(feature = "profiling")]
    let cache_key = DecodeCacheKey {
        data_ptr: data.as_ptr() as usize,
        data_len: data.len(),
        codec: match kind {
            pdf_page_ir::ImageCodecKind::Dct => 0,
            pdf_page_ir::ImageCodecKind::Jpx => 1,
            pdf_page_ir::ImageCodecKind::Jbig2 => 2,
            pdf_page_ir::ImageCodecKind::CcittFax => 3,
        },
        width,
        height,
        bpc: bits_per_component,
        is_mask,
        target: target_size,
    };
    #[cfg(feature = "profiling")]
    if let Some(cache) = &out.decode_cache {
        if let Some(cached) = lock_unpoisoned(&cache.entries).get(&cache_key).cloned() {
            let mut profile = out.profile.borrow_mut();
            profile.increment("codec.decode_cache.hits", 1);
            profile.increment("codec.decode_cache.hit_bytes", cached.samples.len() as u64);
            return Some(cached);
        }
        out.profile
            .borrow_mut()
            .increment("codec.decode_cache.misses", 1);
    }
    // Production decoded-image cache: content-hash probe before any decode.
    let image_cache_key = out.image_cache.as_ref().map(|cache| {
        let key = ImageCacheKey::new(
            kind,
            data,
            width,
            height,
            bits_per_component,
            is_mask,
            parms.as_ref(),
            target_size,
        );
        (cache, key)
    });
    if let Some((cache, key)) = &image_cache_key
        && let Some(cached) = cache.get(key)
    {
        return Some(cached);
    }
    let filter = match kind {
        pdf_page_ir::ImageCodecKind::Dct => StreamFilter::DctDecode,
        pdf_page_ir::ImageCodecKind::Jpx => StreamFilter::Jpx,
        pdf_page_ir::ImageCodecKind::Jbig2 => StreamFilter::Jbig2,
        pdf_page_ir::ImageCodecKind::CcittFax => StreamFilter::CcittFax,
    };
    let Some(codec) = out.codecs.get(filter) else {
        out.diagnostics.note_degraded(format!(
            "{filter:?} image draw skipped: no codec registered"
        ));
        return None;
    };
    let descriptor = ImageDescriptor {
        width,
        height,
        bits_per_component,
        // Only the *family* is consulted, by codecs whose container carries a
        // colour space the PDF dictionary overrides (JPX: ISO 32000-1 §7.4.9).
        // `components` is the declared count, and no codec keys off `object`.
        color_space: declared_space.map(|space| pdf_color::ColorSpaceDesc {
            family: declared_space_family(space),
            components: space.components() as u8,
            object: None,
        }),
        is_mask,
        interpolate: false,
        filters: vec![filter],
        object: None,
    };
    // Translate the IR's backend-neutral parms into the codec API's.
    let params = match parms {
        Some(p) => DecodeParameters {
            decode: None,
            jbig2_globals: p.jbig2_globals.clone(),
            ccitt: Some(pdf_image::CcittParams {
                k: p.k,
                columns: p.columns,
                rows: p.rows,
                black_is_1: p.black_is_1,
                byte_align: p.byte_align,
                end_of_line: p.end_of_line,
                end_of_block: p.end_of_block,
            }),
            target_size,
        },
        None => DecodeParameters {
            target_size,
            ..DecodeParameters::default()
        },
    };
    #[cfg(feature = "profiling")]
    let decode_start = std::time::Instant::now();
    let img = match codec.decode(data, &descriptor, &params, &out.decode_limits) {
        Ok(img) => img,
        Err(e) => {
            // The decode failed: this draw is silently dropped (blank). Record
            // it so a page left blank by an undecodable image is never mistaken
            // for a clean render (Workstream B3 / DEFERRED.md corpus item 1).
            out.diagnostics.note_degraded(format!(
                "{filter:?} {} image decode failed, draw skipped: {e}",
                if is_mask { "mask" } else { "base" }
            ));
            return None;
        }
    };
    #[cfg(feature = "profiling")]
    {
        let mut profile = out.profile.borrow_mut();
        profile.add_duration(
            match filter {
                StreamFilter::DctDecode => "codec.dct.decode",
                StreamFilter::Jpx => "codec.jpx.decode",
                StreamFilter::Jbig2 => "codec.jbig2.decode",
                StreamFilter::CcittFax => "codec.ccitt.decode",
                // The four codec kinds above are the only reachable filters.
                _ => "codec.other.decode",
            },
            decode_start.elapsed(),
        );
        profile.increment("codec.decode.calls", 1);
        profile.increment("codec.encoded_bytes", data.len() as u64);
        profile.increment("codec.decoded_pixels", img.width as u64 * img.height as u64);
        profile.increment("codec.decoded_bytes", img.data.len() as u64);
        profile.allocate_bytes(img.data.len() as u64);
    }
    #[cfg(feature = "profiling")]
    let decoded_allocation_bytes = img.data.len() as u64;
    let (color_space, comps, bpc, output_format_counter) = match img.format {
        DecodedFormat::Mono1 => (
            pdf_page_ir::ImageColorSpace::Gray,
            1,
            1u8,
            "codec.output.mono1",
        ),
        DecodedFormat::Gray8 => (
            pdf_page_ir::ImageColorSpace::Gray,
            1,
            8,
            "codec.output.gray8",
        ),
        DecodedFormat::Gray16 => (
            pdf_page_ir::ImageColorSpace::Gray,
            1,
            16,
            "codec.output.gray16",
        ),
        // Gray+alpha repacks as 2 interleaved channels; the opacity plane is
        // split out below, leaving a 1-channel DeviceGray base raster.
        DecodedFormat::GrayA8 => (
            pdf_page_ir::ImageColorSpace::Gray,
            2,
            8,
            "codec.output.graya8",
        ),
        // Two raw channels the PDF `/ColorSpace` interprets; the reinterpreting
        // branch above swaps `Gray` for the declared 2-component space.
        DecodedFormat::Multi2 => (
            pdf_page_ir::ImageColorSpace::Gray,
            2,
            8,
            "codec.output.multi2",
        ),
        DecodedFormat::Rgb8 => (pdf_page_ir::ImageColorSpace::Rgb, 3, 8, "codec.output.rgb8"),
        // RGBA repacks as 4 interleaved channels; the opacity plane is split out
        // below into `alpha`, leaving a 3-channel DeviceRGB base raster.
        DecodedFormat::Rgba8 => (pdf_page_ir::ImageColorSpace::Rgb, 4, 8, "codec.output.rgba8"),
        DecodedFormat::Cmyk8 => (
            pdf_page_ir::ImageColorSpace::Cmyk,
            4,
            8,
            "codec.output.cmyk8",
        ),
    };
    #[cfg(feature = "profiling")]
    out.profile.borrow_mut().increment(output_format_counter, 1);
    #[cfg(not(feature = "profiling"))]
    let _ = output_format_counter;
    // PreparedImage sampling assumes tight byte-aligned rows; repack if the
    // codec used a wider stride.
    let tight = (img.width as usize * comps * bpc as usize).div_ceil(8);
    #[cfg(feature = "profiling")]
    let repack_start = std::time::Instant::now();
    let samples: std::sync::Arc<[u8]> = if img.stride == tight {
        img.data
    } else {
        let mut packed = vec![0u8; tight * img.height as usize];
        for y in 0..img.height as usize {
            let src = &img.data[y * img.stride..y * img.stride + tight];
            packed[y * tight..(y + 1) * tight].copy_from_slice(src);
        }
        packed.into()
    };
    #[cfg(feature = "profiling")]
    {
        let mut profile = out.profile.borrow_mut();
        profile.add_duration("codec.repack", repack_start.elapsed());
        if img.stride != tight {
            profile.increment("codec.repacked_rows", img.height as u64);
            profile.increment("codec.repacked_bytes", samples.len() as u64);
            profile.allocate_bytes(samples.len() as u64);
            profile.release_bytes(decoded_allocation_bytes);
        }
    }
    // A JPX RGBA decode carries an in-data opacity channel: de-interleave it
    // into a tight 3-channel DeviceRGB base raster plus a grayscale alpha plane
    // (the `/SMaskInData` soft mask). Other formats pass through unchanged.
    let (samples, alpha) = match img.format {
        // The opacity plane is the last of `n` interleaved channels; the base
        // raster keeps the leading `n - 1` (RGB for Rgba8, gray for GrayA8).
        DecodedFormat::Rgba8 | DecodedFormat::GrayA8 => {
            let n = if matches!(img.format, DecodedFormat::Rgba8) { 4 } else { 2 };
            let base_n = n - 1;
            let px = img.width as usize * img.height as usize;
            let mut base = vec![0u8; px * base_n];
            let mut opacity = vec![0u8; px];
            for i in 0..px {
                base[i * base_n..(i + 1) * base_n]
                    .copy_from_slice(&samples[i * n..i * n + base_n]);
                opacity[i] = samples[i * n + base_n];
            }
            (
                std::sync::Arc::<[u8]>::from(base),
                Some(std::sync::Arc::<[u8]>::from(opacity)),
            )
        }
        _ => (samples, None),
    };
    let decoded = CodecSamples {
        samples,
        format: img.format,
        width: img.width,
        height: img.height,
        bpc,
        color_space,
        alpha,
    };
    if let Some((cache, key)) = image_cache_key {
        cache.insert(key, decoded.clone());
    }
    #[cfg(feature = "profiling")]
    if let Some(cache) = &out.decode_cache {
        lock_unpoisoned(&cache.entries).insert(cache_key, decoded.clone());
    }
    Some(decoded)
}

/// Open group / soft-mask-content scope being lowered.
struct ScopeBuilder {
    begin_index: usize,
    bounds: Option<DeviceRect>,
}

/// Union a just-emitted command's device bounds into the innermost open scope.
fn accumulate_scope_bounds(scope_stack: &mut [ScopeBuilder], bounds: Option<DeviceRect>) {
    if let (Some(gb), Some(b)) = (scope_stack.last_mut(), bounds) {
        if b.width == 0 || b.height == 0 {
            return;
        }
        gb.bounds = Some(match gb.bounds {
            Some(cur) => union(cur, b),
            None => b,
        });
    }
}

/// Bounding union of two device rects (ignoring zero-size rects).
/// Union of two optional device bounds (a fill and a stroke of the same run).
fn union_rect(a: Option<DeviceRect>, b: Option<DeviceRect>) -> Option<DeviceRect> {
    match (a, b) {
        (Some(a), Some(b)) => Some(union(a, b)),
        (some, None) | (None, some) => some,
    }
}

fn union(a: DeviceRect, b: DeviceRect) -> DeviceRect {
    if a.width == 0 || a.height == 0 {
        return b;
    }
    if b.width == 0 || b.height == 0 {
        return a;
    }
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.width as i32).max(b.x + b.width as i32);
    let y1 = (a.y + a.height as i32).max(b.y + b.height as i32);
    DeviceRect {
        x: x0,
        y: y0,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    }
}

/// Flatten a clip path, classify it (rect vs mask), intersect its rectangular
/// envelope with ancestors, and push it onto the clip graph.
fn push_clip(
    out: &mut CpuPreparedPage,
    clip_stack: &mut Vec<u32>,
    path: &PathData,
    ctm: Matrix,
    rule: pdf_page_ir::FillRule,
) {
    let pt_start = out.points.len();
    let range_start = out.subpaths.len() as u32;
    flatten_into(path, ctm, &mut out.points, &mut out.subpaths);
    let range_end = out.subpaths.len() as u32;

    let (bx0, by0, bx1, by1) = device_bounds(&out.points[pt_start..], out.size);
    let path_rect = DeviceRect {
        x: bx0,
        y: by0,
        width: (bx1 - bx0).max(0) as u32,
        height: (by1 - by0).max(0) as u32,
    };

    let parent = clip_stack.last().copied();
    let parent_bounds = parent.map(|p| out.clips[p as usize].bounds);
    let bounds = match parent_bounds {
        Some(pb) => intersect(pb, path_rect),
        None => path_rect,
    };

    // Integer-aligned single-subpath rectangle → pure rect clip (no mask).
    let is_rect = range_end - range_start == 1
        && is_integer_rect(
            &out.points[pt_start..],
            bx0 as f32,
            by0 as f32,
            bx1 as f32,
            by1 as f32,
        );

    let kind = if is_rect {
        // Rect clips need no geometry; reclaim the arenas.
        out.points.truncate(pt_start);
        out.subpaths.truncate(range_start as usize);
        ClipKind::Rect
    } else {
        let krule = match rule {
            pdf_page_ir::FillRule::NonZero => FillRule::NonZero,
            pdf_page_ir::FillRule::EvenOdd => FillRule::EvenOdd,
        };
        ClipKind::Path {
            subpath_range: (range_start, range_end),
            rule: krule,
        }
    };
    let has_mask = matches!(kind, ClipKind::Path { .. })
        || parent
            .map(|p| out.clips[p as usize].has_mask)
            .unwrap_or(false);

    let cid = out.clips.len() as u32;
    out.clips.push(PreparedClip {
        parent,
        bounds,
        kind,
        has_mask,
    });
    clip_stack.push(cid);
}

#[allow(clippy::too_many_arguments)]
fn lower_fill(
    out: &mut CpuPreparedPage,
    path: &PathData,
    ctm: Matrix,
    rule: pdf_page_ir::FillRule,
    color: pdf_page_ir::Color,
    op_alpha: f32,
    clip: Option<u32>,
    blend: pdf_page_ir::BlendMode,
    soft_active: bool,
    shading: Option<Box<PreparedShading>>,
) -> Option<DeviceRect> {
    let range_start = out.subpaths.len() as u32;
    let pt_start = out.points.len();
    flatten_into(path, ctm, &mut out.points, &mut out.subpaths);
    let range_end = out.subpaths.len() as u32;
    if range_end == range_start {
        return None; // empty path
    }

    // Drop fills whose device geometry is corrupt (see `device_bounds_sane`).
    let Some((bx0, by0, bx1, by1)) = device_bounds_sane(&out.points[pt_start..], out.size) else {
        out.points.truncate(pt_start);
        out.subpaths.truncate(range_start as usize);
        return None;
    };
    let mut bounds = DeviceRect {
        x: bx0,
        y: by0,
        width: (bx1 - bx0).max(0) as u32,
        height: (by1 - by0).max(0) as u32,
    };

    // Intersect with the clip's rectangular envelope (rect clips are fully
    // handled here; path clips still leave `bounds` as a conservative box).
    let clip_has_mask = if let Some(cid) = clip {
        let c = &out.clips[cid as usize];
        bounds = intersect(bounds, c.bounds);
        c.has_mask
    } else {
        false
    };
    if bounds.width == 0 || bounds.height == 0 {
        out.points.truncate(pt_start);
        out.subpaths.truncate(range_start as usize);
        return None;
    }

    let alpha = to_u8(op_alpha * color.a);
    let rgb = [to_u8(color.r), to_u8(color.g), to_u8(color.b)];
    let opaque = alpha == 255;
    let krule = match rule {
        pdf_page_ir::FillRule::NonZero => FillRule::NonZero,
        pdf_page_ir::FillRule::EvenOdd => FillRule::EvenOdd,
    };

    // Fast-path: a single axis/integer-aligned opaque rectangle under Normal
    // blend with no mask clip (a rect clip only shrinks `bounds`). A shading
    // fill is never the opaque-rect fast path (color varies per pixel).
    let class = if shading.is_none()
        && opaque
        && !clip_has_mask
        && !soft_active
        && matches!(blend, pdf_page_ir::BlendMode::Normal)
        && range_end - range_start == 1
        && is_integer_rect(
            &out.points[pt_start..],
            bx0 as f32,
            by0 as f32,
            bx1 as f32,
            by1 as f32,
        ) {
        DrawClass::OpaqueRect
    } else {
        DrawClass::SolidPath
    };

    out.ops.push(PreparedOp::Draw(PreparedCommand {
        origin: pdf_page_ir::PaintOrigin::default(),
        class,
        subpath_range: (range_start, range_end),
        rule: krule,
        rgb,
        premul: [rgb[0], rgb[1], rgb[2], 255],
        alpha,
        opaque,
        bounds,
        clip,
        clip_has_mask,
        blend,
        shading,
    }));
    Some(bounds)
}

/// A ceiling on tile instances per tiling fill — bounds adversarial patterns
/// with a tiny step over a huge fill (documented; a real clip-aware tiler is
/// the optimization). Beyond this the fill is left unpainted.
const MAX_TILES: usize = 1 << 14;

/// Lower a tiling-pattern fill: flatten the fill shape (device space), then
/// enumerate the lattice tiles whose cell overlaps the fill's device bounds.
#[allow(clippy::too_many_arguments)]
fn lower_tiled(
    out: &mut CpuPreparedPage,
    path: &PathData,
    ctm: Matrix,
    rule: pdf_page_ir::FillRule,
    tiling: &TilingPattern,
    pattern_to_device: Matrix,
    op_alpha: f32,
    clip: Option<u32>,
    blend: pdf_page_ir::BlendMode,
) -> Option<DeviceRect> {
    let range_start = out.subpaths.len() as u32;
    let pt_start = out.points.len();
    flatten_into(path, ctm, &mut out.points, &mut out.subpaths);
    let range_end = out.subpaths.len() as u32;
    if range_end == range_start {
        return None;
    }
    let (bx0, by0, bx1, by1) = device_bounds(&out.points[pt_start..], out.size);
    let mut bounds = DeviceRect {
        x: bx0,
        y: by0,
        width: (bx1 - bx0).max(0) as u32,
        height: (by1 - by0).max(0) as u32,
    };
    let clip_has_mask = if let Some(cid) = clip {
        let c = &out.clips[cid as usize];
        bounds = intersect(bounds, c.bounds);
        c.has_mask
    } else {
        false
    };
    if bounds.width == 0 || bounds.height == 0 {
        out.points.truncate(pt_start);
        out.subpaths.truncate(range_start as usize);
        return None;
    }

    let x_step = tiling.x_step.abs() as f64;
    let y_step = tiling.y_step.abs() as f64;
    let tiles = enumerate_tiles(bounds, pattern_to_device, tiling, x_step, y_step);
    if tiles.is_empty() {
        out.points.truncate(pt_start);
        out.subpaths.truncate(range_start as usize);
        return None;
    }

    let krule = match rule {
        pdf_page_ir::FillRule::NonZero => FillRule::NonZero,
        pdf_page_ir::FillRule::EvenOdd => FillRule::EvenOdd,
    };
    out.ops.push(PreparedOp::TiledFill(Box::new(PreparedTiling {
        origin: pdf_page_ir::PaintOrigin::default(),
        bounds,
        clip,
        clip_has_mask,
        fill_subpaths: (range_start, range_end),
        fill_rule: krule,
        alpha: to_u8(op_alpha),
        cell: tiling.cell.clone(),
        pattern_to_device,
        uncolored: tiling.uncolored,
        under: color_to_bytes(&tiling.under_color),
        tiles,
        blend,
    })));
    Some(bounds)
}

/// Enumerate the device transforms of every lattice tile whose cell bbox
/// overlaps `bounds`, capped at [`MAX_TILES`]. Returns empty when the step is
/// degenerate or the transform is singular.
fn enumerate_tiles(
    bounds: DeviceRect,
    pattern_to_device: Matrix,
    tiling: &TilingPattern,
    x_step: f64,
    y_step: f64,
) -> Vec<Matrix> {
    if x_step <= 1e-6 || y_step <= 1e-6 {
        return Vec::new();
    }
    if !pattern_to_device.is_stably_invertible() {
        return Vec::new();
    }
    let Some(inv) = pattern_to_device.invert() else {
        return Vec::new();
    };
    // Map the device bounds' corners back into pattern space.
    let (dx0, dy0) = (bounds.x as f64, bounds.y as f64);
    let (dx1, dy1) = (
        (bounds.x + bounds.width as i32) as f64,
        (bounds.y + bounds.height as i32) as f64,
    );
    let corners = [
        inv.apply(Point { x: dx0, y: dy0 }),
        inv.apply(Point { x: dx1, y: dy0 }),
        inv.apply(Point { x: dx1, y: dy1 }),
        inv.apply(Point { x: dx0, y: dy1 }),
    ];
    let (mut px0, mut py0, mut px1, mut py1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for c in corners {
        px0 = px0.min(c.x);
        py0 = py0.min(c.y);
        px1 = px1.max(c.x);
        py1 = py1.max(c.y);
    }
    let [bbx0, bby0, bbx1, bby1] = [
        tiling.bbox[0] as f64,
        tiling.bbox[1] as f64,
        tiling.bbox[2] as f64,
        tiling.bbox[3] as f64,
    ];
    // Tile i overlaps in x when i*step + bbox spans [px0, px1].
    let i0 = ((px0 - bbx1) / x_step).floor() as i64;
    let i1 = ((px1 - bbx0) / x_step).ceil() as i64;
    let j0 = ((py0 - bby1) / y_step).floor() as i64;
    let j1 = ((py1 - bby0) / y_step).ceil() as i64;
    let (ni, nj) = ((i1 - i0 + 1).max(0) as u64, (j1 - j0 + 1).max(0) as u64);
    if ni.saturating_mul(nj) > MAX_TILES as u64 {
        return Vec::new();
    }
    let mut tiles = Vec::new();
    for j in j0..=j1 {
        for i in i0..=i1 {
            let t = Matrix::translate(i as f64 * x_step, j as f64 * y_step).then(pattern_to_device);
            tiles.push(t);
        }
    }
    tiles
}

/// Length of the transformed x basis vector (PDFium `CFX_Matrix::GetXUnit`).
///
/// The zero shortcuts are PDFium's and are kept: `hypot(a, 0)` and `|a|` can
/// differ in the last bit, and this feeds a width.
fn x_unit(m: Matrix) -> f64 {
    if m.b == 0.0 {
        m.a.abs()
    } else if m.a == 0.0 {
        m.b.abs()
    } else {
        m.a.hypot(m.b)
    }
}

/// Length of the transformed y basis vector (PDFium `CFX_Matrix::GetYUnit`).
fn y_unit(m: Matrix) -> f64 {
    if m.c == 0.0 {
        m.d.abs()
    } else if m.d == 0.0 {
        m.c.abs()
    } else {
        m.c.hypot(m.d)
    }
}

/// Split `ctm` into `(m1, m2)` such that `m1.then(m2) == ctm`, where `m1` is a
/// pure uniform scale (plus translation) and `m2` carries every anisotropic
/// and rotational part.
///
/// This is PDFium's `CFX_AggDriver::DrawPath` decomposition. `m1`'s scale is
/// `max(|a|, |b|)` — the first CTM column's dominant term — and `m2` is the
/// linear part divided through by it. Because `m1`'s linear part works out to
/// exactly `scale * I`, a circular pen expanded in m1-space is correct, and
/// applying `m2` afterwards produces the ellipse the CTM actually implies.
///
/// Returns `None` when the linear part is degenerate or not invertible, in
/// which case there is no meaningful pen to draw.
fn decompose_pen(ctm: Matrix) -> Option<(Matrix, Matrix)> {
    if !ctm.is_finite() {
        return None;
    }
    let s = ctm.a.abs().max(ctm.b.abs());
    if s == 0.0 || !s.is_finite() {
        return None;
    }
    let m2 = Matrix {
        a: ctm.a / s,
        b: ctm.b / s,
        c: ctm.c / s,
        d: ctm.d / s,
        e: 0.0,
        f: 0.0,
    };
    // Recovering m1 by composition rather than asserting `scale(s, s)` is
    // deliberate: it keeps `m1.then(m2) == ctm` exact, translation included.
    let m1 = ctm.then(m2.invert()?);
    Some((m1, m2))
}

/// Expand a stroked path into fill geometry (advice §4E) and push one
/// non-zero fill command for the stroke.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn lower_stroke(
    out: &mut CpuPreparedPage,
    path: &PathData,
    ctm: Matrix,
    style: &StrokeStyle,
    color: pdf_page_ir::Color,
    op_alpha: f32,
    clip: Option<u32>,
    blend: pdf_page_ir::BlendMode,
    shading: Option<Box<PreparedShading>>,
) -> Option<DeviceRect> {
    // A stroke pen is circular in *user* space, so under an anisotropic or
    // rotated CTM it lands as an ellipse on the device. Expanding the stroke
    // directly in device space can only ever produce a circular pen, so it
    // cannot represent that; picking one scalar scale to compensate is what
    // `sqrt(|det|)` used to do, and under `scale(10, 1)` it drew a uniform
    // 3.16-wide pen where 10-wide-by-1-tall was wanted.
    //
    // PDFium's answer (CFX_AggDriver::DrawPath) is to split the transform
    // rather than average it: `m1` takes a uniform scale, `m2` carries all the
    // anisotropy and rotation. Stroking happens in m1-space, where a circular
    // pen is *correct* by construction, and m2 then transforms the resulting
    // outline — turning the circle into exactly the right ellipse. Note m2 is
    // applied to the stroke's geometry, not to its width.
    // A degenerate linear part leaves no meaningful pen to draw.
    let (m1, m2) = decompose_pen(ctm)?;
    let scale = m1.a as f32;

    // Hairline clamp, in m1-space so that m2's scaling is accounted for: a
    // stroke must not vanish, but "one pixel" is only meaningful after m2.
    let unit = 1.0 / ((x_unit(m2) + y_unit(m2)) / 2.0);
    let raw_dw = (style.width as f32 * scale).max(unit as f32);

    // Defensive clamp against corrupt/adversarial input. The pen is a circle of
    // radius `raw_dw/2` in m1-space; m2 then stretches it, so its largest device
    // extent is `raw_dw * σ_max(m2)` (m2 is normalized, so `x_unit(m2) ≤ √2` and
    // only `y_unit(m2)` — or the m1 scale folded into `raw_dw` — can blow up).
    // A legitimate stroke never dwarfs the whole output; when the width does, it
    // came from garbage (a bad-deflate stream decoding to `set-line-width
    // 11016766`, a mangled CTM, etc.). Fall back to a hairline — PDFium's
    // observable behavior on the same corrupt files — rather than flood the page
    // with a mega-wide pen. Non-finite products are treated as garbage too.
    let pen_axis = x_unit(m2).max(y_unit(m2)) as f32;
    let device_extent = raw_dw * pen_axis;
    let max_output = out.size.width.max(out.size.height).max(1) as f32;
    let dw = if device_extent.is_finite() && device_extent <= max_output {
        raw_dw
    } else {
        unit as f32
    };
    let hw = dw * 0.5;
    let miter = style.miter_limit as f32;

    let dash: Vec<f32> = style
        .dash_pattern
        .iter()
        .map(|d| *d as f32 * scale)
        .collect();
    let phase = style.dash_phase as f32 * scale;
    let dashed = dash.iter().any(|d| *d > 0.0);

    let range_start = out.subpaths.len() as u32;
    let pt_start = out.points.len();
    for (poly, closed) in flatten_polylines(path, m1) {
        if dashed {
            for piece in stroke::dash_polyline(&poly, closed, &dash, phase) {
                stroke::expand_stroke(
                    &piece,
                    false,
                    hw,
                    style.cap,
                    style.join,
                    miter,
                    &mut out.points,
                    &mut out.subpaths,
                );
            }
        } else {
            stroke::expand_stroke(
                &poly,
                closed,
                hw,
                style.cap,
                style.join,
                miter,
                &mut out.points,
                &mut out.subpaths,
            );
        }
    }
    let range_end = out.subpaths.len() as u32;
    if range_end == range_start {
        return None;
    }

    // The outline exists in m1-space; m2 carries it the rest of the way to the
    // device, stretching the circular pen into its true ellipse. This is the
    // one place the stroke's geometry meets the anisotropy.
    if m2 != Matrix::IDENTITY {
        for p in &mut out.points[pt_start..] {
            let q = m2.apply(Point {
                x: p[0] as f64,
                y: p[1] as f64,
            });
            *p = [q.x as f32, q.y as f32];
        }
    }

    // Drop strokes whose device outline is corrupt (see `device_bounds_sane`).
    // The width clamp above already tames a garbage *width*; this also guards a
    // garbage *path* (coordinates far off the viewport).
    let Some((bx0, by0, bx1, by1)) = device_bounds_sane(&out.points[pt_start..], out.size) else {
        out.points.truncate(pt_start);
        out.subpaths.truncate(range_start as usize);
        return None;
    };
    let mut bounds = DeviceRect {
        x: bx0,
        y: by0,
        width: (bx1 - bx0).max(0) as u32,
        height: (by1 - by0).max(0) as u32,
    };
    let clip_has_mask = if let Some(cid) = clip {
        let c = &out.clips[cid as usize];
        bounds = intersect(bounds, c.bounds);
        c.has_mask
    } else {
        false
    };
    if bounds.width == 0 || bounds.height == 0 {
        out.points.truncate(pt_start);
        out.subpaths.truncate(range_start as usize);
        return None;
    }

    let alpha = to_u8(op_alpha * color.a);
    let rgb = [to_u8(color.r), to_u8(color.g), to_u8(color.b)];
    out.ops.push(PreparedOp::Draw(PreparedCommand {
        origin: pdf_page_ir::PaintOrigin::default(),
        class: DrawClass::SolidPath,
        subpath_range: (range_start, range_end),
        rule: FillRule::NonZero, // stroke outline unions under non-zero
        rgb,
        premul: [rgb[0], rgb[1], rgb[2], 255],
        alpha,
        opaque: alpha == 255,
        bounds,
        clip,
        clip_has_mask,
        blend,
        shading,
    }));
    Some(bounds)
}

/// Lower a glyph run to **synthetic rectangle glyphs** (fonts.md Font Phase 1):
/// one box per glyph, sized from its advance, transformed text→device, filled
/// with the run's paint. This makes text placement visible and testable while
/// real outline rasterization (Skrifa / Type 1) is a later font phase. Spaces
/// (simple-font code 32) are skipped so gaps read as gaps.
#[allow(clippy::too_many_arguments)]
fn lower_glyph_boxes(
    out: &mut CpuPreparedPage,
    run: &GlyphRun,
    ctm: Matrix,
    color: pdf_page_ir::Color,
    op_alpha: f32,
    clip: Option<u32>,
    blend: pdf_page_ir::BlendMode,
) -> Option<DeviceRect> {
    if run.glyphs.is_empty() {
        return None;
    }
    let range_start = out.subpaths.len() as u32;
    let pt_start = out.points.len();
    append_glyph_boxes(out, run, ctm);
    finish_glyph_fill(out, range_start, pt_start, clip, color, op_alpha, blend)
}

/// Append a glyph run's fallback boxes (no outline program) into the shared
/// subpath arena. One quad per non-space glyph, sized from the pen advance —
/// the placeholder shape used both for painting and for text clipping when a
/// font carries no embedded outlines.
fn append_glyph_boxes(out: &mut CpuPreparedPage, run: &GlyphRun, ctm: Matrix) {
    // text space → user space (run.transform) → device (ctm).
    let m = run.transform.then(ctm);
    let fs = run.font_size;
    let pad = fs * 0.07;
    let asc = fs * 0.60;
    for (i, gph) in run.glyphs.iter().enumerate() {
        if gph.glyph == 32 {
            continue; // space (simple fonts): leave a gap
        }
        let adv = if i + 1 < run.glyphs.len() {
            run.glyphs[i + 1].x - gph.x
        } else {
            fs * 0.5
        };
        let box_w = (adv * 0.85).clamp(fs * 0.12, fs * 0.7);
        let (x0, y0) = (gph.x + pad, gph.y);
        let (x1, y1) = (x0 + box_w, gph.y + asc);
        let start = out.points.len();
        for p in [
            Point { x: x0, y: y0 },
            Point { x: x1, y: y0 },
            Point { x: x1, y: y1 },
            Point { x: x0, y: y1 },
        ] {
            let d = m.apply(p);
            out.points.push([d.x as f32, d.y as f32]);
        }
        out.subpaths.push((start, out.points.len()));
    }
}

/// Append one glyph run's outlines to the shared subpath arena for use as a
/// clip (no fill emitted). Real Skrifa outlines when the font program is
/// present, else the fallback boxes — the same geometry the painting path
/// would produce, so the clip matches what would be drawn. Unhinted on purpose:
/// a clip mask needs no grid-fitting, and staying unhinted keeps it
/// transform-agnostic and deterministic across render scales.
fn append_glyph_run_clip(
    out: &mut CpuPreparedPage,
    run: &GlyphRun,
    program: Option<&FontProgram>,
    ctm: Matrix,
    synth: GlyphSynthesis,
) {
    if run.glyphs.is_empty() {
        return;
    }
    match program {
        Some(prog) => {
            let upem = prog.units_per_em().max(1) as f64;
            let scale = run.font_size / upem;
            let m = run.transform.then(ctm);
            let gids: Vec<u32> = run.glyphs.iter().map(|g| g.glyph).collect();
            let mut outlines = prog.outlines(&gids);
            // The clip uses the same synthesized geometry the paint would
            // (§9.4.3: the clip is the glyphs as drawn).
            synth.apply(&mut outlines, prog);
            for (gph, outline) in run.glyphs.iter().zip(&outlines) {
                if let Some(outline) = outline {
                    append_outline(out, outline, gph.x, gph.y, scale, m);
                }
            }
        }
        None => append_glyph_boxes(out, run, ctm),
    }
}

/// Push the text-clipping path of one text object: the union of `runs`' glyph
/// outlines intersected into the current clip (ISO 32000-1 §9.4.3), mirroring
/// PDFium's `ProcessClipPath` text branch (cpdf_renderstatus.cpp) which fills a
/// `CFX_Path` of the glyph outlines with `SetClip_PathFill`. An empty run set
/// (no glyph placed an outline) yields an empty clip — nothing passes — the
/// spec's empty-clip-path rule and PDFium's `AppendRect(-1,-1,0,0)` fallback.
fn push_text_clip(
    out: &mut CpuPreparedPage,
    clip_stack: &mut Vec<u32>,
    runs: &[pdf_page_ir::GlyphRunId],
    page: &CompiledPage,
    ctm: Matrix,
    font_programs: &mut HashMap<u32, Option<FontProgram>>,
    shared: Option<&SharedFontProgramCache>,
) {
    let pt_start = out.points.len();
    let range_start = out.subpaths.len() as u32;
    for run_id in runs {
        let gr = &page.glyph_runs[run_id.index()];
        let program = resolve_font_program(font_programs, page, gr.font, shared);
        let synth = GlyphSynthesis::for_font(&page.fonts[gr.font.index()]);
        append_glyph_run_clip(out, gr, program.as_ref(), ctm, synth);
    }
    let range_end = out.subpaths.len() as u32;

    let parent = clip_stack.last().copied();
    let (kind, bounds) = if range_end == range_start {
        // No outlines placed: clip everything out.
        (
            ClipKind::Rect,
            DeviceRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
        )
    } else {
        let (bx0, by0, bx1, by1) = device_bounds(&out.points[pt_start..], out.size);
        let path_rect = DeviceRect {
            x: bx0,
            y: by0,
            width: (bx1 - bx0).max(0) as u32,
            height: (by1 - by0).max(0) as u32,
        };
        let bounds = match parent.map(|p| out.clips[p as usize].bounds) {
            Some(pb) => intersect(pb, path_rect),
            None => path_rect,
        };
        // Glyph outlines fill non-zero (matching the painting path and PDFium).
        (
            ClipKind::Path {
                subpath_range: (range_start, range_end),
                rule: FillRule::NonZero,
            },
            bounds,
        )
    };
    let has_mask = matches!(kind, ClipKind::Path { .. })
        || parent
            .map(|p| out.clips[p as usize].has_mask)
            .unwrap_or(false);

    let cid = out.clips.len() as u32;
    out.clips.push(PreparedClip {
        parent,
        bounds,
        kind,
        has_mask,
    });
    clip_stack.push(cid);
}

fn resolve_font_program(
    local: &mut HashMap<u32, Option<FontProgram>>,
    page: &CompiledPage,
    font: pdf_page_ir::FontId,
    shared: Option<&SharedFontProgramCache>,
) -> Option<FontProgram> {
    if let Some(program) = local.get(&font.0) {
        return program.clone();
    }
    let resource = &page.fonts[font.index()];
    let program = resolve_program_bytes(resource, shared);
    local.insert(font.0, program.clone());
    program
}

/// A parse must cost at least this long to be worth retaining document-wide.
/// Retention trades peak RSS for avoided reparses, so it should pay only when
/// the reparse it avoids is genuinely expensive. Native Type 1 programs parse in
/// milliseconds (the type1 corpus page: ~6.8 ms each) and clear this easily;
/// small subsetted CFF/TrueType programs parse in microseconds and stay
/// uncached, so a document whose fonts are all cheap to parse (the Latin
/// control) pays no retention cost — the cache is self-financing.
const MIN_PARSE_TO_CACHE: std::time::Duration = std::time::Duration::from_micros(400);

/// Parse `resource`'s embedded program, consulting the document-scoped cache
/// first. Only programs that both `benefits_from_parse_cache` (Type 1 / bare
/// CFF) *and* actually took [`MIN_PARSE_TO_CACHE`] to parse are retained;
/// everything cheaper is neither read from nor written to the shared cache.
fn resolve_program_bytes(
    resource: &pdf_page_ir::FontResource,
    shared: Option<&SharedFontProgramCache>,
) -> Option<FontProgram> {
    let Some(shared) = shared else {
        return parse_font_program(resource);
    };
    if resource.program.is_empty() {
        return None;
    }
    let key = FontProgramKey::for_resource(resource);
    if let Some(program) = shared.get(&key) {
        return Some(program);
    }
    let start = std::time::Instant::now();
    let program = parse_font_program(resource);
    let parse_cost = start.elapsed();
    if let Some(prog) = &program
        && prog.benefits_from_parse_cache()
        && parse_cost >= MIN_PARSE_TO_CACHE
    {
        shared.insert(key, prog.clone(), prog.retained_bytes());
    }
    program
}

fn parse_font_program(resource: &pdf_page_ir::FontResource) -> Option<FontProgram> {
    if resource.program.is_empty() {
        None
    } else {
        // The face index matters for collections (.ttc): a system CJK family
        // is one file with many faces.
        FontProgram::parse_indexed(resource.program.clone(), resource.face_index)
    }
}

/// Largest ppem (glyph em size in device pixels) served from the coverage
/// cache. Above this, the escape hatch (PDFium's `|a|+|b| > 50` analog) sends
/// the run to the exact outline fill so oversized display glyphs neither hold
/// large bitmaps in the LRU nor lose crispness. A 200 px em bounds a single
/// cached bitmap to a few tens of KiB.
const MAX_CACHED_PPEM: f64 = 200.0;

/// Hard guard on a single cached glyph bitmap's cell count (defense in depth
/// beside the ppem cap): a glyph whose device bbox somehow exceeds this is not
/// cached and its run falls back. 1 Mi cells = 1 MiB of coverage.
const MAX_GLYPH_CELLS: usize = 1 << 20;

/// Sub-pixel phases per axis for the glyph origin. `1` snaps to whole pixels
/// (maximal reuse, PDFium's grayscale behavior); higher values keep the origin
/// near its exact sub-pixel position — preserving the anti-aliasing the exact
/// outline fill produces — at the cost of a few more distinct cached bitmaps per
/// glyph. Measured default: 4×4 holds text severity at the exact-position
/// baseline while keeping ~99% hit rates. Tunable via
/// `PDF_RENDERER_GLYPH_SUBPIXEL="NX,NY"` for A/B isolation.
fn glyph_subpixel_steps() -> (i32, i32) {
    use std::sync::OnceLock;
    static STEPS: OnceLock<(i32, i32)> = OnceLock::new();
    *STEPS.get_or_init(|| match std::env::var("PDF_RENDERER_GLYPH_SUBPIXEL") {
        Ok(v) => {
            let mut it = v.split(',').map(|s| s.trim().parse::<i32>().ok());
            let nx = it.next().flatten().unwrap_or(4).clamp(1, 64);
            let ny = it.next().flatten().unwrap_or(nx).clamp(1, 64);
            (nx, ny)
        }
        Err(_) => (4, 4),
    })
}

/// Split a device-space origin coordinate into an integer pixel base and a
/// sub-pixel phase bucket in `0..n`. `n == 1` reduces to rounding to the nearest
/// pixel (the phase is always 0).
#[inline]
fn quantize_origin(v: f64, n: i32) -> (i32, i16) {
    let base = v.floor();
    let frac = v - base;
    let mut bucket = (frac * n as f64).round() as i32;
    let mut base = base as i32;
    if bucket >= n {
        bucket -= n;
        base += 1;
    }
    (base, bucket as i16)
}

/// Try to lower a pure-fill glyph run through the shared coverage cache
/// (PDFium mechanism #1). Returns:
///
/// - `None` — the run is **ineligible** (rotated/skewed, oversized, or hinting
///   was intended but unavailable). The caller falls back to the exact outline
///   fill, whose output is unchanged from before this phase.
/// - `Some(bounds)` — the run was emitted as a [`PreparedOp::GlyphRun`]; every
///   glyph is a cache hit or was rendered once and inserted. `bounds` is the
///   device-space extent (or `None` when nothing survives culling).
///
/// Correctness: only render mode 0 (pure fill) reaches here (the caller gates
/// on it), so the pre-existing wrong-fill of stroke/clip modes 1/2/4/5/6 is
/// **never baked into the cache** — those keep the exact outline path. The
/// glyph origin is snapped to whole device pixels (PDFium `FXSYS_roundf`), which
/// is what lets every occurrence share one bitmap; the IR carries each glyph's
/// absolute pen position, so independent rounding introduces no cumulative
/// spacing drift (no `AdjustGlyphSpace` pass needed).
#[allow(clippy::too_many_arguments)]
fn try_cached_glyph_run(
    out: &mut CpuPreparedPage,
    run: &GlyphRun,
    prog: &FontProgram,
    ctm: Matrix,
    color: pdf_page_ir::Color,
    op_alpha: f32,
    clip: Option<u32>,
    blend: pdf_page_ir::BlendMode,
    cache: &SharedGlyphCache,
    font_id: FontProgramKey,
    raster: &mut crate::raster::RasterKernel,
) -> Option<Option<DeviceRect>> {
    if run.glyphs.is_empty() {
        return Some(None);
    }
    const EPS: f64 = 1e-6;
    let upem = prog.units_per_em().max(1) as f64;
    let scale = run.font_size / upem;
    let m = run.transform.then(ctm);

    // Escape hatch: cache only axis-aligned, modestly sized glyphs.
    let axis_aligned = m.b.abs() < EPS && m.c.abs() < EPS;
    if !axis_aligned {
        return None;
    }
    let ppem = run.font_size * m.d.abs();
    if !ppem.is_finite() || ppem <= 0.0 || ppem > MAX_CACHED_PPEM {
        return None;
    }

    // Effective device-space linear map applied to the design-unit outline
    // (`(font_size/upem)·CTM_linear`); axis-aligned, so `lb == lc == 0`.
    let (la, lb, lc, ld) = (m.a * scale, m.b * scale, m.c * scale, m.d * scale);
    let q = |v: f64| (v * 10000.0).round() as i32;
    let (qla, qlb, qlc, qld) = (q(la), q(lb), q(lc), q(ld));

    // Run-level hinting decision, fixed *without* building the (expensive)
    // hinting instance so it is deterministic and matches the key. Type 1 is
    // never hinted. Mirrors `try_hinted_glyphs`.
    let uniform = (m.a.abs() - m.d.abs()).abs() < 1e-4 * m.a.abs().max(1.0);
    let hinted =
        uniform && !prog.is_type1() && out.hinting.should_hint(ppem as f32, axis_aligned);
    let flip_x = m.a.signum();
    let flip_y = m.d.signum();

    let n = run.glyphs.len();
    let (nx, ny) = glyph_subpixel_steps();
    // Per glyph: integer device-pixel origin base plus a sub-pixel phase bucket.
    // The bucket enters the key (so occurrences at the same phase share a
    // bitmap) and the rasterization (so the bitmap carries that phase's exact
    // anti-aliasing); the base is where the bitmap is blitted.
    let placed: Vec<(i32, i32, i16, i16)> = run
        .glyphs
        .iter()
        .map(|g| {
            let o = m.apply(Point { x: g.x, y: g.y });
            let (ox, sx) = quantize_origin(o.x, nx);
            let (oy, sy) = quantize_origin(o.y, ny);
            (ox, oy, sx, sy)
        })
        .collect();

    // Probe the cache for every glyph; misses are extracted in one batch.
    let mut slots: Vec<Option<GlyphPlacement>> = vec![None; n];
    let mut miss_keys: Vec<(usize, GlyphCacheKey)> = Vec::new();
    let mut hits = 0u64;
    for (gi, gph) in run.glyphs.iter().enumerate() {
        let (ox, oy, sx, sy) = placed[gi];
        let key = GlyphCacheKey {
            font: font_id,
            glyph: gph.glyph,
            la: qla,
            lb: qlb,
            lc: qlc,
            ld: qld,
            sx,
            sy,
            hinted,
        };
        match cache.get(&key) {
            Some(bitmap) => {
                hits += 1;
                slots[gi] = place_glyph(bitmap, (ox, oy));
            }
            None => miss_keys.push((gi, key)),
        }
    }

    // Batch-extract only the misses: one `FontRef`/`HintingInstance` for the
    // whole run, never one per glyph.
    if !miss_keys.is_empty() {
        let miss_gids: Vec<u32> = miss_keys.iter().map(|&(gi, _)| run.glyphs[gi].glyph).collect();
        let extracted: Vec<Option<pdf_font::Outline>> = if hinted {
            // Hinting intended but unbuildable for this font/size → the whole
            // run falls back (deterministic: it fails identically every page, so
            // no glyph was ever cached under `hinted = true`).
            match prog.outlines_hinted(&miss_gids, ppem as f32) {
                Some(v) => v,
                None => return None,
            }
        } else {
            prog.outlines(&miss_gids)
        };

        for (&(gi, key), outline) in miss_keys.iter().zip(&extracted) {
            let (ox, oy, sx, sy) = placed[gi];
            // Sub-pixel origin phase baked into the rasterization frame.
            let fox = sx as f64 / nx as f64;
            let foy = sy as f64 / ny as f64;
            let bitmap = match outline {
                Some(outline) if hinted => rasterize_glyph_bitmap(
                    outline,
                    |p| {
                        [
                            (flip_x * p[0] as f64 + fox) as f32,
                            (flip_y * p[1] as f64 + foy) as f32,
                        ]
                    },
                    raster,
                ),
                Some(outline) => rasterize_glyph_bitmap(
                    outline,
                    |p| [(la * p[0] as f64 + fox) as f32, (ld * p[1] as f64 + foy) as f32],
                    raster,
                ),
                None => empty_glyph_bitmap(),
            };
            cache.insert(key, bitmap.clone());
            slots[gi] = place_glyph(bitmap, (ox, oy));
        }
    }

    #[cfg(feature = "profiling")]
    {
        let mut profile = out.profile.borrow_mut();
        profile.increment("lower.glyph.count", n as u64);
        profile.increment("lower.glyph.cache_hits", hits);
        profile.increment("lower.glyph.cache_misses", miss_keys.len() as u64);
        profile.increment("lower.glyph.cached_runs", 1);
    }
    #[cfg(not(feature = "profiling"))]
    let _ = hits;

    let placements: Vec<GlyphPlacement> = slots.into_iter().flatten().collect();
    if placements.is_empty() {
        return Some(None);
    }

    // Device bounds: union of placement rects, clamped to output and clip.
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for p in &placements {
        x0 = x0.min(p.dx);
        y0 = y0.min(p.dy);
        x1 = x1.max(p.dx + p.bitmap.width as i32);
        y1 = y1.max(p.dy + p.bitmap.height as i32);
    }
    x0 = x0.max(0);
    y0 = y0.max(0);
    x1 = x1.min(out.size.width as i32);
    y1 = y1.min(out.size.height as i32);
    if x1 <= x0 || y1 <= y0 {
        return Some(None);
    }
    let mut bounds = DeviceRect {
        x: x0,
        y: y0,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    };
    let clip_has_mask = if let Some(cid) = clip {
        let c = &out.clips[cid as usize];
        bounds = intersect(bounds, c.bounds);
        c.has_mask
    } else {
        false
    };
    if bounds.width == 0 || bounds.height == 0 {
        return Some(None);
    }

    let alpha = to_u8(op_alpha * color.a);
    let rgb = [to_u8(color.r), to_u8(color.g), to_u8(color.b)];
    out.ops.push(PreparedOp::GlyphRun(Box::new(PreparedGlyphRun {
        origin: pdf_page_ir::PaintOrigin::default(),
        placements,
        rgb,
        alpha,
        opaque: alpha == 255,
        bounds,
        clip,
        clip_has_mask,
        blend,
        // A shading-pattern text fill attaches its shading post-hoc (the
        // three glyph lowerings all share this emitter).
        shading: None,
    })));
    Some(Some(bounds))
}

/// Build a placement for a cached bitmap at a snapped origin, or `None` for an
/// empty (zero-area) glyph — a space or a glyph with no outline, which is cached
/// as an empty bitmap so it never re-extracts but paints nothing.
fn place_glyph(bitmap: Arc<GlyphBitmap>, origin: (i32, i32)) -> Option<GlyphPlacement> {
    if bitmap.width == 0 || bitmap.height == 0 {
        return None;
    }
    let (ox, oy) = origin;
    Some(GlyphPlacement {
        dx: ox + bitmap.left,
        dy: oy + bitmap.top,
        bitmap,
    })
}

fn empty_glyph_bitmap() -> Arc<GlyphBitmap> {
    Arc::new(GlyphBitmap {
        left: 0,
        top: 0,
        width: 0,
        height: 0,
        cov: Box::from([] as [u8; 0]),
    })
}

/// Render one glyph's outline into a bbox-tight coverage bitmap at a canonical
/// integer origin. `map` sends an outline point to its device-space offset from
/// the glyph origin (design-unit·linear for the unhinted path, pixel·flip for
/// the hinted path). The bitmap's `(left, top)` is the floor of that offset
/// box's corner, so blitting at `origin + (left, top)` places it exactly.
fn rasterize_glyph_bitmap(
    outline: &pdf_font::Outline,
    map: impl Fn([f32; 2]) -> [f32; 2],
    raster: &mut crate::raster::RasterKernel,
) -> Arc<GlyphBitmap> {
    let mut points: Vec<[f32; 2]> = Vec::new();
    let mut subpaths: Vec<(usize, usize)> = Vec::new();
    flatten_outline_into(outline, map, &mut points, &mut subpaths);
    if points.is_empty() || subpaths.is_empty() {
        return empty_glyph_bitmap();
    }

    let (mut minx, mut miny, mut maxx, mut maxy) =
        (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for p in &points {
        minx = minx.min(p[0]);
        miny = miny.min(p[1]);
        maxx = maxx.max(p[0]);
        maxy = maxy.max(p[1]);
    }
    if !minx.is_finite() || !miny.is_finite() || !maxx.is_finite() || !maxy.is_finite() {
        return empty_glyph_bitmap();
    }
    let left = minx.floor() as i32;
    let top = miny.floor() as i32;
    let width = ((maxx.ceil() as i32) - left).max(1) as usize;
    let height = ((maxy.ceil() as i32) - top).max(1) as usize;
    if width.saturating_mul(height) > MAX_GLYPH_CELLS {
        return empty_glyph_bitmap();
    }

    // Shift into the local bitmap frame (all points ≥ 0).
    for p in &mut points {
        p[0] -= left as f32;
        p[1] -= top as f32;
    }

    let mut cov = vec![0u8; width * height];
    raster.fill(
        &points,
        &subpaths,
        width,
        height,
        FillRule::NonZero,
        |y, x0, x1, row: &mut [u8]| {
            let base = y * width;
            cov[base + x0..=base + x1].copy_from_slice(&row[x0..=x1]);
        },
    );

    Arc::new(GlyphBitmap {
        left,
        top,
        width: width as u32,
        height: height as u32,
        cov: cov.into_boxed_slice(),
    })
}

/// Flatten `outline` into local `points`/`subpaths`, mapping each point through
/// `dev`. Curve subdivision matches [`emit_outline_points`] (`OUTLINE_SEGMENTS`),
/// so a cached glyph's coverage is identical to the outline path's at the same
/// transform up to the origin snap.
fn flatten_outline_into(
    outline: &pdf_font::Outline,
    dev: impl Fn([f32; 2]) -> [f32; 2],
    points: &mut Vec<[f32; 2]>,
    subpaths: &mut Vec<(usize, usize)>,
) {
    let mut pt = 0usize;
    let mut start = points.len();
    let mut cur = [0f32; 2];
    for verb in &outline.verbs {
        match verb {
            pdf_font::OutlineVerb::MoveTo => {
                if points.len() > start {
                    subpaths.push((start, points.len()));
                }
                start = points.len();
                cur = dev(outline.points[pt]);
                points.push(cur);
                pt += 1;
            }
            pdf_font::OutlineVerb::LineTo => {
                cur = dev(outline.points[pt]);
                points.push(cur);
                pt += 1;
            }
            pdf_font::OutlineVerb::QuadTo => {
                let c = dev(outline.points[pt]);
                let e = dev(outline.points[pt + 1]);
                pt += 2;
                flatten_quad(points, cur, c, e);
                cur = e;
            }
            pdf_font::OutlineVerb::CurveTo => {
                let c1 = dev(outline.points[pt]);
                let c2 = dev(outline.points[pt + 1]);
                let e = dev(outline.points[pt + 2]);
                pt += 3;
                flatten_cubic(points, cur, c1, c2, e);
                cur = e;
            }
            pdf_font::OutlineVerb::Close => {
                if points.len() > start {
                    subpaths.push((start, points.len()));
                }
                start = points.len();
            }
        }
    }
    if points.len() > start {
        subpaths.push((start, points.len()));
    }
}

/// Synthetic style application (fonts.md Phase 3): a substituted face
/// missing the requested cut gets the 12° oblique shear and/or the
/// PDFium-level outline embolden, applied to the freshly-extracted outlines
/// in font units — exactly where PDFium's FreeType glyph-load applies
/// `FT_Outline_Embolden` / the italic matrix (`cfx_face.cpp`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct GlyphSynthesis {
    /// Oblique shear tangent (glyph space).
    shear: f32,
    /// Embolden strength as an em fraction.
    embolden_em: f32,
}

impl GlyphSynthesis {
    fn for_font(res: &pdf_page_ir::FontResource) -> Self {
        Self {
            shear: res.synthetic_shear,
            embolden_em: res.synthetic_embolden_em,
        }
    }
    fn is_active(&self) -> bool {
        self.shear != 0.0 || self.embolden_em > 0.0
    }
    /// Apply to extracted outlines (font units).
    fn apply(&self, outlines: &mut [Option<pdf_font::Outline>], prog: &FontProgram) {
        if !self.is_active() {
            return;
        }
        let strength = self.embolden_em * prog.units_per_em().max(1) as f32;
        for outline in outlines.iter_mut().flatten() {
            if strength > 0.0 {
                outline.embolden(strength);
            }
            outline.oblique(self.shear);
        }
    }
}

/// Lower a glyph run to **real outlines** (fonts.md Font Phase 2): each glyph's
/// Skrifa outline (font units) is scaled by `font_size/units_per_em`, placed at
/// its pen position, transformed text→device, flattened, and filled non-zero
/// with the run's paint. Empty glyphs (spaces, missing) contribute nothing.
#[allow(clippy::too_many_arguments)]
fn lower_glyph_outlines(
    out: &mut CpuPreparedPage,
    run: &GlyphRun,
    prog: &FontProgram,
    ctm: Matrix,
    color: pdf_page_ir::Color,
    op_alpha: f32,
    clip: Option<u32>,
    blend: pdf_page_ir::BlendMode,
    synth: GlyphSynthesis,
) -> Option<DeviceRect> {
    if run.glyphs.is_empty() {
        return None;
    }
    let upem = prog.units_per_em().max(1) as f64;
    let scale = run.font_size / upem;
    let m = run.transform.then(ctm);
    let gids: Vec<u32> = run.glyphs.iter().map(|g| g.glyph).collect();

    let range_start = out.subpaths.len() as u32;
    let pt_start = out.points.len();

    // Synthesis forces the exact unhinted path: hinted outlines are already
    // grid-fit pixels, the wrong space to shear or embolden (PDFium loads
    // NO_HINTING when it synthesizes).
    let hinted = if synth.is_active() {
        None
    } else {
        try_hinted_glyphs(out, run, prog, m, &gids)
    };
    if let Some(hinted) = hinted {
        for (gph, outline) in run.glyphs.iter().zip(&hinted.outlines) {
            if let Some(outline) = outline {
                append_hinted_outline(out, outline, gph.x, gph.y, m, hinted.flip_x, hinted.flip_y);
            }
        }
    } else {
        #[cfg(feature = "profiling")]
        let outline_start = std::time::Instant::now();
        let mut outlines = prog.outlines(&gids);
        synth.apply(&mut outlines, prog);
        #[cfg(feature = "profiling")]
        {
            out.profile
                .borrow_mut()
                .add_duration("lower.glyph.outline_extract", outline_start.elapsed());
            out.profile
                .borrow_mut()
                .increment("lower.glyph.count", gids.len() as u64);
        }
        #[cfg(feature = "profiling")]
        let emit_start = std::time::Instant::now();
        for (gph, outline) in run.glyphs.iter().zip(&outlines) {
            if let Some(outline) = outline {
                append_outline(out, outline, gph.x, gph.y, scale, m);
            }
        }
        #[cfg(feature = "profiling")]
        out.profile
            .borrow_mut()
            .add_duration("lower.glyph.emit", emit_start.elapsed());
    }
    finish_glyph_fill(out, range_start, pt_start, clip, color, op_alpha, blend)
}

/// Stroke a text run's glyph outlines (render modes Tr 1/2/5/6, §9.3.1).
///
/// The outlines are built as one user-space [`PathData`] — design units scaled
/// by `font_size/upem`, placed at each glyph origin, and mapped through the
/// text matrix (`run.transform`) but **not** the CTM — then handed to the
/// ordinary path stroker. `lower_stroke` applies the CTM and the current line
/// width, so the pen is scaled by the CTM alone (user-space width), while the
/// glyph geometry carries the text matrix, exactly as the spec prescribes.
#[allow(clippy::too_many_arguments)]
fn lower_glyph_stroke(
    out: &mut CpuPreparedPage,
    run: &GlyphRun,
    prog: &FontProgram,
    ctm: Matrix,
    style: &StrokeStyle,
    color: pdf_page_ir::Color,
    op_alpha: f32,
    clip: Option<u32>,
    blend: pdf_page_ir::BlendMode,
    synth: GlyphSynthesis,
    shading: Option<Box<PreparedShading>>,
) -> Option<DeviceRect> {
    if run.glyphs.is_empty() {
        return None;
    }
    let upem = prog.units_per_em().max(1) as f64;
    let scale = run.font_size / upem;
    let gids: Vec<u32> = run.glyphs.iter().map(|g| g.glyph).collect();
    let mut outlines = prog.outlines(&gids);
    synth.apply(&mut outlines, prog);

    let mut verbs: Vec<pdf_page_ir::PathVerb> = Vec::new();
    let mut points: Vec<Point> = Vec::new();
    for (gph, outline) in run.glyphs.iter().zip(&outlines) {
        if let Some(outline) = outline {
            append_glyph_stroke_path(&mut verbs, &mut points, outline, gph.x, gph.y, scale, run.transform);
        }
    }
    if verbs.is_empty() {
        return None;
    }
    let path = PathData { verbs: verbs.into(), points: points.into() };
    lower_stroke(out, &path, ctm, style, color, op_alpha, clip, blend, shading)
}

/// Append one glyph's outline to a user-space polyline [`PathData`] (MoveTo /
/// LineTo / Close), flattening quadratics and cubics to segments. Points are
/// mapped design-units → text space (`origin + scaled point`) → user space via
/// the text matrix `tm`; the CTM is applied later by the stroker.
fn append_glyph_stroke_path(
    verbs: &mut Vec<pdf_page_ir::PathVerb>,
    points: &mut Vec<Point>,
    outline: &pdf_font::Outline,
    gx: f64,
    gy: f64,
    scale: f64,
    tm: Matrix,
) {
    use pdf_page_ir::PathVerb;
    let map = |o: [f32; 2]| -> Point {
        tm.apply(Point { x: gx + o[0] as f64 * scale, y: gy + o[1] as f64 * scale })
    };
    let mut pt = 0usize;
    let mut cur = Point::default();
    for verb in &outline.verbs {
        match verb {
            pdf_font::OutlineVerb::MoveTo => {
                cur = map(outline.points[pt]);
                pt += 1;
                verbs.push(PathVerb::MoveTo);
                points.push(cur);
            }
            pdf_font::OutlineVerb::LineTo => {
                cur = map(outline.points[pt]);
                pt += 1;
                verbs.push(PathVerb::LineTo);
                points.push(cur);
            }
            pdf_font::OutlineVerb::QuadTo => {
                let c = map(outline.points[pt]);
                let e = map(outline.points[pt + 1]);
                pt += 2;
                for i in 1..=OUTLINE_SEGMENTS {
                    let t = i as f64 / OUTLINE_SEGMENTS as f64;
                    let u = 1.0 - t;
                    verbs.push(PathVerb::LineTo);
                    points.push(Point {
                        x: u * u * cur.x + 2.0 * u * t * c.x + t * t * e.x,
                        y: u * u * cur.y + 2.0 * u * t * c.y + t * t * e.y,
                    });
                }
                cur = e;
            }
            pdf_font::OutlineVerb::CurveTo => {
                let c1 = map(outline.points[pt]);
                let c2 = map(outline.points[pt + 1]);
                let e = map(outline.points[pt + 2]);
                pt += 3;
                for i in 1..=OUTLINE_SEGMENTS {
                    let t = i as f64 / OUTLINE_SEGMENTS as f64;
                    let u = 1.0 - t;
                    let (w0, w1, w2, w3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                    verbs.push(PathVerb::LineTo);
                    points.push(Point {
                        x: w0 * cur.x + w1 * c1.x + w2 * c2.x + w3 * e.x,
                        y: w0 * cur.y + w1 * c1.y + w2 * c2.y + w3 * e.y,
                    });
                }
                cur = e;
            }
            pdf_font::OutlineVerb::Close => verbs.push(PathVerb::Close),
        }
    }
}

/// A run's grid-fitted outlines plus the axis signs needed to place them.
struct HintedGlyphs {
    outlines: Vec<Option<pdf_font::Outline>>,
    flip_x: f64,
    flip_y: f64,
}

/// Grid-fit a run's glyphs, if the policy and this transform allow it.
///
/// Hinting is only meaningful when the glyph lands square on the pixel grid
/// at a known pixels-per-em, so it requires an axis-aligned, uniformly
/// scaled transform. Anything else (rotation, skew, anisotropic scale) falls
/// through to the exact unhinted outline — fonts.md §4 makes the same call
/// for glyph caching.
fn try_hinted_glyphs(
    out: &CpuPreparedPage,
    run: &GlyphRun,
    prog: &FontProgram,
    m: Matrix,
    gids: &[u32],
) -> Option<HintedGlyphs> {
    const EPS: f64 = 1e-6;
    let axis_aligned = m.b.abs() < EPS && m.c.abs() < EPS;
    // Skrifa's hinting size is a single scalar ppem, so a non-uniform scale
    // has no one grid to fit.
    let uniform = (m.a.abs() - m.d.abs()).abs() < 1e-4 * m.a.abs().max(1.0);
    let ppem = (run.font_size * m.d.abs()) as f32;
    if !axis_aligned || !uniform || !out.hinting.should_hint(ppem, axis_aligned) {
        return None;
    }
    let outlines = prog.outlines_hinted(gids, ppem)?;
    Some(HintedGlyphs {
        outlines,
        flip_x: m.a.signum(),
        flip_y: m.d.signum(),
    })
}

/// Place an already-grid-fitted outline.
///
/// The points are pixels, not design units, so the transform must not scale
/// them again — only the glyph origin goes through `m`, and the outline is
/// laid down around it with the axis signs (`m.d` is normally negative: PDF
/// y-up into device y-down).
///
/// The origin's **y is snapped to the pixel grid** and its x is left exact:
/// vertical grid alignment is what hinting buys (baselines, x-height, stem
/// tops), while x positions carry the PDF's own `/Widths` and rounding them
/// would visibly damage spacing.
fn append_hinted_outline(
    out: &mut CpuPreparedPage,
    outline: &pdf_font::Outline,
    ox: f64,
    oy: f64,
    m: Matrix,
    flip_x: f64,
    flip_y: f64,
) {
    let origin = m.apply(Point { x: ox, y: oy });
    let (ox, oy) = (origin.x, origin.y.round());
    let map = |p: &[f32; 2]| -> [f32; 2] {
        [
            (ox + flip_x * p[0] as f64) as f32,
            (oy + flip_y * p[1] as f64) as f32,
        ]
    };
    emit_outline_points(out, outline, map);
}

/// Flatten a glyph outline (font units) into device-space subpaths, placed at
/// pen `(gx, gy)`, scaled by `scale`, and transformed by `m` (text→device).
fn append_outline(
    out: &mut CpuPreparedPage,
    outline: &pdf_font::Outline,
    gx: f64,
    gy: f64,
    scale: f64,
    m: Matrix,
) {
    // Design units → text space (glyph origin + scaled point) → device.
    emit_outline_points(out, outline, |o: &[f32; 2]| {
        let d = m.apply(Point {
            x: gx + o[0] as f64 * scale,
            y: gy + o[1] as f64 * scale,
        });
        [d.x as f32, d.y as f32]
    });
}

/// Flatten `outline` into `out`'s point/subpath arrays, mapping every point
/// through `dev`. Shared by the unhinted (design-unit) and hinted
/// (pixel-space) paths, which differ only in that mapping.
fn emit_outline_points(
    out: &mut CpuPreparedPage,
    outline: &pdf_font::Outline,
    dev: impl Fn(&[f32; 2]) -> [f32; 2],
) {
    let dev = |p: [f32; 2]| dev(&p);
    let mut pt = 0usize;
    let mut start = out.points.len();
    let mut cur = [0f32; 2];
    for verb in &outline.verbs {
        match verb {
            pdf_font::OutlineVerb::MoveTo => {
                if out.points.len() > start {
                    out.subpaths.push((start, out.points.len()));
                }
                start = out.points.len();
                cur = dev(outline.points[pt]);
                out.points.push(cur);
                pt += 1;
            }
            pdf_font::OutlineVerb::LineTo => {
                cur = dev(outline.points[pt]);
                out.points.push(cur);
                pt += 1;
            }
            pdf_font::OutlineVerb::QuadTo => {
                let c = dev(outline.points[pt]);
                let e = dev(outline.points[pt + 1]);
                pt += 2;
                flatten_quad(&mut out.points, cur, c, e);
                cur = e;
            }
            pdf_font::OutlineVerb::CurveTo => {
                let c1 = dev(outline.points[pt]);
                let c2 = dev(outline.points[pt + 1]);
                let e = dev(outline.points[pt + 2]);
                pt += 3;
                flatten_cubic(&mut out.points, cur, c1, c2, e);
                cur = e;
            }
            pdf_font::OutlineVerb::Close => {
                if out.points.len() > start {
                    out.subpaths.push((start, out.points.len()));
                }
                start = out.points.len();
            }
        }
    }
    if out.points.len() > start {
        out.subpaths.push((start, out.points.len()));
    }
}

const OUTLINE_SEGMENTS: usize = 8;

fn flatten_quad(points: &mut Vec<[f32; 2]>, p: [f32; 2], c: [f32; 2], e: [f32; 2]) {
    for i in 1..=OUTLINE_SEGMENTS {
        let t = i as f32 / OUTLINE_SEGMENTS as f32;
        let u = 1.0 - t;
        let x = u * u * p[0] + 2.0 * u * t * c[0] + t * t * e[0];
        let y = u * u * p[1] + 2.0 * u * t * c[1] + t * t * e[1];
        points.push([x, y]);
    }
}

fn flatten_cubic(points: &mut Vec<[f32; 2]>, p: [f32; 2], c1: [f32; 2], c2: [f32; 2], e: [f32; 2]) {
    for i in 1..=OUTLINE_SEGMENTS {
        let t = i as f32 / OUTLINE_SEGMENTS as f32;
        let u = 1.0 - t;
        let (w0, w1, w2, w3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        let x = w0 * p[0] + w1 * c1[0] + w2 * c2[0] + w3 * e[0];
        let y = w0 * p[1] + w1 * c1[1] + w2 * c2[1] + w3 * e[1];
        points.push([x, y]);
    }
}

/// Emit a single non-zero solid-fill command for glyph geometry appended since
/// `range_start`/`pt_start` (shared by the box and outline paths).
fn finish_glyph_fill(
    out: &mut CpuPreparedPage,
    range_start: u32,
    pt_start: usize,
    clip: Option<u32>,
    color: pdf_page_ir::Color,
    op_alpha: f32,
    blend: pdf_page_ir::BlendMode,
) -> Option<DeviceRect> {
    let range_end = out.subpaths.len() as u32;
    if range_end == range_start {
        return None;
    }

    let (bx0, by0, bx1, by1) = device_bounds(&out.points[pt_start..], out.size);
    let mut bounds = DeviceRect {
        x: bx0,
        y: by0,
        width: (bx1 - bx0).max(0) as u32,
        height: (by1 - by0).max(0) as u32,
    };
    let clip_has_mask = if let Some(cid) = clip {
        let c = &out.clips[cid as usize];
        bounds = intersect(bounds, c.bounds);
        c.has_mask
    } else {
        false
    };
    if bounds.width == 0 || bounds.height == 0 {
        out.points.truncate(pt_start);
        out.subpaths.truncate(range_start as usize);
        return None;
    }

    let alpha = to_u8(op_alpha * color.a);
    let rgb = [to_u8(color.r), to_u8(color.g), to_u8(color.b)];
    out.ops.push(PreparedOp::Draw(PreparedCommand {
        origin: pdf_page_ir::PaintOrigin::default(),
        class: DrawClass::SolidPath,
        subpath_range: (range_start, range_end),
        rule: FillRule::NonZero,
        rgb,
        premul: [rgb[0], rgb[1], rgb[2], 255],
        alpha,
        opaque: alpha == 255,
        bounds,
        clip,
        clip_has_mask,
        blend,
        shading: None,
    }));
    Some(bounds)
}

/// Flatten a path into device-space polylines, one per subpath, each tagged
/// with whether it is closed (drives caps vs. joins in stroking).
fn flatten_polylines(path: &PathData, ctm: Matrix) -> Vec<(Vec<[f32; 2]>, bool)> {
    let mut out: Vec<(Vec<[f32; 2]>, bool)> = Vec::new();
    let mut cur: Vec<[f32; 2]> = Vec::new();
    let mut closed = false;
    let mut pt = 0usize;
    let mut last = Point::default();
    let map = |p: Point| {
        let d = ctm.apply(p);
        [d.x as f32, d.y as f32]
    };
    for verb in path.verbs.iter() {
        match verb {
            PathVerb::MoveTo => {
                if !cur.is_empty() {
                    out.push((std::mem::take(&mut cur), closed));
                    closed = false;
                }
                last = path.points[pt];
                cur.push(map(last));
                pt += 1;
            }
            PathVerb::LineTo => {
                last = path.points[pt];
                cur.push(map(last));
                pt += 1;
            }
            PathVerb::CurveTo => {
                let p0 = last;
                let (c1, c2, p3) = (path.points[pt], path.points[pt + 1], path.points[pt + 2]);
                pt += 3;
                for i in 1..=CURVE_SEGMENTS {
                    let t = i as f64 / CURVE_SEGMENTS as f64;
                    cur.push(map(cubic(p0, c1, c2, p3, t)));
                }
                last = p3;
            }
            PathVerb::Close => closed = true,
        }
    }
    if !cur.is_empty() {
        out.push((cur, closed));
    }
    out
}

/// Whether the reduced-resolution decode hint is disabled by environment. Read
/// once; a measurement/debug switch, not a production toggle (default: enabled).
fn jpx_reduce_disabled() -> bool {
    use std::sync::OnceLock;
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var_os("PDF_RENDERER_JPX_REDUCE")
            .map(|v| v == "0" || v == "off" || v == "false")
            .unwrap_or(false)
    })
}

/// Destination footprint of a unit-square image placed by `ctm`, in device
/// pixels, for codecs that can decode at reduced resolution (Phase 2).
///
/// This is the bounding-box extent of the placed unit square, deliberately
/// **unclamped** to the surface: a partly off-screen image still covers its
/// full device extent, and clamping would under-report it and risk decoding at
/// too low a resolution. A degenerate or non-invertible placement yields `None`
/// (decode at full resolution) — such draws paint nothing anyway.
fn codec_target_size(ctm: Matrix) -> Option<(u32, u32)> {
    // Measurement switch: `PDF_RENDERER_JPX_REDUCE=0` disables the reduced-
    // resolution hint so a run reproduces full-resolution, byte-identical
    // output (Phase-1 hash gate) from the same binary that ships reduction on
    // by default. Read once.
    if jpx_reduce_disabled() {
        return None;
    }
    if !ctm.is_stably_invertible() {
        return None;
    }
    let corners = [
        ctm.apply(Point { x: 0.0, y: 0.0 }),
        ctm.apply(Point { x: 1.0, y: 0.0 }),
        ctm.apply(Point { x: 1.0, y: 1.0 }),
        ctm.apply(Point { x: 0.0, y: 1.0 }),
    ];
    let (mut x0, mut y0, mut x1, mut y1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for p in corners {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    let w = (x1 - x0).ceil();
    let h = (y1 - y0).ceil();
    if !w.is_finite() || !h.is_finite() || w < 1.0 || h < 1.0 {
        return None;
    }
    Some((w as u32, h as u32))
}

/// Device-space integer bounds of `points`, culled to the output size.
/// `device_bounds`, but rejects corrupt geometry. Bad-deflate content, mangled
/// CTMs, and adversarial files decode to coordinates orders of magnitude beyond
/// the viewport; clamped to the page they rasterize as a full-page smear (the
/// 4778 black-fill / green-stroke flood). When any raw point is non-finite or
/// lands past `SANE_FACTOR × viewport`, drop the whole primitive — PDFium is
/// defensive here too. `SANE_FACTOR` is generous so legitimate partly-off-page
/// geometry is never touched (a shape 64× the viewport away does not occur in
/// real content, but a garbage 1e9 coordinate always trips it). The min/max is
/// computed in the same pass, so there is no extra traversal versus
/// `device_bounds`.
fn device_bounds_sane(points: &[[f32; 2]], size: DeviceSize) -> Option<(i32, i32, i32, i32)> {
    const SANE_FACTOR: f32 = 64.0;
    let limit = SANE_FACTOR * (size.width.max(size.height).max(1) as f32);
    let (mut x0, mut y0, mut x1, mut y1) = (
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    );
    for p in points {
        if !p[0].is_finite() || !p[1].is_finite() || p[0].abs() > limit || p[1].abs() > limit {
            return None;
        }
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    Some((
        x0.floor().clamp(0.0, size.width as f32) as i32,
        y0.floor().clamp(0.0, size.height as f32) as i32,
        x1.ceil().clamp(0.0, size.width as f32) as i32,
        y1.ceil().clamp(0.0, size.height as f32) as i32,
    ))
}

fn device_bounds(points: &[[f32; 2]], size: DeviceSize) -> (i32, i32, i32, i32) {
    let (mut x0, mut y0, mut x1, mut y1) = (
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    );
    for p in points {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    (
        x0.floor().clamp(0.0, size.width as f32) as i32,
        y0.floor().clamp(0.0, size.height as f32) as i32,
        x1.ceil().clamp(0.0, size.width as f32) as i32,
        y1.ceil().clamp(0.0, size.height as f32) as i32,
    )
}

/// Intersection of two device rects (zero-size when disjoint).
fn intersect(a: DeviceRect, b: DeviceRect) -> DeviceRect {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.width as i32).min(b.x + b.width as i32);
    let y1 = (a.y + a.height as i32).min(b.y + b.height as i32);
    if x1 <= x0 || y1 <= y0 {
        DeviceRect {
            x: x0,
            y: y0,
            width: 0,
            height: 0,
        }
    } else {
        DeviceRect {
            x: x0,
            y: y0,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        }
    }
}

/// True when the subpath's points are exactly the axis-aligned rectangle
/// `[x0,y0]..[x1,y1]` (in some corner order) with integer edges.
fn is_integer_rect(points: &[[f32; 2]], x0: f32, y0: f32, x1: f32, y1: f32) -> bool {
    if points.len() != 4 {
        return false;
    }
    let near_int = |v: f32| (v - v.round()).abs() < 1e-4;
    for p in points {
        if !near_int(p[0]) || !near_int(p[1]) {
            return false;
        }
        let on_x = (p[0] - x0).abs() < 1e-4 || (p[0] - x1).abs() < 1e-4;
        let on_y = (p[1] - y0).abs() < 1e-4 || (p[1] - y1).abs() < 1e-4;
        if !on_x || !on_y {
            return false;
        }
    }
    true
}

fn flatten_into(
    path: &PathData,
    ctm: Matrix,
    points: &mut Vec<[f32; 2]>,
    subpaths: &mut Vec<(usize, usize)>,
) {
    let mut pt = 0usize;
    let mut start = points.len();
    let mut last = Point::default();
    let map = |p: Point| {
        let d = ctm.apply(p);
        [d.x as f32, d.y as f32]
    };
    for verb in path.verbs.iter() {
        match verb {
            PathVerb::MoveTo => {
                if points.len() > start {
                    subpaths.push((start, points.len()));
                }
                start = points.len();
                last = path.points[pt];
                points.push(map(last));
                pt += 1;
            }
            PathVerb::LineTo => {
                last = path.points[pt];
                points.push(map(last));
                pt += 1;
            }
            PathVerb::CurveTo => {
                let p0 = last;
                let (c1, c2, p3) = (path.points[pt], path.points[pt + 1], path.points[pt + 2]);
                pt += 3;
                for i in 1..=CURVE_SEGMENTS {
                    let t = i as f64 / CURVE_SEGMENTS as f64;
                    points.push(map(cubic(p0, c1, c2, p3, t)));
                }
                last = p3;
            }
            PathVerb::Close => {}
        }
    }
    if points.len() > start {
        subpaths.push((start, points.len()));
    }
}

#[inline]
fn cubic(p0: Point, c1: Point, c2: Point, p3: Point, t: f64) -> Point {
    let u = 1.0 - t;
    let (w0, w1, w2, w3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    Point {
        x: w0 * p0.x + w1 * c1.x + w2 * c2.x + w3 * p3.x,
        y: w0 * p0.y + w1 * c1.y + w2 * c2.y + w3 * p3.y,
    }
}

#[inline]
fn to_u8(v: f32) -> u8 {
    (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

fn color_to_bytes(c: &pdf_page_ir::Color) -> [u8; 4] {
    [to_u8(c.r), to_u8(c.g), to_u8(c.b), to_u8(c.a)]
}

/// Prepare a shading resource for rendering: resolve the ramp to bytes and
/// precompute the axis geometry. `to_device` maps shading space to device
/// space; its inverse maps each device pixel back to the shading parameter.
/// Returns `None` for unsupported shading types or a singular transform.
fn prepare_shading(
    res: &ShadingResource,
    to_device: Matrix,
    out_size: DeviceSize,
    honor_background: bool,
) -> Option<PreparedShading> {
    // `/BBox` clip mapped to a device-space AABB once, shared by every kind.
    let device_bbox = res.bbox.and_then(|bb| bbox_to_device_aabb(bb, to_device));
    // Mesh kinds rasterize a device-space layer and need no inverse.
    match &res.kind {
        ShadingKind::MeshTriangles { triangles, background } => {
            let bg = background_bytes(background, honor_background);
            return prepare_mesh_layer(
                triangles.iter().map(|t| {
                    [
                        device_vertex(&t[0], to_device),
                        device_vertex(&t[1], to_device),
                        device_vertex(&t[2], to_device),
                    ]
                }),
                bg,
                out_size,
            )
            .map(|mut sh| {
                sh.bbox = device_bbox;
                sh
            });
        }
        ShadingKind::MeshPatches { patches, background } => {
            let bg = background_bytes(background, honor_background);
            let mut triangles: Vec<[DevVertex; 3]> = Vec::new();
            for p in patches.iter() {
                tessellate_patch(p, to_device, &mut triangles);
            }
            return prepare_mesh_layer(triangles.into_iter(), bg, out_size).map(|mut sh| {
                sh.bbox = device_bbox;
                sh
            });
        }
        _ => {}
    }
    if !to_device.is_stably_invertible() {
        return None;
    }
    let inv = to_device.invert()?;
    match &res.kind {
        ShadingKind::FunctionGrid {
            domain,
            matrix,
            grid_w,
            grid_h,
            colors,
            background,
        } => {
            // Device → domain: invert (shading /Matrix ∘ to_device).
            let full = matrix.then(to_device);
            if !full.is_stably_invertible() {
                return None;
            }
            let (gw, gh) = (*grid_w as usize, *grid_h as usize);
            if gw == 0 || gh == 0 || colors.len() < gw * gh {
                return None;
            }
            Some(PreparedShading {
                kind: ShadingSpanKind::Grid {
                    domain: [
                        domain[0] as f64,
                        domain[1] as f64,
                        domain[2] as f64,
                        domain[3] as f64,
                    ],
                    gw,
                    gh,
                },
                inv: full.invert()?,
                ramp: colors.iter().map(color_to_bytes).collect(),
                extend: [false, false],
                background: background_bytes(background, honor_background),
                bbox: device_bbox,
            })
        }
        ShadingKind::Axial {
            coords,
            extend,
            ramp,
            background,
            ..
        } => {
            let d = [
                (coords[2] - coords[0]) as f64,
                (coords[3] - coords[1]) as f64,
            ];
            let dd = (d[0] * d[0] + d[1] * d[1]).max(1e-12);
            Some(PreparedShading {
                kind: ShadingSpanKind::Axial {
                    p0: [coords[0] as f64, coords[1] as f64],
                    d,
                    dd,
                },
                inv,
                ramp: ramp.iter().map(color_to_bytes).collect(),
                extend: *extend,
                background: background_bytes(background, honor_background),
                bbox: device_bbox,
            })
        }
        ShadingKind::Radial {
            coords,
            extend,
            ramp,
            background,
            ..
        } => Some(PreparedShading {
            kind: ShadingSpanKind::Radial {
                c0: [coords[0] as f64, coords[1] as f64, coords[2] as f64],
                c1: [coords[3] as f64, coords[4] as f64, coords[5] as f64],
            },
            inv,
            ramp: ramp.iter().map(color_to_bytes).collect(),
            extend: *extend,
            background: background_bytes(background, honor_background),
            bbox: device_bbox,
        }),
        ShadingKind::Unsupported { .. } => None,
        // Handled above (early return); unreachable here.
        ShadingKind::MeshTriangles { .. } | ShadingKind::MeshPatches { .. } => None,
    }
}

/// `/Background` resolved to straight RGBA bytes, honoring the §8.7.4.3 rule
/// that `sh` ignores it (pattern fills pass `honor_background = true`).
fn background_bytes(
    background: &Option<pdf_page_ir::Color>,
    honor_background: bool,
) -> Option<[u8; 4]> {
    if !honor_background {
        return None;
    }
    background.as_ref().map(color_to_bytes)
}

/// A mesh vertex transformed to device space, color resolved to bytes.
#[derive(Debug, Clone, Copy)]
struct DevVertex {
    x: f64,
    y: f64,
    rgba: [f32; 4],
}

fn device_vertex(v: &pdf_page_ir::MeshVertex, to_device: Matrix) -> DevVertex {
    let p = to_device.apply(Point {
        x: v.x as f64,
        y: v.y as f64,
    });
    DevVertex {
        x: p.x,
        y: p.y,
        rgba: [
            v.color.r.clamp(0.0, 1.0),
            v.color.g.clamp(0.0, 1.0),
            v.color.b.clamp(0.0, 1.0),
            v.color.a.clamp(0.0, 1.0),
        ],
    }
}

/// Rasterize device-space Gouraud triangles into a `Layer` shading: an RGBA
/// buffer over the triangles' device bounding box (or the whole output when a
/// `/Background` must fill the remainder), sampled at pixel centers with
/// barycentric color interpolation. Alpha 0 marks "not painted" — the layer
/// is consumed by `shade_pixel`, so clips, soft masks, fill shapes, and blend
/// modes all apply downstream exactly as for axial/radial shadings.
fn prepare_mesh_layer(
    triangles: impl Iterator<Item = [DevVertex; 3]>,
    background: Option<[u8; 4]>,
    out_size: DeviceSize,
) -> Option<PreparedShading> {
    let tris: Vec<[DevVertex; 3]> = triangles.collect();
    if tris.is_empty() && background.is_none() {
        return None;
    }
    let (ow, oh) = (out_size.width as f64, out_size.height as f64);
    let (mut x0, mut y0, mut x1, mut y1) = if background.is_some() {
        // Background fills everything the fill shape reaches: cover the output.
        (0.0f64, 0.0f64, ow, oh)
    } else {
        let (mut ax0, mut ay0, mut ax1, mut ay1) =
            (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for t in &tris {
            for v in t {
                ax0 = ax0.min(v.x);
                ay0 = ay0.min(v.y);
                ax1 = ax1.max(v.x);
                ay1 = ay1.max(v.y);
            }
        }
        (ax0, ay0, ax1, ay1)
    };
    x0 = x0.max(0.0).floor();
    y0 = y0.max(0.0).floor();
    x1 = x1.min(ow).ceil();
    y1 = y1.min(oh).ceil();
    if !(x1 > x0 && y1 > y0) {
        return None;
    }
    let (lx0, ly0) = (x0 as i32, y0 as i32);
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let fill = background.unwrap_or([0, 0, 0, 0]);
    let mut layer = vec![fill; w * h];

    for t in &tris {
        fill_gouraud_triangle(&mut layer, w, h, lx0, ly0, t);
    }
    Some(PreparedShading {
        kind: ShadingSpanKind::Layer { x0: lx0, y0: ly0, w, h },
        inv: Matrix::IDENTITY,
        ramp: layer,
        extend: [false, false],
        background,
        // Set by the caller (`prepare_shading`) from the shading's /BBox.
        bbox: None,
    })
}

/// Map a shading `/BBox` (target-space `[x0, y0, x1, y1]`) to a device-space
/// axis-aligned box via `to_device`. Exact under axis-aligned / flipped CTMs;
/// a conservative AABB of the four transformed corners under rotation. `None`
/// when a corner is non-finite (the clip is then simply not applied).
fn bbox_to_device_aabb(bb: [f32; 4], to_device: Matrix) -> Option<[f32; 4]> {
    let corners = [(bb[0], bb[1]), (bb[2], bb[1]), (bb[2], bb[3]), (bb[0], bb[3])];
    let (mut x0, mut y0, mut x1, mut y1) =
        (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (cx, cy) in corners {
        let p = to_device.apply(Point { x: cx as f64, y: cy as f64 });
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Some([x0 as f32, y0 as f32, x1 as f32, y1 as f32])
}

/// Scanline-fill one device-space triangle into the layer with barycentric
/// Gouraud interpolation at pixel centers.
fn fill_gouraud_triangle(
    layer: &mut [[u8; 4]],
    w: usize,
    h: usize,
    lx0: i32,
    ly0: i32,
    t: &[DevVertex; 3],
) {
    let (ax, ay) = (t[0].x, t[0].y);
    let (bx, by) = (t[1].x, t[1].y);
    let (cx, cy) = (t[2].x, t[2].y);
    // Twice the signed area; degenerate triangles paint nothing.
    let area = (bx - ax) * (cy - ay) - (cx - ax) * (by - ay);
    if area.abs() < 1e-12 {
        return;
    }
    let inv_area = 1.0 / area;
    let px0 = (ax.min(bx).min(cx).floor() as i64 - lx0 as i64).max(0) as usize;
    let py0 = (ay.min(by).min(cy).floor() as i64 - ly0 as i64).max(0) as usize;
    let px1 = ((ax.max(bx).max(cx).ceil() as i64 - lx0 as i64).max(0) as usize).min(w);
    let py1 = ((ay.max(by).max(cy).ceil() as i64 - ly0 as i64).max(0) as usize).min(h);
    for py in py0..py1 {
        let yc = (ly0 + py as i32) as f64 + 0.5;
        for px in px0..px1 {
            let xc = (lx0 + px as i32) as f64 + 0.5;
            // Barycentric weights of the pixel center.
            let wa = ((bx - xc) * (cy - yc) - (cx - xc) * (by - yc)) * inv_area;
            let wb = ((cx - xc) * (ay - yc) - (ax - xc) * (cy - yc)) * inv_area;
            let wc = 1.0 - wa - wb;
            if wa < 0.0 || wb < 0.0 || wc < 0.0 {
                continue;
            }
            let mut rgba = [0u8; 4];
            for ch in 0..4 {
                let v = wa * t[0].rgba[ch] as f64
                    + wb * t[1].rgba[ch] as f64
                    + wc * t[2].rgba[ch] as f64;
                rgba[ch] = (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
            }
            layer[py * w + px] = rgba;
        }
    }
}

/// Tessellate one tensor patch into device-space Gouraud triangles: evaluate
/// the bicubic tensor surface on an adaptive `n × n` parameter grid (density
/// from the patch's device extent, the standard subdivision-to-quads
/// approach of PDFium's `CPDF_CoonsPatch::Draw`), bilinearly interpolate the
/// four corner colors in `(u, v)`, and emit two triangles per cell.
fn tessellate_patch(
    patch: &pdf_page_ir::MeshPatch,
    to_device: Matrix,
    out: &mut Vec<[DevVertex; 3]>,
) {
    // Control points to device space once; the surface is affine-invariant.
    let mut p = [[0.0f64; 2]; 16];
    let (mut dx0, mut dy0, mut dx1, mut dy1) =
        (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (i, cp) in patch.points.iter().enumerate() {
        let q = to_device.apply(Point {
            x: cp[0] as f64,
            y: cp[1] as f64,
        });
        p[i] = [q.x, q.y];
        dx0 = dx0.min(q.x);
        dy0 = dy0.min(q.y);
        dx1 = dx1.max(q.x);
        dy1 = dy1.max(q.y);
    }
    // Grid density: ~one cell per 6 device pixels of the larger extent.
    let extent = (dx1 - dx0).max(dy1 - dy0).max(0.0);
    let n = ((extent / 6.0).ceil() as usize).clamp(4, 32);

    let bez = |k: f64, a: f64, b: f64, c: f64, d: f64| {
        let mk = 1.0 - k;
        mk * mk * mk * a + 3.0 * mk * mk * k * b + 3.0 * mk * k * k * c + k * k * k * d
    };
    let eval = |u: f64, v: f64| -> [f64; 2] {
        let mut out_p = [0.0f64; 2];
        for (axis, val) in out_p.iter_mut().enumerate() {
            // Rows i (u-direction curves in v), then the column in u.
            let mut row = [0.0f64; 4];
            for (i, r) in row.iter_mut().enumerate() {
                *r = bez(
                    v,
                    p[i * 4][axis],
                    p[i * 4 + 1][axis],
                    p[i * 4 + 2][axis],
                    p[i * 4 + 3][axis],
                );
            }
            *val = bez(u, row[0], row[1], row[2], row[3]);
        }
        out_p
    };
    let corner = |c: &pdf_page_ir::Color| -> [f32; 4] {
        [
            c.r.clamp(0.0, 1.0),
            c.g.clamp(0.0, 1.0),
            c.b.clamp(0.0, 1.0),
            c.a.clamp(0.0, 1.0),
        ]
    };
    let c00 = corner(&patch.colors[0]); // (u=0, v=0)
    let c01 = corner(&patch.colors[1]); // (u=0, v=1)
    let c11 = corner(&patch.colors[2]); // (u=1, v=1)
    let c10 = corner(&patch.colors[3]); // (u=1, v=0)
    let color_at = |u: f64, v: f64| -> [f32; 4] {
        let mut c = [0.0f32; 4];
        for ch in 0..4 {
            let e0 = (1.0 - v) * c00[ch] as f64 + v * c01[ch] as f64;
            let e1 = (1.0 - v) * c10[ch] as f64 + v * c11[ch] as f64;
            c[ch] = ((1.0 - u) * e0 + u * e1) as f32;
        }
        c
    };

    // Evaluate the (n+1)² grid once, then emit two triangles per cell.
    let mut grid: Vec<DevVertex> = Vec::with_capacity((n + 1) * (n + 1));
    for iv in 0..=n {
        let v = iv as f64 / n as f64;
        for iu in 0..=n {
            let u = iu as f64 / n as f64;
            let q = eval(u, v);
            grid.push(DevVertex {
                x: q[0],
                y: q[1],
                rgba: color_at(u, v),
            });
        }
    }
    let idx = |iu: usize, iv: usize| iv * (n + 1) + iu;
    for iv in 0..n {
        for iu in 0..n {
            let a = grid[idx(iu, iv)];
            let b = grid[idx(iu + 1, iv)];
            let c = grid[idx(iu + 1, iv + 1)];
            let d = grid[idx(iu, iv + 1)];
            out.push([a, b, c]);
            out.push([a, c, d]);
        }
    }
}

/// Lower a `sh` operator into a shading-painted fill of the current clip
/// region (or the whole surface when unclipped).
fn lower_shading_op(
    out: &mut CpuPreparedPage,
    res: &ShadingResource,
    ctm: Matrix,
    clip: Option<u32>,
) -> Option<DeviceRect> {
    // `sh` ignores /Background (§8.7.4.3): honor_background = false.
    let shading = prepare_shading(res, ctm, out.size, false)?;
    // The paint region: the clip's rectangular envelope, else the full output.
    let full = DeviceRect {
        x: 0,
        y: 0,
        width: out.size.width,
        height: out.size.height,
    };
    let (region, clip_has_mask) = match clip {
        Some(cid) => {
            let c = &out.clips[cid as usize];
            (intersect(full, c.bounds), c.has_mask)
        }
        None => (full, false),
    };
    if region.width == 0 || region.height == 0 {
        return None;
    }

    // A device-space rectangle covering the region (points are already device).
    let start = out.points.len();
    let x0 = region.x as f32;
    let y0 = region.y as f32;
    let x1 = (region.x + region.width as i32) as f32;
    let y1 = (region.y + region.height as i32) as f32;
    for p in [[x0, y0], [x1, y0], [x1, y1], [x0, y1]] {
        out.points.push(p);
    }
    let range_start = out.subpaths.len() as u32;
    out.subpaths.push((start, out.points.len()));
    let range_end = out.subpaths.len() as u32;

    out.ops.push(PreparedOp::Draw(PreparedCommand {
        origin: pdf_page_ir::PaintOrigin::default(),
        class: DrawClass::SolidPath,
        subpath_range: (range_start, range_end),
        rule: FillRule::NonZero,
        rgb: [0, 0, 0],
        premul: [0, 0, 0, 255],
        alpha: 255,
        opaque: false,
        bounds: region,
        clip,
        clip_has_mask,
        blend: pdf_page_ir::BlendMode::Normal,
        shading: Some(Box::new(shading)),
    }));
    Some(region)
}

#[cfg(test)]
mod font_cache_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use pdf_page_ir::{FontResource, ResourceKey};

    /// A genuine bare CFF (as a PDF embeds it): the `CFF ` table lifted out of a
    /// bundled Foxit face. Parsing it yields a `wrapped_bare_cff` program, which
    /// is exactly what the shared cache retains.
    fn bare_cff() -> Arc<[u8]> {
        let sfnt = pdf_font::StandardFont::Helvetica.program_data();
        let n = u16::from_be_bytes([sfnt[4], sfnt[5]]) as usize;
        for i in 0..n {
            let rec = 12 + i * 16;
            if &sfnt[rec..rec + 4] == b"CFF " {
                let off =
                    u32::from_be_bytes(sfnt[rec + 8..rec + 12].try_into().unwrap()) as usize;
                let len =
                    u32::from_be_bytes(sfnt[rec + 12..rec + 16].try_into().unwrap()) as usize;
                return Arc::from(&sfnt[off..off + len]);
            }
        }
        panic!("bundled face has no CFF table");
    }

    fn resource(program: Arc<[u8]>) -> FontResource {
        FontResource {
            key: ResourceKey {
                object_number: 1,
                generation: 0,
                variant: 0,
            },
            program,
            face_index: 0,
            synthetic_shear: 0.0,
            synthetic_embolden_em: 0.0,
        }
    }

    #[test]
    fn retains_and_serves_across_workers() {
        // Cache mechanics at the get/insert seam (the parse-cost gate in
        // `resolve_program_bytes` is exercised separately). A program inserted
        // by one worker is served to another keyed by content identity.
        let cache = SharedFontProgramCache::default();
        let res = resource(bare_cff());
        let key = FontProgramKey::for_resource(&res);
        let prog = FontProgram::parse(bare_cff()).expect("parse bare cff");
        assert!(prog.benefits_from_parse_cache());
        cache.insert(key, prog.clone(), prog.retained_bytes());
        assert_eq!(cache.len(), 1);
        let (h0, i0) = cache.stats();
        let served = cache.get(&key).expect("served from cache");
        let (h1, i1) = cache.stats();
        assert_eq!(h1 - h0, 1, "get is a shared hit");
        assert_eq!(i1, i0, "no extra insert");
        assert_eq!(served.units_per_em(), prog.units_per_em());
    }

    #[test]
    fn cheap_parses_are_not_retained() {
        // A bare CFF parses in microseconds, below `MIN_PARSE_TO_CACHE`, so the
        // resolve path must not retain it — this is what keeps a document of
        // cheap-to-parse fonts free of retention cost. The gate is a measured
        // wall-clock parse cost, so a scheduler hiccup under parallel test load
        // can legitimately push one parse over the threshold: retry with a
        // fresh cache and require that an uncontended parse stays uncached.
        for _ in 0..5 {
            let cache = SharedFontProgramCache::default();
            let res = resource(bare_cff());
            resolve_program_bytes(&res, Some(&cache)).expect("parse bare cff");
            if cache.len() == 0 && cache.stats().1 == 0 {
                return;
            }
        }
        panic!("cheap parse was retained on every attempt");
    }

    #[test]
    fn device_bounds_sane_rejects_corrupt_geometry() {
        let size = DeviceSize { width: 100, height: 80 };
        // Normal geometry within the viewport: accepted, clamped to the page.
        assert_eq!(device_bounds_sane(&[[10.0, 10.0], [50.0, 40.0]], size), Some((10, 10, 50, 40)));
        // Off-page but within the sane envelope: accepted (a partly-visible
        // shape clamps to the page edge).
        assert!(device_bounds_sane(&[[-20.0, -20.0], [30.0, 30.0]], size).is_some());
        // A coordinate far past 64× the viewport is corrupt: rejected.
        assert_eq!(device_bounds_sane(&[[0.0, 0.0], [1.0e9, 5.0]], size), None);
        // Non-finite coordinates are rejected.
        assert_eq!(device_bounds_sane(&[[0.0, 0.0], [f32::NAN, 5.0]], size), None);
        assert_eq!(device_bounds_sane(&[[f32::INFINITY, 0.0]], size), None);
    }

    #[test]
    fn identity_is_content_not_pointer() {
        let bytes = bare_cff();
        let r1 = resource(bytes.clone());
        // A *distinct allocation* with identical content is the SAME key — this
        // is what makes the cache hit across independently compiled pages, where
        // each page's font program is a fresh `Arc`.
        let independent: Arc<[u8]> = Arc::from(&bytes[..]);
        assert_ne!(bytes.as_ptr(), independent.as_ptr(), "genuinely distinct allocations");
        assert_eq!(
            FontProgramKey::for_resource(&r1),
            FontProgramKey::for_resource(&resource(independent))
        );
        // Different content -> different key.
        let mut other = bytes.to_vec();
        other[10] ^= 0xff;
        assert_ne!(
            FontProgramKey::for_resource(&r1),
            FontProgramKey::for_resource(&resource(Arc::from(other)))
        );
    }

    #[test]
    fn serves_across_independent_allocations() {
        // The cross-page mechanism: two resources with identical bytes but
        // distinct `Arc`s (as separate page compilations produce) map to one
        // key, so an entry inserted for the first is served for the second.
        let cache = SharedFontProgramCache::default();
        let first = resource(bare_cff());
        let second = resource(Arc::from(&bare_cff()[..]));
        assert_ne!(first.program.as_ptr(), second.program.as_ptr());
        let key1 = FontProgramKey::for_resource(&first);
        let key2 = FontProgramKey::for_resource(&second);
        assert_eq!(key1, key2, "content identity, not pointer");
        let prog = FontProgram::parse(bare_cff()).expect("parse");
        cache.insert(key1, prog.clone(), prog.retained_bytes());
        let (h0, _) = cache.stats();
        assert!(cache.get(&key2).is_some(), "distinct allocation is a hit");
        assert_eq!(cache.stats().0 - h0, 1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn evicts_by_byte_budget() {
        // A budget below one program's charge forces the cache to bound itself:
        // a single entry is always kept (the just-inserted one), never grows.
        let cache = SharedFontProgramCache::new(1);
        let program = FontProgram::parse(bare_cff()).expect("parse");
        for i in 0..8u64 {
            // Distinct synthetic keys sharing one shard behavior; charge each
            // above the cap so eviction runs every insert.
            let key = FontProgramKey {
                h0: 0x1000 + i * 0x40,
                h1: i,
                len: 4096,
                face: 0,
            };
            cache.insert(key, program.clone(), 4096);
        }
        // Per-shard cap of 0 (1/8) keeps at most one entry per shard it touched.
        assert!(cache.len() <= SharedFontProgramCache::SHARDS);
    }

    #[test]
    fn none_shared_cache_still_parses() {
        let res = resource(bare_cff());
        let p = resolve_program_bytes(&res, None).expect("parse without cache");
        assert!(p.benefits_from_parse_cache());
    }
}

#[cfg(test)]
mod glyph_cache_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use pdf_page_ir::{Color, FontId, FontResource, PlacedGlyph, ResourceKey};

    fn helvetica() -> Arc<[u8]> {
        Arc::from(pdf_font::StandardFont::Helvetica.program_data())
    }

    fn font_key() -> FontProgramKey {
        FontProgramKey::for_resource(&FontResource {
            key: ResourceKey {
                object_number: 1,
                generation: 0,
                variant: 0,
            },
            program: helvetica(),
            face_index: 0,
            synthetic_shear: 0.0,
            synthetic_embolden_em: 0.0,
        })
    }

    fn base_key(glyph: u32) -> GlyphCacheKey {
        GlyphCacheKey {
            font: font_key(),
            glyph,
            la: 100,
            lb: 0,
            lc: 0,
            ld: -100,
            sx: 0,
            sy: 0,
            hinted: false,
        }
    }

    fn nonempty_bitmap() -> Arc<GlyphBitmap> {
        Arc::new(GlyphBitmap {
            left: 0,
            top: -8,
            width: 6,
            height: 8,
            cov: vec![200u8; 6 * 8].into_boxed_slice(),
        })
    }

    #[test]
    fn hit_miss_and_stats() {
        let cache = SharedGlyphCache::default();
        let key = base_key(3);
        assert!(cache.get(&key).is_none(), "cold miss");
        cache.insert(key, nonempty_bitmap());
        assert_eq!(cache.len(), 1);
        let (h0, m0, i0) = cache.stats();
        let served = cache.get(&key).expect("served after insert");
        assert_eq!(served.width, 6);
        let (h1, m1, i1) = cache.stats();
        assert_eq!(h1 - h0, 1, "one hit");
        assert_eq!(m1, m0, "no new miss");
        assert_eq!(i1, i0, "no new insert");
    }

    #[test]
    fn key_distinct_for_hinting_and_transform() {
        let cache = SharedGlyphCache::default();
        let unhinted = base_key(5);
        cache.insert(unhinted, nonempty_bitmap());

        // Same glyph, different hinting state → distinct slot (a miss).
        let hinted = GlyphCacheKey { hinted: true, ..unhinted };
        assert!(cache.get(&hinted).is_none(), "hinting state is part of key");

        // Same glyph, different quantized transform → distinct slot.
        let scaled = GlyphCacheKey { ld: -200, ..unhinted };
        assert!(cache.get(&scaled).is_none(), "transform is part of key");

        // Same glyph, different sub-pixel phase → distinct slot.
        let phased = GlyphCacheKey { sx: 2, ..unhinted };
        assert!(cache.get(&phased).is_none(), "sub-pixel phase is part of key");

        // Different glyph index → distinct slot.
        let other = GlyphCacheKey { glyph: 6, ..unhinted };
        assert!(cache.get(&other).is_none(), "glyph id is part of key");

        // The original still hits.
        assert!(cache.get(&unhinted).is_some(), "original entry retained");
    }

    #[test]
    fn evicts_by_byte_budget() {
        // A tiny budget forces bounded residency: each shard keeps at most one
        // entry, so the total never exceeds the shard count regardless of inserts.
        let cache = SharedGlyphCache::new(1);
        for i in 0..64u32 {
            let key = GlyphCacheKey { glyph: i, ..base_key(0) };
            cache.insert(key, nonempty_bitmap());
        }
        assert!(
            cache.len() <= SharedGlyphCache::SHARDS,
            "bounded to one entry per shard, got {}",
            cache.len()
        );
        // Byte accounting stays under the (per-shard × shards) budget.
        assert!(cache.retained_bytes() <= SharedGlyphCache::SHARDS * nonempty_bitmap().charge());
    }

    fn empty_page(w: u32, h: u32) -> CpuPreparedPage {
        CpuPreparedPage {
            size: DeviceSize {
                width: w,
                height: h,
            },
            ops: Vec::new(),
            clips: Vec::new(),
            points: Vec::new(),
            subpaths: Vec::new(),
            codecs: pdf_image::CodecRegistry::default(),
            decode_limits: pdf_image::DecodeLimits::default(),
            hinting: pdf_font::HintingPolicy::None,
            diagnostics: RenderDiagnostics::default(),
            image_cache: None,
            #[cfg(feature = "profiling")]
            profile: std::cell::RefCell::new(pdf_profiling::ProfileReport::new()),
            #[cfg(feature = "profiling")]
            decode_cache: None,
        }
    }

    fn run_at(font_size: f64, transform: Matrix, gid: u32) -> GlyphRun {
        GlyphRun {
            font: FontId(0),
            font_size,
            transform,
            glyphs: Arc::from([PlacedGlyph {
                glyph: gid,
                x: 0.0,
                y: 0.0,
            }]),
            render_mode: 0,
        }
    }

    #[test]
    fn eligible_run_populates_then_hits() {
        let prog = FontProgram::parse(helvetica()).expect("parse helvetica");
        let gid = prog.gid_for_char('A').expect("gid for A");
        let cache = SharedGlyphCache::default();
        let mut raster = crate::raster::RasterKernel::default();
        let color = Color::from_rgb(0.0, 0.0, 0.0);
        // Small, axis-aligned (ppem = 40) → eligible.
        let run = run_at(40.0, Matrix::translate(20.0, 60.0), gid);

        let mut page = empty_page(200, 200);
        let handled = try_cached_glyph_run(
            &mut page,
            &run,
            &prog,
            Matrix::IDENTITY,
            color,
            1.0,
            None,
            pdf_page_ir::BlendMode::Normal,
            &cache,
            font_key(),
            &mut raster,
        );
        assert!(handled.is_some(), "eligible run is handled, not fallback");
        assert!(handled.unwrap().is_some(), "produced device bounds");
        assert_eq!(page.ops.len(), 1, "emitted one glyph-run op");
        assert!(matches!(page.ops[0], PreparedOp::GlyphRun(_)));
        let (h0, _, i0) = cache.stats();
        assert_eq!(i0, 1, "one glyph inserted on the miss");

        // A second identical run hits the cache (no new insert).
        let mut page2 = empty_page(200, 200);
        try_cached_glyph_run(
            &mut page2,
            &run,
            &prog,
            Matrix::IDENTITY,
            color,
            1.0,
            None,
            pdf_page_ir::BlendMode::Normal,
            &cache,
            font_key(),
            &mut raster,
        );
        let (h1, _, i1) = cache.stats();
        assert_eq!(i1, i0, "no new insert on repeat");
        assert_eq!(h1 - h0, 1, "second occurrence is a hit");
    }

    #[test]
    fn rotated_and_oversized_runs_are_ineligible() {
        let prog = FontProgram::parse(helvetica()).expect("parse helvetica");
        let gid = prog.gid_for_char('A').expect("gid");
        let cache = SharedGlyphCache::default();
        let mut raster = crate::raster::RasterKernel::default();
        let color = Color::from_rgb(0.0, 0.0, 0.0);

        // Rotated/skewed transform → escape hatch → fallback (None), nothing cached.
        let skew = Matrix {
            a: 1.0,
            b: 0.4,
            c: -0.4,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        };
        let mut page = empty_page(400, 400);
        let r = try_cached_glyph_run(
            &mut page,
            &run_at(40.0, skew, gid),
            &prog,
            Matrix::IDENTITY,
            color,
            1.0,
            None,
            pdf_page_ir::BlendMode::Normal,
            &cache,
            font_key(),
            &mut raster,
        );
        assert!(r.is_none(), "rotated run is ineligible");
        assert!(page.ops.is_empty(), "no op emitted for a fallback run");

        // Oversized (ppem 400 > cap) → ineligible.
        let big = try_cached_glyph_run(
            &mut page,
            &run_at(400.0, Matrix::translate(10.0, 10.0), gid),
            &prog,
            Matrix::IDENTITY,
            color,
            1.0,
            None,
            pdf_page_ir::BlendMode::Normal,
            &cache,
            font_key(),
            &mut raster,
        );
        assert!(big.is_none(), "oversized run is ineligible");
        assert_eq!(cache.stats().2, 0, "nothing inserted by ineligible runs");
    }

    #[test]
    fn rasterizes_real_outline_to_tight_bitmap() {
        let prog = FontProgram::parse(helvetica()).expect("parse");
        let gid = prog.gid_for_char('H').expect("gid for H");
        let outline = prog.outline(gid).expect("H has an outline");
        let upem = prog.units_per_em() as f64;
        // Unhinted device linear map at 100 px em, y-flipped (device y-down).
        let s = 100.0 / upem;
        let mut raster = crate::raster::RasterKernel::default();
        let bmp = rasterize_glyph_bitmap(
            &outline,
            |p| [(s * p[0] as f64) as f32, (-s * p[1] as f64) as f32],
            &mut raster,
        );
        assert!(bmp.width > 0 && bmp.height > 0, "H has a non-empty bitmap");
        assert!(
            (bmp.cov.len()) == (bmp.width * bmp.height) as usize,
            "coverage sized to bbox"
        );
        assert!(bmp.cov.iter().any(|&c| c > 0), "some coverage painted");
        // 'H' cap height is near the em; at 100 px em it should be tens of px.
        assert!(bmp.height >= 40 && bmp.height <= 120, "plausible H height: {}", bmp.height);
        // Top is above the baseline (negative, since y-down and origin at baseline).
        assert!(bmp.top < 0, "cap sits above the baseline: top={}", bmp.top);
    }

    #[test]
    fn empty_outline_yields_empty_bitmap() {
        let mut raster = crate::raster::RasterKernel::default();
        let outline = pdf_font::Outline::default();
        let bmp = rasterize_glyph_bitmap(&outline, |p| p, &mut raster);
        assert_eq!(bmp.width, 0);
        assert!(place_glyph(bmp, (10, 10)).is_none(), "empty glyph places nothing");
    }
}

#[cfg(test)]
mod image_cache_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pdf_page_ir::{
        Color, ImageColorSpace, ImageIr, InterpolationMode, PageBounds, PageComplexity,
        PageFeatures, Rect, ResourceKey,
    };

    /// A tiny valid grayscale JPEG payload produced by the in-house encoder.
    fn jpeg_payload(seed: u8) -> Vec<u8> {
        let (w, h) = (16u16, 16u16);
        let px: Vec<u8> = (0..w as usize * h as usize)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect();
        pdf_image::jpeg::encoder::write_jpeg(&px, w, h, false, 90, false).expect("encode")
    }

    fn dct_page(payload: Vec<u8>) -> CompiledPage {
        let image = ImageIr {
            key: ResourceKey { object_number: 7, generation: 0, variant: 0 },
            width: 16,
            height: 16,
            is_stencil: false,
            interpolation: InterpolationMode::Nearest,
            soft_mask: None,
            bits_per_component: 8,
            color_space: ImageColorSpace::Gray,
            decode: None,
            samples: None,
            codec: Some(pdf_page_ir::ImageCodecKind::Dct),
            codec_data: Some(Arc::from(payload)),
            codec_parms: None,
            smask: None,
            mask: None,
            smask_in_data: 0,
            lowering_degraded: false,
        };
        CompiledPage {
            schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
            bounds: PageBounds {
                crop: Rect { x0: 0.0, y0: 0.0, x1: 16.0, y1: 16.0 },
                rotate: 0,
            },
            operations: Arc::from([
                DisplayOp::ConcatTransform(Matrix::scale(16.0, 16.0)),
                DisplayOp::DrawImage {
                    image: pdf_page_ir::ImageId(0),
                    paint: pdf_page_ir::PaintId(0),
                    transform: Matrix::IDENTITY,
                    alpha: 1.0,
                    blend: pdf_page_ir::BlendMode::Normal,
                },
            ]),
            paths: Arc::from([]),
            paints: Arc::from([Paint::Solid(Color::BLACK)]),
            stroke_styles: Arc::from([]),
            glyph_runs: Arc::from([]),
            fonts: Arc::from([]),
            images: Arc::from([image]),
            masks: Arc::from([]),
            groups: Arc::from([]),
            shadings: Arc::from([]),
            tilings: Arc::from([]),
            features: PageFeatures::IMAGES,
            complexity: PageComplexity::default(),
        }
    }

    fn registry() -> pdf_image::CodecRegistry {
        pdf_image::CodecRegistry::new([
            Arc::new(pdf_image::JpegCodec) as Arc<dyn pdf_image::ImageCodec>
        ])
    }

    fn lower_page(
        page: &CompiledPage,
        cache: Option<&Arc<SharedImageCache>>,
    ) -> CpuPreparedPage {
        lower_impl(
            page,
            Matrix::IDENTITY,
            DeviceSize { width: 16, height: 16 },
            &registry(),
            &pdf_image::DecodeLimits::default(),
            pdf_font::HintingPolicy::None,
            None,
            None,
            None,
            cache,
            #[cfg(feature = "profiling")]
            None,
        )
    }

    fn render_pixels(prepared: &CpuPreparedPage) -> Vec<u8> {
        let mut surface = crate::surface::Surface::new(16, 16, pdf_render_api::Background::White);
        let mut ctx = crate::exec::CpuWorkerContext::new();
        let mut stats = crate::stats::RenderStats::default();
        crate::exec::execute(prepared, &mut surface, &mut ctx, &mut stats);
        let (_stride, pixels) =
            surface.into_output(pdf_render_api::OutputFormat::Rgba8PremultipliedSrgb);
        pixels
    }

    fn prepared_samples(prepared: &CpuPreparedPage) -> Arc<[u8]> {
        prepared
            .ops
            .iter()
            .find_map(|op| match op {
                PreparedOp::Image(img) => Some(img.samples.clone()),
                _ => None,
            })
            .expect("an image op was lowered")
    }

    /// The A2 gate: rendering with the cache (cold and warm) is byte-identical
    /// to rendering without it.
    #[test]
    fn cache_on_off_renders_byte_identical() {
        let page = dct_page(jpeg_payload(3));
        let cache = Arc::new(SharedImageCache::default());

        let uncached = lower_page(&page, None);
        let cold = lower_page(&page, Some(&cache));
        let warm = lower_page(&page, Some(&cache));

        assert_eq!(cache.stats(), (1, 1, 1), "miss+insert then hit");
        assert_eq!(
            prepared_samples(&uncached),
            prepared_samples(&cold),
            "cold cache decode matches uncached decode"
        );
        assert_eq!(
            prepared_samples(&cold),
            prepared_samples(&warm),
            "warm hit serves the identical payload"
        );
        let base = render_pixels(&uncached);
        assert_eq!(base, render_pixels(&cold), "cache-off vs cache-cold pixels");
        assert_eq!(base, render_pixels(&warm), "cache-off vs cache-warm pixels");
    }

    /// Distinct payloads must not collide, and the LRU must bound residency.
    #[test]
    fn distinct_payloads_get_distinct_entries_and_lru_bounds_bytes() {
        let cache = Arc::new(SharedImageCache::default());
        let page_a = dct_page(jpeg_payload(3));
        let page_b = dct_page(jpeg_payload(200));
        let a = prepared_samples(&lower_page(&page_a, Some(&cache)));
        let b = prepared_samples(&lower_page(&page_b, Some(&cache)));
        assert_ne!(a, b, "different payloads decode differently");
        assert_eq!(cache.len(), 2);

        // A cache with a tiny budget stays bounded under repeated inserts.
        let tiny = Arc::new(SharedImageCache::new(SharedImageCache::SHARDS * 512));
        for seed in 0..32u8 {
            let _ = lower_page(&dct_page(jpeg_payload(seed)), Some(&tiny));
        }
        assert!(
            tiny.retained_bytes() <= SharedImageCache::SHARDS * 512 + 2048,
            "retained {} bytes exceeds budget",
            tiny.retained_bytes()
        );
    }
}
