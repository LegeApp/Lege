// src/hidden_text.rs
//
// DjVu Hidden Text Layer (TXTz chunk) encoder
//
// This implements the zone-based text encoding for searchable/selectable text in DjVu.
// The encoded data is BZZ-compressed (not bzip2!) before being stored as a TXTz chunk.
//
// IMPORTANT: DjVu uses a bottom-left coordinate origin. Input coordinates from hOCR
// (which uses top-left origin) must be converted before encoding.

use std::io::Write;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum HiddenTextError {
    #[error("I/O error during hidden text encoding")]
    Io(#[from] std::io::Error),
    #[error("Coordinate value {0} out of range for 16-bit encoding")]
    CoordinateOutOfRange(i32),
}

/// The type of a zone in the document hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ZoneKind {
    Page = 1,
    Column = 2,
    Region = 3,
    Paragraph = 4,
    Line = 5,
    Word = 6,
    Character = 7,
}

/// A bounding box in DjVu coordinate system (bottom-left origin).
///
/// In DjVu coordinates:
/// - `x` is the left edge
/// - `y` is the BOTTOM edge (not top!)
/// - Origin (0,0) is at the bottom-left of the page
#[derive(Debug, Clone, Copy, Default)]
pub struct BoundingBox {
    /// Left edge X coordinate
    pub x: u16,
    /// Bottom edge Y coordinate (DjVu uses bottom-left origin!)
    pub y: u16,
    /// Width of the box
    pub w: u16,
    /// Height of the box
    pub h: u16,
}

impl BoundingBox {
    /// Returns the right edge X coordinate (xmax in DjVuLibre terms)
    #[inline]
    pub fn xmax(&self) -> u16 {
        self.x.saturating_add(self.w)
    }

    /// Returns the top edge Y coordinate (ymax in DjVuLibre terms)
    #[inline]
    pub fn ymax(&self) -> u16 {
        self.y.saturating_add(self.h)
    }
}

/// A node in the hierarchical text structure.
#[derive(Debug, Clone)]
pub struct Zone {
    pub kind: ZoneKind,
    pub bbox: BoundingBox,
    pub children: Vec<Zone>,
    /// Text is only present at leaf nodes (words or characters).
    pub text: Option<String>,

    // Internal state used during encoding
    text_start: usize,
    text_len: usize,
}

impl Zone {
    pub fn new(kind: ZoneKind, bbox: BoundingBox) -> Self {
        Self {
            kind,
            bbox,
            children: Vec::new(),
            text: None,
            text_start: 0,
            text_len: 0,
        }
    }

    /// Creates a word zone with text and bounding box (in DjVu coordinates)
    pub fn word(text: String, bbox: BoundingBox) -> Self {
        Self {
            kind: ZoneKind::Word,
            bbox,
            children: Vec::new(),
            text: Some(text),
            text_start: 0,
            text_len: 0,
        }
    }
}

/// Represents the complete hidden text structure for a page.
#[derive(Debug, Clone)]
pub struct HiddenText {
    pub root_zone: Zone,
}

impl HiddenText {
    /// Creates a new hidden text structure, typically representing a single page.
    pub fn new(page_bbox: BoundingBox) -> Self {
        Self {
            root_zone: Zone::new(ZoneKind::Page, page_bbox),
        }
    }

    /// Creates a HiddenText layer from a list of word bounding boxes.
    ///
    /// **IMPORTANT**: Input coordinates are expected in top-left origin (hOCR format).
    /// This function converts them to DjVu's bottom-left coordinate system.
    ///
    /// # Arguments
    /// * `page_width`, `page_height` - Page dimensions in pixels
    /// * `words` - Vector of (text, x, y_top, width, height) tuples where:
    ///   - `x` is the left edge
    ///   - `y_top` is the TOP edge (top-left origin, like hOCR)
    ///   - `width`, `height` are the box dimensions
    ///
    /// # Example
    /// ```ignore
    /// let hidden_text = HiddenText::from_word_boxes(
    ///     2550, 3300,
    ///     vec![
    ///         ("Hello".to_string(), 100, 200, 150, 50),  // y=200 is from TOP
    ///         ("World".to_string(), 260, 200, 180, 50),
    ///     ]
    /// );
    /// ```
    pub fn from_word_boxes(
        page_width: u16,
        page_height: u16,
        words: Vec<(String, u16, u16, u16, u16)>, // (text, x, y_top, w, h)
    ) -> Self {
        let mut root = Zone::new(
            ZoneKind::Page,
            BoundingBox {
                x: 0,
                y: 0,  // Bottom of page in DjVu coords
                w: page_width,
                h: page_height,
            },
        );

        // Convert from top-left origin (hOCR) to bottom-left origin (DjVu)
        // and add all words as direct children of the page
        for (text, x, y_top, w, h) in words {
            // Convert Y coordinate: djvu_y_bottom = page_height - (y_top + h)
            let djvu_y = page_height.saturating_sub(y_top.saturating_add(h));
            
            let word_zone = Zone::word(
                text,
                BoundingBox { x, y: djvu_y, w, h },
            );
            root.children.push(word_zone);
        }

        Self { root_zone: root }
    }

    /// Encodes the hidden text structure into the binary format for a TXTa/TXTz chunk.
    ///
    /// **Note**: The output of this function should be compressed with BZZ (not bzip2!)
    /// before being stored in a final DjVu file as a 'TXTz' chunk.
    pub fn encode(&self, writer: &mut impl Write) -> Result<(), HiddenTextError> {
        // 1. Flatten the text from the tree into a single string
        let mut full_text = String::new();
        let mut root_zone = self.root_zone.clone();
        HiddenText::flatten_text_recursive(&mut root_zone, &mut full_text);

        // 2. Write the text component (INT24 length + UTF8 bytes)
        write_u24(writer, full_text.len() as u32)?;
        writer.write_all(full_text.as_bytes())?;

        // 3. Write the zone hierarchy
        const VERSION: u8 = 1;
        writer.write_all(&[VERSION])?;
        Self::encode_zone_recursive(writer, &root_zone, None, None)?;

        Ok(())
    }

    /// Recursively walks the tree, collecting text and assigning text offsets.
    fn flatten_text_recursive(zone: &mut Zone, full_text: &mut String) {
        if let Some(text) = &zone.text {
            zone.text_start = full_text.len();
            full_text.push_str(text);
            zone.text_len = text.len();
        } else {
            zone.text_start = full_text.len();
            for child in &mut zone.children {
                HiddenText::flatten_text_recursive(child, full_text);
            }
            zone.text_len = full_text.len() - zone.text_start;
        }

        // Add separators based on zone type (matching DjVuLibre conventions)
        let sep = match zone.kind {
            ZoneKind::Column => Some('\x0B'),    // VT: Vertical Tab
            ZoneKind::Region => Some('\x1D'),    // GS: Group Separator
            ZoneKind::Paragraph => Some('\x1F'), // US: Unit Separator
            ZoneKind::Line => Some('\n'),        // LF: Line Feed
            ZoneKind::Word => Some(' '),         // Space between words
            _ => None,
        };

        if let Some(sep_char) = sep {
            if !full_text.ends_with(sep_char) {
                full_text.push(sep_char);
                zone.text_len += 1;
            }
        }
    }

    /// Recursively encodes the zone hierarchy into the binary format.
    ///
    /// Zone encoding follows DjVuLibre's algorithm with delta-encoded coordinates:
    /// - Coordinates are relative to previous sibling or parent
    /// - Different zone types use different reference points
    fn encode_zone_recursive(
        writer: &mut impl Write,
        zone: &Zone,
        parent: Option<&Zone>,
        prev_sibling: Option<&Zone>,
    ) -> Result<(), HiddenTextError> {
        // Write zone type (1 byte)
        writer.write_all(&[zone.kind as u8])?;

        // Start with absolute coordinates
        let mut x = zone.bbox.x as i32;
        let mut y = zone.bbox.y as i32;  // This is ymin (bottom edge in DjVu coords)
        let width = zone.bbox.w as i32;
        let height = zone.bbox.h as i32;

        // Calculate delta-encoded coordinates based on DjVuLibre algorithm
        if let Some(prev) = prev_sibling {
            match zone.kind {
                ZoneKind::Page | ZoneKind::Paragraph | ZoneKind::Line => {
                    // For line-breaking zones: offset from lower-left corner of previous,
                    // with x to the right and y DOWN
                    x = x - prev.bbox.x as i32;
                    y = prev.bbox.y as i32 - (y + height);
                }
                _ => {
                    // For COLUMN, WORD, CHARACTER: offset from lower-right corner of previous,
                    // with x to the right and y UP
                    x = x - prev.bbox.xmax() as i32;
                    y = y - prev.bbox.y as i32;
                }
            }
        } else if let Some(p) = parent {
            // First child: offset from upper-left corner of parent,
            // with x to the right and y DOWN
            x = x - p.bbox.x as i32;
            y = p.bbox.ymax() as i32 - (y + height);
        }

        // Validate coordinate ranges (must fit in signed 16-bit with 0x8000 offset)
        for &val in &[x, y, width, height] {
            if val < -32768 || val > 32767 {
                return Err(HiddenTextError::CoordinateOutOfRange(val));
            }
        }

        // Write coordinates (INT16 with +32768 offset)
        write_i16(writer, x)?;
        write_i16(writer, y)?;
        write_i16(writer, width)?;
        write_i16(writer, height)?;

        // Write text info:
        // - offText: Per DjVu3 spec, this field is "Not used. Must be 0."
        // - lenText: Number of characters in this zone (INT24)
        write_i16(writer, 0)?;  // offText MUST be 0 per spec
        write_u24(writer, zone.text_len as u32)?;

        // Write number of children (INT24)
        write_u24(writer, zone.children.len() as u32)?;

        // Recursively encode all children
        let mut prev_child: Option<&Zone> = None;
        for child in &zone.children {
            Self::encode_zone_recursive(writer, child, Some(zone), prev_child)?;
            prev_child = Some(child);
        }

        Ok(())
    }
}

// Helper functions for writing multi-byte integers in DjVu's format.

/// Writes a 24-bit unsigned integer in big-endian format
fn write_u24(writer: &mut impl Write, val: u32) -> Result<(), std::io::Error> {
    writer.write_all(&[(val >> 16) as u8, (val >> 8) as u8, val as u8])
}

/// Writes a 16-bit signed integer with +32768 offset (DjVu's INT16 format)
fn write_i16(writer: &mut impl Write, val: i32) -> Result<(), std::io::Error> {
    let val_u16 = (val + 0x8000) as u16;
    writer.write_all(&val_u16.to_be_bytes())
}
