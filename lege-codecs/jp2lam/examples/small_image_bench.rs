//! Where time and bytes go when the image is small.
//!
//! ```text
//! cargo run --release --example small_image_bench -- [corpus_dir]
//! cargo run --release --features profile --example small_image_bench   # phase breakdown
//! ```
//!
//! Downscales every top-level PNG of the corpus (box filter, sRGB 8-bit) to a
//! ladder of small sizes and encodes each with the three modes a PDF pipeline
//! actually uses. Reports median wall time, output bytes split into JP2 boxes /
//! codestream main header / body, and metric evaluations.
//!
//! Environment: `SMALL_BENCH_ONLY=320x240` and `SMALL_BENCH_MODE=display` narrow
//! the run to one row when reading a profile.
use jp2lam::{
    DisplayProfile, EncodeOptions, ImageView, OutputFormat, PERCEPTUAL_PROBES, PerceptualEffort,
    PerceptualTarget, RateControl, encode_view,
};
use std::sync::atomic::Ordering;
use std::time::Instant;

const SIZES: [(u32, u32); 6] = [
    (64, 48),
    (128, 96),
    (200, 150),
    (320, 240),
    (480, 360),
    (640, 480),
];
const RUNS: usize = 5;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "test-set".into());
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {dir}: {e}"))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no PNGs in {dir}");

    let sources: Vec<(u32, u32, Vec<u8>)> = paths
        .iter()
        .map(|p| {
            let rgb = image::open(p)
                .unwrap_or_else(|e| panic!("open {}: {e}", p.display()))
                .into_rgb8();
            let (w, h) = rgb.dimensions();
            (w, h, rgb.into_raw())
        })
        .collect();
    println!("corpus: {} images from {dir}", sources.len());
    println!(
        "{:>9} {:>22} {:>9} {:>7} {:>7} {:>7} {:>7} {:>6}",
        "size", "mode", "ms", "bytes", "boxes", "mainhdr", "body", "evals"
    );

    let only = std::env::var("SMALL_BENCH_ONLY").ok();
    for (w, h) in SIZES {
        if only.as_deref().is_some_and(|f| f != format!("{w}x{h}")) {
            continue;
        }
        let scaled: Vec<Vec<u8>> = sources
            .iter()
            .map(|(sw, sh, data)| box_downscale_rgb8(data, *sw, *sh, w, h))
            .collect();
        let only_mode = std::env::var("SMALL_BENCH_MODE").ok();
        for (label, options) in modes() {
            if only_mode.as_deref().is_some_and(|f| !label.starts_with(f)) {
                continue;
            }
            #[cfg(feature = "counters")]
            jp2lam::reset();
            let mut ms = 0.0;
            let mut bytes = 0usize;
            let mut boxes = 0usize;
            let mut mainhdr = 0usize;
            let mut evals = 0u64;
            for pixels in &scaled {
                let view = ImageView::from_rgb8_interleaved(w, h, pixels).expect("view");
                let mut times = Vec::with_capacity(RUNS);
                let mut out = Vec::new();
                for _ in 0..RUNS {
                    PERCEPTUAL_PROBES.store(0, Ordering::Relaxed);
                    let start = Instant::now();
                    out = encode_view(view.clone(), &options).expect("encode");
                    times.push(start.elapsed().as_secs_f64() * 1000.0);
                }
                times.sort_by(f64::total_cmp);
                ms += times[RUNS / 2];
                evals += PERCEPTUAL_PROBES.load(Ordering::Relaxed);
                let (box_bytes, header_bytes) = split_jp2(&out);
                bytes += out.len();
                boxes += box_bytes;
                mainhdr += header_bytes;
            }
            let n = scaled.len() as f64;
            println!(
                "{:>9} {label:>22} {:>9.2} {:>7.0} {:>7.0} {:>7.0} {:>7.0} {:>6.1}",
                format!("{w}x{h}"),
                ms / n,
                bytes as f64 / n,
                boxes as f64 / n,
                mainhdr as f64 / n,
                (bytes - boxes - mainhdr) as f64 / n,
                evals as f64 / n,
            );
        }
    }
    jp2lam::print_timing_data();
    #[cfg(feature = "counters")]
    jp2lam::print();
}

fn modes() -> Vec<(&'static str, EncodeOptions)> {
    let display =
        PerceptualTarget::for_display(70.0, DisplayProfile::eink(600, 450), PerceptualEffort::Fast)
            .expect("display target");
    vec![
        (
            "ApproxQuality(75)",
            EncodeOptions {
                rate_control: Some(RateControl::ApproxQuality(75)),
                format: OutputFormat::Jp2,
                ..Default::default()
            },
        ),
        (
            "Quality{75,Fast}",
            EncodeOptions {
                rate_control: Some(RateControl::Quality {
                    level: 75,
                    effort: PerceptualEffort::Fast,
                }),
                ..EncodeOptions::photo(75, OutputFormat::Jp2)
            },
        ),
        (
            "display(70,eink600x450)",
            EncodeOptions {
                rate_control: Some(RateControl::Perceptual(display)),
                ..EncodeOptions::photo(99, OutputFormat::Jp2)
            },
        ),
    ]
}

/// JP2 bytes outside the codestream, and codestream bytes before the first SOT.
fn split_jp2(bytes: &[u8]) -> (usize, usize) {
    let mut offset = 0usize;
    while offset + 8 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let kind = &bytes[offset + 4..offset + 8];
        let len = if len == 0 { bytes.len() - offset } else { len };
        if kind == b"jp2c" {
            let codestream = &bytes[offset + 8..offset + len];
            return (bytes.len() - codestream.len(), main_header_len(codestream));
        }
        offset += len.max(8);
    }
    (0, main_header_len(bytes))
}

fn main_header_len(codestream: &[u8]) -> usize {
    codestream
        .windows(2)
        .position(|w| w == [0xff, 0x90])
        .unwrap_or(codestream.len())
}

/// Area-average downscale of interleaved RGB8, matching the evaluator's filter
/// closely enough for a size ladder (this one averages in sRGB, not linear).
fn box_downscale_rgb8(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0u8; dw as usize * dh as usize * 3];
    let (xs, ys) = (f64::from(sw) / f64::from(dw), f64::from(sh) / f64::from(dh));
    for y in 0..dh as usize {
        let (y0, y1) = span(y, ys, sh);
        for x in 0..dw as usize {
            let (x0, x1) = span(x, xs, sw);
            let mut acc = [0u32; 3];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = (sy * sw as usize + sx) * 3;
                    for c in 0..3 {
                        acc[c] += u32::from(src[i + c]);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            for c in 0..3 {
                out[(y * dw as usize + x) * 3 + c] = ((acc[c] + n / 2) / n) as u8;
            }
        }
    }
    out
}

fn span(index: usize, scale: f64, extent: u32) -> (usize, usize) {
    let lo = (index as f64 * scale).floor() as usize;
    let hi = (((index + 1) as f64 * scale).ceil() as usize).min(extent as usize);
    (lo.min(extent as usize - 1), hi.max(lo + 1))
}
