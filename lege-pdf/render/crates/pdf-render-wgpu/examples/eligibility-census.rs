//! Deterministic sweep-result sample of experimental GPU page eligibility.
//!
//! This does not initialize WGPU. It compiles sampled pages, runs the exact
//! static production classifier, and decode-confirms statically eligible
//! pages through the CPU preparation seam.
//!
//! `cargo run --release -p pdf-render-wgpu --example eligibility-census -- results.csv 240 2 out.csv`
//! `cargo run --release -p pdf-render-wgpu --example eligibility-census -- book.pdf 60 2 out.csv`

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, OutputFormat, OutputResidency, PageTransform, RenderColorPolicy,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;
use pdf_render_wgpu::classify_gpu_eligibility;
use pdf_source::{MmapSource, PdfSource};

const DEFAULT_SAMPLE: usize = 240;
const DEFAULT_SCALE: f64 = 2.0;
const CONSERVATIVE_STORAGE_BINDING_BYTES: u64 = 128 * 1024 * 1024;
const MAX_BOX_FOOTPRINT: f64 = 64.0;

#[derive(Debug)]
struct Candidate {
    score: u64,
    file: String,
    page: u32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = PathBuf::from(args.next().ok_or(
        "usage: eligibility-census <results.csv|book.pdf> [sample-pages] [scale] [out.csv]",
    )?);
    let sample = args
        .next()
        .and_then(|value| value.to_string_lossy().parse::<usize>().ok())
        .unwrap_or(DEFAULT_SAMPLE)
        .max(1);
    let scale = args
        .next()
        .and_then(|value| value.to_string_lossy().parse::<f64>().ok())
        .unwrap_or(DEFAULT_SCALE);
    let output = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("gpu-eligibility-census.csv"));

    let mut candidates = if input
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        load_pdf_candidates(&input)?
    } else {
        load_candidates(&input)?
    };
    candidates.sort_unstable_by_key(|candidate| candidate.score);
    candidates.truncate(sample);
    candidates.sort_unstable_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then_with(|| left.page.cmp(&right.page))
    });
    eprintln!(
        "GPU eligibility census: {} deterministic sweep pages, scale={scale}",
        candidates.len()
    );

    let mut output_writer = BufWriter::new(File::create(&output)?);
    writeln!(
        output_writer,
        "file,page,status,features,operations,images,image_draws,ignored_text_draws,static_eligible,prepared_eligible,reasons,note"
    )?;
    let backend = CpuBackend::default();
    let mut current_file = String::new();
    let mut current_snapshot: Option<DocumentSnapshot> = None;
    let mut counts = BTreeMap::<String, u64>::new();
    let mut static_eligible = 0u64;
    let mut prepared_eligible = 0u64;
    let mut image_pages = 0u64;
    let mut ignored_text_pages = 0u64;
    let mut ignored_text_draws = 0u64;

    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.file != current_file {
            current_file.clone_from(&candidate.file);
            current_snapshot = open_snapshot(Path::new(&candidate.file)).ok();
        }
        let Some(snapshot) = current_snapshot.as_ref() else {
            increment(&mut counts, "open-error");
            write_row(
                &mut output_writer,
                candidate,
                "open-error",
                "",
                0,
                0,
                0,
                0,
                false,
                false,
                "open-error",
                "document could not be opened",
            )?;
            continue;
        };

        let mut parse = ParseContext::new();
        let compiled = match pdf_content::PageCompiler::new()
            .with_annotations(true)
            .compile(snapshot, PageIndex(candidate.page), &mut parse)
        {
            Ok(page) => page,
            Err(error) => {
                increment(&mut counts, "compile-error");
                write_row(
                    &mut output_writer,
                    candidate,
                    "compile-error",
                    "",
                    0,
                    0,
                    0,
                    0,
                    false,
                    false,
                    "compile-error",
                    &error.to_string(),
                )?;
                continue;
            }
        };
        let features = format!("{:?}", compiled.features);
        let operations = compiled.operations.len();
        let images = compiled.images.len();
        let request = request(Arc::new(compiled), scale);
        let report = classify_gpu_eligibility(&request.page, &request);
        if report.image_draws > 0 {
            image_pages += 1;
        }
        if report.ignored_text_draws > 0 {
            ignored_text_pages += 1;
            ignored_text_draws += u64::from(report.ignored_text_draws);
        }
        let is_static_eligible = report.is_eligible();
        if is_static_eligible {
            static_eligible += 1;
        }
        let mut reasons = report
            .reasons
            .iter()
            .map(|reason| reason.as_str())
            .collect::<Vec<_>>();
        let mut note = String::new();
        let mut is_prepared_eligible = false;

        if is_static_eligible {
            let page_bytes =
                request.output_size.width as u64 * request.output_size.height as u64 * 4;
            if page_bytes > CONSERVATIVE_STORAGE_BINDING_BYTES {
                reasons.push("page-storage-over-128m");
            } else {
                match backend.prepare_rgb_image_page(&request) {
                    Ok(Some(prepared))
                        if prepared.images.iter().all(|image| {
                            image.footprint[0].is_finite()
                                && image.footprint[1].is_finite()
                                && image.footprint[0] <= MAX_BOX_FOOTPRINT
                                && image.footprint[1] <= MAX_BOX_FOOTPRINT
                        }) =>
                    {
                        is_prepared_eligible = true;
                        prepared_eligible += 1;
                    }
                    Ok(Some(_)) => reasons.push("footprint-limit"),
                    Ok(None) => reasons.push("prepared-vocabulary-decline"),
                    Err(error) => {
                        reasons.push("preparation-error");
                        note = error.to_string();
                    }
                }
            }
        }
        if reasons.is_empty() {
            increment(&mut counts, "eligible");
        } else {
            for reason in &reasons {
                increment(&mut counts, reason);
            }
        }
        write_row(
            &mut output_writer,
            candidate,
            "ok",
            &features,
            operations,
            images,
            report.image_draws,
            report.ignored_text_draws,
            is_static_eligible,
            is_prepared_eligible,
            &reasons.join(";"),
            &note,
        )?;
        if (index + 1).is_multiple_of(20) || index + 1 == candidates.len() {
            eprintln!(
                "  {}/{} compiled; static={} prepared={}",
                index + 1,
                candidates.len(),
                static_eligible,
                prepared_eligible
            );
        }
    }
    output_writer.flush()?;

    let mut ranked = counts.into_iter().collect::<Vec<_>>();
    ranked.sort_unstable_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    println!("sampled pages: {}", candidates.len());
    println!("pages with image draws: {image_pages}");
    println!("static eligible: {static_eligible}");
    println!("decode-confirmed eligible: {prepared_eligible}");
    if image_pages > 0 {
        println!(
            "decode-confirmed share of image pages: {:.1}%",
            prepared_eligible as f64 * 100.0 / image_pages as f64
        );
    }
    println!(
        "pages with ignored non-painting text: {ignored_text_pages} ({ignored_text_draws} draws)"
    );
    for (reason, count) in ranked {
        println!("{reason}: {count}");
    }
    println!("wrote {}", output.display());
    Ok(())
}

fn open_snapshot(path: &Path) -> Result<DocumentSnapshot, Box<dyn std::error::Error>> {
    let source: Arc<dyn PdfSource> = Arc::new(MmapSource::open(path)?);
    Ok(DocumentSnapshot::open(source, DocumentLimits::default())?)
}

fn load_candidates(path: &Path) -> Result<Vec<Candidate>, Box<dyn std::error::Error>> {
    let reader = BufReader::new(File::open(path)?);
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for (line_number, line) in reader.lines().enumerate() {
        let line = line?;
        if line_number == 0 {
            continue;
        }
        let fields = parse_csv_line(&line);
        if fields.len() < 4 || fields[3] != "pdfium" {
            continue;
        }
        let Ok(page) = fields[2].parse::<u32>() else {
            continue;
        };
        let file = fields[1].clone();
        if !seen.insert((file.clone(), page)) {
            continue;
        }
        candidates.push(Candidate {
            score: stable_score(&file, page),
            file,
            page,
        });
    }
    Ok(candidates)
}

fn load_pdf_candidates(path: &Path) -> Result<Vec<Candidate>, Box<dyn std::error::Error>> {
    let snapshot = open_snapshot(path)?;
    let file = path.to_string_lossy().into_owned();
    Ok((0..snapshot.page_count())
        .map(|page| Candidate {
            score: stable_score(&file, page),
            file: file.clone(),
            page,
        })
        .collect())
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                let _ = chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(character),
        }
    }
    fields.push(field);
    fields
}

fn stable_score(file: &str, page: u32) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in file.bytes().chain(page.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn increment(counts: &mut BTreeMap<String, u64>, key: &str) {
    *counts.entry(key.to_owned()).or_default() += 1;
}

#[allow(clippy::too_many_arguments)]
fn write_row(
    writer: &mut impl Write,
    candidate: &Candidate,
    status: &str,
    features: &str,
    operations: usize,
    images: usize,
    image_draws: u32,
    ignored_text_draws: u32,
    static_eligible: bool,
    prepared_eligible: bool,
    reasons: &str,
    note: &str,
) -> std::io::Result<()> {
    writeln!(
        writer,
        "{},{},{},{},{operations},{images},{image_draws},{ignored_text_draws},{static_eligible},{prepared_eligible},{},{}",
        csv_field(&candidate.file),
        candidate.page,
        csv_field(status),
        csv_field(features),
        csv_field(reasons),
        csv_field(note)
    )
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn request(page: Arc<pdf_page_ir::CompiledPage>, scale: f64) -> RenderRequest {
    let crop = page.bounds.crop;
    let (crop_width, crop_height) = ((crop.x1 - crop.x0) * scale, (crop.y1 - crop.y0) * scale);
    let (width, height) = match page.bounds.rotate {
        90 | 270 => (crop_height, crop_width),
        _ => (crop_width, crop_height),
    };
    let matrix = display_matrix(&page.bounds, scale);
    RenderRequest {
        page,
        transform: PageTransform { matrix },
        crop: None,
        output_size: DeviceSize {
            width: width.ceil().max(1.0) as u32,
            height: height.ceil().max(1.0) as u32,
        },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        color_policy: RenderColorPolicy::Original,
        annotations: AnnotationMode::StaticAppearances,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    }
}

fn display_matrix(bounds: &pdf_page_ir::PageBounds, scale: f64) -> Matrix {
    let crop = bounds.crop;
    match bounds.rotate {
        90 => Matrix {
            a: 0.0,
            b: scale,
            c: scale,
            d: 0.0,
            e: -crop.y0 * scale,
            f: -crop.x0 * scale,
        },
        180 => Matrix {
            a: -scale,
            b: 0.0,
            c: 0.0,
            d: scale,
            e: crop.x1 * scale,
            f: -crop.y0 * scale,
        },
        270 => Matrix {
            a: 0.0,
            b: -scale,
            c: -scale,
            d: 0.0,
            e: crop.y1 * scale,
            f: crop.x1 * scale,
        },
        _ => Matrix {
            a: scale,
            b: 0.0,
            c: 0.0,
            d: -scale,
            e: -crop.x0 * scale,
            f: crop.y1 * scale,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_parser_handles_quoted_commas_and_quotes() {
        assert_eq!(
            parse_csv_line("3,\"/a,b/\"\"book\"\".pdf\",7,pdfium"),
            ["3", "/a,b/\"book\".pdf", "7", "pdfium"]
        );
    }

    #[test]
    fn stable_sampling_score_changes_with_page() {
        assert_ne!(stable_score("/book.pdf", 1), stable_score("/book.pdf", 2));
    }
}
