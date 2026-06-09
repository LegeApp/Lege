use Legencode::streamline::Jbig2Mode;
use anyhow::{Result, anyhow, bail};
use lege::progress::{self, ProgressUpdate};
use lege::text_loader::CLI_TEXT;
use lege::{
    AppConfig, CoverFormat, DebugCropKind, ImageRegionDitherMode, PageRange, PipelineConfig,
    error_println, info_println, is_ocr_available, run_images_to_images_mode,
    run_pdf_layout_crop_debug, run_pdf_to_images_mode, run_pdf_to_png_mode, run_png_mode,
    run_png_mode_with_config, target_profiles,
};

mod version;
mod worker_json;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use version::display_version;

// ============================================================================
// COLOR CONFIGURATION - Easily customize all CLI colors here
// ============================================================================
pub struct ColorConfig {
    // Interactive prompts
    pub prompt: &'static str,    // Prompt color (empty = terminal default)
    pub info: &'static str,      // Info color (empty = terminal default)
    pub highlight: &'static str, // Highlight color (empty = terminal default)

    // Processing stages - subtle, muted colors for visual pleasure
    pub page_start: &'static str,    // Soft blue for page starting
    pub page_complete: &'static str, // Medium-bright green - MOST STRIKING
    pub ocr: &'static str,           // Muted purple/gray for OCR operations
    pub render: &'static str,        // Soft cyan for rendering
    pub detect: &'static str,        // Soft yellow for detection/inference
    pub encode: &'static str,        // Soft magenta for encoding
    pub worker: &'static str,        // Dim cyan for worker/slot messages
    pub dag: &'static str,           // Very dim for DAG internals

    // Status labels
    pub status_label: &'static str, // Dim blue for "Status" label
    pub detail_label: &'static str, // Dim cyan for "Detail" label

    pub reset: &'static str, // Reset to default
}

pub const COLORS: ColorConfig = ColorConfig {
    // Interactive prompts
    // Keep prompt/info text uncolored so terminals choose their readable default.
    prompt: "",
    info: "",
    highlight: "",

    // Processing stages - subtle palette
    page_start: "\x1b[94m",    // Bright blue (soft)
    page_complete: "\x1b[92m", // Bright green (STRIKING) - the star of the show
    ocr: "\x1b[90m",           // Bright black (muted gray)
    render: "\x1b[96m",        // Bright cyan (soft)
    detect: "\x1b[93m",        // Bright yellow (soft)
    encode: "\x1b[95m",        // Bright magenta (soft)
    worker: "\x1b[2;36m",      // Dim cyan (very subtle)
    dag: "\x1b[2;90m",         // Dim bright-black (almost invisible)

    // Status labels
    status_label: "\x1b[2;34m", // Dim blue
    detail_label: "\x1b[2;36m", // Dim cyan

    reset: "\x1b[0m", // Reset
};
// ============================================================================

fn cli_ansi_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }

    if !std::io::stdout().is_terminal() {
        return false;
    }

    #[cfg(windows)]
    {
        // Avoid printing raw ESC sequences on Windows consoles that have not
        // opted into virtual terminal processing, such as some sandboxed VMs.
        std::env::var_os("WT_SESSION").is_some()
            || std::env::var_os("ANSICON").is_some()
            || std::env::var_os("ConEmuANSI")
                .map(|v| v != "OFF")
                .unwrap_or(false)
            || std::env::var_os("TERM")
                .and_then(|v| v.into_string().ok())
                .map(|v| !v.eq_ignore_ascii_case("dumb"))
                .unwrap_or(false)
    }

    #[cfg(not(windows))]
    {
        std::env::var_os("TERM")
            .and_then(|v| v.into_string().ok())
            .map(|v| !v.eq_ignore_ascii_case("dumb"))
            .unwrap_or(false)
    }
}

fn cli_color(code: &'static str) -> &'static str {
    if cli_ansi_enabled() { code } else { "" }
}

fn cli_reset() -> &'static str {
    cli_color(COLORS.reset)
}

use lege::processing_log::{
    self as history_log, ProcessingOptions as LogProcessingOptions,
    ProcessingResult as LogProcessingResult,
};

mod windows_dirs;
use std::io;
use std::time::{Instant, SystemTime};
use sysinfo::{Disks, System};

/// Cleanup function specifically for CLI to ensure clean process exit
fn cleanup_cli_resources() {
    // Give background tasks a moment to complete
    std::thread::sleep(std::time::Duration::from_millis(200));
}

/// Terminate the process without running C atexit handlers.
/// ORT's WebGPU/Dawn backend registers atexit hooks that can segfault during
/// Vulkan teardown; `_exit` bypasses them while still flushing Rust I/O.
fn fast_exit(code: i32) -> ! {
    // Flush stdout/stderr so all output is visible
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    #[cfg(unix)]
    unsafe {
        libc::_exit(code);
    }
    #[cfg(not(unix))]
    std::process::exit(code);
}

/// --probe-json: print machine-readable metadata about a file then exit.
/// stdout is exclusively JSON; errors go to stderr.
fn probe_json(path: PathBuf) -> Result<()> {
    if path.is_dir() {
        let count = count_image_files_in_dir_probe(&path);
        println!(
            "{}",
            serde_json::json!({"kind": "image_folder", "pages": count})
        );
        return Ok(());
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "pdf" => {
            let doc = lopdf::Document::load(&path)
                .map_err(|e| anyhow!("Cannot open PDF '{}': {}", path.display(), e))?;
            let pages = doc.get_pages().len();
            println!("{}", serde_json::json!({"kind": "pdf", "pages": pages}));
        }
        "zip" => {
            let file = std::fs::File::open(&path)
                .map_err(|e| anyhow!("Cannot open ZIP '{}': {}", path.display(), e))?;
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|e| anyhow!("Cannot read ZIP '{}': {}", path.display(), e))?;
            let count = count_images_in_zip_probe(&mut archive, 0);
            println!("{}", serde_json::json!({"kind": "zip", "pages": count}));
        }
        _ => {
            println!("{}", serde_json::json!({"kind": "unknown", "pages": 0}));
        }
    }
    Ok(())
}

/// Count supported image files in a directory (non-recursive).
fn count_image_files_in_dir_probe(dir: &PathBuf) -> usize {
    const IMG_EXTS: &[&str] = &[
        "png", "jpg", "jpeg", "ppm", "pbm", "pgm", "pnm", "tiff", "tif", "bmp", "jp2",
    ];
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    let p = e.path();
                    if !p.is_file() {
                        return false;
                    }
                    p.extension()
                        .and_then(|x| x.to_str())
                        .map(|x| IMG_EXTS.contains(&x.to_ascii_lowercase().as_str()))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

/// Count supported images inside a ZIP, including nested ZIPs (max depth 4).
fn count_images_in_zip_probe<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    depth: u32,
) -> usize {
    const MAX_DEPTH: u32 = 4;
    const IMG_EXTS: &[&str] = &[
        "png", "jpg", "jpeg", "ppm", "pbm", "pgm", "pnm", "tiff", "tif", "bmp", "jp2",
    ];
    if depth > MAX_DEPTH {
        return 0;
    }
    let mut count = 0usize;
    let mut nested: Vec<Vec<u8>> = Vec::new();
    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index_raw(i) else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_ascii_lowercase();
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if IMG_EXTS.contains(&ext) {
            count += 1;
        } else if ext == "zip" && depth < MAX_DEPTH {
            let mut data = Vec::new();
            if entry.read_to_end(&mut data).is_ok() {
                nested.push(data);
            }
        }
    }
    for data in nested {
        let cursor = std::io::Cursor::new(data);
        if let Ok(mut nested_arc) = zip::ZipArchive::new(cursor) {
            count += count_images_in_zip_probe(&mut nested_arc, depth + 1);
        }
    }
    count
}

// Binarization parsing moved to CliConfigBuilder in types.rs

// IMPORTANT: CLI user-facing copy in this file must come from
// `language_service/<locale>/cli_text.json` via `CLI_TEXT` (through
// `text_loader.rs`). Do not introduce hardcoded user-visible strings in
// `main.rs`; update the locale JSON files instead.
fn fmt1(template: &str, a: impl std::fmt::Display) -> String {
    template.replacen("{}", &a.to_string(), 1)
}

fn fmt2(template: &str, a: impl std::fmt::Display, b: impl std::fmt::Display) -> String {
    template
        .replacen("{}", &a.to_string(), 1)
        .replacen("{}", &b.to_string(), 1)
}

fn fmt3(
    template: &str,
    a: impl std::fmt::Display,
    b: impl std::fmt::Display,
    c: impl std::fmt::Display,
) -> String {
    template
        .replacen("{}", &a.to_string(), 1)
        .replacen("{}", &b.to_string(), 1)
        .replacen("{}", &c.to_string(), 1)
}

fn fmt4(
    template: &str,
    a: impl std::fmt::Display,
    b: impl std::fmt::Display,
    c: impl std::fmt::Display,
    d: impl std::fmt::Display,
) -> String {
    template
        .replacen("{}", &a.to_string(), 1)
        .replacen("{}", &b.to_string(), 1)
        .replacen("{}", &c.to_string(), 1)
        .replacen("{}", &d.to_string(), 1)
}

fn hardware_acceleration_status() -> (bool, String) {
    (true, "WGPU (Vulkan/Metal/DX12)".to_string())
}

/// All optional processing flags for non-interactive CLI mode.
#[derive(Default)]
struct CliOptions {
    // --- Output format ---
    text_format: Option<String>, // --text-format ccitt4|jbig2|jpeg|djvu|epub
    cover_format: Option<String>, // --cover-format jpeg|jp2|ccitt4|jbig2|none

    // --- Binarization ---
    binarization: Option<String>, // --binarization adaptive|fixed|heavy
    threshold: Option<u8>,        // --threshold 200
    sauvola_k: Option<f32>,       // --sauvola-k 0.3

    // --- Quality ---
    djvu_quality: Option<u8>, // --djvu-quality 100
    high_quality: bool,       // --high-quality

    // --- Output ---
    output_dir: Option<PathBuf>, // --output <dir>

    // --- Processing toggles ---
    dither: bool,                  // --dither
    no_layout: bool,               // --no-layout
    ocr: Option<bool>,             // --ocr / --no-ocr
    ocr_mode: Option<OcrMode>,     // --ocr-mode fast|best / --best-ocr
    slow_ocr_scale: Option<f32>,   // --best-ocr-scale N
    no_cover: bool,                // --no-cover
    invert: bool,                  // --invert
    jbig2_mode: Option<Jbig2Mode>, // --jbig2-mode generic|symbol|sym-unify
    halftone: bool, // --halftone (JBIG2 halftone segments via jbig2halftone.rs; overrides --dither)
    deskew: bool,   // --deskew
    reflow: bool,   // --reflow (raster reflow; requires layout detection)
    jpeg_compat: bool, // --jpeg-compat
    center_margins: bool, // --center-margins
    crop_margins: bool, // --crop-margins
    force_crop: bool, // --force-crop
    image_only: bool, // --image-only
    original_images: bool, // --original-images (default is already original; explicit opt-in)
    fast_resize: bool, // --fast-resize (force CPU fast_image_resize backend)

    // --- Language ---
    language: Option<String>, // --language <code>

    // --- Debug / data-generation modes ---
    pdf_to_png: Option<u32>,      // --pdf-to-png HEIGHT
    png_folder: bool,             // --png-folder  (first positional arg is the folder)
    crop_areas: Option<String>,   // --crop-areas text|image|both
    debug_format: Option<String>, // --format png|jpg  (for --crop-areas)
    pdf_to_images: bool,          // --pdf-to-images
    images_to_images: bool,       // --images-to-images
    png_quantize: bool,           // --png-quantize
    png_colors: u16,              // --png-colors N
    jp2_debug: Option<u32>,       // --jp2-debug HEIGHT  (render pages → JP2 + size log)
    gray_jp2: bool,               // --gray-jp2  (image regions → grayscale JP2 overlay)

    // --- GUI integration modes ---
    /// Suppress human output and emit newline-delimited JSON progress events to stdout.
    gui_worker: bool, // --gui-worker
    /// Probe a file and print machine-readable metadata as JSON then exit.
    probe_json: bool, // --probe-json
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OcrMode {
    Fast,
    Best,
}

impl OcrMode {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "best" => Ok(Self::Best),
            _ => bail!("Invalid OCR mode '{}'. Use: fast or best", raw),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Best => "best",
        }
    }
}

/// Extract all `--flag` and `--key value` processing options from the arg list,
/// returning the remaining positional args and the parsed options.
fn extract_cli_options(args: Vec<String>) -> Result<(Vec<String>, CliOptions)> {
    let mut remaining: Vec<String> = Vec::with_capacity(args.len());
    let mut opts = CliOptions::default();
    opts.png_colors = 256;

    // Always keep argv[0]
    remaining.push(
        args.first()
            .cloned()
            .ok_or_else(|| anyhow!("Missing argv[0]"))?,
    );

    let mut i = 1usize;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            // --- key-value options ---
            "--text-format" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --text-format"))?;
                let normalized = val.trim().to_ascii_lowercase();
                match normalized.as_str() {
                    "ccitt4" | "jbig2" | "jpeg" | "djvu" | "epub" => {}
                    _ => bail!(
                        "Invalid --text-format '{}'. Use: ccitt4, jbig2, jpeg, djvu, or epub",
                        val
                    ),
                }
                opts.text_format = Some(normalized);
                i += 2;
            }
            "--cover-format" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --cover-format"))?;
                let normalized = val.trim().to_ascii_lowercase();
                match normalized.as_str() {
                    "jpeg" | "jp2" | "ccitt4" | "jbig2" | "none" => {}
                    _ => bail!(
                        "Invalid --cover-format '{}'. Use: jpeg, jp2, ccitt4, jbig2, or none",
                        val
                    ),
                }
                opts.cover_format = Some(normalized);
                i += 2;
            }
            "--jbig2-mode" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --jbig2-mode"))?;
                opts.jbig2_mode = Some(parse_jbig2_mode_flag(val)?);
                i += 2;
            }
            "--binarization" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --binarization"))?;
                let normalized = val.trim().to_ascii_lowercase();
                match normalized.as_str() {
                    "adaptive" | "fixed" | "heavy" => {}
                    _ => bail!(
                        "Invalid --binarization '{}'. Use: adaptive, fixed, or heavy",
                        val
                    ),
                }
                opts.binarization = Some(normalized);
                i += 2;
            }
            "--threshold" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --threshold"))?;
                let thr: u8 = val
                    .parse()
                    .map_err(|_| anyhow!("Invalid --threshold '{}'. Must be 0-255", val))?;
                opts.threshold = Some(thr);
                i += 2;
            }
            "--sauvola-k" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --sauvola-k"))?;
                let k: f32 = val
                    .parse()
                    .map_err(|_| anyhow!("Invalid --sauvola-k '{}'. Must be 0.0-1.0", val))?;
                if !(0.0..=1.0).contains(&k) {
                    bail!("--sauvola-k must be between 0.0 and 1.0, got: {}", k);
                }
                opts.sauvola_k = Some(k);
                i += 2;
            }
            "--djvu-quality" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --djvu-quality"))?;
                let q: u8 = val
                    .parse()
                    .map_err(|_| anyhow!("Invalid --djvu-quality '{}'. Must be 1-100", val))?;
                if q == 0 {
                    bail!("--djvu-quality must be between 1 and 100");
                }
                opts.djvu_quality = Some(q);
                i += 2;
            }
            "--output" | "--out" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after {}", arg))?;
                opts.output_dir = Some(PathBuf::from(sanitize_path_arg(val)));
                i += 2;
            }
            "--language" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --language"))?;
                let normalized = raw.trim().to_ascii_lowercase();
                if normalized.is_empty() {
                    bail!("--language value cannot be empty");
                }
                if !normalized
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    bail!(
                        "Invalid --language '{}'. Use only a-z, 0-9, and underscore",
                        raw
                    );
                }
                opts.language = Some(normalized);
                i += 2;
            }
            "--ocr-mode" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --ocr-mode"))?;
                let mode = OcrMode::parse(val)?;
                opts.ocr = Some(true);
                opts.ocr_mode = Some(mode);
                i += 2;
            }

            // --- debug / data-generation key-value ---
            "--pdf-to-png" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing height after --pdf-to-png"))?;
                let height: u32 = val
                    .parse()
                    .map_err(|_| anyhow!("Invalid height: {}", val))?;
                if height < 100 || height > 10000 {
                    bail!("--pdf-to-png height must be between 100 and 10000 pixels");
                }
                opts.pdf_to_png = Some(height);
                i += 2;
            }
            "--jp2-debug" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing height after --jp2-debug"))?;
                let height: u32 = val
                    .parse()
                    .map_err(|_| anyhow!("Invalid height: {}", val))?;
                if height < 100 || height > 10000 {
                    bail!("--jp2-debug height must be between 100 and 10000 pixels");
                }
                opts.jp2_debug = Some(height);
                i += 2;
            }
            "--crop-areas" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing mode after --crop-areas (text|image|both)"))?;
                let normalized = val.trim().to_ascii_lowercase();
                match normalized.as_str() {
                    "text" | "image" | "both" => {}
                    _ => bail!(
                        "Invalid --crop-areas mode '{}'. Use: text, image, or both",
                        val
                    ),
                }
                opts.crop_areas = Some(normalized);
                i += 2;
            }
            "--format" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --format"))?;
                opts.debug_format = Some(val.clone());
                i += 2;
            }
            "--png-colors" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after --png-colors"))?;
                let colors: u16 = val
                    .parse()
                    .map_err(|_| anyhow!("Invalid --png-colors value: {}", val))?;
                if !(1..=256).contains(&colors) {
                    bail!("--png-colors must be between 1 and 256");
                }
                opts.png_colors = colors;
                i += 2;
            }

            // --- debug / data-generation boolean flags ---
            "--png-folder" => {
                opts.png_folder = true;
                i += 1;
            }
            "--pdf-to-images" => {
                opts.pdf_to_images = true;
                i += 1;
            }
            "--images-to-images" => {
                opts.images_to_images = true;
                i += 1;
            }
            "--png-quantize" => {
                opts.png_quantize = true;
                i += 1;
            }

            // --- boolean flags ---
            "--dither" => {
                opts.dither = true;
                i += 1;
            }
            "--no-layout" => {
                opts.no_layout = true;
                i += 1;
            }
            "--ocr" => {
                opts.ocr = Some(true);
                i += 1;
            }
            "--no-ocr" => {
                opts.ocr = Some(false);
                opts.ocr_mode = None;
                i += 1;
            }
            "--best-ocr" => {
                opts.ocr_mode = Some(OcrMode::Best);
                opts.ocr = Some(true); // best OCR implies --ocr
                i += 1;
            }
            "--best-ocr-scale" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("Missing value after {}", arg))?;
                let scale: f32 = val
                    .trim()
                    .parse()
                    .map_err(|_| anyhow!("Invalid {} '{}'. Use a number >= 1.0", arg, val))?;
                if !(scale.is_finite() && scale >= 1.0) {
                    bail!("Invalid {} '{}'. Use a number >= 1.0", arg, val);
                }
                opts.slow_ocr_scale = Some(scale);
                i += 2;
            }
            "--no-cover" => {
                opts.no_cover = true;
                i += 1;
            }
            "--invert" => {
                opts.invert = true;
                i += 1;
            }
            "--symbol-mode" => {
                opts.jbig2_mode = Some(Jbig2Mode::Symbol);
                i += 1;
            }
            "--sym-unify" => {
                opts.jbig2_mode = Some(Jbig2Mode::SymUnify);
                i += 1;
            }
            "--unify" => {
                opts.jbig2_mode = Some(Jbig2Mode::SymUnify);
                i += 1;
            }
            "--halftone" => {
                opts.halftone = true;
                i += 1;
            }
            "--gray-jp2" => {
                opts.gray_jp2 = true;
                i += 1;
            }
            "--reflow" => {
                opts.reflow = true;
                i += 1;
            }
            "--deskew" => {
                opts.deskew = true;
                i += 1;
            }
            "--jpeg-compat" => {
                opts.jpeg_compat = true;
                i += 1;
            }
            "--high-quality" => {
                opts.high_quality = true;
                i += 1;
            }
            "--center-margins" => {
                opts.center_margins = true;
                i += 1;
            }
            "--crop-margins" => {
                opts.crop_margins = true;
                i += 1;
            }
            "--force-crop" => {
                opts.force_crop = true;
                i += 1;
            }
            "--image-only" => {
                opts.image_only = true;
                i += 1;
            }
            "--original-images" => {
                opts.original_images = true;
                i += 1;
            }
            "--fast-resize" => {
                opts.fast_resize = true;
                i += 1;
            }
            "--gui-worker" => {
                opts.gui_worker = true;
                i += 1;
            }
            "--probe-json" => {
                opts.probe_json = true;
                i += 1;
            }

            _ => {
                remaining.push(args[i].clone());
                i += 1;
            }
        }
    }

    Ok((remaining, opts))
}

fn parse_jbig2_mode_flag(raw: &str) -> Result<Jbig2Mode> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "generic" => Ok(Jbig2Mode::Generic),
        "symbol" => Ok(Jbig2Mode::Symbol),
        "sym-unify" | "sym_unify" | "symunify" => Ok(Jbig2Mode::SymUnify),
        _ => bail!(
            "Invalid --jbig2-mode '{}'. Use: generic, symbol, or sym-unify",
            raw
        ),
    }
}

fn apply_ocr_options(config: &mut PipelineConfig, cli_opts: &CliOptions) {
    if let Some(ocr_val) = cli_opts.ocr {
        config.set_enable_ocr(ocr_val);
        if !ocr_val {
            config.set_slow_ocr(false);
        }
    }

    if let Some(mode) = cli_opts.ocr_mode {
        match mode {
            OcrMode::Fast => {
                config.set_enable_ocr(true);
                config.set_slow_ocr(false);
            }
            OcrMode::Best => {
                config.set_slow_ocr(true);
            }
        }
    }
}

fn emit_worker_status_json(status: lege::progress::ProcessingStatus) {
    let update = ProgressUpdate::Status {
        task_id: 0,
        status,
        metrics: None,
    };
    match serde_json::to_string(&update) {
        Ok(json) => {
            println!("{json}");
            let _ = std::io::stdout().flush();
        }
        Err(err) => {
            eprintln!("[worker-json] serialization error: {err}");
        }
    }
}

fn emit_worker_error_json(error: impl std::fmt::Display) {
    let update = ProgressUpdate::Error {
        task_id: 0,
        error: error.to_string(),
        metrics: None,
    };
    match serde_json::to_string(&update) {
        Ok(json) => {
            println!("{json}");
            let _ = std::io::stdout().flush();
        }
        Err(err) => {
            eprintln!("[worker-json] serialization error: {err}");
        }
    }
}

fn main() -> Result<()> {
    let is_gui_worker = std::env::args().any(|arg| arg == "--gui-worker");
    match run_main() {
        Ok(()) => Ok(()),
        Err(err) if is_gui_worker => {
            emit_worker_error_json(format!("{err:#}"));
            fast_exit(1);
        }
        Err(err) => Err(err),
    }
}

fn run_main() -> Result<()> {
    // Configure dynamic runtime library paths early (before ORT/PDF pipelines initialize).
    lege::configure_runtime_env();

    #[cfg(target_os = "windows")]
    {
        if let Err(e) = lege::windows_dirs::ensure_directories() {
            eprintln!("Warning: failed to create user directories: {e}");
        }
    }

    let args: Vec<String> = std::env::args().collect();

    // Special help: environment variables
    // Version flag
    if args.iter().any(|arg| arg == "--version" || arg == "-v") {
        println!("{}", fmt1(&CLI_TEXT.main.version_line, display_version()));
        println!(
            "{}",
            fmt1(
                &CLI_TEXT.main.internal_version_line,
                version::internal_version()
            )
        );
        return Ok(());
    }

    if let Some(idx) = args.iter().position(|a| a == "--check-ocr-json") {
        let value = args
            .get(idx + 1)
            .ok_or_else(|| anyhow!("Missing path for --check-ocr-json"))?;
        let pdf_path = PathBuf::from(sanitize_path_arg(value));
        let has_ocr = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(check_pdf_ocr_layer(&pdf_path))
            .unwrap_or(false);
        println!("{}", serde_json::json!({ "has_ocr": has_ocr }));
        return Ok(());
    }

    if args.len() >= 3
        && (args[1] == "--help" || args[1] == "-h")
        && args[2].eq_ignore_ascii_case("env-variables")
    {
        print_env_variables_help();
        return Ok(());
    }

    if args.len() >= 3
        && (args[1] == "--help" || args[1] == "-h")
        && args[2].eq_ignore_ascii_case("debug")
    {
        print_debug_help();
        return Ok(());
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }

    // Licenses display mode
    if args.iter().any(|a| a == "--licenses") {
        print_licenses();
        return Ok(());
    }

    // Status / system info mode
    if args.iter().any(|a| a == "--status") {
        show_system_status()?;
        return Ok(());
    }

    if args
        .iter()
        .any(|a| a == "--targets" || a == "--list-targets")
    {
        print_target_profiles();
        return Ok(());
    }

    // Extract all --flag / --key value options, leaving only positional args.
    let (args, mut cli_opts) = extract_cli_options(args)?;

    if cli_opts.gui_worker {
        emit_worker_status_json(lege::progress::ProcessingStatus::Initializing);
    }

    // Machine-readable probe mode: print JSON metadata and exit.
    if cli_opts.probe_json {
        let path_str = args
            .get(1)
            .ok_or_else(|| anyhow!("--probe-json requires a path argument"))?;
        probe_json(PathBuf::from(sanitize_path_arg(path_str)))?;
        return Ok(());
    }

    // Optional override for resize backend to help diagnose GPU shader regressions.
    if cli_opts.fast_resize {
        lege::resize::set_resize_backend_preference(lege::resize::ResizeBackendPreference::FastCpu);
    } else {
        lege::resize::set_resize_backend_preference(lege::resize::ResizeBackendPreference::Auto);
    }

    // No extra positional args and no debug modes -> interactive CLI wizard
    if args.len() == 1
        && cli_opts.pdf_to_png.is_none()
        && cli_opts.jp2_debug.is_none()
        && !cli_opts.png_folder
        && cli_opts.crop_areas.is_none()
        && !cli_opts.pdf_to_images
        && !cli_opts.images_to_images
    {
        if cli_opts.language.is_some() {
            bail!("--language requires a direct CLI input file");
        }
        handle_cli_mode(AppConfig::default())?;
        return Ok(());
    }

    // ----- Debug / data-generation modes -----

    // PDF-to-PNG: lege <file.pdf> [page-range] --pdf-to-png HEIGHT
    if let Some(png_height) = cli_opts.pdf_to_png {
        let pdf_arg = args
            .get(1)
            .ok_or_else(|| anyhow!("Missing PDF path for --pdf-to-png"))?;
        let pdf_path = PathBuf::from(sanitize_path_arg(pdf_arg));
        validate_pdf_file(
            pdf_path
                .to_str()
                .ok_or_else(|| anyhow!("Invalid PDF path"))?,
        )?;
        let page_range = args.get(2).and_then(|c| {
            if !c.starts_with('-') && !c.to_lowercase().ends_with(".pdf") {
                Some(c.clone())
            } else {
                None
            }
        });
        return run_pdf_to_png_mode(
            pdf_path,
            page_range,
            png_height,
            AppConfig::default(),
            cli_opts.png_quantize,
            cli_opts.png_colors,
        );
    }

    // JP2 debug: lege <file.pdf> [page-range] --jp2-debug HEIGHT
    if let Some(jp2_height) = cli_opts.jp2_debug {
        let pdf_arg = args
            .get(1)
            .ok_or_else(|| anyhow!("Missing PDF path for --jp2-debug"))?;
        let pdf_path = PathBuf::from(sanitize_path_arg(pdf_arg));
        validate_pdf_file(
            pdf_path
                .to_str()
                .ok_or_else(|| anyhow!("Invalid PDF path"))?,
        )?;
        let page_range = args.get(2).and_then(|c| {
            if !c.starts_with('-') && !c.to_lowercase().ends_with(".pdf") {
                Some(c.clone())
            } else {
                None
            }
        });
        return lege::pdf_to_png::run_pdf_to_jp2_debug_mode(
            pdf_path,
            page_range,
            jp2_height,
            AppConfig::default(),
        );
    }

    // PNG-folder inference: lege <folder> --png-folder [--deskew]
    if cli_opts.png_folder {
        let folder_arg = args
            .get(1)
            .ok_or_else(|| anyhow!("Missing folder path for --png-folder"))?;
        let folder_path = PathBuf::from(sanitize_path_arg(folder_arg));
        let pipeline_config = build_png_folder_pipeline_config(&cli_opts)?;
        let output_path = resolve_png_folder_output_path(
            cli_opts.output_dir.clone(),
            &folder_path,
            &pipeline_config,
        )?;
        return run_png_mode_with_config(folder_path, Some(output_path), pipeline_config, None);
    }

    // Crop-areas debug: lege <file.pdf> [page-range] --crop-areas text|image|both [--out DIR] [--format png|jpg] [--deskew]
    if let Some(ref crop_mode) = cli_opts.crop_areas {
        let pdf_arg = args
            .get(1)
            .ok_or_else(|| anyhow!("Missing PDF path for --crop-areas"))?;
        let pdf_path = PathBuf::from(sanitize_path_arg(pdf_arg));
        validate_pdf_file(
            pdf_path
                .to_str()
                .ok_or_else(|| anyhow!("Invalid PDF path"))?,
        )?;
        let crop_kind = match crop_mode.as_str() {
            "text" => DebugCropKind::Text,
            "image" => DebugCropKind::Image,
            "both" | _ => DebugCropKind::Both,
        };
        let page_range = args.get(2).and_then(|c| {
            if !c.starts_with('-') && !c.to_lowercase().ends_with(".pdf") {
                Some(c.clone())
            } else {
                None
            }
        });
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(run_pdf_layout_crop_debug(
                pdf_path,
                cli_opts.output_dir,
                crop_kind,
                page_range,
                AppConfig::default(),
                cli_opts.deskew,
                cli_opts.debug_format.as_deref(),
                cli_opts.png_quantize,
                cli_opts.png_colors,
            ));
    }

    // PDF-to-Images: lege <file.pdf> [page-range] --pdf-to-images [--deskew] [--no-layout]
    // Or: lege --pdf-to-images <file.pdf> [page-range] [--deskew] [--no-layout]
    if cli_opts.pdf_to_images {
        // Find PDF path - could be args[1] or args[2] depending on order
        let pdf_arg = if args.len() > 1
            && !args[1].starts_with('-')
            && args[1].to_lowercase().ends_with(".pdf")
        {
            args[1].clone()
        } else if args.len() > 2
            && !args[2].starts_with('-')
            && args[2].to_lowercase().ends_with(".pdf")
        {
            args[2].clone()
        } else {
            return Err(anyhow!(
                "Missing PDF path for --pdf-to-images. Usage: lege <file.pdf> --pdf-to-images [page-range] [options...] or lege --pdf-to-images <file.pdf> [page-range] [options...]"
            ));
        };
        let pdf_path = PathBuf::from(sanitize_path_arg(&pdf_arg));
        validate_pdf_file(
            pdf_path
                .to_str()
                .ok_or_else(|| anyhow!("Invalid PDF path"))?,
        )?;

        // Find page range - check both positions, preferring the non-PDF one
        let page_range = if args.len() > 2
            && !args[2].starts_with('-')
            && !args[2].to_lowercase().ends_with(".pdf")
        {
            Some(args[2].clone())
        } else if args.len() > 1
            && !args[1].starts_with('-')
            && !args[1].to_lowercase().ends_with(".pdf")
        {
            Some(args[1].clone())
        } else {
            None
        };

        return run_pdf_to_images_mode(
            pdf_path,
            page_range,
            cli_opts.output_dir,
            AppConfig::default(),
            !cli_opts.no_layout,
            cli_opts.deskew,
            cli_opts.image_only,
            cli_opts.png_quantize,
            cli_opts.png_colors,
        );
    }

    // Images-to-Images: lege <folder> --images-to-images [--deskew] [--no-layout]
    if cli_opts.images_to_images {
        let folder_arg = args
            .get(1)
            .ok_or_else(|| anyhow!("Missing folder path for --images-to-images"))?;
        let folder_path = PathBuf::from(sanitize_path_arg(folder_arg));
        return run_images_to_images_mode(
            folder_path,
            cli_opts.output_dir,
            AppConfig::default(),
            !cli_opts.no_layout,
            cli_opts.deskew,
            cli_opts.image_only,
            cli_opts.png_quantize,
            cli_opts.png_colors,
        );
    }

    // ----- Normal processing mode -----

    if args.len() >= 2 {
        let mut positional: Vec<String> = args[1..].iter().map(|s| sanitize_path_arg(s)).collect();
        let mut page_range: Option<String> = None;
        let mut target_arg: Option<String> = None;
        let epub_command = pop_trailing_epub_command(&mut positional);
        if epub_command {
            if let Some(fmt) = cli_opts.text_format.as_deref()
                && fmt != "epub"
            {
                bail!(
                    "Trailing `epub` command cannot be combined with --text-format {}",
                    fmt
                );
            }
            cli_opts.text_format = Some("epub".to_string());
        }

        // Trailing target (height/profile) takes precedence over page range so numeric targets aren't misread.
        if let Some(last) = positional.last() {
            if !looks_like_input_arg(last) {
                if parse_target_argument(last).is_ok() {
                    target_arg = positional.pop();
                } else if looks_like_page_range(last) {
                    let candidate = positional.pop().unwrap();
                    let interpreted = interpret_page_range_arg(candidate.clone());
                    if let Some(ref range) = interpreted {
                        validate_page_range(range)?;
                    }
                    page_range = interpreted;
                } else {
                    bail!("Unrecognized argument: {}", last);
                }
            }
        }

        // After removing target, check for a remaining page range hint.
        if let Some(last) = positional.last() {
            if !looks_like_input_arg(last) {
                if looks_like_page_range(last) {
                    let candidate = positional.pop().unwrap();
                    let interpreted = interpret_page_range_arg(candidate.clone());
                    if let Some(ref range) = interpreted {
                        validate_page_range(range)?;
                    }
                    page_range = interpreted;
                } else {
                    bail!("Unrecognized argument: {}", last);
                }
            }
        }

        if positional.is_empty() {
            bail!("No input PDF files were provided.");
        }

        handle_simple_processing(
            positional,
            page_range,
            target_arg,
            cli_opts,
            AppConfig::default(),
        )?;
        return Ok(());
    }

    Ok(())
}

fn print_usage() {
    println!("{}", CLI_TEXT.main.usage_block);
    println!("{}", CLI_TEXT.main.ocr_mode_help_line);
}

fn pop_trailing_epub_command(positionals: &mut Vec<String>) -> bool {
    if positionals.len() >= 2
        && positionals
            .last()
            .is_some_and(|arg| arg.eq_ignore_ascii_case("epub"))
    {
        positionals.pop();
        true
    } else {
        false
    }
}

fn split_trailing_epub_command(input: &str) -> (String, bool) {
    let trimmed = input.trim();
    let Some(last) = trimmed.split_whitespace().last() else {
        return (String::new(), false);
    };
    if !last.eq_ignore_ascii_case("epub") {
        return (trimmed.to_string(), false);
    }
    let command_start = trimmed.len().saturating_sub(last.len());
    (trimmed[..command_start].trim_end().to_string(), true)
}

fn print_licenses() {
    let licenses_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("licenses.html");

    match std::fs::read_to_string(&licenses_path) {
        Ok(licenses) => println!("{}", licenses),
        Err(_) => println!("License information is bundled in docs/licenses.html."),
    }
}

fn print_debug_help() {
    println!("{}", CLI_TEXT.main.debug_help_block);
}

struct TargetSelection {
    width: Option<u32>,
    height: u32,
    profile_label: Option<&'static str>,
}

fn interpret_page_range_arg(arg: String) -> Option<String> {
    let trimmed = arg.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("all")
        || trimmed.eq_ignore_ascii_case("full")
        || trimmed == "*"
    {
        None
    } else {
        Some(arg)
    }
}

fn looks_like_page_range(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "all" || lower == "full" || lower == "*" {
        return true;
    }
    lower
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == ',' || ch == '-' || ch.is_ascii_whitespace())
}

fn looks_like_input_arg(input: &str) -> bool {
    let path = PathBuf::from(input);
    if path.exists() {
        return true;
    }

    let lower = input.to_ascii_lowercase();
    lower.ends_with(".pdf") || lower.ends_with(".zip")
}

fn parse_target_argument(spec: &str) -> Result<Option<TargetSelection>> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        bail!("Target specification cannot be empty");
    }

    if trimmed.eq_ignore_ascii_case("default")
        || trimmed.eq_ignore_ascii_case("proportional")
        || trimmed.eq_ignore_ascii_case("auto")
    {
        return Ok(None);
    }

    if let Some(profile) = target_profiles::find_profile(trimmed) {
        return Ok(Some(TargetSelection {
            width: Some(profile.width),
            height: profile.height,
            profile_label: Some(profile.name),
        }));
    }

    // Allow "height width" input for disproportionate custom sizing in interactive CLI,
    // e.g. "1600 1200" => height 1600, width 1200.
    let whitespace_parts: Vec<&str> = trimmed.split_whitespace().collect();
    if whitespace_parts.len() == 2 {
        let height: u32 = whitespace_parts[0].parse().map_err(|_| {
            anyhow!(
                "Invalid height in target specification: {}",
                whitespace_parts[0]
            )
        })?;
        let width: u32 = whitespace_parts[1].parse().map_err(|_| {
            anyhow!(
                "Invalid width in target specification: {}",
                whitespace_parts[1]
            )
        })?;
        if width == 0 || height == 0 {
            bail!("Target dimensions must be greater than zero");
        }
        return Ok(Some(TargetSelection {
            width: Some(width),
            height,
            profile_label: None,
        }));
    }

    let normalized = trimmed.replace('×', "x").replace('X', "x");
    if let Some((w_part, h_part)) = normalized.split_once('x') {
        let _width: u32 = w_part
            .trim()
            .parse()
            .map_err(|_| anyhow!("Invalid width in target specification: {}", w_part.trim()))?;
        let raw_width: u32 = w_part
            .trim()
            .parse()
            .map_err(|_| anyhow!("Invalid width in target specification: {}", w_part.trim()))?;
        let raw_height: u32 = h_part
            .trim()
            .parse()
            .map_err(|_| anyhow!("Invalid height in target specification: {}", h_part.trim()))?;
        if raw_width == 0 || raw_height == 0 {
            bail!("Target dimensions must be greater than zero");
        }
        let (width, height) = if raw_width > raw_height {
            (raw_height, raw_width)
        } else {
            (raw_width, raw_height)
        };
        return Ok(Some(TargetSelection {
            width: Some(width),
            height,
            profile_label: None,
        }));
    }

    if let Ok(height) = trimmed.parse::<u32>() {
        if height == 0 {
            bail!("Target height must be greater than zero");
        }
        return Ok(Some(TargetSelection {
            width: None,
            height,
            profile_label: None,
        }));
    }

    bail!("Unrecognized target specification: {}", spec);
}

fn slugify_profile_name(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        }
    }
    slug
}

fn print_target_profiles() {
    println!("{}", CLI_TEXT.main.target_profiles_header);
    for profile in target_profiles::TARGET_DEVICE_PROFILES {
        println!(
            "{}",
            fmt3(
                &CLI_TEXT.main.target_profile_item,
                profile.name,
                profile.width,
                profile.height
            )
        );
    }
    println!(
        "{}",
        fmt1(
            &CLI_TEXT.main.target_profile_proportional,
            target_profiles::PROPORTIONAL_OPTION_LABEL
        )
    );
    println!();
    println!("{}", CLI_TEXT.main.target_profiles_custom_note);
    println!("{}", CLI_TEXT.main.target_profiles_page_range_note);
}

fn handle_simple_processing(
    input_paths: Vec<String>,
    page_range: Option<String>,
    target_spec: Option<String>,
    cli_opts: CliOptions,
    config: AppConfig,
) -> Result<()> {
    if input_paths.is_empty() {
        bail!("No input files to process");
    }

    // Normalize and validate inputs
    let mut input_jobs = Vec::with_capacity(input_paths.len());
    let mut display_inputs = Vec::with_capacity(input_paths.len());
    let mut zip_tempdirs = Vec::new();
    for raw in input_paths {
        let cleaned = sanitize_path_arg(&raw);
        let (job, tempdir) = prepare_simple_input(&cleaned)?;
        display_inputs.push(cleaned);
        if let Some(tempdir) = tempdir {
            zip_tempdirs.push(tempdir);
        }
        input_jobs.push(job);
    }

    // Create baseline pipeline config for simple CLI mode
    let mut pipeline_config = PipelineConfig::simple_cli_defaults()
        .map_err(|e| anyhow!("Failed to construct CLI defaults: {}", e))?;

    // ----- Apply all CLI options to pipeline config -----

    // Language
    if let Some(ref language) = cli_opts.language {
        #[cfg(not(target_os = "linux"))]
        {
            bail!("--language is supported on Linux builds only");
        }
        #[cfg(target_os = "linux")]
        pipeline_config.set_ocr_language(language)?;
    }

    // Text format (ccitt4 | jbig2 | djvu | epub)
    if let Some(ref fmt) = cli_opts.text_format {
        pipeline_config.set_text_format(fmt)?;
    }

    // Cover / image format
    if let Some(ref fmt) = cli_opts.cover_format {
        let cover = match fmt.as_str() {
            "jpeg" => CoverFormat::Jpeg,
            "jp2" => CoverFormat::Jp2,
            "ccitt4" => CoverFormat::Ccitt4,
            "jbig2" => CoverFormat::Jbig2,
            "none" => CoverFormat::None,
            _ => bail!("Invalid cover format: {}", fmt),
        };
        pipeline_config.set_image_format(cover);
    }

    // Binarization
    if let Some(ref method) = cli_opts.binarization {
        // Build method string compatible with CliConfigBuilder::parse_binarization_method
        let mut method_str = match method.as_str() {
            "adaptive" => "1".to_string(),
            "fixed" => "2".to_string(),
            "heavy" => "3".to_string(),
            _ => method.clone(),
        };
        if let Some(k) = cli_opts.sauvola_k {
            method_str.push_str(&format!(" k={}", k));
        }
        if let Some(thr) = cli_opts.threshold {
            method_str.push_str(&format!(" thr={}", thr));
        }
        let bin_config = lege::CliConfigBuilder::parse_binarization_method(&method_str);
        pipeline_config.set_binarization(bin_config);
    } else {
        // Even without --binarization, allow standalone --threshold and --sauvola-k
        if cli_opts.threshold.is_some() || cli_opts.sauvola_k.is_some() {
            let mut method_str = "2".to_string(); // fixed threshold as default base
            if let Some(k) = cli_opts.sauvola_k {
                method_str = "1".to_string(); // adaptive if k is given
                method_str.push_str(&format!(" k={}", k));
            }
            if let Some(thr) = cli_opts.threshold {
                method_str.push_str(&format!(" thr={}", thr));
            }
            let bin_config = lege::CliConfigBuilder::parse_binarization_method(&method_str);
            pipeline_config.set_binarization(bin_config);
        }
    }

    // DjVu quality
    if let Some(q) = cli_opts.djvu_quality {
        pipeline_config.set_djvu_iw44_quality(q)?;
    }

    // Boolean toggles — image-region dithering only (requires layout detection; see pipeline).
    // `--halftone` wins over `--dither` (jbig2halftone.rs page encode vs symbol/generic JBIG2).
    // CCITT4 + `--dither` uses clustered-dot 4×4 only (never Stucki); JBIG2/DjVu use Stucki.
    if cli_opts.halftone {
        pipeline_config.set_image_region_dither_mode(ImageRegionDitherMode::Halftone);
    } else if cli_opts.gray_jp2 {
        pipeline_config.set_image_region_dither_mode(ImageRegionDitherMode::GrayJp2);
        pipeline_config.set_keep_original_images(false);
    } else if cli_opts.dither {
        let tf = pipeline_config.text_format();
        if tf == "ccitt4" {
            pipeline_config
                .set_image_region_dither_mode(ImageRegionDitherMode::Ccitt4ClusteredDot4x4);
        } else {
            pipeline_config.set_image_region_dither_mode(ImageRegionDitherMode::Stucki);
        }
    }

    // Image-region modes are format-specific: halftone is JBIG2-only.
    let tf = pipeline_config.text_format();
    if cli_opts.halftone && tf != "jbig2" {
        bail!(
            "--halftone requires --text-format jbig2 (got {}). Halftone segments are JBIG2-only.",
            tf
        );
    }

    if cli_opts.no_layout {
        pipeline_config.set_enable_layout_detection(false);
    }
    apply_ocr_options(&mut pipeline_config, &cli_opts);
    if let Some(scale) = cli_opts.slow_ocr_scale {
        pipeline_config.set_slow_ocr_scale(scale);
    }
    if cli_opts.no_cover {
        pipeline_config.set_no_cover_page(true);
    }
    if cli_opts.invert {
        pipeline_config.set_invert_input(true);
        // Invert disables layout detection (model doesn't support inverted docs)
        if pipeline_config.enable_layout_detection() {
            pipeline_config.set_enable_layout_detection(false);
        }
    }

    if pipeline_config.text_format() == "jbig2" {
        let selected_mode = cli_opts.jbig2_mode.clone().unwrap_or_else(|| {
            if pipeline_config.enable_layout_detection() {
                Jbig2Mode::Symbol
            } else {
                Jbig2Mode::Generic
            }
        });
        pipeline_config.set_jbig2_mode(selected_mode);
    }
    if cli_opts.deskew {
        pipeline_config.set_enable_deskew(true);
    }
    if cli_opts.reflow {
        if !pipeline_config.enable_layout_detection() {
            bail!("--reflow requires layout detection (do not combine with --no-layout/--invert)");
        }
        pipeline_config.set_enable_reflow(true);
    }
    if cli_opts.jpeg_compat {
        pipeline_config.set_jpeg_compat(true);
    }
    if cli_opts.high_quality {
        pipeline_config.set_high_quality_output(true);
    }
    // Default: keep raster crops as originals (PipelineConfig::new). Opt into re-encoding/dither paths.
    if cli_opts.halftone || cli_opts.dither {
        pipeline_config.set_keep_original_images(false);
    }
    if cli_opts.original_images || cli_opts.image_only {
        pipeline_config.set_keep_original_images(true);
    }

    // Margin processing (precedence: force-crop > crop > center)
    if cli_opts.force_crop {
        pipeline_config.set_margin_settings(lege::margin::MarginSettings::CropAndResize);
        pipeline_config.set_crop_footnotes(true);
    } else if cli_opts.crop_margins {
        pipeline_config.set_margin_settings(lege::margin::MarginSettings::CropAndResize);
    } else if cli_opts.center_margins {
        pipeline_config.set_margin_settings(lege::margin::MarginSettings::StandardizeAndCenter);
    }

    // ----- Target sizing -----

    let target_selection = target_spec
        .as_deref()
        .map(parse_target_argument)
        .transpose()?
        .flatten();

    let mut target_description = format!(
        "{}px height (proportional width)",
        pipeline_config.target_height()
    );

    if let Some(selection) = target_selection {
        if let Some(width) = selection.width {
            pipeline_config
                .set_target_dimensions(width, selection.height)
                .map_err(|e| anyhow!("Failed to set target dimensions: {}", e))?;
            pipeline_config
                .set_high_res_render_height(selection.height)
                .map_err(|e| anyhow!("Failed to set render height: {}", e))?;
            target_description = if let Some(label) = selection.profile_label {
                format!("{label} ({}x{} px)", width, selection.height)
            } else {
                format!("{}x{} px", width, selection.height)
            };
        } else {
            pipeline_config
                .set_target_height(selection.height)
                .map_err(|e| anyhow!("Failed to set target height: {}", e))?;
            pipeline_config
                .set_high_res_render_height(selection.height)
                .map_err(|e| anyhow!("Failed to set render height: {}", e))?;
            target_description = format!("{}px height (proportional width)", selection.height);
        }
    } else {
        pipeline_config
            .set_high_res_render_height(pipeline_config.target_height())
            .map_err(|e| anyhow!("Failed to set render height: {}", e))?;
    }

    // Set page range if provided
    if let Some(ref range) = page_range {
        validate_page_range(range)?;
        let page_range_obj = PageRange::parse(range)?;
        pipeline_config.set_page_range(Some(page_range_obj));
    }

    // Determine output directory
    let output_dir = determine_output_directory(cli_opts.output_dir, &display_inputs, &config)?;

    // Show batch summary (suppressed in --gui-worker mode; all human output goes to stderr)
    if !cli_opts.gui_worker {
        info_println!("\n{}", CLI_TEXT.main.simple_mode_header);
        info_println!(
            "{}",
            fmt1(&CLI_TEXT.main.simple_mode_files_queued, input_jobs.len())
        );
        for path in &input_jobs {
            info_println!(
                "{}",
                fmt1(
                    &CLI_TEXT.main.simple_mode_file_item,
                    path.display_path().display()
                )
            );
        }
        if let Some(ref range) = page_range {
            info_println!("{}", fmt1(&CLI_TEXT.main.simple_mode_page_range, range));
        }
        info_println!(
            "{}",
            fmt1(&CLI_TEXT.main.simple_mode_settings, &target_description)
        );
        info_println!(
            "{}",
            fmt1(
                &CLI_TEXT.main.simple_mode_output_directory,
                output_dir.display()
            )
        );
        info_println!("{}\n", CLI_TEXT.main.simple_mode_footer);
    }

    let total_files = input_jobs.len();
    let mut overall_ok = true;
    let gui_worker = cli_opts.gui_worker;

    for (index, input_job) in input_jobs.drain(..).enumerate() {
        let per_file_config = pipeline_config.clone();
        let file_path = input_job.display_path().to_path_buf();
        let output_path = generate_output_path(&file_path, &output_dir, &per_file_config)?;

        if !gui_worker {
            if total_files > 1 {
                info_println!(
                    "{}",
                    fmt3(
                        &CLI_TEXT.main.simple_mode_batch_item,
                        index + 1,
                        total_files,
                        file_path.display()
                    )
                );
                info_println!(
                    "{}",
                    fmt1(
                        &CLI_TEXT.main.simple_mode_batch_output,
                        output_path.display()
                    )
                );
            } else {
                info_println!(
                    "{}",
                    fmt1(&CLI_TEXT.main.simple_mode_input, file_path.display())
                );
                info_println!(
                    "{}",
                    fmt1(&CLI_TEXT.main.simple_mode_output, output_path.display())
                );
            }
        }

        let result = if gui_worker {
            process_input_job_json(input_job, output_path.clone(), per_file_config)
        } else {
            process_input_job(input_job, output_path.clone(), per_file_config)
        };

        match result {
            Ok(()) => {
                if !gui_worker && total_files > 1 {
                    let remaining = total_files - index - 1;
                    info_println!(
                        "{}",
                        fmt3(
                            &CLI_TEXT.main.simple_mode_batch_completed,
                            index + 1,
                            total_files,
                            remaining
                        )
                    );
                }
            }
            Err(error) => {
                overall_ok = false;
                if gui_worker {
                    emit_worker_error_json(format!("{error:#}"));
                    continue;
                }
                error_println!(
                    "{}",
                    fmt2(
                        &CLI_TEXT.main.simple_mode_error_processing,
                        file_path.display(),
                        error
                    )
                );
            }
        }
    }

    cleanup_cli_resources();
    drop(zip_tempdirs);
    if overall_ok {
        fast_exit(0);
    } else {
        fast_exit(1);
    }
}

fn print_env_variables_help() {
    println!("{}", CLI_TEXT.main.env_variables_help_block);
}

fn handle_cli_mode(config: AppConfig) -> Result<()> {
    match run_cli()? {
        Some((file_path, pipeline_config)) => {
            let output_dir = determine_output_directory(
                None,
                &[file_path.to_string_lossy().to_string()],
                &config,
            )?;
            let output_path = generate_output_path(&file_path, &output_dir, &pipeline_config)?;
            let (input_job, tempdir) = prepare_simple_input(
                file_path
                    .to_str()
                    .ok_or_else(|| anyhow!("Invalid input path"))?,
            )?;
            let result = process_input_job(input_job, output_path, pipeline_config);
            drop(tempdir);

            cleanup_cli_resources();
            match result {
                Ok(()) => fast_exit(0),
                Err(error) => {
                    error_println!("Processing failed: {:#}", error);
                    fast_exit(1);
                }
            }
        }
        None => Ok(()),
    }
}

fn run_cli() -> Result<Option<(PathBuf, PipelineConfig)>> {
    // Combined input: file path and processing options
    println!("{}", CLI_TEXT.app.main_title);
    println!("{}", CLI_TEXT.app.file_prompt);
    print!(
        "{}{} {}",
        cli_color(COLORS.prompt),
        CLI_TEXT.main.run_cli_input_prompt,
        cli_reset()
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.is_empty() {
        return Ok(None);
    }
    let (input, epub_command) = split_trailing_epub_command(input);
    let input = input.as_str();

    // Check for special modes
    if input.contains("--png-folder") {
        let enable_deskew = input.contains("--deskew");
        let folder_path_str = input
            .replace("--png-folder", "")
            .replace("--deskew", "")
            .trim()
            .to_string();
        let folder_path = parse_quoted_path(&folder_path_str);
        return handle_png_folder(PathBuf::from(folder_path), enable_deskew);
    }

    if input.contains("--pdf-to-png") {
        return handle_pdf_to_png(input);
    }

    if input.contains("--pdf-to-images") {
        return handle_pdf_to_images(input);
    }

    if input.contains("--images-to-images") {
        return handle_images_to_images(input);
    }

    // Parse file paths and page range
    let (files, page_range) = parse_file_paths_with_range(input);
    if files.is_empty() {
        return Ok(None);
    }

    let file_path = &files[0];
    validate_cli_input_path(file_path)?;

    if epub_command {
        let mut config = PipelineConfig::default();
        config.set_text_format("epub")?;
        config.set_image_format(CoverFormat::None);
        config.set_enable_layout_detection(true);
        if let Some(ref range) = page_range {
            validate_page_range(range)?;
            config.set_page_range(Some(PageRange::parse(range)?));
        }
        return Ok(Some((PathBuf::from(file_path), config)));
    }

    // Check for OCR layer in the PDF right after validation
    if PathBuf::from(file_path)
        .extension()
        .and_then(|s| s.to_str())
        == Some("pdf")
    {
        let has_ocr = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(check_pdf_ocr_layer(&PathBuf::from(file_path)));

        match has_ocr {
            Ok(true) => {
                println!(
                    "\n{}{}{}",
                    cli_color(COLORS.info),
                    CLI_TEXT.interactive.messages.ocr_detected,
                    cli_reset()
                );
                println!("{}", CLI_TEXT.interactive.messages.ocr_detected_msg);
                println!("{}\n", CLI_TEXT.interactive.messages.ocr_detected_tip);
            }
            Ok(false) => {
                println!(
                    "\n{}{}{}",
                    cli_color(COLORS.highlight),
                    CLI_TEXT.interactive.messages.no_ocr_detected,
                    cli_reset()
                );
                println!("{}\n", CLI_TEXT.interactive.messages.no_ocr_detected_msg);
            }
            Err(e) => {
                eprintln!(
                    "{}{}{}",
                    cli_color(COLORS.highlight),
                    fmt1(&CLI_TEXT.main.ocr_check_warning, e),
                    cli_reset()
                );
            }
        }
    }

    // Processing options selection
    let (
        text_format,
        cover_format,
        final_enable_dithering,
        layout_detection_enabled,
        ocr_enabled,
        ocr_mode,
        original_image,
        no_cover_page,
        no_binarization,
        invert_input,
        jbig2_mode,
        center_margins,
        crop_margins,
        force_crop,
        deskew_enabled,
        high_quality_output,
        interactive_halftone,
        djvu_quality,
        enable_reflow,
    ) = loop {
        print!(
            "\n{}{}{}\n",
            cli_color(COLORS.info),
            CLI_TEXT.interactive.processing_options_title,
            cli_reset()
        );
        println!(
            "{}{}{}",
            cli_color(COLORS.prompt),
            CLI_TEXT.interactive.format_prompt,
            cli_reset()
        );
        println!(
            "{}{}{}",
            cli_color(COLORS.prompt),
            CLI_TEXT.interactive.modifiers_prompt,
            cli_reset()
        );
        println!(
            "{}{}{}",
            cli_color(COLORS.highlight),
            CLI_TEXT.interactive.examples_prompt,
            cli_reset()
        );
        println!(
            "{}{}{}",
            cli_color(COLORS.info),
            CLI_TEXT.interactive.default_prompt,
            cli_reset()
        );
        print!(
            "{}{} {}",
            cli_color(COLORS.prompt),
            CLI_TEXT.main.run_cli_input_prompt,
            cli_reset()
        );
        io::stdout().flush()?;

        let mut format_input = String::new();
        io::stdin().read_line(&mut format_input)?;
        let format_input = format_input.trim();

        match parse_format_selection_with_options(format_input) {
            Ok((
                text_format,
                cover_format,
                enable_dithering,
                layout_detection_enabled,
                ocr_enabled,
                ocr_mode,
                original_image,
                no_cover_page,
                no_binarization,
                invert_input,
                jbig2_mode,
                center_margins,
                crop_margins,
                force_crop,
                deskew_enabled,
                high_quality_output,
                interactive_halftone,
                djvu_quality,
                enable_reflow,
            )) => {
                // No immediate rejection; we'll apply precedence rules below when building config
                break (
                    text_format,
                    cover_format,
                    enable_dithering,
                    layout_detection_enabled,
                    ocr_enabled,
                    ocr_mode,
                    original_image,
                    no_cover_page,
                    no_binarization,
                    invert_input,
                    jbig2_mode,
                    center_margins,
                    crop_margins,
                    force_crop,
                    deskew_enabled,
                    high_quality_output,
                    interactive_halftone,
                    djvu_quality,
                    enable_reflow,
                );
            }
            Err(e) => {
                println!("{}", fmt1(&CLI_TEXT.main.processing_options_retry, e));
                continue;
            }
        }
    };

    // Step 3: Binarization method
    // When layout detection is disabled, adaptive and heavy binarization remain
    // selectable; the prompt notes that both benefit from layout detection.
    let binarization_method = if layout_detection_enabled {
        println!(
            "\n{}{}{}",
            cli_color(COLORS.info),
            CLI_TEXT.interactive.binarization_title,
            cli_reset()
        );
        println!(
            "{}",
            fmt3(
                &CLI_TEXT.main.binarization_choices_template,
                &CLI_TEXT.interactive.binarization_methods[0],
                &CLI_TEXT.interactive.binarization_methods[1],
                &CLI_TEXT.interactive.binarization_methods[2]
            )
        );
        println!(
            "{}{}{}",
            cli_color(COLORS.highlight),
            CLI_TEXT.interactive.binarization_advanced,
            cli_reset()
        );
        print!(
            "{}{} {} ",
            cli_color(COLORS.prompt),
            CLI_TEXT.interactive.binarization_prompt,
            cli_reset()
        );
        io::stdout().flush()?;

        let mut binarization_input = String::new();
        io::stdin().read_line(&mut binarization_input)?;
        let binarization_input = binarization_input.trim();

        parse_binarization_method(binarization_input)?
    } else {
        println!(
            "\n{}{}{}",
            cli_color(COLORS.info),
            CLI_TEXT.interactive.binarization_title,
            cli_reset()
        );
        println!(
            "{}{}{}",
            cli_color(COLORS.highlight),
            CLI_TEXT.interactive.binarization_no_layout_note,
            cli_reset()
        );
        println!(
            "{}{}{}",
            cli_color(COLORS.info),
            fmt3(
                &CLI_TEXT.main.binarization_choices_template,
                &CLI_TEXT.interactive.binarization_methods[0],
                &CLI_TEXT.interactive.binarization_methods[1],
                &CLI_TEXT.interactive.binarization_methods[2]
            ),
            cli_reset()
        );
        println!(
            "{}{}{}",
            cli_color(COLORS.highlight),
            CLI_TEXT.interactive.binarization_advanced,
            cli_reset()
        );
        print!(
            "{}{} {} ",
            cli_color(COLORS.prompt),
            CLI_TEXT.interactive.binarization_prompt,
            cli_reset()
        );
        io::stdout().flush()?;

        let mut binarization_input = String::new();
        io::stdin().read_line(&mut binarization_input)?;
        let raw = binarization_input.trim();
        if raw.is_empty() {
            "1".to_string()
        } else if raw.chars().all(|c| c.is_ascii_digit()) {
            match raw {
                "1" | "2" | "3" => raw.to_string(),
                _ => format!("2 thr={}", raw),
            }
        } else {
            parse_binarization_method(raw).unwrap_or_else(|_| "2".to_string())
        }
    };

    // Apply precedence rules before constructing the final config

    // 1) Margin processing precedence: force-crop ('f') > crop ('w') > center ('m')
    let (effective_center_margins, effective_crop_margins, effective_force_crop) = if force_crop {
        // Force crop overrides everything
        if crop_margins || center_margins {
            println!("{}", CLI_TEXT.main.precedence_force_crop_note);
        }
        (false, true, true)
    } else if crop_margins {
        // Crop overrides center
        if center_margins {
            println!("{}", CLI_TEXT.main.precedence_crop_over_center_note);
        }
        (false, true, false)
    } else {
        (center_margins, false, false)
    };

    // 2) Layout detection is enabled by default, disabled by 'a' flag
    // When disabled, margin processing uses pixel-based analysis
    let effective_layout_detection = layout_detection_enabled;
    if (effective_center_margins || effective_crop_margins) && !layout_detection_enabled {
        println!("{}", CLI_TEXT.main.margin_without_layout_note);
        if effective_crop_margins && !effective_force_crop {
            println!("{}", CLI_TEXT.main.footnote_override_note);
        }
    }

    // 3) Dithering logic: 'c' flag enables dithering, otherwise original images
    let effective_enable_dithering = final_enable_dithering;

    // Create pipeline config with selected options (after precedence adjustments)
    let mut config = PipelineConfig::default();

    // Set the configuration using the public setters
    if let Err(e) = config.set_text_format(&text_format) {
        error_println!("{}", fmt1(&CLI_TEXT.main.config_set_text_format_failed, e));
    }
    // Set DjVu quality if specified
    if let Some(quality) = djvu_quality {
        if let Err(e) = config.set_djvu_iw44_quality(quality) {
            error_println!("{}", fmt1(&CLI_TEXT.main.config_set_djvu_quality_failed, e));
        }
    }
    config.set_high_quality_output(high_quality_output);
    // Use unified image format setter for non-binarized images
    config.set_image_format(cover_format);
    if interactive_halftone {
        config.set_image_region_dither_mode(ImageRegionDitherMode::Halftone);
    } else if effective_enable_dithering {
        config.set_dither_images(true);
    } else {
        config.set_dither_images(false);
    }
    config.set_enable_layout_detection(effective_layout_detection);
    config.set_enable_reflow(enable_reflow);
    config.set_enable_ocr(ocr_enabled);
    if ocr_enabled {
        match ocr_mode {
            OcrMode::Fast => config.set_slow_ocr(false),
            OcrMode::Best => config.set_slow_ocr(true),
        }
    }
    config.set_no_cover_page(no_cover_page);
    config.set_invert_input(invert_input);
    config.set_enable_deskew(deskew_enabled);
    config.set_jbig2_mode(jbig2_mode);
    config.set_keep_original_images(original_image);

    // Set margin processing settings
    let margin_settings = if effective_center_margins {
        lege::margin::MarginSettings::StandardizeAndCenter
    } else if effective_crop_margins {
        lege::margin::MarginSettings::CropAndResize
    } else {
        lege::margin::MarginSettings::None
    };
    config.set_margin_settings(margin_settings);
    // Force crop setting controls skipping footnote override logic in margin analysis
    config.set_crop_footnotes(effective_force_crop);

    let target_summary = prompt_target_device(&mut config)?;

    // Log the selected options
    println!(
        "\n{}{}{}",
        cli_color(COLORS.info),
        CLI_TEXT.interactive.selected_options_title,
        cli_reset()
    );
    let text_format_display = if config.text_format() == "jbig2" {
        format!("jbig2 ({})", config.jbig2_mode().as_str())
    } else {
        config.text_format().to_string()
    };
    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_text_encoding,
        cli_reset(),
        text_format_display
    );
    println!(
        "{}{}:{} {:?}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_image_format,
        cli_reset(),
        config.image_format()
    );
    println!(
        "{}{}:{} {:?}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_dithering,
        cli_reset(),
        config.image_region_dither_mode()
    );
    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_original_quality,
        cli_reset(),
        config.keep_original_images()
    );
    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_high_quality_output,
        cli_reset(),
        config.high_quality_output()
    );
    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_target_output,
        cli_reset(),
        target_summary
    );

    let original_layout_detection = config.enable_layout_detection();
    if config.invert_input() && original_layout_detection {
        println!(
            "{}{}:{} {} (disabled - inverted documents not supported)",
            cli_color(COLORS.info),
            CLI_TEXT.main.selected_options_layout_detection,
            cli_reset(),
            original_layout_detection
        );
        println!(
            "{}{}:{} {}",
            cli_color(COLORS.highlight),
            CLI_TEXT.main.selected_options_note,
            cli_reset(),
            CLI_TEXT.interactive.messages.layout_disabled_inverted
        );
        println!(
            "{}{}:{} {}",
            cli_color(COLORS.highlight),
            CLI_TEXT.main.selected_options_reason,
            cli_reset(),
            CLI_TEXT.interactive.messages.layout_disabled_reason
        );
        config.set_enable_layout_detection(false);
    } else {
        println!(
            "{}{}:{} {}",
            cli_color(COLORS.info),
            CLI_TEXT.main.selected_options_layout_detection,
            cli_reset(),
            config.enable_layout_detection()
        );
    }

    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_ocr_enabled,
        cli_reset(),
        config.enable_ocr()
    );
    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_ocr_mode,
        cli_reset(),
        if config.slow_ocr_enabled() {
            OcrMode::Best.as_str()
        } else {
            OcrMode::Fast.as_str()
        }
    );
    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_no_cover_page,
        cli_reset(),
        config.no_cover_page()
    );
    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_invert_input,
        cli_reset(),
        config.invert_input()
    );
    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_deskew_enabled,
        cli_reset(),
        config.enable_deskew()
    );
    println!(
        "{}{}:{} {:?}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_margin_processing,
        cli_reset(),
        config.margin_settings()
    );
    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_force_crop,
        cli_reset(),
        config.crop_footnotes()
    );
    println!(
        "{}{}:{} {}",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_max_retries,
        cli_reset(),
        config.max_retries()
    );
    println!(
        "{}{}:{} {}ms",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_retry_delay,
        cli_reset(),
        config.retry_delay_ms()
    );
    println!(
        "{}{}{}\n",
        cli_color(COLORS.info),
        CLI_TEXT.main.selected_options_footer,
        cli_reset()
    );

    // Set binarization method (inversion no longer changes binarization selection)
    if !no_binarization {
        use lege::CliConfigBuilder;
        let binarization_config = CliConfigBuilder::parse_binarization_method(&binarization_method);
        config.set_binarization(binarization_config);
    }

    if let Some(range) = page_range {
        let page_range = PageRange::parse(&range)?;
        config.set_page_range(Some(page_range));
    }

    Ok(Some((PathBuf::from(file_path), config)))
}

enum SimpleInput {
    Pdf(PathBuf),
    ImageFolder { source: PathBuf, display: PathBuf },
}

impl SimpleInput {
    fn display_path(&self) -> &Path {
        match self {
            SimpleInput::Pdf(path) => path,
            SimpleInput::ImageFolder { display, .. } => display,
        }
    }
}

fn process_input_job(
    input: SimpleInput,
    output_path: PathBuf,
    pipeline_config: PipelineConfig,
) -> Result<()> {
    match input {
        SimpleInput::Pdf(path) => process_single_file(path, output_path, pipeline_config),
        SimpleInput::ImageFolder { source, .. } => {
            run_png_mode_with_config(source, Some(output_path), pipeline_config, None)
        }
    }
}

/// Like process_input_job but emits newline-delimited JSON to stdout instead of human text.
fn process_input_job_json(
    input: SimpleInput,
    output_path: PathBuf,
    pipeline_config: PipelineConfig,
) -> Result<()> {
    use lege::progress;

    let manager = progress::get_progress_manager();
    let receiver = manager.subscribe();

    match input {
        SimpleInput::Pdf(path) => {
            let task_id = progress::spawn_file_processing_task(path, output_path, pipeline_config);
            worker_json::run_json_worker(&receiver, task_id);
        }
        SimpleInput::ImageFolder { source, .. } => {
            let tracker = manager.create_tracker();
            let task_id = tracker.task_id();
            let tracker_for_thread = tracker.clone();
            std::thread::spawn(move || {
                match run_png_mode_with_config(
                    source,
                    Some(output_path),
                    pipeline_config,
                    Some(tracker_for_thread.clone()),
                ) {
                    Ok(()) => tracker_for_thread.finish("Image folder processing completed"),
                    Err(e) => tracker_for_thread.finish_with_error(e),
                }
            });
            worker_json::run_json_worker(&receiver, task_id);
        }
    }
    Ok(())
}

fn prepare_simple_input(path: &str) -> Result<(SimpleInput, Option<tempfile::TempDir>)> {
    let path_buf = PathBuf::from(path);
    if !path_buf.exists() {
        bail!("Input not found: {}", path);
    }

    if path_buf.is_dir() {
        return Ok((
            SimpleInput::ImageFolder {
                source: path_buf.clone(),
                display: path_buf,
            },
            None,
        ));
    }

    if !path_buf.is_file() {
        bail!("Path is neither a file nor a directory: {}", path);
    }

    let ext = path_buf
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "pdf" => {
            validate_pdf_file(path)?;
            Ok((SimpleInput::Pdf(path_buf), None))
        }
        "zip" => {
            let tempdir = extract_zip_to_tempdir(&path_buf)?;
            Ok((
                SimpleInput::ImageFolder {
                    source: tempdir.path().to_path_buf(),
                    display: path_buf,
                },
                Some(tempdir),
            ))
        }
        _ => bail!("Input must be a PDF, ZIP, or image folder: {}", path),
    }
}

fn validate_cli_input_path(path: &str) -> Result<()> {
    let path_buf = PathBuf::from(path);
    if !path_buf.exists() {
        bail!("Input not found: {}", path);
    }
    if path_buf.is_dir() {
        return Ok(());
    }
    let ext = path_buf
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => validate_pdf_file(path),
        "zip" => Ok(()),
        _ => bail!("Input must be a PDF, ZIP, or image folder: {}", path),
    }
}

fn extract_zip_to_tempdir(zip_path: &Path) -> Result<tempfile::TempDir> {
    let file = fs::File::open(zip_path)
        .map_err(|e| anyhow!("Failed to open ZIP {}: {}", zip_path.display(), e))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| anyhow!("Failed to read ZIP {}: {}", zip_path.display(), e))?;
    let tempdir = tempfile::tempdir()?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(enclosed_name) = entry.enclosed_name() else {
            continue;
        };
        let out_path = tempdir.path().join(enclosed_name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out_file = fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out_file)?;
    }

    Ok(tempdir)
}

fn parse_format_selection_with_options(
    input: &str,
) -> Result<(
    String,
    CoverFormat,
    bool,
    bool,
    bool,
    OcrMode,
    bool,
    bool,
    bool,
    bool,
    Jbig2Mode,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,       // halftone (--halftone, JBIG2 halftone segments)
    Option<u8>, // djvu_quality (None for non-DjVu formats, Some for DjVu)
    bool,       // enable_reflow (raster reflow; requires layout detection)
)> {
    if input.is_empty() {
        // Default: Original images with CCITT4 text, JPEG cover, layout detection ENABLED
        // Dithering is OFF by default (quality-first default)
        return Ok((
            "ccitt4".to_string(),
            CoverFormat::Jpeg,
            false, // enable_dithering
            true,  // layout_detection enabled by default
            false, // ocr_enabled
            OcrMode::Fast,
            true,  // original_image (no dithering)
            false, // no_cover_page
            false, // no_binarization
            false, // invert_input
            Jbig2Mode::Symbol,
            false, // center_margins
            false, // crop_margins
            false, // force_crop
            false, // deskew_enabled
            false, // high_quality_output
            false, // halftone
            None,  // djvu_quality (not DjVu)
            false, // enable_reflow
        ));
    }

    let parts: Vec<&str> = input.split_whitespace().collect();
    let main_part = parts[0];

    // Standalone flags (same as non-interactive CLI; filtered out before letter-option parsing)
    let has_high_flag = parts
        .iter()
        .any(|&p| p == "--high" || p == "--high-quality");
    let has_unify_flag = parts.iter().any(|&p| p == "--unify");
    let has_halftone_flag = parts.iter().any(|&p| p == "--halftone");

    // Parse main format option
    let (format_num, c_count, has_s_flag, has_u_flag) = parse_main_format(main_part)?;

    if has_halftone_flag && format_num != 2 {
        return Err(anyhow!(
            "--halftone is only valid with format 2 (JBIG2). Choose `2` or `2 --halftone`, not format {}.",
            format_num
        ));
    }

    // Map the numbered menu to text format
    // 1: CCITT4, 2: JBIG2, 3: DJVU
    let text_format = match format_num {
        1 => "ccitt4".to_string(),
        2 => "jbig2".to_string(),
        3 => "djvu".to_string(),
        _ => {
            return Err(anyhow!(
                "Invalid text encoding format. Only 1 (CCITT4), 2 (JBIG2), or 3 (DJVU) are supported."
            ));
        }
    };

    // Determine cover format
    let cover_format = if text_format == "djvu" {
        // Ignore any explicit cover token; no separate cover for DjVu
        CoverFormat::None
    } else if parts.len() > 1 {
        match parts[1] {
            "jpeg" => CoverFormat::Jpeg,
            "none" => CoverFormat::None,
            _ => CoverFormat::Jpeg,
        }
    } else {
        CoverFormat::Jpeg
    };

    // Determine dithering:
    // ALL formats default to original images (no dithering)
    // 'c' flag ENABLES image-region dithering (JBIG2/DjVu: Stucki; CCITT4: clustered-dot 4×4 only).
    let enable_dithering = c_count > 0;

    // Parse additional options from the remaining parts
    // Also extract option letters that might be embedded in the first part (e.g., "1a" means format 1 with option 'a')
    let mut options_parts = Vec::new();

    // Extract option letters from the first part (after the digit)
    let first_part_options: String = main_part.chars().filter(|c| !c.is_ascii_digit()).collect();
    if !first_part_options.is_empty() {
        // Split individual option letters
        for ch in first_part_options.chars() {
            options_parts.push(ch.to_string());
        }
    }

    // Add remaining parts as options
    let options_start_index = if parts.len() > 1 && matches!(parts[1], "jpeg" | "none") {
        2 // Skip the cover format
    } else {
        1 // Include everything after the format number
    };

    let remaining_parts: Vec<&str> = parts
        .iter()
        .skip(options_start_index)
        .copied()
        .filter(|&p| !matches!(p, "--high" | "--high-quality" | "--unify" | "--halftone"))
        .collect();
    options_parts.extend(remaining_parts.iter().map(|s| s.to_string()));

    let options_str = options_parts.join(" ");
    let (
        layout_detection,
        ocr_enabled,
        ocr_mode,
        mut original_image,
        no_cover_page,
        no_binarization,
        invert_input,
        deskew_enabled,
        center_margins,
        crop_margins,
        force_crop,
        enable_reflow,
    ) = parse_options(&options_str)?;

    // Dithered / halftone figure regions require re-encoding (letters may not be in options_str).
    if enable_dithering || has_halftone_flag {
        original_image = false;
    }

    let final_enable_dithering = enable_dithering;

    let jbig2_mode = if format_num == 2 {
        if has_u_flag || has_unify_flag {
            Jbig2Mode::SymUnify
        } else if has_s_flag || layout_detection {
            Jbig2Mode::Symbol
        } else {
            Jbig2Mode::Generic
        }
    } else {
        Jbig2Mode::Generic
    };

    // Determine DjVu quality based on --high flag
    let djvu_quality = if format_num == 3 {
        // DjVu format
        if has_high_flag {
            Some(100) // High quality mode (97 slices)
        } else {
            Some(75) // Default quality (85 slices)
        }
    } else {
        None // Not DjVu format
    };

    // Note: We do not offer "Original+JBIG2" because JBIG2's advantage is its superior dithering.
    // Using JBIG2 without dithering negates file size benefits and offers no other clear advantage over CCITT4.
    Ok((
        text_format,
        cover_format,
        final_enable_dithering,
        layout_detection,
        ocr_enabled,
        ocr_mode,
        original_image,
        no_cover_page,
        no_binarization,
        invert_input,
        jbig2_mode,
        center_margins,
        crop_margins,
        force_crop,
        deskew_enabled,
        has_high_flag,
        has_halftone_flag,
        djvu_quality,
        enable_reflow,
    ))
}

fn parse_main_format(input: &str) -> Result<(u32, usize, bool, bool)> {
    let c_count = input.chars().filter(|&c| c == 'c').count(); // 'c' enables dithering
    let has_s_flag = input.contains('s'); // 's' enables symbol mode for JBIG2
    let has_u_flag = input.contains('u'); // 'u' enables sym-unify for JBIG2

    // Extract the numeric part
    let numeric_part: String = input.chars().filter(|c| c.is_ascii_digit()).collect();
    if numeric_part.is_empty() {
        return Err(anyhow!("No format number found"));
    }

    let format_num: u32 = numeric_part.parse()?;
    if format_num < 1 || format_num > 3 {
        return Err(anyhow!(
            "Format number must be 1 (CCITT4), 2 (JBIG2), or 3 (DJVU)"
        ));
    }

    // JBIG2-specific modifiers only work with format 2.
    if (has_s_flag || has_u_flag) && format_num != 2 {
        return Err(anyhow!(
            "JBIG2 mode flags ('s' for symbol, 'u' for sym-unify) are only available with JBIG2 format (2)"
        ));
    }

    Ok((format_num, c_count, has_s_flag, has_u_flag))
}

fn parse_options(
    input: &str,
) -> Result<(
    bool,
    bool,
    OcrMode,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
)> {
    let options: Vec<&str> = input.split_whitespace().collect();

    // 'a' now DISABLES layout detection (it's enabled by default)
    let layout_detection = !options.contains(&"a");
    let mut ocr_enabled = false;
    let mut ocr_mode = OcrMode::Fast;
    for option in &options {
        match option.to_ascii_lowercase().as_str() {
            "o1" => {
                ocr_enabled = true;
                ocr_mode = OcrMode::Fast;
            }
            "o2" => {
                ocr_enabled = true;
                ocr_mode = OcrMode::Best;
            }
            other if other.starts_with('o') && other.len() > 1 => {
                bail!(
                    "Invalid OCR option '{}'. Use o1 for fast OCR or o2 for best OCR",
                    option
                );
            }
            _ => {}
        }
    }
    // 'c' now selects DITHERED images (quality-vs-size toggle). Original is default.
    let original_image = !options.contains(&"c");
    let no_cover_page = options.contains(&"d");
    let no_binarization = options.contains(&"i"); // 'i' for image-only (no binarization)
    let invert_input = options.contains(&"g");
    let deskew_enabled = options.contains(&"h");
    let center_margins = options.contains(&"m");
    let crop_margins = options.contains(&"w");
    let force_crop = options.contains(&"f");
    let raster_reflow = options.contains(&"r");
    if raster_reflow && !layout_detection {
        bail!("Raster reflow ('r') requires layout detection. Remove the 'a' flag to use 'r'.");
    }

    Ok((
        layout_detection,
        ocr_enabled,
        ocr_mode,
        original_image,
        no_cover_page,
        no_binarization,
        invert_input,
        deskew_enabled,
        center_margins,
        crop_margins,
        force_crop,
        raster_reflow,
    ))
}

#[cfg(test)]
mod cli_parser_tests {
    use super::*;

    #[test]
    fn interactive_format_4_is_not_epub() {
        let err = match parse_format_selection_with_options("4") {
            Ok(_) => panic!("format 4 should not parse"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("Format number must be"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn trailing_epub_command_is_removed_from_positionals() {
        let mut positional = vec![
            "book.pdf".to_string(),
            "1-10".to_string(),
            "epub".to_string(),
        ];

        assert!(pop_trailing_epub_command(&mut positional));
        assert_eq!(positional, vec!["book.pdf".to_string(), "1-10".to_string()]);
    }

    #[test]
    fn initial_epub_command_strips_only_final_token() {
        let (input, is_epub) = split_trailing_epub_command("\"book path.pdf\" 1-10 epub");

        assert!(is_epub);
        assert_eq!(input, "\"book path.pdf\" 1-10");
    }
}

fn parse_binarization_method(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok("1".to_string());
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();

    let choice = match parts[0].to_lowercase().as_str() {
        "1" | "adaptive" | "sauvola" | "otsu" => "1",
        "2" | "fixed" | "threshold" | "thr" => "2",
        "3" | "heavy" | "sauvola_ai" | "onnx" => "3",
        other => {
            return Err(anyhow!(
                "Invalid choice: {}. Use 1 (Adaptive), 2 (Fixed threshold), or 3 (Heavy Sauvola)",
                other
            ));
        }
    };

    let mut result = choice.to_string();
    let mut positional_threshold_consumed = false;
    for part in &parts[1..] {
        if let Some(k_str) = part.strip_prefix("k=") {
            let k_factor: f32 = k_str
                .parse()
                .map_err(|_| anyhow!("Invalid k= parameter: {}", k_str))?;
            if !(0.0..=1.0).contains(&k_factor) {
                return Err(anyhow!(
                    "k factor must be between 0.0 and 1.0, got: {}",
                    k_factor
                ));
            }
            result.push_str(&format!(" k={}", k_factor));
        } else if let Some(thr_str) = part.strip_prefix("thr=") {
            let threshold: u8 = thr_str
                .parse()
                .map_err(|_| anyhow!("Invalid thr= parameter: {}", thr_str))?;
            result.push_str(&format!(" thr={}", threshold));
            positional_threshold_consumed = true;
        } else if choice == "2" && !positional_threshold_consumed {
            let threshold: u8 = part
                .parse()
                .map_err(|_| anyhow!("Invalid fixed threshold '{}'. Expected 0-255.", part))?;
            result.push_str(&format!(" thr={}", threshold));
            positional_threshold_consumed = true;
        } else if !part.is_empty() {
            return Err(anyhow!(
                "Unrecognized parameter '{}'. Use k=<value>, thr=<value>, or for fixed mode: '2 <0-255>'.",
                part
            ));
        }
    }

    Ok(result)
}

fn parse_file_paths_with_range(input: &str) -> (Vec<String>, Option<String>) {
    // Split by space to separate files from page range
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.is_empty() {
        return (Vec::new(), None);
    }

    // Check if last part looks like a page range (contains numbers and dashes/commas)
    let mut page_range = None;
    let mut file_parts = parts.clone();

    if let Some(last_part) = parts.last() {
        // Simple heuristic: if it contains digits and page range characters, treat as page range
        if last_part.chars().any(|c| c.is_ascii_digit())
            && (last_part.contains('-')
                || last_part.contains(',')
                || last_part.chars().all(|c| c.is_ascii_digit()))
        {
            // Validate as page range
            if validate_page_range(last_part).is_ok() {
                page_range = Some(last_part.to_string());
                file_parts.pop(); // Remove page range from file parts
            }
        }
    }

    // Parse remaining parts as file paths
    let file_input = file_parts.join(" ");
    let files = parse_file_paths(&file_input);

    (files, page_range)
}

fn parse_file_paths(input: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut current_file = String::new();
    let mut quote_char: Option<char> = None;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' | '\'' => {
                if let Some(active) = quote_char {
                    if active == ch {
                        quote_char = None;
                    } else {
                        current_file.push(ch);
                    }
                } else {
                    quote_char = Some(ch);
                }
            }
            '\\' => {
                if let Some(next) = chars.peek().copied() {
                    match next {
                        ' ' | '\t' => {
                            current_file.push(' ');
                            chars.next();
                        }
                        '"' | '\'' => {
                            current_file.push(next);
                            chars.next();
                        }
                        '\\' => {
                            current_file.push('\\');
                            chars.next();
                        }
                        _ => {
                            current_file.push('\\');
                        }
                    }
                } else {
                    current_file.push('\\');
                }
            }
            ' ' | '\t' => {
                if quote_char.is_some() {
                    current_file.push(ch);
                } else if !current_file.is_empty() {
                    files.push(current_file.trim().to_string());
                    current_file.clear();
                }
            }
            _ => {
                current_file.push(ch);
            }
        }
    }

    if !current_file.is_empty() {
        files.push(current_file.trim().to_string());
    }

    files
}

fn sanitize_path_arg(input: &str) -> String {
    parse_quoted_path(input.trim())
}

fn parse_quoted_path(input: &str) -> String {
    let trimmed = input.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn prompt_target_device(config: &mut PipelineConfig) -> Result<String> {
    use std::io::{self, Write};

    let profile_entries: Vec<(usize, &'static target_profiles::TargetDeviceProfile, String)> =
        target_profiles::TARGET_DEVICE_PROFILES
            .iter()
            .enumerate()
            .map(|(idx, profile)| (idx + 1, profile, slugify_profile_name(profile.name)))
            .collect();

    loop {
        println!(
            "\n{}{}{}",
            cli_color(COLORS.info),
            CLI_TEXT.interactive.target_device_title,
            cli_reset()
        );
        println!(
            "{}[0]{} {}",
            cli_color(COLORS.prompt),
            cli_reset(),
            fmt1(
                &CLI_TEXT.interactive.target_device_default,
                config.target_height()
            )
        );
        for (idx, profile, _slug) in &profile_entries {
            println!(
                "{}[{:>2}]{} {} ({}x{})",
                cli_color(COLORS.prompt),
                idx,
                cli_reset(),
                profile.name,
                profile.width,
                profile.height
            );
        }
        println!(
            "{}{}{}",
            cli_color(COLORS.highlight),
            CLI_TEXT.main.target_device_custom_with_hw,
            cli_reset()
        );
        print!(
            "{}{} {}",
            cli_color(COLORS.prompt),
            CLI_TEXT.main.run_cli_input_prompt,
            cli_reset()
        );
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty()
            || input.eq_ignore_ascii_case("default")
            || input.eq_ignore_ascii_case("auto")
            || input.eq_ignore_ascii_case(target_profiles::PROPORTIONAL_OPTION_LABEL)
        {
            let height = config.target_height();
            if let Err(e) = config.set_high_res_render_height(height) {
                println!(
                    "{}{}{}",
                    cli_color(COLORS.highlight),
                    fmt1(&CLI_TEXT.main.target_device_render_height_error, e),
                    cli_reset()
                );
                continue;
            }
            return Ok(format!("{}px height (proportional width)", height));
        }

        let normalized_spec = if let Ok(idx) = input.parse::<usize>() {
            if idx == 0 {
                None
            } else if let Some((_, profile, _)) =
                profile_entries.iter().find(|(number, _, _)| *number == idx)
            {
                Some(profile.name.to_string())
            } else {
                // If the numeric input is not a preset index, treat it as a custom height target.
                // This keeps inputs like "1600" working in interactive mode.
                Some(input.to_string())
            }
        } else {
            let slug = slugify_profile_name(input);
            if let Some((_, profile, _)) = profile_entries
                .iter()
                .find(|(_, _, profile_slug)| *profile_slug == slug)
            {
                Some(profile.name.to_string())
            } else if let Some(profile) = target_profiles::find_profile(input) {
                Some(profile.name.to_string())
            } else {
                Some(input.to_string())
            }
        };

        if let Some(spec) = normalized_spec {
            match parse_target_argument(&spec) {
                Ok(Some(selection)) => {
                    if let Some(width) = selection.width {
                        if let Err(e) = config.set_target_dimensions(width, selection.height) {
                            println!(
                                "{}{}{}",
                                cli_color(COLORS.highlight),
                                fmt1(&CLI_TEXT.main.target_device_apply_dimensions_error, e),
                                cli_reset()
                            );
                            continue;
                        }
                        if let Err(e) = config.set_high_res_render_height(selection.height) {
                            println!(
                                "{}{}{}",
                                cli_color(COLORS.highlight),
                                fmt1(&CLI_TEXT.main.target_device_render_height_error, e),
                                cli_reset()
                            );
                            continue;
                        }
                        return Ok(format!("{}x{} px", width, selection.height));
                    } else {
                        if let Err(e) = config.set_target_height(selection.height) {
                            println!(
                                "{}{}{}",
                                cli_color(COLORS.highlight),
                                fmt1(&CLI_TEXT.main.target_device_set_target_height_error, e),
                                cli_reset()
                            );
                            continue;
                        }
                        if let Err(e) = config.set_high_res_render_height(selection.height) {
                            println!(
                                "{}{}{}",
                                cli_color(COLORS.highlight),
                                fmt1(&CLI_TEXT.main.target_device_render_height_error, e),
                                cli_reset()
                            );
                            continue;
                        }
                        return Ok(format!(
                            "{}px height (proportional width)",
                            selection.height
                        ));
                    }
                }
                Ok(None) => {
                    let height = config.target_height();
                    if let Err(e) = config.set_high_res_render_height(height) {
                        println!(
                            "{}{}{}",
                            cli_color(COLORS.highlight),
                            fmt1(&CLI_TEXT.main.target_device_render_height_error, e),
                            cli_reset()
                        );
                        continue;
                    }
                    return Ok(format!("{}px height (proportional width)", height));
                }
                Err(e) => {
                    println!(
                        "{}{}{}",
                        cli_color(COLORS.highlight),
                        fmt2(&CLI_TEXT.main.target_device_invalid_spec_error, spec, e),
                        cli_reset()
                    );
                    continue;
                }
            }
        } else {
            let height = config.target_height();
            if let Err(e) = config.set_high_res_render_height(height) {
                println!(
                    "{}{}{}",
                    cli_color(COLORS.highlight),
                    fmt1(&CLI_TEXT.main.target_device_render_height_error, e),
                    cli_reset()
                );
                continue;
            }
            return Ok(format!("{}px height (proportional width)", height));
        }
    }
}

fn handle_png_folder(
    folder_path: PathBuf,
    enable_deskew: bool,
) -> Result<Option<(PathBuf, PipelineConfig)>> {
    if !folder_path.exists() {
        return Err(anyhow!(
            "Image folder does not exist: {}",
            folder_path.display()
        ));
    }
    if !folder_path.is_dir() {
        return Err(anyhow!(
            "Path is not a directory: {}",
            folder_path.display()
        ));
    }

    println!("{}", CLI_TEXT.main.image_folder_mode_title);
    println!(
        "{}",
        fmt1(
            &CLI_TEXT.main.image_folder_mode_input,
            folder_path.display()
        )
    );
    if enable_deskew {
        println!("{}", CLI_TEXT.main.image_folder_mode_deskew);
    }
    println!("{}", CLI_TEXT.main.image_folder_mode_description);

    // Call the image processing function from the library
    run_png_mode(folder_path, None, AppConfig::default(), enable_deskew)?;

    // Return None to indicate image mode was handled
    Ok(None)
}

fn handle_pdf_to_png(input: &str) -> Result<Option<(PathBuf, PipelineConfig)>> {
    // Parse input: "file.pdf [page_range] --pdf-to-png HEIGHT"
    let parts: Vec<&str> = input.split("--pdf-to-png").collect();
    if parts.len() != 2 {
        return Err(anyhow!(
            "Invalid PDF-to-PNG format. Use: file.pdf [page_range] --pdf-to-png HEIGHT"
        ));
    }

    let file_and_range = parts[0].trim();
    let height_str = parts[1].trim();

    // Parse height
    let height: u32 = height_str
        .parse()
        .map_err(|_| anyhow!("Invalid height: {}. Must be a positive number", height_str))?;

    if height < 100 || height > 10000 {
        return Err(anyhow!("Height must be between 100 and 10000 pixels"));
    }

    // Parse file path and page range
    let (files, page_range) = parse_file_paths_with_range(file_and_range);
    if files.is_empty() {
        return Err(anyhow!("No PDF file specified"));
    }

    let pdf_path = PathBuf::from(&files[0]);
    validate_pdf_file(&files[0])?;

    println!("{}", CLI_TEXT.main.pdf_to_png_mode_title);
    println!(
        "{}",
        fmt1(&CLI_TEXT.main.pdf_to_png_mode_input, pdf_path.display())
    );
    println!(
        "{}",
        fmt1(&CLI_TEXT.main.pdf_to_png_mode_target_height, height)
    );
    if let Some(ref range) = page_range {
        println!("{}", fmt1(&CLI_TEXT.main.pdf_to_png_mode_page_range, range));
    } else {
        println!("{}", CLI_TEXT.main.pdf_to_png_mode_all_pages);
    }

    // Call the PDF-to-PNG processing function
    run_pdf_to_png_mode(
        pdf_path,
        page_range,
        height,
        AppConfig::default(),
        false,
        256,
    )?;

    // Return None to indicate PDF-to-PNG mode was handled
    Ok(None)
}

fn handle_pdf_to_images(input: &str) -> Result<Option<(PathBuf, PipelineConfig)>> {
    // Parse input: "file.pdf [page_range] --pdf-to-images [--deskew] [--no-layout]"
    let parts: Vec<&str> = input.split("--pdf-to-images").collect();
    if parts.len() != 2 {
        return Err(anyhow!(
            "Invalid PDF-to-Images format. Use: file.pdf [page_range] --pdf-to-images [--deskew] [--no-layout]"
        ));
    }

    let file_and_range = parts[0].trim();
    let options_str = parts[1].trim();

    // Parse options
    let enable_deskew = options_str.contains("--deskew");
    let enable_layout = !options_str.contains("--no-layout");

    // Parse file path and page range
    let (files, page_range) = parse_file_paths_with_range(file_and_range);
    if files.is_empty() {
        return Err(anyhow!("No PDF file specified"));
    }

    let pdf_path = PathBuf::from(&files[0]);
    validate_pdf_file(&files[0])?;

    println!("PDF to Images Mode");
    println!("Input PDF: {}", pdf_path.display());
    println!(
        "Layout detection: {}",
        if enable_layout {
            "ENABLED (PNG output)"
        } else {
            "DISABLED (PBM output)"
        }
    );
    println!(
        "Deskew: {}",
        if enable_deskew { "ENABLED" } else { "disabled" }
    );
    if let Some(ref range) = page_range {
        println!("Page range: {}", range);
    } else {
        println!("Page range: all pages");
    }

    // Call the PDF-to-Images processing function
    run_pdf_to_images_mode(
        pdf_path,
        page_range,
        None, // Use default output directory
        AppConfig::default(),
        enable_layout,
        enable_deskew,
        false, // image_only - default to false in interactive mode
        false,
        256,
    )?;

    // Return None to indicate PDF-to-Images mode was handled
    Ok(None)
}

fn handle_images_to_images(input: &str) -> Result<Option<(PathBuf, PipelineConfig)>> {
    // Parse input: "folder --images-to-images [--deskew] [--no-layout]"
    let parts: Vec<&str> = input.split("--images-to-images").collect();
    if parts.len() != 2 {
        return Err(anyhow!(
            "Invalid Images-to-Images format. Use: folder --images-to-images [--deskew] [--no-layout]"
        ));
    }

    let folder_str = parts[0].trim();
    let options_str = parts[1].trim();

    // Parse options
    let enable_deskew = options_str.contains("--deskew");
    let enable_layout = !options_str.contains("--no-layout");

    let folder_path = parse_quoted_path(folder_str);
    let folder_path = PathBuf::from(folder_path);

    if !folder_path.exists() {
        return Err(anyhow!(
            "Image folder does not exist: {}",
            folder_path.display()
        ));
    }
    if !folder_path.is_dir() {
        return Err(anyhow!(
            "Path is not a directory: {}",
            folder_path.display()
        ));
    }

    println!("Images to Images Mode");
    println!("Input folder: {}", folder_path.display());
    println!(
        "Layout detection: {}",
        if enable_layout {
            "ENABLED (PNG output)"
        } else {
            "DISABLED (PBM output)"
        }
    );
    println!(
        "Deskew: {}",
        if enable_deskew { "ENABLED" } else { "disabled" }
    );

    // Call the Images-to-Images processing function
    run_images_to_images_mode(
        folder_path,
        None, // Use default output directory
        AppConfig::default(),
        enable_layout,
        enable_deskew,
        false, // image_only - default to false in interactive mode
        false,
        256,
    )?;

    // Return None to indicate Images-to-Images mode was handled
    Ok(None)
}

pub fn validate_pdf_file(path: &str) -> Result<()> {
    let path_buf = PathBuf::from(path);

    if !path_buf.exists() {
        return Err(anyhow!("File not found: {}", path));
    }

    if !path_buf.is_file() {
        return Err(anyhow!("Path is not a file: {}", path));
    }

    // Check file extension
    if let Some(ext) = path_buf.extension() {
        if ext.to_string_lossy().to_lowercase() != "pdf" {
            return Err(anyhow!("File is not a PDF: {}", path));
        }
    } else {
        return Err(anyhow!("File has no extension: {}", path));
    }

    // Check if file is readable and has content
    let metadata = fs::metadata(&path_buf)?;
    if metadata.len() == 0 {
        return Err(anyhow!("PDF file is empty: {}", path));
    }

    // Basic PDF header validation
    let mut file = fs::File::open(&path_buf)?;
    let mut header = [0u8; 5];
    use std::io::Read;
    file.read_exact(&mut header)?;

    if &header != b"%PDF-" {
        return Err(anyhow!("Invalid PDF file (missing PDF header): {}", path));
    }

    Ok(())
}

fn validate_page_range(range: &str) -> Result<()> {
    // Simple validation for page range format
    // Supports formats like: "1-5", "1,3,5", "1-5,10,15-20"
    for part in range.split(',') {
        let part = part.trim();
        if part.contains('-') {
            let range_parts: Vec<&str> = part.split('-').collect();
            if range_parts.len() != 2 {
                return Err(anyhow!("Invalid page range format: {}", part));
            }

            let start: u32 = range_parts[0]
                .parse()
                .map_err(|_| anyhow!("Invalid page number: {}", range_parts[0]))?;
            let end: u32 = range_parts[1]
                .parse()
                .map_err(|_| anyhow!("Invalid page number: {}", range_parts[1]))?;

            if start == 0 || end == 0 {
                return Err(anyhow!("Page numbers must be greater than 0"));
            }

            if start > end {
                return Err(anyhow!(
                    "Invalid range: start page {} is greater than end page {}",
                    start,
                    end
                ));
            }
        } else {
            let page: u32 = part
                .parse()
                .map_err(|_| anyhow!("Invalid page number: {}", part))?;
            if page == 0 {
                return Err(anyhow!("Page numbers must be greater than 0"));
            }
        }
    }

    Ok(())
}

fn show_system_status() -> Result<()> {
    info_println!("{}", CLI_TEXT.system_status.title);
    info_println!("{}", CLI_TEXT.system_status.divider);

    // OCR Status
    if is_ocr_available() {
        info_println!("{}", CLI_TEXT.system_status.tesseract_found);
    } else {
        info_println!("{}", CLI_TEXT.system_status.tesseract_missing);
    }

    // System info
    let mut sys = System::new_all();
    sys.refresh_all();

    info_println!(
        "{}",
        CLI_TEXT
            .system_status
            .memory_info
            .replace("{}", &format!("{}", sys.available_memory() / 1024 / 1024))
    );
    info_println!(
        "{}",
        CLI_TEXT
            .system_status
            .cpu_info
            .replace("{}", &format!("{}", sys.cpus().len()))
    );

    let (accel_enabled, accel_detail) = hardware_acceleration_status();
    let accel_text = if accel_enabled {
        fmt1(
            &CLI_TEXT.main.system_hardware_acceleration_enabled,
            accel_detail,
        )
    } else {
        fmt1(
            &CLI_TEXT.main.system_hardware_acceleration_disabled,
            accel_detail,
        )
    };
    info_println!(
        "{}",
        fmt1(
            &CLI_TEXT.main.system_hardware_acceleration_label,
            accel_text
        )
    );

    // Disk space
    let disks = Disks::new_with_refreshed_list();
    info_println!("{}", CLI_TEXT.main.system_storage);
    for disk in &disks {
        let total_gb = disk.total_space() / 1024 / 1024 / 1024;
        let available_gb = disk.available_space() / 1024 / 1024 / 1024;
        let usage_percent = ((total_gb - available_gb) as f64 / total_gb as f64 * 100.0) as u32;

        info_println!(
            "{}",
            fmt4(
                &CLI_TEXT.main.system_storage_line,
                disk.mount_point().display(),
                available_gb,
                total_gb,
                usage_percent
            )
        );
    }

    // Configuration
    if let Some(config_path) = AppConfig::default_config_path() {
        let config_status = if config_path.exists() {
            CLI_TEXT.main.system_config_found.as_str()
        } else {
            CLI_TEXT.main.system_config_missing.as_str()
        };
        info_println!(
            "{}",
            fmt2(
                &CLI_TEXT.main.system_config_file,
                config_path.display(),
                config_status
            )
        );
    }

    info_println!("{}", CLI_TEXT.main.system_footer_divider);

    Ok(())
}

fn determine_output_directory(
    provided: Option<PathBuf>,
    inputs: &[String],
    config: &AppConfig,
) -> Result<PathBuf> {
    if let Some(output) = provided {
        return Ok(output);
    }

    // Use config default if available
    if let Some(ref default_output) = config.default_output {
        return Ok(default_output.clone());
    }

    // Try to find common directory of input files - output to same directory as input
    if !inputs.is_empty() {
        let first_path = PathBuf::from(&inputs[0]);
        if let Some(parent) = first_path.parent() {
            return Ok(parent.to_path_buf());
        }
    }

    // Fallback to current directory
    Ok(PathBuf::from("."))
}

fn unix_to_ymdhm(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let mins = secs / 60;
    let mi = (mins % 60) as u32;
    let hours = mins / 60;
    let h = (hours % 24) as u32;
    let mut days = (hours / 24) as u32;
    let mut y = 1970u32;
    loop {
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31u32,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 1u32;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        mo += 1;
    }
    (y, mo, days + 1, h, mi, s)
}

fn generate_output_path(
    input_path: &PathBuf,
    output_dir: &PathBuf,
    config: &PipelineConfig,
) -> Result<PathBuf> {
    // If the caller supplied a full file path (has extension), use it as-is.
    // This is the --gui-worker path where the GUI already generated the filename.
    if output_dir.extension().is_some() {
        return Ok(output_dir.clone());
    }

    let input_stem = input_path
        .file_stem()
        .ok_or_else(|| anyhow!("Invalid input filename"))?
        .to_string_lossy();

    let now = SystemTime::now();
    let timestamp = {
        use std::time::UNIX_EPOCH;
        let secs = now.duration_since(UNIX_EPOCH)?.as_secs();
        let (y, mo, d, h, mi, s) = unix_to_ymdhm(secs);
        format!("{:04}-{:02}-{:02}_{:02}{:02}{:02}", y, mo, d, h, mi, s)
    };

    // If DJVU is selected, output a .djvu file directly
    if config.text_format() == "djvu" {
        let output_filename = format!("{}_processed_djvu_{}.djvu", input_stem, timestamp);
        return Ok(output_dir.join(output_filename));
    }

    // EPUB: reflowable text-only output
    if config.text_format() == "epub" {
        let output_filename = format!("{}_processed_{}.epub", input_stem, timestamp);
        return Ok(output_dir.join(output_filename));
    }

    // For PDF: use text format as the identifier (no cover format suffix)
    let output_filename = format!(
        "{}_processed_{}_{}.pdf",
        input_stem,
        config.text_format(),
        timestamp
    );
    Ok(output_dir.join(output_filename))
}

fn build_png_folder_pipeline_config(cli_opts: &CliOptions) -> Result<PipelineConfig> {
    let mut pipeline_config = PipelineConfig::simple_cli_defaults()
        .map_err(|e| anyhow!("Failed to construct CLI defaults: {}", e))?;

    if let Some(ref fmt) = cli_opts.text_format {
        pipeline_config.set_text_format(fmt)?;
    }

    if let Some(ref method) = cli_opts.binarization {
        let mut method_str = match method.as_str() {
            "adaptive" => "1".to_string(),
            "fixed" => "2".to_string(),
            "heavy" => "3".to_string(),
            _ => method.clone(),
        };
        if let Some(k) = cli_opts.sauvola_k {
            method_str.push_str(&format!(" k={}", k));
        }
        if let Some(thr) = cli_opts.threshold {
            method_str.push_str(&format!(" thr={}", thr));
        }
        let bin_config = lege::CliConfigBuilder::parse_binarization_method(&method_str);
        pipeline_config.set_binarization(bin_config);
    } else if cli_opts.threshold.is_some() || cli_opts.sauvola_k.is_some() {
        let mut method_str = "2".to_string();
        if let Some(k) = cli_opts.sauvola_k {
            method_str = "1".to_string();
            method_str.push_str(&format!(" k={}", k));
        }
        if let Some(thr) = cli_opts.threshold {
            method_str.push_str(&format!(" thr={}", thr));
        }
        let bin_config = lege::CliConfigBuilder::parse_binarization_method(&method_str);
        pipeline_config.set_binarization(bin_config);
    }

    if let Some(q) = cli_opts.djvu_quality {
        pipeline_config.set_djvu_iw44_quality(q)?;
    }
    if cli_opts.high_quality {
        pipeline_config.set_high_quality_output(true);
    }

    if cli_opts.halftone {
        pipeline_config.set_image_region_dither_mode(ImageRegionDitherMode::Halftone);
    } else if cli_opts.gray_jp2 {
        pipeline_config.set_image_region_dither_mode(ImageRegionDitherMode::GrayJp2);
        pipeline_config.set_keep_original_images(false);
    } else if cli_opts.dither {
        let tf = pipeline_config.text_format();
        if tf == "ccitt4" {
            pipeline_config
                .set_image_region_dither_mode(ImageRegionDitherMode::Ccitt4ClusteredDot4x4);
        } else {
            pipeline_config.set_image_region_dither_mode(ImageRegionDitherMode::Stucki);
        }
    }

    let tf = pipeline_config.text_format();
    if cli_opts.halftone && tf != "jbig2" {
        bail!(
            "--halftone requires --text-format jbig2 (got {}). Halftone segments are JBIG2-only.",
            tf
        );
    }

    if cli_opts.no_layout {
        pipeline_config.set_enable_layout_detection(false);
    }
    apply_ocr_options(&mut pipeline_config, cli_opts);
    if let Some(scale) = cli_opts.slow_ocr_scale {
        pipeline_config.set_slow_ocr_scale(scale);
    }
    if cli_opts.invert {
        pipeline_config.set_invert_input(true);
        if pipeline_config.enable_layout_detection() {
            pipeline_config.set_enable_layout_detection(false);
        }
    }

    if pipeline_config.text_format() == "jbig2" {
        let selected_mode = cli_opts.jbig2_mode.clone().unwrap_or_else(|| {
            if pipeline_config.enable_layout_detection() {
                Jbig2Mode::Symbol
            } else {
                Jbig2Mode::Generic
            }
        });
        pipeline_config.set_jbig2_mode(selected_mode);
    }
    if cli_opts.deskew {
        pipeline_config.set_enable_deskew(true);
    }
    if cli_opts.reflow {
        if !pipeline_config.enable_layout_detection() {
            bail!("--reflow requires layout detection (do not combine with --no-layout/--invert)");
        }
        pipeline_config.set_enable_reflow(true);
    }
    if cli_opts.jpeg_compat {
        pipeline_config.set_jpeg_compat(true);
    }
    if cli_opts.halftone || cli_opts.dither {
        pipeline_config.set_keep_original_images(false);
    }
    if cli_opts.original_images || cli_opts.image_only {
        pipeline_config.set_keep_original_images(true);
    }

    pipeline_config.set_max_parallel_pages(Some(12))?;
    pipeline_config.set_channel_buffer_size(Some(12))?;
    Ok(pipeline_config)
}

fn resolve_png_folder_output_path(
    provided: Option<PathBuf>,
    folder_path: &PathBuf,
    config: &PipelineConfig,
) -> Result<PathBuf> {
    if let Some(output) = provided {
        if output.extension().is_some() && !output.is_dir() {
            return Ok(output);
        }
        return generate_output_path(folder_path, &output, config);
    }

    let output_dir = folder_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    generate_output_path(folder_path, &output_dir, config)
}

/// Check if a PDF file has OCR/text layers by sampling pages
async fn check_pdf_ocr_layer(pdf_path: &PathBuf) -> Result<bool> {
    use std::sync::Arc;

    // Read PDF bytes
    let pdf_bytes = tokio::fs::read(pdf_path)
        .await
        .map_err(|e| anyhow!("Failed to read PDF: {}", e))?;
    let pdf_bytes = Arc::from(pdf_bytes.into_boxed_slice());

    // Create a minimal renderer just for checking text layers
    let mut raster_cfg = lege::pagerender::RasterConfig::default();
    raster_cfg.render_forms = false;

    let renderer = lege::pagerender::PdfiumRenderer::new_from_bytes(pdf_bytes, raster_cfg)?;

    // Check for OCR using the has_any_text_layer method
    let has_ocr = renderer.has_any_text_layer().await?;

    Ok(has_ocr)
}

#[derive(Clone, Copy, Default)]
struct CliStageSnapshot {
    rendered: u32,
    detected: u32,
    encoded: u32,
    deskewed: u32,
}

#[derive(Clone, Copy)]
struct CliStageEvent<'a> {
    current: u32,
    stage_order: u8,
    stage_label: &'a str,
    stage_color: &'a str,
    verb: &'a str,
    include_percentage: bool,
}

fn current_cli_timestamp() -> String {
    let now = chrono::Local::now();
    format!("[{}]", now.format("%H:%M:%S"))
}

fn format_elapsed(elapsed: std::time::Duration) -> String {
    let total_secs = elapsed.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn print_timestamped_line(line: &str) {
    for segment in line.lines() {
        println!("{} {}", current_cli_timestamp(), segment);
    }
}

fn print_stage_progress_line(
    stage_label: &str,
    stage_color: &str,
    verb: &str,
    current: u32,
    total: u32,
    include_percentage: bool,
) {
    if total == 0 {
        return;
    }
    let line = if include_percentage {
        let pct = (((current as f64 / total as f64) * 100.0).round() as u32).min(100);
        format!(
            "{}[{}]{} {}{}{}: {}{}/{}{} ({}%)",
            stage_color,
            stage_label,
            cli_reset(),
            stage_color,
            verb,
            cli_reset(),
            cli_color(COLORS.page_complete),
            current,
            total,
            cli_reset(),
            pct
        )
    } else {
        format!(
            "{}[{}]{} {}{}{}: {}{}/{}{}",
            stage_color,
            stage_label,
            cli_reset(),
            stage_color,
            verb,
            cli_reset(),
            cli_color(COLORS.page_complete),
            current,
            total,
            cli_reset()
        )
    };
    print_timestamped_line(&line);
}

fn emit_cli_stage_progress(
    snapshot: &mut CliStageSnapshot,
    metrics: lege::progress::ProgressMetrics,
) {
    let total = metrics.pages_total.max(1);
    let rendered = metrics.rendered.min(total);
    let detected = metrics.detected.min(rendered);
    let encoded = metrics.encoded.min(match metrics.mode {
        lege::progress::ProgressMode::Layout | lege::progress::ProgressMode::Margin => detected,
        lege::progress::ProgressMode::NoLayout | lege::progress::ProgressMode::HeavySequential => {
            rendered.max(metrics.encoded).min(total)
        }
        lege::progress::ProgressMode::Reflow => metrics.encoded.min(total),
        lege::progress::ProgressMode::Unknown => metrics.encoded.min(total),
    });
    let deskewed = metrics.deskewed.min(rendered.max(encoded));
    let mut events: Vec<CliStageEvent<'static>> = Vec::new();

    let mut push_stage_events = |start: u32,
                                 end: u32,
                                 stage_order: u8,
                                 stage_label: &'static str,
                                 stage_color: &'static str,
                                 verb: &'static str| {
        if end > start {
            for current in (start + 1)..=end {
                events.push(CliStageEvent {
                    current,
                    stage_order,
                    stage_label,
                    stage_color,
                    verb,
                    include_percentage: stage_label == "Encode",
                });
            }
        }
    };

    match metrics.mode {
        lege::progress::ProgressMode::Layout | lege::progress::ProgressMode::Margin => {
            push_stage_events(
                snapshot.rendered,
                rendered,
                0,
                "Render",
                cli_color(COLORS.render),
                "Page rendered",
            );
            push_stage_events(
                snapshot.detected,
                detected,
                1,
                "Infer",
                cli_color(COLORS.detect),
                "Page inferred",
            );
            push_stage_events(
                snapshot.encoded,
                encoded,
                2,
                "Encode",
                cli_color(COLORS.encode),
                "Page encoded",
            );
            snapshot.rendered = rendered;
            snapshot.detected = detected;
            snapshot.encoded = encoded;
        }
        lege::progress::ProgressMode::NoLayout | lege::progress::ProgressMode::HeavySequential => {
            push_stage_events(
                snapshot.encoded,
                encoded,
                2,
                "Encode",
                cli_color(COLORS.encode),
                "Page encoded",
            );
            snapshot.encoded = encoded;
        }
        lege::progress::ProgressMode::Reflow => {}
        lege::progress::ProgressMode::Unknown => {}
    }

    if metrics.enable_deskew {
        push_stage_events(
            snapshot.deskewed,
            deskewed,
            3,
            "Deskew",
            cli_color(COLORS.page_start),
            "Page deskewed",
        );
        snapshot.deskewed = deskewed;
    }

    events.sort_by_key(|event| (event.current, event.stage_order));

    for event in events {
        print_stage_progress_line(
            event.stage_label,
            event.stage_color,
            event.verb,
            event.current,
            total,
            event.include_percentage,
        );
    }
}

fn process_single_file(
    input_path: PathBuf,
    output_path: PathBuf,
    config: PipelineConfig,
) -> Result<()> {
    // Clone config so we can log settings after processing completes.
    let log_config = config.clone();

    // Subscribe first, then spawn so we don't miss early events.
    let manager = progress::get_progress_manager();
    let receiver = manager.subscribe();
    let task_id =
        progress::spawn_file_processing_task(input_path.clone(), output_path.clone(), config);

    // CLI always uses a line-by-line color stream. GUI has a separate renderer.
    let mut progress_error: Option<anyhow::Error> = None;
    let mut stage_snapshot = CliStageSnapshot::default();
    let mut last_status_lines: Option<(String, String, String)> = None;
    let job_started_at = Instant::now();
    loop {
        match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(ProgressUpdate::Status {
                task_id: id,
                status,
                metrics,
            }) if id == task_id => {
                if let Some(m) = metrics {
                    emit_cli_stage_progress(&mut stage_snapshot, m);
                }

                // Stage lines are emitted from metrics above. Avoid duplicate aggregate bars
                // and duplicate encode lines from writer-actor append notifications.
                if matches!(
                    status,
                    lege::progress::ProcessingStatus::LayoutProgress { .. }
                        | lege::progress::ProcessingStatus::NoLayoutProgress { .. }
                        | lege::progress::ProcessingStatus::MarginProgress { .. }
                        | lege::progress::ProcessingStatus::PdfAppend { .. }
                        | lege::progress::ProcessingStatus::PdfAppendMargin { .. }
                ) {
                    continue;
                }
                let (l1, l2, l3) = status.to_cli_display_lines();
                if last_status_lines
                    .as_ref()
                    .is_some_and(|(p1, p2, p3)| *p1 == l1 && *p2 == l2 && *p3 == l3)
                {
                    continue;
                }
                last_status_lines = Some((l1.clone(), l2.clone(), l3.clone()));
                if !l1.is_empty() {
                    print_timestamped_line(&l1);
                }
                if !l2.is_empty() {
                    print_timestamped_line(&l2);
                }
                if !l3.is_empty() {
                    print_timestamped_line(&l3);
                }
            }
            Ok(ProgressUpdate::Completed {
                task_id: id,
                message,
                metrics,
            }) if id == task_id => {
                if let Some(m) = metrics {
                    emit_cli_stage_progress(&mut stage_snapshot, m);
                }
                let elapsed = format_elapsed(job_started_at.elapsed());
                let completed = format!("{}\nElapsed: {}", message, elapsed);
                print_timestamped_line(&fmt1(&CLI_TEXT.main.progress_complete, completed));
                break;
            }
            Ok(ProgressUpdate::Error {
                task_id: id,
                error,
                metrics: _,
            }) if id == task_id => {
                print_timestamped_line(&fmt1(&CLI_TEXT.main.progress_error, &error));
                progress_error = Some(anyhow!(error));
                break;
            }
            Ok(_) => {}
            Err(flume::RecvTimeoutError::Timeout) => continue,
            Err(flume::RecvTimeoutError::Disconnected) => {
                if progress_error.is_none() {
                    progress_error =
                        Some(anyhow!("Processing task disconnected before completion"));
                }
                break;
            }
        }
    }
    let progress_result = if let Some(err) = progress_error {
        Err(err)
    } else {
        Ok(())
    };

    if let Err(err) = progress_result {
        return Err(err);
    }

    // Record processing outcome in log history (best-effort, non-fatal on error).
    let original_size = fs::metadata(&input_path).map(|m| m.len()).unwrap_or(0);
    let compressed_size = fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);
    let page_range_used = log_config.page_range().is_some();

    let processing_result = LogProcessingResult::new(
        input_path.clone(),
        output_path.clone(),
        original_size,
        compressed_size,
        page_range_used,
    );

    let mut log_options = LogProcessingOptions::from_pipeline_config(&log_config);
    log_options.input_path = Some(input_path);
    log_options.output_path = Some(output_path);

    if let Err(err) = history_log::add_log_entry(&processing_result, &log_options) {
        eprintln!("{}", fmt1(&CLI_TEXT.main.history_log_warning, err));
    }

    Ok(())
}
