//! Persistent native TensorRT PP-OCRv6 worker.
//!
//! The C++ worker owns CUDA/TensorRT state for the lifetime of an OCR job. A
//! successful constructor means both model engines completed a real inference
//! preflight. Later worker failures are returned to the caller and never cause
//! an in-job backend switch.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use image::GrayImage;
use serde::Deserialize;

use crate::backend::{
    AcceleratorKind, BackendCapabilities, PageBatch, PageOcrBackend, RecognitionBatch,
};
use crate::types::{OcrLineResult, OcrWord};

const PROTOCOL: &str = "lege-tensorrt-ocr";
const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TensorRtPaddleConfig {
    pub executable: PathBuf,
    pub detector: PathBuf,
    pub recognizer: PathBuf,
    pub dictionary: PathBuf,
    pub recognition_batch: usize,
    #[serde(default)]
    pub dll_directories: Vec<PathBuf>,
}

impl TensorRtPaddleConfig {
    /// Resolve the development or packaged runtime layout rooted at `root`.
    pub fn from_root(root: &Path, recognition_batch: usize) -> Result<Self> {
        let executable_name = if cfg!(windows) {
            "turboocr-text.exe"
        } else {
            "turboocr-text"
        };
        let executable = [
            root.join(executable_name),
            root.join("bin").join(executable_name),
            root.join("build-windows-trt-text-ninja")
                .join(executable_name),
        ]
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            anyhow!(
                "TensorRT OCR executable not found under {}; expected {}, bin/{}, or build-windows-trt-text-ninja/{}",
                root.display(),
                executable_name,
                executable_name,
                executable_name
            )
        })?;
        let model_dir = if root.join("models").is_dir() {
            root.join("models")
        } else {
            root.to_path_buf()
        };
        let mut dll_directories = [
            root.join("runtime"),
            root.join("bin"),
            root.join("lib"),
            executable
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.to_path_buf()),
        ]
        .into_iter()
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
        dll_directories.dedup();
        let config = Self {
            executable,
            detector: model_dir.join("det_tiny.onnx"),
            recognizer: model_dir.join("rec_tiny.onnx"),
            dictionary: model_dir.join("keys_tiny.txt"),
            recognition_batch: recognition_batch.clamp(1, 32),
            dll_directories,
        };
        config.validate()?;
        Ok(config)
    }

    /// Find a packaged runtime next to the CLI or this workspace's development
    /// build. Explicit `LEGE_TENSORRT_OCR_ROOT` takes precedence.
    pub fn discover(recognition_batch: usize) -> Option<Self> {
        Self::discover_result(recognition_batch).ok().flatten()
    }

    /// Discover the runtime while preserving an invalid explicit-root error.
    pub fn discover_result(recognition_batch: usize) -> Result<Option<Self>> {
        if let Some(root) = std::env::var_os("LEGE_TENSORRT_OCR_ROOT") {
            return Self::from_root(Path::new(&root), recognition_batch)
                .with_context(|| {
                    format!(
                        "invalid LEGE_TENSORRT_OCR_ROOT {}",
                        Path::new(&root).display()
                    )
                })
                .map(Some);
        }

        let mut roots = Vec::new();
        if let Ok(executable) = std::env::current_exe()
            && let Some(parent) = executable.parent()
        {
            roots.push(parent.join("tensorrt"));
            roots.push(parent.join("runtime").join("tensorrt"));
            roots.push(parent.to_path_buf());
        }
        if let Ok(current) = std::env::current_dir() {
            roots.push(current.join("lege-document-ocr/turboocr"));
            roots.push(current.join("turboocr"));
            roots.push(current);
        }
        Ok(roots
            .into_iter()
            .find_map(|root| Self::from_root(&root, recognition_batch).ok()))
    }

    pub fn validate(&self) -> Result<()> {
        for (kind, path) in [
            ("executable", &self.executable),
            ("detector", &self.detector),
            ("recognizer", &self.recognizer),
            ("dictionary", &self.dictionary),
        ] {
            if !path.is_file() {
                bail!(
                    "TensorRT OCR {kind} file does not exist: {}",
                    path.display()
                );
            }
        }
        if self.recognition_batch == 0 || self.recognition_batch > 32 {
            bail!("TensorRT recognition batch must be in 1..=32");
        }
        for directory in &self.dll_directories {
            if !directory.is_dir() {
                bail!(
                    "TensorRT OCR DLL directory does not exist: {}",
                    directory.display()
                );
            }
        }
        Ok(())
    }
}

/// Return whether Windows has an NVIDIA GPU or driver footprint. Auto routing
/// uses this as a fail-closed boundary: NVIDIA hardware makes TensorRT
/// mandatory even when its driver installation is currently broken.
pub fn nvidia_hardware_present() -> bool {
    #[cfg(target_os = "windows")]
    {
        if std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .map(|root| root.join("System32").join("nvcuda.dll").is_file())
            .unwrap_or(false)
        {
            return true;
        }

        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let silent_success = |program: &Path, arguments: &[&str]| {
            Command::new(program)
                .args(arguments)
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        };
        if silent_success(Path::new("nvidia-smi"), &["-L"]) {
            return true;
        }

        let Some(windows_root) = std::env::var_os("WINDIR").map(PathBuf::from) else {
            return false;
        };
        let display_adapter_class =
            r"HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
        return silent_success(
            &windows_root.join("System32").join("reg.exe"),
            &["query", display_adapter_class, "/f", "VEN_10DE", "/s"],
        );
    }

    #[cfg(not(target_os = "windows"))]
    false
}

pub struct TensorRtPaddleEngine {
    worker: Mutex<TensorRtWorker>,
    gpu_name: String,
}

impl std::fmt::Debug for TensorRtPaddleEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TensorRtPaddleEngine")
            .field("gpu_name", &self.gpu_name)
            .finish_non_exhaustive()
    }
}

impl TensorRtPaddleEngine {
    /// Start the persistent worker and require a real detector + recognizer
    /// CUDA inference preflight before returning.
    pub fn start(config: &TensorRtPaddleConfig) -> Result<Self> {
        config.validate()?;
        let worker = TensorRtWorker::start(config)?;
        let gpu_name = worker.gpu_name.clone();
        Ok(Self {
            worker: Mutex::new(worker),
            gpu_name,
        })
    }

    pub fn gpu_name(&self) -> &str {
        &self.gpu_name
    }

    fn recognize(&self, image: &GrayImage, language: &str) -> Result<Vec<OcrLineResult>> {
        if !matches!(language, "eng" | "en" | "en-US" | "en-GB") {
            bail!(
                "TensorRT PP-OCRv6-tiny currently has an English dictionary; requested {language}"
            );
        }
        self.worker
            .lock()
            .map_err(|_| anyhow!("TensorRT OCR worker lock was poisoned"))?
            .recognize(image)
    }
}

impl PageOcrBackend for TensorRtPaddleEngine {
    fn name(&self) -> &'static str {
        "tensorrt-paddle"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            page_ocr: true,
            line_batching: true,
            word_geometry: true,
            recognition_confidence: true,
            layout_analysis: false,
            table_recognition: false,
            formula_recognition: false,
            accelerator: AcceleratorKind::Gpu,
        }
    }

    fn recognize_pages(&self, batch: PageBatch<'_>) -> Result<Vec<Vec<OcrLineResult>>> {
        batch
            .pages
            .iter()
            .map(|page| self.recognize(page, batch.language))
            .collect()
    }

    fn recognize_lines(&self, batch: RecognitionBatch<'_>) -> Result<Vec<OcrLineResult>> {
        let mut recognized = Vec::with_capacity(batch.lines.len());
        for line in batch.lines {
            let mut candidates = self.recognize(line, batch.language)?;
            if candidates.is_empty() {
                recognized.push(OcrLineResult {
                    text: String::new(),
                    confidence: None,
                    words: Vec::new(),
                    bbox_highres: [0, 0, line.width(), line.height()],
                });
                continue;
            }
            let mut result = candidates.remove(0);
            result.bbox_highres = [0, 0, line.width(), line.height()];
            recognized.push(result);
        }
        Ok(recognized)
    }
}

struct TensorRtWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    failed: bool,
    gpu_name: String,
}

impl TensorRtWorker {
    fn start(config: &TensorRtPaddleConfig) -> Result<Self> {
        let model_dir = config
            .detector
            .parent()
            .ok_or_else(|| anyhow!("TensorRT detector has no parent directory"))?;
        let runtime_root = if model_dir.file_name().is_some_and(|name| name == "models") {
            model_dir.parent().unwrap_or(model_dir)
        } else {
            model_dir
        };
        let detector = config
            .detector
            .strip_prefix(runtime_root)
            .unwrap_or(&config.detector);
        let recognizer = config
            .recognizer
            .strip_prefix(runtime_root)
            .unwrap_or(&config.recognizer);
        let dictionary = config
            .dictionary
            .strip_prefix(runtime_root)
            .unwrap_or(&config.dictionary);
        let executable =
            std::fs::canonicalize(&config.executable).unwrap_or_else(|_| config.executable.clone());
        let mut command = Command::new(executable);
        command
            .current_dir(runtime_root)
            .arg("--server")
            .arg("--det")
            .arg(detector)
            .arg("--rec")
            .arg(recognizer)
            .arg("--dict")
            .arg(dictionary)
            .arg("--rec-batch")
            .arg(config.recognition_batch.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // The Windows laptop deployment target is VRAM-constrained. Keep the
        // measured batch-8 profile unless the operator explicitly opts into
        // graph engines or a more expensive tactic search.
        if std::env::var_os("TURBO_OCR_CUDA_GRAPHS").is_none() {
            command.env("TURBO_OCR_CUDA_GRAPHS", "0");
        }
        if std::env::var_os("TRT_OPT_LEVEL").is_none() {
            command.env("TRT_OPT_LEVEL", "3");
        }
        if !config.dll_directories.is_empty() {
            command.env("PATH", augmented_path(&config.dll_directories)?);
        }
        let mut child = command.spawn().with_context(|| {
            format!("start TensorRT OCR worker {}", config.executable.display())
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("TensorRT OCR worker stdin was not created"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("TensorRT OCR worker stdout was not created"))?;
        let mut stdout = BufReader::new(stdout);
        let mut ready_line = String::new();
        let ready_bytes = match stdout.read_line(&mut ready_line) {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("read TensorRT OCR READY response");
            }
        };
        if ready_bytes == 0 {
            let status = child.wait().ok();
            bail!("TensorRT OCR preflight exited before READY ({status:?})");
        }
        let ready: ReadyResponse = match serde_json::from_str(ready_line.trim_end()) {
            Ok(ready) => ready,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("parse TensorRT OCR READY response");
            }
        };
        if ready.protocol != PROTOCOL || ready.version != PROTOCOL_VERSION || !ready.ready {
            let _ = child.kill();
            let _ = child.wait();
            bail!("TensorRT OCR worker returned an incompatible READY response");
        }
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            failed: false,
            gpu_name: ready.gpu,
        })
    }

    fn recognize(&mut self, image: &GrayImage) -> Result<Vec<OcrLineResult>> {
        if self.failed {
            bail!("TensorRT OCR worker is unavailable after a previous runtime failure");
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let bytes = image.as_raw();
        if let Err(error) = (|| -> std::io::Result<()> {
            writeln!(
                self.stdin,
                "IMAGE\t{id}\t{}\t{}\t1\t{}",
                image.width(),
                image.height(),
                bytes.len()
            )?;
            self.stdin.write_all(bytes)?;
            self.stdin.flush()
        })() {
            self.failed = true;
            return Err(error).context("send page to TensorRT OCR worker");
        }

        let mut response_line = String::new();
        match self.stdout.read_line(&mut response_line) {
            Ok(0) => {
                self.failed = true;
                let status = self.child.try_wait().ok().flatten();
                bail!("TensorRT OCR worker exited during the job ({status:?})");
            }
            Err(error) => {
                self.failed = true;
                return Err(error).context("read TensorRT OCR worker response");
            }
            Ok(_) => {}
        }
        let response: OcrResponse = match serde_json::from_str(response_line.trim_end()) {
            Ok(response) => response,
            Err(error) => {
                self.failed = true;
                return Err(error).context("parse TensorRT OCR worker response");
            }
        };
        if response.protocol != PROTOCOL
            || response.version != PROTOCOL_VERSION
            || response.id != id
        {
            self.failed = true;
            bail!("TensorRT OCR worker response did not match request {id}");
        }
        if !response.ok {
            self.failed = true;
            bail!(
                "TensorRT OCR runtime failed: {}",
                response.error.as_deref().unwrap_or("unknown worker error")
            );
        }
        Ok(response_to_lines(response, image.width(), image.height()))
    }
}

impl Drop for TensorRtWorker {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "QUIT");
        let _ = self.stdin.flush();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn augmented_path(additions: &[PathBuf]) -> Result<OsString> {
    let mut paths = additions.to_vec();
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).context("construct TensorRT OCR child PATH")
}

#[derive(Debug, Deserialize)]
struct ReadyResponse {
    protocol: String,
    version: u32,
    ready: bool,
    gpu: String,
}

#[derive(Debug, Deserialize)]
struct OcrResponse {
    protocol: String,
    version: u32,
    id: u64,
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    lines: Vec<WorkerLine>,
}

#[derive(Debug, Deserialize)]
struct WorkerLine {
    text: String,
    confidence: f32,
    bbox: [i32; 4],
}

fn response_to_lines(response: OcrResponse, width: u32, height: u32) -> Vec<OcrLineResult> {
    response
        .lines
        .into_iter()
        .filter_map(|line| {
            let x0 = line.bbox[0].clamp(0, width as i32) as u32;
            let y0 = line.bbox[1].clamp(0, height as i32) as u32;
            let x1 = line.bbox[2].clamp(0, width as i32) as u32;
            let y1 = line.bbox[3].clamp(0, height as i32) as u32;
            if line.text.trim().is_empty() || x1 <= x0 || y1 <= y0 {
                return None;
            }
            let confidence = line.confidence.clamp(0.0, 1.0);
            let text = line.text;
            Some(OcrLineResult {
                words: vec![OcrWord {
                    text: text.clone(),
                    bbox_crop_local: [0, 0, x1 - x0, y1 - y0],
                    confidence: Some(confidence),
                }],
                text,
                confidence: Some(confidence),
                bbox_highres: [x0, y0, x1, y1],
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_layout_finds_worker_models_and_runtime_dll_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let bin = root.join("bin");
        let models = root.join("models");
        let runtime = root.join("runtime");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        let executable_name = if cfg!(windows) {
            "turboocr-text.exe"
        } else {
            "turboocr-text"
        };
        std::fs::write(bin.join(executable_name), b"worker").unwrap();
        std::fs::write(models.join("det_tiny.onnx"), b"detector").unwrap();
        std::fs::write(models.join("rec_tiny.onnx"), b"recognizer").unwrap();
        std::fs::write(models.join("keys_tiny.txt"), b"dictionary").unwrap();

        let config = TensorRtPaddleConfig::from_root(root, 8).unwrap();
        assert_eq!(config.executable, bin.join(executable_name));
        assert_eq!(config.detector, models.join("det_tiny.onnx"));
        assert!(config.dll_directories.contains(&runtime));
        assert!(config.dll_directories.contains(&bin));
    }

    #[test]
    fn worker_response_is_clamped_and_mapped_to_ocr_lines() {
        let response = OcrResponse {
            protocol: PROTOCOL.into(),
            version: PROTOCOL_VERSION,
            id: 7,
            ok: true,
            error: None,
            lines: vec![WorkerLine {
                text: "example".into(),
                confidence: 1.2,
                bbox: [-2, 3, 120, 40],
            }],
        };
        let lines = response_to_lines(response, 100, 50);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].bbox_highres, [0, 3, 100, 40]);
        assert_eq!(lines[0].confidence, Some(1.0));
        assert_eq!(lines[0].words[0].bbox_crop_local, [0, 0, 100, 37]);
    }
}
