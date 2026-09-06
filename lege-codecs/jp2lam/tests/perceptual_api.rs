//! Public perceptual encode path: score floor, refusal, and metric identity.

use jp2lam::{
    DisplayProfile, EncodeOptions, Image, METRIC_VERSION, OutputFormat, PerceptualEffort,
    PerceptualTarget, RateControl, StreamEvaluator, decode_jp2, encode, last_achieved_score,
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
                display: None,
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
            rate_control: Some(RateControl::ApproxQuality(75)),
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
            rate_control: Some(RateControl::ApproxQuality(40)),
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

/// The floor contract at display size: what ships must meet the target when
/// re-measured by a fresh evaluator conditioned to the same display.
fn assert_display_floor(image: &Image, score: f64, display: DisplayProfile) {
    let target =
        PerceptualTarget::for_display(score, display, PerceptualEffort::Fast).expect("target");
    let bytes = encode(
        image,
        &EncodeOptions {
            rate_control: Some(RateControl::Perceptual(target)),
            format: OutputFormat::Jp2,
            ..Default::default()
        },
    )
    .expect("display encode");
    let decoded = decode_jp2(&bytes).expect("shipped decoder must accept the emitted stream");
    assert_eq!((decoded.width, decoded.height), (image.width, image.height));
    let mut evaluator =
        StreamEvaluator::for_display(image.as_view().expect("view"), display).expect("evaluator");
    let scored = evaluator.score_stream(&bytes).expect("display metric");
    assert!(
        scored.score >= score,
        "display score {} below floor {score}",
        scored.score
    );
}

#[test]
fn display_target_meets_its_floor_at_display_size() {
    assert_display_floor(&gray_ramp(32, 24), 60.0, DisplayProfile::eink(32, 24));
    assert_display_floor(&rgb_ramp(64, 48), 60.0, DisplayProfile::eink(32, 24));
    assert_display_floor(&rgb_ramp(64, 48), 60.0, DisplayProfile::tablet(24, 24));
}

/// A profile that conditions nothing for the source - a color panel the image
/// already fits, or an e-ink panel for a gray source - is the source-resolution
/// target, and must encode exactly as it: same search, same bytes.
#[test]
fn identity_display_profile_encodes_as_the_plain_target() {
    for (image, display) in [
        (rgb_ramp(64, 48), DisplayProfile::tablet(64, 48)),
        (rgb_ramp(64, 48), DisplayProfile::tablet(200, 200)),
        (noisy_gray(64, 48), DisplayProfile::eink(64, 48)),
    ] {
        let plain = encode_perceptual(&image, 60.0, PerceptualEffort::Fast).expect("plain");
        let target =
            PerceptualTarget::for_display(60.0, display, PerceptualEffort::Fast).expect("target");
        let displayed = encode(
            &image,
            &EncodeOptions {
                rate_control: Some(RateControl::Perceptual(target)),
                format: OutputFormat::Jp2,
                ..Default::default()
            },
        )
        .expect("display encode");
        assert_eq!(
            displayed, plain,
            "identity profile {display:?} must not change the search"
        );
    }
    // The e-ink fold on a color source still conditions.
    assert!(DisplayProfile::eink(64, 48).conditions(64, 48, false));
    assert!(!DisplayProfile::eink(64, 48).conditions(64, 48, true));
    assert!(DisplayProfile::tablet(32, 24).conditions(64, 48, false));
    assert!(!DisplayProfile::tablet(64, 48).conditions(64, 48, false));
}

#[test]
fn display_options_are_a_one_call_form() {
    let image = rgb_ramp(64, 48);
    let options =
        EncodeOptions::display_photo(50.0, DisplayProfile::eink(24, 18)).expect("options");
    let bytes = encode(&image, &options).expect("encode");
    let mut evaluator =
        StreamEvaluator::for_display(image.as_view().expect("view"), DisplayProfile::eink(24, 18))
            .expect("evaluator");
    assert!(evaluator.score_stream(&bytes).expect("score").score >= 50.0);
}

#[test]
fn smallest_metric_size_still_meets_a_display_floor() {
    // 8x8 is the pinned metric's floor; a display box below the source only
    // makes the conditioned size smaller, so the box is kept at the source size.
    assert_display_floor(&gray_ramp(8, 8), 60.0, DisplayProfile::eink(600, 450));
    assert_display_floor(&rgb_ramp(8, 8), 60.0, DisplayProfile::tablet(600, 450));
}

#[test]
fn below_the_metric_floor_a_quality_target_ships_lossless() {
    for image in [gray_ramp(7, 7), rgb_ramp(5, 9)] {
        let options = EncodeOptions::display_photo(70.0, DisplayProfile::eink(600, 450))
            .expect("display options");
        let bytes = encode(&image, &options).expect("sub-metric encode must not fail");
        let decoded = decode_jp2(&bytes).expect("decode");
        for (component, source) in decoded.components.iter().zip(&image.components) {
            assert_eq!(
                component.data, source.data,
                "sub-metric encode must be exact"
            );
        }
    }
}

/// A resampled display encode ships a smaller image, but the floor it promises
/// is still against the original: the search scores its candidates against the
/// original conditioned into the display box, which is exactly what a reader
/// re-measuring the stream computes. So the encoder's own achieved score and an
/// independent re-measurement are the same number, not merely both above the
/// floor. `last_achieved_score` is thread-local, so this holds under a parallel
/// test runner.
#[test]
fn resampled_display_encode_scores_the_original_exactly() {
    let display = DisplayProfile::eink(32, 24).resampling_source();
    for image in [gray_ramp(64, 48), rgb_ramp(64, 48)] {
        let floor = 60.0;
        let target =
            PerceptualTarget::for_display(floor, display, PerceptualEffort::Fast).expect("target");
        let bytes = encode(
            &image,
            &EncodeOptions {
                rate_control: Some(RateControl::Perceptual(target)),
                format: OutputFormat::Jp2,
                ..Default::default()
            },
        )
        .expect("resampled display encode");
        let encoder_score = last_achieved_score();

        let decoded = decode_jp2(&bytes).expect("shipped decoder must accept the emitted stream");
        assert_eq!(
            (decoded.width, decoded.height),
            display.encoded_size(image.width, image.height),
            "a resampled encode emits the display box, not the source size"
        );

        let mut evaluator = StreamEvaluator::for_display(image.as_view().expect("view"), display)
            .expect("evaluator");
        let reader = evaluator
            .score_stream(&bytes)
            .expect("display metric")
            .score;
        assert!(
            reader >= floor,
            "re-measured score {reader} below floor {floor}"
        );
        assert!(
            (reader - encoder_score).abs() < 1e-6,
            "encoder scored {encoder_score}, an independent re-measurement {reader}"
        );
    }
}
