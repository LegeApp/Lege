use anyhow::Result;
use image::RgbImage;
use png::{BitDepth, ColorType, Compression, Encoder};
use rayon::prelude::*;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

const CHANNELS: usize = 4;
const RADIUS_DEC: i32 = 30;
const ALPHA_BIASSHIFT: i32 = 10;
const INIT_ALPHA: i32 = 1 << ALPHA_BIASSHIFT;
const GAMMA: f32 = 1024.0;
const BETA: f32 = 1.0 / GAMMA;
const BETAGAMMA: f32 = 1.0;
const PRIMES: [usize; 4] = [499, 491, 487, 503];

#[derive(Debug, Clone, Copy)]
struct Quad<T> {
    r: T,
    g: T,
    b: T,
    a: T,
}

type Neuron = Quad<f32>;
type Color = Quad<i32>;

#[derive(Debug, Clone, Copy)]
pub struct PngQuantizationOptions {
    pub colors: u16,
}

impl Default for PngQuantizationOptions {
    fn default() -> Self {
        Self { colors: 256 }
    }
}

pub fn write_quantized_rgb_png(
    image: &RgbImage,
    output_path: &Path,
    options: PngQuantizationOptions,
) -> Result<()> {
    let width = image.width();
    let height = image.height();
    let rgba_pixels: Vec<u8> = image
        .as_raw()
        .chunks_exact(3)
        .flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255])
        .collect();

    let colors = options.colors.clamp(1, 256) as usize;
    let nq = NeuQuant::new(10, colors, &rgba_pixels);
    let palette_rgba = nq.color_map_rgba();
    let indexed_image_data: Vec<u8> = rgba_pixels
        .par_chunks_exact(CHANNELS)
        .map(|chunk_rgba| nq.index_of(chunk_rgba) as u8)
        .collect();

    let file = File::create(output_path)?;
    let writer = BufWriter::new(file);
    let mut encoder = Encoder::new(writer, width, height);
    encoder.set_color(ColorType::Indexed);
    encoder.set_depth(BitDepth::Eight);
    encoder.set_compression(Compression::Default);

    let rgb_palette: Vec<u8> = palette_rgba
        .chunks_exact(4)
        .take(colors)
        .flat_map(|rgba| [rgba[0], rgba[1], rgba[2]])
        .collect();
    encoder.set_palette(rgb_palette);

    let mut png_writer = encoder.write_header()?;
    png_writer.write_image_data(&indexed_image_data)?;
    Ok(())
}

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

#[inline]
fn sqr_dist(x: i32, y: i32) -> i32 {
    let d = x - y;
    d * d
}

pub struct NeuQuant {
    network: Vec<Neuron>,
    colormap: Vec<Color>,
    netindex: Vec<usize>,
    bias: Vec<f32>,
    freq: Vec<f32>,
    samplefac: i32,
    netsize: usize,
}

impl NeuQuant {
    pub fn new(samplefac: i32, colors: usize, pixels: &[u8]) -> Self {
        let netsize = colors.max(1);
        let mut this = Self {
            network: Vec::with_capacity(netsize),
            colormap: Vec::with_capacity(netsize),
            netindex: vec![0; 256],
            bias: Vec::with_capacity(netsize),
            freq: Vec::with_capacity(netsize),
            samplefac: samplefac.max(1),
            netsize,
        };
        this.init(pixels);
        this
    }

    pub fn index_of(&self, pixel_rgba: &[u8]) -> usize {
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
        let freq_val = 1.0f32 / self.netsize as f32;
        for i in 0..self.netsize {
            let tmp = i as f32 * 256.0 / self.netsize as f32;
            let a_init = if self.netsize <= 16 {
                i as f32 * (255.0 / (self.netsize - 1).max(1) as f32)
            } else if i < 16 {
                i as f32 * 16.0
            } else {
                255.0
            };

            self.network.push(Neuron {
                r: tmp,
                g: tmp,
                b: tmp,
                a: a_init.clamp(0.0, 255.0),
            });
            self.colormap.push(Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            });
            self.freq.push(freq_val);
            self.bias.push(0.0);
        }

        if pixels.len() >= CHANNELS {
            self.learn(pixels);
        }
        self.build_colormap();
        self.build_netindex();
    }

    fn salter_single(&mut self, alpha_scale: f32, neuron_idx: i32, quad_pix: Quad<f32>) {
        let idx = neuron_idx as usize;
        if idx >= self.netsize {
            return;
        }
        let n = &mut self.network[idx];
        n.b -= alpha_scale * (n.b - quad_pix.b);
        n.g -= alpha_scale * (n.g - quad_pix.g);
        n.r -= alpha_scale * (n.r - quad_pix.r);
        n.a -= alpha_scale * (n.a - quad_pix.a);
    }

    fn alter_neighbour(&mut self, alpha_scale: f32, rad: i32, center_idx: i32, quad_pix: Quad<f32>) {
        let lo = (center_idx - rad).max(0);
        let hi = (center_idx + rad).min(self.netsize as i32 - 1);
        let mut j = center_idx + 1;
        let mut k = center_idx - 1;
        let mut q = 1;

        while (j <= hi) || (k >= lo) {
            let rad_sq = (rad * rad) as f32;
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
        let mut bestd = f32::MAX;
        let mut bestbiasd = f32::MAX;
        let mut bestpos = -1;
        let mut bestbiaspos = -1;

        for i in 0..self.netsize {
            let n = &self.network[i];
            let mut dist = (n.b - b_pix).abs();
            dist += (n.r - r_pix).abs();
            let current_bias = self.bias[i];
            if dist < bestd || dist < bestbiasd + current_bias {
                dist += (n.g - g_pix).abs();
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
            self.bias[i] += BETAGAMMA * current_freq;
        }

        if bestpos == -1 {
            bestpos = 0;
        }
        self.freq[bestpos as usize] += BETA;
        self.bias[bestpos as usize] -= BETAGAMMA;
        if bestbiaspos == -1 {
            bestbiaspos = bestpos;
        }
        bestbiaspos
    }

    fn learn(&mut self, pixels: &[u8]) {
        let mut initrad = self.netsize as i32 / 8;
        if initrad < 1 {
            initrad = 1;
        }
        let radiusbiasshift = 6;
        let radiusbias = 1 << radiusbiasshift;
        let mut bias_radius = initrad * radiusbias;
        let alphadec = (30 + ((self.samplefac - 1) / 3)).max(1);
        let lengthcount = pixels.len() / CHANNELS;
        let samplepixels = lengthcount / self.samplefac as usize;
        if samplepixels == 0 {
            return;
        }
        let n_cycles = ((self.netsize >> 1).max(1)).min(100);
        let delta = (samplepixels / n_cycles).max(1);
        let mut alpha = INIT_ALPHA;
        let mut rad = bias_radius >> radiusbiasshift;
        let mut pos = 0usize;
        let step = *PRIMES
            .iter()
            .find(|&&prime| lengthcount % prime != 0)
            .unwrap_or(&PRIMES[3]);

        for i in 0..samplepixels {
            let pixel_base_idx = (pos % lengthcount) * CHANNELS;
            let p = &pixels[pixel_base_idx..pixel_base_idx + CHANNELS];
            let quad_pix = Quad {
                r: p[0] as f32,
                g: p[1] as f32,
                b: p[2] as f32,
                a: p[3] as f32,
            };

            let winning = self.contest(quad_pix.b, quad_pix.g, quad_pix.r, quad_pix.a);
            let alpha_scale = alpha as f32 / INIT_ALPHA as f32;
            self.salter_single(alpha_scale, winning, quad_pix);
            if rad > 0 {
                self.alter_neighbour(alpha_scale, rad, winning, quad_pix);
            }

            pos += step;
            if (i + 1) % delta == 0 {
                alpha -= alpha / alphadec;
                bias_radius -= bias_radius / RADIUS_DEC;
                rad = bias_radius >> radiusbiasshift;
                if rad < 1 {
                    rad = 0;
                }
            }
        }
    }

    fn build_colormap(&mut self) {
        for i in 0..self.netsize {
            self.colormap[i].b = clamp_round(self.network[i].b);
            self.colormap[i].g = clamp_round(self.network[i].g);
            self.colormap[i].r = clamp_round(self.network[i].r);
            self.colormap[i].a = clamp_round(self.network[i].a);
        }
    }

    fn build_netindex(&mut self) {
        self.colormap.sort_by_key(|c| c.g);
        let mut previous_col = 0usize;
        let mut startpos = 0usize;
        for i in 0..self.netsize {
            let current_col = self.colormap[i].g.clamp(0, 255) as usize;
            if current_col != previous_col {
                self.netindex[previous_col] = (startpos + i) >> 1;
                for j in previous_col + 1..current_col {
                    self.netindex[j] = i;
                }
                previous_col = current_col;
                startpos = i;
            }
        }
        self.netindex[previous_col] = (startpos + self.netsize - 1) >> 1;
        for j in previous_col + 1..256 {
            self.netindex[j] = self.netsize - 1;
        }
    }

    fn search_netindex(&self, b: u8, g: u8, r: u8, a: u8) -> usize {
        let mut best_pos = self.netindex[g as usize];
        let mut best_dist = i32::MAX;
        let first_guess = best_pos;

        for current_pos in first_guess..self.netsize {
            let map_color = self.colormap[current_pos];
            let mut dist = sqr_dist(map_color.g, g as i32);
            if dist > best_dist {
                break;
            }
            dist += sqr_dist(map_color.r, r as i32);
            if dist >= best_dist && map_color.g != g as i32 {
                continue;
            }
            dist += sqr_dist(map_color.b, b as i32);
            if dist >= best_dist && map_color.g != g as i32 {
                continue;
            }
            dist += sqr_dist(map_color.a, a as i32);
            if dist < best_dist {
                best_dist = dist;
                best_pos = current_pos;
            }
        }

        if first_guess > 0 {
            for current_pos in (0..first_guess).rev() {
                let map_color = self.colormap[current_pos];
                let mut dist = sqr_dist(map_color.g, g as i32);
                if dist > best_dist {
                    break;
                }
                dist += sqr_dist(map_color.r, r as i32);
                if dist >= best_dist && map_color.g != g as i32 {
                    continue;
                }
                dist += sqr_dist(map_color.b, b as i32);
                if dist >= best_dist && map_color.g != g as i32 {
                    continue;
                }
                dist += sqr_dist(map_color.a, a as i32);
                if dist < best_dist {
                    best_dist = dist;
                    best_pos = current_pos;
                }
            }
        }

        best_pos
    }
}
