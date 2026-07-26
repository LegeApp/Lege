//! Render pages with both this engine and PDFium, and report where we differ.
//!
//! The roadmap calls PDFium "the differential-testing oracle" (line 18) and
//! reserves `tools/` for exactly this. The point is not pixel equality —
//! anti-aliasing and glyph rasterization differ legitimately — but to find
//! the pages where we are *wrong*: blank, boxed, mis-coloured, missing
//! images. Those show up as an ink-coverage disagreement, which no amount of
//! AA difference produces.
//!
//! ```text
//! pdfium-diff <libpdfium.so> <scale> <file.pdf|dir> [more…]
//! ```
//!
//! Suspect pages are dumped as `ours | pdfium | difference` triptychs into
//! `./pdfium-diff-out/`.

mod compare;
mod multi;
mod pdfium;
mod renderers;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use compare::{compare, write_triptych, Diff};
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, OutputFormat, OutputResidency, PageTransform, RenderBackend,
    RenderError, RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;
use pdf_render_scheduler::{PipelineOutput, RenderScheduler, SchedulerOptions};
use pdf_source::{MmapSource, OwnedBytesSource, PdfSource};

/// Pages sampled per document, spread across it.
const SAMPLE: usize = 6;

/// Whether to write triptych PPMs at all (`PDFIUM_DIFF_DUMP=1`).
///
/// Off by default, because the images are not the product and cannot be
/// consumed at corpus scale: a sweep meets ~33k suspect pages and a triptych
/// is ~47MB, which is ~190GB nobody will ever look at. The CSV is the
/// artifact -- it is what the by-cause ranking reads, and it is what catches
/// the failures that do *not* surface as errors (a Separation page that
/// renders blank "succeeds"; only the ink metric knows).
///
/// Rendering is deterministic on both sides, so any row re-renders
/// bit-identically on demand: to look at one, re-run this tool on that single
/// PDF with `PDFIUM_DIFF_DUMP=1`. That is the investigation path, and it is
/// the only time a human is actually going to open a PPM.
fn dump_enabled() -> bool {
    std::env::var("PDFIUM_DIFF_DUMP").is_ok_and(|v| v != "0")
}

struct Row {
    file: String,
    page: i32,
    diff: Diff,
    /// Image draws dropped because a codec could not decode (Workstream B3).
    degraded: u32,
    note: &'static str,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("render" | "sweep" | "benchmark")
    ) {
        let command = &args[0];
        if let Err(error) = multi::run(command, &args[1..]) {
            eprintln!("{error}");
            std::process::exit(2);
        }
        return;
    }
    // Private process boundary used by the optional PDFium adapter.
    // Pages are zero-based and output names match the common adapter protocol.
    if args.first().map(String::as_str) == Some("--reference-worker") {
        if args.len() != 6 {
            eprintln!(
                "usage: pdfium-diff --reference-worker <libpdfium> <scale> <pdf> <pages> <out>"
            );
            std::process::exit(2);
        }
        let pdfium = load_pdfium(Path::new(&args[1]));
        let scale: f64 = args[2].parse().expect("scale must be a number");
        let pdf = Path::new(&args[3]);
        let pages = args[4].split(',').filter_map(|p| p.parse::<i32>().ok());
        let out = Path::new(&args[5]);
        let _ = std::fs::create_dir_all(out);
        for page in pages {
            let bitmap = pdfium.render(pdf, page, scale).unwrap_or_else(|error| {
                eprintln!("pdfium page {page}: {error}");
                std::process::exit(1);
            });
            let mut rgba = bitmap.bgra;
            for pixel in rgba.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            let path = out.join(format!("page-{page}.png"));
            write_rgba_png(&path, bitmap.width, bitmap.height, &rgba).unwrap_or_else(|error| {
                eprintln!("write {}: {error}", path.display());
                std::process::exit(1);
            });
        }
        return;
    }
    // Internal worker mode. The controller starts one worker per document so
    // a native PDFium abort is contained to that document instead of ending a
    // multi-hour corpus run.
    //   pdfium-diff --worker <libpdfium> <scale> <out-dir> <file.pdf>
    if args.first().map(String::as_str) == Some("--worker") {
        if args.len() != 5 {
            eprintln!("usage: pdfium-diff --worker <libpdfium> <scale> <out-dir> <file.pdf>");
            std::process::exit(2);
        }
        let lib = PathBuf::from(&args[1]);
        let scale: f64 = args[2].parse().expect("scale must be a number");
        let out_dir = PathBuf::from(&args[3]);
        let file = PathBuf::from(&args[4]);
        let pdfium = load_pdfium(&lib);
        run_diff(&pdfium, scale, vec![file], out_dir, dump_enabled());
        return;
    }
    // Batched worker mode (the supervisor's unit of dispatch): iterate a
    // CHUNK of files in-process — one exe+dll load per chunk instead of per
    // file — appending rows to a private fragment CSV the controller merges.
    //   pdfium-diff --worker-chunk <libpdfium> <scale> <out-dir> <manifest> <fragment> <marker>
    if args.first().map(String::as_str) == Some("--worker-chunk") {
        if args.len() != 7 {
            eprintln!(
                "usage: pdfium-diff --worker-chunk <libpdfium> <scale> <out-dir> <manifest> <fragment> <marker>"
            );
            std::process::exit(2);
        }
        let lib = PathBuf::from(&args[1]);
        let scale: f64 = args[2].parse().expect("scale must be a number");
        run_worker_chunk(
            &lib,
            scale,
            PathBuf::from(&args[3]),
            &PathBuf::from(&args[4]),
            &PathBuf::from(&args[5]),
            &PathBuf::from(&args[6]),
        );
        return;
    }
    // `bench` mode: time our engine vs PDFium instead of grading correctness.
    //   pdfium-diff bench <libpdfium.so> <scale> <file.pdf> [per_page_n]
    if args.first().map(String::as_str) == Some("bench") {
        bench(&args[1..]);
        return;
    }
    // Feature-enabled structured attribution. This intentionally does not
    // load PDFium: its companion `bench`/diff modes retain the oracle timing
    // path, while this command isolates our own pipeline stages.
    if args.first().map(String::as_str) == Some("profile") {
        profile(&args[1..]);
        return;
    }
    if args.first().map(String::as_str) == Some("pipeline-profile") {
        pipeline_profile(&args[1..]);
        return;
    }
    // `--rerun-failures` mode: re-grade only the documents that a prior sweep
    // recorded as a drop of the named class, instead of the whole corpus. The
    // cheap inner loop for verifying a fix — read the prior CSV, keep rows in
    // the class, dedupe to distinct documents, run the normal diff on those.
    //   pdfium-diff --rerun-failures <prior.csv> <class> <libpdfium> <scale>
    //   class ∈ failed | blank | destroyed | all
    if args.first().map(String::as_str) == Some("--rerun-failures") {
        if args.len() < 5 {
            eprintln!(
                "usage: pdfium-diff --rerun-failures <prior.csv> <class> <libpdfium> <scale>"
            );
            eprintln!("       class ∈ failed | blank | destroyed | all");
            std::process::exit(2);
        }
        let prior = PathBuf::from(&args[1]);
        let class = args[2].clone();
        let lib = PathBuf::from(&args[3]);
        let scale: f64 = args[4].parse().expect("scale must be a number");
        let files = select_rerun_targets(&prior, &class);
        eprintln!(
            "rerun-failures[{class}]: {} distinct documents selected",
            files.len()
        );
        supervise_diff(&lib, scale, files, PathBuf::from("pdfium-diff-rerun-out"));
        return;
    }

    // `--count` mode: enumerate the PDFs the sweep would cover, per root, with
    // no PDFium load and no rendering. Use it to sanity-check corpus wiring
    // before paying the multi-hour sweep.
    if args.first().map(String::as_str) == Some("--count") {
        let mut grand = 0usize;
        for t in &args[1..] {
            let mut v = Vec::new();
            collect_pdfs(Path::new(t), 0, &mut v);
            v.sort();
            v.dedup();
            println!("{:>7}  {t}", v.len());
            grand += v.len();
        }
        println!("{:>7}  TOTAL", grand);
        return;
    }

    if args.len() < 3 {
        eprintln!("usage: pdfium-diff <libpdfium.so> <scale> <file.pdf|dir>…");
        eprintln!("       pdfium-diff --rerun-failures <prior.csv> <class> <libpdfium.so> <scale>");
        eprintln!("       pdfium-diff --count <file.pdf|dir>…   (enumerate, no render)");
        eprintln!("       pdfium-diff bench <libpdfium.so> <scale> <file.pdf> [per_page_n]");
        eprintln!("       pdfium-diff profile <scale> <file.pdf> [page] [runs] [out.jsonl]");
        eprintln!(
            "       pdfium-diff pipeline-profile <scale> <file.pdf> [runs] [out.jsonl] [compile-workers] [render-workers]"
        );
        std::process::exit(2);
    }
    let lib = PathBuf::from(&args[0]);
    let scale: f64 = args[1].parse().expect("scale must be a number");
    let targets: Vec<PathBuf> = args[2..].iter().map(PathBuf::from).collect();

    let mut files = Vec::new();
    for t in &targets {
        collect_pdfs(t, 0, &mut files);
    }
    files.sort();
    files.dedup();

    supervise_diff(&lib, scale, files, PathBuf::from("pdfium-diff-out"));
}

fn write_rgba_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())
}

/// Load PDFium or exit. SAFETY: the caller names the library.
fn load_pdfium(lib: &Path) -> pdfium::Pdfium {
    match unsafe { pdfium::Pdfium::load(lib) } {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot load PDFium: {e}");
            std::process::exit(1);
        }
    }
}

/// Supervise a corpus sweep with a pool of batched worker processes.
///
/// Design (Windows-hostile bottlenecks removed 2026-07-21):
/// - **Batched workers**: each worker process receives a *chunk* of files
///   via a manifest and iterates them in-process — one exe+pdfium.dll load
///   per chunk instead of per file (process spawn was 100–300 ms × 14k
///   files on Windows).
/// - **Concurrent pool**: `PDFIUM_DIFF_WORKERS` processes run at once
///   (default: half the logical cores). The old controller was serial.
/// - **Controller-owned CSV**: workers never read results.csv (the old
///   per-worker re-parse was O(n²) over the sweep). The controller loads
///   the done-set once, sends each worker only its not-done files plus the
///   already-recorded page list, and merges each worker's private fragment
///   into results.csv on completion — so rows and resume keys stay
///   byte-compatible with existing sweeps.
/// - **Watchdog preserved**: a worker that makes no per-file progress for
///   `PDFIUM_DIFF_TIMEOUT` seconds (default 180) is killed; the file named
///   by its progress marker gets the terminal `page=-1` row exactly as
///   before, and the rest of its chunk is redispatched. Native crashes are
///   likewise contained: completed rows are kept, the marker file is
///   excised, the remainder requeued.
fn supervise_diff(lib: &Path, scale: f64, files: Vec<PathBuf>, out_dir: PathBuf) {
    let _ = std::fs::create_dir_all(&out_dir);
    let csv_path = out_dir.join("results.csv");
    ensure_csv_header(&csv_path);
    let done = load_done(&csv_path);
    if !done.is_empty() {
        eprintln!("resuming: {} pages already recorded", done.len());
    }
    // Per-file recorded pages, so workers can skip them without reading the
    // CSV. Terminal (-1) rows exclude the whole file.
    let mut done_pages: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    for key in &done {
        if let Some((path, page)) = key.rsplit_once('|') {
            done_pages.entry(path).or_default().push(page);
        }
    }

    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cannot locate pdfium-diff worker executable: {error}");
            std::process::exit(1);
        }
    };
    let total = files.len();
    let mut skipped = 0usize;
    let mut crashed = 0usize;
    let mut timed_out = 0usize;

    // Per-FILE progress cap (not per chunk): the watchdog restarts whenever
    // the worker's marker advances to the next file. Override with
    // PDFIUM_DIFF_TIMEOUT=<secs>.
    let worker_timeout = std::env::var("PDFIUM_DIFF_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(180));
    let workers = std::env::var("PDFIUM_DIFF_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(4, |n| (n.get() / 2).max(1))
        });
    let chunk_size = std::env::var("PDFIUM_DIFF_CHUNK")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(32);

    // Not-done files, each carrying its already-recorded page list.
    let mut work: Vec<(PathBuf, String)> = Vec::new();
    for file in &files {
        let display = file.display().to_string();
        if done.contains(&format!("{display}|-1")) {
            skipped += 1;
            continue;
        }
        let skips = done_pages
            .get(display.as_str())
            .map(|pages| pages.join(";"))
            .unwrap_or_default();
        work.push((file.clone(), skips));
    }

    let chunk_dir = out_dir.join("chunks");
    let _ = std::fs::create_dir_all(&chunk_dir);
    let mut queue: std::collections::VecDeque<Vec<(PathBuf, String)>> =
        work.chunks(chunk_size).map(|c| c.to_vec()).collect();

    struct Running {
        child: std::process::Child,
        chunk: Vec<(PathBuf, String)>,
        manifest: PathBuf,
        fragment: PathBuf,
        marker: PathBuf,
        last_marker: String,
        last_progress: Instant,
    }

    let merge_fragment = |fragment: &Path| {
        if let Ok(text) = std::fs::read_to_string(fragment) {
            if !text.is_empty() {
                if let Ok(mut csv) = std::fs::OpenOptions::new().append(true).open(&csv_path) {
                    let _ = csv.write_all(text.as_bytes());
                    let _ = csv.flush();
                }
            }
        }
    };

    let mut seq = 0usize;
    let mut dispatched_files = 0usize;
    let mut running: Vec<Running> = Vec::new();
    let mut last_report = Instant::now();
    loop {
        // Fill the pool.
        while running.len() < workers {
            let Some(chunk) = queue.pop_front() else {
                break;
            };
            seq += 1;
            let manifest = chunk_dir.join(format!("chunk-{seq}.txt"));
            let fragment = chunk_dir.join(format!("chunk-{seq}.csv"));
            let marker = chunk_dir.join(format!("chunk-{seq}.cur"));
            let mut lines = String::new();
            for (file, skips) in &chunk {
                lines.push_str(&file.display().to_string());
                lines.push('\t');
                lines.push_str(skips);
                lines.push('\n');
            }
            let _ = std::fs::write(&manifest, lines);
            let _ = std::fs::remove_file(&fragment);
            let _ = std::fs::remove_file(&marker);
            match Command::new(&exe)
                .arg("--worker-chunk")
                .arg(lib)
                .arg(scale.to_string())
                .arg(&out_dir)
                .arg(&manifest)
                .arg(&fragment)
                .arg(&marker)
                .spawn()
            {
                Ok(child) => {
                    dispatched_files += chunk.len();
                    running.push(Running {
                        child,
                        chunk,
                        manifest,
                        fragment,
                        marker,
                        last_marker: String::new(),
                        last_progress: Instant::now(),
                    });
                }
                Err(error) => {
                    crashed += 1;
                    eprintln!("could not start chunk worker ({error}); terminal-failing its files");
                    for (file, _) in &chunk {
                        append_terminal_failure(
                            &csv_path,
                            file,
                            &format!("could not start worker: {error}"),
                        );
                    }
                }
            }
        }
        if running.is_empty() && queue.is_empty() {
            break;
        }
        if last_report.elapsed() > Duration::from_secs(10) {
            eprintln!(
                "[{dispatched_files}/{total}] {} workers busy; {} chunks queued; {crashed} failures; {timed_out} timeouts; {skipped} terminally skipped",
                running.len(),
                queue.len()
            );
            last_report = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));

        let mut index = 0;
        while index < running.len() {
            let slot = &mut running[index];
            let status = match slot.child.try_wait() {
                Ok(Some(status)) => Some(Ok(status)),
                Ok(None) => {
                    // Per-file watchdog: progress = the marker advancing.
                    let cur = std::fs::read_to_string(&slot.marker).unwrap_or_default();
                    if cur != slot.last_marker {
                        slot.last_marker = cur;
                        slot.last_progress = Instant::now();
                        None
                    } else if slot.last_progress.elapsed() >= worker_timeout {
                        let _ = slot.child.kill();
                        let _ = slot.child.wait();
                        Some(Err(format!(
                            "worker timed out after {}s (killed)",
                            worker_timeout.as_secs()
                        )))
                    } else {
                        None
                    }
                }
                Err(error) => {
                    let _ = slot.child.kill();
                    let _ = slot.child.wait();
                    Some(Err(format!("waiting on worker failed: {error}")))
                }
            };
            let Some(outcome) = status else {
                index += 1;
                continue;
            };
            let slot = running.swap_remove(index);
            // Completed rows are kept whatever happened to the process.
            merge_fragment(&slot.fragment);
            match outcome {
                Ok(status) if status.success() => {}
                bad => {
                    let reason = match bad {
                        Ok(status) => {
                            crashed += 1;
                            format!("worker exited unsuccessfully ({status})")
                        }
                        Err(reason) => {
                            timed_out += 1;
                            reason
                        }
                    };
                    // The marker names the in-flight file: terminal-row it,
                    // requeue everything after it (files before it completed
                    // and their rows were just merged).
                    let marker = std::fs::read_to_string(&slot.marker).unwrap_or_default();
                    let offender = slot
                        .chunk
                        .iter()
                        .position(|(f, _)| f.display().to_string() == marker)
                        .unwrap_or(0);
                    let (file, _) = &slot.chunk[offender];
                    append_terminal_failure(&csv_path, file, &reason);
                    eprintln!("  {reason}; excising {}", file.display());
                    dispatched_files -= slot.chunk.len() - offender;
                    dispatched_files += 1;
                    let rest: Vec<(PathBuf, String)> = slot.chunk[offender + 1..].to_vec();
                    if !rest.is_empty() {
                        queue.push_front(rest);
                    }
                }
            }
            let _ = std::fs::remove_file(&slot.manifest);
            let _ = std::fs::remove_file(&slot.fragment);
            let _ = std::fs::remove_file(&slot.marker);
        }
    }
    let _ = std::fs::remove_dir(&chunk_dir);
    eprintln!(
        "controller complete: {total} files; {crashed} worker failures; {timed_out} timeouts; {skipped} terminally skipped"
    );
}

/// The results.csv header. The trailing `flags=annot` token is a version
/// stamp: both engines now render annotation appearances (pdfium `FPDF_ANNOT`,
/// ours `with_annotations(true)`), so a CSV whose header lacks the stamp was
/// produced by the pre-annotation tool and must not be compared against.
/// `note` stays last for `--rerun-failures` parsing. The first six columns are
/// stable so historical rerun files remain readable.
const CSV_HEADER: &str = "file,page,ink_delta,gross,ours_ink,ref_ink,ours_continuous_ink,ref_continuous_ink,degraded,note,flags=annot+continuous-ink";

/// The installed-font provider, scanned once per worker process and shared by
/// every page. PDFium substitutes non-embedded fonts (notably CJK) against the
/// platform's system faces, so our side must too or the grade unfairly blanks
/// every non-embedded CJK page. Building it lazily keeps documents that never
/// need a system font from paying the directory scan.
fn shared_system_fonts() -> Arc<dyn pdf_font::SystemFontProvider> {
    use std::sync::OnceLock;
    static PROVIDER: OnceLock<Arc<dyn pdf_font::SystemFontProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(pdf_font::FolderFontProvider::system()))
        .clone()
}

fn ensure_csv_header(csv_path: &Path) {
    let needs_header = std::fs::metadata(csv_path).map_or(true, |m| m.len() == 0);
    if needs_header {
        let mut csv = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(csv_path)
            .expect("open results.csv");
        let _ = writeln!(csv, "{CSV_HEADER}");
    }
}

fn append_terminal_failure(csv_path: &Path, file: &Path, note: &str) {
    ensure_csv_header(csv_path);
    let mut csv = std::fs::OpenOptions::new()
        .append(true)
        .open(csv_path)
        .expect("open results.csv");
    let safe_note = note.replace(',', ";").replace('\n', " ").replace('\r', " ");
    let _ = writeln!(
        csv,
        "{},-1,1.0,1.0,0,1.0,0,1.0,0,{}",
        csv_escape(&file.display().to_string()),
        safe_note
    );
    let _ = csv.flush();
}

/// Parse a prior `results.csv`, keep rows whose drop-class matches, and return
/// the distinct, accessible document paths. Column layout is tolerated whether
/// or not continuous-ink and `degraded` columns are present: `note` is always
/// `ink_delta`/`ours_ink`/`ref_ink` sit at fixed offsets after the path.
fn select_rerun_targets(csv: &Path, class: &str) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(csv) else {
        eprintln!("cannot read prior CSV: {}", csv.display());
        std::process::exit(1);
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        // Split the (possibly quoted, comma-containing) path from the rest.
        let (path, rest) = match line.strip_prefix('"') {
            Some(s) => match s.find('"') {
                Some(end) => (s[..end].to_string(), &s[end + 2..]),
                None => continue,
            },
            None => match line.find(',') {
                Some(end) => (line[..end].to_string(), &line[end + 1..]),
                None => continue,
            },
        };
        let f: Vec<&str> = rest.split(',').collect();
        // rest = page, ink_delta, gross, ours_ink, ref_ink,
        //        [ours_continuous_ink, ref_continuous_ink,] [degraded,] note
        if f.len() < 6 {
            continue;
        }
        let ink_delta: f64 = f[1].parse().unwrap_or(0.0);
        let ours: f64 = f[3].parse().unwrap_or(0.0);
        let refi: f64 = f[4].parse().unwrap_or(0.0);
        let note = f.last().copied().unwrap_or("").trim();
        let is_failed =
            !note.is_empty() && !note.starts_with("silent-blank") && !note.starts_with("degraded");
        // Sweep-3 drop-class definitions (PLAN-SWEEP3 §1).
        let is_blank = ours < 0.1 && refi > 0.1;
        let is_destroyed = ink_delta > 0.3 && ours > 0.001 && refi > 0.001;
        let hit = match class {
            "failed" => is_failed,
            "blank" => is_blank,
            "destroyed" => is_destroyed,
            "all" => is_failed || is_blank || is_destroyed,
            other => {
                eprintln!("unknown class {other:?}; use failed|blank|destroyed|all");
                std::process::exit(2);
            }
        };
        if !hit {
            continue;
        }
        // Map the sweep's Linux corpus root to this box's drive if needed.
        let mapped = if Path::new(&path).is_file() {
            PathBuf::from(&path)
        } else if let Some(tail) = path.strip_prefix("/mnt/Samsung980_1TB") {
            PathBuf::from(format!("D:{tail}"))
        } else {
            PathBuf::from(&path)
        };
        if seen.insert(mapped.clone()) {
            if mapped.is_file() {
                out.push(mapped);
            } else {
                eprintln!("  (skipped, not found: {})", mapped.display());
            }
        }
    }
    out.sort();
    out
}

/// The shared per-document worker loop: render sampled pages with both engines,
/// grade, append to `<out_dir>/results.csv` (resumable), and print a ranked
/// summary. The controller starts this with exactly one document per process.
/// Compare every sampled page of one document, appending CSV rows to `csv`
/// (already-recorded pages are skipped via `skip`). Factored out of
/// [`run_diff`] so the supervisor's batched chunk workers share the exact
/// row-emission logic — keys and columns stay byte-compatible with resumes.
#[allow(clippy::too_many_arguments)]
fn diff_one_file(
    pdfium: &pdfium::Pdfium,
    scale: f64,
    file: &Path,
    skip: &dyn Fn(i32) -> bool,
    out_dir: &Path,
    dump: bool,
    csv: &mut std::fs::File,
    rows: &mut Vec<Row>,
    ok: &mut usize,
    skipped: &mut usize,
) {
    let n = match pdfium.page_count(file) {
        Ok(n) if n > 0 => n,
        Ok(_) => {
            *skipped += 1;
            let _ = writeln!(
                csv,
                "{},-1,1.0,1.0,0,1.0,0,1.0,0,pdfium reported zero pages",
                csv_escape(&file.display().to_string())
            );
            let _ = csv.flush();
            return;
        }
        Err(error) => {
            // The reference could not open it either. Keep a terminal row
            // so a resumed corpus does not repeatedly retry this document.
            *skipped += 1;
            let note = format!("pdfium open failed: {error}").replace(',', ";");
            let _ = writeln!(
                csv,
                "{},-1,1.0,1.0,0,1.0,0,1.0,0,{note}",
                csv_escape(&file.display().to_string())
            );
            let _ = csv.flush();
            return;
        }
    };
    let step = (n as usize / SAMPLE).max(1);
    for page in (0..n as usize).step_by(step).take(SAMPLE) {
        let page = page as i32;
        // Skip already-recorded pages BEFORE the reference render — a resume
        // must not pay a PDFium render per recorded row.
        if skip(page) {
            continue;
        }
        let theirs = match pdfium.render(file, page, scale) {
            Ok(b) => b,
            Err(error) => {
                let note = format!("pdfium render failed: {error}").replace(',', ";");
                let _ = writeln!(
                    csv,
                    "{},{page},1.0,1.0,0,1.0,0,1.0,0,{note}",
                    csv_escape(&file.display().to_string())
                );
                let _ = csv.flush();
                continue;
            }
        };
        match render_ours(file, page as u32, theirs.width, theirs.height, scale) {
            Ok((ours, degraded)) => {
                let d = compare(&ours, &theirs.bgra, theirs.width, theirs.height);
                // A dropped codec draw means we blanked content PDFium
                // painted. If nothing else painted either, it is a *silent
                // blank* — the worst kind of miss because ink_delta alone
                // could still look mild. Flag it explicitly (Workstream B3).
                let note: &'static str = if degraded > 0 {
                    if d.ours_ink < 0.001 {
                        "silent-blank(codec)"
                    } else {
                        "degraded(codec)"
                    }
                } else {
                    ""
                };
                if dump && (d.is_suspect() || degraded > 0) {
                    let name = format!(
                        "{}-p{page}.ppm",
                        file.file_stem().unwrap_or_default().to_string_lossy()
                    );
                    let name: String = name
                        .chars()
                        .map(|c| {
                            if c.is_alphanumeric() || c == '-' || c == '.' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect();
                    let _ = write_triptych(
                        &out_dir.join(name),
                        &ours,
                        &theirs.bgra,
                        theirs.width,
                        theirs.height,
                    );
                }
                let _ = writeln!(
                    csv,
                    "{},{page},{:.5},{:.5},{:.5},{:.5},{:.5},{:.5},{degraded},{note}",
                    csv_escape(&file.display().to_string()),
                    d.ink_delta(),
                    d.gross_frac,
                    d.ours_ink,
                    d.ref_ink,
                    d.ours_continuous_ink,
                    d.ref_continuous_ink
                );
                let _ = csv.flush();
                rows.push(Row {
                    file: file
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                    page,
                    diff: d,
                    degraded,
                    note,
                });
                *ok += 1;
            }
            Err(note) => {
                let _ = writeln!(
                    csv,
                    "{},{page},1.0,1.0,0,1.0,0,1.0,0,{note}",
                    csv_escape(&file.display().to_string())
                );
                let _ = csv.flush();
                // We failed where PDFium succeeded: the worst outcome, so
                // surface it rather than dropping it.
                rows.push(Row {
                    file: file
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                    page,
                    diff: Diff {
                        ref_ink: 1.0,
                        ..Default::default()
                    },
                    degraded: 0,
                    note,
                });
            }
        }
    }
}

/// One chunk-worker run: iterate the manifest's files in-process, appending
/// rows to `fragment` (merged into results.csv by the controller on exit)
/// and writing the file about to be processed into `marker` so a hung file
/// can be identified and excised by the watchdog.
///
/// Manifest line format: `<path>\t<page;page;…>` — the pages already
/// recorded for that file (the controller owns the done-set; workers never
/// read results.csv).
fn run_worker_chunk(
    lib: &Path,
    scale: f64,
    out_dir: PathBuf,
    manifest: &Path,
    fragment: &Path,
    marker: &Path,
) {
    let pdfium = load_pdfium(lib);
    let dump = dump_enabled();
    let text = match std::fs::read_to_string(manifest) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read chunk manifest {}: {e}", manifest.display());
            std::process::exit(1);
        }
    };
    let mut csv = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(fragment)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot open chunk fragment {}: {e}", fragment.display());
            std::process::exit(1);
        }
    };
    let mut rows: Vec<Row> = Vec::new();
    let (mut ok, mut skipped) = (0usize, 0usize);
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let (path, skips) = line.split_once('\t').unwrap_or((line, ""));
        let skip_pages: HashSet<i32> = skips.split(';').filter_map(|s| s.parse().ok()).collect();
        // Progress marker BEFORE the file: on a hang/crash the controller
        // reads this to know which file to excise from the redispatch.
        let _ = std::fs::write(marker, path);
        let file = PathBuf::from(path);
        let skip = |page: i32| skip_pages.contains(&page);
        diff_one_file(
            &pdfium,
            scale,
            &file,
            &skip,
            &out_dir,
            dump,
            &mut csv,
            &mut rows,
            &mut ok,
            &mut skipped,
        );
    }
}

fn run_diff(
    pdfium: &pdfium::Pdfium,
    scale: f64,
    files: Vec<PathBuf>,
    out_dir: PathBuf,
    dump: bool,
) {
    let _ = std::fs::create_dir_all(&out_dir);

    // Results are appended as they are produced. A worker may be terminated by
    // malformed native input, but its controller owns the process boundary and
    // records that failure before continuing with the next document.
    let csv_path = out_dir.join("results.csv");
    let done = load_done(&csv_path);
    if !done.is_empty() {
        eprintln!("resuming: {} pages already recorded", done.len());
    }
    let needs_header = std::fs::metadata(&csv_path).map_or(true, |m| m.len() == 0);
    let mut csv = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&csv_path)
        .expect("open results.csv");
    if needs_header {
        let _ = writeln!(csv, "{CSV_HEADER}");
    }

    let mut rows: Vec<Row> = Vec::new();
    let (mut ok, mut skipped) = (0usize, 0usize);
    let total = files.len();

    for (fi, file) in files.iter().enumerate() {
        if fi % 25 == 0 {
            eprintln!(
                "[{fi}/{total}] {} suspect so far",
                rows.iter().filter(|r| r.diff.is_suspect()).count()
            );
        }
        let display = file.display().to_string();
        let skip = |page: i32| done.contains(&format!("{display}|{page}"));
        diff_one_file(
            pdfium,
            scale,
            file,
            &skip,
            &out_dir,
            dump,
            &mut csv,
            &mut rows,
            &mut ok,
            &mut skipped,
        );
    }

    rows.sort_by(|a, b| b.diff.severity().partial_cmp(&a.diff.severity()).unwrap());
    let suspect = rows
        .iter()
        .filter(|r| r.diff.is_suspect() || !r.note.is_empty())
        .count();
    // B3 drop counters: a page whose only content was an undecodable image
    // renders blank. `silent_blanks` are those where we painted nothing at all
    // (a true dropped page); `degraded_pages` also includes pages that painted
    // *something* but still lost a codec draw.
    let silent_blanks = rows
        .iter()
        .filter(|r| r.degraded > 0 && r.diff.ours_ink < 0.001)
        .count();
    let degraded_pages = rows.iter().filter(|r| r.degraded > 0).count();

    println!(
        "compared {ok} pages from {} files ({skipped} unopenable)",
        files.len()
    );
    println!("suspect: {suspect}   silent-blanks: {silent_blanks}   degraded: {degraded_pages}\n");
    println!(
        "{:<38} {:>4} {:>8} {:>8} {:>8} {:>8} {:>8}  {}",
        "file", "page", "inkΔ", "contΔ", "gross", "ours-ink", "ref-ink", "note"
    );
    for r in rows.iter().take(40) {
        if !r.diff.is_suspect() && r.note.is_empty() {
            continue;
        }
        let f: String = r.file.chars().take(36).collect();
        println!(
            "{:<38} {:>4} {:>8.4} {:>8.4} {:>8.4} {:>8.4} {:>8.4}  {}",
            f,
            r.page,
            r.diff.ink_delta(),
            r.diff.continuous_ink_delta(),
            r.diff.gross_frac,
            r.diff.ours_ink,
            r.diff.ref_ink,
            r.note
        );
    }
    if suspect > 0 {
        println!(
            "\ntriptychs (ours | pdfium | diff) in {}/",
            out_dir.display()
        );
    }
}

/// Timing harness: our engine vs PDFium, per-page then whole-document.
///
/// Per-page is single-threaded on both sides (open once, render each page,
/// time each) — the honest raw-speed comparison. Whole-document renders every
/// page: PDFium sequentially (its library is single-threaded), ours through
/// the parallel `RenderScheduler`, which is how the engine is actually driven —
/// so this is where our compile/render pipeline can make up ground.
fn bench(args: &[String]) {
    if args.len() < 3 {
        eprintln!("usage: pdfium-diff bench <libpdfium.so> <scale> <file.pdf> [per_page_n]");
        std::process::exit(2);
    }
    let lib = PathBuf::from(&args[0]);
    let scale: f64 = args[1].parse().expect("scale must be a number");
    let file = PathBuf::from(&args[2]);
    let per_page_n: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);

    // SAFETY: the caller names the library.
    let pdfium = match unsafe { pdfium::Pdfium::load(&lib) } {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot load PDFium: {e}");
            std::process::exit(1);
        }
    };

    // Open once on our side and time it (PDFium's open is timed inside
    // render_doc_timed).
    let bytes = std::fs::read(&file).expect("read file");
    let t_open = Instant::now();
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snap = DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed");
    let our_open = t_open.elapsed();
    let count = snap.page_count();
    let pcount = pdfium.page_count(&file).expect("pdfium page count").max(0) as u32;

    println!("file: {}", file.display());
    println!("pages: ours={count} pdfium={pcount}   scale={scale}");
    println!("open (ours): {:.1} ms\n", ms(our_open));

    // ---- Per-page (single-threaded both sides) --------------------------
    let n = per_page_n.min(count).min(pcount);
    let indices: Vec<i32> = (0..n as i32).collect();
    let (pdf_open, pdf_pp) = pdfium
        .render_doc_timed(&file, &indices, scale)
        .expect("pdfium render");
    let mut ours_pp: Vec<Duration> = Vec::with_capacity(n as usize);
    for p in 0..n {
        let mut ctx = ParseContext::new();
        let t = Instant::now();
        if let Ok(req) = build_request(&snap, PageIndex(p), scale, &mut ctx) {
            let _ = CpuBackend::default().render_to_host(&req);
        }
        ours_pp.push(t.elapsed());
    }

    println!("── per-page render (single-threaded) ──");
    println!(
        "{:>4}  {:>10}  {:>10}  {:>7}",
        "page", "ours(ms)", "pdfium(ms)", "ratio"
    );
    for i in 0..n as usize {
        let o = ms(ours_pp[i]);
        let p = ms(pdf_pp[i]);
        println!(
            "{:>4}  {:>10.1}  {:>10.1}  {:>6.1}x",
            i,
            o,
            p,
            safe_ratio(o, p)
        );
    }
    let ours_mean = mean_ms(&ours_pp);
    let pdf_mean = mean_ms(&pdf_pp);
    println!(
        "mean  {:>10.1}  {:>10.1}  {:>6.1}x   (pdfium open {:.1} ms)\n",
        ours_mean,
        pdf_mean,
        safe_ratio(ours_mean, pdf_mean),
        ms(pdf_open)
    );

    // ---- Whole document -------------------------------------------------
    // PDFium: sequential over every page.
    let all: Vec<i32> = (0..pcount as i32).collect();
    let (_pdf_open2, pdf_all) = pdfium
        .render_doc_timed(&file, &all, scale)
        .expect("pdfium full render");
    let pdf_total: Duration = pdf_all.iter().sum();

    // Ours: the parallel scheduler over every page.
    let backend: Arc<dyn RenderBackend> = Arc::new(CpuBackend::default());
    let opts = SchedulerOptions::default();
    let (cw, rw) = (opts.compile_workers, opts.render_workers);
    let scheduler = RenderScheduler::new(backend, opts);
    let make =
        move |snap: &DocumentSnapshot, page: PageIndex| -> Result<RenderRequest, RenderError> {
            let mut ctx = ParseContext::new();
            build_request(snap, page, scale, &mut ctx)
        };
    let mut ok = 0usize;
    let t = Instant::now();
    scheduler.render_range(&snap, 0..count, &make, None, &mut |out: PipelineOutput| {
        if out.result.is_ok() {
            ok += 1;
        }
    });
    let our_total = t.elapsed();

    println!("── whole document ──");
    println!(
        "ours (parallel, {cw} compile + {rw} render workers): {:.2} s   {ok}/{count} pages rendered   {:.1} pages/s",
        our_total.as_secs_f64(),
        ok as f64 / our_total.as_secs_f64().max(1e-9),
    );
    println!(
        "pdfium (sequential):                                 {:.2} s   {pcount}/{pcount} pages rendered   {:.1} pages/s",
        pdf_total.as_secs_f64(),
        pcount as f64 / pdf_total.as_secs_f64().max(1e-9),
    );
    if ok == 0 {
        println!(
            "\n!! ours rendered 0 pages (all compiles/renders failed) — timing is meaningless."
        );
        return;
    }
    if (ok as u32) < count {
        println!(
            "\n!! ours skipped {} page(s) that failed to compile/render — throughput below is over rendered pages only.",
            count - ok as u32
        );
    }
    // Normalise both sides to per-page throughput so a page-count mismatch does
    // not flatter either engine.
    let ours_pps = ok as f64 / our_total.as_secs_f64().max(1e-9);
    let pdf_pps = pcount as f64 / pdf_total.as_secs_f64().max(1e-9);
    let speedup = ours_pps / pdf_pps.max(1e-9);
    if speedup >= 1.0 {
        println!("\n=> ours is {speedup:.2}x FASTER than PDFium on whole-document throughput.");
    } else {
        println!(
            "\n=> ours is {:.2}x slower than PDFium on whole-document throughput.",
            1.0 / speedup
        );
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
fn mean_ms(v: &[Duration]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().map(|d| ms(*d)).sum::<f64>() / v.len() as f64
}
fn safe_ratio(ours: f64, pdfium: f64) -> f64 {
    if pdfium <= 0.0 {
        0.0
    } else {
        ours / pdfium
    }
}

/// Structured whole-document scheduler measurement. The snapshot is retained
/// between runs; each run compiles and renders the full page range.
fn pipeline_profile(args: &[String]) {
    if args.len() < 2 {
        eprintln!(
            "usage: pdfium-diff pipeline-profile <scale> <file.pdf> [runs] [out.jsonl] [compile-workers] [render-workers]"
        );
        std::process::exit(2);
    }
    let scale: f64 = args[0].parse().expect("scale must be a number");
    let path = PathBuf::from(&args[1]);
    let runs = args
        .get(2)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3)
        .max(1);
    let out = args
        .get(3)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("pipeline-profile.jsonl"));
    let source: Arc<dyn PdfSource> = Arc::new(MmapSource::open(&path).expect("map input"));
    let (snapshot, open) =
        DocumentSnapshot::open_profiled(source, DocumentLimits::default()).expect("open input");
    let count = snapshot.page_count();
    let mut options = SchedulerOptions::default();
    if let Some(value) = args.get(4) {
        options.compile_workers = value.parse().expect("compile workers must be an integer");
    }
    if let Some(value) = args.get(5) {
        options.render_workers = value.parse().expect("render workers must be an integer");
    }
    let mut writer =
        std::io::BufWriter::new(std::fs::File::create(&out).expect("create pipeline output"));

    for _ in 0..runs {
        let backend: Arc<dyn RenderBackend> = Arc::new(CpuBackend::default());
        let scheduler = RenderScheduler::new(backend, options.clone());
        let make =
            move |snap: &DocumentSnapshot, page: PageIndex| -> Result<RenderRequest, RenderError> {
                let mut ctx = ParseContext::new();
                build_request(snap, page, scale, &mut ctx)
            };
        let benchmark_start = Instant::now();
        let mut report =
            scheduler.render_range_profiled(&snapshot, 0..count, &make, None, &mut |_out| {});
        report.add_duration("benchmark.total", benchmark_start.elapsed());
        report.merge(&open);
        write_pipeline_row(&mut writer, &path, &report).expect("write pipeline profile row");
    }
    writer.flush().expect("flush pipeline output");
    eprintln!("wrote structured pipeline profiles to {}", out.display());
}

fn write_pipeline_row(
    writer: &mut impl Write,
    path: &Path,
    report: &pdf_profiling::ProfileReport,
) -> std::io::Result<()> {
    let mut json = format!(
        "{{\"mode\":\"whole_document_pipeline\",\"file\":\"{}\",",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\""),
    );
    report.write_json_fields(&mut json);
    if let Some((rss, peak)) = linux_rss_bytes() {
        use std::fmt::Write as _;
        let _ = write!(
            json,
            ",\"process_rss_bytes\":{rss},\"process_peak_rss_bytes\":{peak}"
        );
    }
    json.push_str("}\n");
    writer.write_all(json.as_bytes())
}

/// Run the renderer's structured profile matrix for one representative page.
/// Output is JSON Lines so a long corpus run remains useful if interrupted.
fn profile(args: &[String]) {
    if args.len() < 2 {
        eprintln!(
            "usage: pdfium-diff profile <scale> <file.pdf> [page] [runs] [out.jsonl] [--mode MODE]"
        );
        std::process::exit(2);
    }
    let scale: f64 = args[0].parse().expect("scale must be a number");
    let path = PathBuf::from(&args[1]);
    let (positional, selected_mode) = profile_args(&args[2..]);
    let page = positional.first().and_then(|v| v.parse().ok()).unwrap_or(0);
    let runs = positional
        .get(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(5usize)
        .max(1);
    let out = positional
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("profile.jsonl"));
    let pdfium_reference = std::env::var_os("PDF_RENDERER_PDFIUM")
        .map(PathBuf::from)
        .map(|lib| {
            let oracle = load_pdfium(&lib);
            oracle
                .render(&path, page as i32, scale)
                .expect("render retained PDFium reference")
        });
    let mut writer =
        std::io::BufWriter::new(std::fs::File::create(&out).expect("create profile output"));
    #[cfg(feature = "dhat-heap")]
    let _heap_profiler = std::env::var_os("PDF_RENDERER_DHAT_OUT").map(|path| {
        dhat::Profiler::builder()
            .file_name(PathBuf::from(path))
            .build()
    });
    #[cfg(not(feature = "dhat-heap"))]
    if std::env::var_os("PDF_RENDERER_DHAT_OUT").is_some() {
        eprintln!("PDF_RENDERER_DHAT_OUT requires building pdfium-diff with --features dhat-heap");
        std::process::exit(2);
    }
    let runs_mode = |mode| selected_mode.is_none_or(|selected| selected == mode);

    // Cold: open, compile, and render anew for every sample.
    if runs_mode(ProfileMode::Cold) {
        for _ in 0..runs {
            let benchmark_start = Instant::now();
            let source: Arc<dyn PdfSource> = Arc::new(MmapSource::open(&path).expect("map input"));
            let (snapshot, mut report) =
                DocumentSnapshot::open_profiled(source, DocumentLimits::default())
                    .expect("open input");
            let mut ctx = ParseContext::new();
            let (compiled, compile) = pdf_content::PageCompiler::new()
                .with_annotations(true)
                .with_system_fonts(shared_system_fonts())
                .compile_profiled(&snapshot, PageIndex(page), &mut ctx)
                .expect("compile page");
            report.merge(&compile);
            let request = request_for_compiled(Arc::new(compiled), scale);
            let mut worker = pdf_render_cpu::CpuWorkerContext::new();
            let (host, _, render) = CpuBackend::default()
                .render_profiled_with(&request, &mut worker)
                .expect("render page");
            report.merge(&render);
            report.add_duration("benchmark.total", benchmark_start.elapsed());
            write_profile_row(
                &mut writer,
                ProfileMode::Cold.name(),
                &path,
                page,
                &host,
                &report,
                pdfium_reference.as_ref(),
            )
            .expect("write profile row");
        }
    }

    // Warm document recompiles against one snapshot; compiled-page render
    // skips compilation; prepared execution also skips CPU lowering/decode.
    if runs_mode(ProfileMode::Warm)
        || runs_mode(ProfileMode::Compiled)
        || runs_mode(ProfileMode::WarmDecoded)
        || runs_mode(ProfileMode::DecodeOnly)
        || runs_mode(ProfileMode::Prepared)
    {
        let source: Arc<dyn PdfSource> = Arc::new(MmapSource::open(&path).expect("map input"));
        let (snapshot, open) =
            DocumentSnapshot::open_profiled(source, DocumentLimits::default()).expect("open input");
        let compiler = pdf_content::PageCompiler::new()
            .with_annotations(true)
            .with_system_fonts(shared_system_fonts());
        let backend = CpuBackend::default();
        let mut worker = pdf_render_cpu::CpuWorkerContext::new();
        let mut ctx = ParseContext::new();
        let (compiled, compile) = compiler
            .compile_profiled(&snapshot, PageIndex(page), &mut ctx)
            .expect("compile page");
        let request = request_for_compiled(Arc::new(compiled), scale);

        for mode in [ProfileMode::Warm, ProfileMode::Compiled] {
            if !runs_mode(mode) {
                continue;
            }
            for _ in 0..runs {
                let benchmark_start = Instant::now();
                let mut report = if mode == ProfileMode::Warm {
                    open.clone()
                } else {
                    pdf_profiling::ProfileReport::new()
                };
                let request = if mode == ProfileMode::Warm {
                    let mut warm_ctx = ParseContext::new();
                    let (compiled_page, cp) = compiler
                        .compile_profiled(&snapshot, PageIndex(page), &mut warm_ctx)
                        .expect("compile page");
                    report.merge(&cp);
                    request_for_compiled(Arc::new(compiled_page), scale)
                } else {
                    request.clone()
                };
                let (host, _, render) = backend
                    .render_profiled_with(&request, &mut worker)
                    .expect("render page");
                report.merge(&render);
                report.add_duration("benchmark.total", benchmark_start.elapsed());
                write_profile_row(
                    &mut writer,
                    mode.name(),
                    &path,
                    page,
                    &host,
                    &report,
                    pdfium_reference.as_ref(),
                )
                .expect("write profile row");
            }
        }

        if runs_mode(ProfileMode::WarmDecoded) {
            let cache = pdf_render_cpu::DecodedImageCache::default();
            // Populate decoded residency outside the measured samples.
            let _ = backend
                .prepare_with_decode_cache_profiled(&request, cache.clone())
                .expect("warm decoded image cache");
            for _ in 0..runs {
                let benchmark_start = Instant::now();
                let (prepared, mut report) = backend
                    .prepare_with_decode_cache_profiled(&request, cache.clone())
                    .expect("prepare page with warm decode cache");
                let (host, _, execute) = backend
                    .execute_prepared_profiled(&request, &prepared, &mut worker)
                    .expect("execute warm-decoded page");
                report.merge(&execute);
                report.add_duration("benchmark.total", benchmark_start.elapsed());
                report.merge(&compile);
                write_profile_row(
                    &mut writer,
                    ProfileMode::WarmDecoded.name(),
                    &path,
                    page,
                    &host,
                    &report,
                    pdfium_reference.as_ref(),
                )
                .expect("write profile row");
            }
        }

        if runs_mode(ProfileMode::DecodeOnly) {
            // Preserve the normal row schema and output identity without
            // including this reference render in decode-only timing.
            let (host, _) = backend
                .render_to_host(&request)
                .expect("render reference page");
            for _ in 0..runs {
                let benchmark_start = Instant::now();
                let mut report = backend
                    .decode_page_profiled(&request)
                    .expect("decode page payloads");
                report.add_duration("benchmark.total", benchmark_start.elapsed());
                report.merge(&compile);
                write_profile_row(
                    &mut writer,
                    ProfileMode::DecodeOnly.name(),
                    &path,
                    page,
                    &host,
                    &report,
                    pdfium_reference.as_ref(),
                )
                .expect("write profile row");
            }
        }

        if runs_mode(ProfileMode::Prepared) {
            let (prepared, prepare) = backend.prepare_profiled(&request).expect("prepare page");
            for _ in 0..runs {
                let benchmark_start = Instant::now();
                let (host, _, mut report) = backend
                    .execute_prepared_profiled(&request, &prepared, &mut worker)
                    .expect("execute prepared page");
                report.add_duration("benchmark.total", benchmark_start.elapsed());
                report.merge(&prepare);
                report.merge(&compile);
                write_profile_row(
                    &mut writer,
                    ProfileMode::Prepared.name(),
                    &path,
                    page,
                    &host,
                    &report,
                    pdfium_reference.as_ref(),
                )
                .expect("write profile row");
            }
        }
    }
    writer.flush().expect("flush profile output");
    eprintln!("wrote structured profiles to {}", out.display());
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProfileMode {
    Cold,
    Warm,
    Compiled,
    WarmDecoded,
    DecodeOnly,
    Prepared,
}

impl ProfileMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "cold" | "cold_end_to_end" => Some(Self::Cold),
            "warm" | "warm_document" => Some(Self::Warm),
            "compiled" | "compiled_page_render" => Some(Self::Compiled),
            "warm-decoded" | "warm_decoded_render" => Some(Self::WarmDecoded),
            "decode-only" | "decode_only" => Some(Self::DecodeOnly),
            "prepared" | "prepared_page_execution" => Some(Self::Prepared),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Cold => "cold_end_to_end",
            Self::Warm => "warm_document",
            Self::Compiled => "compiled_page_render",
            Self::WarmDecoded => "warm_decoded_render",
            Self::DecodeOnly => "decode_only",
            Self::Prepared => "prepared_page_execution",
        }
    }
}

fn profile_args(args: &[String]) -> (Vec<&String>, Option<ProfileMode>) {
    let mut positional = Vec::new();
    let mut mode = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--mode" {
            let Some(value) = args.get(i + 1) else {
                eprintln!(
                    "--mode requires cold, warm, compiled, warm-decoded, decode-only, or prepared"
                );
                std::process::exit(2);
            };
            mode = Some(ProfileMode::parse(value).unwrap_or_else(|| {
                eprintln!(
                    "unknown profile mode {value:?}; use cold, warm, compiled, warm-decoded, decode-only, or prepared"
                );
                std::process::exit(2);
            }));
            i += 2;
        } else if args[i].starts_with('-') {
            eprintln!("unknown profile option {:?}", args[i]);
            std::process::exit(2);
        } else {
            positional.push(&args[i]);
            i += 1;
        }
    }
    (positional, mode)
}

fn request_for_compiled(compiled: Arc<pdf_page_ir::CompiledPage>, scale: f64) -> RenderRequest {
    let crop = compiled.bounds.crop;
    let (cw, ch) = ((crop.x1 - crop.x0) * scale, (crop.y1 - crop.y0) * scale);
    let (dw, dh) = match compiled.bounds.rotate {
        90 | 270 => (ch, cw),
        _ => (cw, ch),
    };
    RenderRequest {
        transform: PageTransform {
            matrix: display_matrix(&compiled.bounds, scale),
        },
        page: compiled,
        crop: None,
        output_size: DeviceSize {
            width: dw.ceil().max(1.0) as u32,
            height: dh.ceil().max(1.0) as u32,
        },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        // Vestigial: the CPU backend does not read this field — annotation
        // appearances are baked into the display list at compile time via
        // `PageCompiler::with_annotations(true)` (all grading paths do). Kept
        // in sync with that intent (and with pdfium's `FPDF_ANNOT`) so it can't
        // be misread as "annotations off" for this sweep.
        annotations: AnnotationMode::StaticAppearances,
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    }
}

fn write_profile_row(
    writer: &mut impl Write,
    mode: &str,
    path: &Path,
    page: u32,
    host: &pdf_render_api::HostPage,
    report: &pdf_profiling::ProfileReport,
    reference: Option<&pdfium::Bitmap>,
) -> std::io::Result<()> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    host.pixels.hash(&mut hasher);
    let mut json = format!(
        "{{\"mode\":\"{mode}\",\"file\":\"{}\",\"page\":{page},\"width\":{},\"height\":{},\"output_hash\":\"{:016x}\",",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\""),
        host.width,
        host.height,
        hasher.finish()
    );
    report.write_json_fields(&mut json);
    if let Some((rss, peak)) = linux_rss_bytes() {
        use std::fmt::Write as _;
        let _ = write!(
            json,
            ",\"process_rss_bytes\":{rss},\"process_peak_rss_bytes\":{peak}"
        );
    }
    if let Some(reference) = reference {
        use std::fmt::Write as _;
        let _ = write!(
            json,
            ",\"pdfium_width\":{},\"pdfium_height\":{}",
            reference.width, reference.height
        );
        if reference.width == host.width && reference.height == host.height {
            let diff = compare(
                &host.pixels,
                &reference.bgra,
                reference.width,
                reference.height,
            );
            let _ = write!(
                json,
                ",\"pdfium_mean_abs\":{},\"pdfium_gross_frac\":{},\"pdfium_ours_ink\":{},\"pdfium_ref_ink\":{},\"pdfium_ours_continuous_ink\":{},\"pdfium_ref_continuous_ink\":{},\"pdfium_ink_delta\":{},\"pdfium_continuous_ink_delta\":{},\"pdfium_severity\":{}",
                diff.mean_abs,
                diff.gross_frac,
                diff.ours_ink,
                diff.ref_ink,
                diff.ours_continuous_ink,
                diff.ref_continuous_ink,
                diff.ink_delta(),
                diff.continuous_ink_delta(),
                diff.severity(),
            );
        } else {
            json.push_str(",\"pdfium_dimension_match\":false");
        }
    }
    json.push_str("}\n");
    writer.write_all(json.as_bytes())
}

/// Linux current/high-water resident set. Each profile mode runs in its own
/// process in the corpus driver, so `VmHWM` is a useful mode-level peak.
fn linux_rss_bytes() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = |name: &str| {
        status
            .lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_ascii_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
    };
    Some((kib("VmRSS:")? * 1024, kib("VmHWM:")? * 1024))
}

/// Compile page `p` and build a render request at PDFium's page grid
/// (page size × scale, y-flipped, cropbox origin) — shared by the per-page and
/// whole-document timing paths.
fn build_request(
    snap: &DocumentSnapshot,
    page: PageIndex,
    scale: f64,
    ctx: &mut ParseContext,
) -> Result<RenderRequest, RenderError> {
    let compiled = pdf_content::PageCompiler::new()
        .with_annotations(true)
        .with_system_fonts(shared_system_fonts())
        .compile(snap, page, ctx)
        .map_err(|_| RenderError::Backend("compile failed".into()))?;
    Ok(request_for_compiled(Arc::new(compiled), scale))
}

/// Collect `.pdf` files recursively. Bounded depth so a symlink loop cannot
/// spin forever.
fn collect_pdfs(root: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    const MAX_DEPTH: u32 = 12;
    if depth > MAX_DEPTH {
        return;
    }
    if root.is_file() {
        if root
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        {
            out.push(root.to_path_buf());
        }
        return;
    }
    // Skip dependency/VCS trees — the pdf.js corpus carries a large
    // `node_modules` with no test PDFs; traversing it wastes minutes.
    if root
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| matches!(n, "node_modules" | ".git" | ".svn" | "target" | ".hg"))
    {
        return;
    }
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        collect_pdfs(&entry.path(), depth + 1, out);
    }
}

/// `file|page` keys already recorded, so a rerun resumes instead of redoing
/// hours of work.
fn load_done(csv: &Path) -> HashSet<String> {
    let Ok(text) = std::fs::read_to_string(csv) else {
        return HashSet::new();
    };
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let (path, rest) = match l.strip_prefix('"') {
                Some(stripped) => {
                    let end = stripped.find('"')?;
                    (stripped[..end].to_string(), stripped.get(end + 2..)?)
                }
                None => {
                    let end = l.find(',')?;
                    (l[..end].to_string(), l.get(end + 1..)?)
                }
            };
            Some(format!("{path}|{}", rest.split(',').next()?))
        })
        .collect()
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') {
        format!("\"{}\"", s.replace('"', "'"))
    } else {
        s.to_string()
    }
}

/// The crop-box → device matrix PDFium's display path uses: `scale`, y-flip,
/// origin at the crop corner, then the page `/Rotate` (clockwise quarter
/// turns, per `CPDF_Page::GetDisplayMatrix`). For 90/270 the device extent is
/// the crop's height×width; callers that derive their own output size must
/// swap accordingly (the diff path takes PDFium's already-rotated w/h).
fn display_matrix(bounds: &pdf_page_ir::PageBounds, scale: f64) -> Matrix {
    let c = bounds.crop;
    let s = scale;
    match bounds.rotate {
        // x' = (y-y0)s, y' = (x-x0)s — top edge lands on the right (cw).
        90 => Matrix {
            a: 0.0,
            b: s,
            c: s,
            d: 0.0,
            e: -c.y0 * s,
            f: -c.x0 * s,
        },
        // x' = (x1-x)s, y' = (y-y0)s.
        180 => Matrix {
            a: -s,
            b: 0.0,
            c: 0.0,
            d: s,
            e: c.x1 * s,
            f: -c.y0 * s,
        },
        // x' = (y1-y)s, y' = (x1-x)s — top edge lands on the left (ccw).
        270 => Matrix {
            a: 0.0,
            b: -s,
            c: -s,
            d: 0.0,
            e: c.y1 * s,
            f: c.x1 * s,
        },
        // 0 (and anything unnormalized): the plain y-flip.
        _ => Matrix {
            a: s,
            b: 0.0,
            c: 0.0,
            d: -s,
            e: -c.x0 * s,
            f: c.y1 * s,
        },
    }
}

/// Render with our engine at exactly PDFium's pixel grid.
///
/// Wrapped in `catch_unwind`: across a corpus this size our own panics are an
/// expected *finding*, not a reason to lose the run. A page we panic on is
/// reported like any other failure.
fn render_ours(
    path: &Path,
    page: u32,
    w: u32,
    h: u32,
    scale: f64,
) -> Result<(Vec<u8>, u32), &'static str> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        render_ours_inner(path, page, w, h, scale)
    }))
    .unwrap_or(Err("PANIC"))
}

fn render_ours_inner(
    path: &Path,
    page: u32,
    w: u32,
    h: u32,
    scale: f64,
) -> Result<(Vec<u8>, u32), &'static str> {
    let bytes = std::fs::read(path).map_err(|_| "unreadable")?;
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snap =
        DocumentSnapshot::open(source, DocumentLimits::default()).map_err(|_| "open failed")?;
    let mut ctx = ParseContext::new();
    // StaticAppearances, matching the FPDF_ANNOT flag on the pdfium side.
    let compiled = pdf_content::PageCompiler::new()
        .with_annotations(true)
        .with_system_fonts(shared_system_fonts())
        .compile(&snap, PageIndex(page), &mut ctx)
        .map_err(|_| "compile failed")?;

    // PDFium renders the crop box at `scale`, y-flipped, origin at its corner,
    // **with the page's /Rotate baked in** (its page dims are post-rotation).
    // Mirror that or every /Rotate 90 page compares 90° off (L3-E).
    let matrix = display_matrix(&compiled.bounds, scale);
    let req = RenderRequest {
        page: Arc::new(compiled),
        transform: PageTransform { matrix },
        crop: None,
        output_size: DeviceSize {
            width: w,
            height: h,
        },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        // Vestigial: the CPU backend does not read this field — annotation
        // appearances are baked into the display list at compile time via
        // `PageCompiler::with_annotations(true)` (all grading paths do). Kept
        // in sync with that intent (and with pdfium's `FPDF_ANNOT`) so it can't
        // be misread as "annotations off" for this sweep.
        annotations: AnnotationMode::StaticAppearances,
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };
    let (host, stats) = CpuBackend::default()
        .render_to_host(&req)
        .map_err(|_| "render failed")?;
    // `degraded_draws` counts image draws dropped because their codec could not
    // decode (Workstream B3). Carried out so a page we blanked can be flagged
    // rather than scored clean.
    Ok((host.pixels.to_vec(), stats.degraded_draws))
}
