// src/hidden_text.rs

use std::io::Write;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum HiddenTextError {
    #[error("I/O error during hidden text encoding")]
    Io(#[from] std::io::Error),
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

/// A simple bounding box.
#[derive(Debug, Clone, Copy, Default)]
pub struct BoundingBox {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
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
}

/// Represents the complete hidden text structure for a page.
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

    /// Encodes the hidden text structure into the binary format for a TXTa/TXTz chunk.
    /// The output of this function should be compressed (e.g., with bzip2) before
    /// being stored in a final DjVu file as a 'TXTz' chunk.
    pub fn encode(&mut self, writer: &mut impl Write) -> Result<(), HiddenTextError> {
        // 1. Flatten the text from the tree into a single string
        let mut full_text = String::new();
        HiddenText::flatten_text_recursive(&mut self.root_zone, &mut full_text);

        // 2. Write the text component
        write_u24(writer, full_text.len() as u32)?;
        writer.write_all(full_text.as_bytes())?;

        // 3. Write the zone hierarchy
        const VERSION: u8 = 1;
        writer.write_all(&[VERSION])?;
        self.encode_zone_recursive(writer, &self.root_zone, None, None)?;

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

        // Add separators based on zone type
        let sep = match zone.kind {
            ZoneKind::Column => Some('\x0B'),    // VT
            ZoneKind::Region => Some('\x1D'),    // GS
            ZoneKind::Paragraph => Some('\x1F'), // US
            ZoneKind::Line => Some('\n'),        // LF
            ZoneKind::Word => Some(' '),
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
    fn encode_zone_recursive(
        &self,
        writer: &mut impl Write,
        zone: &Zone,
        parent: Option<&Zone>,
        prev_sibling: Option<&Zone>,
    ) -> Result<(), HiddenTextError> {
        writer.write_all(&[zone.kind as u8])?;

        let (mut x, mut y) = (zone.bbox.x as i32, zone.bbox.y as i32);
        let mut text_start_offset = zone.text_start as i32;

        // Calculate relative coordinates and text offsets
        if let Some(p) = prev_sibling {
            text_start_offset -= (p.text_start + p.text_len) as i32;
            match zone.kind {
                ZoneKind::Page | ZoneKind::Paragraph | ZoneKind::Line => {
                    x -= p.bbox.x as i32;
                    y = (p.bbox.y as i32) - (y + zone.bbox.h as i32);
                }
                _ => {
                    x -= (p.bbox.x + p.bbox.w) as i32;
                    y -= p.bbox.y as i32;
                }
            }
        } else if let Some(p) = parent {
            text_start_offset -= p.text_start as i32;
            x -= p.bbox.x as i32;
            y = (p.bbox.y + p.bbox.h) as i32 - (y + zone.bbox.h as i32);
        }

        write_i16(writer, x)?;
        write_i16(writer, y)?;
        write_i16(writer, zone.bbox.w as i32)?;
        write_i16(writer, zone.bbox.h as i32)?;

        write_i16(writer, text_start_offset)?;
        write_u24(writer, zone.text_len as u32)?;
        write_u24(writer, zone.children.len() as u32)?;

        let mut prev_child = None;
        for child in &zone.children {
            self.encode_zone_recursive(writer, child, Some(zone), prev_child)?;
            prev_child = Some(child);
        }

        Ok(())
    }
}

// Helper functions for writing multi-byte integers in DjVu's format.
fn write_u24(writer: &mut impl Write, val: u32) -> Result<(), std::io::Error> {
    writer.write_all(&[(val >> 16) as u8, (val >> 8) as u8, val as u8])
}

fn write_i16(writer: &mut impl Write, val: i32) -> Result<(), std::io::Error> {
    let val_u16 = (val + 0x8000) as u16;
    writer.write_all(&val_u16.to_be_bytes())
}
