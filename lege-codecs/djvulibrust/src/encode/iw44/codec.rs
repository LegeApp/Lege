// src/encode/iw44/codec.rs

use super::coeff_map::CoeffMap;
use super::constants::BAND_BUCKETS;
use crate::encode::zc::{BitContext, ZpEncoderCursor};

// State flags for coefficients and buckets
const UNK: u8 = 0x01; // Unknown state
/// Coefficient state flags
const NEW: u8 = 0x02; // New coefficient to be encoded
const ACTIVE: u8 = 0x04; // Active coefficient (already encoded)
const ZERO: u8 = 0x00; // Zero state (coefficient not significant)

/// Widest band is 16 buckets (`BAND_BUCKETS[7..9]`), each 16 coefficients.
const MAX_BAND_BUCKETS: usize = 16;

/// Represents the IW44 codec for encoding wavelet coefficients.
/// Each codec instance owns its own slice state (curbit, curband) as per djvulibre design.
pub struct Codec {
    pub map: CoeffMap,                    // Original coefficient map
    pub emap: CoeffMap,                   // Encoded coefficient map
    pub coeff_state: Vec<u8>,             // Band-0 carry-over state (see `is_null_slice`)
    pub quant_hi: [i32; 10],              // Quantization thresholds for bands 1-9
    pub quant_lo: [i32; 16],              // Quantization thresholds for band 0
    pub ctx_root: BitContext,             // Context for root bit
    pub ctx_bucket: Vec<Vec<BitContext>>, // Contexts for bucket bits [band][ctx]
    pub ctx_start: Vec<BitContext>,       // Contexts for new coefficient activation [ctx]
    pub ctx_mant: BitContext,             // Context for mantissa bits
    /// Coefficient states for the band currently being coded, for one block.
    ///
    /// These were `num_blocks * 1024` and `num_blocks * 64` byte arrays, but
    /// nothing reads a block's state outside the single `encode_buckets` call
    /// that wrote it, so a page-sized encode was streaming ~2 MB through L2/L3
    /// on every band of every bit-plane for data that fits in 272 bytes of L1.
    scratch_coeff: [u8; MAX_BAND_BUCKETS * 16],
    scratch_bucket: [u8; MAX_BAND_BUCKETS],
    // Per-codec slice state (owned by each Y/Cb/Cr codec independently)
    pub curbit: i32,    // Current bitplane (starts at 1, goes to -1 when done)
    pub curband: i32,   // Current band (0-9)
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

        Codec {
            emap: CoeffMap::new(map.iw, map.ih), // Encoded map starts empty
            map,
            coeff_state: vec![ZERO; num_blocks * max_buckets * max_coeffs_per_bucket],
            quant_hi,
            quant_lo,
            ctx_root: 0u8,
            ctx_bucket,
            ctx_start,
            ctx_mant: 0u8,
            scratch_coeff: [ZERO; MAX_BAND_BUCKETS * 16],
            scratch_bucket: [ZERO; MAX_BAND_BUCKETS],
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

    /// This is the encode_slice implementation - temporarily removing slice activity optimization
    pub fn encode_slice<Z: ZpEncoderCursor>(
        &mut self,
        zp: &mut Z,
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
        _bit: i32,
    ) -> u8 {
        let coeff_base = blockno * 64 * 16;

        // Destructured so the coefficient maps can be read while the state
        // arrays are written. Indexing `self.map.blocks[blockno]` inside the
        // bucket loop instead costs a bounds check per bucket, and this
        // function runs once per block per slice -- millions of times for a
        // page-sized image.
        let Self {
            map,
            emap,
            coeff_state,
            scratch_coeff,
            scratch_bucket,
            quant_hi,
            quant_lo,
            ..
        } = self;
        let map_block = &map.blocks[blockno];
        let emap_block = &emap.blocks[blockno];

        // Buckets whose `present` bit is clear in *both* maps are all-zero in
        // both, so every one of their sixteen states is the same constant and
        // does not depend on the data at all. `present` is only ever cleared
        // together with a zeroing write (`zero_bucket`, `set_coeff_at_zigzag_index`)
        // and is set conservatively by every writer, so "bit clear" is a sound
        // proof of "all zeros".
        let empty_mask = !(map_block.present_mask() | emap_block.present_mask());

        // Band 0 carries state across slices (its `cstate` is seeded by
        // `is_null_slice` and read back here), so only bands 1..9 may skip the
        // per-coefficient work. `is_null_slice` guarantees `0 < thres < 0x8000`
        // for those bands, which pins the all-zero state to plain UNK.
        if band != 0 && nbucket > 0 {
            let thres = quant_hi[band as usize];
            let band_mask = if nbucket >= 64 {
                u64::MAX
            } else {
                (((1u64 << nbucket) - 1) << fbucket) & u64::MAX
            };
            if thres > 0 && (empty_mask & band_mask) == band_mask {
                // The whole band is empty in this block: every coefficient is
                // UNK, so there are no NEW and no ACTIVE coefficients. Passes 2
                // and 3 and the NEW->ACTIVE promotion are all gated on NEW or
                // ACTIVE bucket states, and pass 1 reads only `bucket_state`,
                // so nothing in this call ever reads `coeff_state` -- and for
                // bands 1..9 `coeff_state` is rewritten from scratch on every
                // entry to this function, so leaving it stale is invisible.
                scratch_bucket[..nbucket].fill(UNK);
                return UNK;
            }
        }

        let mut bbstate = 0;

        for buck in 0..nbucket {
            let bucket_idx = fbucket + buck;
            // get_bucket_raw returns the backing array directly (all-zero if never written),
            // which is semantically equivalent to the None branch for absent buckets.
            let src16 = map_block.get_bucket_raw(bucket_idx as u8);
            let ep16 = emap_block.get_bucket_raw(bucket_idx as u8);
            // One bounds check for the whole bucket rather than sixteen, and
            // a fixed-size slice the optimizer can vectorize over.
            let Some(cstate) = scratch_coeff
                .get_mut(buck * 16..buck * 16 + 16)
                .and_then(|slice| <&mut [u8; 16]>::try_from(slice).ok())
            else {
                continue;
            };
            let mut bstate = 0;

            if band != 0 {
                // Band other than zero: derive state from pcoeff/epcoeff like DjVuLibre
                let thres = quant_hi[band as usize];
                if thres > 0 && (empty_mask >> bucket_idx) & 1 != 0 {
                    // Both maps are all-zero here: `ep16[i] == 0` and
                    // `0 >= thres` is false, so every state is UNK.
                    cstate.fill(UNK);
                    scratch_bucket[buck] = UNK;
                    bbstate |= UNK;
                    continue;
                }
                if thres > 0 && thres < 0x8000 {
                    // Branchless form of the same three-way select, so the
                    // sixteen coefficients vectorize instead of compiling to
                    // sixteen unpredictable branch pairs. `is_null_slice`
                    // guarantees `0 < thres < 0x8000` for bands 1..9, which
                    // lets the magnitude test run in u16 (`unsigned_abs` maps
                    // i16::MIN to 32768 without overflowing) rather than
                    // widening every coefficient to i32.
                    let thres_u = thres as u16;
                    for i in 0..16 {
                        let sig = (src16[i].unsigned_abs() >= thres_u) as u8;
                        // sig -> NEW|UNK (0x03), !sig -> UNK (0x01)
                        let state = if ep16[i] != 0 { ACTIVE } else { UNK | (sig << 1) };
                        cstate[i] = state;
                        bstate |= state;
                    }
                } else {
                    for i in 0..16 {
                        let state = if ep16[i] != 0 {
                            ACTIVE
                        } else if (src16[i] as i32).abs() >= thres {
                            NEW | UNK
                        } else {
                            UNK
                        };
                        cstate[i] = state;
                        bstate |= state;
                    }
                }
            } else {
                // Band zero: preserve prior coeff_state ZERO/UNK behavior like DjVuLibre
                // CRITICAL: Must read existing cstate[i] value first (C++ does this)
                //
                // Band 0 is the one band whose incoming state is not derived
                // here: `is_null_slice` seeds it for every block before the
                // block loop starts, so that seed still lives in the
                // `coeff_state` array. Its own result is only consumed within
                // this slice, so it goes to the scratch copy like every other
                // band. Reading a stale array (rather than the seed) would be
                // a silent correctness bug, so this reads `coeff_state`
                // explicitly.
                let prev = &coeff_state[coeff_base + bucket_idx * 16..coeff_base + bucket_idx * 16 + 16];
                for i in 0..16 {
                    let thres = quant_lo[i];
                    let mut cstatetmp = prev[i];

                    debug_assert!(
                        cstatetmp == ZERO
                            || cstatetmp == UNK
                            || cstatetmp == ACTIVE
                            || cstatetmp == (NEW | UNK),
                        "Invalid coeff state: {} at gidx={}",
                        cstatetmp,
                        coeff_base + bucket_idx * 16 + i
                    );

                    if cstatetmp != ZERO {
                        cstatetmp = if ep16[i] != 0 {
                            ACTIVE
                        } else if (src16[i] as i32).abs() >= thres {
                            NEW | UNK
                        } else {
                            UNK
                        };
                    }
                    cstate[i] = cstatetmp;
                    bstate |= cstatetmp;
                }
            }

            scratch_bucket[buck] = bstate;
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
                let base_idx = blockno * 64 * 16; // Start of this block's coefficients
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
            !(threshold > 0 && threshold < 0x8000)
        }
    }

    /// Finish processing a slice by reducing quantization thresholds (matches C44's finish_code_slice)
    /// Returns false if encoding should terminate (all thresholds became zero)
    pub fn finish_slice(&mut self, _cur_bit: i32, cur_band: i32) -> bool {
        // IW44's wire format has no alternate lossless termination schedule:
        // DjVuLibre always lets these thresholds reach zero before ending the
        // final band.  Pinning them at one creates an invalid tail of slices.
        let new_hi = self.quant_hi[cur_band as usize] >> 1;
        self.quant_hi[cur_band as usize] = new_hi;

        if cur_band == 0 {
            for i in 0..16 {
                let new_lo = self.quant_lo[i] >> 1;
                self.quant_lo[i] = new_lo;
            }
        }

        // Check if all quantization thresholds are zero (IW44 termination condition)
        let all_zero =
            self.quant_hi[1..].iter().all(|&t| t == 0) && self.quant_lo.iter().all(|&t| t == 0);
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
    fn encode_buckets<Z: ZpEncoderCursor>(
        &mut self,
        zp: &mut Z,
        bit: i32,
        band: i32,
        blockno: usize,
        fbucket: usize,
        nbucket: usize,
    ) -> Result<(), super::EncoderError> {
        // Prepare the state for this block
        let bbstate = self.encode_prepare(band, fbucket, nbucket, blockno, bit);

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
            // Destructured so the emap block and the bucket states can be read
            // while a context is borrowed mutably for the encoder.
            let Self {
                emap,
                scratch_bucket,
                ctx_bucket,
                ..
            } = self;
            let emap_block = &emap.blocks[blockno];
            let ctx_band = &mut ctx_bucket[band as usize];
            let states = &scratch_bucket[..nbucket];
            for (buckno, &state) in states.iter().enumerate() {
                if (state & UNK) != 0 {
                    let mut ctx = 0;
                    if band > 0 {
                        let k = (fbucket + buckno) << 2;
                        let b = emap_block.get_bucket_raw((k >> 4) as u8);
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
                    if (bbstate & ACTIVE) != 0 {
                        ctx |= 4;
                    }
                    let bucket_bit = (state & NEW) != 0;
                    zp.encode(bucket_bit, &mut ctx_band[ctx])?;
                }
            }
        }

        // --- Pass 2: Code new coefficients and their signs ---
        // For each coefficient identified as NEW, encode its existence and sign.
        // THIS IS WHERE THE MAGNITUDE IS FIRST RECORDED.
        // Only run this pass if we have NEW coefficients (gated by root bit or forced for small bands)
        if encode_new_passes {
            let Self {
                map,
                emap,
                scratch_coeff,
                scratch_bucket,
                ctx_start,
                quant_hi,
                quant_lo,
                ..
            } = self;
            let map_block = &map.blocks[blockno];
            let emap_block = &mut emap.blocks[blockno];
            // Loop-invariant for every band but 0, where the threshold is
            // per-coefficient; hoisting it out of the coefficient loop drops a
            // branch and a load from the innermost body.
            let band_thres = if band == 0 { 0 } else { quant_hi[band as usize] };
            for buckno in 0..nbucket {
                let bucket_state_value = scratch_bucket[buckno];
                if (bucket_state_value & NEW) != 0 {
                    let pcoeff_bucket = map_block.get_bucket_raw((fbucket + buckno) as u8);
                    let epcoeff_bucket = emap_block.get_bucket_mut((fbucket + buckno) as u8);

                    let mut gotcha = 0;
                    let maxgotcha = 7;
                    // One bounds check for the bucket's sixteen states.
                    let cstate = &scratch_coeff[buckno * 16..buckno * 16 + 16];
                    let active_bit = if (bucket_state_value & ACTIVE) != 0 {
                        8
                    } else {
                        0
                    };

                    for &state in cstate {
                        if (state & UNK) != 0 {
                            gotcha += 1;
                        }
                    }

                    for i in 0..16 {
                        if (cstate[i] & UNK) != 0 {
                            let ctx = if gotcha >= maxgotcha {
                                maxgotcha
                            } else {
                                gotcha
                            } | active_bit;

                            let is_new = (cstate[i] & NEW) != 0;
                            zp.encode(is_new, &mut ctx_start[ctx])?;

                            if is_new {
                                // 1. Encode the sign bit (this is a raw, non-adaptive bit)
                                let sign = pcoeff_bucket[i] < 0;
                                // Use encode_raw_bit for raw contexts (128, 129) instead of IWencoder
                                zp.iwencoder(sign).map_err(super::EncoderError::ZCodec)?;

                                // 2. Set the initial reconstructed value in emap (magnitude with sign).
                                // Use the BASE threshold for initial reconstruction (not bit-plane shifted)
                                // C++ logic: `epcoeff[i] = thres + (thres>>1);` where thres is the BASE threshold
                                let thres = if band == 0 { quant_lo[i] } else { band_thres };
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
            let Self {
                map,
                emap,
                scratch_coeff,
                scratch_bucket,
                ctx_mant,
                quant_hi,
                quant_lo,
                ..
            } = self;
            let map_block = &map.blocks[blockno];
            let emap_block = &mut emap.blocks[blockno];
            // Same hoist as pass 2: for bands 1..9 both the threshold and the
            // `3 * thresh` adaptive/raw cutoff are constant across the whole
            // pass, and this is the encoder's single hottest loop.
            let band_thres = if band == 0 { 0 } else { quant_hi[band as usize] };
            let band_thres3 = band_thres.saturating_mul(3);
            for buckno in 0..nbucket {
                if (scratch_bucket[buckno] & ACTIVE) != 0 {
                    let pcoeff_bucket = map_block.get_bucket_raw((fbucket + buckno) as u8);
                    let epcoeff_bucket = emap_block.get_bucket_mut((fbucket + buckno) as u8);
                    let cstate = &scratch_coeff[buckno * 16..buckno * 16 + 16];
                    for i in 0..16 {
                        if (cstate[i] & ACTIVE) != 0 {
                            // All operations here are on magnitudes. epcoeff stores magnitudes only.
                            let abs_pcoeff = (pcoeff_bucket[i] as i32).abs();
                            let ecoeff = epcoeff_bucket[i] as i32;

                            // Use the base threshold (no bitplane shift) like DjVuLibre
                            // C++ uses `thres = quant_lo[i]` for band 0 or `quant_hi[band]` otherwise
                            let (thresh, thresh3) = if band == 0 {
                                let t = quant_lo[i];
                                (t, t.saturating_mul(3))
                            } else {
                                (band_thres, band_thres3)
                            };

                            // The refinement bit (`pix`) is 1 if the true magnitude is in the upper half
                            // of the current uncertainty interval [ecoeff - thresh, ecoeff + thresh).
                            let pix = abs_pcoeff >= ecoeff;

                            // Encode the refinement bit adaptively or raw based on magnitude
                            if ecoeff <= thresh3 {
                                zp.encode(pix, ctx_mant)?;
                            } else {
                                // Use encode_raw_bit for raw contexts (128, 129) instead of IWencoder
                                zp.iwencoder(pix).map_err(super::EncoderError::ZCodec)?;
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

        // The NEW -> ACTIVE promotion loop that used to stand here was dead.
        // It wrote two things and nothing else read either of them:
        //
        //   * `signif`, a one-bit-per-coefficient side table whose only reader
        //     was `has_data_for_slice`, a slice-skipping optimization that was
        //     never wired up (`encode_slice` says as much) -- and it was a
        //     scattered read-modify-write over a quarter-megabyte bitmap.
        //   * `coeff_state[gidx] = ACTIVE`, which is overwritten before it can
        //     be read: for bands 1..9 `encode_prepare` re-derives all sixteen
        //     states of every bucket it touches from `map`/`emap` on entry, and
        //     band 0's state is re-seeded by `is_null_slice` at the top of
        //     every band-0 slice. Bands never share buckets, so no other band
        //     can observe it either.
        //
        // The ACTIVE state a coefficient needs on the next bit-plane is carried
        // by `emap` (pass 2 writes the reconstructed magnitude there, and
        // `encode_prepare` maps `epcoeff != 0` back to ACTIVE), not by this
        // loop.

        Ok(())
    }

    /// Mirrors DjVuLibre's Codec::code_slice: encode current slice and advance bit/band
    /// while decaying quantization thresholds. Returns false when encoding ends.
    /// Each codec owns its own curbit/curband state per djvulibre design.
    pub fn code_slice<Z: ZpEncoderCursor>(
        &mut self,
        zp: &mut Z,
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
            let src16 = self.map.blocks[blockno].get_bucket_raw(0);
            let ep16 = self.emap.blocks[blockno].get_bucket_raw(0);
            let mut mse = 0.0f32;
            for i in 0..16 {
                let diff = (src16[i] as i32 - ep16[i] as i32) as f32;
                mse += diff * diff;
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
