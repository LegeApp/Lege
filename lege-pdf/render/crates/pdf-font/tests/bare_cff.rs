#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Bare CFF `/FontFile3` (`Type1C` / `CIDFontType0C`).
//!
//! PDF embeds CFF raw, not wrapped in an SFNT. Skrifa reads SFNT only, so
//! `FontProgram` wraps a bare CFF before parsing. The fixture here is the
//! `CFF ` table lifted straight out of a bundled Foxit face — a genuine bare
//! CFF, exactly what a PDF embeds, with no new files to license.

use std::sync::Arc;

use pdf_font::{FontProgram, StandardFont, is_bare_cff, wrap_bare_cff};

/// Pull the `CFF ` table out of an SFNT — the inverse of what the wrapper
/// does, giving us the bare CFF a PDF would have embedded.
fn extract_cff_table(sfnt: &[u8]) -> Vec<u8> {
    let n = u16::from_be_bytes([sfnt[4], sfnt[5]]) as usize;
    for i in 0..n {
        let rec = 12 + i * 16;
        if &sfnt[rec..rec + 4] == b"CFF " {
            let off = u32::from_be_bytes(sfnt[rec + 8..rec + 12].try_into().unwrap()) as usize;
            let len = u32::from_be_bytes(sfnt[rec + 12..rec + 16].try_into().unwrap()) as usize;
            return sfnt[off..off + len].to_vec();
        }
    }
    panic!("bundled face has no CFF table");
}

fn foxit_cff() -> Vec<u8> {
    extract_cff_table(&StandardFont::Helvetica.program_data())
}

#[test]
fn a_bare_cff_is_recognized_and_an_sfnt_is_not() {
    let cff = foxit_cff();
    assert!(is_bare_cff(&cff), "raw CFF");
    assert!(
        !is_bare_cff(&StandardFont::Helvetica.program_data()),
        "an OTF is not bare CFF"
    );
    assert!(!is_bare_cff(b"not a font"));
    assert!(!is_bare_cff(&[]));
}

#[test]
fn a_bare_cff_would_not_parse_unwrapped_but_does_wrapped() {
    let cff = foxit_cff();
    // This is the whole problem: Skrifa cannot read the bytes a PDF embeds.
    assert!(
        skrifa::FontRef::new(&cff).is_err(),
        "bare CFF is not an SFNT"
    );

    let prog = FontProgram::parse(Arc::from(cff)).expect("wrapped and parsed");
    assert_eq!(prog.units_per_em(), 1000, "from the CFF FontMatrix");
    assert!(prog.num_glyphs() > 100, "from the CharStrings INDEX");
}

#[test]
fn wrapping_preserves_the_glyphs() {
    // The wrapped bare CFF must draw exactly what the original OTF draws:
    // the CFF bytes are identical, only the description around them differs.
    let original = FontProgram::parse(StandardFont::Helvetica.program_data()).unwrap();
    let wrapped = FontProgram::parse(Arc::from(foxit_cff())).unwrap();
    assert_eq!(wrapped.num_glyphs(), original.num_glyphs());

    let a_orig = original.gid_for_name(b"A").expect("original has A");
    let a_wrap = wrapped.gid_for_name(b"A").expect("wrapped has A");
    assert_eq!(a_wrap, a_orig, "same glyph order");
    assert_eq!(
        wrapped.outline(a_wrap),
        original.outline(a_orig),
        "identical outline geometry"
    );
}

#[test]
fn glyph_names_survive_into_the_post_table() {
    // Names are how a simple font's codes reach glyphs: PDF encoding is
    // code -> name -> glyph, and a wrapped CFF has no cmap.
    let wrapped = FontProgram::parse(Arc::from(foxit_cff())).unwrap();
    for name in [&b"A"[..], b"space", b"zero", b"eacute"] {
        let gid = wrapped.gid_for_name(name);
        assert!(
            gid.is_some(),
            "name {} resolves",
            String::from_utf8_lossy(name)
        );
    }
    assert_eq!(wrapped.gid_for_name(b"nosuchglyph"), None);
}

#[test]
fn a_wrapped_glyph_really_draws() {
    let wrapped = FontProgram::parse(Arc::from(foxit_cff())).unwrap();
    let gid = wrapped.gid_for_name(b"A").unwrap();
    let outline = wrapped.outline(gid).expect("'A' has an outline");
    assert!(!outline.verbs.is_empty());
    // `space` is legitimately empty — not a failure.
    let space = wrapped.gid_for_name(b"space").unwrap();
    assert!(wrapped.outline(space).is_none(), "space draws nothing");
}

#[test]
fn garbage_is_declined_rather_than_wrapped() {
    assert!(wrap_bare_cff(b"not a cff").is_none());
    assert!(wrap_bare_cff(&[]).is_none());
    // A plausible header with no body must not produce a bogus font.
    assert!(wrap_bare_cff(&[1, 0, 4, 2]).is_none());
}

/// A CFF INDEX: count, offSize, offsets, then the concatenated items.
fn cff_index(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = (items.len() as u16).to_be_bytes().to_vec();
    if items.is_empty() {
        return out;
    }
    let total: usize = items.iter().map(|i| i.len()).sum();
    out.push(4); // offSize = 4 (simplest: always 32-bit offsets)
    let mut off = 1u32;
    out.extend(off.to_be_bytes());
    for it in items {
        off += it.len() as u32;
        out.extend(off.to_be_bytes());
    }
    for it in items {
        out.extend_from_slice(it);
    }
    let _ = total;
    out
}

/// Build a tiny but valid bare CFF with a **custom Encoding** that maps code
/// `0x41` ('A') to the sole drawable glyph (GID 1) — the shape a symbolic
/// subset font takes, and the case `wrap_bare_cff` must turn into a working
/// `cmap` so `gid_for_code` resolves.
fn minimal_symbolic_cff() -> Vec<u8> {
    // Private DICT: nominalWidthX = 0 (op 21), so charstrings need no width.
    minimal_symbolic_cff_with_private(vec![139u8, 21])
}

fn minimal_symbolic_cff_with_private(private: Vec<u8>) -> Vec<u8> {
    // Two Type 2 charstrings: GID 0 = `.notdef` (endchar), GID 1 = a triangle.
    let notdef = vec![0x0e]; // endchar
    // A Type 2 charstring integer: v in [-107,107] is one byte (v+139); the
    // 108..1131 range uses the two-byte 247-250 form.
    let cs_int = |v: i32| -> Vec<u8> {
        if (-107..=107).contains(&v) {
            vec![(v + 139) as u8]
        } else {
            let w = v - 108;
            vec![(247 + (w >> 8)) as u8, (w & 0xff) as u8]
        }
    };
    // "100 100 rmoveto  200 0 rlineto  0 200 rlineto  endchar" — one contour.
    let mut tri = Vec::new();
    tri.extend(cs_int(100));
    tri.extend(cs_int(100));
    tri.push(0x15); // rmoveto
    tri.extend(cs_int(200));
    tri.extend(cs_int(0));
    tri.push(0x05); // rlineto
    tri.extend(cs_int(0));
    tri.extend(cs_int(200));
    tri.push(0x05); // rlineto
    tri.push(0x0e); // endchar
    let charstrings = cff_index(&[notdef, tri]);

    // Charset (format 0): GID 1 → SID 34 ("A" in the standard strings).
    let charset = vec![0x00, 0x00, 0x22];
    // Encoding (format 0): one code, 0x41 → GID 1.
    let encoding = vec![0x00, 0x01, 0x41];

    // Lay the tables out after a fixed-size Top DICT so offsets are known. We
    // reserve the dict, then patch the absolute offsets in.
    let name_index = cff_index(&[b"MinSym".to_vec()]);
    let string_index = cff_index(&[]); // "A" is a standard string
    let gsubr_index = cff_index(&[]);

    // Encode a Top DICT operand as a 32-bit int (29 b b b b) — always 5 bytes,
    // so the dict size never depends on the value and patching is trivial.
    let int32 = |v: i32| {
        let mut o = vec![29u8];
        o.extend((v as i32).to_be_bytes());
        o
    };
    // Assemble body sections after the header + name + top-dict-index-shell.
    // Top DICT contents: charset(15) Encoding(16) CharStrings(17) Private(18).
    // Build with placeholder zero offsets to learn the dict length.
    let make_dict = |cs: i32, enc: i32, chs: i32, priv_sz: i32, priv_off: i32| {
        let mut d = Vec::new();
        d.extend(int32(cs));
        d.push(15);
        d.extend(int32(enc));
        d.push(16);
        d.extend(int32(chs));
        d.push(17);
        d.extend(int32(priv_sz));
        d.extend(int32(priv_off));
        d.push(18);
        d
    };
    let dict_len = make_dict(0, 0, 0, 0, 0).len();
    let top_index = cff_index(&[vec![0u8; dict_len]]);

    let header_len = 4;
    let prefix_len =
        header_len + name_index.len() + top_index.len() + string_index.len() + gsubr_index.len();
    let charset_off = prefix_len;
    let encoding_off = charset_off + charset.len();
    let charstrings_off = encoding_off + encoding.len();
    let private_off = charstrings_off + charstrings.len();
    let dict = make_dict(
        charset_off as i32,
        encoding_off as i32,
        charstrings_off as i32,
        private.len() as i32,
        private_off as i32,
    );
    assert_eq!(dict.len(), dict_len);
    let top_index = cff_index(&[dict]);

    let mut out = vec![1u8, 0, 4, 1]; // header: v1.0, hdrSize 4, offSize 1
    out.extend(name_index);
    out.extend(top_index);
    out.extend(string_index);
    out.extend(gsubr_index);
    out.extend(charset);
    out.extend(encoding);
    out.extend(charstrings);
    out.extend(private);
    out
}

#[test]
fn a_custom_encoding_becomes_a_working_symbol_cmap() {
    // The Capital One / symbolic-subset case: a bare CFF whose glyphs are
    // reachable only through the font's own Encoding (no `/Differences`, no
    // Unicode cmap). `wrap_bare_cff` must synthesize a cmap so `gid_for_code`
    // finds them — without it every code resolves to `.notdef` and text is
    // blank.
    let cff = minimal_symbolic_cff();
    assert!(is_bare_cff(&cff));
    let prog = FontProgram::parse(Arc::from(cff)).expect("wraps and parses");
    assert_eq!(prog.num_glyphs(), 2);
    // The encoding maps code 0x41 to the drawable glyph; the built-in cmap must
    // carry that through.
    let gid = prog
        .gid_for_code(0x41)
        .expect("code 0x41 resolves via the synthesized cmap");
    assert_eq!(gid, 1, "custom encoding sends 'A' to GID 1");
    assert!(prog.outline(gid).is_some(), "and GID 1 actually draws");
    // A code the encoding never lists stays unmapped.
    assert_eq!(
        prog.gid_for_code(0x20),
        None,
        "unencoded code is not invented"
    );
}

#[test]
fn a_malformed_private_dict_is_repaired_so_outlines_survive() {
    // The bug1308536 case: a subsetter wrote garbage hint data into the
    // Private DICT (broken real-number nibbles, naked non-operator bytes).
    // FreeType-based engines shrug it off; a strict reader rejects the whole
    // dict — and with it every glyph outline, so the page's text vanishes.
    // `wrap_bare_cff` must repair the dict (length-preserving) during the
    // wrap. The tail here is the clean `nominalWidthX 0` the charstrings
    // rely on; the head is a lexeable-but-orphaned real plus bytes that can
    // never begin a DICT token.
    let bad_private = vec![
        0x1e, 0xe4, 0xc0, 0xff, // real number "-4E-0" terminated mid-stream
        0x19, // 25: not a valid operator or operand start
        0xff, // naked 255: not a valid token either
        139, 21, // nominalWidthX = 0 — the recoverable tail
    ];
    let cff = minimal_symbolic_cff_with_private(bad_private);
    let prog = FontProgram::parse(Arc::from(cff)).expect("wraps and parses");
    let gid = prog.gid_for_code(0x41).expect("code resolves");
    assert!(
        prog.outline(gid).is_some(),
        "outline survives the malformed Private DICT"
    );
}

#[test]
fn a_healthy_private_dict_is_wrapped_byte_identically() {
    // Sanitization must never touch a well-formed font: the CFF table inside
    // the wrap is the embedded bytes, verbatim.
    let cff = minimal_symbolic_cff();
    let wrapped = wrap_bare_cff(&cff).expect("wraps");
    assert_eq!(
        extract_cff_table(&wrapped),
        cff,
        "clean CFF passes through unmodified"
    );
}
