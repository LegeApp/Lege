//! Multi-renderer targeted rendering and resumable corpus comparison.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use font8x8::UnicodeFonts;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{PaintLeaf, PaintOrigin};
use pdf_render_cpu::attribution::AttributionMap;
use pdf_render_cpu::CpuBackend;
use pdf_source::{OwnedBytesSource, PdfSource};

use crate::compare::Diff;
use crate::renderers::{
    parse_override, parse_references, PdfRenderer, ProcessRenderer,
    RenderRequest as ExternalRequest, RenderedPage, RendererId,
};

const SAMPLE: usize = 6;
const CSV_HEADER: &str = "schema,file,page,reference,ours_width,ours_height,reference_width,reference_height,ink_delta,gross,ours_ink,reference_ink,ours_continuous_ink,reference_continuous_ink,degraded,status,note,flags=annot+cropbox+white+continuous-ink";
const ATTRIBUTION_HEADER: &str = "schema,file,page,renderer,category_kind,category,diff_pixels,category_pixels,share,diff_threshold,coverage_threshold,note";
const TIMING_HEADER: &str =
    "schema,renderer,requested_pages,rendered_pages,failed_batches,total_ms,ms_per_page,scope";

#[derive(Default)]
struct Options {
    references: Vec<String>,
    paths: BTreeMap<RendererId, PathBuf>,
    pages: Option<Vec<u32>>,
    scale: f64,
    out: Option<PathBuf>,
    dump: bool,
    attribution: bool,
    timing_sample: Option<usize>,
    positional: Vec<PathBuf>,
}

pub fn run(command: &str, args: &[String]) -> Result<(), String> {
    let options = parse_options(command, args)?;
    let ids = parse_references(&options.references)?;
    let renderers = ids
        .into_iter()
        .map(|id| ProcessRenderer::discover(id, &options.paths))
        .collect::<Result<Vec<_>, _>>()?;
    match command {
        "render" => targeted(options, &renderers),
        "sweep" => sweep(options, &renderers),
        "benchmark" => benchmark(options, &renderers),
        _ => Err(format!("unknown multi-renderer command {command:?}")),
    }
}

fn parse_options(command: &str, args: &[String]) -> Result<Options, String> {
    let mut result = Options {
        scale: 1.0,
        ..Options::default()
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--reference" => {
                let value = args.get(i + 1).ok_or("--reference requires a value")?;
                result.references.push(value.clone());
                i += 2;
            }
            "--renderer-path" => {
                let value = args
                    .get(i + 1)
                    .ok_or("--renderer-path requires NAME=PATH")?;
                let (id, path) = parse_override(value)?;
                result.paths.insert(id, path);
                i += 2;
            }
            "--pages" => {
                let value = args.get(i + 1).ok_or("--pages requires a range")?;
                result.pages = Some(parse_pages(value)?);
                i += 2;
            }
            "--scale" => {
                result.scale = args
                    .get(i + 1)
                    .ok_or("--scale requires a number")?
                    .parse()
                    .map_err(|_| "--scale must be a positive number")?;
                if !result.scale.is_finite() || result.scale <= 0.0 {
                    return Err("--scale must be a positive number".into());
                }
                i += 2;
            }
            "--out" => {
                result.out = Some(PathBuf::from(
                    args.get(i + 1).ok_or("--out requires a path")?,
                ));
                i += 2;
            }
            "--dump" => {
                result.dump = true;
                i += 1;
            }
            "--attribution" => {
                result.attribution = true;
                i += 1;
            }
            "--timing-sample" | "--samples" => {
                let value = args.get(i + 1).ok_or("--timing-sample requires a count")?;
                let count = value
                    .parse::<usize>()
                    .map_err(|_| "--timing-sample must be a positive integer")?;
                if count == 0 || count > 100_000 {
                    return Err("--timing-sample must be between 1 and 100000".into());
                }
                result.timing_sample = Some(count);
                i += 2;
            }
            value if value.starts_with('-') => return Err(format!("unknown option {value:?}")),
            value => {
                result.positional.push(PathBuf::from(value));
                i += 1;
            }
        }
    }
    if result.references.is_empty() {
        return Err("missing --reference <name[,name]|all>".into());
    }
    match command {
        "render" if result.positional.len() != 1 => Err(
            "usage: pdfium-diff render <pdf> --pages 0,2-4 --scale 2 --reference all --out <dir>"
                .into(),
        ),
        "sweep" if result.positional.is_empty() => Err(
            "usage: pdfium-diff sweep <pdf|dir>... --scale 1 --reference all --out <dir>".into(),
        ),
        "benchmark" if result.positional.is_empty() => Err(
            "usage: pdfium-diff benchmark <pdf|dir>... --samples 200 --scale 1 --reference all --out <dir>".into(),
        ),
        "benchmark" if result.timing_sample.is_none() => {
            result.timing_sample = Some(200);
            Ok(result)
        }
        _ => Ok(result),
    }
}

fn parse_pages(value: &str) -> Result<Vec<u32>, String> {
    let mut pages = Vec::new();
    for item in value.split(',').filter(|s| !s.is_empty()) {
        if let Some((first, last)) = item.split_once('-') {
            let first: u32 = first
                .parse()
                .map_err(|_| format!("invalid page range {item:?}"))?;
            let last: u32 = last
                .parse()
                .map_err(|_| format!("invalid page range {item:?}"))?;
            if first > last || last - first > 100_000 {
                return Err(format!("invalid page range {item:?}"));
            }
            pages.extend(first..=last);
        } else {
            pages.push(item.parse().map_err(|_| format!("invalid page {item:?}"))?);
        }
    }
    pages.sort_unstable();
    pages.dedup();
    if pages.is_empty() {
        Err("page list is empty".into())
    } else {
        Ok(pages)
    }
}

fn targeted(options: Options, renderers: &[ProcessRenderer]) -> Result<(), String> {
    let pdf = &options.positional[0];
    let pages = options.pages.unwrap_or_else(|| vec![0]);
    let out = options
        .out
        .unwrap_or_else(|| PathBuf::from("renderer-diff-out"));
    std::fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let ours = render_ours(pdf, &pages, options.scale)?;
    let attributions = if options.attribution {
        let maps = render_attributions(pdf, &pages, options.scale)?;
        for (&page, map) in &maps {
            write_attribution_planes(&out, pdf, page, map)?;
        }
        maps
    } else {
        BTreeMap::new()
    };
    let mut attribution_csv = if options.attribution {
        Some(open_attribution_csv(&out.join("attribution.csv"))?)
    } else {
        None
    };
    for page in &ours {
        write_named_png(&out, pdf, "ours", page.page, &page.png)?;
    }
    let mut groups: BTreeMap<u32, Vec<(String, RenderedPage)>> = BTreeMap::new();
    for page in ours {
        groups
            .entry(page.page)
            .or_default()
            .push(("ours".into(), page));
    }
    for renderer in renderers {
        let id = renderer.id();
        match renderer.render_pages(&ExternalRequest {
            pdf,
            pages: &pages,
            scale: options.scale,
        }) {
            Ok(rendered) => {
                for page in rendered {
                    eprintln!(
                        "{id} page {}: {:.1} ms",
                        page.page,
                        page.elapsed.as_secs_f64() * 1000.0
                    );
                    write_named_png(&out, pdf, id.name(), page.page, &page.png)?;
                    if let (Some(csv), Some(map), Some((_, ours))) = (
                        attribution_csv.as_mut(),
                        attributions.get(&page.page),
                        groups.get(&page.page).and_then(|renders| renders.first()),
                    ) {
                        write_attribution_rows(csv, pdf, id, ours, &page, map)?;
                    }
                    groups
                        .entry(page.page)
                        .or_default()
                        .push((id.name().into(), page));
                }
            }
            Err(error) => eprintln!("{id}: {error}"),
        }
    }
    for (page, renders) in groups {
        let path = out.join(format!("{}-p{page}-comparison.png", safe_stem(pdf)));
        write_contact_sheet(&path, &renders)?;
        println!("{}", path.display());
    }
    Ok(())
}

/// Render `pending` as one batch; if that fails and more than one page was
/// asked for, retry each page alone.
///
/// External engines render a whole page list in one invocation, so a single
/// unrenderable page used to discard every page of that file. Sweep 11 lost 68
/// files that way — `hellstorm` declares 386 pages but only ~200 are
/// recoverable, we synthesise blank placeholders for the rest, and every
/// control failed the batch on the one page they could not produce.
///
/// The retry costs one extra invocation per page, but only on a file that has
/// already failed once.
fn render_pages_resilient(
    renderer: &ProcessRenderer,
    pdf: &Path,
    pending: &[u32],
    scale: f64,
) -> (Vec<RenderedPage>, Vec<(u32, String)>) {
    let request = ExternalRequest {
        pdf,
        pages: pending,
        scale,
    };
    match renderer.render_pages(&request) {
        Ok(pages) => (pages, Vec::new()),
        // A batch that dies on one bad page loses every page in it. Retry the
        // pages singly so one poisoned page costs one row, not a whole file.
        Err(_) if pending.len() > 1 => {
            let mut rendered = Vec::new();
            let mut failed = Vec::new();
            for &page in pending {
                let one = ExternalRequest {
                    pdf,
                    pages: std::slice::from_ref(&page),
                    scale,
                };
                match renderer.render_pages(&one) {
                    Ok(mut pages) => rendered.append(&mut pages),
                    Err(error) => failed.push((page, error)),
                }
            }
            (rendered, failed)
        }
        Err(error) => (
            Vec::new(),
            pending.iter().map(|page| (*page, error.clone())).collect(),
        ),
    }
}

fn sweep(options: Options, renderers: &[ProcessRenderer]) -> Result<(), String> {
    let out = options
        .out
        .unwrap_or_else(|| PathBuf::from("renderer-diff-out"));
    std::fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let csv_path = out.join("results.csv");
    ensure_header(&csv_path)?;
    let done = load_done(&csv_path)?;
    let mut csv = std::fs::OpenOptions::new()
        .append(true)
        .open(&csv_path)
        .map_err(|e| format!("open {}: {e}", csv_path.display()))?;
    let mut attribution_csv = if options.attribution {
        Some(open_attribution_csv(&out.join("attribution.csv"))?)
    } else {
        None
    };
    let attribution_done = if options.attribution {
        load_attribution_done(&out.join("attribution.csv"))?
    } else {
        HashSet::new()
    };
    let mut files = Vec::new();
    for path in &options.positional {
        crate::collect_pdfs(path, 0, &mut files);
    }
    files.sort();
    files.dedup();
    if let Some(samples) = options.timing_sample {
        run_benchmark(&files, samples, options.scale, renderers, &out)?;
    }
    for (index, pdf) in files.iter().enumerate() {
        eprintln!("[{index}/{}] {}", files.len(), pdf.display());
        let count = match our_page_count(pdf) {
            Ok(n) if n > 0 => n,
            Ok(_) => continue,
            Err(error) => {
                eprintln!("ours open failed: {error}");
                continue;
            }
        };
        let pages = options
            .pages
            .clone()
            .unwrap_or_else(|| sampled_pages(count));
        let ours = match render_ours(pdf, &pages, options.scale) {
            Ok(pages) => pages
                .into_iter()
                .map(|p| (p.page, p))
                .collect::<BTreeMap<_, _>>(),
            Err(error) => {
                eprintln!("ours render failed: {error}");
                continue;
            }
        };
        let attributions = if options.attribution {
            match render_attributions(pdf, &pages, options.scale) {
                Ok(maps) => maps,
                Err(error) => {
                    eprintln!("ours attribution failed: {error}");
                    BTreeMap::new()
                }
            }
        } else {
            BTreeMap::new()
        };
        for renderer in renderers {
            let id = renderer.id();
            let pending = pages
                .iter()
                .copied()
                .filter(|page| {
                    let key = format!("{}|{page}|{id}", pdf.display());
                    !done.contains(&key)
                        || (options.attribution && !attribution_done.contains(&key))
                })
                .collect::<Vec<_>>();
            if pending.is_empty() {
                continue;
            }
            let (references, failures) =
                render_pages_resilient(renderer, pdf, &pending, options.scale);
            for reference in references {
                let Some(ours) = ours.get(&reference.page) else {
                    continue;
                };
                let (ours_rgba, reference_rgba, width, height) = normalize_pair(ours, &reference)?;
                let diff = compare_rgba(&ours_rgba, &reference_rgba, width, height);
                // Engines disagree on how to turn a fractional page box into
                // whole pixels — hayro floors, we round — so a ±1 edge is a
                // rounding artefact, not a layout difference. Sweep 11 labelled
                // 47,352 otherwise-clean pages `dimension-mismatch` on that
                // alone, which buried the real ones. `normalize_pair` already
                // reconciles the rasters before `compare_rgba` either way.
                let dw = ours.width.abs_diff(reference.width);
                let dh = ours.height.abs_diff(reference.height);
                let status = if dw > 1 || dh > 1 {
                    "dimension-mismatch"
                } else if diff.is_suspect() {
                    "suspect"
                } else {
                    "ok"
                };
                let key = format!("{}|{}|{id}", pdf.display(), reference.page);
                if !done.contains(&key) {
                    writeln!(
                        csv,
                        "3,{},{},{},{},{},{},{},{:.5},{:.5},{:.5},{:.5},{:.5},{:.5},0,{},,",
                        csv_escape(&pdf.display().to_string()),
                        reference.page,
                        id,
                        ours.width,
                        ours.height,
                        reference.width,
                        reference.height,
                        diff.ink_delta(),
                        diff.gross_frac,
                        diff.ours_ink,
                        diff.ref_ink,
                        diff.ours_continuous_ink,
                        diff.ref_continuous_ink,
                        status
                    )
                    .map_err(|e| format!("write CSV: {e}"))?;
                    csv.flush().map_err(|e| format!("flush CSV: {e}"))?;
                }
                if options.attribution && !attribution_done.contains(&key) {
                    if let (Some(attr_csv), Some(map)) =
                        (attribution_csv.as_mut(), attributions.get(&reference.page))
                    {
                        write_attribution_rows(attr_csv, pdf, id, ours, &reference, map)?;
                    }
                }
                if options.dump && (diff.is_suspect() || status == "dimension-mismatch") {
                    let renders =
                        vec![("ours".into(), ours.clone()), (id.name().into(), reference)];
                    write_contact_sheet(
                        &out.join(format!(
                            "{}-p{}-{id}-comparison.png",
                            safe_stem(pdf),
                            renders[0].1.page
                        )),
                        &renders,
                    )?;
                }
            }
            for (page, error) in failures {
                let note = error.replace([',', '\n', '\r'], ";");
                if !done.contains(&format!("{}|{page}|{id}", pdf.display())) {
                    // Metrics are left *empty*, not zero-or-one. An
                    // engine that failed produced no raster, so there
                    // is no measurement — writing `ink=1, gross=1`
                    // made a crashed control indistinguishable from a
                    // total disagreement, and those rows sorted
                    // straight to the top of any "worst pages" list.
                    // (Sweep 11: `hellstorm` and
                    // `Jews-and-the-Military` read as "we render
                    // nothing, everyone else renders everything" when
                    // in fact nobody rendered anything.)
                    writeln!(csv, "{}", failure_csv_row(pdf, page, id, &note))
                        .map_err(|e| format!("write CSV: {e}"))?;
                }
            }
            csv.flush().map_err(|e| format!("flush CSV: {e}"))?;
        }
    }
    Ok(())
}

fn benchmark(options: Options, renderers: &[ProcessRenderer]) -> Result<(), String> {
    let out = options
        .out
        .unwrap_or_else(|| PathBuf::from("renderer-diff-out"));
    std::fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let mut files = Vec::new();
    for path in &options.positional {
        crate::collect_pdfs(path, 0, &mut files);
    }
    files.sort();
    files.dedup();
    run_benchmark(
        &files,
        options.timing_sample.unwrap_or(200),
        options.scale,
        renderers,
        &out,
    )
}

#[derive(Default)]
struct TimingResult {
    requested: usize,
    rendered: usize,
    failed_batches: usize,
    elapsed: std::time::Duration,
}

/// End-to-end cold-document batches: PDF open, selected-page rendering, PNG
/// encoding and (for controls) process startup are all intentionally included.
fn run_benchmark(
    files: &[PathBuf],
    sample_limit: usize,
    scale: f64,
    renderers: &[ProcessRenderer],
    out: &Path,
) -> Result<(), String> {
    let mut batches = Vec::<(PathBuf, Vec<u32>)>::new();
    let mut remaining = sample_limit;
    for pdf in files {
        if remaining == 0 {
            break;
        }
        let Ok(count) = our_page_count(pdf) else {
            continue;
        };
        let mut pages = sampled_pages(count);
        pages.truncate(remaining);
        if !pages.is_empty() {
            remaining -= pages.len();
            batches.push((pdf.clone(), pages));
        }
    }
    let requested = sample_limit - remaining;
    if requested == 0 {
        return Err("timing sample found no renderable pages".into());
    }

    let mut rows = Vec::<(String, TimingResult)>::new();
    let mut ours = TimingResult::default();
    for (pdf, pages) in &batches {
        ours.requested += pages.len();
        let start = std::time::Instant::now();
        match render_ours(pdf, pages, scale) {
            Ok(rendered) => ours.rendered += rendered.len(),
            Err(error) => {
                ours.failed_batches += 1;
                eprintln!("timing ours {}: {error}", pdf.display());
            }
        }
        ours.elapsed += start.elapsed();
    }
    rows.push(("ours".into(), ours));

    for renderer in renderers {
        let mut timing = TimingResult::default();
        for (pdf, pages) in &batches {
            timing.requested += pages.len();
            let start = std::time::Instant::now();
            match renderer.render_pages(&ExternalRequest { pdf, pages, scale }) {
                Ok(rendered) => timing.rendered += rendered.len(),
                Err(error) => {
                    timing.failed_batches += 1;
                    eprintln!("timing {} {}: {error}", renderer.id(), pdf.display());
                }
            }
            timing.elapsed += start.elapsed();
        }
        rows.push((renderer.id().name().into(), timing));
    }

    let path = out.join("timing.csv");
    let mut csv =
        std::fs::File::create(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
    writeln!(csv, "{TIMING_HEADER}").map_err(|e| format!("write timing CSV: {e}"))?;
    eprintln!(
        "timing sample: {requested} pages in {} document batches",
        batches.len()
    );
    for (name, timing) in rows {
        let total_ms = timing.elapsed.as_secs_f64() * 1000.0;
        let per_page = if timing.rendered == 0 {
            f64::NAN
        } else {
            total_ms / timing.rendered as f64
        };
        writeln!(
            csv,
            "1,{name},{},{},{},{total_ms:.3},{per_page:.3},cold-document+process+png",
            timing.requested, timing.rendered, timing.failed_batches
        )
        .map_err(|e| format!("write timing CSV: {e}"))?;
        eprintln!(
            "  {name:12} {total_ms:10.1} ms total  {per_page:8.2} ms/page  failures={}",
            timing.failed_batches
        );
    }
    println!("{}", path.display());
    Ok(())
}

/// Wall-clock budget for a single in-process page render in the multi-oracle
/// sweep, overridable via `PDFIUM_DIFF_OURS_TIMEOUT` (seconds). Default 90s —
/// generous enough for the heaviest legitimate page, short enough that one
/// pathological file cannot stall a multi-hour corpus run.
fn ours_render_timeout() -> std::time::Duration {
    std::env::var("PDFIUM_DIFF_OURS_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(90))
}

fn our_page_count(pdf: &Path) -> Result<u32, String> {
    let bytes = std::fs::read(pdf).map_err(|e| format!("read {}: {e}", pdf.display()))?;
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snapshot = DocumentSnapshot::open(source, DocumentLimits::default())
        .map_err(|_| "open failed".to_string())?;
    Ok(snapshot.page_count())
}

fn sampled_pages(count: u32) -> Vec<u32> {
    let step = (count as usize / SAMPLE).max(1);
    (0..count).step_by(step).take(SAMPLE).collect()
}

fn render_ours(pdf: &Path, pages: &[u32], scale: f64) -> Result<Vec<RenderedPage>, String> {
    let bytes = std::fs::read(pdf).map_err(|e| format!("read {}: {e}", pdf.display()))?;
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snapshot = DocumentSnapshot::open(source, DocumentLimits::default())
        .map_err(|_| "ours open failed".to_string())?;
    let backend = CpuBackend::default();
    let timeout = ours_render_timeout();
    let mut result = Vec::new();
    for &page in pages {
        let mut ctx = ParseContext::new();
        let mut request = crate::build_request(&snapshot, PageIndex(page), scale, &mut ctx)
            .map_err(|e| format!("ours page {page}: {e}"))?;
        // Bound the in-process render with a wall-clock watchdog. A slow or
        // pathological page must not stall the whole corpus sweep: the renderer
        // checks this cancellation token at op boundaries, so tripping it after
        // the deadline turns a hang into a per-page error the caller skips.
        let token = pdf_render_api::CancellationToken::new();
        request.limits.cancellation = Some(token.clone());
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watchdog = {
            let done = done.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + timeout;
                while std::time::Instant::now() < deadline {
                    if done.load(std::sync::atomic::Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                token.cancel();
            })
        };
        let start = std::time::Instant::now();
        let render = backend.render_to_host(&request);
        done.store(true, std::sync::atomic::Ordering::Release);
        let _ = watchdog.join();
        let (host, _) = render.map_err(|e| format!("ours page {page}: {e}"))?;
        let elapsed = start.elapsed();
        let png = encode_rgba(host.width, host.height, &host.pixels)?;
        result.push(RenderedPage {
            page,
            width: host.width,
            height: host.height,
            png,
            elapsed,
        });
    }
    Ok(result)
}

fn render_attributions(
    pdf: &Path,
    pages: &[u32],
    scale: f64,
) -> Result<BTreeMap<u32, AttributionMap>, String> {
    let bytes = std::fs::read(pdf).map_err(|e| format!("read {}: {e}", pdf.display()))?;
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snapshot = DocumentSnapshot::open(source, DocumentLimits::default())
        .map_err(|_| "ours open failed".to_string())?;
    let backend = CpuBackend::default();
    let mut maps = BTreeMap::new();
    for &page in pages {
        let mut ctx = ParseContext::new();
        let request = crate::build_request(&snapshot, PageIndex(page), scale, &mut ctx)
            .map_err(|e| format!("ours attribution page {page}: {e}"))?;
        let prepared = backend
            .prepare_attribution(&request)
            .map_err(|e| format!("ours attribution page {page}: {e}"))?;
        maps.insert(
            page,
            pdf_render_cpu::attribution::render_attribution(&prepared),
        );
    }
    Ok(maps)
}

fn write_attribution_planes(
    out: &Path,
    pdf: &Path,
    page: u32,
    map: &AttributionMap,
) -> Result<(), String> {
    let base = format!("{}-p{page}", safe_stem(pdf));
    write_gray_png(
        &out.join(format!("{base}-leaf.png")),
        map.width,
        map.height,
        &map.leaf,
    )?;
    write_gray_png(
        &out.join(format!("{base}-origin.png")),
        map.width,
        map.height,
        &map.origin,
    )?;
    let legend = format!(
        concat!(
            "{{\n",
            "  \"leaf\": {{\"0\":\"unpainted\",\"1\":\"path\",\"2\":\"shading\",\"3\":\"tiling-pattern\",\"4\":\"text\",\"5\":\"image\"}},\n",
            "  \"origin\": {{\"0\":\"page-content\",\"1\":\"form-xobject\",\"2\":\"annotation-appearance\",\"3\":\"tiling-pattern-cell\",\"4\":\"type3-glyph\",\"5\":\"soft-mask-content\"}},\n",
            "  \"coverage_threshold\": {},\n",
            "  \"origin_precedence\": \"innermost-wins\",\n",
            "  \"diagnostic_only\": true\n",
            "}}\n"
        ),
        map.coverage_threshold
    );
    std::fs::write(out.join(format!("{base}-attribution.json")), legend)
        .map_err(|e| format!("write attribution legend: {e}"))
}

fn write_gray_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), String> {
    let file =
        std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("encode PNG: {e}"))?;
    writer
        .write_image_data(pixels)
        .map_err(|e| format!("encode PNG pixels: {e}"))
}

fn open_attribution_csv(path: &Path) -> Result<std::fs::File, String> {
    if std::fs::metadata(path).map_or(true, |metadata| metadata.len() == 0) {
        std::fs::write(path, format!("{ATTRIBUTION_HEADER}\n"))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    } else {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if text.lines().next() != Some(ATTRIBUTION_HEADER) {
            return Err(format!(
                "{} has an incompatible attribution schema",
                path.display()
            ));
        }
    }
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))
}

fn load_attribution_done(path: &Path) -> Result<HashSet<String>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut counts = BTreeMap::<String, usize>::new();
    for line in text.lines().skip(1) {
        if let Some(fields) = split_csv_prefix(line, 4) {
            *counts
                .entry(format!("{}|{}|{}", fields[1], fields[2], fields[3]))
                .or_default() += 1;
        }
    }
    Ok(counts
        .into_iter()
        .filter_map(|(key, count)| (count >= 13).then_some(key))
        .collect())
}

fn write_attribution_rows(
    csv: &mut std::fs::File,
    pdf: &Path,
    renderer: RendererId,
    ours: &RenderedPage,
    reference: &RenderedPage,
    map: &AttributionMap,
) -> Result<(), String> {
    let (_, _, ours_rgba) = decode_rgba(&ours.png)?;
    let (_, _, reference_rgba) = decode_rgba(&reference.png)?;
    let width = ours.width.max(reference.width);
    let height = ours.height.max(reference.height);
    let ours_rgba = pad_rgba(&ours_rgba, ours.width, ours.height, width, height);
    let reference_rgba = pad_rgba(
        &reference_rgba,
        reference.width,
        reference.height,
        width,
        height,
    );
    let mut leaf_pixels = [0u64; 6];
    let mut leaf_diff = [0u64; 6];
    // Index 6 is synthetic `unattributed`: external-only or unpainted in ours.
    let mut origin_pixels = [0u64; 7];
    let mut origin_diff = [0u64; 7];
    let mut total_diff = 0u64;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let output_index = y * width as usize + x;
            let map_index = (x < map.width as usize && y < map.height as usize)
                .then_some(y * map.width as usize + x);
            let leaf = map_index
                .and_then(|index| map.leaf.get(index).copied())
                .filter(|&value| value <= PaintLeaf::Image as u8)
                .unwrap_or(PaintLeaf::Unpainted as u8) as usize;
            let origin = if leaf == PaintLeaf::Unpainted as usize {
                6
            } else {
                map_index
                    .and_then(|index| map.origin.get(index).copied())
                    .filter(|&value| value <= PaintOrigin::SoftMaskContent as u8)
                    .map(usize::from)
                    .unwrap_or(6)
            };
            leaf_pixels[leaf] += 1;
            origin_pixels[origin] += 1;
            let oi = output_index * 4;
            let gross = ours_rgba[oi]
                .abs_diff(reference_rgba[oi])
                .max(ours_rgba[oi + 1].abs_diff(reference_rgba[oi + 1]))
                .max(ours_rgba[oi + 2].abs_diff(reference_rgba[oi + 2]))
                > crate::compare::GROSS;
            if gross {
                total_diff += 1;
                leaf_diff[leaf] += 1;
                origin_diff[origin] += 1;
            }
        }
    }
    let leaf_names = [
        "unpainted",
        "path",
        "shading",
        "tiling-pattern",
        "text",
        "image",
    ];
    let origin_names = [
        "page-content",
        "form-xobject",
        "annotation-appearance",
        "tiling-pattern-cell",
        "type3-glyph",
        "soft-mask-content",
        "unattributed",
    ];
    for (kind, names, pixels, diffs) in [
        (
            "leaf",
            leaf_names.as_slice(),
            leaf_pixels.as_slice(),
            leaf_diff.as_slice(),
        ),
        (
            "origin",
            origin_names.as_slice(),
            origin_pixels.as_slice(),
            origin_diff.as_slice(),
        ),
    ] {
        for index in 0..names.len() {
            let share = if total_diff == 0 {
                0.0
            } else {
                diffs[index] as f64 / total_diff as f64
            };
            writeln!(
                csv,
                "1,{},{},{renderer},{kind},{},{},{},{share:.6},{},{},geometry-only;innermost-wins",
                csv_escape(&pdf.display().to_string()),
                reference.page,
                names[index],
                diffs[index],
                pixels[index],
                crate::compare::GROSS,
                map.coverage_threshold,
            )
            .map_err(|e| format!("write attribution CSV: {e}"))?;
        }
    }
    csv.flush()
        .map_err(|e| format!("flush attribution CSV: {e}"))
}

fn decode_rgba(png_bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    // `png` 0.18's `Decoder<R>` requires `R: Read + Seek`; a bare `&[u8]` only
    // implements `Read`, so the slice is wrapped rather than passed directly.
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("decode PNG: {e}"))?;
    // 0.18 returns `None` when the frame's buffer size overflows `usize`.
    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| "decode PNG: frame too large to buffer".to_string())?;
    let mut buffer = vec![0; buffer_size];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|e| format!("decode PNG pixels: {e}"))?;
    let input = &buffer[..info.buffer_size()];
    let mut out = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    match info.color_type {
        png::ColorType::Rgba => {
            for p in input.chunks_exact(4) {
                composite(&mut out, p[0], p[1], p[2], p[3]);
            }
        }
        png::ColorType::Rgb => {
            for p in input.chunks_exact(3) {
                out.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &v in input {
                out.extend_from_slice(&[v, v, v, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for p in input.chunks_exact(2) {
                composite(&mut out, p[0], p[0], p[0], p[1]);
            }
        }
        png::ColorType::Indexed => return Err("indexed PNG was not expanded".into()),
    }
    Ok((info.width, info.height, out))
}

fn composite(out: &mut Vec<u8>, r: u8, g: u8, b: u8, a: u8) {
    let blend = |v: u8| ((v as u16 * a as u16 + 255 * (255 - a as u16) + 127) / 255) as u8;
    out.extend_from_slice(&[blend(r), blend(g), blend(b), 255]);
}

fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("encode PNG: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("encode PNG pixels: {e}"))?;
    }
    Ok(bytes)
}

fn normalize_pair(
    a: &RenderedPage,
    b: &RenderedPage,
) -> Result<(Vec<u8>, Vec<u8>, u32, u32), String> {
    let (_, _, ar) = decode_rgba(&a.png)?;
    let (_, _, br) = decode_rgba(&b.png)?;
    let width = a.width.max(b.width);
    let height = a.height.max(b.height);
    Ok((
        pad_rgba(&ar, a.width, a.height, width, height),
        pad_rgba(&br, b.width, b.height, width, height),
        width,
        height,
    ))
}

fn pad_rgba(input: &[u8], in_width: u32, in_height: u32, width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![255; width as usize * height as usize * 4];
    for y in 0..in_height as usize {
        let src = y * in_width as usize * 4;
        let dst = y * width as usize * 4;
        out[dst..dst + in_width as usize * 4]
            .copy_from_slice(&input[src..src + in_width as usize * 4]);
    }
    out
}

fn compare_rgba(ours: &[u8], reference: &[u8], width: u32, height: u32) -> Diff {
    let mut bgra = reference.to_vec();
    for p in bgra.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    crate::compare::compare(ours, &bgra, width, height)
}

fn write_contact_sheet(path: &Path, renders: &[(String, RenderedPage)]) -> Result<(), String> {
    if renders.is_empty() {
        return Ok(());
    }
    let decoded = renders
        .iter()
        .map(|(_, p)| decode_rgba(&p.png))
        .collect::<Result<Vec<_>, _>>()?;
    let cell_width = decoded.iter().map(|v| v.0).max().unwrap_or(1);
    let cell_height = decoded.iter().map(|v| v.1).max().unwrap_or(1);
    let header = 20u32;
    let width = cell_width * renders.len() as u32;
    let height = header + cell_height * 2;
    let mut sheet = vec![255; width as usize * height as usize * 4];
    let ours = pad_rgba(
        &decoded[0].2,
        decoded[0].0,
        decoded[0].1,
        cell_width,
        cell_height,
    );
    for (column, ((name, _), (_, _, rgba))) in renders.iter().zip(decoded.iter()).enumerate() {
        draw_text(&mut sheet, width, column as u32 * cell_width + 4, 4, name);
        let normalized = pad_rgba(
            rgba,
            decoded[column].0,
            decoded[column].1,
            cell_width,
            cell_height,
        );
        blit(
            &mut sheet,
            width,
            &normalized,
            cell_width,
            cell_height,
            column as u32 * cell_width,
            header,
        );
        let diff = if column == 0 {
            vec![255; normalized.len()]
        } else {
            difference(&ours, &normalized)
        };
        blit(
            &mut sheet,
            width,
            &diff,
            cell_width,
            cell_height,
            column as u32 * cell_width,
            header + cell_height,
        );
    }
    let png = encode_rgba(width, height, &sheet)?;
    std::fs::write(path, png).map_err(|e| format!("write {}: {e}", path.display()))
}

fn blit(dst: &mut [u8], dst_width: u32, src: &[u8], width: u32, height: u32, x: u32, y: u32) {
    for row in 0..height as usize {
        let from = row * width as usize * 4;
        let to = ((y as usize + row) * dst_width as usize + x as usize) * 4;
        dst[to..to + width as usize * 4].copy_from_slice(&src[from..from + width as usize * 4]);
    }
}

fn difference(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.chunks_exact(4)
        .zip(b.chunks_exact(4))
        .flat_map(|(a, b)| {
            let d = a[0]
                .abs_diff(b[0])
                .max(a[1].abs_diff(b[1]))
                .max(a[2].abs_diff(b[2]));
            [255 - d, 255 - d, 255 - d, 255]
        })
        .collect()
}

fn draw_text(image: &mut [u8], width: u32, x: u32, y: u32, text: &str) {
    for (index, ch) in text.chars().enumerate() {
        let Some(glyph) = font8x8::BASIC_FONTS.get(ch) else {
            continue;
        };
        for (gy, row) in glyph.iter().enumerate() {
            for gx in 0..8 {
                if row & (1 << gx) != 0 {
                    let px = x as usize + index * 8 + gx;
                    let py = y as usize + gy;
                    if px < width as usize {
                        let i = (py * width as usize + px) * 4;
                        image[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
                    }
                }
            }
        }
    }
}

fn write_named_png(
    out: &Path,
    pdf: &Path,
    renderer: &str,
    page: u32,
    png: &[u8],
) -> Result<(), String> {
    let path = out.join(format!("{}-p{page}-{renderer}.png", safe_stem(pdf)));
    std::fs::write(&path, png).map_err(|e| format!("write {}: {e}", path.display()))
}

fn safe_stem(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn ensure_header(path: &Path) -> Result<(), String> {
    if std::fs::metadata(path).map_or(true, |m| m.len() == 0) {
        std::fs::write(path, format!("{CSV_HEADER}\n"))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    } else {
        let first =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if first.lines().next() != Some(CSV_HEADER) {
            return Err(format!(
                "{} uses the legacy PDFium-only schema; choose a new --out directory",
                path.display()
            ));
        }
    }
    Ok(())
}

fn load_done(path: &Path) -> Result<HashSet<String>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields = split_csv_prefix(line, 4)?;
            Some(format!("{}|{}|{}", fields[1], fields[2], fields[3]))
        })
        .collect())
}

fn split_csv_prefix(line: &str, count: usize) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut chars = line.chars().peekable();
    while fields.len() < count {
        let mut field = String::new();
        if chars.peek() == Some(&'\"') {
            chars.next();
            loop {
                match chars.next()? {
                    '\"' if chars.peek() == Some(&'\"') => {
                        chars.next();
                        field.push('\"');
                    }
                    '\"' => break,
                    c => field.push(c),
                }
            }
            if chars.peek() == Some(&',') {
                chars.next();
            }
        } else {
            while let Some(c) = chars.next() {
                if c == ',' {
                    break;
                }
                field.push(c);
            }
        }
        fields.push(field);
    }
    Some(fields)
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '\"', '\n', '\r']) {
        format!("\"{}\"", value.replace('\"', "\"\""))
    } else {
        value.to_string()
    }
}

fn failure_csv_row(pdf: &Path, page: u32, renderer: RendererId, note: &str) -> String {
    let mut fields = vec![
        "3".to_string(),
        csv_escape(&pdf.display().to_string()),
        page.to_string(),
        renderer.to_string(),
    ];
    // Widths and comparison metrics (columns 5–14) are unknown because the
    // reference renderer produced no raster.
    fields.resize(14, String::new());
    fields.extend([
        "0".to_string(),
        "error".to_string(),
        csv_escape(note),
        String::new(),
    ]);
    debug_assert_eq!(fields.len(), CSV_HEADER.split(',').count());
    fields.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_ranges_are_zero_based_and_deduplicated() {
        assert_eq!(parse_pages("4,0,2-4").unwrap(), vec![0, 2, 3, 4]);
        assert!(parse_pages("4-2").is_err());
    }

    #[test]
    fn padding_is_white() {
        let input = vec![1, 2, 3, 255];
        assert_eq!(
            pad_rgba(&input, 1, 1, 2, 1),
            vec![1, 2, 3, 255, 255, 255, 255, 255]
        );
    }

    #[test]
    fn csv_prefix_handles_quoted_paths() {
        let got = split_csv_prefix("2,\"a,b.pdf\",4,mupdf,1", 4).unwrap();
        assert_eq!(got, vec!["2", "a,b.pdf", "4", "mupdf"]);
    }

    #[test]
    fn result_rows_match_the_declared_schema_width() {
        let row = failure_csv_row(
            Path::new("a,b.pdf"),
            4,
            RendererId::Mupdf,
            "renderer said \"no\"",
        );
        assert_eq!(
            split_csv_prefix(&row, 18).unwrap().len(),
            CSV_HEADER.split(',').count()
        );
        assert!(row.starts_with("3,\"a,b.pdf\",4,mupdf,"));
        assert!(row.contains(",0,error,\"renderer said \"\"no\"\"\","));
    }

    #[test]
    fn benchmark_defaults_to_two_hundred_pages() {
        let args = vec!["sample.pdf".into(), "--reference".into(), "hayro".into()];
        let options = parse_options("benchmark", &args).unwrap();
        assert_eq!(options.timing_sample, Some(200));
    }

    #[test]
    fn attribution_rows_include_external_only_pixels() {
        let ours_png = encode_rgba(2, 1, &[255; 8]).unwrap();
        let reference_png = encode_rgba(2, 1, &[0, 0, 0, 255, 0, 0, 0, 255]).unwrap();
        let ours = RenderedPage {
            page: 0,
            width: 2,
            height: 1,
            png: ours_png,
            elapsed: std::time::Duration::ZERO,
        };
        let reference = RenderedPage {
            page: 0,
            width: 2,
            height: 1,
            png: reference_png,
            elapsed: std::time::Duration::ZERO,
        };
        let map = AttributionMap {
            width: 2,
            height: 1,
            leaf: vec![PaintLeaf::Image as u8, PaintLeaf::Unpainted as u8],
            origin: vec![
                PaintOrigin::FormXObject as u8,
                PaintOrigin::PageContent as u8,
            ],
            coverage_threshold: 8,
        };
        let path = std::env::temp_dir().join(format!(
            "pdfium-diff-attribution-test-{}.csv",
            std::process::id()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        write_attribution_rows(
            &mut file,
            Path::new("sample.pdf"),
            RendererId::Hayro,
            &ours,
            &reference,
            &map,
        )
        .unwrap();
        drop(file);
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!(text.lines().count(), 13);
        assert!(text.contains(",leaf,image,1,1,0.500000,"));
        assert!(text.contains(",origin,unattributed,1,1,0.500000,"));
    }
}
