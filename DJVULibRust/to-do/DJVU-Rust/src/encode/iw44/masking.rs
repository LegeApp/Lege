// src/iw44/masking.rs

use crate::encode::iw44::transform::Encode;
use ::image::GrayImage;

/// Convert GrayImage mask to i8 mask buffer
pub fn image_to_mask8(mask_img: &GrayImage, bw: usize, ih: usize) -> Vec<i8> {
    let mut mask8 = vec![0i8; bw * ih];
    for y in 0..ih {
        for x in 0..(mask_img.width() as usize).min(bw) {
            // Non-zero mask pixels indicate masked-out regions
            let mask_val = mask_img.get_pixel(x as u32, y as u32)[0];
            mask8[y * bw + x] = if mask_val > 0 { 1 } else { 0 };
        }
    }
    mask8
}

/// Performs the "interpolate_mask" step from IW44: fill in masked-out
/// pixels by averaging neighbors across scales, so that later wavelet
/// decompositions don't waste bits on irrelevant regions.
///
/// Port of `interpolate_mask(short*,int,int,int,const signed char*,int)`
/// from IW44EncodeCodec.cpp. Updated to work with i32 data for consistency
/// with transform.rs and eliminate unnecessary type conversions.
pub fn interpolate_mask(
    data: &mut [i32],
    w: usize,
    h: usize,
    rowsize: usize,
    mask: &[i8],
    mskrowsize: usize,
) {
    // 1) build a count buffer: non-masked => high weight, masked => zero
    let mut count = vec![0i32; w * h];
    for y in 0..h {
        for x in 0..w {
            let m = mask[y * mskrowsize + x];
            count[y * w + x] = if m != 0 { 0 } else { 0x1000 };
        }
    }
    // 2) copy original data into a scratch
    let mut scratch = vec![0i32; w * h];
    for y in 0..h {
        for x in 0..w {
            scratch[y * w + x] = data[y * rowsize + x];
        }
    }
    // 3) iterate over scales
    let mut split = 1;
    let mut scale = 2;
    let mut again = true;
    while again && scale < w && scale < h {
        again = false;
        for i in (0..h).step_by(scale) {
            for j in (0..w).step_by(scale) {
                // compute weighted average over the square [i..i+scale)×[j..j+scale)
                let istart = if i + split > h {
                    i.saturating_sub(scale)
                } else {
                    i
                };
                let jstart = if j + split > w {
                    j.saturating_sub(scale)
                } else {
                    j
                };
                let mut gray_sum = 0i32;
                let mut total_w = 0i32;
                let mut saw_zero = false;
                let iend = (i + scale).min(h);
                let jend = (j + scale).min(w);
                let mut ii = istart;
                while ii < iend {
                    let mut jj = jstart;
                    while jj < jend {
                        let wght = count[ii * w + jj];
                        if wght > 0 {
                            total_w += wght;
                            gray_sum += wght * scratch[ii * w + jj];
                        } else if ii >= i && jj >= j {
                            saw_zero = true;
                        }
                        jj += split;
                    }
                    ii += split;
                }
                let idx = i * w + j;
                if total_w == 0 {
                    // still no information here; we'll try again at a coarser scale
                    again = true;
                    count[idx] = 0;
                } else {
                    // fill masked pixels if we saw them
                    let gray = (gray_sum / total_w) as i32;
                    if saw_zero {
                        for yy in i..iend {
                            for xx in j..jend {
                                let cidx = yy * w + xx;
                                if count[cidx] == 0 {
                                    data[yy * rowsize + xx] = gray;
                                    count[cidx] = 1;
                                }
                            }
                        }
                    }
                    // store for next iteration
                    count[idx] = (total_w >> 2) as i32;
                    scratch[idx] = gray;
                }
            }
        }
        split = scale;
        scale <<= 1;
    }
}

/// Performs the "forward_mask" multiscale masked wavelet decomposition
/// from IW44EncodeCodec.cpp. Updated to work with i32 data for consistency
/// with transform.rs and eliminate unnecessary type conversions.
/// At each scale it zeroes out wavelet coefficients under the mask,
/// then reconstructs and re-decomposes to freeze those regions.
pub fn forward_mask(
    data: &mut [i32],
    w: usize,
    h: usize,
    rowsize: usize,
    begin: usize,
    end: usize,
    mask: &[i8],
    mskrowsize: usize,
) {
    // 1) copy mask into an aligned 1-per-pixel array
    let mut smask = vec![0i8; w * h];
    for y in 0..h {
        for x in 0..w {
            smask[y * w + x] = mask[y * mskrowsize + x];
        }
    }
    // 2) scratch buffer for single-level decomposition (now i32)
    let mut scratch = vec![0i32; w * h];

    let mut scale = begin.next_power_of_two();
    while scale < end {
        // copy every scale-th sample into scratch
        for y in (0..h).step_by(scale) {
            for x in (0..w).step_by(scale) {
                scratch[y * w + x] = data[y * rowsize + x];
            }
        }
        // full-band forward transform - use new API
        let levels = ((scale * 2).trailing_zeros() as usize).saturating_sub(1).min(5);
        Encode::forward::<4>(&mut scratch, w, h, w, levels);

        // zero out masked detail coefficients
        for y in (0..h).step_by(scale * 2) {
            // horizontal band
            for x in (scale..w).step_by(scale * 2) {
                if smask[y * w + x] != 0 {
                    scratch[y * w + x] = 0;
                }
            }
            // vertical band
            if y + scale < h {
                for x in (0..w).step_by(scale) {
                    if smask[(y + scale) * w + x] != 0 {
                        scratch[(y + scale) * w + x] = 0;
                    }
                }
            }
        }

        // reconstruct back to pixel domain (inverse IW44 transform would be called here in reference impl, but is not implemented in encoder)
        // restore visible pixels so they remain exact
        for y in (0..h).step_by(scale) {
            for x in (0..w).step_by(scale) {
                if smask[y * w + x] == 0 {
                    scratch[y * w + x] = data[y * rowsize + x];
                }
            }
        }

        // re-decompose to freeze the mask out
        Encode::forward::<4>(&mut scratch, w, h, w, levels);

        // copy the frozen coefficients back into data
        for y in (0..h).step_by(scale) {
            for x in (0..w).step_by(scale) {
                data[y * rowsize + x] = scratch[y * w + x];
            }
        }

        // update the mask for the next coarser scale
        for y in (0..h).step_by(scale * 2) {
            for x in (0..w).step_by(scale * 2) {
                let m00 = smask[y * w + x] != 0;
                let m10 = if y + scale < h {
                    smask[(y + scale) * w + x] != 0
                } else {
                    false
                };
                let left = x >= scale && smask[y * w + x - scale] != 0;
                let right = x + scale < w && smask[y * w + x + scale] != 0;
                smask[y * w + x] = if m00 && m10 && left && right { 1 } else { 0 };
            }
        }

        scale <<= 1;
    }
}
