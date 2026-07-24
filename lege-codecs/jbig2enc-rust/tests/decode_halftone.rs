//! Phase 4 decoder matrix: MMR generic regions, pattern dictionaries (MMR +
//! arithmetic), and halftone regions (arithmetic + MMR gray planes)
//! (jbig2decplan.md §21.3, Gap C/E).
//!
//! Halftone is a lossy tone approximation, so the decoder-correctness invariant
//! is `native == jbig2dec` on the same stream (both are exact decoders). Generic
//! MMR is lossless, so it is additionally checked pixel-exact vs the source.

mod common;

use common::oracle;
use common::pbm::{Pbm, assert_pixels_eq};

use fax::encoder::Encoder as FaxEncoder;
use fax::{Color, VecWriter};

use jbig2enc_rust::decode::error::DecodeError;
use jbig2enc_rust::decode::{DecodeLimits, DecodeOptions, decode_embedded, decode_file};
use jbig2enc_rust::jbig2halftone::{encode_halftone_document_auto, encode_halftone_pdf_split_auto};
use jbig2enc_rust::jbig2structs::{
    FileHeader, GenericRegionParams, Jbig2Config, PageInfo, Segment, SegmentType,
};
use jbig2enc_rust::jbig2sym::BitImage;
use jbig2enc_rust::shared::bitmap::MonoBitmap;

fn mono_to_pbm(bm: &MonoBitmap) -> Pbm {
    let (w, h) = (bm.width(), bm.height());
    let mut pixels = vec![0u8; (w as usize) * (h as usize)];
    for y in 0..h {
        for x in 0..w {
            pixels[(y * w + x) as usize] = bm.get(x, y) as u8;
        }
    }
    Pbm::new(w, h, pixels)
}

/// A bilevel image whose local black density ramps left→right, so the halftone
/// encoder's decimate+quantize spans the full 0..N-1 gray range (making the
/// encoder's emitted plane count equal the spec's HBPP).
fn gradient_bitimage(w: u32, h: u32) -> BitImage {
    let mut img = BitImage::new(w, h).unwrap();
    let mut s: u32 = 0x1234_5678;
    for y in 0..h {
        for x in 0..w {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let r = (s >> 16) & 0xFF;
            let thresh = x * 255 / (w - 1).max(1);
            if r < thresh {
                img.set(x, y, true);
            }
        }
    }
    img
}

#[allow(clippy::field_reassign_with_default)]
fn halftone_config(dict_mmr: bool, gray_mmr: bool, lossless: bool) -> Jbig2Config {
    let mut cfg = Jbig2Config::default();
    cfg.want_full_headers = true;
    cfg.symbol_mode = false;
    cfg.halftone.grid_size_m = 4;
    cfg.halftone.quant_levels_n = 16;
    cfg.halftone.template = 0;
    cfg.halftone.dict_mmr = dict_mmr;
    cfg.halftone.gray_mmr = gray_mmr;
    cfg.halftone.lossless = lossless;
    cfg
}

/// Every halftone mode combination: native decode must equal jbig2dec exactly.
#[test]
fn halftone_modes_standalone_vs_jbig2dec() {
    let (w, h) = (80u32, 60u32);
    let img = gradient_bitimage(w, h);
    let opts = DecodeOptions::default();

    for &dict_mmr in &[false, true] {
        for &gray_mmr in &[false, true] {
            for &lossless in &[false, true] {
                let cfg = halftone_config(dict_mmr, gray_mmr, lossless);
                let region = GenericRegionParams::new(w, h, 300);
                let stream =
                    encode_halftone_document_auto(&img, &cfg, &region, 0, Some(1)).unwrap();
                let label =
                    format!("halftone dict_mmr={dict_mmr} gray_mmr={gray_mmr} lossless={lossless}");

                let doc =
                    decode_file(&stream, &opts).unwrap_or_else(|e| panic!("{label} native: {e}"));
                let native = mono_to_pbm(doc.first_page().expect("a page"));
                assert_eq!(native.width, w);
                assert_eq!(native.height, h);

                if let Some(res) = oracle::decode_standalone(&stream) {
                    let jd = res.unwrap_or_else(|e| panic!("{label} jbig2dec: {e}"));
                    assert_pixels_eq(&native, &jd, &format!("{label} native vs jbig2dec"));
                }
            }
        }
    }
}

/// Halftone via a PDF-style split: the pattern dictionary lives in globals, the
/// page stream carries the page info + halftone region referring to it.
#[test]
fn halftone_embedded_globals_vs_jbig2dec() {
    let (w, h) = (72u32, 56u32);
    let img = gradient_bitimage(w, h);
    let opts = DecodeOptions::default();

    for &(dict_mmr, gray_mmr) in &[(true, false), (false, true), (true, true), (false, false)] {
        let cfg = halftone_config(dict_mmr, gray_mmr, false);
        let region = GenericRegionParams::new(w, h, 300);
        let (globals, page) =
            encode_halftone_pdf_split_auto(&img, &cfg, &region, 1, Some(1)).unwrap();
        let label = format!("halftone split dict_mmr={dict_mmr} gray_mmr={gray_mmr}");

        let native_bm = decode_embedded(Some(&globals), &page, &opts)
            .unwrap_or_else(|e| panic!("{label} native: {e}"));
        let native = mono_to_pbm(&native_bm);

        if let Some(res) = oracle::decode_embedded(Some(&globals), &page) {
            let jd = res.unwrap_or_else(|e| panic!("{label} jbig2dec: {e}"));
            assert_pixels_eq(&native, &jd, &format!("{label} native vs jbig2dec"));
        }
    }
}

// ─── Generic MMR region (lossless) ───────────────────────────────────────────

/// Encode a bitmap as raw T.6 (MMR) data with the same `fax` path the encoder
/// uses for its halftone blocks.
fn mmr_encode(img: &Pbm) -> Vec<u8> {
    let mut encoder = FaxEncoder::new(VecWriter::new());
    for y in 0..img.height {
        let line = (0..img.width).map(|x| {
            if img.get(x, y) != 0 {
                Color::Black
            } else {
                Color::White
            }
        });
        encoder.encode_line(line, img.width).unwrap();
    }
    encoder.finish().unwrap().finish()
}

/// Build a standalone one-page JBIG2 file containing a single MMR-coded generic
/// region for `img`.
fn mmr_generic_document(img: &Pbm) -> Vec<u8> {
    // Region segment info (17 bytes) + generic flags (MMR=1) + MMR data.
    let mut payload = Vec::new();
    payload.extend_from_slice(&img.width.to_be_bytes());
    payload.extend_from_slice(&img.height.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes()); // x
    payload.extend_from_slice(&0u32.to_be_bytes()); // y
    payload.push(0); // region flags: comb op OR
    payload.push(0x01); // generic flags: MMR = 1
    payload.extend(mmr_encode(img));

    let mut out = FileHeader {
        organisation_type: false,
        unknown_n_pages: false,
        n_pages: 1,
    }
    .to_bytes();

    let page = Segment {
        number: 0,
        seg_type: SegmentType::PageInformation,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: vec![],
        page: Some(1),
        payload: PageInfo {
            width: img.width,
            height: img.height,
            xres: 300,
            yres: 300,
            default_pixel: false,
            ..Default::default()
        }
        .to_bytes(),
    };
    page.write_into(&mut out).unwrap();

    let region = Segment {
        number: 1,
        seg_type: SegmentType::ImmediateGenericRegion,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: vec![],
        page: Some(1),
        payload,
    };
    region.write_into(&mut out).unwrap();

    Segment {
        number: 2,
        seg_type: SegmentType::EndOfFile,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 2,
        referred_to: vec![],
        page: None,
        payload: vec![],
    }
    .write_into(&mut out)
    .unwrap();
    out
}

fn random_pbm(w: u32, h: u32, seed: u64) -> Pbm {
    let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut pixels = vec![0u8; (w as usize) * (h as usize)];
    for p in pixels.iter_mut() {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        *p = (z & 1) as u8;
    }
    Pbm::new(w, h, pixels)
}

#[test]
fn generic_mmr_region_roundtrip_and_jbig2dec() {
    let opts = DecodeOptions::default();
    for &w in &[7u32, 8, 17, 32, 33, 64, 65] {
        for &h in &[1u32, 5, 16] {
            let img = random_pbm(w, h, (w as u64) * 100 + h as u64);
            let stream = mmr_generic_document(&img);
            let doc = decode_file(&stream, &opts)
                .unwrap_or_else(|e| panic!("generic MMR w={w} h={h} native: {e}"));
            let native = mono_to_pbm(doc.first_page().expect("page"));
            // MMR is lossless: native must equal source exactly.
            assert_pixels_eq(&img, &native, &format!("generic MMR w={w} h={h} vs source"));
            if let Some(res) = oracle::decode_standalone(&stream) {
                let jd = res.unwrap_or_else(|e| panic!("generic MMR w={w} h={h} jbig2dec: {e}"));
                assert_pixels_eq(
                    &native,
                    &jd,
                    &format!("generic MMR w={w} h={h} vs jbig2dec"),
                );
            }
        }
    }
}

// ─── Malformed-input: typed errors, never panics ─────────────────────────────

/// Truncating a halftone stream at every length must never panic.
#[test]
fn halftone_truncations_never_panic() {
    let img = gradient_bitimage(48, 40);
    let cfg = halftone_config(true, false, false);
    let region = GenericRegionParams::new(48, 40, 300);
    let stream = encode_halftone_document_auto(&img, &cfg, &region, 0, Some(1)).unwrap();
    let opts = DecodeOptions::default();
    let step = (stream.len() / 300).max(1);
    let mut cut = 0;
    while cut <= stream.len() {
        let _ = decode_file(&stream[..cut], &opts);
        cut += step;
    }
}

/// A halftone region that refers to a non-existent pattern dictionary must be a
/// typed missing-referred-segment error.
#[test]
fn halftone_missing_pattern_dict_is_typed_error() {
    // The pdf-split page stream refers to the pattern dictionary in globals;
    // decode it WITHOUT globals.
    let img = gradient_bitimage(48, 40);
    let cfg = halftone_config(true, false, false);
    let region = GenericRegionParams::new(48, 40, 300);
    let (_globals, page) = encode_halftone_pdf_split_auto(&img, &cfg, &region, 1, Some(1)).unwrap();
    let opts = DecodeOptions::default();
    match decode_embedded(None, &page, &opts) {
        Err(DecodeError::MissingReferredSegment { .. }) => {}
        other => panic!("expected MissingReferredSegment, got {other:?}"),
    }
}

/// A pattern dictionary whose GRAYMAX implies an oversized collective bitmap
/// must be a typed limit/overflow error, not an allocation or panic.
#[test]
fn pattern_dict_graymax_overflow_is_typed_error() {
    use jbig2enc_rust::decode::pattern_dictionary::decode_pattern_dictionary;
    use jbig2enc_rust::shared::mq_table::MqContext;

    // flags=MMR, HDPW=0xFF, HDPH=0xFF, GRAYMAX=0xFFFFFFFF → collective width
    // (GRAYMAX+1)*HDPW overflows the width limit long before any allocation.
    let mut payload = Vec::new();
    payload.push(0x01); // HDMMR = 1
    payload.push(0xFF); // HDPW
    payload.push(0xFF); // HDPH
    payload.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes()); // GRAYMAX
    payload.extend_from_slice(&[0u8; 8]); // some data

    let limits = DecodeLimits::default();
    let mut ctx = vec![MqContext::default(); 1 << 16];
    let mut scratch = jbig2enc_rust::decode::generic::GenericScratch::default();
    match decode_pattern_dictionary(&payload, &limits, &mut ctx, &mut scratch) {
        Err(DecodeError::Limit(_)) | Err(DecodeError::Overflow { .. }) => {}
        Err(other) => panic!("expected a limit/overflow error, got {other:?}"),
        Ok(_) => panic!("expected a limit/overflow error, got Ok"),
    }
}
