You can do this cleanly, but the key is **not** “GUI calls CLI and parses normal stdout.” The repo already has a shared progress model, so the right plan is:

**GUI launches `lege` as a hidden worker process.
CLI emits structured machine-readable progress events.
GUI maps those events into the same `AppState` fields it already uses.
Then the GUI drops its dependency on the core `lege` processing crate.**

That gives you the binary-size win without degrading the GUI behavior.

## Current situation

Your workspace currently builds both the root `lege` binary and `GUI/Freya` as default members, and the GUI crate directly depends on the root `lege` crate. That means the GUI binary pulls in a large part of the processing stack, including image/PDF/encoding dependencies, even though the CLI already contains the same logic.  

The GUI backend is doing real processing today. `process_pdf_async()` converts GUI options into `PipelineConfig`, then calls `lege::progress::spawn_file_processing_task()`.  For PDFs, `start_async_processing()` creates output filenames and starts in-process processing through that path. 

The good news: progress is already abstracted. You have `ProcessingStatus`, `ProgressUpdate`, and `ProgressMetrics`, including numeric counters for total pages, rendered, detected, encoded, deskewed, mode, layout/deskew flags, and ETA.  The GUI already consumes these events and maps them into `progress_metrics`, `active_eta`, status lines, completion popups, logs, and queue removal.  

So the minimal viable design is: **preserve the event shape, move the sender across process boundaries.**

---

## Phase 1: Add a machine-readable CLI worker mode

Add a hidden CLI mode, for example:

```text
lege --worker-json <input> --output <output-path> [same existing processing flags...]
```

or, better:

```text
lege --gui-worker --input <path> --output <path> [same flags...]
```

This mode should do three things differently from normal CLI mode:

1. **Disable interactive prompts.**
2. **Suppress human console progress.**
3. **Write newline-delimited JSON events to stdout.**

Do not reuse the existing colored three-line CLI renderer for GUI communication. It is designed for terminal display and uses ANSI/cursor control.  Instead, create a second renderer beside it, maybe:

```rust
pub async fn run_cli_json_events(
    rx: flume::Receiver<crate::progress::ProgressUpdate>,
) -> anyhow::Result<()>
```

Each event should be one JSON object per line:

```json
{"type":"status","task_id":1,"status":"LayoutProgress","metrics":{"pages_total":51,"rendered":12,"detected":10,"encoded":8,"deskewed":0,"mode":"Layout","is_djvu":false,"enable_layout_detection":true,"enable_deskew":false,"eta_seconds":42}}
{"type":"completed","task_id":1,"message":"Successfully processed 51 pages to C:\\out\\book_processed_ccitt4.pdf"}
{"type":"error","task_id":1,"error":"Missing required component(s): pdfium"}
```

Use **stdout only for protocol events**. Send diagnostics, debug logs, warnings, and human text to stderr. This lets the GUI parse stdout line-by-line without accidental garbage.

### Required code change

Your `ProgressUpdate`, `ProcessingStatus`, `ProgressMetrics`, and `ProgressMode` currently derive `Debug`/`Clone`, but not `Serialize`/`Deserialize`. Add serde derives:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ProgressUpdate { ... }

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProcessingStatus { ... }

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProgressMetrics { ... }

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ProgressMode { ... }
```

Then `run_cli_json_events()` can mostly be:

```rust
while let Ok(update) = rx.recv_async().await {
    println!("{}", serde_json::to_string(&update)?);
    std::io::stdout().flush()?;

    if matches!(
        update,
        ProgressUpdate::Completed { .. } | ProgressUpdate::Error { .. }
    ) {
        break;
    }
}
```

---

## Phase 2: Refactor CLI normal processing into a callable worker path

Right now `main.rs` manually extracts options into `CliOptions`, then branches into debug modes, normal processing, PNG modes, image modes, etc.  

Do **not** duplicate that logic for worker mode. Instead, make worker mode a thin variant of the existing direct CLI path.

A practical structure:

```rust
enum OutputMode {
    HumanCli,
    JsonEvents,
}

fn run_direct_job_from_args(
    args: Vec<String>,
    output_mode: OutputMode,
) -> anyhow::Result<i32>
```

For `HumanCli`, keep the existing terminal behavior.

For `JsonEvents`, create a progress manager/tracker, run the same processing path, and attach the JSON progress renderer instead of `run_cli_three_line()`.

The important part is that the worker mode should not invent a second `PipelineConfig` conversion path. It should consume the existing CLI flags, because your premise is correct: the CLI already exposes the GUI’s options and more.

---

## Phase 3: Add GUI-side command generation

Replace `gui_options_to_pipeline_config()` with a new function that maps `ProcessingOptions` to CLI args:

```rust
pub fn gui_options_to_cli_args(
    input_path: &Path,
    output_path: &Path,
    options: &ProcessingOptions,
) -> Vec<OsString>
```

For example:

```rust
args.push("--gui-worker");
args.push(input_path);
args.push("--output");
args.push(output_path);

if matches!(options.output_format, OutputFormat::Djvu) {
    args.extend(["--text-format", "djvu"]);
} else if options.layout_analysis {
    match options.image_processing_type {
        ImageProcessingType::Original => args.extend(["--text-format", "ccitt4"]),
        ImageProcessingType::Dithered => args.extend(["--text-format", "jbig2"]),
    }
} else {
    args.push("--no-layout");
    match options.compression_type {
        CompressionType::Ccitt4 => args.extend(["--text-format", "ccitt4"]),
        CompressionType::Jbig2 => args.extend(["--text-format", "jbig2"]),
    }
}

if options.use_ocr {
    args.push("--ocr");
} else {
    args.push("--no-ocr");
}

if options.deskew_documents {
    args.push("--deskew");
}

if options.jpeg_compat {
    args.push("--jpeg-compat");
}

if options.high_quality_output {
    args.push("--high-quality");
}

if options.invert_input {
    args.push("--invert");
}

if options.center_margins {
    args.push("--center-margins");
}

if options.crop_margins {
    args.push("--crop-margins");
}

if options.use_fixed_threshold {
    args.extend(["--binarization", "fixed"]);
    args.extend(["--threshold", &options.threshold_value.to_string()]);
} else if options.use_heavy_binarization {
    args.extend(["--binarization", "heavy"]);
} else {
    args.extend(["--binarization", "adaptive"]);
    args.extend(["--sauvola-k", &options.k_factor.to_string()]);
}
```

Use `std::process::Command` with `OsString`/`PathBuf`, not shell strings. That avoids Windows quoting issues.

---

## Phase 4: Replace in-process GUI processing with a subprocess runner

Create a GUI-only module:

```text
GUI/Freya/src/worker_process.rs
```

Core responsibilities:

```rust
pub struct WorkerHandle {
    pub child: tokio::process::Child,
    pub task_id: u64,
    pub input_path: PathBuf,
    pub output_path: PathBuf,
}

pub async fn spawn_lege_worker(
    input_path: PathBuf,
    output_path: PathBuf,
    options: ProcessingOptions,
) -> anyhow::Result<WorkerHandle>
```

Implementation details:

```rust
let mut cmd = tokio::process::Command::new(resolve_cli_path()?);
cmd.args(gui_options_to_cli_args(&input_path, &output_path, &options));
cmd.stdin(std::process::Stdio::null());
cmd.stdout(std::process::Stdio::piped());
cmd.stderr(std::process::Stdio::piped());

#[cfg(windows)]
{
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

let mut child = cmd.spawn()?;
```

On Windows, `CREATE_NO_WINDOW` is the relevant part for keeping the CLI hidden. You should also make sure `lege.exe` is bundled next to `lege-gui.exe`, then resolve it by:

```rust
let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();
let cli = exe_dir.join(if cfg!(windows) { "lege.exe" } else { "lege" });
```

Keep stderr captured and append it to the GUI log/debug view. Do not parse stderr for progress.

---

## Phase 5: Preserve the current GUI progress behavior

The current GUI listener is already almost exactly what you want. It subscribes to `ProgressUpdate`, stores `progress_metrics`, derives `active_eta`, updates status lines, handles footnote popups, handles completed/error events, updates logs, and removes completed queue items.  

You can preserve that logic by making the subprocess parser produce the same `ProgressUpdate` values.

Instead of:

```rust
let progress_manager = lege::progress::get_progress_manager();
let receiver = progress_manager.subscribe();
```

you will have a local channel:

```rust
let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WorkerEvent>();
```

or use `flume` to minimize rewrites:

```rust
let (tx, receiver) = flume::unbounded::<ProgressUpdate>();
```

Then the worker stdout reader does:

```rust
let reader = BufReader::new(stdout);
let mut lines = reader.lines();

while let Some(line) = lines.next_line().await? {
    match serde_json::from_str::<ProgressUpdate>(&line) {
        Ok(update) => {
            let _ = tx.send(update);
        }
        Err(e) => {
            // log malformed protocol line, but do not crash GUI
        }
    }
}
```

Then the existing match over `ProgressUpdate::Status`, `Completed`, and `Error` can remain mostly unchanged.

---

## Phase 6: Cancellation

Current cancellation calls `lege::progress::cancel_task(info.id)` for in-process jobs.  That will not work across process boundaries.

For the first subprocess version, cancellation should simply kill the child process:

```rust
let _ = child.kill().await;
```

This is acceptable because each CLI worker is one document. If the user cancels a batch, kill all active children and mark the queued/running items as cancelled.

Later, add graceful cancellation:

```text
lege --gui-worker --control-pipe <named-pipe>
```

Then the GUI can send:

```json
{"type":"cancel"}
```

and the CLI can translate it into your existing `ShutdownSignal`. But that is a Phase 2 improvement. Process kill is simpler and robust enough for the first migration.

---

## Phase 7: Decide batch model: one CLI process per file

Use **one CLI subprocess per queue item**, not one long-lived CLI process for the whole queue.

Reasons:

1. It maps to your current `TrackerInfo { id, input_path, output_path }` model. 
2. Failure isolation is better. A bad PDF kills one worker, not the whole batch.
3. Cancellation is easier.
4. The GUI can keep the same queue-completion logic.
5. It avoids having to design a long-running RPC protocol immediately.

The only downside is repeated startup cost, especially Pdfium/runtime initialization. For Lege’s workload, that is probably acceptable unless users batch many tiny files. If that becomes a problem, move to a persistent worker later.

---

## Phase 8: Move ZIP/image-folder handling out of the GUI

Right now the GUI has its own ZIP extraction and image-folder processing path, including ZIP recursion, tempdir extraction, image extension filtering, page counting, and `run_png_mode_with_config()`.  

This is exactly the kind of duplication you are trying to remove.

The target state should be:

```text
GUI:
- choose file/folder
- precheck display only
- generate output path
- spawn CLI

CLI:
- PDF processing
- image folder processing
- ZIP extraction
- image-to-image mode
- all runtime checks
- all processing
```

Your CLI already has `--png-folder`, `--images-to-images`, and related direct modes in `CliOptions`.   So the GUI should map:

```text
PDF file       -> lege --gui-worker <file.pdf> ...
image folder   -> lege --gui-worker <folder> --png-folder ...
ZIP archive    -> either CLI gains native ZIP input, or GUI temporarily extracts ZIP
```

For ZIPs, I would add CLI-native ZIP handling rather than keep it in the GUI. It removes `zip`, `miniz_oxide`, `tempfile`, and related duplicate code from the GUI crate.

---

## Phase 9: Shrink GUI dependencies

Once the subprocess path works, remove these from `GUI/Freya/Cargo.toml` where possible:

```toml
Legencode = { path = "../../legencode" }
lege = { path = "../.." }
image = { workspace = true }
zip = { workspace = true }
miniz_oxide = { workspace = true }
tempfile = { workspace = true }
lopdf = { workspace = true }
```

The current GUI directly depends on both `Legencode` and `lege`, plus ZIP/image/PDF helpers.  After migration, the GUI should ideally depend on:

```toml
freya
rfd
tokio
serde
serde_json
dirs
uuid
chrono
once_cell
```

Possibly keep `lopdf` only if you still want fast page-count precheck in the GUI. But for size, even that can move to the CLI:

```text
lege --probe-json <path>
```

returns:

```json
{"kind":"pdf","pages":312,"has_ocr":true}
```

Then GUI can drop PDF parsing entirely.

---

## Recommended protocol

Use newline-delimited JSON, not a socket, not temp files, not terminal parsing.

### `ProgressUpdate` line

```json
{
  "kind": "progress_update",
  "event": {
    "Status": {
      "task_id": 1,
      "status": {
        "LayoutProgress": {
          "rendered": 10,
          "detected": 9,
          "encoded": 7,
          "deskewed": 0,
          "total": 50,
          "enable_layout_detection": true,
          "enable_deskew": false,
          "eta": "01m20s"
        }
      },
      "metrics": {
        "pages_total": 50,
        "rendered": 10,
        "detected": 9,
        "encoded": 7,
        "deskewed": 0,
        "mode": "Layout",
        "is_djvu": false,
        "enable_layout_detection": true,
        "enable_deskew": false,
        "eta_seconds": 80
      }
    }
  }
}
```

You can either serialize the Rust enum directly or create a flatter stable protocol type. I would create a separate protocol type:

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    Started {
        task_id: u64,
        input_path: PathBuf,
        output_path: PathBuf,
    },
    Progress {
        task_id: u64,
        status: ProcessingStatus,
        metrics: Option<ProgressMetrics>,
    },
    Completed {
        task_id: u64,
        message: String,
        output_path: PathBuf,
        metrics: Option<ProgressMetrics>,
    },
    Error {
        task_id: u64,
        error: String,
        metrics: Option<ProgressMetrics>,
    },
    Log {
        level: String,
        message: String,
    },
}
```

This avoids locking your IPC format to the exact internal enum layout forever.

---

## Best implementation order

### 1. Add serde derives and JSON renderer

This is the lowest-risk step. The CLI still works the same, but can now emit structured events.

### 2. Add `--gui-worker`

Make it work for a single PDF first. Do not touch ZIP/folder handling yet.

Test manually:

```powershell
.\lege.exe --gui-worker "input.pdf" --output "out.pdf" --text-format ccitt4 --binarization adaptive 1200
```

Expected stdout: JSON lines only.

### 3. Add GUI `Command` runner

Replace only the PDF path in `backend::start_async_processing()` first. Keep the old in-process path behind a temporary feature flag:

```rust
#[cfg(feature = "inprocess-engine")]
```

This lets you compare behavior.

### 4. Convert image folders

Map folder input to the CLI’s existing image-folder mode.

### 5. Convert ZIPs

Add CLI-native ZIP handling or a CLI `--zip-images` mode, then delete the GUI ZIP extraction path.

### 6. Remove `lege` and `Legencode` from GUI dependencies

This is where the binary-size reduction becomes real. Until the dependency is removed, you may still be compiling/linking the processing stack into the GUI.

---

## Subtle issue: output filename consistency

The GUI currently generates output filenames itself.  That is fine in the short term because it lets the GUI know exactly where the output will land before launching the worker.

Longer term, I would move filename generation to the CLI too:

```text
lege --plan-json <input> --output-dir <dir> [flags]
```

returns:

```json
{"output_path":"C:\\...\\book_processed_ccitt4_1781234567.pdf"}
```

But that can wait. For the first migration, keep GUI-side output naming and pass the exact output path to the CLI.

---

## What this buys you

The main binary-size benefit comes when `lege-gui` no longer depends on:

```toml
lege = { path = "../.." }
Legencode = { path = "../../legencode" }
```

Right now the GUI crate imports the processing crate and calls into it directly.  After the subprocess migration, the GUI becomes a controller/view layer. It does not need Pdfium, encoders, OCR, GPU inference, binarization, or the pipeline code linked into it.

The CLI remains 14 MB. The GUI still has Freya/Skia overhead until you solve that separately, but you remove the duplicated Lege processing payload from the GUI.

---

## Recommended final architecture

```text
lege.exe
  - owns all document processing
  - owns Pdfium/encoding/OCR/GPU/runtime setup
  - owns CLI flags
  - owns ZIP/folder/PDF processing
  - emits JSON progress in worker mode

lege-gui.exe
  - owns file picker
  - owns settings UI
  - owns queue state
  - translates ProcessingOptions -> CLI args
  - spawns hidden lege.exe
  - reads JSON progress from stdout
  - kills child process on cancel
  - displays progress exactly as before
```

The central rule: **the GUI should never construct `PipelineConfig` again.** It should construct CLI arguments. That makes the CLI the single runtime contract and ends the duplication.
