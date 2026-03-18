//! Multi-page JBIG2 compression benchmark
//!
//! Measures encode performance and compression for generic, symbol, and
//! experimental symbol-dictionary modes.
//! Separates encode time from fragment writing / decode verification and disk I/O.
//!
//! Run with:
//!   cargo test --test multi_page_benchmark --features "symboldict" -- --nocapture
//!
//! Environment variables:
//!   BENCH_PAGES   — comma-separated page counts (default: "10,20,50")
//!                   example: BENCH_PAGES=1,5,10,20,50,100,all
//!   BENCH_SOURCE  — dataset folder under repo root (default: "confed")
//!   BENCH_WRITE   — set to "0" to disable writing JBIG2 fragments + decoded PBMs (default: write)
//!   BENCH_MODES   — comma-separated modes to run: `generic`, `symbol`, `sym_unify`
//!   BENCH_UNIFY_MIN_CLASS
//!   BENCH_UNIFY_MAX_ERR
//!   BENCH_UNIFY_MAX_DX
//!   BENCH_UNIFY_MAX_DY
//!   BENCH_UNIFY_CLASS_ACCEPT
//!   BENCH_UNIFY_CORE_CLOSE
//!   BENCH_UNIFY_MIN_CORE_RATIO
//!   BENCH_UNIFY_MIN_CLASS_USAGE
//!   BENCH_UNIFY_MIN_PAGE_SPAN
//!   BENCH_UNIFY_MIN_GAIN
//!   BENCH_UNIFY_BORDER_SLACK
//!   BENCH_UNIFY_SCORE_RESCUE_SLACK
//!   BENCH_UNIFY_MAX_BORDER_OUTSIDE
//!   BENCH_UNIFY_CONTEXT_MODE — `jaccard`, `cosine`, or `hybrid`

use jbig2enc_rust::jbig2enc::{EncoderMetrics, Jbig2Encoder, PdfSplitOutput};
use jbig2enc_rust::jbig2structs::{Jbig2Config, SymbolContextMode};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime};

// ── Data types ───────────────────────────────────────────────────────

struct PageInfo {
    array: ndarray::Array2<u8>,
    width: u32,
    height: u32,
    pbm_bytes: u64,
}

struct EncodeResult {
    split: PdfSplitOutput,
    encode_secs: f64,
    metrics: EncoderMetrics,
    decision_log: String,
}

struct BenchRow {
    pages: usize,
    mode: String,
    encode_secs: f64,
    ms_per_page: f64,
    mpix_per_s: f64,
    raw_jbig2_bytes: usize,
    globals_bytes: usize,
    local_dict_bytes: usize,
    text_region_bytes: usize,
    generic_region_bytes: usize,
    savings_vs_baseline: f64,
    savings_vs_pbm: f64,
    avg_page_kb: f64,
    min_page_kb: f64,
    max_page_kb: f64,
    avg_local_dict_kb: f64,
    max_local_dict_kb: f64,
    avg_text_region_kb: f64,
    avg_generic_region_kb: f64,
    max_generic_region_kb: f64,
    cc_secs: f64,
    match_secs: f64,
    cluster_secs: f64,
    planning_secs: f64,
    dict_secs: f64,
    text_secs: f64,
    generic_secs: f64,
    symbols_discovered: usize,
    symbols_exported: usize,
    avg_symbol_reuse: f64,
    global_symbol_count: usize,
    local_symbol_count: usize,
}

// ── PBM loader (straight to Array2, no BitImage intermediate) ────────

fn load_pbm_to_array(path: &Path) -> (ndarray::Array2<u8>, u32, u32) {
    let file = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("Failed to open PBM {}: {}", path.display(), e));
    let mut reader = BufReader::new(file);

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "P4", "Only raw PBM (P4) supported");

    let (width, height) = loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let dims: Vec<usize> = trimmed
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        if dims.len() == 2 {
            break (dims[0], dims[1]);
        }
    };

    let bytes_per_row = (width + 7) / 8;
    let mut packed = vec![0u8; bytes_per_row * height];
    reader.read_exact(&mut packed).unwrap();

    let array = ndarray::Array2::from_shape_fn((height, width), |(y, x)| {
        let byte = packed[y * bytes_per_row + x / 8];
        let bit = 7 - (x % 8);
        ((byte >> bit) & 1) as u8
    });

    (array, width as u32, height as u32)
}

// ── Encode helper (measures only encode, not PDF) ────────────────────

fn run_encode(cfg: &Jbig2Config, pages: &[PageInfo]) -> EncodeResult {
    let t0 = Instant::now();
    let mut enc = Jbig2Encoder::new(cfg);
    for p in pages {
        enc.add_page(&p.array).expect("add_page failed");
    }
    let split = enc.flush_pdf_split().expect("flush failed");
    EncodeResult {
        split,
        encode_secs: t0.elapsed().as_secs_f64(),
        metrics: enc.metrics_snapshot(),
        decision_log: enc.decision_debug_log(),
    }
}

// ── Raw JBIG2 stream stats ───────────────────────────────────────────

fn raw_jbig2_stats(split: &PdfSplitOutput) -> (usize, usize, Vec<f64>) {
    let globals_bytes = split.global_segments.as_ref().map_or(0, |g| g.len());
    let page_sizes_kb: Vec<f64> = split
        .page_streams
        .iter()
        .map(|s| s.len() as f64 / 1024.0)
        .collect();
    let raw_pages: usize = split.page_streams.iter().map(|s| s.len()).sum();
    (raw_pages + globals_bytes, globals_bytes, page_sizes_kb)
}

fn local_dict_stats(split: &PdfSplitOutput) -> (usize, f64, f64) {
    let total = split.local_dict_bytes_per_page.iter().sum::<usize>();
    if split.local_dict_bytes_per_page.is_empty() {
        return (total, 0.0, 0.0);
    }
    let avg = total as f64 / split.local_dict_bytes_per_page.len() as f64 / 1024.0;
    let max = split
        .local_dict_bytes_per_page
        .iter()
        .copied()
        .max()
        .unwrap_or(0) as f64
        / 1024.0;
    (total, avg, max)
}

fn per_page_bytes_stats(values: &[usize]) -> (usize, f64, f64) {
    let total = values.iter().sum::<usize>();
    if values.is_empty() {
        return (total, 0.0, 0.0);
    }
    let avg = total as f64 / values.len() as f64 / 1024.0;
    let max = values.iter().copied().max().unwrap_or(0) as f64 / 1024.0;
    (total, avg, max)
}

// ── Fragment writer / decoder ────────────────────────────────────────

fn jbig2dec_command() -> Command {
    #[cfg(windows)]
    {
        let local = Path::new(".\\jbig2dec.exe");
        if local.exists() {
            return Command::new(local);
        }
        Command::new("jbig2dec.exe")
    }
    #[cfg(not(windows))]
    {
        let local = Path::new("./jbig2dec");
        if local.exists() {
            return Command::new(local);
        }
        Command::new("jbig2dec")
    }
}

fn write_fragments_and_decode(
    split: &PdfSplitOutput,
    out_dir: &Path,
    prefix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(out_dir)?;

    let globals_path = if let Some(globals) = &split.global_segments {
        let path = out_dir.join(format!("{prefix}_globals.jb2"));
        std::fs::write(&path, globals)?;
        Some(path)
    } else {
        None
    };

    for (page_index, page_stream) in split.page_streams.iter().enumerate() {
        let page_num = page_index + 1;
        let fragment_path = out_dir.join(format!("{prefix}_page_{page_num:03}.jb2"));
        let decoded_path = out_dir.join(format!("{prefix}_page_{page_num:03}.pbm"));
        std::fs::write(&fragment_path, page_stream)?;

        let mut cmd = jbig2dec_command();
        cmd.arg("--format")
            .arg("pbm")
            .arg("--output")
            .arg(&decoded_path);

        if let Some(globals_path) = &globals_path {
            cmd.arg(globals_path).arg(&fragment_path);
        } else {
            cmd.arg("--embedded").arg(&fragment_path);
        }

        let output = cmd.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(format!(
                "jbig2dec failed for {}: {}\nstdout:\n{}\nstderr:\n{}",
                fragment_path.display(),
                output.status,
                stdout,
                stderr
            )
            .into());
        }
    }

    Ok(())
}

fn write_decision_log(out_dir: &Path, prefix: &str, log: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    std::fs::write(out_dir.join(format!("{prefix}_decision_debug.log")), log)
}

// ── Config helpers ───────────────────────────────────────────────────

fn parse_page_counts(total_pages: usize) -> Vec<usize> {
    if let Ok(val) = std::env::var("BENCH_PAGES") {
        let mut counts: Vec<usize> = val
            .split(',')
            .map(|s| s.trim())
            .map(|s| {
                if s.eq_ignore_ascii_case("all") {
                    total_pages
                } else {
                    s.parse()
                        .unwrap_or_else(|_| panic!("Invalid BENCH_PAGES value: {s}"))
                }
            })
            .collect();
        counts.sort_unstable();
        counts.dedup();
        counts.retain(|&c| c >= 1 && c <= total_pages);
        counts
    } else {
        let mut counts = vec![10, 20, 50];
        counts.retain(|&c| c <= total_pages);
        counts
    }
}

fn should_write_pdfs() -> bool {
    std::env::var("BENCH_WRITE").map_or(true, |v| v != "0")
}

fn bench_source_dir() -> (String, PathBuf) {
    let source = std::env::var("BENCH_SOURCE").unwrap_or_else(|_| "confed".to_string());
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(&source);
    (source, path)
}

fn bench_modes_filter() -> Option<Vec<String>> {
    std::env::var("BENCH_MODES").ok().map(|modes| {
        let mut selected: Vec<String> = modes
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let needs_symbol_control = selected.iter().any(|mode| mode == "sym_unify");
        if needs_symbol_control && !selected.iter().any(|mode| mode == "symbol") {
            selected.insert(0, "symbol".to_string());
        }
        selected
    })
}

fn mode_enabled(filter: Option<&[String]>, mode: &str) -> bool {
    filter.is_none_or(|modes| modes.iter().any(|m| m == mode))
}

fn env_parse_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(default)
}

fn unify_context_mode() -> SymbolContextMode {
    std::env::var("BENCH_UNIFY_CONTEXT_MODE")
        .ok()
        .and_then(|value| match value.to_ascii_lowercase().as_str() {
            "jaccard" => Some(SymbolContextMode::WeightedJaccard),
            "cosine" => Some(SymbolContextMode::Cosine),
            "hybrid" => Some(SymbolContextMode::Hybrid),
            _ => None,
        })
        .unwrap_or(SymbolContextMode::Hybrid)
}

fn configs_for_count(count: usize, total_pixels: u64) -> Vec<(&'static str, Jbig2Config)> {
    let mut cfgs = vec![];

    let mut cfg_symbol = Jbig2Config::text();
    cfg_symbol.auto_thresh = false;
    cfg_symbol.want_full_headers = false;
    cfgs.push(("symbol", cfg_symbol));

    let mut cfg_unify = Jbig2Config::text_symbol_unify();
    cfg_unify.auto_thresh = false;
    cfg_unify.want_full_headers = false;
    cfg_unify.sym_unify_min_class_size =
        env_parse_or("BENCH_UNIFY_MIN_CLASS", cfg_unify.sym_unify_min_class_size);
    cfg_unify.sym_unify_max_err = env_parse_or("BENCH_UNIFY_MAX_ERR", cfg_unify.sym_unify_max_err);
    cfg_unify.sym_unify_max_dx = env_parse_or("BENCH_UNIFY_MAX_DX", cfg_unify.sym_unify_max_dx);
    cfg_unify.sym_unify_max_dy = env_parse_or("BENCH_UNIFY_MAX_DY", cfg_unify.sym_unify_max_dy);
    cfg_unify.sym_unify_class_accept_limit = env_parse_or(
        "BENCH_UNIFY_CLASS_ACCEPT",
        cfg_unify.sym_unify_class_accept_limit,
    );
    cfg_unify.sym_unify_core_close_threshold = env_parse_or(
        "BENCH_UNIFY_CORE_CLOSE",
        cfg_unify.sym_unify_core_close_threshold,
    );
    cfg_unify.sym_unify_min_core_ratio_permille = env_parse_or(
        "BENCH_UNIFY_MIN_CORE_RATIO",
        cfg_unify.sym_unify_min_core_ratio_permille,
    );
    cfg_unify.sym_unify_min_class_usage = env_parse_or(
        "BENCH_UNIFY_MIN_CLASS_USAGE",
        cfg_unify.sym_unify_min_class_usage,
    );
    cfg_unify.sym_unify_min_page_span = env_parse_or(
        "BENCH_UNIFY_MIN_PAGE_SPAN",
        cfg_unify.sym_unify_min_page_span,
    );
    cfg_unify.sym_unify_min_estimated_gain = env_parse_or(
        "BENCH_UNIFY_MIN_GAIN",
        cfg_unify.sym_unify_min_estimated_gain,
    );
    cfg_unify.sym_unify_border_score_slack = env_parse_or(
        "BENCH_UNIFY_BORDER_SLACK",
        cfg_unify.sym_unify_border_score_slack,
    );
    cfg_unify.sym_unify_score_rescue_slack = env_parse_or(
        "BENCH_UNIFY_SCORE_RESCUE_SLACK",
        cfg_unify.sym_unify_score_rescue_slack,
    );
    cfg_unify.sym_unify_max_border_outside_ink = env_parse_or(
        "BENCH_UNIFY_MAX_BORDER_OUTSIDE",
        cfg_unify.sym_unify_max_border_outside_ink,
    );
    cfg_unify.sym_unify_context_mode = unify_context_mode();
    cfgs.push(("sym_unify", cfg_unify));

    let _ = (count, total_pixels);

    cfgs
}

// ── Main benchmark ───────────────────────────────────────────────────

#[test]
fn multi_page_compression_benchmark() {
    let (source_name, source_dir) = bench_source_dir();
    assert!(source_dir.exists(), "{source_name}/ directory not found");

    let mut pbm_files: Vec<PathBuf> = std::fs::read_dir(&source_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "pbm"))
        .collect();
    pbm_files.sort();

    let total_pages = pbm_files.len();
    assert!(
        total_pages >= 10,
        "Need at least 10 PBM pages, found {}",
        total_pages
    );

    // ── Load dataset ─────────────────────────────────────────────
    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║      Multi-Page JBIG2 Compression Benchmark                ║");
    println!("╚══════════════════════════════════════════════════════════════╝\n");

    println!("Loading {} PBM files...", total_pages);
    let load_start = Instant::now();
    let all_pages: Vec<PageInfo> = pbm_files
        .iter()
        .map(|p| {
            let pbm_bytes = std::fs::metadata(p).unwrap().len();
            let (array, w, h) = load_pbm_to_array(p);
            PageInfo {
                array,
                width: w,
                height: h,
                pbm_bytes,
            }
        })
        .collect();
    println!("Loaded in {:.1}s\n", load_start.elapsed().as_secs_f64());

    let page_counts = parse_page_counts(total_pages);
    let write_outputs = should_write_pdfs();
    let mode_filter = bench_modes_filter();

    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test_output_fragments")
        .join(format!("benchmark_{ts}"));
    if write_outputs {
        std::fs::create_dir_all(&out_dir).unwrap();
    }

    println!("Source: {source_name}/ ({total_pages} pages)  |  Write fragments: {write_outputs}");
    if let Some(filter) = &mode_filter {
        println!("Modes: {:?}", filter);
    }
    println!("Page counts: {:?}\n", page_counts);

    // ── Warmup ───────────────────────────────────────────────────
    {
        let n = std::cmp::min(3, all_pages.len());
        let mut warm_cfg = Jbig2Config::lossless();
        warm_cfg.want_full_headers = false;
        let mut warm = Jbig2Encoder::new(&warm_cfg);
        for p in &all_pages[..n] {
            let _ = warm.add_page(&p.array);
        }
        let _ = warm.flush_pdf_split();
    }

    // ── Table header ─────────────────────────────────────────────
    println!(
        "{:<6} {:<10} {:>10} {:>10} {:>8} {:>10} {:>8} {:>8} {:>8}",
        "Pages",
        "Mode",
        "Raw KB",
        "Globals",
        if mode_enabled(mode_filter.as_deref(), "generic") {
            "vs Gen"
        } else if mode_enabled(mode_filter.as_deref(), "symbol") {
            "vs Sym"
        } else {
            "vs Base"
        },
        "Enc (s)",
        "ms/pg",
        "MPix/s",
        "vs PBM"
    );
    println!("{}", "─".repeat(92));

    let mut all_rows: Vec<BenchRow> = Vec::new();

    for &count in &page_counts {
        let pages = &all_pages[..count];
        let total_pixels: u64 = pages.iter().map(|p| p.width as u64 * p.height as u64).sum();
        let total_pbm_bytes: u64 = pages.iter().map(|p| p.pbm_bytes).sum();
        let run_generic = mode_enabled(mode_filter.as_deref(), "generic");
        let run_symbol = mode_enabled(mode_filter.as_deref(), "symbol");
        let mut gen_raw = 0usize;
        let mut symbol_raw = None::<usize>;

        // ── Generic baseline ─────────────────────────────────────
        if run_generic {
            let mut cfg_generic = Jbig2Config::lossless();
            cfg_generic.want_full_headers = false;
            let gen_result = run_encode(&cfg_generic, pages);

            assert_eq!(
                gen_result.split.page_streams.len(),
                pages.len(),
                "generic: wrong page stream count"
            );

            let (raw, gen_globals, gen_page_kb) = raw_jbig2_stats(&gen_result.split);
            gen_raw = raw;
            if write_outputs {
                write_fragments_and_decode(
                    &gen_result.split,
                    &out_dir.join(format!("generic_{count}p")),
                    &format!("generic_{count}p"),
                )
                .expect("write/decode generic fragments failed");
                write_decision_log(
                    &out_dir.join(format!("generic_{count}p")),
                    &format!("generic_{count}p"),
                    &gen_result.decision_log,
                )
                .expect("write generic decision log failed");
            }

            let gen_avg_page = gen_page_kb.iter().sum::<f64>() / gen_page_kb.len() as f64;
            let gen_min_page = gen_page_kb.iter().cloned().fold(f64::MAX, f64::min);
            let gen_max_page = gen_page_kb.iter().cloned().fold(0.0_f64, f64::max);
            let gen_ms = (gen_result.encode_secs * 1000.0) / count as f64;
            let gen_mpix = (total_pixels as f64 / 1_000_000.0) / gen_result.encode_secs;
            let gen_vs_pbm = (1.0 - gen_raw as f64 / total_pbm_bytes as f64) * 100.0;

            let gen_row = BenchRow {
                pages: count,
                mode: "generic".into(),
                encode_secs: gen_result.encode_secs,
                ms_per_page: gen_ms,
                mpix_per_s: gen_mpix,
                raw_jbig2_bytes: gen_raw,
                globals_bytes: gen_globals,
                local_dict_bytes: 0,
                text_region_bytes: 0,
                generic_region_bytes: raw,
                savings_vs_baseline: 0.0,
                savings_vs_pbm: gen_vs_pbm,
                avg_page_kb: gen_avg_page,
                min_page_kb: gen_min_page,
                max_page_kb: gen_max_page,
                avg_local_dict_kb: 0.0,
                max_local_dict_kb: 0.0,
                avg_text_region_kb: 0.0,
                avg_generic_region_kb: gen_avg_page,
                max_generic_region_kb: gen_max_page,
                cc_secs: gen_result.metrics.symbol_mode.cc_extraction.as_secs_f64(),
                match_secs: gen_result.metrics.symbol_mode.matching_dedup.as_secs_f64(),
                cluster_secs: gen_result.metrics.symbol_mode.clustering.as_secs_f64(),
                planning_secs: gen_result.metrics.symbol_mode.planning.as_secs_f64(),
                dict_secs: gen_result
                    .metrics
                    .symbol_mode
                    .symbol_dict_encoding
                    .as_secs_f64(),
                text_secs: gen_result
                    .metrics
                    .symbol_mode
                    .text_region_encoding
                    .as_secs_f64(),
                generic_secs: gen_result
                    .metrics
                    .symbol_mode
                    .generic_region_encoding
                    .as_secs_f64(),
                symbols_discovered: gen_result.metrics.symbol_stats.symbols_discovered,
                symbols_exported: gen_result.metrics.symbol_stats.symbols_exported,
                avg_symbol_reuse: gen_result.metrics.symbol_stats.avg_symbol_reuse,
                global_symbol_count: gen_result.metrics.symbol_stats.global_symbol_count,
                local_symbol_count: gen_result.metrics.symbol_stats.local_symbol_count,
            };

            println!(
                "{:<6} {:<10} {:>9.1} {:>9.1} {:>7}  {:>9.2} {:>7.1} {:>7.1} {:>7.1}%",
                count,
                "generic",
                gen_raw as f64 / 1024.0,
                gen_globals as f64 / 1024.0,
                "—",
                gen_result.encode_secs,
                gen_ms,
                gen_mpix,
                gen_vs_pbm,
            );
            all_rows.push(gen_row);
        }

        // ── Symbol modes ─────────────────────────────────────────
        let configs = configs_for_count(count, total_pixels)
            .into_iter()
            .filter(|(label, _)| mode_enabled(mode_filter.as_deref(), label))
            .collect::<Vec<_>>();
        for (label, cfg) in &configs {
            let result = run_encode(cfg, pages);

            assert_eq!(
                result.split.page_streams.len(),
                pages.len(),
                "{label}: wrong page stream count"
            );

            let (raw_total, globals_bytes, page_kb) = raw_jbig2_stats(&result.split);
            let (local_dict_bytes, avg_local_dict_kb, max_local_dict_kb) =
                local_dict_stats(&result.split);
            let (text_region_bytes, avg_text_region_kb, _) =
                per_page_bytes_stats(&result.split.text_region_bytes_per_page);
            let (generic_region_bytes, avg_generic_region_kb, max_generic_region_kb) =
                per_page_bytes_stats(&result.split.generic_region_bytes_per_page);
            if write_outputs {
                write_fragments_and_decode(
                    &result.split,
                    &out_dir.join(format!("{label}_{count}p")),
                    &format!("{label}_{count}p"),
                )
                .unwrap_or_else(|e| panic!("write/decode {label} fragments failed: {e}"));
                write_decision_log(
                    &out_dir.join(format!("{label}_{count}p")),
                    &format!("{label}_{count}p"),
                    &result.decision_log,
                )
                .unwrap_or_else(|e| panic!("write {label} decision log failed: {e}"));
            }

            let savings_baseline = if run_generic {
                (1.0 - raw_total as f64 / gen_raw as f64) * 100.0
            } else if run_symbol && *label != "symbol" {
                symbol_raw
                    .map(|base| (1.0 - raw_total as f64 / base as f64) * 100.0)
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            let savings_pbm = (1.0 - raw_total as f64 / total_pbm_bytes as f64) * 100.0;
            let ms = (result.encode_secs * 1000.0) / count as f64;
            let mpix = (total_pixels as f64 / 1_000_000.0) / result.encode_secs;
            let avg_page = page_kb.iter().sum::<f64>() / page_kb.len() as f64;
            let min_page = page_kb.iter().cloned().fold(f64::MAX, f64::min);
            let max_page = page_kb.iter().cloned().fold(0.0_f64, f64::max);

            println!(
                "{:<6} {:<10} {:>9.1} {:>9.1} {:>6.1}%  {:>9.2} {:>7.1} {:>7.1} {:>7.1}%",
                count,
                label,
                raw_total as f64 / 1024.0,
                globals_bytes as f64 / 1024.0,
                savings_baseline,
                result.encode_secs,
                ms,
                mpix,
                savings_pbm,
            );

            all_rows.push(BenchRow {
                pages: count,
                mode: label.to_string(),
                encode_secs: result.encode_secs,
                ms_per_page: ms,
                mpix_per_s: mpix,
                raw_jbig2_bytes: raw_total,
                globals_bytes,
                local_dict_bytes,
                text_region_bytes,
                generic_region_bytes,
                savings_vs_baseline: savings_baseline,
                savings_vs_pbm: savings_pbm,
                avg_page_kb: avg_page,
                min_page_kb: min_page,
                max_page_kb: max_page,
                avg_local_dict_kb,
                max_local_dict_kb,
                avg_text_region_kb,
                avg_generic_region_kb,
                max_generic_region_kb,
                cc_secs: result.metrics.symbol_mode.cc_extraction.as_secs_f64(),
                match_secs: result.metrics.symbol_mode.matching_dedup.as_secs_f64(),
                cluster_secs: result.metrics.symbol_mode.clustering.as_secs_f64(),
                planning_secs: result.metrics.symbol_mode.planning.as_secs_f64(),
                dict_secs: result
                    .metrics
                    .symbol_mode
                    .symbol_dict_encoding
                    .as_secs_f64(),
                text_secs: result
                    .metrics
                    .symbol_mode
                    .text_region_encoding
                    .as_secs_f64(),
                generic_secs: result
                    .metrics
                    .symbol_mode
                    .generic_region_encoding
                    .as_secs_f64(),
                symbols_discovered: result.metrics.symbol_stats.symbols_discovered,
                symbols_exported: result.metrics.symbol_stats.symbols_exported,
                avg_symbol_reuse: result.metrics.symbol_stats.avg_symbol_reuse,
                global_symbol_count: result.metrics.symbol_stats.global_symbol_count,
                local_symbol_count: result.metrics.symbol_stats.local_symbol_count,
            });

            if *label == "symbol" {
                symbol_raw = Some(raw_total);
            }
        }
        println!();
    }

    // ── Summary detail ───────────────────────────────────────────
    println!("─── Page stream detail ───");
    for row in &all_rows {
        if row.mode != "generic" {
            println!(
                "  {} {}p: globals {:.1}KB, local dicts {:.1}KB avg {:.2}KB/page max {:.2}KB, text {:.1}KB avg {:.2}KB/page, generic {:.1}KB avg {:.2}KB/page max {:.2}KB, pages avg {:.1}KB min {:.1}KB max {:.1}KB",
                row.mode,
                row.pages,
                row.globals_bytes as f64 / 1024.0,
                row.local_dict_bytes as f64 / 1024.0,
                row.avg_local_dict_kb,
                row.max_local_dict_kb,
                row.text_region_bytes as f64 / 1024.0,
                row.avg_text_region_kb,
                row.generic_region_bytes as f64 / 1024.0,
                row.avg_generic_region_kb,
                row.max_generic_region_kb,
                row.avg_page_kb,
                row.min_page_kb,
                row.max_page_kb
            );
            println!(
                "    stages cc {:.2}s match {:.2}s cluster {:.2}s plan {:.2}s dict {:.2}s text {:.2}s generic {:.2}s | symbols discovered {} exported {} reuse {:.2} global {} local {}",
                row.cc_secs,
                row.match_secs,
                row.cluster_secs,
                row.planning_secs,
                row.dict_secs,
                row.text_secs,
                row.generic_secs,
                row.symbols_discovered,
                row.symbols_exported,
                row.avg_symbol_reuse,
                row.global_symbol_count,
                row.local_symbol_count,
            );
        }
    }
    println!();

    // ── CSV output ───────────────────────────────────────────────
    if write_outputs {
        let csv_path = out_dir.join("results.csv");
        let mut csv = String::from(
            "pages,mode,encode_secs,ms_per_page,mpix_per_s,raw_jbig2_bytes,globals_bytes,local_dict_bytes,text_region_bytes,generic_region_bytes,savings_vs_baseline,savings_vs_pbm,cc_secs,match_secs,cluster_secs,planning_secs,dict_secs,text_secs,generic_secs,symbols_discovered,symbols_exported,avg_symbol_reuse,global_symbol_count,local_symbol_count,avg_local_dict_kb,max_local_dict_kb,avg_text_region_kb,avg_generic_region_kb,max_generic_region_kb\n",
        );
        for row in &all_rows {
            csv += &format!(
                "{},{},{:.4},{:.2},{:.2},{},{},{},{},{},{:.2},{:.2},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{},{},{:.4},{},{},{:.4},{:.4},{:.4},{:.4},{:.4}\n",
                row.pages,
                row.mode,
                row.encode_secs,
                row.ms_per_page,
                row.mpix_per_s,
                row.raw_jbig2_bytes,
                row.globals_bytes,
                row.local_dict_bytes,
                row.text_region_bytes,
                row.generic_region_bytes,
                row.savings_vs_baseline,
                row.savings_vs_pbm,
                row.cc_secs,
                row.match_secs,
                row.cluster_secs,
                row.planning_secs,
                row.dict_secs,
                row.text_secs,
                row.generic_secs,
                row.symbols_discovered,
                row.symbols_exported,
                row.avg_symbol_reuse,
                row.global_symbol_count,
                row.local_symbol_count,
                row.avg_local_dict_kb,
                row.max_local_dict_kb,
                row.avg_text_region_kb,
                row.avg_generic_region_kb,
                row.max_generic_region_kb
            );
        }
        std::fs::write(&csv_path, csv).unwrap();
        println!(
            "Fragments + decoded PBMs + CSV written to: {}",
            out_dir.display()
        );
    } else {
        println!("(set BENCH_WRITE=1 to write JBIG2 fragments, decoded PBMs, and CSV to disk)");
    }
}
