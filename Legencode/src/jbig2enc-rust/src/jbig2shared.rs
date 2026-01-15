//! Utility functions for the JBIG2 encoder

use crate::jbig2sym::BitImage;
use anyhow::Result;
use lopdf::{self, Dictionary, Object, ObjectId, Stream};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

// ==============================================
// Type conversion utilities
// ==============================================

/// Safely convert from u32 to usize with a panic if the value is too large.
#[inline]
pub fn u32_to_usize(x: u32) -> usize {
    x as usize
}

/// Safely convert from usize to u32 with a panic if the value is too large.
#[inline]
pub fn usize_to_u32(x: usize) -> u32 {
    u32::try_from(x).expect("value exceeds u32 range")
}

// ==============================================
// Debug utilities
// (Moved from jbig2sym.rs)
// ==============================================

/// Save a BitImage to a PBM file in the debug directory
/// Only saves files in debug builds (when not built with --release)
pub fn save_debug_pbm(image: &BitImage, filename: &str) -> std::io::Result<()> {
    if cfg!(debug_assertions) {
        let debug_dir = Path::new("debug-output");
        if !debug_dir.exists() {
            fs::create_dir_all(debug_dir)?;
        }

        let path = debug_dir.join(filename);
        let mut file = File::create(&path)?;

        // Write PBM header
        writeln!(&mut file, "P4\n{} {}\n", image.width, image.height)?;

        // Write image data
        file.write_all(&image.to_jbig2_format())?;
    }

    Ok(())
}

// ==============================================
// PDF-specific utilities
// ==============================================

/// Clamps a bounding box to ensure valid PDF coordinates
///
/// # Arguments
/// * `bbox` - Bounding box as [x_min, y_min, x_max, y_max]
pub fn clamp_pdf_bbox(bbox: [f32; 4]) -> [f32; 4] {
    let [x_min, y_min, x_max, y_max] = bbox;
    [
        x_min.max(0.0),
        y_min.max(0.0),
        x_max.max(0.0),
        y_max.max(0.0),
    ]
}

/// Gets an f32 value from a PDF dictionary with a fallback
///
/// # Arguments
/// * `dict` - The PDF dictionary
/// * `key` - The key to look up
/// * `default` - Default value if key is not found or invalid
pub fn get_f32(dict: &Dictionary, key: &[u8], default: f32) -> f32 {
    dict.get(key).and_then(Object::as_f32).unwrap_or(default)
}

/// Gets an f32 value from a JSON dictionary with a fallback
///
/// # Arguments
/// * `dict` - The JSON dictionary
/// * `key` - The key to look up
/// * `default` - Default value if key is not found or invalid
pub fn get_f32_from_json(
    dict: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    default: f32,
) -> f32 {
    match dict.get(key) {
        Some(serde_json::Value::Number(n)) => n.as_f64().map(|x| x as f32).unwrap_or(default),
        _ => default,
    }
}

/// Creates a PDF stream from binary data with compression
///
/// # Arguments
/// * `data` - The binary data
/// * `dict` - Additional dictionary entries
pub fn create_pdf_stream(data: Vec<u8>, mut dict: Dictionary) -> Result<Stream> {
    // Add compression filter if not already present
    if !dict.has(b"Filter") {
        dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
    }

    Ok(Stream::new(dict, data))
}

/// Creates a PDF object ID for a new object
pub fn new_object_id() -> ObjectId {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst) as u32;
    (id, 0)
}

/// Creates a PDF reference to an object
pub fn object_ref(id: ObjectId) -> Object {
    Object::Reference((id.0, id.1))
}

/// Creates a PDF array from a slice of objects
pub fn object_array<T: Into<Object> + Clone>(items: &[T]) -> Object {
    let vec = items.iter().map(|x| x.clone().into()).collect();
    Object::Array(vec)
}

/// Creates a PDF name object
pub fn name(s: &[u8]) -> Object {
    Object::Name(s.to_vec())
}

/// Creates a PDF string object
pub fn string(s: &str) -> Object {
    Object::string_literal(s)
}

/// Creates a PDF integer object
pub fn integer(i: i64) -> Object {
    Object::Integer(i)
}

/// Creates a PDF real number object
pub fn real(f: f32) -> Object {
    Object::Real(f)
}

/// Creates a PDF boolean object
pub fn boolean(b: bool) -> Object {
    Object::Boolean(b)
}

/// Creates a PDF null object
pub fn null() -> Object {
    Object::Null
}

/// Creates a PDF dictionary from key-value pairs
pub fn dict(pairs: &[(&[u8], Object)]) -> Dictionary {
    let mut dict = Dictionary::new();
    for (k, v) in pairs {
        dict.set(*k, v.clone());
    }
    dict
}

/// Creates a PDF date string in the format required by PDF
pub fn pdf_date() -> String {
    use chrono::offset::Utc;
    let now = Utc::now();
    now.format("D:%Y%m%d%H%M%SZ").to_string()
}

/// Creates a PDF info dictionary with common metadata
pub fn create_info_dict(
    title: Option<&str>,
    author: Option<&str>,
    subject: Option<&str>,
    keywords: Option<&[&str]>,
) -> Dictionary {
    let mut info = Dictionary::new();

    if let Some(t) = title {
        info.set("Title", Object::string_literal(t));
    }
    if let Some(a) = author {
        info.set("Author", Object::string_literal(a));
    }
    if let Some(s) = subject {
        info.set("Subject", Object::string_literal(s));
    }
    if let Some(kws) = keywords {
        let kw_array = kws
            .iter()
            .map(|&kw| Object::string_literal(kw))
            .collect::<Vec<_>>();
        info.set("Keywords", Object::Array(kw_array));
    }

    info.set("Creator", Object::string_literal("jbig2enc-rust"));
    info.set("CreationDate", Object::string_literal(&*pdf_date()));
    info.set("ModDate", Object::string_literal(&*pdf_date()));

    info
}

pub mod jbig2wrapper {
    

    pub fn push_file_header(out: &mut Vec<u8>) {
        out.extend_from_slice(&[0x97, 0x4A, 0x42, 0x32, 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    pub fn push_page_info(out: &mut Vec<u8>, width: u32, height: u32) {
        // Segment header for Page Information Segment (Section 7.4.1)
        // Segment number (arbitrary, but 1 for first page info)
        out.extend_from_slice(&0u32.to_be_bytes());
        // Page Information Segment type (0x00)
        out.push(0x00);
        // Page Information Segment flags (Section 7.4.1.1)
        // Bit 7: Default Pixel Value (0 = black, 1 = white) - set to 1 for white
        // Bit 6: Page Striping (0 = no striping, 1 = striping) - set to 0
        // Bits 5-0: Page X-Resolution and Y-Resolution (0 = no resolution specified)
        out.push(0b10000000); // Flags1: DP=1, PS=0, R=0
        out.push(0x00); // Flags2: Reserved, set to 0

        // Page width and height
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());

        // X and Y resolution (0 = no resolution specified)
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());

        // Page segments (number of segments associated with this page)
        // For a single page with one generic region and EOF, this is 2
        out.extend_from_slice(&2u32.to_be_bytes());
    }

    pub fn push_eof(out: &mut Vec<u8>, segment_number: u32) {
        // Segment header for End of File Segment (Section 7.4.2)
        out.extend_from_slice(&segment_number.to_be_bytes());
        out.push(0x02); // End of File Segment type (0x02)
        out.extend_from_slice(&0u16.to_be_bytes()); // Flags: Reserved, set to 0
        out.extend_from_slice(&0u32.to_be_bytes()); // Segment page association: 0 for global
        out.extend_from_slice(&0u32.to_be_bytes()); // Segment data length: 0 for EOF
    }
}
