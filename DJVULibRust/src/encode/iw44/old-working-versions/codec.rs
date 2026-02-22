// src/encode/iw44/codec.rs

use super::coeff_map::CoeffMap;
use super::constants::{BAND_BUCKETS, IW_NORM, IW_QUANT};
use super::zigzag::ZIGZAG_LOC;
use crate::encode::zc::{BitContext, ZpEncoderCursor};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

// State flags for coefficients and buckets
const UNK: u8 = 0x01; // Unknown state
/// Coefficient state flags
const NEW: u8 = 0x02; // New coefficient to be encoded
const ACTIVE: u8 = 0x04; // Active coefficient (already encoded)
const ZERO: u8 = 0x00; // Zero state (coefficient not significant)

/// Context number used by the DjVu reference for "raw" (non-adaptive) bits
const RAW_CONTEXT_ID: BitContext = 129;
const RAW_CONTEXT_129: BitContext = 129;

/// 1 bit / coefficient (32 Ã— smaller than `Vec<bool>`)
const WORD_BITS: usize = 32;

static ZPTRACE_COUNT: AtomicUsize = AtomicUsize::new(0);

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
}

impl Codec {
    /// Creates a new Codec instance for the given coefficient map and parameters.
    pub fn new(map: CoeffMap, _params: &super::EncoderParams) -> Self {
        let num_blocks = map.num_blocks;
        let max_buckets = 64; // Each block has up to 64 buckets
        let max_coeffs_per_bucket = 16;

        // Initialize quantization thresholds exactly like djvulibre IW44Image.cpp constructor
        let iw_quant = &super::constants::IW_QUANT;
        eprintln!("DEBUG: iw_quant = [{:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}]",
            iw_quant[0], iw_quant[1], iw_quant[2], iw_quant[3], iw_quant[4], iw_quant[5], iw_quant[6], iw_quant[7]);
        let mut quant_lo = [0i32; 16];
        let mut quant_hi = [0i32; 10];

        // Fill quant_lo[0..15] from iw_quant following djvulibre logic EXACTLY
        let mut i = 0;
        let mut q_idx = 0;

        // -- lo coefficients (exact match to C++ logic)
        eprintln!("DEBUG: Starting quant_lo initialization");
        // First loop: for (j=0; i<4; j++) quant_lo[i++] = *q++;
        for _j in 0..4 {
            if i < 4 && q_idx < iw_quant.len() {
                quant_lo[i] = iw_quant[q_idx];
                eprintln!("DEBUG: Loop1: i={}, q_idx={}, quant_lo[{}]={:#x}", i, q_idx, i, quant_lo[i]);
                i += 1;
                q_idx += 1;
            }
        }
        // Second loop: for (j=0; j<4; j++) quant_lo[i++] = *q; (q does NOT advance)
        for _j in 0..4 {
            if i < 8 && q_idx < iw_quant.len() {
                quant_lo[i] = iw_quant[q_idx];
                eprintln!("DEBUG: Loop2: i={}, q_idx={}, quant_lo[{}]={:#x}", i, q_idx, i, quant_lo[i]);
                i += 1;
            }
        }
        q_idx += 1;
        eprintln!("DEBUG: After q_idx+=1, q_idx={}", q_idx);
        // Third loop: for (j=0; j<4; j++) quant_lo[i++] = *q;
        for _j in 0..4 {
            if i < 12 && q_idx < iw_quant.len() {
                quant_lo[i] = iw_quant[q_idx];
                eprintln!("DEBUG: Loop3: i={}, q_idx={}, quant_lo[{}]={:#x}", i, q_idx, i, quant_lo[i]);
                i += 1;
            }
        }
        q_idx += 1;
        eprintln!("DEBUG: After second q_idx+=1, q_idx={}", q_idx);
        // Fourth loop: for (j=0; j<4; j++) quant_lo[i++] = *q;
        for _j in 0..4 {
            if i < 16 && q_idx < iw_quant.len() {
                quant_lo[i] = iw_quant[q_idx];
                eprintln!("DEBUG: Loop4: i={}, q_idx={}, quant_lo[{}]={:#x}", i, q_idx, i, quant_lo[i]);
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

        // Do NOT scale thresholds - use IW_QUANT values as-is, matching C++ behavior

        // Initialize contexts
        let mut ctx_bucket = Vec::with_capacity(10);
        for _ in 0..10 {
            ctx_bucket.push(vec![0u8; 8]); // 8 contexts per band (0-7)
        }
        let ctx_start = vec![0u8; 16]; // 16 contexts (0-15)

        let coeffs = num_blocks * max_buckets * max_coeffs_per_bucket;

        eprintln!("CODEC_NEW: Initial quant_lo[0..4] = [{:#x}, {:#x}, {:#x}, {:#x}]",
                  quant_lo[0], quant_lo[1], quant_lo[2], quant_lo[3]);
        eprintln!("CODEC_NEW: Initial quant_hi[0..4] = [{:#x}, {:#x}, {:#x}, {:#x}]",
                  quant_hi[0], quant_hi[1], quant_hi[2], quant_hi[3]);

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
        }
    }

    /// Returns a reference to the coefficient map.
    pub fn map(&self) -> &CoeffMap {
        &self.map
    }

    fn debug_log(&self, msg: &str) {
        // Write to debug file instead of console
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("codec_debug.log")
        {
            let _ = writeln!(file, "CODEC_DEBUG: {}", msg);
        }
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
    pub fn has_data_for_slice(&self, bit: i32, band: i32) -> bool {
        // First, quick check if there are any active coefficients in this band
        let band = band as usize;
        let thresh_hi = self.quant_hi[band];
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
                            thresh_hi
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
        let th_hi = self.quant_hi[band as usize];
        let coeff_base = blockno * 64 * 16;
        let bucket_base = blockno * 64;

        eprintln!(
            "ENCODE_PREPARE: band={}, bit={}, blockno={}, fbucket={}, nbucket={}, th_hi={:#x}",
            band, bit, blockno, fbucket, nbucket, th_hi
        );

        let mut bbstate = 0;

        for buck in 0..nbucket {
            let bucket_idx = fbucket + buck;
            let coeff_idx0 = coeff_base + bucket_idx * 16;
            let src = self.map.blocks[blockno].get_bucket(bucket_idx as u8);
            let ep = self.emap.blocks[blockno].get_bucket(bucket_idx as u8);
            let mut bstate = 0;

            if band != 0 {
                // Band other than zero: derive state from pcoeff/epcoeff like DjVuLibre
                if let Some(src16) = src {
                    if let Some(ep16) = ep {
                        for i in 0..16 {
                            let gidx = coeff_idx0 + i;
                            let mut cstate = UNK;
                            if ep16[i] != 0 {
                                cstate = ACTIVE;
                            } else if (src16[i] as i32).abs() >= th_hi {
                                cstate = NEW | UNK;
                            }
                            if cstate != UNK {
                                eprintln!(
                                    "  COEFF[{},{}]: coeff={:#x}, step={:#x}, state={}",
                                    bucket_idx,
                                    i,
                                    src16[i] as i32,
                                    th_hi,
                                    if cstate == ACTIVE { "ACTIVE" } else { "NEW" }
                                );
                            }
                            self.coeff_state[gidx] = cstate;
                            bstate |= cstate;
                        }
                    } else {
                        for i in 0..16 {
                            let gidx = coeff_idx0 + i;
                            let mut cstate = UNK;
                            if (src16[i] as i32).abs() >= th_hi {
                                cstate = NEW | UNK;
                            }
                            if cstate != UNK {
                                eprintln!(
                                    "  COEFF[{},{}]: coeff={:#x}, step={:#x}, state=NEW",
                                    bucket_idx, i, src16[i] as i32, th_hi
                                );
                            }
                            self.coeff_state[gidx] = cstate;
                            bstate |= cstate;
                        }
                    }
                } else {
                    // No coefficients allocated
                    bstate = UNK;
                }
            } else {
                // Band zero: use prior coeff_state ZERO/UNK behavior like DjVuLibre
                if let Some(src16) = src {
                    for i in 0..16 {
                        let gidx = coeff_idx0 + i;
                        let thres = self.quant_lo[i];
                        let mut cstatetmp = if thres > 0 && thres < 0x8000 {
                            UNK
                        } else {
                            ZERO
                        };
                        if let Some(ep16) = ep {
                            if ep16[i] != 0 {
                                cstatetmp = ACTIVE;
                            } else if (src16[i] as i32).abs() >= thres {
                                cstatetmp = NEW | UNK;
                            }
                        } else if (src16[i] as i32).abs() >= thres {
                            cstatetmp = NEW | UNK;
                        }
                        
                                                if bit <= 3 && blockno == 0 && bucket_idx == 0 {
                            eprintln!(
                                "  PREPARE bit={} block={} bucket={} coeff={}: value={} ({:#x}), thres={} ({:#x}), state={}",
                                bit, blockno, bucket_idx, i,
                                src16[i] as i32, src16[i] as i32,
                                thres, thres,
                                match cstatetmp {
                                    ZERO => "ZERO",
                                    UNK => "UNK",
                                    NEW | UNK => "NEW|UNK",
                                    ACTIVE => "ACTIVE",
                                    _ => "OTHER"
                                }
                            );
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
    pub fn is_null_slice(&mut self, bit: i32, band: i32) -> bool {
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
                        if bit >= 7 && blockno == 0 {
                            eprintln!(
                                "NULL_DEBUG: bit={}, band={}, coeff[{}] threshold={:#x}, set to UNK",
                                bit, band, i, threshold
                            );
                        }
                    }
                }
            }
            if bit >= 7 {
                eprintln!(
                    "NULL_DEBUG: bit={}, band={}, slice is_null={}",
                    bit, band, is_null
                );
            }
            is_null
        } else {
            // For other bands, just check the threshold (no state update needed)
            let threshold = self.quant_hi[band as usize];
            let is_null = !(threshold > 0 && threshold < 0x8000);
            if bit >= 7 {
                eprintln!(
                    "NULL_DEBUG: bit={}, band={}, threshold={:#x}, is_null={}",
                    bit, band, threshold, is_null
                );
            }
            is_null
        }
    }

    /// Finish processing a slice by reducing quantization thresholds (matches C44's finish_code_slice)
    /// Returns false if encoding should terminate (all thresholds became zero)
    pub fn finish_slice(&mut self, cur_bit: i32, cur_band: i32) -> bool {
        // Reduce quantization threshold for current band
        self.quant_hi[cur_band as usize] >>= 1;
        if cur_band == 0 {
            for i in 0..16 {
                self.quant_lo[i] >>= 1;
            }
        }

        // Check if all quantization thresholds are zero (C44 termination condition)
        // This happens when we finish processing band 9 and the highest threshold becomes 0
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

        // Match C++ logic for root bit encoding:
        // If nbucket < 16 or bbstate has ACTIVE, force NEW flag and skip root bit encoding
        // Otherwise, encode root bit only if UNK is set
        let mut bbstate = bbstate;
        if nbucket < 16 || (bbstate & ACTIVE) != 0 {
            bbstate |= NEW;
            eprintln!("ENCODE_BUCKETS: blockno={}, band={}, bit={}, nbucket={}, bbstate={:#x}, forced NEW (nbucket<16 or ACTIVE)", 
                     blockno, band, bit, nbucket, bbstate);
        } else if (bbstate & UNK) != 0 {
            let root_bit = (bbstate & NEW) != 0;
            eprintln!("ENCODE_BUCKETS: blockno={}, band={}, bit={}, nbucket={}, bbstate={:#x}, root_bit={}", 
                     blockno, band, bit, nbucket, bbstate, root_bit);
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
        }

        // If no NEW coefficients after the above logic, nothing more to encode
        if (bbstate & NEW) == 0 {
            eprintln!(
                "ENCODE_BUCKETS: blockno={}, band={}, bit={}, no NEW coefficients, returning",
                blockno, band, bit
            );
            return Ok(());
        }

        // --- Pass 1: Code bucket bits ---
        // For each bucket with potential new coefficients, encode whether it actually has any.
        if (bbstate & NEW) != 0 {
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
                    eprintln!(
                        "  BUCKET_BIT[{}]: buckno={}, ctx={}, bit={}",
                        blockno, buckno, ctx, bucket_bit
                    );
                    let ctx_val = self.ctx_bucket[band as usize][ctx];
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
        if (bbstate & NEW) != 0 {
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
                            eprintln!(
                                "  COEFF_BIT[{},{}]: ctx={}, bit={}",
                                blockno, i, ctx, is_new
                            );
                            let ctx_val = self.ctx_start[ctx];
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
                                zp.IWencoder(sign)?;

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
        if (bbstate & ACTIVE) != 0 {
            let base_thres = self.quant_hi[band as usize];
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
                                base_thres
                            };

                            // The refinement bit (`pix`) is 1 if the true magnitude is in the upper half
                            // of the current uncertainty interval [ecoeff - thresh, ecoeff + thresh).
                            let pix = abs_pcoeff >= ecoeff;

                            // Encode the refinement bit adaptively or raw based on magnitude
                            if ecoeff <= 3 * thresh {
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
                                zp.IWencoder(pix)?;
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
        if (bbstate & NEW) != 0 {
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

    /// Estimates the encoding error in decibels for quality control.
    pub fn estimate_decibel(&self, frac: f32) -> f32 {
        let mut xmse = vec![0.0; self.map.num_blocks];
        let norm_lo = &IW_NORM[0..16];
        let norm_hi = &[
            0.0,
            IW_NORM[3],
            IW_NORM[6],
            IW_NORM[9],
            IW_NORM[10],
            IW_NORM[11],
            IW_NORM[12],
            IW_NORM[13],
            IW_NORM[14],
            IW_NORM[15],
        ];

        for blockno in 0..self.map.num_blocks {
            let mut mse = 0.0;
            for bandno in 0..10 {
                let fbucket = BAND_BUCKETS[bandno].start;
                let nbucket = BAND_BUCKETS[bandno].size;
                let norm = norm_hi[bandno];
                for buckno in 0..nbucket {
                    if let (Some(pcoeff), Some(epcoeff)) = (
                        self.map.blocks[blockno].get_bucket((fbucket + buckno) as u8),
                        self.emap.blocks[blockno].get_bucket((fbucket + buckno) as u8),
                    ) {
                        for i in 0..16 {
                            let norm_coeff = if bandno == 0 { norm_lo[i] } else { norm };
                            // Compare magnitudes (epcoeff stores unsigned magnitude, pcoeff is signed)
                            let pcoeff_mag = (pcoeff[i] as f32).abs();
                            let epcoeff_mag = epcoeff[i] as f32;
                            let delta = pcoeff_mag - epcoeff_mag;
                            mse += norm_coeff * delta * delta;
                        }
                    } else if let Some(pcoeff) =
                        self.map.blocks[blockno].get_bucket((fbucket + buckno) as u8)
                    {
                        for i in 0..16 {
                            let norm_coeff = if bandno == 0 { norm_lo[i] } else { norm };
                            // No epcoeff, so delta is the full magnitude of pcoeff
                            let delta = (pcoeff[i] as f32).abs();
                            mse += norm_coeff * delta * delta;
                        }
                    }
                }
            }
            xmse[blockno] = mse / 1024.0;
        }

        let p = (self.map.num_blocks as f32 * (1.0 - frac)).floor() as usize;
        let mut xmse_sorted = xmse.clone();
        xmse_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mse_avg = xmse_sorted[p..].iter().sum::<f32>() / (self.map.num_blocks - p) as f32;
        let factor = 255.0 * (1 << super::constants::IW_SHIFT) as f32;
        10.0 * (factor * factor / mse_avg).log10()
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

        // finish_code_slice: halve thresholds, advance band, increment bit on wrap, stop when quant_hi[9]==0
        self.quant_hi[self.curband as usize] >>= 1;
        if self.curband == 0 {
            for q in &mut self.quant_lo {
                *q >>= 1;
            }
        }

        // Slice boundary trace - compare with C++ output (after decay, before advancement)
        if let Ok(v) = std::env::var("IW44_SLICE_TRACE") {
            let v = v.trim();
            if !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false")) {
                eprint!("SLICE_TRACE rust slice={} bit={} band={} quant_hi=[",
                    std::env::var("IW44_SLICE_NUM").unwrap_or_default(),
                    self.curbit, self.curband);
                for i in 0..10 {
                    eprint!("{:#x}{}", self.quant_hi[i], if i == 9 { "" } else { ", " });
                }
                eprint!("] quant_lo=[");
                for i in 0..16 {
                    eprint!("{:#x}{}", self.quant_lo[i], if i == 15 { "" } else { ", " });
                }
                eprintln!("]");
            }
        }

        self.curband += 1;
        if self.curband >= super::constants::BAND_BUCKETS.len() as i32 {
            self.curband = 0;
            self.curbit += 1;
            let q9 = self.quant_hi[super::constants::BAND_BUCKETS.len() - 1];
            eprintln!(
                "CODE_SLICE: advanced to bit={}, quant_hi[9]={:#x}",
                self.curbit, q9
            );
            if q9 == 0 {
                eprintln!("CODE_SLICE: terminating (quant_hi[9]==0)");
                self.curbit = -1;
                return Ok(false);
            }
        }

        Ok(self.curbit >= 0)
    }
}
