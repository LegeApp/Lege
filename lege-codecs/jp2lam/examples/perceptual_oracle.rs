//! Offline SSIMULACRA2 oracle for jp2lam's perceptual controller.
//!
//! Densifies global quantizer quality × PCRD body-byte cuts, scores each
//! candidate with the pinned in-tree metric (`ssimulacra2-jpxl-1`), and writes
//! one resumable JSONL file per source. The labelled quantity is the coarsest
//! feasible `(quant_scale, pcrd_body)` that meets each target — not filesize
//! and not SSIM. This tool does not train a predictor.
//!
//! Default corpus walk is **non-recursive** so `test-set/` does not pull in
//! 12/50 MP frames. Pass `--recursive` only on a quiet host (Session 8c).
//!
//! Usage:
//! ```text
//! cargo run --example perceptual_oracle -- \
//!   test-set /tmp/jp2lam-ssim-oracle 80,85,90 7
//! cargo run --example perceptual_oracle -- \
//!   test-set /tmp/jp2lam-ssim-oracle --recursive
//! ```

use jp2lam::{
    Image, OracleConfig, default_oracle_body_fractions, default_oracle_quant_qualities,
    default_oracle_targets, sweep_source,
};
use std::fs;
use std::path::{Path, PathBuf};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let recursive = args.iter().any(|arg| arg == "--recursive");
    let positional: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| !arg.starts_with("--"))
        .collect();

    let corpus = PathBuf::from(positional.first().copied().unwrap_or("test-set"));
    let output = PathBuf::from(
        positional
            .get(1)
            .copied()
            .unwrap_or("/tmp/jp2lam-ssim-oracle"),
    );
    let targets = parse_targets(positional.get(2).copied())?;
    let max_files = positional.get(3).and_then(|value| value.parse().ok());
    let qualities = parse_qualities(
        args.iter()
            .position(|arg| arg == "--qualities")
            .and_then(|index| args.get(index + 1))
            .map(String::as_str),
    )?;

    let mut inputs = Vec::new();
    collect_pngs(&corpus, &mut inputs, recursive)?;
    inputs.sort();
    if let Some(limit) = max_files {
        inputs.truncate(limit);
    }
    if inputs.is_empty() {
        return Err(format!(
            "no PNG images found below {} (non-recursive by default; pass --recursive for subdirs)",
            corpus.display()
        ));
    }

    fs::create_dir_all(output.join("raw")).map_err(|err| err.to_string())?;
    let config = OracleConfig {
        targets,
        quant_qualities: qualities,
        body_fractions: default_oracle_body_fractions(),
        output_dir: output.clone(),
    };

    println!(
        "schema=jp2lam.perceptual-oracle-raw/1 metric={} sources={} out={}",
        jp2lam::METRIC_VERSION,
        inputs.len(),
        output.display()
    );
    for input in inputs {
        let image = load_png(&input)?;
        let result = sweep_source(&image, Some(&input), &config).map_err(|err| err.to_string())?;
        if result.skipped {
            println!("skip {}", input.display());
            continue;
        }
        for label in &result.labels {
            println!(
                "label source={} target={} status={:?} quant={:?} body={:?} score={:.4} bytes={:?}",
                input.display(),
                label.target,
                label.status,
                label.best_quant_quality,
                label.best_pcrd_body,
                label.achieved_score,
                label.output_bytes
            );
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

fn parse_targets(value: Option<&str>) -> Result<Vec<f64>, String> {
    match value {
        None => Ok(default_oracle_targets()),
        Some(raw) => raw
            .split(',')
            .map(|part| {
                let target: f64 = part
                    .parse()
                    .map_err(|_| format!("invalid target `{part}`"))?;
                if !target.is_finite() || !(0.0..100.0).contains(&target) {
                    return Err(format!("target must be in 0..100, got {target}"));
                }
                Ok(target)
            })
            .collect(),
    }
}

fn parse_qualities(value: Option<&str>) -> Result<Vec<u8>, String> {
    match value {
        None => Ok(default_oracle_quant_qualities()),
        Some(raw) => raw
            .split(',')
            .map(|part| {
                let quality: u8 = part
                    .parse()
                    .map_err(|_| format!("invalid quality `{part}`"))?;
                if quality > 99 {
                    return Err(format!("quantizer quality must be 0..=99, got {quality}"));
                }
                Ok(quality)
            })
            .collect(),
    }
}

fn collect_pngs(directory: &Path, out: &mut Vec<PathBuf>, recursive: bool) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            if recursive {
                collect_pngs(&path, out, true)?;
            }
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        {
            out.push(path);
        }
    }
    Ok(())
}
