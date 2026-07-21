#[cfg(feature = "counters")]
use jp2lam::print;
use jp2lam::{
    ColorSpace, Component, EncodeOptions, Image, OutputFormat, encode, print_timing_data,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn lear_png_path() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("lear.png"))
        .unwrap_or_else(|| PathBuf::new());
    if workspace.exists() {
        return workspace;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lear.png")
}

fn load_lear_rgb() -> Image {
    let path = lear_png_path();
    if !path.exists() {
        eprintln!(
            "{} not found; using synthetic benchmark image",
            path.display()
        );
        return synth_rgb_image();
    }
    let dyn_img = image::open(&path)
        .unwrap_or_else(|e| panic!("failed to open {}: {}", path.display(), e))
        .into_rgb8();
    let width = dyn_img.width();
    let height = dyn_img.height();
    let n = (width * height) as usize;
    let mut r = Vec::with_capacity(n);
    let mut g = Vec::with_capacity(n);
    let mut b = Vec::with_capacity(n);
    for pixel in dyn_img.pixels() {
        r.push(pixel[0] as i32);
        g.push(pixel[1] as i32);
        b.push(pixel[2] as i32);
    }
    Image {
        width,
        height,
        components: vec![
            Component {
                data: r,
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            },
            Component {
                data: g,
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            },
            Component {
                data: b,
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            },
        ],
        colorspace: ColorSpace::Srgb,
    }
}

fn load_lear_gray() -> Image {
    let path = lear_png_path();
    if !path.exists() {
        eprintln!(
            "{} not found; using synthetic benchmark image",
            path.display()
        );
        return synth_gray_image();
    }
    let dyn_img = image::open(&path)
        .unwrap_or_else(|e| panic!("failed to open {}: {}", path.display(), e))
        .into_luma8();
    let width = dyn_img.width();
    let height = dyn_img.height();
    let n = (width * height) as usize;
    let mut data = Vec::with_capacity(n);
    for pixel in dyn_img.pixels() {
        data.push(pixel[0] as i32);
    }
    Image {
        width,
        height,
        components: vec![Component {
            data,
            width,
            height,
            precision: 8,
            signed: false,
            dx: 1,
            dy: 1,
        }],
        colorspace: ColorSpace::Gray,
    }
}

fn synth_rgb_image() -> Image {
    const W: u32 = 1024;
    const H: u32 = 768;
    let n = (W * H) as usize;
    let mut r = Vec::with_capacity(n);
    let mut g = Vec::with_capacity(n);
    let mut b = Vec::with_capacity(n);

    for y in 0..H {
        for x in 0..W {
            let checker = ((x / 16) + (y / 16)) & 1;
            let diagonal = ((x + 2 * y) % 257) as i32;
            let wave = (((x as f32 * 0.035).sin() * (y as f32 * 0.021).cos() * 64.0) + 128.0)
                .round() as i32;
            r.push(((x * 255 / (W - 1)) as i32 + checker as i32 * 32).clamp(0, 255));
            g.push(((y * 255 / (H - 1)) as i32 + wave / 4).clamp(0, 255));
            b.push((diagonal / 2 + wave / 2).clamp(0, 255));
        }
    }

    let component = |data| Component {
        data,
        width: W,
        height: H,
        precision: 8,
        signed: false,
        dx: 1,
        dy: 1,
    };
    Image {
        width: W,
        height: H,
        components: vec![component(r), component(g), component(b)],
        colorspace: ColorSpace::Srgb,
    }
}

fn synth_gray_image() -> Image {
    let rgb = synth_rgb_image();
    let n = (rgb.width * rgb.height) as usize;
    let mut data = Vec::with_capacity(n);
    let r = &rgb.components[0].data;
    let g = &rgb.components[1].data;
    let b = &rgb.components[2].data;
    for i in 0..n {
        data.push(((299 * r[i] + 587 * g[i] + 114 * b[i] + 500) / 1000).clamp(0, 255));
    }
    Image {
        width: rgb.width,
        height: rgb.height,
        components: vec![Component {
            data,
            width: rgb.width,
            height: rgb.height,
            precision: 8,
            signed: false,
            dx: 1,
            dy: 1,
        }],
        colorspace: ColorSpace::Gray,
    }
}

fn run_benchmark(name: &str, image: &Image, quality: u8) {
    let iterations = benchmark_iterations();
    let mut total_time = Duration::ZERO;
    let mut total_cpu_time = Duration::ZERO;
    let mut cpu_samples = 0u32;

    println!(
        "\n=== {} ({}x{}, {} components) ===",
        name,
        image.width,
        image.height,
        image.components.len()
    );

    for i in 0..iterations {
        let cpu_start = ProcessCpuTime::now();
        let start = Instant::now();
        let bytes = encode(
            &image,
            &EncodeOptions {
                quality,
                format: OutputFormat::Jp2,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("encode failed");
        let elapsed = start.elapsed();
        let cpu_elapsed = cpu_start.and_then(|start| start.elapsed());
        total_time += elapsed;
        if let Some(cpu_elapsed) = cpu_elapsed {
            total_cpu_time += cpu_elapsed;
            cpu_samples += 1;
            println!(
                "Iter {}: {} bytes in {:?} wall, {:?} CPU ({:.0}% of one core)",
                i + 1,
                bytes.len(),
                elapsed,
                cpu_elapsed,
                100.0 * cpu_elapsed.as_secs_f64() / elapsed.as_secs_f64()
            );
        } else {
            println!("Iter {}: {} bytes in {:?}", i + 1, bytes.len(), elapsed);
        }
    }

    let avg = total_time / iterations;
    println!(
        "Avg: {:?} ({:.2} MP/s)",
        avg,
        (image.width as f64 * image.height as f64 / 1_000_000.0) / (avg.as_secs_f64())
    );
    if cpu_samples > 0 {
        let avg_cpu = total_cpu_time / cpu_samples;
        println!(
            "CPU Avg: {:?} ({:.0}% of one core)",
            avg_cpu,
            100.0 * avg_cpu.as_secs_f64() / avg.as_secs_f64()
        );
    }

    #[cfg(feature = "counters")]
    print();
}

#[derive(Clone, Copy)]
struct ProcessCpuTime {
    elapsed: Duration,
}

impl ProcessCpuTime {
    fn now() -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            process_cpu_time_linux().map(|elapsed| Self { elapsed })
        }

        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    fn elapsed(self) -> Option<Duration> {
        let end = Self::now()?;
        end.elapsed.checked_sub(self.elapsed)
    }
}

#[cfg(target_os = "linux")]
fn process_cpu_time_linux() -> Option<Duration> {
    process_thread_schedstat_time_linux().or_else(process_stat_time_linux)
}

#[cfg(target_os = "linux")]
fn process_thread_schedstat_time_linux() -> Option<Duration> {
    let mut runtime_ns = 0u64;
    let mut samples = 0u32;
    for entry in std::fs::read_dir("/proc/self/task").ok()? {
        let path = entry.ok()?.path().join("schedstat");
        let schedstat = std::fs::read_to_string(path).ok()?;
        let thread_runtime_ns = schedstat.split_whitespace().next()?.parse::<u64>().ok()?;
        runtime_ns = runtime_ns.checked_add(thread_runtime_ns)?;
        samples += 1;
    }
    (samples > 0).then(|| Duration::from_nanos(runtime_ns))
}

#[cfg(target_os = "linux")]
fn process_stat_time_linux() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields = stat.rsplit_once(") ")?.1;
    let mut parts = fields.split_whitespace();
    // Field 3 (`state`) is the first token after the process name. utime/stime
    // are fields 14/15, so they are tokens 11/12 in this suffix.
    let utime = parts.nth(11)?.parse::<u64>().ok()?;
    let stime = parts.next()?.parse::<u64>().ok()?;
    let ticks = utime + stime;
    Some(Duration::from_secs_f64(
        ticks as f64 / clock_ticks_per_second(),
    ))
}

fn clock_ticks_per_second() -> f64 {
    std::env::var("JP2LAM_CLK_TCK")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|&value| value > 0.0)
        .unwrap_or(100.0)
}

fn benchmark_iterations() -> u32 {
    std::env::var("JP2LAM_BENCH_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&value| value > 0)
        .unwrap_or(3)
}

fn benchmark_quality() -> u8 {
    std::env::var("JP2LAM_BENCH_QUALITY")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(50)
}

fn main() {
    let rgb = load_lear_rgb();
    let gray = load_lear_gray();
    let quality = benchmark_quality();

    println!("Loaded benchmark image: {}x{}", rgb.width, rgb.height);

    // Test grayscale first (simpler pipeline)
    run_benchmark(&format!("Grayscale q={quality}"), &gray, quality);

    // Then RGB
    run_benchmark(&format!("RGB q={quality}"), &rgb, quality);

    // Print profiling data
    print_timing_data();
}
