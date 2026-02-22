// Removed SIMD dependencies for stable Rust compatibility

use crate::image::image_formats::Bitmap;

/// Saturating conversion from i32 to i16 to prevent overflow
#[inline]
fn _sat16(x: i32) -> i16 {
    if x > 32_767 {
        32_767
    } else if x < -32_768 {
        -32_768
    } else {
        x as i16
    }
}

/// Create gray level conversion table (bconv) matching C++ IW44EncodeCodec.cpp:1656
/// This handles different bit depths and ensures proper value clamping.
fn create_bconv_table(grays: u32) -> [i8; 256] {
    let mut bconv = [0i8; 256];
    let g = (grays - 1) as i32;

    for i in 0..256 {
        // C++: bconv[i] = max(0, min(255, i*255/g)) - 128
        let normalized = if g > 0 {
            ((i as i32 * 255) / g).max(0).min(255)
        } else {
            i as i32
        };
        bconv[i] = (normalized - 128) as i8;
    }

    bconv
}

pub struct Encode;

impl Encode {
    /// Fill data16 from a Bitmap (u8), centering values as needed for IW44.
    /// 
    /// IMPORTANT: C++ GPixmap uses bottom-up coordinates (row 0 = bottom of image).
    /// The image crate uses top-down coordinates (row 0 = top of image).
    /// We flip vertically here to match C++ coordinate system.
    pub fn from_u8_image(img: &Bitmap, data16: &mut [i16], w: usize, h: usize) {
        // Create bconv table for gray level normalization (matches C++ line 1656)
        // For standard 8-bit images, grays=256, so bconv[i] = i - 128
        let bconv = create_bconv_table(256);


        // FLIP VERTICALLY to match C++ GPixmap bottom-up coordinate system
        for y in 0..h {
            let src_y = h - 1 - y; // Flip: top becomes bottom
            for x in 0..w {
                let dst_idx = y * w + x;

                let px = if x < img.width() as usize && src_y < img.height() as usize {
                    img.get_pixel(x as u32, src_y as u32).y
                } else {
                    0
                };
                // Apply bconv table, then scale (matches C++ preprocessing)
                // C++: buffer[j] = bconv[pixel[j]]  (line 1685)
                //      data16[j] = (int)(buffer[j]) << iw_shift  (line 1088)
                let centered = bconv[px as usize] as i32;
                let scaled = centered << crate::encode::iw44::constants::IW_SHIFT;
                data16[dst_idx] = scaled as i16;

            }
        }
    }

    /// Fill data16 from a Bitmap (u8) with stride.
    /// 
    /// IMPORTANT: C++ GPixmap uses bottom-up coordinates (row 0 = bottom of image).
    /// The image crate uses top-down coordinates (row 0 = top of image).
    /// We flip vertically here to match C++ coordinate system.
    /// 
    /// Note: C++ fills padding area with zeros, NOT edge replication.
    /// This matches IW44EncodeCodec.cpp Encode::create() behavior.
    pub fn from_u8_image_with_stride(
        img: &Bitmap,
        data16: &mut [i16],
        w: usize,
        h: usize,
        stride: usize,
    ) {
        // Start with all zeros - this handles padding correctly
        data16.fill(0);

        // Create bconv table for gray level normalization (matches C++)
        let bconv = create_bconv_table(256);

        // FLIP VERTICALLY to match C++ GPixmap bottom-up coordinate system
        // Only write to the actual image area (0..h rows, 0..w columns)
        // Padding area (w..stride columns and h..buffer_h rows) stays at zero
        for y in 0..h {
            let src_y = h - 1 - y; // Flip: top becomes bottom
            for x in 0..w {
                let dst_idx = y * stride + x;

                let px = if x < img.width() as usize && src_y < img.height() as usize {
                    img.get_pixel(x as u32, src_y as u32).y
                } else {
                    0
                };
                // Apply bconv table, then scale (matches C++ preprocessing)
                let centered = bconv[px as usize] as i32;
                let scaled = centered << crate::encode::iw44::constants::IW_SHIFT;
                data16[dst_idx] = scaled as i16;
            }
        }
        // Padding area (columns w..stride and rows h..buffer_h) remains zero
        // This matches C++ behavior exactly
    }

    /// Fill data16 from a signed i8 buffer with stride.
    /// 
    /// IMPORTANT: C++ GPixmap uses bottom-up coordinates (row 0 = bottom of image).
    /// The image crate uses top-down coordinates (row 0 = top of image).
    /// We flip vertically here to match C++ coordinate system.
    /// 
    /// Note: C++ fills padding area with zeros, NOT edge replication.
    /// This matches IW44EncodeCodec.cpp Encode::create() behavior.
    pub fn from_i8_channel_with_stride(
        channel_buf: &[i8],
        data16: &mut [i16],
        w: usize,
        h: usize,
        stride: usize,
    ) {
        // Start with all zeros - this handles padding correctly
        data16.fill(0);

        // FLIP VERTICALLY to match C++ GPixmap bottom-up coordinate system
        // Source row 0 (top of visual image) -> Destination row h-1
        // Source row h-1 (bottom of visual image) -> Destination row 0
        // Only write to the actual image area (0..h rows, 0..w columns)
        // Padding area (w..stride columns and h..buffer_h rows) stays at zero
        for y in 0..h {
            let src_y = h - 1 - y; // Flip: top becomes bottom
            for x in 0..w {
                let src_idx = src_y * w + x;
                let dst_idx = y * stride + x;

                let val = if src_idx < channel_buf.len() {
                    channel_buf[src_idx] as i32
                } else {
                    0
                };
                data16[dst_idx] = (val << crate::encode::iw44::constants::IW_SHIFT) as i16;
            }
        }
        // Padding area (columns w..stride and rows h..buffer_h) remains zero
        // This matches C++ behavior exactly
    }

    /// Forward wavelet transform using the streaming algorithm from DjVuLibre.
    /// Now operates on i16 throughout, matching C++'s short* buffer behavior.
    pub fn forward(
        buf: &mut [i16],
        w: usize,
        h: usize,
        rowsize: usize,
        levels: usize,
    ) {
        let mut scale = 1;
        for _ in 0..levels {
            filter_fh(buf, w, h, rowsize, scale);
            filter_fv(buf, w, h, rowsize, scale);
            scale <<= 1;
        }
    }

    /// Prepare image data and perform the wavelet transform.
    /// 
    /// IMPORTANT: C++ GPixmap uses bottom-up coordinates (row 0 = bottom of image).
    /// The caller's pixel_fn should return pixels in visual order (y=0 is top).
    /// We flip vertically here to match C++ coordinate system.
    pub fn prepare_and_transform<F>(data16: &mut [i16], w: usize, h: usize, pixel_fn: F)
    where
        F: Fn(usize, usize) -> i32,
    {
        // FLIP VERTICALLY to match C++ GPixmap bottom-up coordinate system
        for y in 0..h {
            let src_y = h - 1 - y; // Flip: top becomes bottom
            for x in 0..w {
                let dst_idx = y * w + x;
                data16[dst_idx] = pixel_fn(x, src_y) as i16;
            }
        }
        Self::forward(data16, w, h, w, 5); // Default levels=5 as per DjVu spec
    }
}

/// Streaming horizontal filter - operates on i16 like C++ (port of filter_fh from IW44EncodeCodec.cpp:514)
fn filter_fh(buf: &mut [i16], w: usize, h: usize, mut rowsize: usize, scale: usize) {
    let s = scale;
    let s3 = s + s + s;
    rowsize *= scale;

    let mut y = 0usize;
    let mut p = 0usize;

    while y < h {
        let mut q = p + s;
        let e = p + w;

        // Use i32 for intermediate calculations to prevent overflow
        let mut a1 = 0i32;
        let mut a2 = 0i32;
        let mut a3 = 0i32;
        let mut b1 = 0i32;
        let mut b2 = 0i32;
        let mut b3 = 0i32;

        if q < e {
            a1 = buf[q - s] as i32;
            a2 = a1;
            a3 = a1;
            if q + s < e {
                a2 = buf[q + s] as i32;
            }
            if q + s3 < e {
                a3 = buf[q + s3] as i32;
            }
            b3 = (buf[q] as i32) - ((a1 + a2 + 1) >> 1);
            buf[q] = b3 as i16;  // Store back to i16 (plain cast, no saturation)
            q += s + s;
        }

        while q + s3 < e {
            let a0 = a1;
            a1 = a2;
            a2 = a3;
            a3 = buf[q + s3] as i32;
            let b0 = b1;
            b1 = b2;
            b2 = b3;
            // FIX: Prediction uses +8 >> 4 (matches C: ((a1+a2)<<3)+(a1+a2)-a0-a3+8)>>4)
            let _old_val = buf[q];
            b3 = (buf[q] as i32) - ((((a1 + a2) << 3) + (a1 + a2) - a0 - a3 + 8) >> 4);
            buf[q] = b3 as i16;
            
 
            let idx_i = q as isize - s3 as isize;
            if idx_i >= 0 {
                let idx = idx_i as usize;
                // FIX: Update uses +16 >> 5 (matches C: ((b1+b2)<<3)+(b1+b2)-b0-b3+16)>>5)
                let updated = (buf[idx] as i32) + ((((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 16) >> 5);
                buf[idx] = updated as i16;  // Store back to i16
            }
            q += s + s;
        }

        while q < e {
            // Special case: w-3 <= x < w - both prediction and update (matches C++)
            a1 = a2;
            a2 = a3;
            let b0 = b1;
            b1 = b2;
            b2 = b3;
            b3 = (buf[q] as i32) - ((a1 + a2 + 1) >> 1);
            buf[q] = b3 as i16;
            let idx_i = q as isize - s3 as isize;
            if idx_i >= p as isize {
                let idx = idx_i as usize;
                // Complex update filter with +16 >> 5 (matches C)
                let updated = (buf[idx] as i32) + ((((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 16) >> 5);
                buf[idx] = updated as i16;
            }
            q += s + s;
        }

        while (q as isize) - (s3 as isize) < e as isize {
            // Special case: w <= x < w+3 - only update phase
            let b0 = b1;
            b1 = b2;
            b2 = b3;
            b3 = 0;
            let idx_i = q as isize - s3 as isize;
            if idx_i >= p as isize {
                let idx = idx_i as usize;
                // Complex update filter with +16 >> 5 (matches C)
                let updated = (buf[idx] as i32) + ((((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 16) >> 5);
                buf[idx] = updated as i16;
            }
            q += s + s;
        }

        y += scale;
        p += rowsize;
    }
}

/// Streaming vertical filter (port of filter_fv from IW44EncodeCodec.cpp:404)
fn filter_fv(buf: &mut [i16], w: usize, h: usize, rowsize: usize, scale: usize) {
    let s = scale * rowsize;
    let s3 = s + s + s;
    let mut y = 1usize;
    let mut p = s;
    let h_adj = if h > 0 { ((h - 1) / scale) + 1 } else { 0 };
    let hlimit = h_adj;

    while y as isize - 3 < hlimit as isize {
        // 1-Delta (prediction)
        {
            let mut q = p;
            let e = q + w;
            if y >= 3 && y + 3 < hlimit {
                // Generic case: prediction uses +8>>4 (matches C)
                while q < e {
                    let a = if q >= s { buf[q - s] as i32 } else { 0 } + buf[q + s] as i32;
                    let b = if q >= s3 { buf[q - s3] as i32 } else { 0 } + buf[q + s3] as i32;
                    buf[q] = (buf[q] as i32 - (((a << 3) + a - b + 8) >> 4)) as i16;
                    q += scale;
                }
            } else if y < hlimit {
                // Special case: simple average
                let mut q1 = if y + 1 < hlimit { q + s } else { q - s };
                while q < e {
                    let val_qs = buf[q - s] as i32;
                    let val_q1 = buf[q1] as i32;
                    let a = val_qs + val_q1;
                    buf[q] = (buf[q] as i32 - ((a + 1) >> 1)) as i16;
                    q += scale;
                    q1 += scale;
                }
            }
        }

        // 2-Update
        {
            let q_i = p as isize - s3 as isize;
            if q_i >= 0 {
                let mut q = q_i as usize;
                let e = q + w;
                if y >= 6 && y < hlimit {
                    // Generic case: update uses +16>>5 (matches C)
                    while q < e {
                        let a = if q >= s { buf[q - s] as i32 } else { 0 } + buf[q + s] as i32;
                        let b = if q >= s3 { buf[q - s3] as i32 } else { 0 } + buf[q + s3] as i32;
                        buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                        q += scale;
                    }
                } else if y >= 3 {
                    // Special cases with boundary handling (matches C++: else if (y>=3))
                    // q1 corresponds to q+s when (y-2 < hlimit)
                    // q3 corresponds to q+s3 when (y < hlimit)
                    let mut q1 = if y >= 2 && y - 2 < hlimit { Some(q + s) } else { None };
                    let mut q3 = if y < hlimit { Some(q + s3) } else { None };

                    if y >= 6 {
                        // y>=6 but y>=hlimit (generic update couldn't run)
                        while q < e {
                            let a = if q >= s { buf[q - s] as i32 } else { 0 }
                                + q1.map(|idx| buf[idx] as i32).unwrap_or(0);
                            let b = if q >= s3 { buf[q - s3] as i32 } else { 0 }
                                + q3.map(|idx| buf[idx] as i32).unwrap_or(0);
                            buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            q += scale;
                            if let Some(ref mut idx) = q1 {
                                *idx += scale;
                            }
                            if let Some(ref mut idx) = q3 {
                                *idx += scale;
                            }
                        }
                    } else if y >= 4 {
                        // y in [4, 5]
                        while q < e {
                            let a = if q >= s { buf[q - s] as i32 } else { 0 }
                                + q1.map(|idx| buf[idx] as i32).unwrap_or(0);
                            let b = q3.map(|idx| buf[idx] as i32).unwrap_or(0);
                            buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            q += scale;
                            if let Some(ref mut idx) = q1 {
                                *idx += scale;
                            }
                            if let Some(ref mut idx) = q3 {
                                *idx += scale;
                            }
                        }
                    } else {
                        // y == 3
                        while q < e {
                            let a = q1.map(|idx| buf[idx] as i32).unwrap_or(0);
                            let b = q3.map(|idx| buf[idx] as i32).unwrap_or(0);
                            buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            q += scale;
                            if let Some(ref mut idx) = q1 {
                                *idx += scale;
                            }
                            if let Some(ref mut idx) = q3 {
                                *idx += scale;
                            }
                        }
                    }
                }
            }
        }

        // y is in the scaled coordinate system because hlimit = ceil(h/scale)
        // (matches C++: y += 2)
        y += 2;
        p += s + s;
    }
}
/// Mirror index with even symmetry (DjVu style).
#[inline]
fn mirror(mut k: isize, size: usize) -> usize {
    if k < 0 {
        k = -k;
    }
    if k >= size as isize {
        k = (size as isize - 2) - (k - size as isize);
    }
    k as usize
}
