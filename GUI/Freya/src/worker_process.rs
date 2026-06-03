/// Subprocess-based processing worker for the GUI.
///
/// Each queue item spawns a hidden `lege` CLI process in `--gui-worker` mode.
/// Progress events arrive as newline-delimited JSON on the child's stdout and
/// are deserialized into local protocol types, then forwarded to a merged
/// flume channel that the GUI progress loop consumes exactly as before.
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::Result;

use crate::models::{
    CompressionType, CoverImageType, ImageProcessingType, OutputFormat, ProcessingOptions,
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
}

impl Default for WorkerProgressMode {
    fn default() -> Self { Self::Unknown }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
pub struct WorkerProgressMetrics {
    pub pages_total: u32,
    pub rendered: u32,
    pub detected: u32,
    pub encoded: u32,
    pub deskewed: u32,
    pub mode: WorkerProgressMode,
    pub is_djvu: bool,
    pub enable_layout_detection: bool,
    pub enable_deskew: bool,
    pub eta_seconds: Option<u32>,
}

/// Mirror of lege::progress::ProcessingStatus — variants must match the
/// `#[serde(tag = "kind", rename_all = "snake_case")]` representation.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerProcessingStatus {
    Initializing,
    AssemblingOutput,
    Complete { message: String },
    Error { error: String },
    FootnotesDetected { message: String },
    OcrLayerDetected { has_ocr: bool },
    MarginPass1Analyzing,
    MarginAnalysisSummary { summary: String },
    PdfAppend { current: usize, total: usize },
    PdfAppendMargin { current: usize, total: usize },
    LayoutProgress {
        rendered: usize,
        detected: usize,
        encoded: usize,
        deskewed: usize,
        total: usize,
        enable_layout_detection: bool,
        enable_deskew: bool,
        eta: Option<String>,
    },
    NoLayoutProgress {
        rendered: usize,
        encoded: usize,
        deskewed: usize,
        total: usize,
        enable_deskew: bool,
        eta: Option<String>,
    },
    MarginProgress {
        pass1_rendered: usize,
        pass1_detected: usize,
        pass2_processed: usize,
        deskewed: usize,
        total: usize,
        enable_layout_detection: bool,
        enable_deskew: bool,
        eta: Option<String>,
    },
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
            Self::Error { error } => (
                "[Error]".into(),
                "An error occurred.".into(),
                error.clone(),
            ),
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
                "Rendering: complete".into(),
                "Inference: complete | Analyzing document-wide margins...".into(),
            ),
            Self::MarginAnalysisSummary { summary } => (
                "[Margin Analysis - Pass 1]".into(),
                "Margin Analysis: complete".into(),
                summary.clone(),
            ),
            Self::PdfAppend { .. } | Self::PdfAppendMargin { .. } => {
                (String::new(), String::new(), String::new())
            }
            Self::LayoutProgress { .. } => {
                ("[Layout Mode]".into(), String::new(), String::new())
            }
            Self::NoLayoutProgress { .. } => {
                ("[No-Layout Mode]".into(), String::new(), String::new())
            }
            Self::MarginProgress { .. } => {
                ("[Margin Mode]".into(), String::new(), String::new())
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
            Self::Status { status, metrics, .. } => Self::Status { task_id: new_id, status, metrics },
            Self::Completed { message, metrics, .. } => Self::Completed { task_id: new_id, message, metrics },
            Self::Error { error, metrics, .. } => Self::Error { task_id: new_id, error, metrics },
        }
    }
}

// ── Target device profiles (copy of lege::target_profiles) ───────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetDeviceProfile {
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
}

pub const TARGET_DEVICE_PROFILES: &[TargetDeviceProfile] = &[
    TargetDeviceProfile { name: "Amazon Kindle (11th Gen, 2022)", width: 1072, height: 1448 },
    TargetDeviceProfile { name: "Amazon Kindle Paperwhite (11th Gen, 2021)", width: 1236, height: 1648 },
    TargetDeviceProfile { name: "Amazon Kindle Scribe (2022)", width: 1860, height: 2480 },
    TargetDeviceProfile { name: "B&N Nook GlowLight 4 (2021)", width: 1072, height: 1448 },
    TargetDeviceProfile { name: "B&N Nook GlowLight 4e (2022)", width: 758, height: 1024 },
    TargetDeviceProfile { name: "Bigme InkNote Color (2022)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Boyue Likebook P10 (2021)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Huawei MatePad Paper (2022)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Kobo Clara 2E (2022)", width: 1072, height: 1448 },
    TargetDeviceProfile { name: "Kobo Elipsa 2E (2023)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Kobo Libra 2 (2021)", width: 1264, height: 1680 },
    TargetDeviceProfile { name: "Kobo Sage (2021)", width: 1440, height: 1920 },
    TargetDeviceProfile { name: "Onyx Boox Leaf 2 (2022)", width: 1264, height: 1680 },
    TargetDeviceProfile { name: "Onyx Boox Note Air (2020)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Onyx Boox Nova Air 2 (2023)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Onyx Boox Nova3 Color (2021)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Onyx Boox Palma (2023)", width: 824, height: 1648 },
    TargetDeviceProfile { name: "Onyx Boox Tab Ultra (2022)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Onyx Boox Tab X (2023)", width: 1650, height: 2200 },
    TargetDeviceProfile { name: "PocketBook Color (2020)", width: 1072, height: 1448 },
    TargetDeviceProfile { name: "PocketBook Era (2022)", width: 1264, height: 1680 },
    TargetDeviceProfile { name: "PocketBook InkPad Color 2 (2023)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Ratta Supernote A5 X (2020)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Ratta Supernote A6 X (2020)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "reMarkable 2 (2020)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Tolino Epos 3 (2021)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Tolino Vision 6 (2021)", width: 1264, height: 1680 },
    TargetDeviceProfile { name: "Xiaomi Mi Reader Pro (2020)", width: 1404, height: 1872 },
];

pub const PROPORTIONAL_OPTION_LABEL: &str = "Set height, width proportional";

pub fn find_profile(name: &str) -> Option<TargetDeviceProfile> {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let needle = norm(name);
    TARGET_DEVICE_PROFILES
        .iter()
        .copied()
        .find(|p| p.name.eq_ignore_ascii_case(name) || norm(p.name) == needle)
}

// ── CLI arg generation ────────────────────────────────────────────────────────

/// Map GUI ProcessingOptions to CLI arguments for `lege --gui-worker`.
pub fn gui_options_to_cli_args(
    input_path: &PathBuf,
    output_path: &PathBuf,
    options: &ProcessingOptions,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();

    args.push("--gui-worker".into());
    args.push(input_path.as_os_str().into());
    args.push("--output".into());
    // Pass the full output file path; the CLI passes through paths with extensions.
    args.push(output_path.as_os_str().into());

    // Text / output format
    let text_format = if matches!(options.output_format, OutputFormat::Djvu) {
        "djvu"
    } else if options.layout_analysis {
        match options.image_processing_type {
            ImageProcessingType::Original => "ccitt4",
            ImageProcessingType::Dithered => "jbig2",
        }
    } else {
        match options.compression_type {
            CompressionType::Ccitt4 => "ccitt4",
            CompressionType::Jbig2 => "jbig2",
        }
    };
    args.push("--text-format".into());
    args.push(text_format.into());

    // Cover / image format
    let cover_format = match options.cover_image_type {
        CoverImageType::None => "none",
        _ => "jpeg",
    };
    if !matches!(options.output_format, OutputFormat::Djvu) {
        args.push("--cover-format".into());
        args.push(cover_format.into());
    }

    // Layout detection
    if !options.layout_analysis {
        args.push("--no-layout".into());
    }

    // OCR
    if options.use_ocr {
        args.push("--ocr".into());
    } else {
        args.push("--no-ocr".into());
    }

    // Deskew
    if options.deskew_documents {
        args.push("--deskew".into());
    }

    // Image processing
    if matches!(options.image_processing_type, ImageProcessingType::Dithered) && options.layout_analysis {
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

    // Target device / height
    if let Some(ref device) = options.target_device {
        if !device.is_empty() {
            args.push(OsString::from(device.as_str()));
        }
    } else if let Some(height) = options.target_height {
        args.push(OsString::from(height.to_string()));
    }

    // Page range
    if let Some(ref range) = options.page_range {
        if !range.trim().is_empty() {
            args.push(OsString::from(range.as_str()));
        }
    }

    args
}

// ── Worker handle and subprocess spawning ────────────────────────────────────

/// A running `lege --gui-worker` child process.
/// Uses `std::process::Child` so I/O threads are fully independent of the
/// async executor — preventing any interference with freya's render loop.
pub struct WorkerHandle {
    pub child: std::process::Child,
    pub task_id: u64,
    pub input_path: PathBuf,
    pub output_path: PathBuf,
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
    Ok(PathBuf::from(if cfg!(windows) { "lege.exe" } else { "lege" }))
}

/// Spawn a hidden `lege --gui-worker` process.
///
/// stdout and stderr are consumed on dedicated OS threads — completely
/// independent of tokio and freya's async executor.  This prevents any I/O
/// blocking from interfering with GUI rendering.
pub fn spawn_lege_worker(
    gui_task_id: u64,
    input_path: PathBuf,
    output_path: PathBuf,
    options: &ProcessingOptions,
    events_tx: flume::Sender<WorkerProgressUpdate>,
    log_tx: Option<flume::Sender<String>>,
) -> Result<WorkerHandle> {
    let cli_path = resolve_cli_path()?;
    let cli_args = gui_options_to_cli_args(&input_path, &output_path, options);

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

    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!("Failed to launch lege worker ({:?}): {}", cli_path, e)
    })?;

    // Dedicated thread: read stdout, parse JSON, forward events.
    let stdout = child.stdout.take().expect("stdout piped");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => match serde_json::from_str::<WorkerProgressUpdate>(&line) {
                    Ok(update) => {
                        let _ = events_tx.send(update.with_task_id(gui_task_id));
                    }
                    Err(e) => eprintln!("[worker] JSON parse error: {e} — {line}"),
                },
                Err(_) => break,
            }
        }
    });

    // Dedicated thread: drain stderr (forward to log or discard).
    let stderr = child.stderr.take().expect("stderr piped");
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            match line {
                Ok(line) => {
                    if let Some(ref tx) = log_tx {
                        let _ = tx.send(line);
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(WorkerHandle { child, task_id: gui_task_id, input_path, output_path })
}

/// Probe a file using `lege --probe-json` and return the parsed JSON value.
/// Runs the subprocess on a blocking thread so the async executor is not touched.
pub async fn probe_file_json(path: &PathBuf) -> Result<serde_json::Value> {
    let cli_path = resolve_cli_path()?;
    let path = path.clone();

    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&cli_path);
        cmd.arg("--probe-json");
        cmd.arg(&path);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());

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
    let json: serde_json::Value = serde_json::from_str(stdout.trim())?;
    Ok(json)
}
