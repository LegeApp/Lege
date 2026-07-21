//! Calibration harness for the General-profile quality→lambda mapping.
//!
//! Usage: calibrate_general <image.png> <quality>
//!
//! Encodes the PNG (RGB or gray) at the given quality with the General
//! profile, decodes it again, and prints one CSV line:
//!   quality,bytes,psnr
//!
//! Used to fit the `quality_to_lambda` constants when the distortion model
//! changes: record old-curve sizes per quality, then bisect lambda (via a
//! temporary override hook in `select_for_quality`) to match them.

use jp2lam::{BatchDecoder, ColorSpace, Component, EncodeOptions, Image, OutputFormat, encode};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: calibrate_general <png> <quality>");
    let quality: u8 = args
        .next()
        .expect("usage: calibrate_general <png> <quality>")
        .parse()
        .expect("quality must be 0-100");

    let dyn_img = image::open(&path).expect("open png");
    let width = dyn_img.width();
    let height = dyn_img.height();

    let (image, src_planes): (Image, Vec<Vec<i32>>) = match &dyn_img {
        image::DynamicImage::ImageLuma8(g) => {
            let data: Vec<i32> = g.as_raw().iter().map(|&v| i32::from(v)).collect();
            (
                Image {
                    width,
                    height,
                    components: vec![Component {
                        data: data.clone(),
                        width,
                        height,
                        precision: 8,
                        signed: false,
                        dx: 1,
                        dy: 1,
                    }],
                    colorspace: ColorSpace::Gray,
                },
                vec![data],
            )
        }
        _ => {
            let rgb = dyn_img.to_rgb8();
            let n = (width * height) as usize;
            let mut planes = vec![
                Vec::with_capacity(n),
                Vec::with_capacity(n),
                Vec::with_capacity(n),
            ];
            for px in rgb.pixels() {
                for c in 0..3 {
                    planes[c].push(i32::from(px[c]));
                }
            }
            let components = planes
                .iter()
                .map(|p| Component {
                    data: p.clone(),
                    width,
                    height,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                })
                .collect();
            (
                Image {
                    width,
                    height,
                    components,
                    colorspace: ColorSpace::Srgb,
                },
                planes,
            )
        }
    };

    let bytes = encode(
        &image,
        &EncodeOptions {
            quality,
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        },
    )
    .expect("encode");

    let decoded = BatchDecoder::new().decode_one(&bytes).expect("decode");
    let mut se = 0f64;
    let mut n = 0u64;
    for (ci, plane) in src_planes.iter().enumerate() {
        let comp = &decoded.components[ci];
        for (i, &s) in plane.iter().enumerate() {
            let d = f64::from(comp.data[i].clamp(0, 255) - s);
            se += d * d;
            n += 1;
        }
    }
    let psnr = if se == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / (se / n as f64)).log10()
    };
    println!("{quality},{},{psnr:.3}", bytes.len());
}
