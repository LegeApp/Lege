//! Native desktop processing bridge.
//!
//! The viewer owns interaction state, while the established `lege` command
//! remains the processing engine. Keeping the boundary here gives the GUI a
//! cancellable worker protocol without making renderer threads wait on export.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use serde_json::Value;
use winit::event_loop::EventLoopProxy;

use crate::event::ViewerEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingProfile {
    Reading,
    Bilevel,
}

impl ProcessingProfile {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Reading => "Reading",
            Self::Bilevel => "Bilevel",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Pdf,
    Djvu,
    Epub,
}

impl OutputFormat {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pdf => "PDF",
            Self::Djvu => "DjVu",
            Self::Epub => "EPUB",
        }
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Djvu => "djvu",
            Self::Epub => "epub",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Pdf => Self::Djvu,
            Self::Djvu => Self::Epub,
            Self::Epub => Self::Pdf,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextCompression {
    Ccitt4,
    Jbig2,
}

impl TextCompression {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ccitt4 => "CCITT4",
            Self::Jbig2 => "JBIG2",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Ccitt4 => Self::Jbig2,
            Self::Jbig2 => Self::Ccitt4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverMode {
    Preserve,
    Jpeg,
    Jpeg2000,
    None,
}

impl CoverMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preserve => "Preserve",
            Self::Jpeg => "JPEG",
            Self::Jpeg2000 => "JPEG 2000",
            Self::None => "Remove",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Preserve => Self::Jpeg,
            Self::Jpeg => Self::Jpeg2000,
            Self::Jpeg2000 => Self::None,
            Self::None => Self::Preserve,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProcessing {
    Original,
    Dithered,
}

impl ImageProcessing {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Dithered => "Dithered",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Original => Self::Dithered,
            Self::Dithered => Self::Original,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrMode {
    Fast,
    Best,
}

impl OcrMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Best => "Best",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Fast => Self::Best,
            Self::Best => Self::Fast,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginMode {
    None,
    Center,
    Crop,
}

impl MarginMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "Unchanged",
            Self::Center => "Standardize + center",
            Self::Crop => "Crop",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::None => Self::Center,
            Self::Center => Self::Crop,
            Self::Crop => Self::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Binarization {
    Adaptive { sauvola_k: f32 },
    Threshold { value: u8 },
    Heavy,
}

impl Binarization {
    pub fn label(self) -> String {
        match self {
            Self::Adaptive { sauvola_k } => format!("Adaptive (k={sauvola_k:.2})"),
            Self::Threshold { value } => format!("Threshold ({value})"),
            Self::Heavy => "Heavy".to_owned(),
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Adaptive { .. } => Self::Threshold { value: 180 },
            Self::Threshold { .. } => Self::Heavy,
            Self::Heavy => Self::Adaptive { sauvola_k: 0.05 },
        }
    }
}

/// Complete processing configuration surfaced by the native viewer. The fields
/// intentionally mirror the established Freya GUI rather than the debug-only
/// CLI switches.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessingOptions {
    pub output_format: OutputFormat,
    pub compression: TextCompression,
    pub cover: CoverMode,
    pub image_processing: ImageProcessing,
    pub make_epub_sidecar: bool,
    pub layout_analysis: bool,
    pub layout_exclusion_pages: BTreeSet<u32>,
    pub use_ocr: bool,
    pub ocr_mode: OcrMode,
    pub use_jbig2_halftone: bool,
    pub high_quality: bool,
    pub jpeg_compat: bool,
    pub invert: bool,
    pub margin_mode: MarginMode,
    pub crop_footnotes: bool,
    pub crop_free_aspect: bool,
    pub reflow: bool,
    pub grayscale: bool,
    pub binarization: Binarization,
    pub target_height: u32,
    pub target_width: Option<u32>,
}

impl Default for ProcessingOptions {
    fn default() -> Self {
        Self {
            output_format: OutputFormat::Pdf,
            compression: TextCompression::Jbig2,
            cover: CoverMode::Preserve,
            image_processing: ImageProcessing::Original,
            make_epub_sidecar: false,
            layout_analysis: true,
            layout_exclusion_pages: BTreeSet::new(),
            use_ocr: false,
            ocr_mode: OcrMode::Fast,
            use_jbig2_halftone: false,
            high_quality: false,
            jpeg_compat: false,
            invert: false,
            margin_mode: MarginMode::None,
            crop_footnotes: false,
            crop_free_aspect: false,
            reflow: false,
            grayscale: false,
            binarization: Binarization::Adaptive { sauvola_k: 0.05 },
            target_height: 1200,
            target_width: None,
        }
    }
}

impl ProcessingOptions {
    pub fn apply_profile(&mut self, profile: ProcessingProfile) {
        match profile {
            ProcessingProfile::Reading => {
                self.output_format = OutputFormat::Pdf;
                self.compression = TextCompression::Ccitt4;
                self.image_processing = ImageProcessing::Original;
                self.grayscale = false;
                self.binarization = Binarization::Adaptive { sauvola_k: 0.05 };
            }
            ProcessingProfile::Bilevel => {
                self.output_format = OutputFormat::Pdf;
                self.compression = TextCompression::Jbig2;
                self.image_processing = ImageProcessing::Dithered;
                self.grayscale = false;
                self.binarization = Binarization::Adaptive { sauvola_k: 0.05 };
            }
        }
    }

    fn effective_text_format(&self) -> &'static str {
        match self.output_format {
            OutputFormat::Djvu => "djvu",
            OutputFormat::Epub => "epub",
            OutputFormat::Pdf if self.layout_analysis => match self.image_processing {
                ImageProcessing::Original => "ccitt4",
                ImageProcessing::Dithered => "jbig2",
            },
            OutputFormat::Pdf => match self.compression {
                TextCompression::Ccitt4 => "ccitt4",
                TextCompression::Jbig2 => "jbig2",
            },
        }
    }

    pub fn normalize_dependencies(&mut self) {
        if self.reflow {
            self.layout_analysis = true;
            self.invert = false;
            self.margin_mode = MarginMode::None;
            self.crop_footnotes = false;
            self.crop_free_aspect = false;
        } else if self.invert {
            self.layout_analysis = false;
        }
        if !self.layout_analysis {
            self.reflow = false;
            self.use_jbig2_halftone = false;
        }
        if self.grayscale {
            self.use_jbig2_halftone = false;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingScope {
    Document,
    Pages(BTreeSet<u32>),
}

impl ProcessingScope {
    pub fn label(&self) -> String {
        match self {
            Self::Document => "Entire document".to_owned(),
            Self::Pages(pages) if pages.len() == 1 => {
                format!("Page {}", pages.first().copied().unwrap_or(0) + 1)
            }
            Self::Pages(pages) => format!("{} selected pages", pages.len()),
        }
    }

    fn page_range(&self) -> Option<String> {
        let Self::Pages(pages) = self else {
            return None;
        };
        if pages.is_empty() {
            return None;
        }
        let mut ranges = Vec::new();
        let mut start = None;
        let mut previous = 0;
        for page in pages.iter().copied() {
            match start {
                None => {
                    start = Some(page);
                    previous = page;
                }
                Some(_) if page == previous + 1 => previous = page,
                Some(first) => {
                    ranges.push(format_page_range(first, previous));
                    start = Some(page);
                    previous = page;
                }
            }
        }
        if let Some(first) = start {
            ranges.push(format_page_range(first, previous));
        }
        Some(ranges.join(","))
    }
}

fn format_page_range(first: u32, last: u32) -> String {
    if first == last {
        (first + 1).to_string()
    } else {
        format!("{}-{}", first + 1, last + 1)
    }
}

#[derive(Debug, Clone)]
pub struct ProcessingRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    pub profile: ProcessingProfile,
    pub scope: ProcessingScope,
    pub options: ProcessingOptions,
}

#[derive(Debug, Clone)]
pub enum ProcessingUpdate {
    Started { output: PathBuf },
    Progress { title: String, detail: String },
    Completed { message: String, output: PathBuf },
    Cancelled,
    Failed { message: String },
}

#[derive(Debug, Clone)]
pub struct ProcessingControl {
    pid: Arc<AtomicU32>,
    cancelled: Arc<AtomicBool>,
}

impl ProcessingControl {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let pid = self.pid.load(Ordering::Acquire);
        if pid == 0 {
            return;
        }
        #[cfg(unix)]
        #[allow(
            unsafe_code,
            reason = "libc::kill on a raw pid: this thread never holds &mut Child, so Child::kill() is unreachable and there is no safe alternative"
        )]
        // SAFETY: `pid` is a plain process id, not a pointer or handle, so
        // `kill(2)` has no aliasing/memory invariants to uphold here. The
        // worker is intentionally isolated, so termination cannot take the
        // interactive renderer down with it, and a pid that has already
        // exited just yields `ESRCH`, which is ignored.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .status();
        }
    }
}

pub fn start(
    request: ProcessingRequest,
    proxy: EventLoopProxy<ViewerEvent>,
) -> Result<ProcessingControl, String> {
    let cli = resolve_cli_path()?;
    let args = build_args(&request);
    let output = request.output.clone();
    let control = ProcessingControl {
        pid: Arc::new(AtomicU32::new(0)),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let worker_control = control.clone();
    std::thread::Builder::new()
        .name("lege-viewer-processing".to_owned())
        .spawn(move || run_worker(cli, args, output, proxy, worker_control))
        .map_err(|error| format!("failed to start processing worker: {error}"))?;
    Ok(control)
}

fn run_worker(
    cli: PathBuf,
    args: Vec<OsString>,
    output: PathBuf,
    proxy: EventLoopProxy<ViewerEvent>,
    control: ProcessingControl,
) {
    let result = (|| -> Result<(String, bool), String> {
        let mut command = Command::new(&cli);
        if let Some(directory) = cli.parent().filter(|directory| directory.is_dir()) {
            command.current_dir(directory);
            if std::env::var_os("LEGE_ASSET_DIR").is_none() {
                command.env("LEGE_ASSET_DIR", directory);
            }
        }
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("could not launch {}: {error}", cli.display()))?;
        control.pid.store(child.id(), Ordering::Release);
        if control.cancelled.load(Ordering::Acquire) {
            let _ = child.kill();
        }
        let stdout = child
            .stdout
            .take()
            .ok_or("processing worker has no stdout")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("processing worker has no stderr")?;
        let stderr_thread = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                lines.push(line);
                if lines.len() > 20 {
                    lines.remove(0);
                }
            }
            lines.join("\n")
        });
        let mut terminal = None;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(update) = parse_worker_update(&line, &output) {
                if matches!(
                    update,
                    ProcessingUpdate::Completed { .. } | ProcessingUpdate::Failed { .. }
                ) {
                    terminal = Some(update.clone());
                }
                let _ = proxy.send_event(ViewerEvent::Processing(update));
            }
        }
        let status = child
            .wait()
            .map_err(|error| format!("could not wait for processing worker: {error}"))?;
        let stderr = stderr_thread.join().unwrap_or_default();
        control.pid.store(0, Ordering::Release);
        if control.cancelled.load(Ordering::Acquire) {
            return Err("Processing cancelled.".to_owned());
        }
        match terminal {
            Some(ProcessingUpdate::Completed { message, .. }) if status.success() => {
                Ok((message, true))
            }
            Some(ProcessingUpdate::Failed { message }) => Err(message),
            _ if status.success() => Ok(("Processing completed.".to_owned(), false)),
            _ => Err(format!(
                "Processing worker exited with {status}.{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!("\n\n{stderr}")
                }
            )),
        }
    })();
    match result {
        Ok((message, terminal_sent)) if !terminal_sent => {
            let _ = proxy.send_event(ViewerEvent::Processing(ProcessingUpdate::Completed {
                message,
                output,
            }));
        }
        Ok(_) => {}
        Err(message) if message == "Processing cancelled." => {
            let _ = proxy.send_event(ViewerEvent::Processing(ProcessingUpdate::Cancelled));
        }
        Err(message) => {
            let _ = proxy.send_event(ViewerEvent::Processing(ProcessingUpdate::Failed {
                message,
            }));
        }
    }
}

fn parse_worker_update(line: &str, output: &Path) -> Option<ProcessingUpdate> {
    let value: Value = serde_json::from_str(line).ok()?;
    match value.get("type")?.as_str()? {
        "completed" => Some(ProcessingUpdate::Completed {
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Processing completed.")
                .to_owned(),
            output: output.to_path_buf(),
        }),
        "error" => Some(ProcessingUpdate::Failed {
            message: value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Processing failed.")
                .to_owned(),
        }),
        "status" => status_lines(value.get("status"))
            .map(|(title, detail)| ProcessingUpdate::Progress { title, detail }),
        _ => None,
    }
}

fn status_lines(status: Option<&Value>) -> Option<(String, String)> {
    let status = status?;
    let kind = status.get("kind")?.as_str()?;
    let detail = match kind {
        "initializing" => "Preparing processing pipeline…".to_owned(),
        "assembling_output" => "Assembling output document…".to_owned(),
        "layout_progress" | "no_layout_progress" => format_progress(status, "Rendering", "encoded"),
        "margin_progress" => format_progress(status, "Processing", "pass2_processed"),
        "pdf_append" | "pdf_append_margin" => format_progress(status, "Writing", "current"),
        "pipeline_message" => status
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Processing…")
            .to_owned(),
        _ => "Processing…".to_owned(),
    };
    Some((kind.replace('_', " "), detail))
}

fn format_progress(status: &Value, prefix: &str, key: &str) -> String {
    let current = status
        .get(key)
        .or_else(|| status.get("rendered"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = status.get("total").and_then(Value::as_u64).unwrap_or(0);
    if total == 0 {
        prefix.to_owned()
    } else {
        format!("{prefix} {current}/{total}")
    }
}

fn build_args(request: &ProcessingRequest) -> Vec<OsString> {
    let mut options = request.options.clone();
    options.normalize_dependencies();
    let mut args = vec![
        "--gui-worker".into(),
        request.input.as_os_str().into(),
        "--output".into(),
        request.output.as_os_str().into(),
        "--text-format".into(),
        options.effective_text_format().into(),
    ];

    if options.make_epub_sidecar && options.output_format != OutputFormat::Epub {
        args.push("--epub-sidecar-output".into());
        args.push(request.output.with_extension("epub").as_os_str().into());
    }
    if matches!(
        options.output_format,
        OutputFormat::Pdf | OutputFormat::Djvu
    ) {
        args.push("--cover-format".into());
        args.push(
            match options.cover {
                CoverMode::Preserve | CoverMode::Jpeg => "jpeg",
                CoverMode::Jpeg2000 => "jp2",
                CoverMode::None => "none",
            }
            .into(),
        );
        if options.cover == CoverMode::None {
            args.push("--no-cover".into());
        }
    }
    if !options.layout_analysis {
        args.push("--no-layout".into());
    } else if !options.layout_exclusion_pages.is_empty() {
        args.push("--exclude-layout".into());
        args.push(
            ProcessingScope::Pages(options.layout_exclusion_pages.clone())
                .page_range()
                .unwrap_or_default()
                .into(),
        );
    }
    if options.use_ocr {
        args.push("--ocr-mode".into());
        args.push(
            match options.ocr_mode {
                OcrMode::Fast => "fast",
                OcrMode::Best => "best",
            }
            .into(),
        );
    } else {
        args.push("--no-ocr".into());
    }
    if options.reflow {
        args.push("--reflow".into());
    }
    if options.use_jbig2_halftone
        && options.output_format == OutputFormat::Pdf
        && options.layout_analysis
        && options.image_processing == ImageProcessing::Dithered
    {
        args.push("--halftone".into());
    } else if options.layout_analysis && options.image_processing == ImageProcessing::Dithered {
        args.push("--dither".into());
    }
    if options.jpeg_compat {
        args.push("--jpeg-compat".into());
    }
    if options.high_quality {
        args.push("--high-quality".into());
    }
    if options.invert {
        args.push("--invert".into());
    }
    match options.margin_mode {
        MarginMode::None => {}
        MarginMode::Center => args.push("--center-margins".into()),
        MarginMode::Crop => args.push("--crop-margins".into()),
    }
    if options.margin_mode == MarginMode::Crop || options.crop_free_aspect {
        args.push("--crop-free-aspect".into());
    }
    if options.crop_footnotes {
        args.push("--force-crop".into());
    }
    if options.grayscale {
        args.push("--grayscale".into());
    } else {
        match options.binarization {
            Binarization::Adaptive { sauvola_k } => {
                args.extend([
                    "--binarization".into(),
                    "adaptive".into(),
                    "--sauvola-k".into(),
                    sauvola_k.to_string().into(),
                ]);
            }
            Binarization::Threshold { value } => {
                args.extend([
                    "--binarization".into(),
                    "fixed".into(),
                    "--threshold".into(),
                    value.to_string().into(),
                ]);
            }
            Binarization::Heavy => {
                args.extend(["--binarization".into(), "heavy".into()]);
            }
        }
    }
    args.push("--target-height".into());
    args.push(options.target_height.to_string().into());
    if let Some(width) = options.target_width.filter(|_| !options.crop_free_aspect) {
        args.push("--target-width".into());
        args.push(width.to_string().into());
    }
    if let Some(range) = request.scope.page_range() {
        args.push(range.into());
    }
    args
}

fn resolve_cli_path() -> Result<PathBuf, String> {
    let candidate_name = if cfg!(windows) { "lege.exe" } else { "lege" };
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        let candidate = directory.join(candidate_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Ok(PathBuf::from(candidate_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_pages_are_compacted_to_cli_ranges() {
        let scope = ProcessingScope::Pages([0, 1, 2, 5, 7, 8].into_iter().collect());
        assert_eq!(scope.page_range().as_deref(), Some("1-3,6,8-9"));
    }

    #[test]
    fn processing_arguments_keep_selected_range_last() {
        let request = ProcessingRequest {
            input: "source.pdf".into(),
            output: "result.pdf".into(),
            profile: ProcessingProfile::Reading,
            scope: ProcessingScope::Pages([2].into_iter().collect()),
            options: ProcessingOptions::default(),
        };
        assert_eq!(build_args(&request).last(), Some(&OsString::from("3")));
    }

    #[test]
    fn adaptive_binarization_defaults_to_point_zero_five() {
        assert_eq!(
            ProcessingOptions::default().binarization,
            Binarization::Adaptive { sauvola_k: 0.05 }
        );
    }

    #[test]
    fn viewer_options_cover_freya_ocr_layout_and_quality_flags() {
        let mut options = ProcessingOptions::default();
        options.use_ocr = true;
        options.ocr_mode = OcrMode::Best;
        options.high_quality = true;
        options.layout_exclusion_pages = [1, 2, 4].into_iter().collect();
        let request = ProcessingRequest {
            input: "source.pdf".into(),
            output: "result.pdf".into(),
            profile: ProcessingProfile::Reading,
            scope: ProcessingScope::Document,
            options,
        };
        let args = build_args(&request);
        let strings = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            strings
                .windows(2)
                .any(|pair| pair == ["--ocr-mode", "best"])
        );
        assert!(
            strings
                .windows(2)
                .any(|pair| pair == ["--exclude-layout", "2-3,5"])
        );
        assert!(strings.iter().any(|arg| arg == "--high-quality"));
        assert!(!strings.iter().any(|arg| arg == "--no-ocr"));
    }
}
