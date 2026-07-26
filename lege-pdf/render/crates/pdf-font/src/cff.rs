//! Bare CFF support: wrapping, glyph names, and CID→GID recovery.
//!
//! PDF embeds CFF *bare* — `/FontFile3` with subtype `Type1C` (simple) or
//! `CIDFontType0C` (CID-keyed) is a raw CFF, not an SFNT. FreeType accepts
//! that directly, which is why PDFium does; Skrifa reads SFNT only, so
//! without [`wrap_bare_cff`] every such font fails to parse. That failure is
//! quiet and damaging: a simple font then falls back to a *substituted* face
//! (the text reads, but in the wrong typeface, and any glyph the substitute
//! lacks — accented Latin, say — silently disappears), while a CID font
//! drops to placement boxes.
//!
//! The wrapper synthesizes the tables an SFNT reader needs from the CFF's
//! own data — nothing is invented — and the CFF table is the original bytes.
//! It is the runtime twin of `tools/foxit-fonts/extract.py`, which does the
//! same job offline for the bundled faces.
//!
//! # CID→GID (ISO 32000-1 §9.7.4.2)
//!
//! A `CIDFontType0` descendant embeds a CID-keyed CFF (`/FontFile3` with
//! subtype `CIDFontType0C`). Its glyphs are addressed by **glyph id**, but
//! the PDF addresses them by **CID**, and the two are only equal by
//! coincidence: the CFF's `charset` lists, for each GID in order, the CID it
//! carries. A real subset font looks like
//!
//! ```text
//! GID:   0        1       2       3      …
//! CID:  .notdef   3      11      12      …
//! ```
//!
//! so treating a CID as a GID silently draws the *wrong glyph*, or `.notdef`
//! once the CID exceeds the glyph count — which is why CJK text in such a
//! font comes out as boxes.
//!
//! `/CIDToGIDMap` does not help: it is only defined for `CIDFontType2`
//! (TrueType) descendants. For `CIDFontType0` the charset is the only
//! mapping, so it is parsed here.
//!
//! This reads just enough CFF to describe the font — header, INDEXes, the
//! Top DICT, the charset — and deliberately nothing else: charstrings, and
//! therefore outlines, remain Skrifa's job.

use crate::cid::CidToGid;

/// Build a CID→GID map from a CID-keyed CFF.
///
/// Returns `None` when `data` is not CFF, is not CID-keyed (a plain CFF is
/// addressed by GID already), or uses a predefined charset — in each case
/// the caller's existing behaviour is correct.
pub fn cid_to_gid_from_cff(data: &[u8]) -> Option<CidToGid> {
    // A `/FontFile3` may embed the CFF **bare** or wrapped in an OpenType SFNT
    // (`OTTO`, or rarely a `0x00010000`/`true` shell carrying a `CFF ` table —
    // the CAJ/CNKI and many CJK producers ship the latter). When it is wrapped,
    // the charset/charstrings offsets in the Top DICT are relative to the start
    // of the `CFF ` table, so we must parse that slice, not the SFNT bytes —
    // otherwise the header misparses, the map falls back to Identity, and the
    // large subset CIDs land past the tiny glyph count as `.notdef`.
    let data = cff_table(data).unwrap_or(data);
    let mut r = Reader { data, pos: 0 };

    // Header: major, minor, hdrSize, offSize.
    let _major = r.u8()?;
    let _minor = r.u8()?;
    let hdr_size = r.u8()? as usize;
    let _off_size = r.u8()?;
    r.pos = hdr_size;

    let _names = r.index()?;
    let top_dicts = r.index()?;
    // Strings and global subrs are not needed to read the charset.

    let top = top_dicts.first()?;
    let dict = parse_dict(top);

    // A CID-keyed font declares `ROS` (operator 12 30). Without it the font
    // is addressed by GID and there is nothing to remap.
    if !dict.iter().any(|(op, _)| *op == 0x0c1e) {
        return None;
    }
    let charstrings_off = dict.iter().find(|(op, _)| *op == 17)?.1.first().copied()? as usize;
    let n_glyphs = {
        let mut cr = Reader {
            data,
            pos: charstrings_off,
        };
        cr.index()?.len()
    };
    let charset_off = dict.iter().find(|(op, _)| *op == 15)?.1.first().copied()? as usize;
    // 0/1/2 name the predefined charsets (ISOAdobe/Expert/ExpertSubset),
    // which never appear in a CID font.
    if charset_off <= 2 {
        return None;
    }

    let gid_to_cid = parse_charset(data, charset_off, n_glyphs)?;
    // Invert: the PDF looks up by CID.
    let max_cid = gid_to_cid.iter().copied().max().unwrap_or(0) as usize;
    let mut map = vec![0u16; max_cid + 1];
    for (gid, &cid) in gid_to_cid.iter().enumerate() {
        // First GID wins on a duplicate CID (malformed fonts).
        if map[cid as usize] == 0 {
            map[cid as usize] = gid as u16;
        }
    }
    Some(CidToGid::Map(map))
}

/// `charset` maps GID → CID (GID 0 is always `.notdef`/CID 0 and is implicit).
fn parse_charset(data: &[u8], offset: usize, n_glyphs: usize) -> Option<Vec<u16>> {
    let mut r = Reader { data, pos: offset };
    let format = r.u8()?;
    let mut out = Vec::with_capacity(n_glyphs);
    out.push(0); // GID 0
    match format {
        // Format 0: one SID/CID per glyph.
        0 => {
            while out.len() < n_glyphs {
                out.push(r.u16()?);
            }
        }
        // Formats 1/2: ranges of consecutive CIDs. They differ only in the
        // width of the run length.
        1 | 2 => {
            while out.len() < n_glyphs {
                let first = r.u16()?;
                let n_left = if format == 1 {
                    r.u8()? as u32
                } else {
                    r.u16()? as u32
                };
                for i in 0..=n_left {
                    if out.len() >= n_glyphs {
                        break;
                    }
                    out.push(first.checked_add(i as u16)?);
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

/// If `data` is an SFNT (`OTTO`/`0x00010000`/`true`) carrying a `CFF ` table,
/// return that table's bytes; otherwise `None` (the caller then treats `data`
/// as a bare CFF). Collections (`ttcf`) are not handled — an embedded CIDFont
/// is a single face.
fn cff_table(data: &[u8]) -> Option<&[u8]> {
    let tag = data.get(0..4)?;
    let is_sfnt = matches!(tag, b"OTTO" | b"true" | [0x00, 0x01, 0x00, 0x00]);
    if !is_sfnt {
        return None;
    }
    let num_tables = u16::from_be_bytes([*data.get(4)?, *data.get(5)?]) as usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let t = data.get(rec..rec + 4)?;
        if t == b"CFF " {
            let off = u32::from_be_bytes(data.get(rec + 8..rec + 12)?.try_into().ok()?) as usize;
            let len = u32::from_be_bytes(data.get(rec + 12..rec + 16)?.try_into().ok()?) as usize;
            return data.get(off..off.checked_add(len)?);
        }
    }
    None
}

/// Parse a CFF custom `Encoding` (formats 0 and 1) into `(code, gid)` pairs.
///
/// GID 0 (`.notdef`) is never encoded; the array assigns GIDs 1, 2, … to the
/// listed codes in order. Format 0 lists a code per glyph; format 1 lists
/// ranges of consecutive codes. A supplement block (high bit `0x80`) adds extra
/// code→SID aliases — those need the charset to resolve to a GID and are rare,
/// so they are skipped (the primary encoding already covers the glyphs).
fn parse_cff_encoding(data: &[u8], offset: usize, n_glyphs: usize) -> Option<Vec<(u8, u16)>> {
    let mut r = Reader { data, pos: offset };
    let format = r.u8()?;
    let mut out = Vec::new();
    let mut gid: u16 = 1;
    match format & 0x7f {
        0 => {
            let n_codes = r.u8()? as usize;
            for _ in 0..n_codes {
                if (gid as usize) >= n_glyphs {
                    break;
                }
                out.push((r.u8()?, gid));
                gid += 1;
            }
        }
        1 => {
            let n_ranges = r.u8()? as usize;
            for _ in 0..n_ranges {
                let first = r.u8()?;
                let n_left = r.u8()?;
                for i in 0..=n_left {
                    if (gid as usize) >= n_glyphs {
                        break;
                    }
                    out.push((first.saturating_add(i), gid));
                    gid += 1;
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

/// Build a minimal `cmap` (a `(3,0)` symbol subtable, format 4) from a CFF's
/// built-in `(code, gid)` encoding, so an SFNT reader can resolve a raw byte
/// code to a glyph — the route a symbolic simple font needs, since it has no
/// `/Differences` names and no Unicode cmap. Symbol codes live in the `0xF000`
/// range by convention, which is exactly where `FontProgram::gid_for_code`
/// looks after a direct miss.
fn build_cmap(encoding: &[(u8, u16)]) -> Vec<u8> {
    // One segment per code (encodings are not GID-contiguous), plus the
    // mandatory `0xFFFF` terminator. idDelta carries the glyph directly.
    let mut segs: Vec<(u16, u16)> = encoding
        .iter()
        .map(|&(c, g)| (0xF000 | c as u16, g))
        .collect();
    segs.sort_by_key(|&(c, _)| c);
    segs.dedup_by_key(|&mut (c, _)| c);
    let seg_count = segs.len() + 1;

    // The format-4 arrays.
    let mut end = Vec::new();
    let mut start = Vec::new();
    let mut delta = Vec::new();
    for &(code, gid) in &segs {
        end.extend(code.to_be_bytes());
        start.extend(code.to_be_bytes());
        delta.extend((gid.wrapping_sub(code)).to_be_bytes());
    }
    end.extend(0xFFFFu16.to_be_bytes());
    start.extend(0xFFFFu16.to_be_bytes());
    delta.extend(1u16.to_be_bytes());
    let range_offset = vec![0u8; seg_count * 2];

    let seg_x2 = (seg_count * 2) as u16;
    let sub_len = (14 + seg_count * 8) as u16; // header + 4 arrays + reservedPad
    let mut sub = Vec::new();
    sub.extend(4u16.to_be_bytes()); // format
    sub.extend(sub_len.to_be_bytes());
    sub.extend(0u16.to_be_bytes()); // language
    sub.extend(seg_x2.to_be_bytes());
    // searchRange / entrySelector / rangeShift are advisory (readers recompute).
    let entry_sel = (15 - (seg_count as u16).max(1).leading_zeros()) as u16;
    let search_range = 2u16.pow(entry_sel as u32) * 2;
    sub.extend(search_range.to_be_bytes());
    sub.extend(entry_sel.to_be_bytes());
    sub.extend(seg_x2.saturating_sub(search_range).to_be_bytes());
    sub.extend(end);
    sub.extend(0u16.to_be_bytes()); // reservedPad
    sub.extend(start);
    sub.extend(delta);
    sub.extend(range_offset);

    // cmap header: version, one encoding record → the subtable.
    let mut out = Vec::new();
    out.extend(0u16.to_be_bytes()); // version
    out.extend(1u16.to_be_bytes()); // numTables
    out.extend(3u16.to_be_bytes()); // platformID = 3 (Windows)
    out.extend(0u16.to_be_bytes()); // encodingID = 0 (Symbol)
    out.extend(12u32.to_be_bytes()); // offset to subtable
    out.extend(sub);
    out
}

/// True when `data` looks like a bare CFF (rather than an SFNT).
///
/// A CFF starts `major minor hdrSize offSize`; an SFNT starts with a version
/// tag (`0x00010000`, `OTTO`, `ttcf`, `true`). Version 1 is the only one PDF
/// embeds bare.
pub fn is_bare_cff(data: &[u8]) -> bool {
    matches!(data, [1, _, hdr, off_size, ..] if *hdr >= 4 && (1..=4).contains(off_size))
}

/// What the wrapper needs from a CFF to describe it as an SFNT.
struct CffInfo {
    n_glyphs: usize,
    units_per_em: u16,
    bbox: [i16; 4],
    /// GID → PostScript glyph name; empty for a CID-keyed font, whose glyphs
    /// are named by CID and reached through the charset instead.
    names: Vec<Vec<u8>>,
    /// The CFF's built-in `Encoding` as `(code, gid)` pairs — the authoritative
    /// code→glyph map for a **symbolic** simple font, which carries no
    /// `/Differences` and whose codes are not Unicode. Empty when the font uses
    /// a predefined encoding (Standard/Expert, offset 0/1) — the name-based path
    /// covers those — or is CID-keyed (no byte encoding at all).
    encoding: Vec<(u8, u16)>,
}

/// Wrap a bare CFF in a minimal OpenType container so an SFNT reader can
/// load it.
///
/// The `CFF ` table is the input, byte for byte. Everything else is derived
/// from it: `head` from `FontMatrix`/`FontBBox`, `maxp` from the CharStrings
/// INDEX, `post` from the charset's glyph names (which is how a simple
/// font's codes reach glyphs — `/Differences` and the base encodings are
/// name-based). Returns `None` if `data` is not a CFF we can describe.
pub fn wrap_bare_cff(data: &[u8]) -> Option<Vec<u8>> {
    if !is_bare_cff(data) {
        return None;
    }
    let info = read_cff_info(data)?;
    if info.n_glyphs == 0 || info.n_glyphs > u16::MAX as usize {
        return None;
    }

    // Length-preserving repair of malformed Private DICTs, so a strict SFNT
    // reader doesn't reject every outline over unreadable hint data.
    let mut cff = data.to_vec();
    sanitize_private_dicts(&mut cff);

    let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"CFF ", cff),
        (b"head", build_head(&info)),
        (b"maxp", build_maxp(&info)),
        (b"hhea", build_hhea(&info)),
        (b"hmtx", build_hmtx(&info)),
    ];
    if !info.names.is_empty() {
        tables.push((b"post", build_post(&info)));
    }
    // A custom byte encoding becomes a symbol `cmap` so `gid_for_code` resolves
    // a symbolic font's raw codes (which carry no `/Differences` names).
    if !info.encoding.is_empty() {
        tables.push((b"cmap", build_cmap(&info.encoding)));
    }
    tables.sort_by(|a, b| a.0.cmp(b.0));

    let n = tables.len();
    let mut out = Vec::new();
    out.extend(u32::from_be_bytes(*b"OTTO").to_be_bytes()); // CFF outlines
    out.extend((n as u16).to_be_bytes());
    // searchRange / entrySelector / rangeShift are advisory; readers compute
    // what they need. Emit consistent values anyway.
    let pow2 = (n as u16).next_power_of_two() >> if n.is_power_of_two() { 0 } else { 1 };
    out.extend((pow2 * 16).to_be_bytes());
    out.extend((pow2.trailing_zeros() as u16).to_be_bytes());
    out.extend(((n as u16 * 16).saturating_sub(pow2 * 16)).to_be_bytes());

    let mut records = Vec::new();
    let mut blob = Vec::new();
    for (tag, bytes) in &tables {
        let offset = 12 + n * 16 + blob.len();
        records.extend(*tag);
        records.extend(0u32.to_be_bytes()); // checksum: not verified by readers
        records.extend((offset as u32).to_be_bytes());
        records.extend((bytes.len() as u32).to_be_bytes());
        blob.extend_from_slice(bytes);
        while blob.len() % 4 != 0 {
            blob.push(0);
        }
    }
    out.extend(records);
    out.extend(blob);
    Some(out)
}

fn build_head(info: &CffInfo) -> Vec<u8> {
    let mut head = Vec::new();
    head.extend(0x0001_0000u32.to_be_bytes()); // version
    head.extend(0x0001_0000u32.to_be_bytes()); // fontRevision
    head.extend(0u32.to_be_bytes()); // checkSumAdjustment
    head.extend(0x5F0F_3CF5u32.to_be_bytes()); // magicNumber
    head.extend(3u16.to_be_bytes()); // flags
    head.extend(info.units_per_em.to_be_bytes());
    head.extend(0u64.to_be_bytes()); // created
    head.extend(0u64.to_be_bytes()); // modified
    for v in info.bbox {
        head.extend(v.to_be_bytes());
    }
    head.extend(0u16.to_be_bytes()); // macStyle
    head.extend(3u16.to_be_bytes()); // lowestRecPPEM
    head.extend(2i16.to_be_bytes()); // fontDirectionHint
    head.extend(0i16.to_be_bytes()); // indexToLocFormat
    head.extend(0i16.to_be_bytes()); // glyphDataFormat
    head
}

/// `hhea`/`hmtx` exist so an SFNT reader finds the horizontal metrics it
/// expects; the **advances are zero**, because the real widths live in the
/// charstrings and extracting them would mean interpreting every glyph.
///
/// That costs nothing here: a PDF positions text with its own `/Widths`, and
/// `FontProgram::advance` is only consulted for a *substituted* bundled face
/// (a real OTF with real metrics), never for an embedded font.
fn build_hhea(info: &CffInfo) -> Vec<u8> {
    let mut hhea = Vec::new();
    hhea.extend(0x0001_0000u32.to_be_bytes()); // version
    hhea.extend(info.bbox[3].to_be_bytes()); // ascender
    hhea.extend(info.bbox[1].to_be_bytes()); // descender
    hhea.extend(0i16.to_be_bytes()); // lineGap
    hhea.extend(0u16.to_be_bytes()); // advanceWidthMax
    hhea.extend(0i16.to_be_bytes()); // minLeftSideBearing
    hhea.extend(0i16.to_be_bytes()); // minRightSideBearing
    hhea.extend(info.bbox[2].to_be_bytes()); // xMaxExtent
    hhea.extend(1i16.to_be_bytes()); // caretSlopeRise
    hhea.extend(0i16.to_be_bytes()); // caretSlopeRun
    hhea.extend(0i16.to_be_bytes()); // caretOffset
    for _ in 0..4 {
        hhea.extend(0i16.to_be_bytes()); // reserved
    }
    hhea.extend(0i16.to_be_bytes()); // metricDataFormat
    hhea.extend(1u16.to_be_bytes()); // numberOfHMetrics
    hhea
}

/// One `longHorMetric` plus a left-side-bearing per remaining glyph, which is
/// the compact form `numberOfHMetrics == 1` selects.
fn build_hmtx(info: &CffInfo) -> Vec<u8> {
    let mut hmtx = Vec::new();
    hmtx.extend(0u16.to_be_bytes()); // advanceWidth[0]
    hmtx.extend(0i16.to_be_bytes()); // lsb[0]
    for _ in 1..info.n_glyphs {
        hmtx.extend(0i16.to_be_bytes());
    }
    hmtx
}

fn build_maxp(info: &CffInfo) -> Vec<u8> {
    let mut maxp = Vec::new();
    maxp.extend(0x0000_5000u32.to_be_bytes()); // version 0.5 — CFF outlines
    maxp.extend((info.n_glyphs as u16).to_be_bytes());
    maxp
}

/// `post` format 2.0 carrying the CFF's glyph names.
fn build_post(info: &CffInfo) -> Vec<u8> {
    let mut post = Vec::new();
    post.extend(0x0002_0000u32.to_be_bytes()); // version 2.0
    post.extend(0i32.to_be_bytes()); // italicAngle
    post.extend(0i16.to_be_bytes()); // underlinePosition
    post.extend(0i16.to_be_bytes()); // underlineThickness
    post.extend(0u32.to_be_bytes()); // isFixedPitch
    for _ in 0..4 {
        post.extend(0u32.to_be_bytes()); // min/max mem usage
    }
    post.extend((info.n_glyphs as u16).to_be_bytes());
    // Every glyph gets an index into the custom-name array (>= 258); the
    // standard Macintosh set is not worth matching against.
    let mut names = Vec::new();
    for (i, name) in info.names.iter().enumerate() {
        post.extend(((258 + i) as u16).to_be_bytes());
        let truncated = &name[..name.len().min(255)];
        names.push(truncated.len() as u8);
        names.extend_from_slice(truncated);
    }
    post.extend(names);
    post
}

/// Read the header, INDEXes, Top DICT and charset.
fn read_cff_info(data: &[u8]) -> Option<CffInfo> {
    let mut r = Reader { data, pos: 0 };
    let _major = r.u8()?;
    let _minor = r.u8()?;
    let hdr_size = r.u8()? as usize;
    let _off_size = r.u8()?;
    r.pos = hdr_size;

    let _names = r.index()?;
    let top_dicts = r.index()?;
    let strings = r.index()?;
    let dict = parse_dict(top_dicts.first()?);

    let charstrings_off = dict.iter().find(|(op, _)| *op == 17)?.1.first().copied()? as usize;
    let n_glyphs = Reader {
        data,
        pos: charstrings_off,
    }
    .index()?
    .len();

    // FontMatrix (12 7) defaults to 0.001 => 1000 upem. It is a real number,
    // which `parse_dict` discards, so read it separately.
    let units_per_em = read_font_matrix_upem(top_dicts.first()?).unwrap_or(1000);

    // FontBBox (5); default all-zero.
    let bbox = dict
        .iter()
        .find(|(op, _)| *op == 5)
        .map(|(_, v)| {
            let g = |i: usize| v.get(i).copied().unwrap_or(0).clamp(-32768, 32767) as i16;
            [g(0), g(1), g(2), g(3)]
        })
        .unwrap_or([0, 0, 1000, 1000]);

    let is_cid = dict.iter().any(|(op, _)| *op == 0x0c1e);
    let names = if is_cid {
        // A CID font's charset holds CIDs, not names; the CID→GID map is the
        // right tool there, and `post` would be meaningless.
        Vec::new()
    } else {
        let charset_off = dict
            .iter()
            .find(|(op, _)| *op == 15)
            .and_then(|(_, v)| v.first().copied());
        match charset_off {
            // Offsets 0/1/2 are the predefined charsets; ISOAdobe (0) means
            // "SID == GID" over the standard strings, which is the common
            // case for a font that ships no custom charset.
            Some(off) if off > 2 => {
                let sids = parse_charset(data, off as usize, n_glyphs)?;
                sids.iter().map(|&sid| sid_to_name(sid, &strings)).collect()
            }
            _ => (0..n_glyphs)
                .map(|gid| sid_to_name(gid as u16, &strings))
                .collect(),
        }
    };

    // Encoding (op 16): a *custom* one (offset > 1) is parsed directly. When the
    // operator is absent or names the predefined Standard encoding (offset 0),
    // that Standard encoding *is* the font's built-in one, and a **symbolic**
    // simple font reaches its glyphs only through it: the PDF `/Encoding` gives
    // such a font no base names (`BaseEncoding::Symbolic`), and `/Differences`
    // typically covers only the high codes. Leaving this empty left every ASCII
    // code unresolvable, so whole documents of subset Type1C text rendered
    // blank. Materialize it as code -> name -> GID against the charset. Expert
    // (offset 1) is left alone; no corpus file needs it.
    let encoding = if is_cid {
        Vec::new()
    } else {
        match dict
            .iter()
            .find(|(op, _)| *op == 16)
            .and_then(|(_, v)| v.first().copied())
        {
            Some(off) if off > 1 => {
                parse_cff_encoding(data, off as usize, n_glyphs).unwrap_or_default()
            }
            Some(1) => Vec::new(),
            _ => standard_encoding_pairs(&names),
        }
    };

    Some(CffInfo {
        n_glyphs,
        units_per_em,
        bbox,
        names,
        encoding,
    })
}

/// The predefined Adobe Standard encoding as `(code, gid)` pairs, resolved
/// through this font's charset names.
///
/// A CFF with no `Encoding` operator uses the Standard encoding by definition
/// (ISO/IEC 14496-22 / the CFF spec's default), so this reconstructs the byte
/// encoding such a font actually has.
fn standard_encoding_pairs(names: &[Vec<u8>]) -> Vec<(u8, u16)> {
    // Walk the charset once and look each glyph name up in a process-wide
    // reverse index of the Standard encoding. Building any per-parse map here
    // instead is enough work on a full Latin face to trip the font cache's
    // cheap-parse threshold, and this runs for every bare CFF.
    static STD_BY_NAME: std::sync::OnceLock<std::collections::HashMap<&'static [u8], u8>> =
        std::sync::OnceLock::new();
    let index = STD_BY_NAME.get_or_init(|| {
        let mut m = std::collections::HashMap::with_capacity(256);
        for code in 0u16..=255 {
            if let Some(name) = crate::encoding::builtin_glyph_name(
                crate::encoding::BaseEncoding::Standard,
                code as u8,
            ) {
                m.entry(name).or_insert(code as u8);
            }
        }
        m
    });

    let mut out = Vec::new();
    // GID 0 is `.notdef`; a code that resolves there carries no glyph.
    for (gid, name) in names.iter().enumerate().skip(1) {
        let Ok(gid) = u16::try_from(gid) else { break };
        if let Some(&code) = index.get(name.as_slice()) {
            out.push((code, gid));
        }
    }
    out.sort_unstable_by_key(|&(code, _)| code);
    out.dedup_by_key(|&mut (code, _)| code);
    out
}

/// Resolve a SID: below 391 it names a standard string, above it indexes the
/// font's String INDEX.
fn sid_to_name(sid: u16, strings: &[&[u8]]) -> Vec<u8> {
    let sid = sid as usize;
    if sid < CFF_STANDARD_STRINGS.len() {
        CFF_STANDARD_STRINGS[sid].to_vec()
    } else {
        strings
            .get(sid - CFF_STANDARD_STRINGS.len())
            .map(|s| s.to_vec())
            .unwrap_or_default()
    }
}

/// `FontMatrix` (op `12 7`) is a real-number operand, so scan for it by hand.
/// Its first element is 1/upem.
fn read_font_matrix_upem(dict: &[u8]) -> Option<u16> {
    let mut operands: Vec<f64> = Vec::new();
    let mut i = 0usize;
    while i < dict.len() {
        let b0 = dict[i];
        match b0 {
            12 => {
                let op2 = *dict.get(i + 1)?;
                i += 2;
                if op2 == 7 {
                    let s = *operands.first()?;
                    if s > 0.0 {
                        return Some(((1.0 / s).round() as i64).clamp(16, 16384) as u16);
                    }
                    return None;
                }
                operands.clear();
            }
            0..=21 => {
                i += 1;
                operands.clear();
            }
            28 => {
                i += 3;
                operands.push(0.0);
            }
            29 => {
                i += 5;
                operands.push(0.0);
            }
            30 => {
                // Nibble-encoded real: decode enough to recover the value.
                i += 1;
                let mut s = String::new();
                'outer: while i < dict.len() {
                    let b = dict[i];
                    i += 1;
                    for nib in [b >> 4, b & 0x0f] {
                        match nib {
                            0..=9 => s.push((b'0' + nib) as char),
                            0xa => s.push('.'),
                            0xb => s.push('E'),
                            0xc => s.push_str("E-"),
                            0xe => s.push('-'),
                            0xf => break 'outer,
                            _ => {}
                        }
                    }
                }
                operands.push(s.parse().unwrap_or(0.0));
            }
            32..=246 => {
                operands.push(b0 as f64 - 139.0);
                i += 1;
            }
            247..=250 => {
                let b1 = *dict.get(i + 1)? as f64;
                operands.push((b0 as f64 - 247.0) * 256.0 + b1 + 108.0);
                i += 2;
            }
            251..=254 => {
                let b1 = *dict.get(i + 1)? as f64;
                operands.push(-(b0 as f64 - 251.0) * 256.0 - b1 - 108.0);
                i += 2;
            }
            _ => i += 1,
        }
    }
    None
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let hi = self.u8()? as u16;
        let lo = self.u8()? as u16;
        Some((hi << 8) | lo)
    }
    fn offset(&mut self, size: u8) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..size {
            v = (v << 8) | self.u8()? as u32;
        }
        Some(v)
    }

    /// Read a CFF INDEX, returning each element's bytes and leaving `pos`
    /// just past the structure.
    fn index(&mut self) -> Option<Vec<&'a [u8]>> {
        let count = self.u16()? as usize;
        if count == 0 {
            return Some(Vec::new());
        }
        let off_size = self.u8()?;
        if !(1..=4).contains(&off_size) {
            return None;
        }
        let mut offsets = Vec::with_capacity(count + 1);
        for _ in 0..=count {
            offsets.push(self.offset(off_size)? as usize);
        }
        // Offsets are 1-based from just before the data block.
        let base = self.pos.checked_sub(1)?;
        let mut out = Vec::with_capacity(count);
        for w in offsets.windows(2) {
            let (s, e) = (base.checked_add(w[0])?, base.checked_add(w[1])?);
            if s > e || e > self.data.len() {
                return None;
            }
            out.push(&self.data[s..e]);
        }
        self.pos = base.checked_add(*offsets.last()?)?;
        Some(out)
    }
}

/// Parse a CFF DICT into `(operator, operands)` pairs. Two-byte operators
/// (`12 x`) are keyed as `0x0c00 | x`. Real numbers are not needed for the
/// charset, so they are consumed and discarded.
fn parse_dict(data: &[u8]) -> Vec<(u16, Vec<i32>)> {
    let mut out = Vec::new();
    let mut operands: Vec<i32> = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let b0 = data[i];
        match b0 {
            0..=21 => {
                let op = if b0 == 12 {
                    i += 1;
                    0x0c00u16 | *data.get(i).unwrap_or(&0) as u16
                } else {
                    b0 as u16
                };
                i += 1;
                out.push((op, std::mem::take(&mut operands)));
            }
            28 => {
                let v = i16::from_be_bytes([
                    *data.get(i + 1).unwrap_or(&0),
                    *data.get(i + 2).unwrap_or(&0),
                ]);
                operands.push(v as i32);
                i += 3;
            }
            29 => {
                let v = i32::from_be_bytes([
                    *data.get(i + 1).unwrap_or(&0),
                    *data.get(i + 2).unwrap_or(&0),
                    *data.get(i + 3).unwrap_or(&0),
                    *data.get(i + 4).unwrap_or(&0),
                ]);
                operands.push(v);
                i += 5;
            }
            // 30: a real number, nibble-encoded and terminated by 0xf.
            30 => {
                i += 1;
                while i < data.len() {
                    let b = data[i];
                    i += 1;
                    if b & 0x0f == 0x0f || b >> 4 == 0x0f {
                        break;
                    }
                }
                operands.push(0);
            }
            32..=246 => {
                operands.push(b0 as i32 - 139);
                i += 1;
            }
            247..=250 => {
                let b1 = *data.get(i + 1).unwrap_or(&0) as i32;
                operands.push((b0 as i32 - 247) * 256 + b1 + 108);
                i += 2;
            }
            251..=254 => {
                let b1 = *data.get(i + 1).unwrap_or(&0) as i32;
                operands.push(-(b0 as i32 - 251) * 256 - b1 - 108);
                i += 2;
            }
            _ => i += 1,
        }
    }
    out
}

/// A Private DICT operator defined by the CFF spec (Appendix H). Anything
/// else in a Private DICT marks it malformed for [`sanitize_private_dicts`].
fn private_dict_op_known(op: u16) -> bool {
    matches!(op,
        // BlueValues..StemSnapV region + widths + Subrs.
        6..=11 | 19 | 20 | 21
        // BlueScale BlueShift BlueFuzz StemSnapH StemSnapV ForceBold
        | 0x0c09..=0x0c0e
        // LanguageGroup ExpansionFactor initialRandomSeed
        | 0x0c11..=0x0c13)
}

/// One lexed DICT token: how far it reached and what it was.
enum DictToken {
    /// An operand (integer or real) ending at `.0`.
    Operand(usize),
    /// An operator `.1` ending at `.0`.
    Operator(usize, u16),
    /// An undecodable byte (bad encoding, unterminated real, truncation).
    Bad,
}

/// Lex the single DICT token starting at `i`.
fn dict_token(pd: &[u8], i: usize) -> DictToken {
    let b0 = pd[i];
    match b0 {
        12 => match pd.get(i + 1) {
            Some(&b1) => DictToken::Operator(i + 2, 0x0c00 | b1 as u16),
            None => DictToken::Bad,
        },
        0..=21 => DictToken::Operator(i + 1, b0 as u16),
        28 if i + 2 < pd.len() => DictToken::Operand(i + 3),
        29 if i + 4 < pd.len() => DictToken::Operand(i + 5),
        30 => {
            // Real number: nibbles, terminated by an 0xf nibble. Each nibble
            // must be a digit, '.', an exponent marker, or '-' — a producer
            // writing garbage here is exactly what sanitization exists for.
            let mut j = i + 1;
            while j < pd.len() {
                let (hi, lo) = (pd[j] >> 4, pd[j] & 0x0f);
                for nib in [hi, lo] {
                    if nib == 0x0d {
                        return DictToken::Bad; // reserved nibble
                    }
                }
                j += 1;
                if hi == 0x0f || lo == 0x0f {
                    return DictToken::Operand(j);
                }
            }
            DictToken::Bad // ran off the dict without a terminator
        }
        32..=246 => DictToken::Operand(i + 1),
        247..=254 if i + 1 < pd.len() => DictToken::Operand(i + 2),
        _ => DictToken::Bad,
    }
}

/// Whether `pd` parses cleanly as a Private DICT: every token decodes, every
/// operator is a known Private-DICT operator with at least one operand, and
/// no operands trail the last operator.
fn private_dict_is_clean(pd: &[u8]) -> bool {
    let mut i = 0;
    let mut operands = 0usize;
    while i < pd.len() {
        match dict_token(pd, i) {
            DictToken::Operand(next) => {
                operands += 1;
                if operands > 48 {
                    return false;
                }
                i = next;
            }
            DictToken::Operator(next, op) => {
                if !private_dict_op_known(op) || operands == 0 {
                    return false;
                }
                operands = 0;
                i = next;
            }
            DictToken::Bad => return false,
        }
    }
    operands == 0
}

/// Rewrite a malformed Private DICT **in place** (same length) keeping only
/// the entries that decode cleanly, so a strict reader no longer rejects the
/// whole dict — and with it every glyph outline.
///
/// Recovery is FreeType-style: scan tokens, and on an undecodable byte drop
/// the pending operands and resynchronize one byte later. A surviving entry
/// is kept **verbatim** (its raw operand+operator bytes), so valid real
/// numbers pass through untouched. The bytes freed by dropped garbage become
/// a `BlueFuzz` filler whose zero-valued real operand is nibble-padded to
/// make the rewrite exactly the original length — no offset in the file
/// moves. Returns whether the slice was modified.
fn sanitize_private_dict_slice(pd: &mut [u8]) -> bool {
    if private_dict_is_clean(pd) {
        return false;
    }
    // Recover clean entries: (raw span, operator).
    let mut segments: Vec<(std::ops::Range<usize>, u16)> = Vec::new();
    let mut i = 0;
    let mut run_start = 0;
    let mut operands = 0usize;
    while i < pd.len() {
        match dict_token(pd, i) {
            DictToken::Operand(next) => {
                operands += 1;
                i = next;
            }
            DictToken::Operator(next, op) => {
                if private_dict_op_known(op) && operands > 0 && operands <= 48 {
                    segments.push((run_start..next, op));
                }
                i = next;
                run_start = i;
                operands = 0;
            }
            DictToken::Bad => {
                i += 1;
                run_start = i;
                operands = 0;
            }
        }
    }

    let kept = |segs: &[(std::ops::Range<usize>, u16)]| -> usize {
        segs.iter().map(|(r, _)| r.len()).sum()
    };
    // The filler needs `1e [00…] 0f` (≥2 bytes) + the 2-byte BlueFuzz
    // operator: a leftover of 1..=3 bytes cannot host it, so shed the
    // lowest-value entries (hints before widths before Subrs) until the
    // leftover is 0 or ≥4.
    let priority = |op: u16| match op {
        19 => 2u8,    // Subrs: local subroutines — charstrings need these
        20 | 21 => 1, // default/nominalWidthX — width decoding needs these
        _ => 0,       // hints: irrelevant to unhinted outlines
    };
    loop {
        let leftover = pd.len() - kept(&segments);
        if leftover == 0 || leftover >= 4 {
            break;
        }
        let Some(drop_at) = segments
            .iter()
            .enumerate()
            .min_by_key(|(idx, (_, op))| (priority(*op), std::cmp::Reverse(*idx)))
            .map(|(idx, _)| idx)
        else {
            return false; // nothing recoverable and the dict is tiny: give up
        };
        segments.remove(drop_at);
    }

    let mut out = Vec::with_capacity(pd.len());
    for (r, _) in &segments {
        out.extend_from_slice(&pd[r.clone()]);
    }
    let leftover = pd.len() - out.len();
    if leftover > 0 {
        // `BlueFuzz 0`, its real operand zero-padded to spend the leftover.
        out.push(0x1e);
        out.extend(std::iter::repeat_n(0x00u8, leftover - 4));
        out.extend([0x0f, 0x0c, 0x0b]);
    }
    debug_assert_eq!(out.len(), pd.len());
    pd.copy_from_slice(&out);
    true
}

/// Repair malformed Private DICTs inside a bare CFF, in place.
///
/// A subsetter that emits garbage real-number hint data (broken BlueValues /
/// BlueScale nibbles) produces a font that lenient engines (FreeType — so
/// PDFium, MuPDF) render fine but a strict reader rejects wholesale: the
/// Private DICT fails to parse, so **every glyph outline returns empty** and
/// the page's text silently vanishes. Locate each Private DICT (top-level,
/// and per-FD for a CID font) and sanitize it; every rewrite is
/// length-preserving, so no CFF offset moves. Returns whether anything
/// changed.
pub(crate) fn sanitize_private_dicts(cff: &mut [u8]) -> bool {
    // Collect (offset, size) pairs immutably first.
    let mut privates: Vec<(usize, usize)> = Vec::new();
    {
        let data: &[u8] = cff;
        let collect = |dict: &[(u16, Vec<i32>)], out: &mut Vec<(usize, usize)>| {
            if let Some((_, v)) = dict.iter().find(|(op, _)| *op == 18)
                && let (Some(&size), Some(&off)) = (v.first(), v.get(1))
                && size > 0
                && off > 0
                && let (Ok(size), Ok(off)) = (usize::try_from(size), usize::try_from(off))
                && off.checked_add(size).is_some_and(|end| end <= data.len())
            {
                out.push((off, size));
            }
        };
        let mut r = Reader { data, pos: 0 };
        let Some(hdr_size) = (|| {
            r.u8()?;
            r.u8()?;
            let h = r.u8()?;
            r.u8()?;
            Some(h as usize)
        })() else {
            return false;
        };
        r.pos = hdr_size;
        let (Some(_), Some(top_dicts)) = (r.index(), r.index()) else {
            return false;
        };
        let Some(top) = top_dicts.first().map(|d| parse_dict(d)) else {
            return false;
        };
        collect(&top, &mut privates);
        // A CID font keeps its Private DICTs in the FDArray's font dicts.
        if let Some((_, v)) = top.iter().find(|(op, _)| *op == 0x0c24)
            && let Some(&fd_off) = v.first()
            && let Ok(fd_off) = usize::try_from(fd_off)
            && fd_off < data.len()
            && let Some(fd_dicts) = (Reader { data, pos: fd_off }).index()
        {
            for fd in fd_dicts {
                collect(&parse_dict(fd), &mut privates);
            }
        }
    }
    let mut changed = false;
    for (off, size) in privates {
        changed |= sanitize_private_dict_slice(&mut cff[off..off + size]);
    }
    changed
}

/// The 391 CFF standard strings (CFF spec Appendix A). A charset SID below
/// this count names one of these; higher SIDs index the font's own String
/// INDEX.
pub(crate) const CFF_STANDARD_STRINGS: [&[u8]; 391] = [
    b".notdef",
    b"space",
    b"exclam",
    b"quotedbl",
    b"numbersign",
    b"dollar",
    b"percent",
    b"ampersand",
    b"quoteright",
    b"parenleft",
    b"parenright",
    b"asterisk",
    b"plus",
    b"comma",
    b"hyphen",
    b"period",
    b"slash",
    b"zero",
    b"one",
    b"two",
    b"three",
    b"four",
    b"five",
    b"six",
    b"seven",
    b"eight",
    b"nine",
    b"colon",
    b"semicolon",
    b"less",
    b"equal",
    b"greater",
    b"question",
    b"at",
    b"A",
    b"B",
    b"C",
    b"D",
    b"E",
    b"F",
    b"G",
    b"H",
    b"I",
    b"J",
    b"K",
    b"L",
    b"M",
    b"N",
    b"O",
    b"P",
    b"Q",
    b"R",
    b"S",
    b"T",
    b"U",
    b"V",
    b"W",
    b"X",
    b"Y",
    b"Z",
    b"bracketleft",
    b"backslash",
    b"bracketright",
    b"asciicircum",
    b"underscore",
    b"quoteleft",
    b"a",
    b"b",
    b"c",
    b"d",
    b"e",
    b"f",
    b"g",
    b"h",
    b"i",
    b"j",
    b"k",
    b"l",
    b"m",
    b"n",
    b"o",
    b"p",
    b"q",
    b"r",
    b"s",
    b"t",
    b"u",
    b"v",
    b"w",
    b"x",
    b"y",
    b"z",
    b"braceleft",
    b"bar",
    b"braceright",
    b"asciitilde",
    b"exclamdown",
    b"cent",
    b"sterling",
    b"fraction",
    b"yen",
    b"florin",
    b"section",
    b"currency",
    b"quotesingle",
    b"quotedblleft",
    b"guillemotleft",
    b"guilsinglleft",
    b"guilsinglright",
    b"fi",
    b"fl",
    b"endash",
    b"dagger",
    b"daggerdbl",
    b"periodcentered",
    b"paragraph",
    b"bullet",
    b"quotesinglbase",
    b"quotedblbase",
    b"quotedblright",
    b"guillemotright",
    b"ellipsis",
    b"perthousand",
    b"questiondown",
    b"grave",
    b"acute",
    b"circumflex",
    b"tilde",
    b"macron",
    b"breve",
    b"dotaccent",
    b"dieresis",
    b"ring",
    b"cedilla",
    b"hungarumlaut",
    b"ogonek",
    b"caron",
    b"emdash",
    b"AE",
    b"ordfeminine",
    b"Lslash",
    b"Oslash",
    b"OE",
    b"ordmasculine",
    b"ae",
    b"dotlessi",
    b"lslash",
    b"oslash",
    b"oe",
    b"germandbls",
    b"onesuperior",
    b"logicalnot",
    b"mu",
    b"trademark",
    b"Eth",
    b"onehalf",
    b"plusminus",
    b"Thorn",
    b"onequarter",
    b"divide",
    b"brokenbar",
    b"degree",
    b"thorn",
    b"threequarters",
    b"twosuperior",
    b"registered",
    b"minus",
    b"eth",
    b"multiply",
    b"threesuperior",
    b"copyright",
    b"Aacute",
    b"Acircumflex",
    b"Adieresis",
    b"Agrave",
    b"Aring",
    b"Atilde",
    b"Ccedilla",
    b"Eacute",
    b"Ecircumflex",
    b"Edieresis",
    b"Egrave",
    b"Iacute",
    b"Icircumflex",
    b"Idieresis",
    b"Igrave",
    b"Ntilde",
    b"Oacute",
    b"Ocircumflex",
    b"Odieresis",
    b"Ograve",
    b"Otilde",
    b"Scaron",
    b"Uacute",
    b"Ucircumflex",
    b"Udieresis",
    b"Ugrave",
    b"Yacute",
    b"Ydieresis",
    b"Zcaron",
    b"aacute",
    b"acircumflex",
    b"adieresis",
    b"agrave",
    b"aring",
    b"atilde",
    b"ccedilla",
    b"eacute",
    b"ecircumflex",
    b"edieresis",
    b"egrave",
    b"iacute",
    b"icircumflex",
    b"idieresis",
    b"igrave",
    b"ntilde",
    b"oacute",
    b"ocircumflex",
    b"odieresis",
    b"ograve",
    b"otilde",
    b"scaron",
    b"uacute",
    b"ucircumflex",
    b"udieresis",
    b"ugrave",
    b"yacute",
    b"ydieresis",
    b"zcaron",
    b"exclamsmall",
    b"Hungarumlautsmall",
    b"dollaroldstyle",
    b"dollarsuperior",
    b"ampersandsmall",
    b"Acutesmall",
    b"parenleftsuperior",
    b"parenrightsuperior",
    b"twodotenleader",
    b"onedotenleader",
    b"zerooldstyle",
    b"oneoldstyle",
    b"twooldstyle",
    b"threeoldstyle",
    b"fouroldstyle",
    b"fiveoldstyle",
    b"sixoldstyle",
    b"sevenoldstyle",
    b"eightoldstyle",
    b"nineoldstyle",
    b"commasuperior",
    b"threequartersemdash",
    b"periodsuperior",
    b"questionsmall",
    b"asuperior",
    b"bsuperior",
    b"centsuperior",
    b"dsuperior",
    b"esuperior",
    b"isuperior",
    b"lsuperior",
    b"msuperior",
    b"nsuperior",
    b"osuperior",
    b"rsuperior",
    b"ssuperior",
    b"tsuperior",
    b"ff",
    b"ffi",
    b"ffl",
    b"parenleftinferior",
    b"parenrightinferior",
    b"Circumflexsmall",
    b"hyphensuperior",
    b"Gravesmall",
    b"Asmall",
    b"Bsmall",
    b"Csmall",
    b"Dsmall",
    b"Esmall",
    b"Fsmall",
    b"Gsmall",
    b"Hsmall",
    b"Ismall",
    b"Jsmall",
    b"Ksmall",
    b"Lsmall",
    b"Msmall",
    b"Nsmall",
    b"Osmall",
    b"Psmall",
    b"Qsmall",
    b"Rsmall",
    b"Ssmall",
    b"Tsmall",
    b"Usmall",
    b"Vsmall",
    b"Wsmall",
    b"Xsmall",
    b"Ysmall",
    b"Zsmall",
    b"colonmonetary",
    b"onefitted",
    b"rupiah",
    b"Tildesmall",
    b"exclamdownsmall",
    b"centoldstyle",
    b"Lslashsmall",
    b"Scaronsmall",
    b"Zcaronsmall",
    b"Dieresissmall",
    b"Brevesmall",
    b"Caronsmall",
    b"Dotaccentsmall",
    b"Macronsmall",
    b"figuredash",
    b"hypheninferior",
    b"Ogoneksmall",
    b"Ringsmall",
    b"Cedillasmall",
    b"questiondownsmall",
    b"oneeighth",
    b"threeeighths",
    b"fiveeighths",
    b"seveneighths",
    b"onethird",
    b"twothirds",
    b"zerosuperior",
    b"foursuperior",
    b"fivesuperior",
    b"sixsuperior",
    b"sevensuperior",
    b"eightsuperior",
    b"ninesuperior",
    b"zeroinferior",
    b"oneinferior",
    b"twoinferior",
    b"threeinferior",
    b"fourinferior",
    b"fiveinferior",
    b"sixinferior",
    b"seveninferior",
    b"eightinferior",
    b"nineinferior",
    b"centinferior",
    b"dollarinferior",
    b"periodinferior",
    b"commainferior",
    b"Agravesmall",
    b"Aacutesmall",
    b"Acircumflexsmall",
    b"Atildesmall",
    b"Adieresissmall",
    b"Aringsmall",
    b"AEsmall",
    b"Ccedillasmall",
    b"Egravesmall",
    b"Eacutesmall",
    b"Ecircumflexsmall",
    b"Edieresissmall",
    b"Igravesmall",
    b"Iacutesmall",
    b"Icircumflexsmall",
    b"Idieresissmall",
    b"Ethsmall",
    b"Ntildesmall",
    b"Ogravesmall",
    b"Oacutesmall",
    b"Ocircumflexsmall",
    b"Otildesmall",
    b"Odieresissmall",
    b"OEsmall",
    b"Oslashsmall",
    b"Ugravesmall",
    b"Uacutesmall",
    b"Ucircumflexsmall",
    b"Udieresissmall",
    b"Yacutesmall",
    b"Thornsmall",
    b"Ydieresissmall",
    b"001.000",
    b"001.001",
    b"001.002",
    b"001.003",
    b"Black",
    b"Bold",
    b"Book",
    b"Light",
    b"Medium",
    b"Regular",
    b"Roman",
    b"Semibold",
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// A CFF with no `Encoding` operator uses the predefined Standard encoding.
    /// A **symbolic** simple font reaches its glyphs only through that built-in
    /// encoding (the PDF `/Encoding` gives it no base glyph names), so leaving
    /// it unmaterialized mapped every ASCII code to `.notdef` and rendered whole
    /// documents of subset Type1C text blank.
    #[test]
    fn absent_cff_encoding_materializes_the_standard_one() {
        // GID 0 is .notdef; the rest are named as in a subset charset.
        let names: Vec<Vec<u8>> = [b".notdef".as_slice(), b"A", b"B", b"space", b"zero"]
            .iter()
            .map(|n| n.to_vec())
            .collect();
        let pairs = standard_encoding_pairs(&names);

        let gid_of = |code: u8| pairs.iter().find(|(c, _)| *c == code).map(|(_, g)| *g);
        assert_eq!(gid_of(b'A'), Some(1), "StandardEncoding 65 -> /A");
        assert_eq!(gid_of(b'B'), Some(2));
        assert_eq!(gid_of(b' '), Some(3), "StandardEncoding 32 -> /space");
        assert_eq!(gid_of(b'0'), Some(4), "StandardEncoding 48 -> /zero");
        // Codes whose glyph the subset does not carry stay unmapped rather than
        // resolving to some other glyph.
        assert_eq!(gid_of(b'Z'), None);
        // .notdef is never a mapping target.
        assert!(pairs.iter().all(|(_, g)| *g != 0));
    }

    #[test]
    fn a_non_cff_is_declined() {
        assert!(cid_to_gid_from_cff(b"not a cff at all").is_none());
        assert!(cid_to_gid_from_cff(&[]).is_none());
    }

    #[test]
    fn charset_format_0_maps_each_glyph() {
        // 4 glyphs: GID 0 implicit, then CIDs 3, 11, 12.
        let data = [0u8, 0, 3, 0, 11, 0, 12];
        let got = parse_charset(&data, 0, 4).expect("format 0");
        assert_eq!(got, vec![0, 3, 11, 12]);
    }

    #[test]
    fn charset_format_1_expands_ranges() {
        // format 1: first=5, nLeft=2 -> CIDs 5,6,7 for GIDs 1..=3.
        let data = [1u8, 0, 5, 2];
        let got = parse_charset(&data, 0, 4).expect("format 1");
        assert_eq!(got, vec![0, 5, 6, 7]);
    }

    #[test]
    fn charset_format_2_expands_wide_ranges() {
        // format 2: 16-bit nLeft.
        let data = [2u8, 0, 100, 0, 3];
        let got = parse_charset(&data, 0, 5).expect("format 2");
        assert_eq!(got, vec![0, 100, 101, 102, 103]);
    }

    #[test]
    fn charset_stops_at_the_glyph_count() {
        // A range claiming more glyphs than exist must not over-run.
        let data = [1u8, 0, 5, 200];
        let got = parse_charset(&data, 0, 3).expect("bounded");
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn encoding_format_0_maps_codes_in_order() {
        // format 0: 3 codes → GIDs 1,2,3.
        let data = [0u8, 3, 0x41, 0x42, 0x43];
        let got = parse_cff_encoding(&data, 0, 4).expect("format 0");
        assert_eq!(got, vec![(0x41, 1), (0x42, 2), (0x43, 3)]);
    }

    #[test]
    fn encoding_format_1_expands_code_ranges() {
        // format 1: one range first=0x30, nLeft=2 → codes 0x30,0x31,0x32.
        let data = [1u8, 1, 0x30, 2];
        let got = parse_cff_encoding(&data, 0, 4).expect("format 1");
        assert_eq!(got, vec![(0x30, 1), (0x31, 2), (0x32, 3)]);
    }

    #[test]
    fn encoding_stops_at_the_glyph_count() {
        // More codes listed than the font has glyphs must not over-run.
        let data = [0u8, 8, 1, 2, 3, 4, 5, 6, 7, 8];
        let got = parse_cff_encoding(&data, 0, 3).expect("bounded");
        assert_eq!(got.len(), 2, "only GIDs 1 and 2 exist");
    }

    /// A CFF INDEX around `items` (32-bit offsets — simplest to emit).
    fn index(items: &[&[u8]]) -> Vec<u8> {
        let mut out = (items.len() as u16).to_be_bytes().to_vec();
        if items.is_empty() {
            return out;
        }
        out.push(4);
        let mut off = 1u32;
        out.extend(off.to_be_bytes());
        for it in items {
            off += it.len() as u32;
            out.extend(off.to_be_bytes());
        }
        for it in items {
            out.extend_from_slice(it);
        }
        out
    }

    /// A minimal CID-keyed CFF: `ROS`, a 3-glyph CharStrings INDEX, and a
    /// charset mapping GID 1→CID 100, GID 2→CID 200.
    fn minimal_cid_cff() -> Vec<u8> {
        let name = index(&[b"CIDFont"]);
        let charstrings = index(&[&[0x0e][..], &[0x0e], &[0x0e]]); // 3× endchar
        let charset = vec![0u8, 0, 100, 0, 200]; // format 0

        // Top DICT: ROS (12 30) + charset (15) + CharStrings (17), with 16-bit
        // offset operands (28 hi lo) so the dict length is fixed at 13.
        let dict_len = 13usize;
        let top_shell = index(&[&vec![0u8; dict_len]]);
        let charset_off = 4 + name.len() + top_shell.len();
        let charstrings_off = charset_off + charset.len();
        let mut dict = vec![139u8, 139, 139, 12, 30]; // ROS with dummy operands
        dict.extend([28, (charset_off >> 8) as u8, charset_off as u8, 15]);
        dict.extend([28, (charstrings_off >> 8) as u8, charstrings_off as u8, 17]);
        assert_eq!(dict.len(), dict_len);
        let top = index(&[&dict]);

        let mut out = vec![1u8, 0, 4, 1];
        out.extend(name);
        out.extend(top);
        out.extend(charset);
        out.extend(charstrings);
        out
    }

    /// Wrap `cff` bytes in a one-table SFNT (`OTTO` + a `CFF ` record).
    fn sfnt_wrap(cff: &[u8]) -> Vec<u8> {
        let mut out = b"OTTO".to_vec();
        out.extend(1u16.to_be_bytes()); // numTables
        out.extend([0, 0, 0, 0, 0, 0]); // searchRange/entrySelector/rangeShift
        let cff_off = 12 + 16;
        out.extend(b"CFF ");
        out.extend(0u32.to_be_bytes()); // checksum
        out.extend((cff_off as u32).to_be_bytes());
        out.extend((cff.len() as u32).to_be_bytes());
        out.extend_from_slice(cff);
        out
    }

    #[test]
    fn cid_to_gid_reads_a_bare_cid_cff() {
        let cff = minimal_cid_cff();
        let map = cid_to_gid_from_cff(&cff).expect("CID-keyed CFF yields a map");
        assert_eq!(map.gid(100), 1);
        assert_eq!(map.gid(200), 2);
        assert_eq!(map.gid(50), 0, "unmapped CID → .notdef");
    }

    #[test]
    fn cid_to_gid_unwraps_an_sfnt_wrapped_cid_cff() {
        // Producers (CAJ/CNKI, many CJK) embed the CID-keyed CFF inside an
        // OpenType `CFF ` table rather than bare. The charset offset is then
        // relative to that table, so the SFNT must be unwrapped first — else
        // the header misparses and the map silently falls back to Identity.
        let cff = minimal_cid_cff();
        let sfnt = sfnt_wrap(&cff);
        assert!(cff_table(&sfnt).is_some(), "the CFF table is located");
        let map = cid_to_gid_from_cff(&sfnt).expect("SFNT-wrapped CID CFF yields a map");
        assert_eq!(map.gid(100), 1, "same mapping as the bare form");
        assert_eq!(map.gid(200), 2);
    }
}
