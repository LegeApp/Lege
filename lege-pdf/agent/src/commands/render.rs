use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use pdf_document::{DocumentSnapshot, PageIndex};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::{CpuBackend, CpuBackendOptions};

use crate::bounds::Bounds;
use crate::commands::{
    default_annotations, emit_failed, page_compiler, parse_context, resolve_snapshot,
};
use crate::open::DocumentIdentity;
use crate::pages::{PageZero, parse_bbox, parse_page_range};
use crate::schema::{Envelope, OutputMode};
use crate::views::render::RenderPageData;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ImageFormat {
    Png,
    Ppm,
}

#[derive(Debug)]
pub struct RenderArgs<'a> {
    pub path: &'a Path,
    pub password: Option<&'a str>,
    pub pages: Option<&'a str>,
    pub output: &'a str,
    pub dpi: Option<f64>,
    pub scale: Option<f64>,
    pub format: ImageFormat,
    pub crop: Option<&'a str>,
    pub thumbnail: bool,
    pub system_fonts: bool,
    pub bounds: Bounds,
    pub fail_fast: bool,
    pub output_mode: OutputMode,
    pub snapshot: Option<Arc<DocumentSnapshot>>,
    pub identity: Option<DocumentIdentity>,
}

pub fn run(args: RenderArgs<'_>) -> Result<i32> {
    let (identity, snapshot) =
        resolve_snapshot(args.path, args.password, args.snapshot, args.identity)?;
    let document = identity.display_path();
    let mut global_warnings = Vec::new();
    let (page_indices, range_warnings) =
        parse_page_range(args.pages, snapshot.page_count(), args.bounds.max_pages)?;
    global_warnings.extend(range_warnings);

    if page_indices.is_empty() {
        bail!("no pages selected");
    }

    let crop = args.crop.map(parse_bbox).transpose()?;
    let (dpi, scale) = resolve_scale(args.dpi, args.scale, args.thumbnail)?;
    let annotations = if default_annotations() {
        AnnotationMode::StaticAppearances
    } else {
        AnnotationMode::None
    };
    let compiler = page_compiler(args.system_fonts, default_annotations());
    let backend = CpuBackend::new(CpuBackendOptions::default());
    let stem = identity
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page");

    let mut exit = 0i32;
    let multi = page_indices.len() > 1 || matches!(args.output_mode, OutputMode::Jsonl);

    for pz in &page_indices {
        let mut warnings = global_warnings.clone();
        match render_one(
            &snapshot,
            &compiler,
            &backend,
            *pz,
            args.output,
            stem,
            scale,
            dpi,
            args.format,
            crop,
            annotations,
            args.thumbnail,
            &mut warnings,
        ) {
            Ok(data) => {
                let env =
                    Envelope::page_ok(&document, pz.one_based(), serde_json::to_value(&data)?)
                        .with_warnings(warnings);
                emit(&env, &data, args.output_mode, multi)?;
            }
            Err(err) => {
                let env = Envelope::page_failed(&document, pz.one_based(), err.to_string());
                emit_failed(&env, args.output_mode, multi)?;
                if args.fail_fast {
                    return Ok(1);
                }
                exit = 1;
            }
        }
    }
    Ok(exit)
}

pub fn render_value(
    snapshot: &DocumentSnapshot,
    page_one: u32,
    output_template: &str,
    stem: &str,
    dpi: Option<f64>,
    scale: Option<f64>,
    format: ImageFormat,
    crop: Option<[f64; 4]>,
    thumbnail: bool,
    system_fonts: bool,
) -> Result<serde_json::Value> {
    let pz = crate::pages::to_zero_based(page_one, snapshot.page_count())?;
    let (dpi, scale) = resolve_scale(dpi, scale, thumbnail)?;
    let annotations = AnnotationMode::default();
    let compiler = page_compiler(system_fonts, true);
    let backend = CpuBackend::new(CpuBackendOptions::default());
    let mut warnings = Vec::new();
    let data = render_one(
        snapshot,
        &compiler,
        &backend,
        pz,
        output_template,
        stem,
        scale,
        dpi,
        format,
        crop,
        annotations,
        thumbnail,
        &mut warnings,
    )?;
    Ok(serde_json::to_value(data)?)
}

fn resolve_scale(dpi: Option<f64>, scale: Option<f64>, thumbnail: bool) -> Result<(f64, f64)> {
    if dpi.is_some() && scale.is_some() {
        bail!("specify only one of --dpi or --scale");
    }
    let dpi = if thumbnail {
        dpi.unwrap_or(72.0)
    } else {
        dpi.unwrap_or(150.0)
    };
    if !dpi.is_finite() || dpi <= 0.0 {
        bail!("dpi must be a positive number");
    }
    let scale = scale.unwrap_or(dpi / 72.0);
    if !scale.is_finite() || scale <= 0.0 {
        bail!("scale must be a positive number");
    }
    Ok((dpi, scale))
}

fn expand_template(template: &str, page_one: u32, page_index: u32, stem: &str) -> String {
    template
        .replace("{page}", &page_one.to_string())
        .replace("{page_index}", &page_index.to_string())
        .replace("{stem}", stem)
}

#[allow(clippy::too_many_arguments)]
fn render_one(
    snapshot: &DocumentSnapshot,
    compiler: &pdf_content::PageCompiler,
    backend: &CpuBackend,
    pz: PageZero,
    output_template: &str,
    stem: &str,
    mut scale: f64,
    mut dpi: f64,
    format: ImageFormat,
    crop: Option<[f64; 4]>,
    annotations: AnnotationMode,
    thumbnail: bool,
    warnings: &mut Vec<String>,
) -> Result<RenderPageData> {
    let mut ctx = parse_context();
    let page = Arc::new(compiler.compile(snapshot, PageIndex(pz.0), &mut ctx)?);

    let crop_rect = page.bounds.crop;
    let (mut width, mut height) = match page.bounds.rotate {
        90 | 270 => (
            (crop_rect.height() * scale).ceil() as u32,
            (crop_rect.width() * scale).ceil() as u32,
        ),
        _ => (
            (crop_rect.width() * scale).ceil() as u32,
            (crop_rect.height() * scale).ceil() as u32,
        ),
    };

    if thumbnail {
        let max_edge = 512u32;
        let edge = width.max(height).max(1);
        if edge > max_edge {
            let factor = max_edge as f64 / edge as f64;
            scale *= factor;
            dpi *= factor;
            width = ((width as f64) * factor).ceil().max(1.0) as u32;
            height = ((height as f64) * factor).ceil().max(1.0) as u32;
            warnings.push(format!(
                "thumbnail clamped to max edge {max_edge} (scale now {scale:.4})"
            ));
        }
    }

    let device = display_matrix(page.bounds.rotate, crop_rect, scale);
    let request_crop = crop.map(|c| pdf_page_ir::DeviceRect {
        // Render API crop is in device pixels; map PDF crop through scale approx.
        x: ((c[0].min(c[2]) - crop_rect.x0) * scale).floor().max(0.0) as i32,
        y: ((crop_rect.y1 - c[1].max(c[3])) * scale).floor().max(0.0) as i32,
        width: ((c[0] - c[2]).abs() * scale).ceil().max(1.0) as u32,
        height: ((c[1] - c[3]).abs() * scale).ceil().max(1.0) as u32,
    });

    let request = RenderRequest {
        page,
        transform: PageTransform { matrix: device },
        crop: request_crop,
        output_size: DeviceSize { width, height },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations,
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: if thumbnail {
            RenderQuality::Draft
        } else {
            RenderQuality::Normal
        },
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };

    let (host, stats) = backend.render_to_host(&request)?;
    if stats.degraded_draws > 0 {
        warnings.push(format!("{} image draw(s) degraded", stats.degraded_draws));
        for note in &stats.recovery_notes {
            warnings.push(note.clone());
        }
    }

    let out_path = PathBuf::from(expand_template(output_template, pz.one_based(), pz.0, stem));
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }

    match format {
        ImageFormat::Png => write_png(&out_path, &host)?,
        ImageFormat::Ppm => write_ppm(&out_path, &host)?,
    }

    Ok(RenderPageData {
        unit: "device_pixels",
        path: out_path.display().to_string(),
        width: host.width,
        height: host.height,
        dpi,
        scale,
        format: match format {
            ImageFormat::Png => "png".into(),
            ImageFormat::Ppm => "ppm".into(),
        },
        degraded_draws: stats.degraded_draws,
        recovery_notes: stats.recovery_notes.clone(),
    })
}

pub(crate) fn display_matrix(rotate: u16, crop: pdf_page_ir::Rect, s: f64) -> Matrix {
    match rotate {
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
    }
}

/// Render a page into a tightly packed grayscale buffer for the in-process OCR
/// backend. The returned matrix maps PDF points to top-left-origin pixels.
///
/// Takes the `CpuBackend` by reference so callers doing this once per page
/// (e.g. `lege-pdf text --ocr always --pages all`) can share one backend —
/// and its font/glyph/image/soft-mask/prepared-page caches — across the
/// whole run instead of paying `CpuBackend::new` on every page.
pub(crate) fn render_gray_for_ocr(
    snapshot: &DocumentSnapshot,
    compiler: &pdf_content::PageCompiler,
    pz: PageZero,
    dpi: f64,
    max_pixels: u64,
    backend: &CpuBackend,
) -> Result<(image::GrayImage, Matrix)> {
    let mut ctx = parse_context();
    let page = Arc::new(compiler.compile(snapshot, PageIndex(pz.0), &mut ctx)?);
    let crop = page.bounds.crop;
    let mut scale = dpi / 72.0;
    let dimensions = |scale: f64| match page.bounds.rotate {
        90 | 270 => (
            (crop.height() * scale).ceil().max(1.0) as u32,
            (crop.width() * scale).ceil().max(1.0) as u32,
        ),
        _ => (
            (crop.width() * scale).ceil().max(1.0) as u32,
            (crop.height() * scale).ceil().max(1.0) as u32,
        ),
    };
    let (mut width, mut height) = dimensions(scale);
    let pixels = u64::from(width) * u64::from(height);
    if max_pixels > 0 && pixels > max_pixels {
        scale *= (max_pixels as f64 / pixels as f64).sqrt();
        (width, height) = dimensions(scale);
    }
    let device = display_matrix(page.bounds.rotate, crop, scale);
    let request = RenderRequest {
        page,
        transform: PageTransform { matrix: device },
        crop: None,
        output_size: DeviceSize { width, height },
        output_format: OutputFormat::Gray8,
        background: Background::White,
        annotations: AnnotationMode::StaticAppearances,
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };
    let (host, _stats) = backend.render_to_host(&request)?;
    let mut pixels = Vec::with_capacity(width as usize * height as usize);
    for row in host.pixels.chunks(host.stride).take(height as usize) {
        pixels.extend_from_slice(&row[..width as usize]);
    }
    let image = image::GrayImage::from_raw(width, height, pixels)
        .context("constructing tightly packed OCR page")?;
    Ok((image, device))
}

fn write_png(path: &Path, page: &HostPage) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(file, page.width, page.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing PNG header")?;
    // HostPage is premultiplied; write as-is (agents rarely composite).
    let mut rgba = Vec::with_capacity(page.width as usize * page.height as usize * 4);
    let bpp = page.format.bytes_per_pixel();
    for y in 0..page.height as usize {
        let row = &page.pixels[y * page.stride..y * page.stride + page.width as usize * bpp];
        match page.format {
            OutputFormat::Rgba8PremultipliedSrgb => rgba.extend_from_slice(row),
            OutputFormat::Gray8 => {
                for &g in row {
                    rgba.extend_from_slice(&[g, g, g, 255]);
                }
            }
        }
    }
    writer
        .write_image_data(&rgba)
        .context("writing PNG pixels")?;
    Ok(())
}

fn write_ppm(path: &Path, page: &HostPage) -> Result<()> {
    let mut f = std::io::BufWriter::new(
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
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

fn emit(env: &Envelope, data: &RenderPageData, mode: OutputMode, multi: bool) -> Result<()> {
    match mode {
        OutputMode::Human => {
            println!(
                "wrote {} ({}x{} dpi={:.1})",
                data.path, data.width, data.height, data.dpi
            );
            for w in &env.warnings {
                eprintln!("warning: {w}");
            }
            Ok(())
        }
        OutputMode::Json if !multi => env.write_json(),
        OutputMode::Json | OutputMode::Jsonl => env.write_jsonl(),
    }
}
