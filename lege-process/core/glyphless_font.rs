//! Minimal glyphless TrueType font for the invisible OCR/searchable text layer.
//!
//! The text layer is drawn with text-render-mode 3 (never rasterized) and its
//! Unicode is carried by a ToUnicode CMap, so the embedded font program needs
//! no real glyph outlines — it exists only to satisfy PDF's requirement that a
//! CIDFontType2 reference a font. This generates a ~1 KB font with two empty
//! glyphs (`.notdef` + one blank) instead of embedding a ~1 MB system font.
//!
//! This is the same approach OCRmyPDF / Tesseract use for their text layer.
//! `units_per_em` is 1000 so the descendant font's `DW 1000` matches.

/// Fixed metrics for the glyphless font (units_per_em = 1000).
pub const UNITS_PER_EM: u16 = 1000;
pub const ASCENT: i16 = 800;
pub const DESCENT: i16 = -200;
pub const CAP_HEIGHT: i16 = 700;

fn push_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_be_bytes());
}
fn push_i16(v: &mut Vec<u8>, x: i16) {
    v.extend_from_slice(&x.to_be_bytes());
}
fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_be_bytes());
}

/// TrueType table checksum: sum of the (0-padded to 4 bytes) contents as
/// big-endian u32, wrapping.
fn table_checksum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut i = 0;
    while i < data.len() {
        let mut word = [0u8; 4];
        for (k, w) in word.iter_mut().enumerate() {
            if i + k < data.len() {
                *w = data[i + k];
            }
        }
        sum = sum.wrapping_add(u32::from_be_bytes(word));
        i += 4;
    }
    sum
}

fn build_head() -> Vec<u8> {
    let mut t = Vec::new();
    push_u16(&mut t, 1); // majorVersion
    push_u16(&mut t, 0); // minorVersion
    push_u32(&mut t, 0x0001_0000); // fontRevision 1.0
    push_u32(&mut t, 0); // checkSumAdjustment (filled after assembly)
    push_u32(&mut t, 0x5F0F_3CF5); // magicNumber
    push_u16(&mut t, 0x000B); // flags
    push_u16(&mut t, UNITS_PER_EM);
    push_u32(&mut t, 0); // created (hi)
    push_u32(&mut t, 0); // created (lo)
    push_u32(&mut t, 0); // modified (hi)
    push_u32(&mut t, 0); // modified (lo)
    push_i16(&mut t, 0); // xMin
    push_i16(&mut t, DESCENT); // yMin
    push_i16(&mut t, 1000); // xMax
    push_i16(&mut t, ASCENT); // yMax
    push_u16(&mut t, 0); // macStyle
    push_u16(&mut t, 8); // lowestRecPPEM
    push_i16(&mut t, 2); // fontDirectionHint
    push_i16(&mut t, 0); // indexToLocFormat (0 = short)
    push_i16(&mut t, 0); // glyphDataFormat
    t
}

fn build_hhea() -> Vec<u8> {
    let mut t = Vec::new();
    push_u16(&mut t, 1); // majorVersion
    push_u16(&mut t, 0); // minorVersion
    push_i16(&mut t, ASCENT);
    push_i16(&mut t, DESCENT);
    push_i16(&mut t, 0); // lineGap
    push_u16(&mut t, 1000); // advanceWidthMax
    push_i16(&mut t, 0); // minLeftSideBearing
    push_i16(&mut t, 0); // minRightSideBearing
    push_i16(&mut t, 0); // xMaxExtent
    push_i16(&mut t, 1); // caretSlopeRise
    push_i16(&mut t, 0); // caretSlopeRun
    push_i16(&mut t, 0); // caretOffset
    for _ in 0..4 {
        push_i16(&mut t, 0); // reserved
    }
    push_i16(&mut t, 0); // metricDataFormat
    push_u16(&mut t, 1); // numberOfHMetrics
    t
}

fn build_maxp() -> Vec<u8> {
    let mut t = Vec::new();
    push_u32(&mut t, 0x0001_0000); // version 1.0
    push_u16(&mut t, 2); // numGlyphs (.notdef + one blank)
    for _ in 0..13 {
        push_u16(&mut t, 0); // all maxima 0 (empty glyphs)
    }
    t
}

fn build_hmtx() -> Vec<u8> {
    // numberOfHMetrics = 1: one longHorMetric, then (numGlyphs-1) leftSideBearings.
    let mut t = Vec::new();
    push_u16(&mut t, 1000); // advanceWidth (glyph 0)
    push_i16(&mut t, 0); // lsb (glyph 0)
    push_i16(&mut t, 0); // lsb (glyph 1)
    t
}

fn build_cmap() -> Vec<u8> {
    // One (3,1) subtable, format 4, mapping only 0xFFFF -> glyph 0 (a valid,
    // minimal segment map). PDF text bypasses this via CIDToGIDMap Identity.
    let mut sub = Vec::new();
    push_u16(&mut sub, 4); // format
    push_u16(&mut sub, 24); // length
    push_u16(&mut sub, 0); // language
    push_u16(&mut sub, 2); // segCountX2 (segCount = 1)
    push_u16(&mut sub, 2); // searchRange
    push_u16(&mut sub, 0); // entrySelector
    push_u16(&mut sub, 0); // rangeShift
    push_u16(&mut sub, 0xFFFF); // endCode[0]
    push_u16(&mut sub, 0); // reservedPad
    push_u16(&mut sub, 0xFFFF); // startCode[0]
    push_u16(&mut sub, 1); // idDelta[0]
    push_u16(&mut sub, 0); // idRangeOffset[0]

    let mut t = Vec::new();
    push_u16(&mut t, 0); // version
    push_u16(&mut t, 1); // numTables
    push_u16(&mut t, 3); // platformID (Windows)
    push_u16(&mut t, 1); // encodingID (Unicode BMP)
    push_u32(&mut t, 12); // offset to subtable (4 + 8)
    t.extend_from_slice(&sub);
    t
}

fn build_loca() -> Vec<u8> {
    // short format, numGlyphs + 1 = 3 offsets, all 0 (every glyph empty).
    let mut t = Vec::new();
    push_u16(&mut t, 0);
    push_u16(&mut t, 0);
    push_u16(&mut t, 0);
    t
}

fn build_glyf() -> Vec<u8> {
    // Both glyphs empty → zero-length glyf. Pad to 4 bytes so the table isn't
    // zero-length (some parsers reject a 0-byte table).
    vec![0, 0, 0, 0]
}

fn build_post() -> Vec<u8> {
    let mut t = Vec::new();
    push_u32(&mut t, 0x0003_0000); // version 3.0 (no glyph names)
    push_u32(&mut t, 0); // italicAngle
    push_i16(&mut t, -100); // underlinePosition
    push_i16(&mut t, 50); // underlineThickness
    push_u32(&mut t, 1); // isFixedPitch
    push_u32(&mut t, 0); // minMemType42
    push_u32(&mut t, 0); // maxMemType42
    push_u32(&mut t, 0); // minMemType1
    push_u32(&mut t, 0); // maxMemType1
    t
}

fn build_os2() -> Vec<u8> {
    // OS/2 version 4, minimal. Many viewers expect an OS/2 table on a Windows
    // (platform 3) font.
    let mut t = Vec::new();
    push_u16(&mut t, 4); // version
    push_i16(&mut t, 500); // xAvgCharWidth
    push_u16(&mut t, 400); // usWeightClass (Normal)
    push_u16(&mut t, 5); // usWidthClass (Medium)
    push_u16(&mut t, 0); // fsType (installable)
    for _ in 0..10 {
        push_i16(&mut t, 0); // subscript/superscript/strikeout (10 int16)
    }
    push_i16(&mut t, 0); // sFamilyClass
    // panose (10 bytes)
    t.extend_from_slice(&[0u8; 10]);
    for _ in 0..4 {
        push_u32(&mut t, 0); // ulUnicodeRange1..4
    }
    t.extend_from_slice(b"NONE"); // achVendID
    push_u16(&mut t, 0x0040); // fsSelection (REGULAR)
    push_u16(&mut t, 0); // usFirstCharIndex
    push_u16(&mut t, 0xFFFF); // usLastCharIndex
    push_i16(&mut t, ASCENT); // sTypoAscender
    push_i16(&mut t, DESCENT); // sTypoDescender
    push_i16(&mut t, 0); // sTypoLineGap
    push_u16(&mut t, ASCENT as u16); // usWinAscent
    push_u16(&mut t, (-DESCENT) as u16); // usWinDescent
    push_u32(&mut t, 0); // ulCodePageRange1
    push_u32(&mut t, 0); // ulCodePageRange2
    push_i16(&mut t, CAP_HEIGHT); // sxHeight (reuse)
    push_i16(&mut t, CAP_HEIGHT); // sCapHeight
    push_u16(&mut t, 0); // usDefaultChar
    push_u16(&mut t, 0); // usBreakChar
    push_u16(&mut t, 0); // usMaxContext
    t
}

fn build_name() -> Vec<u8> {
    // Mac platform (1,0,0) ASCII strings for a few standard name IDs.
    let name = b"Glyphless";
    let sub = b"Regular";
    let records: [(u16, &[u8]); 4] = [
        (1, name), // Family
        (2, sub),  // Subfamily
        (4, name), // Full name
        (6, name), // PostScript name
    ];

    // Storage: concatenate unique strings, track offsets.
    let mut storage = Vec::new();
    let name_off = storage.len() as u16;
    storage.extend_from_slice(name);
    let sub_off = storage.len() as u16;
    storage.extend_from_slice(sub);

    let count = records.len() as u16;
    let mut t = Vec::new();
    push_u16(&mut t, 0); // format 0
    push_u16(&mut t, count);
    let string_offset = 6 + count * 12; // header + records
    push_u16(&mut t, string_offset);
    for (id, s) in records {
        let (off, len) = if std::ptr::eq(s.as_ptr(), name.as_ptr()) {
            (name_off, name.len() as u16)
        } else {
            (sub_off, sub.len() as u16)
        };
        push_u16(&mut t, 1); // platformID (Macintosh)
        push_u16(&mut t, 0); // encodingID (Roman)
        push_u16(&mut t, 0); // languageID
        push_u16(&mut t, id); // nameID
        push_u16(&mut t, len);
        push_u16(&mut t, off);
    }
    t.extend_from_slice(&storage);
    t
}

/// Build a complete, self-contained glyphless TrueType font.
pub fn build_glyphless_ttf() -> Vec<u8> {
    // (tag, data) in the order we lay them out; the table directory is sorted
    // alphabetically by tag per the spec.
    let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"OS/2", build_os2()),
        (b"cmap", build_cmap()),
        (b"glyf", build_glyf()),
        (b"head", build_head()),
        (b"hhea", build_hhea()),
        (b"hmtx", build_hmtx()),
        (b"loca", build_loca()),
        (b"maxp", build_maxp()),
        (b"name", build_name()),
        (b"post", build_post()),
    ];
    tables.sort_by(|a, b| a.0.cmp(b.0));

    let num_tables = tables.len() as u16;
    // sfnt header (12) + directory (16 per table)
    let mut offset = 12 + 16 * num_tables as u32;

    // Compute per-table offsets (4-byte aligned) and checksums.
    struct Entry {
        tag: [u8; 4],
        checksum: u32,
        offset: u32,
        length: u32,
    }
    let mut entries = Vec::new();
    let mut head_dir_index = 0usize;
    for (i, (tag, data)) in tables.iter().enumerate() {
        if **tag == *b"head" {
            head_dir_index = i;
        }
        let length = data.len() as u32;
        entries.push(Entry {
            tag: **tag,
            checksum: table_checksum(data),
            offset,
            length,
        });
        // advance, padded to 4 bytes
        offset += length;
        offset = (offset + 3) & !3;
    }

    // Assemble the sfnt.
    let entry_selector = (15u16).saturating_sub((num_tables).leading_zeros() as u16);
    let search_range = (1u16 << entry_selector) * 16;
    let range_shift = num_tables * 16 - search_range;

    let mut font = Vec::new();
    push_u32(&mut font, 0x0001_0000); // sfnt version (TrueType)
    push_u16(&mut font, num_tables);
    push_u16(&mut font, search_range);
    push_u16(&mut font, entry_selector);
    push_u16(&mut font, range_shift);
    for e in &entries {
        font.extend_from_slice(&e.tag);
        push_u32(&mut font, e.checksum);
        push_u32(&mut font, e.offset);
        push_u32(&mut font, e.length);
    }
    for (_, data) in &tables {
        font.extend_from_slice(data);
        while font.len() % 4 != 0 {
            font.push(0);
        }
    }

    // head.checkSumAdjustment = 0xB1B0AFBA - checksum(whole font), with the
    // field itself treated as 0 (it currently is, since we wrote 0).
    let whole = table_checksum(&font);
    let adjustment = 0xB1B0_AFBAu32.wrapping_sub(whole);
    // checkSumAdjustment sits 8 bytes into the head table.
    let head_offset = entries[head_dir_index].offset as usize;
    let field = head_offset + 8;
    font[field..field + 4].copy_from_slice(&adjustment.to_be_bytes());

    font
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphless_font_is_small_and_parseable() {
        let font = build_glyphless_ttf();
        assert!(
            font.len() < 4096,
            "glyphless font unexpectedly large: {}",
            font.len()
        );
        // ttf-parser must accept it (same crate that reads system fonts).
        let face = ttf_parser::Face::parse(&font, 0).expect("glyphless font should parse");
        assert_eq!(face.number_of_glyphs(), 2);
        assert_eq!(face.units_per_em(), UNITS_PER_EM);
    }
}
