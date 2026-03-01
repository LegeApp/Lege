// src/encode/iw44/codec.rs

use super::{coeff_map::CoeffMap, constants::{BAND_BUCKETS, IW_NORM}};
use crate::encode::zc::{ZEncoder, BitContext};
use std::f32;
use std::io::Write;


// State flags for coefficients and buckets
const UNK: u8 = 0x01;    // Unknown state
/// Coefficient state flags
const NEW: u8 = 0x02;    // New coefficient to be encoded
const ACTIVE: u8 = 0x04; // Active coefficient (already encoded)
const ZERO: u8 = 0x00;   // Zero state (coefficient not significant)

/// Context number used by the DjVu reference for "raw" (non-adaptive) bits
const RAW_CONTEXT_ID: BitContext = 129;
const RAW_CONTEXT_129: BitContext = 129;

/// 1 bit / coefficient (32 × smaller than `Vec<bool>`)
const WORD_BITS: usize = 32;

#[inline]
fn words_for_coeffs(n: usize) -> usize { (n + WORD_BITS - 1) / WORD_BITS }

/// Represents the IW44 codec for encoding wavelet coefficients.
/// Note: State management (cur_bit, cur_band) has been moved to IWEncoder for synchronization.
pub struct Codec {
    pub map: CoeffMap,           // Original coefficient map
    pub emap: CoeffMap,          // Encoded coefficient map
    pub coeff_state: Vec<u8>,    // State of each coefficient
    pub bucket_state: Vec<u8>,   // State of each bucket
    pub quant_hi: [i32; 10],     // Quantization thresholds for bands 1-9
    pub quant_lo: [i32; 16],     // Quantization thresholds for band 0
    pub ctx_root: BitContext,     // Context for root bit
    pub ctx_bucket: Vec<Vec<BitContext>>, // Contexts for bucket bits [band][ctx]
    pub ctx_start: Vec<BitContext>, // Contexts for new coefficient activation [ctx]
    pub ctx_mant: BitContext,      // Context for mantissa bits
    pub signif: Vec<u32>,        // 1 bit / coefficient (1 == coefficient is already significant)
}

impl Codec {
    /// Creates a new Codec instance for the given coefficient map and parameters.
    pub fn new(map: CoeffMap, _params: &super::EncoderParams) -> Self {
        let num_blocks = map.num_blocks;
        let max_buckets = 64; // Each block has up to 64 buckets
        let max_coeffs_per_bucket = 16;

        // Initialize quantization thresholds based on IW_QUANT values
        // For bands 1-9, use the corresponding IW_QUANT values
        let mut quant_hi = [0i32; 10];
        quant_hi[0] = 0x8000; // Band 0 uses individual quant_lo values
        for i in 1..10 {
            if i < super::constants::IW_QUANT.len() {
                quant_hi[i] = super::constants::IW_QUANT[i];
            } else {
                quant_hi[i] = 0x8000; // fallback
            }
        }
        let quant_lo = super::constants::IW_QUANT; // From constants.rs

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
            .open("codec_debug.log") {
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
    pub fn scan_for_activity(&self, bit: i32, band: i32) -> bool {
        if bit < 0 {
            return false;
        }

        let band = band as usize;
        let thresh_hi = (self.quant_hi[band] >> bit).max(1);
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
                            (self.quant_lo[i] >> bit).max(1)
                        } else {
                            thresh_hi
                        };
                        if (coeffs[i] as i32).abs() >= (step + 1) / 2 {
                            return true; // Found a new significant coefficient
                        }
                    }
                }
            }
        }

        false // Scanned everything, the slice is truly null
    }

    /// This is the encode_slice implementation - temporarily removing slice activity optimization
    pub fn encode_slice<W: Write>(
        &mut self,
        zp: &mut ZEncoder<W>,
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
    pub fn encode_prepare(&mut self, band: i32, fbucket: usize, nbucket: usize, blockno: usize, bit: i32) -> u8 {
        let th_hi = (self.quant_hi[band as usize] >> bit).max(1);
        let coeff_base = blockno * 64 * 16;
        let bucket_base = blockno * 64;

        let mut bbstate = 0;

        for buck in 0..nbucket {
            let bucket_idx = fbucket + buck;
            let coeff_idx0 = coeff_base + bucket_idx * 16;
            let src = self.map.blocks[blockno].get_bucket(bucket_idx as u8);
            let mut bstate = 0;

            if let Some(src16) = src {
                let thres = if band == 0 {
                    // each position has its own quantiser
                    None
                } else {
                    Some(th_hi)
                };

                for i in 0..16 {
                    let gidx = coeff_idx0 + i;
                    let already = self.is_signif(gidx);
                    // threshold depends on band 0 vs >0
                    let step = thres.unwrap_or_else(|| {
                        (self.quant_lo[i] >> bit).max(1)
                    });

                    let state = if already {
                        ACTIVE
                    } else if (src16[i] as i32).abs() >= (step + 1) / 2 {
                        // Note: Multiplying by 2 makes the check more robust against
                        // minor noise from the wavelet transform for near-zero coefficients.
                        // This helps prevent encoding insignificant AC coefficients in solid color images.
                        NEW | UNK
                    } else {
                        UNK
                    };

                    self.coeff_state[gidx] = state;
                    bstate |= state;
                }
            } else {
                // zero bucket, nothing significant yet
                bstate = UNK;
                for i in 0..16 {
                    self.coeff_state[coeff_idx0 + i] = UNK;
                }
            }

            self.bucket_state[bucket_base + bucket_idx] = bstate;
            bbstate |= bstate;
        }

        bbstate
    }

    /// Encodes a sequence of buckets in a block using the ZEncoder.
    fn encode_buckets<W: Write>(&mut self, zp: &mut ZEncoder<W>, bit: i32, band: i32, blockno: usize, fbucket: usize, nbucket: usize) -> Result<(), super::EncoderError> {
        // Prepare the state for this block
        let bbstate = self.encode_prepare(band, fbucket, nbucket, blockno, bit);

        // If the block is completely empty for this slice (no new OR active coefficients),
        // encode a single 'false' bit and finish with this block. This is the "root bit".
        if (bbstate & (NEW | ACTIVE)) == 0 {
            // Only encode the root bit if we have unknown coefficients.
            // If the block is all zeros and will remain so, bbstate will be just UNK.
            if (bbstate & UNK) != 0 {
                zp.encode(false, &mut self.ctx_root).map_err(super::EncoderError::ZCodec)?;
            }
            return Ok(());
        }

        // --- THIS IS THE CORRECTED LOGIC ---
        // We must encode the root bit if there's any uncertainty or activity.
        // The bit's value is determined *only* by the presence of NEW coefficients.
        zp.encode((bbstate & NEW) != 0, &mut self.ctx_root).map_err(super::EncoderError::ZCodec)?;

        // Code bucket bits
        if (bbstate & NEW) != 0 {
            let bucket_offset = blockno * 64;
            for buckno in 0..nbucket {
                if (self.bucket_state[bucket_offset + fbucket + buckno] & UNK) != 0 {
                    let mut ctx = 0;
                    if band > 0 {
                        let k = (fbucket + buckno) << 2;
                        if let Some(b) = self.emap.blocks[blockno].get_bucket((k >> 4) as u8) {
                            let k = k & 0xf;
                            if b[k] != 0 { ctx += 1; }
                            if b[k + 1] != 0 { ctx += 1; }
                            if b[k + 2] != 0 { ctx += 1; }
                            if ctx < 3 && b[k + 3] != 0 { ctx += 1; }
                        }
                    }
                    if (bbstate & ACTIVE) != 0 {
                        ctx |= 4;
                    }
                    zp.encode(
                        (self.bucket_state[bucket_offset + fbucket + buckno] & NEW) != 0,
                        &mut self.ctx_bucket[band as usize][ctx],
                    ).map_err(|e| super::EncoderError::ZCodec(e))?;
                }
            }
        }

        // Code new active coefficients with their signs
        if (bbstate & NEW) != 0 {
            let thres = self.quant_hi[band as usize];
            let coeff_offset = blockno * 64 * 16;
            let bucket_offset = blockno * 64;
            for buckno in 0..nbucket {
                if (self.bucket_state[bucket_offset + fbucket + buckno] & NEW) != 0 {
                    let pcoeff = self.map.blocks[blockno].get_bucket((fbucket + buckno) as u8).unwrap();
                    let epcoeff = self.emap.blocks[blockno].get_bucket_mut((fbucket + buckno) as u8);
                    let mut gotcha = 0;
                    let maxgotcha = 7;
                    let coeff_idx = coeff_offset + (fbucket + buckno) * 16;
                    for i in 0..16 {
                        if (self.coeff_state[coeff_idx + i] & UNK) != 0 {
                            gotcha += 1;
                        }
                    }
                    for i in 0..16 {
                        if (self.coeff_state[coeff_idx + i] & UNK) != 0 {
                            let ctx = if gotcha >= maxgotcha { maxgotcha } else { gotcha } |
                                      if (self.bucket_state[bucket_offset + fbucket + buckno] & ACTIVE) != 0 { 8 } else { 0 };
                            let is_new = (self.coeff_state[coeff_idx + i] & NEW) != 0;
                            zp.encode(is_new, &mut self.ctx_start[ctx]).map_err(|e| super::EncoderError::ZCodec(e))?;
                            if is_new {
                                let sign = pcoeff[i] < 0;
                                // Use IWencoder for sign bits (raw bits)
                                zp.IWencoder(sign).map_err(|e| super::EncoderError::ZCodec(e))?;
                                let thres_local = if band == 0 { self.quant_lo[i] } else { thres };
                                // The threshold for the current bit-plane
                                let plane_thres = thres_local >> bit;
                                // Reconstruct coefficient value (matching C++ logic)
                                let sign_bit = pcoeff[i] < 0;
                                epcoeff[i] = if sign_bit {
                                    (-((plane_thres * 3 + 2) >> 1)) as i16
                                } else {
                                    ((plane_thres * 3 + 2) >> 1) as i16
                                };
                                gotcha = 0;
                            } else if gotcha > 0 {
                                gotcha -= 1;
                            }
                        }
                    }
                }
            }
        }

        // Code mantissa bits
        if (bbstate & ACTIVE) != 0 {
            let base_thres = self.quant_hi[band as usize];
            let bucket_offset = blockno * 64;
            for buckno in 0..nbucket {
                if (self.bucket_state[bucket_offset + fbucket + buckno] & ACTIVE) != 0 {
                    let pcoeff = self.map.blocks[blockno]
                        .get_bucket((fbucket + buckno) as u8).unwrap();
                    let epcoeff = self.emap.blocks[blockno]
                        .get_bucket_mut((fbucket + buckno) as u8);
                    for i in 0..16 {
                        let gidx = (blockno * 64 * 16) + (fbucket + buckno) * 16 + i;
                        if (self.coeff_state[gidx] & ACTIVE) != 0 {
                            let coeff = pcoeff[i].abs() as i32;
                            let ecoeff = epcoeff[i] as i32;
                            // threshold per band or per position for band 0
                            let thres_local = if band == 0 { self.quant_lo[i] } else { base_thres };
                            // Get absolute values for comparison
                            let abs_coeff = coeff.abs();
                            let abs_ecoeff = ecoeff.abs();
                            
                            // Compute mantissa bit
                            let pix = abs_coeff >= abs_ecoeff;
                            
                            // Choose encoder based on estimated coefficient magnitude
                            if abs_ecoeff <= 3 * thres_local {
                                // Low magnitude - use adaptive encoding
                                zp.encode(pix, &mut self.ctx_mant).map_err(|e| super::EncoderError::ZCodec(e))?;
                            } else {
                                // High magnitude - use raw encoding
                                zp.IWencoder(pix).map_err(|e| super::EncoderError::ZCodec(e))?;
                            }
                            
                            // Adjust epcoeff (matching C++ logic exactly)
                            // epcoeff[i] = ecoeff - (pix ? 0 : thres) + (thres >> 1);
                            let adjustment = if pix { 0 } else { thres_local };
                            epcoeff[i] = (ecoeff - adjustment + (thres_local >> 1)) as i16;
                        }
                    }
                }
            }
        }
        
        // --- NEW ➜ ACTIVE promotion (one pass, no branches in hot loop) ---
        if (bbstate & NEW) != 0 {
            let coeff_base = blockno * 64 * 16 + fbucket * 16;
            let bucket_base = blockno * 64;
            for buck in 0..nbucket {
                if (self.bucket_state[bucket_base + fbucket + buck] & NEW) != 0 {
                    for i in 0..16 {
                        let gidx = coeff_base + buck * 16 + i;
                        if (self.coeff_state[gidx] & NEW) != 0 {
                            self.mark_signif(gidx);          // persist
                            self.coeff_state[gidx] = ACTIVE;  // next slice → refinement only
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
        let norm_hi = &[0.0, IW_NORM[3], IW_NORM[6], IW_NORM[9], IW_NORM[10], IW_NORM[11], IW_NORM[12], IW_NORM[13], IW_NORM[14], IW_NORM[15]];

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
                            let delta = (pcoeff[i] as f32 - epcoeff[i] as f32).abs();
                            mse += norm_coeff * delta * delta;
                        }
                    } else if let Some(pcoeff) = self.map.blocks[blockno].get_bucket((fbucket + buckno) as u8) {
                        for i in 0..16 {
                            let norm_coeff = if bandno == 0 { norm_lo[i] } else { norm };
                            let delta = pcoeff[i] as f32;
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
}