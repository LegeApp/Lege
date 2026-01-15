// color-replace/src/bin/tester.rs
use clap::Parser;
use color_ops::{
    get_gpu_context, process_color_main, ColorReplacementOptions, NormalizationOptions,
};
use image::{ImageReader, RgbImage};
use ndarray::Array3;
use rand::{rng, seq::SliceRandom, Rng};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

// Add the missing OutputType enum
#[derive(Debug, Clone, Copy)]
enum OutputType {
    PNG8,
}

/// ───────────────────────────────────────────
/// hard-coded defaults  (★ frozen in permutations)
/// ───────────────────────────────────────────
const DEF_SAMPLE_FRACTION: f32 = 0.003; // 3 ‰
const DEF_K: usize = 2;
const DEF_MAX_ITER: usize = 100;

/// other defaults (may be permuted)
const DEF_VEC_DELTA_THRESH: f32 = 20.0;
const DEF_VEC_LIGHT_THRESH: f32 = 15.0;
const DEF_REP_TOLERANCE: f32 = 3.0;
const DEF_REP_GRAD_THRESH: f32 = 7.0;

/// permutation ranges
const RANGE_VEC_DELTA: (f32, f32) = (6.0, 100.0);
const RANGE_VEC_LIGHT: (f32, f32) = (10.0, 100.0);
const RANGE_REP_TOL: (f32, f32) = (2.0, 50.0);
const RANGE_REP_GRAD: (f32, f32) = (4.0, 50.0);

/// ───────────────────────────────────────────
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    /// File or directory to process
    input_path: PathBuf,
    /// Directory to write outputs
    output_dir: PathBuf,

    // Manual overrides (optional)
    #[arg(long)]
    sample_fraction: Option<f32>,
    #[arg(long)]
    k: Option<usize>,
    #[arg(long)]
    max_iter: Option<usize>,
    #[arg(long)]
    vec_delta_thresh: Option<f32>,
    #[arg(long)]
    vec_light_thresh: Option<f32>,
    #[arg(long)]
    rep_tolerance: Option<f32>,
    #[arg(long)]
    rep_grad_thresh: Option<f32>,
    #[arg(long)]
    num_outputs: Option<i32>,

    /// Percentage of images used when picking the reference colour
    #[arg(long = "ref-pct", default_value_t = 100)]
    ref_pct: u8,

    /// disable vector (grid) pass         (normally leave ON)
    #[arg(long)]
    skip_vec: bool,
    /// disable replacement pass
    #[arg(long)]
    skip_replace: bool,

    /// run N random permutations
    #[arg(long)]
    permute: bool,
    #[arg(long, default_value_t = 100)]
    runs: usize,

    /// Output type: png8
    #[arg(long, default_value = "png8")]
    output_type: String,
}

/// persisted settings
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Settings {
    sample_fraction: f32,
    k: usize,
    max_iter: usize,
    vec_delta_thresh: f32,
    vec_light_thresh: f32,
    rep_tolerance: f32,
    rep_grad_thresh: f32,
    num_outputs: i32,
    skip_vec: bool,
    skip_replace: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sample_fraction: DEF_SAMPLE_FRACTION,
            k: DEF_K,
            max_iter: DEF_MAX_ITER,
            vec_delta_thresh: DEF_VEC_DELTA_THRESH,
            vec_light_thresh: DEF_VEC_LIGHT_THRESH,
            rep_tolerance: DEF_REP_TOLERANCE,
            rep_grad_thresh: DEF_REP_GRAD_THRESH,
            num_outputs: -1,
            skip_vec: false,
            skip_replace: false,
        }
    }
}

/// ───────────────────────────────────────────
fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let cli = Cli::parse();

    // 1) load defaults → settings.json → CLI overrides
    let mut base_cfg: Settings = fs::read_to_string("settings.json")
        .ok()
        .and_then(|txt| serde_json::from_str(&txt).ok())
        .unwrap_or_default();

    macro_rules! ovr {
        ($f:ident) => {
            if let Some(v) = cli.$f {
                base_cfg.$f = v;
            }
        };
    }
    ovr!(sample_fraction);
    ovr!(k);
    ovr!(max_iter);
    ovr!(vec_delta_thresh);
    ovr!(vec_light_thresh);
    ovr!(rep_tolerance);
    ovr!(rep_grad_thresh);
    ovr!(num_outputs);

    base_cfg.skip_vec = cli.skip_vec;
    base_cfg.skip_replace = cli.skip_replace;

    // Parse output type
    let output_type = match cli.output_type.to_lowercase().as_str() {
        "png8" => OutputType::PNG8,
        other => {
            eprintln!("Unknown output type: {}. Using png8.", other);
            OutputType::PNG8
        }
    };

    // 2) collect images + reference subset
    let all_imgs = collect_images(&cli.input_path);
    if all_imgs.is_empty() {
        return Err("No images found".into());
    }

    let subset = reference_subset(&all_imgs, cli.ref_pct);
    let ref_lab = compute_global_reference(&subset, base_cfg.sample_fraction)?;

    // 3) output dir
    fs::create_dir_all(&cli.output_dir)?;

    // 4) single run or permutation benchmark
    if cli.permute {
        run_permutations(cli.runs, base_cfg, &cli, &all_imgs, &ref_lab, output_type)?;
    } else {
        single_run(
            base_cfg.clone(),
            &cli.output_dir,
            &all_imgs,
            &ref_lab,
            output_type,
        )?;
        fs::write("settings.json", serde_json::to_string_pretty(&base_cfg)?)?;
        println!("\nSaved effective settings to settings.json");
    }
    Ok(())
}

/// run exactly one configuration
fn single_run(
    cfg: Settings,
    out_dir: &Path,
    imgs: &[PathBuf],
    ref_lab: &[f32; 3],
    output_type: OutputType,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    echo_config("single-run", &cfg);
    println!("Starting single run processing...");

    let mut done = 0;
    for (idx, img) in imgs.iter().enumerate() {
        println!(
            "--- Processing image {}/{}: {:?} ---",
            idx + 1,
            imgs.len(),
            img
        );
        if cfg.num_outputs >= 0 && done >= cfg.num_outputs {
            println!("Reached num_outputs limit ({}), stopping.", cfg.num_outputs);
            break;
        }
        println!("Calling process_one for {:?}", img);
        match process_one(img, out_dir, &cfg, ref_lab, output_type) {
            Ok(()) => {
                println!(
                    "✓ Successfully processed {:?}",
                    img.file_name().unwrap_or(OsStr::new(""))
                );
                done += 1;
            }
            Err(e) => {
                eprintln!(
                    "✗ Error processing {:?}: {}",
                    img.file_name().unwrap_or(OsStr::new("")),
                    e
                );
            }
        }
        println!("Finished process_one for {:?}", img);
    }
    println!("Finished single run processing.");
    Ok(())
}

/// permutation mode
fn run_permutations(
    runs: usize,
    base: Settings,
    cli: &Cli,
    imgs: &[PathBuf],
    ref_lab: &[f32; 3],
    output_type: OutputType,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    println!("=== permutation mode – {runs} runs ===\n");

    let mut rng = rng();
    let mut csv = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("permutations.csv")?;
    if csv.metadata()?.len() == 0 {
        writeln!(
            csv,
            "run,sample,k,max_iter,vec_delta,vec_light,rep_tol,rep_grad,skip_vec,skip_replace"
        )?;
    }

    for n in 0..runs {
        let mut cfg = base.clone();
        randomise(&mut cfg, &mut rng);

        let tag = format!("perm_{:03}", n);
        let sub = cli.output_dir.join(&tag);
        fs::create_dir_all(&sub)?;

        echo_config(&tag, &cfg);
        single_run(cfg.clone(), &sub, imgs, ref_lab, output_type)?;

        writeln!(
            csv,
            "{tag},{},{},{},{},{},{},{},{},{}",
            cfg.sample_fraction,
            cfg.k,
            cfg.max_iter,
            cfg.vec_delta_thresh,
            cfg.vec_light_thresh,
            cfg.rep_tolerance,
            cfg.rep_grad_thresh,
            cfg.skip_vec,
            cfg.skip_replace
        )?;
    }
    Ok(())
}

/// call library while honouring skip flags
fn process_one(
    img: &Path,
    out_dir: &Path,
    cfg: &Settings,
    ref_lab: &[f32; 3],
    output_type: OutputType,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    println!("Entered process_one for {:?}", img);
    let (vec_delta, vec_light, rep_tol, rep_grad) = if cfg.skip_vec {
        eprintln!("Error: skip_vec=true is not allowed");
        return Err("skip_vec=true is not allowed".into());
    } else if cfg.skip_replace {
        println!("Replacement pass skipped.");
        (cfg.vec_delta_thresh, cfg.vec_light_thresh, 0.0, 0.0)
    } else {
        println!(
            "Using thresholds: vec_delta={}, vec_light={}, rep_tol={}, rep_grad={}",
            cfg.vec_delta_thresh, cfg.vec_light_thresh, cfg.rep_tolerance, cfg.rep_grad_thresh
        );
        (
            cfg.vec_delta_thresh,
            cfg.vec_light_thresh,
            cfg.rep_tolerance,
            cfg.rep_grad_thresh,
        )
    };

    println!("Calling process_image_file from process_one...");
    let result = process_image_file(
        img,
        ref_lab,
        vec_delta,
        vec_light,
        rep_tol,
        rep_grad,
        out_dir,
        output_type,
    );
    println!(
        "Returned from process_image_file. Result: {:?}",
        result.is_ok()
    );
    result
}

/// randomise unfrozen parameters
fn randomise(cfg: &mut Settings, rng: &mut impl Rng) {
    // ★ frozen fields left untouched
    cfg.vec_delta_thresh = rng.random_range(RANGE_VEC_DELTA.0..=RANGE_VEC_DELTA.1);
    cfg.vec_light_thresh = rng.random_range(RANGE_VEC_LIGHT.0..=RANGE_VEC_LIGHT.1);
    cfg.rep_tolerance = rng.random_range(RANGE_REP_TOL.0..=RANGE_REP_TOL.1);
    cfg.rep_grad_thresh = rng.random_range(RANGE_REP_GRAD.0..=RANGE_REP_GRAD.1);

    // flip replacement pass on/off (vector always ON)
    cfg.skip_vec = false;
    cfg.skip_replace = rng.random_bool(0.5);
}

fn echo_config(tag: &str, cfg: &Settings) {
    println!("—— {tag} ——————————————————————————");
    println!("sample_fraction    : {}", cfg.sample_fraction);
    println!("k                  : {}", cfg.k);
    println!("max_iter           : {}", cfg.max_iter);
    println!("vec_delta_thresh   : {}", cfg.vec_delta_thresh);
    println!("vec_light_thresh   : {}", cfg.vec_light_thresh);
    println!("rep_tolerance      : {}", cfg.rep_tolerance);
    println!("rep_grad_thresh    : {}", cfg.rep_grad_thresh);
    println!("skip_replace       : {}", cfg.skip_replace);
    println!(
        "num_outputs        : {}",
        if cfg.num_outputs < 0 {
            "ALL".into()
        } else {
            cfg.num_outputs.to_string()
        }
    );
    println!();
}

// ───────────────────────── helpers ─────────────────────────
fn reference_subset(all: &[PathBuf], pct: u8) -> Vec<PathBuf> {
    let pct = pct.clamp(1, 100);
    let need = ((all.len() as f32 * pct as f32 / 100.0).ceil() as usize).max(1);
    let mut rng = rng();
    let mut list = all.to_vec();
    list.shuffle(&mut rng);
    list.truncate(need);
    list
}

fn compute_global_reference(
    subset: &[PathBuf],
    sample_frac: f32,
) -> Result<[f32; 3], Box<dyn Error + Send + Sync>> {
    let mut best = (f32::INFINITY, None);

    // Initialize GPU context first
    let _gpu = get_gpu_context()?;

    for p in subset {
        let img = load_image_as_f32(p)?;
        let var = compute_background_variance(&img, sample_frac)?;
        if var < best.0 {
            best = (var, Some(p));
        }
    }
    let ref_path = best.1.ok_or("reference selection failed")?;
    let img = load_image_as_f32(ref_path)?;
    let lab = get_reference_lab(&img, sample_frac)?;
    Ok([lab.l, lab.a, lab.b])
}

fn load_image_as_f32(p: &Path) -> Result<Array3<f32>, Box<dyn Error + Send + Sync>> {
    let img: RgbImage = ImageReader::open(p)?.decode()?.into_rgb8();
    let (w, h) = img.dimensions();
    let raw: Vec<f32> = img.into_raw().iter().map(|&b| b as f32).collect();
    Ok(Array3::from_shape_vec((h as usize, w as usize, 3), raw)?)
}

fn collect_images(root: &Path) -> Vec<PathBuf> {
    let exts = ["png", "jpg", "jpeg", "ppm", "bmp", "tif", "tiff"];
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    fs::read_dir(root)
        .unwrap()
        .filter_map(|e| {
            let p = e.ok()?.path();
            let ok = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| exts.contains(&e.to_lowercase().as_str()))
                .unwrap_or(false);
            ok.then_some(p)
        })
        .collect()
}

// Missing functions - placeholder implementations
fn process_image_file(
    img: &Path,
    _ref_lab: &[f32; 3],
    _vec_delta: f32,
    _vec_light: f32,
    _rep_tol: f32,
    _rep_grad: f32,
    out_dir: &Path,
    _output_type: OutputType,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Load image
    let image = ImageReader::open(img)?.decode()?.into_rgb8();
    let (width, height) = image.dimensions();
    let pixels: Vec<u8> = image.into_raw();

    // Process using the actual API - use "text" as a generic class name
    let norm_opts = NormalizationOptions::default();
    let color_opts = ColorReplacementOptions::default();
    let result = process_color_main(
        &pixels,
        width as usize,
        height as usize,
        3,
        "text",
        &norm_opts,
        &color_opts,
    )?;

    // Save result
    let output_path = out_dir.join(format!(
        "processed_{}",
        img.file_name().unwrap().to_str().unwrap()
    ));
    let output_image = image::ImageBuffer::from_fn(width, height, |x, y| {
        let idx = ((y * width + x) * 3) as usize;
        let r = result[idx];
        let g = result[idx + 1];
        let b = result[idx + 2];
        image::Rgb([r, g, b])
    });
    output_image.save(output_path)?;

    Ok(())
}

fn compute_background_variance(
    img: &Array3<f32>,
    sample_frac: f32,
) -> Result<f32, Box<dyn Error + Send + Sync>> {
    let total_pixels = img.len() / 3;
    let sample_size = ((total_pixels as f32) * sample_frac).max(50.0).min(10000.0) as usize;

    // Simple variance calculation on a random sample
    let mut rng = rand::rng();
    let mut samples = Vec::new();

    for _ in 0..sample_size {
        let idx = rng.random_range(0..total_pixels);
        let r = img[[
            idx / (img.shape()[1] * img.shape()[2]),
            (idx / img.shape()[2]) % img.shape()[1],
            0,
        ]];
        let g = img[[
            idx / (img.shape()[1] * img.shape()[2]),
            (idx / img.shape()[2]) % img.shape()[1],
            1,
        ]];
        let b = img[[
            idx / (img.shape()[1] * img.shape()[2]),
            (idx / img.shape()[2]) % img.shape()[1],
            2,
        ]];
        let gray = 0.299 * r + 0.587 * g + 0.114 * b;
        samples.push(gray);
    }

    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let variance = samples.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / samples.len() as f32;

    Ok(variance)
}

#[derive(Debug)]
struct LabColor {
    l: f32,
    a: f32,
    b: f32,
}

fn get_reference_lab(
    img: &Array3<f32>,
    sample_frac: f32,
) -> Result<LabColor, Box<dyn Error + Send + Sync>> {
    // Simple implementation - convert a sample to LAB and take the median
    let total_pixels = img.len() / 3;
    let sample_size = ((total_pixels as f32) * sample_frac).max(50.0).min(1000.0) as usize;

    let mut lab_samples = Vec::new();
    let mut rng = rand::rng();

    for _ in 0..sample_size {
        let idx = rng.random_range(0..total_pixels);
        let r = img[[
            idx / (img.shape()[1] * img.shape()[2]),
            (idx / img.shape()[2]) % img.shape()[1],
            0,
        ]];
        let g = img[[
            idx / (img.shape()[1] * img.shape()[2]),
            (idx / img.shape()[2]) % img.shape()[1],
            1,
        ]];
        let b = img[[
            idx / (img.shape()[1] * img.shape()[2]),
            (idx / img.shape()[2]) % img.shape()[1],
            2,
        ]];

        // Simple RGB to LAB approximation
        let l = 0.299 * r + 0.587 * g + 0.114 * b;
        let a = 0.5 * (r - g);
        let b_comp = 0.5 * (g - b);

        lab_samples.push(LabColor {
            l: l * 100.0,
            a: a * 100.0,
            b: b_comp * 100.0,
        });
    }

    // Return the median
    lab_samples.sort_by(|a, b| a.l.partial_cmp(&b.l).unwrap());
    let median_idx = lab_samples.len() / 2;
    Ok(LabColor {
        l: lab_samples[median_idx].l,
        a: lab_samples[median_idx].a,
        b: lab_samples[median_idx].b,
    })
}
