// GUI backend — subprocess-based processing.
// All heavy processing is delegated to `lege --gui-worker`.

use anyhow::Result;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{ImageProcessingType, OutputFormat, ProcessingOptions};
use crate::worker_process::{WorkerHandle, WorkerProgressUpdate, spawn_lege_worker, probe_file_json};

use crate::models::DocumentItem;
use std::collections::VecDeque;
// Re-export settings helpers for convenience
pub use crate::settings::{
    clear_settings as remove_saved_settings, load_settings as load_saved_settings,
    save_settings as persist_settings,
};

/// Information about a spawned processing task
#[derive(Clone, Debug)]
pub struct TrackerInfo {
    pub id: u64,
    pub input_path: PathBuf,
    pub output_path: PathBuf,
}

/// Check if a PDF file has OCR/text layers using the CLI --probe-json mode.
/// Returns Ok(Some(true)) if OCR found, Ok(Some(false)) if no OCR, Ok(None) if not a PDF, Err on failure
pub async fn check_pdf_has_ocr(pdf_path: &PathBuf) -> Result<Option<bool>> {
    if !is_pdf_file(pdf_path) {
        return Ok(None);
    }
    match probe_file_json(pdf_path).await {
        Ok(json) => {
            let has_ocr = json.get("has_ocr").and_then(|v| v.as_bool());
            Ok(has_ocr)
        }
        Err(_) => Ok(None),
    }
}

/// Estimate the original size for a page range based on file size proportions.
/// Now purely file-system based (no PDF parsing).
pub async fn estimate_original_size_for_page_range(
    input_path: &PathBuf,
    page_range: &Option<String>,
    fallback_size: u64,
) -> u64 {
    if page_range.is_none() {
        return fallback_size;
    }
    // Without full PDF parsing, fall back to the total file size.
    fallback_size
}

/// Validate that a path is a ZIP file
pub fn is_zip_file(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase() == "zip")
        .unwrap_or(false)
}


/// Validate that a path is a PDF file
pub fn is_pdf_file(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase() == "pdf")
        .unwrap_or(false)
}

/// Get all PDF files in a directory
pub fn get_pdf_files_in_directory(dir: &PathBuf) -> Result<Vec<PathBuf>> {
    let mut pdf_files = Vec::new();

    if !dir.is_dir() {
        return Ok(pdf_files);
    }

    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && is_pdf_file(&path) {
            pdf_files.push(path);
        }
    }

    pdf_files.sort();
    Ok(pdf_files)
}

/// Supported image extensions for image folder processing
const SUPPORTED_IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "ppm", "pbm", "pgm", "pnm", "tiff", "tif", "bmp", "jp2",
];

/// Check if a file is a supported image file
pub fn is_image_file(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| SUPPORTED_IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Get all supported image files in a directory
pub fn get_image_files_in_directory(dir: &PathBuf) -> Result<Vec<PathBuf>> {
    let mut image_files = Vec::new();

    if !dir.is_dir() {
        return Ok(image_files);
    }

    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && is_image_file(&path) {
            image_files.push(path);
        }
    }

    image_files.sort();
    Ok(image_files)
}

/// Quickly count the number of pages/images for a queue item via `lege --probe-json`.
pub async fn precheck_page_count(path: PathBuf) -> Option<u32> {
    if path.is_dir() {
        // Fast local count for image directories — no subprocess needed.
        let n = get_image_files_in_directory(&path).ok()?.len();
        return Some(n as u32);
    }
    match probe_file_json(&path).await {
        Ok(json) => json.get("pages").and_then(|v| v.as_u64()).map(|v| v as u32),
        Err(_) => None,
    }
}

/// Calculate total size of a path - handles both files and directories
/// For directories (image folders), sums up all supported image file sizes
pub fn calculate_path_size(path: &PathBuf) -> u64 {
    if path.is_file() {
        // Regular file - just get its size
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    } else if path.is_dir() {
        // Directory (image folder) - sum all image file sizes
        get_image_files_in_directory(path)
            .unwrap_or_default()
            .iter()
            .filter_map(|img_path| std::fs::metadata(img_path).ok())
            .map(|m| m.len())
            .sum()
    } else {
        0
    }
}


/// Generate an appropriate output filename based on the input and processing options
pub fn generate_output_filename(
    input_path: &PathBuf,
    output_base: &PathBuf,
    options: &ProcessingOptions,
) -> PathBuf {
    let file_stem = input_path.file_stem().unwrap_or_default().to_string_lossy();
    // Determine container extension and descriptive parts
    let is_djvu = matches!(options.output_format, OutputFormat::Djvu);
    let text_fmt = if is_djvu {
        "djvu"
    } else {
        match &options.image_processing_type {
            ImageProcessingType::Original => "ccitt4",
            ImageProcessingType::Dithered => {
                if options.ccitt4_dithered_images {
                    "ccitt4"
                } else {
                    "jbig2"
                }
            }
        }
    };
    // Unix timestamp
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename = if is_djvu {
        format!("{}_processed_{}_{}.djvu", file_stem, text_fmt, ts)
    } else {
        // PDF format: just include the text format, no cover format suffix
        format!("{}_processed_{}_{}.pdf", file_stem, text_fmt, ts)
    };

    if output_base.is_dir() {
        output_base.join(filename)
    } else if output_base.extension().is_some() {
        // If output_base has an extension, use it as the full output path
        output_base.clone()
    } else {
        // If output_base is a directory path without trailing slash
        output_base.join(filename)
    }
}

/// Start async processing for multiple files using subprocess workers.
///
/// Returns (tracker_infos, active_workers, merged_events_receiver).
/// The caller is responsible for monitoring workers and handling cancellation.
pub async fn start_async_processing(
    queue: VecDeque<DocumentItem>,
    options: &ProcessingOptions,
) -> Result<(
    Vec<TrackerInfo>,
    Vec<WorkerHandle>,
    flume::Receiver<WorkerProgressUpdate>,
)> {
    if queue.is_empty() {
        return Err(anyhow::anyhow!("No files in queue"));
    }

    let output_path = options
        .output_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No output path specified"))?;

    let (events_tx, events_rx) = flume::unbounded::<WorkerProgressUpdate>();

    let mut tracker_infos = Vec::new();
    let mut worker_handles = Vec::new();
    let mut next_task_id: u64 = 0;

    for document in queue.iter() {
        let task_id = next_task_id;
        next_task_id += 1;

        let input_path = document.file_path.clone();
        let output_file_path = generate_output_filename(&input_path, output_path, options);

        match spawn_lege_worker(
            task_id,
            input_path.clone(),
            output_file_path.clone(),
            options,
            events_tx.clone(),
            None,
        ) {
            Ok(handle) => {
                tracker_infos.push(TrackerInfo {
                    id: task_id,
                    input_path,
                    output_path: output_file_path,
                });
                worker_handles.push(handle);
            }
            Err(e) => {
                eprintln!(
                    "Failed to spawn worker for {}: {}",
                    document.file_path.display(),
                    e
                );
                // Synthesise an error event so the GUI removes the item.
                let _ = events_tx.send(WorkerProgressUpdate::Error {
                    task_id,
                    error: e.to_string(),
                    metrics: None,
                });
            }
        }
    }

    Ok((tracker_infos, worker_handles, events_rx))
}

/// Open folder in system file explorer
pub fn open_folder_in_explorer(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        return Err(anyhow::anyhow!("Path does not exist: {}", path.display()));
    }

    let folder_path = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(folder_path)
            .spawn()?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(folder_path)
            .spawn()?;
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(folder_path)
            .spawn()?;
    }

    Ok(())
}

/// Open a file or URL with the system default handler.
pub fn open_with_system(target: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", target])
            .spawn()?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(target).spawn()?;
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(target).spawn()?;
    }

    Ok(())
}

/// Resolve a bundled docs file located next to the installed binary under `docs/`.
pub fn bundled_docs_path(file_name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    Some(exe_dir.join("docs").join(file_name))
}

// A helper function to truncate the path from the beginning.
// This preserves the file/folder name which is often more useful.
pub fn truncate_path(path: &std::path::Path, max_len: usize) -> String {
    let path_str = path.to_string_lossy();
    if path_str.len() <= max_len {
        return path_str.to_string();
    }

    // We'll try to preserve the end of the path
    let mut components: Vec<_> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    let mut result = String::new();

    while let Some(part) = components.pop() {
        let separator = if result.is_empty() {
            ""
        } else {
            std::path::MAIN_SEPARATOR_STR
        };
        let new_result = format!("{}{}{}", part, separator, result);
        if new_result.len() + 4 > max_len {
            // +4 for "..." and a separator
            break;
        }
        result = new_result;
    }

    format!("...{}{}", std::path::MAIN_SEPARATOR_STR, result)
}
