//! High-resolution encode baseline harness for `jp2lam-hd-encode-plan.md`.
//!
//! This is intentionally an example, not an always-on test: the configured
//! cases can allocate hundreds of MiB in the current full-image architecture.
//!
//! Usage examples:
//!
//! ```text
//! cargo run --release --example hd_memory_baseline -- 12 gray 100
//! cargo run --release --example hd_memory_baseline -- 12,24 rgb 100 512
//! cargo run --release --example hd_memory_baseline -- all both 100 512
//! ```
//!
//! Output is CSV so successive refactor phases can be compared directly.

use jp2lam::{
    EncodeOptions, ImageView, OutputFormat, ResourceLimits, TilePolicy, encode_view_to_writer,
};
#[cfg(feature = "counters")]
use jp2lam::{memory_snapshot, reset};
use std::io::{self, Write};
use std::time::Instant;

const CASES: &[ResolutionCase] = &[
    ResolutionCase {
        label: "12MP",
        width: 4242,
        height: 2828,
    },
    ResolutionCase {
        label: "24MP",
        width: 6000,
        height: 4000,
    },
    ResolutionCase {
        label: "36MP",
        width: 7350,
        height: 4900,
    },
    ResolutionCase {
        label: "45MP",
        width: 8216,
        height: 5477,
    },
    ResolutionCase {
        label: "50MP",
        width: 8688,
        height: 5792,
    },
];

#[derive(Debug, Clone, Copy)]
struct ResolutionCase {
    label: &'static str,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy)]
enum ColorCase {
    Gray,
    Rgb,
}

#[derive(Debug, Clone, Copy)]
struct StageMemoryEstimate {
    caller_owned_source_bytes: u64,
    estimated_encoder_transient_peak: u64,
    estimated_total_owned_peak: u64,
    encode_context_prepared_clone: u64,
    source_load_scratch_one_component: u64,
    dwt_coefficients_one_component: u64,
    dwt_scratch_one_component: u64,
    quantized_one_component: u64,
    codeblock_coefficient_copies_all_components: u64,
    tier1_analysis_coefficient_copies_all_components: u64,
    final_output_buffer: u64,
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let case_filter = args.first().map(String::as_str).unwrap_or("12");
    let color_filter = args.get(1).map(String::as_str).unwrap_or("gray");
    let quality = args
        .get(2)
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(100);
    let working_memory_mib = args
        .get(3)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(512);
    let working_memory_bytes = working_memory_mib.saturating_mul(1024 * 1024);
    let precision = args
        .get(4)
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(8);
    assert!((8..=16).contains(&precision), "precision must be 8..=16");

    let cases = select_cases(case_filter);
    let colors = select_colors(color_filter);

    println!(
        "label,width,height,megapixels,color,components,precision,quality,tile_policy,max_working_memory_bytes,selected_tile_width,selected_tile_height,tile_count,elapsed_ms,rss_start_bytes,rss_end_bytes,peak_rss_bytes,output_bytes,caller_owned_source_bytes,estimated_encoder_transient_peak,estimated_total_owned_peak,encode_context_prepared_clone,source_load_scratch_one_component,dwt_coefficients_one_component,dwt_scratch_one_component,quantized_one_component,codeblock_coefficient_copies_all_components,tier1_analysis_coefficient_copies_all_components,final_output_buffer,counter_tile_samples_peak,counter_dwt_coefficients_peak,counter_dwt_scratch_peak,counter_codeblock_worker_peak,counter_store_memory_peak,counter_store_spilled_bytes,counter_rd_metadata_peak,counter_packet_header_peak,counter_output_buffer_peak"
    );

    for case in cases {
        for color in &colors {
            run_case(case, *color, quality, working_memory_bytes, precision);
        }
    }
}

fn run_case(
    case: ResolutionCase,
    color: ColorCase,
    quality: u8,
    working_memory_bytes: usize,
    precision: u8,
) {
    #[cfg(feature = "counters")]
    reset();
    let component_count = color.component_count();
    let caller_owned_source_bytes = u64::from(case.width)
        .saturating_mul(u64::from(case.height))
        .saturating_mul(component_count as u64)
        .saturating_mul(if precision <= 8 { 1 } else { 2 });
    let rss_start = current_rss_bytes();
    let peak_start = peak_rss_bytes();
    let start = Instant::now();
    let mut sink = CountingWriter::default();
    match color {
        ColorCase::Gray => {
            if precision <= 8 {
                let source = synth_gray(case.width, case.height);
                let view = ImageView::from_gray8(case.width, case.height, &source)
                    .expect("construct gray source view");
                encode_view_to_writer(
                    view,
                    &encode_options(quality, working_memory_bytes),
                    &mut sink,
                )
                .expect("high-resolution gray encode failed");
            } else {
                let source = synth_gray_u16(case.width, case.height, precision);
                let view =
                    ImageView::from_gray16(case.width, case.height, &source, u32::from(precision))
                        .expect("construct high-bit-depth gray source view");
                encode_view_to_writer(
                    view,
                    &encode_options(quality, working_memory_bytes),
                    &mut sink,
                )
                .expect("high-resolution gray encode failed");
            }
        }
        ColorCase::Rgb => {
            if precision <= 8 {
                let source = synth_rgb(case.width, case.height);
                let view = ImageView::from_rgb8_interleaved(case.width, case.height, &source)
                    .expect("construct RGB source view");
                encode_view_to_writer(
                    view,
                    &encode_options(quality, working_memory_bytes),
                    &mut sink,
                )
                .expect("high-resolution RGB encode failed");
            } else {
                let source = synth_rgb_u16(case.width, case.height, precision);
                let view = ImageView::from_rgb16_interleaved(
                    case.width,
                    case.height,
                    &source,
                    u32::from(precision),
                )
                .expect("construct high-bit-depth RGB source view");
                encode_view_to_writer(
                    view,
                    &encode_options(quality, working_memory_bytes),
                    &mut sink,
                )
                .expect("high-resolution RGB encode failed");
            }
        }
    }
    let elapsed = start.elapsed();
    let rss_end = current_rss_bytes();
    let peak_end = peak_rss_bytes().max(peak_start);
    let (tile_width, tile_height) =
        selected_auto_tile_dimensions(case.width, case.height, working_memory_bytes);
    let tile_count = case.width.div_ceil(tile_width) * case.height.div_ceil(tile_height);
    let estimate = estimate_current_architecture_memory(
        tile_width,
        tile_height,
        component_count,
        caller_owned_source_bytes,
        quality,
        0,
    );
    let counters = memory_counter_values();
    let color_label = match color {
        ColorCase::Gray => "gray",
        ColorCase::Rgb => "rgb",
    };

    println!(
        "{},{},{},{:.3},{},{},{},{},auto,{},{},{},{},{:.3},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        case.label,
        case.width,
        case.height,
        case.width as f64 * case.height as f64 / 1_000_000.0,
        color_label,
        component_count,
        precision,
        quality,
        working_memory_bytes,
        tile_width,
        tile_height,
        tile_count,
        elapsed.as_secs_f64() * 1000.0,
        rss_start.unwrap_or(0),
        rss_end.unwrap_or(0),
        peak_end.unwrap_or(0),
        sink.bytes,
        estimate.caller_owned_source_bytes,
        estimate.estimated_encoder_transient_peak,
        estimate.estimated_total_owned_peak,
        estimate.encode_context_prepared_clone,
        estimate.source_load_scratch_one_component,
        estimate.dwt_coefficients_one_component,
        estimate.dwt_scratch_one_component,
        estimate.quantized_one_component,
        estimate.codeblock_coefficient_copies_all_components,
        estimate.tier1_analysis_coefficient_copies_all_components,
        estimate.final_output_buffer,
        counters[0],
        counters[1],
        counters[2],
        counters[3],
        counters[4],
        counters[5],
        counters[6],
        counters[7],
        counters[8]
    );
}

#[cfg(feature = "counters")]
fn memory_counter_values() -> [u64; 9] {
    let snapshot = memory_snapshot();
    [
        snapshot.tile_sample_bytes_peak,
        snapshot.dwt_coefficient_bytes_peak,
        snapshot.dwt_scratch_bytes_peak,
        snapshot.codeblock_worker_bytes_peak,
        snapshot.encoded_store_memory_bytes_peak,
        snapshot.encoded_store_spilled_bytes,
        snapshot.rd_metadata_bytes_peak,
        snapshot.packet_header_bytes_peak,
        snapshot.output_buffer_bytes_peak,
    ]
}

#[cfg(not(feature = "counters"))]
fn memory_counter_values() -> [u64; 9] {
    [0; 9]
}

impl ColorCase {
    fn component_count(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
        }
    }
}

fn encode_options(quality: u8, working_memory_bytes: usize) -> EncodeOptions {
    EncodeOptions {
        quality,
        format: OutputFormat::Jp2,
        profile: Default::default(),
        tile_policy: TilePolicy::Auto,
        resource_limits: ResourceLimits {
            max_working_memory: Some(working_memory_bytes),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len() as u64);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn selected_auto_tile_dimensions(
    image_width: u32,
    image_height: u32,
    working_memory_bytes: usize,
) -> (u32, u32) {
    let mut edge = 4096u32;
    while edge >= 64 {
        let width = edge.min(image_width);
        let height = edge.min(image_height);
        let estimated = width as usize * height as usize * 2 * std::mem::size_of::<i32>();
        if estimated <= working_memory_bytes {
            return (width, height);
        }
        edge /= 2;
    }
    (64.min(image_width), 64.min(image_height))
}

fn select_cases(filter: &str) -> Vec<ResolutionCase> {
    if filter.eq_ignore_ascii_case("all") {
        return CASES.to_vec();
    }
    let requested = filter
        .split(',')
        .filter_map(|value| value.trim().parse::<u32>().ok())
        .collect::<Vec<_>>();
    if requested.is_empty() {
        return vec![CASES[0]];
    }
    requested
        .into_iter()
        .filter_map(|mp| {
            CASES
                .iter()
                .copied()
                .find(|case| case.label.trim_end_matches("MP") == mp.to_string())
        })
        .collect()
}

fn select_colors(filter: &str) -> Vec<ColorCase> {
    match filter.to_ascii_lowercase().as_str() {
        "gray" | "grey" => vec![ColorCase::Gray],
        "rgb" => vec![ColorCase::Rgb],
        "both" => vec![ColorCase::Gray, ColorCase::Rgb],
        _ => vec![ColorCase::Gray],
    }
}

fn estimate_current_architecture_memory(
    width: u32,
    height: u32,
    components: usize,
    caller_owned_source_bytes: u64,
    quality: u8,
    output_bytes: u64,
) -> StageMemoryEstimate {
    let pixels = u64::from(width) * u64::from(height);
    let components = components as u64;
    let one_component_i32 = pixels * std::mem::size_of::<i32>() as u64;
    let one_component_f32 = pixels * std::mem::size_of::<f32>() as u64;
    let is_lossless = quality >= 100;
    let rev53_row_work = u64::from(width.max(height)) * 3 * std::mem::size_of::<i32>() as u64;
    let rev53_vertical_band =
        u64::from(width.min(256)) * u64::from(height) * std::mem::size_of::<i32>() as u64;
    let dwt_scratch_one_component = if is_lossless {
        rev53_row_work.saturating_add(rev53_vertical_band)
    } else {
        one_component_f32
    };
    let quantized_one_component = if is_lossless { 0 } else { one_component_i32 };

    // This is an intentionally coarse current-architecture estimate, not an
    // allocator trace. It models the known active-path overlap hazards after
    // the Phase 4 sequential-component change: one component's source-load
    // scratch and coefficient work are live at a time, compact per-worker
    // code-block scratch is transient, and the buffered `encode_view` result is
    // retained as the final output Vec. It excludes caller-owned source storage
    // so future zero-copy and streaming work can compare the encoder-owned
    // transient budget directly.
    let encode_context_prepared_clone = 0u64;
    let source_load_scratch_one_component = one_component_i32;
    let active_source_load_scratch = if !is_lossless && components == 3 {
        // The current ICT path reloads R, G, and B to synthesize each
        // transformed output component. Components are now processed
        // sequentially, but this source-load overlap still remains within one
        // active component job.
        components * source_load_scratch_one_component
    } else {
        source_load_scratch_one_component
    };
    let codeblock_coefficient_copies_all_components = 0u64;
    let tier1_analysis_coefficient_copies_all_components = 0u64;
    let estimated_encoder_transient_peak = encode_context_prepared_clone
        .saturating_add(active_source_load_scratch)
        .saturating_add(one_component_i32)
        .saturating_add(dwt_scratch_one_component)
        .saturating_add(quantized_one_component)
        .saturating_add(codeblock_coefficient_copies_all_components)
        .saturating_add(tier1_analysis_coefficient_copies_all_components)
        .saturating_add(output_bytes);

    StageMemoryEstimate {
        caller_owned_source_bytes,
        estimated_encoder_transient_peak,
        estimated_total_owned_peak: caller_owned_source_bytes
            .saturating_add(estimated_encoder_transient_peak),
        encode_context_prepared_clone,
        source_load_scratch_one_component,
        dwt_coefficients_one_component: one_component_i32,
        dwt_scratch_one_component,
        quantized_one_component,
        codeblock_coefficient_copies_all_components,
        tier1_analysis_coefficient_copies_all_components,
        final_output_buffer: output_bytes,
    }
}

fn synth_gray(width: u32, height: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        for x in 0..width {
            data.push((((x * 13) ^ (y * 17) ^ (x.wrapping_mul(y) / 257)) & 0xff) as u8);
        }
    }
    data
}

fn synth_rgb(width: u32, height: u32) -> Vec<u8> {
    let pixels = width as usize * height as usize;
    let mut data = Vec::with_capacity(pixels * 3);
    for y in 0..height {
        for x in 0..width {
            let gradient_r = x * 255 / width.max(1);
            let gradient_g = y * 255 / height.max(1);
            let texture = ((x / 16 + y / 16) & 1) * 63;
            data.push(((gradient_r + texture) & 0xff) as u8);
            data.push(((gradient_g + (texture / 2)) & 0xff) as u8);
            data.push((((x * 3 + y * 5) / 11 + texture) & 0xff) as u8);
        }
    }
    data
}

fn synth_gray_u16(width: u32, height: u32, precision: u8) -> Vec<u16> {
    let mask = (1u32 << precision) - 1;
    let mut data = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        for x in 0..width {
            data.push(((x * 257) ^ (y * 509) ^ x.wrapping_mul(y)) as u16 & mask as u16);
        }
    }
    data
}

fn synth_rgb_u16(width: u32, height: u32, precision: u8) -> Vec<u16> {
    let max = (1u32 << precision) - 1;
    let mut data = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height {
        for x in 0..width {
            let texture = ((x / 16 + y / 16) & 1) * (max / 4);
            data.push(((x * max / width.max(1) + texture) & max) as u16);
            data.push(((y * max / height.max(1) + texture / 2) & max) as u16);
            data.push((((x * 3 + y * 5) * 257 / 11 + texture) & max) as u16);
        }
    }
    data
}

fn current_rss_bytes() -> Option<u64> {
    proc_status_kb("VmRSS").map(|kb| kb * 1024)
}

fn peak_rss_bytes() -> Option<u64> {
    proc_status_kb("VmHWM").map(|kb| kb * 1024)
}

#[cfg(target_os = "linux")]
fn proc_status_kb(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let rest = line.strip_prefix(field)?.strip_prefix(':')?.trim();
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })
}

#[cfg(not(target_os = "linux"))]
fn proc_status_kb(_field: &str) -> Option<u64> {
    None
}
