// src/palette.rs

//! Color palette generation and management for DjVu encoding.
//!
//! This module replaces the C++ `DjVuPalette` class. It defines a `Palette` struct
//! to hold a color map and provides a `Quantizer` trait to allow for pluggable
//! color quantization algorithms.
//!
//! Your custom NeuQuant implementation is provided as the default `Quantizer`.

use crate::image::image_formats::{Pixel, Pixmap};
use crate::utils::error::{DjvuError, Result};
use ::image::Rgb;
use bytemuck::{cast_slice, Pod, Zeroable};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Cursor, Read, Write};

// --- Helper trait for u24 operations ---
trait ReadWriteU24 {
    fn read_u24<R: Read>(reader: &mut R) -> Result<u32>;
    fn write_u24<W: Write>(writer: &mut W, value: u32) -> Result<()>;
}

struct U24Helper;

impl ReadWriteU24 for U24Helper {
    fn read_u24<R: Read>(reader: &mut R) -> Result<u32> {
        let mut bytes = [0u8; 3];
        reader.read_exact(&mut bytes)?;
        Ok(((bytes[0] as u32) << 16) | ((bytes[1] as u32) << 8) | (bytes[2] as u32))
    }

    fn write_u24<W: Write>(writer: &mut W, value: u32) -> Result<()> {
        if value > 0xFFFFFF {
            return Err(DjvuError::InvalidArg("Value too large for u24".to_string()));
        }
        let bytes = [
            ((value >> 16) & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        ];
        writer.write_all(&bytes)?;
        Ok(())
    }
}

// --- Bytemuck-compatible color types ---

/// A BGR color representation that can be safely cast to/from bytes
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct BgrColor {
    pub b: u8,
    pub g: u8,
    pub r: u8,
}

/// An RGBA color representation that can be safely cast to/from bytes
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct RgbaColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl From<Rgb<u8>> for BgrColor {
    fn from(rgb: Rgb<u8>) -> Self {
        BgrColor {
            b: rgb.0[2],
            g: rgb.0[1],
            r: rgb.0[0],
        }
    }
}

impl From<BgrColor> for Rgb<u8> {
    fn from(bgr: BgrColor) -> Self {
        Rgb([bgr.r, bgr.g, bgr.b])
    }
}

impl From<Rgb<u8>> for RgbaColor {
    fn from(rgb: Rgb<u8>) -> Self {
        RgbaColor {
            r: rgb.0[0],
            g: rgb.0[1],
            b: rgb.0[2],
            a: 255,
        }
    }
}

// --- Quantizer Trait and Your Implementation ---

/// A trait for algorithms that can create a color palette from a set of pixels.
pub trait Quantizer {
    /// Takes a slice of RGB pixels and returns a palette of at most `max_colors`.
    fn quantize(&self, pixels: &[Rgb<u8>], max_colors: usize) -> Vec<Rgb<u8>>;
}

/// A high-speed color quantizer based on the NeuQuant algorithm.
/// This struct wraps your provided quantization logic.
pub struct NeuQuantQuantizer {
    /// Sampling factor for the learning algorithm (1-30).
    /// Lower is faster but potentially lower quality. A good default is 10.
    pub sample_factor: i32,
}

impl Quantizer for NeuQuantQuantizer {
    /// Runs the NeuQuant algorithm on the input pixels to generate a palette.
    fn quantize(&self, pixels: &[Rgb<u8>], max_colors: usize) -> Vec<Rgb<u8>> {
        // Convert RGB to RGBA using bytemuck for efficient zero-copy conversion
        let rgba_colors: Vec<RgbaColor> = pixels.iter().map(|&rgb| rgb.into()).collect();
        let rgba_bytes: &[u8] = cast_slice(&rgba_colors);

        let nq = your_neuquant::NeuQuant::new(self.sample_factor, max_colors, rgba_bytes);
        let palette_rgba_bytes = nq.color_map_rgba();

        // Convert RGBA bytes back to RGB using bytemuck
        let rgba_colors: &[RgbaColor] = cast_slice(&palette_rgba_bytes);
        rgba_colors
            .iter()
            .map(|&rgba| Rgb([rgba.r, rgba.g, rgba.b]))
            .collect()
    }
}

// --- Palette Data Structure ---

/// Represents a color palette for a DjVu image.
#[derive(Debug, Clone)]
pub struct Palette {
    /// The list of RGB colors in the palette.
    colors: Vec<Pixel>,
    // The `colordata` array from the C++ version, for storing a sequence of color indices.
    // This is used for the foreground layer of compound documents.
    pub color_indices: Vec<u16>,
}

impl Palette {
    /// Creates a new palette by running a quantizer on a source image.
    ///
    /// # Arguments
    /// * `image` - The source pixmap to analyze for colors.
    /// * `max_colors` - The maximum number of colors the final palette should have.
    /// * `quantizer` - An object that implements the `Quantizer` trait.
    pub fn new(image: &Pixmap, max_colors: usize, quantizer: &impl Quantizer) -> Self {
        let pixels: Vec<Pixel> = image.pixels().cloned().collect();
        let colors = quantizer.quantize(&pixels, max_colors);
        Palette {
            colors,
            color_indices: Vec::new(),
        }
    }

    /// Creates a palette directly from a list of colors.
    pub fn from_colors(colors: Vec<Pixel>) -> Self {
        Palette {
            colors,
            color_indices: Vec::new(),
        }
    }

    /// Returns the number of colors in the palette.
    #[inline]
    pub fn len(&self) -> usize {
        self.colors.len()
    }

    /// Finds the index of the color in the palette that is closest to the given color.
    ///
    /// This uses a simple linear search, which is fast enough for small palettes (<= 256 colors).
    pub fn color_to_index(&self, color: &Pixel) -> u16 {
        self.colors
            .iter()
            .enumerate()
            .min_by_key(|(_, pal_color)| {
                let dr = pal_color.0[0] as i32 - color.0[0] as i32;
                let dg = pal_color.0[1] as i32 - color.0[1] as i32;
                let db = pal_color.0[2] as i32 - color.0[2] as i32;
                // Use squared Euclidean distance to avoid sqrt
                dr * dr + dg * dg + db * db
            })
            .map(|(i, _)| i as u16)
            .unwrap_or(0)
    }

    /// Efficiently converts a slice of RGB pixels to color indices using bytemuck operations.
    pub fn pixels_to_indices(&self, pixels: &[Rgb<u8>]) -> Vec<u16> {
        pixels
            .iter()
            .map(|pixel| self.color_to_index(pixel))
            .collect()
    }

    pub fn indices_to_pixels(&self, indices: &[u16]) -> Vec<Rgb<u8>> {
        indices
            .iter()
            .map(|&index| {
                self.index_to_color(index)
                    .copied()
                    .unwrap_or(Rgb([0, 0, 0])) // Default to black for invalid indices
            })
            .collect()
    }

    pub fn set_color_indices(&mut self, indices: Vec<u16>) {
        self.color_indices = indices;
    }

    pub fn color_indices_as_bytes(&self) -> &[u8] {
        cast_slice(&self.color_indices)
    }

    pub fn set_color_indices_from_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() % 2 != 0 {
            return Err(DjvuError::InvalidArg(
                "Byte slice length must be even for u16 conversion".to_string(),
            ));
        }
        let mut cursor = Cursor::new(bytes);
        self.color_indices.clear();
        while cursor.position() < bytes.len() as u64 {
            let index = cursor.read_u16::<BigEndian>()?;
            self.color_indices.push(index);
        }
        Ok(())
    }

    /// Returns the color at a given index in the palette.
    #[inline]
    pub fn index_to_color(&self, index: u16) -> Option<&Pixel> {
        self.colors.get(index as usize)
    }

    /// Encodes the palette into the DjVu `FGbz` chunk format.
    pub fn encode<W: Write>(&self, writer: &mut W) -> Result<()> {
        let version = if self.color_indices.is_empty() {
            0x00
        } else {
            0x80
        };
        writer.write_u8(version)?;

        let palette_size = self.len();
        if palette_size > 65535 {
            return Err(DjvuError::InvalidOperation(
                "Palette size cannot exceed 65535".to_string(),
            ));
        }
        writer.write_u16::<BigEndian>(palette_size as u16)?;

        let bgr_colors: Vec<BgrColor> = self.colors.iter().map(|&rgb| rgb.into()).collect();
        let bgr_bytes: &[u8] = cast_slice(&bgr_colors);
        writer.write_all(bgr_bytes)?;

        if !self.color_indices.is_empty() {
            let data_size = self.color_indices.len();
            if data_size > 0xFF_FFFF {
                return Err(DjvuError::InvalidOperation(
                    "Color index data size cannot exceed 24 bits".to_string(),
                ));
            }
            U24Helper::write_u24(writer, data_size as u32)?;

            // Write each u16 index in BigEndian
            for &index in &self.color_indices {
                writer.write_u16::<BigEndian>(index)?;
            }
        }

        Ok(())
    }

    /// Decodes a palette from the DjVu `FGbz` chunk format. (For completeness)
    pub fn decode<R: Read>(reader: &mut R) -> Result<Self> {
        let version = reader.read_u8()?;
        if (version & 0x7F) != 0 {
            return Err(DjvuError::Stream(
                "Unsupported DjVuPalette version.".to_string(),
            ));
        }

        let palette_size = reader.read_u16::<BigEndian>()? as usize;

        let mut bgr_bytes = vec![0u8; palette_size * 3];
        reader.read_exact(&mut bgr_bytes)?;
        let bgr_colors: &[BgrColor] = cast_slice(&bgr_bytes);
        let colors: Vec<Rgb<u8>> = bgr_colors.iter().map(|&bgr| bgr.into()).collect();

        let mut color_indices = Vec::new();
        if (version & 0x80) != 0 {
            let data_size = U24Helper::read_u24(reader)? as usize;

            // Read the byte slice and parse as BigEndian u16
            let mut index_bytes = vec![0u8; data_size * 2];
            reader.read_exact(&mut index_bytes)?;
            let mut cursor = Cursor::new(&index_bytes);
            for _ in 0..data_size {
                let index = cursor.read_u16::<BigEndian>()?;
                color_indices.push(index);
            }
        }

        Ok(Palette {
            colors,
            color_indices,
        })
    }
}

// --- A namespace for your provided NeuQuant code ---
mod your_neuquant {
    // Paste your entire NeuQuant implementation here.
    // I will paste the code you provided, with minimal changes to make it a valid module.

    // Removed unused import: use rayon::prelude::*;
    const CHANNELS: usize = 4;
    const RADIUS_DEC: i32 = 30;
    const ALPHA_BIASSHIFT: i32 = 10;
    const INIT_ALPHA: i32 = 1 << ALPHA_BIASSHIFT;

    const GAMMA: f32 = 1024.0;
    const BETA: f32 = 1.0 / GAMMA;
    const BETAGAMMA: f32 = 1.0;

    const PRIMES: [usize; 4] = [499, 491, 487, 503];

    #[derive(Clone, Copy, Debug)]
    struct Quad<T> {
        r: T,
        g: T,
        b: T,
        a: T,
    }

    type Neuron = Quad<f32>;
    type Color = Quad<i32>;

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
            let mut this = NeuQuant {
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
                for val in self.netindex.iter_mut() {
                    *val = 0;
                }
                return;
            }

            let freq_val = 1.0f32 / self.netsize as f32;
            for i in 0..self.netsize {
                let tmp = i as f32 * 256.0 / self.netsize as f32;
                let a_init = if self.netsize <= 16 {
                    i as f32 * (255.0 / (self.netsize as f32 - 1.0).max(1.0))
                } else {
                    if i < 16 {
                        (i as f32) * 16.0
                    } else {
                        255.0
                    }
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
            let n = &mut self.network[neuron_idx as usize];
            n.b -= alpha_scale * (n.b - quad_pix.b);
            n.g -= alpha_scale * (n.g - quad_pix.g);
            n.r -= alpha_scale * (n.r - quad_pix.r);
            n.a -= alpha_scale * (n.a - quad_pix.a);
        }

        fn alter_neighbour(
            &mut self,
            alpha_scale: f32,
            rad: i32,
            center_idx: i32,
            quad_pix: Quad<f32>,
        ) {
            let lo = (center_idx - rad).max(0);
            let hi = (center_idx + rad).min(self.netsize as i32 - 1);

            let mut j = center_idx + 1;
            let mut k = center_idx - 1;
            let mut q = 1;

            while j <= hi || k >= lo {
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

            self.freq[bestpos as usize] += BETA;
            self.bias[bestpos as usize] -= BETAGAMMA;
            bestbiaspos
        }

        fn learn(&mut self, pixels: &[u8]) {
            if self.netsize == 0 || pixels.is_empty() {
                return;
            }

            let initrad = self.netsize as i32 / 8;
            let alphadec = (30 + ((self.samplefac - 1) / 3)).max(1);
            let lengthcount = pixels.len() / CHANNELS;
            let samplepixels = (lengthcount / self.samplefac as usize).max(1);
            let n_cycles = 100;
            let delta = (samplepixels / n_cycles).max(1);
            let mut alpha = INIT_ALPHA;
            let mut rad = initrad.max(1);
            let mut pos = 0;
            let step = *PRIMES
                .iter()
                .find(|&&p| lengthcount % p != 0)
                .unwrap_or(&PRIMES[3]);

            for i in 0..samplepixels {
                let p = &pixels[((pos % lengthcount) * CHANNELS)..];
                let quad_pix = Quad {
                    r: p[0] as f32,
                    g: p[1] as f32,
                    b: p[2] as f32,
                    a: p[3] as f32,
                };

                let winning_neuron_idx =
                    self.contest(quad_pix.b, quad_pix.g, quad_pix.r, quad_pix.a);
                let alpha_scale = (alpha as f32) / (INIT_ALPHA as f32);

                self.salter_single(alpha_scale, winning_neuron_idx, quad_pix);
                if rad > 0 {
                    self.alter_neighbour(alpha_scale, rad, winning_neuron_idx, quad_pix);
                }

                pos += step;

                if (i + 1) % delta == 0 {
                    alpha -= alpha / alphadec;
                    let bias_radius = rad * (1 << 6);
                    rad = (bias_radius - (bias_radius / RADIUS_DEC)) >> 6;
                    if rad <= 1 {
                        rad = 0;
                    }
                }
            }
        }

        fn build_colormap(&mut self) {
            for i in 0..self.netsize {
                self.colormap[i].b = (self.network[i].b.max(0.0).min(255.0) + 0.5) as i32;
                self.colormap[i].g = (self.network[i].g.max(0.0).min(255.0) + 0.5) as i32;
                self.colormap[i].r = (self.network[i].r.max(0.0).min(255.0) + 0.5) as i32;
                self.colormap[i].a = (self.network[i].a.max(0.0).min(255.0) + 0.5) as i32;
            }
        }

        fn build_netindex(&mut self) {
            self.colormap.sort_unstable_by_key(|c| c.g);
            let mut previouscol = 0;
            let mut startpos = 0;
            for i in 0..self.netsize {
                let p = &self.colormap[i];
                let p_g = p.g.clamp(0, 255) as usize;
                if p_g != previouscol {
                    self.netindex[previouscol] = (startpos + i) >> 1;
                    for j in (previouscol + 1)..p_g {
                        self.netindex[j] = i;
                    }
                    previouscol = p_g;
                    startpos = i;
                }
            }
            let max_netpos = (self.netsize - 1).max(0);
            self.netindex[previouscol] = (startpos + max_netpos) >> 1;
            for j in (previouscol + 1)..256 {
                self.netindex[j] = max_netpos;
            }
        }
    }
}
