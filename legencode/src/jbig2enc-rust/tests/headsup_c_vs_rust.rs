#![cfg(feature = "c-encoder-bench")]

use jbig2enc_rust::jbig2enc::Jbig2Encoder;
use jbig2enc_rust::jbig2structs::Jbig2Config;
use jbig2enc_rust::jbig2sym::BitImage;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime};

const DEFAULT_DPI: i32 = 300;
const C_SYMBOL_THRESH: f32 = 0.85;
const C_SYMBOL_WEIGHT: f32 = 0.5;

unsafe extern "C" {
    fn jb2_bridge_version() -> *const std::ffi::c_char;
    fn jb2_bridge_pix_from_packed_1bpp(
        data: *const u8,
        width: i32,
        height: i32,
        stride_bytes: i32,
        xres: i32,
        yres: i32,
    ) -> *mut c_void;
    fn jb2_bridge_pix_destroy(pix: *mut c_void);
    fn jb2_bridge_init(
        thresh: f32,
        weight: f32,
        xres: i32,
        yres: i32,
        full_headers: bool,
        refine_level: i32,
    ) -> *mut c_void;
    fn jb2_bridge_destroy(ctx: *mut c_void);
    fn jb2_bridge_add_page(ctx: *mut c_void, pix: *mut c_void);
    fn jb2_bridge_pages_complete(ctx: *mut c_void, length: *mut i32, verbose: bool) -> *mut u8;
    fn jb2_bridge_produce_page(
        ctx: *mut c_void,
        page_no: i32,
        xres: i32,
        yres: i32,
        length: *mut i32,
    ) -> *mut u8;
    fn jb2_bridge_encode_generic(
        pix: *mut c_void,
        full_headers: bool,
        xres: i32,
        yres: i32,
        duplicate_line_removal: bool,
        length: *mut i32,
    ) -> *mut u8;
    fn jb2_bridge_free_buffer(buffer: *mut u8);
}

#[derive(Clone)]
struct RawSplitOutput {
    global_segments: Option<Vec<u8>>,
    page_streams: Vec<Vec<u8>>,
}

struct PreparedPage {
    bitimage: BitImage,
    c_pix: *mut c_void,
    width: u32,
    height: u32,
    pbm_bytes: u64,
}

impl Drop for PreparedPage {
    fn drop(&mut self) {
        if !self.c_pix.is_null() {
            unsafe { jb2_bridge_pix_destroy(self.c_pix) };
            self.c_pix = std::ptr::null_mut();
        }
    }
}

struct EncodeResult {
    split: RawSplitOutput,
    encode_secs: f64,
}

struct BenchRow {
    pages: usize,
    implementation: &'static str,
    mode: &'static str,
    family: &'static str,
    raw_jbig2_bytes: usize,
    globals_bytes: usize,
    encode_secs: f64,
    ms_per_page: f64,
    mpix_per_s: f64,
    savings_vs_pbm: f64,
}

fn load_pbm_to_bitimage_and_bytes(path: &Path) -> (BitImage, Vec<u8>, u32, u32) {
    let file =
        fs::File::open(path).unwrap_or_else(|e| panic!("Failed to open {}: {e}", path.display()));
    let mut reader = BufReader::new(file);

    let mut magic = String::new();
    reader.read_line(&mut magic).unwrap();
    assert_eq!(magic.trim(), "P4", "Only raw PBM (P4) is supported");

    let (width, height) = loop {
        let mut line = String::new();
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

    let bytes_per_row = width.div_ceil(8);
    let mut pbm_body = vec![0u8; bytes_per_row * height];
    reader.read_exact(&mut pbm_body).unwrap();

    let mut image = BitImage::new(width as u32, height as u32).unwrap();
    for y in 0..height {
        for x in 0..width {
            let byte = pbm_body[y * bytes_per_row + x / 8];
            let bit = 7 - (x % 8);
            if ((byte >> bit) & 1) != 0 {
                image.set_usize(x, y, true);
            }
        }
    }

    (image, pbm_body, width as u32, height as u32)
}

fn parse_page_counts(total_pages: usize) -> Vec<usize> {
    if let Ok(val) = std::env::var("HEADSUP_PAGES") {
        let mut counts: Vec<usize> = val
            .split(',')
            .map(|s| s.trim())
            .map(|s| {
                if s.eq_ignore_ascii_case("all") {
                    total_pages
                } else {
                    s.parse()
                        .unwrap_or_else(|_| panic!("Invalid HEADSUP_PAGES value: {s}"))
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

fn should_write_outputs() -> bool {
    std::env::var("HEADSUP_WRITE").map_or(false, |v| v != "0")
}

#[cfg(feature = "symboldict")]
fn include_sym_unify() -> bool {
    std::env::var("HEADSUP_INCLUDE_UNIFY").map_or(true, |v| v != "0")
}

fn headsup_source_dir() -> (String, PathBuf) {
    let source = std::env::var("HEADSUP_SOURCE").unwrap_or_else(|_| "sahib".to_string());
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(&source);
    (source, path)
}

fn raw_jbig2_stats(split: &RawSplitOutput) -> (usize, usize, Vec<f64>) {
    let globals_bytes = split.global_segments.as_ref().map_or(0, |g| g.len());
    let page_sizes_kb: Vec<f64> = split
        .page_streams
        .iter()
        .map(|s| s.len() as f64 / 1024.0)
        .collect();
    let raw_pages: usize = split.page_streams.iter().map(|s| s.len()).sum();
    (raw_pages + globals_bytes, globals_bytes, page_sizes_kb)
}

fn jbig2dec_command() -> Command {
    #[cfg(windows)]
    {
        Command::new("jbig2dec.exe")
    }
    #[cfg(not(windows))]
    {
        Command::new("jbig2dec")
    }
}

fn write_fragments_and_decode(
    split: &RawSplitOutput,
    out_dir: &Path,
    prefix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(out_dir)?;

    let globals_path = if let Some(globals) = &split.global_segments {
        let path = out_dir.join(format!("{prefix}_globals.jb2"));
        fs::write(&path, globals)?;
        Some(path)
    } else {
        None
    };

    for (page_index, page_stream) in split.page_streams.iter().enumerate() {
        let page_num = page_index + 1;
        let fragment_path = out_dir.join(format!("{prefix}_page_{page_num:03}.jb2"));
        let decoded_path = out_dir.join(format!("{prefix}_page_{page_num:03}.pbm"));
        fs::write(&fragment_path, page_stream)?;

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
            return Err(format!(
                "jbig2dec failed for {}: {}\n{}",
                fragment_path.display(),
                output.status,
                stderr
            )
            .into());
        }
    }

    Ok(())
}

fn write_summary_markdown(
    out_dir: &Path,
    source_name: &str,
    rows: &[BenchRow],
) -> std::io::Result<()> {
    let mut out = String::new();
    out.push_str("# Heads-Up Benchmark\n\n");
    out.push_str(&format!(
        "Dataset: `{}`. Basis: same preloaded PBM pages, in-process encoding only, no subprocess encoding overhead.\n\n",
        source_name
    ));
    out.push_str(
        "| Pages | Impl | Mode | Raw KB | Globals KB | Enc s | ms/pg | MPix/s | vs PBM |\n",
    );
    out.push_str("| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for row in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {:.1} | {:.1} | {:.2} | {:.1} | {:.1} | {:.1}% |\n",
            row.pages,
            row.implementation,
            row.mode,
            row.raw_jbig2_bytes as f64 / 1024.0,
            row.globals_bytes as f64 / 1024.0,
            row.encode_secs,
            row.ms_per_page,
            row.mpix_per_s,
            row.savings_vs_pbm
        ));
    }
    fs::write(out_dir.join("HEADSUP_RESULTS.md"), out)
}

fn ffi_take_buffer(ptr: *mut u8, len: i32) -> Vec<u8> {
    assert!(!ptr.is_null(), "C encoder returned a null buffer");
    assert!(len >= 0, "C encoder returned a negative length");
    unsafe {
        let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        jb2_bridge_free_buffer(ptr);
        bytes
    }
}

fn run_rust_encode(cfg: &Jbig2Config, pages: &[PreparedPage]) -> EncodeResult {
    let start = Instant::now();
    let mut enc = Jbig2Encoder::new(cfg);
    for page in pages {
        enc.add_page_bitimage(page.bitimage.clone())
            .expect("Rust add_page_bitimage failed");
    }
    let split = enc.flush_pdf_split().expect("Rust flush failed");
    EncodeResult {
        split: RawSplitOutput {
            global_segments: split.global_segments,
            page_streams: split.page_streams,
        },
        encode_secs: start.elapsed().as_secs_f64(),
    }
}

fn run_c_symbol_encode(pages: &[PreparedPage]) -> EncodeResult {
    let start = Instant::now();
    let ctx = unsafe {
        jb2_bridge_init(
            C_SYMBOL_THRESH,
            C_SYMBOL_WEIGHT,
            DEFAULT_DPI,
            DEFAULT_DPI,
            false,
            -1,
        )
    };
    assert!(!ctx.is_null(), "C jbig2_init failed");

    let split = (|| {
        for page in pages {
            unsafe { jb2_bridge_add_page(ctx, page.c_pix) };
        }

        let mut global_len = 0i32;
        let global_ptr = unsafe { jb2_bridge_pages_complete(ctx, &mut global_len, false) };
        let global_segments = Some(ffi_take_buffer(global_ptr, global_len));

        let mut page_streams = Vec::with_capacity(pages.len());
        for page_no in 0..pages.len() {
            let mut page_len = 0i32;
            let page_ptr =
                unsafe { jb2_bridge_produce_page(ctx, page_no as i32, -1, -1, &mut page_len) };
            page_streams.push(ffi_take_buffer(page_ptr, page_len));
        }

        RawSplitOutput {
            global_segments,
            page_streams,
        }
    })();

    unsafe { jb2_bridge_destroy(ctx) };

    EncodeResult {
        split,
        encode_secs: start.elapsed().as_secs_f64(),
    }
}

fn run_c_generic_encode(pages: &[PreparedPage]) -> EncodeResult {
    let start = Instant::now();
    let mut page_streams = Vec::with_capacity(pages.len());
    for page in pages {
        let mut len = 0i32;
        let ptr = unsafe {
            jb2_bridge_encode_generic(page.c_pix, false, DEFAULT_DPI, DEFAULT_DPI, false, &mut len)
        };
        page_streams.push(ffi_take_buffer(ptr, len));
    }

    EncodeResult {
        split: RawSplitOutput {
            global_segments: None,
            page_streams,
        },
        encode_secs: start.elapsed().as_secs_f64(),
    }
}

fn push_row(
    rows: &mut Vec<BenchRow>,
    pages: &[PreparedPage],
    page_count: usize,
    implementation: &'static str,
    mode: &'static str,
    family: &'static str,
    result: EncodeResult,
) {
    let total_pixels: u64 = pages.iter().map(|p| p.width as u64 * p.height as u64).sum();
    let total_pbm_bytes: u64 = pages.iter().map(|p| p.pbm_bytes).sum();
    let (raw, globals, _) = raw_jbig2_stats(&result.split);
    rows.push(BenchRow {
        pages: page_count,
        implementation,
        mode,
        family,
        raw_jbig2_bytes: raw,
        globals_bytes: globals,
        encode_secs: result.encode_secs,
        ms_per_page: result.encode_secs * 1000.0 / page_count as f64,
        mpix_per_s: if result.encode_secs > 0.0 {
            total_pixels as f64 / 1_000_000.0 / result.encode_secs
        } else {
            0.0
        },
        savings_vs_pbm: 100.0 - (raw as f64 / total_pbm_bytes as f64 * 100.0),
    });
}

#[test]
fn headsup_c_vs_rust_benchmark() {
    let c_version = unsafe {
        let ptr = jb2_bridge_version();
        if ptr.is_null() {
            "<unknown>".to_string()
        } else {
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };

    let (source_name, source_dir) = headsup_source_dir();
    assert!(
        source_dir.exists(),
        "{} directory not found",
        source_dir.display()
    );

    let mut pbm_files: Vec<PathBuf> = fs::read_dir(&source_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pbm"))
        .collect();
    pbm_files.sort();
    assert!(
        !pbm_files.is_empty(),
        "No PBM files found in {}",
        source_dir.display()
    );

    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║      JBIG2Enc vs jbig2enc-rust Heads-Up                    ║");
    println!("╚══════════════════════════════════════════════════════════════╝\n");
    println!("Source: {source_name}/");
    println!("Basis: same preloaded PBM pages, in-process encode only");
    println!("C encoder version: {c_version}");

    let load_start = Instant::now();
    let all_pages: Vec<PreparedPage> = pbm_files
        .iter()
        .map(|path| {
            let pbm_bytes = fs::metadata(path).unwrap().len();
            let (bitimage, pbm_body, width, height) = load_pbm_to_bitimage_and_bytes(path);
            let stride_bytes = width.div_ceil(8);
            let c_pix = unsafe {
                jb2_bridge_pix_from_packed_1bpp(
                    pbm_body.as_ptr(),
                    width as i32,
                    height as i32,
                    stride_bytes as i32,
                    DEFAULT_DPI,
                    DEFAULT_DPI,
                )
            };
            assert!(
                !c_pix.is_null(),
                "Failed to create C PIX from {}",
                path.display()
            );
            PreparedPage {
                bitimage,
                c_pix,
                width,
                height,
                pbm_bytes,
            }
        })
        .collect();
    println!(
        "Prepared {} pages in {:.1}s\n",
        all_pages.len(),
        load_start.elapsed().as_secs_f64()
    );

    let page_counts = parse_page_counts(all_pages.len());
    let write_outputs = should_write_outputs();

    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test_output_fragments")
        .join(format!("headsup_{ts}"));
    if write_outputs {
        fs::create_dir_all(&out_dir).unwrap();
    }

    println!(
        "{:<6} {:<6} {:<10} {:>10} {:>10} {:>10} {:>8} {:>8} {:>8}",
        "Pages", "Impl", "Mode", "Raw KB", "Globals", "vs peer", "Enc (s)", "ms/pg", "MPix/s"
    );
    println!("{}", "─".repeat(92));

    let mut all_rows = Vec::new();
    let mut split_outputs: HashMap<(usize, &'static str, &'static str), RawSplitOutput> =
        HashMap::new();

    for &count in &page_counts {
        let pages = &all_pages[..count];

        let mut rust_generic = Jbig2Config::lossless();
        rust_generic.want_full_headers = false;
        let rust_generic_result = run_rust_encode(&rust_generic, pages);
        split_outputs.insert(
            (count, "rust", "generic"),
            rust_generic_result.split.clone(),
        );
        push_row(
            &mut all_rows,
            pages,
            count,
            "rust",
            "generic",
            "generic",
            rust_generic_result,
        );

        let c_generic_result = run_c_generic_encode(pages);
        split_outputs.insert((count, "c", "generic"), c_generic_result.split.clone());
        push_row(
            &mut all_rows,
            pages,
            count,
            "c",
            "generic",
            "generic",
            c_generic_result,
        );

        let mut rust_symbol = Jbig2Config::text();
        rust_symbol.auto_thresh = false;
        rust_symbol.want_full_headers = false;
        let rust_symbol_result = run_rust_encode(&rust_symbol, pages);
        split_outputs.insert((count, "rust", "symbol"), rust_symbol_result.split.clone());
        push_row(
            &mut all_rows,
            pages,
            count,
            "rust",
            "symbol",
            "symbol",
            rust_symbol_result,
        );

        let c_symbol_result = run_c_symbol_encode(pages);
        split_outputs.insert((count, "c", "symbol"), c_symbol_result.split.clone());
        push_row(
            &mut all_rows,
            pages,
            count,
            "c",
            "symbol",
            "symbol",
            c_symbol_result,
        );

        #[cfg(feature = "symboldict")]
        if include_sym_unify() {
            let mut rust_unify = Jbig2Config::text_symbol_unify();
            rust_unify.auto_thresh = false;
            rust_unify.want_full_headers = false;
            let rust_unify_result = run_rust_encode(&rust_unify, pages);
            split_outputs.insert(
                (count, "rust", "sym_unify"),
                rust_unify_result.split.clone(),
            );
            push_row(
                &mut all_rows,
                pages,
                count,
                "rust",
                "sym_unify",
                "symbol",
                rust_unify_result,
            );
        }
    }

    all_rows.sort_by(|lhs, rhs| {
        lhs.pages
            .cmp(&rhs.pages)
            .then_with(|| lhs.family.cmp(rhs.family))
            .then_with(|| lhs.implementation.cmp(rhs.implementation))
            .then_with(|| lhs.mode.cmp(rhs.mode))
    });

    let mut peer_sizes = HashMap::new();
    for row in &all_rows {
        peer_sizes.insert(
            (row.pages, row.implementation, row.mode),
            row.raw_jbig2_bytes,
        );
    }

    for row in &all_rows {
        let peer = match (row.implementation, row.family) {
            ("rust", "generic") => peer_sizes.get(&(row.pages, "c", "generic")),
            ("c", "generic") => peer_sizes.get(&(row.pages, "rust", "generic")),
            ("rust", "symbol") => peer_sizes.get(&(row.pages, "c", "symbol")),
            ("c", "symbol") => peer_sizes.get(&(row.pages, "rust", "symbol")),
            ("rust", _) if row.mode == "sym_unify" => peer_sizes.get(&(row.pages, "c", "symbol")),
            _ => None,
        };
        let vs_peer = peer.map_or(String::from("   n/a"), |peer_bytes| {
            let pct = 100.0 - (row.raw_jbig2_bytes as f64 / *peer_bytes as f64 * 100.0);
            format!("{pct:>+6.1}%")
        });
        println!(
            "{:<6} {:<6} {:<10} {:>10.1} {:>10.1} {:>10} {:>8.2} {:>8.1} {:>8.1}",
            row.pages,
            row.implementation,
            row.mode,
            row.raw_jbig2_bytes as f64 / 1024.0,
            row.globals_bytes as f64 / 1024.0,
            vs_peer,
            row.encode_secs,
            row.ms_per_page,
            row.mpix_per_s,
        );
    }

    if write_outputs {
        for row in &all_rows {
            if let Some(split) = split_outputs.get(&(row.pages, row.implementation, row.mode)) {
                let prefix = format!("{}_{}_{}p", row.implementation, row.mode, row.pages);
                let mode_dir = out_dir.join(&prefix);
                write_fragments_and_decode(split, &mode_dir, &prefix)
                    .unwrap_or_else(|e| panic!("fragment/decode failed for {}: {e}", prefix));
            }
        }
        write_summary_markdown(&out_dir, &source_name, &all_rows).unwrap();
        println!(
            "\nFragments, decoded PBMs, and markdown summary written to: {}",
            out_dir.display()
        );
    }
}
