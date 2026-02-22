use Legencode::types::BinarizationConfig;
#[cfg(feature = "debug-logging")]
use Legencode::{clear_debug_log, get_debug_log_messages};
use anyhow::{Result, anyhow, bail};
use lege::progress::{self, ProgressUpdate};
use lege::text_loader::CLI_TEXT;
use lege::{
    AppConfig, CoverFormat, PageRange, PipelineConfig, error_println, info_println,
    is_ocr_available, run_pdf_to_png_mode, run_png_mode, run_pdf_layout_crop_debug, DebugCropKind, target_profiles,
};

mod version;
use std::fs;
use std::path::PathBuf;
use version::display_version;

// ============================================================================
// COLOR CONFIGURATION - Easily customize all CLI colors here
// ============================================================================
pub struct ColorConfig {
    // Interactive prompts
    pub prompt: &'static str,           // Bright cyan for user input prompts
    pub info: &'static str,             // Base prompt color for general info
    pub highlight: &'static str,        // Base prompt color for emphasized text
    
    // Processing stages - subtle, muted colors for visual pleasure
    pub page_start: &'static str,       // Soft blue for page starting
    pub page_complete: &'static str,    // Medium-bright green - MOST STRIKING
    pub ocr: &'static str,              // Muted purple/gray for OCR operations
    pub render: &'static str,           // Soft cyan for rendering
    pub detect: &'static str,           // Soft yellow for detection/inference
    pub encode: &'static str,           // Soft magenta for encoding
    pub worker: &'static str,           // Dim cyan for worker/slot messages
    pub dag: &'static str,              // Very dim for DAG internals
    
    // Status labels
    pub status_label: &'static str,     // Dim blue for "Status" label
    pub detail_label: &'static str,     // Dim cyan for "Detail" label
    
    pub reset: &'static str,            // Reset to default
}

pub const COLORS: ColorConfig = ColorConfig {
    // Interactive prompts
    prompt: "\x1b[96m",          // Bright cyan
    info: "\x1b[96m",            // Bright cyan (same base prompt color)
    highlight: "\x1b[96m",       // Bright cyan (same base prompt color)
    
    // Processing stages - subtle palette
    page_start: "\x1b[94m",      // Bright blue (soft)
    page_complete: "\x1b[92m",   // Bright green (STRIKING) - the star of the show
    ocr: "\x1b[90m",             // Bright black (muted gray)
    render: "\x1b[96m",          // Bright cyan (soft)
    detect: "\x1b[93m",          // Bright yellow (soft)
    encode: "\x1b[95m",          // Bright magenta (soft)
    worker: "\x1b[2;36m",        // Dim cyan (very subtle)
    dag: "\x1b[2;90m",           // Dim bright-black (almost invisible)
    
    // Status labels
    status_label: "\x1b[2;34m",  // Dim blue
    detail_label: "\x1b[2;36m",  // Dim cyan
    
    reset: "\x1b[0m",            // Reset
};
// ============================================================================

use lege::processing_log::{
    self as history_log, ProcessingOptions as LogProcessingOptions,
    ProcessingResult as LogProcessingResult,
};

mod windows_dirs;
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{Disks, System};

/// Cleanup function specifically for CLI to ensure clean process exit
async fn cleanup_cli_resources() {
    // Give background tasks a moment to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // In CLI mode, we can be more aggressive about cleanup since the process will exit anyway
    // This helps ensure the process terminates cleanly
}

// Binarization parsing moved to CliConfigBuilder in types.rs

// Text constants for CLI output
static DOC: &str = "[DOC]";
static CHECK: &str = "[OK]";
static FLOPPY: &str = "[SAVE]";

// IMPORTANT: CLI user-facing copy in this file must come from `src/cli_text.json`
// via `CLI_TEXT` (through `text_loader.rs`). Do not introduce hardcoded
// user-visible strings in `main.rs`; update `cli_text.json` instead.
fn fmt1(template: &str, a: impl std::fmt::Display) -> String {
    template.replacen("{}", &a.to_string(), 1)
}

fn fmt2(
    template: &str,
    a: impl std::fmt::Display,
    b: impl std::fmt::Display,
) -> String {
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
    let gpu_disabled = std::env::var("LEGE_DISABLE_GPU")
        .ok()
        .as_deref() == Some("1");
    if gpu_disabled {
        return (false, "explicitly disabled via LEGE_DISABLE_GPU=1".to_string());
    }

    #[cfg(target_os = "linux")]
    {
        if lege::gpu::webgpu_execution_provider_dispatch().is_some() {
            return (true, "WebGPU (Dawn/Vulkan)".to_string());
        }
        return (false, "WebGPU unavailable, CPU fallback".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        // DirectML provider is selected in engine setup on Windows when GPU is enabled.
        return (true, "DirectML".to_string());
    }

    #[cfg(target_os = "macos")]
    {
        // CoreML provider is selected in engine setup on macOS when GPU is enabled.
        return (true, "CoreML".to_string());
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        return (false, "not supported on this platform".to_string());
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Special help: environment variables
    // Version flag
    if args.iter().any(|arg| arg == "--version" || arg == "-v") {
        println!("{}", fmt1(&CLI_TEXT.main.version_line, display_version()));
        println!("{}", fmt1(&CLI_TEXT.main.internal_version_line, version::internal_version()));
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

    // No extra args -> interactive CLI wizard
    if args.len() == 1 {
        handle_cli_mode(AppConfig::default()).await?;
        return Ok(());
    }

    // Direct PDF-to-PNG mode: file.pdf [page_range] --pdf-to-png HEIGHT
    if args.iter().any(|a| a == "--pdf-to-png") {
        let pdf_arg = args.get(1).ok_or_else(|| anyhow!("Missing PDF path"))?;
        let pdf_path = PathBuf::from(sanitize_path_arg(pdf_arg));
        validate_pdf_file(pdf_path.to_str().ok_or_else(|| anyhow!("Invalid PDF path"))?)?;

        let flag_idx = args.iter().position(|a| a == "--pdf-to-png").ok_or_else(|| anyhow!("Missing --pdf-to-png flag"))?;
        let height_str = args.get(flag_idx + 1).ok_or_else(|| anyhow!("Missing height after --pdf-to-png"))?;
        let height: u32 = height_str.parse().map_err(|_| anyhow!("Invalid height: {}", height_str))?;
        if height < 100 || height > 10000 { bail!("Height must be between 100 and 10000 pixels"); }

        let page_range = if flag_idx >= 3 {
            let candidate = args.get(2).unwrap();
            if !candidate.starts_with('-') && !candidate.ends_with(".pdf") { Some(candidate.clone()) } else { None }
        } else { None };

        run_pdf_to_png_mode(pdf_path, page_range, height, AppConfig::default())?;
        return Ok(());
    }

    if args.iter().any(|a| a == "--png-folder") {
        let folder_arg = args.get(1).ok_or_else(|| anyhow!("Missing folder path"))?;
        let folder_path = PathBuf::from(sanitize_path_arg(folder_arg));
        let enable_deskew = args.iter().any(|a| a == "--deskew");
        run_png_mode(folder_path, None, AppConfig::default(), enable_deskew)?;
        return Ok(());
    }

    if args.iter().any(|a| a == "--crop-areas") {
        let pdf_arg = args.get(1).ok_or_else(|| anyhow!("Missing PDF path"))?;
        let pdf_path = PathBuf::from(sanitize_path_arg(pdf_arg));
        validate_pdf_file(pdf_path.to_str().ok_or_else(|| anyhow!("Invalid PDF path"))?)?;
        let flag_idx = args.iter().position(|a| a == "--crop-areas").ok_or_else(|| anyhow!("Missing --crop-areas flag"))?;
        let mode_str = args.get(flag_idx + 1).ok_or_else(|| anyhow!("Missing mode after --crop-areas (text|image|both)"))?.to_ascii_lowercase();
        let crop_kind = match mode_str.as_str() {
            "text" => DebugCropKind::Text,
            "image" => DebugCropKind::Image,
            "both" => DebugCropKind::Both,
            _ => bail!("Invalid mode: {}. Use text|image|both", mode_str),
        };
        let page_range = if flag_idx >= 3 {
            let candidate = args.get(2).unwrap();
            if !candidate.starts_with('-') && !candidate.ends_with(".pdf") { Some(candidate.clone()) } else { None }
        } else { None };
        let mut output_dir: Option<PathBuf> = None;
        let mut format_opt: Option<String> = None;
        let enable_deskew = args.iter().any(|a| a == "--deskew");
        let mut i = 0usize;
        while i + 1 < args.len() {
            if args[i] == "--out" { output_dir = Some(PathBuf::from(sanitize_path_arg(&args[i+1]))); i += 2; continue; }
            if args[i] == "--format" { format_opt = Some(args[i+1].clone()); i += 2; continue; }
            i += 1;
        }
        run_pdf_layout_crop_debug(pdf_path, output_dir, crop_kind, page_range, AppConfig::default(), enable_deskew, format_opt.as_deref()).await?;
        return Ok(());
    }

    // Simple one‑shot processing: pdf [pdf ...] [page_range] [target]
    if args.len() >= 2 {
        let mut positional: Vec<String> = args[1..].iter().map(|s| sanitize_path_arg(s)).collect();
        let mut page_range: Option<String> = None;
        let mut target_arg: Option<String> = None;

        // Trailing target (height/profile) takes precedence over page range so numeric targets aren't misread.
        if let Some(last) = positional.last() {
            if !last.to_lowercase().ends_with(".pdf") {
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
            if !last.to_lowercase().ends_with(".pdf") {
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

        handle_simple_processing(positional, page_range, target_arg, AppConfig::default()).await?;
        return Ok(());
    }

    Ok(())
}

fn print_usage() {
    println!("{}", CLI_TEXT.main.usage_block);
}

fn print_licenses() {
    // Attempt to include the licenses file at compile time; if missing (packaging/build variation),
    // fall back to a short notice to prevent build failure.
    let licenses = include_str!("../docs/THIRD-PARTY-LICENSES.md");
    println!("{}", licenses);
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
        let height: u32 = whitespace_parts[0]
            .parse()
            .map_err(|_| anyhow!("Invalid height in target specification: {}", whitespace_parts[0]))?;
        let width: u32 = whitespace_parts[1]
            .parse()
            .map_err(|_| anyhow!("Invalid width in target specification: {}", whitespace_parts[1]))?;
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
        println!("{}", fmt3(&CLI_TEXT.main.target_profile_item, profile.name, profile.width, profile.height));
    }
    println!("{}", fmt1(&CLI_TEXT.main.target_profile_proportional, target_profiles::PROPORTIONAL_OPTION_LABEL));
    println!();
    println!("{}", CLI_TEXT.main.target_profiles_custom_note);
    println!("{}", CLI_TEXT.main.target_profiles_page_range_note);
}

async fn handle_simple_processing(
    pdf_paths: Vec<String>,
    page_range: Option<String>,
    target_spec: Option<String>,
    config: AppConfig,
) -> Result<()> {
    if pdf_paths.is_empty() {
        bail!("No PDF files to process");
    }

    // Normalize and validate inputs
    let mut normalized_inputs = Vec::with_capacity(pdf_paths.len());
    for raw in pdf_paths {
        let cleaned = sanitize_path_arg(&raw);
        validate_pdf_file(&cleaned)?;
        normalized_inputs.push(cleaned);
    }
    let mut file_paths: Vec<PathBuf> = normalized_inputs.iter().map(|p| PathBuf::from(p)).collect();

    // Create baseline pipeline config for simple CLI mode
    let mut pipeline_config = PipelineConfig::simple_cli_defaults()
        .map_err(|e| anyhow!("Failed to construct CLI defaults: {}", e))?;

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
        // Validate the page range first
        validate_page_range(range)?;
        let page_range_obj = PageRange::parse(range)?;
        pipeline_config.set_page_range(Some(page_range_obj));
    }

    // Determine output directory
    let output_dir = determine_output_directory(None, &normalized_inputs, &config)?;

    // Show batch summary
    info_println!("\n{}", CLI_TEXT.main.simple_mode_header);
    info_println!("{}", fmt1(&CLI_TEXT.main.simple_mode_files_queued, file_paths.len()));
    for path in &file_paths {
        info_println!("{}", fmt1(&CLI_TEXT.main.simple_mode_file_item, path.display()));
    }
    if let Some(ref range) = page_range {
        info_println!("{}", fmt1(&CLI_TEXT.main.simple_mode_page_range, range));
    }
    info_println!("{}", fmt1(&CLI_TEXT.main.simple_mode_settings, &target_description));
    info_println!("{}", fmt1(&CLI_TEXT.main.simple_mode_output_directory, output_dir.display()));
    info_println!("{}\n", CLI_TEXT.main.simple_mode_footer);

    let total_files = file_paths.len();
    let mut overall_ok = true;

    for (index, file_path) in file_paths.drain(..).enumerate() {
        let mut per_file_config = pipeline_config.clone();
        let output_path = generate_output_path(&file_path, &output_dir, &per_file_config)?;

        if total_files > 1 {
            info_println!(
                "{}",
                fmt3(&CLI_TEXT.main.simple_mode_batch_item, index + 1, total_files, file_path.display())
            );
            info_println!("{}", fmt1(&CLI_TEXT.main.simple_mode_batch_output, output_path.display()));
        } else {
            info_println!("{}", fmt1(&CLI_TEXT.main.simple_mode_input, file_path.display()));
            info_println!("{}", fmt1(&CLI_TEXT.main.simple_mode_output, output_path.display()));
        }

        match process_single_file(file_path.clone(), output_path.clone(), per_file_config).await {
            Ok(()) => {
                if total_files > 1 {
                    let remaining = total_files - index - 1;
                    info_println!(
                        "{}",
                        fmt3(&CLI_TEXT.main.simple_mode_batch_completed, index + 1, total_files, remaining)
                    );
                }
            }
            Err(error) => {
                overall_ok = false;
                error_println!("{}", fmt2(&CLI_TEXT.main.simple_mode_error_processing, file_path.display(), error));
            }
        }
    }

    cleanup_cli_resources().await;
    if overall_ok {
        std::process::exit(0);
    } else {
        std::process::exit(1);
    }
}

fn print_env_variables_help() {
    println!("{}", CLI_TEXT.main.env_variables_help_block);
}

async fn handle_cli_mode(config: AppConfig) -> Result<()> {
    match run_cli().await? {
        Some((file_path, pipeline_config)) => {
            let output_dir = determine_output_directory(
                None,
                &[file_path.to_string_lossy().to_string()],
                &config,
            )?;
            let output_path = generate_output_path(&file_path, &output_dir, &pipeline_config)?;
            let result = process_single_file(file_path, output_path, pipeline_config).await;

            // Force cleanup and exit for CLI to ensure clean termination
            cleanup_cli_resources().await;
            if result.is_ok() {
                std::process::exit(0);
            } else {
                std::process::exit(1);
            }
        }
        None => Ok(()),
    }
}

async fn run_cli() -> Result<Option<(PathBuf, PipelineConfig)>> {
    // Combined input: file path and processing options
    println!("{}", CLI_TEXT.app.main_title);
    println!("{}", CLI_TEXT.app.file_prompt);
    print!("{}{} {}", COLORS.prompt, CLI_TEXT.main.run_cli_input_prompt, COLORS.reset);
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.is_empty() {
        return Ok(None);
    }

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
    if input.contains("--crop-areas") {
        return handle_pdf_crop_areas(input).await;
    }

    // Parse file paths and page range
    let (files, page_range) = parse_file_paths_with_range(input);
    if files.is_empty() {
        return Ok(None);
    }

    let file_path = &files[0];
    validate_pdf_file(file_path)?;

    // Check for OCR layer in the PDF right after validation
    if PathBuf::from(file_path)
        .extension()
        .and_then(|s| s.to_str())
        == Some("pdf")
    {
        // Use tokio::task::block_in_place to allow async call in sync context
        let has_ocr = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { check_pdf_ocr_layer(&PathBuf::from(file_path)).await })
        });

        match has_ocr {
            Ok(true) => {
                println!("\n{}{}{}", COLORS.info, CLI_TEXT.interactive.messages.ocr_detected, COLORS.reset);
                println!("{}", CLI_TEXT.interactive.messages.ocr_detected_msg);
                println!("{}\n", CLI_TEXT.interactive.messages.ocr_detected_tip);
            }
            Ok(false) => {
                println!("\n{}{}{}", COLORS.highlight, CLI_TEXT.interactive.messages.no_ocr_detected, COLORS.reset);
                println!("{}\n", CLI_TEXT.interactive.messages.no_ocr_detected_msg);
            }
            Err(e) => {
                eprintln!(
                    "{}{}{}",
                    COLORS.highlight,
                    fmt1(&CLI_TEXT.main.ocr_check_warning, e),
                    COLORS.reset
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
        original_image,
        no_cover_page,
        pdf_compatibility,
        no_binarization,
        invert_input,
        symbol_mode,
        center_margins,
        crop_margins,
        force_crop,
        deskew_enabled,
        djvu_quality,
    ) = loop {
        print!("\n{}{}{}\n", COLORS.info, CLI_TEXT.interactive.processing_options_title, COLORS.reset);
        println!("{}{}{}", COLORS.prompt, CLI_TEXT.interactive.format_prompt, COLORS.reset);
        println!("{}{}{}", COLORS.prompt, CLI_TEXT.interactive.modifiers_prompt, COLORS.reset);
        println!("{}{}{}", COLORS.highlight, CLI_TEXT.interactive.examples_prompt, COLORS.reset);
        println!("{}{}{}", COLORS.info, CLI_TEXT.interactive.default_prompt, COLORS.reset);
        print!("{}{} {}", COLORS.prompt, CLI_TEXT.main.run_cli_input_prompt, COLORS.reset);
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
                original_image,
                no_cover_page,
                pdf_compatibility,
                no_binarization,
                invert_input,
                symbol_mode,
                center_margins,
                crop_margins,
                force_crop,
                deskew_enabled,
                djvu_quality,
            )) => {
                // No immediate rejection; we'll apply precedence rules below when building config
                break (
                    text_format,
                    cover_format,
                    enable_dithering,
                    layout_detection_enabled,
                    ocr_enabled,
                    original_image,
                    no_cover_page,
                    pdf_compatibility,
                    no_binarization,
                    invert_input,
                    symbol_mode,
                    center_margins,
                    crop_margins,
                    force_crop,
                    deskew_enabled,
                    djvu_quality,
                );
            }
            Err(e) => {
                println!("{}", fmt1(&CLI_TEXT.main.processing_options_retry, e));
                continue;
            }
        }
    };

    // Step 3: Binarization method (always selectable, even when inversion is enabled)
    println!("\n{}{}{}", COLORS.info, CLI_TEXT.interactive.binarization_title, COLORS.reset);
    println!("{}", fmt3(&CLI_TEXT.main.binarization_choices_template,
        &CLI_TEXT.interactive.binarization_methods[0],
        &CLI_TEXT.interactive.binarization_methods[1],
        &CLI_TEXT.interactive.binarization_methods[2]
    ));
    println!("{}{}{}", COLORS.highlight, CLI_TEXT.interactive.binarization_advanced, COLORS.reset);
    print!("{}{} {} ", COLORS.prompt, CLI_TEXT.interactive.binarization_prompt, COLORS.reset);
    io::stdout().flush()?;

    let mut binarization_input = String::new();
    io::stdin().read_line(&mut binarization_input)?;
    let binarization_input = binarization_input.trim();

    let binarization_method = parse_binarization_method(binarization_input)?;

    // Apply precedence rules before constructing the final config

    // 1) Margin processing precedence: force-crop ('f') > crop ('w') > center ('m')
    let (effective_center_margins, effective_crop_margins, effective_force_crop) = if force_crop {
        // Force crop overrides everything
        if crop_margins || center_margins {
            println!(
                "{}",
                CLI_TEXT.main.precedence_force_crop_note
            );
        }
        (false, true, true)
    } else if crop_margins {
        // Crop overrides center
        if center_margins {
            println!(
                "{}",
                CLI_TEXT.main.precedence_crop_over_center_note
            );
        }
        (false, true, false)
    } else {
        (center_margins, false, false)
    };

    // 2) Layout detection is enabled by default, disabled by 'a' flag
    // When disabled, margin processing uses pixel-based analysis
    let effective_layout_detection = layout_detection_enabled;
    if (effective_center_margins || effective_crop_margins) && !layout_detection_enabled {
        println!(
            "{}",
            CLI_TEXT.main.margin_without_layout_note
        );
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
    // Use unified image format setter for non-binarized images
    config.set_image_format(cover_format);
    config.set_dither_images(effective_enable_dithering);
    config.set_enable_layout_detection(effective_layout_detection);
    config.set_enable_ocr(ocr_enabled);
    config.set_no_cover_page(no_cover_page);
    config.set_pdf_compatibility_mode(pdf_compatibility);
    config.set_invert_input(invert_input);
    config.set_enable_deskew(deskew_enabled);
    config.set_jbig2_symbol_mode(symbol_mode);
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
    println!("\n{}{}{}", COLORS.info, CLI_TEXT.interactive.selected_options_title, COLORS.reset);
    let text_format_display = if config.text_format() == "jbig2" && config.jbig2_symbol_mode() {
        "jbig2 (symbol mode)".to_string()
    } else {
        config.text_format().to_string()
    };
    println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_text_encoding, COLORS.reset, text_format_display);
    println!("{}{}:{} {:?}", COLORS.info, CLI_TEXT.main.selected_options_image_format, COLORS.reset, config.image_format());
    println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_dithering, COLORS.reset, config.dither_images());
    println!(
        "{}{}:{} {}",
        COLORS.info, CLI_TEXT.main.selected_options_original_quality, COLORS.reset, config.keep_original_images()
    );
    println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_target_output, COLORS.reset, target_summary);

    let original_layout_detection = config.enable_layout_detection();
    if config.invert_input() && original_layout_detection {
        println!(
            "{}{}:{} {} (disabled - inverted documents not supported)",
            COLORS.info, CLI_TEXT.main.selected_options_layout_detection, COLORS.reset, original_layout_detection
        );
        println!("{}{}:{} {}", COLORS.highlight, CLI_TEXT.main.selected_options_note, COLORS.reset, CLI_TEXT.interactive.messages.layout_disabled_inverted);
        println!("{}{}:{} {}", COLORS.highlight, CLI_TEXT.main.selected_options_reason, COLORS.reset, CLI_TEXT.interactive.messages.layout_disabled_reason);
        config.set_enable_layout_detection(false);
    } else {
        println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_layout_detection, COLORS.reset, config.enable_layout_detection());
    }

    println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_ocr_enabled, COLORS.reset, config.enable_ocr());
    println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_no_cover_page, COLORS.reset, config.no_cover_page());
    println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_pdf_compatibility, COLORS.reset, config.pdf_compatibility_mode());
    println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_invert_input, COLORS.reset, config.invert_input());
    println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_deskew_enabled, COLORS.reset, config.enable_deskew());
    println!("{}{}:{} {:?}", COLORS.info, CLI_TEXT.main.selected_options_margin_processing, COLORS.reset, config.margin_settings());
    println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_force_crop, COLORS.reset, config.crop_footnotes());
    println!("{}{}:{} {}", COLORS.info, CLI_TEXT.main.selected_options_max_retries, COLORS.reset, config.max_retries());
    println!("{}{}:{} {}ms", COLORS.info, CLI_TEXT.main.selected_options_retry_delay, COLORS.reset, config.retry_delay_ms());
    println!("{}{}{}\n", COLORS.info, CLI_TEXT.main.selected_options_footer, COLORS.reset);

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

fn parse_format_selection_with_options(
    input: &str,
) -> Result<(
    String,
    CoverFormat,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    Option<u8>, // djvu_quality (None for non-DjVu formats, Some for DjVu)
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
            true,  // original_image (no dithering)
            false, // no_cover_page
            false, // pdf_compatibility
            false, // no_binarization
            false, // invert_input
            false, // symbol_mode
            false, // center_margins
            false, // crop_margins
            false, // force_crop
            false, // deskew_enabled
            None,  // djvu_quality (not DjVu)
        ));
    }

    let parts: Vec<&str> = input.split_whitespace().collect();
    let main_part = parts[0];

    // Check for --high flag in remaining parts (hidden quality flag for DjVu)
    let has_high_flag = parts.iter().any(|&p| p == "--high");

    // Parse main format option
    let (format_num, has_c_flag, has_s_flag) = parse_main_format(main_part)?;

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
    // 'c' flag ENABLES dithering
    let enable_dithering = if format_num == 3 {
        // DJVU doesn't use dithering flag
        false
    } else {
        has_c_flag
    };

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

    let remaining_parts: Vec<&str> = parts.iter().skip(options_start_index).copied()
        .filter(|&p| p != "--high")  // Filter out --high flag
        .collect();
    options_parts.extend(remaining_parts.iter().map(|s| s.to_string()));

    let options_str = options_parts.join(" ");
    let (
        layout_detection,
        ocr_enabled,
        original_image,
        no_cover_page,
        pdf_compatibility,
        no_binarization,
        invert_input,
        deskew_enabled,
        center_margins,
        crop_margins,
        force_crop,
    ) = parse_options(&options_str)?;

    // Disable dithering if 'c' flag is specified (original quality)
    let final_enable_dithering = enable_dithering;

    let symbol_mode = format_num == 1 && has_s_flag;

    // Determine DjVu quality based on --high flag
    let djvu_quality = if format_num == 3 {
        // DjVu format
        if has_high_flag {
            Some(100)  // High quality mode (97 slices)
        } else {
            Some(75)   // Default quality (85 slices)
        }
    } else {
        None  // Not DjVu format
    };

    // Note: We do not offer "Original+JBIG2" because JBIG2's advantage is its superior dithering.
    // Using JBIG2 without dithering negates file size benefits and offers no other clear advantage over CCITT4.
    Ok((
        text_format,
        cover_format,
        final_enable_dithering,
        layout_detection,
        ocr_enabled,
        original_image,
        no_cover_page,
        pdf_compatibility,
        no_binarization,
        invert_input,
        symbol_mode,
        center_margins,
        crop_margins,
        force_crop,
        deskew_enabled,
        djvu_quality,
    ))
}

fn parse_main_format(input: &str) -> Result<(u32, bool, bool)> {
    let has_c_flag = input.contains('c'); // 'c' enables dithering
    let has_s_flag = input.contains('s'); // 's' enables symbol mode for JBIG2

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

    // Symbol mode only works with JBIG2 (format 2); error for other formats
    if has_s_flag && format_num != 2 {
        return Err(anyhow!(
            "Symbol mode ('s') is only available with JBIG2 format (2)"
        ));
    }

    Ok((format_num, has_c_flag, has_s_flag))
}

fn parse_options(
    input: &str,
) -> Result<(
    bool,
    bool,
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
    let ocr_enabled = options.contains(&"b");
    // 'c' now selects DITHERED images (quality-vs-size toggle). Original is default.
    let original_image = !options.contains(&"c");
    let no_cover_page = options.contains(&"d");
    let pdf_compatibility = options.contains(&"e");
    let no_binarization = options.contains(&"i");  // 'i' for image-only (no binarization)
    let invert_input = options.contains(&"g");
    let deskew_enabled = options.contains(&"h");
    let center_margins = options.contains(&"m");
    let crop_margins = options.contains(&"w");
    let force_crop = options.contains(&"f");

    Ok((
        layout_detection,
        ocr_enabled,
        original_image,
        no_cover_page,
        pdf_compatibility,
        no_binarization,
        invert_input,
        deskew_enabled,
        center_margins,
        crop_margins,
        force_crop,
    ))
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
        } else if !part.is_empty() {
            return Err(anyhow!(
                "Unrecognized parameter '{}'. Use k=<value> or thr=<value>.",
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
        println!("\n{}{}{}", COLORS.info, CLI_TEXT.interactive.target_device_title, COLORS.reset);
        println!(
            "{}[0]{} {}",
            COLORS.prompt,
            COLORS.reset,
            fmt1(&CLI_TEXT.interactive.target_device_default, config.target_height())
        );
        for (idx, profile, _slug) in &profile_entries {
            println!(
                "{}[{:>2}]{} {} ({}x{})",
                COLORS.prompt, idx, COLORS.reset, profile.name, profile.width, profile.height
            );
        }
        println!("{}{}{}", COLORS.highlight, CLI_TEXT.main.target_device_custom_with_hw, COLORS.reset);
        print!("{}{} {}", COLORS.prompt, CLI_TEXT.main.run_cli_input_prompt, COLORS.reset);
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
                println!("{}{}{}", COLORS.highlight, fmt1(&CLI_TEXT.main.target_device_render_height_error, e), COLORS.reset);
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
                            println!("{}{}{}", COLORS.highlight, fmt1(&CLI_TEXT.main.target_device_apply_dimensions_error, e), COLORS.reset);
                            continue;
                        }
                        if let Err(e) = config.set_high_res_render_height(selection.height) {
                            println!("{}{}{}", COLORS.highlight, fmt1(&CLI_TEXT.main.target_device_render_height_error, e), COLORS.reset);
                            continue;
                        }
                        return Ok(format!("{}x{} px", width, selection.height));
                    } else {
                        if let Err(e) = config.set_target_height(selection.height) {
                            println!("{}{}{}", COLORS.highlight, fmt1(&CLI_TEXT.main.target_device_set_target_height_error, e), COLORS.reset);
                            continue;
                        }
                        if let Err(e) = config.set_high_res_render_height(selection.height) {
                            println!("{}{}{}", COLORS.highlight, fmt1(&CLI_TEXT.main.target_device_render_height_error, e), COLORS.reset);
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
                        println!("{}{}{}", COLORS.highlight, fmt1(&CLI_TEXT.main.target_device_render_height_error, e), COLORS.reset);
                        continue;
                    }
                    return Ok(format!("{}px height (proportional width)", height));
                }
                Err(e) => {
                    println!("{}{}{}", COLORS.highlight, fmt2(&CLI_TEXT.main.target_device_invalid_spec_error, spec, e), COLORS.reset);
                    continue;
                }
            }
        } else {
            let height = config.target_height();
            if let Err(e) = config.set_high_res_render_height(height) {
                println!("{}{}{}", COLORS.highlight, fmt1(&CLI_TEXT.main.target_device_render_height_error, e), COLORS.reset);
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
    println!("{}", fmt1(&CLI_TEXT.main.image_folder_mode_input, folder_path.display()));
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
    println!("{}", fmt1(&CLI_TEXT.main.pdf_to_png_mode_input, pdf_path.display()));
    println!("{}", fmt1(&CLI_TEXT.main.pdf_to_png_mode_target_height, height));
    if let Some(ref range) = page_range {
        println!("{}", fmt1(&CLI_TEXT.main.pdf_to_png_mode_page_range, range));
    } else {
        println!("{}", CLI_TEXT.main.pdf_to_png_mode_all_pages);
    }

    // Call the PDF-to-PNG processing function
    run_pdf_to_png_mode(pdf_path, page_range, height, AppConfig::default())?;

    // Return None to indicate PDF-to-PNG mode was handled
    Ok(None)
}

async fn handle_pdf_crop_areas(input: &str) -> Result<Option<(PathBuf, PipelineConfig)>> {
    let parts: Vec<&str> = input.split("--crop-areas").collect();
    if parts.len() != 2 {
        return Err(anyhow!("Invalid crop-areas format. Use: file.pdf [page_range] --crop-areas text|image|both [--out DIR] [--format png|jpg] [--deskew]"));
    }
    let file_and_opts = parts[0].trim();
    let mode_and_flags = parts[1].trim();
    let (files, _page_range) = parse_file_paths_with_range(file_and_opts);
    if files.is_empty() {
        return Err(anyhow!("No PDF file specified"));
    }
    let pdf_path = PathBuf::from(&files[0]);
    validate_pdf_file(&files[0])?;

    // Mode
    let mut tokens = mode_and_flags.split_whitespace();
    let mode_token = tokens
        .next()
        .ok_or_else(|| anyhow!("Missing mode after --crop-areas (text|image|both)"))?
        .to_ascii_lowercase();
    let crop_kind = match mode_token.as_str() {
        "text" => DebugCropKind::Text,
        "image" => DebugCropKind::Image,
        "both" => DebugCropKind::Both,
        _ => bail!("Invalid mode: {}. Use text|image|both", mode_token),
    };

    // Flags
    let enable_deskew = mode_and_flags.contains("--deskew");
    let flag_parts: Vec<&str> = mode_and_flags.split_whitespace().collect();
    let mut output_dir: Option<PathBuf> = None;
    let mut format_opt: Option<String> = None;
    let mut i = 0usize;
    while i + 1 < flag_parts.len() {
        if flag_parts[i] == "--out" {
            output_dir = Some(PathBuf::from(parse_quoted_path(flag_parts[i + 1])));
            i += 2;
            continue;
        }
        if flag_parts[i] == "--format" {
            format_opt = Some(flag_parts[i + 1].to_string());
            i += 2;
            continue;
        }
        i += 1;
    }

    run_pdf_layout_crop_debug(
        pdf_path,
        output_dir,
        crop_kind,
        None, // page_range already parsed and applied
        AppConfig::default(),
        enable_deskew,
        format_opt.as_deref(),
    ).await?;
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
        fmt1(&CLI_TEXT.main.system_hardware_acceleration_enabled, accel_detail)
    } else {
        fmt1(&CLI_TEXT.main.system_hardware_acceleration_disabled, accel_detail)
    };
    info_println!(
        "{}",
        fmt1(&CLI_TEXT.main.system_hardware_acceleration_label, accel_text)
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
        info_println!("{}", fmt2(&CLI_TEXT.main.system_config_file, config_path.display(), config_status));
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

fn generate_output_path(
    input_path: &PathBuf,
    output_dir: &PathBuf,
    config: &PipelineConfig,
) -> Result<PathBuf> {
    let input_stem = input_path
        .file_stem()
        .ok_or_else(|| anyhow!("Invalid input filename"))?
        .to_string_lossy();

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    // If DJVU is selected, output a .djvu file directly
    if config.text_format() == "djvu" {
        let output_filename = format!("{}_processed_djvu_{}.djvu", input_stem, timestamp);
        return Ok(output_dir.join(output_filename));
    }

    // For PDF: use text format as the identifier (no cover format suffix)
    let output_filename = format!(
        "{}_processed_{}_{}.pdf",
        input_stem, config.text_format(), timestamp
    );
    Ok(output_dir.join(output_filename))
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

fn print_stage_progress_line(stage_label: &str, stage_color: &str, verb: &str, current: u32, total: u32) {
    if total == 0 {
        return;
    }
    let pct = (((current as f64 / total as f64) * 100.0).round() as u32).min(100);
    println!(
        "{}[{}]{} {}{}{}: {}{}/{}{} ({}%)",
        stage_color,
        stage_label,
        COLORS.reset,
        stage_color,
        verb,
        COLORS.reset,
        COLORS.page_complete,
        current,
        total,
        COLORS.reset,
        pct
    );
}

fn emit_cli_stage_progress(snapshot: &mut CliStageSnapshot, metrics: lege::progress::ProgressMetrics) {
    let total = metrics.pages_total.max(1);

    match metrics.mode {
        lege::progress::ProgressMode::Layout | lege::progress::ProgressMode::Margin => {
            if metrics.rendered > snapshot.rendered {
                for n in (snapshot.rendered + 1)..=metrics.rendered {
                    print_stage_progress_line("Render", COLORS.render, "Page rendered", n, total);
                }
                snapshot.rendered = metrics.rendered;
            }
            if metrics.detected > snapshot.detected {
                for n in (snapshot.detected + 1)..=metrics.detected {
                    print_stage_progress_line("Infer", COLORS.detect, "Page inferred", n, total);
                }
                snapshot.detected = metrics.detected;
            }
            if metrics.encoded > snapshot.encoded {
                for n in (snapshot.encoded + 1)..=metrics.encoded {
                    print_stage_progress_line("Encode", COLORS.encode, "Page encoded", n, total);
                }
                snapshot.encoded = metrics.encoded;
            }
        }
        lege::progress::ProgressMode::NoLayout | lege::progress::ProgressMode::HeavySequential => {
            if metrics.encoded > snapshot.encoded {
                for n in (snapshot.encoded + 1)..=metrics.encoded {
                    print_stage_progress_line("Encode", COLORS.encode, "Page encoded", n, total);
                }
                snapshot.encoded = metrics.encoded;
            }
        }
        lege::progress::ProgressMode::Unknown => {}
    }

    if metrics.deskewed > snapshot.deskewed {
        for n in (snapshot.deskewed + 1)..=metrics.deskewed {
            print_stage_progress_line("Deskew", COLORS.page_start, "Page deskewed", n, total);
        }
        snapshot.deskewed = metrics.deskewed;
    }
}

async fn process_single_file(
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
    loop {
        match receiver.recv_async().await {
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
                if !l1.is_empty() {
                    println!("{}", l1);
                }
                if !l2.is_empty() {
                    println!("{}", l2);
                }
                if !l3.is_empty() {
                    println!("{}", l3);
                }
            }
            Ok(ProgressUpdate::Completed {
                task_id: id,
                message,
                metrics: _,
            }) if id == task_id => {
                println!("{}", fmt1(&CLI_TEXT.main.progress_complete, message));
                break;
            }
            Ok(ProgressUpdate::Error {
                task_id: id,
                error,
                metrics: _,
            }) if id == task_id => {
                println!("{}", fmt1(&CLI_TEXT.main.progress_error, &error));
                progress_error = Some(anyhow!(error));
                break;
            }
            Ok(_) => {}
            Err(_) => break,
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
