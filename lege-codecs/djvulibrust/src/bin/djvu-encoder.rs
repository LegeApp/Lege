//! `djvu-encoder` — standalone command-line DjVu encoder.
//!
//! This is the AGPL-licensed encoder program. It is invoked by other software
//! (including proprietary front-ends) purely as a separate program over an
//! ordinary command-line interface: it reads ordinary image files and a neutral
//! JSON manifest, and it writes a `.djvu` file plus structured diagnostics. It
//! never exposes the encoder's internal object graph, uses no FFI/shared memory/
//! callbacks, and is freely replaceable by any compatible executable.
//!
//! Two front-ends:
//!
//! * `encode-document --manifest pages.json --output out.djvu` — the
//!   pre-separated path. Each page names up to three ordinary interchange files:
//!   a bilevel `mask`, a gray/color `background`, and a word-box `ocr` JSON.
//!
//! * `encode PAGE... --output out.djvu` — the simple path for general use. Each
//!   input is an ordinary page image in a format enabled at build time; the encoder does
//!   its own layer separation (MRC by default, `--photo`/`--bilevel` to force a
//!   single layer).
//!
//! Manifest schema version: 1.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use clap::{Args, Parser, Subcommand};
#[cfg(feature = "rayon")]
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use djvu_encoder::doc::{DjvuBuilder, EncodedPage, Page, PageBuilder, PageEncodeParams};
use djvu_encoder::image::image_formats::{Bitmap, GrayPixel, Pixel, Pixmap};

const MANIFEST_SCHEMA_VERSION: u32 = 1;

// ============================================================================
// Exit codes (documented, stable)
// ============================================================================

/// Structured failure carrying the process exit code it should map to.
struct CliError {
    code: u8,
    message: String,
}

impl CliError {
    fn input(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }
    fn encode(message: impl Into<String>) -> Self {
        Self {
            code: 3,
            message: message.into(),
        }
    }
}

type CliResult<T> = std::result::Result<T, CliError>;

// ============================================================================
// CLI definition
// ============================================================================

#[derive(Parser)]
#[command(
    name = "djvu-encoder",
    version,
    about = "Standalone DjVu encoder (AGPLv3). Encodes ordinary page images into a .djvu file."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Encode a document from a JSON manifest of pre-separated page layers.
    EncodeDocument(EncodeDocumentArgs),
    /// Encode a document directly from a list of ordinary page images.
    Encode(EncodeArgs),
    /// Print name / version / manifest-schema version (for host handshakes).
    Version(VersionArgs),
}

#[derive(Args)]
struct EncodeDocumentArgs {
    /// Path to the JSON manifest describing pages and layer files.
    #[arg(long)]
    manifest: PathBuf,
    /// Output `.djvu` path.
    #[arg(long, short)]
    output: PathBuf,
    /// Emit newline-delimited JSON progress records on stdout.
    #[arg(long)]
    progress_json: bool,
    /// Worker threads (0 = automatic; serial-only builds accept 0 or 1).
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

#[derive(Args)]
struct EncodeArgs {
    /// One or more supported ordinary page images, in page order.
    #[arg(required = true)]
    pages: Vec<PathBuf>,
    /// Output `.djvu` path.
    #[arg(long, short)]
    output: PathBuf,
    /// Output DPI written into the document.
    #[arg(long, default_value_t = 300)]
    dpi: u32,
    /// IW44 background subsample factor (1 = none, 3 = c44 default).
    #[arg(long, default_value_t = 1)]
    bg_subsample: u8,
    /// Encode each page as a single IW44 photo layer (no text mask).
    #[arg(long, conflicts_with = "bilevel")]
    photo: bool,
    /// Encode each page as a single bilevel JB2 layer (no background).
    #[arg(long)]
    bilevel: bool,
    /// Emit newline-delimited JSON progress records on stdout.
    #[arg(long)]
    progress_json: bool,
    /// Worker threads (0 = automatic; serial-only builds accept 0 or 1).
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

#[derive(Args)]
struct VersionArgs {
    /// Emit the version handshake as a single JSON object.
    #[arg(long)]
    json: bool,
}

// ============================================================================
// Manifest schema (neutral interchange — no encoder internals cross here)
// ============================================================================

#[derive(Deserialize)]
struct Manifest {
    /// Manifest schema version. Encoder rejects versions it does not understand.
    #[serde(default = "one")]
    version: u32,
    #[serde(default = "default_dpi")]
    dpi: u32,
    /// IW44 slice budget per background chunk (maps to `PageEncodeParams::slices`).
    #[serde(default)]
    slices: Option<usize>,
    /// IW44 background subsample factor (1 = none, 3 = c44 default).
    #[serde(default = "one_u8")]
    bg_subsample: u8,
    pages: Vec<ManifestPage>,
}

#[derive(Deserialize)]
struct ManifestPage {
    /// Zero-based page number. Defaults to the entry's position in `pages`.
    #[serde(default)]
    index: Option<usize>,
    /// Explicit page size; required only when neither layer file is present.
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    /// Bilevel full-page mask image (PNG/PBM/...); becomes the JB2 foreground.
    #[serde(default)]
    mask: Option<String>,
    /// Gray or color full-page background image; becomes the IW44 background.
    #[serde(default)]
    background: Option<String>,
    /// Optional per-page IW44 background subsample override.
    #[serde(default)]
    bg_subsample: Option<u8>,
    /// Word-box OCR JSON; becomes the hidden text layer.
    #[serde(default)]
    ocr: Option<String>,
}

fn one() -> u32 {
    1
}
fn one_u8() -> u8 {
    1
}
fn default_dpi() -> u32 {
    300
}

/// Neutral OCR interchange: a flat list of word boxes in page pixel coordinates.
#[derive(Deserialize)]
struct OcrDoc {
    words: Vec<OcrWord>,
}

#[derive(Deserialize)]
struct OcrWord {
    text: String,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

// ============================================================================
// Progress reporting
// ============================================================================

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum ProgressEvent<'a> {
    Start {
        pages: usize,
    },
    PageDone {
        page: usize,
        of: usize,
    },
    // Part of the documented progress protocol; reserved for non-fatal notices.
    #[allow(dead_code)]
    Warning {
        message: &'a str,
    },
    Done {
        output: &'a str,
        bytes: usize,
    },
}

struct Progress {
    enabled: bool,
}

impl Progress {
    fn new(enabled: bool) -> Self {
        Self { enabled }
    }
    fn emit(&self, event: ProgressEvent<'_>) {
        if !self.enabled {
            return;
        }
        if let Ok(line) = serde_json::to_string(&event) {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }
}

// ============================================================================
// main
// ============================================================================

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::EncodeDocument(args) => run_encode_document(args),
        Command::Encode(args) => run_encode_simple(args),
        Command::Version(args) => {
            print_version(args.json);
            Ok(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("djvu-encoder: error: {}", err.message);
            ExitCode::from(err.code)
        }
    }
}

fn print_version(json: bool) {
    let name = env!("CARGO_PKG_NAME");
    let version = env!("CARGO_PKG_VERSION");
    if json {
        println!(
            "{{\"name\":\"{name}\",\"version\":\"{version}\",\"manifest_schema\":{MANIFEST_SCHEMA_VERSION}}}"
        );
    } else {
        println!("{name} {version} (manifest schema {MANIFEST_SCHEMA_VERSION})");
    }
}

// ============================================================================
// encode-document
// ============================================================================

fn run_encode_document(args: EncodeDocumentArgs) -> CliResult<()> {
    let manifest_text = std::fs::read_to_string(&args.manifest)
        .map_err(|e| CliError::input(format!("cannot read manifest {:?}: {e}", args.manifest)))?;
    let manifest: Manifest = serde_json::from_str(&manifest_text)
        .map_err(|e| CliError::input(format!("invalid manifest JSON: {e}")))?;

    if manifest.version > MANIFEST_SCHEMA_VERSION {
        return Err(CliError::input(format!(
            "manifest schema version {} is newer than this encoder supports ({}); upgrade djvu-encoder",
            manifest.version, MANIFEST_SCHEMA_VERSION
        )));
    }
    if manifest.pages.is_empty() {
        return Err(CliError::input("manifest has no pages"));
    }

    let base_dir = args
        .manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    // Resolve page numbers: explicit `index` wins, else position. The result
    // must be a permutation of 0..n (the encoder requires every page present).
    let n = manifest.pages.len();
    let mut numbered: Vec<(usize, &ManifestPage)> = manifest
        .pages
        .iter()
        .enumerate()
        .map(|(pos, page)| (page.index.unwrap_or(pos), page))
        .collect();
    numbered.sort_by_key(|(num, _)| *num);
    for (expected, (num, _)) in numbered.iter().enumerate() {
        if *num != expected {
            return Err(CliError::input(format!(
                "page numbers must be a contiguous 0..{n} set; got {num} where {expected} expected"
            )));
        }
    }

    let mut params = PageEncodeParams::default();
    params.dpi = manifest.dpi;
    if let Some(slices) = manifest.slices {
        params.slices = Some(slices);
    }
    params.bg_subsample = manifest.bg_subsample.clamp(1, 12);

    let progress = Progress::new(args.progress_json);
    progress.emit(ProgressEvent::Start { pages: n });

    let bytes = encode_pages(
        n,
        manifest.dpi,
        params,
        args.threads,
        &progress,
        &args.output,
        |page_num| {
            let (_, page) = numbered[page_num];
            build_manifest_page(page_num, page, &base_dir).map(|built| (built, page.bg_subsample))
        },
    )?;

    progress.emit(ProgressEvent::Done {
        output: &args.output.to_string_lossy(),
        bytes,
    });
    Ok(())
}

/// Builds a single `Page` from a manifest entry by loading its layer files.
fn build_manifest_page(page_num: usize, page: &ManifestPage, base_dir: &Path) -> CliResult<Page> {
    let mask = match &page.mask {
        Some(rel) => Some(load_bilevel(&base_dir.join(rel))?),
        None => None,
    };
    let background = match &page.background {
        Some(rel) => Some(load_rgb(&base_dir.join(rel))?),
        None => None,
    };

    // Page dimensions come from the mask, else the background, else explicit
    // width/height (blank pages carry no layer file).
    let (width, height) = match (&mask, &background) {
        (Some(m), _) => m.dimensions(),
        (None, Some(b)) => b.dimensions(),
        (None, None) => match (page.width, page.height) {
            (Some(w), Some(h)) => (w, h),
            _ => {
                return Err(CliError::input(format!(
                    "page {page_num} has no mask, no background, and no width/height"
                )));
            }
        },
    };

    if let Some(m) = &mask {
        let (mw, mh) = m.dimensions();
        if (mw, mh) != (width, height) {
            return Err(CliError::input(format!(
                "page {page_num}: mask {mw}x{mh} does not match page {width}x{height}"
            )));
        }
    }
    if let Some(b) = &background {
        let (bw, bh) = b.dimensions();
        if (bw, bh) != (width, height) {
            return Err(CliError::input(format!(
                "page {page_num}: background {bw}x{bh} does not match page {width}x{height}"
            )));
        }
    }

    let mut builder = PageBuilder::new(page_num, width, height);
    let mut has_layer = false;
    if let Some(mask) = mask {
        builder = builder.with_foreground(mask, 0, 0);
        has_layer = true;
    }
    if let Some(background) = background {
        builder = builder
            .with_background(background)
            .map_err(|e| CliError::encode(format!("page {page_num}: {e}")))?;
        has_layer = true;
    }
    if !has_layer {
        // Blank page: a white IW44 canvas keeps the page in the document.
        let white = Pixmap::from_pixel(width, height, Pixel::white());
        builder = builder
            .with_background(white)
            .map_err(|e| CliError::encode(format!("page {page_num}: {e}")))?;
    }

    if let Some(rel) = &page.ocr {
        let words = load_ocr_words(&base_dir.join(rel), width, height)?;
        if !words.is_empty() {
            builder = builder.with_ocr_words(words);
        }
    }

    builder
        .build()
        .map_err(|e| CliError::encode(format!("page {page_num}: {e}")))
}

// ============================================================================
// encode (simple mode: raw page images in, own layer separation)
// ============================================================================

fn run_encode_simple(args: EncodeArgs) -> CliResult<()> {
    let n = args.pages.len();
    let mut params = PageEncodeParams::default();
    params.dpi = args.dpi;
    params.bg_subsample = args.bg_subsample.clamp(1, 12);

    let progress = Progress::new(args.progress_json);
    progress.emit(ProgressEvent::Start { pages: n });

    let bytes = encode_pages(
        n,
        args.dpi,
        params,
        args.threads,
        &progress,
        &args.output,
        |page_num| {
            let path = &args.pages[page_num];
            build_simple_page(page_num, path, args.photo, args.bilevel).map(|page| (page, None))
        },
    )?;

    progress.emit(ProgressEvent::Done {
        output: &args.output.to_string_lossy(),
        bytes,
    });
    Ok(())
}

/// Builds a page from a single ordinary image, performing our own separation.
fn build_simple_page(page_num: usize, path: &Path, photo: bool, bilevel: bool) -> CliResult<Page> {
    let rgb = load_rgb(path)?;
    let (width, height) = rgb.dimensions();
    let builder = PageBuilder::new(page_num, width, height);

    if photo {
        // Single IW44 photo layer.
        return builder
            .with_background(rgb)
            .map_err(|e| CliError::encode(format!("page {page_num}: {e}")))?
            .build()
            .map_err(|e| CliError::encode(format!("page {page_num}: {e}")));
    }

    let mask = otsu_mask(&rgb);
    if bilevel {
        // Single JB2 bilevel layer.
        return builder
            .with_foreground(mask, 0, 0)
            .build()
            .map_err(|e| CliError::encode(format!("page {page_num}: {e}")));
    }

    // MRC: JB2 ink mask over the original as an IW44 background.
    builder
        .with_foreground(mask, 0, 0)
        .with_background(rgb)
        .map_err(|e| CliError::encode(format!("page {page_num}: {e}")))?
        .build()
        .map_err(|e| CliError::encode(format!("page {page_num}: {e}")))
}

/// Global Otsu threshold on luma → bilevel mask (ink = black where luma < t).
fn otsu_mask(rgb: &Pixmap) -> Bitmap {
    let (w, h) = rgb.dimensions();
    let n = (w as usize) * (h as usize);
    let mut hist = [0u32; 256];
    let mut luma = Vec::with_capacity(n);
    for y in 0..h {
        for x in 0..w {
            let p = rgb.get_pixel(x, y);
            // Rec. 601 luma, integer.
            let l = ((p.r as u32 * 77 + p.g as u32 * 150 + p.b as u32 * 29) >> 8) as u8;
            hist[l as usize] += 1;
            luma.push(l);
        }
    }

    let total = n as f64;
    let sum: f64 = (0..256).map(|i| i as f64 * hist[i] as f64).sum();
    let (mut sum_b, mut w_b, mut max_var, mut threshold) = (0.0f64, 0.0f64, -1.0f64, 128usize);
    for t in 0..256 {
        w_b += hist[t] as f64;
        if w_b == 0.0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0.0 {
            break;
        }
        sum_b += t as f64 * hist[t] as f64;
        let m_b = sum_b / w_b;
        let m_f = (sum - sum_b) / w_f;
        let between = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if between > max_var {
            max_var = between;
            threshold = t;
        }
    }

    let pixels: Vec<GrayPixel> = luma
        .into_iter()
        .map(|l| {
            if (l as usize) < threshold {
                GrayPixel::black()
            } else {
                GrayPixel::white()
            }
        })
        .collect();
    Bitmap::from_vec(w, h, pixels)
}

// ============================================================================
// Shared encode driver
// ============================================================================

/// Encodes `n` pages in parallel via `build`, assembles them in order, and
/// writes the `.djvu`. `build(page_num)` returns the `Page` plus an optional
/// per-page IW44 background-subsample override.
fn encode_pages<F>(
    n: usize,
    dpi: u32,
    params: PageEncodeParams,
    threads: usize,
    progress: &Progress,
    output: &Path,
    build: F,
) -> CliResult<usize>
where
    F: Fn(usize) -> CliResult<(Page, Option<u8>)> + Sync + Send,
{
    let doc = DjvuBuilder::new(n)
        .with_dpi(dpi)
        .with_params(params.clone())
        .build();
    let done = AtomicUsize::new(0);

    let encode_one = |page_num: usize| -> CliResult<EncodedPage> {
        let (page, bg_subsample) = build(page_num)?;
        let mut page_params = params.clone();
        if let Some(subsample) = bg_subsample {
            page_params.bg_subsample = subsample.clamp(1, 12);
        }
        let encoded = doc
            .encode_page_with_params(page, &page_params)
            .map_err(|e| CliError::encode(format!("page {page_num}: {e}")))?;
        let count = done.fetch_add(1, Ordering::Relaxed) + 1;
        progress.emit(ProgressEvent::PageDone { page: count, of: n });
        Ok(encoded)
    };

    #[cfg(feature = "rayon")]
    let mut encoded: Vec<EncodedPage> = if threads == 1 {
        (0..n).map(encode_one).collect::<CliResult<Vec<_>>>()?
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads) // 0 lets rayon choose
            .build()
            .map_err(|e| CliError::encode(format!("thread pool: {e}")))?;
        pool.install(|| {
            (0..n)
                .into_par_iter()
                .map(&encode_one)
                .collect::<CliResult<Vec<_>>>()
        })?
    };

    #[cfg(not(feature = "rayon"))]
    let mut encoded: Vec<EncodedPage> = {
        if threads > 1 {
            return Err(CliError::input(
                "this size-optimized build has no parallel support; use --threads 0 or 1",
            ));
        }
        (0..n).map(encode_one).collect::<CliResult<Vec<_>>>()?
    };

    // Assemble in page order.
    encoded.sort_by_key(|page| page.page_num);
    for page in encoded {
        doc.add_encoded_page(page)
            .map_err(|e| CliError::encode(format!("assemble: {e}")))?;
    }

    let bytes = doc
        .finalize()
        .map_err(|e| CliError::encode(format!("finalize: {e}")))?;
    std::fs::write(output, &bytes)
        .map_err(|e| CliError::input(format!("cannot write {output:?}: {e}")))?;
    Ok(bytes.len())
}

// ============================================================================
// Image loading (ordinary formats in; encoder types out)
// ============================================================================

fn load_rgb(path: &Path) -> CliResult<Pixmap> {
    let img = image::open(path)
        .map_err(|e| CliError::input(format!("cannot read image {path:?}: {e}")))?
        .to_rgb8();
    let (w, h) = (img.width(), img.height());
    let pixels: Vec<Pixel> = img
        .as_raw()
        .chunks_exact(3)
        .map(|c| Pixel::new(c[0], c[1], c[2]))
        .collect();
    Ok(Pixmap::from_vec(w, h, pixels))
}

fn load_bilevel(path: &Path) -> CliResult<Bitmap> {
    let img = image::open(path)
        .map_err(|e| CliError::input(format!("cannot read mask {path:?}: {e}")))?
        .to_luma8();
    let (w, h) = (img.width(), img.height());
    let pixels: Vec<GrayPixel> = img
        .as_raw()
        .iter()
        .map(|&v| {
            if v < 128 {
                GrayPixel::black()
            } else {
                GrayPixel::white()
            }
        })
        .collect();
    Ok(Bitmap::from_vec(w, h, pixels))
}

fn load_ocr_words(
    path: &Path,
    page_width: u32,
    page_height: u32,
) -> CliResult<Vec<(String, u16, u16, u16, u16)>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::input(format!("cannot read ocr {path:?}: {e}")))?;
    let doc: OcrDoc = serde_json::from_str(&text)
        .map_err(|e| CliError::input(format!("invalid ocr JSON {path:?}: {e}")))?;

    let max_w = page_width.min(u16::MAX as u32) as u16;
    let max_h = page_height.min(u16::MAX as u32) as u16;
    let mut words = Vec::with_capacity(doc.words.len());
    for word in doc.words {
        let text = word.text.trim();
        if text.is_empty() || word.w == 0 || word.h == 0 {
            continue;
        }
        if word.x >= max_w || word.y >= max_h {
            continue;
        }
        words.push((text.to_string(), word.x, word.y, word.w, word.h));
    }
    Ok(words)
}
