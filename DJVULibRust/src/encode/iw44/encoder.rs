// src/encode/iw44/encoder.rs

use super::codec::Codec;
use super::coeff_map::CoeffMap;
use crate::encode::zc::ZpEncoderCursor;
use crate::image::image_formats::{Bitmap, Pixmap};
use bytemuck;
use log::{debug, info};
use std::io::Cursor;
use std::sync::OnceLock;
use thiserror::Error;

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
    pub slices: Option<usize>, // Max slices per chunk (C44 default: 74 for first chunk)
    pub bytes: Option<usize>,  // Max bytes per chunk
    pub crcb_mode: CrcbMode,
    pub db_frac: f32,
    pub lossless: bool,
    /// Quantization threshold multiplier (default: 1.0)
    /// Higher values = more aggressive filtering = smaller files, potentially lower quality
    /// Lower values = less aggressive filtering = larger files, potentially higher quality
    /// Range: 0.5 to 2.0 recommended
    pub quant_multiplier: f32,
}

impl Default for EncoderParams {
    fn default() -> Self {
        Self {
            decibels: None,   // No quality limit to match C44 behavior
            slices: Some(74), // C44 default: 74 slices for first chunk
            bytes: None,
            crcb_mode: CrcbMode::Full,
            db_frac: 0.35,
            lossless: false,
            quant_multiplier: 1.0, // Start with C++ default behavior
        }
    }
}

#[inline]
fn signed_to_unsigned_u8(v: i8) -> u8 {
    (v as i16 + 128) as u8
}

fn convert_signed_buffer_to_grayscale(buf: &[i8], w: u32, h: u32) -> Bitmap {
    let pixels: Vec<crate::image::image_formats::GrayPixel> = buf
        .iter()
        .map(|&v| crate::image::image_formats::GrayPixel::new(signed_to_unsigned_u8(v)))
        .collect();
    Bitmap::from_vec(w, h, pixels)
}

const SCALE: i32 = 1 << 16;
const ROUND: i32 = 1 << 15;

static YCC_TABLES: OnceLock<([[i32; 256]; 3], [[i32; 256]; 3], [[i32; 256]; 3])> = OnceLock::new();

fn get_ycc_tables() -> &'static ([[i32; 256]; 3], [[i32; 256]; 3], [[i32; 256]; 3]) {
    YCC_TABLES.get_or_init(|| {
        let mut y = [[0; 256]; 3];
        let mut cb = [[0; 256]; 3];
        let mut cr = [[0; 256]; 3];

        const RGB_TO_YCC: [[f32; 3]; 3] = [
            [0.304348, 0.608696, 0.086956],
            [0.463768, -0.405797, -0.057971],
            [-0.173913, -0.347826, 0.521739],
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

pub fn rgb_to_ycbcr_planes(img_raw: &[u8], out_y: &mut [i8], out_cb: &mut [i8], out_cr: &mut [i8]) {
    assert!(
        img_raw.len() % 3 == 0,
        "input length must be a multiple of 3"
    );
    let npix = img_raw.len() / 3;
    assert_eq!(out_y.len(), npix);
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
    img: &Pixmap,
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

pub fn ycbcr_from_rgb(img: &Pixmap) -> (Vec<i8>, Vec<i8>, Vec<i8>) {
    let (w, h) = img.dimensions();
    let npix = (w * h) as usize;

    let mut y_buf = vec![0i8; npix];
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
    mask: Option<&Bitmap>,
    params: &EncoderParams,
) -> (Codec, Option<Codec>, Option<Codec>) {
    let ymap = CoeffMap::create_from_signed_channel(y_buf, width, height, mask, "Y");
    let y_codec = Codec::new(ymap, params);

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

            let cbmap =
                CoeffMap::create_from_signed_channel(&cb_half, half_width, half_height, None, "Cb");
            let crmap =
                CoeffMap::create_from_signed_channel(&cr_half, half_width, half_height, None, "Cr");
            (
                Some(Codec::new(cbmap, params)),
                Some(Codec::new(crmap, params)),
            )
        }
        CrcbMode::Normal | CrcbMode::Full => {
            let cbmap = CoeffMap::create_from_signed_channel(cb_buf, width, height, mask, "Cb");
            let crmap = CoeffMap::create_from_signed_channel(cr_buf, width, height, mask, "Cr");
            (
                Some(Codec::new(cbmap, params)),
                Some(Codec::new(crmap, params)),
            )
        }
    };

    (y_codec, cb_codec, cr_codec)
}

pub fn encoder_from_rgb_with_helpers(
    img: &Pixmap,
    mask: Option<&Bitmap>,
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
        crcb_half: match params.crcb_mode {
            CrcbMode::Half => true,
            _ => false,
        },
        // Note: curbit/curband state is now owned by each codec (initialized in Codec::new)
    })
}

pub fn encoder_from_gray_with_helpers(
    img: &Bitmap,
    mask: Option<&Bitmap>,
    params: EncoderParams,
) -> Result<IWEncoder, EncoderError> {
    let ymap = CoeffMap::create_from_image(img, mask);
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
        crcb_half: false,  // Grayscale has no chroma
        // Note: curbit/curband state is now owned by each codec (initialized in Codec::new)
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
    crcb_half: bool,  // Added to match C++ behavior
    // Note: curbit/curband state is now owned by each codec independently
}

impl IWEncoder {
    pub fn from_gray(
        img: &Bitmap,
        mask: Option<&Bitmap>,
        params: EncoderParams,
    ) -> Result<Self, EncoderError> {
        encoder_from_gray_with_helpers(img, mask, params)
    }

    pub fn from_rgb(
        img: &Pixmap,
        mask: Option<&Bitmap>,
        params: EncoderParams,
    ) -> Result<Self, EncoderError> {
        info!(
            "IWEncoder::from_rgb called with image {}x{}",
            img.width(),
            img.height()
        );
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

        if !self.params.lossless && self.params.decibels.is_none() && max_slices == 0 {
            return Err(EncoderError::NeedStopCondition);
        }

        // Check if encoding is finished (check Y codec state)
        if self.y_codec.curbit < 0 {
            return Ok((Vec::new(), false));
        }

        let mut chunk_data = Vec::new();
        // Create the ZP encoder for IW44 only. When the `asm_zp` feature is enabled,
        // use the assembly-backed encoder; otherwise, use the Rust implementation.
        #[cfg(feature = "asm_zp")]
        let mut zp_impl = crate::encode::zc::asm::ZEncoder::new(Cursor::new(Vec::new()), true)?;
        #[cfg(not(feature = "asm_zp"))]
        let mut zp_impl = crate::encode::zc::zcodec::ZEncoder::new(Cursor::new(Vec::new()), true)?;
        let mut slices_encoded = 0;
        let mut estdb = -1.0;

        // IMPORTANT: Do NOT reset contexts between progressive chunks of the same image
        // Contexts should only be reset when creating a new encoder for a different image
        // The ZP encoder's adaptive state must persist across progressive chunks

        let _more = self.y_codec.curbit >= 0;
        while slices_encoded < max_slices && self.y_codec.curbit >= 0 {
            // Track bytes before this slice
            let bytes_before = zp_impl.tell_bytes();

            // Encode one slice using codec-controlled scheduling (mirrors DjVuLibre)
            // Each codec manages its own curbit/curband state independently
            let zp: &mut dyn ZpEncoderCursor = &mut zp_impl;
            let should_continue = self.y_codec.code_slice(zp)?;

            // Track bytes after this slice
            let bytes_after = zp_impl.tell_bytes();
            let slice_bytes = bytes_after - bytes_before;

            let _ = (slice_bytes, bytes_after, bytes_before);

            if let Some(ref mut cb) = self.cb_codec {
                if self.total_slices as i32 >= self.crcb_delay {
                    debug!("Encoding Cb slice {}", self.total_slices);
                    let zp: &mut dyn ZpEncoderCursor = &mut zp_impl;
                    cb.code_slice(zp)?;
                }
            }
            if let Some(ref mut cr) = self.cr_codec {
                if self.total_slices as i32 >= self.crcb_delay {
                    debug!("Encoding Cr slice {}", self.total_slices);
                    let zp: &mut dyn ZpEncoderCursor = &mut zp_impl;
                    cr.code_slice(zp)?;
                }
            }

            // A slice is always processed, so we always increment
            slices_encoded += 1;
            self.total_slices += 1;

            // Check slice limit only if not overridden by max_slices parameter
            // When max_slices is usize::MAX, we encode all remaining slices
            if max_slices < usize::MAX {
                if let Some(slice_limit) = self.params.slices {
                    if slices_encoded >= slice_limit {
                        info!(
                            "encode_chunk: Reached slice limit {}, stopping",
                            slice_limit
                        );
                        break;
                    }
                }
            }

            // Check byte limit
            if let Some(byte_limit) = self.params.bytes {
                let current_bytes = zp_impl.tell_bytes();
                if current_bytes >= byte_limit {
                    info!("encode_chunk: Reached byte limit {}, stopping", byte_limit);
                    break;
                }
            }

            // Stop if codec signals no more data
            if !should_continue {
                break;
            }

            // Quality control - estimate decibels (skip if lossless mode)
            if !self.params.lossless {
                if let Some(db_target) = self.params.decibels {
                    // Always check quality after first slice or when appropriate
                    if slices_encoded > 0 || self.y_codec.curband == 0 || estdb >= db_target - super::constants::DECIBEL_PRUNE {
                        estdb = self.y_codec.estimate_decibel(self.params.db_frac);
                        if estdb >= db_target {
                            self.y_codec.curbit = -1;
                            break;
                        }
                    }
                }
            }
        }

        // Finish on the concrete implementation
        let zp_data = zp_impl.finish()?.into_inner();

        // Print bit count summary if enabled (for size gap investigation)
        crate::encode::iw44::codec::print_bit_counts();

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

        if slices_encoded == 0 {
            info!(
                "encode_chunk: No slices encoded (slices_encoded=0). Returning empty chunk."
            );
            return Ok((Vec::new(), false));
        }

        // IMPORTANT: DjVuLibre may output a chunk that contains only headers (no ZP payload)
        // when all processed slices are null. The slice count in the primary header is still
        // meaningful to the decoder. Therefore we must not drop the chunk when zp_data is empty.
        if zp_data.is_empty() {
            info!(
                "encode_chunk: Encoded {} slices but ZP payload is empty (all-null slices). Emitting header-only chunk.",
                slices_encoded
            );
        }

        // Write IW44 chunk header
        chunk_data.push(self.serial);
        chunk_data.push(slices_encoded as u8);

        // Full secondary header only for the first chunk (serial == 0)
        if self.serial == 0 {
            let is_color = self.cb_codec.is_some() && self.cr_codec.is_some();
            // Major version: bit 7 set (0x80) indicates grayscale/BM44, clear indicates color/PM44
            // C++ uses: major = 1 | 0x80 for grayscale, major = 1 for color
            let major = if is_color { 1 } else { 1 | 0x80 };
            chunk_data.push(major);
            chunk_data.push(2); // Minor version 2 per C++
            chunk_data.extend_from_slice(&(w as u16).to_be_bytes());
            chunk_data.extend_from_slice(&(h as u16).to_be_bytes());

            // Tertiary header CrCbDelay byte: For grayscale (no chroma), use 0x00.
            // For color images, set 0x80 flag and OR in the delay value.
            // From C++ IW44EncodeCodec.cpp:
            // - CRCBfull: crcb_half=0, crcb_delay=0 -> crcbdelay = 0x80 | 0 = 0x80
            // - CRCBnormal: crcb_half=0, crcb_delay=10 -> crcbdelay = 0x80 | 10 = 0x8a
            // - CRCBhalf: crcb_half=1, crcb_delay=10 -> crcbdelay = 0x00 | 10 = 0x0a
            let crcb_delay_byte: u8 = if is_color {
                let mut byte = 0x80;
                if self.crcb_delay >= 0 && !self.crcb_half {
                    byte |= self.crcb_delay as u8;
                }
                byte
            } else {
                0x00
            };
            chunk_data.push(crcb_delay_byte);
        }

        // Append ZP payload
        chunk_data.extend_from_slice(&zp_data);
        
        // Determine if more chunks are needed
        let more = self.y_codec.curbit >= 0;

        // Increment serial for next chunk
        self.serial = self.serial.wrapping_add(1);

        Ok((chunk_data, more))
    }
    
}
