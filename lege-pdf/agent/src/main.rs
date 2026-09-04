//! `lege-pdf` — agent-facing structured PDF tool over the native Lege engine.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use lege_pdf_agent::bounds::Bounds;
use lege_pdf_agent::commands::{
    content, images, inspect, mcp, print, render, search, serve, text,
};
use lege_pdf_agent::schema::OutputMode;

#[derive(Debug, Parser)]
#[command(
    name = "lege-pdf",
    version,
    about = "Agent-facing structured PDF inspection, text, images, content, render, search, and serve"
)]
struct Cli {
    /// User/owner password for encrypted documents (never printed).
    #[arg(long, global = true)]
    password: Option<String>,

    /// Emit a single pretty-printed JSON envelope on stdout.
    #[arg(long, global = true, conflicts_with = "jsonl")]
    json: bool,

    /// Emit JSONL records on stdout (one per page for multi-page ops).
    #[arg(long, global = true, conflicts_with = "json")]
    jsonl: bool,

    /// Fail the process on the first page-local error.
    #[arg(long, global = true)]
    fail_fast: bool,

    /// Maximum pages processed per invocation (0 = unlimited).
    #[arg(long, global = true, default_value_t = Bounds::default().max_pages)]
    max_pages: u32,

    /// Maximum items (words/ops/images/matches) emitted per page.
    #[arg(long, global = true, default_value_t = Bounds::default().max_items)]
    max_items: u32,

    /// Soft payload size cap in bytes for a single record.
    #[arg(long, global = true, default_value_t = Bounds::default().max_bytes)]
    max_bytes: u64,

    /// Wall-clock timeout hint in seconds (0 = none; enforced by serve).
    #[arg(long, global = true, default_value_t = Bounds::default().timeout_secs)]
    timeout: u64,

    /// Resolve non-embedded fonts against system fonts (non-deterministic).
    #[arg(long, global = true)]
    system_fonts: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Document health, boxes, encryption, features, and page compile status.
    Inspect {
        file: PathBuf,
        /// One-based page range (e.g. 1,3-5,all). Defaults to all (capped).
        #[arg(long)]
        pages: Option<String>,
    },
    /// Extract page text with optional geometry.
    Text {
        file: PathBuf,
        #[arg(long)]
        pages: Option<String>,
        #[arg(long, value_enum, default_value_t = text::TextLayout::Plain)]
        layout: text::TextLayout,
        /// PDF-space region X0,Y0,X1,Y1.
        #[arg(long)]
        bbox: Option<String>,
        #[arg(long)]
        rtl: bool,
        #[arg(long)]
        no_normalize: bool,
        #[arg(long)]
        no_include_hidden: bool,
        #[arg(long)]
        include_annotations: bool,
        /// OCR policy. `auto` OCRs only pages whose native text is not trustworthy.
        #[arg(long, value_enum, default_value_t = text::OcrMode::Never)]
        ocr: text::OcrMode,
        /// OCR language code understood by the compiled Lege OCR backend.
        #[arg(long, default_value = "eng")]
        ocr_language: String,
    },
    /// Inventory or extract page images.
    Images {
        file: PathBuf,
        #[arg(long)]
        pages: Option<String>,
        #[arg(long, value_enum, default_value_t = images::ImageMode::Inventory)]
        mode: images::ImageMode,
        /// Directory for source/decoded/rendered extraction.
        #[arg(long)]
        extract: Option<PathBuf>,
    },
    /// Semantic content dump / typed ops / resources for one page.
    Content {
        file: PathBuf,
        /// One-based page number.
        #[arg(long)]
        page: u32,
        #[arg(long)]
        ops: bool,
        #[arg(long)]
        resources: bool,
        #[arg(long)]
        objects: bool,
    },
    /// Rasterize pages to PNG or PPM.
    Render {
        file: PathBuf,
        #[arg(long)]
        pages: Option<String>,
        /// Output path template: supports {page}, {page_index}, {stem}.
        #[arg(long)]
        output: String,
        #[arg(long)]
        dpi: Option<f64>,
        #[arg(long)]
        scale: Option<f64>,
        #[arg(long, value_enum, default_value_t = render::ImageFormat::Png)]
        format: render::ImageFormat,
        #[arg(long)]
        crop: Option<String>,
        #[arg(long)]
        thumbnail: bool,
    },
    /// Search text across pages.
    Search {
        file: PathBuf,
        query: String,
        #[arg(long)]
        pages: Option<String>,
        #[arg(long, default_value_t = 32)]
        context: usize,
        #[arg(long)]
        no_case_insensitive: bool,
        /// OCR policy for pages without trustworthy native text.
        #[arg(long, value_enum, default_value_t = text::OcrMode::Never)]
        ocr: text::OcrMode,
        #[arg(long, default_value = "eng")]
        ocr_language: String,
    },
    /// Print a document, or plan the job without spooling it.
    Print {
        /// The PDF to print. Optional only with `--list-printers`.
        #[arg(required_unless_present = "list_printers")]
        file: Option<PathBuf>,
        /// Target queue. Defaults to the system default printer.
        #[arg(long)]
        printer: Option<String>,
        /// Enumerate print queues and exit.
        #[arg(long)]
        list_printers: bool,
        /// One-based page selection: 1,3-5 / odd / even / all.
        #[arg(long)]
        pages: Option<String>,
        /// Paper: a4, letter, … or an explicit 210x297mm / 8.5x11in / 612x792pt.
        #[arg(long)]
        paper: Option<String>,
        #[arg(long, value_enum, default_value_t = print::OrientationArg::Auto)]
        orientation: print::OrientationArg,
        /// Uniform user margin in points.
        #[arg(long, conflicts_with_all = ["margin_mm", "margin_in"])]
        margin: Option<f64>,
        /// Uniform user margin in millimetres.
        #[arg(long, conflicts_with = "margin_in")]
        margin_mm: Option<f64>,
        /// Uniform user margin in inches.
        #[arg(long)]
        margin_in: Option<f64>,
        /// actual | fit | shrink | fill | NN%
        #[arg(long)]
        scaling: Option<String>,
        /// 1 | 2 | 4 | 6 | 9 | 16 | booklet
        #[arg(long)]
        n_up: Option<String>,
        #[arg(long, value_enum, default_value_t = print::NUpOrderArg::RightDown)]
        n_up_order: print::NUpOrderArg,
        /// Draw a hairline around each N-up cell.
        #[arg(long)]
        n_up_border: bool,
        #[arg(long, value_enum, default_value_t = print::DuplexArg::None)]
        duplex: print::DuplexArg,
        #[arg(long, default_value_t = 1)]
        copies: u16,
        #[arg(long)]
        no_collate: bool,
        /// Print the selected pages last-to-first.
        #[arg(long)]
        reverse: bool,
        #[arg(long, value_enum, default_value_t = print::SourceBoxArg::Crop)]
        source_box: print::SourceBoxArg,
        /// Compose in grayscale, or ask the driver for mono on pass-through.
        #[arg(long)]
        gray: bool,
        /// Composition resolution. Ignored on the pass-through route.
        #[arg(long)]
        dpi: Option<f64>,
        /// Spool with the file backend into this directory; no real printer.
        #[arg(long)]
        to_file: Option<PathBuf>,
        /// Report the plan as JSON and spool nothing.
        #[arg(long)]
        dry_run: bool,
        /// In a dry run, ask the real queue for its capabilities instead of
        /// assuming a conservative device.
        #[arg(long, requires = "dry_run")]
        query_device: bool,
    },
    /// Persistent stdio JSONL service with snapshot cache.
    Serve {
        #[arg(long, default_value_t = true)]
        stdio: bool,
        #[arg(long, default_value_t = 4)]
        max_open: usize,
        #[arg(long, default_value_t = 300)]
        idle_timeout: u64,
    },
    /// Run a Model Context Protocol server over stdio.
    Mcp {
        #[arg(long, default_value_t = 4)]
        max_open: usize,
        #[arg(long, default_value_t = 300)]
        idle_timeout: u64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let bounds = Bounds {
        max_pages: cli.max_pages,
        max_items: cli.max_items,
        max_bytes: cli.max_bytes,
        timeout_secs: cli.timeout,
    };
    let output = if cli.jsonl {
        OutputMode::Jsonl
    } else if cli.json {
        OutputMode::Json
    } else {
        OutputMode::Human
    };
    let password = cli.password.as_deref();

    let result = match cli.command {
        Command::Inspect { file, pages } => inspect::run(inspect::InspectArgs {
            path: &file,
            password,
            pages: pages.as_deref(),
            bounds,
            output,
            snapshot: None,
            identity: None,
        }),
        Command::Text {
            file,
            pages,
            layout,
            bbox,
            rtl,
            no_normalize,
            no_include_hidden,
            include_annotations,
            ocr,
            ocr_language,
        } => text::run(text::TextArgs {
            path: &file,
            password,
            pages: pages.as_deref(),
            layout,
            bbox: bbox.as_deref(),
            rtl,
            normalize: !no_normalize,
            include_hidden: !no_include_hidden,
            include_annotations,
            ocr,
            ocr_language: &ocr_language,
            system_fonts: cli.system_fonts,
            bounds,
            fail_fast: cli.fail_fast,
            output,
            snapshot: None,
            identity: None,
        }),
        Command::Images {
            file,
            pages,
            mode,
            extract,
        } => images::run(images::ImagesArgs {
            path: &file,
            password,
            pages: pages.as_deref(),
            mode,
            extract_dir: extract.as_deref(),
            system_fonts: cli.system_fonts,
            bounds,
            fail_fast: cli.fail_fast,
            output,
            snapshot: None,
            identity: None,
        }),
        Command::Content {
            file,
            page,
            ops,
            resources,
            objects,
        } => content::run(content::ContentArgs {
            path: &file,
            password,
            page,
            ops,
            resources,
            objects,
            system_fonts: cli.system_fonts,
            bounds,
            output,
            snapshot: None,
            identity: None,
        }),
        Command::Render {
            file,
            pages,
            output: out_template,
            dpi,
            scale,
            format,
            crop,
            thumbnail,
        } => render::run(render::RenderArgs {
            path: &file,
            password,
            pages: pages.as_deref(),
            output: &out_template,
            dpi,
            scale,
            format,
            crop: crop.as_deref(),
            thumbnail,
            system_fonts: cli.system_fonts,
            bounds,
            fail_fast: cli.fail_fast,
            output_mode: output,
            snapshot: None,
            identity: None,
        }),
        Command::Search {
            file,
            query,
            pages,
            context,
            no_case_insensitive,
            ocr,
            ocr_language,
        } => search::run(search::SearchArgs {
            path: &file,
            password,
            query: &query,
            pages: pages.as_deref(),
            context,
            case_insensitive: !no_case_insensitive,
            ocr,
            ocr_language: &ocr_language,
            system_fonts: cli.system_fonts,
            bounds,
            fail_fast: cli.fail_fast,
            output,
            snapshot: None,
            identity: None,
        }),
        Command::Print {
            file,
            printer,
            list_printers,
            pages,
            paper,
            orientation,
            margin,
            margin_mm,
            margin_in,
            scaling,
            n_up,
            n_up_order,
            n_up_border,
            duplex,
            copies,
            no_collate,
            reverse,
            source_box,
            gray,
            dpi,
            to_file,
            dry_run,
            query_device,
        } => print::run(print::PrintArgs {
            path: file.as_deref(),
            password,
            printer: printer.as_deref(),
            list_printers,
            pages: pages.as_deref(),
            paper: paper.as_deref(),
            orientation,
            margin,
            margin_mm,
            margin_in,
            scaling: scaling.as_deref(),
            n_up: n_up.as_deref(),
            n_up_order,
            n_up_border,
            duplex,
            copies,
            no_collate,
            reverse,
            source_box,
            gray,
            dpi,
            to_file: to_file.as_deref(),
            dry_run,
            query_device,
            bounds,
            output,
        }),
        Command::Serve {
            stdio,
            max_open,
            idle_timeout,
        } => {
            if !stdio {
                eprintln!("only --stdio is supported in this release");
                return ExitCode::from(2);
            }
            serve::run(serve::ServeArgs {
                max_open,
                idle_timeout,
                bounds,
            })
        }
        Command::Mcp {
            max_open,
            idle_timeout,
        } => mcp::run(mcp::McpArgs {
            max_open,
            idle_timeout,
            bounds,
        }),
    };

    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}
