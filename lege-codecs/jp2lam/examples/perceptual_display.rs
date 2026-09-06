//! Display-floor corpus harness for the perceptual controller.
//!
//! Encodes every PNG below a corpus directory at several floors, once per
//! display profile (plain source-resolution plus a few panels), and prints one
//! line per cell: probes spent, output bytes, the score the floor was measured
//! at, and the source-resolution SSIMULACRA2 of the same emitted stream.
//!
//! ```text
//! cargo run --release --example perceptual_display -- test-set 60,65,70,75,80
//! ```

use jp2lam::{
    DisplayProfile, EncodeOptions, Image, OutputFormat, PERCEPTUAL_PROBES, PerceptualEffort,
    PerceptualTarget, RateControl, StreamEvaluator, encode,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// The panels the corpus measures, plus the two box-equals-source cases: a display
/// target whose source already fits the panel is still conditioned (downsampled colour,
/// or folded to luma for e-ink), so it does not reduce to a plain encode and the
/// controller has to predict that conditioning gain too.
fn profiles(width: u32, height: u32) -> Vec<(String, Option<DisplayProfile>)> {
    vec![
        ("plain".into(), None),
        ("eink400x300".into(), Some(DisplayProfile::eink(400, 300))),
        ("eink600x450".into(), Some(DisplayProfile::eink(600, 450))),
        (
            "tablet800x600".into(),
            Some(DisplayProfile::tablet(800, 600)),
        ),
        ("einkbox".into(), Some(DisplayProfile::eink(width, height))),
        (
            "tabletbox".into(),
            Some(DisplayProfile::tablet(width, height)),
        ),
    ]
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let corpus = PathBuf::from(args.first().map_or("test-set", String::as_str));
    let floors: Vec<f64> = match args.get(1) {
        None => vec![60.0, 65.0, 70.0, 75.0, 80.0],
        Some(raw) => raw
            .split(',')
            .map(|part| part.parse().map_err(|_| format!("bad floor `{part}`")))
            .collect::<Result<_, _>>()?,
    };
    let recursive = args.iter().any(|arg| arg == "--recursive");

    let mut inputs = Vec::new();
    collect_pngs(&corpus, &mut inputs, recursive)?;
    inputs.sort();
    if inputs.is_empty() {
        return Err(format!("no PNG images below {}", corpus.display()));
    }

    println!("source floor profile probes bytes display_score source_score millis status");
    for input in &inputs {
        let image = load_png(input)?;
        let view = image.as_view().map_err(|err| err.to_string())?;
        let mut source_eval =
            StreamEvaluator::from_view_ref(&view).map_err(|err| err.to_string())?;
        for (name, display) in profiles(image.width, image.height) {
            let mut display_eval = match display {
                None => None,
                Some(profile) => Some(
                    StreamEvaluator::for_display(
                        image.as_view().map_err(|err| err.to_string())?,
                        profile,
                    )
                    .map_err(|err| err.to_string())?,
                ),
            };
            for &floor in &floors {
                let target = match display {
                    None => PerceptualTarget::new(floor, PerceptualEffort::Fast),
                    Some(profile) => {
                        PerceptualTarget::for_display(floor, profile, PerceptualEffort::Fast)
                    }
                }
                .map_err(|err| err.to_string())?;
                PERCEPTUAL_PROBES.store(0, Ordering::Relaxed);
                let started = std::time::Instant::now();
                let encoded = encode(
                    &image,
                    &EncodeOptions {
                        rate_control: Some(RateControl::Perceptual(target)),
                        format: OutputFormat::Jp2,
                        ..Default::default()
                    },
                );
                let millis = started.elapsed().as_millis();
                let probes = PERCEPTUAL_PROBES.load(Ordering::Relaxed);
                let stem = input.file_stem().unwrap_or_default().to_string_lossy();
                let bytes = match encoded {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        println!("{stem} {floor} {name} {probes} 0 0 0 {millis} MISS:{error}");
                        continue;
                    }
                };
                let source_score = source_eval
                    .score_stream(&bytes)
                    .map_err(|err| err.to_string())?
                    .score;
                let display_score = match display_eval.as_mut() {
                    None => source_score,
                    Some(evaluator) => {
                        evaluator
                            .score_stream(&bytes)
                            .map_err(|err| err.to_string())?
                            .score
                    }
                };
                println!(
                    "{stem} {floor} {name} {probes} {} {display_score:.3} {source_score:.3} {millis} ok",
                    bytes.len()
                );
            }
        }
    }
    Ok(())
}

fn collect_pngs(dir: &Path, out: &mut Vec<PathBuf>, recursive: bool) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|err| err.to_string())?.path();
        if path.is_dir() {
            if recursive {
                collect_pngs(&path, out, recursive)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "png") {
            out.push(path);
        }
    }
    Ok(())
}

fn load_png(path: &Path) -> Result<Image, String> {
    let dynamic = image::open(path).map_err(|err| format!("load {}: {err}", path.display()))?;
    if dynamic.color().has_color() {
        let rgb = dynamic.to_rgb8();
        let (width, height) = rgb.dimensions();
        Image::from_rgb_bytes(width, height, rgb.as_raw()).map_err(|err| err.to_string())
    } else {
        let gray = dynamic.to_luma8();
        let (width, height) = gray.dimensions();
        Image::from_gray_bytes(width, height, gray.as_raw()).map_err(|err| err.to_string())
    }
}
