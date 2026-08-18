use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use pdf_content::semantic::{SemImage, SemanticOp, SemanticPage};
use pdf_document::{DocumentSnapshot, PageIndex};
use pdf_page_ir::{ImageCodecKind, ImageColorSpace, ImageMask, Matrix, PaintOrigin, Point, Rect};

use crate::bounds::Bounds;
use crate::commands::{
    default_annotations, emit_failed, page_compiler, parse_context, resolve_snapshot,
};
use crate::open::DocumentIdentity;
use crate::pages::{PageZero, parse_page_range};
use crate::schema::{Envelope, OutputMode};
use crate::views::image::{ImageDrawView, ImagesPageData, ObjectRefView};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum ImageMode {
    #[default]
    Inventory,
    Source,
    Decoded,
    Rendered,
}

#[derive(Debug)]
pub struct ImagesArgs<'a> {
    pub path: &'a Path,
    pub password: Option<&'a str>,
    pub pages: Option<&'a str>,
    pub mode: ImageMode,
    pub extract_dir: Option<&'a Path>,
    pub system_fonts: bool,
    pub bounds: Bounds,
    pub fail_fast: bool,
    pub output: OutputMode,
    pub snapshot: Option<Arc<DocumentSnapshot>>,
    pub identity: Option<DocumentIdentity>,
}

pub fn run(args: ImagesArgs<'_>) -> Result<i32> {
    if !matches!(args.mode, ImageMode::Inventory) && args.extract_dir.is_none() {
        bail!("--extract DIR is required for mode {:?}", args.mode);
    }
    if let Some(dir) = args.extract_dir {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let (identity, snapshot) =
        resolve_snapshot(args.path, args.password, args.snapshot, args.identity)?;
    let document = identity.display_path();
    let mut global_warnings = Vec::new();
    let (page_indices, range_warnings) =
        parse_page_range(args.pages, snapshot.page_count(), args.bounds.max_pages)?;
    global_warnings.extend(range_warnings);

    let compiler = page_compiler(args.system_fonts, default_annotations());
    let mut exit = 0i32;
    let multi = page_indices.len() > 1 || matches!(args.output, OutputMode::Jsonl);

    for pz in &page_indices {
        let mut warnings = global_warnings.clone();
        match inventory_page(
            &snapshot,
            &compiler,
            *pz,
            args.mode,
            args.extract_dir,
            &args.bounds,
            &mut warnings,
        ) {
            Ok(data) => {
                let env =
                    Envelope::page_ok(&document, pz.one_based(), serde_json::to_value(&data)?)
                        .with_warnings(warnings);
                emit(&env, &data, args.output, multi)?;
            }
            Err(err) => {
                let env = Envelope::page_failed(&document, pz.one_based(), err.to_string());
                emit_failed(&env, args.output, multi)?;
                if args.fail_fast {
                    return Ok(1);
                }
                exit = 1;
            }
        }
    }
    Ok(exit)
}

pub fn images_value(
    snapshot: &DocumentSnapshot,
    page_one: u32,
    mode: ImageMode,
    extract_dir: Option<&Path>,
    system_fonts: bool,
    bounds: Bounds,
) -> Result<serde_json::Value> {
    let pz = crate::pages::to_zero_based(page_one, snapshot.page_count())?;
    let compiler = page_compiler(system_fonts, default_annotations());
    let mut warnings = Vec::new();
    let data = inventory_page(
        snapshot,
        &compiler,
        pz,
        mode,
        extract_dir,
        &bounds,
        &mut warnings,
    )?;
    Ok(serde_json::to_value(data)?)
}

fn inventory_page(
    snapshot: &DocumentSnapshot,
    compiler: &pdf_content::PageCompiler,
    pz: PageZero,
    mode: ImageMode,
    extract_dir: Option<&Path>,
    bounds: &Bounds,
    warnings: &mut Vec<String>,
) -> Result<ImagesPageData> {
    let mut ctx = parse_context();
    let page = compiler.compile_semantic(snapshot, PageIndex(pz.0), &mut ctx)?;
    let mut draws = collect_draws(&page);
    draws = bounds.truncate_items(draws, warnings, "images");

    // Reuse counts by object number or image id.
    let mut counts: HashMap<u64, usize> = HashMap::new();
    for d in &draws {
        *counts.entry(reuse_key(d)).or_insert(0) += 1;
    }
    for d in &mut draws {
        d.reuse_count = counts.get(&reuse_key(d)).copied().unwrap_or(1);
    }

    if mode != ImageMode::Inventory {
        let dir = extract_dir.context("extract directory required for non-inventory image mode")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        for d in &mut draws {
            match extract_one(&page, d, mode, dir, pz) {
                Ok(path) => d.extract_path = Some(path.display().to_string()),
                Err(err) => warnings.push(format!("extract draw {} failed: {err}", d.draw_index)),
            }
        }
    }

    let unique = counts.len();
    let draw_count = draws.len();
    Ok(ImagesPageData {
        unit: "pdf_points",
        mode: format!("{mode:?}").to_ascii_lowercase(),
        images: draws,
        draw_count,
        unique_object_count: unique,
    })
}

fn reuse_key(d: &ImageDrawView) -> u64 {
    match &d.object {
        Some(o) => ((o.number as u64) << 16) | o.generation as u64,
        None => 1u64 << 63 | d.image_id as u64,
    }
}

fn collect_draws(page: &SemanticPage) -> Vec<ImageDrawView> {
    let mut ctm = Matrix::IDENTITY;
    let mut stack = Vec::new();
    let mut origin_stack = vec![PaintOrigin::PageContent];
    let mut draws = Vec::new();

    for op in page.ops.iter() {
        match op {
            SemanticOp::Save => stack.push(ctm),
            SemanticOp::Restore => {
                if let Some(m) = stack.pop() {
                    ctm = m;
                }
            }
            SemanticOp::Concat(m) => ctm = m.then(ctm),
            SemanticOp::BeginPaintOrigin(origin) => origin_stack.push(*origin),
            SemanticOp::EndPaintOrigin => {
                if origin_stack.len() > 1 {
                    origin_stack.pop();
                }
            }
            SemanticOp::DrawImage(id) => {
                let Some(image) = page.images.get(id.index()) else {
                    continue;
                };
                let origin = origin_stack
                    .last()
                    .copied()
                    .unwrap_or(PaintOrigin::PageContent);
                draws.push(image_draw_view(draws.len(), id.0, image, ctm, origin));
            }
            _ => {}
        }
    }
    draws
}

fn image_draw_view(
    draw_index: usize,
    image_id: u32,
    image: &SemImage,
    ctm: Matrix,
    origin: PaintOrigin,
) -> ImageDrawView {
    let painted = painted_bounds(ctm);
    let (has_color_key, has_stencil_mask) = match &image.mask {
        Some(ImageMask::ColorKey(_)) => (true, false),
        Some(ImageMask::Stencil(_)) => (false, true),
        None => (false, false),
    };
    ImageDrawView {
        draw_index,
        image_id,
        object: image.object.map(|id| ObjectRefView {
            number: id.number,
            generation: id.generation,
        }),
        width: image.width,
        height: image.height,
        bits_per_component: image.bits_per_component,
        is_stencil: image.is_mask,
        color_space: color_space_name(image.color_space.as_ref()),
        codec: image.codec.map(codec_name),
        filters: image
            .filters
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect(),
        transform: [ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f],
        painted_bounds: [painted.x0, painted.y0, painted.x1, painted.y1],
        has_soft_mask: image.smask.is_some(),
        has_color_key_mask: has_color_key,
        has_stencil_mask,
        paint_origin: format!("{origin:?}"),
        lowering_degraded: image.lowering_degraded,
        reuse_count: 1,
        extract_path: None,
    }
}

fn painted_bounds(ctm: Matrix) -> Rect {
    let corners = [
        Point { x: 0.0, y: 0.0 },
        Point { x: 1.0, y: 0.0 },
        Point { x: 0.0, y: 1.0 },
        Point { x: 1.0, y: 1.0 },
    ];
    let mut r = Rect {
        x0: f64::INFINITY,
        y0: f64::INFINITY,
        x1: f64::NEG_INFINITY,
        y1: f64::NEG_INFINITY,
    };
    for p in corners {
        let q = ctm.apply(p);
        r.x0 = r.x0.min(q.x);
        r.y0 = r.y0.min(q.y);
        r.x1 = r.x1.max(q.x);
        r.y1 = r.y1.max(q.y);
    }
    r
}

fn color_space_name(cs: Option<&ImageColorSpace>) -> String {
    match cs {
        None => "none".into(),
        Some(ImageColorSpace::Gray) => "device_gray".into(),
        Some(ImageColorSpace::Rgb) => "device_rgb".into(),
        Some(ImageColorSpace::Cmyk) => "device_cmyk".into(),
        Some(ImageColorSpace::Indexed { .. }) => "indexed".into(),
        Some(ImageColorSpace::TintLut { .. }) => "tint_lut".into(),
        Some(ImageColorSpace::TintLut2 { .. }) => "tint_lut2".into(),
        Some(ImageColorSpace::IccRgb { .. }) => "icc_rgb".into(),
        Some(ImageColorSpace::IccCmyk { .. }) => "icc_cmyk".into(),
        Some(ImageColorSpace::Lab { .. }) => "lab".into(),
    }
}

fn codec_name(c: ImageCodecKind) -> String {
    match c {
        ImageCodecKind::Dct => "dct".into(),
        ImageCodecKind::Jpx => "jpx".into(),
        ImageCodecKind::Jbig2 => "jbig2".into(),
        ImageCodecKind::CcittFax => "ccitt".into(),
    }
}

fn extract_one(
    page: &SemanticPage,
    draw: &ImageDrawView,
    mode: ImageMode,
    dir: &Path,
    pz: PageZero,
) -> Result<PathBuf> {
    let image = page
        .images
        .get(draw.image_id as usize)
        .context("image id out of range")?;
    let base = dir.join(format!(
        "p{}_img{}_draw{}",
        pz.one_based(),
        draw.image_id,
        draw.draw_index
    ));
    match mode {
        ImageMode::Inventory => unreachable!(),
        ImageMode::Source => {
            if let Some(data) = &image.codec_data {
                let ext = match image.codec {
                    Some(ImageCodecKind::Dct) => "jpg",
                    Some(ImageCodecKind::Jpx) => "jp2",
                    Some(ImageCodecKind::Jbig2) => "jb2",
                    Some(ImageCodecKind::CcittFax) => "ccitt",
                    None => "bin",
                };
                let path = base.with_extension(ext);
                std::fs::write(&path, data.as_ref())
                    .with_context(|| format!("writing {}", path.display()))?;
                Ok(path)
            } else if !image.inline_data.is_empty() {
                let path = base.with_extension("inline.bin");
                std::fs::write(&path, &image.inline_data)
                    .with_context(|| format!("writing {}", path.display()))?;
                Ok(path)
            } else {
                bail!("no encoded source available for this image");
            }
        }
        ImageMode::Decoded => {
            let Some(samples) = &image.samples else {
                bail!("no decoded samples available (codec-encoded image)");
            };
            let path = base.with_extension("samples.bin");
            std::fs::write(&path, samples.as_ref())
                .with_context(|| format!("writing {}", path.display()))?;
            // Sidecar meta
            let meta = base.with_extension("samples.json");
            let meta_body = serde_json::json!({
                "width": image.width,
                "height": image.height,
                "bits_per_component": image.bits_per_component,
                "color_space": color_space_name(image.color_space.as_ref()),
            });
            std::fs::write(&meta, serde_json::to_vec_pretty(&meta_body)?)?;
            Ok(path)
        }
        ImageMode::Rendered => {
            // Rendered appearance is a full-page crop of painted bounds via render command.
            // For agent use we write a small PPM of the image samples when RGB8-like, else
            // fall back to samples dump.
            if let Some(samples) = &image.samples {
                if image.bits_per_component == 8
                    && matches!(image.color_space, Some(ImageColorSpace::Rgb))
                    && samples.len() >= (image.width as usize * image.height as usize * 3)
                {
                    let path = base.with_extension("ppm");
                    write_rgb_ppm(&path, image.width, image.height, samples)?;
                    return Ok(path);
                }
            }
            bail!(
                "rendered extract requires decoded RGB8 samples; use mode inventory/source/decoded or render the page"
            );
        }
    }
}

fn write_rgb_ppm(path: &Path, width: u32, height: u32, samples: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(f, "P6\n{width} {height}\n255\n")?;
    let need = width as usize * height as usize * 3;
    f.write_all(&samples[..need.min(samples.len())])?;
    Ok(())
}

fn emit(env: &Envelope, data: &ImagesPageData, mode: OutputMode, multi: bool) -> Result<()> {
    match mode {
        OutputMode::Human => {
            println!(
                "page {}: {} draws, {} unique",
                env.page.unwrap_or(0),
                data.draw_count,
                data.unique_object_count
            );
            for img in &data.images {
                println!(
                    "  draw {} id={} {}x{} codec={:?} origin={} bounds={:?} extract={:?}",
                    img.draw_index,
                    img.image_id,
                    img.width,
                    img.height,
                    img.codec,
                    img.paint_origin,
                    img.painted_bounds,
                    img.extract_path
                );
            }
            for w in &env.warnings {
                eprintln!("warning: {w}");
            }
            Ok(())
        }
        OutputMode::Json if !multi => env.write_json(),
        OutputMode::Json | OutputMode::Jsonl => env.write_jsonl(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_content::semantic::ImageId;
    use pdf_page_ir::PageBounds;

    fn blank_image() -> SemImage {
        SemImage {
            object: None,
            width: 1,
            height: 1,
            bits_per_component: 8,
            is_mask: false,
            filters: Vec::new(),
            inline_data: Vec::new(),
            color_space: None,
            interpolate: false,
            decode: None,
            samples: None,
            codec: None,
            codec_data: None,
            codec_parms: None,
            smask: None,
            mask: None,
            smask_in_data: 0,
            lowering_degraded: false,
        }
    }

    /// Nested `cm` composition must follow PDF semantics: each new matrix is
    /// premultiplied onto the CTM (`new.then(ctm)`), not the other way
    /// around. `Save, cm(translate 100,200), cm(scale 200,100), Do, Restore`
    /// must paint at scale [200,100] positioned at [100,200] — if the
    /// composition order were backwards the translation would itself get
    /// scaled, landing the image at e=20000, f=20000 instead.
    #[test]
    fn nested_concat_composes_in_pdf_cm_order() {
        let ops: Vec<SemanticOp> = vec![
            SemanticOp::Save,
            SemanticOp::Concat(Matrix::translate(100.0, 200.0)),
            SemanticOp::Concat(Matrix::scale(200.0, 100.0)),
            SemanticOp::DrawImage(ImageId(0)),
            SemanticOp::Restore,
        ];

        let page = SemanticPage {
            bounds: PageBounds {
                crop: Rect {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 612.0,
                    y1: 792.0,
                },
                rotate: 0,
            },
            ops: Arc::from(ops),
            paths: Arc::from([]),
            text_runs: Arc::from([]),
            images: Arc::from([blank_image()]),
            fonts: Arc::from([]),
            shadings: Arc::from([]),
            tilings: Arc::from([]),
            uses_icc_color: false,
            uses_overprint: false,
        };

        let draws = collect_draws(&page);
        assert_eq!(draws.len(), 1);
        let t = draws[0].transform;
        assert_eq!(t, [200.0, 0.0, 0.0, 100.0, 100.0, 200.0]);
    }
}
