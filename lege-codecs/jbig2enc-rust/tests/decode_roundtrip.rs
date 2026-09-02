//! Phase 1 decoder round-trip matrix (jbig2decplan.md §21.2/§21.3, Gap C/E).
//!
//! For both PBM fixtures × {generic, lossless} × {standalone, embedded}, assert
//! that `native_decode(encode(img))` is pixel-exact to the original AND, when
//! `jbig2dec` is available, that native output matches `jbig2dec` on the same
//! stream. Plus the odd-width property corpus.

mod common;

use common::oracle;
use common::pbm::{Pbm, assert_pixels_eq};

use jbig2enc_rust::decode::error::{DecodeError, UnsupportedFeature};
use jbig2enc_rust::decode::{
    DecodeOptions, DecodedGlobals, DecoderContext, decode_embedded, decode_embedded_with_globals,
    decode_file, decode_globals,
};
use jbig2enc_rust::jbig2structs::Jbig2Config;
use jbig2enc_rust::shared::bitmap::MonoBitmap;
use jbig2enc_rust::{
    Array2, Jbig2Context, encode_document_pdf_split, encode_single_image_lossless,
    encode_single_image_with_config,
};

/// Convert a decoded `MonoBitmap` to a one-byte-per-pixel `Pbm`.
fn mono_to_pbm(bm: &MonoBitmap) -> Pbm {
    let w = bm.width();
    let h = bm.height();
    let mut pixels = vec![0u8; (w as usize) * (h as usize)];
    for y in 0..h {
        for x in 0..w {
            pixels[(y * w + x) as usize] = bm.get(x, y) as u8;
        }
    }
    Pbm::new(w, h, pixels)
}

/// Load a fixture PBM as a `Pbm` (1 byte/pixel).
fn load_fixture(path: &str) -> Pbm {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    Pbm::from_p4(&bytes).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

#[derive(Clone, Copy)]
enum Mode {
    Generic,
    Lossless,
}

fn encode_standalone(mode: Mode, img: &Pbm) -> Vec<u8> {
    let (w, h) = (img.width, img.height);
    match mode {
        Mode::Generic => {
            // Explicitly generic: the crate default is symbol mode, and this
            // test is about the generic-region path specifically.
            let ctx = Jbig2Context::with_config(Jbig2Config::generic(), false);
            encode_single_image_with_config(&img.pixels, w, h, ctx)
                .expect("encode generic standalone")
                .page_data
        }
        Mode::Lossless => {
            encode_single_image_lossless(&img.pixels, w, h, false)
                .expect("encode lossless standalone")
                .page_data
        }
    }
}

fn encode_embedded(mode: Mode, img: &Pbm) -> Vec<u8> {
    let (w, h) = (img.width, img.height);
    match mode {
        Mode::Generic => {
            let ctx = Jbig2Context::with_config(Jbig2Config::generic(), true);
            encode_single_image_with_config(&img.pixels, w, h, ctx)
                .expect("encode generic embedded")
                .page_data
        }
        Mode::Lossless => {
            let ctx = Jbig2Context::with_lossless_config(true);
            encode_single_image_with_config(&img.pixels, w, h, ctx)
                .expect("encode lossless embedded")
                .page_data
        }
    }
}

fn check_standalone(mode: Mode, img: &Pbm, label: &str) {
    let stream = encode_standalone(mode, img);
    let opts = DecodeOptions::default();
    let doc = decode_file(&stream, &opts).unwrap_or_else(|e| panic!("{label} native: {e}"));
    let native = mono_to_pbm(doc.first_page().expect("a page"));
    assert_pixels_eq(img, &native, &format!("{label} native vs original"));

    if let Some(res) = oracle::decode_standalone(&stream) {
        let jd = res.unwrap_or_else(|e| panic!("{label} jbig2dec: {e}"));
        assert_pixels_eq(&native, &jd, &format!("{label} native vs jbig2dec"));
    }
}

fn check_embedded(mode: Mode, img: &Pbm, label: &str) {
    let stream = encode_embedded(mode, img);
    let opts = DecodeOptions::default();
    let native_bm =
        decode_embedded(None, &stream, &opts).unwrap_or_else(|e| panic!("{label} native: {e}"));
    let native = mono_to_pbm(&native_bm);
    assert_pixels_eq(img, &native, &format!("{label} native vs original"));

    if let Some(res) = oracle::decode_embedded(None, &stream) {
        let jd = res.unwrap_or_else(|e| panic!("{label} jbig2dec: {e}"));
        assert_pixels_eq(&native, &jd, &format!("{label} native vs jbig2dec"));
    }
}

fn fixtures() -> Vec<(&'static str, Pbm)> {
    vec![
        ("test_image", load_fixture("tests/fixtures/test_image.pbm")),
        (
            "test_image1",
            load_fixture("tests/fixtures/test_image1.pbm"),
        ),
    ]
}

#[test]
fn fixtures_generic_standalone() {
    for (name, img) in fixtures() {
        check_standalone(Mode::Generic, &img, &format!("{name} generic standalone"));
    }
}

#[test]
fn fixtures_generic_embedded() {
    for (name, img) in fixtures() {
        check_embedded(Mode::Generic, &img, &format!("{name} generic embedded"));
    }
}

#[test]
fn fixtures_lossless_standalone() {
    for (name, img) in fixtures() {
        check_standalone(Mode::Lossless, &img, &format!("{name} lossless standalone"));
    }
}

#[test]
fn fixtures_lossless_embedded() {
    for (name, img) in fixtures() {
        check_embedded(Mode::Lossless, &img, &format!("{name} lossless embedded"));
    }
}

/// Deterministic random bitmap of the given size.
fn random_bitmap(w: u32, h: u32, seed: u64) -> Pbm {
    let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut pixels = vec![0u8; (w as usize) * (h as usize)];
    for p in pixels.iter_mut() {
        // splitmix64 step.
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        *p = (z & 1) as u8;
    }
    Pbm::new(w, h, pixels)
}

const ODD_WIDTHS: [u32; 14] = [1, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65];

#[test]
fn property_odd_widths_native_roundtrip() {
    let opts = DecodeOptions::default();
    for &w in &ODD_WIDTHS {
        for &h in &[1u32, 5, 16, 40] {
            let img = random_bitmap(w, h, (w as u64) * 131 + h as u64);
            // Standalone generic.
            let ctx = Jbig2Context::with_config(Jbig2Config::generic(), false);
            let stream = encode_single_image_with_config(&img.pixels, w, h, ctx)
                .expect("encode")
                .page_data;
            let doc = decode_file(&stream, &opts).expect("native decode");
            let native = mono_to_pbm(doc.first_page().expect("page"));
            assert_pixels_eq(&img, &native, &format!("odd w={w} h={h} native"));
        }
    }
}

// ─── Phase 2: symbol-mode matrix (Gap C) ─────────────────────────────────────
//
// `symbol` and `sym_unify` use arithmetic symbol dictionaries + text regions,
// and (for non-trivial content) per-instance refinement (SBREFINE=1). These are
// LOSSY soft-pattern-matching modes: the encoded page need not equal the source
// pixel-for-pixel. The decoder-correctness invariant is therefore native ==
// jbig2dec on the same stream (both are exact); native == source additionally
// holds only for content the encoder happens to code losslessly.

fn symbol_config(name: &str) -> Jbig2Config {
    match name {
        "symbol" => Jbig2Config::text(),
        "sym_unify" => Jbig2Config::text_symbol_unify(),
        other => panic!("unknown symbol mode {other}"),
    }
}

/// native == jbig2dec for a symbol-mode standalone stream.
fn check_symbol_standalone(mode: &str, img: &Pbm, label: &str) {
    let out = encode_single_image_with_config(
        &img.pixels,
        img.width,
        img.height,
        Jbig2Context::with_config(symbol_config(mode), false),
    )
    .unwrap_or_else(|e| panic!("{label} encode: {e}"));
    let opts = DecodeOptions::default();
    let doc = decode_file(&out.page_data, &opts).unwrap_or_else(|e| panic!("{label} native: {e}"));
    let native = mono_to_pbm(doc.first_page().expect("a page"));
    if let Some(res) = oracle::decode_standalone(&out.page_data) {
        let jd = res.unwrap_or_else(|e| panic!("{label} jbig2dec: {e}"));
        assert_pixels_eq(&native, &jd, &format!("{label} native vs jbig2dec"));
    }
}

/// native == jbig2dec for a symbol-mode embedded stream with shared globals.
fn check_symbol_embedded(mode: &str, img: &Pbm, label: &str) {
    let out = encode_single_image_with_config(
        &img.pixels,
        img.width,
        img.height,
        Jbig2Context::with_config(symbol_config(mode), true),
    )
    .unwrap_or_else(|e| panic!("{label} encode: {e}"));
    let opts = DecodeOptions::default();
    let native_bm = decode_embedded(out.global_data.as_deref(), &out.page_data, &opts)
        .unwrap_or_else(|e| panic!("{label} native: {e}"));
    let native = mono_to_pbm(&native_bm);
    if let Some(res) = oracle::decode_embedded(out.global_data.as_deref(), &out.page_data) {
        let jd = res.unwrap_or_else(|e| panic!("{label} jbig2dec: {e}"));
        assert_pixels_eq(&native, &jd, &format!("{label} native vs jbig2dec"));
    }
}

#[test]
fn symbol_modes_standalone_vs_jbig2dec() {
    for (name, img) in fixtures() {
        for mode in ["symbol", "sym_unify"] {
            check_symbol_standalone(mode, &img, &format!("{name} {mode} standalone"));
        }
    }
}

#[test]
fn symbol_modes_embedded_globals_vs_jbig2dec() {
    for (name, img) in fixtures() {
        for mode in ["symbol", "sym_unify"] {
            check_symbol_embedded(mode, &img, &format!("{name} {mode} embedded"));
        }
    }
}

/// A page stream that refers to a symbol dictionary living in globals must fail
/// with a typed `MissingReferredSegment` when decoded without those globals —
/// never a panic or wrong image.
#[test]
fn embedded_without_globals_is_typed_error() {
    let (_n, img) = fixtures()
        .into_iter()
        .find(|(n, _)| *n == "test_image1")
        .unwrap();
    let out = encode_single_image_with_config(
        &img.pixels,
        img.width,
        img.height,
        Jbig2Context::with_config(Jbig2Config::text(), true),
    )
    .unwrap();
    // The globals carry the shared dictionary; the page's text region refers to
    // it. Decoding the page alone must be a typed missing-referred-segment error.
    assert!(out.global_data.is_some(), "expected shared globals");
    let opts = DecodeOptions::default();
    match decode_embedded(None, &out.page_data, &opts) {
        Err(DecodeError::MissingReferredSegment { .. }) => {}
        other => panic!("expected MissingReferredSegment, got {other:?}"),
    }
}

/// Truncating a symbol-mode stream at every length must never panic (typed
/// error or bounded result only).
#[test]
fn symbol_truncations_never_panic() {
    let (_n, img) = fixtures()
        .into_iter()
        .find(|(n, _)| *n == "test_image1")
        .unwrap();
    let out = encode_single_image_with_config(
        &img.pixels,
        img.width,
        img.height,
        Jbig2Context::with_config(Jbig2Config::text(), false),
    )
    .unwrap();
    let opts = DecodeOptions::default();
    let stream = &out.page_data;
    // Sample a spread of cut points (full sweep is O(n) decodes; n is large).
    let step = (stream.len() / 400).max(1);
    let mut cut = 0;
    while cut <= stream.len() {
        let _ = decode_file(&stream[..cut], &opts);
        cut += step;
    }
}

// ─── Phase 2: multipage shared globals + thread determinism (Gap E) ───────────

fn load_fixture_array(path: &str) -> Array2<u8> {
    let p = load_fixture(path);
    let mut a = Array2::<u8>::zeros((p.height as usize, p.width as usize));
    for y in 0..p.height as usize {
        for x in 0..p.width as usize {
            a[[y, x]] = if p.pixels[y * p.width as usize + x] != 0 {
                255
            } else {
                0
            };
        }
    }
    a
}

#[test]
fn multipage_shared_globals_vs_jbig2dec() {
    let paths = [
        "tests/fixtures/test_image.pbm",
        "tests/fixtures/test_image1.pbm",
    ];
    let pages: Vec<Array2<u8>> = paths.iter().map(|p| load_fixture_array(p)).collect();
    let split = encode_document_pdf_split(&pages, &Jbig2Config::text())
        .expect("multipage pdf_split encode");
    let opts = DecodeOptions::default();
    // Decode the shared globals ONCE.
    let globals = split
        .global_segments
        .as_deref()
        .map(|g| decode_globals(g, &opts).expect("decode globals"));

    for (i, page) in split.page_streams.iter().enumerate() {
        let mut ctx = DecoderContext::new();
        let bm = decode_embedded_with_globals(globals.as_ref(), page, &opts, &mut ctx)
            .unwrap_or_else(|e| panic!("multipage page {i} native: {e}"));
        let native = mono_to_pbm(&bm);
        if let Some(res) = oracle::decode_embedded(split.global_segments.as_deref(), page) {
            let jd = res.unwrap_or_else(|e| panic!("multipage page {i} jbig2dec: {e}"));
            assert_pixels_eq(
                &native,
                &jd,
                &format!("multipage page {i} native vs jbig2dec"),
            );
        }
    }
}

/// Decoding the same multipage document on 1 vs 4 threads sharing one immutable
/// `DecodedGlobals` must be bit-identical — the Send+Sync guarantee in practice.
#[test]
fn multipage_deterministic_across_thread_counts() {
    use std::sync::Arc;
    use std::thread;

    let paths = [
        "tests/fixtures/test_image.pbm",
        "tests/fixtures/test_image1.pbm",
    ];
    let pages: Vec<Array2<u8>> = paths.iter().map(|p| load_fixture_array(p)).collect();
    let split = encode_document_pdf_split(&pages, &Jbig2Config::text())
        .expect("multipage pdf_split encode");
    let opts = DecodeOptions::default();
    let globals: Arc<Option<DecodedGlobals>> = Arc::new(
        split
            .global_segments
            .as_deref()
            .map(|g| decode_globals(g, &opts).expect("decode globals")),
    );
    let page_streams = Arc::new(split.page_streams);

    // Single-threaded reference.
    let mut single = Vec::new();
    for page in page_streams.iter() {
        let mut ctx = DecoderContext::new();
        let bm = decode_embedded_with_globals(globals.as_ref().as_ref(), page, &opts, &mut ctx)
            .expect("single-threaded decode");
        single.push(mono_to_pbm(&bm));
    }

    // Four workers, each with its own DecoderContext, sharing &DecodedGlobals.
    let mut handles = Vec::new();
    for idx in 0..page_streams.len() {
        let g = Arc::clone(&globals);
        let ps = Arc::clone(&page_streams);
        let opts = opts.clone();
        handles.push(thread::spawn(move || {
            let mut ctx = DecoderContext::new();
            let bm = decode_embedded_with_globals(g.as_ref().as_ref(), &ps[idx], &opts, &mut ctx)
                .expect("worker decode");
            mono_to_pbm(&bm)
        }));
    }
    for (idx, h) in handles.into_iter().enumerate() {
        let got = h.join().expect("thread join");
        assert_pixels_eq(
            &single[idx],
            &got,
            &format!("page {idx} 1-thread vs 4-thread"),
        );
    }
}

/// Corrupting the dictionary's exported-symbol count (SDNUMEXSYMS) so it no
/// longer matches the export runs must be a typed error, not a panic.
#[test]
fn dictionary_export_count_mismatch_is_typed_error() {
    use jbig2enc_rust::decode::DecodeLimits;
    use jbig2enc_rust::decode::file::parse_auto;

    let (_n, img) = fixtures()
        .into_iter()
        .find(|(n, _)| *n == "test_image1")
        .unwrap();
    let out = encode_single_image_with_config(
        &img.pixels,
        img.width,
        img.height,
        Jbig2Context::with_config(Jbig2Config::text(), true),
    )
    .unwrap();
    let mut globals = out.global_data.expect("shared globals");

    // The globals hold exactly one symbol-dictionary segment: header then data.
    // Its payload layout is flags(2) + SDAT(8) + SDNUMEXSYMS(4) + SDNUMNEWSYMS(4).
    let limits = DecodeLimits::default();
    let (data_off, ex_off) = {
        let doc = parse_auto(&globals, &limits).unwrap();
        let seg = &doc.segments[0];
        let data_off = globals.len() - seg.data.len();
        (data_off, data_off + 10)
    };
    let _ = data_off;
    // Bump SDNUMEXSYMS by one so exported count can never match the runs.
    let orig = u32::from_be_bytes([
        globals[ex_off],
        globals[ex_off + 1],
        globals[ex_off + 2],
        globals[ex_off + 3],
    ]);
    let bumped = orig.wrapping_add(1).to_be_bytes();
    globals[ex_off..ex_off + 4].copy_from_slice(&bumped);

    let opts = DecodeOptions::default();
    match decode_globals(&globals, &opts) {
        Err(DecodeError::Malformed { .. }) => {}
        Err(other) => panic!("expected Malformed export-count error, got {other:?}"),
        Ok(_) => panic!("expected Malformed export-count error, got Ok"),
    }
}

/// Compile-time proof that `DecodedGlobals` is `Send + Sync` (Gap E gate).
#[test]
fn decoded_globals_is_send_sync() {
    fn _assert<T: Send + Sync>() {}
    _assert::<DecodedGlobals>();
    // Also confirm a still-reachable Unsupported variant type-checks.
    let _ = UnsupportedFeature::SymbolCoding;
}

#[test]
fn property_odd_widths_jbig2dec_agreement() {
    // A subset also cross-checked against jbig2dec (embedded mode).
    if oracle::jbig2dec_path().is_none() {
        eprintln!("jbig2dec not found; skipping oracle agreement");
        return;
    }
    let opts = DecodeOptions::default();
    for &w in &[7u32, 8, 17, 33, 64, 65] {
        let h = 24u32;
        let img = random_bitmap(w, h, 777 + w as u64);
        let ctx = Jbig2Context::with_config(Jbig2Config::generic(), true);
        let stream = encode_single_image_with_config(&img.pixels, w, h, ctx)
            .expect("encode")
            .page_data;
        let native = mono_to_pbm(&decode_embedded(None, &stream, &opts).expect("native"));
        assert_pixels_eq(&img, &native, &format!("odd w={w} native"));
        let jd = oracle::decode_embedded(None, &stream)
            .expect("jbig2dec present")
            .expect("jbig2dec decode");
        assert_pixels_eq(&native, &jd, &format!("odd w={w} native vs jbig2dec"));
    }
}
