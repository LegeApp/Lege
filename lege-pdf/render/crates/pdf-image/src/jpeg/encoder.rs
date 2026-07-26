//! Baseline JPEG encoder — a Rust port of TooJpeg
//! (<https://create.stephan-brumme.com/toojpeg/>), originally written in C++
//! by Stephan Brumme. Adapted from the standalone `toojpeg.rs` port: the
//! decoder in this module's parent is built as its mirror image and shares
//! its tables. The encoder also gives tests a dependency-free way to
//! produce real JPEG streams for round-trip verification.
//!
//! Baseline sequential only (SOF0), 8-bit, grayscale or RGB (4:4:4 or
//! 4:2:0), the ITU T.81 Annex K default quantization and Huffman tables.

// The AAN constants are the canonical values from the JPEG literature;
// keep them exactly as published.
#![allow(clippy::excessive_precision, clippy::approx_constant)]

pub(crate) const DEFAULT_QUANT_LUMINANCE: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
pub(crate) const DEFAULT_QUANT_CHROMINANCE: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// Zigzag position `i` → natural (row-major) position. Shared with the
/// decoder (whose coefficient scan runs in zigzag order).
pub(crate) const ZIGZAG_INV: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

pub(crate) const DC_LUMINANCE_CODES_PER_BITSIZE: [u8; 16] =
    [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
pub(crate) const DC_LUMINANCE_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
pub(crate) const AC_LUMINANCE_CODES_PER_BITSIZE: [u8; 16] =
    [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125];
pub(crate) const AC_LUMINANCE_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
    0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5,
    0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
    0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];
pub(crate) const DC_CHROMINANCE_CODES_PER_BITSIZE: [u8; 16] =
    [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
pub(crate) const DC_CHROMINANCE_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
pub(crate) const AC_CHROMINANCE_CODES_PER_BITSIZE: [u8; 16] =
    [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119];
pub(crate) const AC_CHROMINANCE_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23, 0x33, 0x52, 0xF0,
    0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25, 0xF1, 0x17, 0x18, 0x19, 0x1A, 0x26,
    0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
    0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3,
    0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA,
    0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];
const CODE_WORD_LIMIT: i16 = 2048;

/// The AAN DCT per-frequency scale factors (`cos(k·π/16)`-derived); the same
/// constants scale the decoder's dequantization tables.
pub(crate) const AAN_SCALE_FACTORS: [f32; 8] = [
    1.0,
    1.387039845,
    1.306562965,
    1.175875602,
    1.0,
    0.785694958,
    0.541196100,
    0.275899379,
];

#[derive(Copy, Clone, Debug)]
struct BitCode {
    code: u16,
    num_bits: u8,
}

impl BitCode {
    const fn new(code: u16, num_bits: u8) -> Self {
        Self { code, num_bits }
    }
}

/// MSB-first bit accumulator with JPEG byte stuffing (`0xFF` → `0xFF 0x00`).
/// The decoder's `BitReader` is its exact inverse.
struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    data: u32,
    num_bits: u8,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self {
            out,
            data: 0,
            num_bits: 0,
        }
    }

    fn write_byte(&mut self, byte: u8) {
        self.out.push(byte);
    }

    fn write_stuffed_byte(&mut self, byte: u8) {
        self.out.push(byte);
        if byte == 0xFF {
            self.out.push(0x00);
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    fn write_bits(&mut self, code: u16, num_bits: u8) {
        self.num_bits += num_bits;
        self.data <<= num_bits as u32;
        self.data |= code as u32;
        while self.num_bits >= 8 {
            self.num_bits -= 8;
            let byte = ((self.data >> self.num_bits) & 0xFF) as u8;
            self.write_stuffed_byte(byte);
        }
    }

    /// Pad the residual bits with 1s and emit the final byte.
    fn flush(&mut self) {
        if self.num_bits > 0 {
            let padding = 8 - self.num_bits;
            self.data <<= padding as u32;
            self.data |= (1u32 << padding) - 1;
            let byte = (self.data & 0xFF) as u8;
            self.num_bits = 0;
            self.write_stuffed_byte(byte);
        }
    }

    fn add_marker(&mut self, marker: u8, length: u16) {
        self.write_byte(0xFF);
        self.write_byte(marker);
        self.write_byte((length >> 8) as u8);
        self.write_byte((length & 0xFF) as u8);
    }
}

#[inline(always)]
fn rgb2y(r: u8, g: u8, b: u8) -> f32 {
    0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
}

#[inline(always)]
fn rgb2cb(r: u8, g: u8, b: u8) -> f32 {
    -0.1687 * r as f32 - 0.3313 * g as f32 + 0.500 * b as f32
}

#[inline(always)]
fn rgb2cr(r: u8, g: u8, b: u8) -> f32 {
    0.500 * r as f32 - 0.4187 * g as f32 - 0.0813 * b as f32
}

/// One 8-point AAN forward DCT pass over `block[0], block[stride], ...`.
fn dct(block: &mut [f32], stride: usize) {
    const SQRT_HALF_SQRT: f32 = 1.306562965;
    const INV_SQRT: f32 = 0.707106781;
    const HALF_SQRT_SQRT: f32 = 0.382683432;
    const INV_SQRT_SQRT: f32 = 0.541196100;

    let block0 = block[0];
    let block1 = block[stride];
    let block2 = block[2 * stride];
    let block3 = block[3 * stride];
    let block4 = block[4 * stride];
    let block5 = block[5 * stride];
    let block6 = block[6 * stride];
    let block7 = block[7 * stride];

    let add07 = block0 + block7;
    let sub07 = block0 - block7;
    let add16 = block1 + block6;
    let sub16 = block1 - block6;
    let add25 = block2 + block5;
    let sub25 = block2 - block5;
    let add34 = block3 + block4;
    let sub34 = block3 - block4;

    let add0347 = add07 + add34;
    let sub07_34 = add07 - add34;
    let add1256 = add16 + add25;
    let sub16_25 = add16 - add25;

    block[0] = add0347 + add1256;
    block[4 * stride] = add0347 - add1256;

    let z1 = (sub16_25 + sub07_34) * INV_SQRT;
    block[2 * stride] = sub07_34 + z1;
    block[6 * stride] = sub07_34 - z1;

    let sub23_45 = sub25 + sub34;
    let sub12_56 = sub16 + sub25;
    let sub01_67 = sub16 + sub07;

    let z5 = (sub23_45 - sub01_67) * HALF_SQRT_SQRT;
    let z2 = sub23_45 * INV_SQRT_SQRT + z5;
    let z3 = sub12_56 * INV_SQRT;
    let z4 = sub01_67 * SQRT_HALF_SQRT + z5;
    let z6 = sub07 + z3;
    let z7 = sub07 - z3;
    block[stride] = z6 + z4;
    block[7 * stride] = z6 - z4;
    block[5 * stride] = z7 + z2;
    block[3 * stride] = z7 - z2;
}

#[allow(clippy::too_many_arguments)]
fn encode_block(
    writer: &mut BitWriter,
    block: &mut [[f32; 8]; 8],
    scaled: &[f32; 64],
    last_dc: i16,
    huffman_dc: &[BitCode; 256],
    huffman_ac: &[BitCode; 256],
    codewords: &[BitCode],
) -> i16 {
    let mut block64 = [0.0f32; 64];
    for y in 0..8 {
        for x in 0..8 {
            block64[y * 8 + x] = block[y][x];
        }
    }
    for offset in 0..8 {
        dct(&mut block64[offset * 8..], 1);
    }
    for offset in 0..8 {
        dct(&mut block64[offset..], 8);
    }
    for (idx, coeff) in block64.iter_mut().enumerate() {
        *coeff *= scaled[idx];
    }

    // Round the quantized DC to nearest (the original port's cast dropped
    // the fraction).
    let dc = (block64[0] + if block64[0] >= 0.0 { 0.5 } else { -0.5 }) as i16;

    let mut pos_non_zero = 0;
    let mut quantized = [0i16; 64];
    for i in 1..64 {
        let value = block64[ZIGZAG_INV[i] as usize];
        quantized[i] = (value + if value >= 0.0 { 0.5 } else { -0.5 }) as i16;
        if quantized[i] != 0 {
            pos_non_zero = i;
        }
    }

    let diff = dc - last_dc;
    if diff == 0 {
        writer.write_bits(huffman_dc[0].code, huffman_dc[0].num_bits);
    } else {
        let clamped = diff.clamp(-(CODE_WORD_LIMIT - 1), CODE_WORD_LIMIT - 1);
        let bits = codewords[(clamped + CODE_WORD_LIMIT) as usize];
        let sym = &huffman_dc[bits.num_bits as usize];
        writer.write_bits(sym.code, sym.num_bits);
        writer.write_bits(bits.code, bits.num_bits);
    }

    let mut run = 0u8;
    for &ac in &quantized[1..=pos_non_zero] {
        if ac == 0 {
            run += 1;
            continue;
        }
        while run >= 16 {
            writer.write_bits(huffman_ac[0xF0].code, huffman_ac[0xF0].num_bits);
            run -= 16;
        }
        let clamped = ac.clamp(-(CODE_WORD_LIMIT - 1), CODE_WORD_LIMIT - 1);
        let encoded = codewords[(clamped + CODE_WORD_LIMIT) as usize];
        let symbol = (run << 4) | encoded.num_bits;
        writer.write_bits(
            huffman_ac[symbol as usize].code,
            huffman_ac[symbol as usize].num_bits,
        );
        writer.write_bits(encoded.code, encoded.num_bits);
        run = 0;
    }
    if pos_non_zero < 63 {
        writer.write_bits(huffman_ac[0].code, huffman_ac[0].num_bits);
    }
    dc
}

/// Assign canonical Huffman codes exactly as a decoder reconstructs them
/// from the DHT counts/values lists.
fn generate_huffman_table(num_codes: &[u8], values: &[u8], result: &mut [BitCode]) {
    let mut huffman_code = 0u16;
    let mut value_index = 0usize;
    for num_bits in 1..=16u8 {
        for _ in 0..num_codes[num_bits as usize - 1] {
            result[values[value_index] as usize] = BitCode::new(huffman_code, num_bits);
            huffman_code += 1;
            value_index += 1;
        }
        huffman_code <<= 1;
    }
}

/// Encode `pixels` (RGB, 3 bytes/pixel, or grayscale, 1 byte/pixel) as a
/// baseline JPEG. `downsample` selects 4:2:0 chroma (RGB only).
pub fn write_jpeg(
    pixels: &[u8],
    width: u16,
    height: u16,
    is_rgb: bool,
    quality: u8,
    downsample: bool,
) -> Result<Vec<u8>, &'static str> {
    if width == 0 || height == 0 {
        return Err("invalid image dimensions");
    }
    let num_components = if is_rgb { 3usize } else { 1 };
    if pixels.len() < width as usize * height as usize * num_components {
        return Err("pixel buffer too small");
    }
    let downsample = downsample && is_rgb;

    let mut out = Vec::new();
    let mut writer = BitWriter::new(&mut out);

    // SOI + JFIF APP0.
    writer.write_bytes(&[
        0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0,
    ]);

    let quality = quality.clamp(1, 100) as u16;
    let quality = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };

    let mut quant_luminance = [0u8; 64];
    let mut quant_chrominance = [0u8; 64];
    for i in 0..64 {
        let lum = (DEFAULT_QUANT_LUMINANCE[ZIGZAG_INV[i] as usize] as u16 * quality + 50) / 100;
        let chrom = (DEFAULT_QUANT_CHROMINANCE[ZIGZAG_INV[i] as usize] as u16 * quality + 50) / 100;
        quant_luminance[i] = lum.clamp(1, 255) as u8;
        quant_chrominance[i] = chrom.clamp(1, 255) as u8;
    }

    // DQT.
    let table_length = 2 + (if is_rgb { 2 } else { 1 }) * (1 + 64);
    writer.add_marker(0xDB, table_length as u16);
    writer.write_byte(0);
    writer.write_bytes(&quant_luminance);
    if is_rgb {
        writer.write_byte(1);
        writer.write_bytes(&quant_chrominance);
    }

    // SOF0.
    let frame_length = 2 + 6 + 3 * num_components;
    writer.add_marker(0xC0, frame_length as u16);
    writer.write_byte(8);
    writer.write_byte((height >> 8) as u8);
    writer.write_byte(height as u8);
    writer.write_byte((width >> 8) as u8);
    writer.write_byte(width as u8);
    writer.write_byte(num_components as u8);
    for id in 1..=num_components {
        writer.write_byte(id as u8);
        writer.write_byte(if id == 1 && downsample { 0x22 } else { 0x11 });
        writer.write_byte(if id == 1 { 0 } else { 1 });
    }

    // DHT + code tables.
    let htable_length = if is_rgb { 2 + 208 + 208 } else { 2 + 208 };
    writer.add_marker(0xC4, htable_length as u16);
    writer.write_byte(0x00);
    writer.write_bytes(&DC_LUMINANCE_CODES_PER_BITSIZE);
    writer.write_bytes(&DC_LUMINANCE_VALUES);
    writer.write_byte(0x10);
    writer.write_bytes(&AC_LUMINANCE_CODES_PER_BITSIZE);
    writer.write_bytes(&AC_LUMINANCE_VALUES);

    let mut huffman_luminance_dc = [BitCode::new(0, 0); 256];
    let mut huffman_luminance_ac = [BitCode::new(0, 0); 256];
    generate_huffman_table(
        &DC_LUMINANCE_CODES_PER_BITSIZE,
        &DC_LUMINANCE_VALUES,
        &mut huffman_luminance_dc,
    );
    generate_huffman_table(
        &AC_LUMINANCE_CODES_PER_BITSIZE,
        &AC_LUMINANCE_VALUES,
        &mut huffman_luminance_ac,
    );

    let mut huffman_chrominance_dc = [BitCode::new(0, 0); 256];
    let mut huffman_chrominance_ac = [BitCode::new(0, 0); 256];
    if is_rgb {
        writer.write_byte(0x01);
        writer.write_bytes(&DC_CHROMINANCE_CODES_PER_BITSIZE);
        writer.write_bytes(&DC_CHROMINANCE_VALUES);
        writer.write_byte(0x11);
        writer.write_bytes(&AC_CHROMINANCE_CODES_PER_BITSIZE);
        writer.write_bytes(&AC_CHROMINANCE_VALUES);
        generate_huffman_table(
            &DC_CHROMINANCE_CODES_PER_BITSIZE,
            &DC_CHROMINANCE_VALUES,
            &mut huffman_chrominance_dc,
        );
        generate_huffman_table(
            &AC_CHROMINANCE_CODES_PER_BITSIZE,
            &AC_CHROMINANCE_VALUES,
            &mut huffman_chrominance_ac,
        );
    }

    // SOS.
    let scan_length = 2 + 1 + 2 * num_components + 3;
    writer.add_marker(0xDA, scan_length as u16);
    writer.write_byte(num_components as u8);
    for id in 1..=num_components {
        writer.write_byte(id as u8);
        writer.write_byte(if id == 1 { 0x00 } else { 0x11 });
    }
    writer.write_bytes(&[0, 63, 0]);

    // Quantization scale with the AAN factors folded in (the decoder's
    // dequantization tables are the exact reciprocal).
    let mut scaled_lum_row = [0.0f32; 64];
    let mut scaled_ch_row = [0.0f32; 64];
    for zig in 0..64 {
        let pos = ZIGZAG_INV[zig] as usize;
        let factor = 1.0 / (AAN_SCALE_FACTORS[pos / 8] * AAN_SCALE_FACTORS[pos % 8] * 8.0);
        scaled_lum_row[pos] = factor / quant_luminance[zig] as f32;
        scaled_ch_row[pos] = factor / quant_chrominance[zig] as f32;
    }

    let mut codewords_array = vec![BitCode::new(0, 0); 2 * CODE_WORD_LIMIT as usize];
    let mut num_bits = 1u8;
    let mut mask = 1i16;
    for value in 1..CODE_WORD_LIMIT {
        if value > mask {
            num_bits += 1;
            mask = (mask << 1) | 1;
        }
        codewords_array[(CODE_WORD_LIMIT - value) as usize] =
            BitCode::new((mask - value) as u16, num_bits);
        codewords_array[(CODE_WORD_LIMIT + value) as usize] = BitCode::new(value as u16, num_bits);
    }

    let max_width = width - 1;
    let max_height = height - 1;
    let sampling = if downsample { 2u16 } else { 1 };
    let mcu_size = 8 * sampling;
    let mut last_y_dc = 0i16;
    let mut last_cb_dc = 0i16;
    let mut last_cr_dc = 0i16;
    let mut y_block = [[0.0f32; 8]; 8];
    let mut cb_block = [[0.0f32; 8]; 8];
    let mut cr_block = [[0.0f32; 8]; 8];

    for mcu_y in (0..height).step_by(mcu_size as usize) {
        for mcu_x in (0..width).step_by(mcu_size as usize) {
            for block_y in (0..mcu_size).step_by(8) {
                for block_x in (0..mcu_size).step_by(8) {
                    for delta_y in 0..8u16 {
                        let row = (mcu_y + block_y + delta_y).min(max_height);
                        let mut column = (mcu_x + block_x).min(max_width);
                        for delta_x in 0..8u16 {
                            let pixel_pos =
                                (row as usize * width as usize + column as usize) * num_components;
                            if column < max_width {
                                column += 1;
                            }
                            if !is_rgb {
                                y_block[delta_y as usize][delta_x as usize] =
                                    pixels[pixel_pos] as f32 - 128.0;
                            } else {
                                let r = pixels[pixel_pos];
                                let g = pixels[pixel_pos + 1];
                                let b = pixels[pixel_pos + 2];
                                y_block[delta_y as usize][delta_x as usize] =
                                    rgb2y(r, g, b) - 128.0;
                                if !downsample {
                                    cb_block[delta_y as usize][delta_x as usize] = rgb2cb(r, g, b);
                                    cr_block[delta_y as usize][delta_x as usize] = rgb2cr(r, g, b);
                                }
                            }
                        }
                    }
                    last_y_dc = encode_block(
                        &mut writer,
                        &mut y_block,
                        &scaled_lum_row,
                        last_y_dc,
                        &huffman_luminance_dc,
                        &huffman_luminance_ac,
                        &codewords_array,
                    );
                }
            }

            if is_rgb {
                if downsample {
                    // Average 2x2 pixel quads for one 8x8 chroma block.
                    for delta_y in 0..8u16 {
                        let row = (mcu_y + 2 * delta_y).min(max_height);
                        for delta_x in 0..8u16 {
                            let column = (mcu_x + 2 * delta_x).min(max_width);
                            let right = (column + 1).min(max_width);
                            let down = (row + 1).min(max_height);
                            let at =
                                |r: u16, c: u16| (r as usize * width as usize + c as usize) * 3;
                            let quad = [
                                at(row, column),
                                at(row, right),
                                at(down, column),
                                at(down, right),
                            ];
                            let mut sums = [0u32; 3];
                            for pos in quad {
                                for (ch, sum) in sums.iter_mut().enumerate() {
                                    *sum += pixels[pos + ch] as u32;
                                }
                            }
                            let r_avg = ((sums[0] + 2) / 4) as u8;
                            let g_avg = ((sums[1] + 2) / 4) as u8;
                            let b_avg = ((sums[2] + 2) / 4) as u8;
                            cb_block[delta_y as usize][delta_x as usize] =
                                rgb2cb(r_avg, g_avg, b_avg);
                            cr_block[delta_y as usize][delta_x as usize] =
                                rgb2cr(r_avg, g_avg, b_avg);
                        }
                    }
                }
                last_cb_dc = encode_block(
                    &mut writer,
                    &mut cb_block,
                    &scaled_ch_row,
                    last_cb_dc,
                    &huffman_chrominance_dc,
                    &huffman_chrominance_ac,
                    &codewords_array,
                );
                last_cr_dc = encode_block(
                    &mut writer,
                    &mut cr_block,
                    &scaled_ch_row,
                    last_cr_dc,
                    &huffman_chrominance_dc,
                    &huffman_chrominance_ac,
                    &codewords_array,
                );
            }
        }
    }

    writer.flush();
    writer.write_byte(0xFF);
    writer.write_byte(0xD9);
    Ok(out)
}
