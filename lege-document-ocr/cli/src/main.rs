use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use clap::{Parser, Subcommand, ValueEnum};
use lege_docir::{ProcessingProfile, QualityMode, TextView};
use lege_document_batch::{JobStatus, JobStore, atomic_checkpoint, fingerprint};
use lege_document_export::{ExportFormat, ExportRequest, SearchablePdfPolicy, export_all};
use lege_document_pipeline::correction::CorrectionMode;
use lege_document_pipeline::{
    BackendChoice, BrokeredTensorRtConfig, DocumentProcessor, OcrSchedulerConfig, PipelineConfig,
    TensorRtPaddleConfig, nvidia_hardware_present,
};

#[derive(Debug, Parser)]
#[command(
    name = "lege-ocr",
    version,
    about = "Local, resumable batch PDF OCR and document conversion"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Batch(BatchArgs),
    /// Preflight the installed OCR backend without processing a document.
    Doctor(DoctorArgs),
}

#[derive(Debug, clap::Args)]
struct DoctorArgs {
    #[arg(long, alias = "device", value_enum, default_value_t = BackendArg::Auto)]
    backend: BackendArg,
    #[arg(long, default_value = "eng")]
    language: String,
    /// Packaged TensorRT root containing bin, models, and runtime directories.
    #[arg(long)]
    tensorrt_ocr_root: Option<PathBuf>,
    /// Additional DLL directory inherited by the TensorRT worker. Repeatable.
    #[arg(long)]
    tensorrt_dll_dir: Vec<PathBuf>,
    #[arg(long, default_value_t = 8)]
    tensorrt_rec_batch: usize,
    /// Evidence page-OCR bridge executable for `brokered-tensorrt`.
    #[arg(long)]
    broker_bridge: Option<PathBuf>,
    #[arg(long, default_value = "evidence-trt")]
    broker_endpoint: String,
    #[arg(long, default_value = "turbo-ocr")]
    broker_model: String,
    #[arg(long)]
    broker_revision: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct BatchArgs {
    #[arg(required = true)]
    inputs: Vec<PathBuf>,
    #[arg(short, long)]
    output: PathBuf,
    #[arg(long)]
    recursive: bool,
    #[arg(long, value_enum, default_value_t = ProfileArg::Search)]
    profile: ProfileArg,
    #[arg(long, alias = "device", value_enum, default_value_t = BackendArg::Auto)]
    backend: BackendArg,
    #[arg(long, default_value = "eng")]
    language: String,
    #[arg(
        long = "format",
        alias = "formats",
        value_delimiter = ',',
        default_value = "json,text,html"
    )]
    formats: Vec<String>,
    #[arg(long, value_enum, default_value_t = TextViewArg::Corrected)]
    text_view: TextViewArg,
    #[arg(long)]
    resume: bool,
    #[arg(long)]
    force: bool,
    #[arg(long, value_enum, default_value_t = OnErrorArg::Continue)]
    on_error: OnErrorArg,
    #[arg(long)]
    json_progress: bool,
    /// Commercially redistributable `word frequency` TSV correction pack.
    #[arg(long)]
    dictionary: Option<PathBuf>,
    #[arg(long)]
    no_spellcheck: bool,
    /// Also apply edit-distance spelling candidates; conservative mode only applies spacing repairs.
    #[arg(long, conflicts_with = "no_spellcheck")]
    apply_spelling_edits: bool,
    /// Ignore trustworthy embedded text and run OCR for benchmark/quality evaluation.
    #[arg(long)]
    force_ocr: bool,
    /// Rasterization target for pages that require OCR.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(72..=600))]
    render_dpi: u32,
    /// Safety cap for rasterized page area.
    #[arg(long, default_value_t = 40_000_000)]
    max_page_pixels: u64,
    /// Directory containing a checksum-pinned Paddle `manifest.json` model pack.
    #[arg(long)]
    model_pack: Option<PathBuf>,
    /// TurboOCR root containing the native worker and PP-OCRv6 TensorRT models.
    #[arg(long)]
    tensorrt_ocr_root: Option<PathBuf>,
    /// Additional DLL directory inherited by the TensorRT worker. Repeatable.
    #[arg(long)]
    tensorrt_dll_dir: Vec<PathBuf>,
    /// TensorRT recognizer batch size; 8 is recommended for an 8 GB GPU.
    #[arg(long, default_value_t = 8)]
    tensorrt_rec_batch: usize,
    /// Evidence page-OCR bridge executable for `brokered-tensorrt`.
    #[arg(long)]
    broker_bridge: Option<PathBuf>,
    #[arg(long, default_value = "evidence-trt")]
    broker_endpoint: String,
    #[arg(long, default_value = "turbo-ocr")]
    broker_model: String,
    #[arg(long)]
    broker_revision: Option<String>,
    /// Preserve trustworthy native PDFs or explicitly permit raster fallback.
    #[arg(long, value_enum, default_value_t = PdfModeArg::Preserve)]
    pdf_mode: PdfModeArg,
    /// Concurrent document workers. Zero selects a bounded host default.
    #[arg(long, default_value_t = 0)]
    workers: usize,
    #[arg(long, default_value_t = 64)]
    gpu_batch_lines: usize,
    #[arg(long, default_value_t = 12_000_000)]
    gpu_batch_pixels: u64,
    #[arg(long, default_value_t = 3)]
    gpu_batch_wait_ms: u64,
    #[arg(long, default_value_t = 32)]
    gpu_queue_capacity: usize,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProfileArg {
    Search,
    Structured,
    Scientific,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
enum BackendArg {
    Auto,
    #[value(name = "tensorrt-paddle", alias = "tensor-rt-paddle")]
    TensorRtPaddle,
    #[value(name = "brokered-tensorrt")]
    BrokeredTensorRt,
    Paddle,
    WindowsAi,
    WinocrLegacy,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
enum TextViewArg {
    Raw,
    Normalized,
    Corrected,
}
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum OnErrorArg {
    Continue,
    Stop,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
enum PdfModeArg {
    Preserve,
    Rasterize,
}

#[derive(Clone)]
struct BatchSettings {
    output: PathBuf,
    text_view: TextViewArg,
    resume: bool,
    force: bool,
    on_error: OnErrorArg,
    json_progress: bool,
    pdf_mode: PdfModeArg,
}

struct BatchTask {
    source: PathBuf,
    job: lege_document_batch::Job,
    stem: String,
    pending_formats: Vec<ExportFormat>,
}

fn main() {
    let code = match Cli::parse().command {
        Command::Batch(args) => run_batch(args),
        Command::Doctor(args) => run_doctor(args),
    };
    if let Err(error) = code {
        eprintln!("lege-ocr: {error}");
        std::process::exit(2);
    }
}

fn run_batch(args: BatchArgs) -> Result<(), String> {
    if !(1..=32).contains(&args.tensorrt_rec_batch) {
        return Err("--tensorrt-rec-batch must be in 1..=32".to_string());
    }
    let formats = args
        .formats
        .iter()
        .map(|format| ExportFormat::from_str(format).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen_formats = HashSet::new();
    let formats = formats
        .into_iter()
        .filter(|format| seen_formats.insert(*format))
        .collect::<Vec<_>>();
    std::fs::create_dir_all(&args.output).map_err(|error| error.to_string())?;
    let sources = discover(&args.inputs, args.recursive, Some(&args.output))?;
    if sources.is_empty() {
        return Err("no PDF inputs were found".to_string());
    }
    let tensorrt_paddle = if matches!(args.backend, BackendArg::Auto | BackendArg::TensorRtPaddle) {
        resolve_tensorrt_runtime(
            args.tensorrt_ocr_root.as_deref(),
            &args.tensorrt_dll_dir,
            args.tensorrt_rec_batch,
        )?
    } else {
        None
    };
    let brokered_tensorrt = resolve_brokered_runtime(
        args.backend,
        args.broker_bridge.as_deref(),
        &args.broker_endpoint,
        &args.broker_model,
        args.broker_revision.as_deref(),
    )?;
    let config = PipelineConfig {
        profile: profile(args.profile),
        quality: QualityMode::Thorough,
        backend: backend(args.backend),
        language: args.language.clone(),
        render_dpi: args.render_dpi,
        max_page_pixels: args.max_page_pixels,
        force_ocr: args.force_ocr,
        correction_mode: if args.no_spellcheck {
            CorrectionMode::Disabled
        } else if args.apply_spelling_edits {
            CorrectionMode::Aggressive
        } else {
            CorrectionMode::Conservative
        },
        correction_dictionary: args.dictionary.clone(),
        paddle_model_pack: args.model_pack.clone(),
        tensorrt_paddle,
        brokered_tensorrt,
        scheduler: OcrSchedulerConfig {
            max_batch_lines: args.gpu_batch_lines,
            max_batch_pixels: args.gpu_batch_pixels,
            max_wait_ms: args.gpu_batch_wait_ms,
            queue_capacity: args.gpu_queue_capacity,
        }
        .normalized(),
        ..PipelineConfig::default()
    };
    let processor =
        std::sync::Arc::new(DocumentProcessor::new(config).map_err(|error| error.to_string())?);
    let config_hash = processor
        .configuration_hash()
        .map_err(|error| error.to_string())?;
    eprintln!(
        "lege-ocr: selected `{}` before starting the batch; this backend is fixed for every job",
        processor.selected_backend_name()
    );
    let database_path = args.output.join(".lege-ocr/jobs.sqlite");
    let mut jobs = JobStore::open(&database_path).map_err(|error| error.to_string())?;
    let mut failures = 0_u32;
    let mut tasks = std::collections::VecDeque::new();
    for source in sources {
        let source_fingerprint = match fingerprint(&source) {
            Ok(value) => value,
            Err(error) => {
                failures += 1;
                eprintln!("{}: {error}", source.display());
                if args.on_error == OnErrorArg::Stop {
                    return Err(error.to_string());
                }
                continue;
            }
        };
        let page_count = pdf_page_count(&source).unwrap_or(0);
        let job = jobs
            .create_or_resume(
                &source,
                &source_fingerprint.content_hash,
                &config_hash,
                page_count,
            )
            .map_err(|error| error.to_string())?;
        let stem = output_stem(&source, &source_fingerprint.content_hash);
        let directory = args.output.join(&stem);
        let pending_formats = if args.resume && !args.force {
            formats
                .iter()
                .copied()
                .filter(|format| !artifact_exists(&directory, &stem, *format))
                .collect::<Vec<_>>()
        } else {
            formats.clone()
        };
        if args.resume
            && job.status == JobStatus::Complete
            && pending_formats.is_empty()
            && !args.force
        {
            progress(args.json_progress, "skipped", &source, Some(job.id), None);
            continue;
        }
        tasks.push_back(BatchTask {
            source,
            job,
            stem,
            pending_formats,
        });
    }
    if tasks.is_empty() {
        return if failures == 0 {
            Ok(())
        } else {
            Err(format!(
                "batch completed with {failures} failed document(s)"
            ))
        };
    }
    let settings = std::sync::Arc::new(BatchSettings {
        output: args.output,
        text_view: args.text_view,
        resume: args.resume,
        force: args.force,
        on_error: args.on_error,
        json_progress: args.json_progress,
        pdf_mode: args.pdf_mode,
    });
    let automatic_workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(8);
    let worker_count = if args.workers == 0 {
        automatic_workers
    } else {
        args.workers
    }
    .clamp(1, tasks.len());
    let mut worker_stores = Vec::with_capacity(worker_count);
    drop(jobs);
    for _ in 0..worker_count {
        worker_stores.push(JobStore::open(&database_path).map_err(|error| error.to_string())?);
    }
    let tasks = std::sync::Arc::new(std::sync::Mutex::new(tasks));
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (result_sender, result_receiver) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        for store in worker_stores {
            let processor = std::sync::Arc::clone(&processor);
            let settings = std::sync::Arc::clone(&settings);
            let tasks = std::sync::Arc::clone(&tasks);
            let cancelled = std::sync::Arc::clone(&cancelled);
            let result_sender = result_sender.clone();
            scope.spawn(move || {
                loop {
                    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let task = tasks.lock().ok().and_then(|mut queue| queue.pop_front());
                    let Some(task) = task else { break };
                    let source = task.source.clone();
                    let job_id = task.job.id;
                    let result = process_batch_task(&processor, &store, &settings, task);
                    if result.is_err() && settings.on_error == OnErrorArg::Stop {
                        cancelled.store(true, std::sync::atomic::Ordering::Release);
                    }
                    let _ = result_sender.send((source, job_id, result));
                }
            });
        }
    });
    drop(result_sender);
    let mut first_error = None;
    for (source, job_id, result) in result_receiver {
        if let Err(error) = result {
            failures += 1;
            progress(
                settings.json_progress,
                "failed",
                &source,
                Some(job_id),
                None,
            );
            eprintln!("{}: {error}", source.display());
            first_error.get_or_insert(error);
        }
    }
    if settings.on_error == OnErrorArg::Stop
        && let Some(error) = first_error
    {
        return Err(error);
    }
    if failures > 0 {
        return Err(format!(
            "batch completed with {failures} failed document(s)"
        ));
    }
    Ok(())
}

fn run_doctor(args: DoctorArgs) -> Result<(), String> {
    if !(1..=32).contains(&args.tensorrt_rec_batch) {
        return Err("--tensorrt-rec-batch must be in 1..=32".to_string());
    }
    let tensorrt_paddle = if matches!(args.backend, BackendArg::Auto | BackendArg::TensorRtPaddle) {
        resolve_tensorrt_runtime(
            args.tensorrt_ocr_root.as_deref(),
            &args.tensorrt_dll_dir,
            args.tensorrt_rec_batch,
        )?
    } else {
        None
    };
    let brokered_tensorrt = resolve_brokered_runtime(
        args.backend,
        args.broker_bridge.as_deref(),
        &args.broker_endpoint,
        &args.broker_model,
        args.broker_revision.as_deref(),
    )?;
    let config = PipelineConfig {
        backend: backend(args.backend),
        language: args.language,
        tensorrt_paddle,
        brokered_tensorrt,
        correction_mode: CorrectionMode::Disabled,
        ..PipelineConfig::default()
    };
    let processor = DocumentProcessor::new(config).map_err(|error| error.to_string())?;
    let selected = processor.selected_backend_name();
    let warning = processor.backend_selection_warning();
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "backend": selected,
                "nvidia_hardware_present": nvidia_hardware_present(),
                "warning": warning,
            })
        );
    } else {
        println!("lege-ocr doctor: OK; selected backend `{selected}`");
        if let Some(warning) = warning {
            println!("lege-ocr doctor: {warning}");
        }
    }
    Ok(())
}

fn resolve_tensorrt_runtime(
    root: Option<&Path>,
    dll_directories: &[PathBuf],
    recognition_batch: usize,
) -> Result<Option<TensorRtPaddleConfig>, String> {
    let mut runtime = match root {
        Some(root) => Some(
            TensorRtPaddleConfig::from_root(root, recognition_batch)
                .map_err(|error| error.to_string())?,
        ),
        None => TensorRtPaddleConfig::discover_result(recognition_batch)
            .map_err(|error| error.to_string())?,
    };
    if !dll_directories.is_empty() {
        let runtime = runtime.as_mut().ok_or_else(|| {
            "--tensorrt-dll-dir requires a discoverable runtime or --tensorrt-ocr-root".to_string()
        })?;
        runtime.dll_directories.extend_from_slice(dll_directories);
        runtime.validate().map_err(|error| error.to_string())?;
    }
    Ok(runtime)
}

fn resolve_brokered_runtime(
    backend: BackendArg,
    executable: Option<&Path>,
    endpoint: &str,
    model: &str,
    revision: Option<&str>,
) -> Result<Option<BrokeredTensorRtConfig>, String> {
    if !matches!(backend, BackendArg::BrokeredTensorRt) {
        return Ok(None);
    }
    let config = BrokeredTensorRtConfig {
        executable: executable
            .ok_or_else(|| "--broker-bridge is required for brokered-tensorrt".to_string())?
            .to_path_buf(),
        endpoint: endpoint.to_string(),
        model: model.to_string(),
        revision: revision
            .ok_or_else(|| "--broker-revision is required for brokered-tensorrt".to_string())?
            .to_string(),
    };
    config.validate().map_err(|error| error.to_string())?;
    Ok(Some(config))
}

fn process_batch_task(
    processor: &DocumentProcessor,
    jobs: &JobStore,
    settings: &BatchSettings,
    task: BatchTask,
) -> Result<(), String> {
    jobs.set_job_status(task.job.id, JobStatus::Running, None)
        .map_err(|error| error.to_string())?;
    progress(
        settings.json_progress,
        "started",
        &task.source,
        Some(task.job.id),
        None,
    );
    let result = (|| {
        let existing_pages = if settings.resume && !settings.force {
            load_page_checkpoints(jobs, task.job.id)?
        } else {
            BTreeMap::new()
        };
        let shard_directory = settings
            .output
            .join(".lege-ocr/shards")
            .join(task.job.id.to_string());
        let document = processor
            .process_path_with_checkpoints(&task.source, existing_pages, |page| {
                let shard = shard_directory.join(format!("page-{:06}.json", page.index));
                let bytes = serde_json::to_vec_pretty(page).map_err(|error| error.to_string())?;
                atomic_checkpoint(&shard, &bytes).map_err(|error| error.to_string())?;
                jobs.complete_page(task.job.id, page.index, &shard)
                    .map_err(|error| error.to_string())
            })
            .map_err(|error| error.to_string())?;
        let directory = settings.output.join(&task.stem);
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let request = ExportRequest {
            document: &document,
            output_dir: &directory,
            stem: &task.stem,
            text_view: text_view(settings.text_view),
            overwrite: settings.force || settings.resume,
            searchable_pdf_policy: match settings.pdf_mode {
                PdfModeArg::Preserve => SearchablePdfPolicy::PreserveSource,
                PdfModeArg::Rasterize => SearchablePdfPolicy::Rasterize,
            },
        };
        let artifacts =
            export_all(&request, &task.pending_formats).map_err(|error| error.to_string())?;
        progress(
            settings.json_progress,
            "completed",
            &task.source,
            Some(task.job.id),
            Some(artifacts.len() as u64),
        );
        Ok::<(), String>(())
    })();
    match &result {
        Ok(()) => jobs
            .set_job_status(task.job.id, JobStatus::Complete, None)
            .map_err(|error| error.to_string())?,
        Err(error) => jobs
            .set_job_status(task.job.id, JobStatus::Failed, Some(error))
            .map_err(|database_error| database_error.to_string())?,
    }
    result
}

fn artifact_exists(directory: &Path, stem: &str, format: ExportFormat) -> bool {
    match format {
        ExportFormat::Json => {
            directory.join(format!("{stem}.lege.json")).is_file()
                && directory.join(format!("{stem}.qa.json")).is_file()
                && directory
                    .join(format!("{stem}.processing-manifest.json"))
                    .is_file()
        }
        ExportFormat::Text => directory.join(format!("{stem}.txt")).is_file(),
        ExportFormat::Html => directory.join(format!("{stem}.html/index.html")).is_file(),
        ExportFormat::SearchablePdf => directory.join(format!("{stem}.searchable.pdf")).is_file(),
        ExportFormat::Docx => directory.join(format!("{stem}.docx")).is_file(),
        ExportFormat::Alto => directory.join(format!("{stem}.alto.xml")).is_file(),
        ExportFormat::PageXml => directory.join(format!("{stem}.page.xml")).is_file(),
        ExportFormat::Markdown => directory.join(format!("{stem}.md")).is_file(),
        ExportFormat::Latex => directory.join(format!("{stem}.tex")).is_file(),
        ExportFormat::Xlsx => directory.join(format!("{stem}.tables.xlsx")).is_file(),
        ExportFormat::Csv => directory.join(format!("{stem}.tables.csv")).is_file(),
        ExportFormat::Hocr => directory.join(format!("{stem}.hocr.html")).is_file(),
        ExportFormat::PdfA => false,
    }
}

fn load_page_checkpoints(
    store: &JobStore,
    job_id: i64,
) -> Result<BTreeMap<u32, lege_docir::Page>, String> {
    let mut pages = BTreeMap::new();
    for (page_index, path) in store
        .completed_page_shards(job_id)
        .map_err(|error| error.to_string())?
    {
        let page = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<lege_docir::Page>(&bytes).ok());
        match page {
            Some(page) if page.index == page_index => {
                pages.insert(page_index, page);
            }
            _ => store
                .reset_page(job_id, page_index)
                .map_err(|error| error.to_string())?,
        }
    }
    Ok(pages)
}

fn discover(
    inputs: &[PathBuf],
    recursive: bool,
    excluded_root: Option<&Path>,
) -> Result<Vec<PathBuf>, String> {
    let mut found = BTreeSet::new();
    let mut visited_directories = BTreeSet::new();
    let excluded_root = excluded_root.and_then(|path| path.canonicalize().ok());
    for input in inputs {
        discover_one(
            input,
            recursive,
            excluded_root.as_deref(),
            &mut visited_directories,
            &mut found,
        )?;
    }
    Ok(found.into_iter().collect())
}

fn discover_one(
    path: &Path,
    recursive: bool,
    excluded_root: Option<&Path>,
    visited_directories: &mut BTreeSet<PathBuf>,
    found: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    if path.is_file() {
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            found.insert(path.to_path_buf());
        }
        return Ok(());
    }
    if !path.is_dir() {
        return Err(format!("input does not exist: {}", path.display()));
    }
    let canonical = path.canonicalize().map_err(|error| error.to_string())?;
    if excluded_root.is_some_and(|excluded| canonical.starts_with(excluded))
        || !visited_directories.insert(canonical)
    {
        return Ok(());
    }
    for entry in std::fs::read_dir(path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let child = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() && recursive {
            discover_one(&child, true, excluded_root, visited_directories, found)?;
        } else if child.is_file()
            && child
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            found.insert(child);
        }
    }
    Ok(())
}

fn pdf_page_count(path: &Path) -> Option<u32> {
    let bytes: Arc<[u8]> = std::fs::read(path).ok()?.into();
    lege_pdf_read::RenderSession::open(bytes, None)
        .ok()
        .map(|session| session.page_count())
}

fn output_stem(path: &Path, hash: &str) -> String {
    let base = path
        .file_stem()
        .map(|stem| stem.to_string_lossy())
        .unwrap_or_else(|| "document".into());
    let safe = base
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let suffix = hash
        .strip_prefix("blake3:")
        .unwrap_or(hash)
        .chars()
        .take(8)
        .collect::<String>();
    format!("{safe}-{suffix}")
}

fn progress(json: bool, event: &str, source: &Path, job_id: Option<i64>, count: Option<u64>) {
    if json {
        println!(
            "{}",
            serde_json::json!({"event": event, "source": source, "job_id": job_id, "artifact_count": count})
        );
    } else {
        println!("[{event}] {}", source.display());
    }
}

fn profile(value: ProfileArg) -> ProcessingProfile {
    match value {
        ProfileArg::Search => ProcessingProfile::Search,
        ProfileArg::Structured => ProcessingProfile::Structured,
        ProfileArg::Scientific => ProcessingProfile::Scientific,
    }
}
fn backend(value: BackendArg) -> BackendChoice {
    match value {
        BackendArg::Auto => BackendChoice::Auto,
        BackendArg::TensorRtPaddle => BackendChoice::TensorRtPaddle,
        BackendArg::BrokeredTensorRt => BackendChoice::BrokeredTensorRt,
        BackendArg::Paddle => BackendChoice::Paddle,
        BackendArg::WindowsAi => BackendChoice::WindowsAi,
        BackendArg::WinocrLegacy => BackendChoice::WinOcrLegacy,
    }
}
fn text_view(value: TextViewArg) -> TextView {
    match value {
        TextViewArg::Raw => TextView::Raw,
        TextViewArg::Normalized => TextView::Normalized,
        TextViewArg::Corrected => TextView::Corrected,
    }
}

use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_test_support::builder::PdfBuilder;
    #[test]
    fn output_names_are_deterministic_and_collision_safe() {
        assert_eq!(
            output_stem(Path::new("A Book.pdf"), "blake3:1234567890"),
            "A-Book-12345678"
        );
    }

    #[test]
    fn discovery_excludes_nested_output_tree() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("processed");
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(directory.path().join("input.pdf"), b"pdf").unwrap();
        std::fs::write(output.join("old.searchable.pdf"), b"pdf").unwrap();
        let found = discover(&[directory.path().to_path_buf()], true, Some(&output)).unwrap();
        assert_eq!(found, vec![directory.path().join("input.pdf")]);
    }

    #[test]
    fn native_text_batch_exports_and_resumes_without_invoking_ocr() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("book.pdf");
        let output = directory.path().join("out");
        let mut pdf = PdfBuilder::new();
        pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
        pdf.add_object(
            2,
            "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 300 200]>>",
        );
        pdf.add_object(
            3,
            "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
        );
        pdf.add_stream(4, "", b"BT /F1 20 Tf 20 100 Td (Illinois will) Tj ET");
        pdf.add_object(
            5,
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>",
        );
        pdf.finish_classic_xref("/Root 1 0 R");
        std::fs::write(&source, pdf.into_bytes()).unwrap();

        let args = || BatchArgs {
            inputs: vec![source.clone()],
            output: output.clone(),
            recursive: false,
            profile: ProfileArg::Search,
            backend: BackendArg::WinocrLegacy,
            language: "eng".into(),
            formats: vec!["json".into(), "text".into(), "docx".into()],
            text_view: TextViewArg::Corrected,
            resume: true,
            force: false,
            on_error: OnErrorArg::Stop,
            json_progress: false,
            dictionary: None,
            no_spellcheck: true,
            apply_spelling_edits: false,
            force_ocr: false,
            render_dpi: 300,
            max_page_pixels: 40_000_000,
            model_pack: None,
            tensorrt_ocr_root: None,
            tensorrt_dll_dir: Vec::new(),
            tensorrt_rec_batch: 8,
            broker_bridge: None,
            broker_endpoint: "evidence-trt".into(),
            broker_model: "turbo-ocr".into(),
            broker_revision: None,
            pdf_mode: PdfModeArg::Preserve,
            workers: 2,
            gpu_batch_lines: 64,
            gpu_batch_pixels: 12_000_000,
            gpu_batch_wait_ms: 3,
            gpu_queue_capacity: 32,
        };
        run_batch(args()).unwrap();
        run_batch(args()).unwrap();
        let stem = output_stem(&source, &fingerprint(&source).unwrap().content_hash);
        assert!(
            output
                .join(&stem)
                .join(format!("{stem}.lege.json"))
                .is_file()
        );
        assert_eq!(
            std::fs::read_to_string(output.join(&stem).join(format!("{stem}.txt"))).unwrap(),
            "Illinois will"
        );
    }
}
