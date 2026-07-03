use std::collections::VecDeque;
/// Subprocess-based processing worker for the GUI.
///
/// Each queue item spawns a hidden `lege` CLI process in `--gui-worker` mode.
/// Progress events arrive as newline-delimited JSON on the child's stdout and
/// are deserialized into local protocol types, then forwarded to a merged
/// flume channel that the GUI progress loop consumes exactly as before.
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;

use crate::models::{
    CoverImageType, ImageProcessingType, OcrMode, OutputFormat, ProcessingOptions,
};

// ── Protocol types ────────────────────────────────────────────────────────────
// These mirror lege::progress::* exactly so that serde can round-trip the
// newline-delimited JSON emitted by `lege --gui-worker`.

#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
pub enum WorkerProgressMode {
    Unknown,
    NoLayout,
    Layout,
    Margin,
    HeavySequential,
    Reflow,
    Epub,
}

impl Default for WorkerProgressMode {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
pub struct WorkerProgressMetrics {
    pub pages_total: u32,
    pub rendered: u32,
    pub detected: u32,
    pub encoded: u32,
    pub mode: WorkerProgressMode,
    pub is_djvu: bool,
    pub enable_layout_detection: bool,
    pub eta_seconds: Option<u32>,
}

/// Mirror of lege::progress::ProcessingStatus — variants must match the
/// `#[serde(tag = "kind", rename_all = "snake_case")]` representation.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerProcessingStatus {
    Initializing,
    AssemblingOutput,
    Complete {
        message: String,
    },
    Error {
        error: String,
    },
    FootnotesDetected {
        message: String,
    },
    OcrLayerDetected {
        has_ocr: bool,
    },
    MarginPass1Analyzing,
    MarginAnalysisSummary {
        summary: String,
    },
    PipelineMessage {
        stage: String,
        message: String,
    },
    PdfAppend {
        current: usize,
        total: usize,
    },
    PdfAppendMargin {
        current: usize,
        total: usize,
    },
    ReflowProgress {
        stage: WorkerReflowStage,
        current: usize,
        total: usize,
        eta: Option<String>,
    },
    EpubProgress {
        rendered: usize,
        detected: usize,
        ocr: usize,
        total: usize,
        eta: Option<String>,
    },
    LayoutProgress {
        rendered: usize,
        detected: usize,
        encoded: usize,
        total: usize,
        enable_layout_detection: bool,
        eta: Option<String>,
    },
    NoLayoutProgress {
        rendered: usize,
        encoded: usize,
        total: usize,
        eta: Option<String>,
    },
    MarginProgress {
        pass1_rendered: usize,
        pass1_detected: usize,
        pass2_processed: usize,
        total: usize,
        enable_layout_detection: bool,
        eta: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerReflowStage {
    SourceAnalysis,
    Compose,
    OutputPages,
}

impl WorkerProcessingStatus {
    /// Returns three display lines for the GUI (no ANSI codes).
    pub fn to_gui_display_lines(&self) -> (String, String, String) {
        match self {
            Self::Initializing => (
                "[Initializing]".into(),
                "Preparing pipeline...".into(),
                String::new(),
            ),
            Self::AssemblingOutput => (
                "[Finalizing]".into(),
                "Assembling output file...".into(),
                "This may take a moment.".into(),
            ),
            Self::Complete { message } => (
                "[Complete]".into(),
                message.clone(),
                "Ready for next task.".into(),
            ),
            Self::Error { error } => ("[Error]".into(), "An error occurred.".into(), error.clone()),
            Self::FootnotesDetected { message } => (
                "[Margin Analysis]".into(),
                "Margin Analysis: note".into(),
                format!("{} | Footnotes preserved (centering used).", message),
            ),
            Self::OcrLayerDetected { has_ocr } => {
                if *has_ocr {
                    (
                        "[OCR Detection]".into(),
                        "OCR layer detected in document".into(),
                        "Leave OCR disabled to preserve existing text layer.".into(),
                    )
                } else {
                    (
                        "[OCR Detection]".into(),
                        "No OCR layer found".into(),
                        "Enable OCR if you want to add text layer.".into(),
                    )
                }
            }
            Self::MarginPass1Analyzing => (
                "[Margin Analysis - Pass 1]".into(),
                "Preparing document-wide margin analysis...".into(),
                "Progress will update as pages are analyzed.".into(),
            ),
            Self::MarginAnalysisSummary { summary } => (
                "[Margin Analysis - Pass 1]".into(),
                "Margin Analysis: complete".into(),
                summary.clone(),
            ),
            Self::PipelineMessage { stage, message } => {
                (format!("[{stage}]"), message.clone(), String::new())
            }
            Self::PdfAppend { .. } | Self::PdfAppendMargin { .. } => {
                (String::new(), String::new(), String::new())
            }
            Self::ReflowProgress {
                stage,
                current,
                total,
                eta,
            } => {
                let detail = match stage {
                    WorkerReflowStage::SourceAnalysis => {
                        format!("Rendering source pages and detecting layout: {current}/{total}")
                    }
                    WorkerReflowStage::Compose => {
                        "Building reflow plan from detected regions, rows, and words...".into()
                    }
                    WorkerReflowStage::OutputPages => {
                        format!("Rasterizing and encoding reflowed output pages: {current}/{total}")
                    }
                };
                let title = match stage {
                    WorkerReflowStage::SourceAnalysis => "[Reflow - Source Analysis]",
                    WorkerReflowStage::Compose => "[Reflow - Compose]",
                    WorkerReflowStage::OutputPages => "[Reflow - Output Pages]",
                };
                (
                    title.into(),
                    detail,
                    eta.as_ref()
                        .map(|eta| format!("Estimated time remaining: {eta}"))
                        .unwrap_or_default(),
                )
            }
            Self::EpubProgress {
                rendered,
                detected,
                ocr,
                total,
                eta,
            } => {
                let detail = format!(
                    "Render {rendered}/{total} | Layout {detected}/{total} | OCR {ocr}/{total}"
                );
                (
                    "[EPUB]".into(),
                    detail,
                    eta.as_ref()
                        .map(|eta| format!("Estimated time remaining: {eta}"))
                        .unwrap_or_default(),
                )
            }
            Self::LayoutProgress {
                rendered,
                detected,
                encoded,
                total,
                eta,
                ..
            } => {
                let detail = format!(
                    "Render {rendered}/{total} | Infer {detected}/{total} | Encode {encoded}/{total}"
                );
                (
                    "[Layout Mode]".into(),
                    detail,
                    eta.as_ref()
                        .map(|eta| format!("Estimated time remaining: {eta}"))
                        .unwrap_or_default(),
                )
            }
            Self::NoLayoutProgress {
                rendered,
                encoded,
                total,
                eta,
            } => {
                let detail = format!("Render {rendered}/{total} | Encode {encoded}/{total}");
                (
                    "[No-Layout Mode]".into(),
                    detail,
                    eta.as_ref()
                        .map(|eta| format!("Estimated time remaining: {eta}"))
                        .unwrap_or_default(),
                )
            }
            Self::MarginProgress {
                pass1_rendered,
                pass1_detected,
                pass2_processed,
                total,
                eta,
                ..
            } => {
                let detail = format!(
                    "Analyze {pass1_rendered}/{total} | Infer {pass1_detected}/{total} | Process {pass2_processed}/{total}"
                );
                (
                    "[Margin Mode]".into(),
                    detail,
                    eta.as_ref()
                        .map(|eta| format!("Estimated time remaining: {eta}"))
                        .unwrap_or_default(),
                )
            }
        }
    }
}

/// Mirror of lege::progress::ProgressUpdate.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerProgressUpdate {
    Status {
        task_id: u64,
        status: WorkerProcessingStatus,
        metrics: Option<WorkerProgressMetrics>,
    },
    Completed {
        task_id: u64,
        message: String,
        metrics: Option<WorkerProgressMetrics>,
    },
    Error {
        task_id: u64,
        error: String,
        metrics: Option<WorkerProgressMetrics>,
    },
}

impl WorkerProgressUpdate {
    /// Rewrite the task_id embedded in the event (so GUI-assigned IDs match tracker_infos).
    pub fn with_task_id(self, new_id: u64) -> Self {
        match self {
            Self::Status {
                status, metrics, ..
            } => Self::Status {
                task_id: new_id,
                status,
                metrics,
            },
            Self::Completed {
                message, metrics, ..
            } => Self::Completed {
                task_id: new_id,
                message,
                metrics,
            },
            Self::Error { error, metrics, .. } => Self::Error {
                task_id: new_id,
                error,
                metrics,
            },
        }
    }
}

// ── CLI arg generation ────────────────────────────────────────────────────────

/// Map GUI ProcessingOptions to CLI arguments.
/// Set `gui_worker = true` for JSON-event mode; `false` for the normal human CLI.
pub fn gui_options_to_cli_args(
    input_path: &PathBuf,
    output_path: &PathBuf,
    options: &ProcessingOptions,
    gui_worker: bool,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();

    if gui_worker {
        args.push("--gui-worker".into());
    }
    args.push(input_path.as_os_str().into());
    args.push("--output".into());
    // Pass the full output file path; the CLI passes through paths with extensions.
    args.push(output_path.as_os_str().into());

    // Text / output format
    let text_format = options.effective_text_format();
    args.push("--text-format".into());
    args.push(text_format.into());

    if options.make_epub_also && !matches!(options.output_format, OutputFormat::Epub) {
        args.push("--epub-sidecar-output".into());
        args.push(output_path.with_extension("epub").as_os_str().into());
    }

    // Cover / image format
    let cover_format = match options.cover_image_type {
        CoverImageType::None => "none",
        _ => "jpeg",
    };
    if matches!(options.output_format, OutputFormat::Pdf) {
        args.push("--cover-format".into());
        args.push(cover_format.into());
    }

    // Layout detection
    if !options.layout_analysis {
        args.push("--no-layout".into());
    }
    if options.layout_analysis
        && let Some(ref pages) = options.layout_exclusion_pages
    {
        if !pages.trim().is_empty() {
            args.push("--exclude-layout".into());
            args.push(pages.trim().into());
        }
    }

    // OCR
    if options.use_ocr {
        args.push("--ocr-mode".into());
        args.push(
            match options.ocr_mode {
                OcrMode::Fast => "fast",
                OcrMode::Thorough => "best",
            }
            .into(),
        );
    } else {
        args.push("--no-ocr".into());
    }

    // Reflow
    if options.reflow {
        args.push("--reflow".into());
    }

    // Image processing
    let use_jbig2_halftone = matches!(options.output_format, OutputFormat::Pdf)
        && options.layout_analysis
        && matches!(options.image_processing_type, ImageProcessingType::Dithered)
        && options.use_jbig2_halftone;
    if use_jbig2_halftone {
        args.push("--halftone".into());
    } else if matches!(options.image_processing_type, ImageProcessingType::Dithered)
        && options.layout_analysis
    {
        args.push("--dither".into());
    }

    // Quality
    if options.jpeg_compat {
        args.push("--jpeg-compat".into());
    }
    if options.high_quality_output {
        args.push("--high-quality".into());
    }
    if options.invert_input {
        args.push("--invert".into());
    }

    // Margin processing
    if options.center_margins {
        args.push("--center-margins".into());
    } else if options.crop_margins {
        args.push("--crop-margins".into());
    }
    if options.crop_margins || options.crop_free_aspect {
        args.push("--crop-free-aspect".into());
    }
    if options.crop_footnotes {
        args.push("--force-crop".into());
    }

    // Binarization
    if options.use_fixed_threshold {
        args.push("--binarization".into());
        args.push("fixed".into());
        args.push("--threshold".into());
        args.push(options.threshold_value.to_string().into());
    } else if options.use_heavy_binarization {
        args.push("--binarization".into());
        args.push("heavy".into());
    } else {
        args.push("--binarization".into());
        args.push("adaptive".into());
        args.push("--sauvola-k".into());
        args.push(options.k_factor.to_string().into());
    }

    // Trailing positionals.  ORDER MATTERS: the CLI's trailing-argument parser
    // only prioritises a *target* on the very last token; the second-to-last
    // token is re-checked solely as a page range.  A numeric target height
    // (e.g. "1200") sitting in the second-to-last slot is therefore misread as
    // a malformed page range ("must be in format 'start-end'") and the worker
    // aborts before emitting any event.  So emit the page range FIRST and the
    // target LAST: `<input> [page_range] [target]`.

    // Page range (must come before the target).
    if let Some(ref range) = options.page_range {
        if !range.trim().is_empty() {
            args.push(OsString::from(range.as_str()));
        }
    }

    // Target dimensions / height (must be the final positional).
    if let Some(height) = options.target_height {
        if let Some(width) = options.target_width.filter(|_| !options.crop_free_aspect) {
            args.push(OsString::from(format!("{height}x{width}")));
        } else {
            args.push(OsString::from(height.to_string()));
        }
    }

    args
}

// ── Worker handle and subprocess spawning ────────────────────────────────────

/// A running `lege --gui-worker` child process.
pub struct WorkerHandle {
    /// Child handle.
    pub child: Option<std::process::Child>,
    /// PID, used as a kill fallback.
    pub pid: u32,
    pub task_id: u64,
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    active_pids: Option<Arc<Mutex<Vec<u32>>>>,
    cancelled: Option<Arc<AtomicBool>>,
}

impl WorkerHandle {
    pub fn supervisor(active_pids: Arc<Mutex<Vec<u32>>>, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            child: None,
            pid: 0,
            task_id: u64::MAX,
            input_path: PathBuf::new(),
            output_path: PathBuf::new(),
            active_pids: Some(active_pids),
            cancelled: Some(cancelled),
        }
    }

    pub fn kill(&mut self) {
        if let Some(cancelled) = &self.cancelled {
            cancelled.store(true, Ordering::SeqCst);
        }
        if let Some(active_pids) = &self.active_pids {
            if let Ok(pids) = active_pids.lock() {
                for pid in pids.iter().copied() {
                    kill_pid(pid);
                }
            }
            return;
        }
        if let Some(ref mut c) = self.child {
            let _ = c.kill();
        } else {
            kill_pid(self.pid);
        }
    }
}

fn kill_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status();
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

fn resolve_cli_path() -> Result<PathBuf> {
    // Look for lege.exe next to lege-gui.exe first (installed / release layout).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(if cfg!(windows) { "lege.exe" } else { "lege" });
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    // Dev fallback: assume `lege` is on PATH (cargo run layout).
    Ok(PathBuf::from(if cfg!(windows) {
        "lege.exe"
    } else {
        "lege"
    }))
}

/// Spawn a hidden `lege --gui-worker` child process and stream its
/// newline-delimited JSON progress events into `events_tx`.
pub fn spawn_lege_worker(
    gui_task_id: u64,
    input_path: PathBuf,
    output_path: PathBuf,
    options: &ProcessingOptions,
    events_tx: flume::Sender<WorkerProgressUpdate>,
    log_tx: Option<flume::Sender<String>>,
) -> Result<WorkerHandle> {
    let cli_path = resolve_cli_path()?;
    let cli_args = gui_options_to_cli_args(&input_path, &output_path, options, true);

    let mut cmd = std::process::Command::new(&cli_path);
    cmd.args(&cli_args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to launch lege worker ({:?}): {}", cli_path, e))?;

    let pid = child.id();
    let terminal_seen = Arc::new(AtomicBool::new(false));
    let stderr_lines = Arc::new(Mutex::new(VecDeque::<String>::with_capacity(50)));

    // Dedicated thread: read stdout, parse JSON, forward events.
    let stdout = child.stdout.take().expect("stdout piped");
    let stdout_events_tx = events_tx.clone();
    let stdout_terminal_seen = terminal_seen.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if let Ok(update) = serde_json::from_str::<WorkerProgressUpdate>(&line) {
                        if matches!(
                            update,
                            WorkerProgressUpdate::Completed { .. }
                                | WorkerProgressUpdate::Error { .. }
                        ) {
                            stdout_terminal_seen.store(true, Ordering::SeqCst);
                        }
                        let _ = stdout_events_tx.send(update.with_task_id(gui_task_id));
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Dedicated thread: drain stderr (forward to log or discard).
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_lines_for_thread = stderr_lines.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            match line {
                Ok(line) => {
                    if let Ok(mut lines) = stderr_lines_for_thread.lock() {
                        if lines.len() >= 50 {
                            lines.pop_front();
                        }
                        lines.push_back(line.clone());
                    }
                    if let Some(ref tx) = log_tx {
                        let _ = tx.send(line);
                    }
                }
                Err(_) => break,
            }
        }
    });

    let wait_events_tx = events_tx.clone();
    let wait_terminal_seen = terminal_seen.clone();
    let wait_stderr_lines = stderr_lines.clone();
    std::thread::spawn(move || {
        let wait_result = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(100));
        if wait_terminal_seen.load(Ordering::SeqCst) {
            return;
        }

        let stderr_tail = wait_stderr_lines
            .lock()
            .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();

        let mut error = match wait_result {
            Ok(status) => format!("Worker exited before completion with status: {status}"),
            Err(e) => format!("Failed to wait for worker process: {e}"),
        };
        if !stderr_tail.trim().is_empty() {
            error.push_str("\n\nLast worker stderr:\n");
            error.push_str(stderr_tail.trim());
        }

        let _ = wait_events_tx.send(WorkerProgressUpdate::Error {
            task_id: gui_task_id,
            error,
            metrics: None,
        });
    });

    Ok(WorkerHandle {
        child: None,
        pid,
        task_id: gui_task_id,
        input_path,
        output_path,
        active_pids: None,
        cancelled: None,
    })
}

/// Probe a file using `lege --probe-json` and return the parsed JSON value.
/// Runs the subprocess on a blocking thread so the async executor is not touched.
pub async fn probe_file_json(path: &PathBuf) -> Result<serde_json::Value> {
    let cli_path = resolve_cli_path()?;
    let path = path.clone();
    let path_display = path.display().to_string();

    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&cli_path);
        cmd.arg("--probe-json");
        cmd.arg(&path);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        cmd.output()
    })
    .await??;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!(
            "Probe failed for '{}': status {}{}{}",
            path_display,
            output.status,
            if stderr.trim().is_empty() { "" } else { "\n" },
            stderr.trim()
        );
    }
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        anyhow::anyhow!(
            "Probe returned invalid JSON for '{}': {}{}{}",
            path_display,
            e,
            if stderr.trim().is_empty() { "" } else { "\n" },
            stderr.trim()
        )
    })?;
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::CompressionType;
    use std::ffi::OsStr;

    fn cli_arg_after(args: &[OsString], flag: &str) -> Option<String> {
        args.windows(2)
            .find(|window| window[0] == OsStr::new(flag))
            .map(|window| window[1].to_string_lossy().to_string())
    }

    #[test]
    fn no_layout_cli_ignores_hidden_image_output_format() {
        let mut options = ProcessingOptions::new();
        options.layout_analysis = false;
        options.compression_type = CompressionType::Ccitt4;
        options.image_processing_type = ImageProcessingType::Dithered;

        let args = gui_options_to_cli_args(
            &PathBuf::from("input.pdf"),
            &PathBuf::from("output.pdf"),
            &options,
            true,
        );

        assert_eq!(
            cli_arg_after(&args, "--text-format").as_deref(),
            Some("ccitt4")
        );
        assert!(args.iter().any(|arg| arg == OsStr::new("--no-layout")));
        assert!(!args.iter().any(|arg| arg == OsStr::new("--dither")));
    }

    #[test]
    fn layout_cli_uses_image_output_format() {
        let mut options = ProcessingOptions::new();
        options.layout_analysis = true;
        options.compression_type = CompressionType::Ccitt4;
        options.image_processing_type = ImageProcessingType::Dithered;

        let args = gui_options_to_cli_args(
            &PathBuf::from("input.pdf"),
            &PathBuf::from("output.pdf"),
            &options,
            true,
        );

        assert_eq!(
            cli_arg_after(&args, "--text-format").as_deref(),
            Some("jbig2")
        );
        assert!(!args.iter().any(|arg| arg == OsStr::new("--no-layout")));
        assert!(args.iter().any(|arg| arg == OsStr::new("--dither")));
    }

    #[test]
    fn layout_cli_uses_halftone_instead_of_standard_dither_when_enabled() {
        let mut options = ProcessingOptions::new();
        options.layout_analysis = true;
        options.image_processing_type = ImageProcessingType::Dithered;
        options.use_jbig2_halftone = true;

        let args = gui_options_to_cli_args(
            &PathBuf::from("input.pdf"),
            &PathBuf::from("output.pdf"),
            &options,
            true,
        );

        assert_eq!(
            cli_arg_after(&args, "--text-format").as_deref(),
            Some("jbig2")
        );
        assert!(args.iter().any(|arg| arg == OsStr::new("--halftone")));
        assert!(!args.iter().any(|arg| arg == OsStr::new("--dither")));
    }

    #[test]
    fn hidden_halftone_setting_is_ignored_for_djvu() {
        let mut options = ProcessingOptions::new();
        options.output_format = OutputFormat::Djvu;
        options.layout_analysis = true;
        options.image_processing_type = ImageProcessingType::Dithered;
        options.use_jbig2_halftone = true;

        let args = gui_options_to_cli_args(
            &PathBuf::from("input.pdf"),
            &PathBuf::from("output.djvu"),
            &options,
            true,
        );

        assert_eq!(
            cli_arg_after(&args, "--text-format").as_deref(),
            Some("djvu")
        );
        assert!(!args.iter().any(|arg| arg == OsStr::new("--halftone")));
        assert!(args.iter().any(|arg| arg == OsStr::new("--dither")));
    }

    #[test]
    fn crop_margins_cli_uses_free_aspect_crop() {
        let mut options = ProcessingOptions::new();
        options.crop_margins = true;

        let args = gui_options_to_cli_args(
            &PathBuf::from("input.pdf"),
            &PathBuf::from("output.pdf"),
            &options,
            true,
        );

        assert!(args.iter().any(|arg| arg == OsStr::new("--crop-margins")));
        assert!(
            args.iter()
                .any(|arg| arg == OsStr::new("--crop-free-aspect"))
        );
    }
}
