use std::simd::{LaneCount, SupportedLaneCount};

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

pub struct Encode;

impl Encode {
    /// Fill data32 from a GrayImage (u8), centering values as needed for IW44.
    pub fn from_u8_image(img: &::image::GrayImage, data32: &mut [i32], w: usize, h: usize) {
        for y in 0..h {
            for x in 0..w {
                let px = if x < img.width() as usize && y < img.height() as usize {
                    img.get_pixel(x as u32, y as u32)[0]
                } else {
                    0
                };
                data32[y * w + x] = ((px as i32) - 128) << crate::encode::iw44::constants::IW_SHIFT;
            }
        }
    }

    /// Fill data32 from a GrayImage (u8) with stride, using border extension.
    pub fn from_u8_image_with_stride(
        img: &::image::GrayImage,
        data32: &mut [i32],
        w: usize,
        h: usize,
        stride: usize,
    ) {
        data32.fill(0);
        
        // First, populate the actual image data
        for y in 0..h {
            for x in 0..w {
                let px = if x < img.width() as usize && y < img.height() as usize {
                    img.get_pixel(x as u32, y as u32)[0]
                } else {
                    0
                };
                data32[y * stride + x] = ((px as i32) - 128) << crate::encode::iw44::constants::IW_SHIFT;
            }
        }
        
        // Extend borders to fill padding area (mirror/replicate edge pixels)
        let buffer_h = data32.len() / stride;
        
        // Extend right border (replicate rightmost column)
        for y in 0..h {
            let edge_val = data32[y * stride + (w - 1)];
            for x in w..stride {
                data32[y * stride + x] = edge_val;
            }
        }
        
        // Extend bottom border (replicate bottom row)
        for y in h..buffer_h {
            for x in 0..stride {
                data32[y * stride + x] = data32[(h - 1) * stride + x];
            }
        }
    }

    /// Fill data32 from a signed i8 buffer with stride, using border extension.
    pub fn from_i8_channel_with_stride(
        channel_buf: &[i8],
        data32: &mut [i32],
        w: usize,
        h: usize,
        stride: usize,
    ) {
        data32.fill(0);
        
        // First, populate the actual image data
        for y in 0..h {
            for x in 0..w {
                let idx = y * w + x;
                let val = if idx < channel_buf.len() { channel_buf[idx] as i32 } else { 0 };
                data32[y * stride + x] = val << crate::encode::iw44::constants::IW_SHIFT;
            }
        }
        
        // Extend borders to fill padding area (mirror/replicate edge pixels)
        let buffer_h = data32.len() / stride;
        
        // Extend right border (replicate rightmost column)
        for y in 0..h {
            let edge_val = data32[y * stride + (w - 1)];
            for x in w..stride {
                data32[y * stride + x] = edge_val;
            }
        }
        
        // Extend bottom border (replicate bottom row)
        for y in h..buffer_h {
            for x in 0..stride {
                data32[y * stride + x] = data32[(h - 1) * stride + x];
            }
        }
    }

    /// Forward wavelet transform using the streaming algorithm from DjVuLibre.
    pub fn forward<const LANES: usize>(
        buf: &mut [i32],
        w: usize,
        h: usize,
        rowsize: usize,
        levels: usize,
    ) where
        LaneCount<LANES>: SupportedLaneCount,
    {
        let mut scale = 1;
        for _ in 0..levels {
            // 1. horizontal pass stays 32-bit
            filter_fh(buf, w, h, rowsize, scale);

            // 2. vertical pass expects 16-bit ─ cast, run, cast back
            let mut tmp: Vec<i16> = buf.iter().map(|&v| _sat16(v)).collect();
            filter_fv(&mut tmp, w, h, rowsize, scale);
            for (d, &v) in buf.iter_mut().zip(tmp.iter()) {
                *d = v as i32;
            }

            scale <<= 1;
        }
    }

    /// Prepare image data and perform the wavelet transform.
    pub fn prepare_and_transform<F>(data32: &mut [i32], w: usize, h: usize, pixel_fn: F)
    where
        F: Fn(usize, usize) -> i32,
    {
        for y in 0..h {
            for x in 0..w {
                data32[y * w + x] = pixel_fn(x, y);
            }
        }
        Self::forward::<4>(data32, w, h, w, 5); // Default levels=5 as per DjVu spec
    }
}

/// Streaming horizontal filter (port of filter_fh from IW44EncodeCodec.cpp:514)
fn filter_fh(buf: &mut [i32], w: usize, h: usize, mut rowsize: usize, scale: usize) {
    let s = scale;
    let s3 = s + s + s;
    rowsize *= scale;

    let mut y = 0usize;
    let mut p = 0usize;

    while y < h {
        let mut q = p + s;
        let e = p + w;

        let mut a1 = 0i32;
        let mut a2 = 0i32;
        let mut a3 = 0i32;
        let mut b1 = 0i32;
        let mut b2 = 0i32;
        let mut b3 = 0i32;

        if q < e {
            a1 = buf[q - s];
            a2 = a1;
            a3 = a1;
            if q + s < e { a2 = buf[q + s]; }
            if q + s3 < e { a3 = buf[q + s3]; }
            b3 = buf[q] - ((a1 + a2 + 1) >> 1);
            buf[q] = b3;
            q += s + s;
        }

        while q + s3 < e {
            let a0 = a1; a1 = a2; a2 = a3; a3 = buf[q + s3];
            let b0 = b1; b1 = b2; b2 = b3;
            b3 = buf[q] - ((((a1 + a2) << 3) + (a1 + a2) - a0 - a3 + 8) >> 4);
            buf[q] = b3;
            let idx_i = q as isize - s3 as isize;
            if idx_i >= 0 {
                let idx = idx_i as usize;
                buf[idx] = buf[idx] + ((((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 16) >> 5);
            }
            q += s + s;
        }

        while q < e {
            a1 = a2; a2 = a3;
            let b0 = b1; b1 = b2; b2 = b3;
            b3 = buf[q] - ((a1 + a2 + 1) >> 1);
            buf[q] = b3;
            let idx_i = q as isize - s3 as isize;
            if idx_i >= 0 {
                let idx = idx_i as usize;
                buf[idx] = buf[idx] + ((((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 16) >> 5);
            }
            q += s + s;
        }

        while (q as isize) - (s3 as isize) < e as isize {
            let b0 = b1; b1 = b2; b2 = b3; b3 = 0;
            let idx_i = q as isize - s3 as isize;
            if idx_i >= 0 {
                let idx = idx_i as usize;
                buf[idx] = buf[idx] + ((((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 16) >> 5);
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
    let hlimit = ((h - 1) / scale) + 1;

    while y as isize - 3 < hlimit as isize {
        // 1-Delta
        {
            let mut q = p;
            let e = q + w;
            if y >= 3 && y + 3 < hlimit {
                while q < e {
                    let a = buf[q - s] as i32 + buf[q + s] as i32;
                    let b = buf[q - s3] as i32 + buf[q + s3] as i32;
                    buf[q] = (buf[q] as i32 - (((a << 3) + a - b + 8) >> 4)) as i16;
                    q += scale;
                }
            } else if y < hlimit {
                let mut q1 = if y + 1 < hlimit { q + s } else { q - s };
                while q < e {
                    let a = buf[q - s] as i32 + buf[q1] as i32;
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
                    while q < e {
                        let a = buf[q - s] as i32 + buf[q + s] as i32;
                        let b = buf[q - s3] as i32 + buf[q + s3] as i32;
                        buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                        q += scale;
                    }
                } else if y >= 3 {
                    let mut q1 = if y - 2 < hlimit { Some(q + s) } else { None };
                    let mut q3 = if y < hlimit { Some(q + s3) } else { None };
                    if y >= 6 {
                        while q < e {
                            let a = buf[q - s] as i32 + q1.map_or(0, |idx| buf[idx] as i32);
                            let b = buf[q - s3] as i32 + q3.map_or(0, |idx| buf[idx] as i32);
                            buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            q += scale;
                            if let Some(ref mut idx) = q1 { *idx += scale; }
                            if let Some(ref mut idx) = q3 { *idx += scale; }
                        }
                    } else if y >= 4 {
                        while q < e {
                            let a = buf[q - s] as i32 + q1.map_or(0, |idx| buf[idx] as i32);
                            let b = q3.map_or(0, |idx| buf[idx] as i32);
                            buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            q += scale;
                            if let Some(ref mut idx) = q1 { *idx += scale; }
                            if let Some(ref mut idx) = q3 { *idx += scale; }
                        }
                    } else {
                        while q < e {
                            let a = q1.map_or(0, |idx| buf[idx] as i32);
                            let b = q3.map_or(0, |idx| buf[idx] as i32);
                            buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            q += scale;
                            if let Some(ref mut idx) = q1 { *idx += scale; }
                            if let Some(ref mut idx) = q3 { *idx += scale; }
                        }
                    }
                }
            }
        }

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