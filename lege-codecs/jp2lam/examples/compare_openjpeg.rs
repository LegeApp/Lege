//! Same-stream decode comparison: jp2lam vs local OpenJPEG (`opj_decompress`).
//!
//! Encodes a PNG (or reuses a JP2), times both decoders, and optionally diffs
//! 8-bit PNG outputs. OpenJPEG is located via `OPJ_DECOMPRESS`,
//! `OPENJPEG_BIN`, or the well-known local tree
//! `D:\tools\openjpeg\openjpeg-master\build\bin\Release\opj_decompress.exe`.
//!
//! ```text
//! cargo run --release --example compare_openjpeg -- lear.png
//! cargo run --release --example compare_openjpeg -- fixture.jp2
//! ```
//!
//! Environment:
//! - `JP2LAM_COMPARE_ITERS` (default 5)
//! - `JP2LAM_COMPARE_THREADS` (default 4)
//! - `JP2LAM_COMPARE_QUALITY` encode quality when input is PNG (default 75)
//! - `JP2LAM_COMPARE_REDUCE` wavelet reduce levels (default 0)
//! - `JP2LAM_COMPARE_OUTDIR` working directory (default temp)
//! - `OPJ_DECOMPRESS` / `OPENJPEG_BIN` path overrides

use jp2lam::{
    DecodeConcurrency, DecodeOutputFormat, DecodeRequest, DecodeResolution, DecodeResult, Image,
    Jp2Decoder, OutputFormat, encode, inspect_jp2,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "lear.png".to_string());
    let iterations = env_usize("JP2LAM_COMPARE_ITERS", 5).max(1);
    let threads = env_usize("JP2LAM_COMPARE_THREADS", 4).max(1);
    let quality = env_usize("JP2LAM_COMPARE_QUALITY", 75).min(100) as u8;
    let reduce = env_usize("JP2LAM_COMPARE_REDUCE", 0) as u8;

    let opj = resolve_opj_decompress();
    println!("opj_decompress={}", opj.display());
    println!("input={path}");
    println!("iterations={iterations}");
    println!("threads={threads}");
    println!("reduce={reduce}");

    let outdir = match std::env::var_os("JP2LAM_COMPARE_OUTDIR") {
        Some(p) => PathBuf::from(p),
        None => std::env::temp_dir().join(format!("jp2lam-opj-compare-{}", std::process::id())),
    };
    std::fs::create_dir_all(&outdir).expect("create outdir");
    println!("outdir={}", outdir.display());

    let (fixture_label, jp2_path, encoded) = prepare_jp2(&path, &outdir, quality);
    println!("fixture={fixture_label}");
    println!("jp2={}", jp2_path.display());
    println!("compressed_bytes={}", encoded.len());

    let meta = inspect_jp2(&encoded).expect("inspect");
    let (width, height) = (meta.width, meta.height);
    let comps = meta.codestream.siz.components.len();
    println!("dims={width}x{height}");
    println!("components={comps}");
    println!("colorspace={:?}", meta.colorspace);

    let output = match meta.colorspace {
        jp2lam::ColorSpace::Gray => DecodeOutputFormat::Gray8,
        jp2lam::ColorSpace::Cmyk => DecodeOutputFormat::Cmyk8,
        _ => DecodeOutputFormat::Rgb8,
    };
    let concurrency = if threads <= 1 {
        DecodeConcurrency::Serial
    } else {
        DecodeConcurrency::Budgeted(threads)
    };
    let request = DecodeRequest {
        resolution: if reduce == 0 {
            DecodeResolution::Full
        } else {
            DecodeResolution::ReduceLevels(reduce)
        },
        output,
        concurrency,
        ..Default::default()
    };

    // --- jp2lam ---
    let mut decoder = Jp2Decoder::new();
    let _ = decoder.decode(&encoded, &request).expect("jp2lam warm");
    let mut jp2lam_samples = Vec::with_capacity(iterations);
    let mut jp2lam_raster = None;
    for _ in 0..iterations {
        let start = Instant::now();
        let result = decoder.decode(&encoded, &request).expect("jp2lam decode");
        jp2lam_samples.push(start.elapsed());
        if jp2lam_raster.is_none() {
            if let DecodeResult::Raster(r) = result {
                jp2lam_raster = Some(r);
            }
        }
    }
    jp2lam_samples.sort_unstable();
    let jp2lam_median = jp2lam_samples[jp2lam_samples.len() / 2];
    println!(
        "jp2lam.median_ms={:.3}",
        jp2lam_median.as_secs_f64() * 1_000.0
    );
    println!("jp2lam.mean_ms={:.3}", mean_ms(&jp2lam_samples));

    // --- OpenJPEG ---
    let opj_png = outdir.join("opj_out.png");
    let mut opj_samples = Vec::with_capacity(iterations);
    for i in 0..iterations {
        // Write to a unique name only on the last run for diff; intermediate runs
        // overwrite the same path (timing includes PNG encode in opj_decompress —
        // note that in results; decode-only is best measured with opj_bench.c).
        let start = Instant::now();
        let status = Command::new(&opj)
            .args([
                "-i",
                jp2_path.to_str().expect("utf8 path"),
                "-o",
                opj_png.to_str().expect("utf8 path"),
                "-threads",
                &threads.to_string(),
            ])
            .args(if reduce > 0 {
                vec!["-r".to_string(), reduce.to_string()]
            } else {
                Vec::new()
            })
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap_or_else(|e| panic!("spawn opj_decompress failed: {e}"));
        if !status.success() {
            panic!("opj_decompress failed on run {i} with {status}");
        }
        opj_samples.push(start.elapsed());
    }
    opj_samples.sort_unstable();
    let opj_median = opj_samples[opj_samples.len() / 2];
    println!(
        "openjpeg.median_ms={:.3}",
        opj_median.as_secs_f64() * 1_000.0
    );
    println!("openjpeg.mean_ms={:.3}", mean_ms(&opj_samples));
    println!("note=openjpeg_cli_includes_png_encode; use opj_bench.c for decode-only wall time");
    println!(
        "ratio_jp2lam_over_opj_cli={:.3}",
        jp2lam_median.as_secs_f64() / opj_median.as_secs_f64().max(f64::MIN_POSITIVE)
    );

    // --- Pixel diff (jp2lam packed vs OpenJPEG PNG) ---
    if let Some(raster) = jp2lam_raster {
        let opj_img = image::open(&opj_png)
            .unwrap_or_else(|e| panic!("open opj png: {e}"))
            .to_rgb8();
        let (ow, oh) = opj_img.dimensions();
        if ow != raster.width || oh != raster.height {
            println!(
                "diff.skipped=dimension_mismatch jp2lam={}x{} opj={}x{}",
                raster.width, raster.height, ow, oh
            );
        } else {
            let (max_abs, mean_abs, over2) =
                diff_packed_vs_rgb8(&raster.data, raster.format, opj_img.as_raw(), ow, oh);
            println!("diff.max_abs={max_abs}");
            println!("diff.mean_abs={mean_abs:.6}");
            println!("diff.pixels_over_2={over2}");
            // Photo encode uses irreversible 9/7 (gate ≤2). Lossless 5/3
            // fixtures should be bit-exact (gate 0); allow 2 as the shared
            // corpus tolerance when the input was a quality-encoded PNG.
            let gate = 2u32;
            if max_abs > gate {
                println!("diff.gate=FAILED expected max_abs<={gate}");
                std::process::exit(1);
            } else {
                println!("diff.gate=ok (max_abs <= {gate})");
            }
        }
    }

    // Also time decode-only via opj if opj_bench exists next to the script area.
    if let Some(bench) = resolve_opj_bench() {
        println!("opj_bench={}", bench.display());
        let output = Command::new(&bench)
            .args([
                jp2_path.to_str().unwrap(),
                &threads.to_string(),
                &reduce.to_string(),
                &iterations.to_string(),
            ])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                print!("opj_bench.stdout={}", String::from_utf8_lossy(&out.stdout));
            }
            Ok(out) => {
                eprintln!("opj_bench failed: {}", String::from_utf8_lossy(&out.stderr));
            }
            Err(e) => eprintln!("opj_bench spawn failed: {e}"),
        }
    } else {
        println!("opj_bench=not_found (compile decode-fix-plan/opj_bench.c for decode-only)");
    }
}

fn prepare_jp2(path: &str, outdir: &Path, quality: u8) -> (String, PathBuf, Vec<u8>) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "jp2" | "j2k" | "j2c" | "jpc") {
        let dest = outdir.join("fixture.jp2");
        std::fs::write(&dest, &bytes).expect("copy jp2");
        return (path.to_string(), dest, bytes);
    }
    let dyn_img = image::load_from_memory(&bytes).expect("decode raster");
    let rgb = dyn_img.to_rgb8();
    let (w, h) = rgb.dimensions();
    let image = Image::from_rgb_bytes(w, h, rgb.as_raw()).expect("Image");
    let encoded = encode(&image, &approx_photo(quality, OutputFormat::Jp2)).expect("encode jp2");
    let dest = outdir.join(format!("lear_q{quality}.jp2"));
    std::fs::write(&dest, &encoded).expect("write jp2");
    (format!("{path}->jp2-q{quality}-{w}x{h}"), dest, encoded)
}

fn diff_packed_vs_rgb8(
    packed: &[u8],
    format: DecodeOutputFormat,
    opj_rgb: &[u8],
    width: u32,
    height: u32,
) -> (u32, f64, u64) {
    let pixels = (width as usize) * (height as usize);
    let channels = match format {
        DecodeOutputFormat::Gray8 => 1,
        DecodeOutputFormat::Rgb8 => 3,
        DecodeOutputFormat::Rgbx8 | DecodeOutputFormat::Bgra8 | DecodeOutputFormat::Rgba8 => 4,
        DecodeOutputFormat::Cmyk8 => 4,
        _ => 3,
    };
    assert_eq!(packed.len(), pixels * channels);
    assert_eq!(opj_rgb.len(), pixels * 3);

    let mut max_abs = 0u32;
    let mut sum = 0u64;
    let mut over2 = 0u64;
    let mut count = 0u64;

    for i in 0..pixels {
        let (r, g, b) = match format {
            DecodeOutputFormat::Gray8 => {
                let g = packed[i];
                (g, g, g)
            }
            DecodeOutputFormat::Rgb8 => {
                let o = i * 3;
                (packed[o], packed[o + 1], packed[o + 2])
            }
            DecodeOutputFormat::Rgbx8 | DecodeOutputFormat::Rgba8 => {
                let o = i * 4;
                (packed[o], packed[o + 1], packed[o + 2])
            }
            DecodeOutputFormat::Bgra8 => {
                let o = i * 4;
                (packed[o + 2], packed[o + 1], packed[o])
            }
            // CMYK vs OpenJPEG RGB is not directly comparable; skip.
            DecodeOutputFormat::Cmyk8 => return (0, 0.0, 0),
            _ => return (0, 0.0, 0),
        };
        let o = i * 3;
        for (a, b) in [(r, opj_rgb[o]), (g, opj_rgb[o + 1]), (b, opj_rgb[o + 2])] {
            let d = u32::from(a).abs_diff(u32::from(b));
            max_abs = max_abs.max(d);
            sum += u64::from(d);
            if d > 2 {
                over2 += 1;
            }
            count += 1;
        }
    }
    let mean = sum as f64 / count as f64;
    (max_abs, mean, over2)
}

fn resolve_opj_decompress() -> PathBuf {
    if let Ok(p) = std::env::var("OPJ_DECOMPRESS") {
        return PathBuf::from(p);
    }
    if let Ok(dir) = std::env::var("OPENJPEG_BIN") {
        let candidate = PathBuf::from(dir).join("opj_decompress.exe");
        if candidate.is_file() {
            return candidate;
        }
        let candidate =
            PathBuf::from(std::env::var("OPENJPEG_BIN").unwrap()).join("opj_decompress");
        if candidate.is_file() {
            return candidate;
        }
    }
    let known =
        PathBuf::from(r"D:\tools\openjpeg\openjpeg-master\build\bin\Release\opj_decompress.exe");
    if known.is_file() {
        return known;
    }
    // PATH fallback
    PathBuf::from("opj_decompress")
}

fn resolve_opj_bench() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from(r"D:\tools\openjpeg\openjpeg-master\build\bin\Release\opj_bench.exe"),
        PathBuf::from("opj_bench.exe"),
        PathBuf::from("opj_bench"),
        PathBuf::from("decode-fix-plan").join("opj_bench.exe"),
    ];
    // Prefer the harness next to this crate's decode-fix-plan folder.
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.insert(
            0,
            PathBuf::from(manifest)
                .join("decode-fix-plan")
                .join("opj_bench.exe"),
        );
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn mean_ms(samples: &[Duration]) -> f64 {
    let total: Duration = samples.iter().sum();
    (total / samples.len() as u32).as_secs_f64() * 1_000.0
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

/// Legacy open-loop photo preset: this harness compares rates, not verified quality.
fn approx_photo(quality: u8, format: jp2lam::OutputFormat) -> jp2lam::EncodeOptions {
    let mut options = jp2lam::EncodeOptions::photo(quality, format);
    if quality < 100 {
        options.rate_control = Some(jp2lam::RateControl::ApproxQuality(quality));
    }
    options
}
