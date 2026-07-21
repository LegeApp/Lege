/// Standalone debug tool for the slow OCR pipeline.
///
/// Usage:
///   lege-ocr-debug input.png [--out debug/] [--lang eng] [--detections detections.json]
///
/// Accepts a pre-rendered PNG (no pdfium dependency). If --detections is not provided,
/// the whole image is treated as a single text region.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::RgbImage;
use lege_ocr::{
    OcrPipeline, SlowOcrConfig, coordinate::CoordinateMap, normalize, types::TextRegion,
};

fn main() -> Result<()> {
    let _ = env_logger::try_init();

    let args: Vec<String> = std::env::args().collect();
    let parsed = parse_args(&args)?;

    println!("lege-ocr-debug: loading {}", parsed.input.display());

    // Load image
    let rgb: RgbImage = image::open(&parsed.input)
        .with_context(|| format!("cannot open {}", parsed.input.display()))?
        .into_rgb8();

    let w = rgb.width();
    let h = rgb.height();
    println!("  image size: {w}x{h}");

    // Build the same standardized analysis mask used by the integrated path.
    let gray_flat: Vec<u8> = rgb
        .pixels()
        .map(|p| (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32).round() as u8)
        .collect();
    let binary = normalize::build_analysis_binary(&gray_flat, &rgb);

    // Load detections or create a whole-page region
    let regions = if let Some(ref det_path) = parsed.detections {
        load_detections_json(det_path)
            .with_context(|| format!("cannot load detections from {}", det_path.display()))?
    } else {
        vec![TextRegion {
            page_index: 0,
            region_id: 0,
            class_name: Some("page".to_string()),
            bbox_highres: [0, 0, w, h],
            confidence: 1.0,
        }]
    };

    println!("  {} text regions", regions.len());

    // Build coordinate map (identity since we have no PDF page size info)
    let coord_map = CoordinateMap::identity(w, h, w as f32, h as f32);

    // Configure pipeline with debug output
    let out_dir = parsed
        .out_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("debug"));
    let config = SlowOcrConfig {
        language: parsed.lang.clone(),
        debug: true,
        debug_out_dir: Some(out_dir.clone()),
        ..Default::default()
    };

    let pipeline = OcrPipeline::new(config);

    println!("  running slow OCR pipeline...");
    let result = pipeline.process_page(&rgb, &binary, &regions, &coord_map, 0)?;

    println!("  extracted {} lines", result.lines.len());
    for (i, line) in result.lines.iter().enumerate() {
        println!(
            "    line {i:3}: {:?}",
            &line.text.chars().take(60).collect::<String>()
        );
    }

    println!("  debug output written to {}", out_dir.display());
    Ok(())
}

struct Args {
    input: PathBuf,
    out_dir: Option<PathBuf>,
    lang: String,
    detections: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Args> {
    if args.len() < 2 {
        anyhow::bail!(
            "Usage: lege-ocr-debug <input.png> [--out DIR] [--lang LANG] [--detections FILE]"
        );
    }

    let input = PathBuf::from(&args[1]);
    let mut out_dir = None;
    let mut lang = "eng".to_string();
    let mut detections = None;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--out" | "-o" => {
                i += 1;
                out_dir = Some(PathBuf::from(
                    args.get(i).context("--out requires a value")?,
                ));
            }
            "--lang" | "-l" => {
                i += 1;
                lang = args.get(i).context("--lang requires a value")?.clone();
            }
            "--detections" | "-d" => {
                i += 1;
                detections = Some(PathBuf::from(
                    args.get(i).context("--detections requires a value")?,
                ));
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    Ok(Args {
        input,
        out_dir,
        lang,
        detections,
    })
}

/// Load a JSON file that is a list of `TextRegion`-compatible objects.
///
/// Expected format:
/// ```json
/// [{"region_id": 0, "class_name": "text", "bbox_highres": [x1,y1,x2,y2], "confidence": 0.9}]
/// ```
fn load_detections_json(path: &Path) -> Result<Vec<TextRegion>> {
    let raw = std::fs::read_to_string(path)?;
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
    let mut regions = Vec::new();
    for (i, v) in parsed.iter().enumerate() {
        let class_name = v["class_name"].as_str().map(str::to_owned);
        let confidence = v["confidence"].as_f64().unwrap_or(1.0) as f32;
        let bbox = v["bbox_highres"]
            .as_array()
            .and_then(|a| {
                if a.len() == 4 {
                    Some([
                        a[0].as_u64()? as u32,
                        a[1].as_u64()? as u32,
                        a[2].as_u64()? as u32,
                        a[3].as_u64()? as u32,
                    ])
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow::anyhow!("detection {i} missing bbox_highres [x1,y1,x2,y2]"))?;

        regions.push(TextRegion {
            page_index: 0,
            region_id: v["region_id"].as_u64().unwrap_or(i as u64) as usize,
            class_name,
            bbox_highres: bbox,
            confidence,
        });
    }
    Ok(regions)
}
