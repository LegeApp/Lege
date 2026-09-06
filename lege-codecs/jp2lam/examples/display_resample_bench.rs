//! Encoding the panel instead of the source: `DisplayProfile::resample_source`.
//!
//! ```text
//! cargo run --release --example display_resample_bench -- [corpus_dir ...]
//! ```
//!
//! For every PNG below each corpus directory, encodes the same display target
//! twice - once at source resolution, once with the source resampled into the
//! display box - and reports bytes, encode time, probes, decode time, and the
//! score the reader actually sees. The score is measured the way a reader would:
//! the emitted stream is decoded and conditioned into the display box, and
//! compared with the source conditioned into the same box. Both must clear the
//! floor; the resampled stream has to clear it even though its own encode was
//! verified against an sRGB-space downscale rather than this linear-light one.
//!
//! Environment: `RESAMPLE_ONLY=1` (or `0`) runs only the resampled (or only the
//! source-resolution) side, so a `--features profile` run attributes one path.
use jp2lam::{
    DisplayProfile, EncodeOptions, ImageView, OutputFormat, PERCEPTUAL_PROBES, PerceptualEffort,
    PerceptualTarget, RateControl, StreamEvaluator, decode_jp2, encode_view, last_achieved_score,
};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

const FLOOR: f64 = 70.0;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dirs: Vec<PathBuf> = if args.is_empty() {
        vec!["test-set".into(), "test-set/test-set-4mp".into()]
    } else {
        args.iter().map(PathBuf::from).collect()
    };
    let display = DisplayProfile::eink(600, 450);
    let only: Option<bool> = match std::env::var("RESAMPLE_ONLY").ok().as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    };

    println!("floor={FLOOR} display=eink:600x450");
    println!(
        "{:<28} {:>11} {:>9} {:>8} {:>6} {:>8} {:>8} {:>7}",
        "source", "mode", "bytes", "enc_ms", "probes", "encoder", "reader", "dec_ms"
    );
    let mut totals = [(0u64, 0.0f64, 0.0f64); 2];
    let mut images = 0u32;
    for dir in &dirs {
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "png"))
            .collect();
        paths.sort();
        for path in paths {
            let rgb = image::open(&path)
                .unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
                .into_rgb8();
            let (width, height) = rgb.dimensions();
            let raw = rgb.into_raw();
            let view = ImageView::from_rgb8_interleaved(width, height, &raw).expect("view");
            let label = format!(
                "{} {width}x{height}",
                path.file_stem().unwrap().to_string_lossy()
            );
            images += 1;
            for (slot, resample) in [(0usize, false), (1, true)] {
                if only.is_some_and(|want| want != resample) {
                    continue;
                }
                let profile = if resample {
                    display.resampling_source()
                } else {
                    display
                };
                let target = PerceptualTarget::for_display(FLOOR, profile, PerceptualEffort::Fast)
                    .expect("target");
                let options = EncodeOptions {
                    rate_control: Some(RateControl::Perceptual(target)),
                    format: OutputFormat::Jp2,
                    ..Default::default()
                };
                PERCEPTUAL_PROBES.store(0, Ordering::Relaxed);
                let started = Instant::now();
                let bytes = encode_view(view.clone(), &options).expect("encode");
                let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
                let probes = PERCEPTUAL_PROBES.load(Ordering::Relaxed);
                let encoder_score = last_achieved_score();

                let started = Instant::now();
                let decoded = decode_jp2(&bytes).expect("decode");
                let decode_ms = started.elapsed().as_secs_f64() * 1000.0;
                let expected = if resample {
                    profile.encoded_size(width, height)
                } else {
                    (width, height)
                };
                assert_eq!(
                    (decoded.width, decoded.height),
                    expected,
                    "emitted dimensions"
                );

                // The reader's view: source conditioned into the box on one
                // side, the emitted stream conditioned into the same box on the
                // other. `for_display` accepts a candidate that already decodes
                // at the box size, which is what a resampled encode emits.
                let mut evaluator =
                    StreamEvaluator::for_display(view.clone(), display).expect("evaluator");
                let score = evaluator.score_stream(&bytes).expect("score").score;
                assert!(
                    score >= FLOOR,
                    "{label} resample={resample} scored {score} below floor {FLOOR}"
                );
                // The encoder verified its floor against this very measurement,
                // so its own number is the reader's number: the only slack left
                // is reconstruct-vs-decode, and there is none.
                assert!(
                    (encoder_score - score).abs() < 1e-6,
                    "{label} resample={resample}: encoder scored {encoder_score}, reader {score}"
                );
                println!(
                    "{label:<28} {:>11} {:>9} {encode_ms:>8.1} {probes:>6} {encoder_score:>8.2} {score:>8.2} {decode_ms:>7.1}",
                    if resample { "resampled" } else { "source-res" },
                    bytes.len(),
                );
                totals[slot].0 += bytes.len() as u64;
                totals[slot].1 += encode_ms;
                totals[slot].2 += decode_ms;
            }
        }
    }
    let n = f64::from(images);
    for (slot, name) in [(0usize, "source-res"), (1, "resampled")] {
        println!(
            "mean {name:<11} bytes={:>9.0} enc_ms={:>8.1} dec_ms={:>7.1}",
            totals[slot].0 as f64 / n,
            totals[slot].1 / n,
            totals[slot].2 / n
        );
    }
    jp2lam::print_timing_data();
    println!(
        "resampled/source-res: bytes {:.3}x  encode {:.3}x  decode {:.3}x",
        totals[1].0 as f64 / totals[0].0 as f64,
        totals[1].1 / totals[0].1,
        totals[1].2 / totals[0].2
    );
}
