// src/encode/iw44/codec.rs

use super::coeff_map::CoeffMap;
use super::constants::BAND_BUCKETS;
use crate::encode::zc::{BitContext, ZpEncoderCursor};
use std::sync::atomic::{AtomicUsize, Ordering};

// State flags for coefficients and buckets
const UNK: u8 = 0x01; // Unknown state
/// Coefficient state flags
const NEW: u8 = 0x02; // New coefficient to be encoded
const ACTIVE: u8 = 0x04; // Active coefficient (already encoded)
const ZERO: u8 = 0x00; // Zero state (coefficient not significant)

/// Context number used by the DjVu reference for "raw" (non-adaptive) bits
const RAW_CONTEXT_ID: BitContext = 129;
const RAW_CONTEXT_128: BitContext = 128;
const RAW_CONTEXT_129: BitContext = 129;

/// 1 bit / coefficient (32 × smaller than `Vec<bool>`)
const WORD_BITS: usize = 32;

static ZPTRACE_COUNT: AtomicUsize = AtomicUsize::new(0);

// Bit counting instrumentation for size gap investigation
static ROOT_BITS: AtomicUsize = AtomicUsize::new(0);
static BUCKET_BITS: AtomicUsize = AtomicUsize::new(0);
static START_BITS: AtomicUsize = AtomicUsize::new(0);
static SIGN_BITS: AtomicUsize = AtomicUsize::new(0);
static MANTISSA_ADAPTIVE: AtomicUsize = AtomicUsize::new(0);
static MANTISSA_RAW: AtomicUsize = AtomicUsize::new(0);
static COEFF_NEW_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLICE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn bit_counts_enabled() -> bool {
    match std::env::var("IW44_BIT_COUNTS") {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
}

#[inline]
fn slice_stats_enabled() -> bool {
    match std::env::var("IW44_SLICE_STATS") {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
}

#[inline]
fn coeff_dump_enabled() -> bool {
    match std::env::var("IW44_COEFF_DUMP") {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
}

#[inline]
fn zp_trace_enabled() -> bool {
    match std::env::var("IW44_ZPTRACE") {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
}

#[inline]
fn zp_trace_limit() -> usize {
    std::env::var("IW44_ZPTRACE_LIMIT")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(2000)
}

pub fn print_bit_counts() {
    if bit_counts_enabled() {
        eprintln!("=== IW44 Bit Count Summary ===");
        eprintln!("Root bits: {}", ROOT_BITS.load(std::sync::atomic::Ordering::Relaxed));
        eprintln!("Bucket bits: {}", BUCKET_BITS.load(std::sync::atomic::Ordering::Relaxed));
        eprintln!("Start bits: {}", START_BITS.load(std::sync::atomic::Ordering::Relaxed));
        eprintln!("Sign bits: {}", SIGN_BITS.load(std::sync::atomic::Ordering::Relaxed));
        eprintln!("Mantissa adaptive: {}", MANTISSA_ADAPTIVE.load(std::sync::atomic::Ordering::Relaxed));
        eprintln!("Mantissa raw: {}", MANTISSA_RAW.load(std::sync::atomic::Ordering::Relaxed));
        eprintln!("NEW coefficients activated: {}", COEFF_NEW_COUNT.load(std::sync::atomic::Ordering::Relaxed));
        let total = ROOT_BITS.load(std::sync::atomic::Ordering::Relaxed) +
                   BUCKET_BITS.load(std::sync::atomic::Ordering::Relaxed) +
                   START_BITS.load(std::sync::atomic::Ordering::Relaxed) +
                   SIGN_BITS.load(std::sync::atomic::Ordering::Relaxed) +
                   MANTISSA_ADAPTIVE.load(std::sync::atomic::Ordering::Relaxed) +
                   MANTISSA_RAW.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!("Total bits: {}", total);
        eprintln!("==============================");
    }
}

#[inline]
fn zp_trace(
    kind: &'static str,
    bit: bool,
    ctx_name: &'static str,
    ctx_idx: i32,
    ctx_val: u8,
    plane_bit: i32,
    band: i32,
    blockno: usize,
    buckno: i32,
    coeffi: i32,
) {
    if !zp_trace_enabled() {
        return;
    }
    let n = ZPTRACE_COUNT.fetch_add(1, Ordering::Relaxed);
    if n >= zp_trace_limit() {
        return;
    }
    eprintln!(
        "ZPTRACE kind={} bit={} ctx_name={} ctx_idx={} ctx_val={} plane_bit={} band={} block={} buck={} coeff={}",
        kind,
        bit as u8,
        ctx_name,
        ctx_idx,
        ctx_val,
        plane_bit,
        band,
        blockno,
        buckno,
        coeffi
    );
}

#[inline]
fn words_for_coeffs(n: usize) -> usize {
    (n + WORD_BITS - 1) / WORD_BITS
}

/// Represents the IW44 codec for encoding wavelet coefficients.
/// Each codec instance owns its own slice state (curbit, curband) as per djvulibre design.
pub struct Codec {
    pub map: CoeffMap,                    // Original coefficient map
    pub emap: CoeffMap,                   // Encoded coefficient map
    pub coeff_state: Vec<u8>,             // State of each coefficient
    pub bucket_state: Vec<u8>,            // State of each bucket
    pub quant_hi: [i32; 10],              // Quantization thresholds for bands 1-9
    pub quant_lo: [i32; 16],              // Quantization thresholds for band 0
    pub ctx_root: BitContext,             // Context for root bit
    pub ctx_bucket: Vec<Vec<BitContext>>, // Contexts for bucket bits [band][ctx]
    pub ctx_start: Vec<BitContext>,       // Contexts for new coefficient activation [ctx]
    pub ctx_mant: BitContext,             // Context for mantissa bits
    pub signif: Vec<u32>, // 1 bit / coefficient (1 == coefficient is already significant)
    // Per-codec slice state (owned by each Y/Cb/Cr codec independently)
    pub curbit: i32,  // Current bitplane (starts at 1, goes to -1 when done)
    pub curband: i32, // Current band (0-9)
    pub lossless: bool, // True if encoding in lossless mode (thresholds stay >= 1)
}

impl Codec {
    /// Creates a new Codec instance for the given coefficient map and parameters.
    pub fn new(map: CoeffMap, params: &super::EncoderParams) -> Self {
        let num_blocks = map.num_blocks;
        let max_buckets = 64; // Each block has up to 64 buckets
        let max_coeffs_per_bucket = 16;

        // Initialize quantization thresholds exactly like djvulibre IW44Image.cpp constructor
        let iw_quant = &super::constants::IW_QUANT;
        let mut quant_lo = [0i32; 16];
        let mut quant_hi = [0i32; 10];

        // Fill quant_lo[0..15] from iw_quant following djvulibre logic EXACTLY
        let mut i = 0;
        let mut q_idx = 0;

        // -- lo coefficients (exact match to C++ logic)
        // First loop: for (j=0; i<4; j++) quant_lo[i++] = *q++;
        for _j in 0..4 {
            if i < 4 && q_idx < iw_quant.len() {
                quant_lo[i] = iw_quant[q_idx];
                i += 1;
                q_idx += 1;
            }
        }
        // Second loop: for (j=0; j<4; j++) quant_lo[i++] = *q; (q does NOT advance)
        for _j in 0..4 {
            if i < 8 && q_idx < iw_quant.len() {
                quant_lo[i] = iw_quant[q_idx];
                i += 1;
            }
        }
        q_idx += 1;
        // Third loop: for (j=0; j<4; j++) quant_lo[i++] = *q;
        for _j in 0..4 {
            if i < 12 && q_idx < iw_quant.len() {
                quant_lo[i] = iw_quant[q_idx];
                i += 1;
            }
        }
        q_idx += 1;
        // Fourth loop: for (j=0; j<4; j++) quant_lo[i++] = *q;
        for _j in 0..4 {
            if i < 16 && q_idx < iw_quant.len() {
                quant_lo[i] = iw_quant[q_idx];
                i += 1;
            }
        }
        q_idx += 1; // Now q_idx = 7, pointing to iw_quant[7]

        // Fill quant_hi[0..9] following djvulibre logic
        quant_hi[0] = 0; // Band 0 uses quant_lo values
        for j in 1..10 {
            if q_idx < iw_quant.len() {
                quant_hi[j] = iw_quant[q_idx];
                q_idx += 1;
            } else {
                quant_hi[j] = 0x8000; // fallback
            }
        }

        // Apply quantization multiplier for quality/size tuning (only in lossy mode)
        // In lossless mode, we use normal thresholds and let them decay to 1
        if !params.lossless && params.quant_multiplier != 1.0 {
            for i in 0..16 {
                quant_lo[i] = (quant_lo[i] as f32 * params.quant_multiplier) as i32;
            }
            for j in 1..10 {
                quant_hi[j] = (quant_hi[j] as f32 * params.quant_multiplier) as i32;
            }
        }

        // Initialize contexts
        let mut ctx_bucket = Vec::with_capacity(10);
        for _ in 0..10 {
            ctx_bucket.push(vec![0u8; 8]); // 8 contexts per band (0-7)
        }
        let ctx_start = vec![0u8; 16]; // 16 contexts (0-15)

        let coeffs = num_blocks * max_buckets * max_coeffs_per_bucket;

        Codec {
            emap: CoeffMap::new(map.iw, map.ih), // Encoded map starts empty
            map,
            coeff_state: vec![ZERO; num_blocks * max_buckets * max_coeffs_per_bucket],
            bucket_state: vec![ZERO; num_blocks * max_buckets],
            quant_hi,
            quant_lo,
            ctx_root: 0u8,
            ctx_bucket,
            ctx_start,
            ctx_mant: 0u8,
            signif: vec![0; words_for_coeffs(coeffs)],
            // Initialize slice state (matches djvulibre IW44Image constructor)
            curbit: 1,  // Start at bitplane 1
            curband: 0, // Start at band 0
            lossless: params.lossless,
        }
    }

    /// Returns a reference to the coefficient map.
    pub fn map(&self) -> &CoeffMap {
        &self.map
    }

    #[inline]
    fn is_signif(&self, idx: usize) -> bool {
        (self.signif[idx / WORD_BITS] >> (idx % WORD_BITS)) & 1 != 0
    }

    #[inline]
    fn mark_signif(&mut self, idx: usize) {
        self.signif[idx / WORD_BITS] |= 1 << (idx % WORD_BITS);
    }

    /// Quickly scans if there is any work to be done for a given (bit, band) slice.
    /// Returns true if at least one coefficient is either NEW or ACTIVE.
    /// This is much faster than the full two-pass approach as it returns immediately
    /// upon finding the first instance of activity.
    pub fn has_data_for_slice(&self, _bit: i32, band: i32) -> bool {
        // First, quick check if there are any active coefficients in this band
        let band = band as usize;
        let _th_hi = self.quant_hi[band];
        let bucket_info = BAND_BUCKETS[band];

        for blockno in 0..self.map.num_blocks {
            let coeff_base_idx = blockno * 64 * 16;
            for bucket_offset in 0..bucket_info.size {
                let bucket_idx = bucket_info.start + bucket_offset;

                // Check for ACTIVE coefficients (already significant)
                for i in 0..16 {
                    let gidx = coeff_base_idx + bucket_idx * 16 + i;
                    if self.is_signif(gidx) {
                        return true; // Found an active coefficient, slice has data
                    }
                }

                // Check for NEW coefficients
                if let Some(coeffs) = self.map.blocks[blockno].get_bucket(bucket_idx as u8) {
                    for i in 0..16 {
                        let step = if band == 0 {
                            self.quant_lo[i]
                        } else {
                            self.quant_hi[band]
                        };
                        if (coeffs[i] as i32).abs() >= step {
                            return true; // Found a new significant coefficient (matches C++ and encode_prepare)
                        }
                    }
                }
            }
        }

        false // Scanned everything, the slice is truly null
    }

    /// This is the encode_slice implementation - temporarily removing slice activity optimization
    pub fn encode_slice(
        &mut self,
        zp: &mut dyn ZpEncoderCursor,
        bit: i32,
        band: i32,
    ) -> Result<bool, super::EncoderError> {
        if bit < 0 {
            return Ok(false);
        }

        // Skip the slice activity optimization for now - go directly to block encoding
        let fbucket = BAND_BUCKETS[band as usize].start;
        let nbucket = BAND_BUCKETS[band as usize].size;

        for blockno in 0..self.map.num_blocks {
            self.encode_buckets(zp, bit, band, blockno, fbucket, nbucket)?;
        }

        Ok(true)
    }

    /// Prepares the state of coefficients and buckets for encoding.
    /// Returns block-wide OR of {UNK,NEW,ACTIVE} bits ("bbstate").
    pub fn encode_prepare(
        &mut self,
        band: i32,
        fbucket: usize,
        nbucket: usize,
        blockno: usize,
        bit: i32,
    ) -> u8 {
        let _th_hi = self.quant_hi[band as usize];
        let coeff_base = blockno * 64 * 16;
        let bucket_base = blockno * 64;

        let mut bbstate = 0;

        for buck in 0..nbucket {
            let bucket_idx = fbucket + buck;
            let coeff_idx0 = coeff_base + bucket_idx * 16;
            let src = self.map.blocks[blockno].get_bucket(bucket_idx as u8);
            let ep = self.emap.blocks[blockno].get_bucket(bucket_idx as u8);
            let mut bstate = 0;

            if band != 0 {
                // Band other than zero: derive state from pcoeff/epcoeff like DjVuLibre
                // Use current quant_hi (already decayed by prior slices).
                let thres = self.quant_hi[band as usize];

                if let Some(src16) = src {
                    if let Some(ep16) = ep {
                        for i in 0..16 {
                            let gidx = coeff_idx0 + i;
                            let mut cstate = UNK;
                            if ep16[i] != 0 {
                                cstate = ACTIVE;
                            } else if (src16[i] as i32).abs() >= thres {
                                cstate = NEW | UNK;
                                // Dump coefficients that meet threshold
                                if coeff_dump_enabled() && blockno == 0 && buck == 0 {
                                    eprintln!(
                                        "COEFF_DUMP band={} bit={} thresh={} coeff[{}]={} (abs={}) -> NEW",
                                        band, bit, thres, i, src16[i], (src16[i] as i32).abs()
                                    );
                                }
                            } else if coeff_dump_enabled() && blockno == 0 && buck == 0 && i < 4 {
                                // Also dump first few that DON'T meet threshold for comparison
                                eprintln!(
                                    "COEFF_DUMP band={} bit={} thresh={} coeff[{}]={} (abs={}) -> UNK",
                                    band, bit, thres, i, src16[i], (src16[i] as i32).abs()
                                );
                            }
                            self.coeff_state[gidx] = cstate;
                            bstate |= cstate;
                        }
                    } else {
                        for i in 0..16 {
                            let gidx = coeff_idx0 + i;
                            let mut cstate = UNK;
                            if (src16[i] as i32).abs() >= thres {
                                cstate = NEW | UNK;
                                if coeff_dump_enabled() && blockno == 0 && buck == 0 {
                                    eprintln!(
                                        "COEFF_DUMP band={} bit={} thresh={} coeff[{}]={} (abs={}) -> NEW (no ep)",
                                        band, bit, thres, i, src16[i], (src16[i] as i32).abs()
                                    );
                                }
                            }
                            self.coeff_state[gidx] = cstate;
                            bstate |= cstate;
                        }
                    }
                } else {
                    bstate = UNK;
                }
            } else {
                // Band zero: use prior coeff_state ZERO/UNK behavior like DjVuLibre
                // CRITICAL: Must read existing cstate[i] value first (C++ does this)
                if let Some(src16) = src {
                    for i in 0..16 {
                        let gidx = coeff_idx0 + i;
                        let thres = self.quant_lo[i];
                        // Read existing state (C++: int cstatetmp = cstate[i];)
                        let mut cstatetmp = self.coeff_state[gidx];

                        // Safety check: validate coefficient state
                        #[cfg(debug_assertions)]
                        {
                            debug_assert!(
                                cstatetmp == ZERO || cstatetmp == UNK || cstatetmp == ACTIVE || cstatetmp == (NEW | UNK),
                                "Invalid coeff state: {} at gidx={}", cstatetmp, gidx
                            );
                        }

                        // Only modify if not ZERO
                        if cstatetmp != ZERO {
                            cstatetmp = UNK;
                            if let Some(ep16) = ep {
                                if ep16[i] != 0 {
                                    cstatetmp = ACTIVE;
                                } else if (src16[i] as i32).abs() >= thres {
                                    cstatetmp = NEW | UNK;
                                    if coeff_dump_enabled() && blockno == 0 {
                                        eprintln!(
                                            "COEFF_DUMP band=0 bit={} thresh={} coeff[{}]={} (abs={}) -> NEW",
                                            bit, thres, i, src16[i], (src16[i] as i32).abs()
                                        );
                                    }
                                } else if coeff_dump_enabled() && blockno == 0 && bit == 1 && i < 4 {
                                    eprintln!(
                                        "COEFF_DUMP band=0 bit={} thresh={} coeff[{}]={} (abs={}) -> UNK",
                                        bit, thres, i, src16[i], (src16[i] as i32).abs()
                                    );
                                }
                            } else if (src16[i] as i32).abs() >= thres {
                                cstatetmp = NEW | UNK;
                                if coeff_dump_enabled() && blockno == 0 {
                                    eprintln!(
                                        "COEFF_DUMP band=0 bit={} thresh={} coeff[{}]={} (abs={}) -> NEW (no ep)",
                                        bit, thres, i, src16[i], (src16[i] as i32).abs()
                                    );
                                }
                            }
                        }
                        self.coeff_state[gidx] = cstatetmp;
                        bstate |= cstatetmp;
                    }
                } else {
                    bstate = UNK;
                }
            }

            self.bucket_state[bucket_base + bucket_idx] = bstate;
            bbstate |= bstate;
        }

        bbstate
    }

    /// Check if a slice is null (has no data to encode) based on quantization thresholds
    /// CRITICAL: For band 0, this also updates coeffstate[] array (matches djvulibre behavior)
    pub fn is_null_slice(&mut self, _bit: i32, band: i32) -> bool {
        if band == 0 {
            // For band 0, update coefficient state for ALL blocks' bucket 0 coefficients
            // This matches djvulibre IW44Image.cpp:is_null_slice exactly
            let mut is_null = true;
            for blockno in 0..self.map.num_blocks {
                let base_idx = blockno * 64 * 16;  // Start of this block's coefficients
                for i in 0..16 {
                    let threshold = self.quant_lo[i];
                    // Reset state to ZERO
                    self.coeff_state[base_idx + i] = ZERO;
                    if threshold > 0 && threshold < 0x8000 {
                        // Mark as UNK (unknown) if threshold is active
                        self.coeff_state[base_idx + i] = UNK;
                        is_null = false;
                    }
                }
            }
            is_null
        } else {
            // For other bands, just check the threshold (no state update needed)
            let threshold = self.quant_hi[band as usize];
            let is_null = !(threshold > 0 && threshold < 0x8000);
            is_null
        }
    }

    /// Finish processing a slice by reducing quantization thresholds (matches C44's finish_code_slice)
    /// Returns false if encoding should terminate (all thresholds became zero)
    pub fn finish_slice(&mut self, _cur_bit: i32, cur_band: i32) -> bool {
        // Reduce quantization threshold for current band
        // In lossless mode, keep thresholds at minimum of 1 (not 0)
        let min_threshold = if self.lossless { 1 } else { 0 };

        let old_hi = self.quant_hi[cur_band as usize];
        let new_hi = self.quant_hi[cur_band as usize] >> 1;
        self.quant_hi[cur_band as usize] = new_hi.max(min_threshold);
        
        if slice_stats_enabled() && cur_band == 0 {
            eprintln!(
                "THRESHOLD_DECAY band={} old_thresh={} new_thresh={}",
                cur_band, old_hi, self.quant_hi[cur_band as usize]
            );
        }

        if cur_band == 0 {
            for i in 0..16 {
                let old_lo = self.quant_lo[i];
                let new_lo = self.quant_lo[i] >> 1;
                self.quant_lo[i] = new_lo.max(min_threshold);
                if slice_stats_enabled() && i == 0 {
                    eprintln!(
                        "THRESHOLD_DECAY band=0 coeff[{}] old_thresh={} new_thresh={}",
                        i, old_lo, self.quant_lo[i]
                    );
                }
            }
        }

        // Lossless mode: continue until we've done multiple passes at threshold=1
        if self.lossless {
            // Check if all thresholds have reached the minimum (1)
            let all_at_min = self.quant_hi[1..].iter().all(|&t| t <= min_threshold)
                && self.quant_lo.iter().all(|&t| t <= min_threshold);

            // In lossless mode, after all thresholds reach 1, do additional passes
            // to ensure all coefficients with |value| >= 1 are encoded
            if all_at_min {
                // Continue for more slices to capture all coefficients
                // We'll rely on the slice limit or bytes limit to stop
                return true;
            }
            return true; // Keep encoding
        }

        // Lossy mode termination conditions
        // Check if all quantization thresholds are zero (C44 termination condition)
        let all_zero = self.quant_hi[1..].iter().all(|&t| t == 0) && self.quant_lo.iter().all(|&t| t == 0);
        if all_zero {
            return false; // Signal termination
        }

        // Original C++ condition: stop when we finish band 9 and its threshold is zero
        if cur_band == 9 && self.quant_hi[9] == 0 {
            return false; // Signal termination
        }

        true // Continue encoding
    }

    /// Encodes a sequence of buckets in a block using the ZEncoder.
    fn encode_buckets(
        &mut self,
        zp: &mut dyn ZpEncoderCursor,
        bit: i32,
        band: i32,
        blockno: usize,
        fbucket: usize,
        nbucket: usize,
    ) -> Result<(), super::EncoderError> {
        // Prepare the state for this block
        let bbstate = self.encode_prepare(band, fbucket, nbucket, blockno, bit);

        // Diagnostic logging for coefficient selection
        if std::env::var("IW44_COEFF_STATS").is_ok() && blockno == 0 {
            let mut new_count = 0;
            let mut active_count = 0;
            let mut unk_count = 0;
            for buckno in 0..nbucket {
                let bucket_idx = fbucket + buckno;
                let bucket_state = self.bucket_state[blockno * 64 + bucket_idx];
                if (bucket_state & NEW) != 0 { new_count += 1; }
                if (bucket_state & ACTIVE) != 0 { active_count += 1; }
                if (bucket_state & UNK) != 0 { unk_count += 1; }
            }
            eprintln!(
                "COEFF_STATS band={} bit={} block={} new={} active={} unk={} thresh={}",
                band, bit, blockno, new_count, active_count, unk_count,
                if band == 0 { self.quant_lo[0] } else { self.quant_hi[band as usize] }
            );
        }

        // Decouple NEW from ACTIVE to avoid wasting bits on empty buckets
        // when we only have ACTIVE coefficients to refine
        let has_active = (bbstate & ACTIVE) != 0;
        let has_new = (bbstate & NEW) != 0;
        let has_unk = (bbstate & UNK) != 0;

        // Determine if we should encode NEW-related passes (root, bucket, start)
        let mut encode_new_passes = has_new;

        // Root bit encoding logic (matches C++ IW44EncodeCodec.cpp lines 1309-1322):
        // - If nbucket < 16 OR bbstate & ACTIVE: Force NEW and skip root bit encoding
        // - Otherwise, if UNK is set, encode root bit to gate NEW passes
        // 
        // C++ logic:
        //   if ((nbucket<16) || (bbstate&ACTIVE)) { bbstate |= NEW; }
        //   else if (bbstate & UNK) { zp.encoder(...); }
        if nbucket < 16 || has_active {
            // Force NEW passes and skip root bit encoding (matches C++)
            encode_new_passes = true;
        } else if has_unk {
            // Encode root bit based on actual NEW state
            let root_bit = has_new;
            if bit_counts_enabled() {
                ROOT_BITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            zp_trace(
                "E",
                root_bit,
                "root",
                -1,
                self.ctx_root,
                bit,
                band,
                blockno,
                -1,
                -1,
            );
            zp.encode(root_bit, &mut self.ctx_root)
                .map_err(super::EncoderError::ZCodec)?;

            encode_new_passes = root_bit;
        }

        // Pass 1 and Pass 2 are only run if we have NEW coefficients to encode
        // Pass 3 (ACTIVE refinement) runs independently

        // --- Pass 1: Code bucket bits ---
        // For each bucket with potential new coefficients, encode whether it actually has any.
        // Only run this pass if we have NEW coefficients (gated by root bit or forced for small bands)
        if encode_new_passes {
            let bucket_offset = blockno * 64;
            for buckno in 0..nbucket {
                if (self.bucket_state[bucket_offset + fbucket + buckno] & UNK) != 0 {
                    let mut ctx = 0;
                    if band > 0 {
                        let k = (fbucket + buckno) << 2;
                        if let Some(b) = self.emap.blocks[blockno].get_bucket((k >> 4) as u8) {
                            let k = k & 0xf;
                            if b[k] != 0 {
                                ctx += 1;
                            }
                            if b[k + 1] != 0 {
                                ctx += 1;
                            }
                            if b[k + 2] != 0 {
                                ctx += 1;
                            }
                            if ctx < 3 && b[k + 3] != 0 {
                                ctx += 1;
                            }
                        }
                    }
                    if (bbstate & ACTIVE) != 0 {
                        ctx |= 4;
                    }
                    let bucket_bit =
                        (self.bucket_state[bucket_offset + fbucket + buckno] & NEW) != 0;
                    let ctx_val = self.ctx_bucket[band as usize][ctx];
                    if bit_counts_enabled() {
                        BUCKET_BITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    zp_trace(
                        "E",
                        bucket_bit,
                        "bucket",
                        ctx as i32,
                        ctx_val,
                        bit,
                        band,
                        blockno,
                        buckno as i32,
                        -1,
                    );
                    zp.encode(bucket_bit, &mut self.ctx_bucket[band as usize][ctx])?;
                }
            }
        }

        // --- Pass 2: Code new coefficients and their signs ---
        // For each coefficient identified as NEW, encode its existence and sign.
        // THIS IS WHERE THE MAGNITUDE IS FIRST RECORDED.
        // Only run this pass if we have NEW coefficients (gated by root bit or forced for small bands)
        if encode_new_passes {
            let coeff_offset = blockno * 64 * 16;
            let bucket_offset = blockno * 64;
            for buckno in 0..nbucket {
                if (self.bucket_state[bucket_offset + fbucket + buckno] & NEW) != 0 {
                    let pcoeff_bucket = self.map.blocks[blockno]
                        .get_bucket((fbucket + buckno) as u8)
                        .unwrap();
                    let epcoeff_bucket =
                        self.emap.blocks[blockno].get_bucket_mut((fbucket + buckno) as u8);

                    let mut gotcha = 0;
                    let maxgotcha = 7;
                    let coeff_idx_base = coeff_offset + (fbucket + buckno) * 16;

                    for i in 0..16 {
                        if (self.coeff_state[coeff_idx_base + i] & UNK) != 0 {
                            gotcha += 1;
                        }
                    }

                    for i in 0..16 {
                        if (self.coeff_state[coeff_idx_base + i] & UNK) != 0 {
                            let ctx = if gotcha >= maxgotcha {
                                maxgotcha
                            } else {
                                gotcha
                            } | if (self.bucket_state[bucket_offset + fbucket + buckno]
                                & ACTIVE)
                                != 0
                            {
                                8
                            } else {
                                0
                            };

                            let is_new = (self.coeff_state[coeff_idx_base + i] & NEW) != 0;
                            let ctx_val = self.ctx_start[ctx];
                            if bit_counts_enabled() {
                                START_BITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            zp_trace(
                                "E",
                                is_new,
                                "start",
                                ctx as i32,
                                ctx_val,
                                bit,
                                band,
                                blockno,
                                buckno as i32,
                                i as i32,
                            );
                            zp.encode(is_new, &mut self.ctx_start[ctx])?;

                            if is_new {
                                // 1. Encode the sign bit (this is a raw, non-adaptive bit)
                                let sign = pcoeff_bucket[i] < 0;
                                if bit_counts_enabled() {
                                    SIGN_BITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    COEFF_NEW_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                zp_trace(
                                    "IW",
                                    sign,
                                    "sign",
                                    -1,
                                    0,
                                    bit,
                                    band,
                                    blockno,
                                    buckno as i32,
                                    i as i32,
                                );
                                // Use encode_raw_bit for raw contexts (128, 129) instead of IWencoder
                                zp.iwencoder(sign)
                                    .map_err(super::EncoderError::ZCodec)?;

                                // 2. Set the initial reconstructed value in emap (magnitude with sign).
                                // Use the BASE threshold for initial reconstruction (not bit-plane shifted)
                                // C++ logic: `epcoeff[i] = thres + (thres>>1);` where thres is the BASE threshold
                                let thres = if band == 0 {
                                    self.quant_lo[i]
                                } else {
                                    self.quant_hi[band as usize]
                                };
                                let mag = (thres + (thres >> 1)) as i16;
                                // Store only magnitude in epcoeff (sign is tracked separately in bitstream)
                                epcoeff_bucket[i] = mag;

                                gotcha = 0;
                            } else if gotcha > 0 {
                                gotcha -= 1;
                            }
                        }
                    }
                }
            }
        }

        // --- Pass 3: Code mantissa bits for ACTIVE coefficient refinement ---
        // For coefficients that are already significant, refine their magnitude by one bit.
        // This pass runs independently of Pass 1/2 (can have ACTIVE without NEW)
        if has_active {
            let _base_thres = self.quant_hi[band as usize];
            let bucket_offset = blockno * 64;
            for buckno in 0..nbucket {
                if (self.bucket_state[bucket_offset + fbucket + buckno] & ACTIVE) != 0 {
                    let pcoeff_bucket = self.map.blocks[blockno]
                        .get_bucket((fbucket + buckno) as u8)
                        .unwrap();
                    let epcoeff_bucket =
                        self.emap.blocks[blockno].get_bucket_mut((fbucket + buckno) as u8);
                    for i in 0..16 {
                        let gidx = (blockno * 64 * 16) + (fbucket + buckno) * 16 + i;
                        if (self.coeff_state[gidx] & ACTIVE) != 0 {
                            // All operations here are on magnitudes. epcoeff stores magnitudes only.
                            let abs_pcoeff = (pcoeff_bucket[i] as i32).abs();
                            let ecoeff = epcoeff_bucket[i] as i32;

                            // Use the base threshold (no bitplane shift) like DjVuLibre
                            // C++ uses `thres = quant_lo[i]` for band 0 or `quant_hi[band]` otherwise
                            let thresh = if band == 0 {
                                self.quant_lo[i]
                            } else {
                                self.quant_hi[band as usize]
                            };

                            // The refinement bit (`pix`) is 1 if the true magnitude is in the upper half
                            // of the current uncertainty interval [ecoeff - thresh, ecoeff + thresh).
                            let pix = abs_pcoeff >= ecoeff;

                            // Encode the refinement bit adaptively or raw based on magnitude
                            if ecoeff <= 3 * thresh {
                                if bit_counts_enabled() {
                                    MANTISSA_ADAPTIVE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                zp_trace(
                                    "E",
                                    pix,
                                    "mant",
                                    -1,
                                    self.ctx_mant,
                                    bit,
                                    band,
                                    blockno,
                                    buckno as i32,
                                    i as i32,
                                );
                                zp.encode(pix, &mut self.ctx_mant)?;
                            } else {
                                if bit_counts_enabled() {
                                    MANTISSA_RAW.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                zp_trace(
                                    "IW",
                                    pix,
                                    "mant_raw",
                                    -1,
                                    0,
                                    bit,
                                    band,
                                    blockno,
                                    buckno as i32,
                                    i as i32,
                                );
                                // Use encode_raw_bit for raw contexts (128, 129) instead of IWencoder
                                zp.iwencoder(pix)
                                    .map_err(super::EncoderError::ZCodec)?;
                            }

                            // Update the reconstructed magnitude. epcoeff stores magnitude only.
                            // C++ logic: `epcoeff[i] = ecoeff - (pix ? 0 : thres) + (thres>>1);`
                            let adjustment = if pix { 0 } else { thresh };
                            epcoeff_bucket[i] = (ecoeff - adjustment + (thresh >> 1)) as i16;
                        }
                    }
                }
            }
        }

        // --- State Promotion: NEW -> ACTIVE ---
        // After encoding, any coefficient that was NEW is now considered ACTIVE for subsequent bit-planes.
        // Only promote if we actually encoded NEW coefficients (gated by encode_new_passes)
        if encode_new_passes {
            let coeff_base = blockno * 64 * 16 + fbucket * 16;
            let bucket_base = blockno * 64;
            for buck in 0..nbucket {
                if (self.bucket_state[bucket_base + fbucket + buck] & NEW) != 0 {
                    for i in 0..16 {
                        let gidx = coeff_base + buck * 16 + i;
                        if (self.coeff_state[gidx] & NEW) != 0 {
                            self.mark_signif(gidx);
                            self.coeff_state[gidx] = ACTIVE;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Mirrors DjVuLibre's Codec::code_slice: encode current slice and advance bit/band
    /// while decaying quantization thresholds. Returns false when encoding ends.
    /// Each codec owns its own curbit/curband state per djvulibre design.
    pub fn code_slice(
        &mut self,
        zp: &mut dyn ZpEncoderCursor,
    ) -> Result<bool, super::EncoderError> {
        if self.curbit < 0 {
            return Ok(false);
        }

        // Track slice count for diagnostics
        if slice_stats_enabled() {
            let slice_num = SLICE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            
            // Count NEW and ACTIVE coefficients before encoding
            let mut new_coeffs = 0;
            let mut active_coeffs = 0;
            for i in 0..self.coeff_state.len() {
                if (self.coeff_state[i] & NEW) != 0 {
                    new_coeffs += 1;
                }
                if (self.coeff_state[i] & ACTIVE) != 0 {
                    active_coeffs += 1;
                }
            }
            
            let thresh = if self.curband == 0 {
                self.quant_lo[0]
            } else {
                self.quant_hi[self.curband as usize]
            };
            
            eprintln!(
                "SLICE_STATS slice={} bit={} band={} thresh={} new_coeffs={} active_coeffs={}",
                slice_num, self.curbit, self.curband, thresh, new_coeffs, active_coeffs
            );
        }

        if !self.is_null_slice(self.curbit, self.curband) {
            let band_info = super::constants::BAND_BUCKETS[self.curband as usize];
            for blockno in 0..self.map.num_blocks {
                self.encode_buckets(
                    zp,
                    self.curbit,
                    self.curband,
                    blockno,
                    band_info.start,
                    band_info.size,
                )?;
            }
        }

        // Finish slice: decay thresholds and check termination
        if !self.finish_slice(self.curbit, self.curband) {
            self.curbit = -1;
            return Ok(false);
        }

        // Advance to next band/bit plane
        self.curband += 1;
        if self.curband >= super::constants::BAND_BUCKETS.len() as i32 {
            self.curband = 0;
            self.curbit += 1;
            let q9 = self.quant_hi[super::constants::BAND_BUCKETS.len() - 1];
            if q9 == 0 {
                self.curbit = -1;
                return Ok(false);
            }
        }

        Ok(self.curbit >= 0)
    }

    /// Estimates the quality of the encoded image in decibels.
    /// This matches DjVuLibre's estimate_decibel implementation.
    pub fn estimate_decibel(&self, db_frac: f32) -> f32 {
        let num_blocks = self.map.num_blocks;
        let mut xmse = vec![0.0f32; num_blocks];

        // Compute MSE for each block
        for blockno in 0..num_blocks {
            let mut mse = 0.0;
            let src = self.map.blocks[blockno].get_bucket(0);
            let ep = self.emap.blocks[blockno].get_bucket(0);

            if let (Some(src16), Some(ep16)) = (src, ep) {
                for i in 0..16 {
                    let diff = (src16[i] as i32 - ep16[i] as i32) as f32;
                    mse += diff * diff;
                }
            }
            xmse[blockno] = mse / 1024.0;
        }

        let p = (self.map.num_blocks as f32 * (1.0 - db_frac)).floor() as usize;
        let mut xmse_sorted = xmse.clone();
        xmse_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mse_avg = xmse_sorted[p..].iter().sum::<f32>() / (self.map.num_blocks - p) as f32;
        let factor = 255.0 * (1 << super::constants::IW_SHIFT) as f32;
        10.0 * (factor * factor / mse_avg).log10()
    }
}
