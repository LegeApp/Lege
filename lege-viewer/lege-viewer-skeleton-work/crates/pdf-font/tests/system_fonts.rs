#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Font Phase 7: system font providers.
//!
//! The provider is scanned from a directory we control, so these assert the
//! *mechanism* (indexing, name matching, collections, style selection)
//! without depending on what this machine has installed.

use std::path::PathBuf;
use std::sync::Arc;

use pdf_font::{Charset, FolderFontProvider, FontProgram, SystemFontProvider, SystemFontRequest};
use pdf_test_support::fonts::minimal_ttf_named;

/// A self-cleaning temp directory (the workspace keeps its dependency list
/// ruthlessly minimal, so this beats pulling in a crate for a few tests).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let mut p = std::env::temp_dir();
        p.push(format!("pdf-font-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create temp dir");
        TempDir(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A directory holding `TestFont.ttf` (family "Test", from `minimal_ttf`).
fn font_dir(tag: &str) -> (TempDir, FolderFontProvider) {
    let dir = TempDir::new(tag);
    std::fs::write(dir.path().join("TestFont.ttf"), minimal_ttf_named(Some("Test"))).expect("write");
    // A non-font file must be ignored rather than break the scan.
    std::fs::write(dir.path().join("notes.txt"), b"not a font").expect("write");
    let p = FolderFontProvider::with_paths(&[dir.path().to_path_buf()]);
    (dir, p)
}

fn request(family: &[u8]) -> SystemFontRequest<'_> {
    SystemFontRequest {
        family,
        bold: false,
        italic: false,
        serif: false,
        fixed_pitch: false,
        charset: Charset::Ansi,
    }
}

#[test]
fn a_font_directory_is_indexed_by_family_name() {
    let (_d, p) = font_dir("index");
    assert_eq!(p.family_count(), 1, "one family, and the .txt was skipped");
    assert!(p.has_family("Test"));
}

#[test]
fn lookup_returns_a_usable_program() {
    let (_d, p) = font_dir("lookup");
    let found = p.lookup(&request(b"Test")).expect("family found");
    assert_eq!(found.index, 0, "a plain font is face 0");
    let prog = FontProgram::parse_indexed(found.data.clone(), found.index).expect("parses");
    assert!(prog.gid_for_char('A').is_some(), "the face really is the test font");
}

#[test]
fn name_matching_ignores_case_and_separators() {
    let (_d, p) = font_dir("case");
    for spelling in [&b"Test"[..], b"test", b"TEST", b"Te-st", b"Te_st"] {
        assert!(
            p.lookup(&request(spelling)).is_some(),
            "{}",
            String::from_utf8_lossy(spelling)
        );
    }
}

#[test]
fn a_style_suffix_falls_back_to_the_family_stem() {
    // `Test,Bold` / `Test-BoldMT` name a family that has no bold cut here;
    // the stem must still resolve rather than losing the font entirely.
    let (_d, p) = font_dir("stem");
    assert!(p.lookup(&request(b"Test,Bold")).is_some());
    assert!(p.lookup(&request(b"Test-BoldItalic")).is_some());
}

#[test]
fn an_unknown_family_falls_through() {
    // `None` is what sends the caller to the deterministic bundled faces.
    let (_d, p) = font_dir("share");
    assert!(p.lookup(&request(b"NoSuchFamily")).is_none());
}

#[test]
fn a_missing_directory_is_not_an_error() {
    let p = FolderFontProvider::with_paths(&[PathBuf::from("/definitely/not/here")]);
    assert_eq!(p.family_count(), 0);
    assert!(p.lookup(&request(b"Test")).is_none());
}

#[test]
fn the_provider_is_shareable_across_workers() {
    let (_d, p) = font_dir("x");
    let p: Arc<dyn SystemFontProvider> = Arc::new(p);
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let p = p.clone();
            std::thread::spawn(move || p.lookup(&request(b"Test")).is_some())
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap(), "concurrent lookups all succeed");
    }
}
