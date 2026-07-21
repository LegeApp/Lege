//! Build a layered text page (JB2 ink mask + IW44 gray background + black FGbz)
//! from a cleaned grayscale page, mirroring the djvulibre recipe
//! `cjb2 + c44 + djvumake INFO Sjbz FGbz=#000000 BG44` for A/B validation.
//!
//! Usage: layered_text_page <gray.pgm> <out.djvu> [ink_threshold] [bg_subsample] [bg_quality]

use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use djvu_encoder::encode::jb2::symbol_dict::BitImage;
use djvu_encoder::image::image_formats::{Pixel, Pixmap};
use std::fs;

fn read_pgm_p5(path: &str) -> (u32, u32, Vec<u8>) {
    let data = fs::read(path).expect("read pgm");
    // Parse "P5\n<w> <h>\n<max>\n" allowing arbitrary whitespace.
    let mut fields = Vec::new();
    let mut pos = 0usize;
    while fields.len() < 4 {
        while pos < data.len() && data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if data[pos] == b'#' {
            while data[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        let start = pos;
        while pos < data.len() && !data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        fields.push(std::str::from_utf8(&data[start..pos]).unwrap().to_string());
    }
    pos += 1; // single whitespace after maxval
    assert_eq!(fields[0], "P5", "expected binary PGM");
    let w: u32 = fields[1].parse().unwrap();
    let h: u32 = fields[2].parse().unwrap();
    let pixels = data[pos..pos + (w * h) as usize].to_vec();
    (w, h, pixels)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let src = args
        .get(1)
        .expect("usage: layered_text_page <gray.pgm> <out.djvu> [t] [sub] [q]");
    let out = args.get(2).expect("missing output path");
    let ink_t: u8 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(100);
    let sub: u8 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(3);
    let bg_q: u8 = args.get(5).map(|s| s.parse().unwrap()).unwrap_or(75);

    let (w, h, gray) = read_pgm_p5(src);

    // Ink mask: pixels darker than the threshold become JB2 foreground.
    let mut mask = BitImage::new(w, h)?;
    for y in 0..h as usize {
        for x in 0..w as usize {
            if gray[y * w as usize + x] < ink_t {
                mask.set_usize(x, y, true);
            }
        }
    }

    // Background: cleaned gray with mask pixels filled white (invisible under mask).
    let bg = Pixmap::from_fn(w, h, |x, y| {
        let v = gray[(y * w + x) as usize];
        let v = if v < ink_t { 255 } else { v };
        Pixel::new(v, v, v)
    });

    let page = PageComponents::new_with_dimensions(w, h)
        .with_background(bg)?
        .with_mask(mask)?;

    let params = PageEncodeParams {
        dpi: 290,
        color: false,
        bg_quality: bg_q,
        bg_subsample: sub,
        ..PageEncodeParams::default()
    };
    let bytes = page.encode(&params, 1, 290 * 100 / 254, 1, Some(2.2))?;
    fs::write(out, &bytes)?;
    println!("{} bytes -> {}", bytes.len(), out);
    Ok(())
}
