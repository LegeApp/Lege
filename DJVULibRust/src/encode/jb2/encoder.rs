//! DjVu-compatible JB2 encoder following the official DjVu specification
//!
//! This implements the JB2 encoding as specified in Appendix 2 of the DjVu specification,
//! producing a single Sjbz chunk with arithmetically encoded records.

use crate::encode::jb2::error::Jb2Error;
use crate::encode::jb2::num_coder::{NumCoder, NumContext, BIG_POSITIVE};
use crate::encode::jb2::symbol_dict::BitImage;
use crate::encode::zc::ZEncoder;
use std::io::Write;

// Record types as per DjVu specification Table 6
const START_OF_DATA: i32 = 0;
const NEW_MARK: i32 = 1;
const NEW_MARK_LIBRARY_ONLY: i32 = 2;
const NEW_MARK_IMAGE_ONLY: i32 = 3;
const MATCHED_REFINE: i32 = 4;
const MATCHED_REFINE_LIBRARY_ONLY: i32 = 5;
const MATCHED_REFINE_IMAGE_ONLY: i32 = 6;
const MATCHED_COPY: i32 = 7;
const NON_MARK_DATA: i32 = 8;
const REQUIRED_DICT_OR_RESET: i32 = 9;
const PRESERVED_COMMENT: i32 = 10;
const END_OF_DATA: i32 = 11;

// Constants from DjVuLibre
const CELLCHUNK: usize = 20000;

/// Blit information for page encoding
#[derive(Clone, Debug)]
pub struct Jb2BlitInfo {
    pub left: i32,
    pub bottom: i32,
    pub shapeno: usize,
}

/// Shape information for page encoding
#[derive(Clone, Debug)]
pub struct Jb2ShapeInfo<'a> {
    pub bitmap: &'a BitImage,
    pub parent: i32, // -1 for no parent, -2 for non-mark data
}

/// DjVu-compatible JB2 encoder matching DjVuLibre's exact algorithm.
pub struct JB2Encoder<W: Write> {
    _writer: W,
    image_width: u32,
    image_height: u32,
    // Number coder with tree structure
    num_coder: NumCoder,
    // NumContext variables for different number types (matching DjVuLibre)
    dist_record_type: NumContext,
    dist_match_index: NumContext,
    abs_loc_x: NumContext,
    abs_loc_y: NumContext,
    abs_size_x: NumContext,
    abs_size_y: NumContext,
    image_size_dist: NumContext,
    inherited_shape_count_dist: NumContext,
    rel_size_x: NumContext,
    rel_size_y: NumContext,
    // Relative location contexts (for NEW_MARK, MATCHED_REFINE, MATCHED_COPY)
    offset_type_dist: u8,           // Bit context: new row vs same row
    rel_loc_x_last: NumContext,     // X offset for new row
    rel_loc_y_last: NumContext,     // Y offset for new row
    rel_loc_x_current: NumContext,  // X offset for same row
    rel_loc_y_current: NumContext,  // Y offset for same row
    // Relative location state tracking
    last_left: i32,
    last_right: i32,
    last_bottom: i32,
    last_row_left: i32,
    last_row_bottom: i32,
    // Short list for baseline median calculation (matching DjVuLibre)
    short_list: [i32; 3],
    short_list_pos: usize,
    // Bit contexts for direct bitmap coding (1024 contexts)
    bitdist: [u8; 1024],
    // Bit contexts for cross/refinement coding (2048 contexts)
    cbitdist: [u8; 2048],
    // Bit context for refinement flag
    dist_refinement_flag: u8,
    // State
    gotstartrecordp: bool,
    // Track number of cells used for REQUIRED_DICT_OR_RESET
    cur_ncell: usize,
}

impl<W: Write> JB2Encoder<W> {
    /// Create a new DjVu JB2 encoder
    pub fn new(writer: W) -> Self {
        Self {
            _writer: writer,
            image_width: 0,
            image_height: 0,
            num_coder: NumCoder::new(),
            dist_record_type: 0,
            dist_match_index: 0,
            abs_loc_x: 0,
            abs_loc_y: 0,
            abs_size_x: 0,
            abs_size_y: 0,
            image_size_dist: 0,
            inherited_shape_count_dist: 0,
            rel_size_x: 0,
            rel_size_y: 0,
            // Relative location contexts
            offset_type_dist: 0,
            rel_loc_x_last: 0,
            rel_loc_y_last: 0,
            rel_loc_x_current: 0,
            rel_loc_y_current: 0,
            // Relative location state
            last_left: 0,
            last_right: 0,
            last_bottom: 0,
            last_row_left: 0,
            last_row_bottom: 0,
            // Short list for baseline median
            short_list: [0; 3],
            short_list_pos: 0,
            bitdist: [0; 1024],
            cbitdist: [0; 2048],
            dist_refinement_flag: 0,
            gotstartrecordp: false,
            cur_ncell: 1, // Start at 1 like DjVuLibre
        }
    }

    /// Reset all numerical contexts (called by REQUIRED_DICT_OR_RESET after start)
    fn reset_numcoder(&mut self) {
        self.dist_record_type = 0;
        self.dist_match_index = 0;
        self.abs_loc_x = 0;
        self.abs_loc_y = 0;
        self.abs_size_x = 0;
        self.abs_size_y = 0;
        self.image_size_dist = 0;
        self.inherited_shape_count_dist = 0;
        self.rel_size_x = 0;
        self.rel_size_y = 0;
        // Reset relative location contexts
        self.offset_type_dist = 0;
        self.rel_loc_x_last = 0;
        self.rel_loc_y_last = 0;
        self.rel_loc_x_current = 0;
        self.rel_loc_y_current = 0;
        // Reset relative location state
        self.last_left = 0;
        self.last_right = 0;
        self.last_bottom = 0;
        self.last_row_left = 0;
        self.last_row_bottom = 0;
        self.cur_ncell = 1;
    }

    /// Fill short list with a single value (called at start of image or new row)
    /// This matches DjVuLibre's fill_short_list()
    #[inline]
    fn fill_short_list(&mut self, v: i32) {
        self.short_list[0] = v;
        self.short_list[1] = v;
        self.short_list[2] = v;
        self.short_list_pos = 0;
    }

    /// Update short list and return median value (called for same-row characters)
    /// This matches DjVuLibre's update_short_list()
    /// Returns the median of the last 3 baseline values for baseline stabilization
    #[inline]
    fn update_short_list(&mut self, v: i32) -> i32 {
        // Advance circular buffer position
        self.short_list_pos += 1;
        if self.short_list_pos == 3 {
            self.short_list_pos = 0;
        }
        self.short_list[self.short_list_pos] = v;

        // Return median of the 3 values
        let s = &self.short_list;
        if s[0] >= s[1] {
            if s[0] > s[2] {
                if s[1] >= s[2] { s[1] } else { s[2] }
            } else {
                s[0]
            }
        } else {
            if s[0] < s[2] {
                if s[1] >= s[2] { s[2] } else { s[1] }
            } else {
                s[0]
            }
        }
    }

    /// Encode REQUIRED_DICT_OR_RESET record
    /// Before START_OF_DATA: signals need for inherited dictionary
    /// After START_OF_DATA: resets numcoder contexts
    pub fn encode_required_dict_or_reset(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        inherited_shape_count: Option<usize>,
    ) -> Result<(), Jb2Error> {
        // Encode record type 9
        self.num_coder.code_num(
            zc,
            &mut self.dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            REQUIRED_DICT_OR_RESET,
        )?;

        if !self.gotstartrecordp {
            // Before START_OF_DATA: encode inherited shape count
            let count = inherited_shape_count.unwrap_or(0) as i32;
            self.num_coder.code_num(
                zc,
                &mut self.inherited_shape_count_dist,
                0,
                BIG_POSITIVE,
                count,
            )?;
        } else {
            // After START_OF_DATA: reset contexts
            self.reset_numcoder();
        }

        Ok(())
    }

    /// Check if we need to emit REQUIRED_DICT_OR_RESET for context reset
    fn should_reset_contexts(&self) -> bool {
        self.cur_ncell > CELLCHUNK
    }

    /// Encode a bitmap as a single-page DjVu JB2 stream
    pub fn encode_single_page(&mut self, image: &BitImage) -> Result<Vec<u8>, Jb2Error> {
        self.image_width = image.width as u32;
        self.image_height = image.height as u32;

        let buffer = Vec::new();

        // Create ZP encoder (djvu_compat = true for JB2)
        // Pass buffer by value so we can get it back from finish()
        let mut zc = ZEncoder::new(buffer, true)?;

        // Encode start of image record
        self.encode_start_of_image(&mut zc)?;

        // For simplicity, encode the entire image as a single "non-symbol data" record
        self.encode_non_symbol_data(&mut zc, image, 0, 0)?;

        // Encode end of data record
        self.encode_end_of_data(&mut zc)?;

        // Flush the encoder and get the buffer back
        let buffer = zc.finish()?;

        Ok(buffer)
    }

    /// Encode start of image record (record type 0)
    fn encode_start_of_image(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
    ) -> Result<(), Jb2Error> {
        // Encode record type
        self.num_coder.code_num(
            zc,
            &mut self.dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            START_OF_DATA,
        )?;

        // Encode image size: WIDTH then HEIGHT
        self.num_coder.code_num(
            zc,
            &mut self.image_size_dist,
            0,
            BIG_POSITIVE,
            self.image_width as i32,
        )?;
        self.num_coder.code_num(
            zc,
            &mut self.image_size_dist,
            0,
            BIG_POSITIVE,
            self.image_height as i32,
        )?;

        // Encode eventual image refinement flag (0 = no refinement)
        zc.encode(false, &mut self.dist_refinement_flag)?;

        // Initialize relative location state (CRITICAL: must match DjVuLibre code_image_size)
        // DjVuLibre sets last_left = 1 + image_columns to force "new row" on first blit
        // last_row_bottom = image_rows is the TOP of the page in DjVu bottom-up coords
        self.last_left = 1 + self.image_width as i32;
        self.last_row_left = 0;
        self.last_row_bottom = self.image_height as i32;
        self.last_right = 0;
        self.last_bottom = 0;
        // Initialize short list with row bottom (for baseline median calculation)
        self.fill_short_list(self.last_row_bottom);

        self.gotstartrecordp = true;

        Ok(())
    }

    /// Encode non-symbol data record (record type 8)
    fn encode_non_symbol_data(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        bitmap: &BitImage,
        abs_x: i32,
        abs_y: i32,
    ) -> Result<(), Jb2Error> {
        if !self.gotstartrecordp {
            return Err(Jb2Error::InvalidState("No start record".to_string()));
        }

        // Encode record type
        self.num_coder.code_num(
            zc,
            &mut self.dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            NON_MARK_DATA,
        )?;

        // Encode absolute symbol size
        self.num_coder.code_num(
            zc,
            &mut self.abs_size_x,
            0,
            BIG_POSITIVE,
            bitmap.width as i32,
        )?;
        self.num_coder.code_num(
            zc,
            &mut self.abs_size_y,
            0,
            BIG_POSITIVE,
            bitmap.height as i32,
        )?;

        // Encode bitmap by direct coding (matching DjVuLibre's code_bitmap_directly)
        self.encode_bitmap_directly(zc, bitmap)?;

        // Encode absolute location (1-based as per DjVuLibre)
        self.num_coder.code_num(
            zc,
            &mut self.abs_loc_x,
            1,
            self.image_width as i32,
            abs_x + 1,
        )?;
        // For NON_MARK_DATA, top = bottom + rows - 1 + 1 (adjusted for 1-based)
        let top = abs_y + bitmap.height as i32;
        self.num_coder.code_num(
            zc,
            &mut self.abs_loc_y,
            1,
            self.image_height as i32,
            top,
        )?;

        Ok(())
    }

    /// Encode end of data record (record type 11)
    fn encode_end_of_data(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
    ) -> Result<(), Jb2Error> {
        // Encode record type only
        self.num_coder.code_num(
            zc,
            &mut self.dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            END_OF_DATA,
        )?;
        Ok(())
    }

    /// Encode bitmap using direct coding with 10-bit context template.
    /// This matches DjVuLibre's code_bitmap_directly() exactly.
    fn encode_bitmap_directly(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        bitmap: &BitImage,
    ) -> Result<(), Jb2Error> {
        let dw = bitmap.width as i32;
        let dh = bitmap.height as i32;

        // DjVuLibre scans from top row (dy = rows-1) down to bottom (dy = 0)
        // But first we need to set up row pointers with border padding

        // Create padded row access (simulating GBitmap's minborder(3))
        // We'll create a simple wrapper that returns 0 for out-of-bounds
        // NOTE: Flip Y coordinate because DjVu uses bottom-left origin (y=0 at bottom)
        // while BitImage uses top-left origin (y=0 at top)
        let get_pixel = |x: i32, y: i32| -> u8 {
            if x < 0 || y < 0 || x >= dw || y >= dh {
                0
            } else {
                // Flip Y: DjVu y=0 is at bottom, BitImage y=0 is at top
                let flipped_y = dh - 1 - y;
                bitmap.get_pixel_unchecked(x as usize, flipped_y as usize) as u8
            }
        };

        // Iterate from top row down (DjVuLibre order)
        for dy in (0..dh).rev() {
            // Get initial context for this row
            let mut context = self.get_direct_context(&get_pixel, 0, dy);

            for dx in 0..dw {
                // Get pixel value
                let n = get_pixel(dx, dy);

                // Encode the pixel
                zc.encode(n != 0, &mut self.bitdist[context])?;

                // Shift context for next pixel
                if dx + 1 < dw {
                    context = self.shift_direct_context(context, n, &get_pixel, dx + 1, dy);
                }
            }
        }

        Ok(())
    }

    /// Get the direct context for position (x, y).
    /// This matches DjVuLibre's get_direct_context() exactly.
    fn get_direct_context<F>(&self, get_pixel: &F, x: i32, y: i32) -> usize
    where
        F: Fn(i32, i32) -> u8,
    {
        // DjVuLibre uses up2, up1, up0 where up0 is current row, up1 is row above, up2 is 2 rows above
        // Since we're scanning top-down, "up" means higher y values
        // up2 = y + 2, up1 = y + 1, up0 = y
        let up2_y = y + 2;
        let up1_y = y + 1;
        // up0_y = y (current row)

        // Template positions (column offsets relative to current x):
        // up2: [x-1, x, x+1] -> bits [9, 8, 7]
        // up1: [x-2, x-1, x, x+1, x+2] -> bits [6, 5, 4, 3, 2]
        // up0: [x-2, x-1] -> bits [1, 0]

        ((get_pixel(x - 1, up2_y) as usize) << 9)
            | ((get_pixel(x, up2_y) as usize) << 8)
            | ((get_pixel(x + 1, up2_y) as usize) << 7)
            | ((get_pixel(x - 2, up1_y) as usize) << 6)
            | ((get_pixel(x - 1, up1_y) as usize) << 5)
            | ((get_pixel(x, up1_y) as usize) << 4)
            | ((get_pixel(x + 1, up1_y) as usize) << 3)
            | ((get_pixel(x + 2, up1_y) as usize) << 2)
            | ((get_pixel(x - 2, y) as usize) << 1)
            | ((get_pixel(x - 1, y) as usize) << 0)
    }

    /// Shift the direct context for the next pixel.
    /// This matches DjVuLibre's shift_direct_context() exactly.
    fn shift_direct_context<F>(
        &self,
        context: usize,
        next: u8,
        get_pixel: &F,
        x: i32,
        y: i32,
    ) -> usize
    where
        F: Fn(i32, i32) -> u8,
    {
        let up2_y = y + 2;
        let up1_y = y + 1;

        // Shift and bring in new bits
        // ((context << 1) & 0x37a) preserves bits [9,8,6,5,4,3,1] shifted left
        // Then we add: up1[x+2] at bit 2, up2[x+1] at bit 7, next at bit 0
        ((context << 1) & 0x37a)
            | ((get_pixel(x + 2, up1_y) as usize) << 2)
            | ((get_pixel(x + 1, up2_y) as usize) << 7)
            | (next as usize)
    }

    /// Encode start of dictionary record (width=0, height=0 for dictionaries)
    fn encode_start_of_dict(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
    ) -> Result<(), Jb2Error> {
        // Encode record type
        self.num_coder.code_num(
            zc,
            &mut self.dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            START_OF_DATA,
        )?;

        // Dictionary has 0x0 dimensions
        self.num_coder.code_num(
            zc,
            &mut self.image_size_dist,
            0,
            BIG_POSITIVE,
            0,
        )?;
        self.num_coder.code_num(
            zc,
            &mut self.image_size_dist,
            0,
            BIG_POSITIVE,
            0,
        )?;

        // Encode eventual image refinement flag (0 = no refinement)
        zc.encode(false, &mut self.dist_refinement_flag)?;

        // Initialize state for dictionary (matching DjVuLibre code_image_size for dict)
        // Dict uses last_left=1, last_row_left=0, last_row_bottom=0
        self.last_left = 1;
        self.last_row_left = 0;
        self.last_row_bottom = 0;
        self.last_right = 0;
        self.fill_short_list(self.last_row_bottom);

        self.gotstartrecordp = true;
        self.image_width = 0;
        self.image_height = 0;

        Ok(())
    }

    /// Encode absolute mark size (width, height)
    fn encode_absolute_mark_size(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        width: i32,
        height: i32,
    ) -> Result<(), Jb2Error> {
        self.num_coder.code_num(
            zc,
            &mut self.abs_size_x,
            0,
            BIG_POSITIVE,
            width,
        )?;
        self.num_coder.code_num(
            zc,
            &mut self.abs_size_y,
            0,
            BIG_POSITIVE,
            height,
        )?;
        Ok(())
    }

    /// Encode relative mark size (difference from reference)
    fn encode_relative_mark_size(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        width: i32,
        height: i32,
        ref_width: i32,
        ref_height: i32,
    ) -> Result<(), Jb2Error> {
        self.num_coder.code_num(
            zc,
            &mut self.rel_size_x,
            -BIG_POSITIVE,
            BIG_POSITIVE,
            width - ref_width,
        )?;
        self.num_coder.code_num(
            zc,
            &mut self.rel_size_y,
            -BIG_POSITIVE,
            BIG_POSITIVE,
            height - ref_height,
        )?;
        Ok(())
    }

    /// Encode match index (library shape number)
    fn encode_match_index(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        index: i32,
        max_index: i32,
    ) -> Result<(), Jb2Error> {
        self.num_coder.code_num(
            zc,
            &mut self.dist_match_index,
            0,
            max_index,
            index,
        )?;
        Ok(())
    }

    /// Encode NEW_MARK_LIBRARY_ONLY record (type 2)
    /// Used for adding new shapes to dictionary without blitting
    pub fn encode_new_mark_library_only(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        bitmap: &BitImage,
    ) -> Result<(), Jb2Error> {
        if !self.gotstartrecordp {
            return Err(Jb2Error::InvalidState("No start record".to_string()));
        }

        // Encode record type
        self.num_coder.code_num(
            zc,
            &mut self.dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            NEW_MARK_LIBRARY_ONLY,
        )?;

        // Encode absolute symbol size
        self.encode_absolute_mark_size(zc, bitmap.width as i32, bitmap.height as i32)?;

        // Encode bitmap by direct coding
        self.encode_bitmap_directly(zc, bitmap)?;

        Ok(())
    }

    /// Get the cross-coding context for position (x, y).
    /// This matches DjVuLibre's get_cross_context().
    fn get_cross_context<F, G>(
        &self,
        get_current: &F,
        get_ref: &G,
        x: i32,
        y: i32,
        xd2c: i32, // x offset from current to reference
    ) -> usize
    where
        F: Fn(i32, i32) -> u8,
        G: Fn(i32, i32) -> u8,
    {
        // Current image pixels (up1 = row above, up0 = current row)
        let up1_y = y + 1;
        // Reference image pixels
        let rx = x + xd2c;

        // Bits 0-3: current image causal neighborhood
        // Bits 4-10: reference image 3x3 centered neighborhood
        ((get_current(x - 1, up1_y) as usize) << 10)
            | ((get_current(x, up1_y) as usize) << 9)
            | ((get_current(x + 1, up1_y) as usize) << 8)
            | ((get_current(x - 1, y) as usize) << 7)
            | ((get_ref(rx - 1, y + 1) as usize) << 6)
            | ((get_ref(rx, y + 1) as usize) << 5)
            | ((get_ref(rx + 1, y + 1) as usize) << 4)
            | ((get_ref(rx - 1, y) as usize) << 3)
            | ((get_ref(rx, y) as usize) << 2)
            | ((get_ref(rx + 1, y) as usize) << 1)
            | ((get_ref(rx, y - 1) as usize) << 0)
    }

    /// Encode bitmap by cross-coding against a reference bitmap.
    /// This matches DjVuLibre's code_bitmap_by_cross_coding().
    fn encode_bitmap_by_cross_coding(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        bitmap: &BitImage,
        ref_bitmap: &BitImage,
    ) -> Result<(), Jb2Error> {
        let dw = bitmap.width as i32;
        let dh = bitmap.height as i32;
        let cw = ref_bitmap.width as i32;
        let ch = ref_bitmap.height as i32;

        // Calculate centering offset (matching DjVuLibre)
        let xd2c = (dw / 2 - dw + 1) - (cw / 2 - cw + 1);
        let yd2c = (dh / 2 - dh + 1) - (ch / 2 - ch + 1);

        // Get pixel accessor for current bitmap (with Y flip)
        let get_current = |x: i32, y: i32| -> u8 {
            if x < 0 || y < 0 || x >= dw || y >= dh {
                0
            } else {
                let flipped_y = dh - 1 - y;
                bitmap.get_pixel_unchecked(x as usize, flipped_y as usize) as u8
            }
        };

        // Get pixel accessor for reference bitmap (with Y flip and offset)
        let get_ref = |x: i32, y: i32| -> u8 {
            let ry = y + yd2c;
            if x < 0 || ry < 0 || x >= cw || ry >= ch {
                0
            } else {
                let flipped_y = ch - 1 - ry;
                ref_bitmap.get_pixel_unchecked(x as usize, flipped_y as usize) as u8
            }
        };

        // Iterate from top row down (DjVuLibre order)
        for dy in (0..dh).rev() {
            let mut context = self.get_cross_context(&get_current, &get_ref, 0, dy, xd2c);

            for dx in 0..dw {
                let n = get_current(dx, dy);
                zc.encode(n != 0, &mut self.cbitdist[context])?;

                if dx + 1 < dw {
                    context = self.get_cross_context(&get_current, &get_ref, dx + 1, dy, xd2c);
                }
            }
        }

        Ok(())
    }

    /// Encode MATCHED_REFINE_LIBRARY_ONLY record (type 5)
    /// Used for adding refined shapes to dictionary without blitting
    pub fn encode_matched_refine_library_only(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        bitmap: &BitImage,
        parent_index: i32,
        parent_bitmap: &BitImage,
        lib_size: i32,
    ) -> Result<(), Jb2Error> {
        if !self.gotstartrecordp {
            return Err(Jb2Error::InvalidState("No start record".to_string()));
        }

        // Encode record type
        self.num_coder.code_num(
            zc,
            &mut self.dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            MATCHED_REFINE_LIBRARY_ONLY,
        )?;

        // Encode match index
        self.encode_match_index(zc, parent_index, lib_size - 1)?;

        // Encode relative size
        self.encode_relative_mark_size(
            zc,
            bitmap.width as i32,
            bitmap.height as i32,
            parent_bitmap.width as i32,
            parent_bitmap.height as i32,
        )?;

        // Encode bitmap by cross-coding
        self.encode_bitmap_by_cross_coding(zc, bitmap, parent_bitmap)?;

        Ok(())
    }

    /// Encode a standalone dictionary (Djbz chunk content)
    /// This produces the raw JB2 stream for a dictionary without blits.
    pub fn encode_dictionary(
        &mut self,
        shapes: &[BitImage],
        parents: &[i32], // parent index for each shape, -1 if no parent
        inherited_shape_count: usize,
    ) -> Result<Vec<u8>, Jb2Error> {
        // Reset state for a fresh dictionary stream
        self.num_coder.reset();
        self.reset_numcoder();
        self.gotstartrecordp = false;

        let buffer = Vec::new();
        let mut zc = ZEncoder::new(buffer, true)?;

        // Emit REQUIRED_DICT_OR_RESET if there's an inherited dictionary
        if inherited_shape_count > 0 {
            self.encode_required_dict_or_reset(&mut zc, Some(inherited_shape_count))?;
        }

        // Emit START_OF_DATA (0x0 for dictionaries)
        self.encode_start_of_dict(&mut zc)?;

        // Encode each shape
        for (i, shape) in shapes.iter().enumerate() {
            let parent = parents.get(i).copied().unwrap_or(-1);

            if parent >= 0 {
                // Refined shape - use MATCHED_REFINE_LIBRARY_ONLY
                let parent_idx = parent as usize;
                let parent_shape = if parent_idx < inherited_shape_count {
                    // Parent is in inherited dictionary - we'd need access to it
                    // For now, fall back to direct encoding
                    self.encode_new_mark_library_only(&mut zc, shape)?;
                    continue;
                } else {
                    &shapes[parent_idx - inherited_shape_count]
                };

                let lib_size = (inherited_shape_count + i) as i32;
                self.encode_matched_refine_library_only(
                    &mut zc,
                    shape,
                    parent,
                    parent_shape,
                    lib_size,
                )?;
            } else {
                // New shape - use NEW_MARK_LIBRARY_ONLY
                self.encode_new_mark_library_only(&mut zc, shape)?;
            }

            // Check if we need to reset contexts
            if self.should_reset_contexts() {
                self.encode_required_dict_or_reset(&mut zc, None)?;
            }
        }

        // Emit END_OF_DATA
        self.encode_end_of_data(&mut zc)?;

        Ok(zc.finish()?)
    }

    /// Encode absolute location for a blit (record types 3, 6, 8)
    fn encode_absolute_location(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        left: i32,
        bottom: i32,
        rows: i32,
    ) -> Result<(), Jb2Error> {
        // Check start record
        if !self.gotstartrecordp {
            return Err(Jb2Error::InvalidState("No start record".to_string()));
        }

        // Code LEFT (1-based) and TOP (1-based)
        // TOP = bottom + rows - 1 + 1 (converted to 1-based)
        self.num_coder.code_num(
            zc,
            &mut self.abs_loc_x,
            1,
            self.image_width as i32,
            left + 1,
        )?;

        let top = bottom + rows;
        self.num_coder.code_num(
            zc,
            &mut self.abs_loc_y,
            1,
            self.image_height as i32,
            top,
        )?;

        Ok(())
    }

    /// Encode relative location for a blit (record types 1, 4, 7)
    /// This matches DjVuLibre's code_relative_location function.
    fn encode_relative_location(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        left: i32,
        bottom: i32,
        rows: i32,
        columns: i32,
    ) -> Result<(), Jb2Error> {
        // Check start record
        if !self.gotstartrecordp {
            return Err(Jb2Error::InvalidState("No start record".to_string()));
        }

        // Calculate top and right (DjVuLibre uses 1-based coordinates internally)
        let top = bottom + rows - 1;
        let right = left + columns - 1;

        // Determine if this is a new row (left < last_left means we went to a new row)
        let new_row = left < self.last_left;

        // Encode the new_row bit
        zc.encode(new_row, &mut self.offset_type_dist)?;

        if new_row {
            // New row: encode offset from last_row_left and last_row_bottom
            let x_diff = left - self.last_row_left;
            let y_diff = top - self.last_row_bottom;

            self.num_coder.code_num(
                zc,
                &mut self.rel_loc_x_last,
                -BIG_POSITIVE,
                BIG_POSITIVE,
                x_diff,
            )?;
            self.num_coder.code_num(
                zc,
                &mut self.rel_loc_y_last,
                -BIG_POSITIVE,
                BIG_POSITIVE,
                y_diff,
            )?;

            // Update state for new row (matching DjVuLibre exactly)
            self.last_left = left;
            self.last_row_left = left;
            self.last_right = right;
            self.last_bottom = bottom;
            self.last_row_bottom = bottom;
            // Reset short list with new row's baseline
            self.fill_short_list(bottom);
        } else {
            // Same row: encode offset from last_right and last_bottom
            let x_diff = left - self.last_right;
            let y_diff = bottom - self.last_bottom;

            self.num_coder.code_num(
                zc,
                &mut self.rel_loc_x_current,
                -BIG_POSITIVE,
                BIG_POSITIVE,
                x_diff,
            )?;
            self.num_coder.code_num(
                zc,
                &mut self.rel_loc_y_current,
                -BIG_POSITIVE,
                BIG_POSITIVE,
                y_diff,
            )?;

            // Update state for same row (matching DjVuLibre exactly)
            self.last_left = left;
            self.last_right = right;
            // Use short list median for baseline stabilization
            self.last_bottom = self.update_short_list(bottom);
        }

        Ok(())
    }

    /// Encode NEW_MARK record (type 1) - new shape added to library with blit
    pub fn encode_new_mark(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        bitmap: &BitImage,
        left: i32,
        bottom: i32,
    ) -> Result<(), Jb2Error> {
        if !self.gotstartrecordp {
            return Err(Jb2Error::InvalidState("No start record".to_string()));
        }

        // Encode record type
        self.num_coder.code_num(
            zc,
            &mut self.dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            NEW_MARK,
        )?;

        // Encode absolute symbol size
        self.encode_absolute_mark_size(zc, bitmap.width as i32, bitmap.height as i32)?;

        // Encode bitmap by direct coding
        self.encode_bitmap_directly(zc, bitmap)?;

        // Encode relative location (NEW_MARK uses relative, not absolute!)
        self.encode_relative_location(zc, left, bottom, bitmap.height as i32, bitmap.width as i32)?;

        Ok(())
    }

    /// Encode MATCHED_COPY record (type 7) - reference existing shape from library
    pub fn encode_matched_copy(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        shape_index: i32,
        left: i32,
        bottom: i32,
        shape_height: i32,
        shape_width: i32,
        lib_size: i32,
    ) -> Result<(), Jb2Error> {
        if !self.gotstartrecordp {
            return Err(Jb2Error::InvalidState("No start record".to_string()));
        }

        // Encode record type
        self.num_coder.code_num(
            zc,
            &mut self.dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            MATCHED_COPY,
        )?;

        // Encode match index
        self.encode_match_index(zc, shape_index, lib_size - 1)?;

        // Encode relative location (MATCHED_COPY uses relative, not absolute!)
        self.encode_relative_location(zc, left, bottom, shape_height, shape_width)?;

        Ok(())
    }

    /// Encode MATCHED_REFINE record (type 4) - refined shape added to library with blit
    pub fn encode_matched_refine(
        &mut self,
        zc: &mut ZEncoder<Vec<u8>>,
        bitmap: &BitImage,
        parent_index: i32,
        parent_bitmap: &BitImage,
        left: i32,
        bottom: i32,
        lib_size: i32,
    ) -> Result<(), Jb2Error> {
        if !self.gotstartrecordp {
            return Err(Jb2Error::InvalidState("No start record".to_string()));
        }

        // Encode record type
        self.num_coder.code_num(
            zc,
            &mut self.dist_record_type,
            START_OF_DATA,
            END_OF_DATA,
            MATCHED_REFINE,
        )?;

        // Encode match index
        self.encode_match_index(zc, parent_index, lib_size - 1)?;

        // Encode relative size
        self.encode_relative_mark_size(
            zc,
            bitmap.width as i32,
            bitmap.height as i32,
            parent_bitmap.width as i32,
            parent_bitmap.height as i32,
        )?;

        // Encode bitmap by cross-coding
        self.encode_bitmap_by_cross_coding(zc, bitmap, parent_bitmap)?;

        // Encode relative location (MATCHED_REFINE uses relative, not absolute!)
        self.encode_relative_location(zc, left, bottom, bitmap.height as i32, bitmap.width as i32)?;

        Ok(())
    }

    /// Encode a page with blits referencing shapes from a library
    ///
    /// This produces the raw JB2 stream for a page (Sjbz chunk content).
    /// If `inherited_shape_count` > 0, the page references shapes from an external dictionary.
    pub fn encode_page_with_shapes(
        &mut self,
        width: u32,
        height: u32,
        shapes: &[BitImage],
        parents: &[i32],
        blits: &[(i32, i32, usize)], // (left, bottom, shapeno)
        inherited_shape_count: usize,
        inherited_shapes: Option<&[BitImage]>, // shapes from inherited dict if available
    ) -> Result<Vec<u8>, Jb2Error> {
        // Reset state for a fresh page stream
        self.num_coder.reset();
        self.reset_numcoder();
        self.gotstartrecordp = false;

        let buffer = Vec::new();
        let mut zc = ZEncoder::new(buffer, true)?;

        self.image_width = width;
        self.image_height = height;

        // Emit REQUIRED_DICT_OR_RESET if there's an inherited dictionary
        if inherited_shape_count > 0 {
            self.encode_required_dict_or_reset(&mut zc, Some(inherited_shape_count))?;
        }

        // Emit START_OF_DATA with page dimensions
        self.encode_start_of_image(&mut zc)?;

        // Track which shapes have been encoded to library
        let total_shapes = inherited_shape_count + shapes.len();
        let mut shape_in_lib: Vec<bool> = vec![false; total_shapes];

        // Inherited shapes are already in library
        for i in 0..inherited_shape_count {
            shape_in_lib[i] = true;
        }

        // Encode each blit
        for (blit_idx, &(left, bottom, shapeno)) in blits.iter().enumerate() {
            if shapeno >= total_shapes {
                return Err(Jb2Error::InvalidData(format!(
                    "Invalid shape index {} (max {})",
                    shapeno,
                    total_shapes - 1
                )));
            }

            if shape_in_lib[shapeno] {
                // Shape already in library - use MATCHED_COPY
                let (shape_height, shape_width) = if shapeno < inherited_shape_count {
                    inherited_shapes
                        .and_then(|s| s.get(shapeno))
                        .map(|b| (b.height as i32, b.width as i32))
                        .unwrap_or((10, 10)) // Default if not available
                } else {
                    let bm = &shapes[shapeno - inherited_shape_count];
                    (bm.height as i32, bm.width as i32)
                };

                self.encode_matched_copy(
                    &mut zc,
                    shapeno as i32,
                    left,
                    bottom,
                    shape_height,
                    shape_width,
                    shape_in_lib.iter().filter(|&&x| x).count() as i32,
                )?;
            } else {
                // Shape not in library - encode it
                let local_idx = shapeno - inherited_shape_count;
                let bitmap = &shapes[local_idx];
                let parent = parents.get(local_idx).copied().unwrap_or(-1);

                if parent >= 0 && shape_in_lib[parent as usize] {
                    // Use MATCHED_REFINE
                    let parent_bitmap = if (parent as usize) < inherited_shape_count {
                        inherited_shapes
                            .and_then(|s| s.get(parent as usize))
                            .ok_or_else(|| {
                                Jb2Error::InvalidData("Parent shape not available".to_string())
                            })?
                    } else {
                        &shapes[parent as usize - inherited_shape_count]
                    };

                    self.encode_matched_refine(
                        &mut zc,
                        bitmap,
                        parent,
                        parent_bitmap,
                        left,
                        bottom,
                        shape_in_lib.iter().filter(|&&x| x).count() as i32,
                    )?;
                } else {
                    // Use NEW_MARK
                    self.encode_new_mark(&mut zc, bitmap, left, bottom)?;
                }

                // Mark shape as in library
                shape_in_lib[shapeno] = true;
            }

            // Check if we need to reset contexts
            if self.should_reset_contexts() {
                self.encode_required_dict_or_reset(&mut zc, None)?;
            }
        }

        // Emit END_OF_DATA
        self.encode_end_of_data(&mut zc)?;

        let result = zc.finish()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_pattern_encoding() {
        // Create a simple 10x10 pattern
        let mut image = BitImage::new(10, 10).unwrap();

        // Set center pixel
        image.set_usize(5, 5, true);

        // Create encoder
        let mut encoder = JB2Encoder::new(Vec::new());

        // Encode
        let result = encoder.encode_single_page(&image);
        assert!(result.is_ok());

        let data = result.unwrap();
        assert!(!data.is_empty());
        println!("Encoded {} bytes for 10x10 single pixel", data.len());
    }

    #[test]
    fn test_all_black_pattern() {
        // Create a 8x8 all-black pattern
        let mut image = BitImage::new(8, 8).unwrap();
        for y in 0..8 {
            for x in 0..8 {
                image.set_usize(x, y, true);
            }
        }

        let mut encoder = JB2Encoder::new(Vec::new());
        let result = encoder.encode_single_page(&image);
        assert!(result.is_ok());

        let data = result.unwrap();
        println!("Encoded {} bytes for 8x8 all-black", data.len());
    }

    #[test]
    fn test_checkerboard_pattern() {
        // Create a 16x16 checkerboard
        let mut image = BitImage::new(16, 16).unwrap();
        for y in 0..16 {
            for x in 0..16 {
                if (x + y) % 2 == 0 {
                    image.set_usize(x, y, true);
                }
            }
        }

        let mut encoder = JB2Encoder::new(Vec::new());
        let result = encoder.encode_single_page(&image);
        assert!(result.is_ok());

        let data = result.unwrap();
        println!("Encoded {} bytes for 16x16 checkerboard", data.len());
    }
}
