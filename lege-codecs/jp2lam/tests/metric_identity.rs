//! Session 1 gate: jp2lam scores with the same pinned SSIMULACRA2 crate as JPXL.

use jpxl_perceptual::{LinearRgbView, METRIC_VERSION, score_pair};

/// The production metric identity must stay in lock-step with JPXL.
#[test]
fn metric_version_is_the_jpxl_pin() {
    assert_eq!(METRIC_VERSION, "ssimulacra2-jpxl-1");
    assert_eq!(jp2lam::METRIC_VERSION, METRIC_VERSION);
}

/// Identical linear-RGB planes score exactly 100, bit-identically.
#[test]
fn identical_synthetic_scores_one_hundred() {
    let (width, height) = (64, 48);
    let n = (width * height) as usize;
    let mut r = vec![0.0f32; n];
    let mut g = vec![0.0f32; n];
    let mut b = vec![0.0f32; n];
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) as usize;
            let fx = x as f32 / width as f32;
            let fy = y as f32 / height as f32;
            r[i] = 0.2 + 0.6 * fx;
            g[i] = 0.25 + 0.5 * fy;
            b[i] = 0.3 + 0.4 * (1.0 - fx);
        }
    }
    let view = LinearRgbView::new(width, height, &r, &g, &b).expect("planes match");
    let result = score_pair(view, view).expect("identical pair is in range");
    assert_eq!(result.score, 100.0);
    assert_eq!(result.raw_error, 0.0);

    let again = score_pair(view, view).expect("second score");
    assert_eq!(again.score, result.score);
    assert_eq!(again.raw_error, result.raw_error);
}

/// A distorted copy scores strictly below the identical pair, deterministically.
#[test]
fn a_quantized_copy_scores_below_one_hundred_and_repeats() {
    let (width, height) = (80, 64);
    let n = (width * height) as usize;
    let mut r = vec![0.0f32; n];
    let mut g = vec![0.0f32; n];
    let mut b = vec![0.0f32; n];
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) as usize;
            r[i] = (x as f32 * 0.013) % 1.0;
            g[i] = (y as f32 * 0.017) % 1.0;
            b[i] = ((x + y) as f32 * 0.011) % 1.0;
        }
    }
    let mut rq = r.clone();
    let mut gq = g.clone();
    let mut bq = b.clone();
    for plane in [&mut rq, &mut gq, &mut bq] {
        for sample in plane.iter_mut() {
            *sample = (*sample * 12.0).round() / 12.0;
        }
    }
    let reference = LinearRgbView::new(width, height, &r, &g, &b).expect("ref");
    let candidate = LinearRgbView::new(width, height, &rq, &gq, &bq).expect("cand");
    let first = score_pair(reference, candidate).expect("score");
    let second = score_pair(reference, candidate).expect("score again");
    assert!(
        first.score < 100.0 && first.score > 0.0,
        "quantized copy scored {}",
        first.score
    );
    assert_eq!(first.score, second.score);
    assert_eq!(first.raw_error, second.raw_error);
}
