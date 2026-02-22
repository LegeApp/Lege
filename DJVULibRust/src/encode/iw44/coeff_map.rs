use super::zigzag::ZIGZAG_LOC;
use super::masking;
use super::transform::Encode;
use crate::image::image_formats::Bitmap;

/// Replaces `IW44Image::Block`, storing coefficients for a 32x32 image block.
/// Uses fixed arrays instead of HashMap for maximum performance.
#[derive(Debug, Clone)]
pub struct Block {
    // 64 optional buckets (1024 coeffs / 16 per bucket); None == bucket all-zero
    buckets: [Option<[i16; 16]>; 64],
}

impl Default for Block {
    fn default() -> Self {
        Self {
            buckets: [None; 64],
        }
    }
}

impl Block {
    pub fn read_liftblock(&mut self, liftblock: &[i16; 1024]) {
        for (i, &loc) in ZIGZAG_LOC.iter().enumerate() {
            let coeff = liftblock[loc as usize];
            if coeff != 0 {
                let bucket_idx = (i / 16) as u8;
                let coeff_idx_in_bucket = i % 16;

                // Ensure bucket exists
                if self.buckets[bucket_idx as usize].is_none() {
                    self.buckets[bucket_idx as usize] = Some([0; 16]);
                }

                self.buckets[bucket_idx as usize].as_mut().unwrap()[coeff_idx_in_bucket] = coeff;
            }
        }
    }

    /// Write coefficients from buckets back to a liftblock in zigzag order
    /// This is the inverse of read_liftblock and is needed for proper spatial reconstruction
    pub fn write_liftblock(&self, liftblock: &mut [i16; 1024]) {
        // Clear the output buffer
        liftblock.fill(0);

        // Reconstruct coefficients in zigzag order
        for (i, &loc) in ZIGZAG_LOC.iter().enumerate() {
            let bucket_idx = (i / 16) as u8;
            let coeff_idx_in_bucket = i % 16;

            if let Some(bucket) = self.buckets[bucket_idx as usize].as_ref() {
                liftblock[loc] = bucket[coeff_idx_in_bucket];
            }
            // If bucket is None, coefficient remains 0 (already set by fill)
        }
    }

    #[inline]
    pub fn get_bucket(&self, bucket_idx: u8) -> Option<&[i16; 16]> {
        self.buckets[bucket_idx as usize].as_ref()
    }

    #[inline]
    pub fn get_bucket_mut(&mut self, bucket_idx: u8) -> &mut [i16; 16] {
        if self.buckets[bucket_idx as usize].is_none() {
            self.buckets[bucket_idx as usize] = Some([0; 16]);
        }
        self.buckets[bucket_idx as usize].as_mut().unwrap()
    }

    pub fn zero_bucket(&mut self, bucket_idx: u8) {
        self.buckets[bucket_idx as usize] = None;
    }

    /// Set a bucket directly (used for encoded map)
    #[inline]
    pub fn set_bucket(&mut self, bucket_idx: u8, val: [i16; 16]) {
        self.buckets[bucket_idx as usize] = Some(val);
    }

    /// Get a coefficient at a specific zigzag index
    pub fn get_coeff_at_zigzag_index(&self, zigzag_idx: usize) -> i16 {
        let bucket_idx = (zigzag_idx / 16) as u8;
        let coeff_idx_in_bucket = zigzag_idx % 16;

        if let Some(bucket) = self.buckets[bucket_idx as usize].as_ref() {
            bucket[coeff_idx_in_bucket]
        } else {
            0
        }
    }

    /// Set a coefficient at a specific zigzag index
    pub fn set_coeff_at_zigzag_index(&mut self, zigzag_idx: usize, value: i16) {
        let bucket_idx = (zigzag_idx / 16) as u8;
        let coeff_idx_in_bucket = zigzag_idx % 16;

        if value == 0 {
            // If setting to zero, we might be able to clear the bucket
            if let Some(bucket) = self.buckets[bucket_idx as usize].as_mut() {
                bucket[coeff_idx_in_bucket] = 0;
                // Check if entire bucket is now zero
                if bucket.iter().all(|&x| x == 0) {
                    self.buckets[bucket_idx as usize] = None;
                }
            }
        } else {
            // Ensure bucket exists
            if self.buckets[bucket_idx as usize].is_none() {
                self.buckets[bucket_idx as usize] = Some([0; 16]);
            }
            self.buckets[bucket_idx as usize].as_mut().unwrap()[coeff_idx_in_bucket] = value;
        }
    }
}

/// Replaces `IW44Image::Map`. Owns all the coefficient blocks for one image component (Y, Cb, or Cr).
#[derive(Debug, Clone)]
pub struct CoeffMap {
    pub blocks: Vec<Block>,
    pub iw: usize, // Image width
    pub ih: usize, // Image height
    pub bw: usize, // Padded block width
    pub bh: usize, // Padded block height
    pub num_blocks: usize,
}

impl CoeffMap {
    pub fn new(width: usize, height: usize) -> Self {
        let bw = (width + 31) & !31;
        let bh = (height + 31) & !31;
        let num_blocks = (bw * bh) / (32 * 32);
        CoeffMap {
            blocks: vec![Block::default(); num_blocks],
            iw: width,
            ih: height,
            bw,
            bh,
            num_blocks,
        }
    }

    pub fn width(&self) -> usize {
        self.iw
    }

    pub fn height(&self) -> usize {
        self.ih
    }

    /// Private helper to copy a 32x32 block from the transform buffer to a liftblock
    fn copy_block_data(
        liftblock: &mut [i16; 1024],
        data16: &[i16],
        bw: usize,
        block_x: usize,
        block_y: usize,
    ) {
        let data_start_x = block_x * 32;
        let data_start_y = block_y * 32;

        for i in 0..32 {
            let src_y = data_start_y + i;
            let src_offset = src_y * bw + data_start_x;
            let dst_offset = i * 32;

            // Copy i16 values directly (no conversion needed, data16 is already i16)
            for j in 0..32 {
                liftblock[dst_offset + j] = data16[src_offset + j];
            }
        }
    }

    /// Private helper that does the core work: allocate buffer, transform, populate blocks
    fn create_from_transform<F>(
        width: usize,
        height: usize,
        mask: Option<&Bitmap>,
        transform_fn: F,
    ) -> Self
    where
        F: FnOnce(&mut [i16], usize, usize, usize), // Added stride parameter
    {
        let mut map = Self::new(width, height);

        // Allocate decomposition buffer (padded) - now using i16 to match C++
        let mut data16 = vec![0i16; map.bw * map.bh];

        // Apply transform function to populate data16
        // Pass actual image size (iw, ih) and stride (bw) to handle padding correctly
        transform_fn(&mut data16, map.iw, map.ih, map.bw);

        // Apply the actual wavelet transform to convert pixels to coefficients
        // DjVuLibre runs the transform on the active image region (iw x ih)
        // while using the padded rowsize (bw) for addressing.
        // See IW44Image::Map::Encode::create():
        //   IW44Image::Transform::Encode::forward(data16, iw, ih, bw, 1, 32);
        let levels = ((map.iw.min(map.ih) as f32).log2() as usize).min(5);
        Encode::forward(&mut data16, map.iw, map.ih, map.bw, levels);

        // Apply masking logic if mask is provided
        if let Some(mask_img) = mask {
            // Now masking functions work directly with i32 data
            let mask8 = masking::image_to_mask8(mask_img, map.bw, map.ih);

            // Apply interpolate_mask to fill masked pixels with neighbor averages
            masking::interpolate_mask(&mut data16, map.iw, map.ih, map.bw, &mask8, map.bw);

            // Apply forward_mask for multiscale masked wavelet decomposition
            masking::forward_mask(&mut data16, map.iw, map.ih, map.bw, 1, 32, &mask8, map.bw);
        }

        // Copy transformed coefficients into blocks
        let blocks_w = map.bw / 32;
        // Standard iteration order (top-to-bottom, left-to-right)
        for block_y in 0..(map.bh / 32) {
            for block_x in 0..blocks_w {
                let block_idx = block_y * blocks_w + block_x;
                let mut liftblock = [0i16; 1024];

                Self::copy_block_data(&mut liftblock, &data16, map.bw, block_x, block_y);
                
                map.blocks[block_idx].read_liftblock(&liftblock);
            }
        }

        map
    }

    /// Create coefficients from an image. Corresponds to `Map::Encode::create`.
    pub fn create_from_image(img: &Bitmap, mask: Option<&Bitmap>) -> Self {
        let (w, h) = img.dimensions();
        Self::create_from_transform(w as usize, h as usize, mask, |data16, iw, ih, stride| {
            Encode::from_u8_image_with_stride(img, data16, iw, ih, stride);
        })
    }

    /// Create a CoeffMap from signed Y channel data (centered around 0)
    pub fn create_from_signed_y_buffer(
        y_buf: &[i8],
        width: u32,
        height: u32,
        mask: Option<&Bitmap>,
    ) -> Self {
        Self::create_from_transform(
            width as usize,
            height as usize,
            mask,
            |data16, iw, ih, stride| {
                Encode::from_i8_channel_with_stride(y_buf, data16, iw, ih, stride);
            },
        )
    }

    /// Create a CoeffMap from signed i8 channel data (Y, Cb, or Cr)
    /// The input data should be centered around 0 (range approximately -128 to +127)
    pub fn create_from_signed_channel(
        channel_buf: &[i8],
        width: u32,
        height: u32,
        mask: Option<&Bitmap>,
        _channel_name: &str, // Keep for API compatibility but don't use for debug
    ) -> Self {
        Self::create_from_transform(
            width as usize,
            height as usize,
            mask,
            |data16, iw, ih, stride| {
                Encode::from_i8_channel_with_stride(channel_buf, data16, iw, ih, stride);
            },
        )
    }

    pub fn slash_res(&mut self, res: usize) {
        // Halve the image dimensions
        self.iw = (self.iw + res - 1) / res;
        self.ih = (self.ih + res - 1) / res;
        // Update padded dimensions
        self.bw = (self.iw + 31) & !31;
        self.bh = (self.ih + 31) & !31;
        // Update number of blocks
        self.num_blocks = (self.bw * self.bh) / (32 * 32);

        let min_bucket = match res {
            0..=1 => return,
            2..=3 => 16,
            4..=7 => 4,
            _ => 1,
        };
        // Adjust blocks vector size
        self.blocks.resize(self.num_blocks, Block::default());

        for block in self.blocks.iter_mut() {
            for buckno in min_bucket..64 {
                block.zero_bucket(buckno as u8);
            }
        }
    }
}

#[cfg(test)]
mod zigzag_tests {
    // include!("zigzag_test.rs"); // Commented out since the file doesn't exist
}
