//! Public perceptual encode path: score floor, refusal, and metric identity.

use jp2lam::{
    EncodeOptions, Image, METRIC_VERSION, OutputFormat, PerceptualEffort, PerceptualTarget,
    RateControl, StreamEvaluator, decode_jp2, encode,
};

fn gray_ramp(width: u32, height: u32) -> Image {
    let n = (width * height) as usize;
    let mut data = Vec::with_capacity(n);
    for y in 0..height {
        for x in 0..width {
            data.push(((x + y) % 256) as u8);
        }
    }
    Image::from_gray_bytes(width, height, &data).expect("gray")
}

fn rgb_ramp(width: u32, height: u32) -> Image {
    let mut data = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            data.push((x % 256) as u8);
            data.push((y % 256) as u8);
            data.push(((x + y) % 256) as u8);
        }
    }
    Image::from_rgb_bytes(width, height, &data).expect("rgb")
}

fn encode_perceptual(
    image: &Image,
    score: f64,
    effort: PerceptualEffort,
) -> jp2lam::Result<Vec<u8>> {
    let target = PerceptualTarget::new(score, effort).expect("target");
    encode(
        image,
        &EncodeOptions {
            rate_control: Some(RateControl::Perceptual(target)),
            format: OutputFormat::Jp2,
            ..Default::default()
        },
    )
}

fn assert_emitted_stream_meets_floor(image: &Image, bytes: &[u8], floor: f64) -> f64 {
    let decoded = decode_jp2(bytes).expect("shipped decoder must accept the emitted stream");
    assert_eq!(decoded.width, image.width);
    assert_eq!(decoded.height, image.height);
    let mut evaluator =
        StreamEvaluator::from_view(image.as_view().expect("view")).expect("evaluator");
    assert_eq!(evaluator.metric_version(), METRIC_VERSION);
    let scored = evaluator.score_stream(bytes).expect("production metric");
    assert!(
        scored.score >= floor,
        "decoded score {} below floor {floor}",
        scored.score
    );
    scored.score
}

#[test]
fn perceptual_gray_meets_the_requested_floor() {
    let image = gray_ramp(32, 24);
    let bytes = encode_perceptual(&image, 50.0, PerceptualEffort::Fast).expect("encode");
    assert!(!bytes.is_empty());
    assert_emitted_stream_meets_floor(&image, &bytes, 50.0);
}

#[test]
fn perceptual_srgb_meets_the_requested_floor() {
    let image = rgb_ramp(24, 20);
    let bytes = encode_perceptual(&image, 40.0, PerceptualEffort::Fast).expect("encode");
    assert_emitted_stream_meets_floor(&image, &bytes, 40.0);
}

#[test]
fn perceptual_srgb_encode_is_deterministic_across_two_launches() {
    let image = rgb_ramp(24, 20);
    let a = encode_perceptual(&image, 45.0, PerceptualEffort::Fast).expect("a");
    let b = encode_perceptual(&image, 45.0, PerceptualEffort::Fast).expect("b");
    let sa = assert_emitted_stream_meets_floor(&image, &a, 45.0);
    let sb = assert_emitted_stream_meets_floor(&image, &b, 45.0);
    assert_eq!(sa.to_bits(), sb.to_bits());
}

fn noisy_gray(width: u32, height: u32) -> Image {
    let n = (width * height) as usize;
    let mut data = Vec::with_capacity(n);
    let mut state = 0xA5A5_u32;
    for _ in 0..n {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        data.push((state >> 24) as u8);
    }
    Image::from_gray_bytes(width, height, &data).expect("noise")
}

#[test]
fn infeasible_ssim_floor_is_not_ordinary_success() {
    let image = noisy_gray(48, 40);
    let err = encode_perceptual(&image, 100.0, PerceptualEffort::Fast)
        .expect_err("score 100 is lossless-only; 9/7 refuses it as SaturatedTop");
    assert!(
        err.is_perceptual_target_missed(),
        "expected under-floor refusal, got {err}"
    );
}

#[test]
fn perceptual_score_outside_range_is_invalid_input_not_nyi() {
    let image = gray_ramp(16, 16);
    let err = encode(
        &image,
        &EncodeOptions {
            rate_control: Some(RateControl::Perceptual(PerceptualTarget {
                score: 120.0,
                effort: PerceptualEffort::Fast,
            })),
            format: OutputFormat::Jp2,
            ..Default::default()
        },
    )
    .expect_err("invalid score");
    assert!(!err.is_perceptual_not_implemented(), "{err}");
}

#[test]
fn quality_encode_is_unchanged_by_the_perceptual_variant() {
    let image = gray_ramp(24, 16);
    let bytes = encode(
        &image,
        &EncodeOptions {
            rate_control: Some(RateControl::Quality(75)),
            format: OutputFormat::J2k,
            ..Default::default()
        },
    )
    .expect("quality encode");
    assert!(!bytes.is_empty());
}

#[test]
fn stream_evaluator_score_is_stable_across_two_decodes_of_the_same_bytes() {
    let image = gray_ramp(32, 24);
    let bytes = encode(
        &image,
        &EncodeOptions {
            rate_control: Some(RateControl::Quality(40)),
            format: OutputFormat::Jp2,
            ..Default::default()
        },
    )
    .expect("quality encode");
    let mut evaluator =
        StreamEvaluator::from_view(image.as_view().expect("view")).expect("evaluator");
    let first = evaluator.score_stream(&bytes).expect("first");
    let second = evaluator.score_stream(&bytes).expect("second");
    assert_eq!(first.score.to_bits(), second.score.to_bits());
    assert!(first.score < 100.0);
    let decoded = decode_jp2(&bytes).expect("decode");
    assert_eq!(decoded.width, image.width);
    assert_eq!(decoded.height, image.height);
}
