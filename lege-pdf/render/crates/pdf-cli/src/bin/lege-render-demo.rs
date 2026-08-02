//! `lege-render-demo` — a self-contained performance demonstrator for the Lege
//! PDF rendering engine.
//!
//! Built as an *optional* binary (`--features demo`) so the normal `pdfr`
//! development driver is unaffected. It is deliberately the smallest program
//! that shows the engine's throughput to someone who has never seen it:
//!
//!   1. double-click (or drag a PDF onto it, or pass a path on the command line)
//!   2. it asks for a PDF path and a render resolution
//!   3. it renders **every** page through the parallel scheduler
//!   4. it writes each page as an **uncompressed** PNG into a timestamped folder
//!      next to the executable
//!   5. it prints wall-clock time, throughput, and peak memory
//!
//! PNGs are stored uncompressed on purpose: the demo measures the *renderer*,
//! and a deflate pass would fold image-compression time into the number. It
//! also means the PNGs are byte-exact renderer output, which is what an
//! evaluator wants to inspect.
//!
//! Page writing happens on its own small thread pool so disk I/O never stalls
//! the render pipeline; the report separates "render" from "render + write".

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderBackend, RenderError, RenderLimits, RenderQuality, RenderRequest, RenderedPage,
};
use pdf_render_cpu::CpuBackend;
use pdf_render_scheduler::{PipelineOutput, RenderScheduler, SchedulerOptions};
use pdf_source::{MmapSource, PdfSource};

/// Default render resolution. PDF user space is 72 units/inch, so this is the
/// scale factor `DEFAULT_DPI / 72`. 150 DPI is the conventional comparison
/// point for PDF rasterizer benchmarks.
const DEFAULT_DPI: f64 = 150.0;

fn main() -> ExitCode {
    banner();
    let status = match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("\nError: {err:#}");
            ExitCode::FAILURE
        }
    };
    pause();
    status
}

fn banner() {
    println!("┌──────────────────────────────────────────────────────────┐");
    println!("│  Lege PDF renderer — performance demo                    │");
    println!("└──────────────────────────────────────────────────────────┘");
    println!();
    println!("  Renders every page of a PDF to an uncompressed PNG and reports");
    println!("  wall-clock time and peak memory. Uncompressed keeps deflate out");
    println!("  of the timing — expect roughly 6 MB per page at 150 DPI.");
    println!();
}

fn run() -> Result<()> {
    // A path on the command line covers "drag the PDF onto the .exe"; the
    // prompt covers a plain double-click.
    let path = match std::env::args().skip(1).find(|a| !a.starts_with('-')) {
        Some(arg) => PathBuf::from(unquote(&arg)),
        None => prompt_path()?,
    };
    if !path.is_file() {
        bail!("not a file: {}", path.display());
    }
    let dpi = prompt_dpi()?;
    let scale = dpi / 72.0;

    // ---- open ------------------------------------------------------------
    let t_total = Instant::now();
    let t_open = Instant::now();
    let snapshot = open_document(&path)?;
    let open_time = t_open.elapsed();
    let page_count = snapshot.page_count();
    if page_count == 0 {
        bail!("document has no pages");
    }

    // ---- fonts -----------------------------------------------------------
    // Resolving non-embedded fonts against the machine's installed faces costs
    // one directory scan; it is done before the timed section so the font scan
    // never lands inside the render number.
    let t_fonts = Instant::now();
    let system_fonts: Arc<dyn pdf_font::SystemFontProvider> =
        Arc::new(pdf_font::FolderFontProvider::system());
    let font_scan = t_fonts.elapsed();

    // ---- output folder ---------------------------------------------------
    let out_dir = output_dir()?;
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating {}", out_dir.display()))?;

    let options = SchedulerOptions::default();
    let compile_workers = options.compile_workers;
    let render_workers = options.render_workers;

    println!();
    println!("  file      {}", path.display());
    println!("  pages     {page_count}");
    println!("  scale     {dpi:.0} DPI  ({scale:.4}x)");
    println!("  workers   {compile_workers} compile + {render_workers} render");
    println!("  output    {}", out_dir.display());
    println!();

    // ---- PNG writer pool -------------------------------------------------
    // Bounded so completed pages cannot pile up in RAM faster than the disk
    // drains them; the render pipeline has its own memory budget upstream.
    let writer_threads = writer_thread_count();
    let (tx, rx) = crossbeam_channel::bounded::<WriteJob>(writer_threads * 2);
    let bytes_written = Arc::new(AtomicU64::new(0));
    let mut writers = Vec::with_capacity(writer_threads);
    for _ in 0..writer_threads {
        let rx = rx.clone();
        let dir = out_dir.clone();
        let counter = Arc::clone(&bytes_written);
        writers.push(std::thread::spawn(move || -> Result<()> {
            while let Ok(job) = rx.recv() {
                let file = dir.join(format!("page-{:05}.png", job.page + 1));
                let written = write_png_uncompressed(&file, &job.host)
                    .with_context(|| format!("writing {}", file.display()))?;
                counter.fetch_add(written, Ordering::Relaxed);
            }
            Ok(())
        }));
    }
    drop(rx);

    // ---- render ----------------------------------------------------------
    let backend: Arc<dyn RenderBackend> = Arc::new(CpuBackend::default());
    let scheduler = RenderScheduler::new(backend, options);
    // Built per page: `ParseContext` is per-page state, and the shared font
    // provider behind the compiler is the only thing worth keeping alive.
    let make_request = move |snap: &DocumentSnapshot,
                             page: PageIndex|
          -> Result<RenderRequest, RenderError> {
        let mut ctx = ParseContext::new();
        let compiled = PageCompiler::new()
            .with_annotations(true)
            .with_system_fonts(Arc::clone(&system_fonts))
            .compile(snap, page, &mut ctx)
            .map_err(|e| RenderError::Backend(e.to_string()))?;
        Ok(request_for_page(Arc::new(compiled), scale))
    };

    let mut rendered = 0u32;
    let mut failures: Vec<(u32, String)> = Vec::new();
    let mut pixels = 0u64;
    let t_render = Instant::now();
    scheduler.render_range(
        &snapshot,
        0..page_count,
        &make_request,
        None,
        &mut |out: PipelineOutput| {
            let page = out.page.0;
            match out.result {
                Ok(RenderedPage::Host(host)) => {
                    rendered += 1;
                    pixels += u64::from(host.width) * u64::from(host.height);
                    // A closed channel means every writer died; the joins below
                    // surface the real error, so drop the page quietly here.
                    let _ = tx.send(WriteJob { page, host });
                }
                Ok(RenderedPage::Resident(_)) => {
                    failures.push((page, "backend kept the page GPU-resident".to_owned()));
                }
                Err(err) => failures.push((page, err.to_string())),
            }
            progress(page + 1, page_count);
        },
    );
    let render_time = t_render.elapsed();

    // ---- drain writers ---------------------------------------------------
    drop(tx);
    for writer in writers {
        match writer.join() {
            Ok(result) => result?,
            Err(_) => bail!("a PNG writer thread panicked"),
        }
    }
    let total_time = t_total.elapsed();
    print!("\r{:60}\r", "");

    // ---- report ----------------------------------------------------------
    let bytes = bytes_written.load(Ordering::Relaxed);
    let secs = render_time.as_secs_f64().max(1e-9);
    let row = |label: &str, value: String| println!("  {label:<24}{value}");
    println!("──────────────────────────────────────────────────────────");
    row("open (mmap + xref)", fmt_dur(open_time));
    row("system font scan", fmt_dur(font_scan));
    row(
        &format!("render ({rendered} pages)"),
        fmt_dur(render_time),
    );
    row(
        "throughput",
        format!(
            "{:.1} pages/s   ({} per page)",
            f64::from(rendered) / secs,
            fmt_dur(render_time.checked_div(rendered.max(1)).unwrap_or_default()),
        ),
    );
    row(
        "pixel rate",
        format!("{:.1} Mpx/s", pixels as f64 / 1e6 / secs),
    );
    row("total (incl. PNG write)", fmt_dur(total_time));
    row("PNG written", fmt_bytes(bytes));
    row(
        "peak memory (RSS)",
        match peak_rss_bytes() {
            Some(peak) => fmt_bytes(peak),
            None => "not measured on this platform".to_owned(),
        },
    );
    println!("──────────────────────────────────────────────────────────");
    if !failures.is_empty() {
        println!();
        println!("  {} page(s) did not render:", failures.len());
        for (page, why) in failures.iter().take(10) {
            println!("    page {}: {why}", page + 1);
        }
        if failures.len() > 10 {
            println!("    … and {} more", failures.len() - 10);
        }
    }
    println!();
    println!("  Pages written to: {}", out_dir.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct WriteJob {
    page: u32,
    host: HostPage,
}

/// Strip the surrounding quotes a shell (or Windows "Copy as path" /
/// drag-and-drop) leaves around a path containing spaces.
fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    stripped.to_owned()
}

fn read_line() -> Result<String> {
    let mut line = String::new();
    let n = std::io::stdin()
        .read_line(&mut line)
        .context("reading from stdin")?;
    if n == 0 {
        bail!("no input (stdin closed)");
    }
    Ok(line)
}

fn prompt_path() -> Result<PathBuf> {
    for _ in 0..5 {
        print!("  PDF file (drag it here, or type/paste the path): ");
        std::io::stdout().flush().ok();
        let answer = unquote(&read_line()?);
        if answer.is_empty() {
            println!("  (nothing entered)");
            continue;
        }
        let path = PathBuf::from(&answer);
        if path.is_file() {
            return Ok(path);
        }
        println!("  no such file: {answer}");
    }
    Err(anyhow!("no readable PDF path given"))
}

fn prompt_dpi() -> Result<f64> {
    print!("  Resolution in DPI [{DEFAULT_DPI:.0}]: ");
    std::io::stdout().flush().ok();
    let answer = read_line()?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(DEFAULT_DPI);
    }
    let dpi: f64 = answer
        .parse()
        .with_context(|| format!("'{answer}' is not a number"))?;
    if !dpi.is_finite() || dpi <= 0.0 || dpi > 2400.0 {
        bail!("resolution must be between 1 and 2400 DPI");
    }
    Ok(dpi)
}

fn prompt_password() -> Result<String> {
    print!("  This document is encrypted. Password (blank to give up): ");
    std::io::stdout().flush().ok();
    Ok(read_line()?.trim().to_owned())
}

/// Keep a double-clicked console window open long enough to read the report.
fn pause() {
    println!();
    print!("Press Enter to close…");
    std::io::stdout().flush().ok();
    let mut sink = String::new();
    let _ = std::io::stdin().read_line(&mut sink);
}

fn open_document(path: &Path) -> Result<DocumentSnapshot> {
    // Memory-mapped: the engine reads objects straight out of the map, so the
    // whole file is never copied into the heap.
    let source: Arc<dyn PdfSource> =
        Arc::new(MmapSource::open(path).with_context(|| format!("opening {}", path.display()))?);
    match DocumentSnapshot::open(Arc::clone(&source), DocumentLimits::default()) {
        Ok(snapshot) => Ok(snapshot),
        Err(first) => {
            let password = prompt_password()?;
            if password.is_empty() {
                return Err(anyhow!("{first}"))
                    .with_context(|| format!("opening {}", path.display()));
            }
            DocumentSnapshot::open_with_password(source, DocumentLimits::default(), Some(&password))
                .with_context(|| format!("opening {}", path.display()))
        }
    }
}

/// A timestamped folder beside the executable, falling back to the working
/// directory when the executable lives somewhere read-only.
fn output_dir() -> Result<PathBuf> {
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let name = format!("lege-render_{stamp}");
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    if let Some(dir) = beside_exe {
        // Probe writability before committing to this location.
        if std::fs::create_dir_all(&dir).is_ok() {
            let candidate = dir.join(&name);
            if std::fs::create_dir_all(&candidate).is_ok() {
                return Ok(candidate);
            }
        }
    }
    let cwd = std::env::current_dir().context("resolving the working directory")?;
    Ok(cwd.join(name))
}

fn writer_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 4)
        .unwrap_or(2)
        .clamp(2, 4)
}

fn progress(done: u32, total: u32) {
    print!("\r  rendering… {done}/{total}");
    std::io::stdout().flush().ok();
}

// ---------------------------------------------------------------------------
// Render request
// ---------------------------------------------------------------------------

/// Map page user space (y-up) onto the device surface (y-down): translate the
/// crop-box origin to (0,0) and apply the page `/Rotate` as clockwise quarter
/// turns, matching what a viewer displays.
fn request_for_page(page: Arc<pdf_page_ir::CompiledPage>, scale: f64) -> RenderRequest {
    let crop = page.bounds.crop;
    let (width, height) = match page.bounds.rotate {
        90 | 270 => (
            (crop.height() * scale).ceil() as u32,
            (crop.width() * scale).ceil() as u32,
        ),
        _ => (
            (crop.width() * scale).ceil() as u32,
            (crop.height() * scale).ceil() as u32,
        ),
    };
    let matrix = match page.bounds.rotate {
        90 => Matrix {
            a: 0.0,
            b: scale,
            c: scale,
            d: 0.0,
            e: -crop.y0 * scale,
            f: -crop.x0 * scale,
        },
        180 => Matrix {
            a: -scale,
            b: 0.0,
            c: 0.0,
            d: scale,
            e: crop.x1 * scale,
            f: -crop.y0 * scale,
        },
        270 => Matrix {
            a: 0.0,
            b: -scale,
            c: -scale,
            d: 0.0,
            e: crop.y1 * scale,
            f: crop.x1 * scale,
        },
        _ => Matrix {
            a: scale,
            b: 0.0,
            c: 0.0,
            d: -scale,
            e: -crop.x0 * scale,
            f: crop.y1 * scale,
        },
    };
    RenderRequest {
        page,
        transform: PageTransform { matrix },
        crop: None,
        output_size: DeviceSize { width, height },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations: AnnotationMode::StaticAppearances,
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    }
}

// ---------------------------------------------------------------------------
// Uncompressed PNG
// ---------------------------------------------------------------------------

/// Write `page` as an 8-bit RGB PNG whose IDAT uses *stored* (uncompressed)
/// deflate blocks, and return the file size in bytes.
///
/// Every PNG reader accepts this — a stored block is ordinary deflate — but no
/// entropy coding runs, so the wall-clock number stays a measurement of the
/// renderer rather than of zlib.
fn write_png_uncompressed(path: &Path, page: &HostPage) -> Result<u64> {
    let width = page.width;
    let height = page.height;
    let row_bytes = width as usize * 3;

    // Raw PNG scanlines: one filter byte (0 = None) per row, then RGB triples.
    let mut raw = Vec::with_capacity((row_bytes + 1) * height as usize);
    let bpp = page.format.bytes_per_pixel();
    for y in 0..height as usize {
        raw.push(0u8);
        let start = y * page.stride;
        let row = &page.pixels[start..start + width as usize * bpp];
        match page.format {
            // Premultiplied over an opaque white background: alpha is 255, so
            // the RGB triple is already the final color.
            OutputFormat::Rgba8PremultipliedSrgb => {
                for px in row.chunks_exact(4) {
                    raw.extend_from_slice(&px[..3]);
                }
            }
            OutputFormat::Gray8 => {
                for &g in row {
                    raw.extend_from_slice(&[g, g, g]);
                }
            }
        }
    }

    // `Compression::none()` emits stored deflate blocks plus the zlib
    // header/Adler-32 trailer PNG requires.
    let mut zlib = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(raw.len() + raw.len() / 1024 + 64),
        flate2::Compression::none(),
    );
    zlib.write_all(&raw).context("packing PNG image data")?;
    let idat = zlib.finish().context("finishing PNG image data")?;
    drop(raw);

    let file = std::fs::File::create(path)?;
    let mut out = BufWriter::with_capacity(1 << 20, file);
    out.write_all(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])?;

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolor RGB, no interlace
    let mut total = 8u64;
    total += write_chunk(&mut out, b"IHDR", &ihdr)?;
    total += write_chunk(&mut out, b"IDAT", &idat)?;
    total += write_chunk(&mut out, b"IEND", &[])?;
    out.flush()?;
    Ok(total)
}

fn write_chunk<W: Write>(out: &mut W, kind: &[u8; 4], data: &[u8]) -> Result<u64> {
    let len = u32::try_from(data.len()).context("PNG chunk larger than 4 GiB")?;
    // PNG's chunk CRC is CRC-32/ISO-HDLC over the type code and the payload —
    // the same polynomial zlib exposes.
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    out.write_all(&len.to_be_bytes())?;
    out.write_all(kind)?;
    out.write_all(data)?;
    out.write_all(&crc.sum().to_be_bytes())?;
    Ok(12 + u64::from(len))
}

// ---------------------------------------------------------------------------
// Peak memory
// ---------------------------------------------------------------------------

/// High-water mark of resident memory for this process, in bytes.
#[cfg(target_os = "linux")]
fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find_map(|l| l.strip_prefix("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().next()?.parse().ok()?;
    Some(kib * 1024)
}

/// `PROCESS_MEMORY_COUNTERS` (psapi.h). `cb` and `PageFaultCount` are `DWORD`;
/// the rest are `SIZE_T`, so the layout is identical on 32- and 64-bit.
#[cfg(windows)]
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "reading the OS peak-working-set counter needs a psapi call"
)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
    fn K32GetProcessMemoryInfo(
        process: *mut core::ffi::c_void,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "reading the OS peak-working-set counter needs a psapi call"
)]
fn peak_rss_bytes() -> Option<u64> {
    let cb = u32::try_from(size_of::<ProcessMemoryCounters>()).ok()?;
    let mut counters = ProcessMemoryCounters {
        cb,
        ..Default::default()
    };
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is always valid
    // for the calling process and needs no release. `counters` is a live,
    // correctly sized `PROCESS_MEMORY_COUNTERS` and `cb` is its exact byte
    // size, so the callee writes only within it. The call does not retain the
    // pointer.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, cb) };
    (ok != 0).then(|| counters.peak_working_set_size as u64)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s >= 1.0 {
        format!("{s:.2} s")
    } else if s >= 1e-3 {
        format!("{:.1} ms", s * 1e3)
    } else {
        format!("{:.0} µs", s * 1e6)
    }
}

fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
