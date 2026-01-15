//! Grayscale image quantization using NeuQuant algorithm

use anyhow::Result;
use clap::Parser;
use png::{BitDepth, ColorType, Decoder, Encoder};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::time::Instant;

/// Compression level for PNG output
#[derive(clap::ValueEnum, Clone, Debug, Copy)]
pub enum PngCompressionArg {
    Default,
    Fast,
    Best,
}

/// Command line arguments for the PNG quantization tool
#[derive(Parser, Debug)]
#[command(name = "png_quant_gray", version = "0.1.0")]
#[command(about = "PNG grayscale quantization tool using NeuQuant algorithm")]
pub struct CliArgs {
    /// Input PNG file path
    pub input: String,
    /// Output PNG file path (optional)
    #[arg(short, long)]
    pub output: Option<String>,
    /// Number of colors in output (1-256)
    #[arg(short, long, default_value_t = 256, value_parser = clap::value_parser!(u16).range(1..=256))]
    pub colors: u16,
    /// Sampling factor (1-30, higher is better quality but slower)
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(i32).range(1..=30))]
    pub sample_factor: i32,
    /// PNG compression level
    #[arg(long, value_enum, default_value_t = PngCompressionArg::Default)]
    pub png_compression: PngCompressionArg,
    /// Optimize for white background
    #[arg(long, default_value_t = false)]
    pub whiten: bool,
}

/// Quantize a grayscale image to 1-bit using NeuQuant algorithm
///
/// # Arguments
/// * `pixels` - Input grayscale pixels (8 bits per pixel)
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
///
/// # Returns
/// * `Vec<u8>` - Packed 1-bit per pixel data (0=black, 1=white)
pub fn quantize_to_1bit(pixels: &[u8], width: usize, height: usize) -> Vec<u8> {
    // Create a new NeuQuantGray instance with 2 colors
    let nq = NeuQuantGray::new(
        10, // sample_factor
        2,  // colors (2 for 1-bit)
        pixels,
        width as u32,
        height as u32,
    );

    // Get the color map (should contain 2 colors)
    let _color_map = nq.color_map();

    // Create output buffer (packed 1-bit per pixel)
    let bytes_per_row = (width + 7) / 8;
    let mut result = vec![0u8; bytes_per_row * height];

    // Map each pixel to 0 or 1 based on which color it's closer to
    for y in 0..height {
        for x in 0..width {
            let src_idx = y * width + x;
            let dst_byte = y * bytes_per_row + (x / 8);
            let dst_bit = 7 - (x % 8);

            // Get the index (0 or 1) of the closest color
            let color_index = nq.index_of(&pixels[src_idx]);

            // Set the corresponding bit in the output
            if color_index == 1 {
                // Assuming index 1 is the lighter color
                result[dst_byte] |= 1 << dst_bit;
            }
        }
    }

    result
}

// Reuse math functions
#[inline]
fn clamp_round(a: f32) -> u8 {
    if a.is_nan() {
        0
    } else if a < 0.0 {
        0
    } else if a > 255.0 {
        255
    } else {
        a.round() as u8
    }
}

/// Neural network quantizer for grayscale images
///
/// Implements the NeuQuant algorithm adapted for grayscale images.
/// Based on Anthony Dekker's NeuQuant implementation.
pub struct NeuQuantGray {
    network: Vec<f32>,
    colormap: Vec<u8>,
    netindex: Vec<usize>,
    bias: Vec<f32>,
    freq: Vec<f32>,
    samplefac: i32,
    netsize: usize,
}

impl NeuQuantGray {
    pub fn new(samplefac: i32, colors: usize, pixels: &[u8], width: u32, height: u32) -> Self {
        let netsize = colors.max(1);
        let mut this = NeuQuantGray {
            network: Vec::with_capacity(netsize),
            colormap: Vec::with_capacity(netsize),
            netindex: vec![0; 256],
            bias: Vec::with_capacity(netsize),
            freq: Vec::with_capacity(netsize),
            samplefac: samplefac.max(1),
            netsize,
        };
        this.init_network_palette(pixels, width, height);
        this
    }

    pub fn index_of(&self, pixel: &u8) -> usize {
        if self.netsize == 0 {
            return 0;
        }
        self.search_netindex(*pixel)
    }

    pub fn color_map(&self) -> Vec<u8> {
        self.colormap.clone()
    }

    fn init_network_palette(&mut self, pixels: &[u8], width: u32, height: u32) {
        self.network.clear();
        self.colormap.clear();
        self.bias.clear();
        self.freq.clear();

        if self.netsize == 0 {
            self.build_netindex();
            return;
        }

        if self.netsize == 1 {
            let avg_color = if pixels.is_empty() {
                128.0
            } else {
                let sum: u64 = pixels.iter().map(|&p| p as u64).sum();
                (sum as f32 / pixels.len() as f32).round() * 4.0 // Scale to 0-1023
            };
            self.network.push(avg_color);
        } else {
            let mut histogram = [0usize; 256];
            for &p in pixels {
                histogram[p as usize] += 1;
            }
            let mut bins: Vec<(usize, usize)> =
                histogram.iter().enumerate().map(|(i, &c)| (i, c)).collect();
            bins.sort_by_key(|&(_, count)| std::cmp::Reverse(count)); // Sort by frequency, descending
            let top_bins = bins
                .iter()
                .take(self.netsize)
                .map(|(val, _)| *val as f32 * 4.0)
                .collect::<Vec<_>>(); // Scale to 0-1023
            self.network.extend(if top_bins.is_empty() {
                (0..self.netsize)
                    .map(|i| (i as f32) * 1023.0 / (self.netsize - 1) as f32)
                    .collect::<Vec<_>>()
            } else {
                top_bins
            });
            self.network
                .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        }

        let freq_val = 1.0 / self.netsize as f32;
        self.freq.resize(self.netsize, freq_val);
        self.bias.resize(self.netsize, 0.0);
        self.colormap.resize(self.netsize, 0);

        if !pixels.is_empty() {
            self.learn(pixels, width, height);
        }
        self.build_colormap();
        self.build_netindex();
    }

    fn salter_single(&mut self, alpha_scale: f32, neuron_idx: i32, pixel: f32) {
        let idx = neuron_idx as usize;
        if idx >= self.netsize {
            return;
        }
        let n = &mut self.network[idx];
        *n -= alpha_scale * (*n - pixel * 4.0); // Scale pixel to 0-1023
    }

    fn alter_neighbour(&mut self, alpha_scale: f32, rad: i32, center_idx: i32, pixel: f32) {
        let lo = (center_idx - rad).max(0) as usize;
        let hi = (center_idx + rad).min(self.netsize as i32 - 1) as usize;
        let sigma = rad as f32 / 2.0;

        for i in lo..=hi {
            let distance = (i as i32 - center_idx).abs() as f32;
            let factor = (-distance * distance / (2.0 * sigma * sigma)).exp();
            let local_alpha = alpha_scale * factor;
            self.network[i] -= local_alpha * (self.network[i] - pixel * 4.0); // Scale pixel to 0-1023
        }
    }

    fn contest(&mut self, pixel: f32) -> i32 {
        if self.netsize == 0 {
            return 0;
        }

        let mut best_dist = f32::MAX;
        let mut best_idx = 0;
        let mut best_bias_idx = 0;
        let mut best_bias_dist = f32::MAX;

        for i in 0..self.netsize {
            let dist = (self.network[i] - pixel * 4.0).abs(); // Scale pixel to 0-1023

            if dist < best_dist || dist < best_bias_dist + self.bias[i] {
                if dist < best_dist {
                    best_dist = dist;
                    best_idx = i;
                }

                let bias_dist = dist - self.bias[i];
                if bias_dist < best_bias_dist {
                    best_bias_dist = bias_dist;
                    best_bias_idx = i;
                }
            }

            self.freq[i] -= 0.001 * self.freq[i];
            self.bias[i] += 1.0 * self.freq[i];
        }

        self.freq[best_idx] += 0.001;
        self.bias[best_idx] -= 1.0;
        best_bias_idx as i32
    }

    fn learn(&mut self, pixels: &[u8], width: u32, height: u32) {
        if self.netsize == 0 || pixels.is_empty() {
            return;
        }

        let length = pixels.len();
        let samplepixels = length / self.samplefac as usize;
        if samplepixels == 0 {
            return;
        }

        let initrad = self.netsize as i32 / 8;
        let radiusbias = 1 << 6;
        let bias_radius = initrad * radiusbias;
        let alphadec = 30 + ((self.samplefac - 1) / 3);
        let n_cycles = (self.netsize >> 1).max(1).min(100);
        let delta = (samplepixels / n_cycles).max(1);

        let mut alpha = 1 << 10;
        let mut rad = bias_radius >> 6;
        let block_size = 16;
        let blocks_x = (width as usize + block_size - 1) / block_size;
        let blocks_y = (height as usize + block_size - 1) / block_size;
        let mut variances = Vec::new();

        for by in 0..blocks_y {
            for bx in 0..blocks_x {
                let mut mean = 0.0;
                let mut m2 = 0.0;
                let mut count = 0;
                for y in (by * block_size)..((by + 1) * block_size).min(height as usize) {
                    for x in (bx * block_size)..((bx + 1) * block_size).min(width as usize) {
                        let idx = y * width as usize + x;
                        if idx < pixels.len() {
                            let p = pixels[idx] as f32;
                            count += 1;
                            let delta = p - mean;
                            mean += delta / count as f32;
                            let delta2 = p - mean;
                            m2 += delta * delta2;
                        }
                    }
                }
                variances.push(if count > 1 {
                    m2 / (count - 1) as f32
                } else {
                    0.0
                });
            }
        }

        let total_variance: f32 = variances.iter().sum();
        let samples_per_block = samplepixels / (blocks_x * blocks_y);

        let mut pos = 0;
        for i in 0..samplepixels {
            let block_idx = (i / samples_per_block) % (blocks_x * blocks_y);
            let var_factor = variances[block_idx] / total_variance.max(1.0);
            let step = (3.0 * (1.0 + var_factor)).round() as usize;
            pos = (pos + step) % length;
            let pixel_idx = pos;
            let pixel = pixels[pixel_idx] as f32;

            let pixel_x = (pixel_idx % width as usize) as i32;
            let pixel_y = (pixel_idx / width as usize) as i32;
            let mut variance = 0.0;
            let mut count = 0;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let x = pixel_x + dx;
                    let y = pixel_y + dy;
                    if x >= 0 && x < width as i32 && y >= 0 && y < height as i32 {
                        let idx = (y * width as i32 + x) as usize;
                        if idx < pixels.len() {
                            let diff = pixels[idx] as f32 - pixel;
                            variance += diff * diff;
                            count += 1;
                        }
                    }
                }
            }
            variance /= count as f32;
            let variance_factor = (1.0 / (1.0 + variance / 1000.0)).max(0.5);

            let winning_neuron = self.contest(pixel);
            let alpha_scale = (alpha as f32 / (1 << 10) as f32) * variance_factor;

            self.salter_single(alpha_scale, winning_neuron, pixel);
            if rad > 0 {
                self.alter_neighbour(alpha_scale, rad, winning_neuron, pixel);
            }

            if (i + 1) % delta == 0 {
                alpha -= alpha / alphadec;
                rad = ((bias_radius * ((samplepixels as i32) - (i as i32)))
                    / (samplepixels as i32))
                    >> 6;
            }
        }
    }

    fn build_colormap(&mut self) {
        let min_val = self.network.iter().fold(f32::MAX, |a, &b| a.min(b));
        let max_val = self.network.iter().fold(f32::MIN, |a, &b| a.max(b));
        let scale = if max_val > min_val {
            255.0 / ((max_val - min_val) / 4.0)
        } else {
            1.0
        };
        let offset = min_val / 4.0;

        for i in 0..self.netsize {
            self.colormap[i] = clamp_round((self.network[i] / 4.0 - offset) * scale);
        }
    }

    fn build_netindex(&mut self) {
        if self.netsize == 0 {
            return;
        }

        self.colormap.sort_unstable();
        let mut prev = 0;
        let mut start = 0;

        for i in 0..self.netsize {
            let current = self.colormap[i] as usize;
            if current != prev {
                self.netindex[prev] = (start + i) >> 1;
                for j in prev + 1..current {
                    if j < 256 {
                        self.netindex[j] = i;
                    }
                }
                prev = current;
                start = i;
            }
        }

        self.netindex[prev] = (start + self.netsize - 1) >> 1;
        for j in prev + 1..256 {
            self.netindex[j] = self.netsize - 1;
        }
    }

    fn search_netindex(&self, pixel: u8) -> usize {
        if self.netsize == 0 {
            return 0;
        }

        let mut best_dist = i32::MAX;
        let first_guess = self.netindex[pixel as usize];
        let mut best_idx = first_guess;

        for i in first_guess..self.netsize {
            let dist = (self.colormap[i] as i32 - pixel as i32).abs();
            if dist < best_dist {
                best_dist = dist;
                best_idx = i;
            }
        }

        for i in (0..first_guess).rev() {
            let dist = (self.colormap[i] as i32 - pixel as i32).abs();
            if dist < best_dist {
                best_dist = dist;
                best_idx = i;
            }
        }

        best_idx
    }
}

// Grayscale-specific PNG loading
/// Load a PNG file and convert it to grayscale
///
/// # Arguments
/// * `path` - Path to the input PNG file
///
/// # Returns
/// * `Result<(Vec<u8>, u32, u32)>` - Tuple of (pixels, width, height)
pub fn load_png_grayscale(path: &str) -> Result<(Vec<u8>, u32, u32), anyhow::Error> {
    let (rgba_pixels, width, height) = load_png_rgba(path)?;
    let mut gray_pixels = Vec::with_capacity(rgba_pixels.len() / 4);

    for chunk in rgba_pixels.chunks_exact(4) {
        let y = (0.299 * chunk[0] as f32 + 0.587 * chunk[1] as f32 + 0.114 * chunk[2] as f32)
            .round() as u8;
        gray_pixels.push(y);
    }

    Ok((gray_pixels, width, height))
}

// Original PNG loading/saving functions reused
fn load_png_rgba(path: &str) -> Result<(Vec<u8>, u32, u32)> {
    let file = File::open(path)?;
    let decoder = Decoder::new(BufReader::new(file));
    let mut reader = decoder.read_info()?;

    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    let pixels_data = buf[..info.buffer_size()].to_vec();

    match info.color_type {
        ColorType::Rgba => Ok((pixels_data, info.width, info.height)),
        ColorType::Rgb => {
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for chunk in pixels_data.chunks_exact(3) {
                out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            Ok((out, info.width, info.height))
        }
        ColorType::Grayscale => {
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for &v in &pixels_data {
                out.extend_from_slice(&[v, v, v, 255]);
            }
            Ok((out, info.width, info.height))
        }
        ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for chunk in pixels_data.chunks_exact(2) {
                out.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            Ok((out, info.width, info.height))
        }
        ColorType::Indexed => {
            let palette = reader
                .info()
                .palette
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Indexed PNG has no palette"))?;
            let trns = reader.info().trns.as_ref();
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for &idx_byte in &pixels_data {
                let i = idx_byte as usize;
                if i * 3 + 2 >= palette.len() {
                    return Err(anyhow::anyhow!("Palette index out of bounds"));
                }
                out.push(palette[i * 3]);
                out.push(palette[i * 3 + 1]);
                out.push(palette[i * 3 + 2]);
                let alpha = trns.and_then(|t| t.get(i).copied()).unwrap_or(255);
                out.push(alpha);
            }
            Ok((out, info.width, info.height))
        }
    }
}

// Removed duplicate CliArgs definition since it's already at the top of the file

fn main() -> Result<()> {
    let args = CliArgs::parse();
    println!("Loading image: {}", args.input);

    let (gray_pixels, width, height) = load_png_grayscale(&args.input).map_err(|e| {
        eprintln!(
            "Failed to load {}: {}. Check if file exists and is a supported PNG.",
            args.input, e
        );
        e
    })?;

    if width == 0 || height == 0 {
        anyhow::bail!("Image dimensions are zero, cannot process.");
    }

    println!("Image loaded: {}x{} pixels.", width, height);
    println!(
        "Starting grayscale quantization to {} colors...",
        args.colors
    );

    let quant_start = Instant::now();
    let nq = NeuQuantGray::new(
        args.sample_factor,
        args.colors as usize,
        &gray_pixels,
        width,
        height,
    );
    let mut palette = nq.color_map();
    if args.whiten && (3..=16).contains(&args.colors) {
        let mut hist = [0usize; 256];
        for &p in &gray_pixels {
            hist[p as usize] += 1;
        }
        let (bg_val, _) = hist.iter().enumerate().max_by_key(|&(_, &v)| v).unwrap();
        if let Some((bg_idx, _)) = palette
            .iter()
            .enumerate()
            .min_by_key(|&(_, &v)| (bg_val as i32 - v as i32).abs())
        {
            if let Some((max_idx, &max_val)) = palette.iter().enumerate().max_by_key(|&(_, &v)| v) {
                if max_val != 255 {
                    palette[max_idx] = 255;
                }
                if bg_idx != max_idx {
                    palette.swap(bg_idx, max_idx);
                }
            }
        }
    }
    let quant_duration = quant_start.elapsed();

    println!("Quantization took: {:?}", quant_duration);
    println!("Mapping pixels...");

    let indexed_data: Vec<u8> = gray_pixels
        .par_iter()
        .map(|p| nq.index_of(p) as u8)
        .collect();

    let output_filename = args.output.unwrap_or_else(|| {
        let input_path = std::path::Path::new(&args.input);
        let stem = input_path.file_stem().unwrap_or_default().to_string_lossy();
        format!("{}_gray_{}c.png", stem, args.colors)
    });

    println!("Saving quantized image to: {}", output_filename);
    let file = File::create(&output_filename)?;
    let w = BufWriter::new(file);
    let mut encoder = Encoder::new(w, width, height);
    encoder.set_color(ColorType::Indexed);

    let bit_depth = match palette.len() {
        0..=2 => BitDepth::One,
        3..=4 => BitDepth::Two,
        5..=16 => BitDepth::Four,
        _ => BitDepth::Eight,
    };

    println!(
        "Using bit depth: {:?} for {} colors",
        bit_depth,
        palette.len()
    );
    encoder.set_depth(bit_depth);

    let rgb_palette: Vec<u8> = palette.iter().flat_map(|&g| vec![g, g, g]).collect();
    encoder.set_palette(rgb_palette);

    let mut writer = encoder.write_header()?;

    // For 1-bit images, we need to pack the pixels into bits (8 pixels per byte)
    let image_data = if bit_depth == BitDepth::One {
        let packed: Vec<u8> = indexed_data
            .chunks(8)
            .map(|chunk| {
                let mut byte = 0u8;
                for (i, &pixel) in chunk.iter().enumerate() {
                    if pixel != 0 {
                        // Assuming 0 is background, 1 is foreground
                        byte |= 1 << (7 - i);
                    }
                }
                byte
            })
            .collect();
        packed
    } else {
        indexed_data
    };

    writer.write_image_data(&image_data)?;
    println!("Successfully saved: {}", output_filename);
    Ok(())
}
