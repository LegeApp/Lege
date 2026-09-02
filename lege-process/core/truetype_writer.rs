//! Minimal TrueType (sfnt) assembler: `head`, `hhea`, `maxp`, `hmtx`, `cmap`,
//! `loca`, `glyf`, `post`, `OS/2`, `name`, with correct table checksums and
//! `checkSumAdjustment`.
//!
//! Two callers: the ~1 KB glyphless font behind the invisible OCR text layer
//! (`glyphless_font.rs`, two empty glyphs) and the per-document glyph font that
//! carries a book's printed text as real outlines (`encoding::glyphfont`).
//! Glyphs are simple (no composites, no hinting) and every point is on-curve,
//! which is all the pixel-edge outlines of the first glyph-font stage need; a
//! curve-fitting vectorizer only has to start emitting off-curve points.
//!
//! PDF consumers reach the glyphs through `CIDToGIDMap /Identity`, so the
//! `cmap` is a minimal valid stub, never a character map.

/// One glyph as closed contours in font units. Outer contours run clockwise
/// (y up), inner contours counter-clockwise, per the TrueType convention.
/// Off-curve points are quadratic Bézier controls; two in a row imply an
/// on-curve midpoint. A glyph with no contours is an empty glyph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GlyphOutline {
    pub contours: Vec<Vec<OutlinePoint>>,
    /// Advance width in font units.
    pub advance: u16,
    /// When non-empty, the glyph is a composite drawing other glyphs'
    /// outlines at offsets and `contours` is ignored. Every referenced glyph
    /// must be a simple glyph (no chains).
    pub components: Vec<GlyphComponent>,
}

/// A contour point in font units with its TrueType on/off-curve flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutlinePoint {
    pub x: i16,
    pub y: i16,
    pub on_curve: bool,
}

impl OutlinePoint {
    pub const fn on(x: i16, y: i16) -> Self {
        Self {
            x,
            y,
            on_curve: true,
        }
    }

    pub const fn off(x: i16, y: i16) -> Self {
        Self {
            x,
            y,
            on_curve: false,
        }
    }
}

/// One component of a composite glyph: glyph `glyph` translated by
/// `(dx, dy)` font units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphComponent {
    pub glyph: u16,
    pub dx: i16,
    pub dy: i16,
}

impl GlyphOutline {
    pub fn empty(advance: u16) -> Self {
        Self {
            contours: Vec::new(),
            advance,
            components: Vec::new(),
        }
    }

    pub fn composite(advance: u16, components: Vec<GlyphComponent>) -> Self {
        Self {
            contours: Vec::new(),
            advance,
            components,
        }
    }

    /// `[x_min, y_min, x_max, y_max]` over all points, or `None` when empty.
    pub fn bbox(&self) -> Option<[i16; 4]> {
        let mut it = self.contours.iter().flatten();
        let first = it.next()?;
        let mut b = [first.x, first.y, first.x, first.y];
        for p in it {
            b[0] = b[0].min(p.x);
            b[1] = b[1].min(p.y);
            b[2] = b[2].max(p.x);
            b[3] = b[3].max(p.y);
        }
        Some(b)
    }
}

/// Everything the assembler needs besides the glyphs.
#[derive(Clone, Debug)]
pub struct TrueTypeSpec<'a> {
    /// Family / full / PostScript name. Printable ASCII only.
    pub name: &'a str,
    pub units_per_em: u16,
    pub ascent: i16,
    pub descent: i16,
    pub cap_height: i16,
    /// Glyph 0 is `.notdef`.
    pub glyphs: &'a [GlyphOutline],
}

/// The assembled program plus the metrics a PDF FontDescriptor wants.
#[derive(Clone, Debug)]
pub struct BuiltFont {
    pub data: Vec<u8>,
    /// Union of all glyph bounding boxes (`[0, descent, upem, ascent]` when
    /// every glyph is empty).
    pub bbox: [i16; 4],
    pub num_glyphs: u16,
}

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

/// Encoded `glyf` entry for one glyph plus its maxima.
#[derive(Clone)]
struct EncodedGlyph {
    bytes: Vec<u8>,
    bbox: Option<[i16; 4]>,
    points: u16,
    contours: u16,
}

/// Encode a simple glyph (PDF 32000 has no say here; this is the OpenType
/// `glyf` simple-glyph record). Empty glyphs encode to zero bytes.
/// Encode a composite glyph referencing already encoded simple glyphs.
/// Components that reference empty glyphs are dropped; a composite with
/// nothing left encodes empty.
fn encode_composite(components: &[(GlyphComponent, &EncodedGlyph)]) -> EncodedGlyph {
    let shift = |v: i16, d: i16| (v as i32 + d as i32).clamp(-32768, 32767) as i16;
    let drawn: Vec<(GlyphComponent, [i16; 4], &EncodedGlyph)> = components
        .iter()
        .filter_map(|(c, target)| {
            let tb = target.bbox?;
            Some((
                *c,
                [
                    shift(tb[0], c.dx),
                    shift(tb[1], c.dy),
                    shift(tb[2], c.dx),
                    shift(tb[3], c.dy),
                ],
                *target,
            ))
        })
        .collect();
    let Some(mut bbox) = drawn.first().map(|d| d.1) else {
        return EncodedGlyph {
            bytes: Vec::new(),
            bbox: None,
            points: 0,
            contours: 0,
        };
    };
    for (_, b, _) in &drawn[1..] {
        bbox = [
            bbox[0].min(b[0]),
            bbox[1].min(b[1]),
            bbox[2].max(b[2]),
            bbox[3].max(b[3]),
        ];
    }
    let mut out = Vec::with_capacity(10 + 8 * drawn.len());
    push_i16(&mut out, -1);
    for v in bbox {
        push_i16(&mut out, v);
    }
    let mut points = 0u16;
    let mut contours = 0u16;
    for (i, (c, _, target)) in drawn.iter().enumerate() {
        // ARG_1_AND_2_ARE_WORDS | ARGS_ARE_XY_VALUES | ROUND_XY_TO_GRID,
        // plus MORE_COMPONENTS on all but the last.
        let more = if i + 1 < drawn.len() { 0x0020 } else { 0 };
        push_u16(&mut out, 0x0001 | 0x0002 | 0x0004 | more);
        push_u16(&mut out, c.glyph);
        push_i16(&mut out, c.dx);
        push_i16(&mut out, c.dy);
        points = points.saturating_add(target.points);
        contours = contours.saturating_add(target.contours);
    }
    EncodedGlyph {
        bytes: out,
        bbox: Some(bbox),
        points,
        contours,
    }
}

fn encode_glyph(g: &GlyphOutline) -> EncodedGlyph {
    let contours: Vec<&Vec<OutlinePoint>> = g.contours.iter().filter(|c| c.len() >= 3).collect();
    let Some(bbox) = g.bbox().filter(|_| !contours.is_empty()) else {
        return EncodedGlyph {
            bytes: Vec::new(),
            bbox: None,
            points: 0,
            contours: 0,
        };
    };

    let mut out = Vec::new();
    push_i16(&mut out, contours.len() as i16);
    for v in bbox {
        push_i16(&mut out, v);
    }
    let mut end = 0usize;
    for c in &contours {
        end += c.len();
        push_u16(&mut out, (end - 1) as u16);
    }
    push_u16(&mut out, 0); // instructionLength

    // Flags, then x deltas, then y deltas. Bit 0 = on-curve; bit 1/2 = short
    // x/y; bit 4/5 = x/y is same (no bytes) when not short, or positive sign
    // when short.
    let mut flags = Vec::with_capacity(end);
    let mut xs = Vec::with_capacity(end);
    let mut ys = Vec::with_capacity(end);
    let (mut px, mut py) = (0i16, 0i16);
    for p in contours.iter().flat_map(|c| c.iter()) {
        let (x, y) = (p.x, p.y);
        let mut flag = if p.on_curve { 0x01u8 } else { 0x00u8 };
        let dx = x as i32 - px as i32;
        let dy = y as i32 - py as i32;
        if dx == 0 {
            flag |= 0x10;
        } else if dx.abs() < 256 {
            flag |= 0x02;
            if dx > 0 {
                flag |= 0x10;
            }
            xs.push(dx.unsigned_abs() as u8);
        } else {
            xs.extend_from_slice(&(dx as i16).to_be_bytes());
        }
        if dy == 0 {
            flag |= 0x20;
        } else if dy.abs() < 256 {
            flag |= 0x04;
            if dy > 0 {
                flag |= 0x20;
            }
            ys.push(dy.unsigned_abs() as u8);
        } else {
            ys.extend_from_slice(&(dy as i16).to_be_bytes());
        }
        flags.push(flag);
        px = x;
        py = y;
    }
    out.extend_from_slice(&flags);
    out.extend_from_slice(&xs);
    out.extend_from_slice(&ys);
    while out.len() % 4 != 0 {
        out.push(0);
    }

    EncodedGlyph {
        bytes: out,
        bbox: Some(bbox),
        points: end as u16,
        contours: contours.len() as u16,
    }
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

fn build_post() -> Vec<u8> {
    let mut t = Vec::new();
    push_u32(&mut t, 0x0003_0000); // version 3.0 (no glyph names)
    push_u32(&mut t, 0); // italicAngle
    push_i16(&mut t, -100); // underlinePosition
    push_i16(&mut t, 50); // underlineThickness
    push_u32(&mut t, 0); // isFixedPitch
    push_u32(&mut t, 0); // minMemType42
    push_u32(&mut t, 0); // maxMemType42
    push_u32(&mut t, 0); // minMemType1
    push_u32(&mut t, 0); // maxMemType1
    t
}

fn build_os2(spec: &TrueTypeSpec, avg_width: i16) -> Vec<u8> {
    // OS/2 version 4, minimal. Many viewers expect an OS/2 table on a Windows
    // (platform 3) font.
    let mut t = Vec::new();
    push_u16(&mut t, 4); // version
    push_i16(&mut t, avg_width); // xAvgCharWidth
    push_u16(&mut t, 400); // usWeightClass (Normal)
    push_u16(&mut t, 5); // usWidthClass (Medium)
    push_u16(&mut t, 0); // fsType (installable)
    for _ in 0..10 {
        push_i16(&mut t, 0); // subscript/superscript/strikeout (10 int16)
    }
    push_i16(&mut t, 0); // sFamilyClass
    t.extend_from_slice(&[0u8; 10]); // panose
    for _ in 0..4 {
        push_u32(&mut t, 0); // ulUnicodeRange1..4
    }
    t.extend_from_slice(b"NONE"); // achVendID
    push_u16(&mut t, 0x0040); // fsSelection (REGULAR)
    push_u16(&mut t, 0); // usFirstCharIndex
    push_u16(&mut t, 0xFFFF); // usLastCharIndex
    push_i16(&mut t, spec.ascent); // sTypoAscender
    push_i16(&mut t, spec.descent); // sTypoDescender
    push_i16(&mut t, 0); // sTypoLineGap
    push_u16(&mut t, spec.ascent.max(0) as u16); // usWinAscent
    push_u16(&mut t, spec.descent.saturating_neg().max(0) as u16); // usWinDescent
    push_u32(&mut t, 0); // ulCodePageRange1
    push_u32(&mut t, 0); // ulCodePageRange2
    push_i16(&mut t, spec.cap_height); // sxHeight (reuse)
    push_i16(&mut t, spec.cap_height); // sCapHeight
    push_u16(&mut t, 0); // usDefaultChar
    push_u16(&mut t, 0); // usBreakChar
    push_u16(&mut t, 0); // usMaxContext
    t
}

fn build_name(name: &str) -> Vec<u8> {
    // Mac platform (1,0,0) ASCII strings for a few standard name IDs.
    let name: Vec<u8> = name
        .bytes()
        .filter(|b| b.is_ascii_graphic() || *b == b' ')
        .collect();
    let name: &[u8] = if name.is_empty() { b"Lege" } else { &name };
    let sub: &[u8] = b"Regular";
    let records: [(u16, bool); 4] = [
        (1, true),  // Family
        (2, false), // Subfamily
        (4, true),  // Full name
        (6, true),  // PostScript name
    ];

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
    for (id, is_name) in records {
        let (off, len) = if is_name {
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

/// Build a complete, self-contained TrueType font. `spec.glyphs` must hold at
/// least one glyph (`.notdef`); at most 65535 are representable.
pub fn build_truetype(spec: &TrueTypeSpec) -> BuiltFont {
    assert!(
        !spec.glyphs.is_empty() && spec.glyphs.len() <= u16::MAX as usize,
        "TrueType needs 1..=65535 glyphs, got {}",
        spec.glyphs.len()
    );
    let num_glyphs = spec.glyphs.len() as u16;

    // glyf + loca (long offsets), maxima, hmtx.
    let mut glyf = Vec::new();
    let mut loca = Vec::with_capacity((spec.glyphs.len() + 1) * 4);
    let mut hmtx = Vec::with_capacity(spec.glyphs.len() * 4);
    let mut max_points = 0u16;
    let mut max_contours = 0u16;
    let mut max_composite_points = 0u16;
    let mut max_composite_contours = 0u16;
    let mut max_component_elements = 0u16;
    let mut bbox: Option<[i16; 4]> = None;
    let mut advance_max = 0u16;
    let mut min_lsb = i16::MAX;
    let mut min_rsb = i16::MAX;
    let mut x_max_extent = i16::MIN;
    let mut advance_sum = 0u64;
    // Simple glyphs first, so composites can take their target's bounds.
    let simple: Vec<Option<EncodedGlyph>> = spec
        .glyphs
        .iter()
        .map(|g| g.components.is_empty().then(|| encode_glyph(g)))
        .collect();
    for (g, simple_enc) in spec.glyphs.iter().zip(simple.iter()) {
        let enc = match simple_enc {
            Some(enc) => enc.clone(),
            None => {
                let targets: Vec<(GlyphComponent, &EncodedGlyph)> = g
                    .components
                    .iter()
                    .map(|c| {
                        let target = simple
                            .get(c.glyph as usize)
                            .and_then(Option::as_ref)
                            .expect("composite glyph must reference a simple glyph in the font");
                        (*c, target)
                    })
                    .collect();
                let enc = encode_composite(&targets);
                max_composite_points = max_composite_points.max(enc.points);
                max_composite_contours = max_composite_contours.max(enc.contours);
                max_component_elements = max_component_elements.max(targets.len() as u16);
                enc
            }
        };
        push_u32(&mut loca, glyf.len() as u32);
        glyf.extend_from_slice(&enc.bytes);
        if g.components.is_empty() {
            max_points = max_points.max(enc.points);
            max_contours = max_contours.max(enc.contours);
        }
        let lsb = match enc.bbox {
            Some(b) => {
                bbox = Some(match bbox {
                    None => b,
                    Some(u) => [
                        u[0].min(b[0]),
                        u[1].min(b[1]),
                        u[2].max(b[2]),
                        u[3].max(b[3]),
                    ],
                });
                min_lsb = min_lsb.min(b[0]);
                min_rsb = min_rsb.min((g.advance as i32 - b[2] as i32).clamp(-32768, 32767) as i16);
                x_max_extent = x_max_extent.max(b[2]);
                b[0]
            }
            None => 0,
        };
        push_u16(&mut hmtx, g.advance);
        push_i16(&mut hmtx, lsb);
        advance_max = advance_max.max(g.advance);
        advance_sum += g.advance as u64;
    }
    push_u32(&mut loca, glyf.len() as u32);
    // Pad so the table is never zero-length (some parsers reject a 0-byte table).
    while glyf.len() < 4 {
        glyf.push(0);
    }
    if min_lsb == i16::MAX {
        min_lsb = 0;
        min_rsb = 0;
        x_max_extent = 0;
    }
    let font_bbox = bbox.unwrap_or([0, spec.descent, spec.units_per_em as i16, spec.ascent]);
    let avg_width = (advance_sum / spec.glyphs.len() as u64).min(i16::MAX as u64) as i16;

    // head
    let mut head = Vec::new();
    push_u16(&mut head, 1); // majorVersion
    push_u16(&mut head, 0); // minorVersion
    push_u32(&mut head, 0x0001_0000); // fontRevision 1.0
    push_u32(&mut head, 0); // checkSumAdjustment (filled after assembly)
    push_u32(&mut head, 0x5F0F_3CF5); // magicNumber
    push_u16(&mut head, 0x000B); // flags
    push_u16(&mut head, spec.units_per_em);
    for _ in 0..4 {
        push_u32(&mut head, 0); // created / modified
    }
    for v in font_bbox {
        push_i16(&mut head, v);
    }
    push_u16(&mut head, 0); // macStyle
    push_u16(&mut head, 8); // lowestRecPPEM
    push_i16(&mut head, 2); // fontDirectionHint
    push_i16(&mut head, 1); // indexToLocFormat (1 = long)
    push_i16(&mut head, 0); // glyphDataFormat

    // hhea
    let mut hhea = Vec::new();
    push_u16(&mut hhea, 1); // majorVersion
    push_u16(&mut hhea, 0); // minorVersion
    push_i16(&mut hhea, spec.ascent);
    push_i16(&mut hhea, spec.descent);
    push_i16(&mut hhea, 0); // lineGap
    push_u16(&mut hhea, advance_max);
    push_i16(&mut hhea, min_lsb);
    push_i16(&mut hhea, min_rsb);
    push_i16(&mut hhea, x_max_extent);
    push_i16(&mut hhea, 1); // caretSlopeRise
    push_i16(&mut hhea, 0); // caretSlopeRun
    push_i16(&mut hhea, 0); // caretOffset
    for _ in 0..4 {
        push_i16(&mut hhea, 0); // reserved
    }
    push_i16(&mut hhea, 0); // metricDataFormat
    push_u16(&mut hhea, num_glyphs); // numberOfHMetrics

    // maxp 1.0
    let mut maxp = Vec::new();
    push_u32(&mut maxp, 0x0001_0000);
    push_u16(&mut maxp, num_glyphs);
    push_u16(&mut maxp, max_points);
    push_u16(&mut maxp, max_contours);
    push_u16(&mut maxp, max_composite_points);
    push_u16(&mut maxp, max_composite_contours);
    push_u16(&mut maxp, 1); // maxZones
    push_u16(&mut maxp, 0); // maxTwilightPoints
    push_u16(&mut maxp, 0); // maxStorage
    push_u16(&mut maxp, 0); // maxFunctionDefs
    push_u16(&mut maxp, 0); // maxInstructionDefs
    push_u16(&mut maxp, 0); // maxStackElements
    push_u16(&mut maxp, 0); // maxSizeOfInstructions
    push_u16(&mut maxp, max_component_elements); // maxComponentElements
    push_u16(&mut maxp, if max_component_elements > 0 { 1 } else { 0 }); // maxComponentDepth

    let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"OS/2", build_os2(spec, avg_width)),
        (b"cmap", build_cmap()),
        (b"glyf", glyf),
        (b"head", head),
        (b"hhea", hhea),
        (b"hmtx", hmtx),
        (b"loca", loca),
        (b"maxp", maxp),
        (b"name", build_name(spec.name)),
        (b"post", build_post()),
    ];
    tables.sort_by(|a, b| a.0.cmp(b.0));

    let num_tables = tables.len() as u16;
    // sfnt header (12) + directory (16 per table)
    let mut offset = 12 + 16 * num_tables as u32;

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
        offset += length;
        offset = (offset + 3) & !3;
    }

    let entry_selector = (15u16).saturating_sub((num_tables).leading_zeros() as u16);
    let search_range = (1u16 << entry_selector) * 16;
    let range_shift = num_tables * 16 - search_range;

    let mut font = Vec::with_capacity(offset as usize);
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
    let head_offset = entries[head_dir_index].offset as usize;
    let field = head_offset + 8;
    font[field..field + 4].copy_from_slice(&adjustment.to_be_bytes());

    BuiltFont {
        data: font,
        bbox: font_bbox,
        num_glyphs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x0: i16, y0: i16, x1: i16, y1: i16) -> Vec<OutlinePoint> {
        // Clockwise with y up: top-left, top-right, bottom-right, bottom-left.
        vec![
            OutlinePoint::on(x0, y1),
            OutlinePoint::on(x1, y1),
            OutlinePoint::on(x1, y0),
            OutlinePoint::on(x0, y0),
        ]
    }

    #[test]
    fn off_curve_points_clear_the_on_curve_flag() {
        let g = GlyphOutline {
            contours: vec![vec![
                OutlinePoint::on(0, 0),
                OutlinePoint::off(50, 100),
                OutlinePoint::on(100, 0),
            ]],
            advance: 100,
            components: Vec::new(),
        };
        let e = encode_glyph(&g);
        // header: 2 (contours) + 8 (bbox) + 2 (endPts) + 2 (instructionLength)
        let flags = &e.bytes[14..17];
        assert_eq!(flags[0] & 1, 1);
        assert_eq!(flags[1] & 1, 0);
        assert_eq!(flags[2] & 1, 1);
        assert_eq!(e.bbox, Some([0, 0, 100, 100]));
    }

    #[test]
    fn empty_glyph_encodes_to_nothing() {
        let e = encode_glyph(&GlyphOutline::empty(500));
        assert!(e.bytes.is_empty());
        assert_eq!(e.points, 0);
        assert!(e.bbox.is_none());
    }

    #[test]
    fn simple_glyph_record_layout() {
        let g = GlyphOutline {
            contours: vec![square(0, 0, 100, 200)],
            advance: 120,
            components: Vec::new(),
        };
        let e = encode_glyph(&g);
        assert_eq!(e.contours, 1);
        assert_eq!(e.points, 4);
        assert_eq!(e.bbox, Some([0, 0, 100, 200]));
        // numberOfContours=1, bbox, endPts=[3], instrLen=0, 4 flags, x/y deltas.
        assert_eq!(&e.bytes[0..2], &1i16.to_be_bytes());
        assert_eq!(&e.bytes[10..12], &3u16.to_be_bytes());
        assert_eq!(&e.bytes[12..14], &0u16.to_be_bytes());
        // First point (0,200): x same (0x10), y short positive (0x04|0x20) -> 0x35.
        assert_eq!(e.bytes[14], 0x01 | 0x10 | 0x04 | 0x20);
        assert_eq!(e.bytes.len() % 4, 0);
    }

    #[test]
    fn long_deltas_use_two_bytes() {
        let g = GlyphOutline {
            contours: vec![square(0, 0, 1000, 20)],
            advance: 1000,
            components: Vec::new(),
        };
        let e = encode_glyph(&g);
        // Point 2 (1000, 20): dx = 1000 -> no short flag, two bytes.
        let flag = e.bytes[15];
        assert_eq!(flag & 0x02, 0, "x not short");
        assert_eq!(flag & 0x10, 0, "x not same");
    }

    #[test]
    fn assembled_font_parses_and_reports_glyph_count() {
        let glyphs = vec![
            GlyphOutline::empty(0),
            GlyphOutline {
                contours: vec![square(0, -20, 60, 80)],
                advance: 60,
                components: Vec::new(),
            },
            GlyphOutline {
                contours: vec![square(10, 0, 50, 700), square(20, 100, 40, 600)],
                advance: 70,
                components: Vec::new(),
            },
        ];
        let built = build_truetype(&TrueTypeSpec {
            name: "LegeGlyphs",
            units_per_em: 1000,
            ascent: 800,
            descent: -200,
            cap_height: 700,
            glyphs: &glyphs,
        });
        assert_eq!(built.num_glyphs, 3);
        assert_eq!(built.bbox, [0, -20, 60, 700]);
        let face = lege_pdf_read::read_face_metrics(&built.data, 0)
            .expect("assembled font should parse as a real face");
        assert_eq!(face.num_glyphs, 3);
        assert_eq!(face.units_per_em, 1000);
        // Checksum adjustment makes the whole-file checksum 0xB1B0AFBA.
        assert_eq!(table_checksum(&built.data), 0xB1B0_AFBA);
    }

    #[test]
    fn composite_glyphs_reference_a_simple_glyph_with_an_offset() {
        let glyphs = [
            GlyphOutline::empty(0),
            GlyphOutline {
                contours: vec![square(0, 0, 100, 200)],
                advance: 120,
                components: Vec::new(),
            },
            GlyphOutline::composite(
                130,
                vec![GlyphComponent {
                    glyph: 1,
                    dx: 16,
                    dy: -8,
                }],
            ),
            // Two copies of the square side by side.
            GlyphOutline::composite(
                250,
                vec![
                    GlyphComponent {
                        glyph: 1,
                        dx: 0,
                        dy: 0,
                    },
                    GlyphComponent {
                        glyph: 1,
                        dx: 120,
                        dy: 0,
                    },
                ],
            ),
        ];
        let built = build_truetype(&TrueTypeSpec {
            name: "T",
            units_per_em: 1000,
            ascent: 800,
            descent: -200,
            cap_height: 700,
            glyphs: &glyphs,
        });
        assert_eq!(built.num_glyphs, 4);
        // The composites' bounds are their targets', shifted.
        assert_eq!(built.bbox, [0, -8, 220, 200]);
        let face = lege_pdf_read::read_face_metrics(&built.data, 0).expect("font parses");
        assert_eq!(face.num_glyphs, 4);

        // glyf record of glyph 2: numberOfContours -1, bbox, flags, index, args.
        let (loca, glyf) = (table(&built.data, b"loca"), table(&built.data, b"glyf"));
        let off =
            |i: usize| u32::from_be_bytes(loca[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
        let rec = &glyf[off(2)..off(3)];
        assert_eq!(&rec[..2], &(-1i16).to_be_bytes());
        assert_eq!(&rec[10..12], &0x0007u16.to_be_bytes());
        assert_eq!(&rec[12..14], &1u16.to_be_bytes());
        assert_eq!(&rec[14..16], &16i16.to_be_bytes());
        assert_eq!(&rec[16..18], &(-8i16).to_be_bytes());
        // Glyph 3: two components, the first flagged MORE_COMPONENTS.
        let rec = &glyf[off(3)..off(4)];
        assert_eq!(rec.len(), 10 + 2 * 8);
        assert_eq!(&rec[10..12], &0x0027u16.to_be_bytes());
        assert_eq!(&rec[18..20], &0x0007u16.to_be_bytes());
        assert_eq!(&rec[22..24], &120i16.to_be_bytes());
        let maxp = table(&built.data, b"maxp");
        assert_eq!(&maxp[28..30], &2u16.to_be_bytes(), "maxComponentElements");
        assert_eq!(&maxp[30..32], &1u16.to_be_bytes(), "maxComponentDepth");
    }

    /// Locate a table's bytes in an assembled font.
    fn table<'a>(font: &'a [u8], tag: &[u8; 4]) -> &'a [u8] {
        let n = u16::from_be_bytes([font[4], font[5]]) as usize;
        for i in 0..n {
            let rec = &font[12 + 16 * i..12 + 16 * i + 16];
            if &rec[..4] == tag {
                let off = u32::from_be_bytes(rec[8..12].try_into().unwrap()) as usize;
                let len = u32::from_be_bytes(rec[12..16].try_into().unwrap()) as usize;
                return &font[off..off + len];
            }
        }
        panic!("table {:?} missing", std::str::from_utf8(tag));
    }
}
