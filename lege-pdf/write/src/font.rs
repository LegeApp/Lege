//! Embedded font object graph: FontFile2 stream, FontDescriptor,
//! CIDFontType2 (Identity CIDToGIDMap, DW 1000), Type0 Identity-H,
//! identity ToUnicode CMap. Byte-parity with accumulator.rs behavior.
//!
//! The OCR text layer is invisible (render mode 3), so glyph shapes never
//! matter. Text is encoded as UTF-16BE code units under Identity-H (CID = code
//! unit) and mapped back to Unicode by an identity ToUnicode CMap for
//! extraction/search. The program is embedded whole (already a ~1 KB glyphless
//! font upstream), so there is nothing to subset.
//!
//! The same graph also carries the visible glyph font (raster text clustered
//! into a per-document TrueType program). That font is symbolic, has no
//! ToUnicode (glyph ids are not characters), and supplies per-CID advance
//! widths through `/W`, which is what a viewer uses to advance the pen.

use std::io::Write;
use std::sync::Arc;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use crate::serialize::{write_i64, write_name, write_ref, write_u64};
use crate::sink::{PdfSink, StreamBody};
use crate::types::{ObjectId, Result};

/// The font program plus the descriptor metrics needed to embed it. Supplied by
/// Lege (from `unicode_font::UnicodeFontData`); the writer does no font
/// parsing.
#[derive(Clone, Debug)]
pub struct EmbeddedFont {
    pub data: Arc<[u8]>,
    pub post_script_name: String,
    pub ascent: i32,
    pub descent: i32,
    pub cap_height: i32,
    pub italic_angle: f32,
    /// FontBBox as `[x_min, y_min, x_max, y_max]`.
    pub bbox: [i32; 4],
    /// Symbolic font (flag bit 3) rather than Nonsymbolic (bit 6). Glyph
    /// fonts are symbolic: their glyph ids carry no character meaning.
    pub symbolic: bool,
    /// The ToUnicode CMap to attach: identity for the OCR layer (CID = code
    /// unit), a supplied CMap for a glyph font whose ids were aligned to
    /// recognized text, or none when the ids carry no character meaning.
    pub to_unicode: ToUnicode,
    /// Per-CID advance widths in 1/1000 text-space units, indexed from CID 0,
    /// written as a single `/W [0 [...]]` run. `None` leaves `/DW 1000` alone.
    pub cid_widths: Option<Arc<[u16]>>,
    /// Deflate the font program (`/Filter /FlateDecode` on FontFile2). Off for
    /// the ~1 KB glyphless font, on for glyph fonts whose outline tables
    /// compress several times over.
    pub compress_program: bool,
}

/// How a font's codes map back to Unicode for text extraction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToUnicode {
    /// No CMap: the codes are shapes, not characters.
    None,
    /// CID == UTF-16 code unit, the OCR layer's convention.
    Identity,
    /// A complete CMap stream body, as built by [`to_unicode_cmap`].
    Custom(Arc<[u8]>),
}

impl EmbeddedFont {
    /// The OCR-layer shape: nonsymbolic, identity ToUnicode, default widths.
    pub fn glyphless(
        data: Arc<[u8]>,
        post_script_name: String,
        ascent: i32,
        descent: i32,
        cap_height: i32,
        italic_angle: f32,
        bbox: [i32; 4],
    ) -> Self {
        Self {
            data,
            post_script_name,
            ascent,
            descent,
            cap_height,
            italic_angle,
            bbox,
            symbolic: false,
            to_unicode: ToUnicode::Identity,
            cid_widths: None,
            compress_program: false,
        }
    }
}

/// Write the five-object font graph and return the Type0 font object id (the
/// one referenced from a page's `/Font` resources as `/F0`).
pub fn write_embedded_font<W: Write>(
    sink: &mut PdfSink<W>,
    font: &EmbeddedFont,
) -> Result<ObjectId> {
    write_font_graph(sink, font, None)
}

/// Write the font graph with a pre-reserved Type0 object id. This is how the
/// document-wide glyph font is emitted: pages reference the id as they arrive
/// and the program itself is written at finalization, once every page has
/// contributed its glyphs.
pub fn write_embedded_font_at<W: Write>(
    sink: &mut PdfSink<W>,
    font: &EmbeddedFont,
    type0_id: ObjectId,
) -> Result<()> {
    write_font_graph(sink, font, Some(type0_id)).map(|_| ())
}

/// The shared graph writer. With `reserved` the Type0 object takes that id;
/// otherwise it is allocated last, so arrival-order numbering is unchanged.
fn write_font_graph<W: Write>(
    sink: &mut PdfSink<W>,
    font: &EmbeddedFont,
    reserved: Option<ObjectId>,
) -> Result<ObjectId> {
    let name = to_pdf_name(&font.post_script_name);

    // FontFile2 stream: << /Length1 N /Length bodylen >> body = program bytes,
    // optionally deflated. (No /Subtype — that is only valid on FontFile3.)
    let font_file_id = sink.alloc_id();
    let body = if font.compress_program {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::best());
        enc.write_all(&font.data)?;
        StreamBody::Owned(enc.finish()?)
    } else {
        StreamBody::Shared(font.data.clone())
    };
    let mut ff_dict = Vec::new();
    ff_dict.extend_from_slice(b"<</Length1 ");
    write_u64(&mut ff_dict, font.data.len() as u64);
    if font.compress_program {
        ff_dict.extend_from_slice(b" /Filter /FlateDecode");
    }
    ff_dict.push(b' ');
    ff_dict.extend_from_slice(b"/Length ");
    write_u64(&mut ff_dict, body.len() as u64);
    ff_dict.extend_from_slice(b">>");
    sink.write_stream(font_file_id, &ff_dict, &body)?;

    // FontDescriptor.
    let mut flags = if font.symbolic { 4 } else { 32 }; // Symbolic / Nonsymbolic
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
    if let Some(widths) = font.cid_widths.as_deref().filter(|w| !w.is_empty()) {
        key(&mut c, b"W");
        c.extend_from_slice(b"[0 [");
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                c.push(b' ');
            }
            write_i64(&mut c, *w as i64);
        }
        c.extend_from_slice(b"]]");
    }
    c.extend_from_slice(b">>");
    sink.write_indirect(cid_font_id, &c)?;

    // ToUnicode CMap stream.
    let to_unicode_id = match &font.to_unicode {
        ToUnicode::None => None,
        ToUnicode::Identity => Some(write_cmap_stream(sink, &identity_to_unicode_cmap())?),
        ToUnicode::Custom(cmap) => Some(write_cmap_stream(sink, cmap)?),
    };

    // Type0 parent font.
    let type0_id = reserved.unwrap_or_else(|| sink.alloc_id());
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
    if let Some(to_unicode_id) = to_unicode_id {
        key(&mut t, b"ToUnicode");
        write_ref(&mut t, to_unicode_id);
    }
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

/// Write a CMap as a deflated stream: a glyph font's `bfchar` list is one
/// line per glyph and compresses several times over.
fn write_cmap_stream<W: Write>(sink: &mut PdfSink<W>, cmap: &[u8]) -> Result<ObjectId> {
    let id = sink.alloc_id();
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::best());
    enc.write_all(cmap)?;
    let body = StreamBody::Owned(enc.finish()?);
    let mut tdict = Vec::new();
    tdict.extend_from_slice(b"<</Filter /FlateDecode /Length ");
    write_u64(&mut tdict, body.len() as u64);
    tdict.extend_from_slice(b">>");
    sink.write_stream(id, &tdict, &body)?;
    Ok(id)
}

const CMAP_HEADER: &str = concat!(
    "/CIDInit /ProcSet findresource begin\n",
    "12 dict begin\n",
    "begincmap\n",
    "/CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> def\n",
    "/CMapName /Adobe-Identity-UCS def\n",
    "/CMapType 2 def\n",
    "1 begincodespacerange\n",
    "<0000> <FFFF>\n",
    "endcodespacerange\n",
);
const CMAP_FOOTER: &str = concat!(
    "endcmap\n",
    "CMapName currentdict /CMap defineresource pop\n",
    "end\n",
    "end\n",
);

/// A ToUnicode CMap mapping two-byte codes to strings, one `bfchar` entry
/// per code (in blocks of at most 100, as the CMap spec requires). An empty
/// string maps the code to nothing, which is how a glyph that is only part
/// of a character (the dot of an `i`) stays out of extracted text.
pub fn to_unicode_cmap(entries: &[(u16, String)]) -> Vec<u8> {
    let mut out = String::with_capacity(CMAP_HEADER.len() + CMAP_FOOTER.len() + entries.len() * 24);
    out.push_str(CMAP_HEADER);
    for block in entries.chunks(100) {
        out.push_str(&format!("{} beginbfchar\n", block.len()));
        for (code, text) in block {
            out.push_str(&format!("<{code:04X}> <"));
            for unit in text.encode_utf16() {
                out.push_str(&format!("{unit:04X}"));
            }
            out.push_str(">\n");
        }
        out.push_str("endbfchar\n");
    }
    out.push_str(CMAP_FOOTER);
    out.into_bytes()
}

/// The identity ToUnicode CMap: CID == UTF-16 code unit == Unicode scalar for
/// the BMP, so extraction recovers the original text.
pub fn identity_to_unicode_cmap() -> Vec<u8> {
    let mut out = String::from(CMAP_HEADER);
    out.push_str("1 beginbfrange\n<0000> <FFFF> <0000>\nendbfrange\n");
    out.push_str(CMAP_FOOTER);
    out.into_bytes()
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
        EmbeddedFont::glyphless(
            Arc::from(&[0u8; 32][..]),
            "Glyphless".to_string(),
            1000,
            -200,
            700,
            0.0,
            [-100, -200, 1000, 900],
        )
    }

    #[test]
    fn glyph_font_is_symbolic_with_widths_and_no_tounicode() {
        let mut f = sample_font();
        f.symbolic = true;
        f.to_unicode = ToUnicode::None;
        f.cid_widths = Some(Arc::from(&[0u16, 250, 1000][..]));
        f.compress_program = true;
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        let reserved = sink.alloc_id();
        write_embedded_font_at(&mut sink, &f, reserved).unwrap();
        let t = String::from_utf8_lossy(&sink.finish().unwrap()).into_owned();
        assert!(t.contains("/Flags 4"), "{t}");
        assert!(
            t.contains("/Length1 32 /Filter /FlateDecode /Length "),
            "{t}"
        );
        assert!(t.contains("/W [0 [0 250 1000]]"), "{t}");
        assert!(!t.contains("ToUnicode"), "{t}");
        assert!(
            t.contains("1 0 obj\n<</Type /Font/Subtype /Type0"),
            "reserved id used: {t}"
        );
    }

    #[test]
    fn custom_to_unicode_cmap_is_attached_deflated() {
        use std::io::Read;
        let mut f = sample_font();
        f.symbolic = true;
        f.to_unicode = ToUnicode::Custom(to_unicode_cmap(&[(1, "i".into())]).into());
        let mut sink = PdfSink::new(Vec::new(), "1.7").unwrap();
        write_embedded_font(&mut sink, &f).unwrap();
        let bytes = sink.finish().unwrap();
        let t = String::from_utf8_lossy(&bytes).into_owned();
        assert!(t.contains("/ToUnicode "), "{t}");
        assert!(t.contains("<</Filter /FlateDecode /Length "), "{t}");
        // The CMap is the last stream written (just before the Type0 object).
        let start = bytes.windows(8).rposition(|w| w == b"\nstream\n").unwrap() + 8;
        let end = start
            + bytes[start..]
                .windows(10)
                .position(|w| w == b"\nendstream")
                .unwrap();
        let mut inflated = String::new();
        flate2::read::ZlibDecoder::new(&bytes[start..end])
            .read_to_string(&mut inflated)
            .unwrap();
        assert!(
            inflated.contains("1 beginbfchar\n<0001> <0069>\nendbfchar\n"),
            "{inflated}"
        );
    }

    #[test]
    fn bfchar_cmap_blocks_entries_and_encodes_utf16() {
        let entries: Vec<(u16, String)> = (1..=101u16)
            .map(|code| {
                let text = match code {
                    1 => String::new(),
                    2 => "fi".to_string(),
                    3 => "\u{1F600}".to_string(),
                    _ => "a".to_string(),
                };
                (code, text)
            })
            .collect();
        let cmap = String::from_utf8(to_unicode_cmap(&entries)).unwrap();
        assert!(
            cmap.contains("100 beginbfchar\n<0001> <>\n<0002> <00660069>\n<0003> <D83DDE00>\n"),
            "{cmap}"
        );
        assert!(
            cmap.contains("endbfchar\n1 beginbfchar\n<0065> <0061>\nendbfchar\nendcmap\n"),
            "{cmap}"
        );
        assert_eq!(cmap.matches("beginbfchar").count(), 2);
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
        assert!(t.contains("/ToUnicode "));
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
