//! Side-by-side comparison of two layout models through the same lege-gpu
//! runtime: DocLayout-YOLO (DocStructBench) vs PP-DocLayout-M.
//!
//! Usage:
//!   cargo run -p lege-gpu --example layout_compare -- \
//!     --yolo path/to/yolo-layout.onnx \
//!     --pp   path/to/doclayout-m.prepared.onnx \
//!     --out  /tmp/overlays \
//!     page1.png page2.png ...
//!
//! For each image it runs whichever models were given, prints per-detection
//! label + the 4-way ContentCategory it maps to, a category tally, and warm
//! inference time, and writes a category-colored overlay per (image, model).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use image::{Rgb, RgbImage};
use lege_gpu::vision::{LayoutDetection, LayoutDetector};

/// Collapse a model's raw label into the four categories the Lege pipeline
/// actually acts on. Covers both the YOLO (DocStructBench) and PP-DocLayout
/// vocabularies by name.
fn category(name: &str) -> &'static str {
    match name {
        "figure" | "image" | "chart" | "seal" | "header_image" | "footer_image" => "Image",
        "table" => "Table",
        "abandon" | "header" | "footer" | "number" | "page_number" => "Abandon",
        _ => "Text",
    }
}

fn category_color(cat: &str) -> Rgb<u8> {
    match cat {
        "Image" => Rgb([0, 130, 200]),
        "Table" => Rgb([245, 130, 48]),
        "Abandon" => Rgb([150, 150, 150]),
        _ => Rgb([60, 180, 75]),
    }
}

struct Args {
    yolo: Option<PathBuf>,
    pp: Option<PathBuf>,
    out: PathBuf,
    images: Vec<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut yolo = None;
    let mut pp = None;
    let mut out = PathBuf::from("layout_compare_out");
    let mut images = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--yolo" => yolo = Some(PathBuf::from(it.next().context("--yolo needs a path")?)),
            "--pp" => pp = Some(PathBuf::from(it.next().context("--pp needs a path")?)),
            "--out" => out = PathBuf::from(it.next().context("--out needs a path")?),
            other => images.push(PathBuf::from(other)),
        }
    }
    if images.is_empty() {
        bail!("no input images given");
    }
    if yolo.is_none() && pp.is_none() {
        bail!("give at least one of --yolo / --pp");
    }
    Ok(Args {
        yolo,
        pp,
        out,
        images,
    })
}

fn run_model(
    tag: &str,
    detector: &LayoutDetector,
    image: &RgbImage,
    name: &Path,
    out_dir: &Path,
) -> Result<()> {
    // Warm once, then take the best of three as the inference time.
    let _ = detector.detect_rgb(image)?;
    let mut best = f64::INFINITY;
    let mut detections = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        detections = detector.detect_rgb(image)?;
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
    }

    let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
    for det in &detections {
        *tally.entry(category(det.class_name)).or_default() += 1;
    }
    let tally_str = ["Text", "Image", "Table", "Abandon"]
        .iter()
        .map(|c| format!("{c}={}", tally.get(c).copied().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "  [{tag:5}] {:>3} detections  {:.1} ms  |  {tally_str}",
        detections.len(),
        best
    );
    for det in detections.iter().take(12) {
        println!(
            "         {:16} {:>5.2}  ({:>4.0},{:>4.0})-({:>4.0},{:>4.0}) -> {}",
            det.class_name,
            det.confidence,
            det.bbox[0],
            det.bbox[1],
            det.bbox[2],
            det.bbox[3],
            category(det.class_name),
        );
    }

    let overlay = draw(image, &detections);
    std::fs::create_dir_all(out_dir).ok();
    let stem = name.file_stem().and_then(|s| s.to_str()).unwrap_or("page");
    let out = out_dir.join(format!("{stem}_{tag}.png"));
    overlay
        .save(&out)
        .with_context(|| format!("save {out:?}"))?;
    println!("         overlay -> {}", out.display());
    Ok(())
}

fn draw(image: &RgbImage, detections: &[LayoutDetection]) -> RgbImage {
    let mut out = image.clone();
    let (w, h) = out.dimensions();
    for det in detections {
        let color = category_color(category(det.class_name));
        let x1 = det.bbox[0].round().clamp(0.0, (w - 1) as f32) as u32;
        let y1 = det.bbox[1].round().clamp(0.0, (h - 1) as f32) as u32;
        let x2 = det.bbox[2].round().clamp(0.0, (w - 1) as f32) as u32;
        let y2 = det.bbox[3].round().clamp(0.0, (h - 1) as f32) as u32;
        for t in 0..3 {
            for x in x1..=x2 {
                if y1 + t < h {
                    out.put_pixel(x, y1 + t, color);
                }
                if y2 >= t {
                    out.put_pixel(x, y2 - t, color);
                }
            }
            for y in y1..=y2 {
                if x1 + t < w {
                    out.put_pixel(x1 + t, y, color);
                }
                if x2 >= t {
                    out.put_pixel(x2 - t, y, color);
                }
            }
        }
    }
    out
}

fn main() -> Result<()> {
    let args = parse_args()?;

    let yolo = args
        .yolo
        .as_ref()
        .map(|p| LayoutDetector::from_model_path(p).context("load YOLO model"))
        .transpose()?;
    let pp = args
        .pp
        .as_ref()
        .map(|p| LayoutDetector::from_model_path(p).context("load PP model"))
        .transpose()?;

    for image_path in &args.images {
        let image = image::open(image_path)
            .with_context(|| format!("open {image_path:?}"))?
            .to_rgb8();
        println!(
            "\n=== {} ({}x{}) ===",
            image_path.display(),
            image.width(),
            image.height()
        );
        if let Some(det) = &yolo {
            run_model("yolo", det, &image, image_path, &args.out)?;
        }
        if let Some(det) = &pp {
            run_model("pp", det, &image, image_path, &args.out)?;
        }
    }
    Ok(())
}
