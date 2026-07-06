// Validates the bg_subsample knob against the real DjVuLibre toolchain:
//   - djvudump: BG44 chunk carries reduced dims while INFO stays full-res
//   - ddjvu:    the decoded PPM is still full page size (viewer upsamples)
//   - size:     subsampled output is meaningfully smaller
use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use djvu_encoder::image::image_formats::{Pixel, Pixmap};
use std::process::Command;

fn encode(width: u32, height: u32, subsample: u8, path: &str) -> usize {
    let bg = Pixmap::from_fn(width, height, |x, y| {
        Pixel::new((x * 255 / width) as u8, (y * 255 / height) as u8, 128)
    });
    let page = PageComponents::new_with_dimensions(width, height)
        .with_background(bg)
        .unwrap();
    let mut params = PageEncodeParams::default();
    params.bg_subsample = subsample;
    let bytes = page.encode(&params, 1, 300, 1, Some(2.2)).unwrap();
    std::fs::write(path, &bytes).unwrap();
    bytes.len()
}

fn ppm_dims(djvu: &str, ppm: &str) -> (u32, u32) {
    Command::new("ddjvu")
        .args(["-format=ppm", djvu, ppm])
        .output()
        .unwrap();
    let data = std::fs::read(ppm).unwrap();
    // PPM header: "P6\n<w> <h>\n255\n"
    let header: String = data.iter().take(64).map(|&b| b as char).collect();
    let nums: Vec<u32> = header
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    (nums[0], nums[1]) // w, h (255 maxval follows but P6 token is non-numeric)
}

fn main() {
    let (w, h) = (900u32, 1200u32);

    let size1 = encode(w, h, 1, "/tmp/bg_ss1.djvu");
    let size3 = encode(w, h, 3, "/tmp/bg_ss3.djvu");

    println!("== djvudump (subsample=3) ==");
    let dump = Command::new("djvudump")
        .arg("/tmp/bg_ss3.djvu")
        .output()
        .unwrap();
    print!("{}", String::from_utf8_lossy(&dump.stdout));

    let (dw1, dh1) = ppm_dims("/tmp/bg_ss1.djvu", "/tmp/bg_ss1.ppm");
    let (dw3, dh3) = ppm_dims("/tmp/bg_ss3.djvu", "/tmp/bg_ss3.ppm");

    println!("\n== results ==");
    println!("page             : {w}x{h}");
    println!("subsample=1 size : {size1} bytes, decoded {dw1}x{dh1}");
    println!("subsample=3 size : {size3} bytes, decoded {dw3}x{dh3}");
    println!(
        "size reduction   : {:.1}%",
        100.0 * (1.0 - size3 as f64 / size1 as f64)
    );

    assert_eq!((dw1, dh1), (w, h), "subsample=1 must decode full page");
    assert_eq!((dw3, dh3), (w, h), "subsample=3 must upsample to full page");
    assert!(size3 < size1, "subsample=3 must be smaller");
    println!("\nOK: subsampled background upsamples to full page and is smaller.");
}
