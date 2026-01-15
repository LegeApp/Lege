/// Pruned Rust equivalents of JBIG2 structs and segment headers
use byteorder::{BigEndian, WriteBytesExt};
use std::io::{self, Write};

#[cfg(feature = "trace_encoder")]
use log::debug;

#[cfg(not(feature = "trace_encoder"))]
use crate::debug;

/// JBIG2 file format magic number
pub const JB2_MAGIC: &[u8; 10] = b"\x97JBIG2\r\n\x1A\n";

/// JBIG2 file format version
pub const JB2_VERSION: u8 = 0x02;

/// Top-level configuration for JBIG2 encoding
#[derive(Debug, Clone)]
pub struct Jbig2Config {
    // Generic region settings
    pub generic: GenericRegionConfig,

    // Symbol dictionary settings
    pub sd_template: u8,      // Symbol dictionary template (0-3)
    pub sd_at: [(i8, i8); 4], // Symbol dictionary AT pixels

    // Text region settings
    pub text_ds_offset: u8,       // SBDSOFFSET
    pub text_refine: bool,        // SBREFINE
    pub text_log_strips: u8,      // LOGSBSTRIPS (0-3)
    pub text_ref_corner: u8,      // REFCORNER (0-3)
    pub text_transposed: bool,    // TRANSPOSED
    pub text_comb_op: u8,         // SBCOMBOP (0-4)
    pub text_refine_template: u8, // SBRTEMPLATE (0 or 1)

    // Halftone region settings
    pub halftone: HalftoneConfig,

    // Global settings
    pub dpi: u32,
    pub symbol_mode: bool,
    pub refine: bool,
    pub refine_template: u8,
    pub duplicate_line_removal: bool,
    pub auto_thresh: bool,
    pub hash: bool,
    pub want_full_headers: bool,
    pub is_lossless: bool,
    pub default_pixel: bool,
}

/// Configuration for halftone encoding
#[derive(Debug, Clone, Copy)]
pub struct HalftoneConfig {
    /// The grid size (M x M) for decimation and pattern generation.
    pub grid_size_m: u32,
    /// The number of quantization levels (N).
    pub quant_levels_n: u32,
    /// Sharpening control parameter (L), typically between 0.0 and 2.0.
    pub sharpening_l: f32,
    /// The template to use for encoding grayscale bitplanes (usually 0).
    pub template: u8,
    /// Whether to use lossless encoding (true) or lossy encoding (false).
    /// Lossless guarantees bit-perfect reconstruction but produces larger files.
    pub lossless: bool,
}

impl Default for HalftoneConfig {
    fn default() -> Self {
        Self {
            grid_size_m: 4,     // 4x4 grid is a common default
            quant_levels_n: 16, // 16 gray levels
            sharpening_l: 0.5,  // A moderate amount of sharpening
            template: 0,
            lossless: false, // Default to lossy encoding for better compression
        }
    }
}

impl Default for Jbig2Config {
    fn default() -> Self {
        Self {
            generic: GenericRegionConfig::default(),
            sd_template: 0,
            sd_at: [(0, 0), (0, 0), (0, 0), (0, 0)],
            text_ds_offset: 0,
            text_refine: false,
            text_log_strips: 0,
            text_ref_corner: 0,
            text_transposed: false,
            text_comb_op: 0,
            text_refine_template: 0,
            halftone: HalftoneConfig::default(),
            dpi: 300,
            symbol_mode: true,
            refine: true, // Enable refinement by default for better compression (requires symbol_mode=true)
            refine_template: 0,
            duplicate_line_removal: true,
            auto_thresh: true,
            hash: true,
            want_full_headers: true,
            is_lossless: false,
            default_pixel: false,
        }
    }
}

impl Jbig2Config {
    /// Creates a new configuration with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a configuration optimized for text documents
    pub fn text() -> Self {
        let mut cfg = Self::default();
        cfg.symbol_mode = true;
        cfg.auto_thresh = true;
        cfg.duplicate_line_removal = true;
        cfg
    }

    /// Creates a configuration for lossless image encoding
    pub fn lossless() -> Self {
        let mut cfg = Self::default();
        cfg.symbol_mode = false;
        cfg.refine = false; // Disable refinement when symbol mode is disabled
        cfg.is_lossless = true;
        cfg.duplicate_line_removal = false;
        cfg
    }
}

/// JBIG2 segment types as defined in the specification
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SegmentType {
    #[default]
    SymbolDictionary = 0,
    IntermediateTextRegion = 4,
    ImmediateTextRegion = 6,
    ImmediateLosslessTextRegion = 7,
    PatternDictionary = 16,
    IntermediateHalftoneRegion = 20,
    ImmediateHalftoneRegion = 22,
    ImmediateLosslessHalftoneRegion = 23,
    IntermediateGenericRegion = 36,
    ImmediateGenericRegion = 38,
    ImmediateLosslessGenericRegion = 39,
    IntermediateGenericRefinementRegion = 40,
    ImmediateGenericRefinementRegion = 42,
    ImmediateLosslessGenericRefinementRegion = 43,
    PageInformation = 48,
    EndOfPage = 49,
    EndOfStripe = 50,
    EndOfFile = 51,
    Profiles = 52,
    Tables = 53,
    ColorPalette = 54,
    FileHeader = 56,
    Extension = 62,
}

impl TryFrom<u8> for SegmentType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(SegmentType::SymbolDictionary),
            4 => Ok(SegmentType::IntermediateTextRegion),
            6 => Ok(SegmentType::ImmediateTextRegion),
            7 => Ok(SegmentType::ImmediateLosslessTextRegion),
            16 => Ok(SegmentType::PatternDictionary),
            20 => Ok(SegmentType::IntermediateHalftoneRegion),
            22 => Ok(SegmentType::ImmediateHalftoneRegion),
            23 => Ok(SegmentType::ImmediateLosslessHalftoneRegion),
            36 => Ok(SegmentType::IntermediateGenericRegion),
            38 => Ok(SegmentType::ImmediateGenericRegion),
            39 => Ok(SegmentType::ImmediateLosslessGenericRegion),
            40 => Ok(SegmentType::IntermediateGenericRefinementRegion),
            42 => Ok(SegmentType::ImmediateGenericRefinementRegion),
            43 => Ok(SegmentType::ImmediateLosslessGenericRefinementRegion),
            48 => Ok(SegmentType::PageInformation),
            49 => Ok(SegmentType::EndOfPage),
            50 => Ok(SegmentType::EndOfStripe),
            51 => Ok(SegmentType::EndOfFile),
            52 => Ok(SegmentType::Profiles),
            53 => Ok(SegmentType::Tables),
            54 => Ok(SegmentType::ColorPalette),
            56 => Ok(SegmentType::FileHeader),
            62 => Ok(SegmentType::Extension),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid segment type: {}", value),
            )),
        }
    }
}

// -----------------------------------------------------------------------------
// File header (magic + flags + number of pages)
// -----------------------------------------------------------------------------

/// Represents the JBIG2 file header as per the specification (§D.4.1)
#[derive(Debug)]
pub struct FileHeader {
    pub organisation_type: bool, // 1 bit: 0 = sequential, 1 = random-access
    pub unknown_n_pages: bool,   // 1 bit: 1 = number of pages unknown
    pub n_pages: u32,            // Number of pages (big-endian), omitted if unknown_n_pages is true
}

impl FileHeader {
    pub fn to_bytes(&self) -> Vec<u8> {
        const MAGIC: &[u8] = b"\x97JB2\r\n\x1A\n";
        let mut buf = Vec::with_capacity(8 + 1 + if self.unknown_n_pages { 0 } else { 4 });
        buf.extend_from_slice(MAGIC);

        let mut flags = 0u8;
        if self.organisation_type {
            flags |= 0x01;
        }
        if self.unknown_n_pages {
            flags |= 0x02;
        }
        buf.push(flags);

        if !self.unknown_n_pages {
            buf.write_u32::<BigEndian>(self.n_pages).unwrap();
        }
        buf
    }
}

// -----------------------------------------------------------------------------
// Page information segment payload (§7.4.8)
// -----------------------------------------------------------------------------

/// Represents the page information segment payload
#[derive(Debug, Default)]
pub struct PageInfo {
    pub width: u32,                 // Page width in pixels
    pub height: u32,                // Page height in pixels
    pub xres: u32,                  // X resolution in pixels per inch
    pub yres: u32,                  // Y resolution in pixels per inch
    pub is_lossless: bool,          // Bit 0: 1 if lossless
    pub contains_refinements: bool, // Bit 1: 1 if contains refinement regions
    pub default_pixel: bool,        // Bit 2: Default pixel value (0 = black, 1 = white)
    pub default_operator: u8,       // Bits 3-4: Default combination operator (0-3)
    pub aux_buffers: bool,          // Bit 5: 1 if auxiliary buffers are used
    pub operator_override: bool,    // Bit 6: 1 if combination operator can be overridden
    pub reserved: bool,             // Bit 7: Must be 0
    pub segment_flags: u16,         // Additional segment flags
}

impl PageInfo {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4 * 4 + 1 + 2);
        buf.write_u32::<BigEndian>(self.width).unwrap();
        buf.write_u32::<BigEndian>(self.height).unwrap();
        buf.write_u32::<BigEndian>(self.xres).unwrap();
        buf.write_u32::<BigEndian>(self.yres).unwrap();

        let mut b = 0u8;
        if self.is_lossless {
            b |= 0x01;
        }
        if self.contains_refinements {
            b |= 0x02;
        }
        if self.default_pixel {
            b |= 0x04;
        }
        b |= (self.default_operator & 0x03) << 3;
        if self.aux_buffers {
            b |= 0x20;
        }
        if self.operator_override {
            b |= 0x40;
        }
        // Bit 7 (reserved) remains 0
        buf.push(b);
        buf.write_u16::<BigEndian>(self.segment_flags).unwrap();
        buf
    }
}

// -----------------------------------------------------------------------------
// Generic region parameters (§7.4.6)
// -----------------------------------------------------------------------------

/// Represents the parameters for a generic region segment as per the JBIG2 specification
#[derive(Debug, Clone)]
pub struct GenericRegionParams {
    pub width: u32,               // Region width in pixels
    pub height: u32,              // Region height in pixels
    pub x: u32,                   // X-coordinate of the top-left corner
    pub y: u32,                   // Y-coordinate of the top-left corner
    pub comb_operator: u8,        // Combination operator (0-4: OR, AND, XOR, XNOR, REPLACE)
    pub mmr: bool,                // 1 = MMR coding, 0 = arithmetic coding
    pub template: u8,             // Generic region template (0-3)
    pub tpgdon: bool,             // Typical prediction generic decoding on/off
    pub at: [(i8, i8); 4],        // Adaptive template coordinates (a1x, a1y, ..., a4x, a4y)
    pub at_pixels: Vec<(i8, i8)>, // Adaptive template pixels (for compatibility)
}

impl GenericRegionParams {
    pub fn new(width: u32, height: u32, dpi: u32) -> Self {
        let at_pixels = vec![(3, -1), (-3, -1), (2, -2), (-2, -2)];
        Self {
            width,
            height,
            x: 0,
            y: 0,
            comb_operator: 4, // REPLACE (safer for single image pages)
            mmr: false,
            template: 0,
            tpgdon: true,
            at: [(3, -1), (-3, -1), (2, -2), (-2, -2)],
            at_pixels,
        }
    }

    /// Validation to ensure compliance with JBIG2 spec
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.template > 3 {
            return Err("Template ID must be 0–3");
        }
        if self.at_pixels.len() > 4 {
            return Err("Maximum 4 AT pixels allowed");
        }
        if self.comb_operator > 4 {
            return Err("Invalid combination operator");
        }
        Ok(())
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        use byteorder::{BigEndian, WriteBytesExt};
        // 18 bytes (width, height, x, y, comb_op, flags) + AT bytes
        let at_count = match self.template {
            0 => 4, // Template 0 writes 4 AT pixels to match C behavior
            1 => 1, // Template 1 uses 1 AT pixel
            _ => 0, // Templates 2-3 use 0
        };
        let mut buf = Vec::with_capacity(18 + at_count * 2);

        buf.write_u32::<BigEndian>(self.width).unwrap();
        buf.write_u32::<BigEndian>(self.height).unwrap();
        buf.write_u32::<BigEndian>(self.x).unwrap();
        buf.write_u32::<BigEndian>(self.y).unwrap();
        buf.push(self.comb_operator);

        let mut flags = 0u8;
        if self.mmr {
            flags |= 0x01; // Bit 0: MMR (only for MMR coding)
        }
        flags |= (self.template & 0x03) << 1; // Bits 1-2: GBTEMPLATE
        if self.tpgdon {
            flags |= 0x08; // Bit 3: TPGDON
        }
        // Bits 4-7 are reserved and set to 0
        buf.push(flags);

        // Write AT coordinates to match C implementation behavior:
        // Template 0: 4 AT pixels (despite JBIG2 spec saying 0)
        // Template 1: 1 AT pixel
        // Template 2: 0 AT pixels
        // Template 3: 0 AT pixels
        let at_count = match self.template {
            0 => 4, // Template 0 writes 4 AT pixels to match C behavior
            1 => 1, // Template 1 uses 1 AT pixel
            _ => 0, // Templates 2-3 use 0
        };
        for i in 0..at_count {
            buf.push(self.at[i].0 as u8);
            buf.push(self.at[i].1 as u8);
        }
        buf
    }
}

/// High-level configuration for generic region segments
#[derive(Clone, Debug)]
pub struct GenericRegionConfig {
    // Segment header parameters
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub comb_operator: u8, // Combination operator (0 = OR, 1 = AND, etc.)

    // Arithmetic encoding parameters
    pub template: u8,             // Template ID (0–3)
    pub tpgdon: bool,             // Typical prediction generic decoding
    pub mmr: bool,                // MMR coding (true) or arithmetic (false)
    pub at_pixels: Vec<(i8, i8)>, // Adaptive template pixels (dx, dy)

    // Metadata (optional, for page info alignment)
    pub dpi: u32, // Resolution in DPI
}

impl GenericRegionConfig {
    /// Creates a new generic region config with defaults
    pub fn new(width: u32, height: u32, dpi: u32) -> Self {
        Self {
            width,
            height,
            x: 0,
            y: 0,
            comb_operator: 0, // Default to OR
            template: 0,      // Default to template 0
            tpgdon: false,    // Disable typical prediction for testing
            // Template 0 default AT pixels to match C encoder (even though spec says none needed)
            at_pixels: vec![(3, -1), (-3, -1), (2, -2), (-2, -2)],
            mmr: false, // Default to arithmetic coding
            dpi,
        }
    }

    /// Validation to ensure compliance with JBIG2 spec
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.template > 3 {
            return Err("Template ID must be 0–3");
        }
        if self.at_pixels.len() > 4 {
            return Err("Maximum 4 AT pixels allowed");
        }
        if self.comb_operator > 4 {
            return Err("Invalid combination operator");
        }
        Ok(())
    }
}

impl Default for GenericRegionConfig {
    fn default() -> Self {
        Self::new(0, 0, 300)
    }
}

impl From<GenericRegionConfig> for GenericRegionParams {
    fn from(cfg: GenericRegionConfig) -> Self {
        let mut at = [(0i8, 0i8); 4];
        for (i, &(dx, dy)) in cfg.at_pixels.iter().enumerate().take(4) {
            at[i] = (dx, dy);
        }
        GenericRegionParams {
            width: cfg.width,
            height: cfg.height,
            x: cfg.x,
            y: cfg.y,
            comb_operator: cfg.comb_operator,
            mmr: cfg.mmr, // MMR coding flag from config
            template: cfg.template,
            tpgdon: cfg.tpgdon,
            at,
            at_pixels: cfg.at_pixels.clone(),
        }
    }
}

// -----------------------------------------------------------------------------
// Symbol dictionary parameters (§7.4.2)
// -----------------------------------------------------------------------------

/// Represents the parameters for a symbol dictionary segment
#[derive(Debug)]
pub struct SymbolDictParams {
    pub sd_template: u8,   // Symbol dictionary template (0-3)
    pub at: [(i8, i8); 4], // Adaptive template coordinates (a1x, a1y, ..., a4x, a4y)
    pub exsyms: u32,       // Number of exported symbols
    pub newsyms: u32,      // Number of new symbols
}

impl SymbolDictParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + 8 + 4 + 4);
        let b = self.sd_template & 0x03; // SDTEMPLATE in low 2 bits
        buf.push(b);
        buf.push(0); // Reserved flags
        for &(x, y) in &self.at {
            buf.push(x as u8);
            buf.push(y as u8);
        }
        buf.write_u32::<BigEndian>(self.exsyms).unwrap();
        buf.write_u32::<BigEndian>(self.newsyms).unwrap();
        buf
    }
}

// -----------------------------------------------------------------------------
// Text region parameters (§7.4.3)
// -----------------------------------------------------------------------------

/// Represents the parameters for a text region segment
#[derive(Debug)]
pub struct TextRegionParams {
    pub width: u32,          // Region width in pixels
    pub height: u32,         // Region height in pixels
    pub x: u32,              // X-coordinate of the top-left corner
    pub y: u32,              // Y-coordinate of the top-left corner
    pub ds_offset: u8,       // Signed 5-bit offset (SBDSOFFSET)
    pub refine: bool,        // SBREFINE flag
    pub log_strips: u8,      // LOGSBSTRIPS (0-3)
    pub ref_corner: u8,      // REFCORNER (0-3)
    pub transposed: bool,    // TRANSPOSED flag
    pub comb_op: u8,         // SBCOMBOP (0-4)
    pub refine_template: u8, // SBRTEMPLATE (0 or 1)
}

impl TextRegionParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16 + 2 + if self.refine { 1 } else { 0 });
        buf.write_u32::<BigEndian>(self.width).unwrap();
        buf.write_u32::<BigEndian>(self.height).unwrap();
        buf.write_u32::<BigEndian>(self.x).unwrap();
        buf.write_u32::<BigEndian>(self.y).unwrap();

        let mut sbrflags: u16 = 0;
        // SBHUFF is 0 for arithmetic coding
        if self.refine {
            sbrflags |= 1 << 1; // SBREFINE
        }
        sbrflags |= ((self.log_strips as u16) & 0x03) << 2; // LOGSBSTRIPS
        sbrflags |= ((self.ref_corner as u16) & 0x03) << 4; // REFCORNER
        if self.transposed {
            sbrflags |= 1 << 6; // TRANSPOSED
        }
        sbrflags |= ((self.comb_op as u16) & 0x03) << 7; // SBCOMBOP
                                                         // SBDEFPIXEL is 0
        sbrflags |= ((self.ds_offset as u16) & 0x1F) << 10; // SBDSOFFSET
        if self.refine && self.refine_template == 1 {
            sbrflags |= 1 << 15; // SBRTEMPLATE
        }
        buf.write_u16::<BigEndian>(sbrflags).unwrap();

        if self.refine && self.refine_template == 1 {
            buf.write_u8(self.refine_template).unwrap();
        }
        buf
    }
}

/// Parameters for a JBIG2 halftone region segment
#[derive(Debug, Clone, Default)]
pub struct HalftoneParams {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub grid_width: u32,    // HGRIDW
    pub grid_height: u32,   // HGRIDH
    pub grid_x: u16,        // HGRIDX
    pub grid_y: u16,        // HGRIDY
    pub grid_vector_x: u16, // HVECX
    pub grid_vector_y: u16, // HVECY
    pub pattern_width: u8,  // HPW
    pub pattern_height: u8, // HPH
    pub template: u8,       // HTEMPLATE (0-3)
}

impl HalftoneParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(26);
        buf.write_u32::<BigEndian>(self.width).unwrap();
        buf.write_u32::<BigEndian>(self.height).unwrap();
        buf.write_u32::<BigEndian>(self.x).unwrap();
        buf.write_u32::<BigEndian>(self.y).unwrap();

        let mut flags = 0u8;
        // HMMR is 0 for arithmetic coding
        flags |= (self.template & 0x03) << 1; // HTEMPLATE bits 1-2

        buf.write_u8(flags).unwrap();

        buf.write_u32::<BigEndian>(self.grid_width).unwrap();
        buf.write_u32::<BigEndian>(self.grid_height).unwrap();
        buf.write_u16::<BigEndian>(self.grid_x).unwrap();
        buf.write_u16::<BigEndian>(self.grid_y).unwrap();
        buf.write_u16::<BigEndian>(self.grid_vector_x).unwrap();
        buf.write_u16::<BigEndian>(self.grid_vector_y).unwrap();
        buf.write_u8(self.pattern_width).unwrap();
        buf.write_u8(self.pattern_height).unwrap();

        buf
    }
}

// -----------------------------------------------------------------------------
// Segment header + payload writer (§7.2)
// -----------------------------------------------------------------------------

/// Represents a JBIG2 segment, including header and payload
#[derive(Default)]
pub struct Segment {
    pub number: u32,               // Segment number
    pub seg_type: SegmentType,     // Segment type
    pub deferred_non_retain: bool, // Bit 7 of Flags1: 0 = retain, 1 = non-retain
    pub retain_flags: u8,          // Up to 5 bits for retention flags
    pub page_association_type: u8, // Bits 0-1 of Flags2: 0=explicit, 1=deferred, 2=all pages
    pub referred_to: Vec<u32>,     // List of referred-to segment numbers
    pub page: Option<u32>,         // Page number if applicable
    pub payload: Vec<u8>,          // Segment data
}

fn encode_varint(mut v: u32, buf: &mut Vec<u8>) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

impl Segment {
    pub fn write_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_u32::<BigEndian>(self.number)?;

        // First flags byte: bits 0-5 = segment type, bit 6 = page association size (0=1 byte, 1=4 bytes), bit 7 = deferred non-retain
        // Always use 4-byte page association for compatibility and simplicity
        let page_size_is_4_bytes = true;
        let flags1 = (self.seg_type as u8 & 0x3F)
            | ((page_size_is_4_bytes as u8) << 6)
            | ((self.deferred_non_retain as u8) << 7);
        w.write_u8(flags1)?;

        // Referred-to segment count and retention flags field
        let referred_to_count = self.referred_to.len();
        if referred_to_count <= 4 {
            // Short form: one byte with high 3 bits = count, low 5 bits = (count+1) retention flags (we write zeros)
            let byte = ((referred_to_count as u8) << 5) | 0x00;
            w.write_u8(byte)?;
        } else {
            // Long form: write 0b111xxxxx then varint count then (count+1) retention bits padded to byte boundary
            w.write_u8(0xE0)?; // 1110 0000
            let mut varint_buf = Vec::new();
            encode_varint(referred_to_count as u32, &mut varint_buf);
            w.write_all(&varint_buf)?;
            // Write (n+1) zero bits padded to bytes
            let retain_bits = referred_to_count + 1;
            let retain_bytes = (retain_bits + 7) / 8;
            for _ in 0..retain_bytes {
                w.write_u8(0x00)?;
            }
        }

        // Referred-to segment numbers: use 4 bytes per spec-friendly encoding (matches our 4-byte segment numbers)
        for &r_num in &self.referred_to {
            w.write_u32::<BigEndian>(r_num)?;
        }

        // Page association field (always present in this writer):
        // Use 0 to indicate global/all-pages when self.page is None
        let p_num = self.page.unwrap_or(0);
        w.write_u32::<BigEndian>(p_num)?;

        let payload_len = self.payload.len() as u32;
        debug!("Segment {} payload length: {}", self.number, payload_len);
        w.write_u32::<BigEndian>(payload_len)?;
        w.write_all(&self.payload)?;

        debug!(
            "Segment::write_into: Wrote segment {}: Type={:?}, Page={:?}, PA Type={}, Data Length={}",
            self.number, self.seg_type, self.page, self.page_association_type, payload_len
        );
        Ok(())
    }
}
