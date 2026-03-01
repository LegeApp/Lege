// src/encode/iw44/encoder.rs

use super::codec::Codec;
use super::coeff_map::CoeffMap;
use crate::encode::zc::ZEncoder;
use ::image::{GrayImage, RgbImage};
use bytemuck;
use std::io::Cursor;
use std::sync::OnceLock;
use thiserror::Error;
use log::{debug, info};

#[derive(Error, Debug)]
pub enum EncoderError {
    #[error("At least one stop condition must be set")]
    NeedStopCondition,
    #[error("Input image is empty or invalid")]
    EmptyObject,
    #[error("ZP codec error: {0}")]
    ZCodec(#[from] crate::encode::zc::ZCodecError),
    #[error("General error: {0}")]
    General(#[from] crate::utils::error::DjvuError),
}

#[derive(Debug, Clone, Copy, Default)]
pub enum CrcbMode {
    #[default]
    None,
    Half,
    Normal,
    Full,
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderParams {
    pub decibels: Option<f32>,
    pub crcb_mode: CrcbMode,
    pub db_frac: f32,
}

impl Default for EncoderParams {
    fn default() -> Self {
        Self {
            decibels: Some(90.0),
            crcb_mode: CrcbMode::Full,
            db_frac: 0.35,
        }
    }
}

#[inline]
fn signed_to_unsigned_u8(v: i8) -> u8 { (v as i16 + 128) as u8 }

fn convert_signed_buffer_to_grayscale(buf: &[i8], w: u32, h: u32) -> GrayImage {
    let bytes: Vec<u8> = buf.iter().map(|&v| signed_to_unsigned_u8(v)).collect();
    GrayImage::from_raw(w, h, bytes).expect("Invalid buffer dimensions")
}

const SCALE: i32 = 1 << 16;
const ROUND: i32 = 1 << 15;

static YCC_TABLES: OnceLock<([[i32; 256]; 3], [[i32; 256]; 3], [[i32; 256]; 3])> = OnceLock::new();

fn get_ycc_tables() -> &'static ([[i32; 256]; 3], [[i32; 256]; 3], [[i32; 256]; 3]) {
    YCC_TABLES.get_or_init(|| {
        let mut y  = [[0;256]; 3];
        let mut cb = [[0;256]; 3];
        let mut cr = [[0;256]; 3];
        
        const RGB_TO_YCC: [[f32; 3]; 3] = [
            [ 0.304348,  0.608696,  0.086956],
            [ 0.463768, -0.405797, -0.057971],
            [-0.173913, -0.347826,  0.521739],
        ];
        
        for k in 0..256 {
            y[0][k] = (k as f32 * 65536.0 * RGB_TO_YCC[0][0]) as i32;
            y[1][k] = (k as f32 * 65536.0 * RGB_TO_YCC[0][1]) as i32;
            y[2][k] = (k as f32 * 65536.0 * RGB_TO_YCC[0][2]) as i32;
            
            cb[0][k] = (k as f32 * 65536.0 * RGB_TO_YCC[2][0]) as i32;
            cb[1][k] = (k as f32 * 65536.0 * RGB_TO_YCC[2][1]) as i32;
            cb[2][k] = (k as f32 * 65536.0 * RGB_TO_YCC[2][2]) as i32;

            cr[0][k] = (k as f32 * 65536.0 * RGB_TO_YCC[1][0]) as i32;
            cr[1][k] = (k as f32 * 65536.0 * RGB_TO_YCC[1][1]) as i32;
            cr[2][k] = (k as f32 * 65536.0 * RGB_TO_YCC[1][2]) as i32;
        }
        (y, cb, cr)
    })
}

pub fn rgb_to_ycbcr_planes(
    img_raw: &[u8],
    out_y:   &mut [i8],
    out_cb:  &mut [i8],
    out_cr:  &mut [i8],
) {
    assert!(img_raw.len() % 3 == 0,   "input length must be a multiple of 3");
    let npix = img_raw.len() / 3;
    assert_eq!(out_y.len(),  npix);
    assert_eq!(out_cb.len(), npix);
    assert_eq!(out_cr.len(), npix);

    let (y_tbl, cb_tbl, cr_tbl) = get_ycc_tables();

    for (i, chunk) in img_raw.chunks_exact(3).enumerate() {
        let r = chunk[0] as usize;
        let g = chunk[1] as usize;
        let b = chunk[2] as usize;

        let y = y_tbl[0][r] + y_tbl[1][g] + y_tbl[2][b] + 32768;
        out_y[i] = ((y >> 16) - 128) as i8;

        let cb = cb_tbl[0][r] + cb_tbl[1][g] + cb_tbl[2][b] + 32768;
        out_cb[i] = (cb >> 16).clamp(-128, 127) as i8;

        let cr = cr_tbl[0][r] + cr_tbl[1][g] + cr_tbl[2][b] + 32768;
        out_cr[i] = (cr >> 16).clamp(-128, 127) as i8;
    }
}

pub fn rgb_to_ycbcr_buffers(
    img: &RgbImage,
    out_y: &mut [i8],
    out_cb: &mut [i8],
    out_cr: &mut [i8],
) {
    let pixels: &[[u8; 3]] = bytemuck::cast_slice(img.as_raw());
    assert_eq!(out_y.len(), pixels.len());
    assert_eq!(out_cb.len(), pixels.len());
    assert_eq!(out_cr.len(), pixels.len());

    rgb_to_ycbcr_planes(img.as_raw(), out_y, out_cb, out_cr);
}

pub fn ycbcr_from_rgb(img: &RgbImage) -> (Vec<i8>, Vec<i8>, Vec<i8>) {
    let (w, h) = img.dimensions();
    let npix = (w * h) as usize;

    let mut y_buf  = vec![0i8; npix];
    let mut cb_buf = vec![0i8; npix];
    let mut cr_buf = vec![0i8; npix];

    rgb_to_ycbcr_planes(img.as_raw(), &mut y_buf, &mut cb_buf, &mut cr_buf);
    
    debug!("YCbCr conversion completed for {}x{} image", w, h);
    
    (y_buf, cb_buf, cr_buf)
}

pub fn make_ycbcr_codecs(
    y_buf: &[i8],
    cb_buf: &[i8],
    cr_buf: &[i8],
    width: u32,
    height: u32,
    mask: Option<&GrayImage>,
    params: &EncoderParams,
) -> (Codec, Option<Codec>, Option<Codec>) {
    let ymap     = CoeffMap::create_from_signed_channel(y_buf, width, height, mask, "Y");
    let y_codec  = Codec::new(ymap, params);

    let (cb_codec, cr_codec) = match params.crcb_mode {
        CrcbMode::None => (None, None),
        CrcbMode::Half => {
            let (half_width, half_height) = ((width + 1) / 2, (height + 1) / 2);
            let half_size = (half_width * half_height) as usize;
            
            let mut cb_half = vec![0i8; half_size];
            let mut cr_half = vec![0i8; half_size];
            
            for y in 0..half_height {
                for x in 0..half_width {
                    let dst_idx = (y * half_width + x) as usize;
                    
                    let mut cb_sum = 0i32;
                    let mut cr_sum = 0i32;
                    let mut count = 0;
                    
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let src_x = x * 2 + dx;
                            let src_y = y * 2 + dy;
                            if src_x < width && src_y < height {
                                let src_idx = (src_y * width + src_x) as usize;
                                cb_sum += cb_buf[src_idx] as i32;
                                cr_sum += cr_buf[src_idx] as i32;
                                count += 1;
                            }
                        }
                    }
                    
                    cb_half[dst_idx] = (cb_sum / count) as i8;
                    cr_half[dst_idx] = (cr_sum / count) as i8;
                }
            }
            
            let cbmap = CoeffMap::create_from_signed_channel(&cb_half, half_width, half_height, None, "Cb");
            let crmap = CoeffMap::create_from_signed_channel(&cr_half, half_width, half_height, None, "Cr");
            (Some(Codec::new(cbmap, params)), Some(Codec::new(crmap, params)))
        },
        CrcbMode::Normal | CrcbMode::Full => {
            let cbmap = CoeffMap::create_from_signed_channel(cb_buf, width, height, mask, "Cb");
            let crmap = CoeffMap::create_from_signed_channel(cr_buf, width, height, mask, "Cr");
            (Some(Codec::new(cbmap, params)), Some(Codec::new(crmap, params)))
        }
    };

    (y_codec, cb_codec, cr_codec)
}

pub fn encoder_from_rgb_with_helpers(
    img: &RgbImage,
    mask: Option<&GrayImage>,
    params: EncoderParams,
) -> Result<IWEncoder, EncoderError> {
    let (w, h) = img.dimensions();
    let (y_buf, cb_buf, cr_buf) = ycbcr_from_rgb(img);
    let (y_codec, cb_codec, cr_codec) =
        make_ycbcr_codecs(&y_buf, &cb_buf, &cr_buf, w, h, mask, &params);

    Ok(IWEncoder {
        y_codec,
        cb_codec,
        cr_codec,
        params,
        total_slices: 0,
        total_bytes: 0,
        serial: 0,
        crcb_delay: match params.crcb_mode {
            CrcbMode::None => -1,
            CrcbMode::Half => 10,
            CrcbMode::Normal => 10,
            CrcbMode::Full => 0,
        },
        cur_bit: 9,  // Start at highest bitplane
        cur_band: 0, // Start at band 0
    })
}

pub fn encoder_from_gray_with_helpers(
    img: &GrayImage,
    mask: Option<&GrayImage>,
    params: EncoderParams,
) -> Result<IWEncoder, EncoderError> {
    let ymap    = CoeffMap::create_from_image(img, mask);
    let y_codec = Codec::new(ymap, &params);

    Ok(IWEncoder {
        y_codec,
        cb_codec: None,
        cr_codec: None,
        params,
        total_slices: 0,
        total_bytes: 0,
        serial: 0,
        crcb_delay: -1,
        cur_bit: 9,  // Start at highest bitplane
        cur_band: 0, // Start at band 0
    })
}

pub struct IWEncoder {
    y_codec: Codec,
    cb_codec: Option<Codec>,
    cr_codec: Option<Codec>,
    params: EncoderParams,
    total_slices: usize,
    total_bytes: usize,
    serial: u8,
    crcb_delay: i32,
    // Centralized state management
    cur_bit: i32,    // Current bitplane (starts at 9, decrements to -1)
    cur_band: i32,   // Current band (0-9)
}

impl IWEncoder {
    pub fn from_gray(
        img: &GrayImage,
        mask: Option<&GrayImage>,
        params: EncoderParams,
    ) -> Result<Self, EncoderError> {
        encoder_from_gray_with_helpers(img, mask, params)
    }
    
    pub fn from_rgb(
        img: &RgbImage,
        mask: Option<&GrayImage>,
        params: EncoderParams,
    ) -> Result<Self, EncoderError> {
        info!("IWEncoder::from_rgb called with image {}x{}", img.width(), img.height());
        encoder_from_rgb_with_helpers(img, mask, params)
    }

    pub fn encode_chunk(&mut self, max_slices: usize) -> Result<(Vec<u8>, bool), EncoderError> {
        info!("encode_chunk called with max_slices={}", max_slices);
        
        let (w, h) = {
            let map = self.y_codec.map();
            let w = map.width();
            let h = map.height();
            if w == 0 || h == 0 {
                return Err(EncoderError::EmptyObject);
            }
            (w, h)
        };

        if self.params.decibels.is_none() && max_slices == 0 {
            return Err(EncoderError::NeedStopCondition);
        }

        // Check if encoding is finished (centralized state)
        if self.cur_bit < 0 {
            return Ok((Vec::new(), false));
        }

        let mut chunk_data = Vec::new();
        let mut zp = ZEncoder::new(Cursor::new(Vec::new()), true)?;
        let mut slices_encoded = 0;
        let mut estdb = -1.0;

        let _more = self.cur_bit >= 0;
        while slices_encoded < max_slices && self.cur_bit >= 0 {
            // Debug logging to track slice progression and crcb_delay
            debug!("slice {}  bit={} band={}  (delay={})", 
                   self.total_slices, self.cur_bit, self.cur_band, self.crcb_delay);
            
            // The new `encode_slice` handles empty slices internally by writing a single 'false' bit.
            // We no longer need to check for activity here.
            self.y_codec.encode_slice(&mut zp, self.cur_bit, self.cur_band)?;

            // Cb and Cr codecs encode if they exist and delay conditions are met
            if let Some(ref mut cb) = self.cb_codec {
                if self.total_slices as i32 >= self.crcb_delay {
                    debug!("Encoding Cb slice {}", self.total_slices);
                    cb.encode_slice(&mut zp, self.cur_bit, self.cur_band)?;
                }
            }
            if let Some(ref mut cr) = self.cr_codec {
                if self.total_slices as i32 >= self.crcb_delay {
                    debug!("Encoding Cr slice {}", self.total_slices);
                    cr.encode_slice(&mut zp, self.cur_bit, self.cur_band)?;
                }
            }

            // A slice is always processed, so we always increment and advance.
            slices_encoded += 1;
            self.total_slices += 1;

            // Advance to the next bit/band combination BEFORE checking quality
            // This ensures we don't get stuck repeating the same slice
            self.advance_slice();

            // Quality control - estimate decibels
            if let Some(db_target) = self.params.decibels {
                if self.cur_band == 0 || estdb >= db_target - super::constants::DECIBEL_PRUNE {
                    estdb = self.y_codec.estimate_decibel(self.params.db_frac);
                    if estdb >= db_target {
                        info!("encode_chunk: Reached target decibels {:.2}, stopping", db_target);
                        // Set cur_bit to -1 to signal completion and break the loop.
                        self.cur_bit = -1;
                        break;
                    }
                }
            }
        }

        let zp_data = zp.finish()?.into_inner();
        
        // Debug: Check for suspicious repeating patterns in ZP data
        if zp_data.len() > 100 {
            let mut repeating_detected = false;
            for window_size in 2..=10 {
                if zp_data.len() >= window_size * 3 {
                    let pattern = &zp_data[0..window_size];
                    let mut matches = 0;
                    for chunk in zp_data.chunks_exact(window_size) {
                        if chunk == pattern {
                            matches += 1;
                        }
                    }
                    if matches > zp_data.len() / window_size / 2 {
                        info!("WARNING: Detected repeating pattern of size {} in ZP data ({}% of file)", 
                              window_size, (matches * 100) / (zp_data.len() / window_size));
                        repeating_detected = true;
                        break;
                    }
                }
            }
            if !repeating_detected {
                info!("ZP data looks normal (no major repeating patterns detected)");
            }
        }
        
        if slices_encoded == 0 || zp_data.is_empty() {
            info!("encode_chunk: No new data encoded (slices_encoded={}, zp_data_len={}). Returning empty chunk.", slices_encoded, zp_data.len());
            return Ok((Vec::new(), false));
        }

        info!("encode_chunk: Finished encoding {} slices. ZEncoder produced {} bytes.", slices_encoded, zp_data.len());

        // Write IW44 chunk header
        chunk_data.push(self.serial);
        chunk_data.push(slices_encoded as u8);

        if self.serial == 0 {
            let is_color = self.cb_codec.is_some() && self.cr_codec.is_some();
            let major = if is_color { 1 } else { 0x81 }; // Version 1, 0x80 for grayscale
            chunk_data.push(major);
            chunk_data.push(2); // Minor version 2 per C++
            chunk_data.extend_from_slice(&(w as u16).to_be_bytes());
            chunk_data.extend_from_slice(&(h as u16).to_be_bytes());
            let crcb_delay_byte = if self.crcb_delay >= 0 { self.crcb_delay as u8 } else { 0x80 };
            chunk_data.push(crcb_delay_byte);
        }

        chunk_data.extend_from_slice(&zp_data);

        info!("encode_chunk: Created chunk with serial {}. Total chunk size: {} bytes.", self.serial, chunk_data.len());

        self.serial = self.serial.wrapping_add(1);
        self.total_bytes += chunk_data.len();

        // More data remains if we haven't exhausted all bitplanes.
        // The encoder terminates when cur_bit < 0 (all bitplanes done) OR when quality target is reached
        let more = self.cur_bit >= 0;
        Ok((chunk_data, more))
    }

    /// Advances the slice state (bit, band) in a synchronized manner
    fn advance_slice(&mut self) {
        debug!("advance_slice: BEFORE: bit={}, band={}", self.cur_bit, self.cur_band);
        self.cur_band += 1;
        if self.cur_band >= 10 {
            self.cur_band = 0;
            self.cur_bit -= 1;
        }
        debug!("advance_slice: AFTER: bit={}, band={}", self.cur_bit, self.cur_band);
    }
}