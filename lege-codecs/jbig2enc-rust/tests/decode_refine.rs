//! Phase 3 decoder: refinement coding round-trips (jbig2decplan.md §17/§23,
//! T.88 §6.3/§6.4).
//!
//! Three concerns:
//!  1. The `refine` encoder mode (SBREFINE=1 text regions with RDW != 0
//!     per-instance refinements) decodes pixel-exact vs jbig2dec AND vs the
//!     source image — the S-advance fix validated end to end.
//!  2. Standalone immediate generic refinement region segments (T.88 §7.4.7,
//!     types 42/43) decode vs jbig2dec, single and chained (two regions in a
//!     row must each get a fresh context bank).
//!  3. Refinement context reset semantics (§6.3.2/§7.4.7.5 step 2).

mod common;

use common::oracle;
use common::pbm::{Pbm, assert_pixels_eq};

use jbig2enc_rust::decode::{DecodeOptions, decode_embedded, decode_file};
use jbig2enc_rust::jbig2arith::Jbig2ArithCoder;
use jbig2enc_rust::jbig2enc::Jbig2Encoder;
use jbig2enc_rust::jbig2structs::{
    FileHeader, GenericRegionParams, Jbig2Config, PageInfo, Segment, SegmentType,
};
use jbig2enc_rust::jbig2sym::BitImage;
use jbig2enc_rust::shared::bitmap::MonoBitmap;
use ndarray::Array2;

fn arr(w: u32, h: u32, px: &[u8]) -> Array2<u8> {
    Array2::from_shape_vec((h as usize, w as usize), px.to_vec()).unwrap()
}

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

// ─── 1. refine encoder mode with RDW != 0 (S-advance validation) ─────────────

/// Draw a base "H" glyph width 7 at (ox,oy); if `wide`, add one protruding pixel
/// at x=7 so the trimmed bounding box is width 8 while the shape still matches
/// the width-7 prototype at dx=0 (1px err). This forces RDW=+1 refinements
/// through the natural clustering pipeline (`text_refine` gives dim tolerance 2).
fn draw_h(px: &mut [u8], w: u32, ox: u32, oy: u32, wide: bool) {
    let mut set = |gx: u32, gy: u32| {
        let (x, y) = (ox + gx, oy + gy);
        if x < w {
            px[(y * w + x) as usize] = 1;
        }
    };
    for gy in 0..10 {
        set(0, gy);
        set(6, gy);
    }
    for gx in 1..6 {
        set(gx, 5);
    }
    if wide {
        set(7, 5);
    }
}

/// The refine mode is meant to be lossless. With the S-advance fix, a page whose
/// refined instances have RDW != 0 must decode pixel-exact — native == jbig2dec
/// == source. (Before the fix both decoders agreed with each other but not the
/// source, because the encoder advanced CURS by the prototype width, not the
/// placed refined width; see the commit that fixed text_region_refine.rs.)
#[test]
fn refine_mode_rdw_lossless_vs_jbig2dec_and_source() {
    let cols = 14u32;
    let rows = 8u32;
    let (pitch_x, pitch_y) = (13u32, 13u32);
    let w = 4 + cols * pitch_x;
    let h = 4 + rows * pitch_y;
    let mut px = vec![0u8; (w * h) as usize];
    for ry in 0..rows {
        for rx in 0..cols {
            let wide = (rx + ry) % 2 == 1;
            draw_h(&mut px, w, 2 + rx * pitch_x, 2 + ry * pitch_y, wide);
        }
    }

    let mut cfg = Jbig2Config::text();
    cfg.text_refine = true; // dim tolerance 2 => width-8 instances refine onto width-7 prototype
    cfg.want_full_headers = false;

    let mut enc = Jbig2Encoder::new(&cfg);
    enc.add_page(&arr(w, h, &px)).unwrap();
    let out = enc.flush_pdf_split().expect("flush");
    let m = enc.metrics_snapshot();
    assert!(
        m.symbol_stats.refined_hits > 0,
        "test must exercise refinement; got refined_hits=0"
    );

    let opts = DecodeOptions::default();
    let native = mono_to_pbm(
        &decode_embedded(out.global_segments.as_deref(), &out.page_streams[0], &opts)
            .expect("native decode"),
    );
    let source = Pbm::new(w, h, px);
    assert_pixels_eq(
        &source,
        &native,
        "refine RDW!=0 native vs source (S-advance)",
    );

    if let Some(res) = oracle::decode_embedded(out.global_segments.as_deref(), &out.page_streams[0])
    {
        let jd = res.expect("jbig2dec decode");
        assert_pixels_eq(&native, &jd, "refine RDW!=0 native vs jbig2dec");
        assert_pixels_eq(&source, &jd, "refine RDW!=0 jbig2dec vs source");
    }
}

// ─── 2. standalone immediate generic refinement region segments ──────────────

fn bitimage_from(w: u32, h: u32, set_pixels: &[(u32, u32)]) -> BitImage {
    let mut img = BitImage::new(w, h).unwrap();
    for &(x, y) in set_pixels {
        img.set(x, y, true);
    }
    img
}

/// Bytes of an immediate generic refinement region segment payload (T.88
/// §7.4.7): region info (w,h,x,y, ext-comb = REPLACE), refinement flags
/// (GRTEMPLATE=0, TPGRON=0), AT flags (GRAT nominal (-1,-1) twice), then the
/// arithmetic refinement payload coding `target` against `reference`.
fn refinement_region_payload(target: &BitImage, reference: &BitImage, x: u32, y: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&(target.width as u32).to_be_bytes());
    p.extend_from_slice(&(target.height as u32).to_be_bytes());
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.push(4); // region-info flags: external combination operator = REPLACE
    p.push(0x00); // refinement flags: GRTEMPLATE=0, TPGRON=0
    p.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // GRAT1=(-1,-1), GRAT2=(-1,-1)

    let mut coder = Jbig2ArithCoder::new();
    coder
        .encode_refinement_region(target, reference, 0, 0, 0, &[(-1, -1)])
        .unwrap();
    coder.flush(true);
    p.extend_from_slice(coder.as_bytes());
    p
}

fn generic_region_payload(img: &BitImage, x: u32, y: u32) -> Vec<u8> {
    let mut gr = GenericRegionParams::new(img.width as u32, img.height as u32, 300);
    gr.x = x;
    gr.y = y;
    gr.comb_operator = 4; // REPLACE
    gr.tpgdon = false; // native generic decoder path used here does not do TPGDON
    let coder_data =
        Jbig2ArithCoder::encode_generic_payload(img, gr.template, &gr.at_pixels).unwrap();
    let mut payload = gr.to_bytes();
    payload.extend_from_slice(&coder_data);
    payload
}

/// Assemble a one-page sequential JBIG2 file: page info, a generic region that
/// lays down `reference` at (rx,ry), then `refinements` applied in order (each a
/// (target, x, y) immediate refinement region refining the page window in place).
fn build_refinement_file(
    page_w: u32,
    page_h: u32,
    reference: &BitImage,
    rx: u32,
    ry: u32,
    refinements: &[(&BitImage, u32, u32)],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(
        &FileHeader {
            organisation_type: false,
            unknown_n_pages: false,
            n_pages: 1,
        }
        .to_bytes(),
    );

    let mut seg_num = 0u32;
    Segment {
        number: seg_num,
        seg_type: SegmentType::PageInformation,
        page: Some(1),
        payload: PageInfo {
            width: page_w,
            height: page_h,
            xres: 300,
            yres: 300,
            contains_refinements: true,
            ..Default::default()
        }
        .to_bytes(),
        ..Default::default()
    }
    .write_into(&mut out)
    .unwrap();
    seg_num += 1;

    Segment {
        number: seg_num,
        seg_type: SegmentType::ImmediateGenericRegion,
        page: Some(1),
        payload: generic_region_payload(reference, rx, ry),
        ..Default::default()
    }
    .write_into(&mut out)
    .unwrap();
    seg_num += 1;

    for (target, x, y) in refinements {
        Segment {
            number: seg_num,
            seg_type: SegmentType::ImmediateLosslessGenericRefinementRegion,
            page: Some(1),
            referred_to: Vec::new(), // no referred region => page-buffer reference
            payload: refinement_region_payload(target, reference, *x, *y),
            ..Default::default()
        }
        .write_into(&mut out)
        .unwrap();
        seg_num += 1;
    }
    let _ = seg_num;
    out
}

/// Render the expected page: `reference` at (rx,ry), then each refinement's
/// target REPLACE-composited at its (x,y).
fn expected_page(
    page_w: u32,
    page_h: u32,
    reference: &BitImage,
    rx: u32,
    ry: u32,
    refinements: &[(&BitImage, u32, u32)],
) -> Pbm {
    let mut px = vec![0u8; (page_w * page_h) as usize];
    let mut blit = |img: &BitImage, ox: u32, oy: u32| {
        for gy in 0..img.height as u32 {
            for gx in 0..img.width as u32 {
                let (x, y) = (ox + gx, oy + gy);
                if x < page_w && y < page_h {
                    px[(y * page_w + x) as usize] = img.get_pixel_safely(gx as i32, gy as i32);
                }
            }
        }
    };
    blit(reference, rx, ry);
    for (t, x, y) in refinements {
        blit(t, *x, *y);
    }
    Pbm::new(page_w, page_h, px)
}

fn box_reference() -> BitImage {
    let mut refset = Vec::new();
    for y in 0..16u32 {
        for x in 0..12u32 {
            if x == 0 || x == 11 || y == 0 || y == 15 {
                refset.push((x, y));
            }
        }
    }
    bitimage_from(12, 16, &refset)
}

/// An immediate generic refinement region at the page origin decodes pixel-exact
/// vs jbig2dec (and vs the intended target). Region at (0,0) on a page sized to
/// the region: the page-buffer reference window is the whole page, which both
/// decoders extract identically.
#[test]
fn standalone_refinement_region_vs_jbig2dec() {
    let reference = box_reference();
    let mut target = reference.clone();
    target.set(5, 5, true);
    target.set(6, 8, true);
    target.set(3, 10, true);

    let (pw, ph) = (12u32, 16u32);
    let refinements = [(&target, 0u32, 0u32)];
    let file = build_refinement_file(pw, ph, &reference, 0, 0, &refinements);

    let opts = DecodeOptions::default();
    let native = mono_to_pbm(decode_file(&file, &opts).unwrap().first_page().unwrap());
    let expected = expected_page(pw, ph, &reference, 0, 0, &refinements);
    assert_pixels_eq(
        &expected,
        &native,
        "standalone refinement native vs expected",
    );

    if let Some(res) = oracle::decode_standalone(&file) {
        let jd = res.expect("jbig2dec decode");
        assert_pixels_eq(&native, &jd, "standalone refinement native vs jbig2dec");
    }
}

/// The page-buffer reference is the window `[x, y, w, h]` of the page (T.88
/// §7.4.7.4), i.e. offset by the region's (x,y). Placing the region at a
/// non-zero offset and getting the intended target back proves native extracts
/// the window correctly. (No jbig2dec oracle here: jbig2dec 0.20 extracts the
/// page-buffer reference window from the origin regardless of the region's
/// (x,y), so it desyncs for non-zero offsets — see jbig2dec-gaps-plan.md.)
#[test]
fn standalone_refinement_region_page_window_offset() {
    let reference = box_reference();
    let mut target = reference.clone();
    target.set(5, 5, true);
    target.set(6, 8, true);
    target.set(3, 10, true);

    let (pw, ph) = (24u32, 24u32);
    let refinements = [(&target, 5u32, 3u32)];
    let file = build_refinement_file(pw, ph, &reference, 5, 3, &refinements);

    let opts = DecodeOptions::default();
    let native = mono_to_pbm(decode_file(&file, &opts).unwrap().first_page().unwrap());
    let expected = expected_page(pw, ph, &reference, 5, 3, &refinements);
    assert_pixels_eq(&expected, &native, "offset refinement native vs expected");
}

/// Two refinement region segments in a row must each start from a fresh context
/// bank (T.88 §7.4.7.5 step 2). The second refines the page window left by the
/// first, so correct sequential decode proves the reset happened.
#[test]
fn two_refinement_regions_get_fresh_contexts() {
    let mut refset = Vec::new();
    for y in 0..16u32 {
        for x in 0..12u32 {
            if x == 0 || x == 11 || y == 0 || y == 15 {
                refset.push((x, y));
            }
        }
    }
    let reference = bitimage_from(12, 16, &refset);
    // First target: toggle some pixels.
    let mut t1 = reference.clone();
    t1.set(5, 5, true);
    t1.set(2, 9, true);
    // Second target: the page window is t1 after the first region; refine it to
    // t2 (more toggles). We chain by making the *file* recompute reference per
    // region from the page buffer, so t2 is coded against t1.
    let mut t2 = t1.clone();
    t2.set(7, 11, true);
    t2.set(4, 3, true);

    // Build a file where region 1 refines reference->t1 and region 2 refines
    // t1->t2 (each against the current page window).
    let (pw, ph) = (12u32, 16u32);
    let mut out = Vec::new();
    out.extend_from_slice(
        &FileHeader {
            organisation_type: false,
            unknown_n_pages: false,
            n_pages: 1,
        }
        .to_bytes(),
    );
    Segment {
        number: 0,
        seg_type: SegmentType::PageInformation,
        page: Some(1),
        payload: PageInfo {
            width: pw,
            height: ph,
            xres: 300,
            yres: 300,
            contains_refinements: true,
            ..Default::default()
        }
        .to_bytes(),
        ..Default::default()
    }
    .write_into(&mut out)
    .unwrap();
    Segment {
        number: 1,
        seg_type: SegmentType::ImmediateGenericRegion,
        page: Some(1),
        payload: generic_region_payload(&reference, 0, 0),
        ..Default::default()
    }
    .write_into(&mut out)
    .unwrap();
    Segment {
        number: 2,
        seg_type: SegmentType::ImmediateLosslessGenericRefinementRegion,
        page: Some(1),
        referred_to: Vec::new(),
        payload: refinement_region_payload(&t1, &reference, 0, 0),
        ..Default::default()
    }
    .write_into(&mut out)
    .unwrap();
    Segment {
        number: 3,
        seg_type: SegmentType::ImmediateLosslessGenericRefinementRegion,
        page: Some(1),
        referred_to: Vec::new(),
        payload: refinement_region_payload(&t2, &t1, 0, 0),
        ..Default::default()
    }
    .write_into(&mut out)
    .unwrap();

    let opts = DecodeOptions::default();
    let native = mono_to_pbm(decode_file(&out, &opts).unwrap().first_page().unwrap());
    let expected = expected_page(pw, ph, &t2, 0, 0, &[]);
    assert_pixels_eq(
        &expected,
        &native,
        "chained refinement native vs expected (t2)",
    );

    if let Some(res) = oracle::decode_standalone(&out) {
        let jd = res.expect("jbig2dec decode");
        assert_pixels_eq(&native, &jd, "chained refinement native vs jbig2dec");
    }
}
