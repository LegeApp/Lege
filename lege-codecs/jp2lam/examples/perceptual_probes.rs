//! Probe-count harness for the perceptual controller.
//!
//! Encodes every PNG below a corpus directory at several targets and efforts and
//! prints, per cell, the pixel probes spent, the output bytes and the achieved
//! score. The average probes per encode is the number the controller is tuned on.
//!
//! ```text
//! cargo run --release --example perceptual_probes -- test-set 70,75,80,85,90
//! ```

use jp2lam::{
    EncodeOptions, Image, OutputFormat, PERCEPTUAL_PROBES, PerceptualEffort, PerceptualTarget,
    RateControl, StreamEvaluator, encode,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let corpus = PathBuf::from(args.first().map_or("test-set", String::as_str));
    let targets: Vec<f64> = match args.get(1) {
        None => vec![70.0, 75.0, 80.0, 85.0, 90.0],
        Some(raw) => raw
            .split(',')
            .map(|part| part.parse().map_err(|_| format!("bad target `{part}`")))
            .collect::<Result<_, _>>()?,
    };
    let recursive = args.iter().any(|arg| arg == "--recursive");

    let mut inputs = Vec::new();
    collect_pngs(&corpus, &mut inputs, recursive)?;
    inputs.sort();
    if inputs.is_empty() {
        return Err(format!("no PNG images below {}", corpus.display()));
    }

    println!("source target effort probes bytes score margin");
    let mut probes_total = 0u64;
    let mut cells = 0u64;
    for input in &inputs {
        let image = load_png(input)?;
        let mut evaluator =
            StreamEvaluator::from_view(image.as_view().map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())?;
        for &target in &targets {
            for effort in [PerceptualEffort::Fast, PerceptualEffort::Balanced] {
                PERCEPTUAL_PROBES.store(0, Ordering::Relaxed);
                let bytes = encode(
                    &image,
                    &EncodeOptions {
                        rate_control: Some(RateControl::Perceptual(
                            PerceptualTarget::new(target, effort).map_err(|err| err.to_string())?,
                        )),
                        format: OutputFormat::Jp2,
                        ..Default::default()
                    },
                )
                .map_err(|err| err.to_string())?;
                let probes = PERCEPTUAL_PROBES.load(Ordering::Relaxed);
                let scored = evaluator
                    .score_stream(&bytes)
                    .map_err(|err| err.to_string())?;
                probes_total += probes;
                cells += 1;
                println!(
                    "{} {target} {effort:?} {probes} {} {:.3} {:+.3}",
                    input.file_stem().unwrap_or_default().to_string_lossy(),
                    bytes.len(),
                    scored.score,
                    scored.score - target
                );
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let mean = probes_total as f64 / cells as f64;
    println!("mean_probes_per_encode {mean:.3} cells {cells}");
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
