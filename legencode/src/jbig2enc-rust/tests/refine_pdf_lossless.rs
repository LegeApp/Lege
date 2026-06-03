//! Validate option B: with per-instance refinement retained in PDF mode, lossy
//! symbol matching/clustering must round-trip pixel-perfect through jbig2dec
//! (no substitution). Uses several glyph families, each instance perturbed by a
//! couple of pixels so the matcher/clusterer merges them and must refine.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use jbig2enc_rust::Jbig2Config;
use jbig2enc_rust::jbig2enc::Jbig2Encoder;
use ndarray::Array2;

fn jbig2dec() -> PathBuf {
    std::env::var("JBIG2DEC").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("jbig2dec"))
}

/// Skip (rather than fail) when a usable jbig2dec is not on this machine.
fn jbig2dec_available() -> bool {
    Command::new(jbig2dec()).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}
fn arr(w: u32, h: u32, px: &[u8]) -> Array2<u8> {
    Array2::from_shape_vec((h as usize, w as usize), px.to_vec()).unwrap()
}
fn parse_p4(pbm: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut idx = 0;
    let mut nl = 0;
    while nl < 2 && idx < pbm.len() {
        if pbm[idx] == b'\n' { nl += 1; }
        idx += 1;
    }
    let data = &pbm[idx..];
    let rb = (w as usize + 7) / 8;
    let mut out = vec![0u8; (w * h) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            out[y * w as usize + x] = (data[y * rb + x / 8] >> (7 - (x % 8))) & 1;
        }
    }
    out
}

/// A few distinct base glyphs (7x10), drawn at (ox,oy), perturbed by `seed`.
fn draw_glyph(px: &mut [u8], w: u32, ox: u32, oy: u32, family: u32, seed: u32) {
    let mut set = |gx: u32, gy: u32| {
        let (x, y) = (ox + gx, oy + gy);
        if x < w {
            px[(y * w + x) as usize] = 1;
        }
    };
    match family % 5 {
        0 => { // H
            for gy in 0..10 { set(0, gy); set(6, gy); }
            for gx in 1..6 { set(gx, 5); }
        }
        1 => { // O (box)
            for gx in 0..7 { set(gx, 0); set(gx, 9); }
            for gy in 1..9 { set(0, gy); set(6, gy); }
        }
        2 => { // L
            for gy in 0..10 { set(0, gy); }
            for gx in 0..7 { set(gx, 9); }
        }
        3 => { // T
            for gx in 0..7 { set(gx, 0); }
            for gy in 0..10 { set(3, gy); }
        }
        _ => { // E
            for gy in 0..10 { set(0, gy); }
            for gx in 0..7 { set(gx, 0); set(gx, 9); }
            for gx in 0..5 { set(gx, 5); }
        }
    }
    // perturbation: thicken the left bar by one pixel at a varying height. The
    // added pixel is adjacent to x=0 (same connected component, not a stray 1px CC
    // that CC-analysis would drop), so the glyph stays one symbol but differs by a
    // pixel -> a lossy match the encoder must correct via refinement, not substitute.
    set(1, 1 + (seed % 8));
}

#[test]
fn pdf_refinement_lossless_roundtrip() {
    if !jbig2dec_available() {
        eprintln!("skipping: jbig2dec not available");
        return;
    }
    let cols = 14u32;
    let rows = 8u32;
    let pitch_x = 11u32;
    let pitch_y = 13u32;
    let w = 4 + cols * pitch_x;
    let h = 4 + rows * pitch_y;
    let mut px = vec![0u8; (w * h) as usize];
    for ry in 0..rows {
        for rx in 0..cols {
            // families with a full left column (H,O,L,E) so the x=1 perturbation
            // stays connected to the glyph body (a disconnected 1px CC would be
            // dropped by CC-analysis and is a test artifact, not a codec error).
            let family = [0u32, 1, 2, 4][((rx + ry) % 4) as usize];
            let seed = rx.wrapping_mul(7).wrapping_add(ry.wrapping_mul(13)); // varies per glyph
            draw_glyph(&mut px, w, 2 + rx * pitch_x, 2 + ry * pitch_y, family, seed);
        }
    }

    let mut cfg = Jbig2Config::text(); // standard symbol mode, not sym-unify
    cfg.want_full_headers = false;

    let mut enc = Jbig2Encoder::new(&cfg);
    enc.add_page(&arr(w, h, &px)).unwrap();
    let out = enc.flush_pdf_split().expect("flush");
    let m = enc.metrics_snapshot();

    let dir = tempfile::tempdir().unwrap();
    let pp = dir.path().join("p.jb2");
    let gp = dir.path().join("g.jb2");
    std::fs::File::create(&pp).unwrap().write_all(&out.page_streams[0]).unwrap();
    let mut cmd = Command::new(jbig2dec());
    cmd.arg("-e").arg("-o").arg(dir.path().join("o.pbm"));
    if let Some(g) = out.global_segments.as_deref() {
        std::fs::File::create(&gp).unwrap().write_all(g).unwrap();
        cmd.arg(&gp);
    }
    cmd.arg(&pp);
    let res = cmd.output().expect("spawn");
    assert!(res.status.success(), "jbig2dec failed: {}", String::from_utf8_lossy(&res.stderr));

    let dec = parse_p4(&std::fs::read(dir.path().join("o.pbm")).unwrap(), w, h);
    let diffs = dec.iter().zip(&px).filter(|(a, b)| a != b).count();
    eprintln!(
        "discovered={} exported={} refined={} pixel diffs={diffs}/{}",
        m.symbol_stats.symbols_discovered, m.symbol_stats.symbols_exported,
        m.symbol_stats.refined_hits, px.len()
    );
    assert_eq!(diffs, 0, "PDF symbol-mode roundtrip not lossless: {diffs} px differ");
}
