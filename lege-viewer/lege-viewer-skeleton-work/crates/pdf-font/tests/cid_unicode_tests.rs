#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Golden spot-checks of the transcribed CID→Unicode tables against values
//! read directly from PDFium's `core/fpdfapi/cmaps/<charset>/Adobe-*-UCS2_*.inc`
//! constexpr arrays (the cited hex values in the comments are copied verbatim
//! from those files at the array index = CID).

use pdf_font::{Charset, cid_to_unicode};

fn u(charset: Charset, cid: u32) -> Option<u32> {
    cid_to_unicode(charset, cid).map(|c| c as u32)
}

#[test]
fn gb1_spot_cids_match_adobe_gb1_ucs2_5() {
    let cs = Charset::ChineseSimplified;
    // kGB1CID2Unicode_5 (30284 entries):
    assert_eq!(u(cs, 0), None); // index 0 = 0xFFFD (.notdef filler)
    assert_eq!(u(cs, 1), Some(0x0020)); // 0x0020 space
    assert_eq!(u(cs, 96), Some(0x3000)); // 0x3000 ideographic space
    assert_eq!(u(cs, 353), Some(0xFF5C)); // 0xFF5C fullwidth vertical line
    assert_eq!(u(cs, 999), Some(0x8D25)); // 0x8D25 败
    assert_eq!(u(cs, 7716), Some(0x0020)); // 0x0020 (table maps it to space)
    assert_eq!(u(cs, 22355), Some(0x20AC)); // 0x20AC euro sign
    assert_eq!(u(cs, 30283), Some(0xA4C6)); // 0xA4C6 last entry
    assert_eq!(u(cs, 30284), None); // out of range (len = 30284)
}

#[test]
fn cns1_spot_cids_match_adobe_cns1_ucs2_5() {
    let cs = Charset::ChineseTraditional;
    // kCNS1CID2Unicode_5 (19088 entries):
    assert_eq!(u(cs, 1), Some(0x0020)); // 0x0020
    assert_eq!(u(cs, 99), Some(0x3000)); // 0x3000
    assert_eq!(u(cs, 595), Some(0x4E00)); // 0x4E00 一
    assert_eq!(u(cs, 5064), Some(0x5C68)); // 0x5C68
    assert_eq!(u(cs, 14099), Some(0xFE10)); // 0xFE10 vertical comma
    assert_eq!(u(cs, 19087), Some(0x41ED)); // 0x41ED last entry
    assert_eq!(u(cs, 19088), None);
}

#[test]
fn japan1_spot_cids_match_adobe_japan1_ucs2_4() {
    let cs = Charset::ShiftJis;
    // kJapan1CID2Unicode_4 (15444 entries):
    assert_eq!(u(cs, 1), Some(0x0020)); // 0x0020
    assert_eq!(u(cs, 34), Some(0x0041)); // 0x0041 'A'
    assert_eq!(u(cs, 633), Some(0x3000)); // 0x3000
    assert_eq!(u(cs, 1125), Some(0x4E9C)); // 0x4E9C 亜 (first JIS kanji)
    assert_eq!(u(cs, 4594), Some(0x5AE6)); // 0x5AE6
    assert_eq!(u(cs, 15443), None); // 0xFFFD filler → unmapped
    assert_eq!(u(cs, 15444), None);
}

#[test]
fn korea1_spot_cids_match_adobe_korea1_ucs2_2() {
    let cs = Charset::Hangul;
    // kKorea1CID2Unicode_2 (18352 entries):
    assert_eq!(u(cs, 1), Some(0x0020)); // 0x0020
    assert_eq!(u(cs, 109), Some(0x2013)); // 0x2013 en dash
    assert_eq!(u(cs, 3001), Some(0xCEF4)); // 0xCEF4 컴
    assert_eq!(u(cs, 11677), Some(0xB7C6)); // 0xB7C6 럆
    assert_eq!(u(cs, 18351), Some(0x005C)); // 0x005C last entry (backslash)
    assert_eq!(u(cs, 18352), None);
}

#[test]
fn non_cjk_charsets_have_no_table() {
    assert_eq!(cid_to_unicode(Charset::Ansi, 34), None);
}
