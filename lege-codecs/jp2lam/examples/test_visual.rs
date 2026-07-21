use jp2lam::{ColorSpace, Component, EncodeOptions, Image, OutputFormat, encode};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn lear_png_path() -> PathBuf {
    // Try workspace root first, then crate root
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("lear.png"))
        .unwrap_or_else(|| PathBuf::new());
    if workspace.exists() {
        return workspace;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lear.png")
}

fn load_lear_rgb() -> Image {
    let path = lear_png_path();
    let dyn_img = image::open(&path)
        .unwrap_or_else(|e| panic!("failed to open {}: {}", path.display(), e))
        .into_rgb8();
    let width = dyn_img.width();
    let height = dyn_img.height();
    let n = (width * height) as usize;
    let mut r = Vec::with_capacity(n);
    let mut g = Vec::with_capacity(n);
    let mut b = Vec::with_capacity(n);
    for pixel in dyn_img.pixels() {
        r.push(pixel[0] as i32);
        g.push(pixel[1] as i32);
        b.push(pixel[2] as i32);
    }
    Image {
        width,
        height,
        components: vec![
            Component {
                data: r,
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            },
            Component {
                data: g,
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            },
            Component {
                data: b,
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            },
        ],
        colorspace: ColorSpace::Srgb,
    }
}

fn visual_output_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("visual_outputs");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn main() {
    let image = load_lear_rgb();
    println!("Loaded lear.png: {}x{}", image.width, image.height);

    // Stable dependency-free timestamp for filenames.
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after UNIX epoch")
        .as_secs()
        .to_string();

    // Test different quality settings - focus on mid-to-high range to evaluate curve
    let qualities = [10, 25, 50, 60, 70, 75, 80, 85, 90, 95, 99, 100];

    for quality in qualities {
        let options = EncodeOptions {
            quality,
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        };

        let bytes = encode(&image, &options).expect("encode failed");
        println!("Quality {}: {} bytes", quality, bytes.len());

        // Output to visual_outputs with timestamp
        let filename = format!("lear_q{:03}_{}.jp2", quality, timestamp);
        let path = visual_output_dir().join(filename);
        let _ = fs::write(&path, &bytes);
        println!("Wrote {}", path.display());
    }
}
