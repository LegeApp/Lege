use image::RgbImage;
use jp2lam::{ColorSpace, Component, EncodeOptions, Image, OutputFormat, encode};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    unix_to_datetime_str(secs)
}

// Civil calendar algorithm — https://howardhinnant.github.io/date_algorithms.html
fn unix_to_datetime_str(secs: u64) -> String {
    let sec = (secs % 60) as u32;
    let min = ((secs / 60) % 60) as u32;
    let hour = ((secs / 3600) % 24) as u32;
    let days = secs / 86400;
    let z = days as i64 + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe as i64 + era * 400 + if month <= 2 { 1 } else { 0 };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        year, month, day, hour, min, sec
    )
}

fn output_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("visual-test");
    fs::create_dir_all(&dir).expect("create visual-test dir");
    dir
}

/// Synthesize a 256×256 RGB test image with four distinct regions:
///   TL: smooth bilinear gradient (low-frequency content)
///   TR: 8×8 checkerboard (high-frequency texture)
///   BL: coloured diagonal stripes (diagonal edges)
///   BR: radial gradient with sine-wave modulation (mixed frequencies)
fn synth_image() -> Image {
    const W: u32 = 256;
    const H: u32 = 256;
    let n = (W * H) as usize;
    let mut r = Vec::with_capacity(n);
    let mut g = Vec::with_capacity(n);
    let mut b = Vec::with_capacity(n);

    let hw = W / 2;
    let hh = H / 2;

    for y in 0..H {
        for x in 0..W {
            let (rv, gv, bv): (u8, u8, u8) = match (x < hw, y < hh) {
                // TL: smooth gradient
                (true, true) => {
                    let rv = (x * 255 / (hw - 1)) as u8;
                    let gv = (y * 255 / (hh - 1)) as u8;
                    (rv, gv, 128)
                }
                // TR: 8×8 black-and-white checkerboard
                (false, true) => {
                    let checker = ((x - hw) / 8 + y / 8) % 2 == 0;
                    if checker {
                        (240, 240, 240)
                    } else {
                        (16, 16, 16)
                    }
                }
                // BL: coloured diagonal stripes
                (true, false) => match (x + (y - hh)) / 8 % 3 {
                    0 => (210, 40, 40),
                    1 => (40, 210, 40),
                    _ => (40, 40, 210),
                },
                // BR: radial + sine modulation
                (false, false) => {
                    let cx = (x - hw) as f32 - hw as f32 / 2.0;
                    let cy = (y - hh) as f32 - hh as f32 / 2.0;
                    let dist = (cx * cx + cy * cy).sqrt();
                    let max_d = (hw as f32 * 0.5f32.hypot(0.5) * hw as f32).sqrt();
                    let radial = (dist / max_d * 255.0).min(255.0) as u8;
                    let wave = ((cx * 0.25).sin() * (cy * 0.25).sin() * 80.0 + 128.0)
                        .clamp(0.0, 255.0) as u8;
                    (radial, wave, 255 - radial)
                }
            };
            r.push(rv as i32);
            g.push(gv as i32);
            b.push(bv as i32);
        }
    }

    let comp = |data: Vec<i32>| Component {
        data,
        width: W,
        height: H,
        precision: 8,
        signed: false,
        dx: 1,
        dy: 1,
    };
    Image {
        width: W,
        height: H,
        components: vec![comp(r), comp(g), comp(b)],
        colorspace: ColorSpace::Srgb,
    }
}

fn write_source_png(image: &Image, path: &PathBuf) {
    let n = (image.width * image.height) as usize;
    let r = &image.components[0].data;
    let g = &image.components[1].data;
    let b = &image.components[2].data;
    let mut raw = Vec::with_capacity(n * 3);
    for i in 0..n {
        raw.push(r[i].clamp(0, 255) as u8);
        raw.push(g[i].clamp(0, 255) as u8);
        raw.push(b[i].clamp(0, 255) as u8);
    }
    RgbImage::from_raw(image.width, image.height, raw)
        .expect("source image from raw")
        .save(path)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Encode the synthetic test image at several quality levels and write
/// timestamped JP2 files to target/visual-test/.
///
/// Runs unconditionally on every `cargo test` invocation so there is always a
/// fresh set of outputs for manual inspection. Files accumulate across runs;
/// the user deletes them as desired.
#[test]
fn visual_output_quality_sweep() {
    let ts = timestamp();
    let dir = output_dir();
    let image = synth_image();
    let qualities: &[u8] = &[1, 5, 10, 15, 25, 40, 50, 65, 80, 90, 95, 99];

    println!(
        "\nvisual_output_quality_sweep  ts={ts}  dir={}",
        dir.display()
    );
    println!("{:<38} {:>10} {:>7}", "file", "bytes", "bpp");
    println!("{}", "-".repeat(58));

    let n_pixels = (image.width * image.height) as f64;
    let source_name = format!("{ts}_source.png");
    write_source_png(&image, &dir.join(&source_name));
    println!("{:<38} {:>10} {:>7}", source_name, "-", "-");

    for &quality in qualities {
        let opts = EncodeOptions {
            quality,
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        };
        let bytes =
            encode(&image, &opts).unwrap_or_else(|e| panic!("encode failed at q{quality}: {e}"));

        assert!(
            bytes.len() >= 64,
            "q{quality}: suspiciously small output ({} bytes)",
            bytes.len()
        );
        assert_eq!(&bytes[4..8], b"jP  ", "q{quality}: missing JP2 signature");

        let filename = format!("{ts}_q{quality:02}.jp2");
        let path = dir.join(&filename);
        fs::write(&path, &bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));

        let bpp = bytes.len() as f64 * 8.0 / n_pixels;
        println!("{:<38} {:>10} {:>7.3}", filename, bytes.len(), bpp);
    }
}
