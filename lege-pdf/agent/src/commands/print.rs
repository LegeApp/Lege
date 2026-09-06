//! `lege-pdf print` — office printing over `lege-pdf-print`.
//!
//! Two things happen here and nothing else: CLI strings become a
//! [`PrintOptions`], and the resulting plan or submission becomes a JSON
//! envelope. Every decision that matters — pass-through versus composition,
//! imposition, spooling — belongs to the print crate.
//!
//! `--dry-run` is the mode an agent should reach for: it reports the route,
//! the sheet count, and every placement without contacting a spooler, so it
//! is safe to run against a machine with a real printer attached.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use lege_pdf_print::job::{
    PrintRequest, RouteKind, compose_options_for, print_document_with, route_for, source_pages,
};
use lege_pdf_print::spool::{DeviceCapabilities, PrinterId, Spooler, file::FileSpooler};
use lege_pdf_print::{
    ComposeOptions, Duplex, Margins, NUp, NUpOrder, Orientation, PageBoxKind, PageRange, PaperSize,
    PrintJob, PrintOptions, Scaling, plan_sheets,
};

use crate::bounds::Bounds;
use crate::schema::{Envelope, OutputMode};
use crate::views::print::{
    ComposeView, DeviceView, MarginsView, PaperView, PlacementView, PrintOptionsView,
    PrintPlanData, PrinterView, PrintersData, RasterSizeView, RectView, SheetView,
    SubmittedJobData, TransformView,
};

/// Imposition emits one copy; both platform spoolers take a native copy
/// count, so nothing here multiplies the run.
const COPIES_APPLIED_BY: &str = "spooler";

/// The spooler backend compiled in for this platform.
const PLATFORM_BACKEND: &str = if cfg!(any(target_os = "linux", target_os = "macos")) {
    "cups"
} else if cfg!(windows) {
    "windows"
} else {
    "file"
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OrientationArg {
    Portrait,
    Landscape,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DuplexArg {
    None,
    /// Flip about the long edge — the usual "book" binding.
    Long,
    /// Flip about the short edge — "notepad" binding.
    Short,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum NUpOrderArg {
    RightDown,
    LeftDown,
    DownRight,
    DownLeft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SourceBoxArg {
    Crop,
    Media,
    Trim,
    Bleed,
    Art,
}

#[derive(Debug)]
pub struct PrintArgs<'a> {
    pub path: Option<&'a Path>,
    pub password: Option<&'a str>,
    pub printer: Option<&'a str>,
    pub list_printers: bool,
    pub pages: Option<&'a str>,
    pub paper: Option<&'a str>,
    pub orientation: OrientationArg,
    /// User margin in points. Mutually exclusive with the mm/in forms.
    pub margin: Option<f64>,
    pub margin_mm: Option<f64>,
    pub margin_in: Option<f64>,
    /// Per-edge overrides in points, applied over whichever uniform margin
    /// was given. A binding margin is asymmetric by definition, and duplex
    /// mirrors it onto the back side, so the uniform form alone cannot ask
    /// for the one margin duplex exists to serve.
    pub margin_left: Option<f64>,
    pub margin_right: Option<f64>,
    pub margin_top: Option<f64>,
    pub margin_bottom: Option<f64>,
    pub scaling: Option<&'a str>,
    pub n_up: Option<&'a str>,
    pub n_up_order: NUpOrderArg,
    pub n_up_border: bool,
    pub duplex: DuplexArg,
    pub copies: u16,
    pub no_collate: bool,
    pub reverse: bool,
    pub source_box: SourceBoxArg,
    pub gray: bool,
    /// Composition resolution. Ignored on the pass-through route.
    pub dpi: Option<f64>,
    /// Spool with the `file` backend into this directory; no real printer is
    /// touched.
    pub to_file: Option<&'a Path>,
    pub dry_run: bool,
    /// In a dry run, ask the real queue for its capabilities instead of
    /// assuming a conservative device. Off by default so that `--dry-run`
    /// alone is guaranteed not to contact a spooler.
    pub query_device: bool,
    pub bounds: Bounds,
    pub output: OutputMode,
}

pub fn run(args: PrintArgs<'_>) -> Result<i32> {
    if args.list_printers {
        return list_printers(&args);
    }

    let path = args
        .path
        .context("a PDF path is required unless --list-printers is given")?;
    let document = path.display().to_string();

    let bytes: Arc<[u8]> = Arc::from(
        std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?
            .into_boxed_slice(),
    );
    let pages = source_pages(Arc::clone(&bytes), args.password)
        .with_context(|| format!("opening {}", path.display()))?;
    let page_count = u32::try_from(pages.len()).unwrap_or(u32::MAX);
    if page_count == 0 {
        bail!("{} has no pages", path.display());
    }

    let options = build_options(&args, page_count)?;
    options.validate()?;

    let mut warnings = Vec::new();
    let selected = selected_pages(&options, page_count, args.bounds, &mut warnings);
    if selected.is_empty() {
        bail!("page range selects no pages of {page_count}");
    }

    if args.dry_run {
        plan(
            &args, &document, pages, page_count, options, selected, warnings,
        )
    } else {
        submit(
            &args, &document, bytes, page_count, options, selected, warnings,
        )
    }
}

// ---------------------------------------------------------------- printers

fn list_printers(args: &PrintArgs<'_>) -> Result<i32> {
    let (spooler, backend) = make_spooler(args.to_file)?;
    let printers = spooler.printers().context("enumerating print queues")?;
    let default = spooler
        .default_printer()
        .context("resolving the default print queue")?
        .map(|id| id.as_str().to_owned());

    let data = PrintersData {
        backend,
        printers: printers
            .iter()
            .map(|p| PrinterView {
                id: p.id.as_str().to_owned(),
                description: p.description.clone(),
                location: p.location.clone(),
                is_default: p.is_default,
                accepting_jobs: p.accepting_jobs,
            })
            .collect(),
        default,
    };

    let document = args
        .path
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let env = Envelope::ok(document, serde_json::to_value(&data)?);

    match args.output {
        OutputMode::Human => {
            if data.printers.is_empty() {
                println!("no printers ({backend})");
            }
            for p in &data.printers {
                println!(
                    "{}{}{}",
                    p.id,
                    if p.is_default { " (default)" } else { "" },
                    if p.accepting_jobs {
                        ""
                    } else {
                        " [not accepting jobs]"
                    }
                );
            }
            Ok(0)
        }
        OutputMode::Json => {
            env.write_json()?;
            Ok(0)
        }
        OutputMode::Jsonl => {
            env.write_jsonl()?;
            Ok(0)
        }
    }
}

// -------------------------------------------------------------------- plan

fn plan(
    args: &PrintArgs<'_>,
    document: &str,
    pages: Vec<lege_pdf_print::SourcePage>,
    page_count: u32,
    options: PrintOptions,
    selected: Vec<u32>,
    mut warnings: Vec<String>,
) -> Result<i32> {
    let (capabilities, source) = dry_run_capabilities(args, &mut warnings)?;
    let route = route_for(&options, &capabilities);
    let compose = compose_options(args, &options, &capabilities)?;

    let mut paper_sheets = None;
    let sheets = match route {
        RouteKind::PassThrough => {
            warnings.push(
                "pass-through: the printer's own filter chain places the pages, so no imposition \
                 plan exists"
                    .into(),
            );
            Vec::new()
        }
        RouteKind::Composed => {
            let job = PrintJob::new(pages.clone(), options.clone());
            let imposed = plan_sheets(&job, &capabilities)?;
            paper_sheets = Some(count_paper_sheets(&imposed));
            imposed
                .iter()
                .map(|sheet| sheet_view(sheet, &pages, &compose))
                .collect()
        }
    };
    if options.copies > 1 && args.to_file.is_some() {
        warnings
            .push("copies are the spooler's to apply; the file backend records one copy".into());
    }

    let sides = match route {
        RouteKind::PassThrough => None,
        RouteKind::Composed => Some(u32::try_from(sheets.len()).unwrap_or(u32::MAX)),
    };

    let data = PrintPlanData {
        unit: "points",
        route: route.as_str(),
        dry_run: true,
        page_count,
        selected_pages: selected,
        options: options_view(&options),
        device: device_view(args.printer, source, &capabilities),
        compose: compose_view(&compose),
        sheet_count: sides,
        paper_sheets,
        total_sides: sides.map(|n| n.saturating_mul(u32::from(options.copies.max(1)))),
        copies_applied_by: COPIES_APPLIED_BY,
        sheets,
    };

    let env = Envelope::ok(document, serde_json::to_value(&data)?).with_warnings(warnings);
    match args.output {
        OutputMode::Human => {
            println!(
                "dry run: route={} pages={} sheets={}",
                data.route,
                data.selected_pages.len(),
                data.sheet_count
                    .map_or_else(|| "n/a".to_owned(), |n| n.to_string())
            );
            for w in &env.warnings {
                eprintln!("warning: {w}");
            }
            Ok(0)
        }
        OutputMode::Json => {
            env.write_json()?;
            Ok(0)
        }
        OutputMode::Jsonl => {
            env.write_jsonl()?;
            Ok(0)
        }
    }
}

// ------------------------------------------------------------------ submit

fn submit(
    args: &PrintArgs<'_>,
    document: &str,
    bytes: Arc<[u8]>,
    page_count: u32,
    options: PrintOptions,
    selected: Vec<u32>,
    warnings: Vec<String>,
) -> Result<i32> {
    let (spooler, backend) = make_spooler(args.to_file)?;
    let printer = resolve_printer(spooler.as_ref(), args.printer)?;

    let capabilities = spooler
        .capabilities(&printer)
        .with_context(|| format!("querying {printer}"))?;
    let compose = compose_options(args, &options, &capabilities)?;

    let title = Path::new(document)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("lege-pdf")
        .to_owned();

    let submitted = print_document_with(
        spooler.as_ref(),
        &PrintRequest {
            printer: &printer,
            title: &title,
            pdf_bytes: bytes,
            password: args.password,
            options: &options,
            compose: Some(compose),
        },
    )?;

    let data = SubmittedJobData {
        unit: "points",
        route: submitted.route.kind().as_str(),
        dry_run: false,
        job_id: submitted.id.to_string(),
        printer: printer.as_str().to_owned(),
        backend,
        sheet_count: submitted.route.sheets(),
        copies_applied_by: COPIES_APPLIED_BY,
        spooled_to: args.to_file.map(|d| d.display().to_string()),
        page_count,
        selected_pages: selected,
        options: options_view(&options),
    };

    let env = Envelope::ok(document, serde_json::to_value(&data)?).with_warnings(warnings);
    match args.output {
        OutputMode::Human => {
            println!(
                "submitted {} to {} (route={})",
                data.job_id, data.printer, data.route
            );
            for w in &env.warnings {
                eprintln!("warning: {w}");
            }
            Ok(0)
        }
        OutputMode::Json => {
            env.write_json()?;
            Ok(0)
        }
        OutputMode::Jsonl => {
            env.write_jsonl()?;
            Ok(0)
        }
    }
}

// ------------------------------------------------------------------ pieces

fn make_spooler(to_file: Option<&Path>) -> Result<(Box<dyn Spooler>, &'static str)> {
    match to_file {
        Some(dir) => {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            Ok((Box::new(FileSpooler::new(dir)), "file"))
        }
        None => Ok((lege_pdf_print::spool::platform_spooler(), PLATFORM_BACKEND)),
    }
}

fn resolve_printer(spooler: &dyn Spooler, requested: Option<&str>) -> Result<PrinterId> {
    if let Some(name) = requested {
        return Ok(PrinterId::new(name));
    }
    spooler
        .default_printer()
        .context("resolving the default print queue")?
        .context("no default printer; pass --printer, or --to-file for the file backend")
}

/// The device a `--dry-run` plans against.
///
/// A dry run must be safe to invoke on a machine with a real printer, so by
/// default it contacts nothing and assumes a conservative device: hardware
/// margins at the crate's quarter-inch fallback, and PDF pass-through exactly
/// where the platform actually has it (CUPS yes, winspool no). `--query-device`
/// opts into asking the real queue.
fn dry_run_capabilities(
    args: &PrintArgs<'_>,
    warnings: &mut Vec<String>,
) -> Result<(DeviceCapabilities, &'static str)> {
    if !args.query_device {
        warnings
            .push("device capabilities assumed; pass --query-device to ask the real queue".into());
        return Ok((
            DeviceCapabilities {
                accepts_pdf: cfg!(any(target_os = "linux", target_os = "macos")),
                ..DeviceCapabilities::default()
            },
            "assumed",
        ));
    }
    let (spooler, _) = make_spooler(args.to_file)?;
    let printer = resolve_printer(spooler.as_ref(), args.printer)?;
    let capabilities = spooler
        .capabilities(&printer)
        .with_context(|| format!("querying {printer}"))?;
    Ok((capabilities, "queried"))
}

fn compose_options(
    args: &PrintArgs<'_>,
    options: &PrintOptions,
    capabilities: &DeviceCapabilities,
) -> Result<ComposeOptions> {
    let mut compose = compose_options_for(options, capabilities);
    if let Some(dpi) = args.dpi {
        if !dpi.is_finite() || dpi <= 0.0 {
            bail!("--dpi must be a positive number");
        }
        compose.dpi = dpi;
    }
    Ok(compose)
}

/// One-based pages the range selects, capped by `--max-items`.
fn selected_pages(
    options: &PrintOptions,
    page_count: u32,
    bounds: Bounds,
    warnings: &mut Vec<String>,
) -> Vec<u32> {
    let mut selected: Vec<u32> = options
        .range
        .resolve(page_count)
        .into_iter()
        .map(|i| i + 1)
        .collect();
    let cap = bounds.max_items as usize;
    if cap > 0 && selected.len() > cap {
        warnings.push(format!(
            "selected_pages truncated from {} to max-items={cap}",
            selected.len()
        ));
        selected.truncate(cap);
    }
    selected
}

fn build_options(args: &PrintArgs<'_>, page_count: u32) -> Result<PrintOptions> {
    let paper = match args.paper {
        Some(text) => PaperSize::parse(text)?,
        None => PaperSize::A4,
    };
    let range = match args.pages {
        Some(text) => PageRange::parse(text, page_count)?,
        None => PageRange::All,
    };
    Ok(PrintOptions {
        paper,
        orientation: match args.orientation {
            OrientationArg::Portrait => Orientation::Portrait,
            OrientationArg::Landscape => Orientation::Landscape,
            OrientationArg::Auto => Orientation::Auto,
        },
        margins: parse_margins(args)?,
        scaling: match args.scaling {
            Some(text) => parse_scaling(text)?,
            None => Scaling::ShrinkToFit,
        },
        n_up: match args.n_up {
            Some(text) => parse_n_up(text)?,
            None => NUp::One,
        },
        n_up_order: match args.n_up_order {
            NUpOrderArg::RightDown => NUpOrder::RightThenDown,
            NUpOrderArg::LeftDown => NUpOrder::LeftThenDown,
            NUpOrderArg::DownRight => NUpOrder::DownThenRight,
            NUpOrderArg::DownLeft => NUpOrder::DownThenLeft,
        },
        n_up_border: args.n_up_border,
        duplex: match args.duplex {
            DuplexArg::None => Duplex::None,
            DuplexArg::Long => Duplex::LongEdge,
            DuplexArg::Short => Duplex::ShortEdge,
        },
        range,
        copies: args.copies,
        collate: !args.no_collate,
        reverse: args.reverse,
        source_box: match args.source_box {
            SourceBoxArg::Crop => PageBoxKind::Crop,
            SourceBoxArg::Media => PageBoxKind::Media,
            SourceBoxArg::Trim => PageBoxKind::Trim,
            SourceBoxArg::Bleed => PageBoxKind::Bleed,
            SourceBoxArg::Art => PageBoxKind::Art,
        },
        grayscale: args.gray,
    })
}

fn parse_margins(args: &PrintArgs<'_>) -> Result<Margins> {
    let given = [args.margin, args.margin_mm, args.margin_in]
        .iter()
        .filter(|v| v.is_some())
        .count();
    if given > 1 {
        bail!("specify only one of --margin, --margin-mm, --margin-in");
    }
    let mut margins = if let Some(pt) = args.margin {
        Margins::uniform(pt)
    } else if let Some(mm) = args.margin_mm {
        Margins::millimetres(mm)
    } else if let Some(inches) = args.margin_in {
        Margins::inches(inches)
    } else {
        Margins::ZERO
    };
    // Per-edge overrides come last, so `--margin 36 --margin-left 72` reads
    // as "36 all round, but 72 on the binding edge".
    if let Some(left) = args.margin_left {
        margins.left = left;
    }
    if let Some(right) = args.margin_right {
        margins.right = right;
    }
    if let Some(top) = args.margin_top {
        margins.top = top;
    }
    if let Some(bottom) = args.margin_bottom {
        margins.bottom = bottom;
    }
    margins.validate()?;
    Ok(margins)
}

/// `actual` / `fit` / `shrink` / `fill` / `NN%`.
///
/// The `%` is required on an explicit factor. `Scaling::Percent` is a
/// multiplier — `1.0` is 1:1 — so a bare `50` would silently mean 5000×;
/// demanding the sign removes the ambiguity rather than guessing at it.
fn parse_scaling(text: &str) -> Result<Scaling> {
    let lower = text.trim().to_ascii_lowercase();
    Ok(match lower.as_str() {
        "actual" | "actual-size" | "none" | "100%" => Scaling::ActualSize,
        "fit" | "fit-to-page" => Scaling::FitToPage,
        "shrink" | "shrink-to-fit" => Scaling::ShrinkToFit,
        "fill" | "fill-page" => Scaling::FillPage,
        _ => {
            let percent: f64 = lower
                .strip_suffix('%')
                .and_then(|body| body.trim().parse().ok())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown --scaling {text:?}: expected actual, fit, shrink, fill, or NN%"
                    )
                })?;
            Scaling::Percent(percent / 100.0)
        }
    })
}

fn parse_n_up(text: &str) -> Result<NUp> {
    Ok(match text.trim().to_ascii_lowercase().as_str() {
        "1" => NUp::One,
        "2" => NUp::Two,
        "4" => NUp::Four,
        "6" => NUp::Six,
        "9" => NUp::Nine,
        "16" => NUp::Sixteen,
        "booklet" => NUp::Booklet,
        other => bail!("unknown --n-up {other:?}: expected 1, 2, 4, 6, 9, 16, or booklet"),
    })
}

// -------------------------------------------------------------------- views

fn rect_view(r: lege_pdf_print::Rect) -> RectView {
    RectView {
        x0: r.x0,
        y0: r.y0,
        x1: r.x1,
        y1: r.y1,
    }
}

fn margins_view(m: Margins) -> MarginsView {
    MarginsView {
        left: m.left,
        right: m.right,
        top: m.top,
        bottom: m.bottom,
    }
}

/// Physical sheets of paper: a duplex run pairs each front with the back that
/// follows it, so counting fronts counts paper.
fn count_paper_sheets(sheets: &[lege_pdf_print::Sheet]) -> u32 {
    let fronts = sheets
        .iter()
        .filter(|s| s.side == lege_pdf_print::Side::Front)
        .count();
    u32::try_from(fronts).unwrap_or(u32::MAX)
}

fn sheet_view(
    sheet: &lege_pdf_print::Sheet,
    pages: &[lege_pdf_print::SourcePage],
    compose: &ComposeOptions,
) -> SheetView {
    SheetView {
        index: sheet.index,
        side: match sheet.side {
            lege_pdf_print::Side::Front => "front",
            lege_pdf_print::Side::Back => "back",
        },
        bounds: rect_view(sheet.bounds),
        imageable: rect_view(sheet.imageable),
        landscape: sheet.is_landscape(),
        raster: raster_size_view(sheet, compose),
        placements: sheet
            .placements
            .iter()
            .map(|placement| placement_view(placement, pages))
            .collect(),
    }
}

/// The bitmap this side would compose to, asked for without composing it.
/// A sheet over the pixel budget reports nothing rather than a size that
/// composition would refuse.
fn raster_size_view(
    sheet: &lege_pdf_print::Sheet,
    compose: &ComposeOptions,
) -> Option<RasterSizeView> {
    let (width, height) = lege_pdf_print::compose::sheet_pixel_size(sheet, compose).ok()?;
    let channels: u8 = if compose.grayscale { 1 } else { 3 };
    Some(RasterSizeView {
        width,
        height,
        channels,
        bytes: u64::from(width) * u64::from(height) * u64::from(channels),
    })
}

fn placement_view(
    placement: &lege_pdf_print::Placement,
    pages: &[lege_pdf_print::SourcePage],
) -> PlacementView {
    let t = placement.transform;
    // A placement always names a page of the job it came from; the fallback
    // only keeps a corrupt plan from panicking the reporter.
    let page = pages
        .iter()
        .copied()
        .find(|p| p.index == placement.source_page)
        .unwrap_or(lege_pdf_print::SourcePage::new(
            placement.source_page,
            0.0,
            0.0,
        ));
    PlacementView {
        source_page: placement.source_page.saturating_add(1),
        source_page_index: placement.source_page,
        scale_x: t.a.hypot(t.b),
        scale_y: t.c.hypot(t.d),
        rotation_degrees: t.b.atan2(t.a).to_degrees(),
        translate: [t.e, t.f],
        transform: TransformView {
            a: t.a,
            b: t.b,
            c: t.c,
            d: t.d,
            e: t.e,
            f: t.f,
        },
        cell: rect_view(placement.clip),
        content: rect_view(placement.transformed_bounds(page)),
        painted: rect_view(placement.painted_bounds(page)),
    }
}

fn device_view(
    printer: Option<&str>,
    source: &'static str,
    capabilities: &DeviceCapabilities,
) -> DeviceView {
    DeviceView {
        printer: printer.map(ToOwned::to_owned),
        source,
        accepts_pdf: capabilities.accepts_pdf,
        supports_duplex: capabilities.supports_duplex,
        supports_color: capabilities.supports_color,
        resolution_dpi: capabilities.resolution_dpi,
        hardware_margins: margins_view(capabilities.hardware_margins),
    }
}

fn compose_view(compose: &ComposeOptions) -> ComposeView {
    ComposeView {
        dpi: compose.dpi,
        grayscale: compose.grayscale,
        band_rows: compose.band_rows,
        max_pixels: compose.max_pixels,
    }
}

fn options_view(options: &PrintOptions) -> PrintOptionsView {
    let (width, height) = options.paper.size();
    PrintOptionsView {
        paper: PaperView {
            name: paper_name(options.paper),
            width_pt: width,
            height_pt: height,
            ipp_name: options.paper.ipp_name(),
        },
        orientation: match options.orientation {
            Orientation::Portrait => "portrait",
            Orientation::Landscape => "landscape",
            Orientation::Auto => "auto",
        },
        margins: margins_view(options.margins),
        scaling: match options.scaling {
            Scaling::ActualSize => "actual".to_owned(),
            Scaling::FitToPage => "fit".to_owned(),
            Scaling::ShrinkToFit => "shrink".to_owned(),
            Scaling::FillPage => "fill".to_owned(),
            Scaling::Percent(p) => format!("{}%", p * 100.0),
        },
        n_up: match options.n_up {
            NUp::One => "1",
            NUp::Two => "2",
            NUp::Four => "4",
            NUp::Six => "6",
            NUp::Nine => "9",
            NUp::Sixteen => "16",
            NUp::Booklet => "booklet",
        },
        n_up_order: match options.n_up_order {
            NUpOrder::RightThenDown => "right-down",
            NUpOrder::LeftThenDown => "left-down",
            NUpOrder::DownThenRight => "down-right",
            NUpOrder::DownThenLeft => "down-left",
        },
        n_up_border: options.n_up_border,
        duplex: match options.duplex {
            Duplex::None => "none",
            Duplex::LongEdge => "long",
            Duplex::ShortEdge => "short",
        },
        copies: options.copies,
        collate: options.collate,
        reverse: options.reverse,
        source_box: match options.source_box {
            PageBoxKind::Crop => "crop",
            PageBoxKind::Media => "media",
            PageBoxKind::Trim => "trim",
            PageBoxKind::Bleed => "bleed",
            PageBoxKind::Art => "art",
        },
        grayscale: options.grayscale,
    }
}

fn paper_name(paper: PaperSize) -> &'static str {
    match paper {
        PaperSize::A3 => "a3",
        PaperSize::A4 => "a4",
        PaperSize::A5 => "a5",
        PaperSize::A6 => "a6",
        PaperSize::B5 => "b5",
        PaperSize::Letter => "letter",
        PaperSize::Legal => "legal",
        PaperSize::Tabloid => "tabloid",
        PaperSize::Executive => "executive",
        _ => "custom",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn scaling_accepts_names_and_percentages() {
        assert_eq!(parse_scaling("shrink").unwrap(), Scaling::ShrinkToFit);
        assert_eq!(parse_scaling("Fit").unwrap(), Scaling::FitToPage);
        assert_eq!(parse_scaling("fill-page").unwrap(), Scaling::FillPage);
        assert_eq!(parse_scaling("100%").unwrap(), Scaling::ActualSize);
        assert_eq!(parse_scaling("50%").unwrap(), Scaling::Percent(0.5));
        assert_eq!(parse_scaling("12.5%").unwrap(), Scaling::Percent(0.125));
        assert!(parse_scaling("bigger").is_err());
        // A bare number would be ambiguous between 50% and 50x.
        assert!(parse_scaling("50").is_err());
    }

    #[test]
    fn n_up_accepts_counts_and_booklet() {
        assert_eq!(parse_n_up("4").unwrap(), NUp::Four);
        assert_eq!(parse_n_up("Booklet").unwrap(), NUp::Booklet);
        assert!(parse_n_up("3").is_err());
    }
}
