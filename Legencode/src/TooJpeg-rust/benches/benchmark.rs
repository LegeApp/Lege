use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rand::Rng;
use toojpeg::{encode_jpeg, EncodeOptions, ImageFormat};

fn generate_test_image(width: usize, height: usize) -> Vec<u8> {
    let mut rng = rand::thread_rng();
    let mut pixels = vec![0u8; width * height * 3];

    for chunk in pixels.chunks_exact_mut(3) {
        chunk[0] = rng.gen(); // R
        chunk[1] = rng.gen(); // G
        chunk[2] = rng.gen(); // B
    }

    pixels
}

fn benchmark_encode(c: &mut Criterion) {
    let sizes = [(640, 480), (1920, 1080), (3840, 2160)];
    let qualities = [50, 80, 95];

    for &(width, height) in &sizes {
        for &quality in &qualities {
            let pixels = generate_test_image(width, height);
            let options = EncodeOptions {
                width: width as u32,
                height: height as u32,
                format: ImageFormat::RGB,
                quality,
                ..Default::default()
            };

            let bench_name = format!("encode_{}x{}_q{}", width, height, quality);

            c.bench_function(&bench_name, |b| {
                b.iter(|| {
                    let mut output = Vec::with_capacity(width * height * 3);
                    encode_jpeg(black_box(&pixels), options, &mut output).unwrap();
                    output
                })
            });
        }
    }
}

criterion_group!(benches, benchmark_encode);
criterion_main!(benches);
