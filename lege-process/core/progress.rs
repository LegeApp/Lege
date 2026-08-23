use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use crate::ShutdownSignal;
use anyhow::{Context, Result};
use flume::{Receiver, Sender};
use std::sync::atomic::Ordering as AtomicOrdering;
use tokio::sync::{Notify, broadcast};

/// Detailed processing states that can be surfaced to both CLI and GUI.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessingStatus {
    /// General start-up phase before any page work has begun.
    Initializing,
    /// Document is being assembled/written to disk.
    AssemblingOutput,
    /// Processing finished successfully with a human readable message.
    Complete {
        message: String,
    },
    /// Processing failed with a captured error string.
    Error {
        error: String,
    },

    // --- Special notifications ---
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
    /// Same as PdfAppend but keeps the header stable for Margin Mode to avoid flicker
    PdfAppendMargin {
        current: usize,
        total: usize,
    },
    ReflowProgress {
        stage: ReflowStage,
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

    // --- Progress variants for different modes ---
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflowStage {
    SourceAnalysis,
    Compose,
    OutputPages,
}

impl ProcessingStatus {
    /// Converts the status into three concise display lines for CLI (with ANSI color codes).
    /// This is for backward compatibility
    pub fn to_display_lines(&self) -> (String, String, String) {
        self.to_cli_display_lines()
    }

    /// Converts the status into three concise display lines for CLI (with ANSI color codes).
    pub fn to_cli_display_lines(&self) -> (String, String, String) {
        match self {
            Self::Initializing => (
                "[Initializing]".to_string(),
                "Preparing pipeline...".to_string(),
                String::new(),
            ),
            Self::AssemblingOutput => (
                "[Finalizing]".to_string(),
                "Assembling output file...".to_string(),
                "This may take a moment.".to_string(),
            ),
            Self::Complete { message } => (
                "[Complete]".to_string(),
                message.clone(),
                "Ready for next task.".to_string(),
            ),
            Self::Error { error } => (
                "[Error]".to_string(),
                "An error occurred.".to_string(),
                error.clone(),
            ),
            Self::FootnotesDetected { message } => (
                "[Margin Analysis]".to_string(),
                "Margin Analysis: note".to_string(),
                format!("{} | Footnotes preserved (centering used).", message),
            ),
            Self::OcrLayerDetected { has_ocr } => {
                if *has_ocr {
                    (
                        "[OCR Detection]".to_string(),
                        "OCR layer detected in document".to_string(),
                        "Leave OCR disabled to preserve existing text layer.".to_string(),
                    )
                } else {
                    (
                        "[OCR Detection]".to_string(),
                        "No OCR layer found".to_string(),
                        "Enable OCR if you want to add text layer.".to_string(),
                    )
                }
            }
            Self::MarginPass1Analyzing => (
                "[Margin Analysis - Pass 1]".to_string(),
                "Preparing document-wide margin analysis...".to_string(),
                "Progress will update as pages are analyzed.".to_string(),
            ),
            Self::MarginAnalysisSummary { summary } => (
                "[Margin Analysis - Pass 1]".to_string(),
                "Margin Analysis: complete".to_string(),
                summary.clone(),
            ),
            Self::PipelineMessage { stage, message } => {
                (format!("[{stage}]"), message.clone(), String::new())
            }
            Self::PdfAppend { current, total } => {
                let pct = if *total > 0 {
                    ((*current as f64 / *total as f64) * 100.0).round() as usize
                } else {
                    0
                };
                (
                    "[Encode]".to_string(),
                    format!(
                        "\x1b[95mPage encoded\x1b[0m: \x1b[92m{}/{}\x1b[0m ({}%)",
                        current,
                        total,
                        pct.min(100)
                    ),
                    String::new(),
                )
            }
            Self::PdfAppendMargin { current, total } => {
                let pct = if *total > 0 {
                    ((*current as f64 / *total as f64) * 100.0).round() as usize
                } else {
                    0
                };
                (
                    "[Encode+Margin]".to_string(),
                    format!(
                        "\x1b[95mPage encoded\x1b[0m: \x1b[92m{}/{}\x1b[0m ({}%)",
                        current,
                        total,
                        pct.min(100)
                    ),
                    String::new(),
                )
            }
            Self::ReflowProgress {
                stage,
                current,
                total,
                eta,
            } => {
                let total = *total;
                let current = (*current).min(total);
                let pct = if total > 0 {
                    ((current as f64 / total as f64) * 100.0).round() as usize
                } else {
                    0
                };
                let bar = mini_bar(current, total);
                let eta_line = eta
                    .as_ref()
                    .map(|eta| format!("Estimated time remaining: {eta}"))
                    .unwrap_or_default();
                match stage {
                    ReflowStage::SourceAnalysis => (
                        "[Reflow - Source Analysis]".to_string(),
                        format!(
                            "Rendering source pages and detecting layout: {bar} {current}/{total} ({pct}%)"
                        ),
                        eta_line,
                    ),
                    ReflowStage::Compose => (
                        "[Reflow - Compose]".to_string(),
                        "Building reflow plan from detected regions, rows, and words..."
                            .to_string(),
                        String::new(),
                    ),
                    ReflowStage::OutputPages => (
                        "[Reflow - Output Pages]".to_string(),
                        format!(
                            "Rasterizing and encoding reflowed output pages: {bar} {current}/{total} ({pct}%)"
                        ),
                        eta_line,
                    ),
                }
            }
            Self::EpubProgress {
                rendered,
                detected,
                ocr,
                total,
                eta: _,
            } => {
                let total = *total;
                let r = (*rendered).min(total);
                let d = (*detected).min(total);
                let o = (*ocr).min(total);

                let render_color = "\x1b[96m";
                let detect_color = "\x1b[93m";
                let ocr_color = "\x1b[95m";
                let reset = "\x1b[0m";

                let line1 = format!(
                    "{}Render{} {} {}/{} | {}Layout{} {} {}/{}",
                    render_color,
                    reset,
                    mini_bar(r, total),
                    r,
                    total,
                    detect_color,
                    reset,
                    mini_bar(d, total),
                    d,
                    total
                );
                let line2 = format!(
                    "{}OCR{} {} {}/{}",
                    ocr_color,
                    reset,
                    mini_bar(o, total),
                    o,
                    total
                );
                ("[EPUB]".to_string(), line1, line2)
            }
            Self::LayoutProgress {
                rendered,
                detected,
                encoded,
                total,
                enable_layout_detection: _,
                eta: _,
            } => {
                let total = *total;
                let r = (*rendered).min(total);
                let d = (*detected).min(total);
                let e = (*encoded).min(total);

                // Color codes for visual pleasure
                let render_color = "\x1b[96m"; // Bright cyan for render
                let detect_color = "\x1b[93m"; // Bright yellow for detect/infer
                let encode_color = "\x1b[95m"; // Bright magenta for encode
                let reset = "\x1b[0m";

                let line1 = format!(
                    "{}Render{} {} {}/{} | {}Infer{} {} {}/{}",
                    render_color,
                    reset,
                    mini_bar(r, total),
                    r,
                    total,
                    detect_color,
                    reset,
                    mini_bar(d, total),
                    d,
                    total
                );

                let line2 = format!(
                    "{}Encode{} {} {}/{}",
                    encode_color,
                    reset,
                    mini_bar(e, total),
                    e,
                    total
                );
                ("[Layout Mode]".to_string(), line1, line2)
            }
            Self::NoLayoutProgress {
                rendered,
                encoded,
                total,
                eta: _,
            } => {
                let total = *total;
                let r = (*rendered).min(total);
                let e = (*encoded).min(total);

                // Color codes for visual pleasure
                let render_color = "\x1b[96m"; // Bright cyan for render
                let encode_color = "\x1b[95m"; // Bright magenta for encode
                let reset = "\x1b[0m";

                let line1 = format!(
                    "{}Render{} {} {}/{} | {}Encode{} {} {}/{}",
                    render_color,
                    reset,
                    mini_bar(r, total),
                    r,
                    total,
                    encode_color,
                    reset,
                    mini_bar(e, total),
                    e,
                    total
                );

                ("[No-Layout Mode]".to_string(), line1, String::new())
            }
            Self::MarginProgress {
                pass1_rendered,
                pass1_detected,
                pass2_processed,
                total,
                enable_layout_detection,
                eta: _,
            } => {
                let total = *total;
                let r = (*pass1_rendered).min(total);
                let d = (*pass1_detected).min(total);
                let p2 = (*pass2_processed).min(total);

                if !enable_layout_detection {
                    // Margin no-layout mode: render + margin calculation (combined)
                    // For no-layout margin mode, margin calculation is integrated into rendering
                    let render_color = "\x1b[96m"; // Bright cyan
                    let reset = "\x1b[0m";

                    let render_progress = r.min(total);
                    let line1 = format!(
                        "{}Render{} {} {}/{}",
                        render_color,
                        reset,
                        mini_bar(render_progress, total),
                        render_progress,
                        total
                    );

                    ("[Margin Mode]".to_string(), line1, String::new())
                } else {
                    // Margin layout mode: render, inference, and separate margin calculation
                    // Color codes
                    let render_color = "\x1b[96m"; // Bright cyan
                    let detect_color = "\x1b[93m"; // Bright yellow
                    let pass2_color = "\x1b[95m"; // Bright magenta
                    let reset = "\x1b[0m";

                    let line1 = format!(
                        "{}Render{} {} {}/{} | {}Inference{} {} {}/{}",
                        render_color,
                        reset,
                        mini_bar(r, total),
                        r,
                        total,
                        detect_color,
                        reset,
                        mini_bar(d, total),
                        d,
                        total
                    );

                    // Show pass2/margin calculation on line 2
                    let line2 = if p2 > 0 {
                        let stage_label = "Pass 2";
                        let stage_progress = p2.min(total);
                        format!(
                            "{}{}{} {} {}/{}",
                            pass2_color,
                            stage_label,
                            reset,
                            mini_bar(stage_progress, total),
                            stage_progress,
                            total
                        )
                    } else {
                        // Show margin calc as before
                        let stage1 = r.min(d);
                        let stage_progress = stage1.min(total);
                        format!(
                            "{}Margin Calc{} {} {}/{}",
                            "\x1b[94m",
                            reset,
                            mini_bar(stage_progress, total),
                            stage_progress,
                            total
                        )
                    };

                    ("[Margin Mode]".to_string(), line1, line2)
                }
            }
        }
    }

    /// Converts the status into three concise display lines for GUI (NO ANSI color codes).
    pub fn to_gui_display_lines(&self) -> (String, String, String) {
        match self {
            Self::Initializing => (
                "[Initializing]".to_string(),
                "Preparing pipeline...".to_string(),
                String::new(),
            ),
            Self::AssemblingOutput => (
                "[Finalizing]".to_string(),
                "Assembling output file...".to_string(),
                "This may take a moment.".to_string(),
            ),
            Self::Complete { message } => (
                "[Complete]".to_string(),
                message.clone(),
                "Ready for next task.".to_string(),
            ),
            Self::Error { error } => (
                "[Error]".to_string(),
                "An error occurred.".to_string(),
                error.clone(),
            ),
            Self::FootnotesDetected { message } => (
                "[Margin Analysis]".to_string(),
                "Margin Analysis: note".to_string(),
                format!("{} | Footnotes preserved (centering used).", message),
            ),
            Self::OcrLayerDetected { has_ocr } => {
                if *has_ocr {
                    (
                        "[OCR Detection]".to_string(),
                        "OCR layer detected in document".to_string(),
                        "Leave OCR disabled to preserve existing text layer.".to_string(),
                    )
                } else {
                    (
                        "[OCR Detection]".to_string(),
                        "No OCR layer found".to_string(),
                        "Enable OCR if you want to add text layer.".to_string(),
                    )
                }
            }
            Self::MarginPass1Analyzing => (
                "[Margin Analysis - Pass 1]".to_string(),
                "Preparing document-wide margin analysis...".to_string(),
                "Progress will update as pages are analyzed.".to_string(),
            ),
            Self::MarginAnalysisSummary { summary } => (
                "[Margin Analysis - Pass 1]".to_string(),
                "Margin Analysis: complete".to_string(),
                summary.clone(),
            ),
            Self::PipelineMessage { stage, message } => {
                (format!("[{stage}]"), message.clone(), String::new())
            }
            // PdfAppend and PdfAppendMargin: These "page completed" messages are
            // suppressed to avoid flickering and align with the 3-part progress display pattern
            // (render/inference/encoding). Return empty strings to suppress them.
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
                    ReflowStage::SourceAnalysis => {
                        format!("Rendering source pages and detecting layout: {current}/{total}")
                    }
                    ReflowStage::Compose => {
                        "Building reflow plan from detected regions, rows, and words...".to_string()
                    }
                    ReflowStage::OutputPages => {
                        format!("Rasterizing and encoding reflowed output pages: {current}/{total}")
                    }
                };
                let title = match stage {
                    ReflowStage::SourceAnalysis => "[Reflow - Source Analysis]",
                    ReflowStage::Compose => "[Reflow - Compose]",
                    ReflowStage::OutputPages => "[Reflow - Output Pages]",
                };
                (
                    title.to_string(),
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
                    "[EPUB]".to_string(),
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
                enable_layout_detection: _,
                eta,
            } => {
                let detail = format!(
                    "Render {rendered}/{total} | Infer {detected}/{total} | Encode {encoded}/{total}"
                );
                (
                    "[Layout Mode]".to_string(),
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
                    "[No-Layout Mode]".to_string(),
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
                enable_layout_detection: _,
                eta,
            } => {
                let detail = format!(
                    "Analyze {pass1_rendered}/{total} | Infer {pass1_detected}/{total} | Process {pass2_processed}/{total}"
                );
                (
                    "[Margin Mode]".to_string(),
                    detail,
                    eta.as_ref()
                        .map(|eta| format!("Estimated time remaining: {eta}"))
                        .unwrap_or_default(),
                )
            }
        }
    }
}

fn mini_bar(done: usize, total: usize) -> String {
    let width = 18usize;
    if total == 0 {
        return "[------------------]".to_string();
    }
    let filled = ((done as f64 / total.max(1) as f64) * width as f64).round() as usize;
    let filled = filled.min(width);
    let mut s = String::with_capacity(width + 2);
    s.push('[');
    for i in 0..width {
        s.push(if i < filled { '█' } else { '·' });
    }
    s.push(']');
    s
}

/// Broadcast events emitted by the processing pipeline to both CLI and GUI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressUpdate {
    Status {
        task_id: u64,
        status: ProcessingStatus,
        metrics: Option<ProgressMetrics>,
    },
    Completed {
        task_id: u64,
        message: String,
        metrics: Option<ProgressMetrics>,
    },
    Error {
        task_id: u64,
        error: String,
        metrics: Option<ProgressMetrics>,
    },
}

/// Unified numeric snapshot so GUI and CLI can render without parsing strings.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProgressMetrics {
    pub pages_total: u32,
    pub rendered: u32,
    pub detected: u32,
    pub encoded: u32,
    pub mode: ProgressMode,
    pub is_djvu: bool,
    pub enable_layout_detection: bool,
    pub eta_seconds: Option<u32>,
}

impl Default for ProgressMetrics {
    fn default() -> Self {
        Self {
            pages_total: 0,
            rendered: 0,
            detected: 0,
            encoded: 0,
            mode: ProgressMode::Unknown,
            is_djvu: false,
            enable_layout_detection: false,
            eta_seconds: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ProgressMode {
    Unknown,
    NoLayout,
    Layout,
    Margin,
    HeavySequential,
    Reflow,
    Epub,
}

/// Unified numeric snapshot so GUI and CLI can render without parsing strings.
pub type ProcessingMetrics = ProgressMetrics;

/// Consolidated state for the progress tracker
struct TrackerState {
    metrics: ProgressMetrics,
    start_time: Option<Instant>,
    // Throttle for smoother updates - track last update time
    last_update: Option<Instant>,
}

/// Lightweight tracker that sends progress updates directly via flume.
/// No more internal atomics for progress; the state is held in a shared Mutex.
#[derive(Clone)]
pub struct ProgressTracker {
    task_id: u64,
    sender: Sender<ProgressUpdate>,
    is_done: Arc<AtomicBool>,
    // State is now consolidated into a single struct under a Mutex.
    // This ensures that every update sent is from a consistent snapshot.
    state: Arc<Mutex<TrackerState>>,
    // Minimum time between updates to avoid flickering
    update_throttle_ms: Arc<AtomicUsize>,
}

impl ProgressTracker {
    pub fn new(task_id: u64, sender: Sender<ProgressUpdate>) -> Self {
        Self {
            task_id,
            sender,
            is_done: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(TrackerState {
                metrics: ProgressMetrics::default(),
                start_time: None,
                last_update: None,
            })),
            update_throttle_ms: Arc::new(AtomicUsize::new(100)), // 100ms between updates by default
        }
    }

    pub fn task_id(&self) -> u64 {
        self.task_id
    }

    pub fn set_total_pages(&self, total: usize) {
        let mut state = self.state.lock().unwrap();
        state.metrics.pages_total = total as u32;
    }

    pub fn total_pages(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.metrics.pages_total as usize
    }

    pub fn set_mode_enum(&self, mode: ProgressMode) {
        let mut state = self.state.lock().unwrap();
        state.metrics.mode = mode;
    }

    pub fn set_output_is_djvu(&self, is_djvu: bool) {
        let mut state = self.state.lock().unwrap();
        state.metrics.is_djvu = is_djvu;
    }

    pub fn set_feature_flags(&self, enable_layout_detection: bool) {
        let mut state = self.state.lock().unwrap();
        state.metrics.enable_layout_detection = enable_layout_detection;
    }

    pub fn set_heavy_sequential_mode(&self) {
        let mut state = self.state.lock().unwrap();
        state.metrics.mode = ProgressMode::HeavySequential;
    }

    pub fn set_update_throttle(&self, ms: usize) {
        self.update_throttle_ms.store(ms, Ordering::Relaxed);
    }

    // --- Helper methods that operate on locked state ---

    fn ensure_started_nolock(&self, state: &mut TrackerState, work_has_begun: bool) {
        if work_has_begun && state.start_time.is_none() {
            state.start_time = Some(Instant::now());
        }
    }

    fn compute_eta_nolock(&self, state: &mut TrackerState) {
        let completed = match state.metrics.mode {
            ProgressMode::Layout | ProgressMode::Margin => state
                .metrics
                .rendered
                .min(state.metrics.detected)
                .min(state.metrics.encoded),
            ProgressMode::NoLayout
            | ProgressMode::HeavySequential
            | ProgressMode::Reflow
            | ProgressMode::Epub
            | ProgressMode::Unknown => state.metrics.encoded,
        } as usize;
        let total = state.metrics.pages_total as usize;

        if completed == 0 || total == 0 {
            state.metrics.eta_seconds = None;
            return;
        }

        if let Some(start) = state.start_time {
            let elapsed = start.elapsed();
            if elapsed < Duration::from_millis(800) {
                return;
            }
            let remaining = total.saturating_sub(completed);
            if remaining == 0 {
                return;
            }

            let secs_per_page = elapsed.as_secs_f64() / completed as f64;
            let eta_secs = secs_per_page * remaining as f64;
            state.metrics.eta_seconds = Some(eta_secs.round().clamp(0.0, u32::MAX as f64) as u32);
        }
    }

    fn should_send_update(&self, state: &TrackerState) -> bool {
        let throttle_ms = self.update_throttle_ms.load(Ordering::Relaxed);
        let min_duration = Duration::from_millis(throttle_ms as u64);

        match state.last_update {
            Some(last) => last.elapsed() >= min_duration,
            None => true, // Always send first update
        }
    }

    fn get_current_progress_status_nolock(&self, state: &TrackerState) -> ProcessingStatus {
        let m = &state.metrics;
        let total = m.pages_total as usize;
        if total == 0 {
            return ProcessingStatus::Initializing;
        }
        let eta = m
            .eta_seconds
            .map(|s| human_duration(Duration::from_secs(s as u64)));

        match m.mode {
            ProgressMode::NoLayout => ProcessingStatus::NoLayoutProgress {
                rendered: m.rendered as usize,
                encoded: m.encoded as usize,
                total,
                eta,
            },
            ProgressMode::Layout => ProcessingStatus::LayoutProgress {
                rendered: m.rendered as usize,
                detected: m.detected as usize,
                encoded: m.encoded as usize,
                total,
                enable_layout_detection: m.enable_layout_detection,
                eta,
            },
            ProgressMode::Margin => ProcessingStatus::MarginProgress {
                pass1_rendered: m.rendered as usize,
                pass1_detected: m.detected as usize,
                pass2_processed: m.encoded as usize,
                total,
                enable_layout_detection: m.enable_layout_detection,
                eta,
            },
            ProgressMode::HeavySequential => ProcessingStatus::NoLayoutProgress {
                rendered: m.rendered as usize,
                encoded: m.encoded as usize,
                total,
                eta,
            }, // Heavy sequential behaves like no-layout but with more consistent updates
            ProgressMode::Reflow => ProcessingStatus::ReflowProgress {
                stage: ReflowStage::OutputPages,
                current: m.encoded as usize,
                total,
                eta,
            },
            ProgressMode::Epub => ProcessingStatus::EpubProgress {
                rendered: m.rendered as usize,
                detected: m.detected as usize,
                ocr: m.encoded as usize,
                total,
                eta,
            },
            ProgressMode::Unknown => ProcessingStatus::Initializing,
        }
    }

    fn current_metrics(&self) -> ProgressMetrics {
        let state = self.state.lock().unwrap();
        state.metrics
    }

    pub fn publish_layout_progress(
        &self,
        rendered: usize,
        detected: usize,
        encoded: usize,
        total: usize,
    ) {
        let mut state = self.state.lock().unwrap();

        // Only update if values are actually different or if it's a significant change
        let old_rendered = state.metrics.rendered;
        let old_detected = state.metrics.detected;
        let old_encoded = state.metrics.encoded;

        // Update metrics. Pipeline stages run concurrently, so a later callback can
        // carry an older snapshot of another stage. Keep each counter monotonic.
        state.metrics.pages_total = total as u32;
        state.metrics.rendered = state.metrics.rendered.max(rendered as u32);
        state.metrics.detected = state.metrics.detected.max(detected as u32);
        state.metrics.encoded = state.metrics.encoded.max(encoded as u32);

        if state.metrics.is_djvu {
            state.metrics.rendered = state.metrics.rendered.min(state.metrics.detected);
        }

        // Consider any stage starting as the beginning of work to improve ETA stability
        self.ensure_started_nolock(&mut state, rendered > 0 || detected > 0 || encoded > 0);
        self.compute_eta_nolock(&mut state);

        // Check if this is a meaningful progress update
        let is_meaningful_change = state.metrics.rendered != old_rendered
            || state.metrics.detected != old_detected
            || state.metrics.encoded != old_encoded
            || state.last_update.is_none(); // Always send first update

        if !is_meaningful_change {
            return; // Skip if no meaningful change
        }

        // Check if we should send an update based on throttle
        if !self.should_send_update(&state) {
            return; // Skip update to avoid flickering
        }

        // Update last update time
        state.last_update = Some(Instant::now());

        let status = self.get_current_progress_status_nolock(&state);
        let metrics = state.metrics;
        drop(state); // Release lock before sending
        self.send_update(status, metrics);
    }

    pub fn publish_epub_progress(
        &self,
        rendered: usize,
        detected: usize,
        ocr: usize,
        total: usize,
    ) {
        self.publish_layout_progress(rendered, detected, ocr, total);
    }

    pub fn publish_no_layout_progress(&self, processed: usize, total: usize) {
        let mut state = self.state.lock().unwrap();

        // Only update if values are actually different
        let old_encoded = state.metrics.encoded;

        // Update metrics. No-layout processing uses separate render and encode
        // callbacks; this method represents completed encoding.
        state.metrics.pages_total = total as u32;
        let processed = processed as u32;
        state.metrics.rendered = state.metrics.rendered.max(processed);
        state.metrics.detected = state.metrics.detected.max(processed);
        state.metrics.encoded = state.metrics.encoded.max(processed);

        self.ensure_started_nolock(&mut state, processed > 0);
        self.compute_eta_nolock(&mut state);

        // Check if this is a meaningful progress update
        let is_meaningful_change =
            state.metrics.encoded != old_encoded || state.last_update.is_none(); // Always send first update

        if !is_meaningful_change {
            return; // Skip if no meaningful change
        }

        // Check if we should send an update based on throttle
        if !self.should_send_update(&state) {
            return; // Skip update to avoid flickering
        }

        // Update last update time
        state.last_update = Some(Instant::now());

        let status = self.get_current_progress_status_nolock(&state);
        let metrics = state.metrics;
        drop(state); // Release lock before sending
        self.send_update(status, metrics);
    }

    pub fn publish_no_layout_render_progress(&self, rendered: usize, total: usize) {
        let mut state = self.state.lock().unwrap();

        let old_rendered = state.metrics.rendered;

        state.metrics.pages_total = total as u32;
        state.metrics.rendered = state.metrics.rendered.max(rendered as u32);

        self.ensure_started_nolock(&mut state, rendered > 0);
        self.compute_eta_nolock(&mut state);

        let is_meaningful_change =
            state.metrics.rendered != old_rendered || state.last_update.is_none();

        if !is_meaningful_change {
            return;
        }

        if !self.should_send_update(&state) {
            return;
        }

        state.last_update = Some(Instant::now());

        let status = self.get_current_progress_status_nolock(&state);
        let metrics = state.metrics;
        drop(state);
        self.send_update(status, metrics);
    }

    pub fn publish_margin_progress(
        &self,
        pass1_rendered: usize,
        pass1_detected: usize,
        pass2_processed: usize,
        total: usize,
    ) {
        let mut state = self.state.lock().unwrap();

        // Only update if values are actually different
        let old_rendered = state.metrics.rendered;
        let old_detected = state.metrics.detected;
        let old_encoded = state.metrics.encoded;

        // Update metrics. Margin stages may publish with stale counts from other
        // concurrent stages, so preserve the highest visible value per counter.
        state.metrics.pages_total = total as u32;
        state.metrics.rendered = state.metrics.rendered.max(pass1_rendered as u32);
        state.metrics.detected = state.metrics.detected.max(pass1_detected as u32);
        state.metrics.encoded = state.metrics.encoded.max(pass2_processed as u32); // 'encoded' maps to pass2

        // Start timing as soon as we make visible progress in any pass
        self.ensure_started_nolock(
            &mut state,
            pass1_rendered > 0 || pass1_detected > 0 || pass2_processed > 0,
        );
        self.compute_eta_nolock(&mut state);

        // Check if this is a meaningful progress update
        let is_meaningful_change = state.metrics.rendered != old_rendered
            || state.metrics.detected != old_detected
            || state.metrics.encoded != old_encoded
            || state.last_update.is_none(); // Always send first update

        if !is_meaningful_change {
            return; // Skip if no meaningful change
        }

        // Check if we should send an update based on throttle
        if !self.should_send_update(&state) {
            return; // Skip update to avoid flickering
        }

        // Update last update time
        state.last_update = Some(Instant::now());

        let status = self.get_current_progress_status_nolock(&state);
        let metrics = state.metrics;
        drop(state); // Release lock before sending
        self.send_update(status, metrics);
    }

    /// Updates progress specifically for sequential or heavy processing modes like heavy sauvola
    /// where progress is more linear but potentially slower
    pub fn publish_heavy_processing_progress(
        &self,
        current: usize,
        total: usize,
        processed_pages: Option<usize>,
    ) {
        let mut state = self.state.lock().unwrap();

        let old_encoded = state.metrics.encoded;

        // Update the main total
        state.metrics.pages_total = total as u32;

        // For heavy processing modes, we'll update the 'encoded' metric
        // (which represents completed work) and potentially all stages for full progress
        let completed = current as u32;
        state.metrics.rendered = state.metrics.rendered.max(completed);
        state.metrics.detected = state.metrics.detected.max(completed);
        state.metrics.encoded = state.metrics.encoded.max(completed);

        // If specific page progress is provided, use that instead
        if let Some(pages_completed) = processed_pages {
            let page_count = pages_completed as u32;
            state.metrics.encoded = state.metrics.encoded.max(page_count);
            // Also update other metrics accordingly for a more accurate display
            if state.metrics.encoded > state.metrics.rendered {
                state.metrics.rendered = state.metrics.encoded;
            }
            if state.metrics.encoded > state.metrics.detected {
                state.metrics.detected = state.metrics.encoded;
            }
        }

        self.ensure_started_nolock(&mut state, completed > 0);
        self.compute_eta_nolock(&mut state);

        // Check if this is a meaningful progress update
        let is_meaningful_change =
            state.metrics.encoded != old_encoded || state.last_update.is_none(); // Always send first update

        if !is_meaningful_change {
            return; // Skip if no meaningful change
        }

        // Check if we should send an update based on throttle
        if !self.should_send_update(&state) {
            return; // Skip update to avoid flickering
        }

        // Update last update time
        state.last_update = Some(Instant::now());

        let status = self.get_current_progress_status_nolock(&state);
        let metrics = state.metrics;
        drop(state); // Release lock before sending
        self.send_update(status, metrics);
    }

    pub fn publish_reflow_progress(&self, stage: ReflowStage, current: usize, total: usize) {
        let mut state = self.state.lock().unwrap();

        let old_encoded = state.metrics.encoded;
        let old_total = state.metrics.pages_total;
        let old_mode = state.metrics.mode;

        state.metrics.mode = ProgressMode::Reflow;
        state.metrics.pages_total = total as u32;
        let current = current.min(total) as u32;
        match stage {
            ReflowStage::SourceAnalysis => {
                state.metrics.rendered = current;
                state.metrics.detected = current;
                state.metrics.encoded = current;
            }
            ReflowStage::Compose => {
                state.metrics.rendered = 0;
                state.metrics.detected = 0;
                state.metrics.encoded = 0;
            }
            ReflowStage::OutputPages => {
                state.metrics.rendered = current;
                state.metrics.detected = current;
                state.metrics.encoded = current;
            }
        }

        self.ensure_started_nolock(&mut state, current > 0);
        self.compute_eta_nolock(&mut state);

        let is_meaningful_change = state.metrics.encoded != old_encoded
            || state.metrics.pages_total != old_total
            || state.metrics.mode != old_mode
            || state.last_update.is_none();

        if !is_meaningful_change {
            return;
        }

        if !self.should_send_update(&state) {
            return;
        }

        state.last_update = Some(Instant::now());
        let eta = state
            .metrics
            .eta_seconds
            .map(|s| human_duration(Duration::from_secs(s as u64)));
        let status = ProcessingStatus::ReflowProgress {
            stage,
            current: current as usize,
            total,
            eta,
        };
        let metrics = state.metrics;
        drop(state);
        self.send_update(status, metrics);
    }

    // --- Communication methods ---

    pub fn update(&self, status: ProcessingStatus) {
        let metrics = self.current_metrics();
        self.send_update(status, metrics);
    }

    fn send_update(&self, status: ProcessingStatus, metrics: ProgressMetrics) {
        if self.is_done.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = self.sender.send(ProgressUpdate::Status {
            task_id: self.task_id,
            status,
            metrics: Some(metrics),
        }) {
            eprintln!("warning: failed to send progress update: {e}");
        }
    }

    pub fn finish(&self, message: &str) {
        if self.is_done.swap(true, Ordering::Relaxed) {
            return;
        }

        let metrics = {
            let mut state = self.state.lock().unwrap();
            let total = state.metrics.pages_total;
            state.metrics.rendered = total;
            state.metrics.detected = total;
            state.metrics.encoded = total;
            state.metrics.eta_seconds = None;
            state.metrics
        };

        let message = message.to_string();
        self.update(ProcessingStatus::Complete {
            message: message.clone(),
        });
        let _ = self.sender.send(ProgressUpdate::Completed {
            task_id: self.task_id,
            message,
            metrics: Some(metrics),
        });
    }

    pub fn finish_with_error(&self, error: anyhow::Error) {
        if self.is_done.swap(true, Ordering::Relaxed) {
            return;
        }

        let metrics = self.state.lock().unwrap().metrics;
        let error_text = error.to_string();
        self.update(ProcessingStatus::Error {
            error: error_text.clone(),
        });
        let _ = self.sender.send(ProgressUpdate::Error {
            task_id: self.task_id,
            error: error_text,
            metrics: Some(metrics),
        });
    }
}

/// ProgressManager manages the flume channel and creates trackers.
/// No more complex polling tasks - trackers send updates directly.
#[derive(Clone)]
pub struct ProgressManager {
    // REPLACED broadcast::Sender with flume::Sender
    sender: Sender<ProgressUpdate>,
    // The receiver is held here and cloned for new subscribers.
    receiver: Receiver<ProgressUpdate>,
    next_task_id: Arc<Mutex<u64>>,
}

impl ProgressManager {
    pub fn new() -> Self {
        // Create an unbounded flume channel.
        // For backpressure, you could use `flume::bounded(256)`.
        let (sender, receiver) = flume::unbounded();
        Self {
            sender,
            receiver,
            next_task_id: Arc::new(Mutex::new(1)),
        }
    }

    pub fn create_tracker(&self) -> ProgressTracker {
        let task_id = {
            let mut guard = self.next_task_id.lock().unwrap();
            let current = *guard;
            *guard += 1;
            current
        };

        // NO MORE `tokio::spawn` PER TRACKER!
        // The tracker is now a simple, lightweight struct.
        ProgressTracker::new(task_id, self.sender.clone())
    }

    // The GUI will call this once to get the receiver.
    pub fn subscribe(&self) -> Receiver<ProgressUpdate> {
        self.receiver.clone()
    }
}

struct ProcessingJob {
    input_path: PathBuf,
    output_path: PathBuf,
    config: crate::PipelineConfig,
    tracker: ProgressTracker,
    shutdown_rx: tokio::sync::broadcast::Receiver<ShutdownSignal>,
}

impl ProcessingJob {
    async fn run(self) {
        let tracker = self.tracker.clone();
        let task_id = tracker.task_id();

        // Set appropriate update throttle based on processing mode
        // Heavy sequential processing might need more frequent updates due to slower per-page processing
        if self.config.binarization().use_heavy_duty {
            tracker.set_heavy_sequential_mode();
            // For heavy sequential processing, use a shorter throttle period
            tracker.set_update_throttle(50); // 50ms for heavy processing
        } else {
            // For other modes, use the default 100ms
            tracker.set_update_throttle(100);
        }

        // Process the file with cancellation support
        match process_file_with_tracker(
            self.input_path,
            self.output_path,
            self.config,
            tracker.clone(),
            self.shutdown_rx,
        )
        .await
        {
            Ok(message) => tracker.finish(&message),
            Err(error) => tracker.finish_with_error(error),
        }

        // Clean up cancel sender registration
        clear_cancel_sender(task_id);
    }
}

struct ProcessingQueue {
    queue: Mutex<VecDeque<ProcessingJob>>,
    notify: Notify,
}

impl ProcessingQueue {
    fn new() -> Arc<Self> {
        let queue = Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        });
        let worker = queue.clone();
        std::thread::spawn(move || {
            let runtime = crate::runtime_stats::build_control_runtime()
                .expect("failed to create processing queue runtime");
            runtime.block_on(async move {
                worker.worker_loop().await;
            });
        });
        queue
    }

    fn enqueue(&self, job: ProcessingJob) {
        {
            let mut guard = self.queue.lock().expect("processing queue mutex poisoned");
            guard.push_back(job);
        }
        self.notify.notify_one();
    }

    /// Attempt to cancel a queued job by task id; if found, remove and mark tracker aborted.
    fn cancel_queued(&self, task_id: u64) -> bool {
        let mut guard = self.queue.lock().expect("processing queue mutex poisoned");
        if let Some(pos) = guard.iter().position(|j| j.tracker.task_id() == task_id) {
            let job = guard.remove(pos).expect("queue removal failed");
            clear_cancel_sender(task_id);
            job.tracker
                .finish_with_error(anyhow::anyhow!("Operation aborted"));
            true
        } else {
            false
        }
    }

    fn cancel_all_queued(&self) -> usize {
        let mut guard = self.queue.lock().expect("processing queue mutex poisoned");
        let jobs = guard.drain(..).collect::<Vec<_>>();
        let count = jobs.len();
        drop(guard);
        for job in jobs {
            clear_cancel_sender(job.tracker.task_id());
            job.tracker
                .finish_with_error(anyhow::anyhow!("Operation aborted"));
        }
        count
    }

    async fn worker_loop(self: Arc<Self>) {
        loop {
            let job_opt = {
                let mut guard = self.queue.lock().expect("processing queue mutex poisoned");
                guard.pop_front()
            };

            if let Some(job) = job_opt {
                job.run().await;
            } else {
                self.notify.notified().await;
            }
        }
    }
}

static PROGRESS_MANAGER: std::sync::OnceLock<ProgressManager> = std::sync::OnceLock::new();
static PROCESSING_QUEUE: std::sync::OnceLock<Arc<ProcessingQueue>> = std::sync::OnceLock::new();
static CANCEL_REGISTRY: std::sync::OnceLock<
    Mutex<HashMap<u64, broadcast::Sender<ShutdownSignal>>>,
> = std::sync::OnceLock::new();
static TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);
static TERMINATION_SIGNAL_PENDING: AtomicBool = AtomicBool::new(false);
static TERMINATION_HANDLER_INSTALLED: std::sync::Once = std::sync::Once::new();

pub fn get_progress_manager() -> &'static ProgressManager {
    PROGRESS_MANAGER.get_or_init(ProgressManager::new)
}

fn get_processing_queue() -> &'static Arc<ProcessingQueue> {
    PROCESSING_QUEUE.get_or_init(ProcessingQueue::new)
}

fn get_cancel_registry() -> &'static Mutex<HashMap<u64, broadcast::Sender<ShutdownSignal>>> {
    CANCEL_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a cancel sender for a running task so UI can request cancellation.
pub fn register_cancel_sender(task_id: u64, sender: broadcast::Sender<ShutdownSignal>) {
    let mut reg = get_cancel_registry()
        .lock()
        .expect("cancel registry poisoned");
    reg.insert(task_id, sender);
}

/// Clear the cancel sender registration once the task is done.
pub fn clear_cancel_sender(task_id: u64) {
    let mut reg = get_cancel_registry()
        .lock()
        .expect("cancel registry poisoned");
    reg.remove(&task_id);
}

/// Request cancellation for a given task id.
/// Returns true if a cancel signal was sent or a queued job was removed.
pub fn cancel_task(task_id: u64) -> bool {
    // Check cancel registry first to signal running jobs
    // We do this before checking the queue to handle the race where a job
    // transitions from queued->running between our checks
    let sent_signal = {
        let reg = get_cancel_registry()
            .lock()
            .expect("cancel registry poisoned");
        if let Some(sender) = reg.get(&task_id) {
            let _ = sender.send(ShutdownSignal {
                reason: crate::ShutdownReason::UserCancellation,
                message: Some("Processing cancelled by user".to_string()),
            });
            true
        } else {
            false
        }
    };

    // Also try to remove from queue if not started yet
    // This handles jobs that haven't been picked up by the worker yet
    let removed_from_queue = get_processing_queue().cancel_queued(task_id);

    // Return true if we either sent a signal or removed from queue
    sent_signal || removed_from_queue
}

pub fn termination_requested() -> bool {
    TERMINATION_REQUESTED.load(AtomicOrdering::Acquire)
}

pub fn cancellation_checkpoint(stage: &str) -> anyhow::Result<()> {
    if termination_requested() {
        anyhow::bail!("Processing cancelled {stage}");
    }
    Ok(())
}

pub fn request_global_shutdown(message: &str) -> usize {
    TERMINATION_REQUESTED.store(true, AtomicOrdering::Release);
    let senders = {
        let registry = get_cancel_registry()
            .lock()
            .expect("cancel registry poisoned");
        registry.values().cloned().collect::<Vec<_>>()
    };
    for sender in &senders {
        let _ = sender.send(ShutdownSignal {
            reason: crate::ShutdownReason::UserCancellation,
            message: Some(message.to_string()),
        });
    }
    senders.len() + get_processing_queue().cancel_all_queued()
}

#[cfg(unix)]
extern "C" fn mark_termination_signal(_signal: libc::c_int) {
    TERMINATION_REQUESTED.store(true, AtomicOrdering::Release);
    TERMINATION_SIGNAL_PENDING.store(true, AtomicOrdering::Release);
}

#[cfg(windows)]
unsafe extern "system" fn mark_console_termination(control_type: u32) -> windows_core::BOOL {
    use windows::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    if matches!(
        control_type,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        TERMINATION_REQUESTED.store(true, AtomicOrdering::Release);
        TERMINATION_SIGNAL_PENDING.store(true, AtomicOrdering::Release);
        true.into()
    } else {
        false.into()
    }
}

/// Install the CLI termination bridge. The platform callback only flips
/// atomics; a normal Rust thread performs channel sends and cleanup requests.
pub fn install_termination_handler() {
    #[cfg(unix)]
    TERMINATION_HANDLER_INSTALLED.call_once(|| {
        // SAFETY: `mark_termination_signal` is an async-signal-safe handler that only
        // stores to a lock-free atomic.
        unsafe {
            let handler = mark_termination_signal as *const () as libc::sighandler_t;
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
        std::thread::Builder::new()
            .name("lege-signal".to_string())
            .spawn(|| {
                loop {
                    if TERMINATION_SIGNAL_PENDING.swap(false, AtomicOrdering::AcqRel) {
                        let _ = request_global_shutdown("Processing cancelled by shutdown signal");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
            .expect("failed to spawn termination bridge");
    });

    #[cfg(windows)]
    TERMINATION_HANDLER_INSTALLED.call_once(|| {
        // SAFETY: the callback only performs lock-free atomic stores and stays
        // registered for the lifetime of the process.
        unsafe {
            let _ = windows::Win32::System::Console::SetConsoleCtrlHandler(
                Some(mark_console_termination),
                true,
            );
        }
        std::thread::Builder::new()
            .name("lege-signal".to_string())
            .spawn(|| {
                loop {
                    if TERMINATION_SIGNAL_PENDING.swap(false, AtomicOrdering::AcqRel) {
                        let _ = request_global_shutdown("Processing cancelled by shutdown signal");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
            .expect("failed to spawn termination bridge");
    });
}

/// Compatibility name retained for callers outside the workspace.
pub fn install_sigterm_handler() {
    install_termination_handler();
}

pub fn spawn_file_processing_task(
    input_path: PathBuf,
    output_path: PathBuf,
    config: crate::PipelineConfig,
) -> u64 {
    let manager = get_progress_manager();
    let tracker = manager.create_tracker();
    let task_id = tracker.task_id();
    // Register before enqueueing. Otherwise cancellation can land after the
    // queue worker pops the job but before `ProcessingJob::run` registers its
    // sender, losing the request entirely.
    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
    register_cancel_sender(task_id, shutdown_tx);

    let job = ProcessingJob {
        input_path,
        output_path,
        config,
        tracker,
        shutdown_rx,
    };
    get_processing_queue().enqueue(job);

    task_id
}

async fn process_file_with_tracker(
    input_path: PathBuf,
    output_path: PathBuf,
    config: crate::PipelineConfig,
    tracker: ProgressTracker,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<crate::ShutdownSignal>,
) -> Result<String> {
    if let Ok(signal) = shutdown_rx.try_recv() {
        return Err(anyhow::anyhow!(
            "Processing cancelled before startup: {:?}",
            signal
        ));
    }

    tracker.update(ProcessingStatus::Initializing);

    crate::debug_log::clear_debug_log();

    // Unified dependency preflight (runs once per file start)
    if let Err(e) = unified_dependency_preflight(&config) {
        return Err(e);
    }

    // Determine the appropriate progress mode based on configuration
    let mode = if config.text_format() == "epub" {
        ProgressMode::Epub
    } else if config.margin_settings() != crate::margin::MarginSettings::None {
        ProgressMode::Margin
    } else if config.enable_layout_detection() {
        ProgressMode::Layout
    } else if config.binarization().use_heavy_duty {
        // Heavy Sauvola runs sequentially and needs special progress handling
        ProgressMode::HeavySequential
    } else {
        ProgressMode::NoLayout
    };
    tracker.set_mode_enum(mode);
    tracker.set_output_is_djvu(config.text_format() == "djvu");

    tracker.set_feature_flags(config.enable_layout_detection());

    let pdf_bytes = std::fs::read(&input_path)
        .with_context(|| format!("Failed to read input PDF: {}", input_path.display()))?;
    let pipeline = crate::ProcessingPipeline::new(pdf_bytes, config.clone())?;

    let doc_pages = pipeline.get_page_count();
    let intended_pages = config
        .page_range()
        .map(|r| r.page_count())
        .unwrap_or(doc_pages);
    let total_pages = intended_pages.min(doc_pages).max(1);
    tracker.set_total_pages(total_pages);

    pipeline
        .process_document_dag(&output_path, &tracker, shutdown_rx, |_current, _total| {
            // Progress is tracked via publish_*_progress methods, no need for callback updates
        })
        .await?;

    // DJVU assembly can continue after page encoding has reached total; suppressing
    // a separate "finalizing" phase keeps progress aligned with the 3-stage model.
    if config.text_format() != "djvu" {
        tracker.update(ProcessingStatus::AssemblingOutput);
    }
    Ok(format!(
        "Successfully processed {} pages to {}",
        total_pages,
        output_path.display()
    ))
}

fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}h{:02}m{:02}s", h, m, s)
    } else if m > 0 {
        format!("{:02}m{:02}s", m, s)
    } else {
        format!("{:02}s", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> ProgressTracker {
        let (sender, _receiver) = flume::unbounded();
        ProgressTracker::new(1, sender)
    }

    #[test]
    fn no_layout_render_progress_does_not_advance_encode_counter() {
        let tracker = tracker();
        tracker.set_mode_enum(ProgressMode::NoLayout);

        tracker.publish_no_layout_render_progress(12, 20);
        let metrics = tracker.current_metrics();

        assert_eq!(metrics.rendered, 12);
        assert_eq!(metrics.encoded, 0);

        tracker.publish_no_layout_progress(3, 20);
        let metrics = tracker.current_metrics();

        assert_eq!(metrics.rendered, 12);
        assert_eq!(metrics.encoded, 3);
    }

    #[test]
    fn stage_counters_do_not_decrement_from_stale_updates() {
        let tracker = tracker();
        tracker.set_mode_enum(ProgressMode::Layout);

        tracker.publish_layout_progress(10, 8, 6, 20);
        tracker.publish_layout_progress(7, 9, 4, 20);
        let metrics = tracker.current_metrics();

        assert_eq!(metrics.rendered, 10);
        assert_eq!(metrics.detected, 9);
        assert_eq!(metrics.encoded, 6);
    }

    #[test]
    fn finish_marks_pages_complete() {
        let tracker = tracker();
        tracker.set_total_pages(20);
        tracker.set_feature_flags(false);

        tracker.finish("done");
        let metrics = tracker.current_metrics();

        assert_eq!(metrics.rendered, 20);
        assert_eq!(metrics.encoded, 20);
    }

    #[test]
    fn epub_progress_reports_ocr_instead_of_encode() {
        let (sender, receiver) = flume::unbounded();
        let tracker = ProgressTracker::new(1, sender);
        tracker.set_mode_enum(ProgressMode::Epub);

        tracker.publish_epub_progress(3, 2, 1, 5);

        let update = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("progress update should be sent");
        let ProgressUpdate::Status {
            status, metrics, ..
        } = update
        else {
            panic!("expected status update");
        };
        assert_eq!(metrics.expect("metrics").mode, ProgressMode::Epub);

        let ProcessingStatus::EpubProgress {
            rendered,
            detected,
            ocr,
            total,
            ..
        } = status
        else {
            panic!("expected EPUB progress status");
        };
        assert_eq!((rendered, detected, ocr, total), (3, 2, 1, 5));

        let (_title, _render_line, ocr_line) = ProcessingStatus::EpubProgress {
            rendered,
            detected,
            ocr,
            total,
            eta: None,
        }
        .to_cli_display_lines();
        assert!(ocr_line.contains("OCR"));
        assert!(!ocr_line.contains("Encode"));
    }

    #[test]
    fn cancellation_sender_is_live_before_a_job_starts() {
        let task_id = u64::MAX - 17;
        let (sender, mut receiver) = tokio::sync::broadcast::channel(1);
        register_cancel_sender(task_id, sender);

        assert!(cancel_task(task_id));
        let signal = receiver.try_recv().expect("queued cancellation signal");
        assert_eq!(signal.reason, crate::ShutdownReason::UserCancellation);

        clear_cancel_sender(task_id);
    }
}

// Perform unified dependency checks (OCR if requested, DJVU runtime readiness if needed)
fn unified_dependency_preflight(config: &crate::PipelineConfig) -> Result<()> {
    let mut missing: Vec<String> = Vec::new();

    // OCR preflight if enabled. PaddleOCR's assets are embedded, but model
    // parsing alone does not prove that the selected GPU can compile and run
    // them. Execute a tiny detector + recognizer probe before rendering pages.
    if config.enable_ocr() {
        #[cfg(lege_paddle_ocr)]
        lege_ocr::engine_paddle::PaddleOcrEngine::preflight_embedded().map_err(|e| {
            anyhow::anyhow!(
                "PaddleOCR could not start on this system. OCR processing was not started.\n{e:#}"
            )
        })?;

        #[cfg(all(
            any(target_os = "linux", target_os = "macos"),
            feature = "tesseract-ocr",
            not(feature = "paddle-ocr")
        ))]
        if let Err(e) = crate::ocr::check_tesseract_availability_for_language(config.ocr_language())
        {
            return Err(anyhow::anyhow!(
                "Missing OCR language data '{}.traineddata': {}",
                config.ocr_language(),
                e
            ));
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if crate::is_ocr_available() == false {
            missing.push("tesseract".to_string());
        }
    }

    // DJVU preflight: native encoding has no helper executable, but fail early
    // if the optional debug-output directory cannot be prepared.
    if config.text_format() == "djvu" {
        let temp_dir = crate::app_dirs::djvu_base_dir();
        let djvu_cfg = crate::djvu::DjvuConfig {
            work_dir: temp_dir,
            ..Default::default()
        };
        match crate::djvu::DjvuOrchestrator::new(djvu_cfg) {
            Ok(orchestrator) => {
                if let Err(e) = orchestrator.preflight_check(config.enable_ocr()) {
                    return Err(anyhow::anyhow!(
                        "DjVu output was selected, but native encoder setup failed.\n{e}"
                    ));
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "DjVu output was selected, but the DjVu encoder could not be initialized: {e}"
                ));
            }
        }
    }

    if !missing.is_empty() {
        return Err(anyhow::anyhow!(
            "Missing required component(s): {} (install or bundle before processing)",
            missing.join(", ")
        ));
    }
    Ok(())
}
