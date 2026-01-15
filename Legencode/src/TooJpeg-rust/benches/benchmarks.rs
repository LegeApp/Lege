use criterion::{black_box, criterion_group, criterion_main, Criterion};
use image::{codecs::jpeg::JpegEncoder, ImageBuffer, Rgb, RgbImage};
use rand::Rng;
use std::io::Cursor;
use toojpeg::{encode_jpeg, EncodeOptions, ImageFormat};

fn generate_test_image(width: u32, height: u32) -> (Vec<u8>, RgbImage) {
    // Generate the same random image for both implementations
    let mut rng = rand::thread_rng();
    let mut pixels = Vec::with_capacity((width * height * 3) as usize);

    for _ in 0..(width * height) {
        pixels.push(rng.gen());
        pixels.push(rng.gen());
        pixels.push(rng.gen());
    }

    // Create image for the `image` crate
    let img = ImageBuffer::from_fn(width, height, |x, y| {
        let idx = ((y * width + x) * 3) as usize;
        Rgb([pixels[idx], pixels[idx + 1], pixels[idx + 2]])
    });

    (pixels, img)
}

fn bench_toojpeg(c: &mut Criterion) {
    let (pixels, _) = generate_test_image(1024, 768);

    c.bench_function("encode_1024x768_toojpeg_quality90", |b| {
        b.iter(|| {
            let mut output = Vec::new();
            let options = EncodeOptions {
                width: 1024,
                height: 768,
                format: ImageFormat::RGB,
                quality: 90,
                ..Default::default()
            };
            encode_jpeg(black_box(&pixels), options, &mut output).unwrap();
            output
        })
    });
}

fn bench_image_crate(c: &mut Criterion) {
    let (_, img) = generate_test_image(1024, 768);

    c.bench_function("encode_1024x768_image_crate_quality90", |b| {
        b.iter(|| {
            let mut output = Cursor::new(Vec::new());
            let encoder = JpegEncoder::new_with_quality(&mut output, 90);
            img.write_with_encoder(encoder).unwrap();
            output.into_inner()
        })
    });
}

criterion_group!(benches, bench_toojpeg, bench_image_crate);
criterion_main!(benches);
