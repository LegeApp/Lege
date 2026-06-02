/// Pruned Rust equivalents of JBIG2 structs and segment headers
use byteorder::{BigEndian, WriteBytesExt};
use std::io::{self, Write};

#[cfg(feature = "trace_encoder")]
use log::debug;

#[cfg(not(feature = "trace_encoder"))]
use crate::debug;

/// JBIG2 file format magic number per T.88 §D.4.1 — exactly 8 bytes.
/// (An earlier value of `b"\x97JBIG2\r\n\x1A\n"` contained two extra bytes
/// ("IG") and caused every standalone file produced by this encoder to be
/// rejected by spec-compliant decoders such as jbig2dec.)
pub const JB2_MAGIC: &[u8; 8] = b"\x97JB2\r\n\x1A\n";

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
    pub text_ds_offset: i8,       // SBDSOFFSET (signed 5-bit)
    pub text_refine: bool,        // SBREFINE
    pub text_log_strips: u8,      // LOGSBSTRIPS (0-3)
    pub text_ref_corner: u8, // REFCORNER (0-3): 0=BOTTOMLEFT, 1=TOPLEFT, 2=BOTTOMRIGHT, 3=TOPRIGHT
    pub text_transposed: bool, // TRANSPOSED
    pub text_comb_op: u8,    // SBCOMBOP (0-4)
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
    /// Pixel-error tolerance divisor for symbol matching.
    /// max_err = (width * height) / match_tolerance
    /// Lower = more aggressive matching (smaller files, lower quality).
    /// Typical range: 4 (aggressive) to 10 (conservative). Default: 7.
    pub match_tolerance: u32,
    pub lossy_symbol_mode: LossySymbolMode,
    pub sym_unify_min_class_size: usize,
    pub sym_unify_max_err: u32,
    pub sym_unify_max_dx: i32,
    pub sym_unify_max_dy: i32,
    pub sym_unify_class_accept_limit: u32,
    pub sym_unify_core_close_threshold: u32,
    pub sym_unify_min_core_ratio_permille: u16,
    pub sym_unify_min_class_usage: usize,
    pub sym_unify_min_page_span: usize,
    pub sym_unify_min_estimated_gain: i32,
    pub sym_unify_context_mode: SymbolContextMode,
    pub sym_unify_border_score_slack: u32,
    pub sym_unify_score_rescue_slack: u32,
    pub sym_unify_max_border_outside_ink: u32,
    pub sym_unify_refine_min_subcluster_size: usize,
    pub sym_unify_refine_min_usage: usize,
    pub sym_unify_refine_min_page_span: usize,
    pub sym_unify_refine_max_score: u32,
    pub sym_unify_refine_min_gain: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossySymbolMode {
    Off,
    SymbolUnify,
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
    /// Diagnostic backend for the gray-value image inside the halftone region.
    /// When true, use MMR/T.6 coding instead of arithmetic Annex C coding.
    pub gray_mmr: bool,
    /// Backend for the pattern dictionary bitmap.
    /// MMR keeps the tiled halftone cells exact and avoids arithmetic drift in the
    /// collective pattern bitmap.
    pub dict_mmr: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolContextMode {
    WeightedJaccard,
    Cosine,
    Hybrid,
}

impl Default for HalftoneConfig {
    fn default() -> Self {
        Self {
            grid_size_m: 4,     // 4x4 grid is a common default
            quant_levels_n: 16, // 16 gray levels
            sharpening_l: 0.5,  // A moderate amount of sharpening
            template: 0,
            lossless: false, // Default to lossy encoding for better compression
            gray_mmr: false,
            dict_mmr: true,
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
            text_ref_corner: 1,
            text_transposed: false,
            text_comb_op: 0,
            text_refine_template: 0,
            halftone: HalftoneConfig::default(),
            dpi: 300,
            symbol_mode: false,
            refine: false, // Refinement requires symbol_mode; both disabled until global dictionary encoding is fixed
            refine_template: 0,
            duplicate_line_removal: true,
            auto_thresh: true,
            hash: true,
            want_full_headers: true,
            is_lossless: false,
            default_pixel: false,
            match_tolerance: 7,
            lossy_symbol_mode: LossySymbolMode::Off,
            sym_unify_min_class_size: 2,
            sym_unify_max_err: 12,
            sym_unify_max_dx: 1,
            sym_unify_max_dy: 1,
            sym_unify_class_accept_limit: 16,
            sym_unify_core_close_threshold: 11,
            sym_unify_min_core_ratio_permille: 350,
            sym_unify_min_class_usage: 2,
            sym_unify_min_page_span: 1,
            sym_unify_min_estimated_gain: 0,
            sym_unify_context_mode: SymbolContextMode::Hybrid,
            sym_unify_border_score_slack: 4,
            sym_unify_score_rescue_slack: 2,
            sym_unify_max_border_outside_ink: 1,
            sym_unify_refine_min_subcluster_size: 2,
            sym_unify_refine_min_usage: 12,
            sym_unify_refine_min_page_span: 3,
            sym_unify_refine_max_score: 8,
            sym_unify_refine_min_gain: 20,
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
        // Refinement remains opt-in on the preset because SBREFINE=1 adds
        // per-instance overhead. Enable it explicitly when clustering or
        // near-match preservation is expected to pay off.
        cfg
    }

    pub fn text_symbol_unify() -> Self {
        let mut cfg = Self::text();
        cfg.lossy_symbol_mode = LossySymbolMode::SymbolUnify;
        cfg.refine = false;
        cfg.text_refine = false;
        cfg.sym_unify_min_class_size = 2;
        cfg.sym_unify_max_err = 12;
        cfg.sym_unify_max_dx = 1;
        cfg.sym_unify_max_dy = 1;
        cfg.sym_unify_class_accept_limit = 16;
        cfg.sym_unify_core_close_threshold = 11;
        cfg.sym_unify_min_core_ratio_permille = 350;
        cfg.sym_unify_min_class_usage = 2;
        cfg.sym_unify_min_page_span = 1;
        cfg.sym_unify_min_estimated_gain = 0;
        cfg.sym_unify_context_mode = SymbolContextMode::Hybrid;
        cfg.sym_unify_border_score_slack = 4;
        cfg.sym_unify_score_rescue_slack = 2;
        cfg.sym_unify_max_border_outside_ink = 1;
        cfg.sym_unify_refine_min_subcluster_size = 2;
        cfg.sym_unify_refine_min_usage = 12;
        cfg.sym_unify_refine_min_page_span = 3;
        cfg.sym_unify_refine_max_score = 8;
        cfg.sym_unify_refine_min_gain = 20;
        cfg
    }

    #[inline]
    pub fn uses_symbol_unify(&self) -> bool {
        self.lossy_symbol_mode == LossySymbolMode::SymbolUnify
    }

    #[inline]
    pub fn uses_lossy_symbol_dictionary(&self) -> bool {
        self.lossy_symbol_mode != LossySymbolMode::Off
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
        const MAGIC: &[u8] = b"\x97JB2\r\n\x1A\n"; // T.88 §D.4.1, see JB2_MAGIC.
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
            tpgdon: false,    // Disabled — arithmetic coder does not emit TPGD bits yet
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
    pub refine_aggregate: bool,
    pub refine_template: u8,
    pub refine_at: [(i8, i8); 2],
    pub exsyms: u32,  // Number of exported symbols
    pub newsyms: u32, // Number of new symbols
}

impl SymbolDictParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(
            2 + 8
                + if self.refine_aggregate && self.refine_template == 0 {
                    4
                } else {
                    0
                }
                + 4
                + 4,
        );

        let mut flags = 0u16;
        // T.88 §7.4.2.1 Table 12: SDTEMPLATE occupies bits 2-3 of the flags word.
        flags |= ((self.sd_template & 0x03) as u16) << 2;
        if self.refine_aggregate {
            flags |= 1 << 1; // SDREFAGG
        }
        if self.refine_aggregate && (self.refine_template & 0x01) == 1 {
            flags |= 1 << 12; // SDRTEMPLATE
        }

        buf.write_u16::<BigEndian>(flags).unwrap();
        for &(x, y) in &self.at {
            buf.push(x as u8);
            buf.push(y as u8);
        }
        if self.refine_aggregate && self.refine_template == 0 {
            for &(x, y) in &self.refine_at {
                buf.push(x as u8);
                buf.push(y as u8);
            }
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
    pub ds_offset: i8,       // Signed 5-bit offset (SBDSOFFSET)
    pub refine: bool,        // SBREFINE flag
    pub log_strips: u8,      // LOGSBSTRIPS (0-3)
    pub ref_corner: u8,      // REFCORNER (0-3)
    pub transposed: bool,    // TRANSPOSED flag
    pub comb_op: u8,         // SBCOMBOP (0-4)
    pub refine_template: u8, // SBRTEMPLATE (0 or 1)
}

impl TextRegionParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16 + 1 + 2 + if self.refine { 4 } else { 0 });
        buf.write_u32::<BigEndian>(self.width).unwrap();
        buf.write_u32::<BigEndian>(self.height).unwrap();
        buf.write_u32::<BigEndian>(self.x).unwrap();
        buf.write_u32::<BigEndian>(self.y).unwrap();
        // Region segment combination operator.
        buf.write_u8(self.comb_op & 0x07).unwrap();

        // Layout mirrors jbig2enc's packed jbig2_text_region bitfields.
        let mut flags0 = 0u8;
        flags0 |= ((self.comb_op >> 1) & 0x01) as u8; // SBCOMBOP bit 1 (sbcombop2)
        flags0 |= ((self.ds_offset as u8) & 0x1F) << 2; // SBDSOFFSET (signed → u8 → 5-bit)
        if self.refine && self.refine_template == 1 {
            flags0 |= 1 << 7; // SBRTEMPLATE
        }
        buf.write_u8(flags0).unwrap();

        let mut flags1 = 0u8;
        if self.refine {
            flags1 |= 1 << 1; // SBREFINE
        }
        flags1 |= (self.log_strips & 0x03) << 2; // LOGSBSTRIPS
        flags1 |= (self.ref_corner & 0x03) << 4; // REFCORNER
        if self.transposed {
            flags1 |= 1 << 6; // TRANSPOSED
        }
        flags1 |= (self.comb_op & 0x01) << 7; // SBCOMBOP bit 0 (sbcombop1)
        buf.write_u8(flags1).unwrap();

        // Refinement AT flags: only written for SBRTEMPLATE=0 (which uses 2 AT pixels = 4 bytes).
        // SBRTEMPLATE=1 has no AT pixels.
        if self.refine && self.refine_template == 0 {
            buf.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // (-1,-1), (-1,-1)
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
    pub region_comb_operator: u8,
    pub mmr: bool,
    pub skip_enabled: bool,
    pub halftone_comb_operator: u8,
    pub default_pixel: bool,
    pub grid_width: u32,    // HGRIDW
    pub grid_height: u32,   // HGRIDH
    pub grid_x: i32,        // HGX, fixed-point 24.8
    pub grid_y: i32,        // HGY, fixed-point 24.8
    pub grid_vector_x: u16, // HRX, fixed-point 8.8
    pub grid_vector_y: u16, // HRY, fixed-point 8.8
    pub pattern_width: u8,  // HPW
    pub pattern_height: u8, // HPH
    pub template: u8,       // HTEMPLATE (0-3)
}

impl HalftoneParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(31);
        buf.write_u32::<BigEndian>(self.width).unwrap();
        buf.write_u32::<BigEndian>(self.height).unwrap();
        buf.write_u32::<BigEndian>(self.x).unwrap();
        buf.write_u32::<BigEndian>(self.y).unwrap();
        buf.write_u8(self.region_comb_operator & 0x07).unwrap();

        let mut flags = 0u8;
        if self.mmr {
            flags |= 0x01;
        }
        flags |= (self.template & 0x03) << 1;
        if self.skip_enabled {
            flags |= 0x08;
        }
        flags |= (self.halftone_comb_operator & 0x07) << 4;
        if self.default_pixel {
            flags |= 0x80;
        }

        buf.write_u8(flags).unwrap();

        buf.write_u32::<BigEndian>(self.grid_width).unwrap();
        buf.write_u32::<BigEndian>(self.grid_height).unwrap();
        buf.write_i32::<BigEndian>(self.grid_x).unwrap();
        buf.write_i32::<BigEndian>(self.grid_y).unwrap();
        buf.write_u16::<BigEndian>(self.grid_vector_x).unwrap();
        buf.write_u16::<BigEndian>(self.grid_vector_y).unwrap();

        buf
    }
}

#[derive(Debug, Clone, Default)]
pub struct PatternDictionaryParams {
    pub mmr: bool,
    pub template: u8,
    pub pattern_width: u8,
    pub pattern_height: u8,
    pub gray_max: u32,
}

impl PatternDictionaryParams {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(7);
        let mut flags = 0u8;
        if self.mmr {
            flags |= 0x01;
        }
        flags |= (self.template & 0x03) << 1;
        buf.push(flags);
        buf.push(self.pattern_width);
        buf.push(self.pattern_height);
        buf.write_u32::<BigEndian>(self.gray_max).unwrap();
        buf
    }
}

// -----------------------------------------------------------------------------
// Segment header + payload writer (§7.2)
// -----------------------------------------------------------------------------

/// Represents a JBIG2 segment, including header and payload
#[derive(Debug, Clone, Default)]
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

        // Flags1: segment type, page association size, deferred non-retain.
        let page_num_val = self.page.unwrap_or(0);
        let page_size_is_4_bytes = page_num_val > 0xFF;
        let flags1 = (self.seg_type as u8 & 0x3F)
            | ((page_size_is_4_bytes as u8) << 6)
            | ((self.deferred_non_retain as u8) << 7);
        w.write_u8(flags1)?;

        // Flags2: retain bits + referred-to segment count (compact form).
        // T.88 §7.2.4: counts 0–4 fit in bits 5–7 of flags2; counts ≥ 5 require the
        // long form (bits = 0b111 followed by a 4-byte count and an expanded retain-flags
        // field). The long form is not implemented here, so reject counts > 4.
        let referred_to_count = self.referred_to.len();
        if referred_to_count > 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "referred_to count > 4 requires the long-form segment header which is not implemented",
            ));
        }
        let flags2 = (self.retain_flags & 0x1F) | ((referred_to_count as u8) << 5);
        w.write_u8(flags2)?;

        // T.88 §7.2.5: the referred-to segment number width is determined by the
        // current segment's number — strict less-than comparisons (the spec uses
        // "less than 256" / "less than 65536").
        let ref_size = if self.number < 256 {
            1
        } else if self.number < 65_536 {
            2
        } else {
            4
        };

        for &r_num in &self.referred_to {
            match ref_size {
                1 => w.write_u8(r_num as u8)?,
                2 => w.write_u16::<BigEndian>(r_num as u16)?,
                4 => w.write_u32::<BigEndian>(r_num)?,
                _ => unreachable!(),
            }
        }

        // Page association field is always present (0 means global segment stream).
        if page_size_is_4_bytes {
            w.write_u32::<BigEndian>(page_num_val)?;
        } else {
            w.write_u8(page_num_val as u8)?;
        }

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
