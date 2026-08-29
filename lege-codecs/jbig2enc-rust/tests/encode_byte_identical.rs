//! Byte-identical encoder verification (jbig2decplan.md §23 Phase 0, Gap D/E).
//!
//! This test encodes every fixture in `tests/fixtures/*.pbm` through each
//! encoder mode and organisation, hashes every emitted stream with SHA-256,
//! and compares the result against the committed manifest
//! `tests/fixtures/encode_hashes.txt`.
//!
//! The manifest is the Phase 0 acceptance artifact: it is captured *before*
//! the shared-module refactor and must remain identical *after* it. Any
//! difference is a bug in the refactor (the refactor must not change any
//! encoded byte).
//!
//! To regenerate the manifest after an *intentional* encoder change, run:
//!   UPDATE_ENCODE_HASHES=1 cargo test --test encode_byte_identical

use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use jbig2::jbig2structs::Jbig2Config;
use jbig2::{Array2, Jbig2Context, encode_document_pdf_split, encode_single_image_with_config};
use jbig2enc_rust as jbig2;

const MANIFEST: &str = "tests/fixtures/encode_hashes.txt";

/// The fixtures encoded by this test, in a fixed order.
fn fixtures() -> Vec<&'static str> {
    vec![
        "tests/fixtures/test_image.pbm",
        "tests/fixtures/test_image1.pbm",
    ]
}

/// Encoder modes exercised for the single-image scenarios.
fn modes() -> Vec<(&'static str, Jbig2Config)> {
    let mut refine = Jbig2Config::text();
    refine.refine = true;
    refine.text_refine = true;

    vec![
        // Explicitly generic. This row is about the generic-region path, so
        // it names the preset rather than riding on whatever the default is.
        ("generic", Jbig2Config::generic()),
        ("lossless", Jbig2Config::lossless()),
        ("symbol", Jbig2Config::text()),
        ("sym_unify", Jbig2Config::text_symbol_unify()),
        ("refine", refine),
    ]
}

fn main_manifest_lines() -> Vec<String> {
    let mut lines = Vec::new();

    for fixture in fixtures() {
        let name = Path::new(fixture)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(fixture);
        let (pixels, w, h) = load_pbm_binary(fixture);

        for (mode_name, cfg) in modes() {
            // Standalone (pdf_mode = false): single combined stream.
            let standalone = encode_single_image_with_config(
                &pixels,
                w,
                h,
                Jbig2Context::with_config(cfg.clone(), false),
            )
            .unwrap_or_else(|e| panic!("standalone {name}/{mode_name} encode failed: {e}"));
            lines.push(format!(
                "{name} {mode_name} standalone page {}",
                sha256_hex(&standalone.page_data)
            ));
            if let Some(g) = &standalone.global_data {
                lines.push(format!(
                    "{name} {mode_name} standalone global {}",
                    sha256_hex(g)
                ));
            }

            // Embedded (pdf_mode = true): separate globals + page stream.
            let embedded = encode_single_image_with_config(
                &pixels,
                w,
                h,
                Jbig2Context::with_config(cfg.clone(), true),
            )
            .unwrap_or_else(|e| panic!("embedded {name}/{mode_name} encode failed: {e}"));
            lines.push(format!(
                "{name} {mode_name} embedded page {}",
                sha256_hex(&embedded.page_data)
            ));
            if let Some(g) = &embedded.global_data {
                lines.push(format!(
                    "{name} {mode_name} embedded global {}",
                    sha256_hex(g)
                ));
            }
        }
    }

    // Multi-page PDF split sharing one global dictionary across both fixtures.
    let pages: Vec<Array2<u8>> = fixtures().iter().map(|f| load_pbm_array(f)).collect();
    let split = encode_document_pdf_split(&pages, &Jbig2Config::text())
        .expect("multipage pdf_split encode failed");
    if let Some(g) = &split.global_segments {
        lines.push(format!("multipage symbol split global {}", sha256_hex(g)));
    }
    for (i, page) in split.page_streams.iter().enumerate() {
        lines.push(format!(
            "multipage symbol split page{i} {}",
            sha256_hex(page)
        ));
    }

    lines
}

#[test]
fn encoder_output_is_byte_identical() {
    let produced = main_manifest_lines();

    if std::env::var("UPDATE_ENCODE_HASHES").is_ok() {
        let mut body = String::new();
        body.push_str(
            "# SHA-256 of every encoder stream, per fixture/mode/organisation.\n\
             # Regenerate with UPDATE_ENCODE_HASHES=1 (jbig2decplan.md Phase 0).\n",
        );
        for line in &produced {
            body.push_str(line);
            body.push('\n');
        }
        std::fs::write(MANIFEST, body).expect("write manifest");
        eprintln!("regenerated {MANIFEST} ({} entries)", produced.len());
        return;
    }

    let expected = std::fs::read_to_string(MANIFEST).unwrap_or_else(|e| {
        panic!(
            "cannot read {MANIFEST}: {e}. Generate it once with \
             UPDATE_ENCODE_HASHES=1 cargo test --test encode_byte_identical"
        )
    });
    let expected_lines: Vec<&str> = expected
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .collect();

    assert_eq!(
        expected_lines.len(),
        produced.len(),
        "manifest entry count changed ({} expected vs {} produced) — encoder output changed",
        expected_lines.len(),
        produced.len()
    );

    for (exp, got) in expected_lines.iter().zip(produced.iter()) {
        assert_eq!(
            *exp, got,
            "encoder output diverged from the byte-identical baseline"
        );
    }
}

// ─── PBM loading ────────────────────────────────────────────────────────────

/// Load a raw P4 PBM as a row-major 0/1 buffer plus dimensions.
fn load_pbm_binary(path: &str) -> (Vec<u8>, u32, u32) {
    let file = std::fs::File::open(path).expect("open pbm");
    let mut reader = BufReader::new(file);

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "P4", "only raw PBM (P4) supported");

    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if !line.starts_with('#') && !line.trim().is_empty() {
            break;
        }
    }
    let dims: Vec<usize> = line
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    let (w, h) = (dims[0], dims[1]);

    // BufReader is now positioned immediately after the "P4\n<w> <h>\n"
    // header, so the packed bit rows follow directly.
    let bytes_per_row = w.div_ceil(8);
    let mut data = vec![0u8; bytes_per_row * h];
    reader.read_exact(&mut data).unwrap();

    let mut pixels = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let byte = data[y * bytes_per_row + x / 8];
            let bit = 7 - (x % 8);
            // PBM: bit set = black; the encoder treats non-zero as black.
            if (byte >> bit) & 1 != 0 {
                pixels[y * w + x] = 1;
            }
        }
    }
    (pixels, w as u32, h as u32)
}

/// Load a raw P4 PBM as an `Array2<u8>` with 255 = black (encoder page input).
fn load_pbm_array(path: &str) -> Array2<u8> {
    let (pixels, w, h) = load_pbm_binary(path);
    let mut arr = Array2::<u8>::zeros((h as usize, w as usize));
    for y in 0..h as usize {
        for x in 0..w as usize {
            arr[[y, x]] = if pixels[y * w as usize + x] != 0 {
                255
            } else {
                0
            };
        }
    }
    arr
}

// ─── Minimal SHA-256 (test-only, no external dependency) ─────────────────────

fn sha256_hex(data: &[u8]) -> String {
    let digest = sha256(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod sha_selftest {
    use super::sha256_hex;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
