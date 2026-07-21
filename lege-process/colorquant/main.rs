// Standalone color quantization tester using NeuQuant
// Combines math.rs, lib.rs, and quantize.rs logic into one file
use anyhow::Result;
use clap::Parser;
use png::{BitDepth, ColorType, Decoder, Encoder};
use rayon::prelude::*; // For parallel processing
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::time::Instant;

// --- math.rs ---
#[inline]
fn clamp_round(a: f32) -> i32 {
    if a.is_nan() {
        0
    } else if a < 0.0 {
        0
    } else if a > 255.0 {
        255
    } else {
        (a + 0.5) as i32
    }
}

// --- lib.rs core logic ---
const CHANNELS: usize = 4; // RGBA
const RADIUS_DEC: i32 = 30;
const ALPHA_BIASSHIFT: i32 = 10;
const INIT_ALPHA: i32 = 1 << ALPHA_BIASSHIFT; // 1024

// Constants changed to f32
const GAMMA: f32 = 1024.0;
const BETA: f32 = 1.0 / GAMMA;
const BETAGAMMA: f32 = 1.0; // BETA * GAMMA simplified

const PRIMES: [usize; 4] = [499, 491, 487, 503];

#[derive(Clone, Copy, Debug)]
struct Quad<T> {
    r: T,
    g: T,
    b: T,
    a: T,
}

type Neuron = Quad<f32>; // Using f32 for performance
type Color = Quad<i32>;

pub struct NeuQuant {
    network: Vec<Neuron>,
    colormap: Vec<Color>,
    netindex: Vec<usize>,
    bias: Vec<f32>, // Using f32
    freq: Vec<f32>, // Using f32
    samplefac: i32,
    netsize: usize,
}

impl NeuQuant {
    pub fn new(samplefac: i32, colors: usize, pixels: &[u8]) -> Self {
        let netsize = colors.max(1); // Ensure netsize is at least 1
        let mut this = NeuQuant {
            network: Vec::with_capacity(netsize),
            colormap: Vec::with_capacity(netsize),
            netindex: vec![0; 256],
            bias: Vec::with_capacity(netsize),
            freq: Vec::with_capacity(netsize),
            samplefac: samplefac.max(1), // Ensure samplefac is at least 1
            netsize,
        };
        this.init(pixels);
        this
    }

    // Not used in current main flow with indexed PNG, but kept for API completeness
    #[allow(dead_code)]
    pub fn map_pixel(&self, pixel: &mut [u8]) {
        assert!(pixel.len() == 4);
        if self.netsize == 0 {
            return;
        }
        let (r, g, b, a) = (pixel[0], pixel[1], pixel[2], pixel[3]);
        let i = self.search_netindex(b, g, r, a);
        pixel[0] = self.colormap[i].r as u8;
        pixel[1] = self.colormap[i].g as u8;
        pixel[2] = self.colormap[i].b as u8;
        pixel[3] = self.colormap[i].a as u8;
    }

    pub fn index_of(&self, pixel_rgba: &[u8]) -> usize {
        assert!(pixel_rgba.len() == 4);
        if self.netsize == 0 {
            return 0;
        }
        // NeuQuant traditionally uses BGR order for input to contest
        let b = pixel_rgba[2];
        let g = pixel_rgba[1];
        let r = pixel_rgba[0];
        let a = pixel_rgba[3];
        self.search_netindex(b, g, r, a)
    }

    pub fn color_map_rgba(&self) -> Vec<u8> {
        let mut map = Vec::with_capacity(self.netsize * 4);
        for entry in &self.colormap {
            map.push(entry.r as u8);
            map.push(entry.g as u8);
            map.push(entry.b as u8);
            map.push(entry.a as u8);
        }
        map
    }

    fn init(&mut self, pixels: &[u8]) {
        self.network.clear();
        self.colormap.clear();
        self.bias.clear();
        self.freq.clear();

        if self.netsize == 0 {
            self.build_netindex(); // Initializes netindex to all 0s
            return;
        }

        let freq_val = (1.0f32) / (self.netsize as f32);
        for i in 0..self.netsize {
            let tmp = (i as f32) * 256.0 / (self.netsize as f32);
            // Initialize alpha channel somewhat spread out for initial neurons
            let a_init = if self.netsize <= 16 {
                (i as f32) * (255.0 / (self.netsize -1).max(1) as f32) // Spread alpha from 0 to 255
            } else {
                 if i < 16 { (i as f32) * 16.0 } else { 255.0 } // Original approach
            };

            self.network.push(Neuron { r: tmp, g: tmp, b: tmp, a: a_init.clamp(0.0, 255.0) });
            self.colormap.push(Color { r: 0, g: 0, b: 0, a: 255 }); // Placeholder
            self.freq.push(freq_val);
            self.bias.push(0.0);
        }
        
        // NeuQuant recommends BGR order for network neurons, seems implied by some implementations.
        // Initializing with B=tmp, G=tmp, R=tmp; A=a_init
        // self.network.push(Neuron { b: tmp, g: tmp, r: tmp, a: a_init.clamp(0.0, 255.0) });
        // The current structure Quad {r,g,b,a} and usage in contest (n.b - b_pix, n.r - r_pix) is consistent,
        // so as long as pixel components are passed to contest in B,G,R,A order, it matches.

        if pixels.len() >= CHANNELS {
            self.learn(pixels);
        }
        self.build_colormap();
        self.build_netindex();
    }

    fn salter_single(&mut self, alpha_scale: f32, neuron_idx: i32, quad_pix: Quad<f32>) {
        let neuron_idx_usize = neuron_idx as usize;
        if neuron_idx_usize >= self.netsize { return; }

        let n = &mut self.network[neuron_idx_usize];
        n.b -= alpha_scale * (n.b - quad_pix.b);
        n.g -= alpha_scale * (n.g - quad_pix.g);
        n.r -= alpha_scale * (n.r - quad_pix.r);
        n.a -= alpha_scale * (n.a - quad_pix.a);
    }

    fn alter_neighbour(&mut self, alpha_scale: f32, rad: i32, center_idx: i32, quad_pix: Quad<f32>) {
        let lo = (center_idx - rad).max(0); // Ensure non-negative
        let hi = (center_idx + rad).min(self.netsize as i32 -1); // Ensure within bounds

        let mut j = center_idx + 1;
        let mut k = center_idx - 1;
        let mut q = 1; // Distance from center neuron

        while (j <= hi) || (k >= lo) {
            let rad_sq = (rad * rad) as f32;
            // Weight drops off as square of distance from radius
            let factor = (rad_sq - (q * q) as f32) / rad_sq;
            let local_alpha = alpha_scale * factor;

            if j <= hi {
                let p = &mut self.network[j as usize];
                p.b -= local_alpha * (p.b - quad_pix.b);
                p.g -= local_alpha * (p.g - quad_pix.g);
                p.r -= local_alpha * (p.r - quad_pix.r);
                p.a -= local_alpha * (p.a - quad_pix.a);
                j += 1;
            }
            if k >= lo {
                let p = &mut self.network[k as usize];
                p.b -= local_alpha * (p.b - quad_pix.b);
                p.g -= local_alpha * (p.g - quad_pix.g);
                p.r -= local_alpha * (p.r - quad_pix.r);
                p.a -= local_alpha * (p.a - quad_pix.a);
                k -= 1;
            }
            q += 1;
        }
    }

    fn contest(&mut self, b_pix: f32, g_pix: f32, r_pix: f32, a_pix: f32) -> i32 {
        if self.netsize == 0 { return 0; }

        let mut bestd = f32::MAX;
        let mut bestbiasd = bestd;
        let mut bestpos = -1;
        let mut bestbiaspos = -1;

        for i in 0..self.netsize {
            let n = &self.network[i];
            let mut dist = (n.b - b_pix).abs();
            dist += (n.r - r_pix).abs();
            
            let current_bias = self.bias[i];
            if dist < bestd || dist < bestbiasd + current_bias {
                dist += (n.g - g_pix).abs();
                // Alpha distance weight can be adjusted. Standard NeuQuant applies equal weight.
                // Some variants might use a lower weight for alpha or perceptual weights for RGB.
                // const ALPHA_WEIGHT: f32 = 0.5; // Example: dist += (n.a - a_pix).abs() * ALPHA_WEIGHT;
                dist += (n.a - a_pix).abs();

                if dist < bestd {
                    bestd = dist;
                    bestpos = i as i32;
                }
                let biasdist = dist - current_bias;
                if biasdist < bestbiasd {
                    bestbiasd = biasdist;
                    bestbiaspos = i as i32;
                }
            }
            let current_freq = self.freq[i];
            self.freq[i] -= BETA * current_freq;
            self.bias[i] += BETAGAMMA * current_freq; // BETAGAMMA is 1.0
        }
        
        if bestpos == -1 { bestpos = 0; } // Default to first neuron if none clearly won
        self.freq[bestpos as usize] += BETA;
        self.bias[bestpos as usize] -= BETAGAMMA; // BETAGAMMA is 1.0

        if bestbiaspos == -1 { bestbiaspos = bestpos; } // If no bias winner, use overall winner
        bestbiaspos
    }

    fn learn(&mut self, pixels: &[u8]) {
        if self.netsize == 0 || pixels.is_empty() { return; }

        let mut initrad = self.netsize as i32 / 8;
        if initrad < 1 { initrad = 1; } // Ensure radius is at least 1 if netsize is small
        
        let radiusbiasshift: i32 = 6;
        let radiusbias: i32 = 1 << radiusbiasshift;
        let mut bias_radius = initrad * radiusbias;
        
        let alphadec = (30 + ((self.samplefac - 1) / 3)).max(1);
        
        let lengthcount = pixels.len() / CHANNELS;
        if lengthcount == 0 { return; }

        let samplepixels = lengthcount / self.samplefac as usize;
        if samplepixels == 0 { return; }

        let n_cycles_denom = if self.netsize < 4 { 1 } else { self.netsize >> 1 }; // Avoid div by zero or small netsize issues
        let n_cycles = (n_cycles_denom).max(1).min(100); // Original was min 100, then n for n > 100. Let's cap at 100
        

        let delta = (samplepixels / n_cycles).max(1);

        let mut alpha = INIT_ALPHA;
        let mut rad = bias_radius >> radiusbiasshift;
        if rad < 1 { rad = 0; }


        let mut pos = 0;
        let step = *PRIMES.iter().find(|&&prime| lengthcount % prime != 0).unwrap_or(&PRIMES[3]);
        
        for i in 0..samplepixels {
            let pixel_base_idx = (pos % lengthcount) * CHANNELS;
            let p_slice = &pixels[pixel_base_idx .. pixel_base_idx + CHANNELS];
            
            // Pixels are RGBA, pass to contest as B,G,R,A
            let b_pix = p_slice[2] as f32;
            let g_pix = p_slice[1] as f32;
            let r_pix = p_slice[0] as f32;
            let a_pix = p_slice[3] as f32;
            let quad_pix = Quad { r: r_pix, g: g_pix, b: b_pix, a: a_pix };
            
            let winning_neuron_idx = self.contest(b_pix, g_pix, r_pix, a_pix);
            let alpha_scale = (alpha as f32) / (INIT_ALPHA as f32);

            self.salter_single(alpha_scale, winning_neuron_idx, quad_pix);
            if rad > 0 {
                self.alter_neighbour(alpha_scale, rad, winning_neuron_idx, quad_pix);
            }

            pos += step;
            // Handled by pos % lengthcount above: if pos >= lengthcount { pos -= lengthcount; }

            if (i + 1) % delta == 0 { // Iteration i is 0-indexed
                alpha -= alpha / alphadec;
                bias_radius -= bias_radius / RADIUS_DEC;
                rad = bias_radius >> radiusbiasshift;
                if rad < 1 { rad = 0; }
            }
        }
    }

    fn build_colormap(&mut self) {
        if self.netsize == 0 { return; }
        for i in 0..self.netsize {
            self.colormap[i].b = clamp_round(self.network[i].b);
            self.colormap[i].g = clamp_round(self.network[i].g);
            self.colormap[i].r = clamp_round(self.network[i].r);
            self.colormap[i].a = clamp_round(self.network[i].a);
        }
    }

    fn build_netindex(&mut self) {
        if self.netsize == 0 {
            for val in self.netindex.iter_mut() { *val = 0; }
            return;
        }

        // Sort self.colormap by green component (g) to build the index
        self.colormap.sort_unstable_by_key(|c| c.g);
        
        let mut previouscol = 0; 
        let mut startpos = 0;    
        for i in 0..self.netsize {
            let p_g_val = self.colormap[i].g; // This is i32, but represents 0-255
            // Ensure p_g_val is within expected u8 range before casting to usize for indexing netindex
            let p_g = p_g_val.clamp(0, 255) as usize;

            if p_g != previouscol {
                self.netindex[previouscol] = (startpos + i) >> 1;
                for j_idx in (previouscol + 1)..p_g {
                     if j_idx < 256 { self.netindex[j_idx] = i; }
                }
                previouscol = p_g;
                startpos = i;
            }
        }
        let max_netpos = (self.netsize - 1).max(0); // Ensure not negative if netsize is 1
        self.netindex[previouscol] = (startpos + max_netpos) >> 1;
        for j_idx in (previouscol + 1)..256 {
            self.netindex[j_idx] = max_netpos;
        }
    }

    // Input pixel components b,g,r,a are u8
    fn search_netindex(&self, b: u8, g: u8, r: u8, a: u8) -> usize {
        if self.netsize == 0 { return 0; }

        let mut best_dist = i32::MAX;
        let first_guess = self.netindex[g as usize].min(self.netsize -1);
        let mut best_pos = first_guess;

        // Local helper for squared distance.
        // val_map is i32 (from colormap), val_pix is u8.
        #[inline(always)]
        fn sqr_dist(val_map: i32, val_pix: u8) -> i32 {
            let diff = val_map - val_pix as i32;
            diff * diff
        }

        // Search forwards
        for current_pos in first_guess..self.netsize {
            let map_color = self.colormap[current_pos];
            let mut dist = sqr_dist(map_color.g, g);
            if dist > best_dist { break; } // Early exit if g distance alone is too large
            
            dist += sqr_dist(map_color.r, r);
            if dist >= best_dist && map_color.g != g as i32 {let _ = current_pos.saturating_add(1); continue; } // Optimization from some variants
            
            dist += sqr_dist(map_color.b, b);
            if dist >= best_dist && map_color.g != g as i32 {let _ = current_pos.saturating_add(1); continue; }

            dist += sqr_dist(map_color.a, a);
            if dist < best_dist {
                best_dist = dist;
                best_pos = current_pos;
            }
        }
        
        // Search backwards
        if first_guess > 0 {
            for current_pos in (0..first_guess).rev() {
                let map_color = self.colormap[current_pos];
                let mut dist = sqr_dist(map_color.g, g);
                if dist > best_dist { break; }

                dist += sqr_dist(map_color.r, r);
                if dist >= best_dist && map_color.g != g as i32 { continue; }

                dist += sqr_dist(map_color.b, b);
                if dist >= best_dist && map_color.g != g as i32 { continue; }
                
                dist += sqr_dist(map_color.a, a);
                if dist < best_dist {
                    best_dist = dist;
                    best_pos = current_pos;
                }
            }
        }
        best_pos
    }
}

// --- PNG loading/saving and main ---

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
        ColorType::Indexed => { // Handle indexed PNGs by expanding them
            let palette = reader.info().palette.as_ref().ok_or_else(|| anyhow::anyhow!("Indexed PNG has no palette"))?;
            let trns = reader.info().trns.as_ref();
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for &idx_byte in &pixels_data {
                let i = idx_byte as usize;
                if i * 3 + 2 >= palette.len() { return Err(anyhow::anyhow!("Palette index out of bounds")); }
                out.push(palette[i*3]);
                out.push(palette[i*3+1]);
                out.push(palette[i*3+2]);
                let alpha = trns.and_then(|t| t.get(i).copied()).unwrap_or(255);
                out.push(alpha);
            }
            Ok((out, info.width, info.height))
        }
    }
}

#[derive(clap::ValueEnum, Clone, Debug, Copy)]
enum PngCompressionArg {
    Default,
    Fast,
    Best,
}

#[derive(Parser, Debug)]
#[command(name = "png_quant", version = "0.2.0")]
#[command(about = "PNG color quantization tool using NeuQuant algorithm")]
struct CliArgs {
    /// Input PNG file
    input: String,
    
    /// Output PNG file path
    #[arg(short, long)]
    output: Option<String>,

    /// Number of colors in output palette (1-256)
    #[arg(short, long, default_value_t = 256, value_parser = clap::value_parser!(u16).range(1..=256))]
    colors: u16,

    /// Sampling factor for NeuQuant learning (1-30, lower is faster but potentially lower quality)
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(i32).range(1..=30))]
    sample_factor: i32,

    /// PNG compression strategy for output
    #[arg(long, value_enum, default_value_t = PngCompressionArg::Default)]
    png_compression: PngCompressionArg,

    /// Apply Floyd-Steinberg dithering to the output
    #[arg(long)]
    dither: bool,
}

fn main() -> Result<()> {
    let args = CliArgs::parse();
    
    println!("Loading image: {}", args.input);
    let (pixels_rgba, width, height) = load_png_rgba(&args.input)
        .map_err(|e| {
            eprintln!("Failed to load {}: {}. Check if file exists and is a supported PNG.", args.input, e);
            e
        })?;

    if width == 0 || height == 0 {
        anyhow::bail!("Image dimensions are zero, cannot process.");
    }
    println!("Image loaded: {}x{} pixels.", width, height);

    println!("Starting quantization to {} colors (sample_factor: {}, dither: {})...", args.colors, args.sample_factor, args.dither);
    let quant_map_start_time = Instant::now();

    // --- NeuQuant Training ---
    let nq = NeuQuant::new(args.sample_factor, args.colors as usize, &pixels_rgba);
    let palette_rgba = nq.color_map_rgba(); // RGBA palette from NeuQuant
    
    // --- Pixel Mapping (and optional Dithering) ---
    let mut indexed_image_data: Vec<u8> = Vec::with_capacity((width * height) as usize);

    if args.dither {
        println!("Applying Floyd-Steinberg dithering (sequential)...");
        let mut dither_pixels_f32 = pixels_rgba.iter().map(|&p_val| p_val as f32).collect::<Vec<f32>>();
        let w_usize = width as usize;

        for y in 0..height as usize {
            for x in 0..w_usize {
                let base_idx = (y * w_usize + x) * CHANNELS;

                let r_curr = dither_pixels_f32[base_idx    ].clamp(0.0, 255.0);
                let g_curr = dither_pixels_f32[base_idx + 1].clamp(0.0, 255.0);
                let b_curr = dither_pixels_f32[base_idx + 2].clamp(0.0, 255.0);
                let a_curr = dither_pixels_f32[base_idx + 3].clamp(0.0, 255.0); // Alpha passed for lookup

                let current_pixel = [r_curr as u8, g_curr as u8, b_curr as u8, a_curr as u8];
                let palette_idx = nq.index_of(&current_pixel);
                indexed_image_data.push(palette_idx as u8);

                let chosen_color_entry_idx = palette_idx * CHANNELS;
                let pr = palette_rgba[chosen_color_entry_idx    ] as f32;
                let pg = palette_rgba[chosen_color_entry_idx + 1] as f32;
                let pb = palette_rgba[chosen_color_entry_idx + 2] as f32;
                // Dithering error is typically on RGB, alpha from palette via tRNS
                
                let err_r = r_curr - pr;
                let err_g = g_curr - pg;
                let err_b = b_curr - pb;

                // Distribute error
                if x + 1 < w_usize {
                    let idx_right = base_idx + CHANNELS;
                    dither_pixels_f32[idx_right    ] += err_r * 7.0 / 16.0;
                    dither_pixels_f32[idx_right + 1] += err_g * 7.0 / 16.0;
                    dither_pixels_f32[idx_right + 2] += err_b * 7.0 / 16.0;
                }
                if y + 1 < height as usize {
                    let idx_down_base = base_idx + CHANNELS * w_usize;
                    if x > 0 {
                        let idx_down_left = idx_down_base - CHANNELS;
                        dither_pixels_f32[idx_down_left    ] += err_r * 3.0 / 16.0;
                        dither_pixels_f32[idx_down_left + 1] += err_g * 3.0 / 16.0;
                        dither_pixels_f32[idx_down_left + 2] += err_b * 3.0 / 16.0;
                    }
                    let idx_down = idx_down_base;
                    dither_pixels_f32[idx_down        ] += err_r * 5.0 / 16.0;
                    dither_pixels_f32[idx_down + 1    ] += err_g * 5.0 / 16.0;
                    dither_pixels_f32[idx_down + 2    ] += err_b * 5.0 / 16.0;
                    if x + 1 < w_usize {
                        let idx_down_right = idx_down_base + CHANNELS;
                        dither_pixels_f32[idx_down_right    ] += err_r * 1.0 / 16.0;
                        dither_pixels_f32[idx_down_right + 1] += err_g * 1.0 / 16.0;
                        dither_pixels_f32[idx_down_right + 2] += err_b * 1.0 / 16.0;
                    }
                }
            }
        }
    } else {
        println!("Mapping pixels (parallel)...");
        indexed_image_data = pixels_rgba
            .par_chunks_exact(CHANNELS)
            .map(|chunk_rgba| nq.index_of(chunk_rgba) as u8)
            .collect();
    }
    
    let quant_map_duration = quant_map_start_time.elapsed();
    println!("Quantization & mapping took: {:?}", quant_map_duration);

    // --- Save Indexed PNG ---
    let output_filename = args.output.unwrap_or_else(|| {
        let input_path = std::path::Path::new(&args.input);
        let stem = input_path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = input_path.extension().unwrap_or_default().to_string_lossy();
        format!("{}_quant_{}c.{}", stem, args.colors, ext)
    });
    
    println!("Saving quantized image to: {}", output_filename);
    let file = File::create(&output_filename)?;
    let w = BufWriter::new(file);
    
    let mut encoder = Encoder::new(w, width, height);
    encoder.set_color(ColorType::Indexed);
    encoder.set_depth(BitDepth::Eight);
    
    let compression_setting = match args.png_compression {
        PngCompressionArg::Default => png::Compression::default(),
        PngCompressionArg::Fast => png::Compression::Fast,
        PngCompressionArg::Best => png::Compression::Best,
    };
    encoder.set_compression(compression_setting);
    // Optionally set filter: encoder.set_filter(png::FilterType::Paeth);
    
    // Prepare PLTE (RGB palette)
    let rgb_palette: Vec<u8> = palette_rgba.chunks_exact(4)
        .flat_map(|rgba_chunk| [rgba_chunk[0], rgba_chunk[1], rgba_chunk[2]])
        .collect();
    encoder.set_palette(rgb_palette);
    
    // Prepare tRNS (alpha for palette entries)
    let num_palette_colors = args.colors as usize;
    let mut trns_data: Vec<u8> = Vec::with_capacity(num_palette_colors);
    let mut has_any_transparency = false;
    for (i, rgba_chunk) in palette_rgba.chunks_exact(4).enumerate() {
        if i >= num_palette_colors { break; } // Only consider actual palette size
        let alpha_val = rgba_chunk[3];
        trns_data.push(alpha_val);
        if alpha_val < 255 {
            has_any_transparency = true;
        }
    }

    if has_any_transparency {
        // Optimize tRNS: trim trailing 255s
        if let Some(last_transparent_idx) = trns_data.iter().rposition(|&alpha_byte| alpha_byte < 255) {
            trns_data.truncate(last_transparent_idx + 1);
        } else { // All are 255 (or list empty), effectively no transparency
            trns_data.clear();
        }
        if !trns_data.is_empty() {
            encoder.set_trns(trns_data);
        }
    }
    
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&indexed_image_data)?;
    
    println!("Successfully saved: {}", output_filename);
    Ok(())
}
