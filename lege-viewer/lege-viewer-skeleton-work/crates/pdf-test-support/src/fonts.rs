//! Self-authored minimal fonts for tests — no bundled or licensed font data.
//!
//! [`minimal_ttf`] builds a tiny valid TrueType font (units/em 1000) with glyph
//! 0 = empty `.notdef` and glyph 1 = a **triangle** `(100,50)-(900,50)-(500,650)`.
//! A triangle (not a square) lets a test distinguish a rasterized outline from a
//! synthetic placement box: the outline leaves the glyph's bounding-box corners
//! empty.

fn be16(v: i32) -> [u8; 2] {
    (v as u16).to_be_bytes()
}

/// Build a minimal TrueType font with one triangle glyph.
pub fn minimal_ttf() -> Vec<u8> {
    minimal_ttf_named(None)
}

/// Build a `name` table (format 0) naming `family`, so the font can be
/// indexed the way a system font provider indexes real fonts.
fn name_table(family: &str) -> Vec<u8> {
    // (nameID, value): family, subfamily, full name, PostScript name.
    let entries: [(u16, String); 4] = [
        (1, family.to_string()),
        (2, "Regular".to_string()),
        (4, family.to_string()),
        (6, family.replace(' ', "")),
    ];
    let mut storage = Vec::new();
    let mut records = Vec::new();
    for (id, value) in &entries {
        let utf16: Vec<u8> = value.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        records.extend(3u16.to_be_bytes()); // platformID: Windows
        records.extend(1u16.to_be_bytes()); // encodingID: Unicode BMP
        records.extend(0x0409u16.to_be_bytes()); // languageID: en-US
        records.extend(id.to_be_bytes());
        records.extend((utf16.len() as u16).to_be_bytes());
        records.extend((storage.len() as u16).to_be_bytes());
        storage.extend(utf16);
    }
    let mut name = Vec::new();
    name.extend(0u16.to_be_bytes()); // format 0
    name.extend((entries.len() as u16).to_be_bytes());
    name.extend(((6 + entries.len() * 12) as u16).to_be_bytes()); // stringOffset
    name.extend(records);
    name.extend(storage);
    name
}

/// [`minimal_ttf`], optionally carrying a `name` table declaring `family`.
///
/// A font with no `name` table cannot be identified by family, so a system
/// font provider skips it — exactly as it would a corrupt file.
pub fn minimal_ttf_named(family: Option<&str>) -> Vec<u8> {
    minimal_ttf_inner(family, false)
}

/// [`minimal_ttf`] whose ONLY cmap is a Macintosh (1,0) format-6 subtable
/// mapping code 0x41 → GID 1 — the shape of Office-produced TrueType
/// subsets whose Unicode tables were stripped. Skrifa's `Charmap` cannot
/// read it; the engine's native Mac-cmap fallback must.
pub fn minimal_ttf_mac_cmap_only() -> Vec<u8> {
    minimal_ttf_inner(None, true)
}

fn minimal_ttf_inner(family: Option<&str>, mac_cmap_only: bool) -> Vec<u8> {
    // glyf: one simple 3-point triangle, i16 coordinate deltas.
    let mut glyf = Vec::new();
    glyf.extend(be16(1)); // numberOfContours
    glyf.extend(be16(100)); // xMin
    glyf.extend(be16(50)); // yMin
    glyf.extend(be16(900)); // xMax
    glyf.extend(be16(650)); // yMax
    glyf.extend(be16(2)); // endPtsOfContours[0] (3 points → last index 2)
    glyf.extend(be16(0)); // instructionLength
    glyf.extend([0x01, 0x01, 0x01]); // flags: 3 on-curve points
    for dx in [100, 800, -400] {
        glyf.extend(be16(dx)); // x deltas
    }
    for dy in [50, 0, 600] {
        glyf.extend(be16(dy)); // y deltas
    }
    while glyf.len() % 2 != 0 {
        glyf.push(0); // loca (short) needs even glyph lengths
    }

    // loca (short format): offsets/2 for glyphs 0, 1, and the end sentinel.
    let mut loca = Vec::new();
    loca.extend(be16(0)); // glyph 0 (empty)
    loca.extend(be16(0)); // glyph 1
    loca.extend(be16(glyf.len() as i32 / 2)); // end

    // head (54 bytes).
    let mut head = Vec::new();
    head.extend(0x0001_0000u32.to_be_bytes());
    head.extend(0x0001_0000u32.to_be_bytes());
    head.extend(0u32.to_be_bytes()); // checkSumAdjustment
    head.extend(0x5F0F_3CF5u32.to_be_bytes()); // magicNumber
    head.extend(be16(0)); // flags
    head.extend(1000u16.to_be_bytes()); // unitsPerEm
    head.extend(0i64.to_be_bytes()); // created
    head.extend(0i64.to_be_bytes()); // modified
    head.extend(be16(0)); // xMin
    head.extend(be16(0)); // yMin
    head.extend(be16(1000)); // xMax
    head.extend(be16(1000)); // yMax
    head.extend(be16(0)); // macStyle
    head.extend(8u16.to_be_bytes()); // lowestRecPPEM
    head.extend(be16(2)); // fontDirectionHint
    head.extend(be16(0)); // indexToLocFormat (short)
    head.extend(be16(0)); // glyphDataFormat

    // maxp v1.0 (32 bytes).
    let mut maxp = Vec::new();
    maxp.extend(0x0001_0000u32.to_be_bytes());
    maxp.extend(2u16.to_be_bytes()); // numGlyphs
    maxp.extend(3u16.to_be_bytes()); // maxPoints
    maxp.extend(1u16.to_be_bytes()); // maxContours
    maxp.extend([0u8; 22]);

    // hhea (36 bytes) + hmtx (2 metrics).
    let mut hhea = Vec::new();
    hhea.extend(0x0001_0000u32.to_be_bytes());
    hhea.extend(be16(800)); // ascender
    hhea.extend(be16(-200)); // descender
    hhea.extend(be16(0)); // lineGap
    hhea.extend(1000u16.to_be_bytes()); // advanceWidthMax
    hhea.extend([0u8; 22]);
    hhea.extend(2u16.to_be_bytes()); // numberOfHMetrics
    let mut hmtx = Vec::new();
    for _ in 0..2 {
        hmtx.extend(1000u16.to_be_bytes());
        hmtx.extend(be16(100));
    }

    // cmap: a (3,1) Unicode format-4 subtable mapping 'A' (0x41) → GID 1 (one
    // segment plus the required 0xFFFF terminator), so simple-font code→GID
    // resolution (via the encoding + cmap) has a target.
    let mut sub = Vec::new();
    sub.extend(4u16.to_be_bytes()); // format
    sub.extend(32u16.to_be_bytes()); // length
    sub.extend(0u16.to_be_bytes()); // language
    sub.extend(4u16.to_be_bytes()); // segCountX2 (2 segments)
    sub.extend(4u16.to_be_bytes()); // searchRange
    sub.extend(1u16.to_be_bytes()); // entrySelector
    sub.extend(0u16.to_be_bytes()); // rangeShift
    sub.extend(0x0041u16.to_be_bytes()); // endCode[0]
    sub.extend(0xFFFFu16.to_be_bytes()); // endCode[1] terminator
    sub.extend(0u16.to_be_bytes()); // reservedPad
    sub.extend(0x0041u16.to_be_bytes()); // startCode[0]
    sub.extend(0xFFFFu16.to_be_bytes()); // startCode[1]
    sub.extend(65472u16.to_be_bytes()); // idDelta[0] = -64 → 0x41 + (-64) = 1
    sub.extend(1u16.to_be_bytes()); // idDelta[1]
    sub.extend(0u16.to_be_bytes()); // idRangeOffset[0]
    sub.extend(0u16.to_be_bytes()); // idRangeOffset[1]
    let sub = if mac_cmap_only {
        // Macintosh (1,0) format 6: first=0x41, one entry → GID 1.
        let mut mac = Vec::new();
        mac.extend(6u16.to_be_bytes()); // format
        mac.extend(12u16.to_be_bytes()); // length
        mac.extend(0u16.to_be_bytes()); // language
        mac.extend(0x41u16.to_be_bytes()); // firstCode
        mac.extend(1u16.to_be_bytes()); // entryCount
        mac.extend(1u16.to_be_bytes()); // glyphIdArray[0]
        mac
    } else {
        sub
    };
    let mut cmap = Vec::new();
    cmap.extend(0u16.to_be_bytes()); // version
    cmap.extend(1u16.to_be_bytes()); // numTables
    if mac_cmap_only {
        cmap.extend(1u16.to_be_bytes()); // platformID (Macintosh)
        cmap.extend(0u16.to_be_bytes()); // encodingID (Roman)
    } else {
        cmap.extend(3u16.to_be_bytes()); // platformID (Windows)
        cmap.extend(1u16.to_be_bytes()); // encodingID (Unicode BMP)
    }
    cmap.extend(12u32.to_be_bytes()); // subtable offset (after the 12-byte header)
    cmap.extend(sub);

    let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"cmap", cmap),
        (b"glyf", glyf),
        (b"head", head),
        (b"hhea", hhea),
        (b"hmtx", hmtx),
        (b"loca", loca),
        (b"maxp", maxp),
    ];
    if let Some(family) = family {
        tables.push((b"name", name_table(family)));
        // The table directory must stay sorted by tag.
        tables.sort_by(|a, b| a.0.cmp(b.0));
    }
    let n = tables.len();
    let mut out = Vec::new();
    out.extend(0x0001_0000u32.to_be_bytes()); // sfntVersion
    out.extend((n as u16).to_be_bytes());
    out.extend(64u16.to_be_bytes()); // searchRange
    out.extend(2u16.to_be_bytes()); // entrySelector
    out.extend((((n * 16) as u16).saturating_sub(64)).to_be_bytes()); // rangeShift

    let mut records = Vec::new();
    let mut data = Vec::new();
    for (tag, bytes) in &tables {
        let offset = 12 + n * 16 + data.len();
        records.extend(*tag);
        records.extend(0u32.to_be_bytes()); // checksum (unverified by readers)
        records.extend((offset as u32).to_be_bytes());
        records.extend((bytes.len() as u32).to_be_bytes());
        data.extend_from_slice(bytes);
        while data.len() % 4 != 0 {
            data.push(0);
        }
    }
    out.extend(records);
    out.extend(data);
    out
}

/// Build a minimal but *real* bare Type 1 font (PFA layout), with one
/// triangle glyph named `A` at code 65.
///
/// Written by hand rather than copied from a document: real Type 1 fonts in
/// the wild are licensed typefaces. The two Adobe ciphers are applied for
/// real (eexec key 55665, charstring key 4330), so this exercises the
/// decryption path exactly as a shipped font would.
///
/// Glyph: `0 600 hsbw`, triangle (100,50) → (900,50) → (500,650), closed.
pub fn minimal_type1() -> Vec<u8> {
    /// Adobe's cipher (Type 1 spec §7.2), encrypt direction.
    fn encrypt(plain: &[u8], key: u16, lead: usize) -> Vec<u8> {
        const C1: u16 = 52845;
        const C2: u16 = 22719;
        let mut r = key;
        let mut out = Vec::with_capacity(plain.len() + lead);
        for &p in std::iter::repeat_n(&0x55u8, lead).chain(plain.iter()) {
            let c = p ^ (r >> 8) as u8;
            r = (c as u16).wrapping_add(r).wrapping_mul(C1).wrapping_add(C2);
            out.push(c);
        }
        out
    }
    /// Type 1 charstring number encoding.
    fn num(v: i32, out: &mut Vec<u8>) {
        if (-107..=107).contains(&v) {
            out.push((v + 139) as u8);
        } else if (108..=1131).contains(&v) {
            let v = v - 108;
            out.push((247 + (v >> 8)) as u8);
            out.push((v & 0xFF) as u8);
        } else {
            out.push(255);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }

    // --- the glyph ---------------------------------------------------------
    let mut cs = Vec::new();
    num(0, &mut cs);
    num(600, &mut cs);
    cs.push(13); // hsbw: sidebearing 0, advance 600
    num(100, &mut cs);
    num(50, &mut cs);
    cs.push(21); // rmoveto -> (100, 50)
    num(800, &mut cs);
    num(0, &mut cs);
    cs.push(5); // rlineto -> (900, 50)
    num(-400, &mut cs);
    num(600, &mut cs);
    cs.push(5); // rlineto -> (500, 650)
    cs.push(9); // closepath
    cs.push(14); // endchar
    let cs = encrypt(&cs, 4330, 4);

    // `.notdef`: 0 600 hsbw endchar.
    let mut nd = Vec::new();
    num(0, &mut nd);
    num(600, &mut nd);
    nd.push(13);
    nd.push(14);
    let nd = encrypt(&nd, 4330, 4);

    // --- the eexec-encrypted private section --------------------------------
    let mut private = Vec::new();
    private.extend_from_slice(b"dup /Private 8 dict dup begin\n");
    private.extend_from_slice(b"/lenIV 4 def\n");
    private.extend_from_slice(b"/Subrs 1 array\n");
    private.extend_from_slice(b"dup 0 1 RD ");
    private.extend_from_slice(&encrypt(&[11], 4330, 0)); // subr 0: `return`
    private.extend_from_slice(b" NP\n");
    private.extend_from_slice(b"ND\n");
    private.extend_from_slice(b"/CharStrings 2 dict dup begin\n");
    private.extend_from_slice(format!("/.notdef {} RD ", nd.len()).as_bytes());
    private.extend_from_slice(&nd);
    private.extend_from_slice(b" ND\n");
    private.extend_from_slice(format!("/A {} RD ", cs.len()).as_bytes());
    private.extend_from_slice(&cs);
    private.extend_from_slice(b" ND\n");
    private.extend_from_slice(b"end\nend\nmark currentfile closefile\n");
    let private = encrypt(&private, 55665, 4);

    // --- the cleartext header ----------------------------------------------
    let mut out = Vec::new();
    out.extend_from_slice(b"%!PS-AdobeFont-1.0: TestFont 001.000\n");
    out.extend_from_slice(b"/FontName /TestFont def\n");
    out.extend_from_slice(b"/FontMatrix [0.001 0 0 0.001 0 0] readonly def\n");
    out.extend_from_slice(b"/FontType 1 def\n");
    out.extend_from_slice(b"/FontBBox {0 0 1000 1000} readonly def\n");
    out.extend_from_slice(b"/Encoding 256 array\n");
    out.extend_from_slice(b"0 1 255 {1 index exch /.notdef put} for\n");
    out.extend_from_slice(b"dup 65 /A put\n");
    out.extend_from_slice(b"readonly def\n");
    out.extend_from_slice(b"currentdict end\ncurrentfile eexec\n");
    out.extend_from_slice(&private);
    out.extend_from_slice(b"\n");
    for _ in 0..8 {
        out.extend_from_slice(&[b'0'; 64]);
        out.push(b'\n');
    }
    out.extend_from_slice(b"cleartomark\n");
    out
}
