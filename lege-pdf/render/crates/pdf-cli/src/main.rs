//! `pdfr` — development driver for the engine.
//!
//! Subcommands:
//!   info <file.pdf>                     structural summary + recovery log
//!   doctor <file.pdf>                   read-only document health report (pdf-read)
//!   dump <file.pdf> <page>              semantic display-list dump (Phase 2)
//!   text <file.pdf> <page>              extracted text and optional geometry
//!   render <file.pdf> <page> <out.ppm>  render one page via the selected backend
//!   attribute <file.pdf> <page> <prefix> diagnostic leaf/origin planes
//!
//! Deliberately dependency-light (no clap yet); this binary exists to
//! exercise the full open → compile → render path and will grow a `diff`
//! subcommand.

use std::sync::Arc;

use anyhow::{Context, Result};
use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, OutputFormat, OutputResidency, PageTransform, RenderLimits,
    RenderQuality, RenderRequest,
};
use pdf_render_cpu::{CpuBackend, CpuBackendOptions};
use pdf_render_wgpu::{ExperimentalImageRenderer, ImageRenderExecution, ImageRendererPreference};
use pdf_source::OwnedBytesSource;

/// Remove a boolean flag from `args`, reporting whether it was present.
fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    match args.iter().position(|a| a == name) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    }
}

/// Remove `--name <value>` from `args`, returning the value when present.
fn take_flag_value(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    if i + 1 >= args.len() {
        return None;
    }
    let value = args.remove(i + 1);
    args.remove(i);
    Some(value)
}

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let password = take_flag_value(&mut args, "--password");
    let password = password.as_deref();
    let system_fonts = take_flag(&mut args, "--system-fonts");
    let text_chars = take_flag(&mut args, "--chars");
    let text_rects = take_flag(&mut args, "--rects");
    let text_words = take_flag(&mut args, "--words");
    let text_rtl = take_flag(&mut args, "--rtl");
    let text_no_normalize = take_flag(&mut args, "--no-normalize");
    let scale = take_flag_value(&mut args, "--scale")
        .map(|value| {
            value
                .parse::<f64>()
                .context("scale must be a positive number")
        })
        .transpose()?;
    if scale.is_some_and(|value| !value.is_finite() || value <= 0.0) {
        anyhow::bail!("scale must be a positive number");
    }
    match args.first().map(String::as_str) {
        Some("info") if args.len() == 2 => info(&args[1], password),
        Some("doctor") if args.len() == 2 => doctor(&args[1], password),
        Some("dump") if args.len() == 3 => {
            let page: u32 = args[2].parse().context("page must be an integer")?;
            dump(&args[1], page, password)
        }
        Some("text") if args.len() == 3 => {
            let page: u32 = args[2].parse().context("page must be an integer")?;
            text(
                &args[1],
                page,
                password,
                TextOutput {
                    chars: text_chars,
                    rects: text_rects,
                    words: text_words,
                    rtl: text_rtl,
                    normalize: !text_no_normalize,
                },
            )
        }
        Some("render") if args.len() == 4 => {
            let page: u32 = args[2].parse().context("page must be an integer")?;
            render(&args[1], page, &args[3], system_fonts, password)
        }
        Some("attribute") if args.len() == 4 => {
            let page: u32 = args[2].parse().context("page must be an integer")?;
            attribute(
                &args[1],
                page,
                &args[3],
                scale.unwrap_or(150.0 / 72.0),
                system_fonts,
                password,
            )
        }
        _ => {
            eprintln!(
                "usage:\n  pdfr info <file.pdf> [--password <pw>]\n  \
                 pdfr doctor <file.pdf> [--password <pw>]\n  \
                 pdfr dump <file.pdf> <page> [--password <pw>]\n  \
                 pdfr text <file.pdf> <page> [--chars] [--rects] [--words] [--rtl] [--no-normalize] [--password <pw>]\n  \
                 pdfr render <file.pdf> <page> <out.ppm> [--system-fonts] [--password <pw>]\n  \
                 pdfr attribute <file.pdf> <page> <out-prefix> [--scale N] [--system-fonts] [--password <pw>]\n\n\
                 experimental render backend: LEGE_PDF_IMAGE_RENDERER=cpu|gpu|auto (default: cpu)"
            );
            std::process::exit(2);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TextOutput {
    chars: bool,
    rects: bool,
    words: bool,
    rtl: bool,
    normalize: bool,
}

fn text(path: &str, page_number: u32, password: Option<&str>, output: TextOutput) -> Result<()> {
    let snapshot = open(path, password)?;
    let mut context = ParseContext::new();
    let page = PageCompiler::new()
        .compile_semantic(&snapshot, PageIndex(page_number), &mut context)
        .with_context(|| format!("compiling page {page_number}"))?;
    let text_page = pdf_text::TextPage::build(
        &page,
        &pdf_text::TextPageOptions {
            rtl: output.rtl,
            normalize: output.normalize,
            ..pdf_text::TextPageOptions::default()
        },
    );
    let detailed = output.chars || output.rects || output.words;
    if !detailed {
        print!("{}", text_page.all_text());
        return Ok(());
    }
    if output.chars {
        for (index, info) in text_page.chars().iter().enumerate() {
            let printable = char::from_u32(info.unicode)
                .filter(|character| !character.is_control())
                .unwrap_or('\u{fffd}');
            println!(
                "char {index}: U+{:04X} {:?} '{}' code={} cid={} gid={} box=[{:.4},{:.4},{:.4},{:.4}]",
                info.unicode,
                info.char_type,
                printable,
                info.char_code,
                info.cid,
                info.glyph_id,
                info.char_box.x0,
                info.char_box.y0,
                info.char_box.x1,
                info.char_box.y1,
            );
        }
    }
    if output.rects {
        for (index, rect) in text_page
            .rects(0, text_page.char_count())
            .into_iter()
            .enumerate()
        {
            println!(
                "rect {index}: [{:.4},{:.4},{:.4},{:.4}] text={:?}",
                rect.x0,
                rect.y0,
                rect.x1,
                rect.y1,
                text_page.text_in_rect(rect),
            );
        }
    }
    if output.words {
        for (index, word) in text_page.words().into_iter().enumerate() {
            println!(
                "word {index}: chars={}..{} box=[{:.4},{:.4},{:.4},{:.4}] continued={} text={:?}",
                word.first_char,
                word.first_char + word.char_count,
                word.bbox.x0,
                word.bbox.y0,
                word.bbox.x1,
                word.bbox.y1,
                word.continued,
                word.text,
            );
        }
    }
    Ok(())
}

fn open(path: &str, password: Option<&str>) -> Result<DocumentSnapshot> {
    // MmapSource replaces this once pdf-source Phase 1 lands.
    let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    let source = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open_with_password(source, DocumentLimits::default(), password)
        .with_context(|| format!("opening {path}"))
}

/// qpdf-style read-only document doctor (`pdf-read`): structure, xref
/// health, encryption, per-page compile status, annotations, features.
fn doctor(path: &str, password: Option<&str>) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    let source = Arc::new(OwnedBytesSource::new(bytes));
    let report = pdf_read::examine_with_password(source, DocumentLimits::default(), password);
    print!("{}", report.summary());
    Ok(())
}

fn info(path: &str, password: Option<&str>) -> Result<()> {
    let snapshot = open(path, password)?;
    println!("pages: {}", snapshot.page_count());
    println!("version: {:?}", snapshot.structure().version);
    for event in snapshot.recovery_events() {
        println!("recovery: {event:?}");
    }
    Ok(())
}

fn dump(path: &str, page_number: u32, password: Option<&str>) -> Result<()> {
    let snapshot = open(path, password)?;
    let mut ctx = ParseContext::new();
    let compiler = PageCompiler::new();
    let page = compiler
        .compile_semantic(&snapshot, PageIndex(page_number), &mut ctx)
        .with_context(|| format!("compiling page {page_number}"))?;
    print!(
        "{}",
        pdf_content::dump::dump_semantic(&page, snapshot.names())
    );
    Ok(())
}

/// Build the compiler, optionally resolving non-embedded fonts against the
/// machine's installed fonts (`--system-fonts`, fonts.md Font Phase 7).
///
/// Off by default on purpose: system fonts make output depend on what is
/// installed, so the deterministic bundled-face path stays the default.
fn compiler(system_fonts: bool, annotations: AnnotationMode) -> PageCompiler {
    // `AnnotationMode` (render API) maps onto the compiler's annotation
    // pass: `StaticAppearances` — the default — bakes `/AP /N` streams
    // into the compiled page.
    let c = PageCompiler::new()
        .with_annotations(matches!(annotations, AnnotationMode::StaticAppearances));
    if system_fonts {
        c.with_system_fonts(Arc::new(pdf_font::FolderFontProvider::system()))
    } else {
        c
    }
}

fn render(
    path: &str,
    page_number: u32,
    out: &str,
    system_fonts: bool,
    password: Option<&str>,
) -> Result<()> {
    let snapshot = open(path, password)?;
    let mut ctx = ParseContext::new();
    let annotations = AnnotationMode::default();
    let compiler = compiler(system_fonts, annotations);
    let page = Arc::new(compiler.compile(&snapshot, PageIndex(page_number), &mut ctx)?);

    // 150 DPI initial default; PDF user space is 72 units/inch.
    let scale = 150.0 / 72.0;
    let crop = page.bounds.crop;
    // /Rotate 90/270 swaps the displayed extent (PDFium's page dims are
    // post-rotation; viewers show the page rotated).
    let (width, height) = match page.bounds.rotate {
        90 | 270 => (
            (crop.height() * scale).ceil() as u32,
            (crop.width() * scale).ceil() as u32,
        ),
        _ => (
            (crop.width() * scale).ceil() as u32,
            (crop.height() * scale).ceil() as u32,
        ),
    };

    // Map page user space (y-up) to the device surface (y-down), translating
    // the crop origin to (0,0) and applying the page /Rotate as clockwise
    // quarter turns (CPDF_Page::GetDisplayMatrix semantics):
    //   0:   dx = (x-x0)·s,  dy = (y1-y)·s
    //   90:  dx = (y-y0)·s,  dy = (x-x0)·s
    //   180: dx = (x1-x)·s,  dy = (y-y0)·s
    //   270: dx = (y1-y)·s,  dy = (x1-x)·s
    let s = scale;
    let device = match page.bounds.rotate {
        90 => Matrix {
            a: 0.0,
            b: s,
            c: s,
            d: 0.0,
            e: -crop.y0 * s,
            f: -crop.x0 * s,
        },
        180 => Matrix {
            a: -s,
            b: 0.0,
            c: 0.0,
            d: s,
            e: crop.x1 * s,
            f: -crop.y0 * s,
        },
        270 => Matrix {
            a: 0.0,
            b: -s,
            c: -s,
            d: 0.0,
            e: crop.y1 * s,
            f: crop.x1 * s,
        },
        _ => Matrix {
            a: s,
            b: 0.0,
            c: 0.0,
            d: -s,
            e: -crop.x0 * s,
            f: crop.y1 * s,
        },
    };

    let request = RenderRequest {
        page,
        transform: PageTransform { matrix: device },
        crop: None,
        output_size: DeviceSize { width, height },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations,
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };
    // CPU remains the default. `LEGE_PDF_IMAGE_RENDERER=gpu|auto` opts this
    // production full-page path into the experimental resident image renderer.
    // Ineligible pages and recoverable GPU failures fall back as whole requests.
    let preference = ImageRendererPreference::from_env()?;
    let renderer = ExperimentalImageRenderer::new(preference, CpuBackendOptions::default())?;
    let result = renderer.render_to_host(&request)?;
    let host = result.host;

    write_ppm(out, &host)?;
    println!("wrote {out} ({}x{})", host.width, host.height);
    match result.execution {
        ImageRenderExecution::Gpu(stats) => {
            println!(
                "renderer: wgpu (images={}, cache_hits={}, uploaded={} bytes, readback={} bytes)",
                stats.image_draws, stats.cache_hits, stats.uploaded_bytes, stats.readback_bytes
            );
        }
        ImageRenderExecution::Cpu(stats) => {
            println!("renderer: cpu");
            if stats.degraded_draws > 0 {
                // A dropped codec draw is content PDFium would paint that we could not.
                // Flag it — loudly if it left the page blank (Workstream B3).
                let tag = if stats.is_silent_blank() {
                    "SILENT BLANK"
                } else {
                    "degraded"
                };
                eprintln!(
                    "warning: {tag}: {} image draw(s) dropped (undecodable codec)",
                    stats.degraded_draws
                );
                for note in &stats.recovery_notes {
                    eprintln!("  - {note}");
                }
            }
        }
    }
    Ok(())
}

fn attribute(
    path: &str,
    page_number: u32,
    out_prefix: &str,
    scale: f64,
    system_fonts: bool,
    password: Option<&str>,
) -> Result<()> {
    let snapshot = open(path, password)?;
    let mut ctx = ParseContext::new();
    let annotations = AnnotationMode::default();
    let page = Arc::new(
        compiler(system_fonts, annotations)
            .compile(&snapshot, PageIndex(page_number), &mut ctx)
            .with_context(|| format!("compiling page {page_number}"))?,
    );
    let request = request_for_page(page, scale, annotations);
    let backend = CpuBackend::default();
    let prepared = backend.prepare_attribution(&request)?;
    let map = pdf_render_cpu::attribution::render_attribution(&prepared);

    let leaf_path = format!("{out_prefix}.leaf.png");
    let origin_path = format!("{out_prefix}.origin.png");
    let legend_path = format!("{out_prefix}.legend.json");
    write_gray_png(&leaf_path, map.width, map.height, &map.leaf)?;
    write_gray_png(&origin_path, map.width, map.height, &map.origin)?;
    let leaf = [
        pdf_page_ir::PaintLeaf::Unpainted,
        pdf_page_ir::PaintLeaf::Path,
        pdf_page_ir::PaintLeaf::Shading,
        pdf_page_ir::PaintLeaf::TilingPattern,
        pdf_page_ir::PaintLeaf::Text,
        pdf_page_ir::PaintLeaf::Image,
    ];
    let origin = [
        pdf_page_ir::PaintOrigin::PageContent,
        pdf_page_ir::PaintOrigin::FormXObject,
        pdf_page_ir::PaintOrigin::AnnotationAppearance,
        pdf_page_ir::PaintOrigin::TilingPatternCell,
        pdf_page_ir::PaintOrigin::Type3Glyph,
        pdf_page_ir::PaintOrigin::SoftMaskContent,
    ];
    let entries = |names: &[&str]| {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| format!("\"{index}\":\"{name}\""))
            .collect::<Vec<_>>()
            .join(",")
    };
    let leaf_names = leaf.iter().map(|value| value.name()).collect::<Vec<_>>();
    let origin_names = origin.iter().map(|value| value.name()).collect::<Vec<_>>();
    let legend = format!(
        "{{\n  \"leaf\": {{{}}},\n  \"origin\": {{{}}},\n  \"coverage_threshold\": {},\n  \"scale\": {},\n  \"origin_precedence\": \"innermost-wins\",\n  \"diagnostic_only\": true\n}}\n",
        entries(&leaf_names),
        entries(&origin_names),
        map.coverage_threshold,
        scale,
    );
    std::fs::write(&legend_path, legend).with_context(|| format!("writing {legend_path}"))?;
    println!("wrote {leaf_path}, {origin_path}, {legend_path}");
    Ok(())
}

fn request_for_page(
    page: Arc<pdf_page_ir::CompiledPage>,
    scale: f64,
    annotations: AnnotationMode,
) -> RenderRequest {
    let crop = page.bounds.crop;
    let (width, height) = match page.bounds.rotate {
        90 | 270 => (
            (crop.height() * scale).ceil() as u32,
            (crop.width() * scale).ceil() as u32,
        ),
        _ => (
            (crop.width() * scale).ceil() as u32,
            (crop.height() * scale).ceil() as u32,
        ),
    };
    let device = match page.bounds.rotate {
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
    };
    RenderRequest {
        page,
        transform: PageTransform { matrix: device },
        crop: None,
        output_size: DeviceSize { width, height },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations,
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    }
}

fn write_gray_png(path: &str, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing PNG header")?;
    writer
        .write_image_data(pixels)
        .context("writing PNG pixels")?;
    Ok(())
}

/// Minimal PPM writer (P6, RGB) — placeholder output format until an image
/// encoder is chosen.
fn write_ppm(path: &str, page: &pdf_render_api::HostPage) -> Result<()> {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(
        std::fs::File::create(path).with_context(|| format!("creating {path}"))?,
    );
    write!(f, "P6\n{} {}\n255\n", page.width, page.height)?;
    let bpp = page.format.bytes_per_pixel();
    for y in 0..page.height as usize {
        let row = &page.pixels[y * page.stride..y * page.stride + page.width as usize * bpp];
        match page.format {
            OutputFormat::Rgba8PremultipliedSrgb => {
                for px in row.chunks_exact(4) {
                    f.write_all(&px[..3])?;
                }
            }
            OutputFormat::Gray8 => {
                for &g in row {
                    f.write_all(&[g, g, g])?;
                }
            }
        }
    }
    Ok(())
}
