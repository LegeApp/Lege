//! Embedded font object graph: FontFile2 stream, FontDescriptor,
//! CIDFontType2 (Identity CIDToGIDMap, DW 1000), Type0 Identity-H,
//! identity ToUnicode CMap. Byte-parity with accumulator.rs behavior.
//!
//! The OCR text layer is invisible (render mode 3), so glyph shapes never
//! matter. Text is encoded as UTF-16BE code units under Identity-H (CID = code
//! unit) and mapped back to Unicode by an identity ToUnicode CMap for
//! extraction/search. The program is embedded whole (already a ~1 KB glyphless
//! font upstream), so there is nothing to subset.

use std::io::Write;
use std::sync::Arc;

use crate::serialize::{write_i64, write_name, write_ref, write_u64};
use crate::sink::{PdfSink, StreamBody};
use crate::types::{ObjectId, Result};

/// The font program plus the descriptor metrics needed to embed it. Supplied by
/// Lege (from `unicode_font::UnicodeFontData`); the writer does no font
/// parsing.
#[derive(Clone)]
pub struct EmbeddedFont {
    pub data: Arc<[u8]>,
    pub post_script_name: String,
    pub ascent: i32,
    pub descent: i32,
    pub cap_height: i32,
    pub italic_angle: f32,
    /// FontBBox as `[x_min, y_min, x_max, y_max]`.
    pub bbox: [i32; 4],
}

/// Write the five-object font graph and return the Type0 font object id (the
/// one referenced from a page's `/Font` resources as `/F0`).
pub fn write_embedded_font<W: Write>(
    sink: &mut PdfSink<W>,
    font: &EmbeddedFont,
) -> Result<ObjectId> {
    let name = to_pdf_name(&font.post_script_name);

    // FontFile2 stream: << /Length1 N /Length bodylen >> body = program bytes.
    // (No /Subtype — that is only valid on FontFile3.)
    let font_file_id = sink.alloc_id();
    let mut ff_dict = Vec::new();
    ff_dict.extend_from_slice(b"<</Length1 ");
    write_u64(&mut ff_dict, font.data.len() as u64);
    ff_dict.push(b' ');
    ff_dict.extend_from_slice(b"/Length ");
    write_u64(&mut ff_dict, font.data.len() as u64);
    ff_dict.extend_from_slice(b">>");
    sink.write_stream(
        font_file_id,
        &ff_dict,
        &StreamBody::Shared(font.data.clone()),
    )?;

    // FontDescriptor.
    let mut flags = 32; // Nonsymbolic
    if font.italic_angle.abs() > f32::EPSILON {
        flags |= 64; // Italic
    }
    let descriptor_id = sink.alloc_id();
    let mut d = Vec::new();
    d.extend_from_slice(b"<<");
    kv_name(&mut d, b"Type", b"FontDescriptor");
    key(&mut d, b"FontName");
    write_name(&mut d, &name);
    kv_int(&mut d, b"Ascent", font.ascent as i64);
    kv_int(&mut d, b"Descent", font.descent as i64);
    kv_int(&mut d, b"CapHeight", font.cap_height as i64);
    key(&mut d, b"ItalicAngle");
    crate::serialize::write_real(&mut d, font.italic_angle as f64);
    kv_int(&mut d, b"Flags", flags);
    kv_int(&mut d, b"StemV", 80);
    key(&mut d, b"FontBBox");
    d.push(b'[');
    for (i, v) in font.bbox.iter().enumerate() {
        if i > 0 {
            d.push(b' ');
        }
        write_i64(&mut d, *v as i64);
    }
    d.push(b']');
    key(&mut d, b"FontFile2");
    write_ref(&mut d, font_file_id);
    d.extend_from_slice(b">>");
    sink.write_indirect(descriptor_id, &d)?;

    // CIDFontType2 descendant.
    let cid_font_id = sink.alloc_id();
    let mut c = Vec::new();
    c.extend_from_slice(b"<<");
    kv_name(&mut c, b"Type", b"Font");
    kv_name(&mut c, b"Subtype", b"CIDFontType2");
    key(&mut c, b"BaseFont");
    write_name(&mut c, &name);
    key(&mut c, b"CIDSystemInfo");
    c.extend_from_slice(b"<<");
    key(&mut c, b"Registry");
    crate::serialize::write_literal_string(&mut c, b"Adobe");
    key(&mut c, b"Ordering");
    crate::serialize::write_literal_string(&mut c, b"Identity");
    kv_int(&mut c, b"Supplement", 0);
    c.extend_from_slice(b">>");
    key(&mut c, b"FontDescriptor");
    write_ref(&mut c, descriptor_id);
    kv_name(&mut c, b"CIDToGIDMap", b"Identity");
    kv_int(&mut c, b"DW", 1000);
    c.extend_from_slice(b">>");
    sink.write_indirect(cid_font_id, &c)?;

    // ToUnicode CMap stream.
    let to_unicode_id = sink.alloc_id();
    let cmap = identity_to_unicode_cmap();
    let mut tdict = Vec::new();
    tdict.extend_from_slice(b"<</Length ");
    write_u64(&mut tdict, cmap.len() as u64);
    tdict.extend_from_slice(b">>");
    sink.write_stream(to_unicode_id, &tdict, &StreamBody::Owned(cmap))?;

    // Type0 parent font.
    let type0_id = sink.alloc_id();
    let mut t = Vec::new();
    t.extend_from_slice(b"<<");
    kv_name(&mut t, b"Type", b"Font");
    kv_name(&mut t, b"Subtype", b"Type0");
    key(&mut t, b"BaseFont");
    write_name(&mut t, &name);
    kv_name(&mut t, b"Encoding", b"Identity-H");
    key(&mut t, b"DescendantFonts");
    t.push(b'[');
    write_ref(&mut t, cid_font_id);
    t.push(b']');
    key(&mut t, b"ToUnicode");
    write_ref(&mut t, to_unicode_id);
    t.extend_from_slice(b">>");
    sink.write_indirect(type0_id, &t)?;

    Ok(type0_id)
}

/// Write the non-embedded Helvetica fallback font as an inline dictionary body
/// and return its object id.
pub fn write_helvetica<W: Write>(sink: &mut PdfSink<W>) -> Result<ObjectId> {
    let id = sink.alloc_id();
    let mut d = Vec::new();
    d.extend_from_slice(b"<<");
    kv_name(&mut d, b"Type", b"Font");
    kv_name(&mut d, b"Subtype", b"Type1");
    kv_name(&mut d, b"BaseFont", b"Helvetica");
    d.extend_from_slice(b">>");
    sink.write_indirect(id, &d)?;
    Ok(id)
}

/// Sanitize a PostScript name to PDF name bytes, mapping anything outside the
/// safe printable ASCII range to `_` (parity with the current writer).
pub fn to_pdf_name(name: &str) -> Vec<u8> {
    if name.is_empty() {
        return b"EmbeddedUnicode".to_vec();
    }
    let mut bytes = Vec::with_capacity(name.len());
    for ch in name.chars() {
        let b = ch as u32;
        let valid = ch.is_ascii()
            && (ch as u8) >= b'!'
            && (ch as u8) <= b'~'
            && !matches!(
                ch,
                '#' | '%' | '(' | ')' | '<' | '>' | '[' | ']' | '{' | '}' | '/'
            );
        if valid && b <= u8::MAX as u32 {
            bytes.push(ch as u8);
        } else {
            bytes.push(b'_');
        }
    }
    if bytes.is_empty() {
        b"EmbeddedUnicode".to_vec()
    } else {
        bytes
    }
}

/// The identity ToUnicode CMap: CID == UTF-16 code unit == Unicode scalar for
/// the BMP, so extraction recovers the original text.
pub fn identity_to_unicode_cmap() -> Vec<u8> {
    const CMAP: &str = concat!(
        "/CIDInit /ProcSet findresource begin\n",
        "12 dict begin\n",
        "begincmap\n",
        "/CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> def\n",
        "/CMapName /Adobe-Identity-UCS def\n",
        "/CMapType 2 def\n",
        "1 begincodespacerange\n",
        "<0000> <FFFF>\n",
        "endcodespacerange\n",
        "1 beginbfrange\n",
        "<0000> <FFFF> <0000>\n",
        "endbfrange\n",
        "endcmap\n",
        "CMapName currentdict /CMap defineresource pop\n",
        "end\n",
        "end\n",
    );
    CMAP.as_bytes().to_vec()
}

fn key(d: &mut Vec<u8>, k: &[u8]) {
    write_name(d, k);
    d.push(b' ');
}

fn kv_name(d: &mut Vec<u8>, k: &[u8], v: &[u8]) {
    key(d, k);
    write_name(d, v);
}

fn kv_int(d: &mut Vec<u8>, k: &[u8], v: i64) {
    key(d, k);
    write_i64(d, v);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_font() -> EmbeddedFont {
        EmbeddedFont {
            data: Arc::from(&[0u8; 32][..]),
            post_script_name: "Glyphless".to_string(),
            ascent: 1000,
            descent: -200,
            cap_height: 700,
            italic_angle: 0.0,
            bbox: [-100, -200, 1000, 900],
        }
    }

    #[test]
    fn font_graph_shape_and_order() {
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let type0 = write_embedded_font(&mut sink, &sample_font()).unwrap();
        let t = String::from_utf8_lossy(&sink.finish().unwrap()).into_owned();

        assert!(t.contains("/Length1 32"));
        assert!(t.contains("/Type /FontDescriptor"));
        assert!(t.contains("/FontName /Glyphless"));
        assert!(t.contains("/Flags 32"));
        assert!(t.contains("/StemV 80"));
        assert!(t.contains("/FontBBox [-100 -200 1000 900]"));
        assert!(t.contains("/Subtype /CIDFontType2"));
        assert!(t.contains("/CIDToGIDMap /Identity"));
        assert!(t.contains("/DW 1000"));
        assert!(t.contains("/Subtype /Type0"));
        assert!(t.contains("/Encoding /Identity-H"));
        assert!(t.contains("/Registry (Adobe)"));
        assert!(t.contains("Adobe-Identity-UCS"));
        // Type0 is the last of the five objects.
        assert_eq!(type0.num, 5, "type0 should be the fifth object");
    }

    #[test]
    fn italic_sets_flag() {
        let mut f = sample_font();
        f.italic_angle = -12.0;
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        write_embedded_font(&mut sink, &f).unwrap();
        let t = String::from_utf8_lossy(&sink.finish().unwrap()).into_owned();
        assert!(t.contains("/Flags 96"), "32|64 = 96: {t}");
    }

    #[test]
    fn name_sanitization() {
        assert_eq!(to_pdf_name(""), b"EmbeddedUnicode");
        assert_eq!(to_pdf_name("ABC+Font"), b"ABC+Font");
        assert_eq!(to_pdf_name("bad name/x"), b"bad_name_x");
    }
}
