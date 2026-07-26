#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Font Phase 5: the native Type 1 engine, through `FontProgram`.
//!
//! The fixture is synthesized by `pdf_test_support::fonts::minimal_type1`
//! (real Type 1 faces in the wild are licensed), but it is a genuine Type 1
//! program: both Adobe ciphers are applied, so this exercises eexec and
//! charstring decryption for real.

use std::sync::Arc;

use pdf_font::{FontProgram, OutlineVerb};
use pdf_test_support::fonts::minimal_type1;

fn program() -> FontProgram {
    FontProgram::parse(Arc::from(minimal_type1())).expect("bare Type 1 parses")
}

#[test]
fn bare_type1_is_recognized_and_routed_to_the_native_engine() {
    let p = program();
    assert!(p.is_type1(), "not an SFNT: Skrifa cannot read it");
    assert_eq!(p.units_per_em(), 1000, "from /FontMatrix");
    assert_eq!(p.num_glyphs(), 2); // .notdef + A
}

#[test]
fn charstrings_decrypt_and_draw() {
    let p = program();
    let gid = p.gid_for_name(b"A").expect("charstring name lookup");
    let o = p.outline(gid).expect("'A' has an outline");
    assert_eq!(
        o.verbs,
        vec![
            OutlineVerb::MoveTo,
            OutlineVerb::LineTo,
            OutlineVerb::LineTo,
            OutlineVerb::Close
        ]
    );
    // The exact triangle the charstring describes, in font units.
    assert_eq!(o.points, vec![[100.0, 50.0], [900.0, 50.0], [500.0, 650.0]]);
}

#[test]
fn hsbw_supplies_the_advance() {
    let p = program();
    let gid = p.gid_for_name(b"A").unwrap();
    assert_eq!(p.advance(gid), Some(600.0));
}

#[test]
fn the_fonts_builtin_encoding_resolves_codes() {
    // `dup 65 /A put` in the cleartext /Encoding — this is the built-in
    // encoding a symbolic simple font relies on.
    let p = program();
    let by_code = p.gid_for_code(65).expect("code 65 is encoded");
    let by_name = p.gid_for_name(b"A").unwrap();
    assert_eq!(by_code, by_name);
    assert_eq!(p.gid_for_code(66), None, "unencoded code");
}

#[test]
fn unicode_resolves_through_glyph_names() {
    let p = program();
    assert_eq!(p.gid_for_char('A'), p.gid_for_name(b"A"));
}

#[test]
fn type1_never_claims_hinting() {
    // Font Phase 5 is unhinted by design: the caller must fall back to the
    // exact outline rather than get silently wrong geometry.
    let p = program();
    let gid = p.gid_for_name(b"A").unwrap();
    assert!(p.outlines_hinted(&[gid], 12.0).is_none());
}

#[test]
fn garbage_is_rejected_not_guessed() {
    assert!(FontProgram::parse(Arc::from(&b"not a font at all"[..])).is_none());
    // A Type 1 header with no charstrings must not produce an empty font.
    let truncated: Vec<u8> = b"%!PS-AdobeFont-1.0: X\ncurrentfile eexec\n".to_vec();
    assert!(FontProgram::parse(Arc::from(truncated)).is_none());
}
